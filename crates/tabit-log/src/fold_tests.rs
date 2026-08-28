//! Node→context projection and the structural checks over branches.

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

fn assistant_tool_calls(ids: &[&str]) -> EntryKind {
    let content = OneOrMany::many(
        ids.iter()
            .map(|id| {
                AssistantContent::ToolCall(ToolCall::new(
                    id.to_string(),
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
            status: None,
        },
    }
}

#[test]
fn fold_branch_merges_result_batches_into_one_user_message() {
    let entries = vec![
        entry(user("question")),
        entry(assistant_tool_calls(&["c1", "c2"])),
        entry(tool_result("c1")),
        entry(tool_result("c2")),
        entry(assistant_text("done")),
    ];
    let messages = fold_branch(&entries);
    assert_eq!(messages.len(), 4, "user, assistant, merged results, final");
    assert!(matches!(&messages[2], Message::User { content } if content.len() == 2));
    assert!(matches!(&messages[3], Message::Assistant { .. }));
}

#[test]
fn fold_branch_folds_user_and_assistant_messages_verbatim() {
    let entries = vec![entry(user("q")), entry(assistant_text("a"))];
    let messages = fold_branch(&entries);
    assert_eq!(messages.len(), 2);
    assert!(matches!(&messages[0], Message::User { .. }));
    assert!(matches!(&messages[1], Message::Assistant { .. }));
}

#[test]
fn a_closed_branch_passes() {
    let entries = vec![
        entry(user("q")),
        entry(assistant_tool_calls(&["c1", "c2"])),
        entry(tool_result("c1")),
        entry(tool_result("c2")),
        entry(user("again")),
    ];
    assert!(path_is_closed(&entries).is_ok());
}

#[test]
fn a_branch_ending_mid_batch_is_open() {
    let entries = vec![
        entry(user("q")),
        entry(assistant_tool_calls(&["c1", "c2"])),
        entry(tool_result("c1")),
    ];
    let fault = path_is_closed(&entries).expect_err("c2 unanswered");
    assert!(fault.contains("unanswered"), "{fault}");
}

#[test]
fn a_branch_ending_on_a_calls_assistant_is_open() {
    let entries = vec![entry(user("q")), entry(assistant_tool_calls(&["c1"]))];
    let fault = path_is_closed(&entries).expect_err("calls never answered");
    assert!(fault.contains("mid-roundtrip"), "{fault}");
}

#[test]
fn a_plain_user_message_after_an_open_batch_is_open() {
    let entries = vec![
        entry(user("q")),
        entry(assistant_tool_calls(&["c1"])),
        entry(user("interrupts")),
    ];
    let fault = path_is_closed(&entries).expect_err("mid-batch user message");
    assert!(fault.contains("interrupts"), "{fault}");
}

#[test]
fn a_result_answering_no_open_call_is_open() {
    let entries = vec![entry(user("q")), entry(tool_result("ghost"))];
    let fault = path_is_closed(&entries).expect_err("orphan result");
    assert!(fault.contains("no open call"), "{fault}");
}

#[test]
fn an_assistant_after_an_open_batch_is_open() {
    let entries = vec![
        entry(user("q")),
        entry(assistant_tool_calls(&["c1"])),
        entry(assistant_text("too early")),
    ];
    let fault = path_is_closed(&entries).expect_err("batch left open");
    assert!(fault.contains("open"), "{fault}");
}

#[test]
fn a_non_assistant_message_carries_no_calls() {
    // The entry schema permits any Message inside AssistantMessage; only a
    // genuine assistant message can carry tool calls.
    let entries = vec![entry(EntryKind::AssistantMessage {
        message: Message::User {
            content: OneOrMany::one(UserContent::Text(Text::new("odd but legal"))),
        },
        usage: rig_core::completion::Usage::default(),
    })];
    assert!(path_is_closed(&entries).is_ok());
    assert_eq!(calls_of(&Message::user("x")).len(), 0);
}

#[test]
fn user_message_boundaries_list_every_user_message_in_order() {
    // Prompts and steers are both UserMessage entries — both are valid
    // rewind targets.
    let entries = vec![
        entry(user("first")),
        entry(assistant_text("a")),
        entry(user("second")),
        entry(assistant_tool_calls(&["c1"])),
        entry(tool_result("c1")),
        entry(user("a steer mid-run")),
        entry(assistant_text("b")),
    ];
    let boundaries = user_message_boundaries(&entries);
    let texts: Vec<String> = boundaries
        .iter()
        .map(|entry| match &entry.kind {
            EntryKind::UserMessage {
                message: Message::User { content },
            } => content
                .iter()
                .filter_map(|part| match part {
                    UserContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect(),
            _ => String::new(),
        })
        .collect();
    assert_eq!(texts, vec!["first", "second", "a steer mid-run"]);
}
