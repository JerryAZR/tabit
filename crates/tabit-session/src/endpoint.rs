//! The session host: the backend half of the frontend protocol — a
//! functional layer mounted on its [`Node`] (the ruled architecture:
//! one routing layer, the node; the host is policy). One host
//! connection serves many sessions: a resident worker per session,
//! each worker the classic resident owner (one task owns its
//! [`Session`] exclusively and forever; idle is the wait, running is
//! the pump — no handoff window to patch). Runs in different
//! sessions proceed concurrently; every worker stamps its events
//! with its session id and emits through its channel — which IS how
//! the node's learning table knows where a session lives (law 1:
//! stamped emissions teach).
//!
//! The host, not the workers, owns session lifecycle: `new_session`
//! builds a session through the injected wiring (the binary's assembly
//! knowledge — config, tools, preamble — kept out of this crate),
//! `open_session` loads a stored one, and the startup catalog
//! (`sessions_available`) is a header-only listing so lazy loading
//! holds: only the boot session is resident at startup. Lifecycle
//! rides the node's by-type dispatch (law 3); session-addressed
//! commands ride the learning table (law 2) into the worker's
//! channel — the handler at the command dequeue point.
//!
//! The command path (ruled 2026-08): **the router only routes** —
//! the node resolves a session address and forwards into that
//! session's handler, a black box to the router; routing failures
//! (an unknown session) are its only errors. The handler
//! ([`Worker::deliver`], module code running synchronously at the
//! dequeue point) owns every command's semantics: the mailbox
//! (messages — consumed mid-run by the engine as steers, at the beat
//! by the worker as batches), the cancel token, a pending-checkout
//! slot, a replay-request flag — the conversation intent — plus the
//! shared model register (a state write at receive, never parked:
//! the worker's next run open derives from it, and every pass
//! announces it). The worker task owns the session itself and serves
//! its beat — passes, a parked checkout (the rewind), a parked
//! manual compaction (all three in `serve_parked`'s one order), then
//! message batches, then the idle compaction door — so routing never
//! blocks on a run.
//!
//! Termination (ruled 2026-08 — the core dies with the frontend):
//!
//! - [`SessionHost::close_commands`] is the **polite** close: every
//!   worker finishes its in-flight run, everything already delivered
//!   is honored (delivery is synchronous — there is no queue to
//!   drain), closing stats are captured, and the event stream ends.
//!   In-process consumers that stay alive to read the stream (print
//!   mode) use this.
//! - **Frontend death** — the event receiver is gone, whatever the
//!   reason — aborts every in-flight run and winds every worker down
//!   immediately, regardless of state: a parked permission card or a
//!   half-finished turn must never outlive the user. Interrupted
//!   results synthesize on the next open exactly like a crash; the
//!   log stays durable. The door is the frontend-death watcher over
//!   the node's frontend channel (the receiver's drop, detected
//!   directly).

use crate::interaction::InteractionHub;
use crate::lock::lock;
use crate::notice::{HostSink, NoticeSink, NoticeSlot};
use crate::session::{AbortHandle, MailboxHandle, Session};
use crate::stats::SessionStats;
use crate::store::SessionStore;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tabit_protocol::{
    AvailableSession, EventFrame, ModelSelection, SessionCommand, SessionEvent, StreamId,
};
use tabit_wire::node::{Channel, Inbound, Locality, Node};
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
    /// The session's working directory.
    pub session_cwd: String,
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

/// The host's STRUCTURE — everything mountable before any data
/// exists (owner ruling 2026-09, the prepared-supervisor model): the
/// node (the routing layer this host mounts on — the assembly creates
/// it once, so the subprocess bridge's children register their lanes
/// on the same net), the store, and the boot announcement's lineage.
/// Mounting the structure first is what lets chatty participants
/// speak from their handshake onward: any node may send anything a
/// frontend can (`new_session` at least), so the command surface —
/// the by-type handlers — must exist before the first child boots.
#[derive(Clone)]
pub struct SessionHostWiring {
    /// The node this host and its children share — one net per
    /// process.
    pub node: Arc<Node>,
    /// The sessions directory the catalog lists.
    pub store: SessionStore,
    /// The `parent` field on the boot session's announcement — the
    /// child-role flag speaking at the source of truth.
    pub boot_parent: Option<String>,
    /// The `parent_call` field on the boot session's announcement —
    /// the spawning tool call's correlation id, crossing the same
    /// way `boot_parent` does. Present only in a child-role boot.
    pub boot_parent_call: Option<String>,
}

/// The host's DATA — the parts that only exist once the boot's
/// gathering is done (the extension handshakes resolved, the tools
/// mounted): the two session builders and the startup catalogs.
/// Arrives with the boot session at [`SessionHostMount::attach`].
#[derive(Clone)]
pub struct SessionHostData {
    /// Build a fresh session (`new_session`).
    pub create: SessionSource,
    /// Load a stored session by id (`open_session`).
    pub open: OpenSessionSource,
    /// The extension catalog's wire snapshot, announced once at
    /// startup after the skills catalog (empty = no announcement) —
    /// the binary's boot-time assembly verdict: provenance, standing,
    /// and the load-time conflict reports.
    pub extensions: tabit_protocol::ExtensionsCatalog,
}

