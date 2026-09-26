//! The frontend vocabulary: the commands a frontend submits and the
//! stamped events it consumes, shared verbatim by every transport —
//! typed values over in-process channels, JSON lines at a serialized
//! edge. Commands are fire-and-forget with total semantics (a message
//! steers the run in flight or starts one; abort stops), so this side
//! of the protocol carries no request ids and has no rejection cases.
//! Session-scoped commands name their session explicitly (v3, ruled:
//! no consumer keeps a silent default), and events carry a stream
//! stamp — the session id — because stream order alone cannot
//! attribute concurrent producers.

use crate::events::SessionEvent;
use serde::{Deserialize, Serialize};

/// The protocol version this build speaks. The child's first line is
/// its [`ServerControlFrame::Report`] carrying this version; the
/// spawner reads it and kills an incompatible child (owner ruling
/// 2026-09-25 — the report model: children report first, spawners
/// decide). v19: the report model — `initialize`/`initialize_ack`/
/// `initialize_rejected` are deleted (commands flow from the
/// spawner's first line; startup failures are the report, an
/// unstamped `error` event, and a nonzero exit), and the replay
/// brackets are renamed `replay_begin { total }` / `replay_end`,
/// with a resumed boot replaying automatically. v10:
/// `session_created` deleted (the supersede ruling executed after
/// five versions — `session_opened` with `resumed: false` is the one
/// announcement); `run_failed` carries a typed `kind`; the turn
/// brackets and run terminals carry Unix-ms timestamps. v9:
/// extensions — the `extensions_available` startup announcement.
/// v8: skills — the `skills_available` startup announcement. v7:
/// compaction — the `compact` command and its event family (reshaped
/// in v15 into the
/// `compaction_begin`/`compaction_step`/`compaction_end` envelope).
pub const PROTOCOL_VERSION: u32 = 20;

/// Which session produced an event. The stamp is the session id
/// itself (v3: the `"main"` alias is retired — one name per session);
/// every session announces itself with a stamped `session_opened`, so
/// a consumer learns each stream name from the announce, never from
/// position after the report.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StreamId(String);

impl StreamId {
    /// A session's stream: its session id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The session id this stream names.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An event, optionally stamped with the session that produced it.
/// This is the unit the backend channel carries and — serialized
/// flat — the line a transport edge writes:
/// `{"type":"text_delta","stream":"019…",...}`. The stamp is the
/// session id (v3) and is **absent for backend-level events** (v4):
/// a fact the backend itself produced (the session catalog, host
/// failures) carries no session attribution,
/// and frontends fold unstamped frames connection-level (ruled
/// 2026-08 — no faked session ids).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventFrame {
    /// The session that produced the event (its id); `None` for
    /// backend-level events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamId>,
    /// Who produced the event when it was not the backend itself —
    /// an extension emitting into the shared grammar (v18, the
    /// routing generalization: routing is participant-blind, so the
    /// stamp is attribution, not permission). `None` on everything
    /// the backend emits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// The remaining hops this frame may cross — the livelock
    /// tripwire (2026-09 ruling): each node's intake decrements, and
    /// expiry drops the frame loudly (a misconfigured routing loop —
    /// normally it never fires). Absent means unbounded (old
    /// speakers, local-only traffic); node-originated frames carry
    /// the budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u8>,
    /// The event itself; its `type` tag flattens next to `stream`.
    #[serde(flatten)]
    pub event: SessionEvent,
}

