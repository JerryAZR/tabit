//! Cassette-backed OpenAI Responses coverage for URL-backed PDF documents.
//!
//! Regression coverage for sending a `DocumentSourceKind::Url` PDF through
//! `CompletionModel::completion()`: the request must carry `file_url` without
//! the hardcoded `filename`, which the Responses API rejects alongside a URL
//! with 400 `mutually_exclusive_parameters`.
//! See <https://platform.openai.com/docs/guides/pdf-files>.

use rig::OneOrMany;
use rig::client::CompletionClient;
use rig::completion::CompletionModel;
use rig::message::{DocumentMediaType, Message, UserContent};

use super::super::support::with_openai_cassette;
use crate::support::{assert_contains_any_case_insensitive, assert_nonempty_response};

const PDF_URL: &str = "https://bitcoin.org/bitcoin.pdf";

#[tokio::test]
async fn url_pdf_document_prompt() {
    with_openai_cassette(
        "url_pdf_document/url_pdf_document_prompt",
        |client| async move {
            let model = client.completion_model("gpt-4o");
            let response = model
                .completion(
                    model
                        .completion_request(Message::User {
                            content: OneOrMany::many(vec![
                                UserContent::document_url(PDF_URL, Some(DocumentMediaType::PDF)),
                                UserContent::text(
                                    "What is the title of this paper? Answer in one short sentence.",
                                ),
                            ])
                            .expect("content should be non-empty"),
                        })
                        .preamble("You are a helpful assistant that analyzes documents.".to_string())
                        .temperature_opt(Some(0.0))
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
