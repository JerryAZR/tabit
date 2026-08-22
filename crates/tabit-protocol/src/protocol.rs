//! The frontend vocabulary: the commands a frontend submits and the
//! stamped events it consumes, shared verbatim by every transport —
//! typed values over in-process channels, JSON lines at a serialized
//! edge. Commands are fire-and-forget with total semantics (a message
//! steers the run in flight or starts one; abort stops), so this side of
//! the protocol carries no request ids and has no rejection cases.
//! Events carry a stream stamp because stream order alone cannot
//! attribute concurrent producers; subagents will mint their own stream
//! ids, which is why the stamp exists from day one.

use crate::events::SessionEvent;
use crate::model::ModelSelection;
use serde::{Deserialize, Serialize};

/// The protocol version this build speaks. Clients declare theirs in
/// [`ClientFrame::Initialize`]; a mismatch rejects the connection at the
/// handshake.
pub const PROTOCOL_VERSION: u32 = 1;

/// Which stream produced an event. v1 has exactly one stream — the
/// session's own — so every frame carries [`StreamId::main`]; sibling
/// ids arrive with concurrent producers (subagents).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StreamId(String);

impl StreamId {
    /// The session's own stream, as it appears on the wire.
    pub const MAIN: &'static str = "main";

    /// The session's own stream.
    pub fn main() -> Self {
        Self(Self::MAIN.to_string())
    }

    /// Whether this is the session's own stream.
    pub fn is_main(&self) -> bool {
        self.0 == Self::MAIN
    }
}

/// An event stamped with the stream that produced it. This is the unit
/// the backend channel carries and — serialized flat — the line a
/// transport edge writes: `{"type":"text_delta","stream":"main",...}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventFrame {
    /// The stream that produced the event.
    pub stream: StreamId,
    /// The event itself; its `type` tag flattens next to `stream`.
    #[serde(flatten)]
    pub event: SessionEvent,
}

/// A frontend command, fire-and-forget. The behavior is total over the
/// two session states:
///
/// | command  | idle                  | running                              |
/// |----------|-----------------------|--------------------------------------|
/// | `Message`| starts a run          | steers (next turn boundary)          |
/// | `Abort`  | no-op                 | aborts; discards queued messages     |
///
/// There is nothing to acknowledge and nothing to reject: outcomes are
/// events (`user_message` for acceptance, the run terminals for results).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionCommand {
    /// A user message.
    Message {
        /// The message text.
        text: String,
    },
    /// Stop: abort the run in flight and discard any queued messages.
    Abort,
}

/// One line from the client. The first line must be
/// [`ClientFrame::Initialize`]; everything after is commands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClientFrame {
    /// The connection handshake: the client's protocol version.
    Initialize { protocol_version: u32 },
    /// A session command.
    Command(SessionCommand),
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
