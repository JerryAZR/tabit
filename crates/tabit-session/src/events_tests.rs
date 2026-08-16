use super::*;

#[test]
fn events_round_trip_through_json() {
    let events = vec![
        SessionEvent::UserMessage {
            text: "hi".to_string(),
        },
        SessionEvent::TextDelta {
            text: "hel".to_string(),
        },
        SessionEvent::ReasoningDelta {
            id: "r0".to_string(),
            reasoning: "thinking...".to_string(),
        },
        SessionEvent::ToolCall {
            name: "echo".to_string(),
            call_id: "c1".to_string(),
            internal_call_id: "i1".to_string(),
            arguments: Some("{}".to_string()),
        },
        SessionEvent::ToolResult {
            name: "echo".to_string(),
            internal_call_id: "i1".to_string(),
        },
        SessionEvent::TurnRetried { turn: 2 },
        SessionEvent::CompletionCall {
            input_tokens: 10,
            output_tokens: 4,
        },
        SessionEvent::RunFinished {
            output: "done".to_string(),
            usage: rig_core::completion::Usage::default(),
        },
        SessionEvent::RunFailed {
            message: "provider stream ended early".to_string(),
        },
        SessionEvent::NativeItem {
            item: serde_json::json!({"web_search_call": {}}),
        },
    ];
    for event in &events {
        let json = serde_json::to_string(event).expect("serialize");
        let back: SessionEvent = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, *event);
    }
}
