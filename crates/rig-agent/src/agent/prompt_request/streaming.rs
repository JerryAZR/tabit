use rig_core::{OneOrMany, message::AssistantContent, wasm_compat::WasmBoxedFuture};

use crate::{
    agent::completion::PreparedCompletionRequest,
    agent::drive::{DriveStream, TurnSource, drive_tool_calls, record_usage_on_span},
    agent::hook::{
        AgentHook, HookContext, HookStack, ModelTurnFinished, StepEventKind, StreamResponseFinish,
        TextDelta, ToolCallDelta,
    },
    agent::prompt_request::{assistant_text_from_choice, is_empty_assistant_turn},
    agent::run::{
        AgentRun, PendingToolCall,
        streamed::{StreamedTurnAssembler, StreamedTurnEvent},
    },
    agent::runner::{
        AgentRunner, ModelTurnDecision, build_chat_span, observe_action, resolve_model_turn_action,
    },
    streaming::{StreamedAssistantContent, StreamedUserContent, ToolCallDeltaContent},
    tool::{ToolContext, server::ToolRegistrySnapshot},
};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, pin::Pin, sync::Arc};
use tracing_futures::Instrument;

use super::{CompletionCall, PromptResponse, forward_prompt_setters};
use crate::{
    agent::Agent,
    completion::{CompletionError, PromptError},
};
use rig_core::message::{Message, Text};

