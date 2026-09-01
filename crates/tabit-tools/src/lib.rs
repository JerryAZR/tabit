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
//! Tabit's coding tools: file reading and shell execution, implemented
//! as [`PortableTool`]s via `#[rig_tool]` (the workspace's canonical
//! tool surface) and erasable into [`DynamicTool`]s for session
//! registration.
//!
//! All paths are taken verbatim from the model; relative paths resolve
//! against the process working directory. Errors are user-facing (external
//! errors): clear, graceful, and never a panic.
//!
//! Native only: these tools touch the filesystem and spawn processes.
//!
//! # Cancellation contract (tool authors read this)
//!
//! Cancellation is cooperative and split by ownership — the engine
//! owns *when* to stop, the tool owns *how* to stop what it started.
//! Three layers:
//!
//! - **The token is the ask**: the runtime cancels a per-invocation
//!   [`CancellationToken`](tokio_util::sync::CancellationToken) on
//!   abort (user stop, session shutdown). Tools receive it through
//!   [`ToolContext`]; plain `#[rig_tool]` functions that never spawn
//!   OS resources can ignore it. On native, bodies poll on an
//!   isolated sidecar runtime (ENGINE.md's execution-substrate
//!   ruling), so abort does not drop the body mid-poll — the token
//!   is the mechanism, and a well-behaved body observes it. The
//!   `bash` tool is the reference implementation: it spawns its
//!   child through `process-wrap` (`JobObject` on Windows, a
//!   process-group leader on Unix) so an explicit `kill()` — and
//!   the drop backstop when the body ends — take down the whole
//!   process tree, and it reads both output pipes up front so a
//!   dead child's pipe never deadlocks the reader.
//! - **Bounded bodies are the expectation**: every chain is bounded
//!   by its own timeout or the user. A body that ignores the token
//!   cannot be force-killed (Rust has no safe thread-kill) — it
//!   leaks a sidecar task until it returns, never stalling the
//!   harness; blocking the thread is safe but wasteful, so prefer
//!   async in bodies. Bodies get a full tokio context on native.
//! - **Process death is the backstop**: on session/process exit all
//!   threads and children die with it.
//! - **Force, no grace**: kills go straight to force (no SIGTERM
//!   grace period). Tool calls are user-cancellable and the model
//!   is told the call was interrupted, so there is no cleanup
//!   contract with the child. Where a grace-then-kill is ever
//!   wanted it lives at the resource-acquisition boundary (the
//!   spawner), never in the engine.
//! - **Report shape**: a cancelled call returns a clear
//!   "interrupted" error/result (the session layer synthesizes the
//!   model-visible record for calls that never answered); it never
//!   returns output that looks like a completed run.

use rig_agent::tool::{DynamicTool, ToolContext};
use rig_core::tool::{IntoToolOutput, PortableTool, ToolExecutionError};
use rig_derive::rig_tool;
use std::time::{Duration, Instant};

mod shell;
mod truncate;

/// Default seconds a `bash` command may run.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Maximum seconds a `bash` command may run.
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// Read a UTF-8 text file page by page. Truncation keeps whole lines
/// (2000 lines / 50 KiB, whichever hits first — [`truncate`]) and every
/// truncation notice carries the offset that continues the read: large
/// files are paged, never spilled (the file is already on disk).
/// Directories list their entries inline. Binary and UTF-16/32 files are
/// rejected loudly. Image reads are a committed follow-up (ROADMAP item
/// 4) — they will bypass the text truncation entirely.
#[rig_tool(description = "Read a UTF-8 text file (absolute or relative path). \
                   Output is capped at 2000 lines / 50 KiB, whole lines; the \
                   truncation notice carries the offset to continue from. \
                   Optional offset (1-indexed line number) and limit (line \
                   count) page large files. Reading a directory lists its \
                   entries.")]
