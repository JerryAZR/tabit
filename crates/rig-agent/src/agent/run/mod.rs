//! The agent run's turn state machine — designed in `ENGINE.md` (the
//! document is the contract; changes here change it).
//!
//! [`AgentRun`] owns every *decision* the inner loop makes — turn
//! classification (final / tools / broken), admission, steering points,
//! budgets and retry streaks, and the loop-or-exit decision — without
//! performing any IO itself. The driver advances the machine by calling
//! [`AgentRun::next_step`] and acting on the returned [`AgentRunStep`]:
//!
//! - [`AgentRunStep::CallModel`]: send `history` to the model (as-is —
//!   no prompt/context split) and feed the outcome back via
//!   [`AgentRun::turn_completed`] or
//!   [`AgentRun::turn_completed_streamed`] (or [`AgentRun::broken`] /
//!   [`AgentRun::provider_error`] / [`AgentRun::terminate`]), then the
//!   model-turn hooks' verdict through [`AgentRun::accept_turn`] /
//!   [`AgentRun::veto_turn`].
//! - [`AgentRunStep::CallTools`]: execute the listed tool calls and feed
//!   the results back via [`AgentRun::tool_results`].
//! - [`AgentRunStep::DrainSteers`]: fetch every queued steering message
//!   and feed them via [`AgentRun::steered`] — the one and only drain
//!   point; every turn outcome converges here before anything else.
//! - [`AgentRunStep::Done`]: the run is complete.
//!
//! The loop is: drain → decide → model → path → drain. Entry is
//! [`AgentRun::new`] with the already-joined history; at least one turn
//! runs. Steering legality is structural: the machine offers the drain
//! exactly at [`RunState::DrainingSteers`], so draining anywhere else is
//! unrepresentable.
//!
//! Because the machine never awaits anything, it is runtime-agnostic and
//! the whole run state is `Serialize + Deserialize` (a latent property:
//! nothing serializes a run today). Serialized state embeds the full
//! conversation and carries no cross-version stability guarantee.
//!
//! `AgentRun` deliberately contains no model, tool registry, memory
//! backend, or hook stack; hook invocation is the driver's. To execute a
//! configured [`Agent`](crate::agent::Agent), use
//! [`Agent::runner`](crate::agent::Agent::runner).
//!
//! ```rust,no_run
//! use rig_agent::agent::run::{AgentRun, AgentRunStep, ModelTurn};
//! use rig_core::message::Message;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut run = AgentRun::new(vec![Message::user("What is 2+2?")]).max_turns(3);
//! loop {
//!     match run.next_step()? {
//!         AgentRunStep::CallModel { history, .. } => {
//!             // Send `history` to a model, then:
//!             // run.turn_committed(ModelTurn { .. })?;
//!             # let _ = history;
//!             # break;
//!         }
//!         AgentRunStep::CallTools { calls } => {
//!             // Execute `calls`, then: run.tool_results(results)?;
//!             # let _ = calls;
//!         }
//!         AgentRunStep::DrainSteers => {
//!             // Fetch queued steering messages, then: run.steered(msgs)?;
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
#[cfg(test)]
mod tests;

pub use output_mode::OutputMode;

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use rig_core::{
    OneOrMany,
    message::{AssistantContent, ToolCall, UserContent},
};

use crate::{
    agent::context::Context,
    agent::hook::RetryRequest,
    agent::prompt_request::{
        CompletionCall, PromptResponse, assistant_text_from_choice,
        invalid_tool_retry_user_message, is_empty_assistant_turn, tool_result_message,
    },
    completion::{Message, PromptError, Usage},
    json_utils,
};

pub use streamed::{StreamedTurn, StreamedTurnAssembler, StreamedTurnEvent};

/// Default number of times Tool output mode re-prompts the model for valid
/// structured output before finalizing best-effort (see #1928). Mirrors
/// pydantic-ai's default output-retry budget of 1.
pub(crate) const DEFAULT_OUTPUT_RETRIES: usize = 1;

/// How many consecutive failed turns the machine retries per retryable
/// failure class (model-side defects, retryable provider errors) before
/// failing the run: one retry, two attempts. A drained steer resets both
/// streaks — a present, steering user is their own circuit breaker, and
/// the budgets bound unattended loops. (ENGINE.md, error taxonomy.)
pub(crate) const TURN_RETRY_CAP: usize = 1;

/// How a provider failure classifies for the loop-or-exit decision.
/// Classified by the driver (which observes the raw error) per ENGINE.md's
/// taxonomy; the machine trusts the class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderErrorClass {
    /// Rate-limit, transient transport: retry the turn, bounded.
    Retryable,
    /// Auth, permanent quota, context overflow: fail the run.
    Terminal,
}