/// One session's delivery surface — the module's handler at the
/// command dequeue point, opaque to the router (owner ruling 2026-08:
/// **the router only routes** — it resolves a session address and
/// forwards; every command's semantics live here, in module code,
/// running synchronously at receive). The worker task holds the
/// session itself and consumes the pending intent this struct
/// manages: the beat serves the parked intent in `serve_parked`'s
/// one order (a replay pass, a checkout, a manual compaction), then
/// batches messages, then the idle compaction door.
///
/// Cheap to clone behind its `Arc` — the learning table's channel
/// delivery holds one, and the lifecycle registry (the host's worker
/// list) another.
#[derive(Clone)]
struct Worker {
    mailbox: MailboxHandle,
    abort_handle: AbortHandle,
    /// The notice sink for the handler's own emissions (checkout
    /// errors, model answers) — attached at spawn, once the session's
    /// channel exists (a sink is the channel it emits from; the
    /// channel is the worker's, so the two are minted together in
    /// [`spawn_worker`]).
    notices: Arc<NoticeSlot>,
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
    /// A parked `compact` command (the manual door): served at the
    /// beat ahead of any queued batch. A slot, not a queue — a newer
    /// command replaces an older; abort clears it
    /// (drop-all-pending-intent). The nested option: outer = whether
    /// a compact is parked at all, inner = the invocation's
    /// directives (`compact` without directives is a parked compact,
    /// not no compact).
    compact_slot: Arc<Mutex<Option<Option<String>>>>,
}

impl Worker {
    /// Abort is drop-all-pending-intent — one semantic at every door:
    /// the command, [`SessionHost::abort_all`], the frontend-death
    /// door, and checkout (which aborts its way to its own pause
    /// point). The parked checkout goes first — silently (no
    /// `checked_out` follows; the abort is the marker, FRONTEND.md §7)
    /// and before the cancel, so a worker woken by the abort can never
    /// reach the beat and execute a rewind the abort meant to drop.
    /// State writes (the model register) are not intent and are
    /// already done — abort has nothing to say about them.
    /// The cancel itself (the run's abort plus its immediate
    /// `messages_discarded` notice) lives in the handle.
    /// Abort carries no routing machinery (the routing ruling): the
    /// run token is every tool body's leash, so the abort cascades
    /// through the active tool calls — a subagent tool kills its own
    /// child — and children with no active call survive naturally.
    fn abort(&self) {
        lock(&self.checkout_slot).take();
        lock(&self.compact_slot).take();
        self.abort_handle.abort();
    }

    /// The handler's emission: a notice stamped with the session's
    /// stream, if the worker's sink is attached (a module talking to
    /// its frontend, not the router's business).
    fn notice(&self, event: SessionEvent) {
        if let Some(notices) = self.notices.get() {
            notices.emit(event);
        }
    }

