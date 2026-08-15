//! Anthropic reasoning roundtrip tests.
//!
//! Run cassette tests in replay mode by default, or set
//! `RIG_PROVIDER_TEST_MODE=record` to record against the real provider.

use rig::prelude::*;

use super::super::support::with_anthropic_cassette;
use crate::reasoning::{self, ReasoningRoundtripAgent};

#[tokio::test]
async fn streaming() {
    with_anthropic_cassette("reasoning_roundtrip/streaming", |client| async move {
        reasoning::run_reasoning_roundtrip_streaming(
            ReasoningRoundtripAgent::new(
                client.completion_model("claude-sonnet-4-6"),
                Some(serde_json::json!({
                    "thinking": { "type": "adaptive" }
                })),
            )
            .with_max_tokens(64_000),
        )
        .await;
    })
    .await;
}

#[tokio::test]
async fn nonstreaming() {
    with_anthropic_cassette("reasoning_roundtrip/nonstreaming", |client| async move {
        reasoning::run_reasoning_roundtrip_nonstreaming(
            ReasoningRoundtripAgent::new(
                client.completion_model("claude-sonnet-4-6"),
                Some(serde_json::json!({
                    "thinking": { "type": "adaptive" }
                })),
            )
            .with_max_tokens(64_000),
        )
        .await;
    })
    .await;
}