/// What a driver must do next to advance an [`AgentRun`].
///
/// Deliberately exhaustive: a driver must handle every step, so adding a
/// variant is a breaking change by design.
#[derive(Debug, Clone)]
pub enum AgentRunStep {
    /// Send a completion request to the model and feed the outcome back
    /// via [`AgentRun::turn_committed`] (or the failure feeds).
    CallModel {
        /// The conversation to send, as-is. The message being answered is
        /// the history's last message — a view consumers derive, not a
        /// structural field.
        history: Vec<Message>,
        /// One-based index of this model call within the run.
        turn: usize,
    },
    /// Execute these tool calls and feed the results back via
    /// [`AgentRun::tool_results`]. Calls admitted with a pre-resolved
    /// synthetic result (an unknown tool name) must return that result
    /// without executing anything.
    CallTools { calls: Vec<PendingToolCall> },
    /// Fetch every queued steering message and feed them via
    /// [`AgentRun::steered`] (an empty feed is valid). The one and only
    /// drain point.
    DrainSteers,
    /// The run is complete.
    Done(Box<PromptResponse>),
}

/// One tool call awaiting execution by the driver.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PendingToolCall {
    /// The tool call emitted by the model.
    pub tool_call: ToolCall,
    /// Pre-resolved result for calls the admission scan rejected (a tool
    /// name the model was not offered). When set, the driver must return
    /// this content as the tool result without executing the tool or
    /// invoking tool hooks — the model is told in-band and gets to fix it.
    pub preresolved_result: Option<UserContent>,
    /// Rig-generated identifier correlating this call's stream items, when
    /// the call arrived via a streamed turn. Persisted with the run state
    /// so a resumed process keeps emitting the IDs consumers already saw
    /// in tool-call deltas. Drivers generate a fresh ID when absent.
    #[serde(default)]
    pub internal_call_id: Option<String>,
}

/// A completed model turn fed back to the machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelTurn {
    /// Provider-assigned assistant message ID, when available.
    pub message_id: Option<String>,
    /// The assistant content returned by the model.
    pub choice: OneOrMany<AssistantContent>,
    /// Token usage reported by the provider for this completion request.
    pub usage: Usage,
    /// Why the provider stopped generating, when it reported a reason.
    #[serde(default)]
    pub finish_reason: Option<crate::completion::FinishReason>,
    /// Executable Rig tools advertised to the provider for this turn.
    pub executable_tool_names: BTreeSet<String>,
    /// Tools allowed by the active tool choice for this turn.
    pub allowed_tool_names: BTreeSet<String>,
}

