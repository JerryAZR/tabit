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
    }
}

fn user(text: &str) -> InMsg {
    event(SessionEvent::UserMessage {
        text: text.to_string(),
    })
}

fn delta(text: &str) -> InMsg {
    event(SessionEvent::TextDelta {
        text: text.to_string(),
    })
}

fn last_turn(state: &GuiState) -> &TurnGroup {
    let Some(Group::Turn(turn)) = state.transcript.last() else {
        panic!("expected a trailing turn");
    };
    turn
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
    assert_eq!(state.pending.len(), 1);
    state.reduce(user("who are you?"));
    assert_eq!(state.pending.len(), 0);
    assert!(state.running);
    assert!(matches!(state.transcript.last(), Some(Group::User { .. })));

    state.reduce(delta("I'm "));
    state.reduce(delta("tabit."));
    assert_eq!(last_turn(&state).text, "I'm tabit.");

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
        id: "r0".to_string(),
        reasoning: "checking ".to_string(),
    }));
    state.reduce(event(SessionEvent::ReasoningDelta {
        id: "r0".to_string(),
        reasoning: "the ls tool".to_string(),
    }));
    state.reduce(event(SessionEvent::ToolCall {
        name: "ls".to_string(),
        call_id: "c1".to_string(),
        internal_call_id: "i1".to_string(),
        arguments: Some("{}".to_string()),
    }));
    state.reduce(event(SessionEvent::ToolResult {
        name: "ls".to_string(),
        internal_call_id: "i1".to_string(),
    }));
    state.reduce(delta("done"));

    let turn = last_turn(&state);
    assert_eq!(
        turn.reasoning,
        vec![ReasoningBlock {
            id: "r0".to_string(),
            text: "checking the ls tool".to_string(),
        }]
    );
    assert_eq!(turn.tools.len(), 1);
    assert!(turn.tools[0].done);
    assert_eq!(turn.text, "done");
}

#[test]
fn a_second_turn_opens_a_new_group() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("a"));
    state.reduce(delta("first"));
    state.reduce(event(SessionEvent::ToolCall {
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
    assert_eq!(turns[1].text, "second");
    assert_eq!(turns[0].tools.len(), 1);
}

#[test]
fn turn_retried_drops_the_provisional_turn() {
    let mut state = GuiState::default();
    state.reduce(ack());
    state.reduce(user("a"));
    state.reduce(delta("poisoned"));
    state.reduce(event(SessionEvent::TurnRetried { turn: 1 }));
    assert!(matches!(state.transcript.last(), Some(Group::User { .. })));
    state.reduce(delta("fixed"));
    assert_eq!(last_turn(&state).text, "fixed");
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
    assert_eq!(last_turn(&state).text, "partial");
}

#[test]
fn duplicate_texts_pair_by_fifo() {
    // v1 heuristic: two identical messages pair in order.
    let mut state = GuiState::default();
    state.reduce(ack());
    state.message_sent("same".to_string());
    state.message_sent("same".to_string());
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
fn rejection_ends_the_connection() {
    let mut state = GuiState::default();
    state.reduce(InMsg::Rejected("version mismatch".to_string()));
    assert!(matches!(state.phase, Phase::Exited { clean: false, .. }));
}
