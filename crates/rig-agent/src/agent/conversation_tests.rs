//! The one context builder's contract suite.

use super::*;
use rig_core::message::{ToolFunction, ToolResultContent};

fn user(text: &str) -> Message {
    Message::user(text)
}

fn assistant(content: Vec<rig_core::message::AssistantContent>) -> Message {
    Message::Assistant {
        id: None,
        content: OneOrMany::many(content).expect("non-empty"),
    }
}

fn text_turn(text: &str) -> Message {
    assistant(vec![rig_core::message::AssistantContent::text(text)])
}

fn tool_call(id: &str) -> rig_core::message::AssistantContent {
    rig_core::message::AssistantContent::ToolCall(ToolCall::new(
        id.to_string(),
        ToolFunction::new("echo".to_string(), serde_json::json!({})),
    ))
}

fn tool_result(id: &str) -> ToolResult {
    ToolResult {
        id: id.to_string(),
        call_id: None,
        content: OneOrMany::one(ToolResultContent::text("ok")),
        status: None,
    }
}

fn user_texts(conversation: &mut Conversation) -> Vec<String> {
    conversation
        .messages_vec()
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => content.iter().find_map(|part| match part {
                rig_core::message::UserContent::Text(text) => Some(text.text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect()
}

#[test]
fn user_and_assistant_messages_fold_in_order() {
    let mut conversation = Conversation::new();
    conversation.user(user("q"));
    conversation.assistant(text_turn("a"));
    conversation.user(user("again"));
    assert_eq!(
        user_texts(&mut conversation),
        vec!["q".to_string(), "again".to_string()]
    );
    assert_eq!(conversation.len(), 3);
}

#[test]
fn from_messages_adopts_a_seed() {
    let mut conversation = Conversation::from_messages(vec![user("seed"), text_turn("a")]);
    conversation.user(user("next"));
    assert_eq!(conversation.len(), 3);
    assert_eq!(user_texts(&mut conversation), vec!["seed", "next"]);
}

#[test]
fn deferred_results_merge_into_one_user_message() {
    // The log-side feed shape: one result at a time.
    let mut conversation = Conversation::new();
    conversation.user(user("go"));
    conversation.assistant(assistant(vec![tool_call("c1"), tool_call("c2")]));
    conversation.tool_result(tool_result("c1"));
    conversation.tool_result(tool_result("c2"));
    let messages = conversation.messages_vec();
    assert_eq!(messages.len(), 3, "user, assistant, one merged batch");
    let Message::User { content } = &messages[2] else {
        panic!("the batch is one user message");
    };
    assert_eq!(content.len(), 2);
}

#[test]
fn a_prebatched_user_message_folds_identically() {
    // The engine-side feed shape: one validated batch as a message.
    let deferred: &mut Conversation = &mut Conversation::new();
    deferred.user(user("go"));
    deferred.assistant(assistant(vec![tool_call("c1"), tool_call("c2")]));
    deferred.tool_result(tool_result("c1"));
    deferred.tool_result(tool_result("c2"));

    let mut batched = Conversation::new();
    batched.user(user("go"));
    batched.assistant(assistant(vec![tool_call("c1"), tool_call("c2")]));
    batched.user(Message::User {
        content: OneOrMany::many(vec![
            rig_core::message::UserContent::ToolResult(tool_result("c1")),
            rig_core::message::UserContent::ToolResult(tool_result("c2")),
        ])
        .expect("non-empty"),
    });

    assert_eq!(deferred.messages_vec(), batched.messages_vec());
}

#[test]
fn steered_batches_extend_in_drain_order() {
    let mut conversation = Conversation::new();
    conversation.user(user("first"));
    conversation.extend_users(vec![user("a"), user("b")]);
    assert_eq!(user_texts(&mut conversation), vec!["first", "a", "b"]);
}

#[test]
fn pop_last_assistant_undoes_a_turn() {
    let mut conversation = Conversation::new();
    conversation.user(user("q"));
    conversation.assistant(text_turn("rejected"));
    conversation.pop_last_assistant();
    assert_eq!(conversation.len(), 1);
    // A no-op when the tail is not an assistant turn.
    conversation.pop_last_assistant();
    assert_eq!(conversation.len(), 1);
}

#[test]
fn new_since_splits_at_the_entry_boundary() {
    let mut conversation = Conversation::new();
    conversation.user(user("entry"));
    let entry_len = conversation.len();
    conversation.assistant(text_turn("new"));
    conversation.user(user("steer"));
    assert_eq!(conversation.new_since(entry_len).len(), 2);
    assert_eq!(conversation.new_since(0).len(), 3);
}

#[test]
fn a_complete_roundtrip_is_not_dangling() {
    let mut conversation = Conversation::new();
    conversation.user(user("go"));
    conversation.assistant(assistant(vec![tool_call("c1")]));
    conversation.tool_result(tool_result("c1"));
    let _ = conversation.messages();
    assert!(conversation.dangling().is_none());
}

#[test]
fn an_unanswered_call_dangles() {
    let mut conversation = Conversation::new();
    conversation.user(user("go"));
    conversation.assistant(assistant(vec![tool_call("c1")]));
    let _ = conversation.messages();
    let dangling = conversation.dangling().expect("dangles");
    assert_eq!(dangling.calls.len(), 1);
    assert_eq!(dangling.calls[0].id, "c1");
}

#[test]
fn a_partially_answered_batch_still_dangles_the_rest() {
    // The mid-batch branch shape: some results arrived, one never did.
    let mut conversation = Conversation::new();
    conversation.user(user("go"));
    conversation.assistant(assistant(vec![tool_call("c1"), tool_call("c2")]));
    conversation.tool_result(tool_result("c1"));
    let _ = conversation.messages();
    let dangling = conversation.dangling().expect("c2 dangles");
    assert_eq!(dangling.calls.len(), 1);
    assert_eq!(dangling.calls[0].id, "c2");
}

#[test]
fn interrupted_results_answer_every_call_explicitly() {
    let mut conversation = Conversation::new();
    conversation.user(user("go"));
    conversation.assistant(assistant(vec![tool_call("c1"), tool_call("c2")]));
    let _ = conversation.messages();
    let dangling = conversation.dangling().expect("dangles");
    let results = interrupted_results(&dangling);
    assert_eq!(results.len(), 2);
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
fn a_user_message_closes_the_roundtrip() {
    // A steer or synthetic message after an unanswered turn resets the
    // bookkeeping — the same semantics both feed shapes share.
    let mut conversation = Conversation::new();
    conversation.assistant(assistant(vec![tool_call("c1")]));
    conversation.user(user("actually, stop"));
    let _ = conversation.messages();
    assert!(conversation.dangling().is_none());
}