// The `Send` bound is dropped exactly where `rig-core`'s `WasmCompat*` markers
// go no-op — browser wasm. `rig-core` keys those markers on this same
// predicate, so keep the two in step: a bare `target_arch = "wasm32"` would
// also drop `Send` on WASI, where `rig-core` still requires it.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub type StreamingResult =
    Pin<Box<dyn Stream<Item = Result<MultiTurnStreamItem, StreamingError>> + Send>>;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub type StreamingResult = Pin<Box<dyn Stream<Item = Result<MultiTurnStreamItem, StreamingError>>>>;

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
#[non_exhaustive]
pub enum MultiTurnStreamItem {
    /// A model-call attempt committed — the request is about to be issued.
    /// The first item of every turn: each subsequent item belongs to this
    /// turn until the next `TurnStarted` or the terminal, so a consumer can
    /// attribute everything (deltas, tool calls, usage) from this id alone.
    /// The id is minted from the run's id source (ENGINE.md behavior delta
    /// 10) and never reused: a turn retried by a hook or re-driven after a
    /// provider failure announces again with a fresh id, and a turn that
    /// never commits leaves its announced id uncommitted.
    TurnStarted {
        /// The announced turn id, stable for the whole attempt.
        id: String,
    },
    /// A streamed assistant content item — the content the **model emitted**:
    /// text/reasoning deltas, tool-call deltas, and, when the model turn is
    /// committed, the complete [`StreamedAssistantContent::ToolCall`] for each
    /// tool call Rig routes to execution. Such a call is reported here whether or
    /// not the tool body ultimately runs (a hook skip still reports it);
    /// it is **not** an execution-lifecycle event (see
    /// [`ToolExecutionCommitted`](Self::ToolExecutionCommitted)).
    ///
    /// Two kinds of model tool call are **not** re-emitted as a complete
    /// `ToolCall` item here (their arguments still stream as tool-call deltas):
    /// a call rejected and handled by invalid-tool-call recovery (surfaced via
    /// that recovery path), and a structured-output Tool-mode output-tool call,
    /// which finalizes the run directly — its structured result is surfaced in
    /// the [`FinalResponse`](Self::FinalResponse) rather than as a completed
    /// `ToolCall` item.
    StreamAssistantItem(StreamedAssistantContent),
    /// Confirmation that Rig **executed and committed** a tool call. This is not
    /// a real-time start notification: it is surfaced together with its
    /// `ToolResult` only after the whole batch settles successfully. Use tool
    /// hooks for live host-side start/result observation.
    ///
    /// This item is emitted only for a tool whose body actually ran (it passed
    /// its `ToolCall` hook checks), never for a call dropped by a sibling's
    /// termination, skipped by a hook, or resolved by invalid-call recovery.
    /// Correlate it with the model call and result through `internal_call_id`.
    ToolExecutionCommitted {
        /// The tool call as **executed**: the model's call with any
        /// [`ToolCallAction::Rewrite`](crate::agent::ToolCallAction::Rewrite) hook rewrite
        /// applied (so a redaction rewrite is reflected here, not leaked). The
        /// model's *original* call is reported via
        /// [`StreamAssistantItem`](Self::StreamAssistantItem).
        tool_call: rig_core::message::ToolCall,
        /// Rig-generated id correlating this execution with the model tool call
        /// ([`StreamedAssistantContent::ToolCall::internal_call_id`]) and the
        /// resulting [`StreamedUserContent::ToolResult`].
        internal_call_id: String,
    },
    /// A streamed user content item: the **result** of an executed (or
    /// hook-skipped) tool call. The tool batch commits and surfaces atomically at
    /// every `tool_concurrency` (including the sequential default): results are
    /// surfaced (in call order) only after the whole batch settles successfully —
    /// a run that terminates mid-batch surfaces no successful tool results.
    StreamUserItem(StreamedUserContent),
    /// Details for one successfully completed completion request made by this agent stream.
    ///
    /// This is emitted when a provider call finishes. Usage is the provider's
    /// final usage for that completion request when available; it is not
    /// incremental per streamed token.
    ///
    /// ```rust,ignore
    /// match item {
    ///     MultiTurnStreamItem::CompletionCall(completion_call) => {
    ///         // Zero-valued usage means the provider reported no metrics.
    ///         if completion_call.usage.has_values() {
    ///             let context_tokens = completion_call.usage.input_tokens;
    ///         }
    ///     }
    ///     _ => {}
    /// }
    /// ```
    CompletionCall(CompletionCall),
    /// The completed model turn was rejected by a hook for retry.
    ///
    /// Text and reasoning deltas emitted for this turn were provisional. A
    /// consumer should discard or visually reset output associated with `turn`.
    /// A subsequent attempt is made only if the run's total model-call budget
    /// permits it.
    ModelTurnRetried {
        /// One-based model-call index of the rejected turn.
        turn: usize,
    },
    /// The final result from the stream: the unified [`PromptResponse`] shared
    /// with the blocking surface.
    FinalResponse(PromptResponse),
    /// A steering message the user queued while the run was in flight,
    /// injected at a tool-use roundtrip or after a final model turn. This is
    /// an observation event: the message is already part of the model's
    /// context (and of the final response's history) — session layers record
    /// it as its own user message.
    Steer {
        /// The message text.
        text: String,
    },
}

/// Build the unified [`PromptResponse`] for the streaming surface from the
/// final turn's structured content.
fn final_response_from_content(
    content: OneOrMany<AssistantContent>,
    aggregated_usage: crate::completion::Usage,
    completion_calls: Vec<CompletionCall>,
    history: Option<Vec<Message>>,
) -> PromptResponse {
    let mut response = PromptResponse::new(assistant_text_from_choice(&content), aggregated_usage)
        .with_content(content)
        .with_completion_calls(completion_calls);
    response.messages = history;
    response
}

impl MultiTurnStreamItem {
    pub(crate) fn stream_item(item: StreamedAssistantContent) -> Self {
        Self::StreamAssistantItem(item)
    }

    pub fn final_response(
        content: OneOrMany<AssistantContent>,
        aggregated_usage: crate::completion::Usage,
    ) -> Self {
        Self::FinalResponse(final_response_from_content(
            content,
            aggregated_usage,
            Vec::new(),
            None,
        ))
    }

