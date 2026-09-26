//! The subprocess substrate end to end (ROADMAP item 5): the parent's
//! `subagent` tool spawns THIS
//! repository's real binary in `--json` child role, drives it over
//! the stdio protocol, and the child's model is an httpmock SSE
//! script — offline (AGENTS.md rule 5). What is proven here spans
//! processes: the child-role flags, the parent-carrying announcement
//! from the source, frame forwarding with the child's own stamps, the
//! OS-enforced cwd, ephemerality (no files under the child's
//! directory), and the abort leash's courtesy-with-deadline shape.

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

use httpmock::MockServer;
use httpmock::prelude::*;
use rig_agent::agent::ModelHandle;
use rig_agent::test_utils::{MockCompletionModel, MockStreamEvent};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tabit_protocol::SessionEvent;
use tabit_session::{
    ModelSelection, Node, Session, SessionBuilder, SessionHost, SessionHostWiring, SessionStore,
    subagent,
};

/// Env mutation is process-wide — serialize the tests that point
/// `TABIT_CONFIG` at their mock server (the child process inherits
/// the parent's environment). Async-aware because the guard must
/// span the test's awaits: the env stays set while the child spawns
/// mid-test.
static ENV_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

/// The lock accessor (tokio's Mutex is not const-constructible).
fn env_lock() -> &'static tokio::sync::Mutex<()> {
    ENV_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// A temp dir per test tag, cleaned by the caller.
fn test_dir(tag: &str) -> PathBuf {
    static COUNTER: OnceLock<Mutex<u32>> = OnceLock::new();
    let n = {
        let counter = COUNTER.get_or_init(|| Mutex::new(0));
        let mut n = counter.lock().expect("counter lock");
        *n += 1;
        *n
    };
    let dir = std::env::temp_dir().join(format!("tabit-subproc-tests/{tag}-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A streaming chat-completions answer in the exact chunk shape the
/// wire client parses (mirrors the rig cassettes' shape).
fn sse_answer(text: &str) -> String {
    let first = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "m",
        "choices": [
            {"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}
        ],
    });
    let last = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "m",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
    });
    format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n")
}

/// The child's provider config pointing at the mock server, plus the
/// `TABIT_CONFIG` env pointed at it (the child process loads its own).
fn stage_child_config(tag: &str, server: &MockServer) -> PathBuf {
    let dir = test_dir(tag);
    let config_path = dir.join("child-providers.toml");
    // No `default_model` needed: the child's resolution falls back to
    // the first configured model, which is this one.
    let toml = format!(
        "[providers.p]\nbase_url = \"http://127.0.0.1:{}/v1\"\napi = \"openai-completions\"\nkeyless = true\n\n[[providers.p.models]]\nid = \"m\"\n",
        server.port()
    );
    std::fs::write(&config_path, toml).expect("write child config");
    config_path
}

