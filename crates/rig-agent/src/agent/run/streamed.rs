//! Streamed-turn assembly for [`AgentRun`](super::AgentRun).
//!
//! A streamed model turn arrives as incremental [`StreamedAssistantContent`]
//! items. [`StreamedTurnAssembler`] is the sans-IO accumulator that turns that
//! item stream into the same canonical complete turn the non-streaming path
//! feeds the machine — while telling the driver what to forward to its
//! consumer and surfacing invalid tool calls the moment they appear, so a
//! driver can stop paying for a doomed provider stream early.
//!
//! The protocol, paired with the streamed entry points on
//! [`AgentRun`](super::AgentRun):
//!
//! 1. On [`AgentRunStep::CallModel`](super::AgentRunStep::CallModel), open a
//!    provider stream and create one assembler per turn with the tool names
//!    advertised for that turn.
//! 2. Feed every stream item to [`StreamedTurnAssembler::ingest`] and act on
//!    the returned [`StreamedTurnEvent`]s: forward items to the consumer, and
//!    on [`StreamedTurnEvent::InvalidToolCall`] consult
//!    [`AgentRun::resolve_streamed_invalid_tool_call`](super::AgentRun::resolve_streamed_invalid_tool_call) —
//!    [`StreamedResolution::Repaired`] continues the same stream via
//!    [`StreamedTurnAssembler::resolve_pending_invalid`];
//!    [`StreamedResolution::TurnAbandoned`] means drain the provider stream
//!    for usage and re-enter
//!    [`AgentRun::next_step`](super::AgentRun::next_step).
//! 3. When the provider stream ends, call [`StreamedTurnAssembler::finish`]
//!    and feed the result to
//!    [`AgentRun::streamed_turn`](super::AgentRun::streamed_turn); the run
//!    then proceeds exactly like a non-streamed one
//!    ([`CallTools`](super::AgentRunStep::CallTools) /
//!    [`Done`](super::AgentRunStep::Done)).
//!
//! [`crate::streaming::StreamingPrompt::stream_prompt`] drives this protocol
//! internally; hand-driven runs can use it to stream any
//! [`AgentRun`](super::AgentRun).

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use rig_core::{
    OneOrMany,
    message::{AssistantContent, Reasoning, ToolCall, ToolFunction, ToolResult},
};

use crate::{
    agent::prompt_request::{TOOL_NOT_EXECUTED_DUE_TO_INVALID_PEER, tool_result_message},
    completion::{CompletionError, Message, Usage},
    json_utils,
    streaming::{StreamedAssistantContent, ToolCallDeltaContent},
};

/// Merge an incoming reasoning block into the accumulated reasoning,
/// extending an existing block when provider-assigned IDs match.
pub(crate) fn merge_reasoning_blocks(
    accumulated_reasoning: &mut Vec<Reasoning>,
    incoming: &Reasoning,
) {
    let ids_match = |existing: &Reasoning| {
        matches!(
            (&existing.id, &incoming.id),
            (Some(existing_id), Some(incoming_id)) if existing_id == incoming_id
        )
    };

    if let Some(existing) = accumulated_reasoning
        .iter_mut()
        .rev()
        .find(|existing| ids_match(existing))
    {
        existing.content.extend(incoming.content.clone());
    } else {
        accumulated_reasoning.push(incoming.clone());
    }
}

/// Assemble assistant content in canonical replay order: reasoning blocks,
/// then text, then trailing items (tool calls, images).
pub(crate) fn ordered_streaming_assistant_content(
    reasoning_items: impl IntoIterator<Item = Reasoning>,
    text_items: impl IntoIterator<Item = AssistantContent>,
    trailing_items: impl IntoIterator<Item = AssistantContent>,
) -> Option<OneOrMany<AssistantContent>> {
    let mut content_items = reasoning_items
        .into_iter()
        .map(AssistantContent::Reasoning)
        .collect::<Vec<_>>();
    content_items.extend(text_items);
    content_items.extend(trailing_items);

    OneOrMany::from_iter_optional(content_items)
}

pub(crate) fn assistant_text_items_from_choice(
    choice: &OneOrMany<AssistantContent>,
) -> Vec<AssistantContent> {
    choice
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => (!text.text.is_empty()
                || text.additional_params.is_some())
            .then(|| AssistantContent::Text(text.clone())),
            _ => None,
        })
        .collect()
}

