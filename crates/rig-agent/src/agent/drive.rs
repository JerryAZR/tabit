//! The agent run's coroutine — the loop designed in ENGINE.md (the
//! document is the contract; changes here change it).
//!
//! One run is one async loop over a [`ContextManager`]: CONVERGE (the
//! stop check, then the one unconditional drain), DECIDE (the one
//! policy site), PREPARE (the request is the history), MODEL (the
//! provider call, streaming), SETTLE (observe, classify, fold — final
//! or tool roundtrip). The conversation is written at exactly three
//! kinds of site (drain folds, the final fold, the roundtrip
//! `fold_all`), each an atomic verify-then-commit, so every await
//! point sees the conversation at a roundtrip boundary and abort can
//! simply drop the run future.
//!
//! The medium-specific halves (blocking vs streaming: how a turn is
//! fetched, how tools execute, spans and final items) stay behind
//! [`TurnSource`]; everything medium-independent lives here, once.

use std::{pin::Pin, sync::Arc};

use futures::{Stream, StreamExt, stream};
use rig_core::{OneOrMany, message::UserContent, wasm_compat::WasmCompatSend};
use tracing_futures::Instrument;

use crate::{
    agent::{
        completion::{PreparedCompletionRequest, build_prepared_completion_request},
        hook::{AgentHook, HookContext, ModelSelection, ModelSelectionAction},
        model::ModelHandle,
        prompt_request::{
            PromptResponse,
            streaming::{MultiTurnStreamItem, StreamingError},
        },
        run::{ModelTurn, PendingToolCall, ProviderErrorClass, RunLedger},
        runner::{
            AgentRunner, ToolExecution, append_run_messages, new_execute_tool_span,
            resolve_completion_call, run_single_tool,
        },
    },
    completion::{CompletionError, Message, PromptError},
    streaming::{StreamedAssistantContent, StreamedUserContent},
    tool::server::ToolRegistrySnapshot,
};
use tabit_log::ContextManager;

pub(crate) fn record_usage_on_span(span: &tracing::Span, usage: crate::completion::Usage) {
    span.record("gen_ai.usage.input_tokens", usage.input_tokens);
    span.record("gen_ai.usage.output_tokens", usage.output_tokens);
    span.record(
        "gen_ai.usage.cache_read.input_tokens",
        usage.cached_input_tokens,
    );
    span.record(
        "gen_ai.usage.cache_creation.input_tokens",
        usage.cache_creation_input_tokens,
    );
    span.record(
        "gen_ai.usage.tool_use_prompt_tokens",
        usage.tool_use_prompt_tokens,
    );
    span.record("gen_ai.usage.reasoning_tokens", usage.reasoning_tokens);
}

/// A boxed, medium-specific item stream for one loop phase (model turn
/// or tool batch). Boxed so a generic loop can forward it without the
/// per-phase future leaking into the loop's own (`Send`) inference.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub(crate) type DriveStream<'a> =
    Pin<Box<dyn Stream<Item = Result<PhaseEvent, StreamingError>> + Send + 'a>>;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) type DriveStream<'a> =
    Pin<Box<dyn Stream<Item = Result<PhaseEvent, StreamingError>> + 'a>>;

/// What a phase stream yields: intermediate items as they happen, then
/// exactly one settled value as its final event.
pub(crate) enum PhaseEvent {
    /// An intermediate stream item (assistant delta, tool call/result, a
    /// per-call `CompletionCall`).
    Item(MultiTurnStreamItem),
    /// The model turn completed (the streaming surface has already
    /// yielded every delta of it).
    ModelTurn(Box<ModelTurn>),
    /// The tool batch settled: its results (in call order) and, when a
    /// `ToolResult` hook requested it, the don't-continue reason.
    ToolResults {
        results: Vec<UserContent>,
        stop: Option<String>,
    },
}

/// One item the loop emits to its consumer.
///
/// `Item`s are forwarded to a streaming consumer (and ignored by the
/// blocking fold); `Done` carries both the canonical [`PromptResponse`]
/// the blocking surface returns and the medium-specific final stream
/// item the streaming surface yields.
// The large `Item` variant is the per-delta hot path (one per streamed token);
// boxing it to shrink the variant spread would add an allocation per delta,
// which the streaming path is specifically tuned to avoid. `Done` is yielded
// once per run, so the wasted space on that rare variant is irrelevant.
#[allow(clippy::large_enum_variant)]
pub(crate) enum DriveItem {
    /// An intermediate stream item.
    Item(MultiTurnStreamItem),
    /// The run finished; carries the canonical response the blocking fold
    /// returns. The streaming surface has already received the final item
    /// as the preceding `Item` and ignores this.
    Done(Box<PromptResponse>),
}

