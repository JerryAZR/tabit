//! The session host: the backend half of the frontend protocol. One
//! host connection serves many sessions — a resident loop routes
//! commands to per-session workers, each worker the classic resident
//! owner (one task owns its [`Session`] exclusively and forever; idle
//! is the wait, running is the pump — no handoff window to patch).
//! Runs in different sessions proceed concurrently; every worker
//! stamps its events with its session id, and all events ride one
//! channel (PROTOCOL.md v3).
//!
//! The host, not the workers, owns session lifecycle: `new_session`
//! builds a session through the injected wiring (the binary's assembly
//! knowledge — config, tools, preamble — kept out of this crate),
//! `open_session` loads a stored one, and the startup catalog
//! (`sessions_available`) is a header-only listing so lazy loading
//! holds: only the boot session is resident at startup.
//!
//! The command path (ruled 2026-08): **the router only routes** —
//! the host loop resolves a session address and forwards into that
//! session's handler, a black box to the router; routing failures
//! (an unknown session) are its only errors. The handler
//! ([`Worker::deliver`], module code running synchronously at the
//! dequeue point) owns every command's semantics and the session's
//! pending intent: the mailbox (messages — consumed mid-run by the
//! engine as steers, at the beat by the worker as batches), the
//! cancel token, the interaction hub, a pending-checkout slot, and a
//! replay-request flag. The worker task owns the session itself and
//! serves its beat — passes, then a parked checkout (the rewind, the
//! one session mutation), then message batches — so routing never
//! blocks on a run.
//!
//! Termination (ruled 2026-08 — the core dies with the frontend):
//!
//! - [`SessionHost::close_commands`] is the **polite** close: every
//!   worker finishes its in-flight run, commands already routed are
//!   honored (close is not a barrier), closing stats are captured, and
//!   the event stream ends. In-process consumers that stay alive to
//!   read the stream (print mode) use this.
//! - **Frontend death** — the event receiver is gone, whatever the
//!   reason — aborts every in-flight run and winds every worker down
//!   immediately, regardless of state: a parked permission card or a
//!   half-finished turn must never outlive the user. Interrupted
//!   results synthesize on the next open exactly like a crash; the
//!   log stays durable.

