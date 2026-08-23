use super::*;

const TURN: &str = "0192uuidv7turn";

#[test]
fn events_round_trip_through_json() {
    let events = vec![
        SessionEvent::UserMessage {
            text: "hi".to_string(),
            entry_id: "0192uuidv7user".to_string(),
        },
        SessionEvent::MessageQueued {
            id: "0192uuidv7queued".to_string(),
            text: "queued while running".to_string(),
        },
        SessionEvent::MessagesDiscarded {
            messages: vec![DiscardedMessage {
                id: "0192uuidv7queued".to_string(),
                text: "queued while running".to_string(),
            }],
        },
        SessionEvent::TurnStarted {
            id: TURN.to_string(),
        },
        SessionEvent::TextDelta {
            turn_id: TURN.to_string(),
            text: "hel".to_string(),
        },
        SessionEvent::ReasoningDelta {
            turn_id: TURN.to_string(),
            id: "r0".to_string(),
            reasoning: "thinking...".to_string(),
        },
        SessionEvent::ToolCall {
            turn_id: TURN.to_string(),
            name: "echo".to_string(),
            call_id: "c1".to_string(),
            internal_call_id: "i1".to_string(),
            arguments: Some("{}".to_string()),
        },
        SessionEvent::TurnCommitted {
            id: TURN.to_string(),
        },
        SessionEvent::ToolResult {
            turn_id: TURN.to_string(),
            entry_id: "0192uuidv7entry".to_string(),
            name: "echo".to_string(),
            internal_call_id: "i1".to_string(),
            content: "0".to_string(),
            status: ToolResultStatus::Success,
        },
        SessionEvent::TurnRetried {
            turn_id: TURN.to_string(),
            turn: 2,
        },
        SessionEvent::CompletionCall {
            turn_id: TURN.to_string(),
            input_tokens: 10,
            output_tokens: 4,
        },
        SessionEvent::TurnTruncated {
            turn_id: TURN.to_string(),
        },
        SessionEvent::RunFinished {
            output: "done".to_string(),
            usage: Usage::default(),
        },
        SessionEvent::RunFailed {
            message: "provider stream ended early".to_string(),
        },
        SessionEvent::error_model("default_model `gone` is not usable"),
        SessionEvent::ReplayStarted { total: 7 },
        SessionEvent::ReplayDone,
        SessionEvent::SessionsAvailable {
            sessions: vec![
                AvailableSession {
                    id: "0197".to_string(),
                    created_at: "2026-08-22T10:00:00Z".to_string(),
                    entry_count: 14,
                },
                AvailableSession {
                    id: "0196".to_string(),
                    created_at: "2026-08-21T09:00:00Z".to_string(),
                    entry_count: 0,
                },
            ],
        },
        SessionEvent::SessionCreated {
            id: "0198".to_string(),
            path: "C:/w/.tabit/sessions/20260822_0198.jsonl".to_string(),
        },
        SessionEvent::error_session("no session with id `0195`"),
        SessionEvent::ModelChanged {
            provider: "p".to_string(),
            model: "m".to_string(),
            thinking_level: Some("high".to_string()),
        },
        SessionEvent::error_persist_degraded(3, "records are pending on disk"),
        SessionEvent::NativeItem {
            turn_id: TURN.to_string(),
            item: serde_json::json!({"web_search_call": {}}),
        },
    ];
    for event in &events {
        let json = serde_json::to_string(event).expect("serialize");
        let back: SessionEvent = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, *event);
    }

    // The error carrier: kind is an open string; kind-specific structure
    // (the pending count) rides only when present.
    assert_eq!(
        serde_json::to_string(&SessionEvent::error_model("stale default_model"))
            .expect("serialize"),
        r#"{"type":"error","kind":"model","message":"stale default_model"}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionEvent::error_persist_degraded(3, "pending"))
            .expect("serialize"),
        r#"{"type":"error","kind":"persist_degraded","message":"pending","pending":3}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionEvent::SessionsAvailable {
            sessions: vec![AvailableSession {
                id: "0197".to_string(),
                created_at: "2026-08-22T10:00:00Z".to_string(),
                entry_count: 14,
            }]
        })
        .expect("serialize"),
        r#"{"type":"sessions_available","sessions":[{"id":"0197","created_at":"2026-08-22T10:00:00Z","entry_count":14}]}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionEvent::SessionCreated {
            id: "0198".to_string(),
            path: "C:/w/s.jsonl".to_string()
        })
        .expect("serialize"),
        r#"{"type":"session_created","id":"0198","path":"C:/w/s.jsonl"}"#
    );
    // The wire spelling of the brackets and the truncation warning.
    assert_eq!(
        serde_json::to_string(&SessionEvent::TurnStarted {
            id: TURN.to_string()
        })
        .expect("serialize"),
        r#"{"type":"turn_started","id":"0192uuidv7turn"}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionEvent::TurnTruncated {
            turn_id: TURN.to_string()
        })
        .expect("serialize"),
        r#"{"type":"turn_truncated","turn_id":"0192uuidv7turn"}"#
    );
}
