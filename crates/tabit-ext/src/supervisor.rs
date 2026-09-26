//! The extension supervisor: launch every installed package,
//! take each one's report, watch for death. One supervisor per
//! backend process; the binary owns it (extensions are backend
//! machinery —
//! their contributions reach sessions through the binary's assembly,
//! never through tabit-session).
//!
//! Death policy (ruled): mark dead, report, never respawn mid-run —
//! the mounted contributions stay by construction (proxy tools answer
//! "not running", the skills tables and provider fragments are
//! scan-driven and survive a death). The report surface is the
//! [`Supervisor::reports`] snapshot, the [`ExtensionEvent`] channel
//! the binary logs, and the `extensions_available` wire catalog.
//!
//! The call path is a **router pair per extension** (owner ruling
//! 2026-09): each proxy (the tool adapter) enqueues its request on
//! its extension's outbound lane — a writer task serializes frames
//! onto stdin, and sending is cheap (an unbounded enqueue) — and
//! awaits its result from the inbound side, where the reader task
//! forwards each `tool_result` to the waiting call by `call_id`.
//! Execution is parallel: the extension side may run calls
//! concurrently and return them in any order, which is exactly what
//! the id-tagged forwarding allows. A death answers every waiting
//! adapter with the failure (the drain) — no proxy call ever hangs
//! on a dead extension — and interaction requests route to the
//! waiting call's session through the same forwarding table.
//!
//! The process architecture is the subagent bridge's
//! (`subprocess.rs`), minus the route-all router and the drive loop:
//! a reader task parses stdout (handshake first, tool results and
//! ask lifts after), and the pipe's mechanics — the command writer,
//! the grace reaper, the immediate kill, the crash tail — live in
//! [`tabit_wire::process`], shared with every spawning site. Local to here: the
//! typed-frame reader and the lane machinery; a pre-report
//! failure kills the tree immediately (nothing was proven), a
//! post-report death gets the grace-bounded reclaim.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::manifest::{self, Discovered, Manifest};
use crate::protocol::{
    EXTENSION_PROTOCOL_VERSION, ExtFrame, HookDecl, HookResult, HostFrame, KIND_HOOK_RESULT,
    KIND_SERVICE_RESPONSE, KIND_TOOL_RESULT, Report, ServiceVerb, ToolDecl, ToolWireResult,
};
use rig_agent::tool::services::{HostServices, ModelPromptOk, ModelPromptRequest, ServiceUsage};
use std::io::Write;
use tabit_wire::node::{AnswerOutcome, Channel, Locality, Node, parse_shared, violation_panic};
use tabit_wire::process::ChildWrapper;
use tabit_wire::process::{self, wrap_command};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

/// The default handshake window — one value and one reason for every
/// spawned pipe (extensions here, subagent children in the bridge):
/// slow runtimes (a Python import, a cold Node) need the room;
/// healthy children answer in milliseconds. Lives in
/// [`tabit_wire::process`] with the rest of the shared pipe plumbing;
/// re-exported here for the existing call sites.
pub use tabit_wire::process::BOOT_TIMEOUT;
use tabit_wire::process::{REAP_GRACE, crash_tail, reap_with_grace, spawn_line_writer};

/// One extension's standing, as the host sees it.
#[derive(Debug, Clone)]
pub enum Status {
    /// Spawned, handshaking still in flight.
    Starting,
    /// Reported and validated; capabilities are its declared set
    /// for the host's life.
    Alive,
    /// Out of the game — refused at the scan, refused at the
    /// handshake, or dead since. The reason is the whole report.
    Dead { reason: String },
}

/// A state transition worth reporting: one per firing transition,
/// never a duplicate (the transition door below is race-deduped).
#[derive(Debug, Clone)]
pub struct ExtensionEvent {
    pub name: String,
    pub status: Status,
}

/// The snapshot view — [`Supervisor::reports`].
#[derive(Debug)]
pub struct ExtensionReport {
    pub name: String,
    pub dir: PathBuf,
    pub version: String,
    pub description: Option<String>,
    pub status: Status,
    pub tools: Vec<ToolDecl>,
    pub hooks: Vec<HookDecl>,
}

/// The live state one supervision task mutates through the
/// transition door; [`Supervisor::reports`] snapshots it under the
/// same claim, and [`Supervisor::await_resolved`] waits on the
/// notify for the first transition (the boot ordering).
#[derive(Debug, Default)]
struct ChildState {
    inner: Mutex<ChildFields>,
    resolved: tokio::sync::Notify,
}

#[derive(Debug, Default)]
struct ChildFields {
    status: Option<Status>,
    tools: Vec<ToolDecl>,
    hooks: Vec<HookDecl>,
}

impl ChildState {
    /// Record one lifecycle edge: Starting → Alive, Starting → Dead,
    /// or Alive → Dead (death after life still reports — the edge a
    /// once-ever door would have swallowed). The supervision task is
    /// this state's only writer (verdicts and deaths reach it through
    /// channels), so an invalid edge is a broken invariant, not a
    /// race to arbitrate: it crashes loudly, never gets silently
    /// swallowed. The mutex is for snapshot readers
    /// ([`Supervisor::reports`]); the notify wakes the boot join.
    /// Returns the standing now recorded.
    #[allow(clippy::panic)] // the sanctioned crash below (AGENTS.md doctrine)
    fn transition(&self, to: Status, tools: Vec<ToolDecl>, hooks: Vec<HookDecl>) -> Status {
        let mut fields = tabit_log::lock::lock(&self.inner);
        let from = fields.status.clone().unwrap_or(Status::Starting);
        let valid = matches!(
            (&from, &to),
            (Status::Starting, Status::Alive | Status::Dead { .. })
                | (Status::Alive, Status::Dead { .. })
        );
        if !valid {
            panic!(
                "internal invariant violated: invalid extension lifecycle edge {from:?} -> {to:?}"
            );
        }
        fields.status = Some(to.clone());
        fields.tools = tools;
        fields.hooks = hooks;
        drop(fields);
        self.resolved.notify_waiters();
        to
    }

