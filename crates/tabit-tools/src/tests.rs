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

/// The (text, details) split of a successful multi-part tool result.
fn split_parts(
    result: rig_core::OneOrMany<rig_core::message::ToolResultContent>,
) -> (String, Option<serde_json::Value>) {
    let mut text = String::new();
    let mut details = None;
    for part in result {
        if let Some(t) = part.as_text() {
            text.push_str(t);
        }
        if let Some(j) = part.as_json() {
            details = Some(j.clone());
        }
    }
    (text, details)
}

/// The text of a successful `read` (text reads are single-part).
async fn read_out(path: String, offset: Option<usize>, limit: Option<usize>) -> String {
    split_parts(read(path, offset, limit).await.expect("read")).0
}

fn err_text<T>(result: Result<T, ToolExecutionError>) -> String
where
    T: std::fmt::Debug,
{
    match result {
        Ok(value) => panic!("expected an error, got output: {value:?}"),
        Err(e) => e.to_string(),
    }
}

/// The error behind a failing result, for structure assertions.
fn err<T>(result: Result<T, ToolExecutionError>) -> ToolExecutionError
where
    T: std::fmt::Debug,
{
    match result {
        Ok(value) => panic!("expected an error, got output: {value:?}"),
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
    let out = read_out(path.to_string_lossy().to_string(), None, None).await;
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

    // The promise: the page's content, and a continuation offset that
    // is exactly the next unshown line — wording stays delivery.
    let page = read_out(path.to_string_lossy().to_string(), Some(3), Some(2)).await;
    assert!(page.starts_with("line-3\nline-4"), "{page}");
    assert!(page.contains("offset=5"), "continuation offset: {page}");

    // 1-indexed offset, and offset=0 reads as 1.
    let first = read_out(path.to_string_lossy().to_string(), Some(0), Some(1)).await;
    assert!(first.starts_with("line-1"), "{first}");
    assert!(first.contains("offset=2"), "{first}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn read_truncates_on_whole_lines_with_a_continuation_offset() {
    let dir = temp_dir("read-cap");
    let path = dir.join("big.txt");
    // Rows well past the byte cap, all identical width so the boundary
    // line is computable: 6-byte rows ("row-N\n"), so the cap holds
    // READ_MAX_BYTES/6 whole rows.
    let rows = truncate::READ_MAX_BYTES / 3;
    let body: String = (1..=rows).map(|i| format!("r{i:04}\n")).collect();
    fs::write(&path, body).expect("write");

    let out = read_out(path.to_string_lossy().to_string(), None, None).await;
    let shown = truncate::READ_MAX_BYTES / 6;
    assert!(
        out.contains(&format!(
            "[Showing lines 1-{shown} of {rows}. Use offset={} to continue.]",
            shown + 1
        )),
        "notice carries the next offset: ...{}",
        &out[out.len().saturating_sub(140)..]
    );
    // Whole lines only: the shown content is a prefix of the file.
    assert!(out.starts_with("r0001\nr0002\n"));
    assert!(out.contains(&format!("r{shown:04}\n")));
    assert!(!out.contains(&format!("r{:04}", shown + 1)));
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn read_of_a_minified_line_points_at_the_shell() {
    let dir = temp_dir("read-minified");
    let path = dir.join("min.js");
    fs::write(&path, "x".repeat(truncate::READ_MAX_BYTES + 1)).expect("write");
    let out = read_out(path.to_string_lossy().to_string(), None, None).await;
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

    let out = read_out(dir.to_string_lossy().to_string(), None, None).await;
    assert!(out.contains("is a directory"), "{out}");
    assert!(out.contains("a.txt"), "{out}");
    assert!(out.contains("sub/"), "directories carry a slash: {out}");
    let a = out.find("a.txt").expect("a.txt");
    let s = out.find("sub/").expect("sub/");
    assert!(a < s, "entries sorted: {out}");

    let empty = temp_dir("read-dir-empty");
    let out = read_out(empty.to_string_lossy().to_string(), None, None).await;
    assert!(out.contains("(empty directory)"), "{out}");
    fs::remove_dir_all(&dir).ok();
    fs::remove_dir_all(&empty).ok();
}

#[tokio::test]
async fn read_names_empty_files_and_strips_the_bom() {
    let dir = temp_dir("read-empty-bom");
    let empty = dir.join("empty.txt");
    fs::write(&empty, "").expect("write");
    let out = read_out(empty.to_string_lossy().to_string(), None, None).await;
    assert_eq!(out, "(file is empty)");

    let bom = dir.join("bom.txt");
    fs::write(&bom, "\u{feff}content\n").expect("write");
    let out = read_out(bom.to_string_lossy().to_string(), None, None).await;
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
    // The branch is named (Created vs Overwrote) — the sentence is
    // delivery; the file's new content is the real check.
    assert!(out.starts_with("Overwrote "), "{out}");
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
    let (out, details) = split_parts(
        bash(&mut ctx(), "echo tabit-smoke".to_string(), None)
            .await
            .expect("bash"),
    );
    assert!(
        out.trim().contains("tabit-smoke"),
        "echo output captured: {out}"
    );
    assert!(details.is_none(), "no truncation, no details part");
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
async fn bash_overflow_keeps_both_ends_and_spills_the_full_output() {
    if !bash_dialect_available() {
        eprintln!("skipped: no verified Git Bash on this machine");
        return;
    }
    // 9000 rows, ~70 KiB — well over the 16 KiB shell cap.
    let (out, details) = split_parts(
        bash(
            &mut ctx(),
            "for i in $(seq 1 9000); do echo \"row-$i\"; done".to_string(),
            None,
        )
        .await
        .expect("bash"),
    );
    assert!(
        out.contains("Full output:"),
        "notice points at the spill: ...{out}"
    );
    // Both ends visible: the first rows AND the last rows, middle gone.
    assert!(out.starts_with("row-1\n"), "head kept: {}...", &out[..40]);
    assert!(out.contains("row-9000"), "tail kept");
    assert!(
        out.contains("lines omitted"),
        "the omission is marked: {out}"
    );
    let mid_marker = out.find("lines omitted").expect("marker");
    assert!(
        !out[mid_marker..].contains("row-42\n"),
        "the middle stays dropped"
    );

    let spill = out
        .split("Full output: ")
        .nth(1)
        .expect("spill path")
        .trim_end_matches(']');
    let full = fs::read_to_string(spill).expect("spill file exists");
    assert!(full.starts_with("row-1\n"), "spill holds the full stream");
    assert!(full.contains("row-9000"));

    // details carries the same facts, structured: the spill path is the
    // frontend's handle on the full output (owner ruling).
    let details = details.expect("truncated output carries details");
    assert_eq!(details["truncated"], true);
    assert_eq!(details["spill_path"], spill);
    assert_eq!(details["total_lines"], 9000);
    assert_eq!(
        details["omitted_lines"],
        details["total_lines"].as_u64().unwrap() - details["output_lines"].as_u64().unwrap()
    );
    fs::remove_file(spill).ok();
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_overflow_keeps_both_ends_and_spills_the_full_output() {
    let (out, details) = split_parts(
        powershell(
            &mut ctx(),
            "1..9000 | ForEach-Object { \"row-$_\" }".to_string(),
            None,
        )
        .await
        .expect("powershell"),
    );
    assert!(out.contains("Full output:"), "...{out}");
    // PowerShell emits CRLF — the head is kept either way.
    assert!(out.starts_with("row-1"), "head kept: {}...", &out[..40]);
    assert!(out.contains("row-9000"), "tail kept");
    let spill = out
        .split("Full output: ")
        .nth(1)
        .expect("spill path")
        .trim_end_matches(']');
    let full = fs::read_to_string(spill).expect("spill file exists");
    assert!(full.contains("row-1"), "spill holds the full stream");
    let details = details.expect("truncated output carries details");
    assert_eq!(details["spill_path"], spill);
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
    assert!(reply.contains("dismissed"), "{reply}");
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

// --- edit: the contract, written before the implementation ---

fn rep(old_text: &str, new_text: &str) -> EditReplacement {
    EditReplacement {
        old_text: old_text.to_string(),
        new_text: new_text.to_string(),
    }
}

fn seed(tag: &str, name: &str, content: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = temp_dir(tag);
    let path = dir.join(name);
    fs::write(&path, content).expect("seed");
    (dir, path)
}

fn body(path: &std::path::Path) -> String {
    fs::read_to_string(path).expect("read back")
}

/// The model-facing report of an edit outcome (the faithful copy; the
/// details cargo is asserted separately).
fn report(outcome: EditOutcome) -> String {
    outcome.report
}

#[tokio::test]
async fn edit_replaces_one_unique_match() {
    let (dir, path) = seed("edit-basic", "f.txt", "alpha\nbeta\ngamma\n");
    let out = report(edit_core(&path.to_string_lossy(), &[rep("beta", "BETA")]).expect("edit"));
    assert!(out.starts_with("Edited "), "{out}");
    assert!(out.contains("1 block"), "{out}");
    assert_eq!(body(&path), "alpha\nBETA\ngamma\n");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_reports_line_deltas() {
    let (dir, path) = seed("edit-delta", "f.txt", "one\ntwo\nthree\n");
    let out = report(
        edit_core(
            &path.to_string_lossy(),
            &[rep("two", "two\ntwo-and-a-half")],
        )
        .expect("edit"),
    );
    assert!(out.contains("+1"), "line delta in the result: {out}");
    assert!(out.contains("-0"), "removals named too: {out}");
    // The first changed line is reported so the model can page a re-read.
    assert!(out.contains("line 2"), "{out}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_applies_disjoint_edits_against_the_original() {
    let (dir, path) = seed("edit-multi", "f.txt", "a1\nb2\nc3\nd4\n");
    let out = report(
        edit_core(&path.to_string_lossy(), &[rep("b2", "B2"), rep("d4", "D4")]).expect("edit"),
    );
    assert!(out.contains("2 blocks"), "{out}");
    assert_eq!(body(&path), "a1\nB2\nc3\nD4\n");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_is_partial_by_design_and_names_failures() {
    let (dir, path) = seed(
        "edit-partial",
        "f.txt",
        "keep-one\ntwice\ntwice\nkeep-two\n",
    );
    let out = report(
        edit_core(
            &path.to_string_lossy(),
            &[
                rep("keep-one", "KEEP-ONE"), // applies
                rep("missing", "x"),         // not found
                rep("twice", "x"),           // duplicate
                rep("keep-two", "KEEP-TWO"), // applies
            ],
        )
        .expect("partial application is a result, not an error"),
    );
    assert!(out.contains("2 of 4"), "{out}");
    assert!(out.contains("edit[1]"), "failed edit indexed: {out}");
    assert!(out.contains("not found"), "{out}");
    assert!(out.contains("edit[2]"), "{out}");
    assert!(out.contains("2 occurrences"), "{out}");
    assert_eq!(body(&path), "KEEP-ONE\ntwice\ntwice\nKEEP-TWO\n");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_with_no_successes_is_an_error_and_touches_nothing() {
    let (dir, path) = seed("edit-allfail", "f.txt", "same\nsame\n");
    let before = body(&path);
    let error = err_text(edit_core(
        &path.to_string_lossy(),
        &[rep("same", "x"), rep("nope", "y")],
    ));
    assert!(error.contains("edit[0]"), "{error}");
    assert!(error.contains("edit[1]"), "{error}");
    assert_eq!(body(&path), before, "no partial writes on total failure");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_empty_old_text_is_rejected() {
    let (dir, path) = seed("edit-empty", "f.txt", "content\n");
    let out = report(
        edit_core(
            &path.to_string_lossy(),
            &[rep("", "x"), rep("content", "CONTENT")],
        )
        .expect("partial: the empty one fails, the other applies"),
    );
    assert!(out.contains("edit[0]"), "{out}");
    assert!(out.contains("empty"), "{out}");
    assert_eq!(body(&path), "CONTENT\n");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_normalizes_crlf_and_preserves_the_files_endings() {
    let (dir, path) = seed("edit-crlf", "win.txt", "alpha\r\nbeta\r\ngamma\r\n");
    // The model supplies LF old_text (from read, which shows LF); the
    // file is CRLF. The match must land, and the file must stay CRLF.
    let out = report(edit_core(&path.to_string_lossy(), &[rep("beta", "BETA")]).expect("edit"));
    assert!(out.contains("1 block"), "{out}");
    assert_eq!(body(&path), "alpha\r\nBETA\r\ngamma\r\n");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_preserves_crlf_even_when_the_replacement_introduces_lf_lines() {
    let (dir, path) = seed("edit-crlf-new", "win.txt", "start\r\nend\r\n");
    let out = report(
        edit_core(&path.to_string_lossy(), &[rep("start", "start\ninserted")]).expect("edit"),
    );
    assert!(out.contains("1 block"), "{out}");
    // New lines from the replacement take the file's dominant ending.
    assert_eq!(body(&path), "start\r\ninserted\r\nend\r\n");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_preserves_the_bom() {
    let (dir, path) = seed("edit-bom", "bom.txt", "\u{feff}alpha\nbeta\n");
    let out = report(edit_core(&path.to_string_lossy(), &[rep("beta", "BETA")]).expect("edit"));
    assert!(out.contains("1 block"), "{out}");
    let bytes = fs::read(&path).expect("read back");
    assert!(
        bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
        "BOM preserved: {bytes:?}"
    );
    assert_eq!(String::from_utf8_lossy(&bytes), "\u{feff}alpha\nBETA\n");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_never_matches_across_the_bom() {
    // old_text containing the BOM char should not be required — the BOM
    // is invisible to read, so it is invisible to edit (matching runs on
    // the BOM-stripped text).
    let (dir, path) = seed("edit-bom-match", "bom.txt", "\u{feff}alpha\n");
    let out = report(edit_core(&path.to_string_lossy(), &[rep("alpha", "ALPHA")]).expect("edit"));
    assert!(out.contains("1 block"), "{out}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_rejects_conflicting_overlaps_as_a_pair() {
    let (dir, path) = seed("edit-conflict", "f.txt", "aa bb cc dd\n");
    // Nothing applies (the whole call conflicts), so the report rides
    // the error — still naming both edits.
    let error = err_text(edit_core(
        &path.to_string_lossy(),
        &[rep("aa bb cc", "AA BB CC"), rep("bb cc dd", "different")],
    ));
    assert!(error.contains("edit[0]"), "{error}");
    assert!(error.contains("edit[1]"), "{error}");
    assert!(error.contains("conflict"), "{error}");
    assert_eq!(
        body(&path),
        "aa bb cc dd\n",
        "neither side of a conflict lands"
    );
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_accepts_identical_overlaps_as_agreement() {
    // The same replacement stated twice (a model retrying a padded
    // context, or an LLM emitting a duplicate block) agrees on the
    // shared region by construction: both apply, the change lands once.
    let (dir, path) = seed("edit-compat", "f.txt", "one two three four\n");
    let out = report(
        edit_core(
            &path.to_string_lossy(),
            &[
                rep("one two three", "one TWO three"),
                rep("one two three", "one TWO three"),
            ],
        )
        .expect("identical overlap applies"),
    );
    assert!(out.contains("2 of 2"), "{out}");
    assert_eq!(body(&path), "one TWO three four\n");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_no_change_is_an_error() {
    let (dir, path) = seed("edit-noop", "f.txt", "same\n");
    let error = err(edit_core(&path.to_string_lossy(), &[rep("same", "same")]));
    assert!(error.to_string().contains("no change"), "{error}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_missing_file_and_directory_fail_loudly() {
    let dir = temp_dir("edit-kinds");
    let missing = err_text(edit_core(
        &dir.join("nope.txt").to_string_lossy(),
        &[rep("x", "y")],
    ));
    assert!(
        missing.contains("cannot read") || missing.contains("not found"),
        "{missing}"
    );

    let sub = dir.join("adir");
    fs::create_dir(&sub).expect("dir");
    let is_dir = err_text(edit_core(&sub.to_string_lossy(), &[rep("x", "y")]));
    assert!(is_dir.contains("directory"), "{is_dir}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_rejects_binary_files() {
    let (dir, path) = seed("edit-binary", "blob.bin", "ok");
    fs::write(&path, [0x89, 0x50, 0x00, 0xff]).expect("binary");
    let error = err_text(edit_core(&path.to_string_lossy(), &[rep("x", "y")]));
    assert!(error.contains("UTF-8"), "{error}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_serializes_concurrent_calls_on_one_path() {
    let (dir, path) = seed("edit-serial", "f.txt", "alpha\nbeta\ngamma\ndelta\n");
    let p = || path.to_string_lossy().to_string();
    let (r1, r2) = tokio::join!(
        edit(p(), vec![rep("beta", "BETA")]),
        edit(p(), vec![rep("delta", "DELTA")]),
    );
    assert!(r1.is_ok(), "{r1:?}");
    assert!(r2.is_ok(), "{r2:?}");
    assert_eq!(body(&path), "alpha\nBETA\ngamma\nDELTA\n");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_matches_spanning_multiple_lines() {
    let (dir, path) = seed(
        "edit-multiline",
        "f.txt",
        "fn main() {\n    old_call();\n}\n",
    );
    let out = report(
        edit_core(
            &path.to_string_lossy(),
            &[rep("    old_call();\n}", "    new_call();\n    log();\n}")],
        )
        .expect("edit"),
    );
    assert!(out.contains("1 block"), "{out}");
    assert_eq!(body(&path), "fn main() {\n    new_call();\n    log();\n}\n");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_counts_occurrences_in_lf_space_for_crlf_files() {
    // "x" appears twice in a CRLF file — the duplicate count must see
    // both, i.e. matching runs on the normalized content.
    let (dir, path) = seed("edit-crlf-dup", "win.txt", "x\r\nx\r\n");
    let out = report(
        edit_core(
            &path.to_string_lossy(),
            &[rep("x", "y"), rep("x\r\nx", "y")],
        )
        .expect("partial report"),
    );
    assert!(out.contains("2 occurrences"), "{out}");
    assert!(out.contains("1 of 2"), "the multi-line match lands: {out}");
    fs::remove_dir_all(&dir).ok();
}

// --- coverage fills (the post-tool coverage round) ---

#[tokio::test]
async fn read_names_utf32_encodings() {
    let dir = temp_dir("read-utf32");
    let le = dir.join("le.txt");
    fs::write(&le, [0xff, 0xfe, 0x00, 0x00, 0x41, 0x00, 0x00, 0x00]).expect("write");
    let error = err_text(read(le.to_string_lossy().to_string(), None, None).await);
    assert!(error.contains("UTF-32 LE"), "{error}");

    let be = dir.join("be.txt");
    fs::write(&be, [0x00, 0x00, 0xfe, 0xff, 0x00, 0x00, 0x00, 0x41]).expect("write");
    let error = err_text(read(be.to_string_lossy().to_string(), None, None).await);
    assert!(error.contains("UTF-32 BE"), "{error}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn read_names_utf16_be() {
    // FE FF is the UTF-16 big-endian mark — named, not a bare
    // invalid-UTF-8 report.
    let dir = temp_dir("read-utf16be");
    let path = dir.join("be.txt");
    fs::write(&path, [0xfe, 0xff, 0x00, 0x41]).expect("write");
    let error = err_text(read(path.to_string_lossy().to_string(), None, None).await);
    assert!(error.contains("UTF-16 BE"), "{error}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn read_byte_truncation_says_so() {
    let dir = temp_dir("read-bytecap");
    let path = dir.join("dense.txt");
    // Few lines, huge bytes: the cap truncates, and the notice carries
    // the continuation offset — the only promise now that bytes are
    // the single limit.
    let line = "x".repeat(truncate::READ_MAX_BYTES / 4);
    let body = (0..4).map(|_| line.clone()).collect::<Vec<_>>().join("\n");
    fs::write(&path, body).expect("write");
    let out = read_out(path.to_string_lossy().to_string(), None, None).await;
    assert!(out.contains("Showing lines"), "{out}");
    assert!(out.contains("Use offset="), "continuation offset: {out}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn read_big_directory_listing_is_truncated_with_a_notice() {
    let dir = temp_dir("read-bigdir");
    // 40-byte names, ~1400 entries: ~57 KiB of listing — over the byte
    // cap with the file count kept small.
    for i in 0..1400 {
        fs::write(dir.join(format!("f{i:032}x.txt")), "x").expect("write");
    }
    let out = read_out(dir.to_string_lossy().to_string(), None, None).await;
    assert!(out.contains("entries shown"), "{out}");
    fs::remove_dir_all(&dir).ok();
}

// --- image reads: whole-file image content parts ---

/// The canonical 1x1 transparent PNG.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x62, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

#[tokio::test]
async fn read_returns_an_image_content_part() {
    let dir = temp_dir("read-image");
    let path = dir.join("dot.png");
    fs::write(&path, TINY_PNG).expect("write");
    let parts: Vec<_> = read(path.to_string_lossy().to_string(), None, None)
        .await
        .expect("image read")
        .into_iter()
        .collect();
    assert_eq!(parts.len(), 2, "report text + image: {parts:?}");
    assert!(
        parts[0]
            .as_text()
            .is_some_and(|t| t.contains("image/png") && t.contains("dot.png")),
        "the text part names the read: {:?}",
        parts[0].as_text()
    );
    match &parts[1] {
        rig_core::message::ToolResultContent::Image(image) => {
            assert_eq!(
                image.media_type,
                Some(rig_core::message::ImageMediaType::PNG)
            );
            let rig_core::message::DocumentSourceKind::Base64(data) = &image.data else {
                panic!(
                    "the image part is base64, never raw bytes (the log would bloat as a JSON number array)"
                );
            };
            let decoded = {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .expect("valid base64")
            };
            assert_eq!(decoded, TINY_PNG, "the bytes round-trip whole");
        }
        other => panic!("the second part is the image: {other:?}"),
    }
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn read_rejects_oversized_images_with_guidance() {
    let dir = temp_dir("read-image-huge");
    let path = dir.join("huge.png");
    let mut bytes = TINY_PNG.to_vec();
    bytes.resize(IMAGE_MAX_BYTES + 1, b'x');
    fs::write(&path, &bytes).expect("write");
    let error = err_text(read(path.to_string_lossy().to_string(), None, None).await);
    assert!(error.contains("capped at"), "{error}");
    assert!(error.contains("Downscale"), "{error}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn read_rejects_paging_args_on_images() {
    let dir = temp_dir("read-image-page");
    let path = dir.join("dot.png");
    fs::write(&path, TINY_PNG).expect("write");
    let error = err_text(read(path.to_string_lossy().to_string(), Some(1), None).await);
    assert!(error.contains("whole"), "{error}");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn image_magic_detection_covers_the_provider_set() {
    use rig_core::message::ImageMediaType;
    assert_eq!(image_media_type(TINY_PNG), Some(ImageMediaType::PNG));
    assert_eq!(
        image_media_type(&[0xff, 0xd8, 0xff, 0xe0]),
        Some(ImageMediaType::JPEG)
    );
    assert_eq!(image_media_type(b"GIF89a..."), Some(ImageMediaType::GIF));
    assert_eq!(
        image_media_type(&[b'R', b'I', b'F', b'F', 0, 0, 0, 0, b'W', b'E', b'B', b'P']),
        Some(ImageMediaType::WEBP)
    );
    // RIFF, but not WebP (an AVI): not an image read.
    assert_eq!(
        image_media_type(&[b'R', b'I', b'F', b'F', 0, 0, 0, 0, b'A', b'V', b'I', b' ']),
        None
    );
    assert_eq!(image_media_type(b"plain text"), None);
}

#[tokio::test]
async fn edit_rejects_an_empty_edits_list() {
    let (dir, path) = seed("edit-noedits", "f.txt", "content\n");
    let error = err_text(edit_core(&path.to_string_lossy(), &[]));
    assert!(error.contains("at least one replacement"), "{error}");
    assert_eq!(body(&path), "content\n");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_of_an_empty_file_points_at_write() {
    let (dir, path) = seed("edit-emptyfile", "f.txt", "");
    let error = err_text(edit_core(&path.to_string_lossy(), &[rep("x", "y")]));
    assert!(error.contains("empty"), "{error}");
    assert!(error.contains("write"), "{error}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn bash_one_huge_line_gets_the_honest_notice() {
    if !bash_dialect_available() {
        eprintln!("skipped: no verified Git Bash on this machine");
        return;
    }
    // One line over the whole byte budget (minified-style output): both
    // ends of the line are shown, and the notice says so.
    let (out, details) = split_parts(
        bash(
            &mut ctx(),
            "head -c 60000 /dev/zero | tr '\\0' 'x'".to_string(),
            None,
        )
        .await
        .expect("bash"),
    );
    assert!(out.contains("middle of the line omitted"), "{out}");
    assert!(out.contains("Full output:"), "{out}");
    let spill = out
        .split("Full output: ")
        .nth(1)
        .expect("spill path")
        .trim_end_matches(']');
    let details = details.expect("truncated output carries details");
    assert_eq!(details["spill_path"], spill);
    fs::remove_file(spill).ok();
}

// --- tool_result.details: the edit tool's presentation cargo ---

/// The edit tool's full content parts: report text plus the details
/// JSON when anything applied.
async fn edit_parts(
    path: &std::path::Path,
    edits: Vec<EditReplacement>,
) -> Result<Vec<rig_core::message::ToolResultContent>, ToolExecutionError> {
    let one_or_many = edit(path.to_string_lossy().into_owned(), edits).await?;
    Ok(one_or_many.into_iter().collect())
}

#[tokio::test]
async fn edit_emits_report_text_plus_details_json() {
    let (dir, path) = seed("edit-details", "f.txt", "alpha\nbeta\ngamma\n");
    let parts = edit_parts(&path, vec![rep("beta", "BETA")])
        .await
        .expect("edit");
    // Text first (the faithful copy), then the details part.
    let texts: Vec<&str> = parts.iter().filter_map(|c| c.as_text()).collect();
    assert_eq!(texts.len(), 1, "one text part: {texts:?}");
    assert!(texts[0].starts_with("Edited "), "{}", texts[0]);
    let details: Vec<&serde_json::Value> = parts.iter().filter_map(|c| c.as_json()).collect();
    assert_eq!(details.len(), 1, "one details part");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_details_carries_the_unified_diff_with_context() {
    let (dir, path) = seed(
        "edit-details-diff",
        "f.txt",
        "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n",
    );
    let parts = edit_parts(&path, vec![rep("l5", "L5")])
        .await
        .expect("edit");
    let details = parts
        .iter()
        .find_map(|c| c.as_json())
        .expect("details")
        .clone();
    let diff = &details["diff"];
    assert_eq!(diff["first_changed_line"], 5);
    let hunks = diff["hunks"].as_array().expect("hunks");
    assert_eq!(hunks.len(), 1);
    let hunk = &hunks[0];
    assert_eq!(hunk["old_start"], 1, "context pulls the hunk to line 1");
    assert_eq!(hunk["new_start"], 1);
    // 4 lines of context each side: l1..l4 context, l5 removed, L5
    // added, l6..l9 context (l10 falls outside the window).
    let kinds: Vec<&str> = hunk["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|l| l["kind"].as_str().expect("kind"))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "context", "context", "context", "context", "removed", "added", "context", "context",
            "context", "context"
        ]
    );
    let texts: Vec<&str> = hunk["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|l| l["text"].as_str().expect("text"))
        .collect();
    assert!(texts.contains(&"l5"), "removed line present: {texts:?}");
    assert!(texts.contains(&"L5"), "added line present: {texts:?}");
    assert!(!texts.contains(&"l10"), "outside the context window");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_details_marks_applied_and_rejected_outcomes() {
    let (dir, path) = seed("edit-details-outcomes", "f.txt", "keep\ntwice\ntwice\n");
    let parts = edit_parts(
        &path,
        vec![rep("keep", "KEEP"), rep("missing", "x"), rep("twice", "x")],
    )
    .await
    .expect("partial application");
    let details = parts
        .iter()
        .find_map(|c| c.as_json())
        .expect("details")
        .clone();
    let outcomes = details["outcomes"].as_array().expect("outcomes");
    assert_eq!(outcomes.len(), 3);
    assert_eq!(outcomes[0]["index"], 0);
    assert_eq!(outcomes[0]["applied"], true);
    assert!(outcomes[0].get("reason").is_none());
    assert_eq!(outcomes[1]["applied"], false);
    assert!(
        outcomes[1]["reason"]
            .as_str()
            .expect("reason")
            .contains("not found"),
        "{}",
        outcomes[1]
    );
    assert_eq!(outcomes[2]["applied"], false);
    assert!(
        outcomes[2]["reason"]
            .as_str()
            .expect("reason")
            .contains("2 occurrences"),
        "{}",
        outcomes[2]
    );
    // The report still names the failures — details duplicates the same
    // strings, structured.
    let report = parts.iter().find_map(|c| c.as_text()).expect("report");
    assert!(report.contains("edit[1]"), "{report}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_all_fail_emits_no_details() {
    let (dir, path) = seed("edit-details-none", "f.txt", "same\nsame\n");
    let result = edit_parts(&path, vec![rep("same", "x")]).await;
    let error = result.expect_err("all-fail is an error");
    assert!(error.to_string().contains("occurrences"), "{error}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_details_merge_adjacent_hunks() {
    let (dir, path) = seed("edit-details-merge", "f.txt", "a\nb\nc\nd\ne\nf\ng\nh\n");
    // Two changes 3 lines apart: their 4-line context windows overlap,
    // so they render as ONE hunk.
    let parts = edit_parts(&path, vec![rep("a", "A"), rep("e", "E")])
        .await
        .expect("edit");
    let details = parts
        .iter()
        .find_map(|c| c.as_json())
        .expect("details")
        .clone();
    let hunks = details["diff"]["hunks"].as_array().expect("hunks");
    assert_eq!(hunks.len(), 1, "overlapping context merges: {hunks:?}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_details_separate_distant_hunks_with_correct_starts() {
    // Changes at lines 5 and 35 of 40: two hunks. The second hunk's
    // start line is computed by advancing the line counters through the
    // all-equal gap between the ranges.
    let body: String = (1..=40).map(|i| format!("l{i}\n")).collect();
    let (dir, path) = seed("edit-details-distant", "f.txt", &body);
    let parts = edit_parts(&path, vec![rep("l5", "L5"), rep("l35", "L35")])
        .await
        .expect("edit");
    let details = parts
        .iter()
        .find_map(|c| c.as_json())
        .expect("details")
        .clone();
    let hunks = details["diff"]["hunks"].as_array().expect("hunks");
    assert_eq!(hunks.len(), 2, "distant changes never merge: {hunks:?}");
    assert_eq!(hunks[1]["new_start"], 31, "line 35 minus 4 context lines");
    assert_eq!(hunks[1]["old_start"], 31);
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn edit_details_mark_a_pure_deletions_first_change() {
    // A deletion-only diff has no inserted line to anchor
    // first_changed_line on; the fallback points at where content
    // vanished.
    let (dir, path) = seed("edit-details-del", "f.txt", "a\nb\nc\n");
    let parts = edit_parts(&path, vec![rep("b\n", "")]).await.expect("edit");
    let details = parts
        .iter()
        .find_map(|c| c.as_json())
        .expect("details")
        .clone();
    assert!(
        details["diff"]["first_changed_line"].is_number(),
        "a pure deletion still locates the first change: {details}"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn edit_details_with_no_changes_has_no_hunks() {
    // The base case the edit caller's invariant makes unreachable (an
    // applied edit always changes the file): before == after is empty.
    let details = diff::edit_details("same\n", "same\n", Vec::new());
    assert!(details["diff"]["first_changed_line"].is_null());
    assert_eq!(details["diff"]["hunks"].as_array().expect("hunks").len(), 0);
}
