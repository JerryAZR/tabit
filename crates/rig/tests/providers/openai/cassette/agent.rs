//! OpenAI agent completion smoke test.

use rig::prelude::*;

use super::super::support::with_openai_cassette;
use crate::support::{BASIC_PREAMBLE, BASIC_PROMPT, assert_nonempty_response};

#[tokio::test]
async fn completion_smoke() {
    with_openai_cassette("agent/completion_smoke", |client| async move {
        let model = client.completion_model("gpt-4o");
        let response = model
            .completion(
                model
                    .completion_request(BASIC_PROMPT)
                    .preamble(BASIC_PREAMBLE.to_string())
                    .build(),
            )
            .await
            .expect("completion should succeed");

        let text = crate::support::assistant_text_response(&response.choice)
            .expect("completion should carry assistant text");
        assert_nonempty_response(&text);
    })
    .await;
}
