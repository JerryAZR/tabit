//! OpenAI structured output smoke: a caller-supplied schema is pure
//! pass-through to the provider's native structured output.

use rig::client::AgentClientExt;
use rig::completion::Prompt;
use rig_agent::test_utils::decode_structured_output;

use super::super::support::with_openai_cassette;
use crate::support::{
    STRUCTURED_OUTPUT_PROMPT, SmokeStructuredOutput, assert_smoke_structured_output,
};

#[tokio::test]
async fn structured_output_smoke() {
    with_openai_cassette(
        "structured_output/structured_output_smoke",
        |client| async move {
            let agent = client
                .agent("gpt-4o")
                .output_schema::<SmokeStructuredOutput>()
                .build();

            let response = agent
                .prompt(STRUCTURED_OUTPUT_PROMPT)
                .await
                .expect("structured output prompt should succeed");
            let structured: SmokeStructuredOutput =
                decode_structured_output("openai_structured_output_smoke", &response)
                    .expect("structured output should deserialize");
            assert_smoke_structured_output(&structured);
        },
    )
    .await;
}
