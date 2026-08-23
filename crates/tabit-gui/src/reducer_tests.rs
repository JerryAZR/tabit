use super::*;
use tabit_protocol::{EventFrame, SessionEvent, StreamId};

fn event(event: SessionEvent) -> InMsg {
    InMsg::Event(Box::new(EventFrame {
        stream: StreamId::main(),
        event,
    }))
}

fn ack() -> InMsg {
    InMsg::Ack {
        session_id: "s1".to_string(),
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
        Some(Group::User { text }) if text == "hello"
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
            title: title.to_string(),
            body: "body".to_string(),
            options: vec![tabit_protocol::InteractionOption {
                label: "Allow".to_string(),
                description: None,
            }],
            free_text: true,
        }));
    }
    assert_eq!(state.interactions.len(), 2);
    assert_eq!(state.interactions[0].id, "i1");
    assert_eq!(state.interactions[1].options, vec!["Allow".to_string()]);

    state.interaction_answered("i1");
    assert_eq!(state.interactions.len(), 1);
    assert_eq!(state.interactions[0].id, "i2");
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
            title: "Allow `bash` to run?".to_string(),
            body: "rm -rf target".to_string(),
            options: Vec::new(),
            free_text: true,
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
