//! The extension supervisor: launch every installed package,
//! handshake each, watch for death. One supervisor per backend
//! process; the binary owns it (extensions are backend machinery —
//! their contributions reach sessions through the binary's assembly,
//! never through tabit-session).
//!
//! Death policy (ruled): mark dead, report, never respawn mid-run —
//! mounted contributions stay by construction (none exist in v1).
//! The report surface is the [`Supervisor::reports`] snapshot, the
//! [`ExtensionEvent`] channel the binary logs, and (task 2) the
//! `extensions_available` wire catalog.
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
//! ask lifts after), a writer task owns stdin (the closing token IS
//! the pipe drop), and the supervision task owns the process handle
//! end-to-end — a pre-ack failure kills the tree immediately
//! (nothing was proven), a post-ack death gets the grace-bounded
//! reclaim.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::manifest::{self, Discovered, Manifest};
use crate::process::{self, StderrRing, wrap_command};
use crate::protocol::{
    Ack, EXTENSION_PROTOCOL_VERSION, ExtFrame, HOOK_POINTS, HookDecision, HookDecl, HostFrame,
    ToolDecl, ToolWireResult,
};
use process_wrap::tokio::ChildWrapper;
use rig_agent::tool::interaction::{InteractionOutcome, UserInteraction};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

/// The default handshake window — same value and reason as the
/// subagent bridge's: slow runtimes (a Python import, a cold Node)
/// need the room; healthy extensions answer in milliseconds.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a closing extension gets to exit on its own before the
/// tree kill — same value and reason as the subagent bridge's reap
/// grace (ample for a healthy shutdown; exactly the wedged cases
/// burn it).
const REAP_GRACE: Duration = Duration::from_secs(5);

/// One extension's standing, as the host sees it.
#[derive(Debug, Clone)]
pub enum Status {
    /// Spawned, handshaking still in flight.
    Starting,
    /// Acked; capabilities are its declared set for the host's life.
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

/// The host-side half of one extension's pipe after the spawn: the
/// outgoing frame lane and the pending-call registry.
struct Lane {
    name: String,
    commands: tokio::sync::mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<String, PendingCall>>,
    next_call_id: AtomicU64,
    /// Set at the pipe's end (EOF or garbage), before the pending
    /// drain — a call registering after death fails fast instead of
    /// awaiting a result that can never come.
    dead: AtomicBool,
}

impl Lane {
    fn new(name: String, commands: tokio::sync::mpsc::UnboundedSender<String>) -> Arc<Self> {
        Arc::new(Self {
            name,
            commands,
            pending: Mutex::new(HashMap::new()),
            next_call_id: AtomicU64::new(1),
            dead: AtomicBool::new(false),
        })
    }

