use super::*;
use tabit_protocol::{EventFrame, SessionEvent, StreamId};

/// The session the test's [`ack`] boots; every frame from it carries
/// this stamp (v3: the stream is the session id).
const BOOT: &str = "s1";

fn event(event: SessionEvent) -> InMsg {
    InMsg::Event(Box::new(EventFrame {
        stream: Some(StreamId::new(BOOT)),
        event,
    }))
}

/// An event from some other session (a background stream).
fn from(stream: &str, event: SessionEvent) -> InMsg {
    InMsg::Event(Box::new(EventFrame {
        stream: Some(StreamId::new(stream)),
        event,
    }))
}

/// A backend-level frame: no session stamp (the optional-stream
/// ruling) — the honest shape for the catalog and session creation.
fn backend(event: SessionEvent) -> InMsg {
    InMsg::Event(Box::new(EventFrame {
        stream: None,
        event,
    }))
}

fn ack() -> InMsg {
    InMsg::Ack {
        session_id: BOOT.to_string(),
        session_path: "sessions/s1.jsonl".to_string(),
        model: tabit_protocol::ModelSelection::new("local", "m"),
        resumed: true,
    }
}

fn user(text: &str) -> InMsg {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    event(SessionEvent::UserMessage {
        text: text.to_string(),
        entry_id: format!("e{}", SEQ.fetch_add(1, Ordering::SeqCst)),
    })
}

fn delta(text: &str) -> InMsg {
    event(SessionEvent::TextDelta {
        turn_id: "t1".to_string(),
        text: text.to_string(),
    })
}

fn segments(state: &GuiState) -> Vec<String> {
    let Some(Group::Turn(turn)) = state.transcript.last() else {
        panic!("expected a trailing turn");
    };
    turn.segments
        .iter()
        .map(|s| match s {
            Segment::Reasoning { text, .. } => format!("think:{text}"),
            Segment::Text(text) => format!("text:{text}"),
            Segment::ToolCall(tool) => format!("tool:{}:{}", tool.name, tool.done),
        })
        .collect()
}

#[test]
fn handshake_facts_and_live_phase() {
    let mut state = GuiState::default();
    state.reduce(ack());
    assert_eq!(state.phase, Phase::Live);
    let facts = state.facts.as_ref().unwrap();
    assert_eq!(facts.session_id, "s1");
    assert_eq!(facts.model.provider, "local");
}

#[test]
fn a_run_lifecycle_from_message_to_terminal() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.message_sent("who are you?".to_string());
    assert_eq!(state.pending.len(), 0, "idle send never queues");
    state.reduce(user("who are you?"));
    assert_eq!(state.pending.len(), 0);
    assert!(state.running);
    assert!(matches!(state.transcript.last(), Some(Group::User { .. })));

    state.reduce(delta("I'm "));
    state.reduce(delta("tabit."));
    assert_eq!(segments(&state), vec!["text:I'm tabit."]);

    state.reduce(event(SessionEvent::RunFinished {
        output: "I'm tabit.".to_string(),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 4,
            total_tokens: 14,
            ..Usage::default()
        },
    }));
    assert!(!state.running);
    assert_eq!(state.usage.total_tokens, 14);
}

#[test]
fn tools_and_reasoning_fold_into_the_turn() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("list files"));
    state.reduce(event(SessionEvent::ReasoningDelta {
        turn_id: "t1".to_string(),
        id: "r0".to_string(),
        reasoning: "checking ".to_string(),
    }));
    state.reduce(event(SessionEvent::ReasoningDelta {
        turn_id: "t1".to_string(),
        id: "r0".to_string(),
        reasoning: "the ls tool".to_string(),
    }));
    state.reduce(event(SessionEvent::ToolCall {
        turn_id: "t1".to_string(),
        name: "ls".to_string(),
        call_id: "c1".to_string(),
        internal_call_id: "i1".to_string(),
        arguments: Some("{}".to_string()),
    }));
    state.reduce(event(SessionEvent::ToolResult {
        turn_id: "t1".to_string(),
        entry_id: "e1".to_string(),
        name: "ls".to_string(),
        internal_call_id: "i1".to_string(),
        content: "3 files".to_string(),
        status: tabit_protocol::ToolResultStatus::Success,
    }));
    state.reduce(delta("done"));

    assert_eq!(
        segments(&state),
        vec!["think:checking the ls tool", "tool:ls:true", "text:done",]
    );
}

#[test]
fn segments_render_in_arrival_order() {
    // Owner report: tool calls appeared above text that arrived
    // first. The turn must preserve the wire's interleaving.
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("check"));
    state.reduce(delta("Let me look. "));
    state.reduce(event(SessionEvent::ToolCall {
        turn_id: "t1".to_string(),
        name: "ls".to_string(),
        call_id: "c".to_string(),
        internal_call_id: "i".to_string(),
        arguments: None,
    }));
    state.reduce(event(SessionEvent::ToolResult {
        turn_id: "t1".to_string(),
        entry_id: "e1".to_string(),
        name: "ls".to_string(),
        internal_call_id: "i".to_string(),
        content: String::new(),
        status: tabit_protocol::ToolResultStatus::Success,
    }));
    state.reduce(delta("Found three files."));
    state.reduce(event(SessionEvent::ToolCall {
        turn_id: "t1".to_string(),
        name: "read".to_string(),
        call_id: "c2".to_string(),
        internal_call_id: "i2".to_string(),
        arguments: None,
    }));

    assert_eq!(
        segments(&state),
        vec![
            "text:Let me look. ",
            "tool:ls:true",
            "text:Found three files.",
            "tool:read:false",
        ]
    );
}