    /// Wait for the first transition (the handshake verdict, one way
    /// or the other). The notify future is registered before the
    /// state check so a transition between the two cannot be missed.
    async fn wait_resolved(&self) {
        loop {
            let notified = self.resolved.notified();
            if tabit_log::lock::lock(&self.inner).status.is_some() {
                return;
            }
            notified.await;
        }
    }
}

// The correlation-kind tags the lane's forwarded items hold (the
// correlation-kind law, read back at the claim: a tool result
// answering a hook id, or the reverse, is a contract break) are
// declared once in the protocol beside the frames they name —
// `protocol::{KIND_TOOL_RESULT, KIND_HOOK_RESULT, KIND_SERVICE_RESPONSE}`.

/// The host-side half of one extension's pipe after the spawn: the
/// pipe's one writer (the commands channel everything serializes
/// through), the lane's **node face** — the [`Channel`] whose
/// deliveries are the node's whole vocabulary for this pipe (watched
/// event lines in via its subscriptions, grammar-ask answers and
/// learning-routed commands out) — and the envelope's call contexts.
struct Lane {
    name: String,
    commands: tokio::sync::mpsc::UnboundedSender<String>,
    /// The node-facing face of this pipe. Held asks deliver answers
    /// through it; the watch list subscribes it; intake from it is
    /// the reader's door for the shared grammar.
    channel: Channel,
    /// The open items' session capabilities — the envelope's
    /// correlation table for mid-call service requests (a
    /// `model_prompt` routes through the asking session). Kept in
    /// step with the ask table at every departure site.
    contexts: Mutex<HashMap<String, Arc<dyn HostServices>>>,
    /// Set at the pipe's end (EOF or garbage), before the node sweep
    /// — a call registering after death fails fast instead of
    /// awaiting a result that can never come.
    dead: AtomicBool,
}

impl Lane {
    fn new(name: String, commands: tokio::sync::mpsc::UnboundedSender<String>) -> Arc<Self> {
        // The channel's deliveries ride the same writer as every
        // directed frame: one pipe, one serialization point.
        let writer = commands.clone();
        let channel = Channel::line(&name, move |line: &str| {
            let _ = writer.send(line.to_string());
        });
        Arc::new(Self {
            name,
            commands,
            channel,
            contexts: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
        })
    }

    /// The pipe's last act: sweep the lane's everything on the node —
    /// subscriptions, learned routes, and open round-trips (each
    /// delivery closure owning its failure arm: executions read the
    /// sweep as their transport failure, policy as its fail-open
    /// fallback, grammar asks settle announced) — then stay
    /// dead-flagged for late arrivals.
    fn die(&self, node: &Node, reason: &str) {
        self.dead.store(true, Ordering::SeqCst);
        tabit_log::lock::lock(&self.contexts).clear();
        node.retract(&self.name, reason);
    }

    /// Attach (or, with `None`, skip) an item's session capability.
    fn attach_context(&self, call_id: &str, services: Option<Arc<dyn HostServices>>) {
        if let Some(services) = services {
            tabit_log::lock::lock(&self.contexts).insert(call_id.to_string(), services);
        }
    }

    /// The item is over (answered, given up, or dead): its capability
    /// context goes with it.
    fn end_call(&self, call_id: &str) {
        tabit_log::lock::lock(&self.contexts).remove(call_id);
    }

    /// The open item's session capability, if any — the envelope's
    /// routing target for its correlated requests (fail closed
    /// without one).
    fn capability(&self, call_id: &str) -> Option<Arc<dyn HostServices>> {
        tabit_log::lock::lock(&self.contexts).get(call_id).cloned()
    }
}

/// The proxy surface for one extension — what the binary's tool
/// assembly holds and forwards calls through. Clonable: every proxy
/// tool for one extension shares the lane.
#[derive(Clone)]
pub struct ExtensionHandle {
    lane: Arc<Lane>,
    node: Arc<Node>,
}

