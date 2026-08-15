use super::*;
use crate::entry::{EntryKind, SessionEntry};
use rig_core::OneOrMany;
use rig_core::completion::Message;
use rig_core::message::{
    AssistantContent, Text, ToolCall, ToolFunction, ToolResult, ToolResultContent, UserContent,
};
use serde_json::json;

fn entry(kind: EntryKind) -> SessionEntry {
    SessionEntry::new(None, "t".to_string(), kind)
}

fn user(text: &str) -> EntryKind {
    EntryKind::UserMessage {
        message: Message::User {
            content: OneOrMany::one(UserContent::Text(Text::new(text))),
        },
    }
}

fn assistant_tool_call(id: &str) -> EntryKind {
    assistant_tool_calls(&[id.to_string()])
}

fn assistant_tool_calls(ids: &[String]) -> EntryKind {
    let content = OneOrMany::many(
        ids.iter()
            .map(|id| {
                AssistantContent::ToolCall(ToolCall::new(
                    id.clone(),
                    ToolFunction::new("echo".to_string(), json!({})),
                ))
            })
            .collect::<Vec<_>>(),
    )
    .expect("non-empty");
    EntryKind::AssistantMessage {
        message: Message::Assistant { id: None, content },
        usage: rig_core::completion::Usage::default(),
    }
}

fn assistant_text(text: &str) -> EntryKind {
    EntryKind::AssistantMessage {
        message: Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::text(text)),
        },
        usage: rig_core::completion::Usage::default(),
    }
}

fn tool_result(id: &str) -> EntryKind {
    EntryKind::ToolResult {
        result: ToolResult {
            id: id.to_string(),
            call_id: None,
            content: OneOrMany::one(ToolResultContent::text("ok")),
        },
    }
}

#[test]
fn projects_entries_in_order_and_merges_result_batches() {
    let entries = vec![
        entry(user("question")),
        entry(assistant_tool_calls(&["c1".to_string(), "c2".to_string()])),
        entry(tool_result("c1")),
        entry(tool_result("c2")),
        entry(assistant_text("done")),
    ];
    let (messages, dangling) = project(&entries);
    assert!(dangling.is_none());
    assert_eq!(messages.len(), 4, "user, assistant, merged results, final");
    assert!(matches!(&messages[2], Message::User { content } if content.len() == 2));
    assert!(matches!(&messages[3], Message::Assistant { .. }));
}

#[test]
fn state_entries_do_not_reach_the_context() {
    let entries = vec![
        entry(user("q")),
        entry(EntryKind::ModelChange {
            provider: "p".to_string(),
            model: "m".to_string(),
            thinking_level: None,
        }),
        entry(EntryKind::Label {
            name: "bookmark".to_string(),
        }),
        entry(EntryKind::Custom {
            data: json!({"x": 1}),
        }),
        entry(assistant_text("a")),
    ];
    let (messages, _) = project(&entries);
    assert_eq!(messages.len(), 2);
}

#[test]
fn dangling_tool_roundtrip_is_detected() {
    let entries = vec![entry(user("do it")), entry(assistant_tool_call("c1"))];
    let (messages, dangling) = project(&entries);
    let dangling = dangling.expect("trailing tool calls with no results dangle");
    assert_eq!(dangling.calls.len(), 1);
    assert_eq!(dangling.calls[0].id, "c1");
    assert_eq!(messages.len(), 2, "the assistant turn is still projected");
}

#[test]
fn completed_roundtrip_is_not_dangling() {
    let entries = vec![
        entry(user("do it")),
        entry(assistant_tool_call("c1")),
        entry(tool_result("c1")),
        entry(assistant_text("done")),
    ];
    let (_, dangling) = project(&entries);
    assert!(dangling.is_none());
}

#[test]
fn text_only_tail_is_not_dangling() {
    let entries = vec![entry(user("q")), entry(assistant_text("a"))];
    let (_, dangling) = project(&entries);
    assert!(dangling.is_none());
}

#[test]
fn interrupted_results_answer_every_call_explicitly() {
    let entries = vec![
        entry(user("go")),
        entry(assistant_tool_calls(&["c1".to_string(), "c2".to_string()])),
    ];
    let (_, dangling) = project(&entries);
    let dangling = dangling.expect("dangles");
    let results = interrupted_results(&dangling);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].id, "c1");
    assert_eq!(results[1].id, "c2");
    for result in &results {
        let text = result
            .content
            .iter()
            .filter_map(ToolResultContent::as_text)
            .collect::<String>();
        assert!(
            text.to_lowercase().contains("interrupted"),
            "synthetic result must say it was interrupted: {text}"
        );
    }
}

#[test]
fn last_model_change_wins() {
    let entries = vec![
        entry(EntryKind::ModelChange {
            provider: "p".to_string(),
            model: "first".to_string(),
            thinking_level: None,
        }),
        entry(user("q")),
        entry(EntryKind::ModelChange {
            provider: "p".to_string(),
            model: "second".to_string(),
            thinking_level: Some("high".to_string()),
        }),
    ];
    let (provider, model, level) = last_model_change(&entries).expect("a model change exists");
    assert_eq!((provider, model, level), ("p", "second", Some("high")));
    assert!(last_model_change(&[entry(user("no changes"))]).is_none());
}