/// The parent: a scripted in-process model whose first turn calls the
/// subagent tool, and a second that wraps up. `overrides` picks the
/// call's shape: `Some` passes the cwd override (the parent's own —
/// the OS-enforced scope under test), `None` exercises the
/// inheritance defaults. The model and toolset are inherited (no
/// knobs, ruled 2026-09); the child mounts its own default core
/// toolset — the mock model never calls tools.
fn subprocess_parent(
    store: &SessionStore,
    cwd: &Path,
    node: Arc<Node>,
    task: &str,
    overrides: bool,
) -> Session {
    let config = Arc::new(
        tabit_config::TabitConfig::from_toml_str(
            r#"
[providers.p]
base_url = "http://127.0.0.1:1/v1"
api = "openai-completions"
keyless = true

[[providers.p.models]]
id = "m"
"#,
            Path::new("providers.toml"),
        )
        .expect("parent config"),
    );
    let auth = Arc::new(tabit_config::AuthConfig::default());
    let parts = Arc::new(subagent::SubagentParts {
        tools: Vec::new(),
        max_turns: 8,
        node,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_tabit-core")),
        // Children boot their own hosts — pin an empty root so the
        // suite stays hermetic against the machine's real installs.
        extensions: cwd.join(".tabit/no-extensions"),
    });
    let turns = vec![
        vec![
            MockStreamEvent::ToolCall {
                id: "c1".to_string(),
                name: "subagent".to_string(),
                arguments: if overrides {
                    json!({
                        "task": task,
                        "cwd": cwd.display().to_string(),
                    })
                } else {
                    json!({"task": task})
                },
                call_id: None,
            },
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("parent wrap-up"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
    ];
    SessionBuilder::new(store.clone(), config, auth, ModelSelection::new("p", "m"))
        .expect("builder")
        .preamble("test parent".to_string())
        .model_factory(Arc::new(move |_, _, _| {
            Ok(ModelHandle::new(MockCompletionModel::from_stream_turns(
                turns.clone(),
            )))
        }))
        .subagents(parts)
        .dynamic_tool(subagent::subagent_tool())
        // A REAL directory: it becomes the child process's cwd (the
        // OS-enforced scope is the substrate's point).
        .create(&cwd.display().to_string())
        .expect("parent session")
}

/// A host over a plain store, sharing the node with the parts.
fn host(store: &SessionStore, node: Arc<Node>, session: Session) -> SessionHost {
    let wiring = SessionHostWiring {
        node,
        boot_parent: None,
        boot_parent_call: None,
        store: store.clone(),
    };
    let data = tabit_session::SessionHostData {
        create: Arc::new(|| Err("not driven".to_string())),
        open: Arc::new(|_| Err("not driven".to_string())),
        extensions: Default::default(),
    };
    SessionHost::spawn(session, Vec::new(), wiring, data)
}

#[tokio::test]
async fn a_subprocess_child_announces_streams_and_answers_over_the_real_binary() {
    let _guard = env_lock().lock().await;
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("child done"))
            // A short delay opens the mid-run window for the routed
            // steer below.
            .delay(std::time::Duration::from_millis(700));
    });
    let config_path = stage_child_config("happy", &server);
    // Env mutation under the lock: the only in-process reader of this
    // variable is the child spawn (inside the same locked region), and
    // the two tests serialize on ENV_LOCK — edition 2024 demands the
    // unsafe block for the mutation itself.
    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::set_var("TABIT_CONFIG", &config_path);
    }

    let parent_cwd = test_dir("happy-parent");
    let store = SessionStore::new(test_dir("happy-store"));
    let node = Arc::new(Node::new("test"));
    let parent = subprocess_parent(&store, &parent_cwd, node.clone(), "say the words", true);
    let mut handle = host(&store, node, parent);
    let parent_id = handle.info().session_id.clone();

    handle.message(&parent_id, "go");
    // Collect until the parent wraps up, steering the child mid-run:
    // the steer fires when the child's task enters its conversation —
    // inside the model call's delay window.
    let mut frames = Vec::new();
    let mut steered = false;
    let mut child_id: Option<String> = None;
    let mut subagent_call: Option<String> = None;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(60), handle.next_event())
            .await
            .expect("frames keep coming")
            .expect("the stream stays open");
        let done = matches!(&frame.event,
            SessionEvent::RunFinished { output, .. } if output == "parent wrap-up");
        match &frame.event {
            SessionEvent::SessionOpened {
                id,
                parent: Some(parent),
                ..
            } if parent == &parent_id => child_id = Some(id.clone()),
            SessionEvent::ToolCall {
                name,
                internal_call_id,
                ..
            } if name == "subagent" => subagent_call = Some(internal_call_id.clone()),
            SessionEvent::UserMessage { .. }
                if !steered && frame.stream.as_ref().map(|s| s.as_str()) == child_id.as_deref() =>
            {
                steered = true;
                handle.message(child_id.as_deref().expect("announced"), "steered mid-run");
            }
            _ => {}
        }
        frames.push(frame);
        if done {
            break;
        }
    }
    let child_id = child_id.expect("the child announced");
    let subagent_call = subagent_call.expect("the parent model called the subagent tool");

    // The pairing: the child's announce carries the spawning tool
    // call's correlation id — the same `internal_call_id` the parent's
    // `ToolCall` event announced, so a frontend pairs the two exactly
    // (arrival order could not, under concurrent subagent calls).
    let announced_call = frames.iter().find_map(|frame| match &frame.event {
        SessionEvent::SessionOpened { parent_call, .. } => parent_call.clone(),
        _ => None,
    });
    assert_eq!(
        announced_call,
        Some(subagent_call.clone()),
        "the announce pairs the child with the exact spawning tool call"
    );

    // The announcement carried the parent field from the source and
    // the empty path of an ephemeral session.
    let child_path = frames
        .iter()
        .find_map(|frame| match &frame.event {
            SessionEvent::SessionOpened {
                path,
                parent: Some(parent),
                ..
            } if parent == &parent_id => Some(path.clone()),
            _ => None,
        })
        .expect("the child announced with the parent field");
    assert!(child_path.is_empty(), "ephemeral children have no file");

    // Route-all, structurally: the message addressed to the CHILD
    // mid-run crossed as one wire line and the child's own host
    // consumed it — `message_queued` on the child's own stream is its
    // mailbox acknowledging (the same acknowledgment any session
    // emits; no child-specific code anywhere in the path).
    assert!(steered, "the steer fired mid-run");
    let queued = frames.iter().any(|frame| {
        frame
            .stream
            .as_ref()
            .is_some_and(|s| s.as_str() == child_id)
            && matches!(
                &frame.event,
                SessionEvent::MessageQueued { text, .. } if text == "steered mid-run"
            )
    });
    assert!(
        queued,
        "the child's mailbox acknowledged the routed steer on its own stream"
    );

    // The child's run streamed on its own stamp, terminal included.
    let child_finished = frames.iter().any(|frame| {
        frame.stream.as_ref().is_some_and(|s| s.as_str() == child_id)
            && matches!(&frame.event, SessionEvent::RunFinished { output, .. } if output == "child done")
    });
    assert!(
        child_finished,
        "the child's terminal crossed on its own stamp"
    );

    // The parent's tool result carries the child's id (the details
    // shape both substrates share).
    let details_child = frames.iter().find_map(|frame| match &frame.event {
        SessionEvent::ToolResult { name, details, .. } if name == "subagent" => {
            details.as_ref().and_then(|d| d.get("child_id")).cloned()
        }
        _ => None,
    });
    assert_eq!(
        details_child,
        Some(json!(child_id)),
        "the result names the child process's session"
    );

    handle.close_commands();
    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::remove_var("TABIT_CONFIG");
    }
}

