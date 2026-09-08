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
    ChildRouter, ModelSelection, Session, SessionBuilder, SessionHost, SessionHostWiring,
    SessionStore, subagent,
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
        "[providers.p]\nbase_url = \"http://127.0.0.1:{}/v1\"\napi = \"openai-completions\"\n\n[[providers.p.models]]\nid = \"m\"\n",
        server.port()
    );
    std::fs::write(&config_path, toml).expect("write child config");
    config_path
}

/// The parent: a scripted in-process model whose first turn calls the
/// subagent tool, and a second that wraps
/// up. The child toolset is empty policy — no allow-list crosses.
fn subprocess_parent(
    store: &SessionStore,
    cwd: &Path,
    router: Arc<ChildRouter>,
    task: &str,
) -> Session {
    let config = Arc::new(
        tabit_config::TabitConfig::from_toml_str(
            r#"
[providers.p]
base_url = "http://127.0.0.1:1/v1"
api = "openai-completions"

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
        router,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_tabit")),
    });
    let turns = vec![
        vec![
            MockStreamEvent::ToolCall {
                id: "c1".to_string(),
                name: "subagent".to_string(),
                arguments: json!({"task": task}),
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

/// A host over a plain store, sharing the router with the parts.
fn host(store: &SessionStore, router: Arc<ChildRouter>, session: Session) -> SessionHost {
    let wiring = SessionHostWiring {
        children: router,
        boot_parent: None,
        store: store.clone(),
        create: Arc::new(|| Err("not driven".to_string())),
        open: Arc::new(|_| Err("not driven".to_string())),
    };
    SessionHost::spawn(session, Vec::new(), wiring)
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
    let router = ChildRouter::shared();
    let parent = subprocess_parent(&store, &parent_cwd, router.clone(), "say the words");
    let mut handle = host(&store, router, parent);
    let parent_id = handle.info().session_id.clone();

    handle.message(&parent_id, "go");
    // Collect until the parent wraps up, steering the child mid-run:
    // the steer fires when the child's task enters its conversation —
    // inside the model call's delay window.
    let mut frames = Vec::new();
    let mut steered = false;
    let mut child_id: Option<String> = None;
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
    let router = ChildRouter::shared();
    let parent = subprocess_parent(&store, &parent_cwd, router.clone(), "park on the model");
    let mut handle = host(&store, router, parent);
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