use crate::interaction::InteractionHub;
use crate::lock::lock;
use crate::session::{AbortHandle, MailboxHandle, Session, SessionStats};
use crate::store::SessionStore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tabit_protocol::{
    AvailableSession, EventFrame, ModelSelection, SessionCommand, SessionEvent, StreamId,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// The session facts a frontend needs at startup (handshake payload,
/// banners) — the boot session's.
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

/// How the host builds sessions for `new_session`: the binary's
/// assembly (config resolution, tools, preamble) behind a closure, so
/// this crate stays free of front-facing wiring. Returns the session
/// plus its selection notes (surfaced as `error { kind: model }`
/// frames stamped with the new session).
pub type SessionSource = Arc<dyn Fn() -> Result<(Session, Vec<String>), String> + Send + Sync>;

/// [`SessionSource`]'s sibling for `open_session { id }`: load a
/// stored session by id (the resume path — full parse and repair).
pub type OpenSessionSource =
    Arc<dyn Fn(&str) -> Result<(Session, Vec<String>), String> + Send + Sync>;

/// Everything the host needs beyond the boot session: the store (the
/// startup catalog) and the two session builders.
pub struct SessionHostWiring {
    /// The sessions directory the catalog lists.
    pub store: SessionStore,
    /// Build a fresh session (`new_session`).
    pub create: SessionSource,
    /// Load a stored session by id (`open_session`).
    pub open: OpenSessionSource,
}

/// A command on its way to the host loop: a wire command, or a
/// replay-pass request (the transport edge's way of asking after the
/// handshake — not a wire command itself).
enum HostCommand {
    Command(SessionCommand),
    Replay(String),
}

/// One session's delivery surface — the module's handler at the
/// command dequeue point, opaque to the router (owner ruling 2026-08:
/// **the router only routes** — it resolves a session address and
/// forwards; every command's semantics live here, in module code,
/// running synchronously at receive). The worker task holds the
/// session itself and consumes the pending intent this struct
/// manages: the beat serves parked passes, then a parked checkout,
/// then batches messages.
#[derive(Clone)]
struct Worker {
    mailbox: MailboxHandle,
    abort: AbortHandle,
    interaction: InteractionHub,
    /// The event channel and this session's stamp, for the handler's
    /// own emissions (checkout errors and clears) — a module talking
    /// to its frontend, not the router's business. Weak, per the
    /// channel-lifetime discipline (the hub, the mailbox notices): the
    /// delivery surface lives as long as the host's routing table, and
    /// a strong sender would keep the event stream open after every
    /// real producer is gone — the stream ends with the frontend, not
    /// with the table. A dead channel simply means nobody is left to
    /// tell.
    events: mpsc::WeakUnboundedSender<EventFrame>,
    stream: StreamId,
    /// The read-only entry-id probe — checkout verification at receive
    /// (see [`crate::recorder::EntryIdProbe`]).
    entry_probe: crate::recorder::EntryIdProbe,
    /// Pending checkout intent — a slot, not a queue: a newer checkout
    /// replaces an older (collapse; the newer intent is the intent),
    /// abort clears it (drop-all-pending-intent), and the worker takes
    /// it at its beat for the rewind.
    checkout_slot: Arc<Mutex<Option<String>>>,
    /// A parked replay request (idempotent read — one flag collapses
    /// any number of requests; the beat serves it before batching).
    replay_due: Arc<std::sync::atomic::AtomicBool>,
}

impl Worker {
    /// Deliver a session-scoped command — the handler at the dequeue
    /// point. Everything from here down is this module's semantics.
    #[allow(clippy::unreachable)]
    #[allow(clippy::unreachable)]
    fn deliver(&self, command: SessionCommand) {
        match command {
            SessionCommand::Message { text, .. } => self.mailbox.submit(text),
            SessionCommand::Abort { .. } => {
                // Drop-all-pending-intent: the run (the abort leaf's
                // cancel plus its message clear) and any parked
                // checkout. What already drained is history and stays
                // for the log to tell. A discarded pending checkout
                // emits nothing — no `checked_out` will follow; the
                // abort is the marker (FRONTEND.md §7).
                self.abort.abort();
                *lock(&self.checkout_slot) = None;
            }
            SessionCommand::InteractionResponse {
                id, option, text, ..
            } => {
                // Total: an unknown or dead id logs and drops inside
                // the hub.
                self.interaction.respond(&id, option, text);
            }
            SessionCommand::Checkout { entry_id, .. } => {
                // Validate against this module's own id truth, here at
                // receive: a bad target errors immediately — even
                // mid-run — and nothing else happens.
                if !self.entry_probe.contains(&entry_id) {
                    if let Some(events) = self.events.upgrade() {
                        let _ = events.send(EventFrame {
                            stream: self.stream.clone(),
                            event: SessionEvent::error_checkout(format!(
                                "no entry `{entry_id}` in this session"
                            )),
                        });
                    }
                    return;
                }
                // Discard-at-receive (ruled): the still-pending
                // messages — the ones `message_queued` announced that
                // nothing has drained — are cleared now, in wire
                // order, so their fate is decided the moment the
                // checkout is accepted. What already entered the
                // conversation is history the rewind will drop.
                let cleared = self.mailbox.clear();
                if !cleared.is_empty()
                    && let Some(events) = self.events.upgrade()
                {
                    let _ = events.send(EventFrame {
                        stream: self.stream.clone(),
                        event: SessionEvent::MessagesDiscarded {
                            messages: cleared
                                .into_iter()
                                .map(|queued| tabit_protocol::DiscardedMessage {
                                    text: queued.text(),
                                    id: queued.id,
                                })
                                .collect(),
                        },
                    });
                }
                // Pending intent, not a queue: the newer checkout is
                // the intent.
                lock(&self.checkout_slot).replace(entry_id);
                self.mailbox.work_signal().notify_one();
            }
            // Lifecycle is not session-scoped — the router forwards
            // those to the lifecycle handler. Unreachable by
            // construction; sanctioned crash: see the error doctrine
            // in AGENTS.md.
            SessionCommand::NewSession | SessionCommand::OpenSession { .. } => {
                unreachable!("lifecycle commands are routed to the lifecycle handler")
            }
        }
    }

    /// Park a replay request for the next beat. A read never holds
    /// writes: messages keep flowing (a live run steers them, an idle
    /// queue batches them), and the beat serves the pass ahead of the
    /// next batch — the pass reflects the chain as of the beat, and a
    /// message that has not drained by then renders live after the
    /// bracket (PROTOCOL.md v3 stage 2).
    fn deliver_replay(&self) {
        self.replay_due
            .store(true, std::sync::atomic::Ordering::Release);
        self.mailbox.work_signal().notify_one();
    }
}

/// The frontend half of the backend: submit commands, receive every
/// session's stamped events. Input threads get their own way in via
/// [`SessionHost::command_link`].
pub struct SessionHost {
    info: SessionInfo,
    events: Option<mpsc::UnboundedReceiver<EventFrame>>,
    shutdown: CancellationToken,
    commands: mpsc::UnboundedSender<HostCommand>,
    workers: Arc<Mutex<HashMap<String, Worker>>>,
    closing_stats: Arc<Mutex<HashMap<String, SessionStats>>>,
}

/// A cheap clone for threads that only submit commands (a transport
/// edge's reader). Commands route through the host loop to the named
/// session; sends after the host has wound down are no-ops.
#[derive(Clone)]
pub struct SessionCommandLink {
    commands: mpsc::UnboundedSender<HostCommand>,
}

impl SessionCommandLink {
    /// Submit a command. Fire-and-forget: outcomes arrive as events.
    /// Sends after the host has wound down are no-ops.
    pub fn send(&self, command: SessionCommand) {
        let _ = self.commands.send(HostCommand::Command(command));
    }

    /// Request a session's replay pass — the transport edge's way in
    /// (the bridge asks right after the handshake, when the
    /// `initialize` frame said `replay: true`).
    pub fn replay(&self, session: &str) {
        let _ = self.commands.send(HostCommand::Replay(session.to_string()));
    }
}

/// Emit `messages_discarded` for pairs staged by an abort whose run
/// conclusion never flushed them (idle aborts; aborts racing the
/// wind-down). Mid-run aborts flush at their terminal first — the
/// take makes double-flush harmless.
fn flush_staged_discards(
    mailbox: &MailboxHandle,
    event_tx: &mpsc::UnboundedSender<EventFrame>,
    stream: &StreamId,
) {
    let staged = mailbox.take_staged_discards();
    if staged.is_empty() {
        return;
    }
    let _ = event_tx.send(EventFrame {
        stream: stream.clone(),
        event: SessionEvent::MessagesDiscarded {
            messages: staged
                .into_iter()
                .map(|queued| tabit_protocol::DiscardedMessage {
                    text: queued.text(),
                    id: queued.id,
                })
                .collect(),
        },
    });
}

impl SessionHost {
    /// Hand the boot `session` to the host and get the frontend handle
    /// back. Must be called inside a tokio runtime (the host loop and
    /// the boot worker spawn here). The startup notes (model-preference
    /// degradations from selection) and the session catalog are the
    /// host's first emissions — they land right after the transport's
    /// handshake ack, ahead of anything a worker can produce.
    pub fn spawn(boot: Session, startup_notes: Vec<String>, wiring: SessionHostWiring) -> Self {
        let info = SessionInfo {
            session_id: boot.id().to_string(),
            session_path: boot.path().display().to_string(),
            model: boot.selection().clone(),
            resumed: boot.resumed(),
        };
        let boot_id = info.session_id.clone();
        let boot_stream = StreamId::new(boot_id.clone());
        let (event_tx, event_rx) = mpsc::unbounded_channel::<EventFrame>();
        let (command_tx, mut command_rx) = mpsc::unbounded_channel::<HostCommand>();
        let shutdown = CancellationToken::new();
        // The workers' token is the host's to pull, and only after the
        // host has routed everything queued ahead of the close: a
        // worker sharing the command-side token could observe
        // `cancelled` and exit before its queued messages were routed
        // — breaking "close is not a barrier" by one hop.
        let worker_shutdown = CancellationToken::new();
        let workers = Arc::new(Mutex::new(HashMap::new()));
        let closing_stats = Arc::new(Mutex::new(HashMap::new()));

        // The host's synchronous startup emissions, ordered ahead of
        // any worker frame by construction (one sender, sent before
        // the worker task exists): the boot session's selection
        // degradations, then the catalog. A listing failure is the
        // carrier in place of the announcement — no catalog follows
        // (ruled: external errors ride the channel; PROTOCOL.md v3).
        for note in startup_notes {
            let _ = event_tx.send(EventFrame {
                stream: boot_stream.clone(),
                event: SessionEvent::error_model(note),
            });
        }
        match wiring.store.list() {
            Ok(summaries) => {
                let _ = event_tx.send(EventFrame {
                    stream: boot_stream.clone(),
                    event: SessionEvent::SessionsAvailable {
                        sessions: summaries
                            .into_iter()
                            .map(|summary| AvailableSession {
                                id: summary.id,
                                created_at: summary.created_at,
                                entry_count: summary.entry_count as u64,
                            })
                            .collect(),
                    },
                });
            }
            Err(error) => {
                let _ = event_tx.send(EventFrame {
                    stream: boot_stream.clone(),
                    event: SessionEvent::error_session(format!("could not list sessions: {error}")),
                });
            }
        }

        let (boot_worker, boot_join) = spawn_worker(
            boot,
            event_tx.clone(),
            worker_shutdown.clone(),
            closing_stats.clone(),
        );
        lock(&workers).insert(boot_id.clone(), boot_worker);

        // The death watcher: frontend death (the event receiver is
        // gone) aborts every in-flight run so the workers can wind
        // down immediately, regardless of state (the ruling). The
        // workers cannot see death while pumping — the watcher is
        // their eyes. It exits on either signal and drops its sender
        // clone, so the polite path's stream still ends when the
        // workers do.
        {
            let watcher_shutdown = worker_shutdown.clone();
            let watcher_workers = workers.clone();
            let watcher_events = event_tx.clone();
            tokio::spawn(async move {
                tokio::select! {
                    biased;
                    _ = watcher_shutdown.cancelled() => {}
                    _ = watcher_events.closed() => {
                        // The death path's abort site: preemption
                        // plus the one clear, inside the handle (flag
                        // 6) — the runs' conclusions flush the discard
                        // notices on the way out.
                        for worker in lock(&watcher_workers).values() {
                            worker.abort.abort();
                        }
                    }
                }
            });
        }

        // The resident host loop: routing only — it never awaits a
        // run, so command latency does not exist.
        let mut loop_state = HostLoop {
            workers: workers.clone(),
            wiring,
            event_tx,
            stats: closing_stats.clone(),
            worker_shutdown,
            boot_id: boot_id.clone(),
            joins: vec![boot_join],
        };
        let host_shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = host_shutdown.cancelled() => break,
                    received = command_rx.recv() => match received {
                        None => break,
                        Some(command) => loop_state.handle(command),
                    }
                }
            }
            // Close is not a barrier: commands already queued when the
            // break fired are routed before any worker is cancelled.
            while let Ok(command) = command_rx.try_recv() {
                loop_state.handle(command);
            }
            loop_state.worker_shutdown.cancel();
            for join in loop_state.joins.drain(..) {
                let _ = join.await;
            }
            // The last `event_tx` drops here: the stream ends.
        });

        Self {
            info,
            events: Some(event_rx),
            shutdown,
            commands: command_tx,
            workers,
            closing_stats,
        }
    }

    /// The boot session's facts, captured when the host took over.
    pub fn info(&self) -> &SessionInfo {
        &self.info
    }

    /// Submit a user message to a session: steers the run in flight or
    /// starts one.
    pub fn message(&self, session: &str, text: impl Into<String>) {
        let _ = self
            .commands
            .send(HostCommand::Command(SessionCommand::Message {
                session: session.to_string(),
                text: text.into(),
            }));
    }

    /// Stop a session: abort the run in flight and discard any queued
    /// messages. Aborting while idle is a no-op (including on anything
    /// queued — the queue is discarded with it).
    pub fn abort(&self, session: &str) {
        let _ = self
            .commands
            .send(HostCommand::Command(SessionCommand::Abort {
                session: session.to_string(),
            }));
    }

    /// Move a session's active chain to an entry (checkout — any entry
    /// in the file; an off-chain target is a branch switch). Executed
    /// at the session's pause point: immediately when idle, after the
    /// in-flight run's terminal otherwise (never an implicit abort).
    /// Outcomes arrive as events (`checked_out` + a replay pass, or
    /// `error { kind: checkout }`).
    pub fn checkout(&self, session: &str, entry_id: impl Into<String>) {
        let _ = self
            .commands
            .send(HostCommand::Command(SessionCommand::Checkout {
                session: session.to_string(),
                entry_id: entry_id.into(),
            }));
    }

    /// Abort every session and discard every queue — the
    /// frontend-death door for transport edges (stdin EOF is death:
    /// no run outlives the consumer, ruled 2026-08). Direct, not
    /// routed: death preempts routing.
    pub fn abort_all(&self) {
        for worker in lock(&self.workers).values() {
            worker.abort.abort();
        }
    }

    /// Request a session's replay pass: the resident chain re-emitted
    /// onto the event stream as finalized live events, bracketed by
    /// `replay_started`/`replay_done`. Fire-and-forget like a command
    /// — the pass itself is the acknowledgment. Answered at the
    /// session's next idle beat; requests during a run wait for it.
    pub fn replay(&self, session: &str) {
        let _ = self.commands.send(HostCommand::Replay(session.to_string()));
    }

    /// A cloneable submitter for threads that only send commands.
    pub fn command_link(&self) -> SessionCommandLink {
        SessionCommandLink {
            commands: self.commands.clone(),
        }
    }

    /// Close the host's command side (the polite door — in-process
    /// consumers that stay to read the stream, like print mode). Every
    /// worker finishes any in-flight run, then — close is not a
    /// barrier — runs everything already queued, captures
    /// [`SessionHost::closing_stats`], and the event stream ends.
    /// Sends that raced the wind-down land in one of two places:
    /// before the host's post-break drain, they run; after it, the
    /// channel is closed and they are silent no-ops (the window is a
    /// few instructions wide; the stdio edge never uses this door —
    /// it drops the host, the death door, so nothing unattended
    /// runs).
    pub fn close_commands(&mut self) {
        self.shutdown.cancel();
    }

    /// Take the whole event stream for a long-lived consumer (a
    /// transport forwarder). Once taken, [`SessionHost::next_event`]
    /// yields `None` — one stream, one consumer.
    pub fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<EventFrame>> {
        self.events.take()
    }

    /// The next stamped event, or `None` once the host has wound down
    /// (or the stream was taken).
    pub async fn next_event(&mut self) -> Option<EventFrame> {
        self.events.as_mut()?.recv().await
    }

    /// The boot session's totals captured at worker wind-down, for
    /// callers that want a closing summary (print mode's footer).
    /// `None` until the event stream has ended.
    pub fn closing_stats(&self) -> Option<SessionStats> {
        lock(&self.closing_stats)
            .get(&self.info.session_id)
            .cloned()
    }
}