#[tokio::test]
async fn aborting_the_parent_returns_promptly_and_the_child_flushes_its_terminal() {
    // The abort ruling: forward + close, return now; the reaper bounds
    // the exit. The child's aborted terminal still flushes before its
    // stream ends (the stdin-close death contract).
    let _guard = env_lock().lock().await;
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("never delivered"))
            // The child's first model call parks on the delay — the
            // abort must not wait for it.
            .delay(std::time::Duration::from_secs(60));
    });
    let config_path = stage_child_config("abort", &server);
    // Env mutation under the lock: the only in-process reader of this
    // variable is the child spawn (inside the same locked region), and
    // the two tests serialize on ENV_LOCK — edition 2024 demands the
    // unsafe block for the mutation itself.
    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::set_var("TABIT_CONFIG", &config_path);
    }

    let parent_cwd = test_dir("abort-parent");
    let store = SessionStore::new(test_dir("abort-store"));
    let node = Arc::new(Node::new("test"));
    let parent = subprocess_parent(
        &store,
        &parent_cwd,
        node.clone(),
        "park on the model",
        false,
    );
    let mut handle = host(&store, node, parent);
    let parent_id = handle.info().session_id.clone();

    handle.message(&parent_id, "go");
    let mut child_id: Option<String> = None;
    // Wait until the child's run is LIVE (its task drained, a turn
    // started — parked on the delayed model), then abort the parent.
    // Waiting only for the announcement would race the task's drain:
    // an abort winning that race discards a never-started run (no
    // terminal owed — the discard notice is the report), a different
    // (also correct) outcome this test does not assert.
    let child_id = loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(60), handle.next_event())
            .await
            .expect("frames keep coming")
            .expect("the stream stays open");
        match &frame.event {
            SessionEvent::SessionOpened {
                id,
                parent: Some(parent),
                ..
            } => {
                if parent == &parent_id {
                    child_id = Some(id.clone());
                }
            }
            SessionEvent::TurnStarted { .. }
                if frame
                    .stream
                    .as_ref()
                    .is_some_and(|s| s.as_str() == child_id.as_deref().unwrap_or("")) =>
            {
                break child_id.expect("the child announced before its turn");
            }
            _ => {}
        }
    };
    let started = std::time::Instant::now();
    handle.abort(&parent_id);

    // The parent's terminal is prompt (the parent never waits on the
    // child's cooperation)…
    let mut parent_aborted = false;
    let mut child_aborted = false;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(15), handle.next_event())
            .await
            .unwrap_or_else(|_| {
                panic!("timed out: parent_aborted={parent_aborted} child_aborted={child_aborted}")
            })
            .expect("the stream stays open");
        if let SessionEvent::RunAborted { .. } = &frame.event {
            let on = frame.stream.as_ref().map(|s| s.as_str());
            if on == Some(parent_id.as_str()) {
                parent_aborted = true;
            }
            if on == Some(child_id.as_str()) {
                child_aborted = true;
            }
        }
        if parent_aborted && child_aborted {
            break;
        }
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "the parent returned in {:?} — not the parked model",
        started.elapsed()
    );
    assert!(
        child_aborted,
        "the child's aborted terminal flushed before its stream ended"
    );

    handle.close_commands();
    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::remove_var("TABIT_CONFIG");
    }
}