/// One invalid tool call surfaced mid-stream, awaiting a resolution from
/// [`AgentRun::resolve_streamed_invalid_tool_call`](super::AgentRun::resolve_streamed_invalid_tool_call).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StreamedInvalidToolCall {
    /// The rejected tool call. For a name delta this is a diagnostic call
    /// assembled from the streamed name and any buffered argument deltas.
    pub tool_call: ToolCall,
    /// Rig-generated identifier correlating this call's stream items.
    pub internal_call_id: String,
    /// Raw argument payload for diagnostics, when available.
    pub args: Option<String>,
    /// Executable Rig tools advertised to the provider for this turn.
    pub executable_tool_names: BTreeSet<String>,
    /// Tools allowed by the active tool choice for this turn.
    pub allowed_tool_names: BTreeSet<String>,
}

/// Snapshot of a streamed turn at the moment an invalid tool call appeared.
/// Used by the machine to build diagnostics and rollback messages from
/// exactly what the model has produced so far.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PartialStreamedTurn {
    /// Provider-assigned assistant message ID, when already known.
    pub message_id: Option<String>,
    /// Aggregated assistant text, when any text was streamed this turn.
    pub text: Option<String>,
    /// Accumulated reasoning, with any pending unsigned delta text assembled
    /// into a block.
    pub reasoning: Vec<Reasoning>,
    /// Tool calls already validated (or repaired) this turn.
    pub pending_tool_calls: Vec<ToolCall>,
}

impl PartialStreamedTurn {
    /// The assistant message representing this partial turn, in canonical
    /// order, including `current_tool_call` when provided. `None` when the
    /// turn has produced no representable content.
    pub(crate) fn assistant_message(&self, current_tool_call: Option<ToolCall>) -> Option<Message> {
        let text_items = match &self.text {
            Some(text) if !text.is_empty() => vec![AssistantContent::text(text.clone())],
            _ => Vec::new(),
        };
        let mut tool_items = self
            .pending_tool_calls
            .iter()
            .cloned()
            .map(AssistantContent::ToolCall)
            .collect::<Vec<_>>();
        if let Some(tool_call) = current_tool_call {
            tool_items.push(AssistantContent::ToolCall(tool_call));
        }

        let content = ordered_streaming_assistant_content(
            self.reasoning.iter().cloned(),
            text_items,
            tool_items,
        )?;
        Some(Message::Assistant {
            id: self.message_id.clone(),
            content,
        })
    }

    /// Rollback messages for a retried or skipped streamed turn: the partial
    /// assistant turn plus a user message carrying `feedback` for the invalid
    /// call and a synthetic "not executed" result for each validated peer.
    ///
    /// Infallible by construction: both messages are anchored on the invalid
    /// call (its tool call as assistant content, its result as user content),
    /// so neither can ever be empty.
    pub(crate) fn rollback_messages(
        &self,
        invalid_tool_call: ToolCall,
        feedback: String,
    ) -> (Message, Message) {
        // Assistant message in canonical order (reasoning → text → tool
        // calls), anchored on the invalid call and prepending any
        // accumulated content in reverse canonical order.
        let mut content = OneOrMany::one(AssistantContent::ToolCall(invalid_tool_call.clone()));
        for call in self.pending_tool_calls.iter().rev() {
            content.insert(0, AssistantContent::ToolCall(call.clone()));
        }
        if let Some(text) = &self.text
            && !text.is_empty()
        {
            content.insert(0, AssistantContent::text(text.clone()));
        }
        for reasoning in self.reasoning.iter().rev() {
            content.insert(0, AssistantContent::Reasoning(reasoning.clone()));
        }
        let assistant_message = Message::Assistant {
            id: self.message_id.clone(),
            content,
        };

        // User message: synthetic "not executed" results for validated peers,
        // then the invalid call's own feedback result. Anchored on the
        // feedback result with peers prepended in emission order.
        let mut retry_results = OneOrMany::one(tool_result_message(
            invalid_tool_call.id,
            invalid_tool_call.call_id,
            feedback,
        ));
        for call in self.pending_tool_calls.iter().rev() {
            retry_results.insert(0, tool_result_message(
                call.id.clone(),
                call.call_id.clone(),
                TOOL_NOT_EXECUTED_DUE_TO_INVALID_PEER.to_string(),
            ));
        }
        let user_message = Message::User {
            content: retry_results,
        };

        (assistant_message, user_message)
    }
}