    /// The pipe's last act: answer every pending item — executions
    /// with their failure, policy with its fail-open fallback — then
    /// stay dead-flagged for late arrivals.
    fn die(&self, reason: &str) {
        self.dead.store(true, Ordering::SeqCst);
        for (call_id, pending) in tabit_log::lock::lock(&self.pending).drain() {
            match pending.waiter {
                Waiter::ToolCall(result) => {
                    let _ = result.send(ToolWireResult {
                        call_id,
                        error: Some(reason.to_string()),
                        report: String::new(),
                        details: None,
                    });
                }
                Waiter::Hook { result, fallback } => {
                    let _ = result.send(fallback);
                }
            }
        }
    }
}

/// One outstanding forwarded item, keyed by its correlation id (a
/// call id or a hook id — the lift routes asks by the same key).
struct PendingCall {
    waiter: Waiter,
    /// The calling session's interaction capability — the lift's
    /// routing target for this call's or hook's asks.
    ask: Option<Arc<dyn UserInteraction>>,
}

/// What the awaiting side of a forwarded item receives.
enum Waiter {
    /// A tool call: the wire result, or the transport failure (the
    /// lane was dead before the frame left).
    ToolCall(tokio::sync::oneshot::Sender<ToolWireResult>),
    /// A hook: the decision, or the transport failure. The fallback is
    /// what a death answers with — **policy fails open** (crash
    /// isolation: one dead package cannot brick the tool phase; the
    /// death itself is reported loudly), where a failed *execution*
    /// answers with its error. The asymmetry is the ruling.
    Hook {
        result: tokio::sync::oneshot::Sender<HookDecision>,
        fallback: HookDecision,
    },
}

/// The proxy surface for one extension — what the binary's tool
/// assembly holds and forwards calls through. Clonable: every proxy
/// tool for one extension shares the lane.
#[derive(Clone)]
pub struct ExtensionHandle {
    lane: Arc<Lane>,
}

impl ExtensionHandle {
    /// Call one tool on the extension and await its wire result.
    /// `ask` is the calling session's interaction capability (the
    /// routing target for the extension's mid-call asks); without
    /// one, asks answer dismissed — fail closed, exactly as core
    /// tools behave on a non-interactive session.
    pub async fn call(
        &self,
        tool: &str,
        args: serde_json::Value,
        ask: Option<Arc<dyn UserInteraction>>,
    ) -> Result<ToolWireResult, String> {
        let call_id = format!(
            "{}-{}",
            self.lane.name,
            self.lane.next_call_id.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        tabit_log::lock::lock(&self.lane.pending).insert(
            call_id.clone(),
            PendingCall {
                waiter: Waiter::ToolCall(tx),
                ask,
            },
        );
        // The dead check rides after the insert and inside the same
        // ordering as `die`'s store-then-drain, so a death between
        // insert and check is caught either by the flag or by the
        // drain itself.
        if self.lane.dead.load(Ordering::SeqCst) {
            tabit_log::lock::lock(&self.lane.pending).remove(&call_id);
            return Err(format!("extension `{}` is not running", self.lane.name));
        }
        let frame = serde_json::to_string(&HostFrame::ToolCall {
            call_id: call_id.clone(),
            name: tool.to_string(),
            args,
        })
        .map_err(|error| format!("cannot encode the tool call: {error}"))?;
        if self.lane.commands.send(frame).is_err() {
            tabit_log::lock::lock(&self.lane.pending).remove(&call_id);
            return Err(format!("extension `{}` is not running", self.lane.name));
        }
        match rx.await {
            Ok(result) => Ok(result),
            Err(_) => {
                tabit_log::lock::lock(&self.lane.pending).remove(&call_id);
                Err(format!("extension `{}` closed mid-call", self.lane.name))
            }
        }
    }

    /// Forward one hook event to the extension and await its decision
    /// (the hook lane, checklist task 3). Same shape as [`call`]: the
    /// ask capability rides along for mid-hook asks, a dead lane is a
    /// transport error — the caller owns the policy mapping (the
    /// binary fails policy open: run/keep), while a death *during* the
    /// await resolves through the lane's drain with the same fallback.
    pub async fn hook(
        &self,
        event: &str,
        payload: serde_json::Value,
        ask: Option<Arc<dyn UserInteraction>>,
    ) -> Result<HookDecision, String> {
        let fallback = if event == "tool_result" {
            HookDecision::Keep
        } else {
            HookDecision::Run
        };
        let hook_id = format!(
            "{}-h{}",
            self.lane.name,
            self.lane.next_call_id.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        tabit_log::lock::lock(&self.lane.pending).insert(
            hook_id.clone(),
            PendingCall {
                waiter: Waiter::Hook {
                    result: tx,
                    fallback: fallback.clone(),
                },
                ask,
            },
        );
        if self.lane.dead.load(Ordering::SeqCst) {
            tabit_log::lock::lock(&self.lane.pending).remove(&hook_id);
            return Err(format!("extension `{}` is not running", self.lane.name));
        }
        let frame = serde_json::to_string(&HostFrame::Hook {
            hook_id: hook_id.clone(),
            event: event.to_string(),
            payload,
        })
        .map_err(|error| format!("cannot encode the hook event: {error}"))?;
        if self.lane.commands.send(frame).is_err() {
            tabit_log::lock::lock(&self.lane.pending).remove(&hook_id);
            return Err(format!("extension `{}` is not running", self.lane.name));
        }
        match rx.await {
            Ok(decision) => Ok(decision),
            Err(_) => {
                tabit_log::lock::lock(&self.lane.pending).remove(&hook_id);
                // The lane died and its drain answers every pending
                // item with the fallback; reaching here means our
                // entry was gone first — answer the same.
                Ok(fallback)
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

/// Launch every extension installed under `root`: scan, spawn, and
/// supervise — each in its own task, so a mute extension's handshake
/// timeout never delays the healthy ones. Returns the supervisor
/// plus the event channel; dropping the receiver loses later reports
/// to nowhere, so the binary drains it for its log. Must run on the
/// runtime the binary serves from (it spawns).
pub fn launch(
    root: &Path,
    handshake_timeout: Duration,
) -> (
    Supervisor,
    tokio::sync::mpsc::UnboundedReceiver<ExtensionEvent>,
) {
    let closing = CancellationToken::new();
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut children = Vec::new();
    for found in manifest::scan(root) {
        match found {
            Discovered::Package { dir, manifest } => {
                let name = manifest.name.clone();
                let version = manifest.version.clone();
                let description = manifest.description.clone();
                let state = Arc::new(ChildState::default());
                // The lane exists before the spawn so the reader can
                // route from its first line; `commands` is the same
                // channel the writer owns.
                let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                let lane = Lane::new(name.clone(), command_tx);
                let task = tokio::spawn(supervise(
                    dir.clone(),
                    manifest,
                    handshake_timeout,
                    closing.clone(),
                    state.clone(),
                    lane.clone(),
                    events_tx.clone(),
                    command_rx,
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
                lane.die(&reason);
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
    (Supervisor { closing, children }, events_rx)
}

impl Supervisor {
    /// An empty supervisor — child roles and extension-less boots
    /// hold one so assembly has a single shape.
    pub fn empty() -> Supervisor {
        Supervisor {
            closing: CancellationToken::new(),
            children: Vec::new(),
        }
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
    handshake_timeout: Duration,
    closing: CancellationToken,
    state: Arc<ChildState>,
    lane: Arc<Lane>,
    events: tokio::sync::mpsc::UnboundedSender<ExtensionEvent>,
    mut command_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
) {
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
            );
            return;
        }
    };
    let stdin = match process.stdin().take() {
        Some(stdin) => stdin,
        None => {
            fail_before_ack(
                &mut process,
                &closing,
                &state,
                &lane,
                &events,
                &manifest.name,
                "opened no stdin".to_string(),
            )
            .await;
            return;
        }
    };
    let stdout = match process.stdout().take() {
        Some(stdout) => stdout,
        None => {
            fail_before_ack(
                &mut process,
                &closing,
                &state,
                &lane,
                &events,
                &manifest.name,
                "opened no stdout".to_string(),
            )
            .await;
            return;
        }
    };
    let stderr = match process.stderr().take() {
        Some(stderr) => stderr,
        None => {
            fail_before_ack(
                &mut process,
                &closing,
                &state,
                &lane,
                &events,
                &manifest.name,
                "opened no stderr".to_string(),
            )
            .await;
            return;
        }
    };
    let ring = process::spawn_stderr_ring(stderr);

    // The command writer: lines in, stdin out. The closing token IS
    // the pipe close — deliver what the close raced, then drop.
    {
        let writer_closing = closing.clone();
        tokio::spawn(async move {
            let mut stdin = stdin;
            loop {
                let line = tokio::select! {
                    _ = writer_closing.cancelled() => {
                        // Deliver what the close raced, then drop the pipe.
                        while let Ok(line) = command_rx.try_recv() {
                            if write_line(&mut stdin, &line).await.is_err() {
                                return;
                            }
                        }
                        break;
                    }
                    line = command_rx.recv() => match line {
                        Some(line) => line,
                        None => break,
                    },
                };
                if write_line(&mut stdin, &line).await.is_err() {
                    break;
                }
            }
            // Drop closes the pipe.
        });
    }

    // The frame reader: handshake outcome first, then the tool lane
    // (results resolved, asks lifted). A parseable frame the host
    // does not know is tolerated and ignored (the additive-vocabulary
    // rule); an unparseable line is death — the pipe is the contract
    // (a newer extension's unknown frame type parses as garbage on an
    // older host; both sides version as one workspace until external
    // extensions exist).
    let (handshake_tx, handshake_rx) = tokio::sync::oneshot::channel::<Handshake>();
    let (death_tx, mut death_rx) = tokio::sync::oneshot::channel::<String>();
    {
        let lane = lane.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut handshake_tx = Some(handshake_tx);
            let mut death_tx = Some(death_tx);
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<ExtFrame>(&line) {
                    Ok(ExtFrame::Ack {
                        protocol_version,
                        tools,
                        hooks,
                    }) => {
                        if let Some(tx) = handshake_tx.take() {
                            let _ = tx.send(Handshake::Acked(Ack {
                                protocol_version,
                                tools,
                                hooks,
                            }));
                        }
                        // A re-ack after a good one: tolerated, ignored.
                    }
                    Ok(ExtFrame::ToolResult(result)) => {
                        let pending = tabit_log::lock::lock(&lane.pending).remove(&result.call_id);
                        if let Some(PendingCall {
                            waiter: Waiter::ToolCall(result_tx),
                            ..
                        }) = pending
                        {
                            let _ = result_tx.send(result);
                        }
                    }
                    Ok(ExtFrame::HookResult(result)) => {
                        let pending = tabit_log::lock::lock(&lane.pending).remove(&result.hook_id);
                        if let Some(PendingCall {
                            waiter:
                                Waiter::Hook {
                                    result: result_tx, ..
                                },
                            ..
                        }) = pending
                        {
                            let _ = result_tx.send(result.decision);
                        }
                    }
                    Ok(ExtFrame::InteractionRequest {
                        call_id,
                        id,
                        ui_type,
                        payload,
                    }) => {
                        lift_ask(lane.clone(), call_id, id, ui_type, payload);
                    }
                    Err(_) => {
                        let refusal = format!("sent an unparseable line: {}", snippet(&line));
                        match handshake_tx.take() {
                            Some(tx) => {
                                let _ = tx.send(Handshake::Failed(refusal));
                            }
                            None => {
                                if let Some(tx) = death_tx.take() {
                                    let _ = tx.send(refusal);
                                }
                            }
                        }
                        break;
                    }
                }
            }
            // EOF: the pipe is closed — before the ack it is a failed
            // handshake, after it the process is gone. Either way the
            // lane dies first (fail the pending calls), then the
            // verdict crosses.
            let reason = "the extension process died mid-call".to_string();
            lane.die(&reason);
            if let Some(tx) = handshake_tx.take() {
                let _ = tx.send(Handshake::Failed("closed before the handshake".to_string()));
            } else if let Some(tx) = death_tx.take() {
                let _ = tx.send("the extension process exited".to_string());
            }
        });
    }

    // The handshake, bounded.
    let _ = lane.commands.send(
        serde_json::to_string(&HostFrame::Initialize {
            protocol_version: EXTENSION_PROTOCOL_VERSION,
        })
        .unwrap_or_default(),
    );
    let outcome = tokio::select! {
        outcome = handshake_rx => {
            outcome.unwrap_or(Handshake::Failed("closed before the handshake".to_string()))
        }
        _ = tokio::time::sleep(handshake_timeout) => {
            Handshake::Failed(format!("no handshake within {handshake_timeout:?}"))
        }
    };
    let ack = match outcome {
        Handshake::Failed(reason) => {
            fail_before_ack(
                &mut process,
                &closing,
                &state,
                &lane,
                &events,
                &manifest.name,
                reason,
            )
            .await;
            return;
        }
        Handshake::Acked(ack) => ack,
    };
    if let Err(reason) = validate(&ack) {
        fail_before_ack(
            &mut process,
            &closing,
            &state,
            &lane,
            &events,
            &manifest.name,
            reason,
        )
        .await;
        return;
    }

    let Ack { tools, hooks, .. } = ack;
    let status = state.transition(Status::Alive, tools, hooks);
    let _ = events.send(ExtensionEvent {
        name: manifest.name.clone(),
        status,
    });

    // Alive: watch for death (the reader's signal) or host shutdown
    // (the closing token — stdin drops, a graceful extension exits,
    // a wedged one meets the tree kill).
    let cause = tokio::select! {
        death = &mut death_rx => {
            death.unwrap_or_else(|_| "the extension process exited".to_string())
        }
        _ = closing.cancelled() => {
            reclaim(&mut process).await;
            return; // Host-initiated: not a death report.
        }
    };
    let exit = reclaim(&mut process).await;
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
    resolve_dead(&state, &lane, &events, &manifest.name, reason);
}

/// The interaction lift: route one extension ask to the session whose
/// proxy call is executing (the pending entry's capability), ask
/// through the hub, and carry the answer back down the pipe. No
/// capability (a non-interactive session, or the call already gone)
/// answers dismissed — fail closed.
fn lift_ask(
    lane: Arc<Lane>,
    call_id: String,
    id: String,
    ui_type: String,
    payload: serde_json::Value,
) {
    tokio::spawn(async move {
        let ask = tabit_log::lock::lock(&lane.pending)
            .get(&call_id)
            .and_then(|pending| pending.ask.clone());
        // The lift never strands the guest: a panic inside the
        // capability's future would kill this task silently and the
        // asking extension would wait forever (the lesson from the
        // contract-test hang) — catch it and answer dismissed, the
        // askers' fail-closed case.
        let outcome = match ask {
            Some(ask) => {
                match futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
                    ask.request(&ui_type, payload),
                ))
                .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => InteractionOutcome::Dismissed,
                }
            }
            None => InteractionOutcome::Dismissed,
        };
        let frame = HostFrame::InteractionResponse {
            id,
            outcome: match outcome {
                InteractionOutcome::Answered(payload) => Some(payload),
                InteractionOutcome::Dismissed => None,
            },
        };
        let _ = lane
            .commands
            .send(serde_json::to_string(&frame).unwrap_or_default());
    });
}

/// What the reader decided about the handshake.
enum Handshake {
    Acked(Ack),
    Failed(String),
}

/// The entry command: first token names a file in the package dir
/// when one is there, else resolves on the OS path (a runtime from
/// PATH, a relative script — the package's declared business).
fn resolve_entry(dir: &Path, entry: &[String]) -> (PathBuf, Vec<String>) {
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
fn validate(ack: &Ack) -> Result<(), String> {
    if ack.protocol_version != EXTENSION_PROTOCOL_VERSION {
        return Err(format!(
            "speaks extension protocol version {} (this host speaks {EXTENSION_PROTOCOL_VERSION})",
            ack.protocol_version
        ));
    }
    for hook in &ack.hooks {
        if !HOOK_POINTS.contains(&hook.event.as_str()) {
            return Err(format!(
                "subscribes to unknown hook point `{}` (known: {})",
                hook.event,
                HOOK_POINTS.join(", ")
            ));
        }
    }
    Ok(())
}

/// A pre-ack failure: nothing was proven, so the tree dies now (no
/// grace — the pipe contract was never honored) and the dead report
/// resolves.
async fn fail_before_ack(
    process: &mut Box<dyn ChildWrapper>,
    closing: &CancellationToken,
    state: &Arc<ChildState>,
    lane: &Arc<Lane>,
    events: &tokio::sync::mpsc::UnboundedSender<ExtensionEvent>,
    name: &str,
    reason: String,
) {
    closing.cancel();
    let _ = Box::into_pin(process.kill()).await;
    let _ = process.wait().await;
    resolve_dead(state, lane, events, name, reason);
}

/// The post-ack close: a bounded window to exit on its own, then the
/// tree kill.
async fn reclaim(process: &mut Box<dyn ChildWrapper>) -> Option<std::process::ExitStatus> {
    match tokio::time::timeout(REAP_GRACE, process.wait()).await {
        Ok(status) => status.ok(),
        Err(_) => {
            let _ = Box::into_pin(process.kill()).await;
            process.wait().await.ok()
        }
    }
}

fn resolve_dead(
    state: &Arc<ChildState>,
    lane: &Arc<Lane>,
    events: &tokio::sync::mpsc::UnboundedSender<ExtensionEvent>,
    name: &str,
    reason: String,
) {
    lane.die("the extension is not running");
    let status = state.transition(Status::Dead { reason }, Vec::new(), Vec::new());
    let _ = events.send(ExtensionEvent {
        name: name.to_string(),
        status,
    });
}

async fn write_line(stdin: &mut tokio::process::ChildStdin, line: &str) -> std::io::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

fn exit_describe(exit: std::process::ExitStatus) -> String {
    match exit.code() {
        Some(code) => format!("exit code {code}"),
        None => "terminated without an exit code".to_string(),
    }
}

/// The crash report's tail: the last few stderr lines.
fn crash_tail(ring: &StderrRing) -> String {
    let tail: Vec<String> = tabit_log::lock::lock(ring)
        .iter()
        .rev()
        .take(8)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    tail.join("\n")
}

fn snippet(line: &str) -> String {
    let head: String = line.chars().take(60).collect();
    if head.len() < line.len() {
        format!("{head}…")
    } else {
        head
    }
}
