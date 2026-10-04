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
//! offline, and each child gets its config by per-process env and
//! its extension root pinned to an empty dir (print mode boots the
//! same extension world JSON mode does, so the machine's real
//! installs must not leak in).

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
/// `config`, the given args. For the modes that boot the extension
/// world (print and JSON — the 2026-09-27 unification), the root is
/// pinned to an empty dir so the machine's real installs cannot
/// leak in; `--list` boots nothing and takes no root.
fn run_in(dir: &Path, config: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let mut argv: Vec<String> = Vec::new();
    if !args.contains(&"--list") {
        let empty_root = dir.join("no-extensions");
        std::fs::create_dir_all(&empty_root).expect("empty extension root");
        argv.push("--extensions".to_string());
        argv.push(empty_root.to_str().expect("utf-8 path").to_string());
    }
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let output = Command::new(env!("CARGO_BIN_EXE_tabit-core"))
        .args(&argv)
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
    assert_eq!(
        stdout,
        "the print answer
",
        "stdout is exactly the response, one copy at the terminal: {stdout:?}"
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

/// The tool flags are include/exclude-if-it-exists (owner ruling
/// 2026-09-27): an unknown name in `--tools` matches nothing without
/// error, and the surviving set is what crosses to the provider —
/// the mock only answers a request carrying `read` but not `write`,
/// so a leak matches only the catch-all and the run fails.
#[test]
fn tool_flags_filter_if_it_exists_and_reach_the_request() {
    let server = MockServer::start();
    let filtered = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .body_includes("\"name\":\"read\"")
            .body_excludes("\"name\":\"write\"");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("the filtered answer"));
    });
    let catch_all = server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("the leak answer"));
    });
    let dir = test_dir("tools-flags");
    let config = dir.join("providers.toml");
    std::fs::write(
        &config,
        format!(
            "[providers.p]\nbase_url = \"http://127.0.0.1:{}/v1\"\napi = \"openai-completions\"\nkeyless = true\n\n[[providers.p.models]]\nid = \"m\"\n",
            server.port()
        ),
    )
    .expect("write config");
    let (code, stdout, stderr) = run_in(
        &dir,
        &config,
        &["--tools", "read,typo", "-p", "say the thing"],
    );
    assert_eq!(
        code,
        Some(0),
        "the unknown allow name matched nothing, without error: {stdout}\n{stderr}"
    );
    assert_eq!(
        filtered.calls(),
        1,
        "exactly one request, its toolset filtered: {stdout}"
    );
    assert_eq!(
        catch_all.calls(),
        0,
        "no request carried the filtered-out tool: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The extension world is mode-uniform (owner ruling 2026-09-27): a
/// static package's providers fragment merges in PRINT mode exactly
/// as in JSON mode — the sharp surprise case (a user with a
/// provider-relay extension would otherwise see their provider exist
/// in one mode and not the other). The package is process-free: no
/// `entry`, so it mounts and contributes without launching anything.
#[test]
fn a_static_packages_provider_fragment_serves_print_mode() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("the fragment answer"));
    });
    let dir = test_dir("print-fragment");
    let package = dir.join("ext-root").join("fragship");
    std::fs::create_dir_all(&package).expect("package dir");
    std::fs::write(
        package.join("tabit.json"),
        r#"{"name":"fragship","version":"0.1.0","description":"a static providers fragment"}"#,
    )
    .expect("manifest");
    std::fs::write(
        package.join("providers.toml"),
        format!(
            "[providers.frag]\nbase_url = \"http://127.0.0.1:{}/v1\"\napi = \"openai-completions\"\nkeyless = true\n\n[[providers.frag.models]]\nid = \"m\"\n",
            server.port()
        ),
    )
    .expect("fragment");
    // The user config is empty: the fragment IS the only provider,
    // so answering at all proves the merge happened in print mode.
    let config = dir.join("providers.toml");
    std::fs::write(&config, "").expect("empty user config");

    let (code, stdout, stderr) = run_in(
        &dir,
        &config,
        &[
            "--extensions",
            dir.join("ext-root").to_str().expect("utf-8 path"),
            "--model",
            "frag/m",
            "-p",
            "say the thing",
        ],
    );
    assert_eq!(
        code,
        Some(0),
        "the fragment's provider served the run: {stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("the fragment answer"),
        "the answer rode the fragment's provider: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The backend's tracing finally lands somewhere: the binary installs
/// one stderr subscriber (WARN and up — the TTL tripwire's door), so
/// a discovery warning the assembly actually emits reaches the user
/// instead of vanishing into the no-op sink.
#[test]
fn a_discovery_warning_reaches_stderr_through_the_subscriber() {
    let (_server, config) = staged_provider("tracing-warn", "any answer");
    let dir = config.parent().expect("config dir").to_path_buf();
    let broken = dir.join(".agents").join("skills").join("broken");
    std::fs::create_dir_all(&broken).expect("stage skills dir");
    std::fs::write(
        broken.join("SKILL.md"),
        "---\nname: broken\n---\nno description in the frontmatter\n",
    )
    .expect("stage the broken skill");

    let (code, stdout, stderr) = run_in(&dir, &config, &["-p", "say the thing"]);
    assert_eq!(code, Some(0), "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stderr.contains("skipping skill without a description"),
        "the warn crossed stderr through the subscriber: {stderr}"
    );
    assert!(
        stdout.contains("any answer"),
        "the answer channel is untouched by the diagnostics: {stdout}"
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
        stderr.contains("[rewound: dropped 1 user message(s)"),
        "the rewind reports what it dropped (stderr — stdout is the answer channel): {stderr}"
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
    assert_eq!(report["protocol_version"], 21, "{report}");
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

// ---- The zero-config first run (the ruling reversal, 2026-10) ----

/// A JSON-mode child with live pipes: stdout lines arrive on a
/// channel (a reader thread owns the pipe), stdin writes reach the
/// child. The zero-config tests must answer the boot announcements —
/// the session id exists only after `session_opened`.
struct JsonChild {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    stdin: std::process::ChildStdin,
}

/// Spawn `tabit-core` in `dir` on a hermetic machine: `home` is a
/// fresh temp HOME, the config env pointers are removed, and
/// `TABIT_CONFIG` points at `config` only when given. The extension
/// root is pinned empty like `run_in`'s.
fn json_child(dir: &Path, home: &Path, config: Option<&Path>, args: &[&str]) -> JsonChild {
    use std::io::BufRead as _;
    let empty_root = dir.join("no-extensions");
    std::fs::create_dir_all(&empty_root).expect("empty extension root");
    let mut argv: Vec<String> = vec![
        "--extensions".to_string(),
        empty_root.to_str().expect("utf-8 path").to_string(),
    ];
    argv.extend(args.iter().map(|arg| (*arg).to_string()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_tabit-core"));
    command
        .args(&argv)
        .current_dir(dir)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("TABIT_CONFIG")
        .env_remove("TABIT_CONFIG_EXTRA")
        .env_remove("TABIT_AUTH")
        .env_remove("TABIT_SETTINGS")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(config) = config {
        command.env("TABIT_CONFIG", config);
    }
    let mut child = command.spawn().expect("spawn tabit-core");
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                return;
            }
        }
        // The child died; unblock the reader.
        drop(tx);
    });
    JsonChild {
        child,
        lines: rx,
        stdin,
    }
}

/// The next wire line, parsed. A silent child is a test bug, not a
/// hang: bound every wait.
fn next_frame(child: &JsonChild) -> serde_json::Value {
    let line = child
        .lines
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("a wire line arrives");
    serde_json::from_str(&line).expect("every line is a frame")
}

/// Read frames until `models_available` (the boot announcements'
/// tail), returning everything seen.
fn read_boot(child: &JsonChild) -> Vec<serde_json::Value> {
    let mut frames = vec![next_frame(child)];
    assert_eq!(frames[0]["type"], "report", "the report leads: {frames:?}");
    while !frames
        .iter()
        .any(|frame| frame["type"] == "models_available")
    {
        frames.push(next_frame(child));
    }
    frames
}

/// Close stdin (frontend death) and collect the exit code.
fn close_and_wait(mut child: JsonChild) -> Option<i32> {
    drop(child.stdin);
    child.child.wait().expect("reap").code()
}

#[test]
fn zero_config_boots_selection_less_and_the_run_open_failure_teaches() {
    // The ruling reversal: a bare machine (no providers.toml at the
    // default location, no env pointers) boots fine. The wire says
    // so: `session_opened.model` is null, `models_available` is
    // empty, the teaching note rides as `error { kind: model }`, and
    // the first message's run fails at open with `kind: "model"`.
    let dir = test_dir("zero-config");
    let home = test_dir("zero-config-home");
    let mut child = json_child(&dir, &home, None, &["--json"]);

    let frames = read_boot(&child);
    let opened = frames
        .iter()
        .find(|frame| frame["type"] == "session_opened")
        .expect("the boot session announced");
    assert_eq!(opened["model"], serde_json::Value::Null, "null, present");
    let session = opened["id"].as_str().expect("the session id").to_string();
    let note = frames
        .iter()
        .find(|frame| frame["type"] == "error" && frame["kind"] == "model")
        .expect("the teaching note rides the startup notes");
    assert!(
        note["message"]
            .as_str()
            .is_some_and(|m| m.contains("no usable model") && m.contains("providers.toml")),
        "the note teaches: {note}"
    );
    let catalog = frames
        .iter()
        .find(|frame| frame["type"] == "models_available")
        .expect("the catalog announced");
    assert_eq!(catalog["providers"], serde_json::json!([]), "empty");
    assert!(
        !frames.iter().any(|frame| frame["type"] == "model_changed"),
        "a selection-less session announces no model_changed"
    );

    use std::io::Write as _;
    writeln!(
        child.stdin,
        "{}",
        serde_json::json!({"type": "message", "session": session, "text": "hi"})
    )
    .expect("write the message");
    let failure = loop {
        let frame = next_frame(&child);
        if frame["type"] == "run_failed" {
            break frame;
        }
    };
    assert_eq!(failure["kind"], "model", "the run-open failure: {failure}");
    assert!(
        failure["message"]
            .as_str()
            .is_some_and(|m| m.contains("no model selected") && m.contains("providers.toml")),
        "the failure teaches: {failure}"
    );

    assert_eq!(close_and_wait(child), Some(0), "a clean wind-down");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn explicit_model_on_a_known_but_unusable_provider_boots() {
    // The explicit rung validates existence only: the provider is
    // configured but has no key, so the catalog is empty — yet the
    // boot succeeds with the asked-for selection, and the missing
    // key surfaces as the run-open failure (the lazy agent cache;
    // construction failure is not a startup death).
    let dir = test_dir("unusable-explicit");
    let home = test_dir("unusable-explicit-home");
    let config = dir.join("providers.toml");
    std::fs::write(
        &config,
        "[providers.p]\nbase_url = \"http://127.0.0.1:1/v1\"\napi = \"openai-completions\"\n\n[[providers.p.models]]\nid = \"m\"\n",
    )
    .expect("write config");
    let mut child = json_child(&dir, &home, Some(&config), &["--json", "--model", "p/m"]);

    let frames = read_boot(&child);
    let opened = frames
        .iter()
        .find(|frame| frame["type"] == "session_opened")
        .expect("the boot session announced");
    assert_eq!(
        opened["model"],
        serde_json::json!({"provider": "p", "model": "m", "thinking_level": null}),
        "the explicit selection crossed: {opened}"
    );
    let session = opened["id"].as_str().expect("the session id").to_string();
    let catalog = frames
        .iter()
        .find(|frame| frame["type"] == "models_available")
        .expect("the catalog announced");
    assert_eq!(
        catalog["providers"],
        serde_json::json!([]),
        "no key, not keyless: unusable"
    );

    use std::io::Write as _;
    writeln!(
        child.stdin,
        "{}",
        serde_json::json!({"type": "message", "session": session, "text": "hi"})
    )
    .expect("write the message");
    let failure = loop {
        let frame = next_frame(&child);
        if frame["type"] == "run_failed" {
            break frame;
        }
    };
    assert_eq!(failure["kind"], "model", "{failure}");
    assert!(
        failure["message"]
            .as_str()
            .is_some_and(|m| m.contains("requires a key")),
        "the build failure, named: {failure}"
    );

    assert_eq!(close_and_wait(child), Some(0), "the boot succeeded");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn explicit_model_on_an_unknown_ref_still_fails_startup() {
    // The explicit rung stays loud: a ref config does not know is a
    // startup rejection (plain reason — no setup guide), exit 1.
    let config = dead_provider_config("unknown-model");
    let dir = config.parent().expect("config dir").to_path_buf();
    let (code, stdout, stderr) = run_in(&dir, &config, &["--json", "--model", "ghost/m"]);
    assert_eq!(code, Some(1), "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("could not start the session") && stdout.contains("ghost"),
        "the rejection names the ref: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_set_but_missing_tabit_config_stays_loud() {
    // The replacement ruling survives the reversal: an explicit
    // pointer that misses is never a silent empty config.
    let dir = test_dir("missing-pointer");
    let missing = dir.join("absent.toml");
    let (code, stdout, stderr) = run_in(&dir, &missing, &["--json"]);
    assert_eq!(code, Some(1), "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("first-run setup needed") && stdout.contains("absent.toml"),
        "the guide names the missing pointer: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn print_mode_without_a_selection_fails_the_run_and_teaches() {
    // Non-interactive death is the one acceptable kind (pi's shape):
    // no startup check — the run fails at open, the message teaches,
    // the exit is nonzero.
    let dir = test_dir("print-no-model");
    let home = test_dir("print-no-model-home");
    let empty_root = dir.join("no-extensions");
    std::fs::create_dir_all(&empty_root).expect("empty extension root");
    let output = Command::new(env!("CARGO_BIN_EXE_tabit-core"))
        .args([
            "--extensions",
            empty_root.to_str().expect("utf-8 path"),
            "-p",
            "hi",
        ])
        .current_dir(&dir)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env_remove("TABIT_CONFIG")
        .env_remove("TABIT_CONFIG_EXTRA")
        .env_remove("TABIT_AUTH")
        .env_remove("TABIT_SETTINGS")
        .output()
        .expect("spawn tabit-core");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("run failed") && stderr.contains("no model selected"),
        "the run-open failure is print mode's carrier: {stderr}"
    );
    assert!(
        stderr.contains("no usable model"),
        "the boot's teaching note warned on stderr too: {stderr}"
    );
    assert!(stdout.is_empty(), "no answer exists to print: {stdout}");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
}
