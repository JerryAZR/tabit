//! The backend child process: spawn `tabit-core --json`, run the
//! handshake, own the pipes. One [`Backend`] per window; the window
//! (not the backend) owns the lifecycle — crash isolation is the
//! point (AGENTS.md error doctrine: backend panics must not take the
//! GUI down).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use tabit_protocol::{
    PROTOCOL_VERSION, ServerControlFrame, ServerFrame, SessionCommand, to_wire_line,
};

use crate::reducer::InMsg;

/// How many stderr lines to keep for crash reporting — enough for a
/// full internal-error report (message plus backtrace), bounded against
/// runaway output.
const STDERR_RING: usize = 200;

/// A live `tabit-core --json` child with its pipe threads.
pub struct Backend {
    writer: Sender,
    stderr: Arc<Mutex<Vec<String>>>,
    rx: std::sync::mpsc::Receiver<InMsg>,
}

type Sender = std::sync::mpsc::Sender<String>;

/// Where to find the `tabit-core` binary: the dev override
/// (`TABIT_CORE_BIN`), the sibling of this executable (cargo installs
/// workspace binaries side by side), then PATH — in that order.
fn backend_bin() -> PathBuf {
    if let Ok(path) = std::env::var("TABIT_CORE_BIN") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(format!("tabit-core{}", std::env::consts::EXE_SUFFIX));
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("tabit-core")
}

/// Spawn a backend in `cwd` (the project directory), booting the
/// newest session (`--continue`): returning users get their newest
/// session; an empty store is absorbed backend-side into a fresh start
/// (the boot's `session_opened` carries `resumed: false` — the
/// pinned startup contract). Creating and switching sessions are channel commands
/// (protocol v3) — never respawns. `repaint` is called after every
/// message so the UI wakes immediately.
pub fn spawn(cwd: Option<&Path>, repaint: impl Fn() + Send + 'static) -> std::io::Result<Backend> {
    let mut command = Command::new(backend_bin());
    command.arg("--json").arg("--continue");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    no_console_window(&mut command);
    let mut child = command.spawn()?;

    // The report model (owner ruling 2026-09-25): the backend speaks
    // first — its self-report is the first stdout line, and commands
    // flow from here whenever. The `--continue` spawn replays
    // automatically on the backend side; no request crosses.
    let (writer_tx, writer_rx) = std::sync::mpsc::channel::<String>();
    // Sanctioned crash (AGENTS.md doctrine): pipes are captured the
    // instant Stdio::piped() spawned them.
    #[allow(clippy::expect_used)]
    let stdin = child
        .stdin
        .take()
        .expect("internal invariant violated: stdin pipe captured at spawn");

    let (msg_tx, msg_rx) = std::sync::mpsc::channel::<InMsg>();
    let stderr = Arc::new(Mutex::new(Vec::new()));
    // Writer thread: the command pipe.
    {
        let mut stdin = stdin;
        thread::spawn(move || {
            while let Ok(line) = writer_rx.recv() {
                if writeln!(stdin, "{line}")
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    break; // backend gone; reader thread reports the exit
                }
            }
        });
    }
    // Stderr thread: drain to the ring for crash reporting (captured
    // before the reader thread takes ownership of the child).
    {
        #[allow(clippy::expect_used)]
        let stderr_stream = child
            .stderr
            .take()
            .expect("internal invariant violated: stderr pipe captured at spawn");
        let ring = stderr.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stderr_stream);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let mut ring = ring.lock().unwrap_or_else(|p| p.into_inner());
                if ring.len() == STDERR_RING {
                    ring.remove(0);
                }
                ring.push(line);
            }
        });
    }

    // Reader thread: stdout lines → InMsg; on EOF, reap the child.
    {
        #[allow(clippy::expect_used)]
        let stdout = child
            .stdout
            .take()
            .expect("internal invariant violated: stdout pipe captured at spawn");
        let tx = msg_tx.clone();
        let repaint = repaint;
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<ServerFrame>(&line) {
                    Ok(ServerFrame::Control(ServerControlFrame::Report { protocol_version })) => {
                        // The spawner's version check (the report
                        // model's gate): a backend this GUI cannot
                        // talk to is a kill — the reason crosses like
                        // a rejection, and the child dies with the
                        // Backend.
                        if protocol_version == PROTOCOL_VERSION {
                            let _ = tx.send(InMsg::Report);
                        } else {
                            let _ = tx.send(InMsg::Rejected(format!(
                                "the backend speaks protocol version {protocol_version} — \
                                 this GUI speaks {PROTOCOL_VERSION}"
                            )));
                            break;
                        }
                    }
                    Ok(ServerFrame::Control(ServerControlFrame::ProtocolError { message })) => {
                        let _ = tx.send(InMsg::ProtocolError(message));
                    }
                    Ok(ServerFrame::Event(frame)) => {
                        let _ = tx.send(InMsg::Event(Box::new(frame)));
                    }
                    Err(_) => {
                        // Unparseable from our own backend: surface it;
                        // the connection stays (protocol_error is the
                        // backend's job, this is our side being unable
                        // to read).
                        let _ = tx.send(InMsg::ProtocolError(format!(
                            "unparseable backend line: {line}"
                        )));
                    }
                }
                repaint();
            }
            let code = reap(child);
            let _ = tx.send(InMsg::BackendExited { code });
            repaint();
        });
    }

    Ok(Backend {
        writer: writer_tx,
        stderr,
        rx: msg_rx,
    })
}

