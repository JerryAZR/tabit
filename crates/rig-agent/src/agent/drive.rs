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
        run::{AgentRun, AgentRunStep, PendingToolCall},
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

/// Deliver queued steering to the run's history at a turn end. No budget
/// check: appending to history is unconditional — the max-turn budget
/// gates only the next model call, discovered (and reported as
/// `MaxTurnsError`) when the loop tries to send. Returns the steered
/// texts for the driver to surface.
fn drain_steers(runner: &AgentRunner, run: &mut AgentRun) -> Result<Vec<String>, PromptError> {
    let Some(steering) = &runner.steering else {
        return Ok(Vec::new());
    };
    if !run.ready_for_steering() || !steering.has_pending() {
        return Ok(Vec::new());
    }
    let messages = steering.drain();
    run.steer(messages.clone())?;
    Ok(messages
        .iter()
        .filter_map(|message| message.user_text())
        .collect())
}

/// How many consecutive defective model turns the driver discards and
/// retries before failing the run — one retry, two attempts per turn. A
/// named engine constant, not model config: the retry policy is ours, and
/// a deterministic defect (a token limit cutting calls mid-argument) should
/// burn it fast and fail with the actionable message.
const MAX_DISCARDED_DEFECTIVE_TURNS: usize = 1;

/// Whether a turn error is the model emitting a tool call it broke itself —
/// arguments that cannot be parsed, so the turn can neither execute nor be
/// replayed — as opposed to a transport failure. The defect path discards
/// the turn and retries; transport failures fail the run.
fn is_malformed_tool_call_defect(err: &StreamingError) -> bool {
    matches!(
        err,
        StreamingError::Completion(CompletionError::MalformedToolCall { .. })
    )
}

