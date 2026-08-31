use rig_core::{OneOrMany, message::AssistantContent, wasm_compat::WasmBoxedFuture};

use crate::{
    agent::completion::PreparedCompletionRequest,
    agent::drive::PhaseEvent,
    agent::drive::{DriveStream, TurnSource, drive_tool_calls, record_usage_on_span},
    agent::hook::{AgentHook, HookContext},
    agent::prompt_request::{assistant_text_from_choice, is_empty_assistant_turn},
    agent::run::{
        ModelTurn, PendingToolCall, RunLedger,
        streamed::{StreamedTurnAssembler, StreamedTurnEvent},
    },
    agent::runner::{AgentRunner, build_chat_span},
    streaming::{StreamedAssistantContent, StreamedUserContent},
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
    /// The announced turn committed — its content is final and part of
    /// the run's history. The closing bracket of the turn the matching
    /// [`TurnStarted`](Self::TurnStarted) opened: a turn discarded by a
    /// defect retry, a provider failure, or an abort never commits, and
    /// its announced id stays uncommitted. The loop's own fold is the
    /// durable commit; this item announces it, carrying the committed
    /// content for consumers that render it.
    TurnCommitted {
        /// The announced id of the committed turn — the assistant
        /// entry's id (the one-value rule).
        id: String,
        /// The committed assistant content.
        content: Box<rig_core::OneOrMany<rig_core::message::AssistantContent>>,
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
    /// The completed model turn was rejected — by a hook veto or a typed
    /// defect (malformed tool calls) — and is retried.
    ///
    /// Text and reasoning deltas emitted for this turn were provisional. A
    /// consumer should discard or visually reset output associated with `turn`,
    /// and bill the attempt (its completion call already reported usage). A
    /// subsequent attempt is made only if the run's total model-call budget
    /// permits it.
    ModelTurnRetried {
        /// One-based model-call index of the rejected turn.
        turn: usize,
    },
    /// The final result from the stream: the unified [`PromptResponse`] shared
    /// with the blocking surface.
    FinalResponse(PromptResponse),
    /// A steering message the loop drained and folded — the opening
    /// batch and mid-run steers alike (one drain). This is an
    /// observation event: the message is already folded into the
    /// conversation under `entry_id` (the born-early id from the
    /// mailbox); session layers announce it as its own user message.
    Steer {
        /// The born-early entry id the message folded under.
        id: String,
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
#[derive(Debug, thiserror::Error)]
pub enum StreamingError {
    #[error("CompletionError: {0}")]
    Completion(#[from] CompletionError),
    #[error("PromptError: {0}")]
    Prompt(#[from] Box<PromptError>),
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
    /// already carries). Hooks run in registration order; tool-call argument
    /// rewrites chain into later hooks (see the
    /// [`hook`](crate::agent::hook) module docs).
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
}

impl StreamingTurnSource {
    pub(crate) fn new(
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
        ledger: &'a mut RunLedger,
        prepared: PreparedCompletionRequest,
        chat_span: tracing::Span,
        agent_span: &'a tracing::Span,
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
            // Captured from each completion-call emission so the settled
            // `ModelTurn` carries the turn's usage.
            let mut last_usage = crate::completion::Usage::new();

            let mut assembler = StreamedTurnAssembler::new(
                prepared.executable_tool_names.clone(),
                prepared.allowed_tool_names.clone(),
            );
            let mut completion_call_emitted = false;
            let mut provider_final_seen = false;
            let mut pending_final = None;

            // Emit the turn's single `CompletionCall` exactly once, recording its
            // usage onto the chat span and into the run ledger. Defined here
            // (not a free fn) so it captures `completion_call_emitted`/`chat_span`/
            // `ledger`; the
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
                        let call = ledger.record(usage, $finish_reason);
                        completion_call_emitted = true;
                        Ok(Some(MultiTurnStreamItem::CompletionCall(call)))
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
                            if let Some(item) = item_slot.take() {
                                yield Ok(PhaseEvent::Item(
                                    MultiTurnStreamItem::stream_item(item),
                                ));
                            }
                        }
                        StreamedTurnEvent::EmitToolCallDelta {
                            id,
                            internal_call_id,
                            content,
                        } => {
                            yield Ok(PhaseEvent::Item(
                                MultiTurnStreamItem::StreamAssistantItem(
                                    StreamedAssistantContent::ToolCallDelta {
                                        id,
                                        internal_call_id,
                                        content,
                                    },
                                )
                            ));
                        }
                        StreamedTurnEvent::Completed {
                            usage,
                            finish_reason,
                            emit_final,
                        } => {
                            match emit_completion_call!(usage, finish_reason) {
                                Ok(Some(item)) => yield Ok(PhaseEvent::Item(item)),
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
                let call = ledger.record(crate::completion::Usage::new(), None);
                yield Ok(PhaseEvent::Item(MultiTurnStreamItem::CompletionCall(call)));
            }

            let final_turn_content = stream.choice.clone();
            let streamed_turn = assembler.finish(stream.message_id.clone(), &final_turn_content);
            self.last_message_id = streamed_turn.message_id.clone();
            // The canonical assistant content: `finish` normalizes
            // reasoning/text/tool ordering, so this can differ from the raw
            // `stream.choice` aggregate. The turn settles with the canonical
            // shape; the raw aggregate is kept in `last_final_choice` for the
            // raw/final streaming behavior.
            let canonical_choice = streamed_turn.choice.clone();

            // Only canonical output belongs in content telemetry. Keep
            // caller-owned spans untouched, matching the blocking source.
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
                yield Ok(PhaseEvent::Item(MultiTurnStreamItem::stream_item(item)));

            }
            self.last_final_choice = final_turn_content;

            // Settle: the loop classifies and
            // folds — this source's part in the turn is done.
            let mut turn = ModelTurn::new(
                streamed_turn.message_id.clone(),
                streamed_turn.choice.clone(),
                last_usage,
                None::<crate::completion::FinishReason>,
                prepared.executable_tool_names.clone(),
                prepared.allowed_tool_names.clone(),
            );
            turn.internal_call_ids = streamed_turn.internal_call_ids.clone();
            yield Ok(PhaseEvent::ModelTurn(Box::new(turn)));
        })
    }

    fn run_tool_calls<'a>(
        &'a self,
        runner: &'a AgentRunner,
        hook_ctx: &'a HookContext,
        calls: Vec<PendingToolCall>,
        tool_snapshot: Arc<ToolRegistrySnapshot>,
    ) -> DriveStream<'a> {
        // The streaming surface chains nothing onto its tool spans, and forwards
        // the ToolCall/ToolResult items to the consumer.
        drive_tool_calls(runner, hook_ctx, calls, tool_snapshot, |span| span, true)
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
        if is_empty_assistant_turn(&self.last_final_choice) {
            tracing::warn!(
                agent_name = self.agent_name.as_str(),
                message_id = ?self.last_message_id,
                "Streaming turn completed without assistant text; final response will be empty"
            );
        }
        // Always surface the accumulated messages (parity with the blocking
        // `run()`), regardless of whether the caller supplied input history.
        let final_messages: Option<Vec<Message>> =
            Some(response.messages.clone().unwrap_or_default());
        Some(MultiTurnStreamItem::final_response_with_completion_calls(
            self.last_final_choice.clone(),
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
