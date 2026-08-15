//! OpenAI agent completion smoke test.

use rig::completion::Prompt;
use rig::prelude::*;

use super::super::support::with_openai_cassette;
use crate::support::{BASIC_PREAMBLE, BASIC_PROMPT, assert_nonempty_response};

#[tokio::test]
async fn completion_smoke() {
    with_openai_cassette("agent/completion_smoke", |client| async move {
        let agent = client.agent("gpt-4o").preamble(BASIC_PREAMBLE).build();

        let response = agent
            .prompt(BASIC_PROMPT)
            .await
            .expect("completion should succeed");

        assert_nonempty_response(&response);
    })
    .await;
}