    /// Deliver a session-scoped command — the handler at the dequeue
    /// point (the learning table forwarded into the session's
    /// channel). Everything from here down is this module's
    /// semantics. Interaction responses never arrive here: they are
    /// response-type, claimed by the node's ask table before session
    /// routing is ever consulted (law 5).
    #[allow(clippy::unreachable)]
    fn deliver(&self, command: SessionCommand) {
        match command {
            SessionCommand::Message { text, .. } => self.mailbox.submit(text),
            SessionCommand::Abort { .. } => self.abort(),
            SessionCommand::Continue { .. } => self.mailbox.continue_run(),
            SessionCommand::Checkout { entry_id, .. } => {
                // Validate against this module's own id truth, here at
                // receive: a bad target errors immediately — even
                // mid-run — and nothing else happens.
                if !self.entry_probe.contains(&entry_id) {
                    self.notice(SessionEvent::error_checkout(format!(
                        "no entry `{entry_id}` in this session"
                    )));
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
                    self.notice(SessionEvent::error_model(message));
                    return;
                }
                // A state write, not pending intent: one register write
                // (entry + live cell, any thread — the recorder's
                // append is internally locked), announced now. The
                // worker is uninvolved — no park, no wake, no abort
                // question: the next run open derives the agent, and
                // every pass announces the cell.
                self.model_register.write(selection.clone());
                self.notice(SessionEvent::model_changed(
                    &selection,
                    self.model_register.facts(&selection),
                ));
            }
            // The manual compaction door: parks as pending intent and
            // runs at the beat (idle position, ahead of queued
            // batches). Compaction never aborts a run — it does not
            // move the chain, so a run in flight finishes first.
            SessionCommand::Compact { directives, .. } => {
                // Park the manual compact with its directives (the
                // parking key: a second compact replaces the first —
                // last wins, exactly one invocation serves).
                *lock(&self.compact_slot) = Some(directives);
                self.mailbox.work_signal().notify_one();
            }
            // Response-type commands are claimed at the ask table
            // before routing; lifecycle is by-type. Both are
            // unreachable by construction; sanctioned crash: see the
            // error doctrine in AGENTS.md.
            SessionCommand::InteractionResponse { .. } => {
                unreachable!("interaction responses are claimed by the ask table")
            }
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
    node: Arc<Node>,
    host_channel: Channel,
    /// The lifecycle registry — the workers by session id, for the
    /// doors that need the worker itself (open_session's already-open
    /// check, the replay request, the abort sweep). Routing is NOT
    /// this map's business: session-addressed commands route by the
    /// node's learning table (law 2), taught by each worker's own
    /// stamped emissions.
    workers: Arc<Mutex<HashMap<String, Arc<Worker>>>>,
    closing_stats: Arc<Mutex<HashMap<String, SessionStats>>>,
    /// The workers' wind-down: pulled by the polite close, the
    /// frontend-death door, and the facade's own drop (the stdio
    /// edge's explicit death). The wind-down task awaits every join
    /// and then ends the event stream.
    worker_shutdown: CancellationToken,
    /// Fires once every worker has wound down and its last event has
    /// landed — the event stream's end.
    stream_end: CancellationToken,
}

impl Drop for SessionHost {
    fn drop(&mut self) {
        // The edge's death door: dropping the host IS the frontend's
        // death (the edge aborts first, explicitly). The death door
        // in the frontend channel's delivery covers the in-process
        // consumer that drops its receiver instead.
        self.worker_shutdown.cancel();
    }
}

/// A cheap clone for threads that only submit commands (a transport
/// edge's reader). Commands enter through the node's intake — one
/// door, the same laws as any arrival; sends after the host has wound
/// down simply find no worker listening.
#[derive(Clone)]
pub struct SessionCommandLink {
    node: Arc<Node>,
    host_channel: Channel,
}

impl SessionCommandLink {
    /// Submit a command. Fire-and-forget: outcomes arrive as events
    /// (routing by the node's laws — session-addressed by the
    /// learning table, lifecycle by type, responses by ask-table
    /// claim). The replay request rides this too — under the report
    /// model, replay is the door's idempotent path (`open_session` of
    /// an already-open session) plus the automatic pass a resumed
    /// boot serves at attach.
    pub fn send(&self, command: SessionCommand) {
        self.node
            .intake(&self.host_channel, Inbound::Command(command));
    }
}

/// The frontend's event stream, mounted on the node — constructible
/// **before any functional layer exists**: the structure-first mount
/// (the boot's routing is ready before any data is gathered — the
/// extensions boot next, the session host last, and every frame any
/// of them emits from its first line crosses through this stream in
/// arrival order). Participants are peers, not subordinates: any
/// node may speak from its handshake onward (a co-frontend
/// extension's `new_session`, a child's steer), so the net's
/// structure — this stream, the tables, the by-type handlers — is
/// complete before the first child boots, and no buffering exists
/// anywhere: the boot's own announcements land behind whatever
/// crossed earlier, in arrival order.
pub struct FrontendStream {
    events: mpsc::UnboundedReceiver<EventFrame>,
    /// The sender the death-watch awaits (dropped receivers close
    /// it); held here so the host — which spawns inside a runtime —
    /// owns the watcher task.
    events_tx: mpsc::UnboundedSender<EventFrame>,
    /// Fires when the stream's receiver drops — the frontend-death
    /// signal; the host (whenever it spawns) owns the door it opens.
    gone: CancellationToken,
}

/// Mount the frontend stream on the node: the facade channel
/// (subscribed to every event, its delivery feeding the stream), and
/// the receiver-drop watcher that fires `gone`.
pub fn mount_frontend(node: &Arc<Node>) -> FrontendStream {
    let (event_tx, event_rx) = mpsc::unbounded_channel::<EventFrame>();
    let send_events = event_tx.clone();
    let frontend = Channel::local(
        "frontend",
        move |frame: &EventFrame| {
            let _ = send_events.send(frame.clone());
        },
        |_| {},
    );
    node.subscribe_channel_all(Locality::Both, &frontend);
    FrontendStream {
        events: event_rx,
        events_tx: event_tx,
        gone: CancellationToken::new(),
    }
}

/// The mounted-but-unattached host: the structure is up (the
/// frontend stream, the host's channel and sink, the by-type
/// lifecycle handlers, the death doors), the boot session and the
/// boot's data are not. Chatty participants that spoke before the
/// attach — any node may, from its handshake onward — had their
/// lifecycle commands parked; [`SessionHostMount::attach`] serves
/// them in arrival order, behind the boot's announcements.
pub struct SessionHostMount {
    wiring: SessionHostWiring,
    events: mpsc::UnboundedReceiver<EventFrame>,
    workers: Arc<Mutex<HashMap<String, Arc<Worker>>>>,
    joins: Arc<Mutex<Vec<JoinHandle<()>>>>,
    closing_stats: Arc<Mutex<HashMap<String, SessionStats>>>,
    worker_shutdown: CancellationToken,
    stream_end: CancellationToken,
    sink: HostSink,
    host_channel: Channel,
    door: Arc<Lifecycle>,
}

impl SessionHost {
    /// Mount the host's STRUCTURE — callable (and called) before any
    /// data exists and before any child process boots: the frontend
    /// stream, the host's channel and sink, the worker tables, the
    /// death doors, the wind-down, and the by-type lifecycle handlers
    /// on the node's command table. From this moment the net accepts
    /// every participant's speech: session-addressed commands route
    /// by the learning table (a miss is the uniform error — sessions
    /// do not exist yet, and that is a routed outcome, not a drop),
    /// and lifecycle commands park until [`SessionHostMount::attach`]
    /// arms their builders. Must run inside a tokio runtime (the
    /// watchers and the wind-down spawn here).
    pub fn mount(wiring: SessionHostWiring, frontend: FrontendStream) -> SessionHostMount {
        let node = wiring.node.clone();
        let event_rx = frontend.events;
        let frontend_events_tx = frontend.events_tx;
        let frontend_gone = frontend.gone;

        let workers: Arc<Mutex<HashMap<String, Arc<Worker>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let closing_stats: Arc<Mutex<HashMap<String, SessionStats>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let worker_shutdown = CancellationToken::new();
        let stream_end = CancellationToken::new();
        let joins: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));

        let death: Arc<dyn Fn() + Send + Sync> = {
            let workers = workers.clone();
            let worker_shutdown = worker_shutdown.clone();
            let fired = std::sync::atomic::AtomicBool::new(false);
            Arc::new(move || {
                if !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    for worker in lock(&workers).values() {
                        worker.abort();
                    }
                    worker_shutdown.cancel();
                }
            })
        };
        // The frontend-death watchers (the receiver's drop IS the
        // frontend's death, whatever the reason — detected directly
        // via the sender's `closed`, not on the next failed emission:
        // a worker parked on a card emits nothing, and must still
        // wind down). The door the drop opens aborts every in-flight
        // run and pulls the wind-down token.
        {
            let watch_tx = frontend_events_tx.clone();
            let gone = frontend_gone.clone();
            tokio::spawn(async move {
                watch_tx.closed().await;
                gone.cancel();
            });
        }
        {
            let death = death.clone();
            let gone = frontend_gone.clone();
            tokio::spawn(async move {
                gone.cancelled().await;
                death();
            });
        }

