//! Node→context projection and the tail closedness check.

use super::*;
use crate::entry::{EntryKind, SessionEntry};
use serde_json::json;
use tabit_providers::OneOrMany;
use tabit_providers::completion::Message;
use tabit_providers::message::{
    AssistantContent, Text, ToolCall, ToolFunction, ToolResult, ToolResultContent, UserContent,
};

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
        usage: tabit_providers::completion::Usage::default(),
        delta_tokens: None,
        cost: None,
    }
}

fn assistant_text(text: &str) -> EntryKind {
    EntryKind::AssistantMessage {
        message: Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::text(text)),
        },
        usage: tabit_providers::completion::Usage::default(),
        delta_tokens: None,
        cost: None,
    }
}

fn tool_result(id: &str) -> EntryKind {
    EntryKind::ToolResult {
        result: ToolResult {
            id: id.to_string(),
            call_id: None,
            details: None,
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

/// The attachments ruling: images ride compaction. The fold — which IS
/// the compaction request's history (tabit-session's pass builds its
/// view from `fold_branch`) — carries a multi-part user message
/// verbatim, image parts included.
#[test]
fn fold_branch_carries_a_multi_part_user_message_verbatim() {
    use tabit_providers::message::ImageMediaType;
    let attached = EntryKind::UserMessage {
        message: Message::User {
            content: OneOrMany::many(vec![
                UserContent::text("see <attachment path=\"/tmp/shot.png\"/>"),
                UserContent::text("\n\nshot.png"),
                UserContent::image_base64("aGVsbG8=", Some(ImageMediaType::PNG), None),
            ])
            .expect("three parts"),
        },
    };
    let entries = vec![entry(attached), entry(assistant_text("a"))];
    let messages = fold_branch(&entries);
    assert_eq!(messages.len(), 2);
    let Message::User { content } = &messages[0] else {
        panic!("a user message");
    };
    assert_eq!(content.len(), 3, "every part survived the fold");
    assert!(matches!(
        &content.iter().last(),
        Some(UserContent::Image(_))
    ));
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
    assert!(tail_is_closed(&entries).is_ok());
}

#[test]
fn a_complete_roundtrip_at_the_tail_passes() {
    let entries = vec![
        entry(user("q")),
        entry(assistant_tool_calls(&["c1", "c2"])),
        entry(tool_result("c1")),
        entry(tool_result("c2")),
    ];
    assert!(tail_is_closed(&entries).is_ok());
}

#[test]
fn a_branch_ending_mid_batch_is_open() {
    let entries = vec![
        entry(user("q")),
        entry(assistant_tool_calls(&["c1", "c2"])),
        entry(tool_result("c1")),
    ];
    let fault = tail_is_closed(&entries).expect_err("c2 unanswered");
    assert!(fault.contains("unanswered"), "{fault}");
}

#[test]
fn a_branch_ending_on_a_calls_assistant_is_open() {
    let entries = vec![entry(user("q")), entry(assistant_tool_calls(&["c1"]))];
    let fault = tail_is_closed(&entries).expect_err("calls never answered");
    assert!(fault.contains("unanswered"), "{fault}");
}

#[test]
fn a_tail_result_without_its_assistant_is_open() {
    let entries = vec![entry(user("q")), entry(tool_result("ghost"))];
    let fault = tail_is_closed(&entries).expect_err("result behind a user message");
    assert!(fault.contains("not their assistant"), "{fault}");
}

#[test]
fn a_tail_result_answering_no_open_call_is_open() {
    let entries = vec![
        entry(user("q")),
        entry(assistant_tool_calls(&["c1"])),
        entry(tool_result("ghost")),
    ];
    let fault = tail_is_closed(&entries).expect_err("orphan result in the tail run");
    assert!(fault.contains("no open call"), "{fault}");
}

#[test]
fn a_result_run_with_nothing_behind_it_is_open() {
    let entries = vec![entry(tool_result("c1"))];
    let fault = tail_is_closed(&entries).expect_err("results cannot start a path");
    assert!(fault.contains("no assistant behind them"), "{fault}");
}

#[test]
fn a_non_assistant_message_carries_no_calls() {
    // The entry schema permits any Message inside AssistantMessage; only a
    // genuine assistant message can carry tool calls.
    let entries = vec![entry(EntryKind::AssistantMessage {
        message: Message::User {
            content: OneOrMany::one(UserContent::Text(Text::new("odd but legal"))),
        },
        usage: tabit_providers::completion::Usage::default(),
        delta_tokens: None,
        cost: None,
    })];
    assert!(tail_is_closed(&entries).is_ok());
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

fn compaction(summary: &str) -> EntryKind {
    EntryKind::Compaction {
        summary: summary.to_string(),
        cut_child: "irrelevant-to-the-fold".to_string(),
        tokens_before: 0,
        tokens_after: 0,
        usage: tabit_providers::completion::Usage::default(),
        cost: None,
    }
}

#[test]
fn a_compaction_node_truncates_the_prefix_and_wraps_the_summary() {
    let folded = fold_branch(&[
        entry(user("first question")),
        entry(assistant_text("first answer")),
        entry(compaction("the summary")),
        entry(user("after the cut")),
        entry(assistant_text("after answer")),
    ]);
    // The walked context is [summary] + tail — nothing before the
    // insertion survives, and the summary enters as a user-role
    // wrapped message.
    assert_eq!(folded.len(), 3);
    let Message::User { content } = &folded[0] else {
        panic!("the summary enters as a user message");
    };
    let UserContent::Text(text) = content.first() else {
        panic!("a text part");
    };
    assert!(text.text.contains("the summary"));
    assert!(
        text.text
            .starts_with(crate::fold::COMPACTION_SUMMARY_PREFIX.trim())
    );
    assert!(
        text.text
            .ends_with(crate::fold::COMPACTION_SUMMARY_SUFFIX.trim())
    );
}

#[test]
fn only_the_last_compaction_on_a_path_survives() {
    let folded = fold_branch(&[
        entry(user("old")),
        entry(compaction("first summary")),
        entry(user("middle")),
        entry(compaction("second summary")),
        entry(user("tail")),
    ]);
    let Message::User { content } = &folded[0] else {
        panic!("user message first");
    };
    let UserContent::Text(text) = content.first() else {
        panic!("a text part");
    };
    assert!(text.text.contains("second summary"));
    assert!(!text.text.contains("first summary"));
    assert_eq!(folded.len(), 2, "the summary plus the tail: {folded:?}");
}

#[test]
fn a_compaction_boundary_is_closed_for_the_tail_check() {
    // A path ending AT a compaction node (a checkout target) is a
    // legal, closed branch.
    let path = vec![
        entry(user("q")),
        entry(assistant_text("a")),
        entry(compaction("s")),
    ];
    assert_eq!(tail_is_closed(&path), Ok(()));
}

#[test]
fn an_empty_path_is_trivially_closed() {
    assert_eq!(super::tail_is_closed(&[]), Ok(()));
}