impl ExtensionHandle {
    /// Call one tool on the extension and await its wire result.
    /// `services` is the calling session's host-service capability
    /// (the routing target for the extension's mid-call envelope
    /// requests — asks and model prompts); without one, asks answer
    /// dismissed and verbs error — fail closed, exactly as core
    /// tools behave on a non-interactive session. `cancel` is the
    /// run's token: firing it sends [`HostFrame::Cancel`] down the
    /// pipe (the guest's signal to stop — the token-and-detach
    /// contract, crossing the process boundary), discards the held
    /// entry (racing asks answer dismissed), and fails the call.
    pub async fn call(
        &self,
        tool: &str,
        args: serde_json::Value,
        services: Option<Arc<dyn HostServices>>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ToolWireResult, String> {
        // The ruled id mint (2026-09): a UUIDv7 — the id crosses
        // the pipe and registers on the guest's ask table, so
        // collision-freedom rests on construction, never on a
        // name grammar.
        let call_id = uuid::Uuid::now_v7().to_string();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let failure_id = call_id.clone();
        self.node.hold(
            &self.lane.name,
            &call_id,
            KIND_TOOL_RESULT,
            move |outcome| {
                let result = match outcome {
                    tabit_wire::asks::Outcome::Answered(answer) => {
                        tabit_wire::asks::unanswer::<ToolWireResult>(answer)
                    }
                    tabit_wire::asks::Outcome::Orphaned(reason) => ToolWireResult {
                        call_id: failure_id,
                        error: Some(reason),
                        report: String::new(),
                        details: None,
                    },
                };
                let _ = tx.send(result);
            },
        );
        self.lane.attach_context(&call_id, services);
        // The dead check rides after the hold and inside the same
        // ordering as `die`'s flag-then-sweep, so a death between
        // hold and check is caught either by the flag or by the
        // sweep itself.
        if self.lane.dead.load(Ordering::SeqCst) {
            self.abandon(&call_id);
            return Err(format!("extension `{}` is not running", self.lane.name));
        }
        let frame = serde_json::to_string(&HostFrame::ToolCall {
            call_id: call_id.clone(),
            name: tool.to_string(),
            args,
        })
        .map_err(|error| format!("cannot encode the tool call: {error}"))?;
        if self.lane.commands.send(frame).is_err() {
            self.abandon(&call_id);
            return Err(format!("extension `{}` is not running", self.lane.name));
        }
        let outcome = tokio::select! {
            result = rx => result.map_err(|_| {
                self.abandon(&call_id);
                format!("extension `{}` closed mid-call", self.lane.name)
            }),
            _ = cancel.cancelled() => {
                self.cancel(&call_id);
                return Err(format!("extension `{}` call was cancelled", self.lane.name));
            }
        };
        match outcome {
            Ok(result) => Ok(result),
            Err(error) => {
                self.abandon(&call_id);
                Err(error)
            }
        }
    }

    /// The caller's own give-up (a dead lane, a failed send, a
    /// cancellation): discard the question and its context without
    /// settling — the awaiter has moved on by its own path, and a
    /// racing answer finds a gone id, tolerated.
    fn abandon(&self, call_id: &str) {
        self.node.discard(call_id);
        self.lane.end_call(call_id);
    }

    /// The cancel bookkeeping: drop the pending entry (racing asks
    /// lift to nothing and answer dismissed; a racing result is an
    /// unknown id, tolerated) and tell the guest to stop. Shared by
    /// the call and hook lanes — the id is whichever correlation.
    fn cancel(&self, call_id: &str) {
        self.abandon(call_id);
        let frame = HostFrame::Cancel {
            call_id: call_id.to_string(),
        };
        #[allow(clippy::expect_used)] // sanctioned crash: pure-data serialization
        let frame = serde_json::to_string(&frame).expect("HostFrame always serializes");
        let _ = self.lane.commands.send(frame);
    }

    /// Forward one hook event to the extension and await its answer —
    /// the point's own type, parsed off the wire at the delivery
    /// (`P::Answer`: the pairing is a compile-time guarantee — a
    /// `tool_result` consult cannot answer a verdict). Same shape as
    /// [`call`]: the session's host-service capability rides along for
    /// mid-hook envelope requests, a dead lane is a transport error —
    /// the caller owns the policy mapping — while a death *during*
    /// the await and a cancellation resolve through the delivery
    /// closure with the point's declared neutral: **fail open**, the
    /// one home of the fallback.
    pub async fn hook<P: tabit_protocol::points::HookPoint>(
        &self,
        payload: serde_json::Value,
        services: Option<Arc<dyn HostServices>>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<P::Answer, String> {
        let hook_id = uuid::Uuid::now_v7().to_string();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.node.hold(
            &self.lane.name,
            &hook_id,
            KIND_HOOK_RESULT,
            move |outcome| {
                let answer = match outcome {
                    tabit_wire::asks::Outcome::Answered(answer) => {
                        let frame = tabit_wire::asks::unanswer::<HookResult>(answer);
                        match serde_json::from_value::<P::Answer>(frame.answer) {
                            Ok(answer) => Ok(answer),
                            // A malformed answer is a failed handler:
                            // the Err the caller folds to the neutral.
                            Err(error) => Err(format!("the hook answer does not parse: {error}")),
                        }
                    }
                    // A death resolves the point FAIL OPEN — its
                    // declared neutral (crash isolation: one dead
                    // package cannot brick the tool phase, while the
                    // death itself is reported loudly); a failed
                    // *execution* answers with its error. The
                    // asymmetry is the ruling.
                    tabit_wire::asks::Outcome::Orphaned(_) => Ok(P::neutral()),
                };
                let _ = tx.send(answer);
            },
        );
        self.lane.attach_context(&hook_id, services);
        if self.lane.dead.load(Ordering::SeqCst) {
            self.abandon(&hook_id);
            return Err(format!("extension `{}` is not running", self.lane.name));
        }
        let frame = serde_json::to_string(&HostFrame::Hook {
            hook_id: hook_id.clone(),
            event: P::NAME.to_string(),
            payload,
        })
        .map_err(|error| format!("cannot encode the hook event: {error}"))?;
        if self.lane.commands.send(frame).is_err() {
            self.abandon(&hook_id);
            return Err(format!("extension `{}` is not running", self.lane.name));
        }
        // Cancellation resolves the point FAIL OPEN (the ruling's
        // symmetry: a hook the host gave up on is treated as
        // absence — the neutral for its point) while telling the
        // guest to stop — a wedged policy extension must not
        // outlive the run it was gating.
        let outcome = tokio::select! {
            answer = rx => answer,
            _ = cancel.cancelled() => {
                self.cancel(&hook_id);
                return Ok(P::neutral());
            }
        };
        match outcome {
            // A parse failure rides as the handler's Err — the caller
            // folds it to the neutral (fail open).
            Ok(result) => result,
            Err(_) => {
                self.abandon(&hook_id);
                // The lane died and its drain answers every pending
                // item with the neutral; reaching here means our
                // entry was gone first — answer the same.
                Ok(P::neutral())
            }
        }
    }
}

/// The supervisor: every launched extension plus the scan-level
/// refusals. Dropping it closes every child (stdin EOF, bounded by
/// the tree kill); [`Supervisor::shutdown`] additionally waits for
/// the reclamation.
pub struct Supervisor {
    closing: CancellationToken,
    children: Vec<Supervised>,
    /// The process's node — every lane's face hangs on it, and the
    /// proxy handles hold it for their holds and answers.
    node: Arc<Node>,
    /// Every scanned manifest's `disables` names, concatenated —
    /// the role-shaping declarations, joined into the deny list the
    /// binary's assembly builds (`--without`'s storage).
    manifest_disables: Vec<String>,
}

struct Supervised {
    name: String,
    dir: PathBuf,
    version: String,
    description: Option<String>,
    state: Arc<ChildState>,
    lane: Arc<Lane>,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// Launch every extension found under `root` — scan plus [`launch`],
/// the everything-found boot (the host's own tests; the binary
/// partitions out the disabled first, so it calls [`launch`] on its
/// own scan).
pub fn launch_root(
    root: &Path,
    boot_timeout: Duration,
    host: LaunchContext,
) -> (
    Supervisor,
    tokio::sync::mpsc::UnboundedReceiver<ExtensionEvent>,
) {
    launch(manifest::scan(root), boot_timeout, host)
}

/// The host facts one launch serves every pipe with: the node every
/// lane mounts on (one net per process — the lanes' subscriptions,
/// asks, and learned routes live in its tables), the binary path
/// owned-session spawners need, and the backend cwd.
pub struct LaunchContext {
    /// The host's node — the same net the session host and the
    /// subprocess bridge ride.
    pub node: Arc<Node>,
    /// The running backend's own executable path (`host_facts`'s
    /// `core_path` — the host IS the binary).
    pub core_path: String,
    /// The backend's working directory.
    pub cwd: String,
}

/// Launch a scan's findings: spawn and supervise each package, report
/// each refusal — each in its own task, so a mute extension's
/// handshake timeout never delays the healthy ones. Returns the
/// supervisor plus the event channel; dropping the receiver loses
/// later reports to nowhere, so the binary drains it for its log.
/// Must run on the runtime the binary serves from (it spawns).
pub fn launch(
    found: Vec<Discovered>,
    boot_timeout: Duration,
    host: LaunchContext,
) -> (
    Supervisor,
    tokio::sync::mpsc::UnboundedReceiver<ExtensionEvent>,
) {
    let closing = CancellationToken::new();
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let lanes: Arc<Mutex<HashMap<String, CancellationToken>>> =
        Arc::new(Mutex::new(HashMap::new()));
    // The mint-law containment (the 2026-09 ruling): the lanes are
    // killable senders, so a violation contains by killing the
    // offending lane — one violator dies, not the host. Anything
    // else re-registered a live id on this node's tables and is the
    // sanctioned crash it always was.
    {
        let lanes = lanes.clone();
        host.node.on_mint_violation(move |owner, id| {
            let token = tabit_log::lock::lock(&lanes).get(owner).cloned();
            match token {
                Some(token) => {
                    let _ = writeln!(
                        std::io::stderr(),
                        "tabit: killing extension `{owner}` — it re-registered the live ask id `{id}`"
                    );
                    token.cancel();
                }
                None => violation_panic(owner, id),
            }
        });
    }
    let node = host.node.clone();
    let mut children = Vec::new();
    let mut disables = Vec::new();
    for found in found {
        match found {
            Discovered::Package { dir, manifest } => {
                let name = manifest.name.clone();
                let version = manifest.version.clone();
                let description = manifest.description.clone();
                disables.extend(manifest.disables.iter().cloned());
                let state = Arc::new(ChildState::default());
                // The lane exists before the spawn so the reader can
                // route from its first line; `commands` is the same
                // channel the writer owns.
                let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                let lane = Lane::new(name.clone(), command_tx);
                // The containment kill token: cancelling it is the
                // mint-violation policy for this lane.
                let killed = CancellationToken::new();
                tabit_log::lock::lock(&lanes).insert(name.clone(), killed.clone());
                let task = tokio::spawn(supervise(
                    dir.clone(),
                    manifest,
                    boot_timeout,
                    closing.clone(),
                    killed,
                    state.clone(),
                    lane.clone(),
                    events_tx.clone(),
                    command_rx,
                    host.core_path.clone(),
                    host.cwd.clone(),
                    node.clone(),
                    lanes.clone(),
                ));
                children.push(Supervised {
                    name,
                    dir,
                    version,
                    description,
                    state,
                    lane,
                    task: Some(task),
                });
            }
            Discovered::Refused { dir, reason } => {
                let name = dir
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dir.display().to_string());
                let state = Arc::new(ChildState::default());
                let (dead_commands, dead_reader) = tokio::sync::mpsc::unbounded_channel::<String>();
                drop(dead_reader); // sends on a dead lane fail, never queue
                let lane = Lane::new(name.clone(), dead_commands);
                lane.die(&node, &reason);
                let status = state.transition(Status::Dead { reason }, Vec::new(), Vec::new());
                let _ = events_tx.send(ExtensionEvent {
                    name: name.clone(),
                    status,
                });
                children.push(Supervised {
                    name,
                    dir,
                    version: String::new(),
                    description: None,
                    state,
                    lane,
                    task: None,
                });
            }
        }
    }
    (
        Supervisor {
            closing,
            children,
            node,
            manifest_disables: disables,
        },
        events_rx,
    )
}

impl Supervisor {
    /// An empty supervisor over the process's node — print mode's
    /// assembly holds one so every consumer has a single shape (real
    /// hosts get a supervisor over their scan, even when it finds
    /// nothing).
    pub fn empty(node: Arc<Node>) -> Supervisor {
        Supervisor {
            closing: CancellationToken::new(),
            children: Vec::new(),
            node,
            manifest_disables: Vec::new(),
        }
    }

    /// Every scanned manifest's `disables` names — the binary's
    /// assembly joins them into its deny list (`--without`'s
    /// storage), so a package's role-shaping declaration is removed
    /// by the same filter that already exists.
    pub fn manifest_disables(&self) -> &[String] {
        &self.manifest_disables
    }

    /// The standing of every extension, resolved so far and current.
    pub fn reports(&self) -> Vec<ExtensionReport> {
        self.children
            .iter()
            .map(|child| {
                let fields = tabit_log::lock::lock(&child.state.inner);
                ExtensionReport {
                    name: child.name.clone(),
                    dir: child.dir.clone(),
                    version: child.version.clone(),
                    description: child.description.clone(),
                    status: fields.status.clone().unwrap_or(Status::Starting),
                    tools: fields.tools.clone(),
                    hooks: fields.hooks.clone(),
                }
            })
            .collect()
    }

    /// Wait until every extension's handshake verdict resolved — the
    /// boot ordering the binary's assembly needs (tools exist at
    /// session build; a broken package costs one boot, loudly, and
    /// never delays another extension — the waits are concurrent
    /// inside their supervision tasks, only the joining is ordered).
    pub async fn await_resolved(&self) {
        for child in &self.children {
            child.state.wait_resolved().await;
        }
    }

    /// The proxy surface for a named extension (`None` when not
    /// installed under this supervisor's root).
    pub fn extension(&self, name: &str) -> Option<ExtensionHandle> {
        self.children
            .iter()
            .find(|child| child.name == name)
            .map(|child| ExtensionHandle {
                lane: child.lane.clone(),
                node: self.node.clone(),
            })
    }

    /// Close every child and wait for the reclamation (tests and
    /// callers that want the trees fully reclaimed). Bounded per
    /// child — a supervision task that somehow outlives its own
    /// bounds is abandoned, not awaited forever.
    pub async fn shutdown(mut self) {
        self.closing.cancel();
        let children = std::mem::take(&mut self.children);
        for child in children {
            if let Some(task) = child.task {
                let _ = tokio::time::timeout(REAP_GRACE + Duration::from_secs(1), task).await;
            }
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        // The closing token is the shutdown signal every writer and
        // supervision task selects on; nothing async belongs here.
        self.closing.cancel();
    }
}

/// One extension's lifetime, start to death.
#[allow(clippy::too_many_arguments)]
async fn supervise(
    dir: PathBuf,
    manifest: Manifest,
    boot_timeout: Duration,
    supervisor_closing: CancellationToken,
    killed: CancellationToken,
    state: Arc<ChildState>,
    lane: Arc<Lane>,
    events: tokio::sync::mpsc::UnboundedSender<ExtensionEvent>,
    command_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    core_path: String,
    cwd: String,
    node: Arc<Node>,
    lanes: Arc<Mutex<HashMap<String, CancellationToken>>>,
) {
    // A child of the supervisor's token: this extension's failure
    // closes only its own pipe (fail_before_mount cancels this one),
    // while host shutdown cascades through the hierarchy to every
    // child. One shared token here was the fleet-kill bug — one
    // broken package's pre-report failure tore down every healthy
    // sibling, silently (the review round's top finding). `killed`
    // is the containment door: the mint-law policy cancels it.
    let closing = supervisor_closing.child_token();
    let (program, args) = resolve_entry(&dir, &manifest.entry);
    let spawned = wrap_command(&program, &args, &dir).spawn();
    let mut process = match spawned {
        Ok(process) => process,
        Err(error) => {
            resolve_dead(
                &state,
                &lane,
                &events,
                &manifest.name,
                format!("cannot spawn `{}`: {error}", program.display()),
                &node,
                &lanes,
            );
            return;
        }
    };
    let stdin = match process.stdin().take() {
        Some(stdin) => stdin,
        None => {
            fail_before_mount(
                &mut process,
                &closing,
                &state,
                &lane,
                &events,
                &manifest.name,
                "opened no stdin".to_string(),
                &node,
                &lanes,
            )
            .await;
            return;
        }
    };
    let stdout = match process.stdout().take() {
        Some(stdout) => stdout,
        None => {
            fail_before_mount(
                &mut process,
                &closing,
                &state,
                &lane,
                &events,
                &manifest.name,
                "opened no stdout".to_string(),
                &node,
                &lanes,
            )
            .await;
            return;
        }
    };
    let stderr = match process.stderr().take() {
        Some(stderr) => stderr,
        None => {
            fail_before_mount(
                &mut process,
                &closing,
                &state,
                &lane,
                &events,
                &manifest.name,
                "opened no stderr".to_string(),
                &node,
                &lanes,
            )
            .await;
            return;
        }
    };
    let ring = process::spawn_stderr_ring(stderr);

    // The command writer: lines in, stdin out (the shared pipe
    // contract — the closing token IS the pipe close).
    spawn_line_writer(stdin, command_rx, Some(closing.clone()));

    // The frame reader: handshake outcome first, then the lanes.
    // Everything the shared grammar carries enters through the
    // node's intake from the lane's channel — one door, every law
    // (commands route, stamped frames forward verbatim, unstamped
    // emissions attribute their origin, and an interaction request
    // registers its ask with the answer routing home down this same
    // lane). Compatibility is one-directional (ruled 2026-09): a
    // NEWER host keeps an older extension working (the host sends
    // only what the extension declared; the extension side is told
    // to ignore frames it does not know), but an extension speaking
    // vocabulary its host lacks — an unparseable line, an unknown
    // frame type, a result answering the wrong kind of correlation —
    // is a contract break: death, with the snippet. An extension
    // built for a later host must be refused, not run half-working;
    // additions the EXTENSION can emit ride the protocol version so
    // older hosts refuse at the handshake's exact match instead of
    // mid-stream.
    let (handshake_tx, handshake_rx) = tokio::sync::oneshot::channel::<Boot>();
    let (death_tx, mut death_rx) = tokio::sync::oneshot::channel::<String>();
    {
        let lane = lane.clone();
        let node = node.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut handshake_tx = Some(handshake_tx);
            let mut death_tx = Some(death_tx);
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<ExtFrame>(&line) {
                    Ok(ExtFrame::Report {
                        protocol_version,
                        tools,
                        hooks,
                        watch,
                    }) => {
                        if let Some(tx) = handshake_tx.take() {
                            let _ = tx.send(Boot::Reported(Report {
                                protocol_version,
                                tools,
                                hooks,
                                watch,
                            }));
                        }
                        // A re-report after a good one: tolerated, ignored.
                    }
                    Ok(ExtFrame::ToolResult(result)) => {
                        // The correlation-kind law: a tool result must
                        // answer a call. The wrong kind is a contract
                        // break (death); the gone id a tolerated drop.
                        let call_id = result.call_id.clone();
                        match node.answer(&call_id, KIND_TOOL_RESULT, Box::new(result)) {
                            AnswerOutcome::Delivered => {
                                lane.end_call(&call_id);
                            }
                            AnswerOutcome::WrongKind(kind) => {
                                refuse(
                                    &mut handshake_tx,
                                    &mut death_tx,
                                    format!(
                                        "sent a tool result for a non-call id `{call_id}` ({kind})"
                                    ),
                                );
                                break;
                            }
                            AnswerOutcome::Missed => {}
                        }
                    }
                    Ok(ExtFrame::HookResult(result)) => {
                        let hook_id = result.hook_id.clone();
                        match node.answer(&hook_id, KIND_HOOK_RESULT, Box::new(result)) {
                            AnswerOutcome::Delivered => {
                                lane.end_call(&hook_id);
                            }
                            AnswerOutcome::WrongKind(kind) => {
                                refuse(
                                    &mut handshake_tx,
                                    &mut death_tx,
                                    format!(
                                        "sent a hook result for a non-hook id `{hook_id}` ({kind})"
                                    ),
                                );
                                break;
                            }
                            AnswerOutcome::Missed => {}
                        }
                    }
                    Ok(ExtFrame::ServiceRequest {
                        request_id,
                        call_id,
                        verb,
                    }) => {
                        hold_service(node.clone(), lane.clone(), request_id, call_id, verb);
                    }
                    // Not a lane frame: the shared grammar rides the
                    // same lines (flat, byte-identical with the
                    // frontend edge), entering through the node's
                    // intake from this lane — commands route, frames
                    // fan (stamped verbatim, unstamped
                    // origin-attributed to the lane), and an
                    // interaction request registers its ask with the
                    // answer routing home down this pipe. A line
                    // parseable as none of these is the contract
                    // break it always was.
                    Err(_) => {
                        if let Some(inbound) = parse_shared(&line) {
                            node.intake(&lane.channel, inbound);
                            continue;
                        }
                        refuse(
                            &mut handshake_tx,
                            &mut death_tx,
                            format!(
                                "sent an unparseable or unknown-type line: {}",
                                snippet(&line)
                            ),
                        );
                        break;
                    }
                }
            }
            // EOF: the pipe is closed — before the report it is a
            // failed boot, after it the process is gone. Either way the
            // lane dies first (the node sweep settles its open
            // round-trips and retracts its registrations), then the
            // verdict crosses.
            let reason = "the extension process died mid-call".to_string();
            lane.die(&node, &reason);
            if let Some(tx) = handshake_tx.take() {
                let _ = tx.send(Boot::Failed("closed before the report".to_string()));
            } else if let Some(tx) = death_tx.take() {
                let _ = tx.send("the extension process exited".to_string());
            }
        });
    }

