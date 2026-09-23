//! The frontend-role client: spawn a tabit-core process in `--json`
//! child role and speak the frozen wire to it. This is the runtime
//! every child-driver shares — the subagent bridge (tabit-session)
//! and the extension SDK's owned-session wrapper (the sharing ruling
//! 2026-09; before this, the bridge hand-rolled its own). The GUI
//! still carries a deliberate sync twin of this runtime (it runs no
//! tokio; the twin lacks the bounded handshake and the grace reaper)
//! — a known, paused-frontend exception, not the rule: when the GUI
//! revives, it either adopts a sync core extracted here or the twin
//! goes.
//!
//! What lives here, precisely:
//!
//! - **The child-role knobs** ([`ChildSpec`]): the CLI flags that
//!   shape a child (parent identity, model, tool allow/deny lists,
//!   budget, preamble, extension root, ephemeral-vs-resume). The
//!   flags are the wire-level contract of tabit-core's child role —
//!   one builder so no driver drifts from the CLI it drives.
//! **One mechanism, policies above it.** A node's child frames fan
//! to local consumers and upstream relay through the pump's tap,
//! with the settle fold watching the same stream — the child-
//! management pattern every driver shares. The drivers differ only
//! in the policy they wire into the tap: core's bridge (learn +
//! relay always on, the fold takes the terminal) and the SDK's
//! wrapper (registered handlers, relay opt-in).
//!
//! - **The runtime** ([`ChildSpec::spawn`]): wrap-and-spawn with the
//!   process cwd (the OS enforces the scope), the command writer
//!   whose close is the stdin drop, the stderr ring, the bounded
//!   `Initialize` handshake, and the frame pump — control frames
//!   resolve the handshake and die as diagnostics after it; stamped
//!   frames cross to the caller **as-is** (forward, don't re-stamp)
//!   and, when a tap is set, reach it in pump order.
//! - **The handle** ([`ChildHandle`]): commands out, frames in, the
//!   closing token, the exit machinery. The drive fold — mapping a
//!   child's run to the driver's own terminal vocabulary — is the
//!   consumer's policy and stays with it.
//!
//! Session machinery is deliberately absent: the router taps
//! ([`ChildSpec::on_stamped_frame`], [`ChildSpec::on_exit`]) are the
//! seams the bridge hangs its table-keeping on; a driver with no
//! routing (the SDK's owned children) sets neither.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tabit_log::lock::lock;
use tabit_protocol::{
    ClientFrame, EventFrame, ModelSelection, PROTOCOL_VERSION, ServerControlFrame, ServerFrame,
    SessionCommand, SessionEvent, StreamId,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::process::{
    HANDSHAKE_TIMEOUT, crash_tail, kill_now, reap_with_grace, spawn_command_writer,
    spawn_stderr_ring, wrap_command,
};

/// Sees one stamped frame in pump order, with the speaking child's
/// id (the bridge's forward-and-learn seam).
pub type StampedFrameTap = Arc<dyn Fn(&str, &EventFrame) + Send + Sync>;

/// Learns the child's exit, after reclamation (the router's
/// unregister seam).
pub type ExitTap = Arc<dyn Fn(&str) + Send + Sync>;

/// Shape one subprocess child before the spawn: the child-role CLI
/// knobs as builder methods. Everything omitted inherits the default
/// (ephemeral, the given cwd).
pub struct ChildSpec {
    exe: PathBuf,
    cwd: PathBuf,
    parent: Option<String>,
    parent_call: Option<String>,
    model: Option<ModelSelection>,
    tools: Option<Vec<String>>,
    without: Option<Vec<String>>,
    ephemeral: bool,
    session: Option<PathBuf>,
    extensions: Option<PathBuf>,
    max_turns: Option<usize>,
    /// The child's preamble — replaces the default base text while
    /// the environment block, AGENTS.md files, and skills catalog
    /// append as usual. The child's preamble belongs to its spawner.
    preamble: Option<String>,
    on_stamped_frame: Option<StampedFrameTap>,
    on_exit: Option<ExitTap>,
}

impl ChildSpec {
    /// Begin a child of the given executable, running in `cwd` (the
    /// process cwd — the OS enforces the scope every tool and path
    /// inside resolves against).
    pub fn new(exe: PathBuf, cwd: PathBuf) -> Self {
        Self {
            exe,
            cwd,
            parent: None,
            parent_call: None,
            model: None,
            tools: None,
            without: None,
            ephemeral: true,
            session: None,
            extensions: None,
            max_turns: None,
            preamble: None,
            on_stamped_frame: None,
            on_exit: None,
        }
    }

    /// The child's working directory — the process cwd; every tool
    /// and path inside resolves against it by OS fact.
    pub fn cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = cwd;
        self
    }

    /// The child's parent session id — crosses as `--parent` so the
    /// child announces its lineage at the source of truth. Absent for
    /// spawners that are not sessions (an extension's owned child).
    pub fn parent(mut self, id: String) -> Self {
        self.parent = Some(id);
        self
    }

    /// The spawning tool call's correlation id — crosses as
    /// `--parent-call` so the child's `session_opened` announce pairs
    /// with the `ToolCall` event the frontend already holds.
    pub fn parent_call(mut self, id: String) -> Self {
        self.parent_call = Some(id);
        self
    }

    /// The child's model selection (`provider/model` crosses as the
    /// `--model` ref; the thinking level is the child config's).
    pub fn model(mut self, selection: ModelSelection) -> Self {
        self.model = Some(selection);
        self
    }

    /// Restrict the child's toolset to these names; an unknown name
    /// fails the child loudly at startup.
    pub fn tools(mut self, names: Vec<String>) -> Self {
        self.tools = Some(names);
        self
    }

    /// Tools the child must NOT run — the deny twin of
    /// [`ChildSpec::tools`], crossing as `--without` and applied
    /// child-side over the full toolset (core and extension proxies
    /// alike).
    pub fn without(mut self, names: Vec<String>) -> Self {
        self.without = Some(names);
        self
    }

    /// A persisted child: an ordinary session file under the child's
    /// cwd, resumable through `--session` like any other. The default
    /// (and this flag's opposite) is ephemeral.
    pub fn ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = ephemeral;
        self
    }

    /// Resume the stored session at `path` instead of starting fresh
    /// (implies persisted; [`ChildSpec::ephemeral`] is ignored).
    pub fn session(mut self, path: PathBuf) -> Self {
        self.session = Some(path);
        self
    }

    /// The child's extension root, crossing as `--extensions` — the
    /// child boots its own host against it.
    pub fn extensions(mut self, path: PathBuf) -> Self {
        self.extensions = Some(path);
        self
    }

    /// The per-child model-call budget.
    pub fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    /// The child's preamble — crosses as `--preamble` and replaces
    /// the default base (identity and standing body). The spawner
    /// owns the child's voice; tabit still owns the truthful context.
    pub fn preamble(mut self, text: String) -> Self {
        self.preamble = Some(text);
        self
    }

    /// The pump-order tap for stamped frames (the bridge's
    /// forward-and-learn seam). Frames cross to the handle's channel
    /// either way; the tap is the extra, ordered look.
    pub fn on_stamped_frame(mut self, tap: StampedFrameTap) -> Self {
        self.on_stamped_frame = Some(tap);
        self
    }

    /// The exit tap (the router's unregister seam), invoked after the
    /// reaper finished with the process.
    pub fn on_exit(mut self, tap: ExitTap) -> Self {
        self.on_exit = Some(tap);
        self
    }

    /// Run the child: spawn, handshake, reaper. Errors are display
    /// strings — the caller (a tool body, an SDK wrapper) turns them
    /// into its failure report.
    pub async fn spawn(self) -> Result<ChildHandle, String> {
        let Self {
            exe,
            cwd,
            parent,
            parent_call,
            model,
            tools,
            without,
            ephemeral,
            session,
            extensions,
            max_turns,
            preamble,
            on_stamped_frame,
            on_exit,
        } = self;

        let mut args: Vec<String> = vec!["--json".to_string()];
        if let Some(id) = &parent {
            args.push("--parent".to_string());
            args.push(id.clone());
        }
        if let Some(id) = &parent_call {
            args.push("--parent-call".to_string());
            args.push(id.clone());
        }
        if let Some(selection) = &model {
            args.push("--model".to_string());
            args.push(format!("{}/{}", selection.provider, selection.model));
        }
        if let Some(max_turns) = max_turns {
            args.push("--max-turns".to_string());
            args.push(max_turns.to_string());
        }
        if let Some(tools) = &tools {
            args.push("--tools".to_string());
            args.push(tools.join(","));
        }
        if let Some(without) = &without {
            args.push("--without".to_string());
            args.push(without.join(","));
        }
        if let Some(text) = &preamble {
            args.push("--preamble".to_string());
            args.push(text.clone());
        }
        if let Some(path) = &extensions {
            args.push("--extensions".to_string());
            args.push(path.display().to_string());
        }
        if let Some(path) = &session {
            args.push("--session".to_string());
            args.push(path.display().to_string());
        } else if ephemeral {
            args.push("--ephemeral".to_string());
        }

        let mut process = wrap_command(&exe, &args, &cwd)
            .spawn()
            .map_err(|error| format!("cannot spawn the child `{}`: {error}", exe.display()))?;
        let stdin = process
            .stdin()
            .take()
            .ok_or("the child process opened no stdin")?;
        let stdout = process
            .stdout()
            .take()
            .ok_or("the child process opened no stdout")?;
        let stderr = process
            .stderr()
            .take()
            .ok_or("the child process opened no stderr")?;

        // The closing token: the child's shutdown signal, shared by the
        // stdin writer (the pipe drop) and the reaper (the grace
        // timer). Cancelling it IS the close.
        let closing = CancellationToken::new();

        // The command writer: lines in, stdin out — the shared pipe
        // contract: the closing token IS the stdin close (the driver
        // holds a sender clone, so dropping senders cannot be the
        // mechanism); on close, everything already queued (the abort
        // line crossed first) is written, then the pipe drops — EOF,
        // the child's death contract.
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        spawn_command_writer(stdin, command_rx, closing.clone());

        // The stderr ring — the crash report's tail.
        let ring = spawn_stderr_ring(stderr);

        // The frame pump: handshake frames resolve here, stamped
        // frames cross as-is (their stream stamps are already their
        // session ids) and reach the tap in pump order, everything
        // mirrored to the frames channel.
        let (frame_tx, frame_rx) = tokio::sync::mpsc::unbounded_channel::<EventFrame>();
        let (handshake_tx, handshake_rx) = tokio::sync::oneshot::channel::<Handshake>();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            let mut child_id: Option<String> = None;
            let mut handshake_tx = Some(handshake_tx);
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(frame) = serde_json::from_str::<ServerFrame>(&line) else {
                    continue;
                };
                match frame {
                    ServerFrame::Control(control) => {
                        if let Some(tx) = handshake_tx.take() {
                            let outcome = match &control {
                                ServerControlFrame::InitializeAck { session_id, .. } => {
                                    child_id = Some(session_id.clone());
                                    Handshake::Acked(session_id.clone())
                                }
                                ServerControlFrame::InitializeRejected { reason } => {
                                    Handshake::Rejected(reason.clone())
                                }
                                ServerControlFrame::ProtocolError { message } => {
                                    Handshake::Rejected(message.clone())
                                }
                            };
                            let _ = tx.send(outcome);
                        }
                        // Post-handshake control frames from a child
                        // (its protocol errors) are its own diagnostics
                        // — consumed here, never forwarded.
                    }
                    ServerFrame::Event(frame) => {
                        if let (Some(tap), Some(id)) = (&on_stamped_frame, &child_id) {
                            tap(id, &frame);
                        }
                        let _ = frame_tx.send(frame);
                    }
                }
            }
        });

        // Send the handshake and await the child's answer, bounded.
        let _ = command_tx.send(tabit_protocol::to_wire_line(&ClientFrame::Initialize {
            protocol_version: PROTOCOL_VERSION,
            replay: false,
        }));
        let handshake = tokio::select! {
            outcome = handshake_rx => {
                outcome.map_err(|_| "the child process closed before the handshake".to_string())?
            }
            _ = tokio::time::sleep(HANDSHAKE_TIMEOUT) => {
                kill_now(&mut process, &closing).await;
                return Err("the child process did not answer the handshake".to_string());
            }
        };
        let child_id = match handshake {
            Handshake::Acked(id) => id,
            Handshake::Rejected(reason) => {
                kill_now(&mut process, &closing).await;
                return Err(format!(
                    "the child process rejected the handshake: {reason}"
                ));
            }
        };

        // The reaper that bounds the child's lifetime (Drop of the
        // handle closes it): a natural exit reaps itself; the close
        // path gets the grace-then-tree-kill.
        let child_id_for_exit = child_id.clone();
        let closing_for_reaper = closing.clone();
        let exit = Arc::new(Mutex::new(None::<String>));
        let exit_for_reaper = exit.clone();
        let join = tokio::spawn(async move {
            let status = tokio::select! {
                status = process.wait() => Some(status),
                _ = closing_for_reaper.cancelled() => None,
            };
            let status: Option<std::process::ExitStatus> = match status {
                Some(result) => result.ok(),
                None => reap_with_grace(&mut process).await,
            };
            if let Some(status) = status {
                *lock(&exit_for_reaper) =
                    Some(format!("exit code {}", status.code().unwrap_or(-1)));
            }
            if let Some(tap) = &on_exit {
                tap(&child_id_for_exit);
            }
        });

        Ok(ChildHandle {
            id: child_id.clone(),
            stream: StreamId::new(child_id),
            commands: command_tx,
            frames: frame_rx,
            closing,
            reaper: join,
            exit,
            stderr_ring: ring,
        })
    }
}

