//! Subagent tests: a scripted parent drives the real `subagent` tool;
//! the child runs on its own scripted factory. The host test exercises
//! the full routing — announcement with `parent`, the child's own
//! stream, the parent's tool result.

use crate::subagent::{SubagentParts, subagent_tool};
use crate::tests::{Factory, temp_store, text_turn};
use crate::{Session, SessionHost, SessionHostWiring};
use rig_agent::agent::ModelHandle;
use rig_agent::completion::Message;
use rig_agent::test_utils::MockCompletionModel;
use rig_core::message::UserContent;
use serde_json::json;
use std::sync::Arc;
use tabit_protocol::{SessionEvent, StreamId};

fn plain_wiring(store: &crate::store::SessionStore) -> SessionHostWiring {
    SessionHostWiring {
        store: store.clone(),
        create: Arc::new(|| Err("not driven".to_string())),
        open: Arc::new(|_| Err("not driven".to_string())),
    }
}

/// A factory that answers every model request with the same turn
/// script — the child model.
fn child_factory(
    turns: Vec<Vec<rig_agent::test_utils::MockStreamEvent>>,
) -> crate::session::ModelFactory {
    Arc::new(move |_provider: &str, _model: &str, _cache_key: &str| {
        Ok(ModelHandle::new(MockCompletionModel::from_stream_turns(
            turns.clone(),
        )))
    })
}

/// The parent's scripted subagent call: one turn that calls the tool
/// with a task, then the wrap-up answer.
fn subagent_call_turn() -> Vec<rig_agent::test_utils::MockStreamEvent> {
    use rig_agent::test_utils::MockStreamEvent;
    vec![
        MockStreamEvent::ToolCall {
            id: "c1".to_string(),
            name: "subagent".to_string(),
            arguments: json!({"task": "count the files"}),
            call_id: None,
        },
        MockStreamEvent::final_response_with_default_usage(),
    ]
}

/// The parts, over a child factory running `turns` per child.
fn parts(
    store: &crate::store::SessionStore,
    turns: Vec<Vec<rig_agent::test_utils::MockStreamEvent>>,
) -> Arc<SubagentParts> {
    let config = crate::tests::test_config();
    Arc::new(SubagentParts {
        config: config.clone(),
        auth: crate::tests::test_auth(),
        store: store.clone(),
        tools: vec![crate::tests::echo_tool()],
        max_turns: 8,
        model_factory: child_factory(turns),
    })
}

fn subagent_parent(
    store: &crate::store::SessionStore,
    turns: Vec<Vec<rig_agent::test_utils::MockStreamEvent>>,
    parts: Arc<SubagentParts>,
) -> Session {
    Factory::new(turns)
        .into_builder(store.clone())
        .preamble("You are a test agent.".to_string())
        .dynamic_tool(subagent_tool())
        .subagents(parts)
        .create("C:/w")
        .expect("parent session")
}

