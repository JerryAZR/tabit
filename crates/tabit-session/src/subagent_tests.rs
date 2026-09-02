//! Subagent tests: a scripted parent drives the real `subagent` tool;
//! the child runs on its own scripted factory. The host test exercises
//! the full routing — announcement with `parent`, the child's own
//! stream, the parent's tool result.

use crate::subagent::{SubagentParts, subagent_tool};
use crate::tests::{Factory, temp_store, text_turn};
use crate::{Session, SessionHost, SessionHostWiring};
use rig_agent::agent::ModelHandle;
use rig_agent::test_utils::MockCompletionModel;
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

/// A factory that answers every model request with the same one-turn
/// script — the child model.
fn child_factory(answer: &str) -> crate::session::ModelFactory {
    let answer = answer.to_string();
    Arc::new(move |_provider: &str, _model: &str, _cache_key: &str| {
        Ok(ModelHandle::new(MockCompletionModel::from_stream_turns(
            vec![text_turn(&answer)],
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

/// The parts, over a child factory that answers `answer`.
fn parts(store: &crate::store::SessionStore, answer: &str) -> Arc<SubagentParts> {
    let config = crate::tests::test_config();
    Arc::new(SubagentParts {
        config: config.clone(),
        auth: crate::tests::test_auth(),
        store: store.clone(),
        tools: vec![crate::tests::echo_tool()],
        base_preamble: "You are a test agent.".to_string(),
        max_turns: 8,
        model_factory: child_factory(answer),
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
        parts(&store, "three files"),
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
        base_preamble: "You are a test agent.".to_string(),
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
    let parts = parts(&store, "x");
    let names: Vec<&str> = parts.tools.iter().map(|t| t.name()).collect();
    assert!(
        !names.contains(&"subagent"),
        "children cannot spawn children: {names:?}"
    );
    std::fs::remove_dir_all(store.dir()).ok();
}