/// The preamble crossing, end to end (ruled 2026-09: the spawner owns
/// the child's voice, tabit still owns the truthful context): a
/// `SubprocessBuilder` override reaches the child process as
/// `--preamble`, REPLACES the default base (identity + standing
/// body), and the context appends as usual. Three mocks discriminate
/// every outcome — the marker mock (one request, and its regex also
/// requires the env block after the marker: appended, in order), the
/// default-identity mock (must never match: replaced, not extended),
/// and the catch-all (neither). The child's report names the mock
/// that served it.
#[tokio::test]
async fn a_preamble_override_replaces_the_preamble_and_appends_the_context() {
    let _guard = env_lock().lock().await;
    let server = MockServer::start();
    let default_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .body_includes("You are tabit");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("default"));
    });
    let marker_mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .body_matches("SUBAGENT-PREAMBLE-MARKER[\\s\\S]*cwd:");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("custom"));
    });
    let _catch_all = server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("neither"));
    });
    let config_path = stage_child_config("preamble", &server);
    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::set_var("TABIT_CONFIG", &config_path);
    }

    let child_cwd = test_dir("preamble-child");
    let ctx = subagent::SpawnContext::new(
        Arc::new(subagent::SubagentParts {
            tools: Vec::new(),
            max_turns: 8,
            node: Arc::new(Node::new("test")),
            exe: PathBuf::from(env!("CARGO_BIN_EXE_tabit-core")),
            extensions: child_cwd.join(".tabit/no-extensions"),
        }),
        Arc::new(tabit_session::subagent_pool::SubagentPool::new()),
        "preamble-test-parent".to_string(),
        ModelSelection::new("p", "m"),
        child_cwd.clone(),
    );
    let mut child = ctx
        .spawn_subprocess()
        .cwd(child_cwd.clone())
        .model(ModelSelection::new("p", "m"))
        .max_turns(8)
        .ephemeral(true)
        .preamble(
            "You are a delegated subagent. SUBAGENT-PREAMBLE-MARKER is your whole policy."
                .to_string(),
        )
        .spawn()
        .await
        .expect("the child spawns");
    let summary = ctx
        .drive_subprocess(
            &mut child,
            rig_agent::completion::Message::user("report"),
            None,
        )
        .await;
    child.wait_exit().await;

    assert_eq!(
        summary.outcome,
        tabit_session::RunOutcome::Completed,
        "the child completed ({:?})",
        summary.output
    );
    assert_eq!(
        summary.output, "custom",
        "the child's model was served by the override-matching mock"
    );
    assert_eq!(
        marker_mock.calls(),
        1,
        "exactly one request, carrying the override text"
    );
    assert_eq!(
        default_mock.calls(),
        0,
        "the default prompt was replaced, not extended"
    );

    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::remove_var("TABIT_CONFIG");
    }
}

/// A chat-completions SSE answer whose turn calls one tool (the
/// child's scripted model turn — mirrors the rig cassette shape).
fn sse_tool_call(id: &str, name: &str, arguments: &str) -> String {
    let first = json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk",
        "created": 0, "model": "m",
        "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
            "index": 0, "id": id, "type": "function",
            "function": {"name": name, "arguments": arguments},
        }]}, "finish_reason": null}],
    });
    let last = json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk",
        "created": 0, "model": "m",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
    });
    format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n")
}

/// Locate a workspace binary (same-package `CARGO_BIN_EXE_*` does not
/// reach cross-package bins; test exes run from
/// `<target>/<profile>/deps`, one level below the binaries).
fn workspace_bin(name: &str) -> PathBuf {
    std::env::current_exe()
        .expect("current exe")
        .parent()
        .and_then(|deps| deps.parent())
        .map(|dir| dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| panic!("workspace binary `{name}` not built — run the workspace gate"))
}

