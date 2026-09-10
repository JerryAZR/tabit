//! The subprocess bridge: the second execution substrate's parent
//! half (ROADMAP item 5 — a first-class substrate, not a fallback).
//!
//! A subprocess child is the tabit binary itself in `--json` child
//! role (`--parent`, `--tools`, `--ephemeral`/`--session`), spawned
//! with the child's cwd as the **process** cwd — the OS enforces the
//! scope every tool, extension, and path resolves against, instead of
//! a convention each tool author must follow. The bridge acts as the
//! child's frontend over the frozen stdio edge:
//!
//! - **forward, don't re-stamp**: the child's stamped frames already
//!   carry the child's session id as their stream stamp; they cross
//!   to the real frontend as-is. The child's backend-level frames
//!   (the handshake, its catalog, its unstamped errors) are consumed
//!   here — they would collide with the parent's connection-level
//!   fold.
//! - **learning** (the Ethernet-switch model): every forwarded frame
//!   teaches the router which child subtree owns its stamp, so a
//!   command addressed to a grandchild walks hop by hop — see
//!   [`crate::routing`].
//! - **abort is a courtesy with a deadline** (owner ruling 2026-09):
//!   on the leash's cancel the bridge forwards `abort`, closes stdin
//!   (the death contract — the child aborts, flushes, and exits on
//!   its own), and returns `Aborted` immediately; a reaper bounds the
//!   child's exit with the tree kill (the Job Object / process group
//!   takes the child's bash descendants with it). The graceful path
//!   buys the write-behind flush for persisted children; it never
//!   buys the parent's latency.