#[test]
fn a_second_turn_opens_a_new_group() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("a"));
    state.reduce(delta("first"));
    state.reduce(event(SessionEvent::ToolCall {
        turn_id: "t1".to_string(),
        name: "ls".to_string(),
        call_id: "c".to_string(),
        internal_call_id: "i".to_string(),
        arguments: None,
    }));
    state.reduce(event(SessionEvent::RunFinished {
        output: String::new(),
        usage: Usage::default(),
    }));
    // The next run's first delta opens a fresh turn.
    state.reduce(user("b"));
    state.reduce(delta("second"));
    let turns: Vec<&TurnGroup> = state
        .transcript
        .iter()
        .filter_map(|g| match g {
            Group::Turn(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(turns.len(), 2);
    assert_eq!(
        turns[1]
            .segments
            .iter()
            .filter_map(|s| match s {
                Segment::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<String>(),
        "second"
    );
    assert_eq!(
        turns[0]
            .segments
            .iter()
            .filter(|s| matches!(s, Segment::ToolCall(_)))
            .count(),
        1
    );
}

#[test]
fn turn_retried_drops_the_provisional_turn() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("a"));
    state.reduce(delta("poisoned"));
    state.reduce(event(SessionEvent::TurnRetried {
        turn_id: "t1".to_string(),
        turn: 1,
    }));
    assert!(matches!(state.transcript.last(), Some(Group::User { .. })));
    state.reduce(delta("fixed"));
    assert_eq!(segments(&state), vec!["text:fixed"]);
}

#[test]
fn run_failure_and_abort_end_the_run() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("a"));
    state.reduce(event(SessionEvent::RunFailed {
        message: "provider died".to_string(),
    }));
    assert!(!state.running);
    assert!(matches!(
        state.transcript.last(),
        Some(Group::Notice { error: true, .. })
    ));

    state.reduce(user("b"));
    state.reduce(delta("partial"));
    state.reduce(event(SessionEvent::RunAborted {
        output: String::new(),
    }));
    assert!(!state.running);
    // Aborted partial text stays visible.
    assert_eq!(segments(&state), vec!["text:partial"]);
}

#[test]
fn idle_sends_never_queue() {
    // Owner ruling: on idle the queue is known to drain immediately —
    // no waiting state; user_message (milliseconds later) is the
    // acknowledgment.
    let mut state = GuiState::default();
    state.reduce(ack());
    state.message_sent("hello".to_string());
    assert!(state.pending.is_empty(), "idle send does not wait");
    state.reduce(user("hello"));
    assert!(state.pending.is_empty());
    assert!(matches!(
        state.transcript.last(),
        Some(Group::User { text, .. }) if text == "hello"
    ));
}

#[test]
fn steers_wait_and_pair_by_fifo() {
    // v1 heuristic: identical steers pair in order.
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("start"));
    state.message_sent("same".to_string());
    state.message_sent("same".to_string());
    assert_eq!(state.pending.len(), 2, "mid-run sends wait");
    state.reduce(user("same"));
    assert_eq!(state.pending.len(), 1);
    state.reduce(user("same"));
    assert_eq!(state.pending.len(), 0);
}

#[test]
fn backend_exit_classifies_clean_vs_crash() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("a"));
    state.reduce(delta("mid"));
    state.reduce(InMsg::BackendExited { code: Some(101) });
    match &state.phase {
        Phase::Exited { clean, reason } => {
            assert!(!*clean);
            assert!(reason.contains("101"), "{reason}");
        }
        other => panic!("expected exit, got {other:?}"),
    }

    let mut idle = GuiState::default();
    idle.reduce(ack());
    idle.reduce(InMsg::BackendExited { code: Some(0) });
    assert!(matches!(idle.phase, Phase::Exited { clean: true, .. }));

    // An internal-error crash is never clean, even idle: the stderr
    // report is the payload the user must send back.
    let mut idle_crash = GuiState::default();
    idle_crash.reduce(ack());
    idle_crash.reduce(InMsg::BackendExited { code: Some(101) });
    match &idle_crash.phase {
        Phase::Exited { clean, reason } => {
            assert!(!*clean);
            assert!(reason.contains("internal error"), "{reason}");
        }
        other => panic!("expected exit, got {other:?}"),
    }
}

#[test]
fn protocol_error_is_a_notice_and_the_connection_survives() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(InMsg::ProtocolError("bad line".to_string()));
    assert_eq!(state.phase, Phase::Live);
    assert!(matches!(
        state.transcript.last(),
        Some(Group::Notice { error: true, .. })
    ));
}