    pub fn final_response_with_history(
        content: OneOrMany<AssistantContent>,
        aggregated_usage: crate::completion::Usage,
        history: Option<Vec<Message>>,
    ) -> Self {
        Self::FinalResponse(final_response_from_content(
            content,
            aggregated_usage,
            Vec::new(),
            history,
        ))
    }

    pub(crate) fn final_response_with_completion_calls(
        content: OneOrMany<AssistantContent>,
        aggregated_usage: crate::completion::Usage,
        completion_calls: Vec<CompletionCall>,
        history: Option<Vec<Message>>,
    ) -> Self {
        Self::FinalResponse(final_response_from_content(
            content,
            aggregated_usage,
            completion_calls,
            history,
        ))
    }
}
/// Build the final streamed content for a finished run (#1928).
///
/// When the finishing turn carries a tool call it is a Tool-mode output-tool
/// call (a real tool call would have routed to `CallTools`, not `Done`). In that
/// case the tool call AND the model's prose are dropped, any reasoning/image
/// content is kept, and `output` is appended as the final text — so the streamed
/// [`PromptResponse::output`] string is the structured output rather than the
/// prose, with no unanswered tool_use, matching the non-streaming `output`. Note
/// this shapes only the surfaced [`PromptResponse::content`]; the persisted
/// message history is built by the state machine (which keeps the prose, like the
/// blocking driver), so `content` and `messages` intentionally differ on prose in
/// this case.
/// Otherwise returns `None` and the caller surfaces the turn's content unchanged.
fn finalize_streamed_choice(
    last_final_choice: &OneOrMany<AssistantContent>,
    output: &str,
) -> Option<OneOrMany<AssistantContent>> {
    let finalized_via_output_tool = last_final_choice
        .iter()
        .any(|item| matches!(item, AssistantContent::ToolCall(_)));
    if !finalized_via_output_tool {
        return None;
    }
    let mut items: Vec<AssistantContent> = last_final_choice
        .iter()
        .filter(|item| {
            !matches!(
                item,
                AssistantContent::ToolCall(_) | AssistantContent::Text(_)
            )
        })
        .cloned()
        .collect();
    items.push(AssistantContent::text(output.to_string()));
    Some(
        OneOrMany::from_iter_optional(items)
            .unwrap_or_else(|| OneOrMany::one(AssistantContent::text(output.to_string()))),
    )
}