/// A frontend command, fire-and-forget — also the whole of the
/// client's wire vocabulary (v19: with the handshake gone, a client
/// line IS a command; commands may flow from the spawner's first
/// line, before or after the child's report). Session-scoped
/// commands name their session explicitly (v3, ruled: a deliberate
/// wire break — no consumer keeps a silent default, so nothing can
/// "forget to update"); session ids arrive from
/// `sessions_available`/`session_opened`. The behavior is
/// total over the two session states:
///
/// | command               | idle                   | running                              |
/// |-----------------------|------------------------|--------------------------------------|
/// | `Message`             | starts a run           | steers (next turn boundary)          |
/// | `Abort`               | no-op                  | aborts; discards queued messages     |
/// | `InteractionResponse` | no-op (logged)         | routes the answer by id to the asker |
/// | `Checkout`            | rewinds; replays       | aborts the run; rewinds; replays     |
/// | `Compact`             | runs the box at the beat | parks; runs when the run ends      |
///
/// Outcomes are events (`user_message` for acceptance, the run
/// terminals for results); a command naming an unknown session yields
/// an unstamped `error { kind: session }` (backend-level — the
/// optional-stream ruling: the routing failure belongs to no session
/// open in this backend).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionCommand {
    /// A user message for a session.
    Message {
        /// The target session id.
        session: String,
        /// The message text.
        text: String,
    },
    /// Stop a session: abort the run in flight and discard any queued
    /// messages.
    Abort {
        /// The target session id.
        session: String,
    },
    /// Start a run over the existing conversation with no new user
    /// message (retry / continue): the loop's first drain takes
    /// whatever steers rode along, and the model answers the
    /// conversation as it stands. A no-op on an empty conversation —
    /// nothing to continue.
    Continue {
        /// The target session id.
        session: String,
    },
    /// Answer a pending `interaction_request`. Total, like every command:
    /// a response for an unknown or dead request is a logged no-op (the
    /// asker went away with its run — terminals close everything). The
    /// `payload` is the answer shaped by the asking template's
    /// convention (v4) — always an answer; the frontend never
    /// expresses dismissal (that is backend-derived). `session` is the
    /// echo of the request frame's stamp (v3's always-explicit rule);
    /// v18 makes it optional for the one answerer that has no session
    /// to name — the backend itself, routing an answer back to an
    /// extension's ask over its pipe (the id is the correlation; the
    /// routing generalization is participant-blind).
    InteractionResponse {
        /// The session whose request is being answered; absent only on
        /// the backend's routed-back answers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// The request id being answered.
        id: String,
        /// The answer payload (see `templates`).
        payload: serde_json::Value,
    },
    /// Create a fresh session in this backend. The outcome is a
    /// stamped `session_opened { resumed: false }` for the new
    /// session (one announcement shape for every path — v10; the
    /// `session_created` interim died with its one-version window) —
    /// or an unstamped `error { kind: session }` if the session
    /// cannot be built. Nothing replays; the session is empty.
    NewSession,
    /// Load a stored session (if needed) and replay it onto the event
    /// channel, stamped with its id — the pass itself is the
    /// acknowledgment. Idempotent: an already-open session re-replays.
    OpenSession {
        /// The stored session id to open.
        id: String,
    },
    /// Move a session's active chain to an entry (any entry in the
    /// session's file — an off-chain target is a branch switch). A run
    /// in flight is **aborted first** (ruled 2026-08: the user
    /// rewinding has declared the run's continuation obsolete —
    /// checkout composes abort, it never waits on a run), then the
    /// rewind executes at the session's pause point. Success emits
    /// `messages_discarded` (only what was submitted before this
    /// command), `run_aborted` (only if a run was in flight), then
    /// `checked_out` and a full replay pass; an unknown entry emits
    /// `error { kind: checkout }` immediately and changes nothing
    /// (FRONTEND.md §5).
    Checkout {
        /// The target session id.
        session: String,
        /// The entry the chain will end at (inclusive).
        entry_id: String,
    },
    /// Switch a session's model: exactly a [`ModelSelection`] — a
    /// state write that happens entirely at receive. Validated against
    /// config (an unusable ref is an immediate
    /// `error { kind: model }` — even mid-run, where a picker wants
    /// the feedback), then the register write lands at once (the
    /// `model_change` entry and the live selection, one shared-write
    /// operation) and `model_changed` follows immediately. A run in
    /// flight finishes untouched on the model it bound at run open;
    /// the next run derives the new agent. Not
    /// conversation intent: abort never touches it, and there is no
    /// pending state — what was announced is already durable.
    Model {
        /// The target session id.
        session: String,
        /// Provider id from tabit config.
        provider: String,
        /// Model id within the provider.
        model: String,
        /// Active thinking level name, when the model defines levels
        /// (`None` clears — absent on the wire means the same).
        #[serde(default)]
        thinking_level: Option<String>,
    },
    /// Run compaction now — the manual door (v7). The same machinery
    /// as the automatic doors, forced regardless of the trigger
    /// conditions and guarded only by the short-history skip. Idle, it
    /// runs at the session's next beat; mid-run it parks and runs when
    /// the run ends (compaction never aborts a run — it does not move
    /// the chain, so there is nothing to make obsolete). Outcomes: the
    /// `compaction_*` bracket. `directives` is the user's free-text
    /// guidance for THIS invocation (v16): appended to the
    /// summarization instruction, never persisted, never replayed —
    /// "focus on details relevant to task X which we will start
    /// next".
    Compact {
        /// The target session id.
        session: String,
        /// Free-text summarizer directives (see the variant docs).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        directives: Option<String>,
    },
}