#[test]
fn an_absorbed_continue_miss_announces_the_fresh_start() {
    // The GUI always asks to resume; resumed: false means the backend
    // started fresh — one muted note, and the connection is Live.
    let mut state = GuiState::default();
    state.reduce(InMsg::Ack {
        session_id: "s".to_string(),
        session_path: "s.jsonl".to_string(),
        model: tabit_protocol::ModelSelection::new("local", "m"),
        resumed: false,
    });
    assert_eq!(state.phase, Phase::Live);
    match state.transcript.first() {
        Some(Group::Notice { text, error: false }) => {
            assert!(text.contains("started fresh"), "{text}");
        }
        other => panic!("expected a muted fresh-start note, got {other:?}"),
    }
}

#[test]
fn rejection_carries_its_reason_into_the_transcript() {
    let mut state = GuiState::default();
    state.reduce(InMsg::Rejected(
        "first-run setup needed: no config
create ~/.tabit/providers.toml"
            .to_string(),
    ));
    assert!(state.handshake_rejected);
    assert!(matches!(state.phase, Phase::Exited { clean: false, .. }));
    // The full guide is transcript material, the status reason is short.
    match state.transcript.last() {
        Some(Group::Notice { text, error: true }) => {
            assert!(text.contains("providers.toml"), "{text}");
        }
        other => panic!("expected an error notice, got {other:?}"),
    }
    // The exit event that follows never clobbers the story.
    state.reduce(InMsg::BackendExited { code: Some(1) });
    match &state.phase {
        Phase::Exited { reason, .. } => assert!(reason.contains("handshake rejected"), "{reason}"),
        other => panic!("expected exit, got {other:?}"),
    }
}

#[test]
fn startup_exit_is_not_mid_run() {
    // A backend that dies before the handshake is a startup failure,
    // never "mid-run" — the most common first-run shape.
    let mut state = GuiState::default();
    state.reduce(InMsg::BackendExited { code: Some(1) });
    match &state.phase {
        Phase::Exited { clean, reason } => {
            assert!(!*clean);
            assert!(reason.contains("code 1"), "{reason}");
            assert!(!reason.contains("mid-run"), "{reason}");
        }
        other => panic!("expected exit, got {other:?}"),
    }
}

#[test]
fn interaction_cards_open_in_order_and_close_on_answer() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("go"));
    for (id, title) in [
        ("i1", "Allow `bash` to run?"),
        ("i2", "Question from the assistant"),
    ] {
        state.reduce(event(SessionEvent::InteractionRequested {
            id: id.to_string(),
            ui_type: tabit_protocol::templates::ui::CONFIRM.to_string(),
            payload: serde_json::json!({
                "title": title,
                "body": "body",
                "options": [{"label": "Allow"}],
                "free_text": true
            }),
        }));
    }
    assert_eq!(state.interactions.len(), 2);
    assert_eq!(state.interactions[0].id(), "i1");
    assert!(matches!(
        &state.interactions[1],
        super::InteractionCard::Confirm { options, .. }
            if options == &vec!["Allow".to_string()]
    ));

    state.interaction_answered("i1");
    assert_eq!(state.interactions.len(), 1);
    assert_eq!(state.interactions[0].id(), "i2");
}

#[test]
fn every_run_terminal_closes_all_open_cards() {
    for terminal in [
        SessionEvent::RunFinished {
            output: String::new(),
            usage: Usage::default(),
        },
        SessionEvent::RunAborted {
            output: String::new(),
        },
        SessionEvent::RunFailed {
            message: "boom".to_string(),
        },
    ] {
        let mut state = GuiState::default();
        state.reduce(ack());
        state.reduce(user("go"));
        state.reduce(event(SessionEvent::InteractionRequested {
            id: "i1".to_string(),
            ui_type: tabit_protocol::templates::ui::ASK.to_string(),
            payload: serde_json::json!({"prompt": "rm -rf target?"}),
        }));
        state.reduce(event(terminal));
        assert!(
            state.interactions.is_empty(),
            "the terminal must close every open card"
        );
        assert!(!state.running);
    }
}

#[test]
fn turn_truncated_is_a_non_error_notice_and_the_run_continues() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("a"));
    state.reduce(delta("partial answer"));
    state.reduce(event(SessionEvent::TurnTruncated {
        turn_id: "t1".to_string(),
    }));
    // Informational (ENGINE.md delta 9): the run is still live and the
    // notice renders as a non-error row.
    assert!(state.running);
    assert!(matches!(
        state.transcript.last(),
        Some(Group::Notice { error: false, .. })
    ));
    // Deltas after the warning keep folding (the notice is its own row,
    // so a fresh text row opens after it — the run itself is unaffected).
    state.reduce(delta(" continues"));
    assert_eq!(
        segments(&state).last().map(String::as_str),
        Some("text: continues")
    );
    assert!(state.running, "the warning never ends the run");
}

#[test]
fn abort_clears_pending_steers_with_the_cards() {
    // Backend flag 6: abort discards the queue — the queued rows can
    // never be acknowledged and must not pair with the next run.
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("a"));
    state.reduce(delta("working"));
    // A mid-run send enters the waiting display (steer queued).
    state.message_sent("steer this".to_string());
    assert_eq!(state.pending.len(), 1);
    state.reduce(event(SessionEvent::RunAborted {
        output: String::new(),
    }));
    assert!(state.pending.is_empty(), "abort discards queued steers");
}

