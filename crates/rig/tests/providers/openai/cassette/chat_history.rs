//! OpenAI high-level Chat history regression tests.
//!
//! Run cassette tests in replay mode by default, or set
//! `RIG_PROVIDER_TEST_MODE=record` to record against the real provider.

fn test_cell(prompt: &str) -> tabit_log::ConversationCell {
    std::sync::Arc::new(std::sync::RwLock::new(tabit_log::ContextManager::seeded(
        vec![Message::user(prompt)],
    )))
}

fn cell_conversation(cell: &tabit_log::ConversationCell) -> Vec<Message> {
    tabit_log::lock::read(cell).messages()
}
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use rig::completion::Message;
use rig::prelude::*;

use super::super::support::with_openai_cassette;
use crate::reasoning::{self, WeatherTool};

#[tokio::test]
async fn chat_appends_reasoning_tool_turns_to_caller_history() {
    with_openai_cassette(
        "chat_history/chat_appends_reasoning_tool_turns_to_caller_history",
        |client| async move {
            let call_count = Arc::new(AtomicUsize::new(0));
            let agent = client
                .with_system_instructions_as_messages()
                .agent("gpt-5.2")
                .preamble(reasoning::TOOL_SYSTEM_PROMPT)
                .max_tokens(4096)
                .tool(WeatherTool::new(call_count.clone()))
                .additional_params(serde_json::json!({
                    "reasoning": { "effort": "high" }
                }))
                .default_max_turns(2)
                .build();
            let cell = test_cell(reasoning::TOOL_USER_PROMPT);

            let result = agent
                .prompt_over(cell.clone())
                .max_turns(2)
                .await
                .expect("[openai] run failed before it could grow the conversation");

            reasoning::assert_nonstreaming_universal(&result, &call_count, "openai");
            reasoning::assert_chat_history_preserves_reasoning_tool_roundtrip(
                &cell_conversation(&cell),
                &result,
                "openai",
            );
        },
    )
    .await;
}
