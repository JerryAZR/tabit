//! The context's contract suite: it keeps committed messages, in
//! order, untouched — and does nothing else.

use super::*;
use rig_core::OneOrMany;
use rig_core::message::{ToolCall, ToolFunction, ToolResult, ToolResultContent, UserContent};

fn user(text: &str) -> Message {
    Message::user(text)
}

fn assistant(text: &str) -> Message {
    Message::Assistant {
        id: None,
        content: OneOrMany::one(rig_core::message::AssistantContent::text(text)),
    }
}

fn tool_results_message() -> Message {
    Message::User {
        content: OneOrMany::many(vec![
            UserContent::ToolResult(ToolResult {
                id: "c1".to_string(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::text("one")),
                status: None,
            }),
            UserContent::ToolResult(ToolResult {
                id: "c2".to_string(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::text("two")),
                status: None,
            }),
        ])
        .expect("non-empty"),
    }
}

#[test]
fn folds_keep_messages_in_order() {
    let mut context = Context::new();
    context.fold(user("q"));
    context.fold(assistant("a"));
    context.fold(user("again"));
    let messages = context.messages();
    assert_eq!(messages.len(), 3);
    assert!(matches!(&messages[0], Message::User { .. }));
    assert!(matches!(&messages[1], Message::Assistant { .. }));
    assert!(matches!(&messages[2], Message::User { .. }));
}

#[test]
fn a_batch_folds_as_one_ordered_extension() {
    let mut context = Context::new();
    context.fold(user("seed"));
    context.fold_all(vec![assistant("a"), user("next")]);
    assert_eq!(context.messages().len(), 3);
    // An empty batch is a no-op.
    context.fold_all(Vec::new());
    assert_eq!(context.messages().len(), 3);
}

#[test]
fn seeding_is_a_new_context_plus_one_batch() {
    let seed = vec![user("one"), assistant("two")];
    let mut seeded = Context::new();
    seeded.fold_all(seed.clone());
    assert_eq!(seeded.messages(), seed.as_slice());
}

#[test]
fn into_messages_hands_the_list_over() {
    let mut context = Context::new();
    context.fold(user("only"));
    let messages = context.into_messages();
    assert_eq!(messages.len(), 1);
    assert!(matches!(&messages[0], Message::User { .. }));
}

#[test]
fn messages_are_kept_verbatim() {
    // The context does not normalize, reorder, or interpret: what was
    // folded is what a request carries — field for field.
    let mut context = Context::new();
    let folded = vec![user("q"), assistant("a"), tool_results_message()];
    context.fold_all(folded.clone());
    assert_eq!(context.messages(), folded.as_slice());
}

#[test]
fn consecutive_same_role_messages_stay_separate() {
    // Grouping and alternation rules are the callers' law, not the
    // context's: two user messages in a row (a steer behind a prompt)
    // remain two messages.
    let mut context = Context::new();
    context.fold(user("prompt"));
    context.fold(user("a steer"));
    assert_eq!(context.messages().len(), 2);
}

#[test]
fn tool_content_is_opaque() {
    // Tool calls and results ride messages as content; the context
    // never inspects them. An assistant turn full of calls, answered
    // by a batch, is just two messages.
    let call_turn = Message::Assistant {
        id: Some("turn-1".to_string()),
        content: OneOrMany::many(vec![
            rig_core::message::AssistantContent::ToolCall(ToolCall::new(
                "c1".to_string(),
                ToolFunction::new("echo".to_string(), serde_json::json!({})),
            )),
            rig_core::message::AssistantContent::ToolCall(ToolCall::new(
                "c2".to_string(),
                ToolFunction::new("echo".to_string(), serde_json::json!({})),
            )),
        ])
        .expect("non-empty"),
    };
    let mut context = Context::new();
    context.fold(user("go"));
    context.fold(call_turn);
    context.fold(tool_results_message());
    assert_eq!(context.messages().len(), 3);
}

#[test]
fn default_and_new_agree() {
    assert_eq!(Context::new(), Context::default());
    assert_eq!(Context::default().messages(), &[] as &[Message]);
}