#[test]
fn pending_rows_resolve_by_id_not_position() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("first message"));
    state.running = true;

    // Two identical mid-run sends: local rows first, then the backend's
    // queued acknowledgments upgrade them by text; ids make the rows
    // resolve individually even though the texts match.
    state.message_sent("same text".to_string());
    state.message_sent("same text".to_string());
    state.reduce(event(SessionEvent::MessageQueued {
        id: "q1".to_string(),
        text: "same text".to_string(),
    }));
    state.reduce(event(SessionEvent::MessageQueued {
        id: "q2".to_string(),
        text: "same text".to_string(),
    }));
    assert_eq!(state.pending.len(), 2);

    // One resolves by id (drained); the other is handed back by a
    // discard — duplicate texts disambiguate exactly because the id,
    // not the position or text, is the key.
    state.reduce(event(SessionEvent::UserMessage {
        text: "same text".to_string(),
        entry_id: "q2".to_string(),
    }));
    assert_eq!(state.pending.len(), 1);
    assert_eq!(state.pending[0].id.as_deref(), Some("q1"));
    state.reduce(event(SessionEvent::MessagesDiscarded {
        messages: vec![tabit_protocol::DiscardedMessage {
            id: "q1".to_string(),
            text: "same text".to_string(),
        }],
    }));
    assert!(state.pending.is_empty(), "the discarded id leaves pending");
}

#[test]
fn replay_brackets_are_inert_and_model_changed_updates_the_facts() {
    let mut state = GuiState::default();
    state.reduce(ack());
    let before = state.facts.as_ref().expect("facts").model.clone();
    state.reduce(event(SessionEvent::ReplayStarted { total: 3 }));
    state.reduce(event(SessionEvent::ModelChanged {
        provider: "other".to_string(),
        model: "m2".to_string(),
        thinking_level: None,
    }));
    state.reduce(event(SessionEvent::ReplayDone));
    // The brackets changed nothing; the model change moved the facts —
    // the picker follows history, not just the handshake.
    assert_eq!(state.facts.as_ref().expect("facts").model.model, "m2");
    assert_ne!(state.facts.as_ref().expect("facts").model, before);
}

#[test]
fn tool_results_land_on_their_calls_with_content_and_failure() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("run it"));
    state.reduce(event(SessionEvent::ToolCall {
        turn_id: "t1".to_string(),
        name: "bash".to_string(),
        call_id: "c1".to_string(),
        internal_call_id: "i1".to_string(),
        arguments: None,
    }));
    state.reduce(event(SessionEvent::ToolResult {
        turn_id: "t1".to_string(),
        entry_id: "e1".to_string(),
        name: "bash".to_string(),
        internal_call_id: "i1".to_string(),
        content: "command exited with status 3:\nboom".to_string(),
        status: tabit_protocol::ToolResultStatus::Failed { exit_code: Some(3) },
    }));

    let Some(Group::Turn(turn)) = state.transcript.last() else {
        panic!("expected a turn");
    };
    let Some(Segment::ToolCall(row)) = turn.segments.first() else {
        panic!("expected the tool call row");
    };
    assert!(row.done);
    let result = row.result.as_ref().expect("the result landed");
    assert!(result.failed);
    assert!(result.content.contains("status 3"), "{}", result.content);
}

#[test]
fn the_startup_catalog_populates_the_switcher() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(backend(SessionEvent::SessionsAvailable {
        sessions: vec![
            tabit_protocol::AvailableSession {
                id: "s2".to_string(),
                created_at: "2026-08-22T10:00:00Z".to_string(),
                entry_count: 7,
            },
            tabit_protocol::AvailableSession {
                id: BOOT.to_string(),
                created_at: "2026-08-22T11:00:00Z".to_string(),
                entry_count: 3,
            },
        ],
    }));
    assert_eq!(state.sessions.len(), 2);
    let boot = state
        .sessions
        .iter()
        .find(|row| row.id == BOOT)
        .expect("the boot session is a row");
    assert_eq!(boot.entry_count, 3);
    assert!(!boot.running, "nothing is running yet");
}