#[tokio::test]
async fn a_subagent_answers_as_the_tools_result_and_streams_its_own_events() {
    let store = temp_store("subagent-host");
    let parent = subagent_parent(
        &store,
        vec![subagent_call_turn(), text_turn("parent wrap-up")],
        parts(&store, vec![text_turn("three files")]),
    );
    let mut handle = SessionHost::spawn(parent, Vec::new(), plain_wiring(&store));
    let parent_id = handle.info().session_id.clone();

    handle.message(&parent_id, "go");
    let mut frames = Vec::new();
    // Read until the parent's run finishes — the child's stream ends
    // before it.
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), handle.next_event())
            .await
            .expect("frames keep coming")
            .expect("the stream stays open");
        let done = matches!(&frame.event, SessionEvent::RunFinished { output, .. } if output == "parent wrap-up");
        frames.push(frame);
        if done {
            break;
        }
    }

    // The child announced through the same door, with the parent field
    // (v5), on its own stream — the announcement that carries a parent
    // (the boot's, earlier in the stream, does not).
    let announcement = frames
        .iter()
        .find_map(|f| match &f.event {
            SessionEvent::SessionOpened {
                id,
                parent: parent @ Some(_),
                ..
            } => Some((f, id.clone(), parent.clone())),
            _ => None,
        })
        .expect("a session_opened beyond the boot's");
    let (frame, child_id, announced_parent) = announcement;
    assert_eq!(
        frame.stream.as_ref().map(StreamId::as_str),
        Some(child_id.as_str()),
        "the child announces on its own stream"
    );
    assert_eq!(announced_parent.as_deref(), Some(parent_id.as_str()));
    let SessionEvent::SessionOpened { path, .. } = &frame.event else {
        unreachable!("matched above")
    };
    assert!(path.is_empty(), "ephemeral children carry no path: {path}");

    // The child's run ran whole on its own stream: task as user
    // message, the child answer, a terminal.
    let child_frames: Vec<_> = frames
        .iter()
        .filter(|f| f.stream.as_ref().is_some_and(|s| s.as_str() == child_id))
        .collect();
    assert!(
        child_frames
            .iter()
            .any(|f| matches!(&f.event, SessionEvent::UserMessage { text, .. } if text == "count the files")),
        "the task is the child's user message"
    );
    assert!(
        child_frames
            .iter()
            .any(|f| matches!(&f.event, SessionEvent::RunFinished { output, .. } if output == "three files")),
        "the child's answer streams: {:?}",
        child_frames.iter().map(|f| &f.event).collect::<Vec<_>>()
    );

    // The parent's tool result carries the child's answer as content,
    // the audit facts as details.
    let result = frames
        .iter()
        .find_map(|f| match &f.event {
            SessionEvent::ToolResult {
                name,
                content,
                details,
                ..
            } if name == "subagent" => Some((content.clone(), details.clone())),
            _ => None,
        })
        .expect("the parent's subagent tool result");
    assert!(
        result.0.contains("three files"),
        "the child's answer is the tool result: {}",
        result.0
    );
    let details = result.1.expect("details carry the audit facts");
    assert_eq!(details["child_id"], child_id);
    assert_eq!(details["outcome"], "completed");
    assert_eq!(details["turns"], 1);
    assert_eq!(
        details["usage"]["total_tokens"], 110,
        "the child's usage rides details"
    );

    // No child file anywhere — ephemeral.
    let leftovers: Vec<_> = std::fs::read_dir(store.dir())
        .map(|entries| entries.filter_map(|e| e.ok()).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .collect();
    assert_eq!(leftovers.len(), 1, "one file — the parent's: {leftovers:?}");
    handle.close_commands();
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn a_failing_child_is_an_error_result_not_a_fake_answer() {
    use rig_agent::test_utils::MockStreamEvent;
    // The child's stream errors on its only turn.
    let failing: crate::session::ModelFactory = Arc::new(move |_p: &str, _m: &str, _k: &str| {
        Ok(ModelHandle::new(MockCompletionModel::from_stream_turns(
            vec![vec![
                MockStreamEvent::Error(rig_core::test_utils::MockError::provider(
                    "provider melted",
                )),
                MockStreamEvent::final_response_with_default_usage(),
            ]],
        )))
    });
    let store = temp_store("subagent-fail");
    let parts = Arc::new(SubagentParts {
        config: crate::tests::test_config(),
        auth: crate::tests::test_auth(),
        store: store.clone(),
        tools: vec![],
        max_turns: 4,
        model_factory: failing,
    });
    // The parent's script: call the subagent, then wrap up regardless.
    let parent = subagent_parent(
        &store,
        vec![subagent_call_turn(), text_turn("recovered")],
        parts,
    );
    let mut handle = SessionHost::spawn(parent, Vec::new(), plain_wiring(&store));
    let id = handle.info().session_id.clone();
    handle.message(&id, "go");
    let mut frames = Vec::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), handle.next_event())
            .await
            .expect("frames keep coming")
            .expect("the stream stays open");
        let done = matches!(&frame.event, SessionEvent::RunFinished { output, .. } if output == "recovered");
        frames.push(frame);
        if done {
            break;
        }
    }
    // The parent's run RECOVERED (the failed tool result is data, not a
    // run failure), and the result is error-shaped with the reason.
    let result = frames
        .iter()
        .find_map(|f| match &f.event {
            SessionEvent::ToolResult {
                name,
                content,
                status,
                ..
            } if name == "subagent" => Some((content.clone(), status.clone())),
            _ => None,
        })
        .expect("the subagent tool result");
    assert!(
        result.0.contains("subagent failed"),
        "the failure is named: {}",
        result.0
    );
    assert!(
        matches!(result.1, tabit_protocol::ToolResultStatus::Failed { .. }),
        "the status is failed, never a success-shaped stub"
    );
    handle.close_commands();
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn without_mounted_parts_the_tool_refuses_clearly() {
    // A session without subagent parts mounted: the tool says so,
    // in-band.
    let store = temp_store("subagent-unmounted");
    let parent = Factory::new(vec![subagent_call_turn(), text_turn("anyway")])
        .into_builder(store.clone())
        .preamble("You are a test agent.".to_string())
        .dynamic_tool(subagent_tool())
        .create("C:/w")
        .expect("parent session");
    let mut handle = SessionHost::spawn(parent, Vec::new(), plain_wiring(&store));
    let id = handle.info().session_id.clone();
    handle.message(&id, "go");
    let mut frames = Vec::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), handle.next_event())
            .await
            .expect("frames keep coming")
            .expect("the stream stays open");
        let done = matches!(&frame.event, SessionEvent::RunFinished { .. });
        frames.push(frame);
        if done {
            break;
        }
    }
    let result = frames
        .iter()
        .find_map(|f| match &f.event {
            SessionEvent::ToolResult { name, content, .. } if name == "subagent" => {
                Some(content.clone())
            }
            _ => None,
        })
        .expect("the tool result");
    assert!(
        result.contains("not available"),
        "the refusal names the problem: {result}"
    );
    handle.close_commands();
    std::fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn the_child_toolset_excludes_the_subagent_tool() {
    // Recursion depth is enforced by omission — the parts the assembly
    // builds (mirrored here) never carry the subagent tool.
    let store = temp_store("subagent-norecursion");
    let parts = parts(&store, vec![text_turn("x")]);
    let names: Vec<&str> = parts.tools.iter().map(|t| t.name()).collect();
    assert!(
        !names.contains(&"subagent"),
        "children cannot spawn children: {names:?}"
    );
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn a_child_tool_call_dispatches_through_the_sidecar_inside_the_parent_body() {
    // The nesting question: the child's own tool phase dispatches from
    // inside a parent body that is itself on the sidecar runtime. The
    // child calls echo, sees its result, and answers — the whole nest
    // must complete, not deadlock.
    let store = temp_store("subagent-nested-tool");
    let parent = subagent_parent(
        &store,
        vec![subagent_call_turn(), text_turn("parent wrap-up")],
        parts(
            &store,
            vec![
                crate::tests::tool_turn("k1", "echo"),
                text_turn("child done after tool"),
            ],
        ),
    );
    let mut handle = SessionHost::spawn(parent, Vec::new(), plain_wiring(&store));
    let parent_id = handle.info().session_id.clone();
    handle.message(&parent_id, "go");
    let mut frames = Vec::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), handle.next_event())
            .await
            .expect("frames keep coming")
            .expect("the stream stays open");
        let done = matches!(&frame.event, SessionEvent::RunFinished { output, .. } if output == "parent wrap-up");
        frames.push(frame);
        if done {
            break;
        }
    }

    let child_stream = frames.iter().find_map(|f| match &f.event {
        SessionEvent::SessionOpened {
            id,
            parent: Some(_),
            ..
        } => Some(id.clone()),
        _ => None,
    });
    let child_id = child_stream.expect("the child announced");
    let child = |want: fn(&SessionEvent) -> bool| {
        frames
            .iter()
            .any(|f| f.stream.as_ref().is_some_and(|s| s.as_str() == child_id) && want(&f.event))
    };
    assert!(
        child(|e| matches!(e, SessionEvent::ToolCall { name, .. } if name == "echo")),
        "the child called its own tool"
    );
    assert!(
        child(|e| matches!(e, SessionEvent::ToolResult { name, .. } if name == "echo")),
        "and got its result"
    );
    // The parent's subagent result carries the post-tool answer.
    let result = frames
        .iter()
        .find_map(|f| match &f.event {
            SessionEvent::ToolResult { name, content, .. } if name == "subagent" => {
                Some(content.clone())
            }
            _ => None,
        })
        .expect("the parent's subagent result");
    assert!(
        result.contains("child done after tool"),
        "the child finished after its tool: {result}"
    );
    handle.close_commands();
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn aborting_the_parent_leashes_the_child_promptly() {
    // The cancellation contract: the parent's run token is the child's
    // leash. The child parks on a five-second tool; the parent's abort
    // must bring BOTH runs to their aborted terminals in milliseconds,
    // not after the tool elapses.
    use std::time::Instant;

    let slow = rig_agent::tool::DynamicTool::new(
        "slow",
        "Sleeps five seconds",
        json!({"type":"object","properties":{}}),
        |_ctx, _args| {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                Ok(rig_agent::tool::ToolOutput::text("finally"))
            })
        },
    );
    let store = temp_store("subagent-abort");
    let config = crate::tests::test_config();
    let parts = Arc::new(SubagentParts {
        config,
        auth: crate::tests::test_auth(),
        store: store.clone(),
        tools: vec![slow],
        max_turns: 4,
        model_factory: child_factory(vec![
            crate::tests::tool_turn("s1", "slow"),
            text_turn("never reached"),
        ]),
    });
    let parent = subagent_parent(&store, vec![subagent_call_turn()], parts);
    let mut handle = SessionHost::spawn(parent, Vec::new(), plain_wiring(&store));
    let parent_id = handle.info().session_id.clone();

    handle.message(&parent_id, "go");
    // Wait until the child is parked on its slow tool, then abort.
    let mut child_id = None;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), handle.next_event())
            .await
            .expect("frames keep coming")
            .expect("the stream stays open");
        match &frame.event {
            SessionEvent::SessionOpened {
                id,
                parent: Some(_),
                ..
            } => {
                child_id = Some(id.clone());
            }
            SessionEvent::ToolCall { name, .. } if name == "slow" => break,
            _ => {}
        }
    }
    let child_id = child_id.expect("the child announced");
    let started = Instant::now();
    handle.abort(&parent_id);

    // Both aborted terminals arrive promptly — the leash, not the
    // five-second tool.
    let mut parent_aborted = false;
    let mut child_aborted = false;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(3), handle.next_event())
            .await
            .expect("the terminals arrive within the leash budget")
            .expect("the stream stays open while the host lives");
        if let SessionEvent::RunAborted { .. } = &frame.event {
            if frame
                .stream
                .as_ref()
                .is_some_and(|s| s.as_str() == parent_id)
            {
                parent_aborted = true;
            }
            if frame
                .stream
                .as_ref()
                .is_some_and(|s| s.as_str() == child_id)
            {
                child_aborted = true;
            }
        }
        if parent_aborted && child_aborted {
            break;
        }
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "both runs aborted in {:?} — the leash, not the tool",
        started.elapsed()
    );
    handle.close_commands();
    std::fs::remove_dir_all(store.dir()).ok();
}

