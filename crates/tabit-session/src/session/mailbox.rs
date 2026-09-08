//! The run-agnostic message mailbox: the one door every user message
//! enters, the handles frontends hold, and the engine's steering view.

use super::Session;
use super::wire::user_text;
use crate::lock::lock;
use crate::notice::{NoticeSink, NoticeSlot};
use rig_agent::completion::Message;
use tabit_protocol::{EventFrame, SessionEvent, StreamId};
use tokio_util::sync::CancellationToken;

/// A queued user message with its born-early entry id (PROTOCOL.md v2):
/// minted at accept, announced by `message_queued` when a run is live,
/// carried into the log when the message drains, restated by
/// `user_message { entry_id }` — and handed back by `messages_discarded`
/// if a clear discards it first. One id, one closed ledger: every
/// `message_queued` id ends in exactly one `user_message` or
/// `messages_discarded`.
pub(crate) struct QueuedMessage {
    pub(crate) id: String,
    pub(crate) message: Message,
}

impl QueuedMessage {
    pub(crate) fn text(&self) -> String {
        user_text(&self.message)
    }
}

/// The run-agnostic message mailbox: the one door every user message
/// enters ([`Session::submit`], or an engine drain mid-run), emptied by
/// [`Session::pump`] — as the next run's initial prompt or, while a run
/// is in flight, as a steer injected at the next turn boundary. The
/// mailbox outlives runs, so a message submitted at any instant is never
/// lost; only the clear sites discard (abort, checkout — each only what
/// was submitted before it, the before/after rule both share).
#[derive(Clone, Default)]
pub(crate) struct Mailbox {
    queue: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<QueuedMessage>>>,
    /// True while a pump may drain at any instant (a run is live): the
    /// gate for submit-time `message_queued` notices. Idle sends never
    /// queue — they drain immediately, so `user_message` is the
    /// acknowledgment.
    live: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// A parked continue intent (the `continue` command): the
    /// worker's beat takes it and starts a run over the existing
    /// conversation with no new message.
    continue_pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The submit-time notice sink, attached by the resident worker at
    /// spawn (see [`crate::notice`] for the channel discipline). Absent
    /// for direct [`Session`] consumers: no frontend, no notices.
    notices: std::sync::Arc<NoticeSlot>,
    /// Wakes the resident worker when work arrives. One permit covers any
    /// number of pushes; the queue itself is the source of truth — the
    /// signal exists only so an empty queue can be waited on.
    work: std::sync::Arc<tokio::sync::Notify>,
}

impl Mailbox {
    /// Attach the event channel for submit-time notices (the resident
    /// worker, at spawn), stamped with the session's stream.
    pub(crate) fn attach_notices(
        &self,
        events: &tokio::sync::mpsc::UnboundedSender<EventFrame>,
        stream: StreamId,
    ) {
        let _ = self.notices.set(NoticeSink::new(events, stream));
    }

    /// A pump began: submissions from here until [`Self::run_ended`] are
    /// acknowledged with `message_queued`.
    pub(crate) fn run_started(&self) {
        self.live.store(true, std::sync::atomic::Ordering::Release);
    }

