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
//! dequeue point) owns every command's semantics: the mailbox
//! (messages — consumed mid-run by the engine as steers, at the beat
//! by the worker as batches), the cancel token, the interaction hub,
//! a pending-checkout slot, a replay-request flag — the conversation
//! intent — plus the shared model register (a state write at receive,
//! never parked: the worker's next run open derives from it, and
//! every pass announces it). The worker task owns the session itself
//! and serves its beat — passes, then a parked checkout (the rewind),
//! then message batches — so routing never blocks on a run.
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
    abort_handle: AbortHandle,
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
    /// (see [`crate::session::SharedConversation`]).
    entry_probe: crate::session::SharedConversation,
    /// Pending checkout intent — a slot, not a queue: a newer checkout
    /// replaces an older (collapse; the newer intent is the intent),
    /// abort clears it (drop-all-pending-intent), and the worker takes
    /// it at its beat for the rewind.
    checkout_slot: Arc<Mutex<Option<String>>>,
    /// The shared model register — the `model` command's write path at
    /// receive: `write` records the entry and swaps the live cell in
    /// one operation, from this thread (a state write, not pending
    /// intent — the worker is uninvolved; the next run open derives
    /// the agent, every pass announces the cell, abort never hears
    /// about it). Receive-time validation is [`Self::model_probe`].
    model_register: crate::session::ModelRegister,
    /// Receive-time validation against the session's config (the
    /// checkout probe's sibling): an unusable ref is an
    /// `error { kind: model }` at the command, even mid-run.
    model_probe: crate::session::ModelProbe,
    /// A parked replay request (idempotent read — one flag collapses
    /// any number of requests; the beat serves it before batching).
    replay_due: Arc<std::sync::atomic::AtomicBool>,
}

impl Worker {
    /// Abort is drop-all-pending-intent — one semantic at every door:
    /// the command, [`SessionHost::abort_all`], the frontend-death
    /// watcher, and checkout (which aborts its way to its own pause
    /// point). The parked checkout goes first — silently (no
    /// `checked_out` follows; the abort is the marker, FRONTEND.md §7)
    /// and before the cancel, so a worker woken by the abort can never
    /// reach the beat and execute a rewind the abort meant to drop.
    /// State writes (the model register) are not intent and are
    /// already done — abort has nothing to say about them.
    /// The cancel itself (the run's abort plus its immediate
    /// `messages_discarded` notice) lives in the handle.
    fn abort(&self) {
        lock(&self.checkout_slot).take();
        self.abort_handle.abort();
    }