/// The assembled streamed turn, fed to
/// [`AgentRun::streamed_turn`](super::AgentRun::streamed_turn).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StreamedTurn {
    /// Provider-assigned assistant message ID, when available.
    pub message_id: Option<String>,
    /// The assistant content to record in history: canonical
    /// (reasoning → text → tool calls) when the turn produced reasoning or
    /// tool calls, otherwise the provider's aggregated choice as-is.
    pub choice: OneOrMany<AssistantContent>,
    /// Executable Rig tools advertised to the provider for this turn.
    pub executable_tool_names: BTreeSet<String>,
    /// Tools allowed by the active tool choice for this turn.
    pub allowed_tool_names: BTreeSet<String>,
    /// `(tool_call_id, internal_call_id)` pairs for this turn's tool calls,
    /// in emission order. Carried into the run state so a resumed process
    /// keeps the IDs consumers already saw in tool-call deltas.
    #[serde(default)]
    pub internal_call_ids: Vec<(String, String)>,
}

/// What the machine decided about a mid-stream invalid tool call.
///
/// Deliberately exhaustive: a driver must handle every resolution, so adding
/// a variant is a breaking change by design.
#[derive(Debug)]
pub enum StreamedResolution {
    /// The tool name was repaired. Apply it via
    /// [`StreamedTurnAssembler::resolve_pending_invalid`] and keep consuming
    /// the provider stream.
    Repaired {
        /// The validated replacement tool name.
        tool_name: String,
    },
    /// The turn was rolled back (retry) or the call skipped; corrective
    /// messages are already in the history. Drain the provider stream for
    /// usage, record the completion call, then call
    /// [`AgentRun::next_step`](super::AgentRun::next_step).
    TurnAbandoned {
        /// For a skipped call, the synthetic tool result to surface to the
        /// consumer stream.
        skipped_tool_result: Option<ToolResult>,
    },
}

/// What a driver must do with one ingested stream item.
///
/// Deliberately exhaustive: a driver must handle every event, so adding a
/// variant is a breaking change by design.
#[derive(Debug, Clone)]
pub enum StreamedTurnEvent {
    /// Forward the ingested item to the consumer as-is (text, reasoning, or
    /// reasoning deltas, after accumulation).
    EmitIngested,
    /// Forward this tool-call delta. Argument deltas buffered while the tool
    /// name awaited validation are replayed through this event.
    EmitToolCallDelta {
        /// Provider-supplied tool call ID.
        id: String,
        /// Rig-generated identifier correlating this call's stream items.
        internal_call_id: String,
        /// The (possibly repaired) name or argument delta.
        content: ToolCallDeltaContent,
    },
    /// The model emitted an unknown or disallowed tool call. Resolve it via
    /// [`AgentRun::resolve_streamed_invalid_tool_call`](super::AgentRun::resolve_streamed_invalid_tool_call),
    /// then apply the outcome with
    /// [`StreamedTurnAssembler::resolve_pending_invalid`].
    InvalidToolCall(Box<StreamedInvalidToolCall>),
    /// The provider supplied its typed final payload. Record its usage (see
    /// [`AgentRun::record_streamed_completion_call`](super::AgentRun::record_streamed_completion_call));
    /// this does not establish that the provider stream reached EOF. When
    /// `emit_final` is set, the turn streamed text and the driver should buffer
    /// the final item until EOF finalizes the turn.
    Completed {
        /// Provider-reported usage for this call. Zero-valued usage means the
        /// provider reported no usage metrics.
        usage: Usage,
        /// Whether the ingested final item should be forwarded to the
        /// consumer (set when the turn streamed text).
        emit_final: bool,
    },
}

#[derive(Default)]
struct ToolCallDeltaState {
    name_validated: bool,
    buffered_arguments: Vec<String>,
}

enum PendingInvalid {
    /// A complete tool call with a disallowed name.
    FullCall {
        tool_call: Box<ToolCall>,
        internal_call_id: String,
    },
    /// A streamed tool-name delta with a disallowed name.
    NameDelta {
        id: String,
        internal_call_id: String,
    },
}