    /// The pump ended: submits queue for the next run again.
    pub(crate) fn run_ended(&self) {
        self.live.store(false, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn push(&self, message: Message) {
        let queued = QueuedMessage {
            id: crate::ids::new_entry_id(),
            message,
        };
        // The live snapshot races the pump's own start/end — both orders
        // resolve to a consistent ledger: a notice for a message that
        // drains immediately ends in `user_message` (the GUI drops the
        // pending row on it), and an un-noticed message drains as the
        // next prompt with `user_message` as its only acknowledgment.
        let live = self.live.load(std::sync::atomic::Ordering::Acquire);
        if live {
            self.notice_queued(queued.id.clone(), queued.text());
        }
        lock(&self.queue).push_back(queued);
        self.work.notify_one();
    }

    /// Tell the frontend a live-run submission waits. A dead or absent
    /// channel is a no-op (the frontend is gone, or there never was one).
    fn notice_queued(&self, id: String, text: String) {
        if let Some(sink) = self.notices.get() {
            sink.emit(SessionEvent::MessageQueued { id, text });
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        lock(&self.queue).is_empty()
    }

    /// Park a continue intent.
    fn continue_run(&self) {
        self.continue_pending
            .store(true, std::sync::atomic::Ordering::Release);
        self.work.notify_one();
    }

    /// Whether a continue intent is parked (the actor's beat check).
    pub(crate) fn has_continue(&self) -> bool {
        self.continue_pending
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// The beat's take of a parked continue intent.
    pub(crate) fn take_continue(&self) -> bool {
        self.continue_pending
            .swap(false, std::sync::atomic::Ordering::Acquire)
    }

    /// Discard everything queued, returning the pairs (the caller emits
    /// `messages_discarded` where its event flow allows).
    pub(crate) fn clear(&self) -> Vec<QueuedMessage> {
        lock(&self.queue).drain(..).collect()
    }

    /// The one clear-and-tell: discard everything queued and emit
    /// `messages_discarded` immediately, through the same notice
    /// channel `message_queued` rides (the abort site and the checkout
    /// handler both — one emitter, one timing; a dead or absent channel
    /// is a no-op, the frontend is gone or there never was one).
    pub(crate) fn clear_noticing(&self) {
        let cleared = self.clear();
        if cleared.is_empty() {
            return;
        }
        if let Some(sink) = self.notices.get() {
            sink.emit(SessionEvent::MessagesDiscarded {
                messages: cleared
                    .into_iter()
                    .map(|queued| tabit_protocol::DiscardedMessage {
                        text: queued.text(),
                        id: queued.id,
                    })
                    .collect(),
            });
        }
    }

    /// The one drain take: everything queued, as id-carrying pairs (the
    /// born-early ids minted at `message_queued`). The engine's loop
    /// drains this — opening batch and mid-run steers alike — and
    /// folds each under its id; the ids ride the `Steer` items back
    /// for the `user_message` events.
    pub(super) fn take_all(&self) -> Vec<(String, Message)> {
        lock(&self.queue)
            .drain(..)
            .map(|queued| (queued.id, queued.message))
            .collect()
    }

    /// Whether anything is queued (the pump's run-or-idle check).
    pub(super) fn has_queued(&self) -> bool {
        !lock(&self.queue).is_empty()
    }

    /// The work signal the resident worker waits on. A push before the
    /// wait stores a permit, so no wakeup can be lost.
    pub(crate) fn work_signal(&self) -> &tokio::sync::Notify {
        &self.work
    }
}

/// Submit messages to the session's mailbox from anywhere — including
/// from inside a run's event callback, where the session itself is
/// borrowed. A cheap clone of the mailbox; see [`Session::submit`] for
/// the semantics (steers the run in flight, otherwise queues for the
/// next one).
#[derive(Clone)]
pub struct MailboxHandle {
    mailbox: Mailbox,
}

impl MailboxHandle {
    /// Queue a user message. Always accepted.
    pub fn submit(&self, text: impl Into<String>) {
        self.mailbox.push(Message::user(text.into()));
    }

    /// Park a continue intent: the worker's next beat starts a run
    /// over the existing conversation with no new message.
    pub(crate) fn continue_run(&self) {
        self.mailbox.continue_run();
    }

    /// Whether a continue intent is parked (the actor's beat check).
    pub(crate) fn has_continue(&self) -> bool {
        self.mailbox.has_continue()
    }

    /// Whether anything is queued (the actor's idle check).
    pub(crate) fn is_empty(&self) -> bool {
        self.mailbox.is_empty()
    }

    /// The work signal the resident worker waits on.
    pub(crate) fn work_signal(&self) -> &tokio::sync::Notify {
        self.mailbox.work_signal()
    }
}

/// Cancel the run currently in flight. Cheap to hold; cancelling when no
/// run is in flight does nothing.
#[derive(Clone)]
pub struct AbortHandle {
    token: std::sync::Arc<std::sync::Mutex<CancellationToken>>,
    mailbox: Mailbox,
}

impl AbortHandle {
    /// Abort the current run, if any, and discard what was queued at
    /// abort time — one semantic, one site (PROTOCOL.md flag 6): the
    /// discard notice is immediate, through the mailbox's notice
    /// channel; messages arriving after this queue normally and start
    /// the next run. Aborting while idle just discards the queue.
    pub fn abort(&self) {
        lock(&self.token).cancel();
        self.mailbox.clear_noticing();
    }
}

/// The engine-side view of the mailbox: what an outer loop drains at
/// each convergence — the opening batch and mid-run steers alike (one
/// queue, one drain; ENGINE.md's not-pre-joined rule). The engine folds
/// what it drains under the born-early ids; the session never folds.
pub(super) struct SessionSteers {
    pub(super) mailbox: Mailbox,
}

impl rig_agent::SteeringSource for SessionSteers {
    fn drain(&self) -> Vec<(String, Message)> {
        self.mailbox.take_all()
    }

    /// The post-tool stop's queue discard (ENGINE.md, stop semantics):
    /// the stop exits through the engine, the mailbox is the session's —
    /// this is the one clear-and-tell for it, same emitter and timing
    /// as abort's (flag 6).
    fn discard_pending(&self) {
        self.mailbox.clear_noticing();
    }
}

impl Session {
    /// The session's mailbox as a clonable handle: submits work while a
    /// run borrows the session (frontends' actor holds one).
    pub(crate) fn mailbox_handle(&self) -> MailboxHandle {
        MailboxHandle {
            mailbox: self.mailbox.clone(),
        }
    }

    /// A handle for aborting the current outer loop. See [`AbortHandle`].
    pub fn abort_handle(&self) -> AbortHandle {
        AbortHandle {
            token: self.abort.clone(),
            mailbox: self.mailbox.clone(),
        }
    }

    /// Point the mailbox's submit-time notices at the worker's event
    /// channel (`message_queued` for live-run submissions), stamped with
    /// the session's stream. Called by the session worker at spawn,
    /// alongside [`Self::attach_interaction`].
    pub fn attach_mailbox_notices(
        &self,
        events: &tokio::sync::mpsc::UnboundedSender<EventFrame>,
        stream: StreamId,
    ) {
        self.mailbox.attach_notices(events, stream);
    }
}