/// The session a session-scoped command names (v3's always-explicit
/// addressing — lifecycle commands are the exception and never reach
/// this helper).
#[allow(clippy::unreachable)]
fn session_address(command: &SessionCommand) -> &str {
    match command {
        SessionCommand::Message { session, .. }
        | SessionCommand::Abort { session }
        | SessionCommand::InteractionResponse { session, .. }
        | SessionCommand::Checkout { session, .. } => session,
        // Matched before the session-scoped arm in `handle`;
        // unreachable by construction. Sanctioned crash: see the
        // error doctrine in AGENTS.md.
        SessionCommand::NewSession | SessionCommand::OpenSession { .. } => {
            unreachable!("lifecycle commands carry no session address")
        }
    }
}

/// The host loop's own state: what routing needs, plus the worker
/// joins it owns to the end (awaiting them is what orders the stream's
/// end after every worker's last event) and the workers' shutdown
/// token, pulled only after the pre-close queue has been routed.
struct HostLoop {
    workers: Arc<Mutex<HashMap<String, Worker>>>,
    wiring: SessionHostWiring,
    event_tx: mpsc::UnboundedSender<EventFrame>,
    stats: Arc<Mutex<HashMap<String, SessionStats>>>,
    worker_shutdown: CancellationToken,
    boot_id: String,
    joins: Vec<JoinHandle<()>>,
}

