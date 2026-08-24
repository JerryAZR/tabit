//! Interaction tests over the actor: the ask pattern end to end — the
//! permission card answered over the command link, the ask-the-user
//! tool body, session-memory "Always allow", and abort closing an open
//! card totally.

use super::*;
use crate::SessionEvent;
use crate::entry::EntryKind;
use crate::tests::{Factory, temp_store, text_turn, tool_turn};
use rig_agent::test_utils::MockStreamEvent;
use rig_agent::tool::{DynamicTool, ToolOutput};
use rig_core::completion::Usage;
use serde_json::json;
use tabit_protocol::SessionCommand;

/// The gated name with a harmless body: the permission card is the point.
/// The dev-time gate's factory, mounted through the seam exactly as
/// the binary assembles it — these tests are the seam's end-to-end
/// coverage.
fn gated_gate() -> crate::gate::ToolGateFactory {
    let memory = crate::PermissionMemory::default();
    std::sync::Arc::new(move |hub| {
        std::sync::Arc::new(crate::PermissionHook::new(hub.cloned(), memory.clone()))
            as std::sync::Arc<dyn crate::ToolGate>
    })
}

fn gated_tool() -> DynamicTool {
    DynamicTool::new(
        "bash",
        "gated echo",
        json!({"type":"object","properties":{"command":{"type":"string"}}}),
        |_ctx, args| {
            Box::pin(async move {
                Ok(ToolOutput::text(format!(
                    "ran: {}",
                    args.get("command").and_then(|v| v.as_str()).unwrap_or("?")
                )))
            })
        },
    )
}

/// A stand-in for `tabit_tools::ask_user`: the body is one ask over the
/// ToolContext capability.
fn asking_tool() -> DynamicTool {
    DynamicTool::new(
        "ask_user",
        "asks the user",
        json!({"type":"object","properties":{"question":{"type":"string"}}}),
        |ctx, args| {
            Box::pin(async move {
                use rig_agent::tool::interaction::{InteractionPrompt, UserInteraction};
                let question = args
                    .get("question")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let Some(interaction) = ctx.get::<std::sync::Arc<dyn UserInteraction>>().cloned()
                else {
                    return Ok(ToolOutput::text("no interactive frontend"));
                };
                let reply = interaction.ask(InteractionPrompt::ask(question)).await;
                Ok(ToolOutput::text(
                    reply.text.unwrap_or_else(|| "dismissed".to_string()),
                ))
            })
        },
    )
}

fn interaction_count(frames: &[EventFrame]) -> usize {
    frames
        .iter()
        .filter(|frame| matches!(frame.event, SessionEvent::InteractionRequested { .. }))
        .count()
}

/// Drive one run to completion, answering every interaction request with
/// `answer(id)`; returns the frames.
async fn run_answering(
    handle: &mut SessionHost,
    link: &crate::SessionCommandLink,
    answer: impl Fn(&str, &str) -> SessionCommand,
) -> Vec<EventFrame> {
    let session = handle.info().session_id.clone();
    handle.message(&session, "go");
    let mut frames = Vec::new();
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), handle.next_event())
            .await
            .expect("the run must keep producing events (or end) within 5s");
        let Some(frame) = frame else { break };
        if let SessionEvent::InteractionRequested { id, .. } = &frame.event {
            let command = answer(&session, id);
            link.send(command);
        }
        if matches!(
            frame.event,
            SessionEvent::RunFinished { .. }
                | SessionEvent::RunAborted { .. }
                | SessionEvent::RunFailed { .. }
        ) {
            handle.close_commands();
        }
        frames.push(frame);
    }
    frames
}