    // The report, bounded (the report model: the extension speaks
    // first; the host decides — an incompatible report is the kill
    // below, and the host's facts cross only after the check).
    let outcome = tokio::select! {
        outcome = handshake_rx => {
            outcome.unwrap_or(Boot::Failed("closed before the report".to_string()))
        }
        _ = tokio::time::sleep(boot_timeout) => {
            Boot::Failed(format!("no report within {boot_timeout:?}"))
        }
    };
    let report = match outcome {
        Boot::Failed(reason) => {
            fail_before_mount(
                &mut process,
                &closing,
                &state,
                &lane,
                &events,
                &manifest.name,
                reason,
                &node,
                &lanes,
            )
            .await;
            return;
        }
        Boot::Reported(report) => report,
    };
    if let Err(reason) = validate(&report) {
        fail_before_mount(
            &mut process,
            &closing,
            &state,
            &lane,
            &events,
            &manifest.name,
            reason,
            &node,
            &lanes,
        )
        .await;
        return;
    }

    // The host's facts cross now — after the report cleared the
    // version check. Serialization here is pure data over a serde
    // type — the impossible failure is the sanctioned crash, never a
    // silent empty line (which the guest would only see as a broken
    // contract).
    #[allow(clippy::expect_used)] // sanctioned crash: pure-data serialization
    let facts = serde_json::to_string(&HostFrame::HostFacts { core_path, cwd })
        .expect("HostFrame::HostFacts always serializes");
    let _ = lane.commands.send(facts);