// --- the framework split: overrides, allow-lists, and extension-style
// --- spawners driving children without the example tool ---

/// A tool that reports the session cwd it runs under — the probe for
/// cwd scoping.
fn cwd_probe_tool() -> rig_agent::tool::DynamicTool {
    rig_agent::tool::DynamicTool::new(
        "cwd_probe",
        "Reports the session cwd",
        json!({"type":"object","properties":{}}),
        |ctx, _args| {
            Box::pin(async move {
                let cwd = ctx
                    .get::<rig_agent::tool::SessionCwd>()
                    .map(|c| c.0.display().to_string())
                    .unwrap_or_else(|| "(no session cwd)".to_string());
                Ok(rig_agent::tool::ToolOutput::text(cwd))
            })
        },
    )
}

/// A factory that records every (provider, model) request and keeps
/// the models, so tests can read back what the child actually sent.
type Recording = (
    crate::session::ModelFactory,
    Arc<std::sync::Mutex<Vec<(String, String)>>>,
    Arc<std::sync::Mutex<Vec<MockCompletionModel>>>,
);

fn recording_child_factory(turns: Vec<Vec<rig_agent::test_utils::MockStreamEvent>>) -> Recording {
    let requested = Arc::new(std::sync::Mutex::new(Vec::new()));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let factory: crate::session::ModelFactory = {
        let requested = requested.clone();
        let models = models.clone();
        Arc::new(move |provider: &str, model: &str, _cache_key: &str| {
            requested
                .lock()
                .expect("requested")
                .push((provider.to_string(), model.to_string()));
            let mock = MockCompletionModel::from_stream_turns(turns.clone());
            models.lock().expect("models").push(mock.clone());
            Ok(ModelHandle::new(mock))
        })
    };
    (factory, requested, models)
}

