//! OpenAI Responses API long-session regression tests.
//!
//! These tests lock down multi-turn, multi-tool agent sessions against the
//! Responses API: sequential tool roundtrips, parallel tool calls in a single
//! model turn, long chat-history replay, reasoning-enabled tool sessions, and
//! usage accounting across turns.
//!
//! Run cassette tests in replay mode by default, or set
//! `RIG_PROVIDER_TEST_MODE=record` to record against the real provider.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;
use rig::agent::MultiTurnStreamItem;
use rig::completion::Message;
use rig::prelude::*;
use rig::streaming::{StreamingChat, StreamingPrompt};
use rig::tool::Tool;

use super::super::support::with_openai_cassette;
use crate::reasoning::{self, WeatherTool};
use crate::support::{
    ALPHA_SIGNAL_OUTPUT, Adder, AlphaSignal, BETA_SIGNAL_OUTPUT, BetaSignal,
    ORDERED_TOOL_STREAM_PREAMBLE, ORDERED_TOOL_STREAM_PROMPT, Subtract, TWO_TOOL_STREAM_PREAMBLE,
    TWO_TOOL_STREAM_PROMPT, assert_mentions_expected_number, assert_two_tool_roundtrip_contract,
    collect_stream_observation,
};

const SEQUENTIAL_TOOLS_PREAMBLE: &str = "\
You are a calculator. Use the provided tools instead of doing arithmetic yourself. \
Call exactly one tool at a time and wait for its result before deciding the next step.";

const SEQUENTIAL_TOOLS_PROMPT: &str = "\
First use the add tool to compute 3 + 4. After you receive that result, use the \
subtract tool to subtract 5 from it. Then state the final number in one short sentence.";

#[tokio::test]
async fn sequential_tool_calls_streaming() {
    with_openai_cassette(
        "responses_sessions/sequential_tool_calls_streaming",
        |client| async move {
            let agent = client
                .agent("gpt-4o")
                .preamble(SEQUENTIAL_TOOLS_PREAMBLE)
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
async fn parallel_tool_calls_single_turn_streaming() {
    with_openai_cassette(
        "responses_sessions/parallel_tool_calls_single_turn_streaming",
        |client| async move {
            let agent = client
                .agent("gpt-4o")
                .preamble(TWO_TOOL_STREAM_PREAMBLE)
                .tool(AlphaSignal)
                .tool(BetaSignal)
                .build();

            let mut stream = agent
                .stream_prompt(TWO_TOOL_STREAM_PROMPT)
                .max_turns(5)
                .await;
            let observation = collect_stream_observation(&mut stream).await;

            assert_two_tool_roundtrip_contract(
                &observation,
                &[AlphaSignal::NAME, BetaSignal::NAME],
                &[ALPHA_SIGNAL_OUTPUT, BETA_SIGNAL_OUTPUT],
            );
        },
    )
    .await;
}

#[tokio::test]
async fn reasoning_session_two_tool_calls_streaming() {
    with_openai_cassette(
        "responses_sessions/reasoning_session_two_tool_calls_streaming",
        |client| async move {
            let call_count = Arc::new(AtomicUsize::new(0));
            let agent = client
                .agent("gpt-5.2")
                .preamble(reasoning::TOOL_SYSTEM_PROMPT)
                .max_tokens(6000)
                .tool(WeatherTool::new(call_count.clone()))
                .additional_params(serde_json::json!({
                    "reasoning": { "effort": "low" }
                }))
                .build();

            let stream = agent
                .stream_chat(vec![Message::user(
                    "I need the current weather in Tokyo and in Paris. Use the get_weather \
                     tool once per city, then compare the two cities in one short paragraph \
                     that mentions both city names.",
                )])
                .max_turns(5)
                .await;

            let stats = reasoning::collect_stream_stats(stream, "openai").await;

            assert!(
                stats.errors.is_empty(),
                "stream had errors: {:?}",
                stats.errors
            );
            let invocations = call_count.load(Ordering::SeqCst);
            assert!(
                invocations >= 2,
                "expected get_weather to run once per city, got {invocations}"
            );
            assert!(
                stats
                    .tool_calls_in_stream
                    .iter()
                    .filter(|name| name.as_str() == WeatherTool::NAME)
                    .count()
                    >= 2,
                "expected at least two get_weather calls in the stream, saw {:?}",
                stats.tool_calls_in_stream
            );
            assert!(
                stats.tool_results_in_stream >= 2,
                "expected a tool result per call, got {}",
                stats.tool_results_in_stream
            );
            assert!(
                stats.reasoning_block_count >= 1,
                "expected reasoning output from a reasoning-enabled session"
            );
            assert!(
                stats.got_final_response,
                "stream should emit a final response"
            );
            let final_text = stats.final_turn_text.to_ascii_lowercase();
            assert!(
                final_text.contains("tokyo") && final_text.contains("paris"),
                "final answer should mention both cities, got {:?}",
                stats.final_turn_text
            );
        },
    )
    .await;
}

#[tokio::test]
async fn usage_accumulates_across_streaming_multi_turn() {
    with_openai_cassette(
        "responses_sessions/usage_accumulates_across_streaming_multi_turn",
        |client| async move {
            let agent = client
                .agent("gpt-4o")
                .preamble(ORDERED_TOOL_STREAM_PREAMBLE)
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