    let Report {
        tools,
        hooks,
        watch,
        ..
    } = report;
    // The report's watch list subscribes the lane's channel on the
    // node: each watched kind's frames reach the lane's event
    // delivery, which writes the wire line down its stdin. Both
    // doors — the host's own sessions and whatever arrives from
    // elsewhere — are the watch, stated as its locality. Death
    // retracts the lane's every registration (the
    // node sweep).
    for kind in &watch {
        node.subscribe_channel(kind, Locality::Both, &lane.channel);
    }
    let status = state.transition(Status::Alive, tools, hooks);
    let _ = events.send(ExtensionEvent {
        name: manifest.name.clone(),
        status,
    });

    // Alive: watch for death — the reader's signal, or the process
    // exit itself when stdout's write end is held open by a
    // descendant (a server under an extension; the reaper pattern
    // the subagent bridge already uses) — or host shutdown (the
    // closing token — stdin drops, a graceful extension exits, a
    // wedged one meets the tree kill).
    let cause = tokio::select! {
        death = &mut death_rx => {
            death.unwrap_or_else(|_| "the extension process exited".to_string())
        }
        _ = closing.cancelled() => {
            reap_with_grace(&mut process).await;
            return; // Host-initiated: not a death report.
        }
        exit = process.wait() => {
            // stdout never closed (a grandchild holds it), so the
            // reader cannot see this death — the exit is the signal.
            // The lane's drain answers every pending call; the exit
            // status itself adds nothing the reason needs.
            let _ = exit;
            "the extension process exited while its stdout stayed open".to_string()
        }
        _ = killed.cancelled() => {
            // The mint-law containment: this sender re-registered a
            // live ask id on the node's table — state proven
            // untrustworthy — kill the tree now (no grace).
            process::kill_now(&mut process, &closing).await;
            let reason = "killed: it re-registered a live ask id (the mint law)".to_string();
            resolve_dead(&state, &lane, &events, &manifest.name, reason, &node, &lanes);
            return;
        }
    };
    let exit = reap_with_grace(&mut process).await;
    let reason = match exit {
        Some(exit) => format!("{cause} ({})", exit_describe(exit)),
        None => cause,
    };
    let tail = crash_tail(&ring);
    let reason = if tail.is_empty() {
        reason
    } else {
        format!("{reason}\nstderr tail:\n{tail}")
    };
    resolve_dead(
        &state,
        &lane,
        &events,
        &manifest.name,
        reason,
        &node,
        &lanes,
    );
}