        // The host's own channel: backend-level emissions (catalog,
        // lifecycle errors) and the link's way in.
        let host_channel = Channel::local("host", |_| {}, |_| {});
        let sink = HostSink::new(&node, &host_channel);

        // The lifecycle door — the by-type handlers, live from here.
        // Until attach arms the builders, arrivals park (the drain is
        // the attach's last act).
        let door = Arc::new(Lifecycle {
            node: node.clone(),
            sink: sink.clone(),
            workers: workers.clone(),
            joins: joins.clone(),
            stats: closing_stats.clone(),
            worker_shutdown: worker_shutdown.clone(),
            door: Mutex::new(DoorState {
                armed: None,
                parked: Vec::new(),
            }),
        });
        {
            let created = door.clone();
            node.handle("new_session", move |command: &SessionCommand| {
                if matches!(command, SessionCommand::NewSession) {
                    created.new_session();
                }
            });
            let opened = door.clone();
            node.handle("open_session", move |command: &SessionCommand| {
                if let SessionCommand::OpenSession { id } = command {
                    opened.open_session(id);
                }
            });
        }

        // The wind-down task: once the shutdown token is pulled, await
        // every worker join (each captures its last event) and then
        // end the stream.
        {
            let worker_shutdown = worker_shutdown.clone();
            let joins = joins.clone();
            let stream_end = stream_end.clone();
            tokio::spawn(async move {
                worker_shutdown.cancelled().await;
                loop {
                    let join = {
                        let mut held = lock(&joins);
                        if held.is_empty() {
                            break;
                        }
                        held.remove(0)
                    };
                    let _ = join.await;
                }
                stream_end.cancel();
            });
        }

        SessionHostMount {
            wiring,
            events: event_rx,
            workers,
            joins,
            closing_stats,
            worker_shutdown,
            stream_end,
            sink,
            host_channel,
            door,
        }
    }

    /// The fused boot for callers with everything at hand (print
    /// mode, tests): mount the structure, attach the boot at once.
    pub fn spawn(
        boot: Session,
        startup_notes: Vec<String>,
        wiring: SessionHostWiring,
        data: SessionHostData,
    ) -> Self {
        let frontend = mount_frontend(&wiring.node);
        Self::spawn_with_frontend(boot, startup_notes, wiring, data, frontend)
    }

    /// [`SessionHost::spawn`] over a pre-mounted frontend stream.
    pub fn spawn_with_frontend(
        boot: Session,
        startup_notes: Vec<String>,
        wiring: SessionHostWiring,
        data: SessionHostData,
        frontend: FrontendStream,
    ) -> Self {
        Self::mount(wiring, frontend).attach(boot, startup_notes, data)
    }
}

