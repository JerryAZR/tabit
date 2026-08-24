//! The serializable event stream a tabit frontend consumes.
//!
//! v1 events are the item-level view of one outer loop plus session-level
//! bookkeeping. The enum is closed: the CLI/RPC surface (ROADMAP item 7)
//! ships in this workspace, so an added variant is a coordinated change,
//! not a compatibility hazard.

use crate::model::ModelSelection;
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
    /// An error condition that is not a run terminal — config trouble,
    /// persistence degrade, a failed `model`/`checkout` command. One
    /// carrier for all of them so a minimal frontend implements a single
    /// handler (show the message) while a rich one switches on `kind`.
    /// `run_failed` stays its own event (a run terminal, not an error
    /// condition). Unknown kinds fall back to generic display — the same
    /// forward-compat rule as unknown event types, which is why `kind`
    /// is an open string, not a closed enum.
    Error {
        /// The error kind (an open string; see the well-known values on
        /// [`ErrorKind`]).
        kind: String,
        /// The error, in display form.
        message: String,
        /// Kind-specific structure: `persist_degraded`'s count of
        /// records pending on disk.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending: Option<u64>,
    },
    /// Replay began: the backend is re-emitting the session's active
    /// chain as finalized live events — the same shapes a live run
    /// produces, ids included verbatim, deltas whole. `total` is the
    /// number of events the pass will emit between this and
    /// `replay_done`.
    ReplayStarted {
        /// The pass's event count (the progress denominator).
        total: u64,
    },
    /// The replay pass ended: every event it announced has been emitted.
    ReplayDone,
    /// A `checkout` succeeded: the session's active chain now ends at
    /// `entry_id` (inclusive). Followed immediately by a full replay
    /// pass bracketing the rewound chain — the pass is the re-render
    /// (`base_id: null` = the frontend drops everything it holds; the
    /// reserved suffix upgrade flips it to `Some` and shrinks the
    /// pass behind the same bracket, PROTOCOL.md v3 stage 2).
    CheckedOut {
        /// The entry the chain now ends at — the command's target.
        entry_id: String,
        /// Where the frontend may stop dropping: `null` = full
        /// re-render (today's only mode).
        #[serde(default)]
        base_id: Option<String>,
    },
    /// The session catalog, announced once at startup right after the
    /// ack's startup notes: every stored session, newest first, from a
    /// header-only listing (lazy loading — only the boot session is
    /// loaded). Minimal by ruling; a plain object fields can grow
    /// into. A brand-new session has no file yet and is absent.
    SessionsAvailable {
        /// Every stored session, newest first.
        sessions: Vec<AvailableSession>,
    },
    /// A `new_session` command succeeded: a fresh session exists in
    /// this backend, empty (nothing replays). Unstamped,
    /// backend-level — the payload carries the new session's id (the
    /// optional-stream ruling); its selection notes, if any, follow
    /// stamped with the new session's id.
    SessionCreated {
        /// The new session's id.
        id: String,
        /// The new session's file path (materializes at its first
        /// user message).
        path: String,
        /// The selection the new session starts with (a fresh
        /// resolution — it can differ from the boot's, e.g. a resumed
        /// boot model vs `default_model`).
        model: ModelSelection,
    },
    /// The active model changed (a `ModelChange` log entry replayed, or
    /// — from slice 3 — a `model` command applied).
    ModelChanged {
        /// Provider id from tabit config.
        provider: String,
        /// Model id within the provider.
        model: String,
        /// Active thinking level name, when the model defines levels.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_level: Option<String>,
    },
    /// A provider-native output item rig does not model, preserved
    /// verbatim for forwarding.
    NativeItem {
        /// The turn the item belongs to.
        turn_id: String,
        /// The raw item.
        item: Value,
    },
    /// A tool or hook asked the user a question; answered by
    /// `SessionCommand::InteractionResponse`. The vocabulary is
    /// routing-generic (v4): `ui_type` names the widget the frontend
    /// should render — `native:*` types render in every conforming
    /// frontend (see `templates`), extension types (`ext:<id>:*`)
    /// render where the extension's widgets live — and `payload` is
    /// opaque cargo the asker shaped however it wants. Several may be
    /// open at once (concurrent chains, any answer order); a run
    /// terminal closes every unanswered request — no close event,
    /// none needed. Never persisted, never replayed; the durable
    /// record is the tool result.
    InteractionRequested {
        /// Backend-minted request id (UUIDv7, like every protocol id).
        id: String,
        /// The widget type (`templates` owns the native names).
        ui_type: String,
        /// The ask, opaque to the core.
        payload: serde_json::Value,
    },
}

/// One stored session in the startup `sessions_available` catalog.
/// Minimal by ruling (fields grow when a consumer needs them); the
/// header-only listing keeps startup cheap with many sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableSession {
    /// The session id — also its event stream stamp.
    pub id: String,
    /// Creation time (RFC 3339, from the file header).
    pub created_at: String,
    /// Entries in the session file (all branches and markers).
    pub entry_count: u64,
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

impl SessionEvent {
    /// A `model`-kind error: a model preference degraded (a stale
    /// `default_model`, a resumed session's model gone) or a `model`
    /// command failed config validation.
    pub fn error_model(message: impl Into<String>) -> Self {
        Self::Error {
            kind: ErrorKind::MODEL.to_string(),
            message: message.into(),
            pending: None,
        }
    }

    /// A `session`-kind error: a session command failed (an unknown
    /// target id, an unreadable file, a session that could not be
    /// built, a failed catalog listing).
    pub fn error_session(message: impl Into<String>) -> Self {
        Self::Error {
            kind: ErrorKind::SESSION.to_string(),
            message: message.into(),
            pending: None,
        }
    }

    /// A `persist_degraded`-kind error: records are pending on disk.
    pub fn error_persist_degraded(pending: u64, message: impl Into<String>) -> Self {
        Self::Error {
            kind: ErrorKind::PERSIST_DEGRADED.to_string(),
            message: message.into(),
            pending: Some(pending),
        }
    }

    /// A `checkout`-kind error: a checkout command failed (an unknown
    /// entry, or the rewind could not apply — its message carries the
    /// detail). Nothing was discarded or moved.
    pub fn error_checkout(message: impl Into<String>) -> Self {
        Self::Error {
            kind: ErrorKind::CHECKOUT.to_string(),
            message: message.into(),
            pending: None,
        }
    }
}

/// The well-known `error` event kinds. Open on purpose: an unknown kind
/// from a newer backend displays generically instead of failing the
/// frontend.
pub struct ErrorKind;

impl ErrorKind {
    /// A model preference degraded or a `model` command failed
    /// validation.
    pub const MODEL: &'static str = "model";
    /// A session command failed: an unknown target id, an unreadable
    /// session file, a session that could not be built, a failed
    /// catalog listing.
    pub const SESSION: &'static str = "session";
    /// A `checkout` command targeted a missing entry or not a cut point.
    pub const CHECKOUT: &'static str = "checkout";
    /// Persistence degraded: this many records are pending on disk.
    pub const PERSIST_DEGRADED: &'static str = "persist_degraded";
    /// Persistence recovered: pending records reached the disk.
    pub const PERSIST_RECOVERED: &'static str = "persist_recovered";
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
