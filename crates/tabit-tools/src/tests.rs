use super::*;
use rig_agent::tool::tool_definition;
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

fn err_text(result: Result<String, ToolExecutionError>) -> String {
    match result {
        Ok(text) => panic!("expected an error, got output: {text}"),
        Err(e) => e.to_string(),
    }
}

#[tokio::test]
async fn read_returns_file_contents() {
    let dir = temp_dir("read-ok");
    let path = dir.join("note.txt");
    fs::write(&path, "first\nsecond\n").expect("write");
    let out = read(path.to_string_lossy().to_string())
        .await
        .expect("read");
    assert_eq!(out, "first\nsecond\n");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn read_errors_are_clear_about_what_and_why() {
    let dir = temp_dir("read-err");
    let missing = err_text(read(dir.join("nope.txt").to_string_lossy().to_string()).await);
    assert!(missing.contains("nope.txt"), "{missing}");
    assert!(missing.contains("cannot read"), "{missing}");

    fs::create_dir(dir.join("adir")).expect("dir");
    let is_dir = err_text(read(dir.join("adir").to_string_lossy().to_string()).await);
    assert!(is_dir.contains("directory"), "{is_dir}");

    let bin = dir.join("blob.bin");
    fs::write(&bin, [0xff, 0xfe, 0x00, 0x01]).expect("binary");
    let not_utf8 = err_text(read(bin.to_string_lossy().to_string()).await);
    assert!(not_utf8.contains("UTF-8"), "{not_utf8}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn read_truncates_large_files_with_a_notice() {
    let dir = temp_dir("read-cap");
    let path = dir.join("big.txt");
    let body = "x".repeat(READ_CAP_BYTES + 1024);
    fs::write(&path, &body).expect("write");
    let out = read(path.to_string_lossy().to_string())
        .await
        .expect("read");
    assert!(out.len() > READ_CAP_BYTES, "notice appended");
    assert!(
        out.contains("[file truncated: showed"),
        "truncation is explicit: {}...",
        &out[out.len() - 120..]
    );
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn ls_lists_entries_with_kinds() {
    let dir = temp_dir("ls-ok");
    fs::write(dir.join("a.txt"), "a").expect("write");
    fs::write(dir.join("b.txt"), "bb").expect("write");
    fs::create_dir(dir.join("sub")).expect("subdir");

    let out = ls(Some(dir.to_string_lossy().to_string()))
        .await
        .expect("ls");
    assert!(out.contains("a.txt"), "{out}");
    assert!(out.contains("b.txt"), "{out}");
    assert!(out.contains("sub"), "{out}");
    assert!(out.contains("dir"), "{out}");
    // rows are sorted
    let a = out.find("a.txt").expect("a");
    let b = out.find("b.txt").expect("b");
    let s = out.find("sub").expect("sub");
    assert!(a < b && b < s, "sorted: {out}");

    let empty = temp_dir("ls-empty");
    let out = ls(Some(empty.to_string_lossy().to_string()))
        .await
        .expect("ls");
    assert_eq!(out, "(empty directory)\n");

    let missing = err_text(ls(Some("definitely/not/here".to_string())).await);
    assert!(missing.contains("cannot list"), "{missing}");
    fs::remove_dir_all(&dir).ok();
    fs::remove_dir_all(&empty).ok();
}

#[tokio::test]
async fn bash_runs_a_command_and_captures_output() {
    let out = bash("echo tabit-smoke".to_string(), None)
        .await
        .expect("bash");
    assert!(
        out.trim().contains("tabit-smoke"),
        "echo output captured: {out}"
    );
}

#[tokio::test]
async fn bash_reports_nonzero_exits_with_output() {
    let error = err_text(bash("exit 3".to_string(), None).await);
    // PowerShell has no `exit 3` semantics as a command line; the fallback
    // path only exists where bash is missing. This machine has Git Bash.
    if interpreter().argv0.ends_with("bash.exe") || interpreter().argv0 == "bash" {
        assert!(error.contains("status 3"), "{error}");
    }
}

#[tokio::test]
async fn bash_kills_commands_that_exceed_their_timeout() {
    let sleep_cmd = if cfg!(windows) && interpreter().argv0.eq_ignore_ascii_case("powershell") {
        "Start-Sleep -Seconds 30"
    } else {
        "sleep 30"
    };
    let error = err_text(bash(sleep_cmd.to_string(), Some(1)).await);
    assert!(error.contains("timeout"), "{error}");
    assert!(error.contains("killed"), "{error}");
}

#[tokio::test]
async fn bash_missing_program_is_a_clear_error() {
    let error = err_text(bash("this-command-does-not-exist-xyz".to_string(), None).await);
    assert!(
        error.contains("command exited") || error.contains("not recognized"),
        "failure surfaces the platform's own diagnosis: {error}"
    );
}

#[tokio::test]
async fn portable_structs_are_named_and_erased_correctly() {
    assert_eq!(<Read as PortableTool>::NAME, "read");
    assert_eq!(<Ls as PortableTool>::NAME, "ls");
    assert_eq!(<Bash as PortableTool>::NAME, "bash");

    let mut set = rig_agent::tool::ToolSet::default();
    set.add_dynamic_tool(dynamic(Read));
    set.add_dynamic_tool(dynamic(Bash));
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
