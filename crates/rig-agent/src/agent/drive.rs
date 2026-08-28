//! The medium-independent agent drive loop shared by the blocking
//! ([`AgentRunner::run`](crate::agent::runner::AgentRunner::run)) and streaming
//! ([`StreamingPromptRequest`](crate::agent::prompt_request::streaming::StreamingPromptRequest))
//! surfaces.
//!
//! `drive_agent` owns the outer loop — turn counting, the `CompletionCall`
//! hook, request preparation, memory persistence — and delegates the
//! medium-specific model call, tool execution, span shaping and finalization
//! to a [`TurnSource`]. `drive_tool_calls` executes a turn's tool batch
//! atomically on behalf of both surfaces. The per-medium `TurnSource`
//! implementations live next to their surfaces: [`UnaryTurnSource`] in
//! [`runner`](super::runner) and `StreamingTurnSource` in
//! [`prompt_request::streaming`](super::prompt_request::streaming).

use std::{pin::Pin, sync::Arc};

use futures::{Stream, StreamExt, stream};
use rig_core::{message::UserContent, wasm_compat::WasmCompatSend};
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
        run::{AgentRun, AgentRunStep, PendingToolCall, ProviderErrorClass},
        runner::{
            AgentRunner, CompletionCallOutcome, ToolExecution, append_run_messages,
            new_execute_tool_span, resolve_completion_call, run_single_tool,
        },
    },
    completion::{CompletionError, Message, PromptError},
    streaming::{StreamedAssistantContent, StreamedUserContent},
    tool::server::ToolRegistrySnapshot,
};

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

/// A boxed, medium-specific item stream for one engine step (model turn or tool
/// batch). Boxed so a generic [`drive_agent`] can forward it without the
/// per-step future leaking into the engine's own (`Send`) inference.
// Same browser-wasm predicate as `StreamingResult` above, for the same reason.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub(crate) type DriveStream<'a> =
    Pin<Box<dyn Stream<Item = Result<MultiTurnStreamItem, StreamingError>> + Send + 'a>>;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) type DriveStream<'a> =
    Pin<Box<dyn Stream<Item = Result<MultiTurnStreamItem, StreamingError>> + 'a>>;

/// One item emitted by the shared engine [`drive_agent`].
///
/// `Item`s are forwarded to a streaming consumer (and ignored by the blocking
/// fold); `Done` carries both the canonical [`PromptResponse`] the blocking
/// surface returns and the medium-specific final stream item the streaming
/// surface yields.
// The large `Item` variant is the per-delta hot path (one per streamed token);
// boxing it to shrink the variant spread would add an allocation per delta,
// which the streaming path is specifically tuned to avoid. `Done` is yielded
// once per run, so the wasted space on that rare variant is irrelevant.
#[allow(clippy::large_enum_variant)]
pub(crate) enum DriveItem {
    /// An intermediate stream item (assistant delta, tool call/result, a
    /// per-call `CompletionCall`, or — last, for the streaming surface — the
    /// final response item).
    Item(MultiTurnStreamItem),
    /// The run finished; carries the canonical response the blocking fold
    /// returns. The streaming surface has already received the final item as the
    /// preceding `Item` and ignores this.
    Done(Box<PromptResponse>),
}

/// The per-medium half of the agent loop: how a turn is fetched from the model,
/// how its tools are executed, and how the run's spans/usage/final item are
/// shaped. The medium-independent outer loop (turn counting, the `CompletionCall`
/// hook, request preparation, memory) lives once in [`drive_agent`]; only the
/// genuinely divergent pieces are behind this trait. Invalid-tool-call recovery
/// is one of them — it lives inside each source's `run_model_turn` (end-of-turn
/// for blocking, mid-stream for streaming), not in `drive_agent`.
pub(crate) trait TurnSource: WasmCompatSend {
    /// Build this medium's per-turn `chat` span (name + parenting + any
    /// `follows_from` chaining differ between blocking and streaming).
    fn open_chat_span(
        &self,
        runner: &AgentRunner,
        effective_preamble: Option<&str>,
    ) -> tracing::Span;