/// The per-medium half of the loop: how a turn is fetched from the model,
/// how its tools are executed, and how the run's spans/usage/final item
/// are shaped. The medium-independent loop (the drain, the decision,
/// request preparation, memory) lives once in [`drive_agent`]; only the
/// genuinely divergent pieces are behind this trait. Invalid-tool-call
/// recovery is one of them — it lives inside each source's
/// `run_model_turn` (end-of-turn for blocking, mid-stream for
/// streaming), not in the loop.
pub(crate) trait TurnSource: WasmCompatSend {
    /// Build this medium's per-turn `chat` span (name + parenting + any
    /// `follows_from` chaining differ between blocking and streaming).
    fn open_chat_span(
        &self,
        runner: &AgentRunner,
        effective_preamble: Option<&str>,
    ) -> tracing::Span;

    /// Run one model turn: issue the provider call, record its
    /// completion call into the ledger, yield every intermediate item,
    /// and settle with the completed [`ModelTurn`]. Yielding an `Err`
    /// terminates the turn (the loop classifies it).
    #[allow(clippy::too_many_arguments)]
    fn run_model_turn<'a>(
        &'a mut self,
        runner: &'a AgentRunner,
        hook_ctx: &'a HookContext,
        ledger: &'a mut RunLedger,
        prepared: PreparedCompletionRequest,
        chat_span: tracing::Span,
        agent_span: &'a tracing::Span,
        prompt: Message,
    ) -> DriveStream<'a>;

    /// Execute a turn's tool calls, yielding intermediate items and
    /// settling with the batch's results plus any post-batch stop.
    fn run_tool_calls<'a>(
        &'a self,
        runner: &'a AgentRunner,
        hook_ctx: &'a HookContext,
        calls: Vec<PendingToolCall>,
        tool_snapshot: Arc<ToolRegistrySnapshot>,
    ) -> DriveStream<'a>;

    /// Record run-level telemetry onto the agent span at `Done`. Gated on
    /// `created_agent_span` so a caller-supplied outer span is never polluted.
    fn record_run_level_telemetry(
        &self,
        agent_span: &tracing::Span,
        response: &PromptResponse,
        created_agent_span: bool,
    );

    /// Build the final stream item surfaced at `Done`, or `None` when the
    /// surface discards it (the blocking fold) so the loop skips the work.
    fn final_item(&self, response: &PromptResponse) -> Option<MultiTurnStreamItem>;
}

/// Convert a [`StreamingError`] back into a [`PromptError`] for the
/// blocking surface, which folds the loop. Lossless: every streaming
/// error originates as one of these.
pub(crate) fn streaming_error_into_prompt(err: StreamingError) -> PromptError {
    match err {
        StreamingError::Completion(err) => PromptError::CompletionError(err),
        StreamingError::Prompt(err) => *err,
    }
}

pub(crate) fn store_error_usage(runner: &AgentRunner, ledger: &RunLedger) {
    if let Some(usage) = &runner.error_usage {
        *usage.lock().unwrap_or_else(|error| error.into_inner()) = ledger.usage();
    }
}

/// How a failed turn classifies — the loop owns error observation and
/// classification (ENGINE.md's taxonomy).
enum TurnFailure {
    /// A model-side defect: the model emitted a tool call whose arguments
    /// cannot be parsed. The turn is discarded and retried, bounded.
    Defect(String),
    /// A provider/transport failure, retryable or terminal.
    Provider(ProviderErrorClass),
}

fn classify_turn_failure(err: &StreamingError) -> TurnFailure {
    match err {
        StreamingError::Completion(CompletionError::MalformedToolCall { tool, reason }) => {
            TurnFailure::Defect(format!("`{tool}`: {reason}"))
        }
        StreamingError::Completion(other) => TurnFailure::Provider(classify_provider_error(other)),
        StreamingError::Prompt(boxed) => match &**boxed {
            PromptError::CompletionError(inner) => {
                TurnFailure::Provider(classify_provider_error(inner))
            }
            _ => TurnFailure::Provider(ProviderErrorClass::Terminal),
        },
    }
}

/// Classify a provider failure per ENGINE.md's taxonomy. Deliberately
/// coarse — a simple, documented judgment, not provider forensics:
/// rate limits (HTTP 429, or a rate-limit-shaped message) retry; everything
/// else is terminal. Widening this is a one-function change.
fn classify_provider_error(err: &CompletionError) -> ProviderErrorClass {
    if err
        .provider_response_status()
        .is_some_and(|status| status.as_u16() == 429)
        || err.to_string().to_lowercase().contains("rate limit")
    {
        ProviderErrorClass::Retryable
    } else {
        ProviderErrorClass::Terminal
    }
}

