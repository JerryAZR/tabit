use super::*;
use rig_core::OneOrMany;
use rig_core::completion::{Message, Usage};
use rig_core::message::{ToolCall, ToolFunction, ToolResult, ToolResultContent};
use serde_json::json;

fn user_entry() -> EntryKind {
    EntryKind::UserMessage {
        message: Message::User {
            content: OneOrMany::one(rig_core::message::UserContent::Text(
                rig_core::message::Text::new("hello"),
            )),
        },
    }
}

fn assistant_entry_with_tool_call() -> EntryKind {
    EntryKind::AssistantMessage {
        message: Message::Assistant {
            id: None,
            content: OneOrMany::one(rig_core::message::AssistantContent::ToolCall(
                ToolCall::new(
                    "call-1".to_string(),
                    ToolFunction::new("echo".to_string(), json!({"v": 1})),
                ),
            )),
        },
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            ..Usage::default()
        },
    }
}

#[test]
fn entry_round_trips_every_kind() {
    let kinds = vec![
        user_entry(),
        assistant_entry_with_tool_call(),
        EntryKind::ToolResult {
            result: ToolResult {
                id: "call-1".to_string(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::text("ok")),
                status: None,
            },
        },
        EntryKind::ModelChange {
            provider: "p".to_string(),
            model: "m".to_string(),
            thinking_level: Some("high".to_string()),
        },
        EntryKind::Label {
            name: "before-refactor".to_string(),
        },
        EntryKind::Custom {
            data: json!({"any": true}),
        },
    ];
    for kind in kinds {
        let entry = SessionEntry::new(
            Some("parent".to_string()),
            "2026-08-15T00:00:00Z".into(),
            kind,
        );
        let line = serde_json::to_string(&entry).expect("entry serializes");
        let back: SessionEntry = serde_json::from_str(&line).expect("entry parses back");
        // Compare serialized forms: rig's `Text` flatten round-trips an
        // absent `additional_params` as `Some({})`, which serializes
        // identically but compares unequal as a struct.
        let back_line = serde_json::to_string(&back).expect("back serializes");
        assert_eq!(back_line, line);
    }
}

#[test]
fn unknown_kind_fails_loudly() {
    let raw = r#"{"id":"a","parent_id":null,"timestamp":"t","kind":"from_the_future":{"x":1}}"#;
    let result = serde_json::from_str::<SessionEntry>(raw);
    assert!(result.is_err());
}

#[test]
fn unknown_entry_fields_are_rejected() {
    let raw = r#"{"id":"a","parent_id":null,"timestamp":"t","kind":"label","name":"x","extra":1}"#;
    assert!(serde_json::from_str::<SessionEntry>(raw).is_err());
}

#[test]
fn header_round_trips_and_rejects_unknown_fields() {
    let header = SessionHeader {
        version: SESSION_FORMAT_VERSION,
        id: "0195c0de-0000-7000-8000-000000000000".to_string(),
        created_at: "2026-08-15T00:00:00Z".to_string(),
        cwd: "C:/work".to_string(),
        parent_session: None,
    };
    let line = serde_json::to_string(&header).expect("header serializes");
    let back: SessionHeader = serde_json::from_str(&line).expect("header parses back");
    assert_eq!(back, header);
    assert!(
        serde_json::from_str::<SessionHeader>(
            r#"{"version":1,"id":"x","created_at":"t","cwd":"c","bogus":true}"#
        )
        .is_err()
    );
}

#[test]
fn context_entry_classification() {
    assert!(user_entry().is_context_entry());
    assert!(assistant_entry_with_tool_call().is_context_entry());
    let result_kind = EntryKind::ToolResult {
        result: ToolResult {
            id: "c".to_string(),
            call_id: None,
            content: OneOrMany::one(ToolResultContent::text("")),
            status: None,
        },
    };
    assert!(result_kind.is_context_entry());
    assert!(
        !EntryKind::Label {
            name: "n".to_string()
        }
        .is_context_entry()
    );
    assert!(
        !EntryKind::ModelChange {
            provider: "p".to_string(),
            model: "m".to_string(),
            thinking_level: None,
        }
        .is_context_entry()
    );
}