    /// Run one model turn: issue the provider call, feed the result into the
    /// sans-IO machine, and yield any intermediate items. Returning normally
    /// advances the loop; yielding an `Err` terminates the run.
    #[allow(clippy::too_many_arguments)]
    fn run_model_turn<'a>(
        &'a mut self,
        runner: &'a AgentRunner,
        hook_ctx: &'a HookContext,
        run: &'a mut AgentRun,
        prepared: PreparedCompletionRequest,
        chat_span: tracing::Span,
        agent_span: &'a tracing::Span,
        prompt: Message,
    ) -> DriveStream<'a>;

    /// Execute a turn's tool calls, feeding the results into the machine and
    /// yielding any intermediate items.
    fn run_tool_calls<'a>(
        &'a self,
        runner: &'a AgentRunner,
        hook_ctx: &'a HookContext,
        run: &'a mut AgentRun,
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
    /// surface discards it (the blocking fold) so the engine skips the work.
    fn final_item(&self, response: &PromptResponse) -> Option<MultiTurnStreamItem>;
}

/// Convert a [`StreamingError`] back into a [`PromptError`] for the blocking
/// surface ([`AgentRunner::run`]), which folds the shared engine. Lossless:
/// every streaming error originates as one of these.
pub(crate) fn streaming_error_into_prompt(err: StreamingError) -> PromptError {
    match err {
        StreamingError::Completion(err) => PromptError::CompletionError(err),
        StreamingError::Prompt(err) => *err,
    }
}

pub(crate) fn store_error_usage(runner: &AgentRunner, run: &AgentRun) {
    if let Some(usage) = &runner.error_usage {
        *usage.lock().unwrap_or_else(|error| error.into_inner()) = run.usage();
    }
}

/// How a failed turn classifies for the machine's feeds — the driver owns
/// error observation and classification (ENGINE.md's taxonomy); the machine
/// owns the resulting control flow.
enum TurnFailure {
    /// A model-side defect: the model emitted a tool call whose arguments
    /// cannot be parsed. The turn is discarded and retried, bounded.
    Defect(String),
    /// A provider/transport failure, retryable or terminal.
    Provider(ProviderErrorClass),
    /// A hook stopped the run.
    Stop(String),
}