impl ModelTurn {
    /// Create a model turn from response parts and the tool names advertised
    /// for the turn.
    pub fn new(
        message_id: Option<String>,
        choice: OneOrMany<AssistantContent>,
        usage: Usage,
        finish_reason: Option<crate::completion::FinishReason>,
        executable_tool_names: BTreeSet<String>,
        allowed_tool_names: BTreeSet<String>,
    ) -> Self {
        Self {
            message_id,
            choice,
            usage,
            finish_reason,
            executable_tool_names,
            allowed_tool_names,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum RunState {
    /// Ready to emit [`AgentRunStep::CallModel`]. Entry point (the run's
    /// opening input is settled) and every loop re-entry.
    Preparing,
    /// A model turn is in flight; waiting for its outcome feed.
    ModelTurn,
    /// The completed turn, classified and parked — folded nowhere. The
    /// model-turn hooks observe it here; their verdict (accept or veto)
    /// decides whether it ever folds (ENGINE.md, TurnParked).
    TurnParked(Box<ParkedTurn>),
    /// A committed final (tool-free) turn, awaiting the drain.
    FinalTurn,
    /// A discarded defective turn (never entered history), awaiting the
    /// drain.
    BrokenTurn,
    /// The admitted batch is executing; waiting for
    /// [`AgentRun::tool_results`].
    ExecutingTools(Vec<PendingToolCall>),
    /// The convergence: waiting for the driver's steer feed, after which
    /// the machine decides — loop or exit.
    DrainingSteers,
    /// Terminal: the run completed successfully.
    Done(Box<PromptResponse>),
    /// Terminal: the run failed.
    Failed,
}

/// A completed turn held between completion and hook acceptance. The
/// classification is computed at park time so the veto can reject a
/// tools turn without re-deriving anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ParkedTurn {
    message_id: Option<String>,
    choice: OneOrMany<AssistantContent>,
    executable_tool_names: BTreeSet<String>,
    allowed_tool_names: BTreeSet<String>,
    internal_call_ids: Vec<(String, String)>,
    /// Whether the turn carries (non-output) tool calls: `Tools` turns
    /// are not vetoable (rejecting them would strand unanswered calls —
    /// the hook contract); everything else is final-shaped.
    carries_tools: bool,
}

/// What accepting a parked turn produced — the driver's cue for the
/// durable roundtrip close (ENGINE.md, the durable roundtrip).
// `Message` is large (~288 bytes) and rides the `Final` arm; the enum is
// a transient per-turn value, so boxing it would be noise.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum AcceptOutcome {
    /// The accepted turn was final. `feedback` is the engine-authored
    /// closing message when the turn closed with a re-prompt (the
    /// output-mode policies): the model saw it, so it belongs to the
    /// roundtrip.
    Final {
        /// The engine-authored closing message, when one was folded.
        feedback: Option<Message>,
    },
    /// The accepted turn launched a tool batch; the roundtrip closes at
    /// settlement.
    Tools,
}

/// The sans-IO agent loop state machine. See the [module docs](self) for
/// the driving protocol and `ENGINE.md` for the design.
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentRun {
    max_turns: usize,
    /// Name of the synthetic output tool when the agent uses Tool output
    /// mode (see #1928). A model turn calling this tool finalizes the run
    /// with the call's arguments as the response, instead of executing it
    /// as a tool.
    #[serde(default)]
    output_tool_name: Option<String>,
    /// JSON schema the Tool-mode output must satisfy, used to re-prompt on
    /// missing required fields before finalizing best-effort (#1928).
    #[serde(default)]
    output_schema: Option<serde_json::Value>,
    /// Budget for re-prompting the model in Tool output mode when it
    /// finalizes without calling the output tool, or calls it with
    /// arguments missing required fields. Exhausting it finalizes
    /// best-effort.
    #[serde(default)]
    max_output_retries: usize,
    #[serde(default)]
    output_retries: usize,
    /// The whole conversation, joined at entry and appended by the machine
    /// (committed turns, tool results, steers, corrective feedback). The
    /// request is this conversation, as-is — the run holds a run-scoped
    /// instance of the one shared context builder (the same fold the
    /// session layer persists from).
    context: Context,
    /// Where the run's own messages begin: `messages()` is
    /// `context[entry_len..]` (the memory append at Done).
    entry_len: usize,
    current_turn: usize,
    usage: Usage,
    completion_calls: Vec<CompletionCall>,
    completion_call_index: usize,
    /// The classified provider error from the last turn, if it failed.
    /// Skipped by serde: `PromptError` is not serializable, serialization
    /// is a latent property nothing exercises, and a resumed run re-learns
    /// failures by running.
    #[serde(skip)]
    pending_error: Option<(ProviderErrorClass, PromptError)>,
    /// Consecutive discarded defective turns.
    #[serde(default)]
    defect_streak: usize,
    /// Consecutive retryable provider errors.
    #[serde(default)]
    provider_retry_streak: usize,
    /// A hook stopped the run; exits at the next decision.
    #[serde(default)]
    terminating: Option<String>,
    /// The last drain took messages (resets every retry streak).
    #[serde(default)]
    steers_drained: bool,
    /// The last committed turn was final: a Done candidate unless the
    /// drain re-opens the run.
    #[serde(default)]
    pending_final: bool,
    /// Loop unconditionally on the next decision (output-mode feedback,
    /// final-turn rejection).
    #[serde(default)]
    retry_requested: bool,
    /// The response built for the pending final turn, finalized at Done.
    #[serde(default)]
    pending_response: Option<PromptResponse>,
    /// Set once the current streamed model turn's completion call has been
    /// recorded, rejecting duplicate records; reset when the next
    /// [`AgentRunStep::CallModel`] is emitted.
    #[serde(default)]
    streamed_completion_call_recorded: bool,
    state: RunState,
}

impl AgentRun {
    /// Create a run over the already-joined history. The final message is
    /// the first turn's prompt (the outer loop's drain joined the opening
    /// batch); the machine sends the history as-is.
    ///
    /// An empty history violates the entry contract (ENGINE.md: the run is
    /// entered with at least the message being answered) and panics — an
    /// internal error, failed loud.
    pub fn new(history: Vec<Message>) -> Self {
        assert!(
            !history.is_empty(),
            "AgentRun::new: history must not be empty — the run is entered with at \
             least the message being answered"
        );
        // The run's own messages (the memory append at Done) begin after
        // the input *context*: the final entry — the message being
        // answered — is the run's opening message, not context.
        let entry_len = history.len() - 1;
        let mut context = Context::new();
        context.fold_all(history);
        Self {
            context,
            max_turns: 1,
            output_tool_name: None,
            output_schema: None,
            max_output_retries: 0,
            output_retries: 0,
            entry_len,
            current_turn: 0,
            usage: Usage::new(),
            completion_calls: Vec::new(),
            completion_call_index: 0,
            pending_error: None,
            defect_streak: 0,
            provider_retry_streak: 0,
            terminating: None,
            steers_drained: false,
            pending_final: false,
            retry_requested: false,
            pending_response: None,
            streamed_completion_call_recorded: false,
            state: RunState::Preparing,
        }
    }

