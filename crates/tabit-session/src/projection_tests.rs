use super::*;
use crate::entry::{EntryKind, SessionEntry, SideKind, SideRecord};
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
            status: None,
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
fn only_nodes_reach_the_projector() {
    // Format v3 made the old skip-arms unconstructible: state lives in
    // side records, which are not entries at all. Every entry kind the
    // projector can see contributes to the context.
    let entries = vec![entry(user("q")), entry(assistant_text("a"))];
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
fn the_register_reads_the_files_last_model_change_backwards() {
    let records = vec![
        side(SideKind::ModelChange {
            provider: "p".to_string(),
            model: "first".to_string(),
            thinking_level: None,
        }),
        FileRecord::Node(entry(user("q"))),
        side(SideKind::ModelChange {
            provider: "p".to_string(),
            model: "second".to_string(),
            thinking_level: Some("high".to_string()),
        }),
    ];
    let (provider, model, level) =
        last_model_change_in_file(&records).expect("a model change exists");
    assert_eq!((provider, model, level), ("p", "second", Some("high")));
    assert!(
        last_model_change_in_file(&[FileRecord::Node(entry(user("no changes")))]).is_none(),
        "nodes never carry the register"
    );
}

fn side(kind: SideKind) -> FileRecord {
    FileRecord::Side(SideRecord {
        timestamp: "t".to_string(),
        kind,
    })
}

#[test]
fn assistant_entry_holding_a_non_assistant_message_is_not_dangling() {
    // The entry schema permits any Message inside AssistantMessage; only a
    // genuine assistant message can dangle tool calls.
    let entries = vec![entry(EntryKind::AssistantMessage {
        message: Message::User {
            content: OneOrMany::one(UserContent::Text(Text::new("odd but legal"))),
        },
        usage: rig_core::completion::Usage::default(),
    })];
    let (messages, dangling) = project(&entries);
    assert_eq!(messages.len(), 1);
    assert!(dangling.is_none());
}

#[test]
fn partially_answered_batch_dangles_only_the_unanswered_call() {
    // A branch point can land mid-batch: some results arrived, one call
    // was never answered. Only the unanswered call dangles.
    let entries = vec![
        entry(user("q")),
        entry(assistant_tool_calls(&["c1".to_string(), "c2".to_string()])),
        entry(tool_result("c1")),
    ];
    let (messages, dangling) = project(&entries);
    let dangling = dangling.expect("c2 was never answered");
    assert_eq!(dangling.calls.len(), 1);
    assert_eq!(dangling.calls[0].id, "c2");
    assert_eq!(messages.len(), 3, "user, assistant, the one result");
}

#[test]
fn results_answer_calls_by_provider_call_id_too() {
    // Providers that correlate by call_id (OpenAI-style) answer calls
    // whose canonical id never appears in any result.
    let mut call = ToolCall::new(
        "internal-1".to_string(),
        ToolFunction::new("echo".to_string(), json!({})),
    );
    call.call_id = Some("call-abc".to_string());
    let entries = vec![
        entry(user("q")),
        entry(EntryKind::AssistantMessage {
            message: Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::ToolCall(call)),
            },
            usage: rig_core::completion::Usage::default(),
        }),
        entry(EntryKind::ToolResult {
            result: ToolResult {
                id: "unrelated".to_string(),
                call_id: Some("call-abc".to_string()),
                content: OneOrMany::one(ToolResultContent::text("ok")),
                status: None,
            },
        }),
    ];
    let (_, dangling) = project(&entries);
    assert!(dangling.is_none(), "the call_id answered the call");
}

#[test]
fn a_projected_branch_is_all_context() {
    // v3: the projector sees the active branch's nodes only; the
    // checkout that selected the branch is a side record that never
    // reaches this function.
    let entries = vec![
        entry(user("q")),
        entry(assistant_text("a")),
        entry(user("again")),
        entry(assistant_text("b")),
    ];
    let (messages, dangling) = project(&entries);
    assert!(dangling.is_none());
    assert_eq!(messages.len(), 4);
}

#[test]
fn user_message_boundaries_list_every_user_message_in_order() {
    // Prompts and steers are both UserMessage entries — both are valid
    // rewind targets.
    let entries = vec![
        entry(user("first")),
        entry(assistant_text("a")),
        entry(user("second")),
        entry(assistant_tool_calls(&["c1".to_string()])),
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