/// Hold one extension service request on the node's ask table (the
/// ruling: service requests are asks — the response claims by id)
/// and dispatch its verb. The delivery writes the response line back
/// down the asking lane whatever settles it; the lane's death sweep
/// discards the entry (the pipe is gone — fail-soft by design).
fn hold_service(
    node: Arc<Node>,
    lane: Arc<Lane>,
    request_id: String,
    call_id: String,
    verb: ServiceVerb,
) {
    let commands = lane.commands.clone();
    let id = request_id.clone();
    // The id is the GUEST's mint, crossed the pipe — the contained
    // door: a live id is the sender's violation (the policy kills
    // the lane) and the request dies with it, un-dispatched. The
    // frame never panics the host on an external bug.
    let held = node.try_hold(
        &lane.name,
        &request_id,
        KIND_SERVICE_RESPONSE,
        move |outcome| {
            if let tabit_wire::asks::Outcome::Answered(answer) = outcome {
                let frame = tabit_wire::asks::unanswer::<HostFrame>(answer);
                #[allow(clippy::expect_used)] // sanctioned crash: pure-data serialization
                let line = serde_json::to_string(&frame).expect("HostFrame always serializes");
                let _ = commands.send(line);
            }
        },
    );
    if !held {
        return;
    }
    let node = node.clone();
    tokio::spawn(async move {
        let services = lane.capability(&call_id);
        let response = match verb {
            ServiceVerb::ModelPrompt {
                prompt,
                model,
                max_tokens,
            } => {
                let outcome = match services {
                    Some(services) => {
                        services
                            .model_prompt(
                                &lane.name,
                                ModelPromptRequest {
                                    prompt,
                                    model,
                                    max_tokens,
                                },
                            )
                            .await
                    }
                    None => Err("no session context is attached to this call".to_string()),
                };
                match outcome {
                    Ok(ModelPromptOk {
                        text,
                        usage:
                            ServiceUsage {
                                input_tokens,
                                output_tokens,
                                total_tokens,
                            },
                    }) => HostFrame::ServiceResponse {
                        request_id,
                        result: Some(serde_json::json!({
                            "text": text,
                            "usage": {
                                "input_tokens": input_tokens,
                                "output_tokens": output_tokens,
                                "total_tokens": total_tokens,
                            },
                        })),
                        error: None,
                    },
                    Err(message) => HostFrame::ServiceResponse {
                        request_id,
                        result: None,
                        error: Some(message),
                    },
                }
            }
        };
        // The response claims the held entry by id (the ruling: the
        // ask table is the one round-trip law); a late response
        // after the lane's sweep finds a gone id and drops.
        let _ = node.answer(&id, KIND_SERVICE_RESPONSE, Box::new(response));
    });
}