#[derive(Debug, thiserror::Error)]
pub enum StreamingError {
    #[error("CompletionError: {0}")]
    Completion(#[from] CompletionError),
    #[error("PromptError: {0}")]
    Prompt(#[from] Box<PromptError>),
}

impl From<rig_core::memory::MemoryError> for StreamingError {
    fn from(err: rig_core::memory::MemoryError) -> Self {
        Self::Prompt(Box::new(PromptError::MemoryError(err)))
    }
}

/// A builder for creating prompt requests with customizable options.
/// Uses generics to track which options have been set during the build process.
///
/// When the agent has no configured `default_max_turns`, the implicit budget is
/// one model call. Use [`.max_turns()`](Self::max_turns) to override the agent's
/// configured or implicit budget; a tool call followed by a model-authored final
/// answer generally requires at least two model calls.
pub struct StreamingPromptRequest {
    /// The hook-aware driver this streaming request configures and runs.
    runner: AgentRunner,
}

impl StreamingPromptRequest {
    /// Create a new `StreamingPromptRequest` from an agent, including its
    /// default hooks.
    pub fn new(agent: Arc<Agent>, prompt: impl Into<Message>) -> StreamingPromptRequest {
        Self::from_agent(agent.as_ref(), prompt)
    }

    /// Create a new StreamingPromptRequest from an agent, cloning the agent's
    /// data and default hook stack.
    pub fn from_agent(agent: &Agent, prompt: impl Into<Message>) -> StreamingPromptRequest {
        StreamingPromptRequest {
            runner: AgentRunner::from_agent(agent, prompt),
        }
    }

    /// Build a request from a full conversation: the history's final
    /// message is the turn being sent, the rest precede it as context.
    /// An empty conversation fails loudly when the request is sent.
    pub fn from_agent_history(agent: &Agent, conversation: Vec<Message>) -> StreamingPromptRequest {
        StreamingPromptRequest {
            runner: AgentRunner::from_agent_conversation(agent, conversation),
        }
    }

    /// Set the total model-call budget, including the initial call and every
    /// retry or continuation. Zero emits no model calls; one permits only the
    /// initial call.
    ///
    /// Named to match the blocking
    /// [`PromptRequest::max_turns`](super::PromptRequest::max_turns) and
    /// [`TypedPromptRequest::max_turns`](super::TypedPromptRequest::max_turns)
    /// builders so the same call reads identically on either surface.
    pub fn max_turns(mut self, turns: usize) -> Self {
        self.runner = self.runner.max_turns(turns);
        self
    }

    /// Execute up to `concurrency` of a turn's tool calls at once (1 by default,
    /// i.e. sequential). See [`AgentRunner::tool_concurrency`]: at any
    /// `concurrency` the stream emits the model's `ToolCall` items (call order),
    /// then — atomically, after the whole tool batch settles successfully — the
    /// per-tool `ToolExecutionCommitted` + `ToolResult` items in **call order** (not
    /// completion order). The streamed message history is unchanged at any
    /// `concurrency`.
    pub fn tool_concurrency(mut self, concurrency: usize) -> Self {
        self.runner = self.runner.tool_concurrency(concurrency);
        self
    }

    /// Append a hook to this request's hook stack (on top of any the agent
    /// already carries). Hooks run in registration order; how their results
    /// compose is event-dependent (model selections and `ToolCall`/`ToolResult` rewrites
    /// chain, `CompletionCall` request patches accumulate and merge, while model-turn
    /// steering and observe-only/recovery events use first-non-`Continue`-wins). See the
    /// [`hook`](crate::agent::hook) module docs.
    pub fn add_hook<H>(mut self, hook: H) -> Self
    where
        H: AgentHook + 'static,
    {
        self.runner = self.runner.add_hook(hook);
        self
    }

    /// Attach the steering source whose queued user messages join the run at
    /// tool-use roundtrips and after final model turns (each surfaced as a
    /// [`Steer`](MultiTurnStreamItem::Steer) item). Messages beyond the
    /// model-call budget stay queued in the source.
    pub fn steering(mut self, steering: Arc<dyn crate::agent::runner::SteeringSource>) -> Self {
        self.runner = self.runner.steering(steering);
        self
    }

    /// Set the source of announced turn ids — see
    /// [`AgentRunner::turn_id_source`](crate::agent::runner::AgentRunner::turn_id_source).
    pub fn turn_id_source(mut self, source: crate::agent::runner::TurnIdSource) -> Self {
        self.runner = self.runner.turn_id_source(source);
        self
    }

    forward_prompt_setters!(runner);

    async fn send(self) -> StreamingResult {
        self.runner.stream().await
    }
}

/// [`TurnSource`] for the streaming surface: each turn opens a provider stream,
/// drives a [`StreamedTurnAssembler`], and yields assistant/tool deltas.
pub(crate) struct StreamingTurnSource {
    /// The raw provider choice of the most recent turn; the final response
    /// surfaces it as-is, even when canonical reordering was recorded in history.
    last_final_choice: OneOrMany<AssistantContent>,
    last_message_id: Option<String>,
    /// Resolved agent name, kept only for the empty-turn diagnostic warning.
    agent_name: String,
    /// Whether we created the agent span (vs. adopting a caller's ambient span);
    /// gates recording `gen_ai.completion` onto it, matching the blocking source
    /// so neither surface pollutes a caller-supplied span.
    created_agent_span: bool,
    /// Whether sensitive run-level prompt and completion content may be recorded.
    record_telemetry_content: bool,
    /// Hot-path interest gates, computed once: skip building/dispatching the
    /// high-frequency delta events when no hook observes them.
    observes_text_delta: bool,
    observes_tool_call_delta: bool,
}

impl StreamingTurnSource {
    pub(crate) fn new(
        hooks: &HookStack,
        agent_name: String,
        created_agent_span: bool,
        record_telemetry_content: bool,
    ) -> Self {
        Self {
            last_final_choice: OneOrMany::one(AssistantContent::text("")),
            last_message_id: None,
            agent_name,
            created_agent_span,
            record_telemetry_content,
            observes_text_delta: hooks.observes(StepEventKind::TextDelta),
            observes_tool_call_delta: hooks.observes(StepEventKind::ToolCallDelta),
        }
    }
}

impl TurnSource for StreamingTurnSource {
    fn open_chat_span(
        &self,
        runner: &AgentRunner,
        effective_preamble: Option<&str>,
    ) -> tracing::Span {
        build_chat_span!(runner, effective_preamble, "chat_streaming", "chat")
    }

    fn run_model_turn<'a>(
        &'a mut self,
        runner: &'a AgentRunner,
        hook_ctx: &'a HookContext,
        run: &'a mut AgentRun,
        prepared: PreparedCompletionRequest,
        chat_span: tracing::Span,
        agent_span: &'a tracing::Span,
        current_prompt: Message,
    ) -> DriveStream<'a> {
        Box::pin(async_stream::stream! {
            let mut stream = match prepared
                .builder
                .stream()
                .instrument(chat_span.clone())
                .await
            {
                Ok(stream) => stream,
                Err(err) => {
                    yield Err(err.into());
                    return;
                }
            };
            // Captured from each completion-call emission so the normalized
            // `ModelTurnFinished` event carries the turn's usage.
            let mut last_usage = crate::completion::Usage::new();

            let mut assembler = StreamedTurnAssembler::new(
                prepared.executable_tool_names.clone(),
                prepared.allowed_tool_names.clone(),
            );
            let mut completion_call_emitted = false;
            let turn_abandoned = false;
            let mut provider_final_seen = false;
            let mut pending_final = None;
            // Mirrors the blocking driver's `response_hook_suppressed`: a turn
            // whose invalid tool call was repaired is a recovered turn, so its
            // response-finish hook is suppressed.
            let turn_recovered = false;

            // Emit the turn's single `CompletionCall` exactly once, recording its
            // usage onto the chat span and into the run. Defined here (not a free
            // fn) so it captures `completion_call_emitted`/`chat_span`/`run`; the
            // `yield` stays at each call site because `async_stream::stream!`
            // cannot see a `yield` produced inside a nested macro expansion.
            // Returns the item to yield (`Some` the first time, `None` after), or
            // the terminal error to surface.
            macro_rules! emit_completion_call {
                ($usage:expr, $finish_reason:expr) => {{
                    let usage = $usage;
                    last_usage = usage;
                    if !completion_call_emitted {
                        if usage.has_values() {
                            record_usage_on_span(&chat_span, usage);
                        }
                        match run.record_streamed_completion_call(usage, $finish_reason) {
                            Ok(call) => {
                                completion_call_emitted = true;
                                Ok(Some(MultiTurnStreamItem::CompletionCall(call)))
                            }
                            Err(err) => Err(Box::new(err).into()),
                        }
                    } else {
                        Ok(None)
                    }
                }};
            }

            while let Some(item) = stream.next().await {
                let item = match item {
                    Ok(item) => item,
                    Err(err) => {
                        yield Err(err.into());
                        return;
                    }
                };
                if provider_final_seen {
                    yield Err(CompletionError::ResponseError(
                        "provider stream emitted visible assistant content after its final response"
                            .to_string(),
                    )
                    .into());
                    return;
                }
                let mut events: VecDeque<StreamedTurnEvent> = match assembler.ingest(&item) {
                    Ok(events) => events.into(),
                    Err(err) => {
                        yield Err(err.into());
                        return;
                    }
                };
                // At most one event per ingested item forwards the item itself;
                // moving it out of the slot avoids a clone per streamed delta.
                let mut item_slot = Some(item);
                while let Some(event) = events.pop_front() {
                    match event {
                        StreamedTurnEvent::EmitIngested => {
                            if self.observes_text_delta
                                && let Some(StreamedAssistantContent::Text(text)) =
                                    item_slot.as_ref()
                                && let Some(reason) = observe_action(
                                    runner
                                        .hooks
                                        .on_text_delta(
                                            hook_ctx,
                                            TextDelta {
                                                delta: &text.text,
                                                aggregated: assembler.aggregated_text(),
                                            },
                                        )
                                        .await,
                                )
                            {
                                yield Err(StreamingError::Prompt(Box::new(
                                    run.cancel_error(reason),
                                )));
                                return;
                            }
                            if let Some(item) = item_slot.take() {
                                yield Ok(MultiTurnStreamItem::stream_item(item));
                            }
                        }
                        StreamedTurnEvent::EmitToolCallDelta {
                            id,
                            internal_call_id,
                            content,
                        } => {
                            if self.observes_tool_call_delta {
                                let (delta_name, delta_text) = match &content {
                                    ToolCallDeltaContent::Name(name) => (Some(name.as_str()), ""),
                                    ToolCallDeltaContent::Delta(delta) => (None, delta.as_str()),
                                };
                                if let Some(reason) = observe_action(
                                    runner
                                        .hooks
                                        .on_tool_call_delta(
                                            hook_ctx,
                                            ToolCallDelta {
                                                tool_call_id: &id,
                                                internal_call_id: &internal_call_id,
                                                tool_name: delta_name,
                                                delta: delta_text,
                                            },
                                        )
                                        .await,
                                ) {
                                    yield Err(StreamingError::Prompt(Box::new(
                                        run.cancel_error(reason),
                                    )));
                                    return;
                                }
                            }

                            yield Ok(MultiTurnStreamItem::StreamAssistantItem(
                                StreamedAssistantContent::ToolCallDelta {
                                    id,
                                    internal_call_id,
                                    content,
                                },
                            ));
                        }
                        StreamedTurnEvent::Completed {
                            usage,
                            finish_reason,
                            emit_final,
                        } => {
                            match emit_completion_call!(usage, finish_reason) {
                                Ok(Some(item)) => yield Ok(item),
                                Ok(None) => {}
                                Err(err) => {
                                    yield Err(err);
                                    return;
                                }
                            }
                            provider_final_seen = true;

                            if emit_final
                                && matches!(
                                    item_slot.as_ref(),
                                    Some(StreamedAssistantContent::Final(_))
                                )
                            {
                                pending_final = item_slot.take();
                            }
                        }
                    }
                }
            }

            if turn_abandoned {
                return;
            }

            // The provider stream ended without its terminal record. Per the
            // emission contract (`rig_core::streaming`), that absence means
            // truncation and must never be treated as a successful zero-usage
            // completion: reject the turn before any usage fallback, assembly,
            // history mutation, or tool dispatch can occur.
            if !provider_final_seen {
                yield Err(CompletionError::ResponseError(
                    "provider stream ended without a terminal record; treating the turn as truncated"
                        .to_string(),
                )
                .into());
                return;
            }

            if let Some(err) = assembler.pending_delta_error() {
                yield Err(err.into());
                return;
            }

            // Final fallback: no usage was ever learned, so there is nothing to
            // record onto the span and this is the last read of the flag — kept
            // inline (not `emit_completion_call!`) so it doesn't emit a dead
            // `completion_call_emitted = true` write.
            if !completion_call_emitted {
                match run.record_streamed_completion_call(crate::completion::Usage::new(), None) {
                    Ok(call) => yield Ok(MultiTurnStreamItem::CompletionCall(call)),
                    Err(err) => {
                        yield Err(Box::new(err).into());
                        return;
                    }
                }
            }

            let final_turn_content = stream.choice.clone();
            let streamed_turn = assembler.finish(stream.message_id.clone(), &final_turn_content);
            if pending_final.is_some()
                && !turn_recovered
                && let Some(reason) = observe_action(
                    runner
                        .hooks
                        .on_stream_response_finish(
                            hook_ctx,
                            StreamResponseFinish {
                                prompt: &current_prompt,
                                content: &streamed_turn.choice,
                                usage: last_usage,
                                message_id: streamed_turn.message_id.as_deref(),
                            },
                        )
                        .await,
                )
            {
                yield Err(StreamingError::Prompt(Box::new(run.cancel_error(reason))));
                return;
            }
            self.last_message_id = streamed_turn.message_id.clone();
            // The canonical assistant content: `finish` normalizes
            // reasoning/text/tool ordering, so this can differ from the raw
            // `stream.choice` aggregate. `ModelTurnFinished` — the normalized
            // per-turn event — carries this, matching what is recorded into run
            // history; the raw `stream.choice` is kept in `last_final_choice` for
            // the raw/final streaming behavior.
            let canonical_choice = streamed_turn.choice.clone();
            if let Err(err) = run.turn_committed_streamed(streamed_turn) {
                yield Err(Box::new(err).into());
                return;
            }
            // Normalized per-turn event, fired once the turn is parked for
            // acceptance on the streaming surface — including tool-only /
            // reasoning-only turns that fire no `StreamResponseFinish`.
            // Suppressed for recovered turns, mirroring the blocking surface's
            // `Continue` arm.
            if !turn_recovered {
                let action = runner
                    .hooks
                    .on_model_turn_finished(
                        hook_ctx,
                        ModelTurnFinished {
                            turn: hook_ctx.turn(),
                            content: &canonical_choice,
                            usage: last_usage,
                        },
                    )
                    .await;
                match resolve_model_turn_action(run, action) {
                    Ok(ModelTurnDecision::Advance) => {}
                    Ok(ModelTurnDecision::Retried) => {
                        yield Ok(MultiTurnStreamItem::ModelTurnRetried {
                            turn: hook_ctx.turn(),
                        });
                        return;
                    }
                    Ok(ModelTurnDecision::Terminate(reason)) => {
                        // Before model-turn steering was added, Stop observed
                        // this already completed provider turn: its buffered
                        // final and content telemetry were visible before the
                        // cancellation. Preserve that behavior while Retry
                        // alone suppresses the provisional final.
                        if self.created_agent_span && self.record_telemetry_content {
                            agent_span.record(
                                "gen_ai.completion",
                                assistant_text_from_choice(&canonical_choice),
                            );
                        }
                        rig_core::telemetry::record_model_output(
                            &chat_span,
                            &canonical_choice,
                            runner.record_telemetry_content,
                        );
                        if let Some(item) = pending_final.take() {
                            yield Ok(MultiTurnStreamItem::stream_item(item));
                        }
                        yield Err(StreamingError::Prompt(Box::new(run.cancel_error(reason))));
                        return;
                    }
                    Err(err) => {
                        yield Err(StreamingError::Prompt(Box::new(err)));
                        return;
                    }
                }
            }

            // Only hook-accepted canonical output belongs in content telemetry.
            // Keep caller-owned spans untouched, matching the blocking source.
            if self.created_agent_span && self.record_telemetry_content {
                agent_span.record(
                    "gen_ai.completion",
                    assistant_text_from_choice(&canonical_choice),
                );
            }
            rig_core::telemetry::record_model_output(
                &chat_span,
                &canonical_choice,
                runner.record_telemetry_content,
            );

            if let Some(item) = pending_final {
                yield Ok(MultiTurnStreamItem::stream_item(item));
            }
            self.last_final_choice = final_turn_content;
        })
    }

