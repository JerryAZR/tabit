//! The notice channel: the one home of the session's frontend-channel
//! discipline.
//!
//! A notice is an event emitted *outside* the run's event fold —
//! mailbox acknowledgments (`message_queued`, `messages_discarded`),
//! persist degraded/recovered transitions, interaction requests, the
//! worker's command-time errors, the register announcement. Every one
//! rides the same unbounded [`EventFrame`] channel the run's events do,
//! and every holder plays by one rule: **weak, so the stream ends with
//! the frontend**. A strong sender held past the frontend's lifetime
//! would keep the channel open after every real consumer is gone (the
//! termination contract); a dead channel therefore means nobody is
//! left to tell, and emitting into one is a no-op.
//!
//! Two shapes cover every holder: [`NoticeSink`] for holders born with
//! the channel (the interaction hub, the endpoint worker), and
//! [`NoticeSlot`] for the attach-once case — the sink does not exist
//! until the resident worker spawns, so the mailbox and the persist
//! notices keep an `Arc<NoticeSlot>` the worker sets exactly once
//! (`OnceLock`; clones share the one attach through the `Arc`).

use tabit_protocol::{EventFrame, SessionEvent, StreamId};
use tokio::sync::mpsc;

/// The session's stamped, weak handle on the frontend's event channel.
/// The channel and the stream stamp are one value because they are one
/// fact: they attach together, or not at all — an emission can never
/// find a channel without its stamp.
///
/// Publicly an opaque token: a spawner outside a host wiring (tests,
/// alternative assemblies) can hold and pass `None`, but only the
/// crate mints real sinks ([`NoticeSink::new`] stays crate-private —
/// the one downgrade site).
#[derive(Clone)]
pub struct NoticeSink {
    events: mpsc::WeakUnboundedSender<EventFrame>,
    stream: StreamId,
}

impl NoticeSink {
    /// Downgrade the channel's strong end into a notice sink — the one
    /// downgrade site, so every holder is weak from here on.
    pub(crate) fn new(events: &mpsc::UnboundedSender<EventFrame>, stream: StreamId) -> Self {
        Self {
            events: events.downgrade(),
            stream,
        }
    }

    /// Emit a notice, stamped with the session's stream. Returns whether
    /// the channel was live to take the frame: a dead or never-attached
    /// channel is a silent no-op for fire-and-forget notices, but the
    /// interaction hub's ask cares — an ask that cannot reach a
    /// frontend resolves dismissed instead of hanging.
    pub(crate) fn emit(&self, event: SessionEvent) -> bool {
        let Some(events) = self.events.upgrade() else {
            return false;
        };
        events
            .send(EventFrame {
                stream: Some(self.stream.clone()),
                origin: None,
                event,
            })
            .is_ok()
    }

    /// Send a frame that already carries its own stamp — the subprocess
    /// bridge's rule (forward, don't re-stamp): a child's frame keeps
    /// the child's stream id as it crosses onto the parent's channel.
    /// Same liveness contract as [`Self::emit`].
    pub(crate) fn forward(&self, frame: EventFrame) -> bool {
        let Some(events) = self.events.upgrade() else {
            return false;
        };
        events.send(frame).is_ok()
    }
}

/// The attach-once cell for a sink that does not exist until the
/// resident worker spawns (mailbox and persist notices). `set` runs
/// exactly once, at spawn; a second attempt is ignored, and `None`
/// before the attach means the same as a dead channel after it —
/// nobody is there to tell.
pub(crate) type NoticeSlot = std::sync::OnceLock<NoticeSink>;

/// The backend's weak handle on the same channel, for emissions that
/// are nobody's session: an extension speaking the shared grammar
/// emits events origin-stamped and unstamped by stream (the routing
/// generalization — routing is participant-blind, the origin field is
/// the attribution). Same weak discipline as [`NoticeSink`]: the
/// stream ends with the frontend, and a dead channel means nobody is
/// left to tell.
#[derive(Clone)]
pub struct BackendSink {
    events: mpsc::WeakUnboundedSender<EventFrame>,
}

impl BackendSink {
    /// The one downgrade site for backend-level emissions.
    pub(crate) fn new(events: &mpsc::UnboundedSender<EventFrame>) -> Self {
        Self {
            events: events.downgrade(),
        }
    }

    /// Forward a frame verbatim — the stream stamp survives, the
    /// origin names the conduit (an owned child's traffic crossing
    /// its owner's pipe; forward-don't-re-stamp, the bridge tap's
    /// rule). Returns whether the channel was live.
    pub fn forward(&self, origin: &str, frame: EventFrame) -> bool {
        let Some(events) = self.events.upgrade() else {
            return false;
        };
        let stamped = EventFrame {
            stream: frame.stream,
            origin: Some(origin.to_string()),
            event: frame.event,
        };
        events.send(stamped).is_ok()
    }

    /// Emit an extension's event, origin-stamped and backend-level
    /// (no stream). Returns whether the channel was live.
    pub fn emit(&self, origin: &str, event: SessionEvent) -> bool {
        let Some(events) = self.events.upgrade() else {
            return false;
        };
        events
            .send(EventFrame {
                stream: None,
                origin: Some(origin.to_string()),
                event,
            })
            .is_ok()
    }
}
