//! The backend child process: spawn `tabit --json`, run the
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
    ClientFrame, ModelSelection, PROTOCOL_VERSION, ServerControlFrame, ServerFrame, SessionCommand,
};

use crate::reducer::InMsg;

/// How many stderr lines to keep for crash reporting.
const STDERR_RING: usize = 40;

/// A live `tabit --json` child with its pipe threads.
pub struct Backend {
    writer: Sender,
    stderr: Arc<Mutex<Vec<String>>>,
    rx: std::sync::mpsc::Receiver<InMsg>,
}

type Sender = std::sync::mpsc::Sender<String>;

/// Where to find the `tabit` binary. The supported flow needs no
/// guessing: the launcher hands its exact path over with `--tabit`.
/// The fallbacks (env override, sibling, PATH) serve direct
/// `cargo run -p tabit-gui` development.
fn tabit_bin(launcher_provided: Option<&Path>) -> PathBuf {
    if let Some(path) = launcher_provided {
        return path.to_path_buf();
    }
    if let Ok(path) = std::env::var("TABIT_BIN") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(format!("tabit{}", std::env::consts::EXE_SUFFIX));
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("tabit")
}

/// Spawn a backend in `cwd` (the project directory). `resume_newest`
/// passes `--continue`; the caller handles the no-sessions failure by
/// respawning fresh (a pre-handshake spawn failure exits 1 with a
/// stderr note — FRONTEND.md §3.5). `repaint` is called after every
/// message so the UI wakes immediately.
pub fn spawn(
    cwd: Option<&Path>,
    tabit: Option<&Path>,
    resume_newest: bool,
    repaint: impl Fn() + Send + 'static,
) -> std::io::Result<Backend> {
    let mut command = Command::new(tabit_bin(tabit));
    command.arg("--json");
    if resume_newest {
        command.arg("--continue");
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn()?;

    // Handshake first line out.
    let (writer_tx, writer_rx) = std::sync::mpsc::channel::<String>();
    // Sanctioned crash (AGENTS.md doctrine): pipes are captured the
    // instant Stdio::piped() spawned them.
    #[allow(clippy::expect_used)]
    let mut stdin = child
        .stdin
        .take()
        .expect("internal invariant violated: stdin pipe captured at spawn");
    let init = serde_json::to_string(&ClientFrame::Initialize {
        protocol_version: PROTOCOL_VERSION,
    })
    .unwrap_or_else(|_| "{\"protocol_version\":1}".to_string());
    let _ = writeln!(stdin, "{init}");
    let _ = stdin.flush();

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
                    Ok(ServerFrame::Control(ServerControlFrame::InitializeAck {
                        protocol_version: _,
                        session_id,
                        session_path,
                        model:
                            ModelSelection {
                                provider,
                                model,
                                thinking_level,
                            },
                    })) => {
                        let _ = tx.send(InMsg::Ack {
                            session_id,
                            session_path,
                            model: ModelSelection {
                                provider,
                                model,
                                thinking_level,
                            },
                        });
                    }
                    Ok(ServerFrame::Control(ServerControlFrame::InitializeRejected { reason })) => {
                        let _ = tx.send(InMsg::Rejected(reason));
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

    /// Send a message (steers a live run, starts one when idle).
    pub fn send_message(&self, text: &str) {
        let _ = self.writer.send(
            serde_json::to_string(&SessionCommand::Message {
                text: text.to_string(),
            })
            .unwrap_or_default(),
        );
    }

    /// Abort the live run (and clear the queue backend-side).
    pub fn abort(&self) {
        let _ = self
            .writer
            .send(serde_json::to_string(&SessionCommand::Abort).unwrap_or_default());
    }

    /// The tail of the backend's stderr, for crash reporting.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

fn reap(mut child: Child) -> Option<i32> {
    child.wait().ok().and_then(|status| status.code())
}