impl HostLoop {
    /// Route one command. The router only routes: lifecycle is the
    /// host's own module (the v3 ruling — session lifecycle never
    /// waits on a session); everything else is session-scoped, so
    /// resolve the address and forward into the session's handler —
    /// a black box to this loop. The only errors born here are
    /// routing failures (an unknown session).
    fn handle(&mut self, command: HostCommand) {
        match command {
            HostCommand::Command(SessionCommand::NewSession) => self.new_session(),
            HostCommand::Command(SessionCommand::OpenSession { id }) => self.open_session(&id),
            HostCommand::Replay(session) => {
                if let Some(worker) = self.worker(&session) {
                    worker.deliver_replay();
                }
            }
            HostCommand::Command(command) => {
                if let Some(worker) = self.worker(session_address(&command)) {
                    worker.deliver(command);
                }
            }
        }
    }

    /// The named session's leaves, or the `error { kind: session }`
    /// frame on the wire when no such session is open here.
    fn worker(&self, session: &str) -> Option<Worker> {
        lock(&self.workers).get(session).cloned().or_else(|| {
            let _ = self.event_tx.send(EventFrame {
                stream: StreamId::new(session.to_string()),
                event: SessionEvent::error_session(format!(
                    "unknown session `{session}` — not open in this backend \
                     (open_session loads it; sessions_available lists the stored ones)"
                )),
            });
            None
        })
    }

