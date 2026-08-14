//! Anthropic agent completion smoke test.

use rig::completion::Prompt;
use rig::prelude::*;

use super::super::support::with_anthropic_cassette;
use crate::support::{BASIC_PREAMBLE, BASIC_PROMPT, assert_nonempty_response};

#[tokio::test]
async fn completion_smoke() {
    with_anthropic_cassette("agent/completion_smoke", |client| async move {
        let agent = client
            .agent("claude-sonnet-4-6")
            .preamble(BASIC_PREAMBLE)
            .max_tokens(64_000)
            .build();

        let response = agent
            .prompt(BASIC_PROMPT)
            .await
            .expect("completion should succeed");

        assert_nonempty_response(&response);
    })
    .await;
}
