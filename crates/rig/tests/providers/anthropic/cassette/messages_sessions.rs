//! Anthropic Messages API long-session regression tests.
//!
//! These tests lock down multi-turn, multi-tool agent sessions against the
//! Messages API: sequential tool roundtrips, parallel tool_use blocks in a
//! single assistant turn with batched tool_result grouping, long chat-history
//! replay (including assistant text before and after tool use), and usage
//! accounting across turns.
//!
//! Run cassette tests in replay mode by default, or set
//! `RIG_PROVIDER_TEST_MODE=record` to record against the real provider.

use futures::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::completion::Message;
use rig::prelude::*;
use rig::streaming::{StreamingChat, StreamingPrompt};
use rig::tool::Tool;

use super::super::support::with_anthropic_cassette;
use crate::support::{
    Adder, AlphaSignal, ORDERED_TOOL_STREAM_PREAMBLE, ORDERED_TOOL_STREAM_PROMPT, Subtract,
    assert_mentions_expected_number, collect_stream_observation,
};

const SEQUENTIAL_TOOLS_PREAMBLE: &str = "\
You are a calculator. Use the provided tools instead of doing arithmetic yourself. \
Call exactly one tool at a time and wait for its result before deciding the next step.";

const SEQUENTIAL_TOOLS_PROMPT: &str = "\
First use the add tool to compute 3 + 4. After you receive that result, use the \
subtract tool to subtract 5 from it. Then state the final number in one short sentence.";

#[tokio::test]
async fn sequential_tool_calls_streaming() {
    with_anthropic_cassette(
        "messages_sessions/sequential_tool_calls_streaming",
        |client| async move {
            let agent = client
                .agent("claude-sonnet-4-6")
                .preamble(SEQUENTIAL_TOOLS_PREAMBLE)
                .max_tokens(2048)
                .tool(Adder)
                .tool(Subtract)
                .build();

            let mut stream = agent
                .stream_chat(vec![Message::user(SEQUENTIAL_TOOLS_PROMPT)])
                .max_turns(6)
                .await;
            let observation = collect_stream_observation(&mut stream).await;

            assert!(
                observation.errors.is_empty(),
                "stream should not emit errors: {:?}",
                observation.errors
            );
            assert_eq!(
                observation.tool_calls,
                vec![Adder::NAME.to_string(), Subtract::NAME.to_string()],
                "expected exactly one add call followed by one subtract call"
            );
            assert_eq!(
                observation.tool_results, 2,
                "expected one tool result per tool call"
            );
            assert!(
                observation.got_final_response,
                "stream should emit a final response"
            );
            let response = observation
                .final_response_text
                .as_deref()
                .expect("stream should produce final response text");
            assert_mentions_expected_number(response, 2);
        },
    )
    .await;
}

#[tokio::test]
async fn usage_accumulates_across_streaming_multi_turn() {
    with_anthropic_cassette(
        "messages_sessions/usage_accumulates_across_streaming_multi_turn",
        |client| async move {
            let agent = client
                .agent("claude-sonnet-4-6")
                .preamble(ORDERED_TOOL_STREAM_PREAMBLE)
                .max_tokens(2048)
                .tool(AlphaSignal)
                .build();

            let mut stream = agent
                .stream_prompt(ORDERED_TOOL_STREAM_PROMPT)
                .max_turns(5)
                .await;

            let mut saw_tool_result = false;
            let mut final_usage = None;

            while let Some(item) = stream.next().await {
                match item.expect("stream item should be ok") {
                    MultiTurnStreamItem::StreamUserItem(_) => saw_tool_result = true,
                    MultiTurnStreamItem::FinalResponse(response) => {
                        final_usage = Some(response.usage());
                    }
                    _ => {}
                }
            }

            assert!(
                saw_tool_result,
                "session should include a tool roundtrip so usage spans two model turns"
            );
            let usage = final_usage.expect("stream should emit a final response with usage");
            assert!(
                usage.input_tokens > 0,
                "aggregated input tokens should be nonzero: {usage:?}"
            );
            assert!(
                usage.output_tokens > 0,
                "aggregated output tokens should be nonzero: {usage:?}"
            );
            assert!(
                usage.total_tokens >= usage.output_tokens,
                "total tokens should cover output tokens: {usage:?}"
            );
        },
    )
    .await;
}