    fn run_tool_calls<'a>(
        &'a self,
        runner: &'a AgentRunner,
        hook_ctx: &'a HookContext,
        run: &'a mut AgentRun,
        calls: Vec<PendingToolCall>,
        tool_snapshot: Arc<ToolRegistrySnapshot>,
    ) -> DriveStream<'a> {
        // The streaming surface chains nothing onto its tool spans, and forwards
        // the ToolCall/ToolResult items to the consumer.
        drive_tool_calls(
            runner,
            hook_ctx,
            run,
            calls,
            tool_snapshot,
            |span| span,
            true,
        )
    }

    fn record_run_level_telemetry(
        &self,
        agent_span: &tracing::Span,
        response: &PromptResponse,
        created_agent_span: bool,
    ) {
        if created_agent_span {
            record_usage_on_span(agent_span, response.usage);
        }
    }

    fn final_item(&self, response: &PromptResponse) -> Option<MultiTurnStreamItem> {
        // Tool output mode (#1928): when the finishing turn made the output-tool
        // call, surface the run's structured output as the final content.
        let final_choice = finalize_streamed_choice(&self.last_final_choice, &response.output)
            .unwrap_or_else(|| {
                if is_empty_assistant_turn(&self.last_final_choice) {
                    tracing::warn!(
                        agent_name = self.agent_name.as_str(),
                        message_id = ?self.last_message_id,
                        "Streaming turn completed without assistant text; final response will be empty"
                    );
                }
                self.last_final_choice.clone()
            });
        // Always surface the accumulated messages (parity with the blocking
        // `run()`), regardless of whether the caller supplied input history.
        let final_messages: Option<Vec<Message>> =
            Some(response.messages.clone().unwrap_or_default());
        Some(MultiTurnStreamItem::final_response_with_completion_calls(
            final_choice,
            response.usage,
            response.completion_calls.clone(),
            final_messages,
        ))
    }
}