    /// `new_session`: announce, then spawn. The creation frame and its
    /// notes land ahead of anything the worker can emit (one sender,
    /// sent first).
    fn new_session(&mut self) {
        let (session, notes) = match (self.wiring.create)() {
            Ok(built) => built,
            Err(message) => {
                let _ = self.event_tx.send(EventFrame {
                    stream: StreamId::new(self.boot_id.clone()),
                    event: SessionEvent::error_session(format!(
                        "could not build a new session: {message}"
                    )),
                });
                return;
            }
        };
        let id = session.id().to_string();
        let stream = StreamId::new(id.clone());
        // The selection rides the frame — a fresh session resolves its
        // own model, which can differ from the boot's, and nothing
        // else on the wire will say so (the session is empty; no
        // `model_changed` replays).
        let _ = self.event_tx.send(EventFrame {
            stream: stream.clone(),
            event: SessionEvent::SessionCreated {
                id: id.clone(),
                path: session.path().display().to_string(),
                model: session.selection().clone(),
            },
        });
        for note in notes {
            let _ = self.event_tx.send(EventFrame {
                stream: stream.clone(),
                event: SessionEvent::error_model(note),
            });
        }
        self.add_worker(id, session);
    }

    /// `open_session`: already open means re-replay (idempotent);
    /// otherwise load, surface the notes, spawn, and answer with the
    /// pass — the pass itself is the acknowledgment.
    fn open_session(&mut self, id: &str) {
        if let Some(worker) = lock(&self.workers).get(id).cloned() {
            worker.deliver_replay();
            return;
        }
        let (session, notes) = match (self.wiring.open)(id) {
            Ok(loaded) => loaded,
            Err(message) => {
                let _ = self.event_tx.send(EventFrame {
                    stream: StreamId::new(id.to_string()),
                    event: SessionEvent::error_session(format!(
                        "could not open session `{id}`: {message}"
                    )),
                });
                return;
            }
        };
        let stream = StreamId::new(id.to_string());
        for note in notes {
            let _ = self.event_tx.send(EventFrame {
                stream: stream.clone(),
                event: SessionEvent::error_model(note),
            });
        }
        let worker = self.add_worker(id.to_string(), session);
        worker.deliver_replay();
    }

