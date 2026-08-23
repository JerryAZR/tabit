//! The session worker: the backend half of the frontend protocol. One
//! resident task owns the [`Session`] exclusively and forever —
//! ownership never moves, so there is no handoff window to patch. The
//! worker waits for mailbox work (or shutdown) and pumps to quiescence
//! inline. The two capabilities that must act while a run is in flight
//! are shared leaves, so they never need the worker's attention:
//! a message submitted mid-run steers it through the mailbox (the
//! engine drains at turn boundaries), and abort preempts through the
//! cancel token.
//!
//! Termination (ruled 2026-08 — the core dies with the frontend):
//!
//! - [`SessionHandle::close_commands`] is the **polite** close: the
//!   in-flight run finishes, commands already queued are honored
//!   (close is not a barrier), closing stats are captured, and the
//!   event stream ends. In-process consumers that stay alive to read
//!   the stream (print mode) use this.
//! - **Frontend death** — the event receiver is gone, whatever the
//!   reason (the GUI process exited, the transport dropped it) —
//!   aborts the in-flight run and winds the worker down immediately,
//!   regardless of state: a parked permission card or a half-finished
//!   turn must never outlive the user. Interrupted results synthesize
//!   on the next open exactly like a crash; the log stays durable.

use crate::interaction::InteractionHub;
use crate::lock::lock;
use crate::session::{AbortHandle, MailboxHandle, Session, SessionStats};
use std::sync::{Arc, Mutex};
use tabit_protocol::{EventFrame, ModelSelection, SessionCommand, StreamId};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The session facts a frontend needs at startup (handshake payload,
/// banners).
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// The session id.
    pub session_id: String,
    /// The session file path.
    pub session_path: String,
    /// The active model selection.
    pub model: ModelSelection,
    /// Whether the session continues an existing chain (or started
    /// fresh — see [`Session::resumed`]).
    pub resumed: bool,
}

/// The frontend half of a session: submit commands, receive stamped
/// events. Input threads get their own way in via
/// [`SessionHandle::command_link`].
pub struct SessionHandle {
    info: SessionInfo,
    mailbox: MailboxHandle,
    abort: AbortHandle,
    interaction: InteractionHub,
    events: Option<mpsc::UnboundedReceiver<EventFrame>>,
    shutdown: CancellationToken,
    closing_stats: Arc<Mutex<Option<SessionStats>>>,
}

/// A cheap clone for threads that only submit commands (a transport
/// edge's reader). Commands act directly on the shared leaves: a
/// message queues in the mailbox, abort cancels the run in flight and
/// discards the queue.
#[derive(Clone)]
pub struct SessionCommandLink {
    mailbox: MailboxHandle,
    abort: AbortHandle,
    interaction: InteractionHub,
}

impl SessionCommandLink {
    /// Submit a command. Fire-and-forget: outcomes arrive as events.
    /// Sends after the session has wound down are no-ops.
    pub fn send(&self, command: SessionCommand) {
        match command {
            SessionCommand::Message { text } => self.mailbox.submit(text),
            SessionCommand::Abort => abort_and_clear(&self.abort, &self.mailbox),
            // Total: an unknown or dead id logs and drops inside the hub.
            SessionCommand::InteractionResponse { id, option, text } => {
                self.interaction.respond(&id, option, text);
            }
        }
    }
}

/// Abort the run in flight and discard the queue — one semantic. Both
/// encode sites (the command link and the handle) route through here so
/// `messages_discarded` emission lands in one place: the mailbox stages
/// the discarded pairs and the aborted run's conclusion flushes them
/// after its terminal (PROTOCOL.md v2 — the notice rides the wind-down).
fn abort_and_clear(abort: &AbortHandle, mailbox: &MailboxHandle) {
    abort.abort();
    mailbox.abort_clear();
}