/// The run: one coroutine over the conversation. See the module docs
/// and ENGINE.md (layer 2) — the loop IS that design.
// `clippy::panic`: one deliberate internal-invariant crash (the empty
// conversation is unrepresentable — a run starts only with a
// non-empty conversation). AGENTS.md's error doctrine sanctions it.
#[allow(clippy::panic, clippy::too_many_lines)]
pub(crate) fn drive_agent<'a, S>(
    runner: AgentRunner,
    mut source: S,
    conversation: &'a mut ContextManager,
    agent_span: tracing::Span,
    created_agent_span: bool,
    memory_handle: Option<(Arc<dyn rig_core::memory::ConversationMemory>, String)>,
    is_streaming: bool,
) -> impl Stream<Item = Result<DriveItem, StreamingError>> + 'a
where
    S: TurnSource + 'a,
{
    async_stream::stream! {
        // Run-scoped hook context: minted once, shared by every hook event on
        // both surfaces. `is_streaming` records which surface is driving; the
        // per-turn index is advanced at each PREPARE below.
        let hook_ctx = HookContext::new(
            is_streaming,
            runner.agent_name.clone(),
            runner.tool_context.clone(),
        );
        // The run's completion-call ledger: one entry per issued provider
        // call, usage aggregated as it is learned (mid-stream for the
        // streaming surface).
        let mut ledger = RunLedger::default();
        // Set only after a model turn commits successfully and consumed by
        // its immediately following tool phase. This pins tool execution to
        // the definitions sent that turn.
        let mut pending_tool_snapshot: Option<Arc<ToolRegistrySnapshot>> = None;
        // Live routing state: the model behind the preceding *issued*
        // attempt. It advances immediately before the selected model's
        // operation is invoked, so a preparation failure leaves it
        // unchanged while a provider error still counts.
        let mut previous_model: Option<ModelHandle> = None;
        // A provider failure's original error shape is restored at the exit.
        let mut provider_failure = false;

        // The loop locals (ENGINE.md: the whole of the run state).
        let mut defect_streak = 0usize;
        let mut provider_streak = 0usize;
        let mut pending_error: Option<PromptError> = None;
        let mut pending_error_terminal = false;
        let mut terminating: Option<String> = None;
        let mut turns_used = 0usize; // committed turns only
        let mut current_turn = 0usize; // issued model calls (announced ids)
        let entry_len = conversation.messages().len();

        'outer: loop {
            // ── CONVERGE ────────────────────────────────────────────
            // The stop check precedes the drain: the turn that set the
            // flag finished and committed naturally, and a stop never
            // drains (the stop-semantics ruling — the queue discard with
            // notice is the session actor's, at the stopped terminal).
            if let Some(reason) = terminating.take() {
                store_error_usage(&runner, &ledger);
                yield Err(Box::new(
                    PromptError::prompt_cancelled(conversation.messages().to_vec(), reason),
                )
                .into());
                break 'outer;
            }
            // THE drain — unconditional for every non-stop outcome: the
            // loop shape makes a bypass unrepresentable (every SETTLE
            // branch falls through to here).
            let steers = runner
                .steering
                .as_ref()
                .map(|steering| steering.drain())
                .unwrap_or_default();
            for text in steers.iter().filter_map(Message::user_text) {
                yield Ok(DriveItem::Item(MultiTurnStreamItem::Steer { text }));
            }
            if !steers.is_empty() {
                for message in steers {
                    conversation.fold(message);
                }
                // A steering user is their own circuit breaker.
                defect_streak = 0;
                provider_streak = 0;
            }

            // ── DECIDE ──────────────────────────────────────────────
            // The one policy site. The first pass cannot exit: nothing is
            // set and max_turns >= 1 — at-least-one-turn by construction.
            if pending_error_terminal {
                store_error_usage(&runner, &ledger);
                let err = pending_error
                    .take()
                    .expect("a terminal classification carries its error");
                yield Err(if provider_failure {
                    match err {
                        crate::completion::PromptError::CompletionError(completion) => {
                            StreamingError::Completion(completion)
                        }
                        other => Box::new(other).into(),
                    }
                } else {
                    Box::new(err).into()
                });
                break 'outer;
            }
            if defect_streak > crate::agent::run::TURN_RETRY_CAP {
                store_error_usage(&runner, &ledger);
                yield Err(Box::new(
                    PromptError::prompt_cancelled(
                        conversation.messages().to_vec(),
                        "model-side defect streak exhausted: the model kept emitting \
                         malformed tool calls",
                    ),
                )
                .into());
                break 'outer;
            }
            if provider_streak > crate::agent::run::TURN_RETRY_CAP {
                store_error_usage(&runner, &ledger);
                yield Err(Box::new(
                    PromptError::prompt_cancelled(
                        conversation.messages().to_vec(),
                        "provider retry streak exhausted",
                    ),
                )
                .into());
                break 'outer;
            }
            if turns_used >= runner.max_turns {
                store_error_usage(&runner, &ledger);
                yield Err(Box::new(
                    PromptError::prompt_cancelled(
                        conversation.messages().to_vec(),
                        format!(
                            "agent reached the maximum turn count ({})",
                            runner.max_turns
                        ),
                    ),
                )
                .into());
                break 'outer;
            }

            // ── PREPARE ─────────────────────────────────────────────
            let history = conversation.messages();
            // The message being answered — a derived view; the conversation
            // is never empty here (a run starts only on a non-empty one,
            // and the loop only folds into it).
            let prompt = match history.last() {
                Some(message) => message.clone(),
                None => panic!(
                    "drive: the conversation is empty at PREPARE — a run starts \
                     only on a non-empty conversation"
                ),
            };
            current_turn += 1;
            if runner.max_turns > 1 {
                tracing::info!(
                    "Current conversation Turns: {}/{}",
                    current_turn,
                    runner.max_turns
                );
            }
            hook_ctx.set_turn(current_turn);

            // Completion-call hooks: their merged `RequestPatch` shapes the
            // request (observe-and-patch; there is no stop action — the
            // veto deletion's ruling extends here, ENGINE.md hooks).
            let request_patch =
                resolve_completion_call(&runner.hooks, &hook_ctx, &prompt, &history, current_turn)
                    .await;

            // Resolve routing once at the model-call boundary.
            let selected_model = match runner.hooks.on_model_select(
                &hook_ctx,
                ModelSelection {
                    prompt: &prompt,
                    history: &history,
                    request_patch: request_patch.as_ref(),
                    previous_model: previous_model.as_ref(),
                    default_model: &runner.model,
                    selected_model: &runner.model,
                },
            ) {
                ModelSelectionAction::Continue => runner.model.clone(),
                ModelSelectionAction::Select(model) => model,
            };

            // This turn's base system prompt — the patched-or-baseline
            // preamble.
            let effective_preamble = request_patch
                .as_ref()
                .and_then(|o| o.preamble.as_deref())
                .or(runner.preamble.as_deref());

            let chat_span = source.open_chat_span(&runner, effective_preamble);

            let mut prepared = match build_prepared_completion_request(
                &selected_model,
                &history,
                runner.preamble.as_deref(),
                &runner.static_context,
                runner.temperature,
                runner.max_tokens,
                runner.additional_params.as_ref(),
                runner.record_telemetry_content,
                runner.tool_choice.as_ref(),
                &runner.tool_server_handle,
                runner.output_schema.as_ref(),
                request_patch.as_ref(),
            )
            .await
            {
                Ok(prepared) => prepared,
                Err(err) => {
                    // Request construction can fail on user content (an
                    // attachment a provider cannot carry) — external input,
                    // so it fails gracefully as a terminal error.
                    pending_error = Some(err.into());
                    pending_error_terminal = true;
                    continue 'outer;
                }
            };
            let turn_tool_snapshot = prepared.tool_snapshot.clone();
            if runner.record_telemetry_content {
                let input_messages = prepared.builder.messages_for_telemetry();
                rig_core::telemetry::record_model_input(&chat_span, &input_messages, true);
                prepared.builder = prepared.builder.record_content_telemetry(false);
            }

            // The attempt is now committed: advance `previous_model`
            // immediately before the model turn is driven. An issued
            // attempt counts even when the provider returns an error.
            previous_model = Some(selected_model);

            // Announce the turn (ENGINE.md, delta 10): the attempt is
            // irreversible, so this is "the model call begins". Mint the
            // id, publish it to hooks for the rest of the attempt, and
            // emit the announcement before any content of the attempt.
            let turn_id = (runner.turn_id_source)();
            hook_ctx.set_turn_id(turn_id.clone());
            yield Ok(DriveItem::Item(MultiTurnStreamItem::TurnStarted {
                id: turn_id,
            }));

            // ── MODEL ───────────────────────────────────────────────
            // `cancel` races every await inside the phase (the outer
            // layer drops the run future on abort); nothing below runs
            // after that, and the conversation stands at a roundtrip
            // boundary.
            let mut turn_stream = source.run_model_turn(
                &runner,
                &hook_ctx,
                &mut ledger,
                prepared,
                chat_span,
                &agent_span,
                prompt,
            );
            let mut completed: Option<Box<ModelTurn>> = None;
            let mut turn_error = None;
            let mut turn_protocol_fault: Option<&'static str> = None;
            while let Some(item) = turn_stream.next().await {
                match item {
                    Ok(PhaseEvent::Item(item)) => yield Ok(DriveItem::Item(item)),
                    Ok(PhaseEvent::ModelTurn(turn)) => completed = Some(turn),
                    Ok(PhaseEvent::ToolResults { .. }) => {
                        // A model turn never settles with tool results.
                        turn_protocol_fault =
                            Some("model turn settled with tool results");
                        break;
                    }
                    Err(err) => {
                        turn_error = Some(err);
                        break;
                    }
                }
            }
            drop(turn_stream);
            if let Some(fault) = turn_protocol_fault {
                store_error_usage(&runner, &ledger);
                yield Err(StreamingError::Completion(
                    CompletionError::ResponseError(fault.to_string()),
                ));
                break 'outer;
            }
            if let Some(err) = turn_error {
                // Classify; the loop routes through the convergence (the
                // drain rides along, the decision rules).
                match classify_turn_failure(&err) {
                    TurnFailure::Defect(_reason) => {
                        tracing::warn!(
                            turn = current_turn,
                            "model turn carried a malformed tool call; discarding \
                             the turn and retrying the request"
                        );
                        // The discard is surfaced (ENGINE.md, delta 13):
                        // consumers rewind the provisional output. The
                        // turn was a local — nothing to un-fold.
                        yield Ok(DriveItem::Item(MultiTurnStreamItem::ModelTurnRetried {
                            turn: current_turn,
                        }));
                        defect_streak += 1;
                    }
                    TurnFailure::Provider(class) => {
                        if matches!(class, ProviderErrorClass::Retryable) {
                            tracing::warn!(
                                turn = current_turn,
                                "retryable provider error; draining and retrying the request"
                            );
                            provider_streak += 1;
                        } else {
                            pending_error_terminal = true;
                        }
                        pending_error = Some(streaming_error_into_prompt(err));
                        provider_failure = true;
                    }
                }
                continue 'outer;
            }
            let turn = match completed {
                Some(turn) => turn,
                None => {
                    store_error_usage(&runner, &ledger);
                    yield Err(StreamingError::Completion(CompletionError::ResponseError(
                        "model turn ended without settling a turn".to_string(),
                    )));
                    break 'outer;
                }
            };

            // ── SETTLE ──────────────────────────────────────────────
            // Hooks observe the completed turn; there is no verdict
            // (ENGINE.md: the veto is deleted).
            AgentHook::on_model_turn_finished(
                &runner.hooks,
                &hook_ctx,
                crate::agent::hook::ModelTurnFinished {
                    turn: current_turn,
                    content: &turn.choice,
                    usage: turn.usage,
                },
            )
            .await;

            // A completed turn resets the failure streaks (a committed
            // model call counts; the parked machine did this at park).
            defect_streak = 0;
            provider_streak = 0;
            pending_error = None;
            pending_error_terminal = false;

            if !turn.carries_tools() {
                // FINAL — fold (empty finals fold nothing: one decision
                // site, ENGINE.md implementation judgments), commit, exit.
                if !crate::agent::prompt_request::is_empty_assistant_turn(&turn.choice) {
                    conversation.fold(Message::Assistant {
                        id: turn.message_id.clone(),
                        content: turn.choice.clone(),
                    });
                }
                if let Some(id) = hook_ctx.turn_id() {
                    yield Ok(DriveItem::Item(MultiTurnStreamItem::TurnCommitted {
                        id: id.clone(),
                    }));
                }
                turns_used += 1;

                let run_messages = conversation.messages()[entry_len.min(
                    conversation.messages().len(),
                )..]
                    .to_vec();
                let response = PromptResponse::new(
                    crate::agent::prompt_request::assistant_text_from_choice(&turn.choice),
                    ledger.usage(),
                )
                .with_messages(run_messages)
                .with_completion_calls(ledger.calls().to_vec())
                .with_content(turn.choice.clone());
                tracing::info!(
                    turn = current_turn,
                    max_turns = runner.max_turns,
                    "Agent run finished"
                );
                source.record_run_level_telemetry(&agent_span, &response, created_agent_span);
                append_run_messages(
                    memory_handle.as_ref(),
                    response.messages.as_deref().unwrap_or_default(),
                )
                .await;
                if let Some(final_item) = source.final_item(&response) {
                    yield Ok(DriveItem::Item(final_item));
                }
                yield Ok(DriveItem::Done(Box::new(response)));
                break 'outer;
            }

            // TOOLS — the turn stays a local until its results exist;
            // admission first (unknown names become in-band results).
            let calls = crate::agent::run::admit(&turn);
            let tool_snapshot = match pending_tool_snapshot.take() {
                Some(snapshot) => snapshot,
                None => turn_tool_snapshot,
            };
            let mut tool_stream =
                source.run_tool_calls(&runner, &hook_ctx, calls, tool_snapshot);
            let mut settled: Option<(Vec<UserContent>, Option<String>)> = None;
            let mut tool_error = None;
            let mut tool_protocol_fault: Option<&'static str> = None;
            while let Some(item) = tool_stream.next().await {
                match item {
                    Ok(PhaseEvent::Item(item)) => yield Ok(DriveItem::Item(item)),
                    Ok(PhaseEvent::ModelTurn(_)) => {
                        tool_protocol_fault = Some("tool batch settled with a model turn");
                        break;
                    }
                    Ok(PhaseEvent::ToolResults { results, stop }) => {
                        settled = Some((results, stop));
                    }
                    Err(err) => {
                        tool_error = Some(err);
                        break;
                    }
                }
            }
            drop(tool_stream);
            if let Some(fault) = tool_protocol_fault {
                store_error_usage(&runner, &ledger);
                yield Err(StreamingError::Completion(
                    CompletionError::ResponseError(fault.to_string()),
                ));
                break 'outer;
            }
            if let Some(err) = tool_error {
                match classify_turn_failure(&err) {
                    TurnFailure::Defect(_reason) => {
                        yield Ok(DriveItem::Item(MultiTurnStreamItem::ModelTurnRetried {
                            turn: current_turn,
                        }));
                        defect_streak += 1;
                    }
                    TurnFailure::Provider(class) => {
                        if matches!(class, ProviderErrorClass::Retryable) {
                            provider_streak += 1;
                        } else {
                            pending_error_terminal = true;
                        }
                        pending_error = Some(streaming_error_into_prompt(err));
                        provider_failure = true;
                    }
                }
                continue 'outer;
            }
            let (results, batch_stop) = match settled {
                Some(settled) => settled,
                None => {
                    store_error_usage(&runner, &ledger);
                    yield Err(StreamingError::Completion(CompletionError::ResponseError(
                        "tool batch ended without settling results".to_string(),
                    )));
                    break 'outer;
                }
            };

            // The roundtrip commits whole: the assistant and its complete
            // batch, verified and enqueued as one unit (ENGINE.md, the
            // durable conversation). Results are non-empty for a tools
            // turn; the construction cannot fail.
            let results_content = match OneOrMany::from_iter_optional(results) {
                Some(content) => content,
                None => {
                    store_error_usage(&runner, &ledger);
                    yield Err(StreamingError::Completion(CompletionError::ResponseError(
                        "internal invariant violated: a tools turn settled with no results"
                            .to_string(),
                    )));
                    break 'outer;
                }
            };
            conversation.fold_all(vec![
                Message::Assistant {
                    id: turn.message_id.clone(),
                    content: turn.choice.clone(),
                },
                Message::User {
                    content: results_content,
                },
            ]);
            if let Some(id) = hook_ctx.turn_id() {
                yield Ok(DriveItem::Item(MultiTurnStreamItem::TurnCommitted {
                    id: id.clone(),
                }));
            }
            turns_used += 1;
            // The batch is committed; now — and only now — the loop
            // learns a hook's run-stop decision (flag-blind by
            // construction, ENGINE.md stop taxonomy).
            if let Some(reason) = batch_stop {
                terminating = Some(reason);
            }
            continue 'outer;
        }
    }
}

