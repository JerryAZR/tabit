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
//! The binary's mode dispatch, end to end over the real executable:
//! print mode's happy path (banner on stderr, answer and footer on
//! stdout), the promptless `--rewind`, `--list` over an empty and a
//! staged store, and JSON mode's two startup-failure shapes (the
//! broken config's setup guide; the unreadable session's plain
//! reason). The provider is an httpmock SSE server — the suite stays
//! offline, and each child gets its config by per-process env (no
//! global mutation: print and list modes never boot extensions, so
//! the machine's installs cannot leak in).

use std::path::{Path, PathBuf};
use std::process::Command;

use httpmock::MockServer;
use httpmock::prelude::*;

/// A temp dir per test tag (unique per invocation), removed by the
/// caller when done.
fn test_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::Mutex<u32> = std::sync::Mutex::new(0);
    let n = {
        let mut n = COUNTER.lock().expect("counter lock");
        *n += 1;
        *n
    };
    let dir = std::env::temp_dir().join(format!("tabit-modes/{tag}-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A streaming chat-completions answer in the exact chunk shape the
/// wire client parses (mirrors the subprocess suite's helper).
fn sse_answer(text: &str) -> String {
    let first = serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "m",
        "choices": [
            {"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}
        ],
    });
    let last = serde_json::json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "m",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
    });
    format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n")
}

/// One real-binary run: the executable in `dir`, `TABIT_CONFIG` at
/// `config`, the given args. Returns (exit code, stdout, stderr).
fn run_in(dir: &Path, config: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_tabit-core"))
        .args(args)
        .current_dir(dir)
        .env("TABIT_CONFIG", config)
        .output()
        .expect("spawn tabit-core");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The mock provider plus a config file pointing at it.
fn staged_provider(tag: &str, answer: &str) -> (MockServer, PathBuf) {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer(answer));
    });
    let dir = test_dir(tag);
    let config = dir.join("providers.toml");
    std::fs::write(
        &config,
        format!(
            "[providers.p]\nbase_url = \"http://127.0.0.1:{}/v1\"\napi = \"openai-completions\"\nkeyless = true\n\n[[providers.p.models]]\nid = \"m\"\n",
            server.port()
        ),
    )
    .expect("write config");
    (server, config)
}

/// A valid config that answers nothing (no request is made before the
/// failures these tests stage) — a dead port is the honest shape.
fn dead_provider_config(tag: &str) -> PathBuf {
    let dir = test_dir(tag);
    let config = dir.join("providers.toml");
    std::fs::write(
        &config,
        "[providers.p]\nbase_url = \"http://127.0.0.1:1/v1\"\napi = \"openai-completions\"\nkeyless = true\n\n[[providers.p.models]]\nid = \"m\"\n",
    )
    .expect("write config");
    config
}

