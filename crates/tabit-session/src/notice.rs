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
#[derive(Clone)]
pub(crate) struct NoticeSink {
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
                event,
            })
            .is_ok()
    }
}

/// The attach-once cell for a sink that does not exist until the
/// resident worker spawns (mailbox and persist notices). `set` runs
/// exactly once, at spawn; a second attempt is ignored, and `None`
/// before the attach means the same as a dead channel after it —
/// nobody is there to tell.
pub(crate) type NoticeSlot = std::sync::OnceLock<NoticeSink>;