impl Backend {
    /// Drain everything arrived since last call.
    pub fn drain(&self) -> Vec<InMsg> {
        let mut msgs = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            msgs.push(msg);
        }
        msgs
    }

    /// Send a message to a session (steers a live run, starts one when
    /// idle).
    pub fn send_message(&self, session: &str, text: &str) {
        let _ = self.writer.send(to_wire_line(&SessionCommand::Message {
            session: session.to_string(),
            text: text.to_string(),
        }));
    }

    /// Abort a session's live run (and clear its queue backend-side).
    pub fn abort(&self, session: &str) {
        let _ = self.writer.send(to_wire_line(&SessionCommand::Abort {
            session: session.to_string(),
        }));
    }

    /// Create a fresh session in the backend (the outcome arrives as
    /// its stamped `session_opened`, `resumed: false`).
    pub fn new_session(&self) {
        let _ = self.writer.send(to_wire_line(&SessionCommand::NewSession));
    }

    /// Open (load if needed, then replay) a stored session; the pass
    /// that follows is the acknowledgment.
    pub fn open_session(&self, id: &str) {
        let _ = self.writer.send(to_wire_line(&SessionCommand::OpenSession {
            id: id.to_string(),
        }));
    }

    /// Checkout a session at an entry (rewind/branch). Safe any time:
    /// the backend parks it until the session's run ends (pause-point
    /// semantics, FRONTEND.md §7). `messages_discarded` (if any) →
    /// `checked_out` → the replay pass follow.
    pub fn checkout(&self, session: &str, entry_id: &str) {
        let _ = self.writer.send(to_wire_line(&SessionCommand::Checkout {
            session: session.to_string(),
            entry_id: entry_id.to_string(),
        }));
    }

    /// Switch a session's model (the register write; stage 3) — a
    /// state write that happens entirely at receive: a ref the
    /// backend's config cannot resolve is an immediate
    /// `error { kind: model }`; otherwise the entry and the live
    /// selection land at once and `model_changed` follows immediately
    /// (a run in flight finishes untouched; the next run derives the
    /// new agent).
    pub fn model(&self, session: &str, provider: &str, model: &str) {
        let _ = self.writer.send(to_wire_line(&SessionCommand::Model {
            session: session.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            thinking_level: None,
        }));
    }

    /// Answer an interaction request of a session. The payload is the
    /// answer shaped by the asking template; a stale id is a backend
    /// no-op.
    pub fn send_interaction_response(&self, session: &str, id: &str, payload: &serde_json::Value) {
        let _ = self
            .writer
            .send(to_wire_line(&SessionCommand::InteractionResponse {
                session: Some(session.to_string()),
                id: id.to_string(),
                payload: payload.clone(),
            }));
    }

    /// The tail of the backend's stderr, for crash reporting.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// A console-subsystem child spawned by a windowless (detached) GUI
/// would allocate and flash its own console on Windows — suppress it;
/// the pipes are unaffected.
#[cfg(windows)]
fn no_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_console_window(_command: &mut Command) {}

fn reap(mut child: Child) -> Option<i32> {
    child.wait().ok().and_then(|status| status.code())
}
