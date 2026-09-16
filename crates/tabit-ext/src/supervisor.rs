//! The extension supervisor: launch every installed package,
//! handshake each, watch for death. One supervisor per backend
//! process; the binary owns it (extensions are backend machinery —
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
//! [`crate::process`], shared with the bridge. Local to here: the
//! typed-frame reader and the lane machinery; a pre-ack failure
//! kills the tree immediately (nothing was proven), a post-ack death
//! gets the grace-bounded reclaim.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::manifest::{self, Discovered, Manifest};
use crate::process::{self, wrap_command};
use crate::protocol::{
    Ack, EXTENSION_PROTOCOL_VERSION, ExtFrame, HOOK_POINTS, HookDecision, HookDecl, HostFrame,
    ServiceVerb, ToolDecl, ToolWireResult,
};
use process_wrap::tokio::ChildWrapper;
use rig_agent::tool::interaction::InteractionOutcome;
use rig_agent::tool::services::{HostServices, ModelPromptOk, ModelPromptRequest, ServiceUsage};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

/// The default handshake window — one value and one reason for every
/// spawned pipe (extensions here, subagent children in the bridge):
/// slow runtimes (a Python import, a cold Node) need the room;
/// healthy children answer in milliseconds. Lives in
/// [`crate::process`] with the rest of the shared pipe plumbing;
/// re-exported here for the existing call sites.
pub use crate::process::HANDSHAKE_TIMEOUT;
use crate::process::{REAP_GRACE, crash_tail, reap_with_grace, spawn_command_writer};

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
/// call id or a hook id — the envelope routes service requests by
/// the same key).
struct PendingCall {
    waiter: Waiter,
    /// The calling session's host-service capability — the envelope's
    /// routing target for this call's or hook's requests (verb zero
    /// included: the ask).
    services: Option<Arc<dyn HostServices>>,
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
    /// `services` is the calling session's host-service capability
    /// (the routing target for the extension's mid-call envelope
    /// requests — asks and model prompts); without one, asks answer
    /// dismissed and verbs error — fail closed, exactly as core
    /// tools behave on a non-interactive session. `cancel` is the
    /// run's token: firing it sends [`HostFrame::Cancel`] down the
    /// pipe (the guest's signal to stop — the token-and-detach
    /// contract, crossing the process boundary), removes the pending
    /// entry (racing asks answer dismissed), and fails the call.
    pub async fn call(
        &self,
        tool: &str,
        args: serde_json::Value,
        services: Option<Arc<dyn HostServices>>,
        cancel: tokio_util::sync::CancellationToken,
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
                services,
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
        let outcome = tokio::select! {
            result = rx => result.map_err(|_| {
                tabit_log::lock::lock(&self.lane.pending).remove(&call_id);
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
                tabit_log::lock::lock(&self.lane.pending).remove(&call_id);
                Err(error)
            }
        }
    }

    /// The cancel bookkeeping: drop the pending entry (racing asks
    /// lift to nothing and answer dismissed; a racing result is an
    /// unknown id, tolerated) and tell the guest to stop. Shared by
    /// the call and hook lanes — the id is whichever correlation.
    fn cancel(&self, call_id: &str) {
        tabit_log::lock::lock(&self.lane.pending).remove(call_id);
        let frame = HostFrame::Cancel {
            call_id: call_id.to_string(),
        };
        #[allow(clippy::expect_used)] // sanctioned crash: pure-data serialization
        let frame = serde_json::to_string(&frame).expect("HostFrame always serializes");
        let _ = self.lane.commands.send(frame);
    }

    /// Forward one hook event to the extension and await its decision
    /// (the hook lane, checklist task 3). Same shape as [`call`]: the
    /// session's host-service capability rides along for mid-hook
    /// envelope requests, a dead lane is a transport error — the
    /// caller owns the policy mapping (the binary fails policy open:
    /// run/keep), while a death *during* the await resolves through
    /// the lane's drain with the same fallback.
    pub async fn hook(
        &self,
        event: &str,
        payload: serde_json::Value,
        services: Option<Arc<dyn HostServices>>,
        cancel: tokio_util::sync::CancellationToken,
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
                services,
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
        // Cancellation resolves the policy FAIL OPEN (the ruling's
        // symmetry: a hook the host gave up on is treated as
        // absence, the neutral decision for its point) while telling
        // the guest to stop — a wedged policy extension must not
        // outlive the run it was gating.
        let outcome = tokio::select! {
            decision = rx => decision,
            _ = cancel.cancelled() => {
                self.cancel(&hook_id);
                return Ok(fallback);
            }
        };
        match outcome {
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

/// Launch every extension found under `root` — scan plus [`launch`],
/// the everything-found boot (the host's own tests; the binary
/// partitions out the disabled first, so it calls [`launch`] on its
/// own scan).
pub fn launch_root(
    root: &Path,
    handshake_timeout: Duration,
) -> (
    Supervisor,
    tokio::sync::mpsc::UnboundedReceiver<ExtensionEvent>,
) {
    launch(manifest::scan(root), handshake_timeout)
}

/// Launch a scan's findings: spawn and supervise each package, report
/// each refusal — each in its own task, so a mute extension's
/// handshake timeout never delays the healthy ones. Returns the
/// supervisor plus the event channel; dropping the receiver loses
/// later reports to nowhere, so the binary drains it for its log.
/// Must run on the runtime the binary serves from (it spawns).
pub fn launch(
    found: Vec<Discovered>,
    handshake_timeout: Duration,
) -> (
    Supervisor,
    tokio::sync::mpsc::UnboundedReceiver<ExtensionEvent>,
) {
    let closing = CancellationToken::new();
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut children = Vec::new();
    for found in found {
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
    /// An empty supervisor — print mode's assembly holds one so every
    /// consumer has a single shape (real hosts get a supervisor over
    /// their scan, even when it finds nothing).
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
    supervisor_closing: CancellationToken,
    state: Arc<ChildState>,
    lane: Arc<Lane>,
    events: tokio::sync::mpsc::UnboundedSender<ExtensionEvent>,
    command_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    // A child of the supervisor's token: this extension's failure
    // closes only its own pipe (fail_before_ack cancels this one),
    // while host shutdown cascades through the hierarchy to every
    // child. One shared token here was the fleet-kill bug — one
    // broken package's pre-ack failure tore down every healthy
    // sibling, silently (the review round's top finding).
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

    // The command writer: lines in, stdin out (the shared pipe
    // contract — the closing token IS the pipe close).
    spawn_command_writer(stdin, command_rx, closing.clone());

    // The frame reader: handshake outcome first, then the tool lane
    // (results resolved, asks lifted). Compatibility is one-directional
    // (ruled 2026-09): a NEWER host keeps an older extension working
    // (the host sends only what the extension declared; the extension
    // side is told to ignore frames it does not know), but an
    // extension speaking vocabulary its host lacks — an unparseable
    // line, an unknown frame type, a result answering the wrong kind
    // of correlation — is a contract break: death, with the snippet.
    // An extension built for a later host must be refused, not run
    // half-working; additions the EXTENSION can emit ride the
    // protocol version so older hosts refuse at the handshake's
    // exact match instead of mid-stream.
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
                        let entry = tabit_log::lock::lock(&lane.pending).remove(&result.call_id);
                        match entry {
                            Some(PendingCall {
                                waiter: Waiter::ToolCall(result_tx),
                                ..
                            }) => {
                                let _ = result_tx.send(result);
                            }
                            Some(_) => {
                                refuse(
                                    &mut handshake_tx,
                                    &mut death_tx,
                                    format!(
                                        "sent a tool result for a non-call id `{}`",
                                        result.call_id
                                    ),
                                );
                                break;
                            }
                            // An answer to an already-gone call (the
                            // asker detached): dropped, not a fault.
                            None => {}
                        }
                    }
                    Ok(ExtFrame::HookResult(result)) => {
                        let entry = tabit_log::lock::lock(&lane.pending).remove(&result.hook_id);
                        match entry {
                            Some(PendingCall {
                                waiter:
                                    Waiter::Hook {
                                        result: result_tx, ..
                                    },
                                ..
                            }) => {
                                let _ = result_tx.send(result.decision);
                            }
                            Some(_) => {
                                refuse(
                                    &mut handshake_tx,
                                    &mut death_tx,
                                    format!(
                                        "sent a hook result for a non-hook id `{}`",
                                        result.hook_id
                                    ),
                                );
                                break;
                            }
                            None => {}
                        }
                    }
                    Ok(ExtFrame::ServiceRequest {
                        request_id,
                        call_id,
                        verb,
                    }) => {
                        dispatch_service(lane.clone(), request_id, call_id, verb);
                    }
                    Err(_) => {
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

    // The handshake, bounded. Serialization here is pure data over a
    // serde type — the impossible failure is the sanctioned crash,
    // never a silent empty line (which the guest would only see as a
    // broken contract).
    #[allow(clippy::expect_used)] // sanctioned crash: pure-data serialization
    let initialize = serde_json::to_string(&HostFrame::Initialize {
        protocol_version: EXTENSION_PROTOCOL_VERSION,
    })
    .expect("HostFrame::Initialize always serializes");
    let _ = lane.commands.send(initialize);
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
    resolve_dead(&state, &lane, &events, &manifest.name, reason);
}

/// The envelope dispatcher: route one extension service request to
/// the session whose call or hook is in flight (the pending entry's
/// capability) and carry the answer back down the pipe. The verbs
/// are fixed (the task-5 ruling); `ask` is verb zero — the hub's
/// existing lift — and `model_prompt` is verb one, billed through
/// the same capability. No capability on the pending entry (a
/// non-interactive session, or the call already gone) answers the
/// ask dismissed and every other verb with an error — fail closed,
/// exactly as core tools behave.
fn dispatch_service(lane: Arc<Lane>, request_id: String, call_id: String, verb: ServiceVerb) {
    tokio::spawn(async move {
        let services = tabit_log::lock::lock(&lane.pending)
            .get(&call_id)
            .and_then(|pending| pending.services.clone());
        let response = match verb {
            ServiceVerb::Ask { ui_type, payload } => {
                // Verb zero: the lift never contains a core panic
                // (ruled 2026-09 — the future is core's code, and the
                // crash hook owns core panics).
                let outcome = match services {
                    Some(services) => services.ask(&ui_type, payload).await,
                    None => InteractionOutcome::Dismissed,
                };
                HostFrame::ServiceResponse {
                    request_id,
                    result: match outcome {
                        InteractionOutcome::Answered(answer) => Some(answer),
                        InteractionOutcome::Dismissed => None,
                    },
                    error: None,
                }
            }
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
        // Pure data over a serde type — the impossible failure is the
        // sanctioned crash, never a silent empty line.
        #[allow(clippy::expect_used)] // sanctioned crash: pure-data serialization
        let frame = serde_json::to_string(&response).expect("HostFrame always serializes");
        let _ = lane.commands.send(frame);
    });
}

/// What the reader decided about the handshake.
enum Handshake {
    Acked(Ack),
    Failed(String),
}

/// One contract break spotted by the reader: before the ack it fails
/// the handshake; after it, it is the death signal.
fn refuse(
    handshake_tx: &mut Option<tokio::sync::oneshot::Sender<Handshake>>,
    death_tx: &mut Option<tokio::sync::oneshot::Sender<String>>,
    refusal: String,
) {
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
    process::kill_now(process, closing).await;
    resolve_dead(state, lane, events, name, reason);
}

/// The post-ack close lives in [`crate::process::reap_with_grace`]
/// (the shared pipe contract).

fn resolve_dead(
    state: &Arc<ChildState>,
    lane: &Arc<Lane>,
    events: &tokio::sync::mpsc::UnboundedSender<ExtensionEvent>,
    name: &str,
    reason: String,
) {
    lane.die("the extension is not running");
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