/// The single agent drive loop, shared by the blocking and streaming surfaces.
///
/// Owns the medium-independent loop — `next_step` dispatch, the `CompletionCall`
/// hook + request preparation, the `Done` memory append — and delegates the
/// medium-specific model call, tool execution, span shaping and finalization to
/// a [`TurnSource`]. The streaming surface forwards the yielded [`DriveItem`]s;
/// the blocking surface folds them to `Done`.
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
        let hook_ctx = HookContext::new(is_streaming, runner.agent_name.clone());
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
        // Consecutive model turns discarded because the model emitted a tool
        // call with unparseable arguments. Reset on any committed turn — the
        // cap bounds the resample streak, not the run.
        let mut discarded_defective_turns: usize = 0;

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
                AgentRunStep::CallModel { prompt, history, turn } => {
                    drop(pending_tool_snapshot.take());
                    if runner.max_turns > 1 {
                        tracing::info!("Current conversation Turns: {}/{}", turn, runner.max_turns);
                    }
                    hook_ctx.set_turn(turn);

                    // Completion-call hooks resolve FIRST: a stop here suppresses
                    // model selection entirely, and their merged `RequestPatch`
                    // is handed to the selection hooks below.
                    let request_patch =
                        match resolve_completion_call(&runner.hooks, &hook_ctx, &prompt, &history, turn).await {
                            CompletionCallOutcome::Terminate(reason) => {
                                store_error_usage(&runner, &run);
                                yield Err(StreamingError::Prompt(Box::new(run.cancel_error(reason))));
                                break 'outer;
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
                            store_error_usage(&runner, &run);
                            yield Err(StreamingError::Prompt(Box::new(run.cancel_error(reason))));
                            break 'outer;
                        }
                    };

                    // Record this turn's base system prompt — the patched-or-baseline
                    // preamble, before any output-mode augmentation the request builder
                    // appends. Borrow rather than clone since it only needs to outlive
                    // span creation.
                    let effective_preamble = request_patch
                        .as_ref()
                        .and_then(|o| o.preamble.as_deref())
                        .or(runner.preamble.as_deref());

                    let chat_span = source.open_chat_span(&runner, effective_preamble);

                    // Pin Tool output mode once committed so later turns stay
                    // consistent even if the per-turn tool set changes (#1928).
                    let committed_output_tool = run.output_tool_name().map(str::to_owned);
                    let mut prepared = match build_prepared_completion_request(
                        &selected_model,
                        prompt.clone(),
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
                        &runner.output_mode,
                        committed_output_tool.as_deref(),
                        runner.output_tool_description.as_deref(),
                        runner.augment_output_preamble,
                        request_patch.as_ref(),
                    )
                    .await
                    {
                        Ok(prepared) => prepared,
                        Err(err) => {
                            store_error_usage(&runner, &run);
                            yield Err(err.into());
                            break 'outer;
                        }
                    };
                    run.set_output_tool_name(prepared.output_tool_name.clone());
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
                        if is_malformed_tool_call_defect(&err)
                            && discarded_defective_turns < MAX_DISCARDED_DEFECTIVE_TURNS
                        {
                            discarded_defective_turns += 1;
                            tracing::warn!(
                                turn,
                                discarded = discarded_defective_turns,
                                "model turn carried a malformed tool call; \
                                 discarding the turn and retrying the request"
                            );
                            if let Err(state_err) = run.discard_turn() {
                                store_error_usage(&runner, &run);
                                yield Err(Box::new(state_err).into());
                                break 'outer;
                            }
                            // Steers that arrived during the defective turn
                            // ride along in the retry request — this is a
                            // turn boundary like any other.
                            match drain_steers(&runner, &mut run) {
                                Ok(texts) => {
                                    for text in texts {
                                        yield Ok(DriveItem::Item(
                                            MultiTurnStreamItem::Steer { text },
                                        ));
                                    }
                                }
                                Err(steer_err) => {
                                    store_error_usage(&runner, &run);
                                    yield Err(Box::new(steer_err).into());
                                    break 'outer;
                                }
                            }
                            continue 'outer;
                        }
                        store_error_usage(&runner, &run);
                        if is_malformed_tool_call_defect(&err) {
                            yield Err(StreamingError::Prompt(Box::new(run.cancel_error(
                                format!(
                                    "the model repeatedly emitted tool calls with \
                                     malformed arguments ({discarded_defective_turns} \
                                     consecutive turns discarded and retried); the \
                                     conversation history is unchanged — resend the \
                                     prompt to try again, or raise the model's output \
                                     token limit if the calls keep getting cut. Last \
                                     failure: {err}"
                                ),
                            ))));
                            break 'outer;
                        }
                        yield Err(err);
                        break 'outer;
                    }
                    // A committed turn resets the defect streak.
                    discarded_defective_turns = 0;
                    pending_tool_snapshot = Some(turn_tool_snapshot);
                    // Turn end: deliver steering to history (a turn with
                    // tool calls is not steerable yet — its tools run
                    // first; the drain below the tool arm catches those).
                    for text in match drain_steers(&runner, &mut run) {
                        Ok(texts) => texts,
                        Err(err) => {
                            store_error_usage(&runner, &run);
                            yield Err(Box::new(err).into());
                            break 'outer;
                        }
                    } {
                        yield Ok(DriveItem::Item(MultiTurnStreamItem::Steer { text }));
                    }
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
                        store_error_usage(&runner, &run);
                        yield Err(err);
                        break 'outer;
                    }
                    // Turn end: results are in history; steers append
                    // after them, before the loop decides whether another
                    // model call fits the budget.
                    for text in match drain_steers(&runner, &mut run) {
                        Ok(texts) => texts,
                        Err(err) => {
                            store_error_usage(&runner, &run);
                            yield Err(Box::new(err).into());
                            break 'outer;
                        }
                    } {
                        yield Ok(DriveItem::Item(MultiTurnStreamItem::Steer { text }));
                    }
                }
                AgentRunStep::Done(response) => {
                    // Run-completion marker, unifying the blocking and streaming
                    // drivers' run-finished logs into one shared event.
                    tracing::info!(
                        turn = run.turn(),
                        max_turns = runner.max_turns,
                        "Agent run finished"
                    );
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
/// The batch commits and surfaces all-or-nothing:
///
/// - The model tool-call events ([`StreamedAssistantContent::ToolCall`]) are
///   emitted up front — they report what the model emitted at turn commit.
/// - Every tool then runs (sequentially at `tool_concurrency <= 1`, else
///   concurrently bounded by it), with outcomes **collected, not surfaced**.
/// - On the first hook termination / fail-closed error the batch fails fast: no
///   new tool starts, not-yet-started concurrent siblings are dropped,
///   already-started ones are drained, and the deterministic lowest call-index
///   error is surfaced with **no** successful [`ToolExecutionCommitted`] /
///   [`StreamUserItem`](MultiTurnStreamItem::StreamUserItem) items and **no**
///   history commit.
/// - Only if the whole batch settles successfully are the per-tool
///   [`ToolExecutionCommitted`](MultiTurnStreamItem::ToolExecutionCommitted) + result
///   items surfaced (in call order, only for tools whose body actually ran) and
///   the results committed to run history.
///
/// When `forward_items` is `false` (the blocking fold) no stream items are built,
/// but the collect/commit and fail-fast behavior is identical, so `run()` and
/// `stream()` return the same terminal reason. `chain_tool_span` lets the
/// blocking surface chain spans into its linear `follows_from` sequence.
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
        let full_history_for_errors = run.full_history();
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

        // Run all tools, COLLECTING outcomes in call order — nothing is surfaced
        // or committed until the whole batch settles (atomic per-batch). On the
        // first hook termination / fail-closed error we stop starting new tools;
        // already-started ones are drained; the lowest call-index error wins; and
        // no successful result is surfaced or committed.
        let mut collected: Vec<Option<CollectedToolResult>> =
            (0..call_count).map(|_| None).collect();
        let mut first_error: Option<(usize, PromptError)> = None;

        if runner.concurrency <= 1 {
            // Sequential: run in call order, fail-fast on the first terminating
            // error so the remaining tools never start.
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
                    &full_history_for_errors,
                )
                .instrument(span)
                .await;
                match outcome {
                    Ok(outcome) => {
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
                    Err(err) => {
                        first_error = Some((index, err));
                        break;
                    }
                }
            }
        } else {
            // Concurrent: bounded by `tool_concurrency`. A shared `terminating`
            // flag makes a not-yet-started sibling skip (its side effect never
            // runs) once any sibling terminates — avoiding the Semantic-Kernel
            // fail-open — while already-in-flight siblings are drained so the
            // lowest call-index terminator wins and no task is left detached.
            let terminating = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let unordered = stream::iter(prepared.into_iter().enumerate())
                .map(|(index, call)| {
                    let PreparedToolCall { tool_call, preresolved_result, internal_call_id, span } = call;
                    let tool_snapshot = &tool_snapshot;
                    let full_history_for_errors = &full_history_for_errors;
                    let terminating = terminating.clone();
                    async move {
                        if let Some(result) = preresolved_result {
                            return (
                                index,
                                Some(Ok(CollectedToolResult {
                                    content: result,
                                    internal_call_id,
                                    surface: ToolSurface::Preresolved,
                                })),
                            );
                        }
                        // `None` marks a dropped (never-started) sibling.
                        if terminating.load(std::sync::atomic::Ordering::SeqCst) {
                            return (index, None);
                        }
                        let outcome = run_single_tool(
                            runner,
                            hook_ctx,
                            tool_snapshot,
                            &tool_call,
                            &internal_call_id,
                            full_history_for_errors,
                        )
                        .await;
                        let mapped = outcome.map(|o| {
                            let surface = match o.execution {
                                ToolExecution::Executed(effective) => {
                                    ToolSurface::Executed(effective)
                                }
                                ToolExecution::Skipped => ToolSurface::Skipped,
                            };
                            CollectedToolResult {
                                content: o.content,
                                internal_call_id,
                                surface,
                            }
                        });
                        (index, Some(mapped))
                    }
                    .instrument(span)
                })
                .buffer_unordered(runner.concurrency);
            futures::pin_mut!(unordered);

            while let Some((index, outcome)) = unordered.next().await {
                // A dropped sibling records nothing.
                let result = match outcome {
                    Some(result) => result,
                    None => continue,
                };
                match result {
                    Ok(collected_result) => {
                        if let Some(slot) = collected.get_mut(index) {
                            *slot = Some(collected_result);
                        }
                    }
                    Err(err) => {
                        // Fail-fast: stop starting new siblings; keep draining
                        // in-flight ones so the lowest call-index terminator wins.
                        terminating.store(true, std::sync::atomic::Ordering::SeqCst);
                        if first_error.as_ref().is_none_or(|(i, _)| index < *i) {
                            first_error = Some((index, err));
                        }
                    }
                }
            }
        }

        // Settle. On termination: surface only the deterministic error — no
        // execution commit, no result, no history commit (all-or-nothing).
        if let Some((_, err)) = first_error {
            yield Err(StreamingError::Prompt(Box::new(err)));
            return;
        }

        // Success: prepare each call's stream items and results in call order,
        // commit the results, then surface the buffered items. An executed call
        // surfaces `ToolExecutionCommitted`
        // (with the effective, hook-rewritten call) then its `ToolResult`; a
        // hook-skipped call surfaces its `ToolResult` only (nothing ran); a
        // preresolved call surfaces nothing (already surfaced during the model
        // turn) but is still committed. Every non-dropped slot is filled; a
        // dropped slot only occurs after a termination, handled above.
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

        for item in surface_items {
            yield Ok(item);
        }
    })
}