/// What the reader decided about the report.
enum Boot {
    Reported(Report),
    Failed(String),
}

/// One contract break spotted by the reader: before the report it
/// fails the mount; after it, it is the death signal.
fn refuse(
    handshake_tx: &mut Option<tokio::sync::oneshot::Sender<Boot>>,
    death_tx: &mut Option<tokio::sync::oneshot::Sender<String>>,
    refusal: String,
) {
    match handshake_tx.take() {
        Some(tx) => {
            let _ = tx.send(Boot::Failed(refusal));
        }
        None => {
            if let Some(tx) = death_tx.take() {
                let _ = tx.send(refusal);
            }
        }
    }
}

/// The entry command: first token names a file in the package dir
/// when one is there, else resolves on the OS path (a runtime from
/// PATH, a relative script — the package's declared business).
/// `Some` is the caller's invariant: static packages never reach the
/// spawn (the binary's partition filters them before launch).
fn resolve_entry(dir: &Path, entry: &Option<Vec<String>>) -> (PathBuf, Vec<String>) {
    #[allow(clippy::expect_used)]
    let entry = entry
        .as_ref()
        .expect("internal invariant violated: a static package was launched");
    #[allow(clippy::expect_used)]
    let first = entry
        .first()
        .expect("internal invariant violated: scan refused empty entries");
    let program = Path::new(first);
    let program = if program.is_relative() {
        let local = dir.join(program);
        if local.is_file() {
            local
        } else {
            program.to_path_buf()
        }
    } else {
        program.to_path_buf()
    };
    let args = entry.iter().skip(1).cloned().collect();
    (program, args)
}