#[test]
fn background_events_update_liveness_but_never_the_transcript() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(backend(SessionEvent::SessionsAvailable {
        sessions: vec![tabit_protocol::AvailableSession {
            id: "s2".to_string(),
            created_at: "2026-08-22T10:00:00Z".to_string(),
            entry_count: 7,
        }],
    }));

    // A run starts in the background session: the active view is
    // untouched, the row goes live.
    state.reduce(from(
        "s2",
        SessionEvent::UserMessage {
            text: "review this".to_string(),
            entry_id: "e9".to_string(),
        },
    ));
    state.reduce(from(
        "s2",
        SessionEvent::TextDelta {
            turn_id: "t9".to_string(),
            text: "looking".to_string(),
        },
    ));
    assert!(state.transcript.is_empty(), "background text never renders");
    assert!(!state.running, "the active session is not the one running");
    assert!(
        state
            .sessions
            .iter()
            .find(|row| row.id == "s2")
            .unwrap()
            .running
    );

    // An error in the background always surfaces: a notice here, an
    // attention mark on the row.
    state.reduce(from(
        "s2",
        SessionEvent::error_persist_degraded(4, "disk full"),
    ));
    assert!(
        state
            .transcript
            .iter()
            .any(|group| matches!(group, Group::Notice { error: true, .. })),
        "background errors never vanish with the view"
    );
    assert!(
        state
            .sessions
            .iter()
            .find(|row| row.id == "s2")
            .unwrap()
            .attention
    );

    // The run ends in the background; the mark SURVIVES the terminal
    // (its whole point — the error is still unseen) and clears only
    // when the user switches there.
    state.reduce(from(
        "s2",
        SessionEvent::RunFinished {
            output: String::new(),
            usage: Usage::default(),
        },
    ));
    assert!(
        !state
            .sessions
            .iter()
            .find(|row| row.id == "s2")
            .unwrap()
            .running
    );
    assert!(
        state
            .sessions
            .iter()
            .find(|row| row.id == "s2")
            .unwrap()
            .attention,
        "an unseen error outlives its run"
    );
    state.open_session("s2");
    assert!(
        !state
            .sessions
            .iter()
            .find(|row| row.id == "s2")
            .unwrap()
            .attention,
        "switching there is seeing it"
    );
}

#[test]
fn switching_is_optimistic_and_the_replay_pass_rebuilds() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("first question"));
    state.reduce(delta("first answer"));
    state.reduce(backend(SessionEvent::SessionsAvailable {
        sessions: vec![tabit_protocol::AvailableSession {
            id: "s2".to_string(),
            created_at: "2026-08-22T10:00:00Z".to_string(),
            entry_count: 7,
        }],
    }));

    // The user picks s2: the view clears immediately (optimistic), the
    // open_session command follows out-of-band.
    state.open_session("s2");
    assert_eq!(state.active, "s2");
    assert!(state.transcript.is_empty(), "the old view is gone at once");
    assert!(state.pending.is_empty());

    // The pass arrives on s2's stream: the reset bracket, then the
    // rebuilt transcript — same arms as live traffic.
    state.reduce(from("s2", SessionEvent::ReplayStarted { total: 3 }));
    state.reduce(from(
        "s2",
        SessionEvent::UserMessage {
            text: "older question".to_string(),
            entry_id: "e1".to_string(),
        },
    ));
    state.reduce(from(
        "s2",
        SessionEvent::TextDelta {
            turn_id: "t1".to_string(),
            text: "older answer".to_string(),
        },
    ));
    state.reduce(from("s2", SessionEvent::ReplayDone));
    assert_eq!(state.transcript.len(), 2);
    assert!(
        matches!(state.transcript.first(), Some(Group::User { text, .. }) if text == "older question")
    );

    // A stray event for the old stream is background now.
    state.reduce(delta("late first-session text"));
    assert_eq!(
        state.transcript.len(),
        2,
        "the un-viewed stream does not render"
    );
}

#[test]
fn session_created_switches_to_the_empty_new_session() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("work in the boot session"));

    // Honest shape: the creation frame is backend-level (no stamp;
    // the payload carries the new id).
    state.reduce(backend(SessionEvent::SessionCreated {
        id: "s9".to_string(),
        path: "sessions/20260822_s9.jsonl".to_string(),
        model: tabit_protocol::ModelSelection::new("local", "m9"),
    }));
    // The brand-new session is empty — no replay will come; the switch
    // alone is the whole state, and the facts follow.
    assert_eq!(state.active, "s9");
    assert!(state.transcript.is_empty());
    assert_eq!(state.facts.as_ref().unwrap().session_id, "s9");
    assert_eq!(
        state.facts.as_ref().unwrap().session_path,
        "sessions/20260822_s9.jsonl"
    );
    assert_eq!(state.sessions.last().unwrap().id, "s9");

    // Its first live message streams into the fresh view.
    state.reduce(from(
        "s9",
        SessionEvent::UserMessage {
            text: "clean start".to_string(),
            entry_id: "n1".to_string(),
        },
    ));
    assert!(state.running);
    assert!(
        matches!(state.transcript.first(), Some(Group::User { text, .. }) if text == "clean start")
    );
}

#[test]
fn a_replay_bracket_resets_the_view_structurally() {
    // Even mid-history (a checkout's pass, stage 2) the bracket is the
    // rebuild point: prior view content cannot survive it.
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("one"));
    state.reduce(delta("answer one"));
    state.reduce(event(SessionEvent::ReplayStarted { total: 1 }));
    assert!(
        state.transcript.is_empty(),
        "the bracket discards the old view"
    );
    state.reduce(user("the branched history's message"));
    state.reduce(event(SessionEvent::ReplayDone));
    assert_eq!(state.transcript.len(), 1);
}

