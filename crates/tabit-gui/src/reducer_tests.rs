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
    event(SessionEvent::UserMessage {
        text: text.to_string(),
    })
}

fn delta(text: &str) -> InMsg {
    event(SessionEvent::TextDelta {
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
        name: "ls".to_string(),
        call_id: "c".to_string(),
        internal_call_id: "i".to_string(),
        arguments: None,
    }));
    state.reduce(event(SessionEvent::ToolResult {
        name: "ls".to_string(),
        internal_call_id: "i".to_string(),
    }));
    state.reduce(delta("Found three files."));
    state.reduce(event(SessionEvent::ToolCall {
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
    state.reduce(event(SessionEvent::TurnRetried { turn: 1 }));
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