/// Drive `parent` (whose script ends with a `wrap_up` answer) and
/// collect frames through that terminal.
async fn drive_to_wrap_up(
    handle: &mut SessionHost,
    parent_id: &str,
    wrap_up: &str,
) -> Vec<tabit_protocol::EventFrame> {
    handle.message(parent_id, "go");
    let mut frames = Vec::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), handle.next_event())
            .await
            .expect("frames keep coming")
            .expect("the stream stays open");
        let done =
            matches!(&frame.event, SessionEvent::RunFinished { output, .. } if output == wrap_up);
        frames.push(frame);
        if done {
            return frames;
        }
    }
}

/// The system message the child's first request rode — its preamble.
fn child_preamble(models: &Arc<std::sync::Mutex<Vec<MockCompletionModel>>>) -> String {
    let requests = models
        .lock()
        .expect("models")
        .last()
        .expect("the child requested")
        .requests();
    // OneOrMany is never empty; `first()` is total.
    match requests[0].chat_history.first() {
        Message::System { content } => content.clone(),
        other => panic!("the preamble rides as the leading system message: {other:?}"),
    }
}

#[tokio::test]
async fn model_and_cwd_overrides_reach_the_child_and_its_preamble() {
    let store = temp_store("subagent-overrides");
    let scope = std::env::temp_dir().join("tabit-subagent-scope");
    std::fs::create_dir_all(&scope).expect("scope dir");

    let (factory, requested, models) = recording_child_factory(vec![
        crate::tests::tool_turn("p1", "cwd_probe"),
        text_turn("scoped answer"),
    ]);
    // The child's config must define the override model.
    let config = Arc::new(
        tabit_config::TabitConfig::from_toml_str(
            r#"
[providers.p]
base_url = "http://127.0.0.1:9999/v1"
api = "openai-completions"

[[providers.p.models]]
id = "m"

[[providers.p.models]]
id = "cheap"
"#,
            std::path::Path::new("providers.toml"),
        )
        .expect("config"),
    );
    let parts = Arc::new(SubagentParts {
        config,
        auth: crate::tests::test_auth(),
        store: store.clone(),
        tools: vec![cwd_probe_tool()],
        max_turns: 8,
        model_factory: factory,
    });

    use rig_agent::test_utils::MockStreamEvent;
    let call = vec![
        MockStreamEvent::ToolCall {
            id: "c1".to_string(),
            name: "subagent".to_string(),
            arguments: json!({
                "task": "study the reference project",
                "model": "p/cheap",
                "cwd": scope.display().to_string(),
            }),
            call_id: None,
        },
        MockStreamEvent::final_response_with_default_usage(),
    ];
    let parent = Factory::new(vec![call, text_turn("parent wrap-up")])
        .into_builder(store.clone())
        .preamble("You are a test agent.".to_string())
        .dynamic_tool(subagent_tool())
        .subagents(parts)
        .create("C:/w")
        .expect("parent session");
    let mut handle = SessionHost::spawn(parent, Vec::new(), plain_wiring(&store));
    let parent_id = handle.info().session_id.clone();
    let frames = drive_to_wrap_up(&mut handle, &parent_id, "parent wrap-up").await;

    // The model override reached the factory, and the announcement.
    assert!(
        requested
            .lock()
            .expect("requested")
            .contains(&("p".to_string(), "cheap".to_string())),
        "the child was built for p/cheap: {:?}",
        requested.lock().expect("requested")
    );
    let (_, child_model) = frames
        .iter()
        .find_map(|f| match &f.event {
            SessionEvent::SessionOpened {
                model,
                parent: Some(_),
                ..
            } => Some((f, model.clone())),
            _ => None,
        })
        .expect("the child announced");
    assert_eq!(child_model.model, "cheap");

    // The cwd override reached the child's tools…
    let probe = frames
        .iter()
        .find_map(|f| match &f.event {
            SessionEvent::ToolResult { name, content, .. } if name == "cwd_probe" => {
                Some(content.clone())
            }
            _ => None,
        })
        .expect("the probe ran");
    assert!(
        probe.contains("tabit-subagent-scope"),
        "the child's tools resolve against the override cwd: {probe}"
    );
    // …and its preamble says where it is (the per-agent truthfulness).
    let preamble = child_preamble(&models);
    assert!(
        preamble.contains("tabit-subagent-scope"),
        "the child's prompt names its own cwd: {}…",
        &preamble[..preamble.len().min(400)]
    );
    handle.close_commands();
    std::fs::remove_dir_all(store.dir()).ok();
    std::fs::remove_dir_all(&scope).ok();
}

