//! Child-process plumbing shared by every tree the backend spawns:
//! the extension supervisor here, the subagent bridge in
//! tabit-session (`subprocess.rs` imports both helpers). One home —
//! killing a child must reclaim its descendants (a bash under a
//! subagent, a server under an extension), and the crash report
//! always wants the stderr tail.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, BufReader};

/// The wrapped child's type (what `wrap_command(...).spawn()`
/// yields) — re-exported so the bridge (a downstream crate without a
/// direct process-wrap dependency) can name it.
pub use process_wrap::tokio::ChildWrapper;

/// The stderr ring's depth — the crash report's tail.
pub const STDERR_RING: usize = 200;

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
