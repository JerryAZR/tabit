//! A sans-IO, steppable, serializable state machine for the agent prompt loop.
//!
//! [`AgentRun`] owns every *decision* the agent loop makes — turn counting,
//! tool-call validation, invalid tool-call recovery, chat-history threading,
//! usage aggregation and final response construction — without performing any
//! IO itself. A driver advances the machine by calling [`AgentRun::next_step`]
//! and acting on the returned [`AgentRunStep`]:
//!
//! - [`AgentRunStep::CallModel`]: send a completion request to the model and
//!   feed the result back via [`AgentRun::model_response`].
//! - [`AgentRunStep::CallTools`]: execute the listed tool calls (with whatever
//!   concurrency the driver chooses) and feed the results back via
//!   [`AgentRun::tool_results`].
//! - [`AgentRunStep::Done`]: the run is complete.
//!
//! Because the machine never awaits anything, it is runtime-agnostic and the
//! whole run state is `Serialize + Deserialize`: a driver can serialize a run
//! between steps (for example while tool calls are pending), persist it, and
//! resume it later in another process. Note that serialized run state embeds
//! the full conversation accumulated so far — persisting it inherits whatever
//! sensitivity the conversation content has — and the serialization format
//! carries no cross-version stability guarantee yet: resume with the same rig
//! version that suspended the run.
//!
//! `AgentRun` deliberately contains no model, tool registry, memory backend, or
//! hook stack. Hand-driving it is a low-level provider integration: the caller
//! owns all IO and any lifecycle policy. To execute a configured [`Agent`](crate::agent::Agent)
//! with its hooks, tools, retrieval, and memory, use
//! [`Agent::runner`](crate::agent::Agent::runner); constructing an `AgentRun`
//! directly is not an alternate way to execute an `Agent`.
//!
//! [`crate::completion::Prompt::prompt`] and
//! [`Agent::runner`](crate::agent::Agent::runner) drive this machine internally;
//! the same machine can be driven by hand for custom provider control flow:
//!
//! ```rust,no_run
//! use rig_agent::agent::run::{AgentRun, AgentRunStep, ModelTurn, ModelTurnOutcome};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut run = AgentRun::new("What is 2+2?").max_turns(3);
//! loop {
//!     match run.next_step()? {
//!         AgentRunStep::CallModel { prompt, history, .. } => {
//!             // Send `prompt` + `history` to a model, then:
//!             // run.model_response(ModelTurn { ... })?;
//!             # let _ = (prompt, history);
//!             # break;
//!         }
//!         AgentRunStep::CallTools { calls } => {
//!             // Execute `calls`, then: run.tool_results(results)?;
//!             # let _ = calls;
//!         }
//!         AgentRunStep::Done(response) => {
//!             println!("{}", response.output);
//!             break;
//!         }
//!     }
//! }
//! # Ok(())
//! # }
//! ```

pub mod output_mode;
pub mod streamed;

pub use output_mode::OutputMode;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use rig_core::{
    OneOrMany,
    message::{AssistantContent, ToolCall, ToolChoice, ToolResult, ToolResultContent, UserContent},
};

use crate::{
    agent::hook::{InvalidToolCallAction, InvalidToolCallContext, RetryRequest},
    agent::prompt_request::{
        CompletionCall, PromptResponse, TOOL_NOT_EXECUTED_DUE_TO_INVALID_PEER,
        assistant_text_from_choice, build_full_history, build_history_for_request,
        invalid_tool_retry_user_message, is_empty_assistant_turn, tool_result_message,
    },
    completion::{Message, PromptError, Usage},
    json_utils,
};

pub use streamed::{
    PartialStreamedTurn, StreamedInvalidToolCall, StreamedResolution, StreamedTurn,
    StreamedTurnAssembler, StreamedTurnEvent,
};

/// Build the canonical "the model called a tool that isn't available" error.
/// The identical shape is raised from every recovery-rejection path
/// (`resolve_invalid_tool_call`, `resolve_streamed_invalid_tool_call`) and the
/// streamed fail-fast in `streamed_turn`; this collapses the copied struct
/// literal to one place while leaving each caller's control flow untouched.
fn unknown_tool_call_error(
    tool_name: String,
    available_tools: Vec<String>,
    allowed_tools: Vec<String>,
    chat_history: Vec<Message>,
) -> PromptError {
    PromptError::UnknownToolCall {
        tool_name,
        available_tools,
        allowed_tools,
        chat_history: Box::new(chat_history),
    }
}

/// Default number of times Tool output mode re-prompts the model for valid
/// structured output before finalizing best-effort (see #1928). Mirrors
/// pydantic-ai's default output-retry budget of 1.
pub(crate) const DEFAULT_OUTPUT_RETRIES: usize = 1;

/// What a driver must do next to advance an [`AgentRun`].
///
/// Deliberately exhaustive: a driver must handle every step, so adding a
/// variant is a breaking change by design.
#[derive(Debug, Clone)]
pub enum AgentRunStep {
    /// Send a completion request to the model and feed the result back via
    /// [`AgentRun::model_response`].
    CallModel {
        /// The prompt message for this turn (the latest message in the run).
        prompt: Message,
        /// The chat history preceding `prompt`: the caller-provided input
        /// history followed by messages accumulated by earlier turns.
        history: Vec<Message>,
        /// One-based index of this model call within the run.
        turn: usize,
    },
    /// Execute these tool calls and feed the results back via
    /// [`AgentRun::tool_results`].
    CallTools {
        /// The tool calls of the current assistant turn, in emission order.
        calls: Vec<PendingToolCall>,
    },
    /// The run is complete.
    Done(PromptResponse),
}

/// One tool call awaiting execution by the driver.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PendingToolCall {
    /// The tool call emitted by the model (with any repaired tool name applied).
    pub tool_call: ToolCall,
    /// Pre-resolved result for tool calls suppressed by invalid tool-call
    /// recovery. When set, the driver must return this content as the tool
    /// result without executing the tool or invoking tool hooks.
    pub preresolved_result: Option<UserContent>,
    /// Rig-generated identifier correlating this call's stream items, when
    /// the call arrived via a streamed turn. Persisted with the run state so
    /// a resumed process keeps emitting the IDs consumers already saw in
    /// tool-call deltas. Drivers generate a fresh ID when absent.
    #[serde(default)]
    pub internal_call_id: Option<String>,
}

/// A completed model turn fed back to [`AgentRun::model_response`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelTurn {
    /// Provider-assigned assistant message ID, when available.
    pub message_id: Option<String>,
    /// The assistant content returned by the model.
    pub choice: OneOrMany<AssistantContent>,
    /// Token usage reported by the provider for this completion request.
    pub usage: Usage,
    /// Executable Rig tools advertised to the provider for this turn.
    pub executable_tool_names: BTreeSet<String>,
    /// Tools allowed by the active [`ToolChoice`] for this turn.
    pub allowed_tool_names: BTreeSet<String>,
}