fn classify_turn_failure(err: &StreamingError) -> TurnFailure {
    match err {
        StreamingError::Completion(CompletionError::MalformedToolCall { tool, reason }) => {
            TurnFailure::Defect(format!("`{tool}`: {reason}"))
        }
        StreamingError::Completion(other) => TurnFailure::Provider(classify_provider_error(other)),
        StreamingError::Prompt(boxed) => match &**boxed {
            PromptError::PromptCancelled { reason, .. } => TurnFailure::Stop(reason.clone()),
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

/// The single agent drive loop, shared by the blocking and streaming surfaces.
///
/// Owns the medium-independent loop — `next_step` dispatch, the
/// `CompletionCall` hook + request preparation, the drain, the `Done`
/// memory append — and delegates the medium-specific model call, tool
/// execution, span shaping and finalization to a [`TurnSource`]. The
/// streaming surface forwards the yielded [`DriveItem`]s; the blocking
/// surface folds them to `Done`.
///
/// The loop mirrors ENGINE.md's inner machine: model turn → outcome feed
/// → `DrainSteers` (the one drain) → decision. Turn sources feed
/// *committed* turns to the machine themselves; failures are classified
/// here and fed (`broken` / `provider_error` / `terminate`), so every
/// outcome converges at the drain before anything else happens.
// `clippy::panic`: one deliberate internal-invariant crash (the empty
// history is unrepresentable — the machine panics at construction and only
// appends). AGENTS.md's error doctrine sanctions it.
#[allow(clippy::panic)]
pub(crate) fn drive_agent<S>(
    runner: AgentRunner,
    mut source: S,
    mut run: AgentRun,
    agent_span: tracing::Span,
    created_agent_span: bool,
    memory_handle: Option<(Arc<dyn rig_core::memory::ConversationMemory>, String)>,
    is_streaming: bool,
) -> impl Stream<Item = Result<DriveItem, StreamingError>>
where
    S: TurnSource,
{
    async_stream::stream! {
        // Run-scoped hook context: minted once, shared by every hook event on
        // both surfaces. `is_streaming` records which surface is driving; the
        // per-turn index is advanced on each `CallModel` step below.
        let hook_ctx = HookContext::new(is_streaming, runner.agent_name.clone(), runner.tool_context.clone());
        // Set only after a model turn commits successfully and consumed by its
        // immediately following CallTools step. This keeps the sans-IO run state
        // serializable while pinning execution to the definitions sent that turn.
        let mut pending_tool_snapshot: Option<Arc<ToolRegistrySnapshot>> = None;
        // Live routing state stays in the driver, not the serde `AgentRun`. It
        // records the model behind the preceding *issued* attempt: it advances
        // immediately before the selected model's unary or streaming operation
        // is invoked, so a completion-call stop, selection stop, or preparation
        // failure leaves it unchanged while a provider error still counts.
        let mut previous_model: Option<ModelHandle> = None;
        // A provider failure awaits the machine's decision (through the
        // drain); its original `Completion` shape is restored at the exit.
        let mut provider_failure = false;

        'outer: loop {
            let step = match run.next_step() {
                Ok(step) => step,
                Err(err) => {
                    store_error_usage(&runner, &run);
                    yield Err(Box::new(err).into());
                    break 'outer;
                }
            };

            match step {
                AgentRunStep::CallModel { history, turn } => {
                    drop(pending_tool_snapshot.take());
                    if runner.max_turns > 1 {
                        tracing::info!("Current conversation Turns: {}/{}", turn, runner.max_turns);
                    }
                    hook_ctx.set_turn(turn);
                    // The message being answered — a derived view; the machine
                    // guarantees the history is never empty (ENGINE.md: no
                    // prompt/context split).
                    let prompt = match history.last() {
                        Some(message) => message.clone(),
                        None => panic!(
                            "drive: model-call history is empty — the machine guarantees \
                             at least the message being answered"
                        ),
                    };

                    // Completion-call hooks resolve FIRST: a stop here suppresses
                    // model selection entirely, and their merged `RequestPatch`
                    // is handed to the selection hooks below.
                    let request_patch =
                        match resolve_completion_call(&runner.hooks, &hook_ctx, &prompt, &history, turn).await {
                            CompletionCallOutcome::Terminate(reason) => {
                                // Route the stop through the drain (the machine's
                                // law); the error surfaces from the decision.
                                if let Err(err) = run.terminate(reason) {
                                    store_error_usage(&runner, &run);
                                    yield Err(Box::new(err).into());
                                    break 'outer;
                                }
                                continue 'outer;
                            }
                            CompletionCallOutcome::Proceed(request_patch) => request_patch,
                        };

                    // Resolve routing once at the model-call boundary, after the
                    // completion-call hooks proceed. The resulting handle is
                    // cloned into the prepared attempt, so request preparation
                    // inspects the *selected* model's captured capabilities and
                    // the same handle executes the request.
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
                        ModelSelectionAction::Stop(reason) => {
                            if let Err(err) = run.terminate(reason) {
                                store_error_usage(&runner, &run);
                                yield Err(Box::new(err).into());
                                break 'outer;
                            }
                            continue 'outer;
                        }
                    };

                    // Record this turn's base system prompt — the patched-or-baseline
                    // preamble. Borrow rather than clone since it only needs to
                    // outlive span creation.
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
                            // Request construction can fail on user content
                            // (an attachment a provider cannot carry) — external
                            // input, so it fails gracefully as a terminal error
                            // rather than panicking.
                            if let Err(state_err) = run.provider_error(
                                ProviderErrorClass::Terminal,
                                err.into(),
                            ) {
                                store_error_usage(&runner, &run);
                                yield Err(Box::new(state_err).into());
                                break 'outer;
                            }
                            provider_failure = true;
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
                    // immediately before the model turn is driven (the
                    // streaming request is issued on first poll of the turn
                    // stream). An issued attempt counts even when
                    // the provider returns an error; every stop/error path
                    // above left `previous_model` untouched.
                    previous_model = Some(selected_model);

                    // Announce the turn (ENGINE.md behavior delta 10): the
                    // attempt is irreversible, so this is "the model call
                    // begins". Mint the id, publish it to hooks for the rest
                    // of the attempt, and emit the announcement before any
                    // content of the attempt — consumers learn the turn
                    // started before first-token latency elapses.
                    let turn_id = (runner.turn_id_source)();
                    hook_ctx.set_turn_id(turn_id.clone());
                    yield Ok(DriveItem::Item(MultiTurnStreamItem::TurnStarted {
                        id: turn_id,
                    }));

                    let mut turn_stream = source.run_model_turn(
                        &runner,
                        &hook_ctx,
                        &mut run,
                        prepared,
                        chat_span,
                        &agent_span,
                        prompt,
                    );
                    let mut turn_error = None;
                    while let Some(item) = turn_stream.next().await {
                        match item {
                            Ok(item) => yield Ok(DriveItem::Item(item)),
                            Err(err) => {
                                turn_error = Some(err);
                                break;
                            }
                        }
                    }
                    drop(turn_stream);
                    if let Some(err) = turn_error {
                        // Classify and feed; the loop then routes through the
                        // drain, where steers ride along and the decision
                        // (retry, bounded, or fail) is made by the machine.
                        let class = classify_turn_failure(&err);
                        let fed = match &class {
                            TurnFailure::Defect(reason) => {
                                tracing::warn!(
                                    turn,
                                    "model turn carried a malformed tool call; \
                                     discarding the turn and retrying the request"
                                );
                                // The discard is surfaced (ENGINE.md delta 13):
                                // consumers rewind the provisional output and
                                // bill the attempt, instead of the turn
                                // vanishing silently.
                                yield Ok(DriveItem::Item(MultiTurnStreamItem::ModelTurnRetried {
                                    turn,
                                }));
                                run.broken(reason.clone())
                            }
                            TurnFailure::Provider(class) => {
                                if matches!(class, ProviderErrorClass::Retryable) {
                                    tracing::warn!(
                                        turn,
                                        "retryable provider error; draining and retrying the request"
                                    );
                                }
                                run.provider_error(*class, streaming_error_into_prompt(err))
                            }
                            TurnFailure::Stop(reason) => run.terminate(reason.clone()),
                        };
                        if let Err(state_err) = fed {
                            store_error_usage(&runner, &run);
                            yield Err(Box::new(state_err).into());
                            break 'outer;
                        }
                        provider_failure |= matches!(class, TurnFailure::Provider(_));
                        continue 'outer;
                    }
                    // Clean end: the source fed the committed turn; the
                    // machine decides what surfaces next.
                    pending_tool_snapshot = Some(turn_tool_snapshot);
                }
                AgentRunStep::CallTools { calls } => {
                    let Some(tool_snapshot) = pending_tool_snapshot.take() else {
                        store_error_usage(&runner, &run);
                        yield Err(StreamingError::Completion(CompletionError::ResponseError(
                            "agent requested tool execution without a prepared registry snapshot"
                                .to_string(),
                        )));
                        break 'outer;
                    };
                    let mut tool_stream = source.run_tool_calls(
                        &runner,
                        &hook_ctx,
                        &mut run,
                        calls,
                        tool_snapshot,
                    );
                    let mut tool_error = None;
                    while let Some(item) = tool_stream.next().await {
                        match item {
                            Ok(item) => yield Ok(DriveItem::Item(item)),
                            Err(err) => {
                                tool_error = Some(err);
                                break;
                            }
                        }
                    }
                    drop(tool_stream);
                    if let Some(err) = tool_error {
                        let class = classify_turn_failure(&err);
                        let fed = match &class {
                            TurnFailure::Defect(reason) => run.broken(reason.clone()),
                            TurnFailure::Provider(class) => {
                                run.provider_error(*class, streaming_error_into_prompt(err))
                            }
                            TurnFailure::Stop(reason) => run.terminate(reason.clone()),
                        };
                        if let Err(state_err) = fed {
                            store_error_usage(&runner, &run);
                            yield Err(Box::new(state_err).into());
                            break 'outer;
                        }
                        provider_failure |= matches!(class, TurnFailure::Provider(_));
                        continue 'outer;
                    }
                    // Tool results were fed by the tool source; the loop
                    // routes through the drain next.
                }
                AgentRunStep::DrainSteers => {
                    // THE drain point (the machine's only one): take
                    // everything queued, surface each message, feed the
                    // machine — whose decision then returns the next step,
                    // the failure, or nothing (Done next round).
                    let messages = runner
                        .steering
                        .as_ref()
                        .map(|steering| steering.drain())
                        .unwrap_or_default();
                    for text in messages.iter().filter_map(Message::user_text) {
                        yield Ok(DriveItem::Item(MultiTurnStreamItem::Steer { text }));
                    }
                    if let Err(err) = run.steered(messages) {
                        // The decision failed the run (terminal error,
                        // exhausted retries, budget, or a stop) or the
                        // driver drove out of protocol; either way the run
                        // is over. A provider failure exits with its
                        // original error shape.
                        store_error_usage(&runner, &run);
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
                }
                AgentRunStep::Done(boxed_response) => {
                    // Run-completion marker, unifying the blocking and streaming
                    // drivers' run-finished logs into one shared event.
                    tracing::info!(
                        turn = run.turn(),
                        max_turns = runner.max_turns,
                        "Agent run finished"
                    );
                    let response = *boxed_response;
                    source.record_run_level_telemetry(&agent_span, &response, created_agent_span);
                    append_run_messages(
                        memory_handle.as_ref(),
                        response.messages.as_deref().unwrap_or_default(),
                    )
                    .await;
                    // Build the final item only when the surface forwards it
                    // (streaming). The blocking fold discards it, so its source
                    // returns `None` and the extra full-response clone is skipped.
                    if let Some(final_item) = source.final_item(&response) {
                        yield Ok(DriveItem::Item(final_item));
                    }
                    yield Ok(DriveItem::Done(Box::new(response)));
                    break 'outer;
                }
            }
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
///   is fed to the machine at settle — the run ends `failed(reason)` at the
///   next decision instead of looping.
/// - When the whole batch settles, the per-tool
///   [`ToolExecutionCommitted`](MultiTurnStreamItem::ToolExecutionCommitted) + result
///   items are surfaced (in call order, only for tools whose body actually ran) and
///   the results committed to run history.
///
/// When `forward_items` is `false` (the blocking fold) no stream items are built,
/// but the collect/commit behavior is identical, so `run()` and `stream()`
/// return the same terminal reason. `chain_tool_span` lets the blocking
/// surface chain spans into its linear `follows_from` sequence.
pub(crate) fn drive_tool_calls<'a, F>(
    runner: &'a AgentRunner,
    hook_ctx: &'a HookContext,
    run: &'a mut AgentRun,
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
    //     during the model turn); committed to history only.
    enum ToolSurface {
        // Boxed to keep this enum small next to the empty `Skipped`/`Preresolved`.
        Executed(Box<rig_core::message::ToolCall>),
        Skipped,
        Preresolved,
    }
    // A collected tool outcome, held (not surfaced or committed) until the whole
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
                        yield Ok(MultiTurnStreamItem::stream_item(
                            StreamedAssistantContent::ToolCall {
                                tool_call: pending.tool_call.clone(),
                                internal_call_id: internal_call_id.clone(),
                            },
                        ));
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
        // machine learns only at settle; when several fire, the lowest call
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
            // arbitrary order; results still commit in call order.
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

        // Settle: commit the results, then hand the machine the collected
        // run-stop decision (if any) — the flag exists only after the batch
        // is fully committed, so the tool phase is flag-blind by
        // construction. Every slot is filled: settlement is unconditional
        // and every chain returns an outcome.
        let mut committed: Vec<UserContent> = Vec::with_capacity(call_count);
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
            committed.push(content);
        }

        if let Err(err) = run.tool_results(committed) {
            yield Err(Box::new(err).into());
            return;
        }

        // The roundtrip closed: the assistant turn and its complete batch
        // committed together (ENGINE.md, the durable roundtrip) — the cue a
        // session layer commits its pending roundtrip atomically.
        if forward_items
            && let Some(turn_id) = hook_ctx.turn_id()
        {
            surface_items.push(MultiTurnStreamItem::RoundtripClosed { turn_id });
        }

        // The batch is committed; now — and only now — the machine learns a
        // hook's run-stop decision. The flag cannot affect the batch that
        // produced it: the tool phase was flag-blind by construction
        // (ENGINE.md, stop taxonomy).
        if let Some((_, reason)) = batch_stop
            && let Err(err) = run.terminate(reason)
        {
            yield Err(Box::new(err).into());
            return;
        }

        for item in surface_items {
            yield Ok(item);
        }
    })
}