#[tokio::test]
async fn an_allow_list_restricts_the_child_toolset() {
    // The allow-list keeps `echo` and drops `cwd_probe`; the child's
    // scripted call to the dropped tool fails in-band, loudly.
    let store = temp_store("subagent-allowlist");
    let (factory, _requested, models) = recording_child_factory(vec![
        crate::tests::tool_turn("a1", "cwd_probe"),
        text_turn("after the refusal"),
    ]);
    let parts = Arc::new(SubagentParts {
        config: crate::tests::test_config(),
        auth: crate::tests::test_auth(),
        store: store.clone(),
        tools: vec![crate::tests::echo_tool(), cwd_probe_tool()],
        max_turns: 8,
        model_factory: factory,
    });
    use rig_agent::test_utils::MockStreamEvent;
    let call = vec![
        MockStreamEvent::ToolCall {
            id: "c1".to_string(),
            name: "subagent".to_string(),
            arguments: json!({
                "task": "read-only work",
                "tools": ["echo"],
            }),
            call_id: None,
        },
        MockStreamEvent::final_response_with_default_usage(),
    ];
    let parent = Factory::new(vec![call, text_turn("parent wrap-up")])
        .into_builder(store.clone())
        .preamble("You are a test agent.".to_string())
        .dynamic_tool(subagent_tool())
        .subagents(parts)
        .create("C:/w")
        .expect("parent session");
    let mut handle = SessionHost::spawn(parent, Vec::new(), plain_wiring(&store));
    let id = handle.info().session_id.clone();
    let _frames = drive_to_wrap_up(&mut handle, &id, "parent wrap-up").await;

    // The dropped tool was called and answered IN-BAND: the engine
    // folds an unknown-tool error into the model-facing result (no
    // wire tool_result event for never-registered tools), so the
    // proof is the model's own next request carrying the error.
    let requests = models
        .lock()
        .expect("models")
        .last()
        .expect("the child requested")
        .requests();
    let history = &requests[1].chat_history;
    let in_band = history.iter().any(|message| match message {
        Message::User { content } => content.iter().any(|part| match part {
            UserContent::ToolResult(result) => result.content.iter().any(|c| {
                c.as_text()
                    .is_some_and(|t| t.contains("cwd_probe") && t.contains("not offered"))
            }),
            _ => false,
        }),
        _ => false,
    });
    assert!(
        in_band,
        "the dropped tool's call came back as an in-band error: {:?}",
        history
    );
    handle.close_commands();
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn extension_style_spawners_drive_children_through_the_framework() {
    // The separation proof: a tool that is NOT the example `subagent`
    // builds a child with its own policy (its own preamble, no gate)
    // and drives it through the SpawnContext alone.
    let (factory, _requested, models) =
        recording_child_factory(vec![text_turn("from the custom spawner")]);
    let spawner = rig_agent::tool::DynamicTool::new(
        "research",
        "The extension's own subagent shape",
        json!({"type":"object","properties":{"topic":{"type":"string"}}}),
        |ctx, args| {
            let topic = args
                .get("topic")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Box::pin(async move {
                use rig_agent::tool::ToolExecutionError;
                let spawn = ctx
                    .get::<Arc<crate::subagent::SpawnContext>>()
                    .cloned()
                    .ok_or_else(|| ToolExecutionError::other("no spawn context"))?;
                let parts = spawn.parts();
                // The extension's policy: its own preamble, no gate, an
                // empty toolset, the parent's model and cwd.
                let mut child = crate::SessionBuilder::new(
                    parts.store.clone(),
                    parts.config.clone(),
                    parts.auth.clone(),
                    spawn.parent_selection().clone(),
                )
                .map_err(|e| ToolExecutionError::other(e.to_string()))?
                .preamble(format!("CUSTOM BRIEF: research {topic} thoroughly."))
                .max_turns(parts.max_turns)
                .model_factory(parts.model_factory.clone())
                .ephemeral(&spawn.parent_cwd().display().to_string())
                .map_err(|e| ToolExecutionError::other(e.to_string()))?;
                if let Some(hub) = spawn.parent_hub() {
                    child.attach_interaction(hub.clone());
                }
                spawn.announce(&child);
                let token = ctx.get::<tokio_util::sync::CancellationToken>().cloned();
                let summary = spawn.drive(&mut child, Message::user(topic), token).await;
                Ok(rig_agent::tool::ToolOutput::text(summary.output))
            })
        },
    );

    let store = temp_store("subagent-extension");
    let parts = Arc::new(SubagentParts {
        config: crate::tests::test_config(),
        auth: crate::tests::test_auth(),
        store: store.clone(),
        tools: vec![],
        max_turns: 8,
        model_factory: factory,
    });
    use rig_agent::test_utils::MockStreamEvent;
    let call = vec![
        MockStreamEvent::ToolCall {
            id: "c1".to_string(),
            name: "research".to_string(),
            arguments: json!({"topic": "history of rust"}),
            call_id: None,
        },
        MockStreamEvent::final_response_with_default_usage(),
    ];
    let parent = Factory::new(vec![call, text_turn("parent wrap-up")])
        .into_builder(store.clone())
        .preamble("You are a test agent.".to_string())
        .dynamic_tool(spawner)
        .subagents(parts)
        .create("C:/w")
        .expect("parent session");
    let mut handle = SessionHost::spawn(parent, Vec::new(), plain_wiring(&store));
    let id = handle.info().session_id.clone();
    let frames = drive_to_wrap_up(&mut handle, &id, "parent wrap-up").await;

    // The extension's tool got the child's answer…
    let result = frames
        .iter()
        .find_map(|f| match &f.event {
            SessionEvent::ToolResult { name, content, .. } if name == "research" => {
                Some(content.clone())
            }
            _ => None,
        })
        .expect("the extension tool's result");
    assert_eq!(result, "from the custom spawner");
    // …the child announced with the parent field…
    assert!(frames.iter().any(|f| matches!(&f.event,
            SessionEvent::SessionOpened { parent: Some(p), .. } if p == &id)));
    // …and the child's preamble was the EXTENSION's composition.
    let preamble = child_preamble(&models);
    assert!(
        preamble.starts_with("CUSTOM BRIEF"),
        "the extension owns the policy: {preamble:?}"
    );
    handle.close_commands();
    std::fs::remove_dir_all(store.dir()).ok();
}