pub async fn read(
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<String, ToolExecutionError> {
    let meta = std::fs::metadata(&path)
        .map_err(|e| ToolExecutionError::other(format!("cannot read `{path}`: {e}")))?;
    if meta.is_dir() {
        return list_directory(&path);
    }
    let bytes = std::fs::read(&path)
        .map_err(|e| ToolExecutionError::other(format!("cannot read `{path}`: {e}")))?;
    if let Some(encoding) = non_utf8_bom(&bytes) {
        return Err(ToolExecutionError::other(format!(
            "`{path}` is {encoding}-encoded; this tool reads UTF-8 only — \
             re-save the file as UTF-8 or inspect it with the shell tool"
        )));
    }
    let mut text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            return Err(ToolExecutionError::other(format!(
                "`{path}` is not valid UTF-8 ({} bytes); binary files are not \
                 supported by this tool ({error})",
                error.utf8_error().valid_up_to()
            )));
        }
    };
    // Strip a UTF-8 BOM: what read shows must be exactly what edit will
    // match later — one file-reading convention for the text tools.
    if text.starts_with('\u{feff}') {
        text.replace_range(..'\u{feff}'.len_utf8(), "");
    }
    if text.is_empty() {
        return Ok("(file is empty)".to_string());
    }

    let lines = truncate::split_lines(&text);
    let total_lines = lines.len();
    // 1-indexed input, 0-indexed slicing; offset 0 reads as 1.
    let start = offset.unwrap_or(1).saturating_sub(1);
    if start >= total_lines {
        return Err(ToolExecutionError::other(format!(
            "offset {} is beyond the end of `{path}` ({total_lines} lines)",
            start + 1
        )));
    }
    let end = limit
        .map(|l| start.saturating_add(l).min(total_lines))
        .unwrap_or(total_lines);
    let selected = lines
        .get(start..end)
        .ok_or_else(|| ToolExecutionError::other(format!("empty line range {start}..{end}")))?
        .join("\n");

    let trunc = truncate::truncate_head(&selected);
    if trunc.first_line_exceeds_limit {
        let line_no = start + 1;
        return Ok(format!(
            "[Line {line_no} of `{path}` alone exceeds the 50 KiB output \
             limit. Use the shell tool to extract a slice of it (bash: \
             sed -n '{line_no}p' \"{path}\" | head -c 51200).]"
        ));
    }
    let mut out = trunc.content;
    if trunc.truncated {
        let last_shown = start + trunc.output_lines;
        let reason = if trunc.truncated_by == Some(truncate::TruncatedBy::Bytes) {
            " (50 KiB limit)"
        } else {
            ""
        };
        out.push_str(&format!(
            "\n\n[Showing lines {}-{last_shown} of {total_lines}{reason}. \
             Use offset={} to continue.]",
            start + 1,
            last_shown + 1
        ));
    } else if end < total_lines {
        out.push_str(&format!(
            "\n\n[{} more lines in `{path}`. Use offset={} to continue.]",
            total_lines - end,
            end + 1
        ));
    }
    Ok(out)
}

/// A UTF-16/32 byte-order mark, if present — Windows tooling (PowerShell
/// redirects) writes these; naming the encoding beats a bare
/// invalid-UTF-8 report.
fn non_utf8_bom(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xff, 0xfe, 0x00, 0x00, ..] => Some("UTF-32 LE"),
        [0x00, 0x00, 0xfe, 0xff, ..] => Some("UTF-32 BE"),
        [0xff, 0xfe, ..] => Some("UTF-16 LE"),
        [0xfe, 0xff, ..] => Some("UTF-16 BE"),
        _ => None,
    }
}

/// Inline directory listing: a header naming the directory, then sorted
/// entry names with `/` on directories — lean on purpose (no size/mtime
/// columns; bash `ls -la` covers metadata). Capped through the shared
/// truncation.
fn list_directory(path: &str) -> Result<String, ToolExecutionError> {
    let entries = std::fs::read_dir(path)
        .map_err(|e| ToolExecutionError::other(format!("cannot list `{path}`: {e}")))?;
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| ToolExecutionError::other(format!("listing `{path}`: {e}")))?;
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            name.push('/');
        }
        names.push(name);
    }
    names.sort();
    let mut listing = format!("{path} is a directory:\n");
    if names.is_empty() {
        listing.push_str("(empty directory)");
    } else {
        listing.push_str(&names.join("\n"));
    }
    let trunc = truncate::truncate_head(&listing);
    let mut out = trunc.content;
    if trunc.truncated {
        // The header rides line 1 of the listing; entries follow.
        let shown = trunc.output_lines.saturating_sub(1);
        out.push_str(&format!(
            "\n\n[{shown} of {} entries shown; use the shell tool to page \
             the full listing]",
            names.len()
        ));
    }
    Ok(out)
}