/// The handshake's load-time contract: the version must match
/// exactly, the hook points must be the engine's.
fn validate(report: &Report) -> Result<(), String> {
    if report.protocol_version != EXTENSION_PROTOCOL_VERSION {
        return Err(format!(
            "speaks extension protocol version {} (this host speaks {EXTENSION_PROTOCOL_VERSION})",
            report.protocol_version
        ));
    }
    for hook in &report.hooks {
        if !tabit_protocol::points::LIST.contains(&hook.event.as_str()) {
            return Err(format!(
                "subscribes to unknown hook point `{}` (known: {})",
                hook.event,
                tabit_protocol::points::LIST.join(", ")
            ));
        }
    }
    Ok(())
}

/// A pre-report failure: nothing was proven, so the tree dies now (no
/// grace — the pipe contract was never honored) and the dead report
/// resolves.
#[allow(clippy::too_many_arguments)] // the launch context is irreducible; the alternative is a struct of seven
async fn fail_before_mount(
    process: &mut Box<dyn ChildWrapper>,
    closing: &CancellationToken,
    state: &Arc<ChildState>,
    lane: &Arc<Lane>,
    events: &tokio::sync::mpsc::UnboundedSender<ExtensionEvent>,
    name: &str,
    reason: String,
    node: &Arc<Node>,
    lanes: &Arc<Mutex<HashMap<String, CancellationToken>>>,
) {
    process::kill_now(process, closing).await;
    resolve_dead(state, lane, events, name, reason, node, lanes);
}

// The post-ack close lives in [`tabit_wire::process::reap_with_grace`]
// (the shared pipe contract).
fn resolve_dead(
    state: &Arc<ChildState>,
    lane: &Arc<Lane>,
    events: &tokio::sync::mpsc::UnboundedSender<ExtensionEvent>,
    name: &str,
    reason: String,
    node: &Arc<Node>,
    lanes: &Arc<Mutex<HashMap<String, CancellationToken>>>,
) {
    // The real reason reaches the sweep: every orphaned round-trip's
    // failure text names the actual death (the exit code, the crash
    // tail, the mint kill) — the model-visible error is the honest
    // one. The reader-EOF path swept earlier with its own reason;
    // re-sweeping here is idempotent (the entries are gone).
    lane.die(node, &reason);
    tabit_log::lock::lock(lanes).remove(&lane.name);
    // The declared capabilities survive the death — the catalog's
    // "what it would have served" report. (Pre-ack deaths never
    // recorded any; post-ack deaths keep their ack's declarations.)
    let (tools, hooks) = {
        let fields = tabit_log::lock::lock(&state.inner);
        (fields.tools.clone(), fields.hooks.clone())
    };
    let status = state.transition(Status::Dead { reason }, tools, hooks);
    let _ = events.send(ExtensionEvent {
        name: name.to_string(),
        status,
    });
}

fn exit_describe(exit: std::process::ExitStatus) -> String {
    match exit.code() {
        Some(code) => format!("exit code {code}"),
        None => "terminated without an exit code".to_string(),
    }
}

/// The crash report's tail: the last few stderr lines.
fn snippet(line: &str) -> String {
    let head: String = line.chars().take(60).collect();
    if head.len() < line.len() {
        format!("{head}…")
    } else {
        head
    }
}