    /// Set the total model-call budget. Committed model calls consume it;
    /// discarded turns return their slot. The first turn always runs (the
    /// at-least-one-turn invariant); exhaustion fails the run at the
    /// decision with [`PromptError::MaxTurnsError`].
    pub fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Configure Tool output-mode validation (#1928): the JSON schema the
    /// output-tool arguments should satisfy, and how many times to
    /// re-prompt the model — when it finalizes without calling the output
    /// tool, or calls it with arguments missing required fields — before
    /// finalizing best-effort.
    pub fn with_output_validation(
        mut self,
        output_schema: Option<serde_json::Value>,
        max_output_retries: usize,
    ) -> Self {
        self.output_schema = output_schema;
        self.max_output_retries = max_output_retries;
        self
    }

    /// Set the synthetic output-tool name for Tool output mode (see #1928).
    /// When a model turn calls this tool, the run finalizes with the call's
    /// arguments (serialized JSON) as the response.
    pub fn with_output_tool_name(mut self, name: impl Into<String>) -> Self {
        self.output_tool_name = Some(name.into());
        self
    }

    /// Set (or clear) the output-tool name in place. The driver resolves
    /// the name from the prepared request inside the run loop, where the
    /// agent's tool set (and thus the resolved output mode) is known.
    pub(crate) fn set_output_tool_name(&mut self, name: Option<String>) {
        // The name is committed once and pinned for the whole run, so the
        // request the driver builds each turn stays consistent with the
        // intercept (and a tool set that shifts mid-run cannot flip the mode).
        if self.output_tool_name.is_none() {
            self.output_tool_name = name;
        }
    }

    /// The synthetic output-tool name committed for this run, if any.
    pub(crate) fn output_tool_name(&self) -> Option<&str> {
        self.output_tool_name.as_deref()
    }

    /// Top-level `required` schema fields absent from the output-tool
    /// arguments. A lightweight structural check (not full JSON Schema
    /// validation): empty when there is no schema, no `required` array, or
    /// every required field is present. Non-object arguments (e.g. `null`)
    /// count every required field as missing.
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
    /// schema's required fields — i.e. it is acceptable structured output
    /// even though the model returned it as plain text instead of an
    /// output-tool call.
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

    // === Outcomes and steps ==============================================

    /// Advance the machine and return the next action for the driver.
    ///
    /// # Errors
    /// - [`PromptError::MaxTurnsError`] when the loop wants another model
    ///   call but the budget is exhausted (decided at the drain's exit).
    /// - [`PromptError::PromptCancelled`] when driven out of protocol (the
    ///   message names the violation).
    pub fn next_step(&mut self) -> Result<AgentRunStep, PromptError> {
        match std::mem::replace(&mut self.state, RunState::Failed) {
            RunState::Preparing => {
                // The at-least-one-turn invariant: the first turn issues
                // unconditionally; the budget gates only the loop.
                self.streamed_completion_call_recorded = false;
                self.current_turn += 1;
                self.state = RunState::ModelTurn;
                Ok(AgentRunStep::CallModel {
                    history: self.context.messages().to_vec(),
                    turn: self.current_turn,
                })
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
            RunState::FinalTurn | RunState::BrokenTurn => {
                self.state = RunState::DrainingSteers;
                Ok(AgentRunStep::DrainSteers)
            }
            RunState::DrainingSteers => {
                // Re-offered if the driver polls again before feeding; the
                // feed is what advances the machine.
                self.state = RunState::DrainingSteers;
                Ok(AgentRunStep::DrainSteers)
            }
            RunState::Done(response) => {
                let step = AgentRunStep::Done(response.clone());
                self.state = RunState::Done(response);
                Ok(step)
            }
            state @ (RunState::ModelTurn | RunState::TurnParked(_) | RunState::Failed) => {
                let reason = match &state {
                    RunState::ModelTurn => {
                        "next_step called while a model turn is pending; feed its outcome first \
                         (turn_completed / broken / provider_error / terminate)"
                    }
                    RunState::TurnParked(_) => {
                        "next_step called while a turn is parked; feed the model-turn hooks' \
                         verdict first (accept_turn / veto_turn)"
                    }
                    _ => "next_step called after the run already failed or was misdriven",
                };
                self.state = state;
                Err(self.protocol_violation(reason))
            }
        }
    }

    /// Feed a completed model turn (the blocking surface): classify and
    /// park it — folded nowhere. The model-turn hooks observe the parked
    /// turn; [`AgentRun::accept_turn`] / [`AgentRun::veto_turn`] feed
    /// their verdict.
    pub fn turn_completed(&mut self, turn: ModelTurn) -> Result<(), PromptError> {
        if !matches!(self.state, RunState::ModelTurn) {
            return Err(
                self.protocol_violation("turn_completed called without a pending CallModel step")
            );
        }
        self.record_completion_call(turn.usage, turn.finish_reason);
        self.park(
            turn.message_id,
            turn.choice,
            turn.executable_tool_names,
            turn.allowed_tool_names,
            Vec::new(),
        )
    }

    /// Feed a completed streamed model turn: classify and park it. Usage
    /// was recorded during streaming (with a zero-usage fallback here, so
    /// exactly one completion call exists per model call).
    pub fn turn_completed_streamed(&mut self, turn: StreamedTurn) -> Result<(), PromptError> {
        if !matches!(self.state, RunState::ModelTurn) {
            return Err(self.protocol_violation(
                "turn_completed_streamed called without a pending CallModel step",
            ));
        }
        if !self.streamed_completion_call_recorded {
            self.record_completion_call(Usage::new(), None);
            self.streamed_completion_call_recorded = true;
        }
        self.park(
            turn.message_id,
            turn.choice,
            turn.executable_tool_names,
            turn.allowed_tool_names,
            turn.internal_call_ids,
        )
    }

    /// The park (shared by both surfaces): classify the completed turn,
    /// reset the failure streaks (a completed turn reset them before the
    /// reorder too, veto or not), and hold everything the accept needs.
    fn park(
        &mut self,
        message_id: Option<String>,
        choice: OneOrMany<AssistantContent>,
        executable_tool_names: BTreeSet<String>,
        allowed_tool_names: BTreeSet<String>,
        internal_call_ids: Vec<(String, String)>,
    ) -> Result<(), PromptError> {
        let carries_tools = match &self.output_tool_name {
            // An output-tool call finalizes the run (Tool output mode), so
            // it does not make the turn a tools turn — it is final-shaped
            // and vetoable like any other.
            Some(output_tool_name) => choice.iter().any(|item| match item {
                AssistantContent::ToolCall(call) => call.function.name != *output_tool_name,
                _ => false,
            }),
            None => choice
                .iter()
                .any(|item| matches!(item, AssistantContent::ToolCall(_))),
        };
        self.defect_streak = 0;
        self.provider_retry_streak = 0;
        self.pending_error = None;
        self.state = RunState::TurnParked(Box::new(ParkedTurn {
            message_id,
            choice,
            executable_tool_names,
            allowed_tool_names,
            internal_call_ids,
            carries_tools,
        }));
        Ok(())
    }

    /// Take the parked turn out of the state machine, leaving `Failed`
    /// parked as the placeholder until the caller sets the real next
    /// state. A state that is not TurnParked is a protocol violation.
    fn take_parked(&mut self, caller: &str) -> Result<Box<ParkedTurn>, PromptError> {
        match std::mem::replace(&mut self.state, RunState::Failed) {
            RunState::TurnParked(parked) => Ok(parked),
            other => {
                self.state = other;
                Err(self
                    .protocol_violation(&format!("{caller} called without a parked model turn")))
            }
        }
    }

    /// Accept the parked turn — the model-turn hooks approved it. Folds
    /// and transitions per the parked classification; returns what the
    /// driver needs for the durable roundtrip close.
    pub fn accept_turn(&mut self) -> Result<AcceptOutcome, PromptError> {
        let parked = self.take_parked("accept_turn")?;
        let ParkedTurn {
            message_id,
            choice,
            executable_tool_names,
            allowed_tool_names,
            internal_call_ids,
            carries_tools,
        } = *parked;
        let items: Vec<AssistantContent> = choice.iter().cloned().collect();
        let has_tool_calls = items
            .iter()
            .any(|item| matches!(item, AssistantContent::ToolCall(_)));

        // Tool output mode: a call to the synthetic output tool finalizes
        // the run with the call's arguments as the response (#1928).
        if has_tool_calls
            && let Some(output_tool_name) = self.output_tool_name.clone()
            && let Some(tool_call) = items.iter().find_map(|item| match item {
                AssistantContent::ToolCall(tc) if tc.function.name == output_tool_name => Some(tc),
                _ => None,
            })
        {
            return self.finalize_output_turn(message_id, choice, tool_call.clone());
        }

        if !carries_tools {
            // Final turn. In Tool output mode, text that is not valid
            // structured output re-prompts with feedback while budget
            // remains; everything else commits as the Done candidate.
            if let Some(output_tool_name) = self.output_tool_name.clone()
                && !is_empty_assistant_turn(&choice)
                && self.can_reprompt_for_output()
                && !self.text_satisfies_output_schema(&assistant_text_from_choice(&choice))
            {
                self.output_retries += 1;
                self.context.fold(Message::Assistant {
                    id: message_id,
                    content: choice.clone(),
                });
                let feedback = Message::user(format!(
                    "Provide your final answer by calling the `{output_tool_name}` tool with \
                     the structured result as its arguments, not as plain text."
                ));
                self.context.fold(feedback.clone());
                self.retry_requested = true;
                self.state = RunState::FinalTurn;
                return Ok(AcceptOutcome::Final {
                    feedback: Some(feedback),
                });
            }

            if !is_empty_assistant_turn(&choice) {
                self.context.fold(Message::Assistant {
                    id: message_id,
                    content: choice.clone(),
                });
            }
            let response = PromptResponse::new(assistant_text_from_choice(&choice), self.usage)
                .with_messages(self.messages().to_vec())
                .with_completion_calls(self.completion_calls.clone())
                .with_content(choice);
            self.pending_response = Some(response);
            self.pending_final = true;
            self.state = RunState::FinalTurn;
            return Ok(AcceptOutcome::Final { feedback: None });
        }

        // Tools: fold the assistant turn, then the admission scan. A name
        // the model was not offered is a model-side mistake — the call
        // returns a synthetic in-band result naming the problem, the model
        // is told, and the run continues. Never a failure, never a pause.
        self.output_retries = 0;
        self.context.fold(Message::Assistant {
            id: message_id,
            content: choice.clone(),
        });
        let mut internal_call_ids = internal_call_ids;
        let calls: Vec<PendingToolCall> = items
            .iter()
            .filter_map(|item| match item {
                AssistantContent::ToolCall(tool_call) => {
                    let internal_call_id = internal_call_ids
                        .iter()
                        .position(|(id, _)| *id == tool_call.id)
                        .map(|index| internal_call_ids.remove(index).1);
                    let preresolved_result = (self.tool_rejected(
                        tool_call,
                        &executable_tool_names,
                        &allowed_tool_names,
                    ))
                    .map(|text| {
                        tool_result_message(tool_call.id.clone(), tool_call.call_id.clone(), text)
                    });
                    Some(PendingToolCall {
                        tool_call: tool_call.clone(),
                        preresolved_result,
                        internal_call_id,
                    })
                }
                _ => None,
            })
            .collect();
        self.state = RunState::ExecutingTools(calls);
        Ok(AcceptOutcome::Tools)
    }

    /// Veto the parked turn — a model-turn hook rejected it. `Repeat`
    /// discards the turn (it never folds); `Feedback` folds it followed
    /// by corrective feedback. Either way the run re-prompts through the
    /// drain. Rejecting a turn that carries tool calls is a protocol
    /// violation — the calls would strand unanswered (the hook
    /// contract).
    pub fn veto_turn(&mut self, request: RetryRequest) -> Result<(), PromptError> {
        let carries_tools = match &self.state {
            RunState::TurnParked(parked) => parked.carries_tools,
            _ => {
                return Err(self.protocol_violation("veto_turn called without a parked model turn"));
            }
        };
        if carries_tools {
            self.state = RunState::Failed;
            return Err(self.protocol_violation(
                "a model-turn hook vetoed a turn carrying tool calls; retry tool-free turns \
                 only (steer tool turns through the tool-call hooks)",
            ));
        }
        let parked = self.take_parked("veto_turn")?;
        let ParkedTurn {
            message_id, choice, ..
        } = *parked;
        if let RetryRequest::Feedback(feedback) = request {
            // An empty assistant turn stays out of history (the accept
            // path skips it too): there is nothing to preserve, and the
            // feedback alone carries the correction.
            if !is_empty_assistant_turn(&choice) {
                self.context.fold(Message::Assistant {
                    id: message_id,
                    content: choice,
                });
            }
            self.context.fold(Message::user(feedback));
        }
        self.pending_final = false;
        self.pending_response = None;
        self.retry_requested = true;
        self.state = RunState::FinalTurn;
        Ok(())
    }

    /// The synthetic in-band result for a call the admission scan rejects,
    /// or `None` when the call is admitted. Rejected = the name was not
    /// offered to the model or is disallowed by the active tool choice.
    fn tool_rejected(
        &self,
        tool_call: &ToolCall,
        executable_tool_names: &BTreeSet<String>,
        allowed_tool_names: &BTreeSet<String>,
    ) -> Option<String> {
        if allowed_tool_names.contains(&tool_call.function.name) {
            return None;
        }
        let mut available: Vec<&str> = executable_tool_names.iter().map(String::as_str).collect();
        available.sort_unstable();
        Some(format!(
            "unknown or disallowed tool `{}`: it was not offered to the model. Available \
             tools: {}. Call one of the available tools instead.",
            tool_call.function.name,
            available.join(", ")
        ))
    }

    /// Tool output mode's finalization policy (#1928): validate the
    /// output-tool arguments against the schema; re-prompt with feedback
    /// while budget remains, otherwise finalize — persisting the turn as
    /// the assistant's final *text* (keeping any reasoning, dropping every
    /// tool call) so replayed history carries no unanswered tool_use.
    fn finalize_output_turn(
        &mut self,
        message_id: Option<String>,
        choice: OneOrMany<AssistantContent>,
        tool_call: ToolCall,
    ) -> Result<AcceptOutcome, PromptError> {
        let output_tool_name = self
            .output_tool_name
            .clone()
            .unwrap_or_else(|| tool_call.function.name.clone());
        let args = tool_call.function.arguments.clone();
        let missing = self.missing_required_output_fields(&args);

        if !missing.is_empty() && self.can_reprompt_for_output() {
            self.output_retries += 1;
            // The rejected turn commits in full, answered by tool results:
            // the feedback rides the output call's result, siblings report
            // not-executed — replayable history, no dangling tool_use.
            self.context.fold(Message::Assistant {
                id: message_id,
                content: choice.clone(),
            });
            let feedback = format!(
                "The `{output_tool_name}` arguments were missing required field(s): {}. Call \
                 `{output_tool_name}` again with every required field.",
                missing.join(", ")
            );
            // The output-tool call is in `choice`, so the message always
            // builds; an empty one would strand the turn's calls
            // unanswerable — an internal invariant, failed loud.
            #[allow(clippy::expect_used)]
            let user_message = invalid_tool_retry_user_message(&choice, &tool_call.id, feedback)
                .expect("output-tool retry feedback answers every call in the turn");
            self.context.fold(user_message.clone());
            self.retry_requested = true;
            self.state = RunState::FinalTurn;
            return Ok(AcceptOutcome::Final {
                feedback: Some(user_message),
            });
        }

        // Finalize. The turn is persisted as text (see the method doc).
        let output = json_utils::serialize_json_value(&args);
        let output_tool_calls = choice
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    AssistantContent::ToolCall(tc) if tc.function.name == output_tool_name
                )
            })
            .count();
        let mut final_items: Vec<AssistantContent> = choice
            .iter()
            .filter(|item| !matches!(item, AssistantContent::ToolCall(_)))
            .cloned()
            .collect();
        final_items.push(AssistantContent::text(output.clone()));
        let content = OneOrMany::from_iter_optional(final_items);
        if let Some(content) = content.clone() {
            self.context.fold(Message::Assistant {
                id: message_id,
                content,
            });
        }
        let mut response = PromptResponse::new(output, self.usage)
            .with_messages(self.messages().to_vec())
            .with_completion_calls(self.completion_calls.clone())
            .with_output_tool_calls(output_tool_calls);
        if let Some(content) = content {
            response = response.with_content(content);
        }
        self.pending_response = Some(response);
        self.pending_final = true;
        self.state = RunState::FinalTurn;
        Ok(AcceptOutcome::Final { feedback: None })
    }

