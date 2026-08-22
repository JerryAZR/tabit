//! Interaction tests over the actor: the ask pattern end to end — the
//! permission card answered over the command link, the ask-the-user
//! tool body, session-memory "Always allow", and abort closing an open
//! card totally.

use super::*;
use crate::SessionEvent;
use crate::tests::{Factory, temp_store, text_turn, tool_turn};
use rig_agent::tool::{DynamicTool, ToolOutput};
use serde_json::json;
use tabit_protocol::SessionCommand;

/// The gated name with a harmless body: the permission card is the point.
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
    handle: &mut SessionHandle,
    link: &crate::SessionCommandLink,
    answer: impl Fn(&str) -> SessionCommand,
) -> Vec<EventFrame> {
    handle.message("go");
    let mut frames = Vec::new();
    while let Some(frame) = handle.next_event().await {
        if let SessionEvent::InteractionRequested { id, .. } = &frame.event {
            let command = answer(id);
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
    .create("C:/w")
    .expect("session");
    let mut handle = SessionHandle::spawn(session);
    let link = handle.command_link();

    let frames = run_answering(&mut handle, &link, |id| {
        SessionCommand::InteractionResponse {
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
    .create("C:/w")
    .expect("session");
    let mut handle = SessionHandle::spawn(session);
    let link = handle.command_link();

    let frames = run_answering(&mut handle, &link, |id| {
        SessionCommand::InteractionResponse {
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
    .create("C:/w")
    .expect("session");
    let mut handle = SessionHandle::spawn(session);
    let link = handle.command_link();

    let frames = run_answering(&mut handle, &link, |id| {
        SessionCommand::InteractionResponse {
            id: id.to_string(),
            option: Some("Always allow".to_string()),
            text: None,
        }
    })
    .await;

    // One card (the first call); the second passes on session memory.
    assert_eq!(interaction_count(&frames), 1);
    assert_eq!(finished_outputs(&frames), vec!["done"]);
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
    let mut handle = SessionHandle::spawn(session);
    let link = handle.command_link();

    let frames = run_answering(&mut handle, &link, |id| {
        SessionCommand::InteractionResponse {
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
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHandle::spawn(session);
    let mut events = handle.take_events().expect("the event stream");

    handle.message("run it");
    let mut saw_card = false;
    while let Some(frame) = events.recv().await {
        if matches!(frame.event, SessionEvent::InteractionRequested { .. }) {
            saw_card = true;
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
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn abort_with_a_card_open_closes_the_question_totally() {
    let store = temp_store("permit-abort");
    let session = Factory::new(vec![tool_turn("t1", "bash"), text_turn("unreachable")])
        .into_builder(store.clone())
        .dynamic_tool(gated_tool())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHandle::spawn(session);
    let link = handle.command_link();

    handle.message("run it");
    let mut frames = Vec::new();
    let mut stale_id = None;
    while let Some(frame) = handle.next_event().await {
        if let SessionEvent::InteractionRequested { id, .. } = &frame.event {
            stale_id = Some(id.clone());
            // Abort with the card open: the question dies with the run.
            link.send(SessionCommand::Abort);
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
            id,
            option: Some("Allow".to_string()),
            text: None,
        });
    }
    std::fs::remove_dir_all(store.dir()).ok();
}