#[test]
fn a_new_session_lands_even_while_the_current_one_runs() {
    // The owner's report: "new session" mid-conversation did nothing.
    // Creation is connection-level — it must switch the view with the
    // current session mid-run, and the abandoned run's liveness must
    // show on its row.
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(backend(SessionEvent::SessionsAvailable {
        sessions: vec![tabit_protocol::AvailableSession {
            id: BOOT.to_string(),
            created_at: "2026-08-22T11:00:00Z".to_string(),
            entry_count: 3,
        }],
    }));
    state.reduce(user("work in progress"));
    assert!(state.running);

    state.reduce(backend(SessionEvent::SessionCreated {
        id: "s9".to_string(),
        path: "sessions/20260822_s9.jsonl".to_string(),
        model: tabit_protocol::ModelSelection::new("local", "m9"),
    }));
    assert_eq!(
        state.active, "s9",
        "creation switches immediately, mid-run or not"
    );
    assert!(state.transcript.is_empty());
    assert!(!state.running, "the fresh session is idle");
    assert!(
        state
            .sessions
            .iter()
            .find(|row| row.id == BOOT)
            .unwrap()
            .running,
        "the abandoned run's dot survives the switch"
    );

    // The old session's stream keeps arriving in the background; its
    // terminal settles the dot without touching the new view.
    state.reduce(delta("still streaming"));
    assert!(
        state.transcript.is_empty(),
        "background content never renders"
    );
    state.reduce(from(
        BOOT,
        SessionEvent::RunFinished {
            output: "done later".to_string(),
            usage: Usage::default(),
        },
    ));
    assert!(
        !state
            .sessions
            .iter()
            .find(|row| row.id == BOOT)
            .unwrap()
            .running
    );
    assert!(state.transcript.is_empty());
}

#[test]
fn an_active_sessions_run_state_is_mirrored_onto_its_row() {
    // The switcher dot must be right even when the run started while
    // the session was being viewed (the liveness mirror runs before
    // the active/background split).
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(backend(SessionEvent::SessionsAvailable {
        sessions: vec![tabit_protocol::AvailableSession {
            id: BOOT.to_string(),
            created_at: "2026-08-22T11:00:00Z".to_string(),
            entry_count: 0,
        }],
    }));
    state.reduce(user("go"));
    assert!(
        state
            .sessions
            .iter()
            .find(|row| row.id == BOOT)
            .unwrap()
            .running,
        "the row reflects the run viewed live"
    );
}

#[test]
fn a_replay_pass_never_marks_the_session_running() {
    // The review round's finding: a pass carries `user_message`s but
    // never a terminal, so an unguarded fold left the session "running"
    // forever after startup replay or a switch (phantom dot, abort
    // button, 10 Hz repaint spin, sends misread as steers).
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(backend(SessionEvent::SessionsAvailable {
        sessions: vec![tabit_protocol::AvailableSession {
            id: "s2".to_string(),
            created_at: "2026-08-22T10:00:00Z".to_string(),
            entry_count: 9,
        }],
    }));
    state.open_session("s2");
    state.reduce(from("s2", SessionEvent::ReplayStarted { total: 3 }));
    state.reduce(from(
        "s2",
        SessionEvent::UserMessage {
            text: "history question".to_string(),
            entry_id: "e1".to_string(),
        },
    ));
    state.reduce(from(
        "s2",
        SessionEvent::TextDelta {
            turn_id: "t1".to_string(),
            text: "history answer".to_string(),
        },
    ));
    // Inside the bracket: the view grows, liveness does not.
    assert!(!state.running, "a pass is history, not a run");
    assert!(
        !state
            .sessions
            .iter()
            .find(|row| row.id == "s2")
            .unwrap()
            .running,
        "the row is not poisoned either"
    );
    state.reduce(from("s2", SessionEvent::ReplayDone));
    assert!(!state.running);
    assert!(
        !state
            .sessions
            .iter()
            .find(|row| row.id == "s2")
            .unwrap()
            .running
    );

    // After the bracket, live traffic tracks liveness again.
    state.reduce(from(
        "s2",
        SessionEvent::UserMessage {
            text: "a real one".to_string(),
            entry_id: "e9".to_string(),
        },
    ));
    assert!(state.running, "live runs still mark liveness");
    state.reduce(from(
        "s2",
        SessionEvent::RunFinished {
            output: String::new(),
            usage: Usage::default(),
        },
    ));
    assert!(!state.running);
}

#[test]
fn a_background_pass_after_a_fast_switch_does_not_poison_the_row() {
    // open B, switch away before its pass lands: the pass arrives on a
    // non-viewed stream — its content must not mark B running.
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(backend(SessionEvent::SessionsAvailable {
        sessions: vec![tabit_protocol::AvailableSession {
            id: "s2".to_string(),
            created_at: "2026-08-22T10:00:00Z".to_string(),
            entry_count: 9,
        }],
    }));
    state.open_session("s2");
    state.open_session(BOOT); // switch back before the pass arrives
    state.reduce(from("s2", SessionEvent::ReplayStarted { total: 1 }));
    state.reduce(from(
        "s2",
        SessionEvent::UserMessage {
            text: "late history".to_string(),
            entry_id: "e1".to_string(),
        },
    ));
    state.reduce(from("s2", SessionEvent::ReplayDone));
    assert!(
        !state
            .sessions
            .iter()
            .find(|row| row.id == "s2")
            .unwrap()
            .running,
        "a background pass is history too"
    );
}