    /// Deliver a session-scoped command — the handler at the dequeue
    /// point. Everything from here down is this module's semantics.
    #[allow(clippy::unreachable)]
    fn deliver(&self, command: SessionCommand) {
        match command {
            SessionCommand::Message { text, .. } => self.mailbox.submit(text),
            SessionCommand::Abort { .. } => self.abort(),
            SessionCommand::Continue { .. } => self.mailbox.continue_run(),
            SessionCommand::InteractionResponse { id, payload, .. } => {
                // Total: an unknown or dead id logs and drops inside
                // the hub; the payload is the asker's to parse.
                self.interaction.respond(&id, payload);
            }
            SessionCommand::Checkout { entry_id, .. } => {
                // Validate against this module's own id truth, here at
                // receive: a bad target errors immediately — even
                // mid-run — and nothing else happens.
                if !self.entry_probe.contains(&entry_id) {
                    if let Some(events) = self.events.upgrade() {
                        let _ = events.send(EventFrame {
                            stream: Some(self.stream.clone()),
                            event: SessionEvent::error_checkout(format!(
                                "no entry `{entry_id}` in this session"
                            )),
                        });
                    }
                    return;
                }
                // Checkout aborts first (ruled 2026-08: the user
                // rewinding has declared the run's continuation
                // obsolete — checkout composes abort, it does not wait
                // on the run). The abort's clear IS the
                // discard-at-receive: what was submitted before this
                // command dies now, in wire order, its notice emitted
                // immediately; what already entered the conversation
                // is history the rewind drops. Messages submitted
                // after queue normally for the rewound chain.
                self.abort();
                // Pending intent, not a queue: the newer checkout is
                // the intent.
                lock(&self.checkout_slot).replace(entry_id);
                self.mailbox.work_signal().notify_one();
            }
            SessionCommand::Model {
                session: _,
                provider,
                model,
                thinking_level,
            } => {
                // Validate against config here, at receive — the
                // checkout probe's pattern: a picker gets its error
                // immediately, even mid-run.
                let selection = ModelSelection {
                    provider,
                    model,
                    thinking_level,
                };
                if let Err(message) = (self.model_probe)(&selection) {
                    if let Some(events) = self.events.upgrade() {
                        let _ = events.send(EventFrame {
                            stream: Some(self.stream.clone()),
                            event: SessionEvent::error_model(message),
                        });
                    }
                    return;
                }
                // A state write, not pending intent: one register write
                // (entry + live cell, any thread — the recorder's
                // append is internally locked), announced now. The
                // worker is uninvolved — no park, no wake, no abort
                // question: the next run open derives the agent, and
                // every pass announces the cell.
                self.model_register.write(selection.clone());
                if let Some(events) = self.events.upgrade() {
                    announce_model(&selection, &events, &self.stream);
                }
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
            model: boot.selection(),
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
                stream: Some(boot_stream.clone()),
                event: SessionEvent::error_model(note),
            });
        }
        match wiring.store.list() {
            Ok(summaries) => {
                // Backend-level: no session produced this (the optional-
                // stream ruling).
                let _ = event_tx.send(EventFrame {
                    stream: None,
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
                    stream: None,
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
                        // The death path's abort site — the same
                        // drop-all-pending-intent as the command's: a
                        // parked checkout must not outlive the user
                        // (its rewind is durable). Preemption plus the
                        // one clear live inside the handle (flag 6) —
                        // the runs' conclusions flush the discard
                        // notices on the way out.
                        for worker in lock(&watcher_workers).values() {
                            worker.abort();
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

    /// Start a run over the session's existing conversation with no
    /// new message (retry / continue). A no-op on an empty
    /// conversation.
    pub fn continue_run(&self, session: &str) {
        let _ = self
            .commands
            .send(HostCommand::Command(SessionCommand::Continue {
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

    /// Switch a session's model — the register write, immediate: the
    /// entry and the live selection land at receive (one shared-write
    /// operation, any thread), and `model_changed` follows at once. A
    /// ref config cannot resolve is an `error { kind: model }`,
    /// nothing moves. A run in flight is untouched (passes bind the
    /// agent at run open); the next run derives the new agent. Abort
    /// is irrelevant — a state write is not conversation intent.
    pub fn model(&self, session: &str, selection: ModelSelection) {
        let _ = self
            .commands
            .send(HostCommand::Command(SessionCommand::Model {
                session: session.to_string(),
                provider: selection.provider,
                model: selection.model,
                thinking_level: selection.thinking_level,
            }));
    }

    /// Abort every session — discard every queue and every parked
    /// checkout — the frontend-death door for transport edges (stdin
    /// EOF is death: no run outlives the consumer, and no rewind
    /// executes unattended, ruled 2026-08). Direct, not routed: death
    /// preempts routing.
    pub fn abort_all(&self) {
        for worker in lock(&self.workers).values() {
            worker.abort();
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
        | SessionCommand::Continue { session }
        | SessionCommand::InteractionResponse { session, .. }
        | SessionCommand::Checkout { session, .. }
        | SessionCommand::Model { session, .. } => session,
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
                stream: None,
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
                    stream: None,
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
        // Backend-level: the payload carries the new session's id (no
        // faked stamp — the optional-stream ruling).
        let _ = self.event_tx.send(EventFrame {
            stream: None,
            event: SessionEvent::SessionCreated {
                id: id.clone(),
                path: session.path().display().to_string(),
                model: session.selection(),
            },
        });
        for note in notes {
            let _ = self.event_tx.send(EventFrame {
                stream: Some(stream.clone()),
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
                    stream: None,
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
                stream: Some(stream.clone()),
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
    let abort_handle = session.abort_handle();
    let interaction = InteractionHub::new(event_tx.clone(), stream.clone());
    let checkout_slot = Arc::new(Mutex::new(None::<String>));
    let replay_due = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_events = event_tx.downgrade();
    let worker_stream = stream.clone();
    let entry_probe = session.entry_id_probe();
    let model_probe = session.model_probe();
    let model_register = session.model_register();
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
        session.attach_persist_notices(event_tx.downgrade(), stream.clone());
        // The resident worker. Ownership never moves: idle is the wait
        // below, running is the pump call - two positions of one loop,
        // not two tasks. One wake (the work signal) serves every
        // pending thing; the beat at the loop top is the single drain
        // point.
        loop {
            // The beat, in its ruled order: a parked pass answers
            // first (a read of the chain as it stands), then a parked
            // checkout (the rewind - the one session mutation - plus
            // its re-render), then the empties check batches messages.
            // Reads and rewinds requested ahead of a message answer
            // ahead of it; a message's inclusion in a pass is decided
            // solely by whether it drained before the beat. (The model
            // register needs no beat arm: its writes land at receive,
            // and the passes announce it live.)
            if replay_due.swap(false, std::sync::atomic::Ordering::Acquire) {
                emit_replay(&session, &event_tx, &stream);
            }
            if let Some(entry_id) = lock(&checkout_slot).take() {
                execute_checkout(&mut session, &event_tx, &stream, entry_id);
            }
            if !worker_mailbox.is_empty() || worker_mailbox.has_continue() {
                // The pump returns on an aborted outcome (a checkout
                // aborts its way here), so anything parked behind a
                // run executes at this beat before a later message
                // starts the next batch on the old chain.
                session
                    .pump(&mut |event| {
                        // The receiver is gone only when the
                        // frontend is; there is no one left to
                        // tell.
                        let _ = event_tx.send(EventFrame {
                            stream: Some(stream.clone()),
                            event,
                        });
                    })
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
                    // close (the same beat order), then wind down.
                    // (Register writes are already durable — receive
                    // wrote them.)
                    if replay_due.swap(false, std::sync::atomic::Ordering::Acquire) {
                        emit_replay(&session, &event_tx, &stream);
                    }
                    if let Some(entry_id) = lock(&checkout_slot).take() {
                        execute_checkout(&mut session, &event_tx, &stream, entry_id);
                    }
                    // The clean-exit flush attempt (flag 8): one more
                    // drain before the stream ends.
                    session.flush_log();
                    break;
                }
                // The frontend is gone; the death watcher has already
                // aborted any in-flight run, so the pump has returned.
                // The process may outlive this wind-down (an
                // in-process consumer dropped the host), so the same
                // clean-exit drain applies.
                _ = event_tx.closed() => {
                    session.flush_log();
                    break;
                }
                // The one wake: any pending thing (a message push, a
                // parked checkout or pass) lands here and loops back
                // to the beat.
                _ = worker_mailbox.work_signal().notified() => {}
            }
        }
        lock(&stats).insert(id, session.stats());
        // The worker's `event_tx` drops here; the stream ends when the
        // host's does too.
    });
    (
        Worker {
            mailbox,
            abort_handle,
            interaction,
            events: worker_events,
            stream: worker_stream,
            entry_probe,
            checkout_slot: worker_slot,
            model_register,
            model_probe,
            replay_due: worker_replay_due,
        },
        join,
    )
}

/// The register announcement: a `model_changed` carrying a selection —
/// at every receive-time write (the `model` command's own outcome) and
/// before every replay pass (a session becoming visible always tells
/// its model). One construction site, two askers.
fn announce_model(
    selection: &ModelSelection,
    event_tx: &mpsc::UnboundedSender<EventFrame>,
    stream: &StreamId,
) {
    let _ = event_tx.send(EventFrame {
        stream: Some(stream.clone()),
        event: SessionEvent::ModelChanged {
            provider: selection.provider.clone(),
            model: selection.model.clone(),
            thinking_level: selection.thinking_level.clone(),
        },
    });
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
    let res = session.rewind_to_entry(&entry_id);
    if let Err(error) = res {
        let _ = event_tx.send(EventFrame {
            stream: Some(stream.clone()),
            event: SessionEvent::error_checkout(error.to_string()),
        });
        return;
    }
    let _ = event_tx.send(EventFrame {
        stream: Some(stream.clone()),
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
/// askers — the transport's replay request, checkout's re-render, and
/// the open_session boot pass — each led by the register announcement
/// ([`announce_model`], shared with the applied model switch): a
/// session becoming visible (boot, open, re-replay, checkout) always
/// tells the frontend its active selection. Idempotent by
/// construction — a pass never moves the register, so the value
/// repeats; replayed history itself never carries `model_changed` (the
/// register ruling: state is announced live, not reconstructed).
fn emit_replay(session: &Session, event_tx: &mpsc::UnboundedSender<EventFrame>, stream: &StreamId) {
    announce_model(&session.selection(), event_tx, stream);
    let events = session.replay_events();
    let total = events.len() as u64;
    let _ = event_tx.send(EventFrame {
        stream: Some(stream.clone()),
        event: SessionEvent::ReplayStarted { total },
    });
    for event in events {
        let _ = event_tx.send(EventFrame {
            stream: Some(stream.clone()),
            event,
        });
    }
    let _ = event_tx.send(EventFrame {
        stream: Some(stream.clone()),
        event: SessionEvent::ReplayDone,
    });
}

#[cfg(test)]
#[path = "endpoint_tests.rs"]
mod tests;
