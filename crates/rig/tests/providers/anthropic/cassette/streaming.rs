//! Anthropic streaming smoke test.

use rig::prelude::*;
use rig::streaming::StreamingPrompt;

use super::super::support::with_anthropic_cassette;
use crate::support::{
    STREAMING_PREAMBLE, STREAMING_PROMPT, assert_nonempty_response,
    collect_stream_final_response_and_provider_final,
};

#[tokio::test]
async fn streaming_smoke() {
    with_anthropic_cassette("streaming/streaming_smoke", |client| async move {
        let agent = client
            .agent("claude-sonnet-4-6")
            .preamble(STREAMING_PREAMBLE)
            .max_tokens(64_000)
            .build();

        let mut stream = agent.stream_prompt(STREAMING_PROMPT).await;
        let (response, provider_final): (_, rig::streaming::StreamFinal) =
            collect_stream_final_response_and_provider_final(&mut stream)
                .await
                .expect("streaming prompt should succeed");

        assert_nonempty_response(&response);
        assert_eq!(provider_final.provider, "anthropic");
        assert!(provider_final.usage.total_tokens > 0);
    })
    .await;
}