/// The 2026-09 ruling, proven across real processes: a subagent child
/// boots its own extension host and serves the packages' tools — the
/// child's scripted model calls the extension's `echo`, and the
/// result crossing back is the double's own text.
#[tokio::test]
async fn a_subprocess_child_boots_its_own_extension_host_and_serves_its_tools() {
    let _guard = env_lock().lock().await;
    let server = MockServer::start();
    // The child's two turns: call the extension's tool, then wrap up
    // once the result is in history (the markers are mutually
    // exclusive — the task text rides in history forever).
    server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .body_includes("child-task-8d21")
            .body_excludes("EXT-ECHOED");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_tool_call(
                "call-1",
                "echo",
                r#"{"text":"from the child"}"#,
            ));
    });
    server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .body_includes("EXT-ECHOED:from the child");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("child done"));
    });
    // The denied sibling (the per-invocation blacklist, ruled
    // 2026-09): its task marker rides in a request that carries NO
    // echo tool definition (`--without echo` removed the proxy from
    // the child's full toolset) — the model is served a plain answer,
    // discriminating against the served child's marker.
    server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .body_includes("denied-task-8d21")
            .body_excludes(r#""name":"echo""#);
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("denied child done"));
    });
    let config_path = stage_child_config("child-ext", &server);
    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::set_var("TABIT_CONFIG", &config_path);
    }

    // The child's extension root: the echo double, installed as a
    // package like any other.
    let ext_root = test_dir("child-ext-extensions");
    let package = ext_root.join("echoer");
    std::fs::create_dir_all(&package).expect("package dir");
    std::fs::write(
        package.join("tabit.json"),
        serde_json::to_string(&json!({
            "name": "echoer",
            "version": "0.1.0",
            "entry": [workspace_bin("ext-double").display().to_string(), "tools-echo"],
        }))
        .expect("manifest"),
    )
    .expect("manifest");

    let parent_cwd = test_dir("child-ext-parent");
    let store = SessionStore::new(test_dir("child-ext-store"));
    let node = Arc::new(Node::new("test"));
    let config = Arc::new(
        tabit_config::TabitConfig::from_toml_str(
            r#"
[providers.p]
base_url = "http://127.0.0.1:1/v1"
api = "openai-completions"
keyless = true

[[providers.p.models]]
id = "m"
"#,
            Path::new("providers.toml"),
        )
        .expect("parent config"),
    );
    let auth = Arc::new(tabit_config::AuthConfig::default());
    let parts = Arc::new(subagent::SubagentParts {
        tools: Vec::new(),
        max_turns: 8,
        node: node.clone(),
        exe: PathBuf::from(env!("CARGO_BIN_EXE_tabit-core")),
        extensions: ext_root.clone(),
    });
    let turns = vec![
        vec![
            MockStreamEvent::ToolCall {
                id: "c1".to_string(),
                name: "subagent".to_string(),
                arguments: json!({"task": "child-task-8d21"}),
                call_id: None,
            },
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("parent wrap-up"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
    ];
    let parent = SessionBuilder::new(store.clone(), config, auth, ModelSelection::new("p", "m"))
        .expect("builder")
        .preamble("test parent".to_string())
        .model_factory(Arc::new(move |_, _, _| {
            Ok(ModelHandle::new(MockCompletionModel::from_stream_turns(
                turns.clone(),
            )))
        }))
        .subagents(parts)
        .dynamic_tool(subagent::subagent_tool())
        .create(&parent_cwd.display().to_string())
        .expect("parent session");
    let mut handle = host(&store, node.clone(), parent);
    let parent_id = handle.info().session_id.clone();
    handle.message(&parent_id, "go");

    let mut child_echo: Option<String> = None;
    let mut child_id: Option<String> = None;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(60), handle.next_event())
            .await
            .expect("frames keep coming")
            .expect("the stream stays open");
        let done = matches!(&frame.event,
            SessionEvent::RunFinished { output, .. } if output == "parent wrap-up");
        let on_child_stream =
            frame.stream.as_ref().map(|s| s.as_str()) == child_id.as_deref() && child_id.is_some();
        match &frame.event {
            SessionEvent::SessionOpened {
                id,
                parent: Some(parent),
                ..
            } if parent == &parent_id => child_id = Some(id.clone()),
            SessionEvent::ToolCall { name, .. } if on_child_stream && name == "echo" => {
                child_echo = Some("called".to_string());
            }
            SessionEvent::ToolResult { name, content, .. } if on_child_stream && name == "echo" => {
                assert!(content.contains("EXT-ECHOED:from the child"), "{content}");
            }
            _ => {}
        }
        if done {
            break;
        }
    }
    assert!(
        child_echo.is_some(),
        "the child's model called the extension's tool — its host booted"
    );

    // The deny crossing: the same child, `--without echo` — the
    // extension proxy is gone from its toolset, so its request (which
    // still carries the task marker) has no echo definition and the
    // plain-answer arm serves it.
    let deny_ctx = subagent::SpawnContext::new(
        Arc::new(subagent::SubagentParts {
            tools: Vec::new(),
            max_turns: 8,
            node: Arc::new(Node::new("test")),
            exe: PathBuf::from(env!("CARGO_BIN_EXE_tabit-core")),
            extensions: ext_root,
        }),
        Arc::new(tabit_session::subagent_pool::SubagentPool::new()),
        parent_id.clone(),
        ModelSelection::new("p", "m"),
        parent_cwd.clone(),
    );
    let mut denied = deny_ctx
        .spawn_subprocess()
        .cwd(parent_cwd.clone())
        .model(ModelSelection::new("p", "m"))
        .max_turns(8)
        .ephemeral(true)
        .without(vec!["echo".to_string()])
        .spawn()
        .await
        .expect("the denied child spawns");
    let denied_summary = deny_ctx
        .drive_subprocess(
            &mut denied,
            rig_agent::completion::Message::user("denied-task-8d21"),
            None,
        )
        .await;
    denied.wait_exit().await;
    assert_eq!(
        denied_summary.outcome,
        tabit_session::RunOutcome::Completed,
        "the denied child completed ({:?})",
        denied_summary.output
    );
    assert_eq!(
        denied_summary.output, "denied child done",
        "the denied child's request carried no echo tool to call"
    );

    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::remove_var("TABIT_CONFIG");
    }
}