impl IntoFuture for StreamingPromptRequest {
    type Output = StreamingResult; // what `.await` returns
    type IntoFuture = WasmBoxedFuture<'static, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        // Wrap send() in a future, because send() returns a stream immediately
        Box::pin(async move { self.send().await })
    }
}

/// Helper function to stream assistant-visible completion output to stdout.
///
/// This helper prints streamed assistant text and reasoning. Streaming metadata
/// events, such as `MultiTurnStreamItem::CompletionCall`, are not printed;
/// metadata is returned on the [`PromptResponse`] via accessors such as
/// [`PromptResponse::completion_calls`]. A model-turn retry prints a visible
/// boundary because text already written to stdout cannot be retracted.
pub async fn stream_to_stdout(
    stream: &mut StreamingResult,
) -> Result<PromptResponse, std::io::Error> {
    let mut final_res = PromptResponse::empty();
    print!("Response: ");
    while let Some(content) = stream.next().await {
        match content {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(
                Text { text, .. },
            ))) => {
                print!("{text}");
                std::io::Write::flush(&mut std::io::stdout())?;
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Reasoning(
                reasoning,
            ))) => {
                let reasoning = reasoning.display_text();
                print!("{reasoning}");
                std::io::Write::flush(&mut std::io::stdout())?;
            }
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                final_res = res;
            }
            Ok(MultiTurnStreamItem::ModelTurnRetried { turn }) => {
                print!("\n[model turn {turn} rejected; retry requested]\nResponse: ");
                std::io::Write::flush(&mut std::io::stdout())?;
            }
            Err(err) => {
                eprintln!("Error: {err}");
            }
            _ => {}
        }
    }

    Ok(final_res)
}

#[cfg(test)]
#[allow(irrefutable_let_patterns, unreachable_patterns)]
#[path = "streaming_tests.rs"]
mod streaming_tests;
