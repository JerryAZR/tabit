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
//! Tabit's coding tools: file reading, directory listing, and shell
//! execution, implemented as [`PortableTool`]s via `#[rig_tool]` (the
//! workspace's canonical tool surface) and erasable into [`DynamicTool`]s
//! for session registration.
//!
//! All paths are taken verbatim from the model; relative paths resolve
//! against the process working directory. Errors are user-facing (external
//! errors): clear, graceful, and never a panic.
//!
//! Native only: these tools touch the filesystem and spawn processes.

use rig_agent::tool::{DynamicTool, ToolContext};
use rig_core::tool::{IntoToolOutput, PortableTool, ToolExecutionError};
use rig_derive::rig_tool;
use std::time::{Duration, Instant};

/// Maximum bytes of a file the `read` tool returns.
pub const READ_CAP_BYTES: usize = 256 * 1024;
/// Maximum bytes of combined output the `bash` tool returns.
pub const OUTPUT_CAP_BYTES: usize = 128 * 1024;
/// Default seconds a `bash` command may run.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Maximum seconds a `bash` command may run.
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// Read a UTF-8 text file. Output is capped at
/// [`READ_CAP_BYTES`] with an explicit truncation notice.
#[rig_tool(
    description = "Read a UTF-8 text file from an absolute or relative path. \
                   Returns the file contents; large files are truncated with a notice."
)]
pub async fn read(path: String) -> Result<String, ToolExecutionError> {
    let meta = std::fs::metadata(&path)
        .map_err(|e| ToolExecutionError::other(format!("cannot read `{path}`: {e}")))?;
    if meta.is_dir() {
        return Err(ToolExecutionError::other(format!(
            "`{path}` is a directory; use the ls tool for directories"
        )));
    }
    let bytes = std::fs::read(&path)
        .map_err(|e| ToolExecutionError::other(format!("cannot read `{path}`: {e}")))?;
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            return Err(ToolExecutionError::other(format!(
                "`{path}` is not valid UTF-8 ({} bytes); binary files are not \
                 supported by this tool ({error})",
                error.utf8_error().valid_up_to()
            )));
        }
    };
    cap_text(text, READ_CAP_BYTES, "file")
}

/// List a directory's entries: type, size, and modification time.
#[rig_tool(
    description = "List a directory's entries with type, size, and modified time. \
                   Defaults to the current directory."
)]
pub async fn ls(path: Option<String>) -> Result<String, ToolExecutionError> {
    let dir = path.as_deref().unwrap_or(".");
    let entries = std::fs::read_dir(dir)
        .map_err(|e| ToolExecutionError::other(format!("cannot list `{dir}`: {e}")))?;
    let mut rows: Vec<(String, &'static str, u64, String)> = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| ToolExecutionError::other(format!("listing `{dir}`: {e}")))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry
            .metadata()
            .map_err(|e| ToolExecutionError::other(format!("`{dir}/{name}`: {e}")))?;
        let kind = if meta.is_dir() { "dir" } else { "file" };
        let size = if meta.is_dir() { 0 } else { meta.len() };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| {
                humantime::format_rfc3339_seconds(std::time::SystemTime::UNIX_EPOCH + d).to_string()
            })
            .unwrap_or_else(|| "-".to_string());
        rows.push((name, kind, size, modified));
    }
    rows.sort();
    let mut out = String::new();
    for (name, kind, size, modified) in rows {
        out.push_str(&format!("{kind:>4} {size:>10}  {modified}  {name}\n"));
    }
    if out.is_empty() {
        out.push_str("(empty directory)\n");
    }
    Ok(out)
}

/// Run a shell command. On Windows the tool prefers `bash` on PATH (Git
/// Bash) so commands keep POSIX syntax; it falls back to PowerShell only
/// when no bash exists. Combined output (stdout, then stderr) is capped at
/// [`OUTPUT_CAP_BYTES`], and commands that exceed their timeout are killed.
#[rig_tool(description = "Run a shell command and return its combined output. \
                   Commands run through bash (on Windows: Git Bash when on PATH, \
                   else PowerShell). Non-zero exits report the exit code. \
                   Output is capped at 128 KiB; commands time out after 30 seconds \
                   unless timeout_secs says otherwise.")]
