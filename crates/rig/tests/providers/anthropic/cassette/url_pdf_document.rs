//! Cassette-backed Anthropic coverage for URL-backed PDF documents.
//!
//! Regression coverage for sending a `DocumentSourceKind::Url` PDF without an
//! explicit media type through the request-side message conversion: the
//! document must map to a `"source": {"type": "url", ...}` content block.
//! See <https://docs.anthropic.com/en/docs/build-with-claude/pdf-support>.

use rig::OneOrMany;
use rig::message::{Message, UserContent};
use rig::prelude::*;

use super::super::support::with_anthropic_cassette;
use crate::support::{assert_contains_any_case_insensitive, assert_nonempty_response};

const PDF_URL: &str = "https://bitcoin.org/bitcoin.pdf";

#[tokio::test]
async fn url_pdf_document_prompt() {
    with_anthropic_cassette(
        "url_pdf_document/url_pdf_document_prompt",
        |client| async move {
            let model = client.completion_model("claude-sonnet-4-6");
            let response = model
                .completion(model.completion_request(
                Message::User {
                    content: OneOrMany::many(vec![
                        UserContent::document_url(PDF_URL, None),
                        UserContent::text(
                            "What is the title of this paper? Answer in one short sentence.",
                        ),
                    ])
                    .expect("content should be non-empty"),
                        })
                        .preamble("You are a helpful assistant that analyzes documents.".to_string())
                        .temperature_opt(Some(0.0))
                        .max_tokens(64_000)
                        .build(),
                )
                .await
                .expect("URL PDF document prompt should succeed");

            let text = crate::support::assistant_text_response(&response.choice)
                .expect("URL PDF document prompt should carry assistant text");
            assert_nonempty_response(&text);
            assert_contains_any_case_insensitive(&text, &["bitcoin"]);
        },
    )
    .await;
}
