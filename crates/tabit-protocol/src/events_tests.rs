use super::*;

const TURN: &str = "0192uuidv7turn";

#[test]
fn events_round_trip_through_json() {
    let events = vec![
        SessionEvent::UserMessage {
            text: "hi".to_string(),
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
