//! Host-mode e2e: the frontend-facing `--json` entry spawns a served
//! child for the argv boot and routes (owner ruling 2026-09). These
//! tests pin the frontend's contract THROUGH the extra process: the
//! startup sequence and its order, the ack's session fact, the
//! lifecycle forward door, the replay request riding the door's
//! idempotent path, the child's stderr tee, and the stream's end when
//! the child dies. Real binaries over real pipes — the same law as
//! the extension_tools suite.

#![cfg_attr(
    test,
    allow(
        clippy::err_expect,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::panic_in_result_fn,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

use std::io::{BufRead, BufReader, Write as _};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use tabit_protocol::{
    ClientFrame, EventFrame, PROTOCOL_VERSION, ServerControlFrame, ServerFrame, SessionCommand,
    SessionEvent,
};

const BOUND: Duration = Duration::from_secs(30);

fn test_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tabit-host-mode-tests/{}-{}",
        tag,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("test dir");
    dir
}

/// The hermetic world: an isolated config (a provider that is never
/// called — no message is sent, the model is only built), a
/// redirected home, an empty extension root, and a work dir whose
/// cwd the whole tree resolves against.
struct Stage {
    work: std::path::PathBuf,
    config: std::path::PathBuf,
}

fn stage(tag: &str) -> Stage {
    let dir = test_dir(tag);
    let config = dir.join("providers.toml");
    std::fs::write(
        &config,
        "[providers.p]\nbase_url = \"http://127.0.0.1:1/v1\"\napi = \"openai-completions\"\nkeyless = true\n\n[[providers.p.models]]\nid = \"m\"\n",
    )
    .expect("config");
    let auth = dir.join("auth.toml");
    std::fs::write(&auth, "providers = {}\n").expect("auth");
    let extensions = dir.join("extensions");
    std::fs::create_dir_all(&extensions).expect("extensions root");
    let home = dir.join("home");
    std::fs::create_dir_all(&home).expect("redirected home");
    let work = dir.join("work");
    std::fs::create_dir_all(&work).expect("work");
    Stage { work, config }
}

/// One host-mode backend over real pipes. Stderr is captured (not
/// echoed): the tee test reads it; the others hold it silently.
struct Backend {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: Receiver<String>,
    stderr: Receiver<String>,
}

fn spawn_backend(stage: &Stage) -> Backend {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tabit-core"));
    command
        .arg("--json")
        .arg("--ephemeral")
        .arg("--extensions")
        .arg(stage.config.parent().unwrap().join("extensions"))
        .current_dir(&stage.work)
        .env("TABIT_CONFIG", &stage.config)
        .env(
            "TABIT_AUTH",
            stage.config.parent().unwrap().join("auth.toml"),
        )
        .env("USERPROFILE", stage.config.parent().unwrap().join("home"))
        .env("HOME", stage.config.parent().unwrap().join("home"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn the host");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let (out_tx, out_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().by_ref().flatten() {
            if out_tx.send(line).is_err() {
                break;
            }
        }
    });
    let (err_tx, err_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().by_ref().flatten() {
            if err_tx.send(line).is_err() {
                break;
            }
        }
    });
    Backend {
        child,
        stdin,
        lines: out_rx,
        stderr: err_rx,
    }
}

impl Backend {
    fn send(&mut self, line: &str) {
        writeln!(self.stdin, "{line}")
            .and_then(|()| self.stdin.flush())
            .expect("write to the host");
    }

