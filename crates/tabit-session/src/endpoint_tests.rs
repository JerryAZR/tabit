//! Endpoint tests: the host over a scripted mock session — command
//! semantics, stream stamps, multi-session routing, and the
//! termination contract.

use super::*;
use crate::SessionEvent;
use crate::tests::{Factory, echo_tool, temp_store, text_turn, tool_turn};
use rig_agent::tool::{DynamicTool, ToolOutput};
use serde_json::json;
use std::time::Duration;
use tabit_protocol::SessionCommand;

/// Wiring whose builders refuse (tests that never drive session
/// lifecycle); the store stays real so the catalog is honest.
fn plain_wiring(store: &SessionStore) -> SessionHostWiring {
    SessionHostWiring {
        store: store.clone(),
        create: std::sync::Arc::new(|| Err("new_session is not driven".to_string())),
        open: std::sync::Arc::new(|_| Err("open_session is not driven".to_string())),
    }
}

/// The boot session id, as the host's consumer learns it.
fn boot_id(handle: &SessionHost) -> String {
    handle.info().session_id.clone()
}

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

/// Collect frames until the host winds down: closing the command side
/// lets every in-flight pump finish, then the stream ends.
async fn drain(handle: &mut SessionHost) -> Vec<EventFrame> {
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
            SessionEvent::UserMessage { text, .. } => Some(text.clone()),
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
async fn startup_degradations_are_the_workers_first_frames() {
    let store = temp_store("endpoint-notes");
    let session = Factory::new(vec![text_turn("hi")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(
        session,
        vec!["default_model `gone` is not usable".to_string()],
        plain_wiring(&store),
    );
    let id = boot_id(&handle);

    handle.message(&id, "go");
    let frames = drain(&mut handle).await;

    // The degradation is the first frame the frontend sees — ahead of
    // the catalog and any run event — as a `model`-kind error
    // (external errors ride the channel; stderr is not a frontend
    // concern).
    match frames.first().map(|f| &f.event) {
        Some(SessionEvent::Error { kind, message, .. }) => {
            assert_eq!(kind, tabit_protocol::ErrorKind::MODEL);
            assert!(message.contains("default_model"), "{message}");
        }
        other => panic!("the degradation must lead the stream, got {other:?}"),
    }
    // The catalog follows the notes, ahead of any run event.
    assert!(matches!(
        frames.get(1).map(|f| &f.event),
        Some(SessionEvent::SessionsAvailable { .. })
    ));
    assert!(
        frames
            .iter()
            .filter(|f| matches!(f.event, SessionEvent::Error { .. }))
            .count()
            == 1,
        "exactly one error frame for one note"
    );
    assert_eq!(finished_outputs(&frames), vec!["hi"]);
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn an_idle_message_runs_to_completion_over_the_stream() {
    let store = temp_store("endpoint-idle");
    let session = Factory::new(vec![text_turn("hello there")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);

    handle.message(&id, "hi");
    let frames = drain(&mut handle).await;

    // Every frame is stamped with the session's own stream (its id),
    // the run is bracketed by its user message and terminal, and
    // acceptance is observable.
    assert!(frames.iter().all(|f| f.stream.as_str() == id));
    assert_eq!(user_texts(&frames), vec!["hi"]);
    assert_eq!(finished_outputs(&frames), vec!["hello there"]);
    assert!(matches!(
        frames.last().map(|f| &f.event),
        Some(SessionEvent::RunFinished { .. })
    ));

    // The info block carries the startup facts, and closing stats landed.
    assert_eq!(
        handle.info().model,
        tabit_protocol::ModelSelection::new("p", "m")
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
    let session = Factory::new(vec![tool_turn("t1", "slow"), text_turn("done after steer")])
        .into_builder(store.clone())
        .dynamic_tool(slow_tool())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);

    handle.message(&id, "run the tool");
    // A second message while the run is in flight: submitted from the
    // event-stream side while the slow tool is mid-execution (a fast tool
    // never yields, so the run would finish before the submit lands and
    // the message would start a second run instead), steered into the
    // current run rather than queueing a second one.
    let mut submitted = false;
    let mut frames = Vec::new();
    while let Some(frame) = handle.next_event().await {
        if !submitted && matches!(frame.event, SessionEvent::ToolCall { .. }) {
            submitted = true;
            handle.message(&id, "also this");
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

    // The waiting steer was acknowledged at submit (`message_queued`),
    // and only that message — the initial idle send never queues (it
    // drains immediately; `user_message` is its acknowledgment).
    let queued: Vec<(String, String)> = frames
        .iter()
        .filter_map(|frame| match &frame.event {
            SessionEvent::MessageQueued { id, text } => Some((id.clone(), text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        queued.len(),
        1,
        "exactly one queued acknowledgment, for the mid-run submit"
    );
    assert_eq!(queued[0].1, "also this");
    // One closed ledger: a user_message carrying that same id resolves it.
    let steer_id = &queued[0].0;
    assert!(
        frames.iter().any(|frame| matches!(&frame.event,
            SessionEvent::UserMessage { text, entry_id }
                if text == "also this" && entry_id == steer_id)),
        "the steer's user_message resolves its queued id"
    );
    // And the id is real: the log's steer entry keeps it verbatim.
    let loaded = store
        .open_path(handle.info().session_path.as_ref())
        .expect("reload");
    assert!(
        loaded.entries.iter().any(|entry| matches!(&entry.kind,
            crate::EntryKind::UserMessage { message }
                if entry.id == *steer_id && crate::session::user_text(message) == "also this")),
        "the steer's born-early id is its log entry's id"
    );
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn two_rapid_messages_both_land_in_order() {
    let store = temp_store("endpoint-rapid");
    let session = Factory::new(vec![text_turn("a"), text_turn("b")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);

    handle.message(&id, "first");
    handle.message(&id, "second");
    let frames = drain(&mut handle).await;

    // Rapid messages deterministically batch: pushes are synchronous
    // and the worker only wakes once the caller yields, so both land in
    // one drain — one run, both messages as its opening input, one
    // completion. (On a multi-thread runtime a push racing the worker's
    // drain may steer instead; the guarantee is no-loss and order, and
    // single-threaded schedulers — tests, scripts — get exact batching.)
    assert_eq!(user_texts(&frames), vec!["first", "second"]);
    assert_eq!(finished_outputs(&frames), vec!["a"]);
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn abort_while_idle_discards_queued_messages() {
    let store = temp_store("endpoint-abort-idle");
    let session = Factory::new(vec![text_turn("never")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);

    // Queued while idle (the worker cannot have started: no await yet),
    // then stopped before any run: the queue goes with it.
    handle.message(&id, "queued one");
    handle.message(&id, "queued two");
    handle.abort(&id);
    let frames = drain(&mut handle).await;
    // No run ever happened — and nothing user-authored leaves
    // silently (flag 6): both queued pairs come back as one
    // `messages_discarded`, after the catalog.
    let types: Vec<&str> = frames
        .iter()
        .map(|f| match &f.event {
            SessionEvent::SessionsAvailable { .. } => "sessions_available",
            SessionEvent::MessagesDiscarded { .. } => "messages_discarded",
            _ => "other",
        })
        .collect();
    assert_eq!(types, vec!["sessions_available", "messages_discarded"]);
    let texts: Vec<String> = match &frames[1].event {
        SessionEvent::MessagesDiscarded { messages } => {
            messages.iter().map(|m| m.text.clone()).collect()
        }
        _ => panic!("expected the discard: {frames:?}"),
    };
    assert_eq!(texts, vec!["queued one", "queued two"]);
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
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);

    handle.message(&id, "run the tool");
    let mut saw_aborted = false;
    let mut queued = 0;
    let mut sent = false;
    let mut frames = Vec::new();
    while let Some(frame) = handle.next_event().await {
        match &frame.event {
            SessionEvent::ToolCall { .. } if !sent => {
                // Queued behind the run, then stopped — while the tool is
                // still executing, so the abort provably lands mid-run.
                // Both through a link, covering the link's command
                // dispatch.
                sent = true;
                let link = handle.command_link();
                link.send(SessionCommand::Message {
                    session: id.clone(),
                    text: "queued behind".to_string(),
                });
                link.send(SessionCommand::Abort {
                    session: id.clone(),
                });
            }
            SessionEvent::RunAborted { .. } => saw_aborted = true,
            SessionEvent::UserMessage { text, .. } if text != "run the tool" => queued += 1,
            _ => {}
        }
        if terminal(&frame.event) {
            handle.close_commands();
        }
        frames.push(frame);
    }
    assert!(saw_aborted);
    assert_eq!(queued, 0, "the queued message was discarded");
    // The discard came back as an event: the staged pair, after the
    // terminal (the notice rides the wind-down), id matching the
    // `message_queued` acknowledgment the submit earned mid-run.
    let aborted_at = frames
        .iter()
        .position(|frame| matches!(frame.event, SessionEvent::RunAborted { .. }))
        .expect("an abort terminal");
    let discards: Vec<(usize, Vec<(String, String)>)> = frames
        .iter()
        .enumerate()
        .filter_map(|(index, frame)| match &frame.event {
            SessionEvent::MessagesDiscarded { messages } => Some((
                index,
                messages
                    .iter()
                    .map(|m| (m.id.clone(), m.text.clone()))
                    .collect(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        discards.len(),
        1,
        "one discard event carrying the cleared queue"
    );
    let (discard_at, pairs) = &discards[0];
    assert!(
        *discard_at > aborted_at,
        "the discard notice follows the terminal"
    );
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].1, "queued behind");
    let acknowledged = frames
        .iter()
        .filter_map(|frame| match &frame.event {
            SessionEvent::MessageQueued { id, text } => Some((id.clone(), text.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        acknowledged,
        vec![(pairs[0].0.clone(), "queued behind".to_string())],
        "the discarded pair's id is the one message_queued announced"
    );
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
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);

    handle.message(&id, "will fail");
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
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);
    let link = handle.command_link();

    link.send(SessionCommand::Message {
        session: id,
        text: "via link".to_string(),
    });
    let frames = drain(&mut handle).await;
    assert_eq!(user_texts(&frames), vec!["via link"]);
    std::fs::remove_dir_all(store.dir()).ok();
}

#[path = "interaction_tests.rs"]
mod interaction_tests;

/// Read frames until one matches `want`; returns it (startup
/// announcements and unrelated streams pass through unread).
async fn until_event(handle: &mut SessionHost, want: fn(&SessionEvent) -> bool) -> EventFrame {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = handle
                .next_event()
                .await
                .expect("the host keeps producing events");
            if want(&frame.event) {
                return frame;
            }
        }
    })
    .await
    .expect("the awaited event arrives before timeout")
}

#[tokio::test]
async fn new_session_runs_a_second_stream_and_both_route_by_id() {
    let store = temp_store("endpoint-multi");
    let session = Factory::new(vec![text_turn("boot answer")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let create_store = store.clone();
    let wiring = SessionHostWiring {
        store: store.clone(),
        create: std::sync::Arc::new(move || {
            Factory::new(vec![text_turn("new answer")])
                .into_builder(create_store.clone())
                .create("C:/w")
                .map(|session| (session, Vec::new()))
                .map_err(|error| error.to_string())
        }),
        open: std::sync::Arc::new(|_| Err("not driven".to_string())),
    };
    let mut handle = SessionHost::spawn(session, Vec::new(), wiring);
    let boot = boot_id(&handle);
    let link = handle.command_link();

    link.send(SessionCommand::NewSession);
    let created = match until_event(&mut handle, |event| {
        matches!(event, SessionEvent::SessionCreated { .. })
    })
    .await
    .event
    {
        SessionEvent::SessionCreated { id, model, .. } => {
            assert_eq!(
                model,
                tabit_protocol::ModelSelection::new("p", "m"),
                "the frame carries the new session's selection"
            );
            id
        }
        other => panic!("expected session_created, got {other:?}"),
    };
    assert_ne!(created, boot, "a second session means a second stream");

    // The new session answers on its own stamp; the boot session
    // answers on its own — one channel, attribution by stamp.
    link.send(SessionCommand::Message {
        session: created.clone(),
        text: "to the new one".to_string(),
    });
    let answered = until_event(&mut handle, |event| {
        matches!(event, SessionEvent::RunFinished { .. })
    })
    .await;
    assert_eq!(answered.stream.as_str(), created);
    assert!(matches!(
        &answered.event,
        SessionEvent::RunFinished { output, .. } if output == "new answer"
    ));

    link.send(SessionCommand::Message {
        session: boot.clone(),
        text: "to the boot one".to_string(),
    });
    let answered = until_event(&mut handle, |event| {
        matches!(event, SessionEvent::RunFinished { .. })
    })
    .await;
    assert_eq!(answered.stream.as_str(), boot);
    assert!(matches!(
        &answered.event,
        SessionEvent::RunFinished { output, .. } if output == "boot answer"
    ));

    // The routing proof is above (each answer on its own stamp); the
    // wind-down just has to end cleanly with both workers joined.
    drain(&mut handle).await;
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn open_session_loads_a_stored_session_and_replays_it() {
    let store = temp_store("endpoint-open");
    // A stored session with one round of history, written by a direct
    // prompt (the boot session will be a different, fresh one).
    let stored_id = {
        let mut session = Factory::new(vec![text_turn("stored answer")])
            .into_builder(store.clone())
            .create("C:/w")
            .expect("session");
        session.prompt("hello").await;
        session.id().to_string()
    };
    let boot_session = Factory::new(vec![text_turn("boot answer")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let open_store = store.clone();
    let wiring = SessionHostWiring {
        store: store.clone(),
        create: std::sync::Arc::new(|| Err("not driven".to_string())),
        open: std::sync::Arc::new(move |id: &str| {
            let path = open_store
                .list()
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|summary| summary.id == id)
                .ok_or_else(|| format!("no stored session with id `{id}`"))?
                .path;
            Factory::new(vec![text_turn("reopened answer")])
                .into_builder(open_store.clone())
                .resume(&path)
                .map(|(session, _)| (session, Vec::new()))
                .map_err(|error| error.to_string())
        }),
    };
    let mut handle = SessionHost::spawn(boot_session, Vec::new(), wiring);
    let boot = boot_id(&handle);

    // Lazy loading: the startup catalog lists the stored session (the
    // fresh boot has not materialized on disk).
    let catalog = match until_event(&mut handle, |event| {
        matches!(event, SessionEvent::SessionsAvailable { .. })
    })
    .await
    .event
    {
        SessionEvent::SessionsAvailable { sessions } => sessions,
        other => panic!("expected sessions_available, got {other:?}"),
    };
    assert!(
        catalog.iter().any(|session| session.id == stored_id),
        "the stored session is in the catalog: {catalog:?}"
    );
    assert!(
        !catalog.iter().any(|session| session.id == boot),
        "the unmaterialized boot session is not"
    );

    // open_session loads it and answers with the pass — stamped with
    // the opened id, carrying the stored history whole.
    handle.command_link().send(SessionCommand::OpenSession {
        id: stored_id.clone(),
    });
    let mut pass = Vec::new();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), handle.next_event())
            .await
            .expect("the pass keeps producing events")
            .expect("the stream stays open through the pass");
        assert_eq!(
            frame.stream.as_str(),
            stored_id,
            "the pass is stamped with the opened id"
        );
        match frame.event {
            SessionEvent::ReplayStarted { .. } => {}
            SessionEvent::ReplayDone => break,
            SessionEvent::Error { message, .. } => panic!("open failed: {message}"),
            event => pass.push(kind_of(&event)),
        }
    }
    assert_eq!(
        pass,
        vec![
            "model_changed",
            "user_message",
            "turn_started",
            "text_delta",
            "completion_call",
            "turn_committed",
        ],
        "the stored chain replays whole"
    );

    // The opened session is a live worker now: a message runs on it.
    handle.message(&stored_id, "again");
    let answered = until_event(&mut handle, |event| {
        matches!(event, SessionEvent::RunFinished { .. })
    })
    .await;
    assert_eq!(answered.stream.as_str(), stored_id);
    assert!(matches!(
        &answered.event,
        SessionEvent::RunFinished { output, .. } if output == "reopened answer"
    ));

    // Idempotent: a second open re-replays the (now longer) chain.
    handle.command_link().send(SessionCommand::OpenSession {
        id: stored_id.clone(),
    });
    let mut pass_two = 0;
    loop {
        let frame = until_event(&mut handle, |event| {
            matches!(
                event,
                SessionEvent::ReplayStarted { .. }
                    | SessionEvent::ReplayDone
                    | SessionEvent::Error { .. }
            )
        })
        .await;
        match frame.event {
            SessionEvent::ReplayStarted { .. } => pass_two += 1,
            SessionEvent::ReplayDone => break,
            SessionEvent::Error { message, .. } => panic!("re-open failed: {message}"),
            _ => {}
        }
    }
    assert_eq!(pass_two, 1, "one more pass, stamped with the opened id");

    drain(&mut handle).await;
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn an_unknown_session_target_is_a_session_error_on_that_stream() {
    let store = temp_store("endpoint-unknown");
    let session = Factory::new(vec![text_turn("ok")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));

    handle.command_link().send(SessionCommand::Message {
        session: "no-such".to_string(),
        text: "lost?".to_string(),
    });
    let error = until_event(
        &mut handle,
        |event| matches!(event, SessionEvent::Error { kind, .. } if kind == "session"),
    )
    .await;
    assert_eq!(
        error.stream.as_str(),
        "no-such",
        "stamped with the targeted id"
    );
    drain(&mut handle).await;
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn a_replay_request_streams_the_pass_onto_the_event_channel() {
    // A session with history (run once, then resumed): the pass carries
    // it, bracketed and counted, and the stream continues normally
    // afterwards.
    let store = temp_store("endpoint-replay");
    let path = {
        let mut session = Factory::new(vec![text_turn("first answer")])
            .into_builder(store.clone())
            .create("C:/w")
            .expect("session");
        session.prompt("hello").await;
        session.path().to_path_buf()
    };
    let session = Factory::new(vec![text_turn("second answer")])
        .into_builder(store.clone())
        .resume(&path)
        .expect("resume")
        .0;
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);

    handle.replay(&id);
    let mut pass = Vec::new();
    let mut done = false;
    while !done {
        let frame = handle
            .next_event()
            .await
            .expect("the worker answers a replay request");
        match frame.event {
            // The startup catalog precedes the pass; session-level
            // announcements are not pass content.
            SessionEvent::SessionsAvailable { .. } | SessionEvent::SessionCreated { .. } => {}
            SessionEvent::ReplayStarted { .. } => pass.push("started".to_string()),
            SessionEvent::ReplayDone => {
                pass.push("done".to_string());
                done = true;
            }
            event => pass.push(kind_of(&event)),
        }
    }
    // model change, user message, a bracketed turn with whole text.
    assert_eq!(
        pass,
        vec![
            "started",
            "model_changed",
            "user_message",
            "turn_started",
            "text_delta",
            "completion_call",
            "turn_committed",
            "done",
        ]
    );

    // The stream continues: a message after the pass runs normally.
    handle.message(&id, "again");
    let frames = drain(&mut handle).await;
    assert_eq!(user_texts(&frames), vec!["again"]);
    assert_eq!(finished_outputs(&frames), vec!["second answer"]);
    std::fs::remove_dir_all(store.dir()).ok();
}

fn kind_of(event: &SessionEvent) -> String {
    match event {
        SessionEvent::ModelChanged { .. } => "model_changed".to_string(),
        SessionEvent::UserMessage { .. } => "user_message".to_string(),
        SessionEvent::TurnStarted { .. } => "turn_started".to_string(),
        SessionEvent::TextDelta { .. } => "text_delta".to_string(),
        SessionEvent::CompletionCall { .. } => "completion_call".to_string(),
        SessionEvent::TurnCommitted { .. } => "turn_committed".to_string(),
        other => format!("other:{other:?}"),
    }
}

#[tokio::test]
async fn a_catalog_failure_is_the_carrier_in_place_of_the_announcement() {
    // The store's directory unreadable (here: occupied by a file) —
    // external error, graceful and clear: a `session`-kind error
    // stamped boot, and no announcement behind it.
    let dir = std::env::temp_dir().join(format!("tabit-endpoint-notadir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::write(&dir, b"not a directory").expect("plant the blocker");
    let session = Factory::new(vec![text_turn("hi")])
        .into_builder(temp_store("endpoint-catalog-fail"))
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(
        session,
        Vec::new(),
        SessionHostWiring {
            store: SessionStore::new(&dir),
            create: std::sync::Arc::new(|| Err("not driven".to_string())),
            open: std::sync::Arc::new(|_| Err("not driven".to_string())),
        },
    );

    let catalog_error = until_event(
        &mut handle,
        |event| matches!(event, SessionEvent::Error { kind, .. } if kind == "session"),
    )
    .await;
    assert_eq!(
        catalog_error.stream.as_str(),
        handle.info().session_id,
        "untargeted outcomes carry the boot stream"
    );
    // Nothing else was announced (the frames before the error are the
    // empty-notes case: the error is first).
    let frames = drain(&mut handle).await;
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame.event, SessionEvent::SessionsAvailable { .. }))
    );
    std::fs::remove_file(&dir).ok();
}

#[tokio::test]
async fn lifecycle_failures_and_notes_ride_the_carrier() {
    let store = temp_store("endpoint-lifecycle-errors");
    let session = Factory::new(vec![text_turn("hi")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(
        session,
        Vec::new(),
        SessionHostWiring {
            store: store.clone(),
            // The builder degrades: the failure is boot-stamped, the
            // notes are new-session-stamped.
            create: std::sync::Arc::new(|| {
                Err("any model to run with (providers.toml defines no models)".to_string())
            }),
            open: std::sync::Arc::new(|id: &str| Err(format!("no stored session with id `{id}`"))),
        },
    );
    let boot = boot_id(&handle);
    let link = handle.command_link();

    // new_session cannot build: an error on the boot stream (the
    // command had no target id).
    link.send(SessionCommand::NewSession);
    let error = until_event(
        &mut handle,
        |event| matches!(event, SessionEvent::Error { kind, .. } if kind == "session"),
    )
    .await;
    assert_eq!(
        error.stream.as_str(),
        boot,
        "untargeted failure stamps boot"
    );

    // open_session names a session that does not exist: an error on
    // the targeted stream.
    link.send(SessionCommand::OpenSession {
        id: "no-such-session".to_string(),
    });
    let error = until_event(
        &mut handle,
        |event| matches!(event, SessionEvent::Error { kind, .. } if kind == "session"),
    )
    .await;
    assert_eq!(error.stream.as_str(), "no-such-session");
    drain(&mut handle).await;
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn a_created_sessions_selection_notes_follow_its_stream() {
    let store = temp_store("endpoint-create-notes");
    let session = Factory::new(vec![text_turn("hi")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let create_store = store.clone();
    let mut handle = SessionHost::spawn(
        session,
        Vec::new(),
        SessionHostWiring {
            store: store.clone(),
            create: std::sync::Arc::new(move || {
                Factory::new(vec![text_turn("new answer")])
                    .into_builder(create_store.clone())
                    .create("C:/w")
                    .map(|session| {
                        (
                            session,
                            vec!["default_model `gone` is not usable".to_string()],
                        )
                    })
                    .map_err(|error| error.to_string())
            }),
            open: std::sync::Arc::new(|_| Err("not driven".to_string())),
        },
    );
    handle.command_link().send(SessionCommand::NewSession);

    // session_created first, its degradation right behind it, both on
    // the new session's stream.
    let created = until_event(&mut handle, |event| {
        matches!(event, SessionEvent::SessionCreated { .. })
    })
    .await;
    let created_id = match created.event {
        SessionEvent::SessionCreated { id, .. } => id,
        other => panic!("expected session_created, got {other:?}"),
    };
    let note = until_event(
        &mut handle,
        |event| matches!(event, SessionEvent::Error { kind, .. } if kind == "model"),
    )
    .await;
    assert_eq!(note.stream.as_str(), created_id);
    drain(&mut handle).await;
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn new_session_is_never_blocked_by_a_running_session() {
    // The ruling: session lifecycle writes no session's file, so no
    // pause point is ever needed — creation (and loading) are
    // host-level, independent of every worker. Pinned at the extreme:
    // the boot session's run is provably mid-tool when the new session
    // is created, messaged, and finished.
    let store = temp_store("endpoint-create-midrun");
    let session = Factory::new(vec![tool_turn("t1", "slow"), text_turn("never")])
        .into_builder(store.clone())
        .dynamic_tool(slow_tool())
        .create("C:/w")
        .expect("session");
    let create_store = store.clone();
    let wiring = SessionHostWiring {
        store: store.clone(),
        create: std::sync::Arc::new(move || {
            Factory::new(vec![text_turn("new answer")])
                .into_builder(create_store.clone())
                .create("C:/w")
                .map(|session| (session, Vec::new()))
                .map_err(|error| error.to_string())
        }),
        open: std::sync::Arc::new(|_| Err("not driven".to_string())),
    };
    let mut handle = SessionHost::spawn(session, Vec::new(), wiring);
    let boot = boot_id(&handle);
    let link = handle.command_link();

    // The boot run is in flight (the slow tool is executing)…
    link.send(SessionCommand::Message {
        session: boot.clone(),
        text: "run the slow tool".to_string(),
    });
    let _tool_call = until_event(&mut handle, |event| {
        matches!(event, SessionEvent::ToolCall { .. })
    })
    .await;

    // …and the new session is created, messaged, and FINISHES while
    // the boot run is still inside the tool (it sleeps 300ms; the new
    // session's turn is an instant scripted text — the arrival order
    // below is the proof of non-blocking).
    link.send(SessionCommand::NewSession);
    let created = match until_event(&mut handle, |event| {
        matches!(event, SessionEvent::SessionCreated { .. })
    })
    .await
    .event
    {
        SessionEvent::SessionCreated { id, .. } => id,
        other => panic!("expected session_created, got {other:?}"),
    };
    link.send(SessionCommand::Message {
        session: created.clone(),
        text: "meanwhile".to_string(),
    });
    let finished = until_event(&mut handle, |event| {
        matches!(event, SessionEvent::RunFinished { .. })
    })
    .await;
    assert_eq!(
        finished.stream.as_str(),
        created,
        "the new session's terminal arrives before the boot run's"
    );

    // The boot run eventually finishes too — both completed on their
    // own streams.
    let boot_done = until_event(&mut handle, |event| {
        matches!(
            event,
            SessionEvent::RunFinished { .. } | SessionEvent::RunAborted { .. }
        )
    })
    .await;
    assert_eq!(boot_done.stream.as_str(), boot);
    drain(&mut handle).await;
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn a_post_abort_message_survives_and_runs() {
    // Flag 6 restored to the ruling: one clear, at the abort site.
    // What was queued at abort time is discarded (visibly); anything
    // arriving after the abort queues normally and starts the next
    // run — the epilogue no longer sweeps the window behind it.
    let store = temp_store("endpoint-post-abort");
    let session = Factory::new(vec![tool_turn("t1", "slow"), text_turn("after the abort")])
        .into_builder(store.clone())
        .dynamic_tool(slow_tool())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);
    let link = handle.command_link();

    link.send(SessionCommand::Message {
        session: id.clone(),
        text: "run the slow tool".to_string(),
    });
    let _tool_call = until_event(&mut handle, |event| {
        matches!(event, SessionEvent::ToolCall { .. })
    })
    .await;
    // Queued behind the run, then aborted — the pair is discarded at
    // the site, the notice flushes after the terminal.
    link.send(SessionCommand::Message {
        session: id.clone(),
        text: "queued behind".to_string(),
    });
    link.send(SessionCommand::Abort {
        session: id.clone(),
    });
    let aborted = until_event(&mut handle, |event| {
        matches!(event, SessionEvent::RunAborted { .. })
    })
    .await;
    assert_eq!(aborted.stream.as_str(), id);
    let discarded = until_event(&mut handle, |event| {
        matches!(event, SessionEvent::MessagesDiscarded { .. })
    })
    .await;
    assert_eq!(discarded.stream.as_str(), id);

    // The post-abort message (sent after the terminal): queues
    // normally, starts the next run, runs to completion.
    link.send(SessionCommand::Message {
        session: id.clone(),
        text: "after the abort".to_string(),
    });
    let finished = until_event(&mut handle, |event| {
        matches!(event, SessionEvent::RunFinished { .. })
    })
    .await;
    assert_eq!(finished.stream.as_str(), id);
    assert!(matches!(
        &finished.event,
        SessionEvent::RunFinished { output, .. } if output == "after the abort"
    ));
    drain(&mut handle).await;
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn frontend_death_aborts_every_sessions_run() {
    // The v3 termination contract at multi-session scale: the watcher
    // sweeps every worker. Both sessions' runs are provably mid-tool
    // when the receiver drops; the wind-down (observable as the boot's
    // closing stats — the host awaits every join before the stream
    // ends) must have aborted both, durably (an `Aborted` marker and
    // the synthesized tail in each log).
    let store = temp_store("endpoint-death-multi");
    let session = Factory::new(vec![tool_turn("t1", "slow"), text_turn("never")])
        .into_builder(store.clone())
        .dynamic_tool(slow_tool())
        .create("C:/w")
        .expect("session");
    let create_store = store.clone();
    let wiring = SessionHostWiring {
        store: store.clone(),
        create: std::sync::Arc::new(move || {
            Factory::new(vec![tool_turn("t2", "slow"), text_turn("never")])
                .into_builder(create_store.clone())
                .dynamic_tool(slow_tool())
                .create("C:/w")
                .map(|session| (session, Vec::new()))
                .map_err(|error| error.to_string())
        }),
        open: std::sync::Arc::new(|_| Err("not driven".to_string())),
    };
    let mut handle = SessionHost::spawn(session, Vec::new(), wiring);
    let boot = boot_id(&handle);
    let link = handle.command_link();

    link.send(SessionCommand::Message {
        session: boot.clone(),
        text: "run one".to_string(),
    });
    link.send(SessionCommand::NewSession);
    let created = match until_event(&mut handle, |event| {
        matches!(event, SessionEvent::SessionCreated { .. })
    })
    .await
    .event
    {
        SessionEvent::SessionCreated { id, .. } => id,
        other => panic!("expected session_created, got {other:?}"),
    };
    link.send(SessionCommand::Message {
        session: created.clone(),
        text: "run two".to_string(),
    });
    // Both runs provably in flight: one tool call per stream.
    let mut tool_streams = Vec::new();
    while tool_streams.len() < 2 {
        let frame = until_event(&mut handle, |event| {
            matches!(event, SessionEvent::ToolCall { .. })
        })
        .await;
        if !tool_streams.contains(&frame.stream.as_str().to_string()) {
            tool_streams.push(frame.stream.as_str().to_string());
        }
    }
    assert_eq!(
        tool_streams,
        vec![boot.clone(), created.clone()],
        "each session's run is mid-tool"
    );

    // Frontend death: the receiver drops. The wind-down ends the
    // stream only after every worker joins — the boot's closing stats
    // appearing proves the host finished awaiting them all.
    let events = handle.take_events().expect("the event stream");
    drop(events);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if handle.closing_stats().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the host must wind down when the frontend dies");

    // Durable proof both runs aborted at the watcher's hand: each log
    // records the abort marker.
    let boot_path = handle.info().session_path.clone();
    let loaded = store
        .open_path(std::path::Path::new(&boot_path))
        .unwrap_or_else(|error| panic!("the boot session's log reopens: {error}"));
    assert!(
        loaded
            .entries
            .iter()
            .any(|e| matches!(e.kind, crate::EntryKind::Aborted)),
        "the boot session recorded the abort"
    );
    // The created session's file materialized (its run started) and
    // aborted the same way.
    let created_path = store
        .list()
        .expect("list")
        .into_iter()
        .find(|summary| summary.id == created)
        .expect("the created session materialized")
        .path;
    let loaded = store.open_path(&created_path).expect("created log reopens");
    assert!(
        loaded
            .entries
            .iter()
            .any(|e| matches!(e.kind, crate::EntryKind::Aborted)),
        "the created session recorded the abort too"
    );
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn a_replay_request_for_a_running_session_answers_after_its_terminal() {
    // "The one wait in the design" (PROTOCOL.md v3): the pass for a
    // session whose own run is in flight waits for that run's
    // terminal — pinned as an ordering contract, not an accident: no
    // bracket interleaves the run's events.
    let store = temp_store("endpoint-replay-midrun");
    let session = Factory::new(vec![tool_turn("t1", "slow"), text_turn("done")])
        .into_builder(store.clone())
        .dynamic_tool(slow_tool())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);

    handle.message(&id, "run the slow tool");
    let _tool_call = until_event(&mut handle, |event| {
        matches!(event, SessionEvent::ToolCall { .. })
    })
    .await;

    // The pass is requested mid-run. The run continues to its terminal
    // with nothing bracketed inside it…
    handle.replay(&id);
    let mut saw_bracket_before_terminal = false;
    let finished = loop {
        let frame = handle.next_event().await.expect("the run continues");
        match frame.event {
            SessionEvent::ReplayStarted { .. } | SessionEvent::ReplayDone => {
                saw_bracket_before_terminal = true;
            }
            SessionEvent::RunFinished { .. } => break frame,
            _ => {}
        }
    };
    assert_eq!(finished.stream.as_str(), id);
    assert!(
        !saw_bracket_before_terminal,
        "the pass never interleaves the in-flight run"
    );

    // …and answers right after it, stamped with the session's id.
    let started = until_event(&mut handle, |event| {
        matches!(event, SessionEvent::ReplayStarted { .. })
    })
    .await;
    assert_eq!(started.stream.as_str(), id);
    loop {
        let frame = until_event(&mut handle, |event| {
            matches!(event, SessionEvent::ReplayDone | SessionEvent::Error { .. })
        })
        .await;
        match frame.event {
            SessionEvent::ReplayDone => break,
            SessionEvent::Error { message, .. } => panic!("replay failed: {message}"),
            _ => {}
        }
    }
    drain(&mut handle).await;
    std::fs::remove_dir_all(store.dir()).ok();
}

// ---------------------------------------------------------------------------
// Checkout (PROTOCOL.md v3 stage 2): pause-point semantics, the
// watermark discard rule, and the full-re-render pass.

/// The entry id of a user message, by text, from frames collected so
/// far (live frames — the id a frontend would learn from the event).
fn entry_id_of(frames: &[EventFrame], text: &str) -> String {
    frames
        .iter()
        .find_map(|frame| match &frame.event {
            SessionEvent::UserMessage {
                text: seen,
                entry_id,
            } if seen == text => Some(entry_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no user_message for {text:?} among {} frames", frames.len()))
}

/// Read frames into `frames` until one matches `stop` (it is included).
async fn collect_until(
    handle: &mut SessionHost,
    frames: &mut Vec<EventFrame>,
    stop: fn(&SessionEvent) -> bool,
) {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), handle.next_event())
            .await
            .expect("timed out waiting for the awaited event")
            .expect("the stream ended before the awaited event");
        let hit = stop(&frame.event);
        frames.push(frame);
        if hit {
            return;
        }
    }
}

/// The user texts of one checkout's pass: from its `checked_out` to
/// the last `replay_done` (no other replay is requested here).
fn bracket_users(frames: &[EventFrame], checked_at: usize) -> Vec<String> {
    let done_at = frames[checked_at..]
        .iter()
        .position(|frame| matches!(frame.event, SessionEvent::ReplayDone))
        .expect("the pass closes");
    user_texts(&frames[checked_at..checked_at + done_at + 1])
}

/// The user texts of the session file's active chain — log truth.
fn chain_users(handle: &SessionHost, store: &SessionStore) -> Vec<String> {
    let loaded = store
        .open_path(std::path::PathBuf::from(&handle.info().session_path).as_path())
        .expect("reload");
    loaded
        .chain
        .iter()
        .filter_map(|entry| match &entry.kind {
            crate::EntryKind::UserMessage { message } => Some(crate::session::user_text(message)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn checkout_rewinds_replays_and_branches_the_next_prompt() {
    let store = temp_store("endpoint-checkout-idle");
    let session = Factory::new(vec![
        text_turn("first answer"),
        text_turn("second answer"),
        text_turn("branch answer"),
    ])
    .into_builder(store.clone())
    .create("C:/w")
    .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);

    // Two exchanges, each to its terminal; then an idle checkout at
    // the FIRST user entry.
    let mut frames = Vec::new();
    handle.message(&id, "one");
    collect_until(&mut handle, &mut frames, terminal).await;
    handle.message(&id, "two");
    collect_until(&mut handle, &mut frames, terminal).await;
    let first_entry = entry_id_of(&frames, "one");
    handle.checkout(&id, &first_entry);
    collect_until(&mut handle, &mut frames, |e| {
        matches!(e, SessionEvent::ReplayDone)
    })
    .await;

    // The success sequence: checked_out (the target, base_id null —
    // full re-render) then a pass bracketing the rewound chain. The
    // second exchange is off the chain now.
    let checked_at = frames
        .iter()
        .position(|frame| {
            matches!(&frame.event, SessionEvent::CheckedOut { entry_id, base_id }
            if entry_id == &first_entry && base_id.is_none())
        })
        .expect("checked_out with the target and a null base_id");
    assert_eq!(bracket_users(&frames, checked_at), vec!["one"]);

    // The next prompt branches from the target: it runs, and the log's
    // chain holds exactly the rewound prefix plus the new branch.
    handle.message(&id, "branch");
    frames.extend(drain(&mut handle).await);
    assert_eq!(
        finished_outputs(&frames),
        vec!["first answer", "second answer", "branch answer"]
    );
    assert_eq!(chain_users(&handle, &store), vec!["one", "branch"]);
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn a_checkout_during_a_run_parks_until_the_terminal() {
    let store = temp_store("endpoint-checkout-park");
    let session = Factory::new(vec![
        tool_turn("t1", "slow"),
        text_turn("done"),
        text_turn("branch answer"),
    ])
    .into_builder(store.clone())
    .dynamic_tool(slow_tool())
    .create("C:/w")
    .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);

    handle.message(&id, "go");
    let mut frames = Vec::new();
    let mut sent = false;
    let mut saw_checked_out = false;
    let mut go_entry = None;
    while let Some(frame) = handle.next_event().await {
        match &frame.event {
            SessionEvent::UserMessage { text, entry_id } if text == "go" => {
                go_entry = Some(entry_id.clone());
            }
            SessionEvent::ToolCall { .. } if !sent => {
                // Provably mid-run (the slow tool is executing): a
                // steer, then the checkout — both under the run.
                sent = true;
                handle.message(&id, "also this");
                handle.checkout(&id, go_entry.clone().expect("go's entry id"));
            }
            SessionEvent::CheckedOut { .. } => saw_checked_out = true,
            _ => {}
        }
        let done = matches!(frame.event, SessionEvent::ReplayDone);
        frames.push(frame);
        if saw_checked_out && done {
            break;
        }
    }

    // The run ran to its own terminal — the checkout never aborts —
    // and the parked checkout executed at that pause point.
    let terminal_at = frames
        .iter()
        .position(|frame| terminal(&frame.event))
        .expect("the run's terminal");
    assert!(matches!(
        &frames[terminal_at].event,
        SessionEvent::RunFinished { .. }
    ));
    let checked_at = frames
        .iter()
        .position(|frame| matches!(frame.event, SessionEvent::CheckedOut { .. }))
        .expect("the parked checkout executed");
    assert!(
        terminal_at < checked_at,
        "the checkout waits for the terminal"
    );
    // The steer drained into the run (its user_message precedes the
    // terminal) — the ledger closed there; the rewind then takes it
    // off the chain with the run's answer. Nothing was discarded.
    assert_eq!(
        user_texts(&frames[..terminal_at + 1]),
        vec!["go", "also this"]
    );
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame.event, SessionEvent::MessagesDiscarded { .. }))
    );
    assert_eq!(bracket_users(&frames, checked_at), vec!["go"]);

    // The session is fully alive after the pause point: the next
    // prompt branches from the target.
    handle.message(&id, "branch");
    frames.extend(drain(&mut handle).await);
    assert_eq!(finished_outputs(&frames), vec!["done", "branch answer"]);
    assert_eq!(chain_users(&handle, &store), vec!["go", "branch"]);
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn checkout_discards_what_was_submitted_before_it_and_keeps_the_rest() {
    let store = temp_store("endpoint-checkout-watermark");
    let session = Factory::new(vec![text_turn("first"), text_turn("second")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);

    // One exchange to its terminal, so the worker is provably back in
    // its select wait when the burst routes (the sends are
    // synchronous; the host routes all three without yielding, and
    // the worker wakes with the checkout ready — the biased select
    // answers it before the work signal).
    let mut frames = Vec::new();
    handle.message(&id, "go");
    collect_until(&mut handle, &mut frames, terminal).await;
    let entry = entry_id_of(&frames, "go");

    handle.message(&id, "queued before");
    handle.checkout(&id, &entry);
    handle.message(&id, "after");
    frames.extend(drain(&mut handle).await);

    // Wire order held: the before-message died by the checkout's
    // clear (never history — the ledger closes with the discard), the
    // after-message survived to run on the rewound chain.
    let discards: Vec<Vec<String>> = frames
        .iter()
        .filter_map(|frame| match &frame.event {
            SessionEvent::MessagesDiscarded { messages } => {
                Some(messages.iter().map(|m| m.text.clone()).collect())
            }
            _ => None,
        })
        .collect();
    assert_eq!(discards, vec![vec!["queued before"]]);
    assert!(!frames.iter().any(|frame| matches!(
        &frame.event,
        SessionEvent::UserMessage { text, .. } if text == "queued before"
    )));
    let checked_at = frames
        .iter()
        .position(|frame| matches!(frame.event, SessionEvent::CheckedOut { .. }))
        .expect("checked_out");
    assert_eq!(bracket_users(&frames, checked_at), vec!["go"]);
    let done_at = checked_at
        + frames[checked_at..]
            .iter()
            .position(|frame| matches!(frame.event, SessionEvent::ReplayDone))
            .expect("the pass closes");
    let after_at = frames
        .iter()
        .position(|frame| {
            matches!(
                &frame.event,
                SessionEvent::UserMessage { text, .. } if text == "after"
            )
        })
        .expect("the survivor ran");
    assert!(
        after_at > done_at,
        "the survivor's user_message follows the pass"
    );
    assert_eq!(finished_outputs(&frames), vec!["first", "second"]);
    assert_eq!(chain_users(&handle, &store), vec!["go", "after"]);
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn consecutive_checkouts_execute_in_order_and_the_last_one_wins() {
    let store = temp_store("endpoint-checkout-order");
    let session = Factory::new(vec![text_turn("a1"), text_turn("b1"), text_turn("c1")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);

    let mut frames = Vec::new();
    handle.message(&id, "one");
    collect_until(&mut handle, &mut frames, terminal).await;
    handle.message(&id, "two");
    collect_until(&mut handle, &mut frames, terminal).await;
    let first = entry_id_of(&frames, "one");
    let second = entry_id_of(&frames, "two");

    // A target is any entry in the file: rewinding past the second
    // exchange does not invalidate it — the second checkout is a
    // branch switch. Both execute, in wire order; the last wins.
    handle.checkout(&id, &first);
    handle.checkout(&id, &second);
    frames.extend(drain(&mut handle).await);

    let checked: Vec<String> = frames
        .iter()
        .filter_map(|frame| match &frame.event {
            SessionEvent::CheckedOut { entry_id, .. } => Some(entry_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(checked, vec![first.clone(), second.clone()], "wire order");
    let first_at = frames
        .iter()
        .position(|frame| {
            matches!(&frame.event,
            SessionEvent::CheckedOut { entry_id, .. } if entry_id == &first)
        })
        .expect("the first checkout");
    let second_at = frames
        .iter()
        .position(|frame| {
            matches!(&frame.event,
            SessionEvent::CheckedOut { entry_id, .. } if entry_id == &second)
        })
        .expect("the second checkout");
    assert!(first_at < second_at);
    assert_eq!(bracket_users(&frames, first_at), vec!["one"]);
    assert_eq!(bracket_users(&frames, second_at), vec!["one", "two"]);
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn an_unknown_entry_checkout_is_an_error_and_a_no_op() {
    let store = temp_store("endpoint-checkout-unknown");
    let session = Factory::new(vec![text_turn("a"), text_turn("b")])
        .into_builder(store.clone())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);

    let mut frames = Vec::new();
    handle.message(&id, "one");
    collect_until(&mut handle, &mut frames, terminal).await;
    handle.checkout(&id, "no-such-entry");
    collect_until(&mut handle, &mut frames, |e| {
        matches!(e, SessionEvent::Error { .. })
    })
    .await;
    handle.message(&id, "two");
    frames.extend(drain(&mut handle).await);

    // The command failed loudly on the channel, kind checkout, and
    // changed nothing: no checked_out, no discard, the conversation
    // continues untouched.
    assert!(frames.iter().any(|frame| matches!(&frame.event,
        SessionEvent::Error { kind, message, .. }
            if kind == tabit_protocol::ErrorKind::CHECKOUT && message.contains("no-such-entry"))));
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame.event, SessionEvent::CheckedOut { .. }))
    );
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame.event, SessionEvent::MessagesDiscarded { .. }))
    );
    assert_eq!(user_texts(&frames), vec!["one", "two"]);
    assert_eq!(finished_outputs(&frames), vec!["a", "b"]);
    assert_eq!(chain_users(&handle, &store), vec!["one", "two"]);
    std::fs::remove_dir_all(store.dir()).ok();
}

#[tokio::test]
async fn abort_then_checkout_composes_at_the_pause_point() {
    let store = temp_store("endpoint-checkout-abort");
    let session = Factory::new(vec![tool_turn("t1", "slow"), text_turn("x")])
        .into_builder(store.clone())
        .dynamic_tool(slow_tool())
        .create("C:/w")
        .expect("session");
    let mut handle = SessionHost::spawn(session, Vec::new(), plain_wiring(&store));
    let id = boot_id(&handle);

    // The composition a frontend sends when it wants stop-then-rewind:
    // abort first, checkout right behind it. Race-free by design —
    // abort acts at once, the checkout executes at the pause point
    // however the abort wound down.
    handle.message(&id, "go");
    let mut frames = Vec::new();
    let mut sent = false;
    let mut go_entry = None;
    while let Some(frame) = handle.next_event().await {
        match &frame.event {
            SessionEvent::UserMessage { text, entry_id } if text == "go" => {
                go_entry = Some(entry_id.clone());
            }
            SessionEvent::ToolCall { .. } if !sent => {
                sent = true;
                handle.abort(&id);
                handle.checkout(&id, go_entry.clone().expect("go's entry id"));
            }
            _ => {}
        }
        let done = matches!(frame.event, SessionEvent::ReplayDone);
        frames.push(frame);
        if done
            && frames
                .iter()
                .any(|f| matches!(f.event, SessionEvent::CheckedOut { .. }))
        {
            break;
        }
    }
    let aborted_at = frames
        .iter()
        .position(|frame| matches!(frame.event, SessionEvent::RunAborted { .. }))
        .expect("the abort terminal");
    let checked_at = frames
        .iter()
        .position(|frame| matches!(frame.event, SessionEvent::CheckedOut { .. }))
        .expect("the checkout executed after the abort");
    assert!(aborted_at < checked_at);
    assert_eq!(bracket_users(&frames, checked_at), vec!["go"]);
    assert_eq!(chain_users(&handle, &store), vec!["go"]);
    std::fs::remove_dir_all(store.dir()).ok();
}
