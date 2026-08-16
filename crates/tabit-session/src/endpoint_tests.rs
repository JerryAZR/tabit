//! Endpoint tests: the actor over a scripted mock session — command
//! semantics, stream stamps, and the termination contract.

use super::*;
use crate::SessionEvent;
use crate::protocol::SessionCommand;
use crate::tests::{Factory, echo_tool, temp_store, text_turn, tool_turn};
use rig_agent::tool::{DynamicTool, ToolOutput};
use serde_json::json;
use std::time::Duration;

/// A tool that takes real time, so a run is provably in flight while the
/// actor processes commands sent alongside it (the instant mock would
/// let the run finish first).
fn slow_tool() -> DynamicTool {
    DynamicTool::new(
        "slow",
        "Sleeps, then echoes",
        json!({"type":"object","properties":{"value":{"type":"string"}}}),
        |_ctx, args| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                Ok(ToolOutput::text(
                    args.get("value").and_then(|v| v.as_str()).unwrap_or(""),
                ))
            })
        },
    )
}

/// Whether the event ends a pump iteration (every run terminates with
/// exactly one of these).
fn terminal(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::RunFinished { .. }
            | SessionEvent::RunAborted { .. }
            | SessionEvent::RunFailed { .. }
    )
}

/// Collect frames until the actor winds down: closing the command side
/// lets the in-flight pump finish, then the stream ends.
async fn drain(handle: &mut SessionHandle) -> Vec<EventFrame> {
    handle.close_commands();
    let mut frames = Vec::new();
    while let Some(frame) = handle.next_event().await {
        frames.push(frame);
    }
    frames
}

fn user_texts(frames: &[EventFrame]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|frame| match &frame.event {
            SessionEvent::UserMessage { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn finished_outputs(frames: &[EventFrame]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|frame| match &frame.event {
            SessionEvent::RunFinished { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_idle_message_runs_to_completion_over_the_stream() {
    let store = temp_store("endpoint-idle");
    let session = Factory::new(vec![text_turn("hello there")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHandle::spawn(session);

    handle.message("hi");
    let frames = drain(&mut handle).await;

    // Every frame is stamped with the main stream, the run is bracketed
    // by its user message and terminal, and acceptance is observable.
    assert!(frames.iter().all(|f| f.stream.is_main()));
    assert_eq!(user_texts(&frames), vec!["hi"]);
    assert_eq!(finished_outputs(&frames), vec!["hello there"]);
    assert!(matches!(
        frames.last().map(|f| &f.event),
        Some(SessionEvent::RunFinished { .. })
    ));

    // The info block carries the startup facts, and closing stats landed.
    assert_eq!(
        handle.info().model,
        crate::model::ModelSelection::new("p", "m")
    );
    assert!(
        handle.closing_stats().is_some(),
        "stats captured at wind-down"
    );
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn a_message_mid_run_steers_instead_of_starting_a_second_run() {
    let store = temp_store("endpoint-steer");
    let session = Factory::new(vec![tool_turn("t1", "echo"), text_turn("done after steer")])
        .into_builder(store.clone())
        .dynamic_tool(echo_tool())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHandle::spawn(session);

    handle.message("run the tool");
    // A second message while the run is in flight: submitted from the
    // event-stream side (deterministic mid-run timing — the tool call is
    // emitted before its execution), steered into the current run rather
    // than queueing a second one.
    let mut submitted = false;
    let mut frames = Vec::new();
    while let Some(frame) = handle.next_event().await {
        if !submitted && matches!(frame.event, SessionEvent::ToolCall { .. }) {
            submitted = true;
            handle.message("also this");
        }
        if terminal(&frame.event) {
            handle.close_commands();
        }
        frames.push(frame);
    }
    assert!(submitted, "the steer landed mid-run");
    assert_eq!(user_texts(&frames), vec!["run the tool", "also this"]);
    // One run total: the steer extended it rather than queueing another.
    assert_eq!(finished_outputs(&frames), vec!["done after steer"]);
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn two_rapid_messages_both_land_in_order() {
    let store = temp_store("endpoint-rapid");
    let session = Factory::new(vec![text_turn("a"), text_turn("b")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHandle::spawn(session);

    handle.message("first");
    handle.message("second");
    let frames = drain(&mut handle).await;

    // Both accepted, in submit order (the mailbox is FIFO). Whether the
    // second steered the first run or ran as its own depends on when the
    // pump task picked the first message up — either way nothing is lost
    // and the final output is the scripted second turn.
    assert_eq!(user_texts(&frames), vec!["first", "second"]);
    assert_eq!(
        finished_outputs(&frames).last().map(String::as_str),
        Some("b")
    );
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn abort_mid_run_discards_the_queue_and_ends_the_run() {
    let store = temp_store("endpoint-abort");
    let session = Factory::new(vec![tool_turn("t1", "slow"), text_turn("never")])
        .into_builder(store.clone())
        .dynamic_tool(slow_tool())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHandle::spawn(session);

    handle.message("run the tool");
    let mut saw_aborted = false;
    let mut queued = 0;
    let mut sent = false;
    while let Some(frame) = handle.next_event().await {
        match &frame.event {
            SessionEvent::ToolCall { .. } if !sent => {
                // Queued behind the run, then stopped — while the tool is
                // still executing, so the abort provably lands mid-run.
                sent = true;
                handle.message("queued behind");
                handle.abort();
            }
            SessionEvent::RunAborted { .. } => saw_aborted = true,
            SessionEvent::UserMessage { text } if text != "run the tool" => queued += 1,
            _ => {}
        }
        if terminal(&frame.event) {
            handle.close_commands();
        }
    }
    assert!(saw_aborted);
    assert_eq!(queued, 0, "the queued message was discarded");
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn a_failed_run_emits_run_failed_and_ends_the_stream_cleanly() {
    let store = temp_store("endpoint-failure");
    // max_turns(1): the tool turn needs a second model call, so the run
    // fails; the protocol surfaces it as a run_failed event.
    let session = Factory::new(vec![tool_turn("c1", "echo"), text_turn("never")])
        .into_builder(store.clone())
        .max_turns(1)
        .dynamic_tool(echo_tool())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHandle::spawn(session);

    handle.message("will fail");
    let mut failed = 0;
    while let Some(frame) = handle.next_event().await {
        if matches!(frame.event, SessionEvent::RunFailed { .. }) {
            failed += 1;
        }
        if terminal(&frame.event) {
            handle.close_commands();
        }
    }
    assert_eq!(failed, 1, "the failure arrived as an event");
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn a_link_outlives_the_handle_and_still_submits() {
    let store = temp_store("endpoint-link");
    let session = Factory::new(vec![text_turn("ok")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHandle::spawn(session);
    let link = handle.command_link();

    link.send(SessionCommand::Message {
        text: "via link".to_string(),
    });
    let frames = drain(&mut handle).await;
    assert_eq!(user_texts(&frames), vec!["via link"]);
    std::fs::remove_dir_all(store.dir()).ok();
}
