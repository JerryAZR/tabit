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

fn tool_result_entry() -> EntryKind {
    EntryKind::ToolResult {
        result: ToolResult {
            id: "call-1".to_string(),
            call_id: None,
            content: OneOrMany::one(ToolResultContent::text("ok")),
            status: None,
        },
    }
}

#[test]
fn node_kinds_round_trip() {
    for kind in [
        user_entry(),
        assistant_entry_with_tool_call(),
        tool_result_entry(),
    ] {
        let entry = SessionEntry::new(
            Some("parent".to_string()),
            "2026-08-15T00:00:00Z".into(),
            kind,
        );
        let line = serde_json::to_string(&entry).expect("entry serializes");
        // A node line parses back as a node record.
        let back: FileRecord = serde_json::from_str(&line).expect("record parses back");
        let FileRecord::Node(back_entry) = back else {
            panic!("node kinds stay nodes: {line}");
        };
        let back_line = serde_json::to_string(&back_entry).expect("back serializes");
        assert_eq!(back_line, line);
    }
}

#[test]
fn side_kinds_round_trip_without_ids_or_parents() {
    let kinds = vec![
        SideKind::ModelChange {
            provider: "p".to_string(),
            model: "m".to_string(),
            thinking_level: Some("high".to_string()),
        },
        SideKind::Checkout {
            to: Some("0197-node".to_string()),
        },
        SideKind::Checkout { to: None },
        SideKind::Aborted,
        SideKind::Label {
            name: "before-refactor".to_string(),
        },
        SideKind::Custom {
            data: json!({"any": true}),
        },
    ];
    for kind in kinds {
        let record = SideRecord {
            timestamp: "2026-08-15T00:00:00Z".to_string(),
            kind,
        };
        let line = serde_json::to_string(&record).expect("record serializes");
        assert!(!line.contains("\"id\""), "side records carry no id: {line}");
        assert!(
            !line.contains("\"parent_id\""),
            "side records carry no parent: {line}"
        );
        let back: FileRecord = serde_json::from_str(&line).expect("record parses back");
        let FileRecord::Side(side) = back else {
            panic!("side kinds stay side records: {line}");
        };
        let back_line = serde_json::to_string(&side).expect("back serializes");
        assert_eq!(back_line, line);
    }
}

#[test]
fn unknown_node_kind_fails_loudly() {
    let raw = r#"{"id":"a","parent_id":null,"timestamp":"t","kind":"from_the_future":{"x":1}}"#;
    let result = serde_json::from_str::<SessionEntry>(raw);
    assert!(result.is_err());
    // And through the untagged record shape: no variant fits.
    assert!(serde_json::from_str::<FileRecord>(raw).is_err());
}

#[test]
fn unknown_side_kind_fails_loudly() {
    let raw = r#"{"timestamp":"t","kind":"from_the_future":{"x":1}}"#;
    let result = serde_json::from_str::<SideRecord>(raw);
    assert!(result.is_err());
}

#[test]
fn unknown_entry_fields_are_rejected() {
    let raw = r#"{"id":"a","parent_id":null,"timestamp":"t","kind":"user_message","message":{"role":"user","content":"x"},"extra":1}"#;
    assert!(serde_json::from_str::<SessionEntry>(raw).is_err());
    let raw = r#"{"timestamp":"t","kind":"aborted","extra":1}"#;
    assert!(serde_json::from_str::<SideRecord>(raw).is_err());
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
    assert_eq!(SESSION_FORMAT_VERSION, 3);
    let line = serde_json::to_string(&header).expect("header serializes");
    let back: SessionHeader = serde_json::from_str(&line).expect("header parses back");
    assert_eq!(back, header);
    assert!(
        serde_json::from_str::<SessionHeader>(
            r#"{"version":3,"id":"x","created_at":"t","cwd":"c","bogus":true}"#
        )
        .is_err()
    );
}