/// Sans-IO accumulator that assembles one streamed model turn. See the
/// [module docs](self) for the driving protocol.
pub struct StreamedTurnAssembler {
    executable_tool_names: BTreeSet<String>,
    allowed_tool_names: BTreeSet<String>,
    text: String,
    saw_text: bool,
    accumulated_reasoning: Vec<Reasoning>,
    pending_reasoning_delta_text: String,
    pending_reasoning_delta_id: Option<String>,
    pending_tool_calls: Vec<(ToolCall, String)>,
    delta_states: HashMap<(String, String), ToolCallDeltaState>,
    pending_invalid: Option<PendingInvalid>,
}

impl StreamedTurnAssembler {
    /// Create an assembler for one streamed turn with the tool names
    /// advertised to the provider for that turn.
    pub fn new(
        executable_tool_names: BTreeSet<String>,
        allowed_tool_names: BTreeSet<String>,
    ) -> Self {
        Self {
            executable_tool_names,
            allowed_tool_names,
            text: String::new(),
            saw_text: false,
            accumulated_reasoning: Vec::new(),
            pending_reasoning_delta_text: String::new(),
            pending_reasoning_delta_id: None,
            pending_tool_calls: Vec::new(),
            delta_states: HashMap::new(),
            pending_invalid: None,
        }
    }

    /// Aggregated assistant text streamed so far this turn (empty until the
    /// first text delta).
    pub fn aggregated_text(&self) -> &str {
        &self.text
    }

    /// Normalize a snapshot of the provider aggregate into the content that
    /// would be committed for this turn, without consuming the assembler.
    fn canonical_choice(
        &self,
        provider_choice: &OneOrMany<AssistantContent>,
    ) -> OneOrMany<AssistantContent> {
        let mut reasoning = self.accumulated_reasoning.clone();
        if reasoning.is_empty() && !self.pending_reasoning_delta_text.is_empty() {
            let mut assembled = Reasoning::new(&self.pending_reasoning_delta_text);
            if let Some(id) = self.pending_reasoning_delta_id.clone() {
                assembled = assembled.with_id(id);
            }
            reasoning.push(assembled);
        }

        if !self.pending_tool_calls.is_empty() || !reasoning.is_empty() {
            let text_items = assistant_text_items_from_choice(provider_choice);
            let tool_items = self
                .pending_tool_calls
                .iter()
                .map(|(tool_call, _)| AssistantContent::ToolCall(tool_call.clone()))
                .collect::<Vec<_>>();
            ordered_streaming_assistant_content(reasoning, text_items, tool_items)
                .unwrap_or_else(|| provider_choice.clone())
        } else {
            provider_choice.clone()
        }
    }