// ---------------------------------------------------------------------------
// The follow-up surface (owner ruling 2026-09-26): completed subagents
// park in the session's pool under friendly ids, the `followup` tool
// addresses them, and the pool ages them out at the parent's turn
// boundary. Both tests ride the real binary end to end.
// ---------------------------------------------------------------------------

/// One scripted parent turn: plain text (a text turn is terminal —
/// each such turn is its own run).
fn text_turn(text: &str) -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::text(text.to_string()),
        MockStreamEvent::final_response_with_default_usage(),
    ]
}

/// One scripted parent turn that calls one tool.
fn tool_turn(id: &str, name: &str, arguments: serde_json::Value) -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
            call_id: None,
        },
        MockStreamEvent::final_response_with_default_usage(),
    ]
}

/// A parent session whose scripted model is held OUTSIDE the factory
/// (clones share state): later runs' turns are pushed at runtime —
/// `push_stream_turn` — so a script can address runtime-minted ids.
/// The parent mounts both subagent tools; its own provider is offline
/// (the factory is the script; the child's is the mock server).
fn pooled_parent(
    store: &SessionStore,
    cwd: &Path,
    node: Arc<Node>,
    first_turns: Vec<Vec<MockStreamEvent>>,
) -> (Session, rig_agent::test_utils::MockCompletionModel) {
    let config = Arc::new(
        tabit_config::TabitConfig::from_toml_str(
            r#"
[providers.p]
base_url = "http://127.0.0.1:1/v1"
api = "openai-completions"
keyless = true

[[providers.p.models]]
id = "m"
"#,
            Path::new("providers.toml"),
        )
        .expect("parent config"),
    );
    let auth = Arc::new(tabit_config::AuthConfig::default());
    let parts = Arc::new(subagent::SubagentParts {
        tools: Vec::new(),
        max_turns: 8,
        node,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_tabit-core")),
        extensions: cwd.join(".tabit/no-extensions"),
    });
    let model = rig_agent::test_utils::MockCompletionModel::from_stream_turns(first_turns);
    let scripted = model.clone();
    let session = SessionBuilder::new(store.clone(), config, auth, ModelSelection::new("p", "m"))
        .expect("builder")
        .preamble("test parent".to_string())
        .model_factory(Arc::new(move |_, _, _| {
            Ok(ModelHandle::new(scripted.clone()))
        }))
        .subagents(parts)
        .dynamic_tool(subagent::subagent_tool())
        .dynamic_tool(subagent::followup_tool())
        .create(&cwd.display().to_string())
        .expect("parent session");
    (session, model)
}

/// Drive the parent's next run to its terminal, collecting every
/// frame (the parent's and its children's alike).
async fn run_parent(handle: &mut SessionHost, parent_id: &str) -> Vec<tabit_protocol::EventFrame> {
    let mut frames = Vec::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(60), handle.next_event())
            .await
            .expect("frames keep coming")
            .expect("the stream stays open");
        let done = matches!(&frame.event,
            SessionEvent::RunFinished { .. }
            if frame.stream.as_ref().map(|s| s.as_str()) == Some(parent_id));
        frames.push(frame);
        if done {
            return frames;
        }
    }
}

/// A named tool result's (content, details) from a frame batch.
fn result_of(
    frames: &[tabit_protocol::EventFrame],
    name: &str,
) -> Option<(String, serde_json::Value)> {
    frames.iter().find_map(|frame| match &frame.event {
        SessionEvent::ToolResult {
            name: tool,
            content,
            details,
            ..
        } if tool == name => Some((content.clone(), details.clone().unwrap_or_default())),
        _ => None,
    })
}