#[test]
fn print_mode_answers_one_prompt_end_to_end() {
    let (_server, config) = staged_provider("print-happy", "the print answer");
    let dir = config.parent().expect("config dir").to_path_buf();
    let (code, stdout, stderr) = run_in(&dir, &config, &["-p", "say the thing"]);
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("the print answer"),
        "the answer rides stdout: {stdout}"
    );
    assert!(
        stderr.contains("tokens 3 in / 2 out"),
        "the footer carries the run's usage: {stderr}"
    );
    assert!(
        stderr.contains("--- session"),
        "the footer names the session file: {stderr}"
    );
    assert!(
        stderr.contains("started"),
        "the banner is stderr's (stdout stays the answer channel): {stderr}"
    );
    let sessions = dir.join(".tabit").join("sessions");
    assert_eq!(
        std::fs::read_dir(&sessions)
            .map(|entries| entries.count())
            .unwrap_or(0),
        1,
        "the run left exactly one session file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_promptless_rewind_drops_the_last_user_message_then_branches() {
    let (_server, config) = staged_provider("rewind", "the branch answer");
    let dir = config.parent().expect("config dir").to_path_buf();

    // The session under rewind: one completed prompt.
    let (code, stdout, _) = run_in(&dir, &config, &["-p", "first prompt"]);
    assert_eq!(code, Some(0), "the seeding run: {stdout}");

    // The promptless rewind: the marker, no run, exit 0.
    let (code, stdout, stderr) = run_in(&dir, &config, &["--continue", "--rewind", "1"]);
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("[rewound: dropped 1 user message(s)"),
        "the rewind reports what it dropped: {stdout}"
    );
    assert!(
        !stdout.contains("branch answer"),
        "a promptless rewind runs nothing: {stdout}"
    );

    // The branch: the next prompt answers from before the dropped
    // message, in the SAME file (one session, not a fork).
    let (code, stdout, _) = run_in(&dir, &config, &["--continue", "-p", "branch prompt"]);
    assert_eq!(code, Some(0), "the branch run: {stdout}");
    assert!(stdout.contains("the branch answer"), "{stdout}");
    let sessions = dir.join(".tabit").join("sessions");
    assert_eq!(
        std::fs::read_dir(&sessions)
            .map(|entries| entries.count())
            .unwrap_or(0),
        1,
        "rewind branches within the session file, it does not fork one"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn list_mode_names_the_staged_sessions() {
    // An empty store: the honest no-sessions line, exit 0.
    let config = dead_provider_config("list-empty");
    let dir = config.parent().expect("config dir").to_path_buf();
    let (code, stdout, _) = run_in(&dir, &config, &["--list"]);
    assert_eq!(code, Some(0), "{stdout}");
    assert!(
        stdout.contains("no sessions in"),
        "the empty store says so: {stdout}"
    );

    // One staged session: a row with its entry count and path.
    let (_server, full_config) = staged_provider("list-full", "an answer");
    let full_dir = full_config.parent().expect("config dir").to_path_buf();
    let (code, stdout, _) = run_in(&full_dir, &full_config, &["-p", "seed"]);
    assert_eq!(code, Some(0), "the seeding run: {stdout}");
    let (code, stdout, _) = run_in(&full_dir, &full_config, &["--list"]);
    assert_eq!(code, Some(0), "{stdout}");
    assert!(
        stdout.contains("entries") && stdout.contains(".tabit"),
        "the row carries the count and the file: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&full_dir);
}

#[test]
fn a_broken_config_is_a_json_setup_failure_with_the_guide() {
    let dir = test_dir("json-setup");
    let config = dir.join("providers.toml");
    std::fs::write(&config, "not toml").expect("broken config");
    let (code, stdout, stderr) = run_in(&dir, &config, &["--json"]);
    assert_eq!(code, Some(1), "stdout: {stdout}\nstderr: {stderr}");
    let mut lines = stdout.lines();
    let report: serde_json::Value =
        serde_json::from_str(lines.next().expect("the report line")).expect("report parses");
    assert_eq!(report["type"], "report", "the child speaks first: {report}");
    assert_eq!(report["protocol_version"], 20, "{report}");
    let failure: serde_json::Value =
        serde_json::from_str(lines.next().expect("the failure event line"))
            .expect("failure parses");
    assert_eq!(failure["type"], "error", "{failure}");
    assert!(
        failure["message"]
            .as_str()
            .is_some_and(|message| message.contains("first-run setup needed")),
        "a config problem carries the teaching guide: {failure}"
    );
    assert!(
        stderr.contains("first-run setup needed"),
        "the reason echoes on the human surface: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unreadable_session_is_a_json_startup_failure_without_the_guide() {
    let config = dead_provider_config("json-open");
    let dir = config.parent().expect("config dir").to_path_buf();
    let session = dir.join("not-a-session.jsonl");
    std::fs::write(&session, "garbage, not a session log\n").expect("garbage session");
    let (code, stdout, stderr) = run_in(
        &dir,
        &config,
        &["--json", "--session", session.to_str().expect("utf8 path")],
    );
    assert_eq!(code, Some(1), "stdout: {stdout}\nstderr: {stderr}");
    let mut lines = stdout.lines();
    let report: serde_json::Value =
        serde_json::from_str(lines.next().expect("the report line")).expect("report parses");
    assert_eq!(report["type"], "report", "{report}");
    let failure = lines.next().expect("the failure event line");
    assert!(
        failure.contains("could not start the session"),
        "a non-config failure carries the plain reason: {failure}"
    );
    assert!(
        !stdout.contains("first-run setup needed"),
        "the setup guide is advice for a problem the user does not have: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
