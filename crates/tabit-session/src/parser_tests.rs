//! The one-pass load: what it produces, and everything it rejects.

use super::*;
use crate::entry::{EntryKind, SessionEntry, SideKind, SideRecord};
use rig_core::OneOrMany;
use rig_core::completion::{Message, Usage};
use rig_core::message::{ToolCall, ToolFunction, ToolResult, ToolResultContent};

fn user_node(id: &str, parent: Option<&str>, text: &str) -> FileRecord {
    FileRecord::Node(SessionEntry::with_id(
        id.to_string(),
        parent.map(str::to_string),
        "t".to_string(),
        EntryKind::UserMessage {
            message: Message::user(text),
        },
    ))
}

fn assistant_node(id: &str, parent: Option<&str>, calls: &[(&str, &str)]) -> FileRecord {
    let content = if calls.is_empty() {
        OneOrMany::one(rig_core::message::AssistantContent::text("done"))
    } else {
        OneOrMany::many(
            calls
                .iter()
                .map(|(call_id, name)| {
                    rig_core::message::AssistantContent::ToolCall(ToolCall::new(
                        call_id.to_string(),
                        ToolFunction::new(name.to_string(), serde_json::json!({})),
                    ))
                })
                .collect::<Vec<_>>(),
        )
        .expect("non-empty")
    };
    FileRecord::Node(SessionEntry::with_id(
        id.to_string(),
        parent.map(str::to_string),
        "t".to_string(),
        EntryKind::AssistantMessage {
            message: Message::Assistant { id: None, content },
            usage: Usage::default(),
        },
    ))
}

fn result_node(id: &str, parent: Option<&str>, call_id: &str) -> FileRecord {
    FileRecord::Node(SessionEntry::with_id(
        id.to_string(),
        parent.map(str::to_string),
        "t".to_string(),
        EntryKind::ToolResult {
            result: ToolResult {
                id: call_id.to_string(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::text("ok")),
                status: None,
            },
        },
    ))
}

fn side(kind: SideKind) -> FileRecord {
    FileRecord::Side(SideRecord {
        timestamp: "t".to_string(),
        kind,
    })
}

fn model_change(provider: &str, model: &str) -> SideKind {
    SideKind::ModelChange {
        provider: provider.to_string(),
        model: model.to_string(),
        thinking_level: None,
    }
}

/// Serialize records into a file body (header first).
fn body(records: &[FileRecord]) -> String {
    let header = serde_json::json!({
        "version": crate::entry::SESSION_FORMAT_VERSION,
        "id": "sid",
        "created_at": "t",
        "cwd": "C:/w",
    });
    let mut out = serde_json::to_string(&header).expect("header");
    for record in records {
        out.push('\n');
        out.push_str(&serde_json::to_string(record).expect("record"));
    }
    out
}

fn parse_ok(records: &[FileRecord]) -> Parsed {
    parse(&body(records), Path::new("test.jsonl")).expect("parse")
}

fn parse_err(records: &[FileRecord]) -> SessionError {
    parse(&body(records), Path::new("test.jsonl")).expect_err("must fail")
}

#[test]
fn a_clean_session_parses_into_tree_context_register_and_stats() {
    let parsed = parse_ok(&[
        side(model_change("p", "m1")),
        user_node("u1", None, "hello"),
        assistant_node("a1", Some("u1"), &[("c1", "read")]),
        result_node("r1", Some("a1"), "c1"),
        user_node("u2", Some("r1"), "again"),
    ]);
    assert_eq!(
        parsed.register.as_ref().map(|r| r.model.clone()),
        Some("m1".into())
    );
    assert_eq!(parsed.tree.head(), Some("u2"));
    // The context is derived, never parsed: the manager's view.
    let messages = crate::context_manager::ContextManager::from_tree(
        parsed.tree.clone(),
        std::sync::Arc::new(std::sync::Mutex::new(tabit_log::NullBuffer)),
    )
    .messages();
    assert_eq!(messages.len(), 4);
    assert!(matches!(&messages[2], Message::User { content } if content.len() == 1));
    assert_eq!(parsed.stats.total_usage().total_tokens, 0);
    assert!(parsed.file_len > 0);
}