/// Ask the user a question and return their answer — the whole body is
/// one interaction roundtrip over the session's
/// [`UserInteraction`](rig_agent::tool::interaction::UserInteraction)
/// capability (ENGINE.md's tool phase: a tool body may ask; this one
/// asks once). Fails in-band when the session has no interactive
/// frontend.
#[rig_tool(
    description = "Ask the user a question and return their answer. Use it when \
                   you need information, a decision, or a confirmation only the \
                   user can provide; do not guess on their behalf. The answer \
                   text is returned verbatim; a dismissed question says so."
)]
pub async fn ask_user(
    #[rig(context)] context: &mut ToolContext,
    question: String,
) -> Result<String, ToolExecutionError> {
    use rig_agent::tool::interaction::UserInteraction;
    let Some(interaction) = context.get::<std::sync::Arc<dyn UserInteraction>>() else {
        return Err(ToolExecutionError::other(
            "this session has no interactive frontend — there is no user to ask; state that and continue with what you have",
        ));
    };
    // An ordinary template consumer: native:ask, payload opaque to the
    // core.
    let payload = serde_json::to_value(tabit_protocol::templates::AskCard { prompt: question })
        .map_err(|error| ToolExecutionError::other(error.to_string()))?;
    Ok(
        match interaction
            .request(tabit_protocol::templates::ui::ASK, payload)
            .await
        {
            rig_agent::tool::interaction::InteractionOutcome::Answered(payload) => {
                match serde_json::from_value::<tabit_protocol::templates::AskAnswer>(payload) {
                    Ok(answer) => answer
                        .text
                        .unwrap_or_else(|| "the user submitted an empty answer".to_string()),
                    Err(_) => "the user's answer could not be read".to_string(),
                }
            }
            rig_agent::tool::interaction::InteractionOutcome::Dismissed => {
                "the user dismissed the question without answering".to_string()
            }
        },
    )
}

/// Run a shell command through bash. On Windows this tool is registered
/// only where a Git-for-Windows install was positively identified at
/// registration ([`shell`]): correctness over coverage — a wrong bash
/// (WSL's launcher, a Cygwin root) is worse than none, so nothing is
/// guessed from a bare `bash.exe` on PATH. Combined output (stdout, then
/// stderr) is capped at [`OUTPUT_CAP_BYTES`]; commands that exceed their
/// timeout, or are cancelled through the run's cancellation token, are
/// killed — process tree included (see the crate-level cancellation
/// contract).
#[rig_tool(description = "Run a shell command and return its combined output. \
                   Commands run through bash (POSIX syntax; on Windows this is \
                   Git Bash). Non-zero exits report the exit code. Output is \
                   capped at 128 KiB; commands time out after 30 seconds \
                   unless timeout_secs says otherwise.")]
pub async fn bash(
    #[rig(context)] context: &mut ToolContext,
    command: String,
    timeout_secs: Option<u64>,
) -> Result<String, ToolExecutionError> {
    let interpreter = shell::bash().map_err(ToolExecutionError::other)?;
    run_shell(&interpreter, context, command, timeout_secs).await
}

/// The PowerShell-dialect counterpart of [`bash`], registered on Windows
/// machines with no verified Git Bash — the model always gets a shell
/// whose dialect matches the tool's description.
#[cfg(windows)]
#[rig_tool(description = "Run a shell command and return its combined output. \
                   Commands run through Windows PowerShell — write PowerShell \
                   syntax (Get-ChildItem, $env:NAME, Select-String, ...). \
                   Non-zero exits report the exit code. Output is capped at \
                   128 KiB; commands time out after 30 seconds unless \
                   timeout_secs says otherwise.")]
pub async fn powershell(
    #[rig(context)] context: &mut ToolContext,
    command: String,
    timeout_secs: Option<u64>,
) -> Result<String, ToolExecutionError> {
    run_shell(&shell::powershell(), context, command, timeout_secs).await
}