/// What the child answered at the handshake.
enum Handshake {
    Acked(String),
    Rejected(String),
}

/// One live subprocess child: the driver's surface. Commands go out
/// as wire lines; stamped frames arrive on the channel; dropping the
/// handle closes the child (stdin EOF, bounded by the reaper's tree
/// kill). The drive fold is the driver's — this type carries the
/// machinery, not the policy.
pub struct ChildHandle {
    id: String,
    stream: StreamId,
    commands: tokio::sync::mpsc::UnboundedSender<String>,
    frames: tokio::sync::mpsc::UnboundedReceiver<EventFrame>,
    closing: CancellationToken,
    reaper: tokio::task::JoinHandle<()>,
    exit: Arc<Mutex<Option<String>>>,
    stderr_ring: Arc<Mutex<VecDeque<String>>>,
}

impl ChildHandle {
    /// The child session's id — its stream stamp and (where a router
    /// exists) its routing address.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The child's stream stamp (its session id).
    pub fn stream(&self) -> &StreamId {
        &self.stream
    }

    /// One wire line out (a serialized command).
    pub fn send_line(&self, line: String) {
        let _ = self.commands.send(line);
    }

    /// The command sender (a router's delivery lane — the bridge's
    /// table keeps one per registered child).
    pub fn commands(&self) -> tokio::sync::mpsc::UnboundedSender<String> {
        self.commands.clone()
    }