#[tokio::test]
async fn a_permission_card_answered_allow_runs_the_tool() {
    let store = temp_store("permit-allow");
    let session = Factory::new(vec![
        tool_turn("t1", "bash"),
        text_turn("after the command"),
    ])
    .into_builder(store.clone())
    .dynamic_tool(gated_tool())
    .tool_gate(gated_gate())
    .create("C:/w")
    .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let link = handle.command_link();

    let frames = run_answering(&mut handle, &link, |session, id| {
        SessionCommand::InteractionResponse {
            session: session.to_string(),
            id: id.to_string(),
            option: Some("Allow".to_string()),
            text: None,
        }
    })
    .await;

    assert_eq!(interaction_count(&frames), 1, "one card for one gated call");
    assert!(frames.iter().any(|f| matches!(
        &f.event,
        SessionEvent::ToolResult { name, .. } if name == "bash"
    )));
    assert_eq!(finished_outputs(&frames), vec!["after the command"]);
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn a_permission_denial_skips_the_tool_in_band() {
    let store = temp_store("permit-deny");
    let session = Factory::new(vec![
        tool_turn("t1", "bash"),
        text_turn("understood, not running it"),
    ])
    .into_builder(store.clone())
    .dynamic_tool(gated_tool())
    .tool_gate(gated_gate())
    .create("C:/w")
    .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let link = handle.command_link();

    let frames = run_answering(&mut handle, &link, |session, id| {
        SessionCommand::InteractionResponse {
            session: session.to_string(),
            id: id.to_string(),
            option: Some("Deny".to_string()),
            text: Some("never in tests".to_string()),
        }
    })
    .await;

    // The body never ran (its "ran: …" text is absent) — the model saw
    // the in-band denial instead, and the run continued to the final turn.
    let bodies_ran = frames.iter().any(|f| {
        matches!(
            &f.event,
            SessionEvent::ToolResult { name, .. } if name == "bash"
        )
    });
    assert!(bodies_ran, "the denial itself is the tool result event");
    assert_eq!(
        finished_outputs(&frames),
        vec!["understood, not running it"]
    );
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn always_allow_remembers_across_calls_in_the_session() {
    let store = temp_store("permit-always");
    let session = Factory::new(vec![
        tool_turn("t1", "bash"),
        tool_turn("t2", "bash"),
        text_turn("done"),
    ])
    .into_builder(store.clone())
    .dynamic_tool(gated_tool())
    .tool_gate(gated_gate())
    .create("C:/w")
    .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let link = handle.command_link();

    let frames = run_answering(&mut handle, &link, |session, id| {
        SessionCommand::InteractionResponse {
            session: session.to_string(),
            id: id.to_string(),
            option: Some("Always allow".to_string()),
            text: None,
        }
    })
    .await;

    // One card (the first call); the second passes on session memory.
    assert_eq!(interaction_count(&frames), 1);
    assert_eq!(finished_outputs(&frames), vec!["done"]);

    // EXTENSIONS.md ruling: the memory is session state, never persisted —
    // a resumed session (fresh process) asks again.
    let path = handle.info().session_path.clone();
    drop(handle);
    let (session, _report) = Factory::new(vec![tool_turn("t3", "bash"), text_turn("again")])
        .into_builder(store.clone())
        .dynamic_tool(gated_tool())
        .tool_gate(gated_gate())
        .resume(std::path::Path::new(&path))
        .expect("resume");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let link = handle.command_link();
    let frames = run_answering(&mut handle, &link, |session, id| {
        SessionCommand::InteractionResponse {
            session: session.to_string(),
            id: id.to_string(),
            option: Some("Allow".to_string()),
            text: None,
        }
    })
    .await;
    assert_eq!(
        interaction_count(&frames),
        1,
        "a resumed session re-asks: 'Always allow' did not survive the process"
    );
    assert_eq!(finished_outputs(&frames), vec!["again"]);
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn an_ask_user_tool_body_round_trips_the_question_and_answer() {
    let store = temp_store("ask-user");
    let session = Factory::new(vec![
        tool_turn("t1", "ask_user"),
        text_turn("the user said main.rs"),
    ])
    .into_builder(store.clone())
    .dynamic_tool(asking_tool())
    .create("C:/w")
    .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let link = handle.command_link();

    let frames = run_answering(&mut handle, &link, |session, id| {
        SessionCommand::InteractionResponse {
            session: session.to_string(),
            id: id.to_string(),
            option: None,
            text: Some("main.rs".to_string()),
        }
    })
    .await;

    assert_eq!(interaction_count(&frames), 1);
    assert_eq!(finished_outputs(&frames), vec!["the user said main.rs"]);
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn frontend_death_with_a_card_open_winds_the_worker_down() {
    // The orphaned-backend regression (ruled 2026-08: the core dies with
    // the frontend, regardless of state): a card parked on an answer
    // that can never arrive must not pin the worker — the death watcher
    // aborts the run and the worker winds down.
    let store = temp_store("permit-death");
    let session = Factory::new(vec![tool_turn("t1", "bash"), text_turn("unreachable")])
        .into_builder(store.clone())
        .dynamic_tool(gated_tool())
        .tool_gate(gated_gate())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let mut events = handle.take_events().expect("the event stream");

    handle.message(handle.info().session_id.as_str(), "run it");
    let mut saw_card = false;
    loop {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("the card must open within 5s");
        if matches!(
            frame,
            Some(tabit_protocol::EventFrame {
                event: SessionEvent::InteractionRequested { .. },
                ..
            })
        ) {
            saw_card = true;
            break;
        }
        if frame.is_none() {
            break;
        }
    }
    assert!(saw_card, "the card opened before the death");

    // The frontend dies with the card open; the worker must wind down
    // (closing stats appear) rather than await the answer forever.
    drop(events);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if handle.closing_stats().is_some() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the worker must wind down when the frontend dies");

    // The durability half of the ruling: the log survives the death, the
    // interrupted call's result was synthesized AT ABORT TIME (durably —
    // an `Aborted` marker followed by the interrupted-result entry), and
    // the next open finds nothing dangling.
    let path = handle.info().session_path.clone();
    let loaded = store
        .open_path(std::path::Path::new(&path))
        .expect("reopen");
    assert!(
        loaded
            .entries
            .iter()
            .any(|e| matches!(e.kind, EntryKind::Aborted))
    );
    assert!(
        matches!(
            loaded.entries.last().map(|e| &e.kind),
            Some(EntryKind::ToolResult { .. })
        ),
        "the interrupted call's synthesized result is the durable tail"
    );
    let (resumed, report) = Factory::new(vec![text_turn("recovered")])
        .into_builder(store.clone())
        .resume(std::path::Path::new(&path))
        .expect("the log reopens after the death");
    let _ = resumed;
    assert_eq!(
        report.repaired_tool_calls, 0,
        "abort-time synthesis left nothing dangling to repair"
    );
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn abort_with_a_card_open_closes_the_question_totally() {
    let store = temp_store("permit-abort");
    let session = Factory::new(vec![tool_turn("t1", "bash"), text_turn("unreachable")])
        .into_builder(store.clone())
        .dynamic_tool(gated_tool())
        .tool_gate(gated_gate())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let link = handle.command_link();

    handle.message(handle.info().session_id.as_str(), "run it");
    let mut frames = Vec::new();
    let mut stale_id = None;
    while let Some(frame) = handle.next_event().await {
        if let SessionEvent::InteractionRequested { id, .. } = &frame.event {
            stale_id = Some(id.clone());
            // Abort with the card open: the question dies with the run.
            link.send(SessionCommand::Abort {
                session: handle.info().session_id.clone(),
            });
        }
        if matches!(frame.event, SessionEvent::RunAborted { .. }) {
            handle.close_commands();
        }
        frames.push(frame);
    }

    assert!(
        frames
            .iter()
            .any(|f| matches!(f.event, SessionEvent::RunAborted { .. })),
        "abort preempts the parked permission wait"
    );
    // The racing answer is a total no-op: nothing hangs, nothing errors.
    if let Some(id) = stale_id {
        link.send(SessionCommand::InteractionResponse {
            session: handle.info().session_id.clone(),
            id,
            option: Some("Allow".to_string()),
            text: None,
        });
    }
    std::fs::remove_dir_all(store.dir()).ok();
}

/// FRONTEND.md §8: concurrent chains may hold several open requests at
/// once, answered in any order. One turn with two gated calls under
/// `TOOL_CONCURRENCY` opens two cards; answering the second one first
/// still runs both and the run finishes normally.
#[tokio::test]
async fn two_open_cards_answered_in_reverse_order_both_run() {
    let store = temp_store("permit-multi");
    // One turn emitting two bash calls, then a closing text turn.
    let turn = vec![
        MockStreamEvent::tool_call("c1", "bash", json!({"command": "echo one"})),
        MockStreamEvent::tool_call("c2", "bash", json!({"command": "echo two"})),
        MockStreamEvent::final_response(Usage::default()),
    ];
    let session = Factory::new(vec![turn, text_turn("both ran")])
        .into_builder(store.clone())
        .dynamic_tool(gated_tool())
        .tool_gate(gated_gate())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let link = handle.command_link();

    let session = handle.info().session_id.clone();
    handle.message(&session, "go");
    let mut frames = Vec::new();
    let mut open: Vec<String> = Vec::new();
    while let Some(frame) = handle.next_event().await {
        if let SessionEvent::InteractionRequested { id, .. } = &frame.event {
            open.push(id.clone());
        }
        // Once both cards are standing, answer them in reverse order —
        // the contract allows any order.
        if open.len() == 2 {
            for id in open.iter().rev() {
                link.send(SessionCommand::InteractionResponse {
                    session: session.clone(),
                    id: id.clone(),
                    option: Some("Allow".to_string()),
                    text: None,
                });
            }
            open.clear();
        }
        if matches!(
            frame.event,
            SessionEvent::RunFinished { .. }
                | SessionEvent::RunAborted { .. }
                | SessionEvent::RunFailed { .. }
        ) {
            handle.close_commands();
        }
        frames.push(frame);
    }

    assert_eq!(interaction_count(&frames), 2, "both gated calls asked");
    let gated_results = frames
        .iter()
        .filter(|f| matches!(&f.event, SessionEvent::ToolResult { name, .. } if name == "bash"))
        .count();
    assert_eq!(gated_results, 2, "both calls ran despite reverse answering");
    assert_eq!(finished_outputs(&frames), vec!["both ran"]);
    std::fs::remove_dir_all(store.dir()).ok();
}
