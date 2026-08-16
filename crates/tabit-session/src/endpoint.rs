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
//! Termination: [`SessionHandle::close_commands`] (explicit) or dropping
//! the frontend's entire handle (the event receiver goes with it).
//! Either way the in-flight run finishes — accepted messages run —
//! closing stats are captured, and the event stream ends.

use crate::lock::lock;
use crate::model::ModelSelection;
use crate::protocol::{EventFrame, SessionCommand, StreamId};
use crate::session::{AbortHandle, MailboxHandle, Session, SessionStats};
use std::sync::{Arc, Mutex};
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
}

/// The frontend half of a session: submit commands, receive stamped
/// events. Input threads get their own way in via
/// [`SessionHandle::command_link`].
pub struct SessionHandle {
    info: SessionInfo,
    mailbox: MailboxHandle,
    abort: AbortHandle,
    events: mpsc::UnboundedReceiver<EventFrame>,
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
}

impl SessionCommandLink {
    /// Submit a command. Fire-and-forget: outcomes arrive as events.
    /// Sends after the session has wound down are no-ops.
    pub fn send(&self, command: SessionCommand) {
        match command {
            SessionCommand::Message { text } => self.mailbox.submit(text),
            SessionCommand::Abort => {
                self.abort.abort();
                self.mailbox.clear();
            }
        }
    }
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
        };
        let mailbox = session.mailbox_handle();
        let abort = session.abort_handle();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<EventFrame>();
        let shutdown = CancellationToken::new();
        let closing_stats = Arc::new(Mutex::new(None));
        let worker_shutdown = shutdown.clone();
        let worker_stats = closing_stats.clone();
        let worker_mailbox = mailbox.clone();
        tokio::spawn(async move {
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
                    // The frontend dropped its whole handle: the run in
                    // flight finishes (the log stays durable), then stop.
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
            events: event_rx,
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
        self.abort.abort();
        self.mailbox.clear();
    }

    /// A cloneable submitter for threads that only send commands.
    pub fn command_link(&self) -> SessionCommandLink {
        SessionCommandLink {
            mailbox: self.mailbox.clone(),
            abort: self.abort.clone(),
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

    /// The next stamped event, or `None` once the worker has wound down.
    pub async fn next_event(&mut self) -> Option<EventFrame> {
        self.events.recv().await
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
