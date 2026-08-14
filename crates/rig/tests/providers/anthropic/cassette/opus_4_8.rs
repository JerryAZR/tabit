//! Dedicated Claude Opus 4.8 cassette coverage.

use rig::completion::NormalizeCompletionResponse;
use rig::completion::{
    AssistantContent, CompletionModel, CompletionResponse as RigCompletionResponse, Document,
    Message, ProviderToolDefinition,
};
use rig::message::Text;
use rig::prelude::*;
use rig::telemetry::ProviderResponseExt;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::support::{assert_contains_any_case_insensitive, assistant_text_response};

/// Descriptor name the Anthropic client normalizes responses under; needed
/// when a test converts a `raw_completion` response itself.
const ANTHROPIC_PROVIDER: &str = "anthropic";

const DOCUMENT_GLOBAL_SYSTEM_INSTRUCTION: &str = "Answer in Spanish only. Use one short sentence.";

#[tokio::test]
async fn web_search_with_dynamic_filtering_succeeds() {
    super::super::support::with_anthropic_cassette(
        "opus_4_8/web_search_with_dynamic_filtering_succeeds",
        |client| async move {
            let model = client.completion_model("claude-opus-4-8");
            let request = model
                .completion_request(
                    "Search for the current prices of AAPL and GOOGL, then calculate which has a better P/E ratio.",
                )
                .provider_tool(
                    ProviderToolDefinition::new("web_search_20260209")
                        .with_config("name", json!("web_search")),
                )
                .max_tokens(1024)
                .build();
            // One request, two views: `raw_completion` returns Anthropic's own
            // response and the same value normalizes into rig's, so the
            // provider-text fallback below still costs a single interaction.
            let raw = model
                .raw_completion(request)
                .await
                .expect("Opus 4.8 dynamic web-search request should succeed");
            let raw_text = raw.get_text_response();
            let response: RigCompletionResponse = raw.normalize(ANTHROPIC_PROVIDER)
                .expect("Opus 4.8 dynamic web-search response should normalize");

            assert!(
                response.choice.iter().any(|content| {
                    content_raw_type(content) == Some("code_execution_tool_result")
                }),
                "dynamic web-search response should preserve a code_execution_tool_result block",
            );
            assert!(
                assistant_text_response(&response.choice)
                    .or(raw_text)
                    .is_some_and(|text| !text.trim().is_empty()),
                "dynamic web-search response should contain assistant text",
            );
        },
    )
    .await;
}

#[tokio::test]
async fn documents_keep_leading_system_message_top_level() {
    super::super::support::with_anthropic_cassette(
        "opus_4_8/documents_keep_leading_system_message_top_level",
        |client| async move {
            let model = client.completion_model("claude-opus-4-8");
            let request = model
                .completion_request(
                    "According to the document, what color is the clear daytime sky?",
                )
                .messages([
                    Message::system(DOCUMENT_GLOBAL_SYSTEM_INSTRUCTION),
                    Message::assistant("Entendido."),
                ])
                .document(Document {
                    id: "sky-note".to_string(),
                    text: "A clear daytime sky is blue.".to_string(),
                    additional_props: Default::default(),
                })
                .max_tokens(64)
                .build();
            let raw = model.raw_completion(request).await.expect(
                "Opus 4.8 request with documents and a leading system message should succeed",
            );
            let raw_text = raw.get_text_response();
            let response: RigCompletionResponse = raw.normalize(ANTHROPIC_PROVIDER).expect(
                "Opus 4.8 response with documents and a leading system message should normalize",
            );

            let text = assistant_text_response(&response.choice)
                .or(raw_text)
                .expect("response should contain assistant text");
            assert_contains_any_case_insensitive(&text, &["azul"]);
        },
    )
    .await;

    assert_cassette_hoists_system_instruction(
        "opus_4_8/documents_keep_leading_system_message_top_level",
        DOCUMENT_GLOBAL_SYSTEM_INSTRUCTION,
    );
    assert_cassette_document_request_order(
        "opus_4_8/documents_keep_leading_system_message_top_level",
        DOCUMENT_GLOBAL_SYSTEM_INSTRUCTION,
    );
}