#[test]
fn consecutive_results_merge_into_one_user_message() {
    let parsed = parse_ok(&[
        user_node("u1", None, "go"),
        assistant_node("a1", Some("u1"), &[("c1", "a"), ("c2", "b")]),
        result_node("r1", Some("a1"), "c1"),
        result_node("r2", Some("r1"), "c2"),
    ]);
    let messages = crate::context_manager::ContextManager::from_tree(
        parsed.tree.clone(),
        std::sync::Arc::new(std::sync::Mutex::new(tabit_log::NullBuffer)),
    )
    .messages();
    assert_eq!(messages.len(), 3, "user + assistant + ONE merged batch");
    let Message::User { content } = &messages[2] else {
        panic!("the batch folds as one user message");
    };
    assert_eq!(content.len(), 2, "both results inside it");
}

#[test]
fn usage_counts_all_branches_and_discarded_attempts() {
    let mut assistant = assistant_node("a1", Some("u1"), &[]);
    if let FileRecord::Node(entry) = &mut assistant
        && let EntryKind::AssistantMessage { usage, .. } = &mut entry.kind
    {
        usage.total_tokens = 10;
    }
    let mut discarded_usage = Usage::new();
    discarded_usage.total_tokens = 4;
    let parsed = parse_ok(&[
        side(model_change("p", "m1")),
        user_node("u1", None, "go"),
        assistant,
        side(SideKind::Discarded {
            usage: discarded_usage,
        }),
    ]);
    assert_eq!(parsed.stats.total_usage().total_tokens, 14);
    assert_eq!(parsed.stats.per_model().len(), 1);
    assert_eq!(parsed.stats.per_model()[0].usage.total_tokens, 14);
}

#[test]
fn a_torn_tail_fails_loud_with_its_line() {
    let mut raw = body(&[user_node("u1", None, "go")]);
    raw.push_str("\n{\"id\":\"tor"); // a crash mid-append
    match parse(&raw, Path::new("t.jsonl")) {
        Err(SessionError::Parse { line, .. }) => assert_eq!(line, 3),
        other => panic!("torn tail must fail loud, got {other:?}"),
    }
}

#[test]
fn a_trailing_open_batch_is_corruption() {
    let err = parse_err(&[
        user_node("u1", None, "go"),
        assistant_node("a1", Some("u1"), &[("c1", "read")]),
        result_node("r1", Some("a1"), "c1"),
        assistant_node("a2", Some("r1"), &[("c2", "read")]),
        // c2 never answered.
    ]);
    assert!(err.to_string().contains("unanswered"), "{err}");
}

#[test]
fn a_result_without_its_assistant_is_corruption() {
    let err = parse_err(&[
        user_node("u1", None, "go"),
        result_node("r1", Some("u1"), "ghost"),
    ]);
    assert!(err.to_string().contains("not their assistant"), "{err}");
}

#[test]
fn a_broken_parent_link_is_corruption() {
    let err = parse_err(&[
        user_node("u1", None, "go"),
        user_node("u2", Some("ghost"), "orphan"),
    ]);
    assert!(err.to_string().contains("parents"), "{err}");
}

#[test]
fn a_checkout_to_an_unknown_node_is_corruption() {
    let err = parse_err(&[
        user_node("u1", None, "go"),
        side(SideKind::Checkout {
            to: Some("ghost".to_string()),
        }),
    ]);
    assert!(err.to_string().contains("unknown or later node"), "{err}");
}

#[test]
fn a_future_format_version_is_rejected() {
    let raw = body(&[user_node("u1", None, "go")]);
    let mut header: serde_json::Value =
        serde_json::from_str(raw.lines().next().unwrap_or("")).expect("header");
    header["version"] = serde_json::Value::Number(99u32.into());
    let rest = raw.split_once('\n').map(|(_, r)| r).unwrap_or("");
    let raw = format!("{header}\n{rest}");
    match parse(&raw, Path::new("t.jsonl")) {
        Err(SessionError::Corrupt { message, .. }) => {
            assert!(message.contains("version 99"), "{message}")
        }
        other => panic!("expected version error, got {other:?}"),
    }
}

#[test]
fn an_empty_file_is_a_parse_error() {
    match parse("", Path::new("t.jsonl")) {
        Err(SessionError::Parse { line, .. }) => assert_eq!(line, 1),
        other => panic!("empty files fail loud, got {other:?}"),
    }
}