/// The shell tool this machine registers: `bash` where a Git-for-Windows
/// install is positively identified (probe-verified absolute `bash.exe`),
/// `powershell` otherwise on Windows, `bash` on Unix. One decision site —
/// the assembly never branches on the platform's shell itself.
pub fn shell_tool() -> DynamicTool {
    #[cfg(windows)]
    return match shell::resolved() {
        shell::Shell::Bash(_) => dynamic_contextual(Bash),
        shell::Shell::Powershell => dynamic_contextual(Powershell),
    };
    #[cfg(not(windows))]
    return dynamic_contextual(Bash);
}

/// The shared execution core of the shell tools: spawn under the resolved
/// interpreter, tree-kill discipline, both deadlines (command timeout +
/// cancellation token), combined output, cap.
async fn run_shell(
    interpreter: &shell::Interpreter,
    context: &mut ToolContext,
    command: String,
    timeout_secs: Option<u64>,
) -> Result<String, ToolExecutionError> {
    let timeout = Duration::from_secs(
        timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS),
    );
    // A pre-cancelled token refuses before spawning: "the command never
    // ran" is structural, not a race against a fast command.
    if context
        .get::<tokio_util::sync::CancellationToken>()
        .is_some_and(|token| token.is_cancelled())
    {
        return Err(ToolExecutionError::other(
            "command was interrupted before starting — it did not run".to_string(),
        ));
    }
    let mut wrapped = process_wrap::std::CommandWrap::with_new(&interpreter.argv0, |cmd| {
        cmd.args(interpreter.args)
            .arg(&command)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());
    });
    // Tree kill: the process group dies with its leader on Unix; the job
    // object takes the whole tree down on Windows. The drop guard below is
    // the drop-without-cancel backstop (the tokio-only KillOnDrop shim is
    // not available in the std flavor).
    #[cfg(unix)]
    wrapped.wrap(process_wrap::std::ProcessGroup::leader());
    #[cfg(windows)]
    wrapped.wrap(process_wrap::std::JobObject);
    let mut child = wrapped.spawn().map_err(|e| {
        ToolExecutionError::other(format!("cannot start `{}`: {e}", interpreter.argv0))
    })?;

    // Both pipes up front: a full undrained pipe would block the child
    // while the other stream is still being read.
    // The pipes were configured on the command; a missing one means the
    // wrapper dropped them — surface it as an external error, not a panic.
    let stdout_pipe = child
        .stdout()
        .take()
        .ok_or_else(|| ToolExecutionError::other("stdout pipe missing after spawn"))?;
    let stderr_pipe = child
        .stderr()
        .take()
        .ok_or_else(|| ToolExecutionError::other("stderr pipe missing after spawn"))?;
    let stdout_reader = spawn_reader(Some(stdout_pipe));
    let stderr_reader = spawn_reader(Some(stderr_pipe));
    let output = run_with_deadlines(
        child,
        timeout,
        context.get::<tokio_util::sync::CancellationToken>(),
        &interpreter.argv0,
        stdout_reader,
        stderr_reader,
    )?;
    let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.stderr.is_empty() {
        combined.push_str("\n--- stderr ---\n");
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    // Keep the tail (errors and final results live at the end); when the
    // head is dropped, spill the full output to a temp file — the dropped
    // bytes are unrecoverable without re-running the command, which may
    // be slow or side-effecting. Spill files are never deleted by us: the
    // path was promised to the model; OS temp hygiene owns it.
    let trunc = truncate::truncate_tail(&combined);
    let mut visible = trunc.content;
    if trunc.truncated {
        let spill = spill_full_output(&combined)?;
        let detail = if trunc.last_line_partial {
            // One line over the whole budget (minified output): say that,
            // not "1 of 1 lines".
            format!(
                "the final line is {} bytes; showing its last {}",
                trunc.total_bytes, trunc.output_bytes
            )
        } else {
            format!(
                "showing the last {} of {} lines ({} of {} bytes)",
                trunc.output_lines, trunc.total_lines, trunc.output_bytes, trunc.total_bytes
            )
        };
        visible.push_str(&format!("\n\n[{detail}. Full output: {}]", spill.display()));
    }
    if output.status.success() {
        Ok(visible)
    } else {
        // The exit status rides structure too (the protocol's
        // `failed { exit_code }`): numeric codes pass through as
        // numbers-in-text, so the frontend colors the row without
        // parsing the prose. Signal kills have no code — the prose
        // already says "abnormal termination".
        let mut error = ToolExecutionError::other(format!(
            "command exited with {}:\n{visible}",
            exit_description(&output.status)
        ));
        if let Some(code) = output.status.code() {
            error = error.with_code(code.to_string());
        }
        Err(error)
    }
}