fn content_raw_type(content: &AssistantContent) -> Option<&str> {
    let AssistantContent::Text(text) = content else {
        return None;
    };

    anthropic_raw_content_type(text)
}

fn anthropic_raw_content_type(text: &Text) -> Option<&str> {
    text.additional_params
        .as_ref()
        .and_then(|params| params.get("anthropic_content"))
        .and_then(|raw_content| raw_content.get("type"))
        .and_then(Value::as_str)
}

#[derive(Deserialize)]
struct RecordedInteraction {
    when: RecordedRequest,
}

#[derive(Deserialize)]
struct RecordedRequest {
    body: Option<String>,
}

fn assert_cassette_hoists_system_instruction(scenario: &str, expected_system_text: &str) {
    let request_bodies = recorded_request_bodies(scenario);
    let top_level_system_contains_instruction = request_bodies.iter().any(|body| {
        body.get("system")
            .and_then(Value::as_array)
            .is_some_and(|system| {
                system
                    .iter()
                    .any(|block| block_contains_text(block, expected_system_text))
            })
    });
    let messages_contain_system_role_instruction = request_bodies.iter().any(|body| {
        body.get("messages")
            .and_then(Value::as_array)
            .is_some_and(|messages| {
                messages.iter().any(|message| {
                    message.get("role").and_then(Value::as_str) == Some("system")
                        && message_contains_text(message, expected_system_text)
                })
            })
    });

    assert!(
        top_level_system_contains_instruction,
        "expected cassette {scenario} to contain the leading system instruction in top-level system",
    );
    assert!(
        !messages_contain_system_role_instruction,
        "expected cassette {scenario} not to send the leading system instruction as messages[] role=system",
    );
}

fn assert_cassette_document_request_order(scenario: &str, expected_system_text: &str) {
    let request_bodies = recorded_request_bodies(scenario);
    let body = request_bodies
        .iter()
        .find(|body| {
            body.get("system")
                .and_then(Value::as_array)
                .is_some_and(|system| {
                    system
                        .iter()
                        .any(|block| block_contains_text(block, expected_system_text))
                })
        })
        .unwrap_or_else(|| {
            panic!("expected cassette {scenario} to include document ordering request")
        });

    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("expected cassette {scenario} request to contain messages[]"));
    let roles = messages
        .iter()
        .map(|message| message.get("role").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        roles,
        [Some("user"), Some("assistant"), Some("user")],
        "expected document request to preserve document -> assistant -> prompt order",
    );
    assert!(
        message_content_has_type(&messages[0], "document"),
        "expected first message to contain the normalized document block",
    );
    assert!(
        message_contains_text(&messages[1], "Entendido."),
        "expected second message to preserve prior assistant turn",
    );
    assert!(
        message_contains_text(
            &messages[2],
            "According to the document, what color is the clear daytime sky?",
        ),
        "expected final message to remain the prompt",
    );
}

fn recorded_request_bodies(scenario: &str) -> Vec<Value> {
    let cassette_path = crate::cassettes::cassette_path("anthropic", scenario);
    let contents = std::fs::read_to_string(&cassette_path).unwrap_or_else(|error| {
        panic!(
            "provider cassette {} should be readable after recording: {error}",
            cassette_path.display()
        )
    });

    serde_yaml::Deserializer::from_str(&contents)
        .filter_map(|document| {
            let interaction = RecordedInteraction::deserialize(document)
                .expect("cassette interaction should deserialize");
            interaction
                .when
                .body
                .and_then(|body| serde_json::from_str::<Value>(&body).ok())
        })
        .collect()
}

fn message_contains_text(message: &Value, expected_text: &str) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| {
            content
                .iter()
                .any(|block| block_contains_text(block, expected_text))
        })
}

fn message_content_has_type(message: &Value, expected_type: &str) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| {
            content
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some(expected_type))
        })
}

fn block_contains_text(block: &Value, expected_text: &str) -> bool {
    block.get("text").and_then(Value::as_str) == Some(expected_text)
}