    /// Ingest one provider stream item and return what the driver must do.
    ///
    /// # Errors
    /// Returns an error when the provider stream is inconsistent (argument
    /// deltas finishing without a validated tool name) or when an invalid
    /// tool call is still awaiting resolution.
    pub fn ingest(
        &mut self,
        item: &StreamedAssistantContent,
    ) -> Result<Vec<StreamedTurnEvent>, CompletionError> {
        if self.pending_invalid.is_some() {
            return Err(CompletionError::ResponseError(
                "streamed turn ingested while an invalid tool call awaits resolution".to_string(),
            ));
        }

        match item {
            StreamedAssistantContent::Text(text) => {
                if !self.saw_text {
                    self.text.clear();
                    self.saw_text = true;
                }
                self.text.push_str(&text.text);
                Ok(vec![StreamedTurnEvent::EmitIngested])
            }
            StreamedAssistantContent::Reasoning(reasoning) => {
                merge_reasoning_blocks(&mut self.accumulated_reasoning, reasoning);
                Ok(vec![StreamedTurnEvent::EmitIngested])
            }
            StreamedAssistantContent::ReasoningDelta { reasoning, id } => {
                // Deltas lack signatures/encrypted content that full blocks
                // carry; mixing them into accumulated reasoning causes
                // providers like Anthropic to reject with "signature required",
                // so they are kept aside until the turn ends.
                self.pending_reasoning_delta_text.push_str(reasoning);
                if self.pending_reasoning_delta_id.is_none() {
                    self.pending_reasoning_delta_id = Some(id.clone());
                }
                Ok(vec![StreamedTurnEvent::EmitIngested])
            }
            StreamedAssistantContent::ToolCall {
                tool_call,
                internal_call_id,
            } => {
                if !self.allowed_tool_names.contains(&tool_call.function.name) {
                    let invalid = StreamedInvalidToolCall {
                        tool_call: tool_call.clone(),
                        internal_call_id: internal_call_id.clone(),
                        args: Some(json_utils::serialize_json_value(
                            &tool_call.function.arguments,
                        )),
                        executable_tool_names: self.executable_tool_names.clone(),
                        allowed_tool_names: self.allowed_tool_names.clone(),
                    };
                    self.pending_invalid = Some(PendingInvalid::FullCall {
                        tool_call: Box::new(tool_call.clone()),
                        internal_call_id: internal_call_id.clone(),
                    });
                    return Ok(vec![StreamedTurnEvent::InvalidToolCall(Box::new(invalid))]);
                }

                self.pending_tool_calls
                    .push((tool_call.clone(), internal_call_id.clone()));
                Ok(Vec::new())
            }
            StreamedAssistantContent::ToolCallDelta {
                id,
                internal_call_id,
                content,
            } => {
                let key = (id.clone(), internal_call_id.clone());
                match content {
                    ToolCallDeltaContent::Name(name) => {
                        if !self.allowed_tool_names.contains(name) {
                            let buffered_args = self
                                .delta_states
                                .get(&key)
                                .map(|state| state.buffered_arguments.join(""))
                                .unwrap_or_default();
                            let invalid = StreamedInvalidToolCall {
                                tool_call: self.name_delta_diagnostic_tool_call(
                                    id,
                                    name,
                                    &buffered_args,
                                ),
                                internal_call_id: internal_call_id.clone(),
                                args: Some(buffered_args),
                                executable_tool_names: self.executable_tool_names.clone(),
                                allowed_tool_names: self.allowed_tool_names.clone(),
                            };
                            self.pending_invalid = Some(PendingInvalid::NameDelta {
                                id: id.clone(),
                                internal_call_id: internal_call_id.clone(),
                            });
                            return Ok(vec![StreamedTurnEvent::InvalidToolCall(Box::new(invalid))]);
                        }

                        Ok(self.validate_delta_name(&key, name.clone()))
                    }
                    ToolCallDeltaContent::Delta(arguments) => {
                        let state = self.delta_states.entry(key.clone()).or_default();
                        if state.name_validated {
                            Ok(vec![StreamedTurnEvent::EmitToolCallDelta {
                                id: id.clone(),
                                internal_call_id: internal_call_id.clone(),
                                content: ToolCallDeltaContent::Delta(arguments.clone()),
                            }])
                        } else {
                            state.buffered_arguments.push(arguments.clone());
                            Ok(Vec::new())
                        }
                    }
                }
            }
            StreamedAssistantContent::Final(final_response) => {
                if let Some(err) = self.pending_delta_error() {
                    return Err(err);
                }

                let usage = final_response.usage;
                let emit_final = self.saw_text;
                self.saw_text = false;
                Ok(vec![StreamedTurnEvent::Completed { usage, emit_final }])
            }
            StreamedAssistantContent::Unknown(_) => {
                // Unmodeled provider item (e.g. a hosted-tool result): forward it
                // to the consumer but do not fold it into the accumulated
                // assistant message — there is no `AssistantContent::Unknown`, and
                // it must not perturb text/tool-call/reasoning accumulation.
                Ok(vec![StreamedTurnEvent::EmitIngested])
            }
        }
    }

    /// Apply the machine's resolution for the invalid tool call surfaced by
    /// the last [`StreamedTurnEvent::InvalidToolCall`]. For a repaired name
    /// this returns the deltas to forward (the repaired name plus any
    /// buffered argument deltas).
    pub fn resolve_pending_invalid(
        &mut self,
        resolution: &StreamedResolution,
    ) -> Vec<StreamedTurnEvent> {
        let Some(pending) = self.pending_invalid.take() else {
            return Vec::new();
        };

        match (resolution, pending) {
            (
                StreamedResolution::Repaired { tool_name },
                PendingInvalid::FullCall {
                    mut tool_call,
                    internal_call_id,
                },
            ) => {
                tool_call.function.name = tool_name.clone();
                self.pending_tool_calls.push((*tool_call, internal_call_id));
                Vec::new()
            }
            (
                StreamedResolution::Repaired { tool_name },
                PendingInvalid::NameDelta {
                    id,
                    internal_call_id,
                },
            ) => {
                let key = (id, internal_call_id);
                self.validate_delta_name(&key, tool_name.clone())
            }
            (
                StreamedResolution::TurnAbandoned { .. },
                PendingInvalid::NameDelta {
                    id,
                    internal_call_id,
                },
            ) => {
                // The abandoned call's buffered state must not trip the
                // pending-delta consistency check while usage is drained.
                self.delta_states.remove(&(id, internal_call_id));
                Vec::new()
            }
            (StreamedResolution::TurnAbandoned { .. }, PendingInvalid::FullCall { .. }) => {
                Vec::new()
            }
        }
    }