    /// Feed a model-side defect (a typed malformed-tool-call error): the
    /// turn is discarded — it never entered history — and the defect streak
    /// bumps. The turn slot is returned (`max_turns` counts turns the model
    /// answered).
    pub fn broken(&mut self, _reason: String) -> Result<(), PromptError> {
        if !matches!(self.state, RunState::ModelTurn) {
            return Err(self.protocol_violation("broken called without a pending CallModel step"));
        }
        self.current_turn = self.current_turn.saturating_sub(1);
        self.defect_streak += 1;
        self.pending_error = None;
        self.state = RunState::BrokenTurn;
        Ok(())
    }

    /// Feed a provider/transport failure, classified per ENGINE.md's
    /// taxonomy. Like a defect, the failed turn never entered history and
    /// its slot is returned; the decision retries (bounded) or fails.
    pub fn provider_error(
        &mut self,
        class: ProviderErrorClass,
        error: PromptError,
    ) -> Result<(), PromptError> {
        if !matches!(self.state, RunState::ModelTurn) {
            return Err(
                self.protocol_violation("provider_error called without a pending CallModel step")
            );
        }
        self.current_turn = self.current_turn.saturating_sub(1);
        self.pending_error = Some((class, error));
        self.state = RunState::DrainingSteers;
        Ok(())
    }