impl SessionHandle {
    /// Hand `session` to its resident worker and get the frontend handle
    /// back. Must be called inside a tokio runtime (the worker is
    /// spawned here).
    pub fn spawn(mut session: Session) -> Self {
        let info = SessionInfo {
            session_id: session.id().to_string(),
            session_path: session.path().display().to_string(),
            model: session.selection().clone(),
            resumed: session.resumed(),
        };
        let mailbox = session.mailbox_handle();
        let abort = session.abort_handle();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<EventFrame>();
        let interaction = InteractionHub::new(event_tx.clone());
        let shutdown = CancellationToken::new();
        let closing_stats = Arc::new(Mutex::new(None));
        let worker_shutdown = shutdown.clone();
        let worker_stats = closing_stats.clone();
        let worker_mailbox = mailbox.clone();
        let worker_interaction = interaction.clone();
        let worker_events = event_tx.clone();
        let worker_abort = abort.clone();
        let watcher_shutdown = shutdown.clone();
        // The death watcher: frontend death (the event receiver is gone)
        // aborts the in-flight run so the worker can wind down
        // immediately, regardless of state (the ruling). The worker
        // itself cannot see death while pumping — the watcher is its
        // eyes. It exits on either signal and drops its sender clone,
        // so the polite path's stream still ends when the worker does.
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = watcher_shutdown.cancelled() => {}
                _ = worker_events.closed() => {
                    worker_abort.abort();
                }
            }
        });
        tokio::spawn(async move {
            // The hub and the mailbox's submit-time notices both reach the
            // worker's event channel, so both exist only here — attach
            // them before the first pump can run.
            session.attach_interaction(worker_interaction);
            session.attach_mailbox_notices(event_tx.clone());
            // The resident worker. Ownership never moves: idle is the
            // wait below, running is the pump call — two positions of
            // one loop, not two tasks.
            loop {
                if !worker_mailbox.is_empty() {
                    session
                        .pump(&mut |event| {
                            // The receiver is gone only when the frontend
                            // is; there is no one left to tell.
                            let _ = event_tx.send(EventFrame {
                                stream: StreamId::main(),
                                event,
                            });
                        })
                        .await;
                    continue;
                }
                tokio::select! {
                    biased;
                    _ = worker_shutdown.cancelled() => {
                        // Close is not a barrier: pushes are synchronous,
                        // so anything sent before closing is already
                        // queued — run it before winding down. (Pushes
                        // that race the wind-down simply run too; nothing
                        // is lost.)
                        if !worker_mailbox.is_empty() {
                            continue;
                        }
                        break;
                    }
                    // The frontend is gone; the death watcher has already
                    // aborted any in-flight run, so the pump has returned.
                    _ = event_tx.closed() => break,
                    _ = worker_mailbox.work_signal().notified() => {}
                }
            }
            if let Ok(stats) = session.stats() {
                *lock(&worker_stats) = Some(stats);
            }
            // `event_tx` drops here: the stream ends.
        });
        Self {
            info,
            mailbox,
            abort,
            interaction,
            events: Some(event_rx),
            shutdown,
            closing_stats,
        }
    }

    /// The session facts captured when the worker took over.
    pub fn info(&self) -> &SessionInfo {
        &self.info
    }

    /// Submit a user message: steers the run in flight or starts one.
    pub fn message(&self, text: impl Into<String>) {
        self.mailbox.submit(text);
    }

    /// Stop: abort the run in flight and discard queued messages.
    /// Aborting while idle is a no-op (including on anything queued —
    /// the queue is discarded with it).
    pub fn abort(&self) {
        abort_and_clear(&self.abort, &self.mailbox);
    }

    /// A cloneable submitter for threads that only send commands.
    pub fn command_link(&self) -> SessionCommandLink {
        SessionCommandLink {
            mailbox: self.mailbox.clone(),
            abort: self.abort.clone(),
            interaction: self.interaction.clone(),
        }
    }

    /// Close the session's command side. The worker finishes any
    /// in-flight run, then — close is not a barrier — runs everything
    /// already queued, captures [`SessionHandle::closing_stats`], and
    /// the event stream ends. Sends from surviving links that race the
    /// wind-down still run; once the stream has ended they are no-ops.
    pub fn close_commands(&mut self) {
        self.shutdown.cancel();
    }

    /// Take the whole event stream for a long-lived consumer (a
    /// transport forwarder). Once taken, [`SessionHandle::next_event`]
    /// yields `None` — one stream, one consumer.
    pub fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<EventFrame>> {
        self.events.take()
    }

    /// The next stamped event, or `None` once the worker has wound
    /// down (or the stream was taken).
    pub async fn next_event(&mut self) -> Option<EventFrame> {
        self.events.as_mut()?.recv().await
    }

    /// Session totals captured at worker wind-down, for callers that
    /// want a closing summary (print mode's footer). `None` until the
    /// event stream has ended.
    pub fn closing_stats(&self) -> Option<SessionStats> {
        lock(&self.closing_stats).clone()
    }
}

#[cfg(test)]
#[path = "endpoint_tests.rs"]
mod tests;