#[test]
fn cards_survive_a_view_switch_and_route_by_their_own_session() {
    // The parked-permission deadlock: switch away from a session whose
    // run waits on a card, switch back — the card must be answerable
    // again, and answering must reach the card's session (not the
    // active one).
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(backend(SessionEvent::SessionsAvailable {
        sessions: vec![tabit_protocol::AvailableSession {
            id: "s2".to_string(),
            created_at: "2026-08-22T10:00:00Z".to_string(),
            entry_count: 4,
        }],
    }));
    state.reduce(event(SessionEvent::InteractionRequested {
        id: "ask-1".to_string(),
        ui_type: tabit_protocol::templates::ui::CONFIRM.to_string(),
        payload: serde_json::json!({
            "title": "Run command?",
            "body": "rm -rf target",
            "options": [],
            "free_text": false
        }),
    }));
    assert_eq!(state.interactions.len(), 1);
    assert_eq!(state.interactions[0].session(), BOOT);

    // Switch away and back: the card survives (and the switch-back's
    // replay bracket does not clear it — cards never replay).
    state.open_session("s2");
    assert_eq!(state.interactions.len(), 1, "switching does not drop cards");
    state.open_session(BOOT);
    assert_eq!(state.interactions[0].session(), BOOT);

    // The terminal closes only its own session's cards.
    state.reduce(event(SessionEvent::RunAborted {
        output: String::new(),
    }));
    assert!(state.interactions.is_empty());
}

#[test]
fn a_background_question_raises_attention_and_dies_with_its_run() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(backend(SessionEvent::SessionsAvailable {
        sessions: vec![tabit_protocol::AvailableSession {
            id: "s2".to_string(),
            created_at: "2026-08-22T10:00:00Z".to_string(),
            entry_count: 4,
        }],
    }));
    state.reduce(from(
        "s2",
        SessionEvent::UserMessage {
            text: "go".to_string(),
            entry_id: "e1".to_string(),
        },
    ));
    state.reduce(from(
        "s2",
        SessionEvent::InteractionRequested {
            id: "ask-2".to_string(),
            ui_type: tabit_protocol::templates::ui::CONFIRM.to_string(),
            payload: serde_json::json!({
                "title": "Run command?",
                "body": "cargo test",
                "options": [],
                "free_text": false
            }),
        },
    ));
    // The card is held (answerable after switching) and the row asks
    // for attention.
    assert_eq!(state.interactions[0].session(), "s2");
    assert!(
        state
            .sessions
            .iter()
            .find(|row| row.id == "s2")
            .unwrap()
            .attention
    );

    // A background terminal closes its session's card, not the
    // active view's state.
    state.reduce(from(
        "s2",
        SessionEvent::RunAborted {
            output: String::new(),
        },
    ));
    assert!(
        state.interactions.is_empty(),
        "the question died with its run"
    );
}

#[test]
fn a_checkout_pass_rebuilds_the_transcript_and_liveness_stays_settled() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("one"));
    state.reduce(delta("first answer"));
    state.reduce(event(SessionEvent::RunFinished {
        output: "first answer".to_string(),
        usage: Usage::default(),
    }));
    state.reduce(user("two"));
    state.reduce(delta("second answer"));
    state.reduce(event(SessionEvent::RunFinished {
        output: "second answer".to_string(),
        usage: Usage::default(),
    }));
    assert_eq!(state.transcript.len(), 4);

    // The checkout itself changes no view state (it executes at a
    // pause point — liveness already settled); the pass that follows
    // IS the rebuild, the same path as a view switch.
    state.reduce(event(SessionEvent::CheckedOut {
        entry_id: "e0".to_string(),
        base_id: None,
    }));
    assert_eq!(
        state.transcript.len(),
        4,
        "checked_out alone rebuilds nothing"
    );
    state.reduce(event(SessionEvent::ReplayStarted { total: 2 }));
    // A fixed id: the rebuilt row must carry the entry id verbatim —
    // it is the next checkout target.
    state.reduce(event(SessionEvent::UserMessage {
        text: "one".to_string(),
        entry_id: "target-1".to_string(),
    }));
    state.reduce(delta("first answer"));
    state.reduce(event(SessionEvent::ReplayDone));
    assert!(!state.running, "a pass is history, never liveness");
    assert_eq!(
        state.transcript.len(),
        2,
        "one user row and its answer turn"
    );
    assert!(matches!(
        state.transcript.first(),
        Some(Group::User { text, .. }) if text == "one"
    ));
    // The rebuilt row keeps its entry id — the next checkout target.
    assert!(matches!(
        state.transcript.first(),
        Some(Group::User { entry_id, .. }) if entry_id == "target-1"
    ));
}

#[test]
fn a_failed_checkout_surfaces_as_an_error_notice() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("one"));
    state.reduce(event(SessionEvent::RunFinished {
        output: String::new(),
        usage: Usage::default(),
    }));
    state.reduce(event(SessionEvent::Error {
        kind: "checkout".to_string(),
        message: "no entry `nope` in this session".to_string(),
        pending: None,
    }));
    match state.transcript.last() {
        Some(Group::Notice { text, error }) => {
            assert!(text.contains("no entry"), "{text}");
            assert!(error);
        }
        other => panic!("the failed checkout surfaces, got {other:?}"),
    }
    // The transcript before it is untouched — the checkout was a no-op.
    assert!(matches!(state.transcript.first(), Some(Group::User { .. })));
}

