use super::*;
use rig_core::tool::PortableTool;
use std::fs;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir()
        .join("tabit-tools-tests")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn ctx() -> rig_agent::tool::ToolContext {
    rig_agent::tool::ToolContext::new()
}

fn err_text(result: Result<String, ToolExecutionError>) -> String {
    match result {
        Ok(text) => panic!("expected an error, got output: {text}"),
        Err(e) => e.to_string(),
    }
}

/// The error behind a failing result, for structure assertions.
fn err(result: Result<String, ToolExecutionError>) -> ToolExecutionError {
    match result {
        Ok(text) => panic!("expected an error, got output: {text}"),
        Err(e) => e,
    }
}

/// Whether the `bash` tool's dialect is usable here: every Unix, Windows
/// only with a positively identified Git Bash. POSIX-syntax tests gate on
/// it — on a Windows machine without one, the shell tool is `powershell`.
fn bash_dialect_available() -> bool {
    #[cfg(windows)]
    return matches!(shell::resolved(), shell::Shell::Bash(_));
    #[cfg(not(windows))]
    return true;
}

#[tokio::test]
async fn read_returns_file_contents() {
    let dir = temp_dir("read-ok");
    let path = dir.join("note.txt");
    fs::write(&path, "first\nsecond\n").expect("write");
    let out = read(path.to_string_lossy().to_string(), None, None)
        .await
        .expect("read");
    // Line-paged output carries no trailing newline (pi behaves the same).
    assert_eq!(out, "first\nsecond");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn read_errors_are_clear_about_what_and_why() {
    let dir = temp_dir("read-err");
    let missing = err_text(
        read(
            dir.join("nope.txt").to_string_lossy().to_string(),
            None,
            None,
        )
        .await,
    );
    assert!(missing.contains("nope.txt"), "{missing}");
    assert!(missing.contains("cannot read"), "{missing}");

    let bin = dir.join("blob.bin");
    fs::write(&bin, [0xff, 0xfe, 0x00, 0x01]).expect("binary");
    // FF FE is the UTF-16 LE mark — the error names the encoding.
    let utf16 = err_text(read(bin.to_string_lossy().to_string(), None, None).await);
    assert!(utf16.contains("UTF-16"), "{utf16}");

    let raw = dir.join("blob2.bin");
    fs::write(&raw, [0x89, 0x50, 0x4e, 0x47, 0x00, 0x01]).expect("binary");
    let not_utf8 = err_text(read(raw.to_string_lossy().to_string(), None, None).await);
    assert!(not_utf8.contains("UTF-8"), "{not_utf8}");

    let beyond = err_text(read("Cargo.toml".to_string(), Some(10_000), None).await);
    assert!(beyond.contains("beyond the end"), "{beyond}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn read_pages_with_offset_and_limit() {
    let dir = temp_dir("read-page");
    let path = dir.join("lines.txt");
    let body: String = (1..=10).map(|i| format!("line-{i}\n")).collect();
    fs::write(&path, body).expect("write");

    let page = read(path.to_string_lossy().to_string(), Some(3), Some(2))
        .await
        .expect("page");
    assert_eq!(
        page,
        "line-3\nline-4\n\n[6 more lines in `...`. Use offset=5 to continue.]"
            .replace("...", &path.to_string_lossy())
    );

    // 1-indexed offset, and offset=0 reads as 1.
    let first = read(path.to_string_lossy().to_string(), Some(0), Some(1))
        .await
        .expect("first");
    assert_eq!(
        first,
        "line-1\n\n[9 more lines in `...`. Use offset=2 to continue.]"
            .replace("...", &path.to_string_lossy())
    );
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn read_truncates_on_whole_lines_with_a_continuation_offset() {
    let dir = temp_dir("read-cap");
    let path = dir.join("big.txt");
    let body: String = (1..=truncate::MAX_LINES + 100)
        .map(|i| format!("row-{i}\n"))
        .collect();
    fs::write(&path, body).expect("write");

    let out = read(path.to_string_lossy().to_string(), None, None)
        .await
        .expect("read");
    assert!(
        out.contains(&format!(
            "[Showing lines 1-{} of {}. Use offset={} to continue.]",
            truncate::MAX_LINES,
            truncate::MAX_LINES + 100,
            truncate::MAX_LINES + 1
        )),
        "notice carries the next offset: ...{}",
        &out[out.len().saturating_sub(140)..]
    );
    // Whole lines only: the shown content is a prefix of the file.
    assert!(out.starts_with("row-1\nrow-2\n"));
    assert!(out.contains(&format!("row-{}\n", truncate::MAX_LINES)));
    assert!(!out.contains(&format!("row-{}", truncate::MAX_LINES + 1)));
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn read_of_a_minified_line_points_at_the_shell() {
    let dir = temp_dir("read-minified");
    let path = dir.join("min.js");
    fs::write(&path, "x".repeat(truncate::MAX_BYTES + 1)).expect("write");
    let out = read(path.to_string_lossy().to_string(), None, None)
        .await
        .expect("read");
    assert!(
        out.contains("exceeds the 50 KiB output limit"),
        "the model is told why there is no content: {out}"
    );
    assert!(out.contains("shell tool"), "{out}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn read_lists_directories_inline() {
    let dir = temp_dir("read-dir");
    fs::write(dir.join("a.txt"), "a").expect("write");
    fs::create_dir(dir.join("sub")).expect("subdir");

    let out = read(dir.to_string_lossy().to_string(), None, None)
        .await
        .expect("directory reads list entries");
    assert!(out.contains("is a directory"), "{out}");
    assert!(out.contains("a.txt"), "{out}");
    assert!(out.contains("sub/"), "directories carry a slash: {out}");
    let a = out.find("a.txt").expect("a.txt");
    let s = out.find("sub/").expect("sub/");
    assert!(a < s, "entries sorted: {out}");

    let empty = temp_dir("read-dir-empty");
    let out = read(empty.to_string_lossy().to_string(), None, None)
        .await
        .expect("empty directory");
    assert!(out.contains("(empty directory)"), "{out}");
    fs::remove_dir_all(&dir).ok();
    fs::remove_dir_all(&empty).ok();
}

#[tokio::test]
async fn read_names_empty_files_and_strips_the_bom() {
    let dir = temp_dir("read-empty-bom");
    let empty = dir.join("empty.txt");
    fs::write(&empty, "").expect("write");
    let out = read(empty.to_string_lossy().to_string(), None, None)
        .await
        .expect("empty read");
    assert_eq!(out, "(file is empty)");

    let bom = dir.join("bom.txt");
    fs::write(&bom, "\u{feff}content\n").expect("write");
    let out = read(bom.to_string_lossy().to_string(), None, None)
        .await
        .expect("bom read");
    assert_eq!(out, "content", "BOM stripped: {out:?}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn write_creates_new_files_and_names_parents() {
    let dir = temp_dir("write-new");
    let target = dir.join("a").join("b").join("f.rs");
    let out = write(
        target.to_string_lossy().to_string(),
        "fn main() {}\n".to_string(),
        None,
    )
    .await
    .expect("write");
    assert!(out.starts_with("Created "), "{out}");
    assert!(out.contains("created 2 parent dirs"), "{out}");
    assert_eq!(
        fs::read_to_string(&target).expect("read back"),
        "fn main() {}\n"
    );
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn write_refuses_an_existing_file_without_the_flag() {
    let dir = temp_dir("write-refuse");
    let target = dir.join("keep.txt");
    fs::write(&target, "precious").expect("seed");

    // No flag and false both refuse; the file is untouched.
    for flag in [None, Some(false)] {
        let error = err_text(
            write(
                target.to_string_lossy().to_string(),
                "gone".to_string(),
                flag,
            )
            .await,
        );
        assert!(error.contains("already exists (8 bytes)"), "{error}");
        assert!(error.contains("overwrite: true"), "{error}");
        assert_eq!(fs::read_to_string(&target).expect("untouched"), "precious");
    }

    let out = write(
        target.to_string_lossy().to_string(),
        "replacement".to_string(),
        Some(true),
    )
    .await
    .expect("overwrite");
    assert_eq!(
        out,
        format!("Overwrote {} (11 bytes, was 8 bytes)", target.display())
    );
    assert_eq!(
        fs::read_to_string(&target).expect("read back"),
        "replacement"
    );
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn write_fails_loudly_on_directory_and_file_parent() {
    let dir = temp_dir("write-kinds");
    let sub = dir.join("subdir");
    fs::create_dir(&sub).expect("dir");
    let error = err_text(write(sub.to_string_lossy().to_string(), "x".to_string(), None).await);
    assert!(error.contains("is a directory"), "{error}");

    let blocker = dir.join("blocker");
    fs::write(&blocker, "file").expect("seed");
    let target = blocker.join("child.txt");
    let error = err_text(write(target.to_string_lossy().to_string(), "x".to_string(), None).await);
    assert!(error.contains("not a directory"), "{error}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn writes_to_one_path_serialize_through_the_lock() {
    let dir = temp_dir("write-serial");
    let target = dir.join("contended.txt");
    // With the flag off, an overwrite must fail — so if N concurrent
    // writes all target one path and the lock serializes them, exactly
    // one wins and N-1 fail cleanly. Without the lock the interleavings
    // are racy; with it, the outcome is deterministic.
    let mut handles = Vec::new();
    for i in 0..4 {
        let path = target.to_string_lossy().to_string();
        handles.push(tokio::spawn(async move {
            write(path, format!("winner-{i}"), None).await
        }));
    }
    let mut ok = 0;
    let mut refused = 0;
    for h in handles {
        match h.await.expect("join") {
            Ok(_) => ok += 1,
            Err(e) => {
                assert!(e.to_string().contains("already exists"), "{e}");
                refused += 1;
            }
        }
    }
    assert_eq!((ok, refused), (1, 3), "exactly one writer wins one path");
    let content = fs::read_to_string(&target).expect("read back");
    assert!(content.starts_with("winner-"), "{content}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn bash_runs_a_command_and_captures_output() {
    if !bash_dialect_available() {
        eprintln!("skipped: no verified Git Bash on this machine");
        return;
    }
    let out = bash(&mut ctx(), "echo tabit-smoke".to_string(), None)
        .await
        .expect("bash");
    assert!(
        out.trim().contains("tabit-smoke"),
        "echo output captured: {out}"
    );
}

#[tokio::test]
async fn bash_reports_nonzero_exits_with_output() {
    if !bash_dialect_available() {
        eprintln!("skipped: no verified Git Bash on this machine");
        return;
    }
    let failure = err(bash(&mut ctx(), "exit 3".to_string(), None).await);
    let error = failure.to_string();
    assert!(error.contains("status 3"), "{error}");
    assert_eq!(
        failure.code(),
        Some("3"),
        "the exit status rides structure (the protocol's exit_code), \
         not just prose"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_reports_nonzero_exits_with_output() {
    let failure = err(powershell(&mut ctx(), "exit 3".to_string(), None).await);
    let error = failure.to_string();
    assert!(error.contains("status 3"), "{error}");
    assert_eq!(
        failure.code(),
        Some("3"),
        "the exit status rides structure (the protocol's exit_code), \
         not just prose"
    );
}

#[tokio::test]
async fn bash_kills_commands_that_exceed_their_timeout() {
    if !bash_dialect_available() {
        eprintln!("skipped: no verified Git Bash on this machine");
        return;
    }
    let error = err_text(bash(&mut ctx(), "sleep 30".to_string(), Some(1)).await);
    assert!(error.contains("timeout"), "{error}");
    assert!(error.contains("killed"), "{error}");
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_kills_commands_that_exceed_their_timeout() {
    let error =
        err_text(powershell(&mut ctx(), "Start-Sleep -Seconds 30".to_string(), Some(1)).await);
    assert!(error.contains("timeout"), "{error}");
    assert!(error.contains("killed"), "{error}");
}

#[tokio::test]
async fn bash_missing_program_is_a_clear_error() {
    if !bash_dialect_available() {
        eprintln!("skipped: no verified Git Bash on this machine");
        return;
    }
    let error = err_text(
        bash(
            &mut ctx(),
            "this-command-does-not-exist-xyz".to_string(),
            None,
        )
        .await,
    );
    assert!(
        error.contains("command exited"),
        "failure surfaces the shell's own diagnosis: {error}"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_missing_program_is_a_clear_error() {
    let error = err_text(
        powershell(
            &mut ctx(),
            "this-command-does-not-exist-xyz".to_string(),
            None,
        )
        .await,
    );
    assert!(
        error.contains("command exited") || error.contains("not recognized"),
        "failure surfaces the shell's own diagnosis: {error}"
    );
}

#[tokio::test]
async fn portable_structs_are_named_and_erased_correctly() {
    assert_eq!(<Read as PortableTool>::NAME, "read");
    // bash is contextual (it takes the cancellation-bearing ToolContext).
    assert_eq!(<Bash as rig_agent::tool::Tool>::NAME, "bash");
    #[cfg(windows)]
    assert_eq!(<Powershell as rig_agent::tool::Tool>::NAME, "powershell");

    let mut set = rig_agent::tool::ToolSet::default();
    set.add_dynamic_tool(dynamic(Read));
    set.add_dynamic_tool(dynamic_contextual(Bash));
    let defs = set.get_tool_definitions();
    let read_def = defs.iter().find(|d| d.name == "read").expect("read def");
    assert!(read_def.description.contains("UTF-8"));
    assert_eq!(read_def.parameters["properties"]["path"]["type"], "string");
    let bash_def = defs.iter().find(|d| d.name == "bash").expect("bash def");
    let required = bash_def.parameters["required"].as_array();
    assert!(
        required.is_none_or(|r| r.iter().all(|v| v != "timeout_secs")),
        "timeout_secs must be optional: {:?}",
        required
    );
}

/// The registration decision is total: exactly one shell tool, named for
/// the dialect this machine resolved (never a `bash` tool that secretly
/// runs PowerShell).
#[tokio::test]
async fn shell_tool_registers_the_resolved_dialect() {
    let mut set = rig_agent::tool::ToolSet::default();
    set.add_dynamic_tool(shell_tool());
    let defs = set.get_tool_definitions();
    assert_eq!(defs.len(), 1, "one shell tool, not a set: {defs:?}");

    #[cfg(windows)]
    let expected = match shell::resolved() {
        shell::Shell::Bash(_) => "bash",
        shell::Shell::Powershell => "powershell",
    };
    #[cfg(not(windows))]
    let expected = "bash";
    assert_eq!(defs[0].name, expected);

    // The description must name the dialect the model is writing in.
    if expected == "powershell" {
        assert!(defs[0].description.contains("PowerShell"));
    } else {
        assert!(defs[0].description.contains("bash"));
    }
}

#[tokio::test]
async fn dynamic_tool_executes_the_portable_body() {
    let mut set = rig_agent::tool::ToolSet::default();
    set.add_dynamic_tool(dynamic(Read));
    let dir = temp_dir("dyn-read");
    fs::write(dir.join("f.txt"), "via-dynamic").expect("write");
    let mut ctx = ToolContext::default();
    let result = set
        .execute(
            "read",
            serde_json::json!({"path": dir.join("f.txt").to_string_lossy()}).to_string(),
            &mut ctx,
        )
        .await;
    assert!(result.is_success(), "dynamic call: {:?}", result);
    let text = result.output().as_text().unwrap_or_default();
    assert!(text.contains("via-dynamic"), "{text}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn bash_overflow_keeps_the_tail_and_spills_the_full_output() {
    if !bash_dialect_available() {
        eprintln!("skipped: no verified Git Bash on this machine");
        return;
    }
    // 6000 rows: over the 2000-line cap, far under the byte cap.
    let out = bash(
        &mut ctx(),
        "for i in $(seq 1 6000); do echo \"row-$i\"; done".to_string(),
        None,
    )
    .await
    .expect("bash");
    assert!(
        out.contains("Full output:"),
        "notice points at the spill: ...{out}"
    );
    // The visible window is the tail — late rows present, early rows gone.
    assert!(
        out.starts_with("row-4001"),
        "tail window: {}...",
        &out[..40]
    );
    assert!(out.contains("row-6000"));
    assert!(
        !out.contains("\nrow-42\n"),
        "dropped head must stay dropped"
    );

    let spill = out
        .split("Full output: ")
        .nth(1)
        .expect("spill path")
        .trim_end_matches(']');
    let full = fs::read_to_string(spill).expect("spill file exists");
    assert!(full.starts_with("row-1\n"), "spill holds the full stream");
    assert!(full.contains("row-6000"));
    fs::remove_file(spill).ok();
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_overflow_keeps_the_tail_and_spills_the_full_output() {
    let out = powershell(
        &mut ctx(),
        "1..6000 | ForEach-Object { \"row-$_\" }".to_string(),
        None,
    )
    .await
    .expect("powershell");
    assert!(out.contains("Full output:"), "...{out}");
    assert!(out.contains("row-6000"));
    let spill = out
        .split("Full output: ")
        .nth(1)
        .expect("spill path")
        .trim_end_matches(']');
    let full = fs::read_to_string(spill).expect("spill file exists");
    assert!(full.contains("row-1"), "spill holds the full stream");
    fs::remove_file(spill).ok();
}

#[tokio::test]
async fn pre_cancelled_bash_never_runs() {
    if !bash_dialect_available() {
        eprintln!("skipped: no verified Git Bash on this machine");
        return;
    }
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let mut context = rig_agent::tool::ToolContext::new();
    context.insert(token);
    let error = bash(&mut context, "echo must-not-run".to_string(), None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("did not run"), "{error}");
}

// --- ask_user: the shipped tool against a scripted interaction capability ---

use futures::future::BoxFuture;
use rig_agent::tool::interaction::{InteractionOutcome, UserInteraction};

/// A capability double that answers every ask with one canned outcome ---
/// the shipped tool's mapping is the DUT, not the roundtrip.
struct ScriptedInteraction(InteractionOutcome);

impl UserInteraction for ScriptedInteraction {
    fn request(
        &self,
        _ui_type: &str,
        _payload: serde_json::Value,
    ) -> BoxFuture<'static, InteractionOutcome> {
        let reply = self.0.clone();
        Box::pin(async move { reply })
    }
}

async fn ask_with(outcome: InteractionOutcome) -> Result<String, ToolExecutionError> {
    let mut context = ctx();
    let capability: std::sync::Arc<dyn UserInteraction> =
        std::sync::Arc::new(ScriptedInteraction(outcome));
    context.insert(capability);
    <AskUser as rig_agent::tool::Tool>::call(
        &AskUser,
        &mut context,
        AskUserParameters {
            question: "which file should I edit?".to_string(),
        },
    )
    .await
}

#[tokio::test]
async fn ask_user_returns_free_text_verbatim() {
    let reply = ask_with(InteractionOutcome::Answered(serde_json::json!({
        "text": "main.rs"
    })))
    .await
    .expect("an answered ask succeeds");
    assert_eq!(reply, "main.rs");
}

#[tokio::test]
async fn ask_user_reports_a_dismissal_in_band() {
    let reply = ask_with(InteractionOutcome::Dismissed)
        .await
        .expect("a dismissed ask is not an error — the model is told");
    assert_eq!(reply, "the user dismissed the question without answering");
}

#[tokio::test]
async fn ask_user_without_a_frontend_fails_in_band() {
    let mut context = ctx();
    let error = <AskUser as rig_agent::tool::Tool>::call(
        &AskUser,
        &mut context,
        AskUserParameters {
            question: "anyone there?".to_string(),
        },
    )
    .await
    .expect_err("no capability in the context must fail the call");
    let message = error.to_string();
    assert!(
        message.contains("no interactive frontend"),
        "the error must tell the model why: {message}"
    );
}
