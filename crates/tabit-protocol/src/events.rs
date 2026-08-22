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
    },
    /// A text delta from the assistant.
    TextDelta {
        /// The delta text.
        text: String,
    },
    /// A reasoning delta from the assistant.
    ReasoningDelta {
        /// Correlation id (unique within the run).
        id: String,
        /// The delta text.
        reasoning: String,
    },
    /// The model emitted a complete tool call (before execution decisions).
    ToolCall {
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
        /// Tool name.
        name: String,
        /// Rig correlation id for the execution.
        internal_call_id: String,
    },
    /// A completed model turn was rejected by a hook and will be retried;
    /// any provisional output for that turn should be discarded.
    TurnRetried {
        /// One-based model-call index of the rejected turn.
        turn: usize,
    },
    /// A completion request finished; its usage is final for that request.
    CompletionCall {
        /// Input tokens reported by the provider.
        input_tokens: u64,
        /// Output tokens reported by the provider.
        output_tokens: u64,
    },
    /// A committed turn ended truncated: the provider cut generation short at
    /// its output limit (`finish_reason: length`). Informational, not a
    /// failure — the run continues exactly as usual, and a steer is the
    /// user's way to ask the model to go on.
    TurnTruncated,
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

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