pub async fn bash(
    command: String,
    timeout_secs: Option<u64>,
) -> Result<String, ToolExecutionError> {
    let timeout = Duration::from_secs(
        timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS),
    );
    let interpreter = interpreter();
    let mut child = std::process::Command::new(&interpreter.argv0)
        .args(interpreter.args)
        .arg(&command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            ToolExecutionError::other(format!("cannot start `{}`: {e}", interpreter.argv0))
        })?;
    let output = run_with_timeout(&mut child, timeout, &interpreter.argv0)?;
    let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.stderr.is_empty() {
        combined.push_str("\n--- stderr ---\n");
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    let combined = cap_text(combined, OUTPUT_CAP_BYTES, "command output")?;
    if output.status.success() {
        Ok(combined)
    } else {
        Err(ToolExecutionError::other(format!(
            "command exited with {}:\n{combined}",
            exit_description(&output.status)
        )))
    }
}

/// The interpreter `bash` runs through on this platform.
struct Interpreter {
    argv0: String,
    /// Flags before the command text (`["-lc"]` for login-path bash,
    /// `["-NoProfile", "-Command"]` for PowerShell).
    args: &'static [&'static str],
}

#[cfg(windows)]
fn interpreter() -> Interpreter {
    // Git Bash puts bash.exe on PATH; prefer it so commands keep POSIX
    // syntax. `where` prints one match per line.
    let found = std::process::Command::new("where")
        .arg("bash")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.lines()
                .next()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
        });
    match found {
        Some(bash_path) => Interpreter {
            argv0: bash_path,
            args: &["-c"],
        },
        None => Interpreter {
            argv0: "powershell".to_string(),
            args: &["-NoProfile", "-Command"],
        },
    }
}

#[cfg(not(windows))]
fn interpreter() -> Interpreter {
    Interpreter {
        argv0: "bash".to_string(),
        args: &["-c"],
    }
}

struct CapturedOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn exit_description(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("status {code}"),
        None => "an abnormal termination signal".to_string(),
    }
}

/// Wait for `child`, capturing piped stdout/stderr on reader threads, and
/// kill it if it exceeds `timeout`.
///
/// Hand-rolled over `std::process` on purpose: this crate is sync (the
/// async tool surface awaits at the dispatch boundary), `tokio::process`
/// would drag a runtime dependency into a std-only tool crate, and the
/// poll-try_wait loop below is the entire algorithm.
fn run_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
    argv0: &str,
) -> Result<CapturedOutput, ToolExecutionError> {
    let stdout_reader = spawn_reader(child.stdout.take());
    let stderr_reader = spawn_reader(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ToolExecutionError::other(format!(
                        "command exceeded its {}s timeout and was killed \
                         (raise timeout_secs if it legitimately needs longer)",
                        timeout.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                return Err(ToolExecutionError::other(format!(
                    "waiting for `{argv0}`: {e}"
                )));
            }
        }
    };
    Ok(CapturedOutput {
        status,
        stdout: join_reader(stdout_reader),
        stderr: join_reader(stderr_reader),
    })
}

type ReaderJoin = Option<std::thread::JoinHandle<Vec<u8>>>;

fn spawn_reader<R>(pipe: Option<R>) -> ReaderJoin
where
    R: std::io::Read + Send + 'static,
{
    pipe.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            // A failed read still returns what arrived; the tool output
            // cap handles size, and the process exit explains the rest.
            let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
            buf
        })
    })
}

fn join_reader(handle: ReaderJoin) -> Vec<u8> {
    handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

/// Truncate `text` to `cap` bytes on a char boundary, appending an explicit
/// notice.
fn cap_text(text: String, cap: usize, what: &str) -> Result<String, ToolExecutionError> {
    if text.len() <= cap {
        return Ok(text);
    }
    let mut cut = cap;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut truncated = String::from(&text[..cut]);
    truncated.push_str(&format!(
        "\n... [{what} truncated: showed {cut} of {} bytes] ...\n",
        text.len()
    ));
    Ok(truncated)
}

/// Erase any [`PortableTool`] into a [`DynamicTool`] so sessions (which
/// rebuild their agent on model switches) can hold mixed tool sets in one
/// vector.
pub fn dynamic<T>(tool: T) -> DynamicTool
where
    T: PortableTool + Send + Sync + 'static,
{
    let tool = std::sync::Arc::new(tool);
    DynamicTool::new(
        <T as PortableTool>::NAME.to_string(),
        tool.description(),
        tool.parameters(),
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let tool = tool.clone();
            Box::pin(async move {
                let typed: <T as PortableTool>::Args = serde_json::from_value(args)
                    .map_err(|e| ToolExecutionError::other(format!("invalid arguments: {e}")))?;
                let output = <T as PortableTool>::call(tool.as_ref(), typed)
                    .await
                    .map_err(|e| tool.map_error(e))?;
                output.into_tool_output()
            })
        },
    )
}

#[cfg(test)]
mod tests;