impl ModelTurn {
    /// Create a model turn from response parts and the tool names advertised
    /// for the turn.
    pub fn new(
        message_id: Option<String>,
        choice: OneOrMany<AssistantContent>,
        usage: Usage,
        executable_tool_names: BTreeSet<String>,
        allowed_tool_names: BTreeSet<String>,
    ) -> Self {
        Self {
            message_id,
            choice,
            usage,
            executable_tool_names,
            allowed_tool_names,
        }
    }
}

/// Result of feeding a model turn (or an invalid tool-call resolution) into
/// the machine.
///
/// Deliberately exhaustive: a driver must handle every outcome, so adding a
/// variant is a breaking change by design.
#[derive(Debug)]
pub enum ModelTurnOutcome {
    /// The turn was accepted. Unless `response_hook_suppressed` is set, the
    /// driver should run its completion-response hook now, then call
    /// [`AgentRun::next_step`].
    ///
    /// `response_hook_suppressed` is set when invalid tool-call recovery
    /// (repair or skip) modified the turn, matching the agent loop's behavior
    /// of not invoking `on_completion_response` for recovered turns.
    Continue {
        /// Whether the driver should suppress its completion-response hook.
        response_hook_suppressed: bool,
    },
    /// The model emitted a tool call that is unknown or disallowed for this
    /// turn. The driver must decide how to recover (typically by asking its
    /// invalid tool-call hook) and answer via
    /// [`AgentRun::resolve_invalid_tool_call`].
    NeedsResolution(InvalidToolCallContext),
    /// The turn was rolled back with corrective feedback appended to the
    /// history. Call [`AgentRun::next_step`] to obtain the retry
    /// [`AgentRunStep::CallModel`].
    TurnRetried,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResolvingState {
    message_id: Option<String>,
    /// The unmodified model output, used for diagnostic histories and retry
    /// messages (repairs are never reflected in those).
    original_choice: OneOrMany<AssistantContent>,
    /// Working copy of the assistant content; repairs rename tool calls here.
    items: Vec<AssistantContent>,
    /// Index of the next item to validate.
    next_index: usize,
    executable_tool_names: BTreeSet<String>,
    allowed_tool_names: BTreeSet<String>,
    /// Synthetic tool results for skipped tool calls, keyed by tool call ID.
    skipped: BTreeMap<String, UserContent>,
    recovered: bool,
    any_skipped: bool,
    has_tool_calls: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TurnState {
    message_id: Option<String>,
    items: Vec<AssistantContent>,
    has_tool_calls: bool,
    skipped: BTreeMap<String, UserContent>,
    /// `(tool_call_id, internal_call_id)` pairs for streamed turns, in
    /// emission order; empty for non-streamed turns.
    #[serde(default)]
    internal_call_ids: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum RunState {
    /// Ready to emit [`AgentRunStep::CallModel`].
    PreparingRequest,
    /// Waiting for [`AgentRun::model_response`].
    AwaitingModel,
    /// Scanning the model turn's tool calls for validity; may be waiting for
    /// [`AgentRun::resolve_invalid_tool_call`].
    ResolvingToolCalls(Box<ResolvingState>),
    /// The turn was accepted; ready to emit [`AgentRunStep::CallTools`] or
    /// [`AgentRunStep::Done`].
    AwaitingAdvance(Box<TurnState>),
    /// Waiting for [`AgentRun::tool_results`] for these pending tool calls.
    /// Carrying the calls in the state keeps a serialized run self-contained:
    /// a resumed process re-obtains them from [`AgentRun::next_step`].
    ExecutingTools(Vec<PendingToolCall>),
    /// Terminal: the run completed successfully.
    Done(Box<PromptResponse>),
    /// Terminal: the run returned an error.
    Failed,
}

/// The sans-IO agent loop state machine. See the [module docs](self) for the
/// driving protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRun {
    max_turns: usize,
    max_invalid_tool_call_retries: usize,
    tool_choice: Option<ToolChoice>,
    /// Name of the synthetic output tool when the agent uses Tool output mode
    /// (see #1928). A model turn calling this tool finalizes the run with the
    /// call's arguments as the response, instead of executing it as a tool.
    #[serde(default)]
    output_tool_name: Option<String>,
    /// JSON schema the Tool-mode output must satisfy, used to re-prompt on
    /// missing required fields before finalizing best-effort (#1928).
    #[serde(default)]
    output_schema: Option<serde_json::Value>,
    /// Budget for re-prompting the model in Tool output mode when it finalizes
    /// without calling the output tool, or calls it with arguments missing
    /// required fields. Exhausting it finalizes best-effort.
    #[serde(default)]
    max_output_retries: usize,
    #[serde(default)]
    output_retries: usize,
    chat_history: Option<Vec<Message>>,
    new_messages: Vec<Message>,
    current_turn: usize,
    usage: Usage,
    completion_calls: Vec<CompletionCall>,
    completion_call_index: usize,
    invalid_tool_call_retries: usize,
    /// Set while a streamed turn rollback awaits its completion-call record;
    /// see [`AgentRun::record_streamed_completion_call`].
    #[serde(default)]
    rollback_pending: bool,
    /// Set once the current streamed model turn's completion call has been
    /// recorded, rejecting duplicate records; reset when the next
    /// [`AgentRunStep::CallModel`] is emitted.
    #[serde(default)]
    streamed_completion_call_recorded: bool,
    state: RunState,
}

impl AgentRun {
    /// Create a run for one prompt with no input history, a one-model-call
    /// budget, and no invalid tool-call retries.
    pub fn new(prompt: impl Into<Message>) -> Self {
        Self {
            max_turns: 1,
            max_invalid_tool_call_retries: 0,
            tool_choice: None,
            output_tool_name: None,
            output_schema: None,
            max_output_retries: 0,
            output_retries: 0,
            chat_history: None,
            new_messages: vec![prompt.into()],
            current_turn: 0,
            usage: Usage::new(),
            completion_calls: Vec::new(),
            completion_call_index: 0,
            invalid_tool_call_retries: 0,
            rollback_pending: false,
            streamed_completion_call_recorded: false,
            state: RunState::PreparingRequest,
        }
    }

    /// Set the input chat history preceding the prompt.
    pub fn with_history(mut self, history: Vec<Message>) -> Self {
        self.chat_history = Some(history);
        self
    }

    /// Set the total model-call budget, including the initial call and every
    /// retry or continuation. A budget of zero emits no model calls. Exceeding
    /// the budget makes [`AgentRun::next_step`] return
    /// [`PromptError::MaxTurnsError`].
    pub fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Configure Tool output-mode validation (#1928): the JSON schema the
    /// output-tool arguments should satisfy, and how many times to re-prompt the
    /// model — when it finalizes without calling the output tool, or calls it
    /// with arguments missing required fields — before finalizing best-effort.
    pub fn with_output_validation(
        mut self,
        output_schema: Option<serde_json::Value>,
        max_output_retries: usize,
    ) -> Self {
        self.output_schema = output_schema;
        self.max_output_retries = max_output_retries;
        self
    }

    /// Top-level `required` schema fields absent from the output-tool arguments.
    /// A lightweight structural check (not full JSON Schema validation): empty
    /// when there is no schema, no `required` array, or every required field is
    /// present. Non-object arguments (e.g. `null`) count every required field as
    /// missing.
    fn missing_required_output_fields(&self, args: &serde_json::Value) -> Vec<String> {
        let Some(required) = self
            .output_schema
            .as_ref()
            .and_then(|schema| schema.get("required"))
            .and_then(|required| required.as_array())
        else {
            return Vec::new();
        };
        let object = args.as_object();
        required
            .iter()
            .filter_map(|field| field.as_str())
            .filter(|field| object.is_none_or(|object| !object.contains_key(*field)))
            .map(str::to_owned)
            .collect()
    }

    /// Whether `text` already parses as a JSON object satisfying the output
    /// schema's required fields — i.e. it is acceptable structured output even
    /// though the model returned it as plain text instead of an output-tool call.
    fn text_satisfies_output_schema(&self, text: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(text.trim())
            .ok()
            .is_some_and(|value| self.missing_required_output_fields(&value).is_empty())
    }

    /// Whether the run may re-prompt for valid Tool-mode output: both the
    /// output-retry budget and the total model-call budget must remain.
    /// Otherwise, finalize best-effort rather than surface a max-turns error.
    fn can_reprompt_for_output(&self) -> bool {
        self.output_retries < self.max_output_retries && self.current_turn < self.max_turns
    }

    /// Roll the run back to re-prompt for valid output (#1928). The caller must
    /// have already appended the assistant turn and the corrective feedback
    /// message to the history. Consumes one output-retry, then emits the retry
    /// [`AgentRunStep::CallModel`].
    fn reprompt_for_output(&mut self) -> Result<AgentRunStep, PromptError> {
        self.output_retries += 1;
        self.state = RunState::PreparingRequest;
        self.next_step()
    }

    /// Set the retry budget for [`InvalidToolCallAction::Retry`]
    /// resolutions. Invalid tool-call retries also consume the total model-call
    /// budget.
    pub fn max_invalid_tool_call_retries(mut self, retries: usize) -> Self {
        self.max_invalid_tool_call_retries = retries;
        self
    }

    /// Set the tool choice active for this run. Used to reject
    /// [`InvalidToolCallAction::Skip`] resolutions under
    /// [`ToolChoice::None`] and reported in invalid tool-call contexts.
    pub fn with_tool_choice(mut self, tool_choice: ToolChoice) -> Self {
        self.tool_choice = Some(tool_choice);
        self
    }

    /// Set the synthetic output-tool name for Tool output mode (see #1928).
    /// When a model turn calls this tool, the run finalizes with the call's
    /// arguments (serialized JSON) as the response.
    pub fn with_output_tool_name(mut self, name: impl Into<String>) -> Self {
        self.output_tool_name = Some(name.into());
        self
    }

    /// Set (or clear) the output-tool name in place. The driver resolves the
    /// name from the prepared request inside the run loop, where the agent's
    /// tool set (and thus the resolved output mode) is known.
    pub(crate) fn set_output_tool_name(&mut self, name: Option<String>) {
        // The name is committed once and pinned for the whole run, so the
        // request the driver builds each turn stays consistent with the
        // intercept (and a tool set that shifts mid-run cannot flip the mode).
        if self.output_tool_name.is_none() {
            self.output_tool_name = name;
        }
    }

    /// The synthetic output-tool name committed for this run, if any. The driver
    /// passes this back when preparing later turns so Tool output mode stays
    /// pinned even if the per-turn tool set changes (see #1928).
    pub(crate) fn output_tool_name(&self) -> Option<&str> {
        self.output_tool_name.as_deref()
    }

    /// Aggregated token usage across all completed model calls so far.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Number of model calls emitted so far (including retries).
    pub fn turn(&self) -> usize {
        self.current_turn
    }

    /// Details for each completed model call so far.
    pub fn completion_calls(&self) -> &[CompletionCall] {
        &self.completion_calls
    }

    /// Messages accumulated by this run (the prompt plus all assistant turns
    /// and tool results), excluding the input history.
    pub fn messages(&self) -> &[Message] {
        &self.new_messages
    }

    /// Canonical content for the accepted model turn awaiting advancement.
    pub(crate) fn accepted_turn_choice(&self) -> Option<OneOrMany<AssistantContent>> {
        let RunState::AwaitingAdvance(turn) = &self.state else {
            return None;
        };

        OneOrMany::from_iter_optional(turn.items.clone())
    }

    /// Reject the accepted, tool-free model turn and prepare another model call.
    ///
    /// [`RetryRequest::Repeat`] discards the rejected assistant response and
    /// reuses the same prompt and preceding history with fresh request
    /// preparation. [`RetryRequest::Feedback`] records the rejected response
    /// followed by corrective user feedback. Canonical empty assistant turns
    /// are omitted from history, matching normal turn advancement. Both modes
    /// preserve completion-call and usage accounting, and the next call consumes
    /// the existing total model-call budget.
    ///
    /// Tool-bearing turns cannot be retried through this operation because
    /// preserving them without matching tool results would create invalid
    /// provider-visible history. Use tool-call hooks to steer those turns.
    pub fn retry_model_turn(&mut self, request: RetryRequest) -> Result<(), PromptError> {
        let turn = match std::mem::replace(&mut self.state, RunState::Failed) {
            RunState::AwaitingAdvance(turn) => turn,
            other => {
                self.state = other;
                return Err(self.protocol_violation(
                    "retry_model_turn called without an accepted turn awaiting advancement",
                ));
            }
        };

        if turn.has_tool_calls {
            return Err(PromptError::prompt_cancelled(
                self.full_history(),
                "model-turn retry does not support tool-bearing model turns; use tool-call hooks instead",
            ));
        }

        match request {
            RetryRequest::Repeat => {}
            RetryRequest::Feedback(feedback) => {
                let Some(content) = OneOrMany::from_iter_optional(turn.items) else {
                    return Err(PromptError::prompt_cancelled(
                        self.full_history(),
                        "model-turn retry lost the rejected assistant content",
                    ));
                };
                if !is_empty_assistant_turn(&content) {
                    self.new_messages.push(Message::Assistant {
                        id: turn.message_id,
                        content,
                    });
                }
                self.new_messages.push(Message::user(feedback));
            }
        }

        self.state = RunState::PreparingRequest;
        Ok(())
    }

    /// The full conversation: input history followed by [`Self::messages`].
    pub fn full_history(&self) -> Vec<Message> {
        build_full_history(self.chat_history.as_deref(), self.new_messages.clone())
    }

    /// Whether the run reached [`AgentRunStep::Done`].
    pub fn is_done(&self) -> bool {
        matches!(self.state, RunState::Done(_))
    }

    /// The final response once the run is done, without cloning it.
    /// [`AgentRun::next_step`] in the done state returns an owned clone
    /// (including the full accumulated message history); prefer this when
    /// only inspecting the result.
    pub fn response(&self) -> Option<&PromptResponse> {
        match &self.state {
            RunState::Done(response) => Some(response),
            _ => None,
        }
    }

    /// Build the cancellation error a driver should return when one of its
    /// hooks terminates the run, carrying the current full history.
    pub fn cancel_error(&self, reason: impl Into<String>) -> PromptError {
        PromptError::prompt_cancelled(self.full_history(), reason)
    }

    /// The invalid tool call currently awaiting
    /// [`AgentRun::resolve_invalid_tool_call`], if any. Useful to re-derive
    /// the resolution context after deserializing a suspended run.
    pub fn pending_invalid_tool_call(&self) -> Option<InvalidToolCallContext> {
        let RunState::ResolvingToolCalls(resolving) = &self.state else {
            return None;
        };
        let AssistantContent::ToolCall(tool_call) = resolving.items.get(resolving.next_index)?
        else {
            return None;
        };
        if resolving
            .allowed_tool_names
            .contains(&tool_call.function.name)
        {
            return None;
        }

        Some(InvalidToolCallContext {
            tool_name: tool_call.function.name.clone(),
            tool_call_id: Some(tool_call.id.clone()),
            internal_call_id: None,
            args: Some(json_utils::serialize_json_value(
                &tool_call.function.arguments,
            )),
            available_tools: resolving.executable_tool_names.iter().cloned().collect(),
            allowed_tools: resolving.allowed_tool_names.iter().cloned().collect(),
            tool_choice: self.tool_choice.clone(),
            chat_history: self.diagnostic_history(resolving),
            is_streaming: false,
        })
    }

    /// Advance the machine and return the next action for the driver.
    ///
    /// # Errors
    /// - [`PromptError::MaxTurnsError`] when the total model-call budget is exhausted.
    /// - [`PromptError::PromptCancelled`] when the machine is driven out of
    ///   protocol (for example, calling this while a model response is
    ///   pending).
    pub fn next_step(&mut self) -> Result<AgentRunStep, PromptError> {
        match std::mem::replace(&mut self.state, RunState::Failed) {
            RunState::PreparingRequest => {
                let Some((prompt_ref, history_for_turn)) = self.new_messages.split_last() else {
                    return Err(PromptError::prompt_cancelled(
                        self.full_history(),
                        "malformed agent run state: `new_messages` is empty in the \
                         `PreparingRequest` state, but it must end with the pending prompt — \
                         the resumed run state is invalid",
                    ));
                };
                let prompt = prompt_ref.clone();

                if self.current_turn >= self.max_turns {
                    return Err(PromptError::MaxTurnsError {
                        max_turns: self.max_turns,
                        chat_history: self.full_history().into(),
                        prompt: prompt.into(),
                    });
                }

                let history =
                    build_history_for_request(self.chat_history.as_deref(), history_for_turn);
                self.current_turn += 1;
                self.rollback_pending = false;
                self.streamed_completion_call_recorded = false;
                self.state = RunState::AwaitingModel;
                Ok(AgentRunStep::CallModel {
                    prompt,
                    history,
                    turn: self.current_turn,
                })
            }
            RunState::AwaitingAdvance(turn_state) => {
                let TurnState {
                    message_id,
                    items,
                    has_tool_calls,
                    skipped,
                    mut internal_call_ids,
                } = *turn_state;
                let Some(choice) = OneOrMany::from_iter_optional(items.clone()) else {
                    return Err(PromptError::prompt_cancelled(
                        self.full_history(),
                        "malformed agent run state: the pending model turn \
                         (`AwaitingAdvance`) has no assistant content — `items` must hold at \
                         least one entry; the resumed run state is invalid",
                    ));
                };

                // Tool output mode (#1928): a call to the synthetic output tool
                // finalizes the run with the call's arguments as the response,
                // instead of executing it as a tool. First match wins; any
                // sibling tool calls in the same turn are dropped.
                if has_tool_calls
                    && let Some(output_tool_name) = self.output_tool_name.clone()
                    && let Some(tool_call) = items.iter().find_map(|item| match item {
                        AssistantContent::ToolCall(tc) if tc.function.name == output_tool_name => {
                            Some(tc)
                        }
                        _ => None,
                    })
                {
                    let output_tool_calls = items
                        .iter()
                        .filter(|item| {
                            matches!(
                                item,
                                AssistantContent::ToolCall(tc)
                                    if tc.function.name == output_tool_name
                            )
                        })
                        .count();
                    let args = tool_call.function.arguments.clone();
                    let tool_call_id = tool_call.id.clone();
                    let output = json_utils::serialize_json_value(&args);

                    // Validate the output against the schema's required fields and
                    // re-prompt while budget remains, so a model that omits fields
                    // gets a chance to fix it before we finalize best-effort.
                    let missing = self.missing_required_output_fields(&args);
                    if !missing.is_empty() && self.can_reprompt_for_output() {
                        self.new_messages.push(Message::Assistant {
                            id: message_id,
                            content: choice.clone(),
                        });
                        let feedback = format!(
                            "The `{output_tool_name}` arguments were missing required field(s): \
                             {}. Call `{output_tool_name}` again with every required field.",
                            missing.join(", ")
                        );
                        if let Some(user_message) =
                            invalid_tool_retry_user_message(&choice, &tool_call_id, feedback)
                        {
                            self.new_messages.push(user_message);
                        }
                        return self.reprompt_for_output();
                    }

                    // Finalize. The turn is persisted as the assistant's final
                    // *text* (keeping any reasoning, dropping every tool call)
                    // rather than the raw output-tool call. Otherwise the saved
                    // history would carry an unanswered tool_use, which providers
                    // reject when the conversation is replayed on a later turn.
                    let mut final_items: Vec<AssistantContent> = items
                        .iter()
                        .filter(|item| !matches!(item, AssistantContent::ToolCall(_)))
                        .cloned()
                        .collect();
                    final_items.push(AssistantContent::text(output.clone()));
                    let final_content = OneOrMany::from_iter_optional(final_items);
                    if let Some(content) = final_content.clone() {
                        self.new_messages.push(Message::Assistant {
                            id: message_id,
                            content,
                        });
                    }

                    let mut response = PromptResponse::new(output, self.usage)
                        .with_messages(self.new_messages.clone())
                        .with_completion_calls(self.completion_calls.clone())
                        .with_output_tool_calls(output_tool_calls);
                    if let Some(content) = final_content {
                        response = response.with_content(content);
                    }
                    self.state = RunState::Done(Box::new(response.clone()));
                    return Ok(AgentRunStep::Done(response));
                }

                if !is_empty_assistant_turn(&choice) {
                    self.new_messages.push(Message::Assistant {
                        id: message_id,
                        content: choice.clone(),
                    });
                }

                if has_tool_calls {
                    // The model is making progress with real tools, so reset the
                    // output-retry budget: it is per finalization attempt, not a
                    // single per-run allowance an early stray turn could burn
                    // before the model genuinely needs to produce output (#1928).
                    self.output_retries = 0;
                    let calls: Vec<PendingToolCall> = items
                        .iter()
                        .filter_map(|item| match item {
                            AssistantContent::ToolCall(tool_call) => {
                                // Consume pairs positionally so duplicate
                                // provider IDs within one turn stay
                                // distinguishable.
                                let internal_call_id = internal_call_ids
                                    .iter()
                                    .position(|(id, _)| *id == tool_call.id)
                                    .map(|index| internal_call_ids.remove(index).1);
                                Some(PendingToolCall {
                                    tool_call: tool_call.clone(),
                                    preresolved_result: skipped.get(&tool_call.id).cloned(),
                                    internal_call_id,
                                })
                            }
                            _ => None,
                        })
                        .collect();
                    self.state = RunState::ExecutingTools(calls.clone());
                    Ok(AgentRunStep::CallTools { calls })
                } else {
                    // Tool output mode (#1928): the model produced a final text
                    // answer without calling the output tool. Re-prompt while
                    // budget remains so it returns structured output; the
                    // assistant text was already appended above, so just add the
                    // corrective feedback. Empty turns finalize best-effort.
                    //
                    // But if the text already *is* valid output (parses as JSON
                    // with every required field), accept it rather than wasting a
                    // turn — the model answered correctly, just via the wrong
                    // channel.
                    if let Some(output_tool_name) = self.output_tool_name.clone()
                        && !is_empty_assistant_turn(&choice)
                        && self.can_reprompt_for_output()
                        && !self.text_satisfies_output_schema(&assistant_text_from_choice(&choice))
                    {
                        let feedback = format!(
                            "Provide your final answer by calling the `{output_tool_name}` tool \
                             with the structured result as its arguments, not as plain text."
                        );
                        self.new_messages.push(Message::user(feedback));
                        return self.reprompt_for_output();
                    }

                    let response =
                        PromptResponse::new(assistant_text_from_choice(&choice), self.usage)
                            .with_messages(self.new_messages.clone())
                            .with_completion_calls(self.completion_calls.clone())
                            .with_content(choice.clone());
                    self.state = RunState::Done(Box::new(response.clone()));
                    Ok(AgentRunStep::Done(response))
                }
            }
            RunState::ExecutingTools(calls) => {
                // Idempotent, like Done: a process resuming a serialized run
                // re-obtains the pending tool calls from the state itself.
                let step = AgentRunStep::CallTools {
                    calls: calls.clone(),
                };
                self.state = RunState::ExecutingTools(calls);
                Ok(step)
            }
            RunState::Done(response) => {
                let step = AgentRunStep::Done((*response).clone());
                self.state = RunState::Done(response);
                Ok(step)
            }
            state @ (RunState::AwaitingModel | RunState::ResolvingToolCalls(_)) => {
                let reason = match &state {
                    RunState::AwaitingModel => {
                        "next_step called while a model response is pending; feed it via model_response first"
                    }
                    _ => {
                        "next_step called while an invalid tool-call resolution is pending; answer it via resolve_invalid_tool_call first"
                    }
                };
                self.state = state;
                Err(self.protocol_violation(reason))
            }
            RunState::Failed => Err(self.protocol_violation(
                "next_step called after the run already failed or was misdriven",
            )),
        }
    }

    /// Feed the model's response for the pending [`AgentRunStep::CallModel`].
    ///
    /// Records the completion call and aggregates usage, then validates the
    /// turn's tool calls against the advertised tool names. See
    /// [`ModelTurnOutcome`] for what the driver must do next.
    pub fn model_response(&mut self, turn: ModelTurn) -> Result<ModelTurnOutcome, PromptError> {
        if !matches!(self.state, RunState::AwaitingModel) {
            return Err(
                self.protocol_violation("model_response called without a pending CallModel step")
            );
        }
        if self.streamed_completion_call_recorded {
            return Err(self.protocol_violation(
                "model_response called after record_streamed_completion_call for the same turn; feed streamed turns via streamed_turn",
            ));
        }

        self.record_completion_call(turn.usage);

        let items: Vec<AssistantContent> = turn.choice.iter().cloned().collect();
        let has_tool_calls = items
            .iter()
            .any(|item| matches!(item, AssistantContent::ToolCall(_)));

        self.state = RunState::ResolvingToolCalls(Box::new(ResolvingState {
            message_id: turn.message_id,
            original_choice: turn.choice,
            items,
            next_index: 0,
            executable_tool_names: turn.executable_tool_names,
            allowed_tool_names: turn.allowed_tool_names,
            skipped: BTreeMap::new(),
            recovered: false,
            any_skipped: false,
            has_tool_calls,
        }));

        self.advance_resolution()
    }

    /// Record one provider completion call: assign it the next call index,
    /// push it, and aggregate its usage into the run total. The single home for
    /// this accounting arithmetic, shared by the non-streamed and streamed
    /// ingestion paths. Callers own the once-per-turn `streamed_completion_call_recorded`
    /// guard/flag; this helper never touches it, so it cannot be mistaken for
    /// "a completion call happened" and re-introduce a double count.
    fn record_completion_call(&mut self, usage: Usage) -> CompletionCall {
        let call = CompletionCall::new(self.completion_call_index, usage);
        self.completion_call_index += 1;
        self.completion_calls.push(call);
        self.usage += usage;
        call
    }

    /// Park an accepted model turn in [`RunState::AwaitingAdvance`]. Both the
    /// non-streamed (`advance_resolution`) and streamed (`streamed_turn`)
    /// ingestion paths converge here, differing only in the `skipped` map and
    /// the streamed `internal_call_ids`.
    fn finalize_turn(
        &mut self,
        message_id: Option<String>,
        items: Vec<AssistantContent>,
        has_tool_calls: bool,
        skipped: BTreeMap<String, UserContent>,
        internal_call_ids: Vec<(String, String)>,
    ) {
        self.state = RunState::AwaitingAdvance(Box::new(TurnState {
            message_id,
            items,
            has_tool_calls,
            skipped,
            internal_call_ids,
        }));
    }

    /// Answer a pending [`ModelTurnOutcome::NeedsResolution`].
    ///
    /// Applies the agent loop's recovery semantics:
    /// - [`InvalidToolCallAction::Fail`] fails the run with
    ///   [`PromptError::UnknownToolCall`].
    /// - [`InvalidToolCallAction::Retry`] rolls the turn back with
    ///   corrective feedback while budget remains, consuming the total
    ///   model-call budget.
    /// - [`InvalidToolCallAction::Repair`] renames the tool call; the
    ///   repaired name is revalidated against the allowed tools.
    /// - [`InvalidToolCallAction::Stop`] cancels the run with
    ///   `PromptError::prompt_cancelled` and the supplied reason.
    /// - [`InvalidToolCallAction::Skip`] records a synthetic tool result
    ///   and suppresses execution of every tool call in the turn. Rejected
    ///   under [`ToolChoice::None`].
    pub fn resolve_invalid_tool_call(
        &mut self,
        action: InvalidToolCallAction,
    ) -> Result<ModelTurnOutcome, PromptError> {
        // Take the resolving state; rejection paths below restore it so an
        // out-of-protocol call does not corrupt a drivable run.
        let mut resolving = match std::mem::replace(&mut self.state, RunState::Failed) {
            RunState::ResolvingToolCalls(resolving) => resolving,
            other => {
                self.state = other;
                return Err(self.protocol_violation(
                    "resolve_invalid_tool_call called without a pending invalid tool call",
                ));
            }
        };
        let tool_call = match resolving.items.get(resolving.next_index) {
            Some(AssistantContent::ToolCall(tool_call))
                if !resolving
                    .allowed_tool_names
                    .contains(&tool_call.function.name) =>
            {
                tool_call.clone()
            }
            _ => {
                let index = resolving.next_index;
                self.state = RunState::ResolvingToolCalls(resolving);
                return Err(self.protocol_violation(&format!(
                    "resolve_invalid_tool_call: `items[{index}]` is not a pending invalid \
                     tool call — check `pending_invalid_tool_call()` first; a resumed run \
                     with a malformed `next_index` also lands here"
                )));
            }
        };

        let diagnostic_history = self.diagnostic_history(&resolving);
        let executable_tool_names: Vec<String> =
            resolving.executable_tool_names.iter().cloned().collect();
        let allowed_tool_names: Vec<String> =
            resolving.allowed_tool_names.iter().cloned().collect();

        match action {
            InvalidToolCallAction::Fail => Err(unknown_tool_call_error(
                tool_call.function.name,
                executable_tool_names,
                allowed_tool_names,
                diagnostic_history,
            )),
            InvalidToolCallAction::Retry { feedback } => {
                if self.invalid_tool_call_retries >= self.max_invalid_tool_call_retries {
                    return Err(unknown_tool_call_error(
                        tool_call.function.name,
                        executable_tool_names,
                        allowed_tool_names,
                        diagnostic_history,
                    ));
                }
                self.invalid_tool_call_retries += 1;

                self.new_messages.push(Message::Assistant {
                    id: resolving.message_id.clone(),
                    content: resolving.original_choice.clone(),
                });
                let Some(user_message) = invalid_tool_retry_user_message(
                    &resolving.original_choice,
                    &tool_call.id,
                    feedback,
                ) else {
                    return Err(PromptError::prompt_cancelled(
                        diagnostic_history,
                        "invalid tool call retry produced no retry messages",
                    ));
                };
                self.new_messages.push(user_message);
                self.state = RunState::PreparingRequest;
                Ok(ModelTurnOutcome::TurnRetried)
            }
            InvalidToolCallAction::Repair { tool_name } => {
                if !allowed_tool_names.contains(&tool_name) {
                    return Err(unknown_tool_call_error(
                        tool_name,
                        executable_tool_names,
                        allowed_tool_names,
                        diagnostic_history,
                    ));
                }
                if let Some(AssistantContent::ToolCall(tool_call)) =
                    resolving.items.get_mut(resolving.next_index)
                {
                    tool_call.function.name = tool_name;
                }
                resolving.recovered = true;
                self.state = RunState::ResolvingToolCalls(resolving);
                self.advance_resolution()
            }
            InvalidToolCallAction::Stop { reason } => {
                self.state = RunState::Failed;
                Err(PromptError::prompt_cancelled(diagnostic_history, reason))
            }
            InvalidToolCallAction::Skip { reason } => {
                if matches!(self.tool_choice, Some(ToolChoice::None)) {
                    return Err(unknown_tool_call_error(
                        tool_call.function.name,
                        executable_tool_names,
                        allowed_tool_names,
                        diagnostic_history,
                    ));
                }
                let user_content = if let Some(call_id) = tool_call.call_id.clone() {
                    UserContent::tool_result_with_call_id(
                        tool_call.id.clone(),
                        call_id,
                        OneOrMany::one(reason.into()),
                    )
                } else {
                    UserContent::tool_result(tool_call.id.clone(), OneOrMany::one(reason.into()))
                };
                resolving.skipped.insert(tool_call.id.clone(), user_content);
                resolving.recovered = true;
                resolving.any_skipped = true;
                resolving.next_index += 1;
                self.state = RunState::ResolvingToolCalls(resolving);
                self.advance_resolution()
            }
        }
    }

    /// Discard the pending invalid tool call without marking the turn as
    /// recovered.
    ///
    /// This is the extractor driver's fallback after every invalid-tool hook
    /// has declined to act. Keeping it distinct from [`InvalidToolCallAction::Skip`]
    /// preserves the extractor's legacy response semantics: unrelated calls
    /// disappear, a sibling output call can still finalize the turn, and
    /// response observers still receive the canonical response fields.
    pub(crate) fn ignore_invalid_tool_call(&mut self) -> Result<ModelTurnOutcome, PromptError> {
        let mut resolving = match std::mem::replace(&mut self.state, RunState::Failed) {
            RunState::ResolvingToolCalls(resolving) => resolving,
            other => {
                self.state = other;
                return Err(self.protocol_violation(
                    "ignore_invalid_tool_call called without a pending invalid tool call",
                ));
            }
        };

        match resolving.items.get(resolving.next_index) {
            Some(AssistantContent::ToolCall(tool_call))
                if !resolving
                    .allowed_tool_names
                    .contains(&tool_call.function.name) => {}
            _ => {
                let index = resolving.next_index;
                self.state = RunState::ResolvingToolCalls(resolving);
                return Err(self.protocol_violation(&format!(
                    "ignore_invalid_tool_call: `items[{index}]` is not a pending invalid \
                     tool call — check `pending_invalid_tool_call()` first; a resumed run \
                     with a malformed `next_index` also lands here"
                )));
            }
        }

        resolving.items.remove(resolving.next_index);
        resolving.has_tool_calls = resolving
            .items
            .iter()
            .any(|item| matches!(item, AssistantContent::ToolCall(_)));
        if resolving.items.is_empty() {
            resolving.items.push(AssistantContent::text(""));
        }
        self.state = RunState::ResolvingToolCalls(resolving);
        self.advance_resolution()
    }

    /// Feed the tool results for the pending [`AgentRunStep::CallTools`].
    ///
    /// Results may be in any order; they are appended as a single user
    /// message, matching what providers expect for parallel tool calls. Each
    /// result must be a tool result answering one of the pending calls, and
    /// every pending call must be answered — exactly what providers require
    /// to accept the next request.
    pub fn tool_results(&mut self, results: Vec<UserContent>) -> Result<(), PromptError> {
        let RunState::ExecutingTools(pending) = &self.state else {
            return Err(
                self.protocol_violation("tool_results called without a pending CallTools step")
            );
        };
        // Match results against pending calls by tool call ID as a multiset,
        // so duplicate provider IDs within one turn stay answerable.
        let mut unanswered: Vec<String> = pending
            .iter()
            .map(|call| call.tool_call.id.clone())
            .collect();

        if results.is_empty() {
            self.state = RunState::Failed;
            return Err(PromptError::prompt_cancelled(
                self.full_history(),
                "tool execution produced no tool results",
            ));
        }
        for result in &results {
            let UserContent::ToolResult(tool_result) = result else {
                return Err(self.protocol_violation(
                    "tool_results received content that is not a tool result",
                ));
            };
            let Some(index) = unanswered.iter().position(|id| *id == tool_result.id) else {
                return Err(self.protocol_violation(&format!(
                    "tool_results received a result for unknown or already-answered tool call id `{}`",
                    tool_result.id
                )));
            };
            unanswered.swap_remove(index);
        }
        if !unanswered.is_empty() {
            return Err(self.protocol_violation(&format!(
                "tool_results left pending tool call id(s) unanswered: {unanswered:?}"
            )));
        }

        // `results` is non-empty (checked above), so construction succeeds.
        let Some(content) = OneOrMany::from_iter_optional(results) else {
            return Err(
                self.protocol_violation("internal: tool results vanished during validation")
            );
        };

        self.new_messages.push(Message::User { content });
        self.state = RunState::PreparingRequest;
        Ok(())
    }

    /// Scan forward for the next invalid tool call; finish the turn when the
    /// scan completes.
    fn advance_resolution(&mut self) -> Result<ModelTurnOutcome, PromptError> {
        let mut resolving = match std::mem::replace(&mut self.state, RunState::Failed) {
            RunState::ResolvingToolCalls(resolving) => resolving,
            other => {
                self.state = other;
                return Err(self.protocol_violation(
                    "internal: advance_resolution outside of tool-call resolution",
                ));
            }
        };
        while let Some(item) = resolving.items.get(resolving.next_index) {
            match item {
                AssistantContent::ToolCall(tool_call)
                    if !resolving
                        .allowed_tool_names
                        .contains(&tool_call.function.name) =>
                {
                    break;
                }
                _ => resolving.next_index += 1,
            }
        }

        if resolving.next_index < resolving.items.len() {
            self.state = RunState::ResolvingToolCalls(resolving);
            return match self.pending_invalid_tool_call() {
                Some(context) => Ok(ModelTurnOutcome::NeedsResolution(context)),
                None => Err(self.protocol_violation(
                    "internal: pending invalid tool call could not be derived",
                )),
            };
        }

        let ResolvingState {
            message_id,
            items,
            mut skipped,
            recovered,
            any_skipped,
            has_tool_calls,
            ..
        } = *resolving;

        // When any tool call was skipped, none of the turn's tool calls
        // execute: peers get a synthetic "not executed" result.
        if any_skipped {
            for item in &items {
                if let AssistantContent::ToolCall(tool_call) = item {
                    skipped.entry(tool_call.id.clone()).or_insert_with(|| {
                        tool_result_message(
                            tool_call.id.clone(),
                            tool_call.call_id.clone(),
                            TOOL_NOT_EXECUTED_DUE_TO_INVALID_PEER.to_string(),
                        )
                    });
                }
            }
        }

        self.finalize_turn(message_id, items, has_tool_calls, skipped, Vec::new());
        Ok(ModelTurnOutcome::Continue {
            response_hook_suppressed: recovered,
        })
    }

    // ── Streamed-turn entry points ──────────────────────────────────────
    // Paired with [`streamed::StreamedTurnAssembler`]; see that module's
    // docs for the full driving protocol.

    /// Record one provider completion call for a streamed turn.
    ///
    /// Streamed turns learn usage from the provider's final stream event —
    /// including for turns abandoned by invalid tool-call recovery, where the
    /// stream is drained for usage after the rollback — so recording is
    /// decoupled from turn ingestion. Valid while a model response is pending
    /// or between a turn rollback and the next [`AgentRunStep::CallModel`];
    /// aggregates `usage` into the run total. Zero-valued usage means the
    /// provider reported no usage metrics.
    pub fn record_streamed_completion_call(
        &mut self,
        usage: Usage,
    ) -> Result<CompletionCall, PromptError> {
        let recordable = matches!(self.state, RunState::AwaitingModel)
            || (matches!(self.state, RunState::PreparingRequest) && self.rollback_pending);
        if !recordable {
            return Err(self.protocol_violation(
                "record_streamed_completion_call called without a pending or rolled-back CallModel step",
            ));
        }
        if self.streamed_completion_call_recorded {
            return Err(self.protocol_violation(
                "record_streamed_completion_call called twice for the same model turn",
            ));
        }
        self.streamed_completion_call_recorded = true;

        Ok(self.record_completion_call(usage))
    }

    /// The recovery-hook context for an invalid tool call surfaced
    /// mid-stream by a [`streamed::StreamedTurnAssembler`].
    pub fn streamed_invalid_tool_call_context(
        &self,
        partial: &PartialStreamedTurn,
        invalid: &StreamedInvalidToolCall,
    ) -> InvalidToolCallContext {
        InvalidToolCallContext {
            tool_name: invalid.tool_call.function.name.clone(),
            tool_call_id: Some(invalid.tool_call.id.clone()),
            internal_call_id: Some(invalid.internal_call_id.clone()),
            args: invalid.args.clone(),
            available_tools: invalid.executable_tool_names.iter().cloned().collect(),
            allowed_tools: invalid.allowed_tool_names.iter().cloned().collect(),
            tool_choice: self.tool_choice.clone(),
            chat_history: self
                .streamed_diagnostic_history(partial, Some(invalid.tool_call.clone())),
            is_streaming: true,
        }
    }

    /// Resolve an invalid tool call surfaced mid-stream.
    ///
    /// Applies the same recovery semantics as
    /// [`AgentRun::resolve_invalid_tool_call`], but rollback messages are
    /// assembled from the partial streamed turn — exactly what the model has
    /// produced so far — and a successful retry or skip abandons the turn
    /// (see [`StreamedResolution`]) instead of finishing it.
    pub fn resolve_streamed_invalid_tool_call(
        &mut self,
        partial: &PartialStreamedTurn,
        invalid: &StreamedInvalidToolCall,
        action: InvalidToolCallAction,
    ) -> Result<StreamedResolution, PromptError> {
        if !matches!(self.state, RunState::AwaitingModel) {
            return Err(self.protocol_violation(
                "resolve_streamed_invalid_tool_call called without a pending CallModel step",
            ));
        }

        let diagnostic_history =
            self.streamed_diagnostic_history(partial, Some(invalid.tool_call.clone()));
        let executable_tool_names: Vec<String> =
            invalid.executable_tool_names.iter().cloned().collect();
        let allowed_tool_names: Vec<String> = invalid.allowed_tool_names.iter().cloned().collect();

        match action {
            InvalidToolCallAction::Fail => {
                self.state = RunState::Failed;
                Err(unknown_tool_call_error(
                    invalid.tool_call.function.name.clone(),
                    executable_tool_names,
                    allowed_tool_names,
                    diagnostic_history,
                ))
            }
            InvalidToolCallAction::Retry { feedback } => {
                if self.invalid_tool_call_retries >= self.max_invalid_tool_call_retries {
                    self.state = RunState::Failed;
                    return Err(unknown_tool_call_error(
                        invalid.tool_call.function.name.clone(),
                        executable_tool_names,
                        allowed_tool_names,
                        diagnostic_history,
                    ));
                }
                self.invalid_tool_call_retries += 1;

                let (assistant_message, user_message) =
                    partial.rollback_messages(invalid.tool_call.clone(), feedback);
                self.new_messages.push(assistant_message);
                self.new_messages.push(user_message);
                self.rollback_pending = true;
                self.state = RunState::PreparingRequest;
                Ok(StreamedResolution::TurnAbandoned {
                    skipped_tool_result: None,
                })
            }
            InvalidToolCallAction::Repair { tool_name } => {
                if !invalid.allowed_tool_names.contains(&tool_name) {
                    self.state = RunState::Failed;
                    return Err(unknown_tool_call_error(
                        tool_name,
                        executable_tool_names,
                        allowed_tool_names,
                        diagnostic_history,
                    ));
                }
                Ok(StreamedResolution::Repaired { tool_name })
            }
            InvalidToolCallAction::Stop { reason } => {
                self.state = RunState::Failed;
                Err(PromptError::prompt_cancelled(diagnostic_history, reason))
            }
            InvalidToolCallAction::Skip { reason } => {
                if matches!(self.tool_choice, Some(ToolChoice::None)) {
                    self.state = RunState::Failed;
                    return Err(unknown_tool_call_error(
                        invalid.tool_call.function.name.clone(),
                        executable_tool_names,
                        allowed_tool_names,
                        diagnostic_history,
                    ));
                }

                // Synthetic skip reason: emit verbatim text, matching the
                // non-streamed `resolve_invalid_tool_call` skip path (parity) and
                // avoiding re-parsing a rejection message as structured output.
                let skipped_tool_result = ToolResult {
                    id: invalid.tool_call.id.clone(),
                    call_id: invalid.tool_call.call_id.clone(),
                    content: OneOrMany::one(ToolResultContent::text(reason.clone())),
                };
                let (assistant_message, user_message) =
                    partial.rollback_messages(invalid.tool_call.clone(), reason);
                self.new_messages.push(assistant_message);
                self.new_messages.push(user_message);
                self.rollback_pending = true;
                self.state = RunState::PreparingRequest;
                Ok(StreamedResolution::TurnAbandoned {
                    skipped_tool_result: Some(skipped_tool_result),
                })
            }
        }
    }

    /// Feed the assembled streamed turn for the pending
    /// [`AgentRunStep::CallModel`].
    ///
    /// Remaining tool calls are validated fail-fast — mid-stream resolution
    /// already had recovery-hook access — and the turn then advances through
    /// [`AgentRun::next_step`] exactly like a non-streamed one.
    pub fn streamed_turn(&mut self, turn: StreamedTurn) -> Result<(), PromptError> {
        if !matches!(self.state, RunState::AwaitingModel) {
            return Err(
                self.protocol_violation("streamed_turn called without a pending CallModel step")
            );
        }

        // Guarantee exactly one CompletionCall per model call: drivers that
        // never learned usage (no record before the turn completed) still get
        // the call recorded, with no reported usage.
        if !self.streamed_completion_call_recorded {
            // `Usage::new()` is the additive identity for `Usage`'s `AddAssign`,
            // so routing the no-usage fallback through `record_completion_call`
            // leaves the run total unchanged while unifying the accounting.
            self.record_completion_call(Usage::new());
            self.streamed_completion_call_recorded = true;
        }

        let items: Vec<AssistantContent> = turn.choice.iter().cloned().collect();
        let has_tool_calls = items
            .iter()
            .any(|item| matches!(item, AssistantContent::ToolCall(_)));

        for item in &items {
            let AssistantContent::ToolCall(tool_call) = item else {
                continue;
            };
            if !turn.allowed_tool_names.contains(&tool_call.function.name) {
                let mut diagnostic_messages = self.new_messages.clone();
                if !is_empty_assistant_turn(&turn.choice) {
                    diagnostic_messages.push(Message::Assistant {
                        id: turn.message_id.clone(),
                        content: turn.choice.clone(),
                    });
                }
                let diagnostic_history =
                    build_full_history(self.chat_history.as_deref(), diagnostic_messages);
                self.state = RunState::Failed;
                return Err(unknown_tool_call_error(
                    tool_call.function.name.clone(),
                    turn.executable_tool_names.iter().cloned().collect(),
                    turn.allowed_tool_names.iter().cloned().collect(),
                    diagnostic_history,
                ));
            }
        }

        self.finalize_turn(
            turn.message_id,
            items,
            has_tool_calls,
            BTreeMap::new(),
            turn.internal_call_ids,
        );
        Ok(())
    }

    /// Diagnostic history for a streamed turn: the run's messages plus the
    /// partial assistant turn under inspection.
    fn streamed_diagnostic_history(
        &self,
        partial: &PartialStreamedTurn,
        current_tool_call: Option<ToolCall>,
    ) -> Vec<Message> {
        let mut messages = self.new_messages.clone();
        if let Some(assistant) = partial.assistant_message(current_tool_call) {
            messages.push(assistant);
        }
        build_full_history(self.chat_history.as_deref(), messages)
    }

    /// History used for invalid tool-call diagnostics: the run's messages plus
    /// the unmodified assistant turn under inspection.
    fn diagnostic_history(&self, resolving: &ResolvingState) -> Vec<Message> {
        let mut diagnostic_messages = self.new_messages.clone();
        diagnostic_messages.push(Message::Assistant {
            id: resolving.message_id.clone(),
            content: resolving.original_choice.clone(),
        });
        build_full_history(self.chat_history.as_deref(), diagnostic_messages)
    }

    fn protocol_violation(&self, reason: &str) -> PromptError {
        PromptError::prompt_cancelled(
            self.full_history(),
            format!("agent run driver protocol violation: {reason}"),
        )
    }
}

#[cfg(test)]
mod tests;