/// The command tag constants — [`SessionCommand::tag`]'s values,
/// pinned in one place for the routing layer's by-type tables (the
/// command twin of the event [`tags`](crate::tags)).
pub mod command_tags {
    /// The answer to an ask: the interaction ask's kind tag.
    pub const INTERACTION_RESPONSE: &str = "interaction_response";
}

impl SessionCommand {
    /// The wire tag of one command kind — the `type` field's value
    /// (the command twin of [`SessionEvent::tag`]; the routing layer's
    /// by-type tables key on it). Exhaustive by construction: a new
    /// variant breaks this compile until it is tagged.
    #[must_use]
    pub const fn tag(&self) -> &'static str {
        match self {
            SessionCommand::Message { .. } => "message",
            SessionCommand::Abort { .. } => "abort",
            SessionCommand::Continue { .. } => "continue",
            SessionCommand::InteractionResponse { .. } => "interaction_response",
            SessionCommand::NewSession => "new_session",
            SessionCommand::OpenSession { .. } => "open_session",
            SessionCommand::Checkout { .. } => "checkout",
            SessionCommand::Model { .. } => "model",
            SessionCommand::Compact { .. } => "compact",
        }
    }
}

/// The server's non-event lines: handshake outcomes and transport-level
/// errors (as opposed to run outcomes, which are events).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerControlFrame {
    /// The child's self-report — its first line on the channel, before
    /// any event (owner ruling 2026-09-25, the report model: a spawned
    /// child can assume its spawner is there and pump, while the
    /// spawner can assume nothing until the child self-reports).
    /// Protocol-level facts only: the version. Session facts arrive by
    /// event — the boot announces itself with a stamped
    /// `session_opened` like every other session. The spawner reads
    /// the version and kills an incompatible child; a startup failure
    /// is the report, an unstamped `error` event carrying the reason,
    /// and a nonzero exit.
    Report {
        /// The protocol version this child speaks.
        protocol_version: u32,
    },
    /// A line the edge could not turn into a command. The connection
    /// stays open.
    ProtocolError {
        /// What went wrong.
        message: String,
    },
}

/// Anything the server writes at a serialized edge: a control frame or a
/// stamped event. The untagged shape is for parsers (tests, future
/// clients); writers serialize the concrete frame directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerFrame {
    /// A handshake or transport-error line.
    Control(ServerControlFrame),
    /// A stamped session event.
    Event(EventFrame),
}

/// Serialize a frame (either direction) to its wire line. Every protocol
/// type serializes — the shapes are strings, numbers, and options, and
/// the round-trip tests hold that invariant — so a failure here is an
/// internal error and crashes loudly (AGENTS.md doctrine) instead of
/// silently dropping a frame the protocol promised. One policy for every
/// edge: no call site invents its own fallback.
pub fn to_wire_line<T: Serialize + ?Sized>(value: &T) -> String {
    // Sanctioned crash (AGENTS.md doctrine): unserializable protocol
    // data is a bug; a silent skip would drop a promised frame.
    #[allow(clippy::expect_used)]
    serde_json::to_string(value).expect("protocol frames always serialize (round-trip tested)")
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
