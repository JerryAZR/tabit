//! The serializable event stream a tabit frontend consumes.
//!
//! v1 events are the item-level view of one outer loop plus session-level
//! bookkeeping. The enum is closed: the CLI/RPC surface (ROADMAP item 7)
//! ships in this workspace, so an added variant is a coordinated change,
//! not a compatibility hazard.

use crate::usage::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One observable moment in a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// The user aborted the run; `output` carries whatever assistant text
    /// had arrived before the abort.
    RunAborted { output: String },
    /// A user message was accepted and recorded.
    UserMessage {
        /// The message text.
        text: String,
        /// The message's durable entry id — the id `message_queued`
        /// announced at submit (born early: minted at accept, carried into
        /// the log at drain).
        entry_id: String,
    },
    /// A message was accepted while a run is live: it steers at the next
    /// turn boundary. The submit-time acknowledgment for messages that
    /// wait — idle sends never queue (they drain immediately, so
    /// `user_message` milliseconds later is the acknowledgment). The `id`
    /// is the message's eventual entry id; the pending display drops
    /// exactly when a `user_message` or `messages_discarded` carrying it
    /// arrives.
    MessageQueued {
        /// The message's entry id, minted at accept.
        id: String,
        /// The message text.
        text: String,
    },
    /// Queued messages were discarded (a mailbox clear: abort, checkout,
    /// the prompt barrier). The pairs hand back what the user authored —
    /// ids included, so pending displays resolve by id; the messages were
    /// never part of the conversation and are not persisted.
    MessagesDiscarded {
        /// The discarded messages.
        messages: Vec<DiscardedMessage>,
    },
    /// A model call began: the request is issued. The opening bracket of a
    /// turn — every turn-scoped event until the matching `TurnCommitted`
    /// (or the turn's discard: `TurnRetried`, a run terminal) carries this
    /// turn's id. The id is the committed turn's eventual entry id (born
    /// early, ENGINE.md behavior delta 10); a turn that never commits
    /// leaves it uncommitted, and ids are never reused.
    TurnStarted {
        /// The announced turn id.
        id: String,
    },
    /// The turn closed by `TurnStarted { id }` committed: its content is
    /// final and durably recorded. A turn that ends in `TurnRetried`, a
    /// run terminal, or an abort never commits.
    TurnCommitted {
        /// The announced id of the committed turn.
        id: String,
    },
    /// A text delta from the assistant.
    TextDelta {
        /// The turn the delta belongs to.
        turn_id: String,
        /// The delta text.
        text: String,
    },
    /// A reasoning delta from the assistant.
    ReasoningDelta {
        /// The turn the delta belongs to.
        turn_id: String,
        /// Correlation id (unique within the run).
        id: String,
        /// The delta text.
        reasoning: String,
    },
    /// The model emitted a complete tool call (before execution decisions).
    ToolCall {
        /// The turn the call belongs to.
        turn_id: String,
        /// Tool name.
        name: String,
        /// Provider tool-call id.
        call_id: String,
        /// Rig correlation id for the execution.
        internal_call_id: String,
        /// The JSON arguments as a string, when parseable.
        arguments: Option<String>,
    },
    /// A tool body finished executing and its result was committed.
    ToolResult {
        /// The turn whose tool batch the result belongs to.
        turn_id: String,
        /// The result's durable entry id (minted at record time).
        entry_id: String,
        /// Tool name.
        name: String,
        /// Rig correlation id for the execution.
        internal_call_id: String,
        /// Exactly the text the model saw — the faithful copy (tools cap
        /// output at the source, failure text included), so the frontend
        /// never needs a second channel and never sees more than the
        /// model did. Text parts joined; non-text content (images) has no
        /// textual form.
        content: String,
        /// Structure only, never prose: the human-readable failure detail
        /// lives in `content` (a detail field would fork the truth and
        /// drift).
        status: ToolResultStatus,
    },
    /// A completed model turn was rejected by a hook and will be retried;
    /// any provisional output for that turn should be discarded.
    TurnRetried {
        /// The announced id of the discarded turn.
        turn_id: String,
        /// One-based model-call index of the rejected turn.
        turn: usize,
    },
    /// A completion request finished; its usage is final for that request.
    CompletionCall {
        /// The turn the request belongs to.
        turn_id: String,
        /// Input tokens reported by the provider.
        input_tokens: u64,
        /// Output tokens reported by the provider.
        output_tokens: u64,
    },
    /// A committed turn ended truncated: the provider cut generation short at
    /// its output limit (`finish_reason: length`). Informational, not a
    /// failure — the run continues exactly as usual, and a steer is the
    /// user's way to ask the model to go on.
    TurnTruncated {
        /// The turn that ended truncated.
        turn_id: String,
    },
    /// The outer loop finished successfully.
    RunFinished {
        /// The final assistant text.
        output: String,
        /// Aggregated usage across the whole run.
        usage: Usage,
    },
    /// An outer loop failed: provider stream errors, or a persistence
    /// failure after a completed run — in which case this follows
    /// `RunFinished`. Not a command outcome (commands cannot fail); the
    /// mailbox keeps draining, so later messages still run.
    RunFailed {
        /// The failure, in display form.
        message: String,
    },
    /// A provider-native output item rig does not model, preserved
    /// verbatim for forwarding.
    NativeItem {
        /// The turn the item belongs to.
        turn_id: String,
        /// The raw item.
        item: Value,
    },
    /// A tool gate (permission) or tool body asked the user a question;
    /// answered by `SessionCommand::InteractionResponse`. Several may be
    /// open at once (concurrent chains, any answer order). A run terminal
    /// closes every unanswered request — there is no close event, and none
    /// is needed: a question lives inside its tool's execution, and the run
    /// always ends in exactly one terminal. Never persisted, never
    /// replayed; the durable record is the tool result.
    InteractionRequested {
        /// Backend-minted request id (UUIDv7, like every protocol id).
        id: String,
        /// Short card heading (e.g. "Run command?").
        title: String,
        /// The question or the content under review (e.g. the command).
        body: String,
        /// Button-style answers; empty for pure free-text asks.
        options: Vec<InteractionOption>,
        /// Whether an optional free-text answer/explanation is invited;
        /// a present text is delivered to the model.
        free_text: bool,
    },
}

/// One button-style answer on an interaction request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InteractionOption {
    /// The answer's label; the response echoes it in `option`.
    pub label: String,
    /// Display hint, when present.
    pub description: Option<String>,
}

/// One discarded queued message, handed back by
/// [`SessionEvent::MessagesDiscarded`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscardedMessage {
    /// The entry id the message carried from accept time.
    pub id: String,
    /// The message text, for salvage as a draft.
    pub text: String,
}

/// The structured outcome of one tool execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolResultStatus {
    /// The tool body ran and returned normally.
    Success,
    /// No successful run of the body: an execution error, a refusal, a
    /// runtime skip, or a synthetic failure report. The detail is the
    /// event's `content`.
    Failed {
        /// The tool's exit status, when it reported one numerically
        /// (bash: the process exit code).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i64>,
    },
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