    /// Spawn a session's worker and register it. Returns the routing
    /// leaves for immediate use.
    fn add_worker(&mut self, id: String, session: Session) -> Worker {
        let (worker, join) = spawn_worker(
            session,
            self.event_tx.clone(),
            self.worker_shutdown.clone(),
            self.stats.clone(),
        );
        lock(&self.workers).insert(id, worker.clone());
        self.joins.push(join);
        worker
    }
}

/// Spawn one session's resident worker: the classic loop — ownership
/// never moves (idle is the wait below, running is the pump call),
/// with the session's id as its stream stamp. Returns the routing
/// leaves and the task handle.
fn spawn_worker(
    mut session: Session,
    event_tx: mpsc::UnboundedSender<EventFrame>,
    shutdown: CancellationToken,
    stats: Arc<Mutex<HashMap<String, SessionStats>>>,
) -> (Worker, JoinHandle<()>) {
    let id = session.id().to_string();
    let stream = StreamId::new(id.clone());
    let mailbox = session.mailbox_handle();
    let abort = session.abort_handle();
    let interaction = InteractionHub::new(event_tx.clone(), stream.clone());
    let checkout_slot = Arc::new(Mutex::new(None::<String>));
    let replay_due = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_events = event_tx.downgrade();
    let worker_stream = stream.clone();
    let entry_probe = session.entry_id_probe();
    let worker_slot = checkout_slot.clone();
    let worker_replay_due = replay_due.clone();
    let worker_mailbox = mailbox.clone();
    let task_interaction = interaction.clone();
    let join = tokio::spawn(async move {
        // The hub and the mailbox's submit-time notices both reach the
        // event channel, so both exist only here - attach them before
        // the first pump can run.
        session.attach_interaction(task_interaction);
        session.attach_mailbox_notices(event_tx.clone(), stream.clone());
        // The resident worker. Ownership never moves: idle is the wait
        // below, running is the pump call - two positions of one loop,
        // not two tasks. One wake (the work signal) serves every
        // pending thing; the beat at the loop top is the single drain
        // point.
        loop {
            // Discards staged by an abort that no run conclusion
            // flushed (abort while idle - there is no terminal coming;
            // mid-run aborts flush at their run's conclusion first).
            flush_staged_discards(&worker_mailbox, &event_tx, &stream);
            // The beat, in its ruled order: a parked pass answers
            // first (a read of the chain as it stands), then a parked
            // checkout (the rewind - the one session mutation - plus
            // its re-render), then the empties check batches messages.
            // Reads and rewinds requested ahead of a message answer
            // ahead of it; a message's inclusion in a pass is decided
            // solely by whether it drained before the beat.
            if replay_due.swap(false, std::sync::atomic::Ordering::Acquire) {
                emit_replay(&session, &event_tx, &stream);
            }
            if let Some(entry_id) = lock(&checkout_slot).take() {
                execute_checkout(&mut session, &event_tx, &stream, entry_id);
            }
            if !worker_mailbox.is_empty() {
                // The pause seam: a checkout parked while this pump
                // drained a batch must rewind before a survivor starts
                // the next batch on the old chain.
                session
                    .pump_with_pause(
                        &mut |event| {
                            // The receiver is gone only when the
                            // frontend is; there is no one left to
                            // tell.
                            let _ = event_tx.send(EventFrame {
                                stream: stream.clone(),
                                event,
                            });
                        },
                        || lock(&checkout_slot).is_none(),
                    )
                    .await;
                continue;
            }
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    // Close is not a barrier: pushes are synchronous,
                    // so anything sent before closing is already
                    // queued - run it before winding down. (Pushes
                    // that race the wind-down simply run too; nothing
                    // is lost.)
                    if !worker_mailbox.is_empty() {
                        continue;
                    }
                    // Serve what the handler parked ahead of the
                    // close, then wind down through the same flush the
                    // idle beat uses - an abort's staged pairs must
                    // not die with the worker (flag 6 - nothing
                    // user-authored leaves silently, not even at
                    // close).
                    if replay_due.swap(false, std::sync::atomic::Ordering::Acquire) {
                        emit_replay(&session, &event_tx, &stream);
                    }
                    if let Some(entry_id) = lock(&checkout_slot).take() {
                        execute_checkout(&mut session, &event_tx, &stream, entry_id);
                    }
                    flush_staged_discards(&worker_mailbox, &event_tx, &stream);
                    break;
                }
                // The frontend is gone; the death watcher has already
                // aborted any in-flight run, so the pump has returned.
                _ = event_tx.closed() => break,
                // The one wake: any pending thing (a message push, a
                // parked checkout or pass, an abort) lands here and
                // loops back to the beat.
                _ = worker_mailbox.work_signal().notified() => {}
            }
        }
        match session.stats() {
            Ok(session_stats) => {
                lock(&stats).insert(id, session_stats);
            }
            // Nobody is left to tell at wind-down - but silence is not
            // the doctrine: trace it (the closing summary's absence is
            // the user-visible symptom).
            Err(error) => {
                tracing::warn!(session = %id, %error, "closing stats unreadable at wind-down");
            }
        }
        // The worker's `event_tx` drops here; the stream ends when the
        // host's does too.
    });
    (
        Worker {
            mailbox,
            abort,
            interaction,
            events: worker_events,
            stream: worker_stream,
            entry_probe,
            checkout_slot: worker_slot,
            replay_due: worker_replay_due,
        },
        join,
    )
}