    /// Feed a hook stop: the run terminates with the reason at the next
    /// decision — through the drain, so queued steers land in history.
    pub fn terminate(&mut self, reason: impl Into<String>) -> Result<(), PromptError> {
        if matches!(self.state, RunState::Done(_) | RunState::Failed) {
            return Err(self.protocol_violation("terminate called after the run already finished"));
        }
        self.terminating = Some(reason.into());
        self.state = RunState::DrainingSteers;
        Ok(())
    }

    /// Feed the drain: everything the driver fetched from its steering
    /// source (an empty feed is valid). Appends the messages, resets every
    /// retry streak when any arrived, then decides — loop, Done, or
    /// Failed. This is the machine's only steering point.
    pub fn steered(&mut self, messages: Vec<Message>) -> Result<(), PromptError> {
        if !matches!(self.state, RunState::DrainingSteers) {
            return Err(self.protocol_violation("steered called outside the drain point"));
        }
        if !messages.is_empty() {
            self.context.fold_all(messages);
            self.steers_drained = true;
            self.defect_streak = 0;
            self.provider_retry_streak = 0;
        }
        self.decide()
    }

    /// The loop-or-exit decision — pure, the drain's exit transition, the
    /// one home of every loop-or-exit conditional (ENGINE.md).
    fn decide(&mut self) -> Result<(), PromptError> {
        let steers_drained = self.steers_drained;
        self.steers_drained = false;
        if let Some(reason) = self.terminating.take() {
            self.state = RunState::Failed;
            return Err(self.cancel_error(reason));
        }
        if self.retry_requested {
            self.retry_requested = false;
            return self.loop_or_fail_budget();
        }
        if let Some((class, error)) = self.pending_error.take() {
            match class {
                ProviderErrorClass::Retryable if self.provider_retry_streak < TURN_RETRY_CAP => {
                    self.provider_retry_streak += 1;
                    return self.loop_or_fail_budget();
                }
                ProviderErrorClass::Retryable => {
                    self.state = RunState::Failed;
                    return Err(self.cancel_error(format!(
                        "the provider kept failing after {} retried attempt(s); the \
                         conversation history is unchanged — resend the prompt to try \
                         again. Last failure: {error}",
                        self.provider_retry_streak,
                    )));
                }
                ProviderErrorClass::Terminal => {
                    self.state = RunState::Failed;
                    return Err(error);
                }
            }
        }
        if self.defect_streak > TURN_RETRY_CAP {
            self.state = RunState::Failed;
            let streak = self.defect_streak;
            return Err(self.cancel_error(format!(
                "the model repeatedly emitted tool calls with malformed arguments \
                 ({streak} consecutive turns discarded and retried); the \
                 conversation history is unchanged — resend the prompt to try again, or \
                 raise the model's output token limit if the calls keep getting cut."
            )));
        }
        if self.pending_final {
            if steers_drained {
                // The drain re-opened the run: the final turn stays
                // committed, the queued messages continue the conversation.
                self.pending_final = false;
                self.pending_response = None;
                return self.loop_or_fail_budget();
            }
            let response = self
                .pending_response
                .take()
                .unwrap_or_else(|| PromptResponse::new(String::new(), self.usage));
            self.state = RunState::Done(Box::new(response));
            return Ok(());
        }
        // Tool results reported (or a within-budget retry): loop.
        self.loop_or_fail_budget()
    }

