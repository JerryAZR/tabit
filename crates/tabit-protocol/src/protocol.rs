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
use crate::model::ModelSelection;
use serde::{Deserialize, Serialize};

/// The protocol version this build speaks. Clients declare theirs in
/// [`ClientFrame::Initialize`]; a mismatch rejects the connection at the
/// handshake.
pub const PROTOCOL_VERSION: u32 = 4;

/// Which session produced an event. The stamp is the session id
/// itself (v3: the `"main"` alias is retired — one name per session);
/// the boot session's id arrives in `initialize_ack`, so a consumer
/// knows every stream name before its first event frame.
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
/// a fact the backend itself produced (the session catalog,
/// `session_created`, host failures) carries no session attribution,
/// and frontends fold unstamped frames connection-level (ruled
/// 2026-08 — no faked session ids).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventFrame {
    /// The session that produced the event (its id); `None` for
    /// backend-level events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamId>,
    /// The event itself; its `type` tag flattens next to `stream`.
    #[serde(flatten)]
    pub event: SessionEvent,
}

/// A frontend command, fire-and-forget. Session-scoped commands name
/// their session explicitly (v3, ruled: a deliberate wire break — no
/// consumer keeps a silent default, so nothing can "forget to
/// update"); the boot session's id arrives in `initialize_ack`, other
/// ids from `sessions_available`/`session_created`. The behavior is
/// total over the two session states:
///
/// | command               | idle                   | running                              |
/// |-----------------------|------------------------|--------------------------------------|
/// | `Message`             | starts a run           | steers (next turn boundary)          |
/// | `Abort`               | no-op                  | aborts; discards queued messages     |
/// | `InteractionResponse` | no-op (logged)         | routes the answer by id to the asker |
/// | `Checkout`            | rewinds; replays       | parks; executes at the run's terminal |
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
    /// Answer a pending `interaction_request`. Total, like every command:
    /// a response for an unknown or dead request is a logged no-op (the
    /// asker went away with its run — terminals close everything). The
    /// `payload` is the answer shaped by the asking template's
    /// convention (v4) — always an answer; the frontend never
    /// expresses dismissal (that is backend-derived).
    InteractionResponse {
        /// The session whose request is being answered (the echo of
        /// the request frame's stamp — v3's always-explicit rule).
        session: String,
        /// The request id being answered.
        id: String,
        /// The answer payload (see `templates`).
        payload: serde_json::Value,
    },
    /// Create a fresh session in this backend. The outcome is
    /// `session_created { id, path, model }` — unstamped,
    /// backend-level (the payload carries the new id; the
    /// optional-stream ruling) — or an equally unstamped
    /// `error { kind: session }` if the session cannot be built.
    /// Nothing replays; the session is empty.
    NewSession,
    /// Load a stored session (if needed) and replay it onto the event
    /// channel, stamped with its id — the pass itself is the
    /// acknowledgment. Idempotent: an already-open session re-replays.
    OpenSession {
        /// The stored session id to open.
        id: String,
    },
    /// Move a session's active chain to an entry (any entry in the
    /// session's file — an off-chain target is a branch switch). Idle:
    /// executes immediately. A run in flight: **parks** and executes
    /// at that run's terminal (the pause point) — never rejected,
    /// never an implicit abort. Success emits `messages_discarded`
    /// (only what was submitted before this command) then
    /// `checked_out` and a full replay pass; an unknown entry emits
    /// `error { kind: checkout }` and changes nothing
    /// (PROTOCOL.md v3 stage 2).
    Checkout {
        /// The target session id.
        session: String,
        /// The entry the chain will end at (inclusive).
        entry_id: String,
    },
}

/// One line from the client. The first line must be
/// [`ClientFrame::Initialize`]; everything after is commands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClientFrame {
    /// The connection handshake: the client's protocol version, and
    /// whether it wants the session's active chain re-emitted as
    /// finalized live events (the replay pass) right after the ack.
    Initialize {
        protocol_version: u32,
        /// Request the replay pass (absent means no: a frontend that
        /// keeps its own state, or a fresh connect with nothing to
        /// replay).
        #[serde(default, skip_serializing_if = "is_false")]
        replay: bool,
    },
    /// A session command.
    Command(SessionCommand),
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The server's non-event lines: handshake outcomes and transport-level
/// errors (as opposed to run outcomes, which are events).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerControlFrame {
    /// The handshake succeeded; the session facts follow.
    InitializeAck {
        /// The version the server settled on.
        protocol_version: u32,
        /// The session id.
        session_id: String,
        /// The session file path.
        session_path: String,
        /// The active model selection.
        model: ModelSelection,
        /// Whether the session continues an existing chain. `false`
        /// after a `--continue` that found nothing means the backend
        /// started fresh instead — an absorbed miss, not an error (the
        /// pinned startup contract).
        resumed: bool,
    },
    /// The handshake failed (version mismatch); the connection closes.
    InitializeRejected {
        /// Why.
        reason: String,
    },
    /// A line the edge could not turn into a frame (unparseable, or a
    /// command sent before `initialize`). The connection stays open.
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