use crate::subagent::SpawnContext;
use rig_agent::completion::Message;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tabit_protocol::{
    ClientFrame, EventFrame, ModelSelection, PROTOCOL_VERSION, ServerFrame, SessionCommand,
    SessionEvent, StreamId,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

/// How long a closing child gets to exit on its own before the tree
/// kill — ample for the write-behind flush on a healthy disk, and
/// exactly the pathological cases (a wedged tool body, a stalled
/// flush) burn it.
const REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// The handshake window: a child that cannot acknowledge `initialize`
/// in this time is dead on arrival — killed at spawn, loudly.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The stderr ring's depth — the crash report's tail.
const STDERR_RING: usize = 200;

/// Shapes one subprocess child before the spawn: the child-role flags
/// as builder knobs. Everything omitted inherits the default
/// (ephemeral, the parent's cwd).
pub struct SubprocessBuilder {
    exe: PathBuf,
    parent_id: String,
    parent_call: Option<String>,
    cwd: PathBuf,
    model: Option<ModelSelection>,
    tools: Option<Vec<String>>,
    ephemeral: bool,
    session: Option<PathBuf>,
    max_turns: Option<usize>,
    router: Arc<crate::routing::ChildRouter>,
    notice: Option<crate::notice::NoticeSink>,
}

impl SubprocessBuilder {
    /// Begin from a spawner's context — the exe, the parent identity,
    /// the shared router, and the weak frontend handle all come from
    /// the assembly's parts.
    pub fn new(ctx: &SpawnContext) -> Self {
        Self {
            exe: ctx.parts().exe.clone(),
            parent_id: ctx.parent_id().to_string(),
            parent_call: None,
            cwd: ctx.parent_cwd().to_path_buf(),
            model: None,
            tools: None,
            ephemeral: true,
            session: None,
            max_turns: None,
            router: ctx.parts().router.clone(),
            notice: ctx.notice(),
        }
    }

    /// The child's working directory — the process cwd; every tool
    /// and path inside resolves against it by OS fact.
    pub fn cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = cwd;
        self
    }

    /// The child's model selection (`provider/model` crosses as the
    /// `--model` ref; the thinking level is the child config's).
    pub fn model(mut self, selection: ModelSelection) -> Self {
        self.model = Some(selection);
        self
    }

    /// Restrict the child's toolset to these names (the child's own
    /// default toolset already excludes the subagent tool — recursion
    /// by omission); an unknown name fails the child loudly at
    /// startup.
    pub fn tools(mut self, names: Vec<String>) -> Self {
        self.tools = Some(names);
        self
    }

    /// A persisted child: an ordinary session file under the child's
    /// cwd, resumable through `open_session` like any other. The
    /// default (and this flag's opposite) is ephemeral.
    pub fn ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = ephemeral;
        self
    }

    /// The per-child model-call budget.
    pub fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    /// The spawning tool call's correlation id — crosses as
    /// `--parent-call` so the child's `session_opened` announce pairs
    /// with the `ToolCall` event the frontend already holds (exact
    /// under concurrent subagent calls). Absent for spawners outside
    /// a model turn.
    pub fn parent_call(mut self, id: String) -> Self {
        self.parent_call = Some(id);
        self
    }

    /// Run the child: spawn, handshake, registration. Errors are
    /// display strings — the caller (a tool body) turns them into its
    /// failure report.
    pub async fn spawn(self) -> Result<SubprocessChild, String> {
        let Self {
            exe,
            parent_id,
            parent_call,
            cwd,
            model,
            tools,
            ephemeral,
            session,
            max_turns,
            router,
            notice,
        } = self;

        let mut args: Vec<String> = vec![
            "--json".to_string(),
            "--parent".to_string(),
            parent_id.clone(),
        ];
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
        if let Some(path) = &session {
            args.push("--session".to_string());
            args.push(path.display().to_string());
        } else if ephemeral {
            args.push("--ephemeral".to_string());
        }

        let mut process = wrap_command(&exe, &args, &cwd).spawn().map_err(|error| {
            format!(
                "cannot spawn the subagent process `{}`: {error}",
                exe.display()
            )
        })?;
        let stdin = process
            .stdin()
            .take()
            .ok_or("the subagent process opened no stdin")?;
        let stdout = process
            .stdout()
            .take()
            .ok_or("the subagent process opened no stdout")?;
        let stderr = process
            .stderr()
            .take()
            .ok_or("the subagent process opened no stderr")?;

        // The closing token: the child's shutdown signal, shared by the
        // stdin writer (the pipe drop) and the reaper (the grace
        // timer). Cancelling it IS the close.
        let closing = CancellationToken::new();

        // The command writer: lines in, stdin out. The closing token IS
        // the stdin close (the drive holds a sender clone, so dropping
        // senders cannot be the mechanism): on close, everything
        // already queued (the abort line crossed first) is written,
        // then the pipe drops — EOF, the child's death contract.
        let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let writer_closing = closing.clone();
        let mut stdin = stdin;
        tokio::spawn(async move {
            loop {
                let line = tokio::select! {
                    _ = writer_closing.cancelled() => {
                        // Deliver what the close raced (the abort line
                        // sent before the close, still queued), then
                        // drop the pipe.
                        while let Ok(line) = command_rx.try_recv() {
                            if stdin.write_all(line.as_bytes()).await.is_err()
                                || stdin.write_all(b"\n").await.is_err()
                            {
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
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                {
                    break;
                }
                let _ = stdin.flush().await;
            }
            // Drop closes the pipe: the child's death contract.
        });

        // The stderr ring — the crash report's tail.
        let ring = Arc::new(Mutex::new(VecDeque::<String>::new()));
        {
            let ring = ring.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut ring = crate::lock::lock(&ring);
                    if ring.len() == STDERR_RING {
                        ring.pop_front();
                    }
                    ring.push_back(line);
                }
            });
        }

        // The frame pump: handshake frames consumed here, stamped
        // frames forwarded as-is and learned, everything mirrored to
        // the drive channel.
        let (frame_tx, frame_rx) = tokio::sync::mpsc::unbounded_channel::<EventFrame>();
        let (handshake_tx, handshake_rx) = tokio::sync::oneshot::channel::<Handshake>();
        let pump_router = router.clone();
        let pump_notice = notice;
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
                                tabit_protocol::ServerControlFrame::InitializeAck {
                                    session_id,
                                    ..
                                } => {
                                    child_id = Some(session_id.clone());
                                    Handshake::Acked(session_id.clone())
                                }
                                tabit_protocol::ServerControlFrame::InitializeRejected {
                                    reason,
                                } => Handshake::Rejected(reason.clone()),
                                tabit_protocol::ServerControlFrame::ProtocolError { message } => {
                                    Handshake::Rejected(message.clone())
                                }
                            };
                            let _ = tx.send(outcome);
                        }
                        // Post-handshake control frames from a child
                        // (its protocol errors) are its own diagnostics
                        // — logged, never forwarded.
                    }
                    ServerFrame::Event(frame) => {
                        // Forward as-is (the child's stamps are already
                        // its session ids) and learn: a frame's stamp
                        // teaches which subtree owns the id.
                        if let (Some(notice), Some(id)) = (&pump_notice, &child_id)
                            && let Some(stream) = &frame.stream
                        {
                            pump_router.learn(stream.as_str(), id);
                            notice.forward(frame.clone());
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
                outcome.map_err(|_| "the subagent process closed before the handshake".to_string())?
            }
            _ = tokio::time::sleep(HANDSHAKE_TIMEOUT) => {
                return Err("the subagent process did not answer the handshake".to_string());
            }
        };
        let child_id = match handshake {
            Handshake::Acked(id) => id,
            Handshake::Rejected(reason) => {
                return Err(format!(
                    "the subagent process rejected the handshake: {reason}"
                ));
            }
        };

        // Registration: routing's Process entry, and the reaper that
        // bounds the child's lifetime (Drop of this struct closes it).
        let closing_for_reaper = closing.clone();
        let reaper_router = router.clone();
        let reaper_id = child_id.clone();
        let exit = Arc::new(Mutex::new(None::<String>));
        let exit_for_reaper = exit.clone();
        let join = tokio::spawn(async move {
            let status = tokio::select! {
                status = process.wait() => Some(status),
                _ = closing_for_reaper.cancelled() => None,
            };
            let status: Option<std::process::ExitStatus> = match status {
                Some(result) => result.ok(),
                None => match tokio::time::timeout(REAP_GRACE, process.wait()).await {
                    Ok(result) => result.ok(),
                    Err(_) => {
                        // The grace burned: the tree kill (Job Object /
                        // process group takes the descendants too).
                        let _ = Box::into_pin(process.kill()).await;
                        process.wait().await.ok()
                    }
                },
            };
            if let Some(status) = status {
                *crate::lock::lock(&exit_for_reaper) =
                    Some(format!("exit code {}", status.code().unwrap_or(-1)));
            }
            reaper_router.unregister(&reaper_id);
        });
        router.register(&child_id, command_tx.clone());

        Ok(SubprocessChild {
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

/// One live subprocess child: the drive surface. Dropping it closes
/// the child (stdin EOF, bounded by the reaper's tree kill).
pub struct SubprocessChild {
    id: String,
    stream: StreamId,
    commands: tokio::sync::mpsc::UnboundedSender<String>,
    frames: tokio::sync::mpsc::UnboundedReceiver<EventFrame>,
    closing: CancellationToken,
    reaper: tokio::task::JoinHandle<()>,
    exit: Arc<Mutex<Option<String>>>,
    stderr_ring: Arc<Mutex<VecDeque<String>>>,
}

impl SubprocessChild {
    /// The child session's id — its stream stamp and routing address.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Send the task and drive to the child's terminal under the
    /// leash — [`SpawnContext::drive_subprocess`]'s body. The child
    /// announces itself (its `--parent` flag spoke at the source);
    /// its frames are already on the frontend's channel.
    pub(crate) async fn drive(
        &mut self,
        task: Message,
        token: Option<CancellationToken>,
    ) -> crate::session::RunSummary {
        let text = message_text(&task);
        let _ = self
            .commands
            .send(tabit_protocol::to_wire_line(&SessionCommand::Message {
                session: self.id.clone(),
                text,
            }));

        let mut events: Vec<SessionEvent> = Vec::new();
        loop {
            let cancelled = async {
                match &token {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = cancelled => {
                    // Abort is a courtesy with a deadline (the ruling):
                    // forward the abort, close stdin (the child aborts,
                    // flushes, exits — or the reaper kills the tree at
                    // the grace), and report Aborted now. The parent's
                    // tool body never waits on the child's cooperation.
                    let _ = self.commands.send(tabit_protocol::to_wire_line(
                        &SessionCommand::Abort { session: self.id.clone() },
                    ));
                    self.close();
                    return crate::session::RunSummary {
                        outcome: crate::session::RunOutcome::Aborted,
                        output: String::new(),
                        usage: Default::default(),
                        events,
                    };
                }
                frame = self.frames.recv() => {
                    let Some(frame) = frame else {
                        // The stream ended without a terminal: the child
                        // process died. A synthetic RunFailed carries the
                        // exit status and the stderr tail — the same
                        // mapping the tool's Failed arm already keeps.
                        events.push(SessionEvent::RunFailed {
                            message: self.crash_report(),
                        });
                        return crate::session::RunSummary {
                            outcome: crate::session::RunOutcome::Failed,
                            output: String::new(),
                            usage: Default::default(),
                            events,
                        };
                    };
                    if frame.stream.as_ref() != Some(&self.stream) {
                        continue; // A grandchild's frame — already forwarded.
                    }
                    let event = frame.event;
                    let terminal = match &event {
                        SessionEvent::RunFinished { output, usage, .. } => Some((
                            crate::session::RunOutcome::Completed,
                            output.clone(),
                            engine_usage(*usage),
                        )),
                        SessionEvent::RunAborted { output } => Some((
                            crate::session::RunOutcome::Aborted,
                            output.clone(),
                            Default::default(),
                        )),
                        SessionEvent::RunFailed { message } => Some((
                            crate::session::RunOutcome::Failed,
                            message.clone(),
                            Default::default(),
                        )),
                        _ => None,
                    };
                    events.push(event);
                    if let Some((outcome, output, usage)) = terminal {
                        self.close();
                        return crate::session::RunSummary {
                            outcome,
                            output,
                            usage,
                            events,
                        };
                    }
                }
            }
        }
    }

    /// Begin the child's shutdown (idempotent): stdin closes, the
    /// reaper's grace timer arms.
    fn close(&self) {
        self.closing.cancel();
    }

    /// The crash report: the exit status and the stderr tail.
    fn crash_report(&self) -> String {
        let exit = crate::lock::lock(&self.exit)
            .clone()
            .unwrap_or_else(|| "no exit recorded".to_string());
        let tail: Vec<String> = crate::lock::lock(&self.stderr_ring)
            .iter()
            .rev()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if tail.is_empty() {
            format!("the subagent process died unexpectedly ({exit})")
        } else {
            format!(
                "the subagent process died unexpectedly ({exit}); stderr tail:\n{}",
                tail.join("\n")
            )
        }
    }

    /// Wait for the reaper to finish (tests and callers that want the
    /// process fully reclaimed).
    pub async fn wait_exit(&mut self) {
        let _ = (&mut self.reaper).await;
    }
}

impl Drop for SubprocessChild {
    fn drop(&mut self) {
        // The commands sender drops with the struct; the closing token
        // arms the reaper either way. Nothing async here — the reaper
        // owns the wait.
        self.close();
    }
}

/// The task's text (the shared user-text fold — empty when the
/// message carries no text parts; the child treats it as the task).
fn message_text(message: &Message) -> String {
    crate::session::wire::user_text(message)
}

/// The wire usage folded back into the engine's shape (the reverse of
/// the wire fold — the shared five fields, the engine-internal rest
/// zeroed).
fn engine_usage(usage: tabit_protocol::Usage) -> rig_agent::completion::Usage {
    rig_agent::completion::Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        ..Default::default()
    }
}

/// Build the wrapped command: a Job Object on Windows (with
/// CREATE_NO_WINDOW — no console flash), a process group elsewhere —
/// `kill` reclaims the child's whole tree (its bash descendants must
/// not orphan).
#[cfg(windows)]
fn wrap_command(
    exe: &std::path::Path,
    args: &[String],
    cwd: &std::path::Path,
) -> process_wrap::tokio::CommandWrap {
    let mut command = tokio::process::Command::new(exe);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut wrap: process_wrap::tokio::CommandWrap = command.into();
    // The CreationFlags shim is the one way flags survive the JobObject
    // wrapper (which sets its own via CREATE_SUSPENDED).
    wrap.wrap(process_wrap::tokio::CreationFlags(
        windows::Win32::System::Threading::PROCESS_CREATION_FLAGS(0x0800_0000),
    ));
    wrap.wrap(process_wrap::tokio::JobObject);
    wrap
}

#[cfg(not(windows))]
fn wrap_command(
    exe: &std::path::Path,
    args: &[String],
    cwd: &std::path::Path,
) -> process_wrap::tokio::CommandWrap {
    let mut command = tokio::process::Command::new(exe);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut wrap: process_wrap::tokio::CommandWrap = command.into();
    wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
    wrap
}