    /// Loop to another model call, or fail with the budget error when the
    /// budget is exhausted. (The at-least-one-turn invariant means the
    /// first turn never passes through here.)
    fn loop_or_fail_budget(&mut self) -> Result<(), PromptError> {
        if self.current_turn >= self.max_turns {
            self.state = RunState::Failed;
            let prompt = self
                .context
                .messages()
                .last()
                .cloned()
                .unwrap_or_else(|| Message::user(String::new()));
            return Err(PromptError::MaxTurnsError {
                max_turns: self.max_turns,
                chat_history: Box::new(self.context.messages().to_vec()),
                prompt: Box::new(prompt),
            });
        }
        self.state = RunState::Preparing;
        Ok(())
    }

    /// Feed the executed tool batch's results. Results pair with pending
    /// calls by tool call ID as a multiset, so duplicate provider IDs
    /// within one turn stay answerable.
    pub fn tool_results(&mut self, results: Vec<UserContent>) -> Result<(), PromptError> {
        let RunState::ExecutingTools(pending) = &self.state else {
            return Err(
                self.protocol_violation("tool_results called without a pending CallTools step")
            );
        };
        let pending = pending.clone();
        let mut unanswered: Vec<String> = pending
            .iter()
            .map(|call| call.tool_call.id.clone())
            .collect();

        if results.is_empty() {
            self.state = RunState::Failed;
            return Err(PromptError::prompt_cancelled(
                self.context.messages().to_vec(),
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

        self.context.fold(Message::User { content });
        self.state = RunState::DrainingSteers;
        Ok(())
    }

    /// Record one provider completion call: assign it the next call index,
    /// push it, and aggregate its usage into the run total. The single home
    /// for this accounting arithmetic, shared by the non-streamed and
    /// streamed ingestion paths.
    fn record_completion_call(
        &mut self,
        usage: Usage,
        finish_reason: Option<crate::completion::FinishReason>,
    ) -> CompletionCall {
        let call = CompletionCall::new(self.completion_call_index, usage, finish_reason);
        self.completion_call_index += 1;
        self.completion_calls.push(call.clone());
        self.usage += usage;
        call
    }

    /// Record a streamed turn's completion call as soon as its usage is
    /// learned (before the turn completes). Exactly one per model call;
    /// duplicate records are protocol violations.
    pub fn record_streamed_completion_call(
        &mut self,
        usage: Usage,
        finish_reason: Option<crate::completion::FinishReason>,
    ) -> Result<CompletionCall, PromptError> {
        let recordable = matches!(self.state, RunState::ModelTurn);
        if !recordable {
            return Err(self.protocol_violation(
                "record_streamed_completion_call called without a pending CallModel step",
            ));
        }
        if self.streamed_completion_call_recorded {
            return Err(self.protocol_violation(
                "record_streamed_completion_call called twice for the same model turn",
            ));
        }
        self.streamed_completion_call_recorded = true;
        Ok(self.record_completion_call(usage, finish_reason))
    }

    // === Views ============================================================

    /// Messages accumulated by this run (everything after the entry
    /// history) — the memory append at Done. `entry_len` never exceeds
    /// `history.len()`: the machine only appends.
    pub fn messages(&self) -> &[Message] {
        let messages = self.context.messages();
        &messages[self.entry_len.min(messages.len())..]
    }

    /// The full conversation: entry history followed by the run's messages.
    pub fn full_history(&self) -> Vec<Message> {
        self.context.messages().to_vec()
    }

    /// Whether the run reached [`RunState::Done`].
    pub fn is_done(&self) -> bool {
        matches!(self.state, RunState::Done(_))
    }

    /// The final response once the run is done, without cloning it.
    pub fn response(&self) -> Option<&PromptResponse> {
        match &self.state {
            RunState::Done(response) => Some(response),
            _ => None,
        }
    }

    /// Every provider completion call this run made, with usage — including
    /// attempts that were discarded (the tokens were spent).
    pub fn completion_calls(&self) -> &[CompletionCall] {
        &self.completion_calls
    }

    /// The run's aggregated token usage so far.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// The one-based index of the model call in flight (or last issued).
    pub fn turn(&self) -> usize {
        self.current_turn
    }

    /// Build the cancellation error a driver should return when the run
    /// stops early, carrying the current full history.
    pub fn cancel_error(&self, reason: impl Into<String>) -> PromptError {
        PromptError::prompt_cancelled(self.context.messages().to_vec(), reason)
    }

    fn protocol_violation(&self, reason: &str) -> PromptError {
        PromptError::prompt_cancelled(
            self.context.messages().to_vec(),
            format!("agent run driver protocol violation: {reason}"),
        )
    }
}