impl SessionHostMount {
    /// Attach the boot: spawn the boot worker, emit the pinned
    /// startup announcements (the session, its notes, the catalogs),
    /// arm the lifecycle door's data, and serve whatever parked
    /// during the gathering — in arrival order, behind the
    /// announcements.
    pub fn attach(
        self,
        boot: Session,
        startup_notes: Vec<String>,
        data: SessionHostData,
    ) -> SessionHost {
        let SessionHostMount {
            wiring,
            events: event_rx,
            workers,
            joins,
            closing_stats,
            worker_shutdown,
            stream_end,
            sink,
            host_channel,
            door,
        } = self;
        let info = SessionInfo {
            session_id: boot.id().to_string(),
            session_path: boot.wire_path(),
            session_cwd: boot.cwd().display().to_string(),
            model: boot.selection(),
            resumed: boot.resumed(),
        };
        let boot_id = info.session_id.clone();
        let boot_stream = StreamId::new(boot_id.clone());
        let node = wiring.node.clone();

        // The boot worker first: the startup announcements emit from
        // its channel, which is what teaches the learning table where
        // the boot session lives (the worker itself emits nothing at
        // spawn — it waits).
        let boot_skills = boot.skills_available();
        let (boot_worker, boot_channel, boot_join) =
            spawn_worker(boot, &node, worker_shutdown.clone(), closing_stats.clone());
        lock(&workers).insert(boot_id.clone(), boot_worker.clone());
        lock(&joins).push(boot_join);
        let boot_sink = NoticeSink::new(&node, &boot_channel, boot_stream.clone());

        // The host's synchronous startup emissions, ordered ahead of
        // any worker frame by construction (emitted here, before any
        // command can have reached a worker): the boot session's
        // "became visible" announcement (the same shape every other
        // session gets — the boot is not a special case), then its
        // selection degradations, then the catalog. A listing failure
        // is the carrier in place of the announcement — no catalog
        // follows (ruled: external errors ride the channel; PROTOCOL.md
        // v3).
        boot_sink.emit(SessionEvent::SessionOpened {
            id: info.session_id.clone(),
            path: info.session_path.clone(),
            cwd: info.session_cwd.clone(),
            model: info.model.clone(),
            resumed: info.resumed,
            parent: wiring.boot_parent.clone(),
            parent_call: wiring.boot_parent_call.clone(),
        });
        for note in startup_notes {
            boot_sink.emit(SessionEvent::error_model(note));
        }
        // The boot session's skills — session-level (owner ruling
        // 2026-09, landed): stamped with the session's stream, one
        // catalog per session build. Only when discovery found
        // something — with per-stream folding, absence is
        // unambiguous.
        if !boot_skills.is_empty() {
            boot_sink.emit(SessionEvent::SkillsAvailable {
                skills: boot_skills,
            });
        }
        match wiring.store.list() {
            Ok(summaries) => {
                // Backend-level: no session produced this (the optional-
                // stream ruling). Every catalog row is file-backed (a
                // session with no file is not in the store), so the
                // project cwd derives from the store root once:
                // `<cwd>/.tabit/sessions` -> the `<cwd>` above it.
                let project_cwd = wiring
                    .store
                    .dir()
                    .parent()
                    .and_then(Path::parent)
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_default();
                sink.emit(
                    None,
                    SessionEvent::SessionsAvailable {
                        sessions: summaries
                            .into_iter()
                            .map(|summary| AvailableSession {
                                id: summary.id,
                                created_at: summary.created_at,
                                entry_count: summary.entry_count as u64,
                                path: summary.path.display().to_string(),
                                cwd: project_cwd.clone(),
                            })
                            .collect(),
                    },
                );
            }
            Err(error) => {
                sink.emit(
                    None,
                    SessionEvent::error_session(format!("could not list sessions: {error}")),
                );
            }
        }
        // The extension catalog rides right after the session catalog —
        // same backend-level reasons (one process, one extension
        // host), and the conflict reports are load-time facts: they
        // belong to the boot that produced them.
        if !data.extensions.extensions.is_empty() {
            sink.emit(
                None,
                SessionEvent::ExtensionsAvailable {
                    extensions: data.extensions.extensions.clone(),
                    conflicts: data.extensions.conflicts.clone(),
                },
            );
        }

        // A resumed boot replays automatically (owner ruling
        // 2026-09-25): the resident chain re-emits right after the
        // announcements — a `--continue`/`--session` connect needs no
        // request. A fresh boot (or an absorbed `--continue` miss)
        // has nothing to replay. On request, the door's idempotent
        // path serves: `open_session` of an already-open session
        // re-replays it, any time.
        if info.resumed {
            boot_worker.deliver_replay();
        }

        // The lifecycle door arms (its handlers have been live since
        // the mount) and whatever parked during the gathering serves
        // now, in arrival order, behind the announcements.
        door.arm(data);

        SessionHost {
            info,
            events: Some(event_rx),
            node,
            host_channel,
            workers,
            closing_stats,
            worker_shutdown,
            stream_end,
        }
    }
}

impl SessionHost {
    /// The boot session's facts, captured when the host took over.
    pub fn info(&self) -> &SessionInfo {
        &self.info
    }

    /// Submit a user message to a session: steers the run in flight or
    /// starts one.
    pub fn message(&self, session: &str, text: impl Into<String>) {
        self.command_link().send(SessionCommand::Message {
            session: session.to_string(),
            text: text.into(),
        });
    }

    /// Stop a session: abort the run in flight and discard any queued
    /// messages. Aborting while idle is a no-op (including on anything
    /// queued — the queue is discarded with it).
    pub fn abort(&self, session: &str) {
        self.command_link().send(SessionCommand::Abort {
            session: session.to_string(),
        });
    }

    /// Start a run over the session's existing conversation with no
    /// new message (retry / continue). A no-op on an empty
    /// conversation.
    pub fn continue_run(&self, session: &str) {
        self.command_link().send(SessionCommand::Continue {
            session: session.to_string(),
        });
    }

    /// Move a session's active chain to an entry (checkout — any entry
    /// in the file; an off-chain target is a branch switch). Executed
    /// at the session's pause point: immediately when idle, after the
    /// in-flight run's terminal otherwise (never an implicit abort).
    /// Outcomes arrive as events (`checked_out` + a replay pass, or
    /// `error { kind: checkout }`).
    pub fn checkout(&self, session: &str, entry_id: impl Into<String>) {
        self.command_link().send(SessionCommand::Checkout {
            session: session.to_string(),
            entry_id: entry_id.into(),
        });
    }

