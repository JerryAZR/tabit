//! The notice sinks: the functional layer's emission handles. A
//! notice is an event emitted *outside* the run's event fold —
//! mailbox acknowledgments (`message_queued`, `messages_discarded`),
//! persist degraded/recovered transitions, interaction requests, the
//! worker's command-time errors, the register announcement.
//!
//! A sink is three facts — the node, the channel to emit from, and
//! (where it is a session's) the stream stamp — and emission is one
//! act: [`Node::emit`] from the channel. Who hears the frame (the
//! frontend's forwarder, watching extensions) is the routing layer's
//! business, decided by subscription; the emitter never thinks about
//! it. The channel is load-bearing, not plumbing: a session-stamped
//! emission from the session's channel is what teaches the learning
//! table that the session lives there (law 1), so every sink must
//! hold the channel its session routes through.
//!
//! Two shapes cover every holder: [`NoticeSink`] for the
//! session-stamped emitters (the worker, the interaction hub, the
//! module-level taps) and [`HostSink`]/[`BackendSink`] for the
//! host-level ones (lifecycle announcements, catalog) — the
//! backend-level sink adds the origin attribution the routing
//! generalization pinned (extensions speak the shared grammar
//! origin-stamped).

use std::sync::Arc;

use tabit_protocol::{EventFrame, SessionEvent, StreamId};
use tabit_wire::node::{Channel, Node};

/// The attach-once cell for a sink that does not exist until the
/// resident worker spawns (mailbox, persist, and module-level taps):
/// the worker sets it exactly once at spawn; a second attempt is
/// ignored, and an unset cell means nobody is there to tell yet.
pub(crate) type NoticeSlot = std::sync::OnceLock<NoticeSink>;

/// The session's stamped emission handle: emits as the session's
/// worker channel, stamped with the session's stream.
///
/// Publicly an opaque token: a spawner outside a host wiring (tests,
/// alternative assemblies) can hold and pass `None`, but only the
/// crate mints real sinks ([`NoticeSink::new`] stays crate-private).
#[derive(Clone)]
pub struct NoticeSink {
    node: Arc<Node>,
    channel: Channel,
    stream: StreamId,
}

impl NoticeSink {
    /// Mint the sink over the worker's channel — the one construction
    /// site, so the channel/stamp pairing is always the worker's own.
    pub(crate) fn new(node: &Arc<Node>, channel: &Channel, stream: StreamId) -> Self {
        Self {
            node: node.clone(),
            channel: channel.clone(),
            stream,
        }
    }

    /// Emit a notice, stamped with the session's stream. The frame
    /// fans to every subscriber through the node; nobody hearing it
    /// (a dead frontend) is the routing layer's silence, not an
    /// error to report.
    pub(crate) fn emit(&self, event: SessionEvent) {
        self.node.emit(
            &self.channel,
            EventFrame {
                stream: Some(self.stream.clone()),
                origin: None,
                ttl: None,
                event,
            },
        );
    }
}

/// The host's own way onto the net: lifecycle announcements, the
/// startup catalog — emissions that are nobody's session (`None` =
/// backend-level) or a session's opening frames before its worker
/// exists.
#[derive(Clone)]
pub struct HostSink {
    node: Arc<Node>,
    channel: Channel,
}

impl HostSink {
    /// The one construction site, over the host's channel.
    pub(crate) fn new(node: &Arc<Node>, channel: &Channel) -> Self {
        Self {
            node: node.clone(),
            channel: channel.clone(),
        }
    }

    /// Put one event on the net, stamped with its stream (`None` =
    /// backend-level).
    pub fn emit(&self, stream: Option<StreamId>, event: SessionEvent) {
        self.node.emit(
            &self.channel,
            EventFrame {
                stream,
                origin: None,
                ttl: None,
                event,
            },
        );
    }
}

/// The backend's origin-stamped handle, for emissions that are
/// nobody's session: an extension speaking the shared grammar emits
/// events origin-stamped and unstamped by stream (the routing
/// generalization — routing is participant-blind, the origin field
/// is the attribution).
#[derive(Clone)]
pub struct BackendSink {
    node: Arc<Node>,
    channel: Channel,
}

impl BackendSink {
    /// The one construction site, over the host's channel.
    pub(crate) fn new(node: &Arc<Node>, channel: &Channel) -> Self {
        Self {
            node: node.clone(),
            channel: channel.clone(),
        }
    }

    /// Forward a frame verbatim — the stream stamp survives, the
    /// origin names the conduit (an owned child's traffic crossing
    /// its owner's pipe; forward-don't-re-stamp, the bridge's rule).
    pub fn forward(&self, origin: &str, frame: EventFrame) {
        self.node.emit(
            &self.channel,
            EventFrame {
                stream: frame.stream,
                origin: Some(origin.to_string()),
                ttl: frame.ttl,
                event: frame.event,
            },
        );
    }

    /// Emit an extension's event, origin-stamped and backend-level
    /// (no stream).
    pub fn emit(&self, origin: &str, event: SessionEvent) {
        self.node.emit(
            &self.channel,
            EventFrame {
                stream: None,
                origin: Some(origin.to_string()),
                ttl: None,
                event,
            },
        );
    }
}