    fn next_frame(&mut self) -> ServerFrame {
        let line = match self.lines.recv_timeout(BOUND) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => panic!("no frame within the bound"),
            Err(RecvTimeoutError::Disconnected) => panic!("the host closed its stdout"),
        };
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("unparseable frame {line}: {e}"))
    }

    /// The next stderr line carrying `needle`, within the bound.
    fn next_stderr_line(&self, needle: &str) -> String {
        loop {
            match self.stderr.recv_timeout(BOUND) {
                Ok(line) if line.contains(needle) => return line,
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => panic!("no `{needle}` stderr line in the bound"),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the host closed its stderr before `{needle}`")
                }
            }
        }
    }

    fn initialize(&mut self, replay: bool) -> String {
        self.send(&tabit_protocol::to_wire_line(&ClientFrame::Initialize {
            protocol_version: PROTOCOL_VERSION,
            replay,
        }));
        match self.next_frame() {
            ServerFrame::Control(ServerControlFrame::InitializeAck { session_id, .. }) => {
                session_id
            }
            other => panic!("expected initialize_ack, got {other:?}"),
        }
    }

    fn send_command(&mut self, command: &SessionCommand) {
        self.send(&tabit_protocol::to_wire_line(command));
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One stamped event frame within the bound, skipping the unstamped
/// backend-level ones (the catalogs) — the caller that wants those
/// reads them explicitly.
fn next_stamped(backend: &mut Backend) -> EventFrame {
    loop {
        match backend.next_frame() {
            ServerFrame::Event(frame) if frame.stream.is_some() => return frame,
            ServerFrame::Event(_) => continue,
            ServerFrame::Control(other) => panic!("unexpected control frame {other:?}"),
        }
    }
}

#[test]
fn the_startup_contract_crosses_the_host_process_in_order() {
    let stage = stage("contract");
    let mut backend = spawn_backend(&stage);
    let boot = backend.initialize(false);

    // The pinned order (FRONTEND.md §3): the boot's announcement
    // first — stamped with the ack's session id, `resumed: false` for
    // the ephemeral boot — then the session catalog, backend-level.
    let opened = next_stamped(&mut backend);
    let SessionEvent::SessionOpened {
        id, resumed, path, ..
    } = opened.event
    else {
        panic!("expected session_opened, got {:?}", opened.event);
    };
    assert_eq!(id, boot, "the announce names the ack's session");
    assert!(!resumed, "the ephemeral boot starts fresh");
    assert!(
        path.is_empty(),
        "the ephemeral announce has no file: {path}"
    );

    match backend.next_frame() {
        ServerFrame::Event(EventFrame {
            stream: None,
            event: SessionEvent::SessionsAvailable { .. },
            ..
        }) => {}
        other => panic!("expected backend-level sessions_available, got {other:?}"),
    }
}

#[test]
fn the_forward_door_serves_lifecycle_in_the_child() {
    let stage = stage("forward");
    let mut backend = spawn_backend(&stage);
    let boot = backend.initialize(false);
    next_staged_opened(&mut backend); // the boot's announce
    skip_catalog(&mut backend);

    // new_session crosses the forward door; the child's door serves
    // it — a second, distinct, fresh session announces on its own
    // stream.
    backend.send_command(&SessionCommand::NewSession);
    let second = next_stamped(&mut backend);
    let SessionEvent::SessionOpened {
        id,
        resumed,
        parent,
        ..
    } = second.event
    else {
        panic!(
            "expected the new session's announce, got {:?}",
            second.event
        );
    };
    assert_ne!(id, boot, "a fresh session, not a re-announce");
    assert!(!resumed);
    assert!(
        parent.is_none(),
        "a top-level session nests in a subprocess"
    );
    assert_eq!(
        second.stream.as_ref().map(|s| s.as_str()),
        Some(id.as_str()),
        "the announce is stamped with its own session"
    );

    // open_session of the (already-open) boot is the idempotent
    // re-replay — the same act the replay request rides.
    backend.send_command(&SessionCommand::OpenSession { id: boot.clone() });
    // The pass opens with the model's re-announcement (emit_replay's
    // shape), then the brackets — all on the boot's stream.
    loop {
        let frame = next_stamped(&mut backend);
        assert_eq!(
            frame.stream.as_ref().map(|s| s.as_str()),
            Some(boot.as_str()),
            "the re-replay rides the boot's stream: {:?}",
            frame.event
        );
        match frame.event {
            SessionEvent::ModelChanged { .. } => continue,
            SessionEvent::ReplayStarted { .. } => break,
            other => panic!("expected the re-replay's opening, got {other:?}"),
        }
    }
}

fn next_staged_opened(backend: &mut Backend) -> String {
    let frame = next_stamped(backend);
    let SessionEvent::SessionOpened { id, .. } = frame.event else {
        panic!("expected session_opened, got {:?}", frame.event);
    };
    id
}

fn skip_catalog(backend: &mut Backend) {
    match backend.next_frame() {
        ServerFrame::Event(EventFrame {
            stream: None,
            event: SessionEvent::SessionsAvailable { .. },
            ..
        }) => {}
        other => panic!("expected the session catalog, got {other:?}"),
    }
}

#[test]
fn initialize_with_replay_rides_the_forwarded_door() {
    let stage = stage("replay");
    let mut backend = spawn_backend(&stage);
    let boot = backend.initialize(true);

    // The replay request became an open_session of the boot (the
    // door's idempotent path), forwarded to the child: the pass lands
    // after the startup frames, bracketed, on the boot's stream. The
    // empty ephemeral chain replays model_changed + the brackets.
    let mut saw_opened = false;
    let mut saw_done = false;
    let deadline = std::time::Instant::now() + BOUND;
    while std::time::Instant::now() < deadline && !saw_done {
        let ServerFrame::Event(frame) = backend.next_frame() else {
            continue;
        };
        let on_boot = frame.stream.as_ref().map(|s| s.as_str()) == Some(boot.as_str());
        match frame.event {
            SessionEvent::SessionOpened { .. } => saw_opened = true,
            SessionEvent::ReplayDone if on_boot => saw_done = true,
            _ => {}
        }
    }
    assert!(saw_opened, "the startup announce crossed");
    assert!(saw_done, "the replay pass crossed, on the boot's stream");
}

#[test]
fn the_childs_stderr_tees_to_the_hosts_terminal() {
    let stage = stage("tee");
    let mut backend = spawn_backend(&stage);
    // The banner is the child's (session assembled there); the tee
    // makes it this process's stderr output — the human spawned the
    // host, and the session's diagnostics are the host's.
    let banner = backend.next_stderr_line("session");
    assert!(
        banner.contains("started"),
        "the child's banner line verbatim: {banner}"
    );
    let boot = backend.initialize(false);
    assert!(!boot.is_empty());
}

#[test]
fn the_childs_death_ends_the_frontend_stream() {
    let stage = stage("death");
    let mut backend = spawn_backend(&stage);
    let _boot = backend.initialize(false);
    let _opened = next_staged_opened(&mut backend);

    // Kill the served child (the host's only descendant): the exit
    // tap cancels the stream's end, the edge resolves, and the
    // frontend's connection closes — stdout EOF, the process gone.
    let host_pid = backend.child.id();
    let found = std::process::Command::new("powershell")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(format!(
            "(Get-CimInstance Win32_Process -Filter \"ParentProcessId = {host_pid}\").ProcessId"
        ))
        .output()
        .expect("query the host's children");
    let child_pid: u32 = String::from_utf8_lossy(&found.stdout)
        .split_whitespace()
        .next()
        .and_then(|first| first.parse().ok())
        .expect("the host has exactly one child by now");
    let killed = std::process::Command::new("taskkill")
        .args(["/PID", &child_pid.to_string(), "/F"])
        .output()
        .expect("kill the served child");
    assert!(killed.status.success(), "the child died: {killed:?}");

    // Stdout reaches EOF within the bound (the reader thread's
    // channel closes), and the host process itself exits.
    let deadline = std::time::Instant::now() + BOUND;
    loop {
        match backend.lines.recv_timeout(Duration::from_secs(1)) {
            Ok(_) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the stream never ended after the child died"
                );
            }
        }
    }
    let deadline = std::time::Instant::now() + BOUND;
    loop {
        match backend.child.try_wait().expect("poll the host") {
            Some(_) => break,
            None => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the host process never exited after its child died"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}