    /// The frames channel — the drive fold consumes here.
    pub fn frames(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<EventFrame> {
        &mut self.frames
    }

    /// Begin the child's shutdown (idempotent): stdin closes, the
    /// reaper's grace timer arms.
    pub fn close(&self) {
        self.closing.cancel();
    }

    /// The crash report: the exit status and the stderr tail.
    pub fn crash_report(&self) -> String {
        let exit = lock(&self.exit)
            .clone()
            .unwrap_or_else(|| "no exit recorded".to_string());
        let tail = crash_tail(&self.stderr_ring);
        if tail.is_empty() {
            format!("the child process died unexpectedly ({exit})")
        } else {
            format!("the child process died unexpectedly ({exit}); stderr tail:\n{tail}")
        }
    }

    /// Wait for the reaper to finish (tests and callers that want the
    /// process fully reclaimed).
    pub async fn wait_exit(&mut self) {
        let _ = (&mut self.reaper).await;
    }

    /// Submit the child's task (one user message) — the first half of
    /// [`Self::settle`]'s recipe, split so a driver can steer between
    /// them.
    pub fn prompt(&self, task: String) {
        self.send_line(tabit_protocol::to_wire_line(&SessionCommand::Message {
            session: self.id.clone(),
            text: task,
        }));
    }

    /// Drive the child to its run terminal under the abort leash —
    /// THE fold every driver shares (core's subagent tool and the
    /// extension SDK's owned children alike; one implementation, the
    /// Nth-fold law). The terminal scan over this child's stream
    /// (grandchildren's frames skip — their owners forward them),
    /// the crash synthesis, and the abort courtesy-with-deadline all
    /// live here; mapping the settlement to the driver's own
    /// vocabulary is the caller's policy.
    pub async fn settle(&mut self, token: Option<CancellationToken>) -> Settlement {
        self.settle_with_tap(token, |_| {}).await
    }

    /// [`Self::settle`] with a per-frame tap — every frame of this
    /// child's run reaches the tap (before the fold's own handling)
    /// so a driver's event subscribers and THE one fold share the
    /// stream instead of racing two readers over it.
    pub async fn settle_with_tap(
        &mut self,
        token: Option<CancellationToken>,
        tap: impl Fn(&EventFrame),
    ) -> Settlement {
        let mut events: Vec<SessionEvent> = Vec::new();
        let started_at_ms = unix_ms();
        loop {
            let cancelled = async {
                match &token {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = cancelled => {
                    // Abort is a courtesy with a deadline: forward the
                    // abort, close stdin (the child aborts, flushes,
                    // exits — or the reaper kills the tree at the
                    // grace), and report Aborted now. The driver never
                    // waits on the child's cooperation.
                    self.send_line(tabit_protocol::to_wire_line(&SessionCommand::Abort {
                        session: self.id.clone(),
                    }));
                    self.close();
                    return Settlement::Aborted {
                        output: String::new(),
                        events,
                    };
                }
                frame = self.frames.recv() => {
                    let Some(frame) = frame else {
                        // The stream ended without a terminal: the child
                        // process died. The crash report carries the
                        // exit status and the stderr tail, shaped as the
                        // run-failed event the drivers already keep.
                        events.push(SessionEvent::RunFailed {
                            message: self.crash_report(),
                            kind: tabit_protocol::RunFailedKind::ENGINE.to_string(),
                            started_at_ms,
                            completed_at_ms: unix_ms(),
                        });
                        return Settlement::Crashed { events };
                    };
                    tap(&frame);
                    if frame.stream.as_ref() != Some(&self.stream) {
                        continue; // A grandchild's frame — already forwarded.
                    }
                    let event = frame.event;
                    let terminal = match &event {
                        SessionEvent::RunFinished { output, .. } => {
                            Some((Terminal::Completed, output.clone()))
                        }
                        SessionEvent::RunAborted { output, .. } => {
                            Some((Terminal::Aborted, output.clone()))
                        }
                        SessionEvent::RunFailed { message, .. } => {
                            Some((Terminal::Failed, message.clone()))
                        }
                        _ => None,
                    };
                    events.push(event);
                    if let Some((terminal, text)) = terminal {
                        self.close();
                        return match terminal {
                            Terminal::Completed => Settlement::Completed {
                                output: text,
                                events,
                            },
                            Terminal::Aborted => Settlement::Aborted {
                                output: text,
                                events,
                            },
                            Terminal::Failed => Settlement::FailedWith {
                                message: text,
                                events,
                            },
                        };
                    }
                }
            }
        }
    }
}

/// Which terminal the driven child's run reached — the fold's
/// private discriminator.
enum Terminal {
    Completed,
    Aborted,
    Failed,
}

/// How a driven child's run ended — the wire-level settlement the
/// drivers map to their own vocabularies.
#[derive(Debug, Clone)]
pub enum Settlement {
    /// The run finished; `output` is the final answer.
    Completed {
        output: String,
        events: Vec<SessionEvent>,
    },
    /// The run aborted (the leash fired, or the child aborted
    /// itself); `output` is whatever partial text it produced.
    Aborted {
        output: String,
        events: Vec<SessionEvent>,
    },
    /// The run failed; `message` is the failure.
    FailedWith {
        message: String,
        events: Vec<SessionEvent>,
    },
    /// The child process died without a terminal (a run-failed event
    /// with the crash report heads `events`).
    Crashed { events: Vec<SessionEvent> },
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        // The commands sender drops with the struct; the closing token
        // arms the reaper either way. Nothing async here — the reaper
        // owns the wait.
        self.close();
    }
}