/// Execute a turn's tool calls **atomically per batch**, shared by both surfaces.
///
/// The batch is a sealed unit once launched (ENGINE.md, tool phase):
///
/// - The model tool-call events ([`StreamedAssistantContent::ToolCall`]) are
///   emitted up front — they report what the model emitted at turn commit.
/// - Every chain (gate → body → post) then runs — sequentially at
///   `tool_concurrency <= 1`, else concurrently bounded by it — and nothing
///   can stop the batch: settlement is unconditional, so no sibling's
///   decision or failure can strand a parked chain.
/// - A `ToolResult` hook's `Stop(reason)` does **not** kill anything: the
///   chain's result commits with the rest, and the lowest call-index reason
///   settles with the batch — the loop sets its `terminating` flag only
///   after `fold_all`, so the tool phase is flag-blind by construction.
/// - When the whole batch settles, the per-tool
///   [`ToolExecutionCommitted`](MultiTurnStreamItem::ToolExecutionCommitted) + result
///   items are surfaced (in call order, only for tools whose body actually ran);
///   the results settle to the loop, which commits the roundtrip.
///
/// When `forward_items` is `false` (the blocking fold) no stream items are built,
/// but the collect behavior is identical, so `run()` and `stream()`
/// return the same terminal reason. `chain_tool_span` lets the blocking
/// surface chain spans into its linear `follows_from` sequence.
pub(crate) fn drive_tool_calls<'a, F>(
    runner: &'a AgentRunner,
    hook_ctx: &'a HookContext,
    calls: Vec<PendingToolCall>,
    tool_snapshot: Arc<ToolRegistrySnapshot>,
    chain_tool_span: F,
    forward_items: bool,
) -> DriveStream<'a>
where
    F: Fn(tracing::Span) -> tracing::Span + WasmCompatSend + 'a,
{
    // Per-call working state: a stable internal_call_id and the execute span,
    // paired with the model's tool call. `span` is `Span::none()` for a
    // preresolved (invalid-recovery) call, which never executes.
    struct PreparedToolCall {
        tool_call: rig_core::message::ToolCall,
        preresolved_result: Option<UserContent>,
        internal_call_id: String,
        span: tracing::Span,
    }
    // How a settled tool call is surfaced on the stream once the batch succeeds:
    //   - `Executed`: `ToolExecutionCommitted` (with the effective, hook-rewritten
    //     call) + the `ToolResult`.
    //   - `Skipped`: the `ToolResult` only (a `ToolCall` hook returned `Skip`, so
    //     nothing ran — no execution commit — but the model still sees the result).
    //   - `Preresolved`: neither (an invalid-recovery result, already surfaced
    //     during the model turn); settles to the loop only.
    enum ToolSurface {
        // Boxed to keep this enum small next to the empty `Skipped`/`Preresolved`.
        Executed(Box<rig_core::message::ToolCall>),
        Skipped,
        Preresolved,
    }
    // A collected tool outcome, held (not surfaced or settled) until the whole
    // batch settles.
    struct CollectedToolResult {
        content: UserContent,
        internal_call_id: String,
        surface: ToolSurface,
    }

    Box::pin(async_stream::stream! {
        let call_count = calls.len();

        // Assign each call a stable internal_call_id and, for calls that will
        // actually execute, an execute span. Emit the MODEL tool-call events now,
        // right after the turn committed: these report what the model emitted and
        // are *not* execution-lifecycle events. A preresolved call emits no model
        // tool-call event (its synthetic result was already surfaced during the
        // model turn) and gets no execute span.
        let mut prepared: Vec<PreparedToolCall> = Vec::with_capacity(call_count);
        for pending in calls {
            let internal_call_id = pending.internal_call_id.unwrap_or_else(rig_core::id::generate);
            let (span, preresolved_result) = match pending.preresolved_result {
                Some(result) => (tracing::Span::none(), Some(result)),
                None => {
                    if forward_items {
                        yield Ok(PhaseEvent::Item(MultiTurnStreamItem::stream_item(
                            StreamedAssistantContent::ToolCall {
                                tool_call: pending.tool_call.clone(),
                                internal_call_id: internal_call_id.clone(),
                            },
                        )));
                    }
                    (chain_tool_span(new_execute_tool_span()), None)
                }
            };
            prepared.push(PreparedToolCall {
                tool_call: pending.tool_call,
                preresolved_result,
                internal_call_id,
                span,
            });
        }

        // Run all chains, collecting outcomes in call order. Settlement is
        // unconditional (ENGINE.md, stop taxonomy): nothing fails the batch.
        // A chain's `stop_reason` records a post-batch run-stop decision the
        // loop learns only at settle; when several fire, the lowest call
        // index wins (deterministic, like the results themselves).
        let mut collected: Vec<Option<CollectedToolResult>> =
            (0..call_count).map(|_| None).collect();
        let mut batch_stop: Option<(usize, String)> = None;

        if runner.concurrency <= 1 {
            // Sequential: chains run in call order.
            for (index, call) in prepared.into_iter().enumerate() {
                let PreparedToolCall { tool_call, preresolved_result, internal_call_id, span } = call;
                if let Some(result) = preresolved_result {
                    if let Some(slot) = collected.get_mut(index) {
                        *slot = Some(CollectedToolResult {
                            content: result,
                            internal_call_id,
                            surface: ToolSurface::Preresolved,
                        });
                    }
                    continue;
                }
                let outcome = run_single_tool(
                    runner,
                    hook_ctx,
                    &tool_snapshot,
                    &tool_call,
                    &internal_call_id,
                )
                .instrument(span)
                .await;
                if let Some(reason) = outcome.stop_reason
                    && batch_stop.as_ref().is_none_or(|(i, _)| index < *i)
                {
                    batch_stop = Some((index, reason));
                }
                let surface = match outcome.execution {
                    ToolExecution::Executed(effective) => ToolSurface::Executed(effective),
                    ToolExecution::Skipped => ToolSurface::Skipped,
                };
                if let Some(slot) = collected.get_mut(index) {
                    *slot = Some(CollectedToolResult {
                        content: outcome.content,
                        internal_call_id,
                        surface,
                    });
                }
            }
        } else {
            // Concurrent: chains bounded by `tool_concurrency`, completing in
            // arbitrary order; results still settle in call order.
            let unordered = stream::iter(prepared.into_iter().enumerate())
                .map(|(index, call)| {
                    let PreparedToolCall { tool_call, preresolved_result, internal_call_id, span } = call;
                    let tool_snapshot = &tool_snapshot;
                    async move {
                        if let Some(result) = preresolved_result {
                            return (
                                index,
                                Some(CollectedToolResult {
                                    content: result,
                                    internal_call_id,
                                    surface: ToolSurface::Preresolved,
                                }),
                                None,
                            );
                        }
                        let outcome = run_single_tool(
                            runner,
                            hook_ctx,
                            tool_snapshot,
                            &tool_call,
                            &internal_call_id,
                        )
                        .await;
                        let surface = match outcome.execution {
                            ToolExecution::Executed(effective) => ToolSurface::Executed(effective),
                            ToolExecution::Skipped => ToolSurface::Skipped,
                        };
                        (
                            index,
                            Some(CollectedToolResult {
                                content: outcome.content,
                                internal_call_id,
                                surface,
                            }),
                            outcome.stop_reason.map(|reason| (index, reason)),
                        )
                    }
                    .instrument(span)
                })
                .buffer_unordered(runner.concurrency);
            futures::pin_mut!(unordered);

            while let Some((index, collected_result, stop)) = unordered.next().await {
                if let Some(slot) = collected.get_mut(index) {
                    *slot = collected_result;
                }
                if let Some(stop) = stop
                    && batch_stop.as_ref().is_none_or(|(i, _)| index < *i)
                {
                    batch_stop = Some(stop);
                }
            }
        }

        // Settle: surface the batch's items, then hand the loop the results
        // (in call order) and the collected run-stop decision (if any) —
        // the flag reaches the loop only after it commits the roundtrip,
        // so the tool phase is flag-blind by construction. Every slot is
        // filled: settlement is unconditional and every chain returns an
        // outcome.
        let mut settled: Vec<UserContent> = Vec::with_capacity(call_count);
        let mut surface_items: Vec<MultiTurnStreamItem> =
            Vec::with_capacity(call_count.saturating_mul(2));
        for slot in collected {
            let CollectedToolResult { content, internal_call_id, surface } = match slot {
                Some(collected_result) => collected_result,
                None => {
                    yield Err(StreamingError::Prompt(Box::new(PromptError::CompletionError(
                        CompletionError::ResponseError(
                            "tool execution finished without producing every result".to_string(),
                        ),
                    ))));
                    return;
                }
            };
            if forward_items {
                // An executed call also surfaces its execution commit; a skipped
                // call surfaces only its result; a preresolved call surfaces
                // nothing here.
                let surface_result = match surface {
                    ToolSurface::Executed(tool_call) => {
                        surface_items.push(MultiTurnStreamItem::ToolExecutionCommitted {
                            tool_call: *tool_call,
                            internal_call_id: internal_call_id.clone(),
                        });
                        true
                    }
                    ToolSurface::Skipped => true,
                    ToolSurface::Preresolved => false,
                };
                if surface_result
                    && let UserContent::ToolResult(tool_result) = &content
                {
                    surface_items.push(MultiTurnStreamItem::StreamUserItem(
                        StreamedUserContent::ToolResult {
                            tool_result: tool_result.clone(),
                            internal_call_id,
                        },
                    ));
                }
            }
            settled.push(content);
        }

        for item in surface_items {
            yield Ok(PhaseEvent::Item(item));
        }
        yield Ok(PhaseEvent::ToolResults {
            results: settled,
            stop: batch_stop.map(|(_, reason)| reason),
        });
    })
}