/// Execute the parked checkout at a pause point: rewind the chain,
/// announce, re-render. The discard already happened at receive (the
/// handler's clear); an execution-time failure - the rewind cannot
/// apply - is the command's error event and a no-op (verification
/// caught the common failure at receive; these are the environmental
/// ones: persist trouble, the chain's model gone from config).
fn execute_checkout(
    session: &mut Session,
    event_tx: &mpsc::UnboundedSender<EventFrame>,
    stream: &StreamId,
    entry_id: String,
) {
    if let Err(error) = session.rewind_to_entry(&entry_id) {
        let _ = event_tx.send(EventFrame {
            stream: stream.clone(),
            event: SessionEvent::error_checkout(error.to_string()),
        });
        return;
    }
    let _ = event_tx.send(EventFrame {
        stream: stream.clone(),
        event: SessionEvent::CheckedOut {
            entry_id,
            // Full re-render (the suffix mode's reserved seam).
            base_id: None,
        },
    });
    emit_replay(session, event_tx, stream);
}

/// The replay pass (PROTOCOL.md v2): the resident chain projected
/// into finalized live events, bracketed. One emission path for its
/// two askers — the transport's replay request and checkout's
/// re-render.
fn emit_replay(session: &Session, event_tx: &mpsc::UnboundedSender<EventFrame>, stream: &StreamId) {
    let events = session.replay_events();
    let total = events.len() as u64;
    let _ = event_tx.send(EventFrame {
        stream: stream.clone(),
        event: SessionEvent::ReplayStarted { total },
    });
    for event in events {
        let _ = event_tx.send(EventFrame {
            stream: stream.clone(),
            event,
        });
    }
    let _ = event_tx.send(EventFrame {
        stream: stream.clone(),
        event: SessionEvent::ReplayDone,
    });
}

#[cfg(test)]
#[path = "endpoint_tests.rs"]
mod tests;
