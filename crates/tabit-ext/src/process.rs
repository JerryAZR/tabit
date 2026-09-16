//! Child-process plumbing shared by every tree the backend spawns:
//! the extension supervisor here, the subagent bridge in
//! tabit-session (`subprocess.rs` imports the helpers). One home —
//! killing a child must reclaim its descendants (a bash under a
//! subagent, a server under an extension), the crash report always
//! wants the stderr tail, and the pipe protocol (a command writer
//! whose close IS the stdin drop, a grace-then-kill reaper) is the
//! same contract on both pipes. What stays at each site: the frame
//! READER (the supervisor routes typed extension frames, the bridge
//! forwards stamped frames verbatim — different output artifacts) and
//! each side's cancel/abort contract. The coding `bash` tool shares
//! none of this: it is the std flavor, feeds no stdin, and its whole
//! output is the product (no ring) — it aligns on the process-wrap
//! version and keeps its own documented kill loop.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

/// The wrapped child's type (what `wrap_command(...).spawn()`
/// yields) — re-exported so the bridge (a downstream crate without a
/// direct process-wrap dependency) can name it.
pub use process_wrap::tokio::ChildWrapper;

/// The stderr ring's depth — the crash report's tail.
pub const STDERR_RING: usize = 200;

/// How long a closing child gets to exit on its own before the tree
/// kill. One value for both pipes (extensions, subagent children):
/// long enough for a cooperative flush, short enough that a wedged
/// child does not outlive its parent's patience. The coding `bash`
/// tool allows itself no grace at all — its force-no-grace ruling is
/// local to it.
pub const REAP_GRACE: Duration = Duration::from_secs(5);

/// How long a spawned child gets to complete its handshake before
/// the host kills it. Extensions ack their initialize; subagent
/// children run the frontend initialize — one bound for both,
/// absorbing cold starts on a loaded machine.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// A crash-report tail buffer: the last [`STDERR_RING`] stderr lines
/// of a spawned child.
pub type StderrRing = Mutex<VecDeque<String>>;

/// Drain a child's stderr into a bounded ring on the current tokio
/// runtime. The task ends at the pipe's EOF (the process is gone).
pub fn spawn_stderr_ring(stderr: tokio::process::ChildStderr) -> Arc<StderrRing> {
    let ring = Arc::new(StderrRing::default());
    let sink = ring.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut sink = tabit_log::lock::lock(&sink);
            if sink.len() == STDERR_RING {
                sink.pop_front();
            }
            sink.push_back(line);
        }
    });
    ring
}

/// The crash report's tail: the last few ring lines, in order.
pub fn crash_tail(ring: &StderrRing) -> String {
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

/// One LF-terminated, flushed line onto a child's stdin.
pub async fn write_line(stdin: &mut tokio::process::ChildStdin, line: &str) -> std::io::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

/// The command writer: lines in, stdin out. The closing token IS the
/// pipe close (the drive holds a sender clone, so dropping senders
/// cannot be the mechanism): on close, everything already queued
/// (the abort line that raced it) is written, then the pipe drops —
/// EOF, the child's death contract.
pub fn spawn_command_writer(
    mut stdin: tokio::process::ChildStdin,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<String>,
    closing: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            let line = tokio::select! {
                _ = closing.cancelled() => {
                    // Deliver what the close raced, then drop the pipe.
                    while let Ok(line) = commands.try_recv() {
                        if write_line(&mut stdin, &line).await.is_err() {
                            return;
                        }
                    }
                    break;
                }
                line = commands.recv() => match line {
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

/// Kill and reap now — the pre-handshake failure path (a child that
/// never handshook) and immediate teardown: cancel the closing token
/// (the writer and the reaper observe the close), kill the tree,
/// reap the exit. Callers layer their reporting on top.
pub async fn kill_now(process: &mut Box<dyn ChildWrapper>, closing: &CancellationToken) {
    closing.cancel();
    let _ = Box::into_pin(process.kill()).await;
    let _ = process.wait().await;
}

/// The post-close reaper: a bounded window ([`REAP_GRACE`]) to exit
/// on its own, then the tree kill. Returns the exit status when one
/// was observed.
pub async fn reap_with_grace(
    process: &mut Box<dyn ChildWrapper>,
) -> Option<std::process::ExitStatus> {
    match tokio::time::timeout(REAP_GRACE, process.wait()).await {
        Ok(status) => status.ok(),
        Err(_) => {
            let _ = Box::into_pin(process.kill()).await;
            process.wait().await.ok()
        }
    }
}

/// Build the wrapped command: a Job Object on Windows (with
/// CREATE_NO_WINDOW — no console flash), a process group elsewhere —
/// `kill` reclaims the child's whole tree (its bash descendants must
/// not orphan). `KillOnDrop` closes the gap the natural exit path
/// otherwise leaves: the serving runtime's tasks are cancelled before
/// their reclamation futures finish, so without kill-on-close a
/// wedged child (one that ignores stdin EOF) would outlive the
/// backend. With the wrapper, dropping the process handle kills the
/// tree — the OS-provided door the explicit reaper path merely
/// reaches faster.
#[cfg(windows)]
pub fn wrap_command(
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
    wrap.wrap(process_wrap::tokio::KillOnDrop);
    wrap
}

#[cfg(not(windows))]
pub fn wrap_command(
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
    wrap.wrap(process_wrap::tokio::KillOnDrop);
    wrap
}
