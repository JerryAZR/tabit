//! The session actor: the backend half of the frontend protocol. One
//! task owns the [`Session`] (single owner, no session locks); commands
//! arrive on an unbounded channel, stamped events leave on one. When a
//! message arrives idle, the actor moves the session into a spawned pump
//! task — so commands keep being processed while the run is in flight: a
//! message submitted mid-run steers it through the mailbox, and abort
//! cancels it and discards the queue.
//!
//! Termination contract: once every command sender is closed
//! ([`SessionHandle::close_commands`] or the last
//! [`SessionHandle::command_link`] dropped), the actor finishes any
//! in-flight pump, captures [`SessionHandle::closing_stats`], and the
//! event stream ends.

use crate::events::SessionEvent;
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
    commands: mpsc::UnboundedSender<SessionCommand>,
    events: mpsc::UnboundedReceiver<EventFrame>,
    shutdown: CancellationToken,
    closing_stats: Arc<Mutex<Option<SessionStats>>>,
}

/// A cheap clone of the handle's command sender, for a thread that only
/// submits (a transport edge's reader). Dropping the last link (or
/// [`SessionHandle::close_commands`]) starts the termination contract.
#[derive(Clone)]
pub struct SessionCommandLink {
    commands: mpsc::UnboundedSender<SessionCommand>,
}

impl SessionCommandLink {
    /// Submit a command. Fire-and-forget: outcomes arrive as events.
    /// Sending after the session ended is a no-op.
    pub fn send(&self, command: SessionCommand) {
        // The actor is gone only when the whole session is; there is no
        // per-command failure to report.
        let _ = self.commands.send(command);
    }
}

impl SessionHandle {
    /// Hand `session` to its actor and get the frontend handle back. Must
    /// be called inside a tokio runtime (the actor is spawned here).
    pub fn spawn(session: Session) -> Self {
        let info = SessionInfo {
            session_id: session.id().to_string(),
            session_path: session.path().display().to_string(),
            model: session.selection().clone(),
        };
        let mailbox = session.mailbox_handle();
        let abort = session.abort_handle();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (session_tx, session_rx) = mpsc::channel(1);
        let shutdown = CancellationToken::new();
        let closing_stats = Arc::new(Mutex::new(None));
        tokio::spawn(
            Actor {
                session: Some(session),
                mailbox,
                abort,
                events: event_tx,
                commands: command_rx,
                session_return: session_rx,
                session_tx,
                shutdown: shutdown.clone(),
                closing_stats: closing_stats.clone(),
            }
            .run(),
        );
        Self {
            info,
            commands: command_tx,
            events: event_rx,
            shutdown,
            closing_stats,
        }
    }

    /// The session facts captured when the actor took over.
    pub fn info(&self) -> &SessionInfo {
        &self.info
    }

    /// Submit a user message: steers the run in flight or starts one.
    pub fn message(&self, text: impl Into<String>) {
        self.command_link()
            .send(SessionCommand::Message { text: text.into() });
    }

    /// Stop: abort the run in flight and discard queued messages.
    pub fn abort(&self) {
        self.command_link().send(SessionCommand::Abort);
    }

    /// A cloneable sender for threads that only submit commands.
    pub fn command_link(&self) -> SessionCommandLink {
        SessionCommandLink {
            commands: self.commands.clone(),
        }
    }

    /// Close the command side — the actor finishes any in-flight pump,
    /// captures closing stats, and the event stream ends. Later sends
    /// from surviving links are no-ops.
    pub fn close_commands(&mut self) {
        self.shutdown.cancel();
    }

    /// The next stamped event, or `None` once the actor has wound down.
    pub async fn next_event(&mut self) -> Option<EventFrame> {
        self.events.recv().await
    }

    /// Session totals captured at actor wind-down, for callers that want
    /// a closing summary (print mode's footer). `None` until the event
    /// stream has ended.
    pub fn closing_stats(&self) -> Option<SessionStats> {
        lock(&self.closing_stats).clone()
    }
}

/// The backend actor: owns the session between pumps, and the pump task
/// owns it during one drain-to-quiescence.
struct Actor {
    session: Option<Session>,
    mailbox: MailboxHandle,
    abort: AbortHandle,
    events: mpsc::UnboundedSender<EventFrame>,
    commands: mpsc::UnboundedReceiver<SessionCommand>,
    session_return: mpsc::Receiver<Session>,
    session_tx: mpsc::Sender<Session>,
    shutdown: CancellationToken,
    closing_stats: Arc<Mutex<Option<SessionStats>>>,
}

impl Actor {
    async fn run(mut self) {
        let mut open = true;
        // Exit when commands are closed AND the session is back (the
        // in-flight pump is allowed to finish — accepted messages run).
        while open || self.session.is_none() {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled(), if open => {
                    open = false;
                    // Commands already queued were accepted from the
                    // caller's perspective; honor them before winding
                    // down.
                    while let Ok(command) = self.commands.try_recv() {
                        self.handle_command(command);
                    }
                }
                command = self.commands.recv(), if open => match command {
                    None => open = false,
                    Some(command) => self.handle_command(command),
                },
                returned = self.session_return.recv() => {
                    self.session = returned;
                    // Leftovers (a message that missed the last drain of a
                    // failed run) get their pump; usually the mailbox is
                    // empty here.
                    if open && self.session.is_some() && !self.mailbox.is_empty() {
                        self.start_pump();
                    }
                }
            }
        }
        if let Some(session) = &self.session
            && let Ok(stats) = session.stats()
        {
            *lock(&self.closing_stats) = Some(stats);
        }
        // `self.events` drops here: the stream ends.
    }

    fn handle_command(&mut self, command: SessionCommand) {
        match command {
            SessionCommand::Message { text } => {
                self.mailbox.submit(text);
                if self.session.is_some() {
                    self.start_pump();
                }
            }
            SessionCommand::Abort => {
                self.abort.abort();
                self.mailbox.clear();
            }
        }
    }

    /// Move the session into a pump task; it comes back on
    /// `session_return` when the mailbox is drained.
    fn start_pump(&mut self) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        let events = self.events.clone();
        let session_tx = self.session_tx.clone();
        tokio::spawn(async move {
            session
                .pump(&mut |event: SessionEvent| {
                    // The receiver is gone only when the frontend is;
                    // there is no one left to tell.
                    let _ = events.send(EventFrame {
                        stream: StreamId::main(),
                        event,
                    });
                })
                .await;
            let _ = session_tx.send(session).await;
        });
    }
}

#[cfg(test)]
#[path = "endpoint_tests.rs"]
mod tests;
