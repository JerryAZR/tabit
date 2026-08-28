//! Anthropic structured output smoke test: a caller-supplied schema is
//! pure pass-through to the provider's native structured output.

use rig::client::AgentClientExt;
use rig::completion::Prompt;

use super::super::support::with_anthropic_cassette;
use crate::support::{
    STRUCTURED_OUTPUT_PROMPT, SmokeStructuredOutput, assert_smoke_structured_output,
};
use rig_agent::test_utils::decode_structured_output;

#[tokio::test]
async fn structured_output_smoke() {
    with_anthropic_cassette(
        "structured_output/structured_output_smoke",
        |client| async move {
            let agent = client
                .agent("claude-sonnet-4-6")
                .output_schema::<SmokeStructuredOutput>()
                .max_tokens(64_000)
                .build();

            let response = agent
                .prompt(STRUCTURED_OUTPUT_PROMPT)
                .await
                .expect("structured output prompt should succeed");
            let structured: SmokeStructuredOutput =
                decode_structured_output("anthropic_structured_output_smoke", &response)
                    .expect("structured output should deserialize");

            assert_smoke_structured_output(&structured);
        },
    )
    .await;
}