#[test]
fn branch_switching_via_checkout_rebuilds_head_and_context() {
    let parsed = parse_ok(&[
        side(model_change("p", "m")),
        user_node("u1", None, "one"),
        assistant_node("a1", Some("u1"), &[]),
        user_node("u2", Some("a1"), "two"),
        side(SideKind::Checkout {
            to: Some("a1".to_string()),
        }),
        user_node("u3", Some("a1"), "three"),
    ]);
    assert_eq!(parsed.tree.head(), Some("u3"));
    let texts: Vec<String> = crate::context_manager::ContextManager::from_tree(
        parsed.tree.clone(),
        std::sync::Arc::new(std::sync::Mutex::new(tabit_log::NullBuffer)),
    )
    .messages()
    .iter()
    .filter_map(|m| m.user_text())
    .collect();
    assert_eq!(texts, ["one", "three"], "the active branch only");
    // The abandoned node stays in the tree.
    assert!(parsed.tree.contains("u2"));
}

#[test]
fn a_v4_file_with_a_compaction_entry_derives_the_insertion() {
    let parsed = parse_ok(&[
        user_node("u1", None, "hello"),
        assistant_node("a1", Some("u1"), &[]),
        user_node("u2", Some("a1"), "again"),
        assistant_node("a2", Some("u2"), &[]),
        compaction_node("x1", Some("a1"), "u2"),
    ]);
    // The head does not move; the walked path routes through the
    // insertion.
    assert_eq!(parsed.tree.head(), Some("a2"));
    assert_eq!(
        parsed
            .tree
            .path_to_head()
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["u1", "a1", "x1", "u2", "a2"]
    );
    // The derived context is [summary] + tail.
    let messages = crate::context_manager::ContextManager::from_tree(
        parsed.tree.clone(),
        std::sync::Arc::new(std::sync::Mutex::new(tabit_log::NullBuffer)),
    )
    .messages();
    assert_eq!(messages.len(), 3, "summary + u2 + a2: {messages:?}");
}

#[test]
fn a_v3_file_still_loads() {
    let mut body = body(&[user_node("u1", None, "hello")]);
    // Rewrite the header's version to 3 — a v3 file is a subset of
    // the v4 vocabulary by construction.
    body = body.replace(
        &format!("\"version\":{}", crate::entry::SESSION_FORMAT_VERSION),
        "\"version\":3",
    );
    let parsed = parse(&body, Path::new("test.jsonl")).expect("v3 loads");
    assert_eq!(parsed.tree.head(), Some("u1"));
}

#[test]
fn a_v2_file_is_rejected() {
    let mut body = body(&[]);
    body = body.replace(
        &format!("\"version\":{}", crate::entry::SESSION_FORMAT_VERSION),
        "\"version\":2",
    );
    let error = parse(&body, Path::new("test.jsonl")).expect_err("v2 rejected");
    assert!(error.to_string().contains("unsupported session format"));
}

#[test]
fn a_mismatched_compaction_entry_fails_loud() {
    // The compaction names u2 as its cut child but parents a1's
    // sibling u1 — the edge it claims does not exist.
    let error = parse_err(&[
        user_node("u1", None, "hello"),
        assistant_node("a1", Some("u1"), &[]),
        user_node("u2", Some("a1"), "again"),
        compaction_node("x1", Some("u1"), "u2"),
    ]);
    assert!(error.to_string().contains("cut child"), "{error}");
}

#[test]
fn a_compaction_entry_carrying_usage_counts_into_stats() {
    let parsed = parse_ok(&[
        user_node("u1", None, "hello"),
        assistant_node("a1", Some("u1"), &[]),
        FileRecord::Node(SessionEntry::with_id(
            "x1".to_string(),
            Some("u1".to_string()),
            "t".to_string(),
            EntryKind::Compaction {
                summary: "s".to_string(),
                cut_child: "a1".to_string(),
                tokens_before: 0,
                usage: rig_core::completion::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                },
            },
        )),
    ]);
    assert_eq!(
        parsed.stats.total_usage().input_tokens + parsed.stats.total_usage().output_tokens,
        15
    );
}

fn compaction_node(id: &str, parent: Option<&str>, cut_child: &str) -> FileRecord {
    FileRecord::Node(SessionEntry::with_id(
        id.to_string(),
        parent.map(str::to_string),
        "t".to_string(),
        EntryKind::Compaction {
            summary: "summarized history".to_string(),
            cut_child: cut_child.to_string(),
            tokens_before: 42,
            usage: rig_core::completion::Usage::default(),
        },
    ))
}