    /// Switch a session's model — the register write, immediate: the
    /// entry and the live selection land at receive (one shared-write
    /// operation, any thread), and `model_changed` follows at once. A
    /// ref config cannot resolve is an `error { kind: model }`,
    /// nothing moves. A run in flight is untouched (passes bind the
    /// agent at run open); the next run derives the new agent. Abort
    /// is irrelevant — a state write is not conversation intent.
    pub fn model(&self, session: &str, selection: ModelSelection) {
        self.command_link().send(SessionCommand::Model {
            session: session.to_string(),
            provider: selection.provider,
            model: selection.model,
            thinking_level: selection.thinking_level,
        });
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
    /// `replay_begin`/`replay_end`. Fire-and-forget like a command
    /// — the pass itself is the acknowledgment. Answered at the
    /// session's next idle beat; requests during a run wait for it.
    pub fn replay(&self, session: &str) {
        if let Some(worker) = lock(&self.workers).get(session).cloned() {
            worker.deliver_replay();
        }
    }

    /// A cloneable submitter for threads that only send commands.
    pub fn command_link(&self) -> SessionCommandLink {
        SessionCommandLink {
            node: self.node.clone(),
            host_channel: self.host_channel.clone(),
        }
    }

    /// Close the host's command side (the polite door — in-process
    /// consumers that stay to read the stream, like print mode). Every
    /// worker finishes any in-flight run, then — delivery is
    /// synchronous, so everything already submitted has landed —
    /// closing stats are captured, and the event stream ends.
    /// Submissions that race the wind-down find workers already
    /// winding down: silent no-ops, the same few-instruction window
    /// the queue once had (the stdio edge never uses this door — it
    /// drops the host, the death door, so nothing unattended runs).
    pub fn close_commands(&mut self) {
        self.worker_shutdown.cancel();
    }

    /// Take the whole event stream for a long-lived consumer (a
    /// transport forwarder). Once taken, [`SessionHost::next_event`]
    /// yields `None` — one stream, one consumer.
    pub fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<EventFrame>> {
        self.events.take()
    }

    /// The next stamped event, or `None` once the host has wound down
    /// (or the stream was taken). The stream ends only after every
    /// worker's last event has landed (the wind-down awaits the joins
    /// before ending it).
    pub async fn next_event(&mut self) -> Option<EventFrame> {
        let events = self.events.as_mut()?;
        tokio::select! {
            frame = events.recv() => frame,
            _ = self.stream_end.cancelled() => {
                // Every worker has wound down; hand over what landed,
                // then the end.
                events.try_recv().ok()
            }
        }
    }

    /// The stream's end signal — the transport forwarder's way to
    /// stop when the host winds down (fired after every worker's last
    /// event has landed).
    pub(crate) fn stream_end_signal(&self) -> CancellationToken {
        self.stream_end.clone()
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

/// The host's lifecycle door (by-type on the node's handler table,
/// live from the MOUNT — the prepared-supervisor law: any node may
/// speak from its handshake onward, so the command surface exists
/// before the first child boots). `new_session` builds through the
/// data, `open_session` loads or re-replays, and every spawned
/// worker's channel is registered into the learning table by its own
/// announcement (the emit teaches). The builders are the boot's DATA
/// — they arrive at attach; an arrival before that parks, and is
/// served in arrival order once armed.
struct Lifecycle {
    node: Arc<Node>,
    sink: HostSink,
    workers: Arc<Mutex<HashMap<String, Arc<Worker>>>>,
    joins: Arc<Mutex<Vec<JoinHandle<()>>>>,
    stats: Arc<Mutex<HashMap<String, SessionStats>>>,
    worker_shutdown: CancellationToken,
    /// The door's one state: the armed builders (once the boot's
    /// gathering is done) and the commands that arrived before them,
    /// under ONE lock — the park decision and the arm-and-take are
    /// each a single atomic act, so a command that races the attach
    /// either parks into the set arm takes or serves through the
    /// builders arm holds; nothing is lost between them.
    door: Mutex<DoorState>,
}

struct DoorState {
    armed: Option<LifecycleCore>,
    parked: Vec<ParkedLifecycle>,
}

/// The lifecycle door's data half: what only exists after the boot's
/// gathering resolved.
#[derive(Clone)]
struct LifecycleCore {
    create: SessionSource,
    open: OpenSessionSource,
}

/// One lifecycle command that arrived before the data did.
enum ParkedLifecycle {
    NewSession,
    OpenSession { id: String },
}

impl Lifecycle {
    /// Arm the door (the attach act): the builders exist, and
    /// whatever parked during the gathering serves now, in arrival
    /// order, behind the boot's announcements. Arming and taking the
    /// parked set is ONE lock claim — a concurrent arrival either
    /// parks into the set being taken or serves through the builders
    /// being armed; there is no window between them.
    fn arm(&self, data: SessionHostData) {
        let parked = {
            let mut door = lock(&self.door);
            door.armed = Some(LifecycleCore {
                create: data.create,
                open: data.open,
            });
            std::mem::take(&mut door.parked)
        };
        let Some(core) = self.armed() else {
            return;
        };
        self.drain(parked, core);
    }

    fn armed(&self) -> Option<LifecycleCore> {
        lock(&self.door).armed.clone()
    }

    fn drain(&self, parked: Vec<ParkedLifecycle>, core: LifecycleCore) {
        for parked in parked {
            // The drain serves with the armed core in hand — the
            // unarmed case is unrepresentable (the core is a
            // parameter), so no silent drop exists anywhere.
            let core = core.clone();
            match parked {
                ParkedLifecycle::NewSession => self.serve_new_session(core),
                ParkedLifecycle::OpenSession { id } => self.serve_open_session(core, &id),
            }
        }
    }

    fn new_session(&self) {
        if let Some(core) = self.enter(ParkedLifecycle::NewSession) {
            self.serve_new_session(core);
        }
    }

    fn open_session(&self, id: &str) {
        if let Some(core) = self.enter(ParkedLifecycle::OpenSession { id: id.to_string() }) {
            self.serve_open_session(core, id);
        }
    }

    /// One arrival through the door: park it (the builders are not
    /// gathered yet) or hand it the armed builders — one lock claim
    /// decides which.
    fn enter(&self, parked: ParkedLifecycle) -> Option<LifecycleCore> {
        let mut door = lock(&self.door);
        match door.armed.clone() {
            Some(core) => Some(core),
            None => {
                door.parked.push(parked);
                None
            }
        }
    }

    /// `new_session`: announce, then spawn. The creation frame and its
    /// notes land ahead of anything the worker can emit (emitted
    /// here, before any command can have reached it).
    fn serve_new_session(&self, core: LifecycleCore) {
        let (session, notes) = match (core.create)() {
            Ok(built) => built,
            Err(message) => {
                self.sink.emit(
                    None,
                    SessionEvent::error_session(format!(
                        "could not build a new session: {message}"
                    )),
                );
                return;
            }
        };
        let id = session.id().to_string();
        let stream = StreamId::new(id.clone());
        let (path, cwd, model, resumed) = (
            session.wire_path(),
            session.cwd().display().to_string(),
            session.selection(),
            session.resumed(),
        );
        let skills = session.skills_available();
        let (worker, channel, join) = spawn_worker(
            session,
            &self.node,
            self.worker_shutdown.clone(),
            self.stats.clone(),
        );
        // One announcement shape for every path (v10): the stamped
        // `session_opened` carries `resumed: false` for a fresh
        // session — the selection rides the frame because nothing
        // else on the wire will say so (the session is empty; no
        // `model_changed` replays). Selection notes follow on the
        // same stream, the same order `open_session` uses. The
        // emission from the session's channel is what teaches the
        // learning table its route.
        let opened = NoticeSink::new(&self.node, &channel, stream.clone());
        opened.emit(SessionEvent::SessionOpened {
            id: id.clone(),
            path,
            cwd,
            model,
            resumed,
            parent: None,
            parent_call: None,
        });
        for note in notes {
            opened.emit(SessionEvent::error_model(note));
        }
        // The session's skills, stamped with its stream (the
        // session-level catalog ruling) — every session becoming
        // visible announces its own catalog.
        if !skills.is_empty() {
            opened.emit(SessionEvent::SkillsAvailable { skills });
        }
        lock(&self.workers).insert(id, worker);
        lock(&self.joins).push(join);
    }

    /// `open_session`: already open means re-replay (idempotent);
    /// otherwise load, surface the notes, spawn, and answer with the
    /// pass — the pass itself is the acknowledgment.
    fn serve_open_session(&self, core: LifecycleCore, id: &str) {
        if let Some(worker) = lock(&self.workers).get(id).cloned() {
            worker.deliver_replay();
            return;
        }
        let (session, notes) = match (core.open)(id) {
            Ok(loaded) => loaded,
            Err(message) => {
                self.sink.emit(
                    None,
                    SessionEvent::error_session(format!(
                        "could not open session `{id}`: {message}"
                    )),
                );
                return;
            }
        };
        let stream = StreamId::new(id.to_string());
        let (path, cwd, model, resumed) = (
            session.wire_path(),
            session.cwd().display().to_string(),
            session.selection(),
            session.resumed(),
        );
        let skills = session.skills_available();
        let (worker, channel, join) = spawn_worker(
            session,
            &self.node,
            self.worker_shutdown.clone(),
            self.stats.clone(),
        );
        let opened = NoticeSink::new(&self.node, &channel, stream);
        opened.emit(SessionEvent::SessionOpened {
            id: id.to_string(),
            path,
            cwd,
            model,
            resumed,
            parent: None,
            parent_call: None,
        });
        for note in notes {
            opened.emit(SessionEvent::error_model(note));
        }
        // The resumed session's skills, stamped with its stream
        // — a session opened from another directory announces ITS
        // catalog (the reason the catalog is session-level).
        if !skills.is_empty() {
            opened.emit(SessionEvent::SkillsAvailable { skills });
        }
        lock(&self.workers).insert(id.to_string(), worker.clone());
        lock(&self.joins).push(join);
        worker.deliver_replay();
    }
}

/// Spawn one session's resident worker: the classic loop — ownership
/// never moves (idle is the wait below, running is the pump call),
/// with the session's id as its stream stamp. Returns the routing
/// leaves (the handler surface), the session's channel (the
/// learning-table entry its emissions teach), and the task handle.
fn spawn_worker(
    mut session: Session,
    node: &Arc<Node>,
    shutdown: CancellationToken,
    stats: Arc<Mutex<HashMap<String, SessionStats>>>,
) -> (Arc<Worker>, Channel, JoinHandle<()>) {
    let id = session.id().to_string();
    let stream = StreamId::new(id.clone());
    let mailbox = session.mailbox_handle();
    let abort_handle = session.abort_handle();
    let interaction = InteractionHub::new(node.clone(), stream.clone());
    let checkout_slot = Arc::new(Mutex::new(None::<String>));
    let replay_due = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let compact_slot = Arc::new(Mutex::new(None::<Option<String>>));
    let entry_probe = session.entry_id_probe();
    let model_probe = session.model_probe();
    let model_register = session.model_register();
    // The worker's emission sink attaches after the channel exists (a
    // sink is the channel it emits from).
    let notices: Arc<NoticeSlot> = Arc::new(std::sync::OnceLock::new());
    let worker = Arc::new(Worker {
        mailbox: mailbox.clone(),
        abort_handle,
        notices: notices.clone(),
        entry_probe,
        checkout_slot: checkout_slot.clone(),
        model_register,
        model_probe,
        replay_due: replay_due.clone(),
        compact_slot: compact_slot.clone(),
    });
    // The session's channel: session-addressed commands arrive here
    // (the learning table's entry — taught by the very emissions this
    // channel makes), and `deliver` is what runs when they do.
    let delivering = worker.clone();
    let channel = Channel::local(
        &id,
        |_| {},
        move |command: &SessionCommand| {
            delivering.deliver(command.clone());
        },
    );
    let sink = NoticeSink::new(node, &channel, stream.clone());
    let _ = notices.set(sink.clone());

    let worker_slot = checkout_slot;
    let worker_compact_slot = compact_slot;
    let worker_replay_due = replay_due;
    let worker_mailbox = mailbox;
    let stats_id = id;
    // The attaches run at spawn, before the task: the sink exists (it
    // is the channel above), and delivery is synchronous — a command
    // can arrive (and want to emit) before the task has ever been
    // polled. Attach-once, deterministic, no scheduler race.
    session.attach_interaction(interaction);
    session.attach_mailbox_notices(sink.clone());
    session.attach_persist_notices(sink.clone());
    session.attach_event_tap(sink.clone());
    let join = tokio::spawn(async move {
        // The resident worker. Ownership never moves: idle is the wait
        // below, running is the pump call - two positions of one loop,
        // not two tasks. One wake (the work signal) serves every
        // pending thing; the beat at the loop top is the single drain
        // point.
        loop {
            // The beat: the parked intent answers ahead of the pump
            // arm - the order's one home is `serve_parked`. Reads and
            // rewinds requested ahead of a message answer ahead of it;
            // a message's inclusion in a pass is decided solely by
            // whether it drained before the beat. (The model register
            // needs no beat arm: its writes land at receive, and the
            // passes announce it live.)
            serve_parked(
                &mut session,
                &sink,
                &worker_replay_due,
                &worker_slot,
                &worker_compact_slot,
            )
            .await;
            if worker_mailbox.has_queued() || worker_mailbox.has_continue() {
                // The pump returns on an aborted outcome (a checkout
                // aborts its way here), so anything parked behind a
                // run executes at this beat before a later message
                // starts the next batch on the old chain.
                session
                    .pump(&mut |event| {
                        // The receiver is gone only when the
                        // frontend is; there is no one left to
                        // tell.
                        sink.emit(event);
                    })
                    .await;
                continue;
            }
            // The idle compaction door: with the mailbox empty, the
            // beat evaluates A ∨ B and runs the box when it fires. A
            // queued message arriving mid-compaction waits for it
            // (always-queue) and runs on the compacted context.
            session.compact_idle().await;
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    // Close is not a barrier: delivery is
                    // synchronous, so anything submitted before the
                    // close is already queued - run it before winding
                    // down. (Submissions that race the wind-down
                    // simply run too; nothing is lost.)
                    if worker_mailbox.has_queued() {
                        continue;
                    }
                    // Serve what the handler parked ahead of the
                    // close (the order's one home: `serve_parked`),
                    // then wind down. (Register writes are already
                    // durable — receive wrote them.) The death door
                    // cleared any parked intent before pulling this
                    // token (abort first, wind down second), so what
                    // runs here is the polite close's residue only.
                    serve_parked(
                        &mut session,
                        &sink,
                        &worker_replay_due,
                        &worker_slot,
                        &worker_compact_slot,
                    )
                    .await;
                    // The clean-exit flush attempt (flag 8): one more
                    // drain before the stream ends.
                    session.flush_log();
                    break;
                }
                // The one wake: any pending thing (a message push, a
                // parked checkout or pass) lands here and loops back
                // to the beat.
                _ = worker_mailbox.work_signal().notified() => {}
            }
        }
        lock(&stats).insert(stats_id, session.stats());
    });
    (worker, channel, join)
}

/// Serve the parked conversation intent in the ruled order: a parked
/// pass answers first (a read of the chain as it stands), then a
/// parked checkout (the rewind - the one session mutation - plus its
/// re-render), then a parked manual compaction (the forced door, ahead
/// of any queued batch). The order's one home - the worker's beat and
/// the shutdown arm both call here, so a new parked intent joins this
/// list exactly once.
async fn serve_parked(
    session: &mut Session,
    sink: &NoticeSink,
    replay_due: &std::sync::atomic::AtomicBool,
    checkout_slot: &Mutex<Option<String>>,
    compact_slot: &Mutex<Option<Option<String>>>,
) {
    if replay_due.swap(false, std::sync::atomic::Ordering::Acquire) {
        emit_replay(session, sink);
    }
    if let Some(entry_id) = lock(checkout_slot).take() {
        execute_checkout(session, sink, entry_id);
    }
    // The guard drops before the await (the lock contract — no
    // guard across an await).
    let parked_compact = lock(compact_slot).take();
    if let Some(directives) = parked_compact {
        session.compact_manual(directives).await;
    }
}

/// Execute the parked checkout at a pause point: rewind the chain,
/// announce, re-render. The discard already happened at receive (the
/// handler's clear); an execution-time failure - the rewind cannot
/// apply - is the command's error event and a no-op (verification
/// caught the common failure at receive; these are the environmental
/// ones: persist trouble, the chain's model gone from config).
fn execute_checkout(session: &mut Session, sink: &NoticeSink, entry_id: String) {
    let res = session.rewind_to_entry(&entry_id);
    if let Err(error) = res {
        sink.emit(SessionEvent::error_checkout(error.to_string()));
        return;
    }
    sink.emit(SessionEvent::CheckedOut {
        entry_id,
        // Full re-render (the suffix mode's reserved seam).
        base_id: None,
    });
    emit_replay(session, sink);
}

/// The replay pass (PROTOCOL.md v2): the resident chain projected
/// into finalized live events, bracketed. One emission path for its
/// askers — the transport's replay request, checkout's re-render, and
/// the open_session boot pass — each led by the register announcement
/// ([`SessionEvent::model_changed`], shared with the applied model
/// switch): a session becoming visible (boot, open, re-replay,
/// checkout) always tells the frontend its active selection. Idempotent
/// by construction — a pass never moves the register, so the value
/// repeats; replayed history itself never carries `model_changed` (the
/// register ruling: state is announced live, not reconstructed).
fn emit_replay(session: &Session, sink: &NoticeSink) {
    let selection = session.selection();
    sink.emit(SessionEvent::model_changed(
        &selection,
        session.model_facts(&selection),
    ));
    let events = session.replay_events();
    let total = events.len() as u64;
    sink.emit(SessionEvent::ReplayBegin { total });
    for event in events {
        sink.emit(event);
    }
    sink.emit(SessionEvent::ReplayEnd);
}

#[cfg(test)]
#[path = "endpoint_tests.rs"]
mod tests;
