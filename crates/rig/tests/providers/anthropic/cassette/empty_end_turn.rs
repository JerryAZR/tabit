//! Anthropic cassette regression coverage for empty `end_turn` tool follow-ups.
//!
//! Run cassette tests in replay mode by default, or set
//! `RIG_PROVIDER_TEST_MODE=record` to record against the real provider.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use rig::{
    completion::{CompletionModel, ToolDefinition},
    message::{AssistantContent, Message},
    prelude::*,
    tool::Tool,
};
use serde::Deserialize;
use serde_json::json;

const TERMINAL_NOTIFY_PREAMBLE: &str = "\
When the user reports their status, call `notify` with a short summary. \
Do not answer with any normal assistant text before or after the tool call. \
Once the tool result is available, the assistant turn is complete and you must end the turn with no content.";

const TERMINAL_NOTIFY_PROMPT: &str = "I finished the deploy.";

#[derive(Deserialize)]
struct NotifyArgs {
    msg: String,
}

#[derive(Debug, thiserror::Error)]
#[error("notify error")]
struct NotifyError;

fn notify_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: Notify::NAME.to_string(),
        description: "Send a short notification for a user status update.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "msg": {
                    "type": "string",
                    "description": "The short notification to send."
                }
            },
            "required": ["msg"]
        }),
    }
}

struct Notify {
    call_count: Arc<AtomicUsize>,
}

impl Tool for Notify {
    const NAME: &'static str = "notify";
    type Error = NotifyError;
    type Args = NotifyArgs;
    type Output = String;

    fn description(&self) -> String {
        "Send a short notification for a user status update.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        notify_tool_definition().parameters
    }

    async fn call(
        &self,
        _context: &mut rig::tool::ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(format!("sent: {}", args.msg))
    }
}

#[tokio::test]
async fn raw_followup_empty_end_turn_normalizes_to_empty_text_choice() {
    super::super::support::with_anthropic_cassette(
        "empty_end_turn/raw_followup_empty_end_turn_normalizes_to_empty_text_choice",
        |client| async move {
            let model = client.completion_model("claude-sonnet-4-6");

            let first_turn = model
                .completion_request(TERMINAL_NOTIFY_PROMPT)
                .preamble(TERMINAL_NOTIFY_PREAMBLE.to_string())
                .max_tokens(1024)
                .tool(notify_tool_definition())
                .send()
                .await
                .expect("first Anthropic turn should succeed");

            let tool_call = first_turn
                .choice
                .iter()
                .find_map(|item| match item {
                    AssistantContent::ToolCall(tool_call) => Some(tool_call.clone()),
                    _ => None,
                })
                .expect("first Anthropic turn should emit a notify tool call");

            let followup = model
                .completion_request(Message::tool_result_with_call_id(
                    tool_call.id.clone(),
                    tool_call.call_id.clone(),
                    "sent: deploy finished",
                ))
                .preamble(TERMINAL_NOTIFY_PREAMBLE.to_string())
                .max_tokens(1024)
                .message(Message::Assistant {
                    id: first_turn.message_id.clone(),
                    content: first_turn.choice.clone(),
                })
                .send()
                .await
                .expect("follow-up Anthropic turn should not error on empty end_turn");

            assert_eq!(
                followup.choice.len(),
                1,
                "expected normalized empty follow-up choice, got {:?}",
                followup.choice
            );

            match followup.choice.first() {
                AssistantContent::Text(text) => assert!(
                    text.text.is_empty(),
                    "expected empty follow-up text sentinel, got {:?}",
                    text.text
                ),
                other => panic!("expected empty text sentinel, got {other:?}"),
            }
        },
    )
    .await;
}