#[test]
fn unknown_and_malformed_interaction_widgets_surface_as_notices_not_cards() {
    let mut state = GuiState::default();
    state.reduce(ack());
    // An extension widget this frontend cannot render: reported, never
    // answered, never a card.
    state.reduce(event(SessionEvent::InteractionRequested {
        id: "i1".to_string(),
        ui_type: "ext:demo:map".to_string(),
        payload: serde_json::json!({"region": "north"}),
    }));
    match state.transcript.last() {
        Some(Group::Notice { text, error }) => {
            assert!(text.contains("ext:demo:map"), "{text}");
            assert!(text.contains("not answered"), "{text}");
            assert!(error);
        }
        other => panic!("the unknown widget surfaces, got {other:?}"),
    }
    assert!(state.interactions.is_empty());
    // A native card in a shape this frontend cannot parse: same
    // treatment — a notice, not a broken card.
    state.reduce(event(SessionEvent::InteractionRequested {
        id: "i2".to_string(),
        ui_type: tabit_protocol::templates::ui::CONFIRM.to_string(),
        payload: serde_json::json!({"unexpected": true}),
    }));
    match state.transcript.last() {
        Some(Group::Notice { text, error }) => {
            assert!(text.contains("cannot read"), "{text}");
            assert!(error);
        }
        other => panic!("the malformed payload surfaces, got {other:?}"),
    }
    assert!(
        state.interactions.is_empty(),
        "no card opened for either request"
    );
}

#[test]
fn a_catalog_reannouncement_preserves_liveness_and_attention() {
    let mut state = GuiState::default();
    state.reduce(ack());
    let catalog = || {
        backend(SessionEvent::SessionsAvailable {
            sessions: vec![tabit_protocol::AvailableSession {
                id: "s2".to_string(),
                created_at: "2026-08-22T10:00:00Z".to_string(),
                entry_count: 3,
            }],
        })
    };
    state.reduce(catalog());
    // Liveness from a background stream: a run starts, and an error
    // raises the attention flag.
    state.reduce(from(
        "s2",
        SessionEvent::UserMessage {
            text: "go".to_string(),
            entry_id: "e0".to_string(),
        },
    ));
    state.reduce(from(
        "s2",
        SessionEvent::Error {
            kind: "model".to_string(),
            message: "background failure".to_string(),
            pending: None,
        },
    ));
    let row = state
        .sessions
        .iter()
        .find(|row| row.id == "s2")
        .expect("the catalog row");
    assert!(row.running && row.attention);
    // The re-announcement keeps what the window already knows.
    state.reduce(catalog());
    let row = state
        .sessions
        .iter()
        .find(|row| row.id == "s2")
        .expect("the re-announced row");
    assert!(row.running, "a re-announced row keeps its run dot");
    assert!(row.attention, "a re-announced row keeps its attention flag");
}

#[test]
fn a_backend_only_queued_notice_tracks_by_id_until_resolved() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("go"));
    // No local echo for this one — the notice itself must open the row.
    state.reduce(event(SessionEvent::MessageQueued {
        id: "q9".to_string(),
        text: "from another surface".to_string(),
    }));
    assert_eq!(state.pending.len(), 1);
    assert_eq!(state.pending[0].id.as_deref(), Some("q9"));
    // Resolved exactly by id: the user_message carrying it.
    state.reduce(event(SessionEvent::UserMessage {
        text: "from another surface".to_string(),
        entry_id: "q9".to_string(),
    }));
    assert_eq!(state.pending.len(), 0, "the row left when its id drained");
    // And the discard resolution path: a queued row leaves by id too.
    state.reduce(event(SessionEvent::MessageQueued {
        id: "q10".to_string(),
        text: "queued".to_string(),
    }));
    state.reduce(event(SessionEvent::MessagesDiscarded {
        messages: vec![tabit_protocol::DiscardedMessage {
            id: "q10".to_string(),
            text: "queued".to_string(),
        }],
    }));
    assert_eq!(state.pending.len(), 0, "the discard dropped its row");
}

#[test]
fn a_provider_native_item_folds_as_its_own_row() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("search"));
    state.reduce(delta("checking "));
    assert_eq!(segments(&state), vec!["text:checking "]);
    state.reduce(event(SessionEvent::NativeItem {
        turn_id: "t1".to_string(),
        item: serde_json::json!({"web_search_call": {}}),
    }));
    assert!(matches!(
        state.transcript.last(),
        Some(Group::Native { item }) if item.contains("web_search_call")
    ));
    // The turn's text segment closed before the native row — the item
    // never joins the text buffer.
    let Some(Group::Turn(turn)) = state.transcript.iter().rev().nth(1) else {
        panic!("the turn group sits under the native row");
    };
    assert!(matches!(
        turn.segments.last(),
        Some(Segment::Text(text)) if text == "checking "
    ));
}

#[test]
fn a_signal_death_is_reported_as_a_kill_not_an_exit_code() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(InMsg::BackendExited { code: None });
    let Phase::Exited { clean, reason } = &state.phase else {
        panic!("the exit settled the phase");
    };
    assert!(
        reason.contains("killed"),
        "the reason says killed: {reason}"
    );
    assert!(*clean, "an idle death lost nothing");
}