/// Write oversized command output to a temp file. Ids are pid + a
/// monotonic counter — unique per process, no randomness dependency.
fn spill_full_output(full: &str) -> Result<std::path::PathBuf, ToolExecutionError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SPILL_COUNT: AtomicU64 = AtomicU64::new(0);
    let n = SPILL_COUNT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("tabit-bash-{}-{n}.log", std::process::id()));
    std::fs::write(&path, full).map_err(|e| {
        ToolExecutionError::other(format!(
            "cannot save full output to {}: {e}",
            path.display()
        ))
    })?;
    Ok(path)
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

/// Kills the process tree when dropped while armed — the std-flavor
/// stand-in for process-wrap's tokio-only `KillOnDrop`: if the tool's
/// future is dropped mid-run, the child must not outlive it. Disarmed on
/// every path that observes the exit.
struct TreeKillGuard {
    child: Box<dyn process_wrap::std::ChildWrapper + Send + Sync>,
    armed: bool,
}

impl TreeKillGuard {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// Force-kill the tree and reap it.
    fn kill_tree(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for TreeKillGuard {
    fn drop(&mut self) {
        if self.armed {
            self.kill_tree();
        }
    }
}

/// Wait for the child under both deadlines — the command timeout and the
/// run's cancellation token — force-killing the process *tree* when either
/// fires (no grace period: a killed command's partial effects are reported
/// as an interruption, not cleaned up). Piped output is captured by the
/// reader threads handed in by the caller; the guard's Drop is the
/// cancel-without-poll backstop.
///
/// Hand-rolled over the wrapper's `try_wait` on purpose: this crate is
/// sync, `tokio::process` would drag a runtime into a std-only tool crate,
/// and the poll loop below is the entire algorithm. The kill itself is
/// `process-wrap`'s (`killpg` on Unix, job-object termination on Windows).
fn run_with_deadlines(
    child: Box<dyn process_wrap::std::ChildWrapper + Send + Sync>,
    timeout: Duration,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    argv0: &str,
    stdout_reader: ReaderJoin,
    stderr_reader: ReaderJoin,
) -> Result<CapturedOutput, ToolExecutionError> {
    let mut guard = TreeKillGuard { child, armed: true };
    let deadline = Instant::now() + timeout;
    let status = loop {
        if cancel.is_some_and(|token| token.is_cancelled()) {
            guard.kill_tree();
            return Err(ToolExecutionError::other(
                "command was interrupted before completing — its effects may be                  partial; check before relying on anything it wrote"
                    .to_string(),
            ));
        }
        match guard.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    guard.kill_tree();
                    return Err(ToolExecutionError::other(format!(
                        "command exceeded its {}s timeout and was killed                          (raise timeout_secs if it legitimately needs longer)",
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
    guard.armed = false;
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

/// Erase a contextual `#[rig_tool]` (one taking `#[rig(context)]
/// &mut ToolContext`) into a [`DynamicTool`], cloning the tool per call so
/// the context stays per-dispatch. The contextual counterpart of
/// [`dynamic`].
pub fn dynamic_contextual<T>(tool: T) -> DynamicTool
where
    T: rig_agent::tool::Tool + Send + Sync + 'static,
{
    // One shared instance per call site; contextual tools are stateless
    // (`call` takes `&self`), so no per-call clone is needed.
    let tool = std::sync::Arc::new(tool);
    let name = <T as rig_agent::tool::Tool>::NAME.to_string();
    let description = tool.description();
    let parameters = tool.parameters();
    DynamicTool::new(
        name,
        description,
        parameters,
        move |ctx: &mut ToolContext, args: serde_json::Value| {
            let tool = tool.clone();
            Box::pin(async move {
                let typed: <T as rig_agent::tool::Tool>::Args = serde_json::from_value(args)
                    .map_err(|e| ToolExecutionError::other(format!("invalid arguments: {e}")))?;
                let output = <T as rig_agent::tool::Tool>::call(tool.as_ref(), ctx, typed)
                    .await
                    .map_err(|e| tool.map_error(e))?;
                output.into_tool_output()
            })
        },
    )
}

#[cfg(test)]
mod tests;