/// The follow-up's continuity, end to end over the real binary: the
/// `subagent` tool's completed child PARKS (its result names the
/// friendly id), the `followup` tool reaches the SAME child session —
/// the child's second completion request carries the first task's
/// marker in history (the mock serving the second answer matches BOTH
/// markers; a respawned child would miss it and fail), and the second
/// result names the same ids.
#[tokio::test]
async fn a_followup_continues_the_same_child_session_across_runs() {
    let _guard = env_lock().lock().await;
    let server = MockServer::start();
    let _first = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .body_includes("TASK-MARKER-7f3a")
            .body_excludes("FOLLOWUP-MARKER-7f3a");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("child first answer"));
    });
    let second = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .body_includes("TASK-MARKER-7f3a")
            .body_includes("FOLLOWUP-MARKER-7f3a");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("child second answer"));
    });
    let config_path = stage_child_config("followup", &server);
    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::set_var("TABIT_CONFIG", &config_path);
    }

    let parent_cwd = test_dir("followup-parent");
    let store = SessionStore::new(test_dir("followup-store"));
    let node = Arc::new(Node::new("test"));
    let (parent, model) = pooled_parent(
        &store,
        &parent_cwd,
        node.clone(),
        vec![
            tool_turn(
                "c1",
                "subagent",
                json!({"task": "work on TASK-MARKER-7f3a"}),
            ),
            text_turn("parent wrap one"),
        ],
    );
    let mut handle = host(&store, node, parent);
    let parent_id = handle.info().session_id.clone();
    handle.message(&parent_id, "go");
    let run1 = run_parent(&mut handle, &parent_id).await;

    // The parked result names the friendly id and the child session.
    let (content, details) = result_of(&run1, "subagent").expect("the subagent result");
    let id = details
        .get("id")
        .and_then(|v| v.as_str())
        .expect("the result names the friendly id")
        .to_string();
    let child_id = details
        .get("child_id")
        .and_then(|v| v.as_str())
        .expect("the result names the child session")
        .to_string();
    assert!(
        content.contains("followup"),
        "the parked result teaches the follow-up: {content}"
    );

    // Run two: the follow-up, scripted now that the runtime id is
    // known (the model is held outside the factory; clones share
    // state, so the push reaches the session's model).
    model.push_stream_turn(tool_turn(
        "c2",
        "followup",
        json!({"id": id, "message": "now FOLLOWUP-MARKER-7f3a please"}),
    ));
    model.push_stream_turn(text_turn("parent wrap two"));
    handle.message(&parent_id, "again");
    let run2 = run_parent(&mut handle, &parent_id).await;

    let (follow_content, follow_details) =
        result_of(&run2, "followup").expect("the followup result");
    assert!(
        follow_content.contains("child second answer"),
        "the same child answered the follow-up: {follow_content}"
    );
    assert_eq!(
        follow_details.get("id").and_then(|v| v.as_str()),
        Some(id.as_str()),
        "the follow-up result names the same friendly id"
    );
    assert_eq!(
        follow_details.get("child_id").and_then(|v| v.as_str()),
        Some(child_id.as_str()),
        "the follow-up reached the same child session"
    );
    assert_eq!(
        second.calls(),
        1,
        "one same-session follow-up request — the first task rode history"
    );

    handle.close_commands();
    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::remove_var("TABIT_CONFIG");
    }
}

/// The aging boundary, end to end (the sweep rides the session's own
/// TurnStarted): a child used in turn T is followable through turn
/// T+5's tools — the success follow-up below is the sixth turn's
/// first call — and collected at turn T+6's start, so the same call
/// one idle cycle later is the expiry error. Aging turns are short
/// runs (a text turn is terminal); the counter rides turn starts, not
/// runs.
#[tokio::test]
async fn a_parked_subagent_lives_five_idle_turns_then_collects() {
    let _guard = env_lock().lock().await;
    let server = MockServer::start();
    let _first = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .body_includes("TASK-MARKER-91c4")
            .body_excludes("FOLLOWUP-MARKER-91c4");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("child first answer"));
    });
    let second = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .body_includes("TASK-MARKER-91c4")
            .body_includes("FOLLOWUP-MARKER-91c4");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("child second answer"));
    });
    let config_path = stage_child_config("aging", &server);
    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::set_var("TABIT_CONFIG", &config_path);
    }

    let parent_cwd = test_dir("aging-parent");
    let store = SessionStore::new(test_dir("aging-store"));
    let node = Arc::new(Node::new("test"));
    let (parent, model) = pooled_parent(
        &store,
        &parent_cwd,
        node.clone(),
        vec![
            tool_turn(
                "c1",
                "subagent",
                json!({"task": "work on TASK-MARKER-91c4"}),
            ),
            text_turn("parent wrap one"),
        ],
    );
    let mut handle = host(&store, node, parent);
    let parent_id = handle.info().session_id.clone();

    // Turn 1: the subagent (used); turn 2: the wrap.
    handle.message(&parent_id, "go");
    let run1 = run_parent(&mut handle, &parent_id).await;
    let (_, details) = result_of(&run1, "subagent").expect("the subagent result");
    let id = details
        .get("id")
        .and_then(|v| v.as_str())
        .expect("the friendly id")
        .to_string();

    // Turns 3-5: three idle one-turn runs.
    for n in 0..3 {
        model.push_stream_turn(text_turn(&format!("aging {n}")));
        handle.message(&parent_id, "idle");
        run_parent(&mut handle, &parent_id).await;
    }

    // Turn 6 — the FIFTH subsequent turn: still alive at its start,
    // so its first tool call is the success follow-up (turn 7 wraps).
    model.push_stream_turn(tool_turn(
        "c2",
        "followup",
        json!({"id": id, "message": "now FOLLOWUP-MARKER-91c4 please"}),
    ));
    model.push_stream_turn(text_turn("parent wrap two"));
    handle.message(&parent_id, "still within five");
    let run2 = run_parent(&mut handle, &parent_id).await;
    let (content, _) = result_of(&run2, "followup").expect("the followup result");
    assert!(
        content.contains("child second answer"),
        "the fifth subsequent turn still reaches the child: {content}"
    );

    // Turns 8-13: six idle one-turn runs — the fifth subsequent turn
    // passes unused, and turn 13's start collects.
    for n in 0..6 {
        model.push_stream_turn(text_turn(&format!("aging more {n}")));
        handle.message(&parent_id, "idle");
        run_parent(&mut handle, &parent_id).await;
    }

    // Turn 14: the same follow-up is now the expiry error (turn 15
    // wraps; the error is a tool result, the run continues).
    model.push_stream_turn(tool_turn(
        "c3",
        "followup",
        json!({"id": id, "message": "are you still there"}),
    ));
    model.push_stream_turn(text_turn("parent wrap three"));
    handle.message(&parent_id, "past five");
    let run3 = run_parent(&mut handle, &parent_id).await;
    let (content, _) = result_of(&run3, "followup").expect("the followup result");
    assert!(
        content.contains("no live subagent") && content.contains(&id),
        "the collected child is the clear expiry error: {content}"
    );
    assert_eq!(
        second.calls(),
        1,
        "the collected child served no request after collection"
    );

    handle.close_commands();
    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::remove_var("TABIT_CONFIG");
    }
}