    /// Error when argument deltas were buffered for a tool call whose name
    /// never validated — a provider-stream consistency violation.
    pub fn pending_delta_error(&self) -> Option<CompletionError> {
        self.delta_states
            .iter()
            .find(|(_, state)| !state.name_validated && !state.buffered_arguments.is_empty())
            .map(|((id, internal_call_id), state)| {
                CompletionError::ResponseError(format!(
                    "streamed tool call arguments received before a validated tool name for id `{id}` and internal_call_id `{internal_call_id}` ({} buffered argument delta(s))",
                    state.buffered_arguments.len()
                ))
            })
    }

    /// Snapshot of the turn so far, for diagnostics and rollback messages.
    pub fn partial_turn(&self, message_id: Option<String>) -> PartialStreamedTurn {
        let mut reasoning = self.accumulated_reasoning.clone();
        if reasoning.is_empty() && !self.pending_reasoning_delta_text.is_empty() {
            let mut assembled = Reasoning::new(&self.pending_reasoning_delta_text);
            if let Some(id) = self.pending_reasoning_delta_id.clone() {
                assembled = assembled.with_id(id);
            }
            reasoning.push(assembled);
        }

        PartialStreamedTurn {
            message_id,
            text: self.saw_text.then(|| self.text.clone()),
            reasoning,
            pending_tool_calls: self
                .pending_tool_calls
                .iter()
                .map(|(tool_call, _)| tool_call.clone())
                .collect(),
        }
    }

    /// Assemble the completed turn. `final_choice` is the provider's
    /// aggregated choice for the turn
    /// ([`crate::streaming::StreamingCompletionResponse::choice`]).
    pub fn finish(
        self,
        message_id: Option<String>,
        final_choice: &OneOrMany<AssistantContent>,
    ) -> StreamedTurn {
        let choice = self.canonical_choice(final_choice);
        let internal_call_ids: Vec<(String, String)> = self
            .pending_tool_calls
            .iter()
            .map(|(tool_call, internal_call_id)| (tool_call.id.clone(), internal_call_id.clone()))
            .collect();

        StreamedTurn {
            message_id,
            choice,
            executable_tool_names: self.executable_tool_names,
            allowed_tool_names: self.allowed_tool_names,
            internal_call_ids,
        }
    }

    fn name_delta_diagnostic_tool_call(
        &self,
        id: &str,
        name: &str,
        buffered_args: &str,
    ) -> ToolCall {
        let diagnostic_args = if buffered_args.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(buffered_args).unwrap_or(serde_json::Value::Null)
        };
        ToolCall::new(
            id.to_string(),
            ToolFunction::new(name.to_string(), diagnostic_args),
        )
    }

    fn validate_delta_name(
        &mut self,
        key: &(String, String),
        name: String,
    ) -> Vec<StreamedTurnEvent> {
        let state = self.delta_states.entry(key.clone()).or_default();
        state.name_validated = true;
        let buffered_arguments = std::mem::take(&mut state.buffered_arguments);

        let mut events = vec![StreamedTurnEvent::EmitToolCallDelta {
            id: key.0.clone(),
            internal_call_id: key.1.clone(),
            content: ToolCallDeltaContent::Name(name),
        }];
        events.extend(buffered_arguments.into_iter().map(|arguments| {
            StreamedTurnEvent::EmitToolCallDelta {
                id: key.0.clone(),
                internal_call_id: key.1.clone(),
                content: ToolCallDeltaContent::Delta(arguments),
            }
        }));
        events
    }
}

#[cfg(test)]
#[path = "streamed_tests.rs"]
mod streamed_tests;