/// A follow-up whose child run FAILS reaps the entry (nothing is
/// gained by parking a failed child — the ruling): the result is the
/// failure, and the very next address of the same id is the expiry
/// error, not a second chance.
#[tokio::test]
async fn a_failed_followup_reaps_the_child_and_the_next_address_is_the_expiry() {
    let _guard = env_lock().lock().await;
    let server = MockServer::start();
    let _first = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .body_includes("TASK-MARKER-4b8e")
            .body_excludes("FOLLOWUP-MARKER-4b8e");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_answer("child first answer"));
    });
    let failing = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .body_includes("TASK-MARKER-4b8e")
            .body_includes("FOLLOWUP-MARKER-4b8e");
        then.status(500).body("provider down");
    });
    let config_path = stage_child_config("failed-followup", &server);
    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::set_var("TABIT_CONFIG", &config_path);
    }

    let parent_cwd = test_dir("failed-followup-parent");
    let store = SessionStore::new(test_dir("failed-followup-store"));
    let node = Arc::new(Node::new("test"));
    let (parent, model) = pooled_parent(
        &store,
        &parent_cwd,
        node.clone(),
        vec![
            tool_turn(
                "c1",
                "subagent",
                json!({"task": "work on TASK-MARKER-4b8e"}),
            ),
            text_turn("parent wrap one"),
        ],
    );
    let mut handle = host(&store, node, parent);
    let parent_id = handle.info().session_id.clone();
    handle.message(&parent_id, "go");
    let run1 = run_parent(&mut handle, &parent_id).await;
    let (_, details) = result_of(&run1, "subagent").expect("the subagent result");
    let id = details
        .get("id")
        .and_then(|v| v.as_str())
        .expect("the friendly id")
        .to_string();

    // The failing follow-up: the child's provider 500s, the child's
    // run fails, and the result is the failure.
    model.push_stream_turn(tool_turn(
        "c2",
        "followup",
        json!({"id": id, "message": "now FOLLOWUP-MARKER-4b8e please"}),
    ));
    model.push_stream_turn(text_turn("parent wrap two"));
    handle.message(&parent_id, "follow up");
    let run2 = run_parent(&mut handle, &parent_id).await;
    let (content, _) = result_of(&run2, "followup").expect("the followup result");
    assert!(
        content.contains("the subagent failed"),
        "the failed follow-up reports the child's failure: {content}"
    );
    assert!(
        failing.calls() >= 1,
        "the failing follow-up request was served ({} attempts, retries included)",
        failing.calls()
    );

    // The entry is gone: the same id is now the expiry error.
    model.push_stream_turn(tool_turn(
        "c3",
        "followup",
        json!({"id": id, "message": "try again"}),
    ));
    model.push_stream_turn(text_turn("parent wrap three"));
    handle.message(&parent_id, "retry");
    let run3 = run_parent(&mut handle, &parent_id).await;
    let (content, _) = result_of(&run3, "followup").expect("the followup result");
    assert!(
        content.contains("no live subagent") && content.contains(&id),
        "the failed child left the pool: {content}"
    );

    handle.close_commands();
    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::remove_var("TABIT_CONFIG");
    }
}
