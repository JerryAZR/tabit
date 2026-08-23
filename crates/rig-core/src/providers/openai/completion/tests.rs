/// Boundary-minted tool ids (`tool-{index}`, from id-less streamed calls)
/// replay to the chat wire as a self-consistent pair: the assistant
/// message's `tool_calls[].id` and the tool result's `tool_call_id` carry
/// the same minted value. The wire requires both fields, so gating minted
/// ids out (the Responses reasoning treatment) is impossible here — and
/// unnecessary: a gateway that omitted ids has no server-side id to
/// validate against, so the consistent pair is accepted. This pins the
/// per-wire upstream rule documented on `SyntheticIds`.
#[test]
fn minted_tool_ids_replay_as_a_consistent_pair() {
    let assistant = crate::message::Message::Assistant {
        id: None,
        content: crate::OneOrMany::one(crate::message::AssistantContent::tool_call(
            "tool-0",
            "get_weather",
            serde_json::json!({"city": "Tokyo"}),
        )),
    };
    let tool_result = crate::message::Message::User {
        content: crate::OneOrMany::one(crate::message::UserContent::tool_result(
            "tool-0",
            crate::OneOrMany::one(crate::message::ToolResultContent::text("22C")),
        )),
    };

    let assistant_wire: Vec<super::Message> = assistant.try_into().expect("assistant converts");
    let result_wire: Vec<super::Message> = tool_result.try_into().expect("tool result converts");

    let call_id = assistant_wire
        .iter()
        .find_map(|message| match message {
            super::Message::Assistant { tool_calls, .. } => {
                tool_calls.first().map(|call| call.id.clone())
            }
            _ => None,
        })
        .expect("assistant message carries the tool call");
    let result_id = result_wire
        .iter()
        .find_map(|message| match message {
            super::Message::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .expect("tool result message present");

    assert_eq!(call_id, "tool-0");
    assert_eq!(
        result_id, call_id,
        "the minted pair must be self-consistent"
    );
}

use super::*;
use crate::completion::CompletionRequestBuilder;
use crate::telemetry::ProviderResponseExt;
use crate::test_utils::MockCompletionModel;
use std::collections::HashMap;

fn test_document(id: &str, text: &str) -> crate::completion::Document {
    crate::completion::Document {
        id: id.to_string(),
        text: text.to_string(),
        additional_props: HashMap::new(),
    }
}

fn request_with_multi_block_tool_result() -> CoreCompletionRequest {
    let tool_result = message::ToolResult {
        id: "result-id".to_string(),
        call_id: Some("call-id".to_string()),
        content: OneOrMany::many(vec![
            message::ToolResultContent::text("first"),
            message::ToolResultContent::text("second"),
        ])
        .expect("multiple tool-result blocks should be non-empty"),
    };

    CoreCompletionRequest {
        model: None,
        preamble: None,
        // The assistant turn that produced the call must precede its
        // result: the conversion layer rejects orphan tool results.
        chat_history: OneOrMany::many(vec![
            message::Message::Assistant {
                id: None,
                content: OneOrMany::one(message::AssistantContent::tool_call(
                    "call-id",
                    "report",
                    serde_json::json!({}),
                )),
            },
            message::Message::User {
                content: OneOrMany::one(message::UserContent::ToolResult(tool_result)),
            },
        ])
        .expect("history should be non-empty"),
        documents: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    }
}

#[test]
fn mixed_user_content_preserves_order_around_tool_results() {
    let content = OneOrMany::many(vec![
        message::UserContent::text("before"),
        message::UserContent::tool_result_with_call_id(
            "result-id",
            "call-id".to_string(),
            OneOrMany::one(message::ToolResultContent::text("tool output")),
        ),
        message::UserContent::text("after"),
    ])
    .expect("mixed content should be non-empty");

    let messages = Vec::<Message>::try_from(content).expect("message conversion");

    assert!(matches!(
        messages.as_slice(),
        [
            Message::User { content: before, .. },
            Message::ToolResult { tool_call_id, .. },
            Message::User { content: after, .. },
        ] if matches!(before.first(), UserContent::Text { text } if text == "before")
            && tool_call_id == "call-id"
            && matches!(after.first(), UserContent::Text { text } if text == "after")
    ));
}

#[test]
fn video_data_uri_with_unrecognized_mime_round_trips_as_url() {
    let original = "data:video/quicktime;base64,AAAA";
    let openai_content = UserContent::Video {
        video_url: VideoUrl {
            url: original.to_string(),
        },
    };

    let rig_content: message::UserContent = openai_content.into();
    // Unrecognized MIME: kept as a URL source, not decomposed.
    assert!(matches!(
        &rig_content,
        message::UserContent::Video(video)
            if matches!(&video.data, message::DocumentSourceKind::Url(url) if url == original)
    ));

    let back = UserContent::try_from(rig_content).expect("video should convert back");
    assert!(matches!(
        back,
        UserContent::Video { video_url } if video_url.url == original
    ));
}

#[test]
fn video_data_uri_with_known_mime_decomposes_to_base64() {
    let openai_content = UserContent::Video {
        video_url: VideoUrl {
            url: "data:video/mp4;base64,AAAA".to_string(),
        },
    };

    let rig_content: message::UserContent = openai_content.into();
    assert!(matches!(
        &rig_content,
        message::UserContent::Video(video)
            if video.media_type == Some(crate::message::VideoMediaType::MP4)
                && matches!(&video.data, message::DocumentSourceKind::Base64(data) if data == "AAAA")
    ));
}

#[test]
fn tool_result_array_content_preserves_multiple_text_blocks() {
    let request = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request: request_with_multi_block_tool_result(),
        strict_tools: false,
        tool_result_array_content: true,
        supports_response_format: true,
        supports_tools: true,
    })
    .expect("request conversion should succeed");

    let wire = serde_json::to_value(&request.messages).expect("messages should serialize");

    // The correlated assistant tool-call turn precedes the result on the
    // wire; the tool message itself is what this test pins.
    let serde_json::Value::Array(messages) = wire else {
        panic!("messages should serialize to an array");
    };
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[1],
        serde_json::json!({
            "role": "tool",
            "tool_call_id": "call-id",
            "content": [
                {
                    "type": "text",
                    "text": "first"
                },
                {
                    "type": "text",
                    "text": "second"
                }
            ]
        })
    );
}

#[test]
fn tool_result_string_content_flattens_multiple_text_blocks() {
    let request = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request: request_with_multi_block_tool_result(),
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    })
    .expect("request conversion should succeed");

    let wire = serde_json::to_value(&request.messages).expect("messages should serialize");

    // The correlated assistant tool-call turn precedes the result on the
    // wire; the tool message itself is what this test pins.
    let serde_json::Value::Array(messages) = wire else {
        panic!("messages should serialize to an array");
    };
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[1],
        serde_json::json!({
            "role": "tool",
            "tool_call_id": "call-id",
            "content": "first\nsecond"
        })
    );
}

/// A tool result whose correlation key matches no prior assistant tool
/// call is an orphan: the conversion fails loudly, naming the id and the
/// history index, instead of forwarding a request OpenAI would reject.
#[test]
fn orphan_tool_result_history_fails_request_conversion() {
    let request = CoreCompletionRequest {
        model: None,
        preamble: None,
        chat_history: OneOrMany::many(vec![
            message::Message::user("Run the report."),
            message::Message::User {
                content: OneOrMany::one(message::UserContent::ToolResult(message::ToolResult {
                    id: "result-id".to_string(),
                    call_id: Some("call-orphan".to_string()),
                    content: OneOrMany::one(message::ToolResultContent::text("output")),
                })),
            },
        ])
        .expect("history should be non-empty"),
        documents: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    };

    let error = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    })
    .expect_err("an orphan tool result must fail request conversion");
    assert!(
        error.to_string().contains(
            "tool result \"call-orphan\" has no matching tool call in the conversation history"
        ),
        "unexpected error: {error}"
    );
    assert!(
        error.to_string().contains("history index 1"),
        "the error must name the message index: {error}"
    );
}

#[test]
fn multiple_tool_result_blocks_convert_to_distinct_content_parts() {
    let result = message::ToolResult {
        id: "result-id".to_string(),
        call_id: Some("call-id".to_string()),
        content: OneOrMany::many(vec![
            message::ToolResultContent::text("first"),
            message::ToolResultContent::json(serde_json::json!({
                "status": "ok"
            })),
            message::ToolResultContent::text("second"),
        ])
        .expect("tool-result content should be non-empty"),
    };

    let converted = Message::try_from(result).expect("tool result should convert");

    assert_eq!(
        converted,
        Message::ToolResult {
            tool_call_id: "call-id".to_string(),
            content: ToolResultContentValue::Array(vec![
                ToolResultContent::from("first".to_string()),
                ToolResultContent::from(r#"{"status":"ok"}"#.to_string()),
                ToolResultContent::from("second".to_string()),
            ]),
        }
    );
}

#[test]
fn test_openai_request_uses_request_model_override() {
    let request = crate::completion::CompletionRequest {
        model: Some("gpt-4.1".to_string()),
        preamble: None,
        chat_history: crate::OneOrMany::one("Hello".into()),
        documents: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    };

    let openai_request = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    })
    .expect("request conversion should succeed");
    let serialized = serde_json::to_value(openai_request).expect("serialization should succeed");

    assert_eq!(serialized["model"], "gpt-4.1");
}

#[test]
fn test_openai_request_uses_default_model_when_override_unset() {
    let request = crate::completion::CompletionRequest {
        model: None,
        preamble: None,
        chat_history: crate::OneOrMany::one("Hello".into()),
        documents: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    };

    let openai_request = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    })
    .expect("request conversion should succeed");
    let serialized = serde_json::to_value(openai_request).expect("serialization should succeed");

    assert_eq!(serialized["model"], "gpt-4o-mini");
}

/// A mixed `additional_params.tools` array splits by shape: function tools
/// merge into the typed `tools` field (left in the flattened params they
/// would replace the typed field at serialization — the flattened key wins),
/// while non-function entries stay behind for a provider's `prepare_request`
/// hook to fold its native tools from.
#[test]
fn additional_params_function_tools_merge_and_native_tools_stay() {
    let request = crate::completion::CompletionRequest {
        model: None,
        preamble: None,
        chat_history: crate::OneOrMany::one("Hello".into()),
        documents: vec![],
        tools: vec![crate::completion::ToolDefinition {
            name: "builder_tool".to_string(),
            description: "from the builder".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: Some(serde_json::json!({
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "params_tool",
                        "description": "from additional_params",
                        "parameters": {"type": "object", "properties": {}}
                    }
                },
                {"type": "browser_search"}
            ]
        })),
        output_schema: None,
        record_telemetry_content: false,
    };

    let openai_request = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    })
    .expect("request conversion should succeed");

    // The conversion is the contract: function tools merge into the typed
    // list, non-function entries stay behind for `prepare_request` hooks
    // (which read `additional_params` before serialization — a leftover
    // `tools` key would shadow the typed field on the flattened wire body,
    // which is exactly why merging, not passing through, is the fix).
    let names: Vec<&str> = openai_request
        .tools
        .iter()
        .map(|tool| tool.function.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["builder_tool", "params_tool"],
        "the builder's typed tools and the merged function tools coexist"
    );
    assert_eq!(
        openai_request.additional_params,
        Some(serde_json::json!({"tools": [{"type": "browser_search"}]})),
        "non-function entries stay behind for the provider's prepare_request hook"
    );

    // When every additional tool is a function tool, nothing stays behind:
    // the serialized body carries the merged list under `tools` with no
    // flattened collision.
    let request = crate::completion::CompletionRequest {
        additional_params: Some(serde_json::json!({
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "params_tool",
                        "description": "from additional_params",
                        "parameters": {"type": "object", "properties": {}}
                    }
                }
            ]
        })),
        model: None,
        preamble: None,
        chat_history: crate::OneOrMany::one("Hello".into()),
        documents: vec![],
        tools: vec![crate::completion::ToolDefinition {
            name: "builder_tool".to_string(),
            description: "from the builder".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        output_schema: None,
        record_telemetry_content: false,
    };
    let openai_request = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    })
    .expect("request conversion should succeed");
    let serialized = serde_json::to_value(openai_request).expect("serialization should succeed");
    let names: Vec<&str> = serialized["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert_eq!(names, vec!["builder_tool", "params_tool"]);
}

#[test]
fn openai_chat_request_keeps_documents_after_system_messages() {
    let request = CompletionRequestBuilder::new(MockCompletionModel::default(), "Prompt")
        .message(crate::completion::Message::system("System prompt"))
        .message(crate::completion::Message::user("Earlier user turn"))
        .message(crate::completion::Message::assistant(
            "Earlier assistant turn",
        ))
        .document(test_document("doc1", "Document text."))
        .build();

    let openai_request = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    })
    .expect("request conversion should succeed");

    let serialized =
        serde_json::to_value(&openai_request.messages).expect("messages should serialize");
    let messages = serialized.as_array().expect("messages should be an array");

    assert_eq!(messages.len(), 5);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "user");
    assert!(
        messages[1].to_string().contains("<file id: doc1>"),
        "document message should follow system message: {messages:?}"
    );
    assert_eq!(messages[2]["role"], "user");
    assert!(
        messages[2].to_string().contains("Earlier user turn"),
        "prior user history should follow document message: {messages:?}"
    );
    assert_eq!(messages[3]["role"], "assistant");
    assert!(
        messages[3].to_string().contains("Earlier assistant turn"),
        "prior assistant history should follow prior user history: {messages:?}"
    );
    assert_eq!(messages[4]["role"], "user");
    assert!(
        messages[4].to_string().contains("Prompt"),
        "prompt should remain last: {messages:?}"
    );
}

#[test]
fn openai_chat_direct_request_keeps_documents_after_system_messages() {
    let request = CoreCompletionRequest {
        model: None,
        preamble: None,
        chat_history: crate::OneOrMany::many(vec![
            crate::completion::Message::system("System prompt"),
            crate::completion::Message::assistant("Earlier assistant turn"),
            crate::completion::Message::system("Mid-conversation instruction"),
            crate::completion::Message::user("Prompt"),
        ])
        .unwrap(),
        documents: vec![test_document("doc1", "Document text.")],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    };

    let openai_request = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    })
    .expect("request conversion should succeed");

    let serialized =
        serde_json::to_value(&openai_request.messages).expect("messages should serialize");
    let messages = serialized.as_array().expect("messages should be an array");

    assert_eq!(messages.len(), 5);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "user");
    assert!(
        messages[1].to_string().contains("<file id: doc1>"),
        "document message should follow leading system messages: {messages:?}"
    );
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[3]["role"], "system");
    assert_eq!(messages[4]["role"], "user");
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.to_string().contains("<file id: doc1>"))
            .count(),
        1,
        "document message should appear exactly once: {messages:?}"
    );
}

#[test]
fn assistant_reasoning_alone_is_dropped() {
    let assistant_content = OneOrMany::one(message::AssistantContent::reasoning("hidden"));

    let converted: Vec<Message> = assistant_content
        .try_into()
        .expect("conversion should work");

    assert!(converted.is_empty());
}

// Regression test: providers that serve thinking models over the OpenAI
// Chat Completions schema (DeepSeek-R1, GLM-4.6, Qwen3-Thinking) return
// 400 "thinking is enabled but reasoning_content is missing" on the next
// turn if the prior assistant tool-call message didn't echo the reasoning.
#[test]
fn assistant_reasoning_is_attached_to_tool_call_message() {
    let assistant_content = OneOrMany::many(vec![
        message::AssistantContent::reasoning("hidden"),
        message::AssistantContent::text("visible"),
        message::AssistantContent::tool_call(
            "call_1",
            "subtract",
            serde_json::json!({"x": 2, "y": 1}),
        ),
    ])
    .expect("non-empty assistant content");

    let converted: Vec<Message> = assistant_content
        .try_into()
        .expect("conversion should work");
    assert_eq!(converted.len(), 1);

    match &converted[0] {
        Message::Assistant {
            content,
            tool_calls,
            reasoning,
            ..
        } => {
            assert_eq!(
                content,
                &vec![AssistantContent::Text {
                    text: "visible".to_string()
                }]
            );
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].id, "call_1");
            assert_eq!(tool_calls[0].function.name, "subtract");
            assert_eq!(
                tool_calls[0].function.arguments,
                serde_json::json!({"x": 2, "y": 1})
            );
            assert_eq!(reasoning.as_deref(), Some("hidden"));
        }
        _ => panic!("expected assistant message"),
    }

    let json = serde_json::to_value(&converted[0]).expect("serialize");
    assert_eq!(json["reasoning_content"], "hidden");
}

#[test]
fn assistant_reasoning_roundtrips_back_to_rig_message() {
    let assistant = Message::Assistant {
        content: vec![AssistantContent::Text {
            text: "visible".to_string(),
        }],
        reasoning: Some("hidden".to_string()),
        refusal: None,
        audio: None,
        name: None,
        tool_calls: vec![],
        reasoning_details: vec![],
        images: vec![],
    };

    let rig_msg: message::Message = assistant.try_into().expect("convert back");

    let message::Message::Assistant { content, .. } = rig_msg else {
        panic!("expected assistant");
    };

    let items: Vec<_> = content.into_iter().collect();
    assert_eq!(items.len(), 2);
    assert!(matches!(items[0], message::AssistantContent::Reasoning(_)));
    assert!(matches!(items[1], message::AssistantContent::Text(_)));
}

#[test]
fn provider_response_text_response_reads_assistant_multipart_output() {
    let response = CompletionResponse {
        id: "resp_123".to_owned(),
        object: "chat.completion".to_owned(),
        created: 0,
        model: "gpt-4o".to_owned(),
        system_fingerprint: None,
        choices: vec![Choice {
            index: 0,
            message: Message::Assistant {
                content: vec![
                    AssistantContent::Text {
                        text: "first".to_owned(),
                    },
                    AssistantContent::Refusal {
                        refusal: "second".to_owned(),
                    },
                    AssistantContent::Text {
                        text: "third".to_owned(),
                    },
                ],
                reasoning: Some("hidden".to_owned()),
                refusal: None,
                audio: None,
                name: None,
                tool_calls: vec![],
                reasoning_details: vec![],
                images: vec![],
            },
            logprobs: None,
            finish_reason: "stop".to_owned(),
        }],
        usage: None,
    };

    assert_eq!(
        response.get_text_response(),
        Some("first\nsecond\nthird".to_owned())
    );
}

#[test]
fn provider_response_text_response_falls_back_to_assistant_refusal_field() {
    let response = CompletionResponse {
        id: "resp_123".to_owned(),
        object: "chat.completion".to_owned(),
        created: 0,
        model: "gpt-4o".to_owned(),
        system_fingerprint: None,
        choices: vec![Choice {
            index: 0,
            message: Message::Assistant {
                content: vec![],
                reasoning: None,
                refusal: Some("blocked".to_owned()),
                audio: None,
                name: None,
                tool_calls: vec![],
                reasoning_details: vec![],
                images: vec![],
            },
            logprobs: None,
            finish_reason: "stop".to_owned(),
        }],
        usage: None,
    };

    assert_eq!(response.get_text_response(), Some("blocked".to_owned()));
}

#[test]
fn test_max_tokens_is_forwarded_to_request() {
    let request = crate::completion::CompletionRequest {
        model: None,
        preamble: None,
        chat_history: crate::OneOrMany::one("Hello".into()),
        documents: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: Some(4096),
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    };

    let openai_request = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    })
    .expect("request conversion should succeed");
    let serialized = serde_json::to_value(openai_request).expect("serialization should succeed");

    assert_eq!(serialized["max_tokens"], 4096);
}

#[test]
fn test_max_tokens_omitted_when_none() {
    let request = crate::completion::CompletionRequest {
        model: None,
        preamble: None,
        chat_history: crate::OneOrMany::one("Hello".into()),
        documents: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    };

    let openai_request = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    })
    .expect("request conversion should succeed");
    let serialized = serde_json::to_value(openai_request).expect("serialization should succeed");

    assert!(serialized.get("max_tokens").is_none());
}

#[test]
fn request_conversion_errors_when_all_messages_are_filtered() {
    let request = CoreCompletionRequest {
        model: None,
        preamble: None,
        chat_history: OneOrMany::one(message::Message::Assistant {
            id: None,
            content: OneOrMany::one(message::AssistantContent::reasoning("hidden")),
        }),
        documents: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    };

    let result = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    });

    assert!(matches!(result, Err(CompletionError::RequestError(_))));
}

#[test]
fn request_conversion_omits_response_format_on_initial_tool_turn() {
    let request = CoreCompletionRequest {
        model: None,
        preamble: None,
        chat_history: OneOrMany::one(message::Message::user(
            "Hello, whats the weather in London?",
        )),
        documents: vec![],
        tools: vec![completion::ToolDefinition {
            name: "weather".to_string(),
            description: "Get the weather".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "city": { "type": "string" }
                },
                "required": ["city"]
            }),
        }],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: Some(
            serde_json::from_value(serde_json::json!({
                "title": "WeatherResponse",
                "type": "object",
                "properties": {
                    "city": { "type": "string" },
                    "weather": { "type": "string" }
                },
                "required": ["city", "weather"]
            }))
            .expect("schema should deserialize"),
        ),
        record_telemetry_content: false,
    };

    let openai_request = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    })
    .expect("request conversion should succeed");

    let serialized = serde_json::to_value(openai_request).expect("serialization should succeed");

    assert!(
        serialized.get("response_format").is_none(),
        "initial tool turn should omit response_format: {serialized:?}"
    );
}

#[test]
fn request_conversion_restores_response_format_after_tool_result() {
    let request = CoreCompletionRequest {
        model: None,
        preamble: None,
        chat_history: OneOrMany::many(vec![
            message::Message::user("Hello, whats the weather in London?"),
            message::Message::Assistant {
                id: None,
                content: OneOrMany::one(message::AssistantContent::tool_call(
                    "call_1",
                    "weather",
                    serde_json::json!({ "city": "London" }),
                )),
            },
            message::Message::tool_result(
                "call_1",
                "The weather in London is all fire and brimstone",
            ),
        ])
        .expect("history should be non-empty"),
        documents: vec![],
        tools: vec![completion::ToolDefinition {
            name: "weather".to_string(),
            description: "Get the weather".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "city": { "type": "string" }
                },
                "required": ["city"]
            }),
        }],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: Some(
            serde_json::from_value(serde_json::json!({
                "title": "WeatherResponse",
                "type": "object",
                "properties": {
                    "city": { "type": "string" },
                    "weather": { "type": "string" }
                },
                "required": ["city", "weather"]
            }))
            .expect("schema should deserialize"),
        ),
        record_telemetry_content: false,
    };

    let openai_request = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    })
    .expect("request conversion should succeed");

    let serialized = serde_json::to_value(openai_request).expect("serialization should succeed");

    assert!(
        serialized.get("response_format").is_some(),
        "follow-up turn should restore response_format: {serialized:?}"
    );
}

#[test]
fn deserialize_llama_cpp_tool_call() {
    let request = r#"{
            "choices": [{
                "finish_reason": "tool_calls",
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{ "type": "function", "function": { "name": "hello_world", "arguments": { "city": "Paris" } }, "id": "xxx" }]
                }
            }],
            "created": 0,
            "model": "gpt-4o-mini",
            "system_fingerprint": "fp_xxx",
            "object": "chat.completion",
            "usage": { "completion_tokens": 13, "prompt_tokens": 255, "total_tokens": 268 },
            "id": "xxx"
        }
        "#;
    let response = serde_json::from_str::<ApiResponse<CompletionResponse>>(request).unwrap();

    let ApiResponse::Ok(response) = response else {
        panic!("expected successful completion response");
    };
    assert_eq!(response.choices.len(), 1);

    let Message::Assistant { tool_calls, .. } = &response.choices[0].message else {
        panic!("expected assistant message");
    };
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "xxx");
    assert_eq!(tool_calls[0].function.name, "hello_world");
    assert_eq!(
        tool_calls[0].function.arguments,
        serde_json::json!({"city": "Paris"})
    );
}

#[test]
fn deserialize_openai_stringified_tool_call() {
    let request = r#"{
            "choices": [{
                "finish_reason": "tool_calls",
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{ "type": "function", "function": { "name": "hello_world", "arguments": "{\"city\":\"Paris\"}" }, "id": "xxx" }]
                }
            }],
            "created": 0,
            "model": "gpt-4o-mini",
            "system_fingerprint": "fp_xxx",
            "object": "chat.completion",
            "usage": { "completion_tokens": 13, "prompt_tokens": 255, "total_tokens": 268 },
            "id": "xxx"
        }
        "#;
    let response = serde_json::from_str::<ApiResponse<CompletionResponse>>(request).unwrap();

    let ApiResponse::Ok(response) = response else {
        panic!("expected successful completion response");
    };
    assert_eq!(response.choices.len(), 1);

    let Message::Assistant { tool_calls, .. } = &response.choices[0].message else {
        panic!("expected assistant message");
    };
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "xxx");
    assert_eq!(tool_calls[0].function.name, "hello_world");
    assert_eq!(
        tool_calls[0].function.arguments,
        serde_json::json!({"city": "Paris"})
    );
}

#[test]
fn deserialize_llama_cpp_response_with_reasoning_content() {
    let request = r#"
        {
            "choices": [
                {
                    "finish_reason": "stop",
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "reasoning_content": "Now I understand the structure better. I need to: ..."
                    }
                }
            ],
            "created": 1776750378,
            "model": "unsloth/Qwen3.6-35B-A3B-GGUF:Q8_0",
            "system_fingerprint": "fp_xxx",
            "object": "chat.completion",
            "usage": {
                "completion_tokens": 920,
                "prompt_tokens": 27806,
                "total_tokens": 28726,
                "prompt_tokens_details": { "cached_tokens": 18698 }
            },
            "id": "chatcmpl-xxxx",
            "timings": {
                "cache_n": 18698,
                "prompt_n": 9108,
                "prompt_ms": 226645.81,
                "prompt_per_token_ms": 24.884256697408873,
                "prompt_per_second": 40.186050648807495,
                "predicted_n": 920,
                "predicted_ms": 177167.955,
                "predicted_per_token_ms": 192.57386413043477,
                "predicted_per_second": 5.192812661860888
            }
        }
        "#;
    let response = serde_json::from_str::<ApiResponse<CompletionResponse>>(request).unwrap();
    let ApiResponse::Ok(response) = response else {
        panic!("expected successful completion response");
    };

    let response: completion::CompletionResponse =
            response
                .normalize(<crate::providers::openai::OpenAICompletionsExt as OpenAICompatibleProvider>::PROVIDER_NAME)
                .unwrap();

    assert_eq!(response.choice.len(), 1);

    let completion::message::AssistantContent::Reasoning(reasoning) = response.choice.first()
    else {
        panic!("expected assistant content to be reasoning");
    };
    assert_eq!(
        reasoning.first_text(),
        Some("Now I understand the structure better. I need to: ...")
    );
}

#[test]
fn pdf_base64_document_serializes_as_file_content_part() {
    let doc = message::UserContent::Document(message::Document {
        data: DocumentSourceKind::Base64("JVBERi0xLjQK".into()),
        media_type: Some(message::DocumentMediaType::PDF),
        additional_params: None,
    });
    let converted: UserContent = doc.try_into().expect("conversion should succeed");
    let json = serde_json::to_value(&converted).expect("serialize");

    assert_eq!(json["type"], "file");
    assert_eq!(
        json["file"]["file_data"],
        "data:application/pdf;base64,JVBERi0xLjQK"
    );
    assert_eq!(json["file"]["filename"], "document.pdf");
    assert!(json["file"].get("file_id").is_none());
}

#[test]
fn file_id_document_serializes_as_file_content_part() {
    let doc = message::UserContent::Document(message::Document {
        data: DocumentSourceKind::FileId("file_abc".into()),
        media_type: None,
        additional_params: None,
    });
    let converted: UserContent = doc.try_into().expect("conversion should succeed");
    let json = serde_json::to_value(&converted).expect("serialize");

    assert_eq!(json["type"], "file");
    assert_eq!(json["file"]["file_id"], "file_abc");
    assert!(json["file"].get("file_data").is_none());
}

#[test]
fn base64_image_without_detail_defaults_to_auto() {
    let image = message::UserContent::Image(message::Image {
        data: DocumentSourceKind::Base64("iVBORw0KGgo=".into()),
        media_type: Some(message::ImageMediaType::PNG),
        detail: None,
        additional_params: None,
    });
    let converted: UserContent = image.try_into().expect("conversion should succeed");
    let UserContent::Image { image_url } = converted else {
        panic!("expected image content");
    };

    assert_eq!(image_url.url, "data:image/png;base64,iVBORw0KGgo=");
    assert_eq!(image_url.detail, Some(ImageDetail::Auto));
}

// Regression guard: callers passing markdown/plain text wrapped in
// `UserContent::Document` should keep getting flattened to `text`.
#[test]
fn non_pdf_document_still_serializes_as_text() {
    let doc = message::UserContent::Document(message::Document {
        data: DocumentSourceKind::String("# Markdown".into()),
        media_type: None,
        additional_params: None,
    });
    let converted: UserContent = doc.try_into().expect("conversion should succeed");
    let json = serde_json::to_value(&converted).expect("serialize");

    assert_eq!(json["type"], "text");
    assert_eq!(json["text"], "# Markdown");
}

#[test]
fn pdf_url_document_returns_conversion_error() {
    let doc = message::UserContent::Document(message::Document {
        data: DocumentSourceKind::Url("https://example.com/x.pdf".into()),
        media_type: Some(message::DocumentMediaType::PDF),
        additional_params: None,
    });
    let res: Result<UserContent, _> = doc.try_into();
    assert!(matches!(
        res,
        Err(message::MessageError::ConversionError(_))
    ));
}

#[test]
fn pdf_raw_document_returns_conversion_error() {
    let doc = message::UserContent::Document(message::Document {
        data: DocumentSourceKind::Raw(b"%PDF-1.4\n".to_vec()),
        media_type: Some(message::DocumentMediaType::PDF),
        additional_params: None,
    });
    let res: Result<UserContent, _> = doc.try_into();
    assert!(matches!(
        res,
        Err(message::MessageError::ConversionError(_))
    ));
}

#[test]
fn file_user_content_deserializes_from_wire_json() {
    let raw = r#"{"type":"file","file":{"file_data":"data:application/pdf;base64,AAAA","filename":"x.pdf"}}"#;
    let parsed: UserContent = serde_json::from_str(raw).expect("deserialize");
    let UserContent::File { file } = parsed else {
        panic!("expected File variant");
    };
    assert_eq!(
        file.file_data.as_deref(),
        Some("data:application/pdf;base64,AAAA")
    );
    assert_eq!(file.filename.as_deref(), Some("x.pdf"));
    assert!(file.file_id.is_none());
}

#[test]
fn file_variant_round_trips_back_to_pdf_document() {
    let wire = UserContent::File {
        file: FileData {
            file_data: Some("data:application/pdf;base64,QUJD".to_string()),
            file_id: None,
            filename: Some("document.pdf".to_string()),
        },
    };
    let rig: message::UserContent = wire.into();
    let message::UserContent::Document(doc) = rig else {
        panic!("expected Document");
    };
    assert_eq!(doc.media_type, Some(message::DocumentMediaType::PDF));
    assert!(matches!(doc.data, DocumentSourceKind::Base64(ref b) if b == "QUJD"));
}

#[test]
fn file_variant_with_file_id_only_round_trips_to_document_file_id() {
    let wire = UserContent::File {
        file: FileData {
            file_data: None,
            file_id: Some("file_abc".to_string()),
            filename: None,
        },
    };
    let rig: message::UserContent = wire.into();
    let message::UserContent::Document(doc) = rig else {
        panic!("expected Document");
    };
    assert_eq!(doc.media_type, None);
    assert!(matches!(doc.data, DocumentSourceKind::FileId(ref id) if id == "file_abc"));

    let converted: UserContent = message::UserContent::Document(doc)
        .try_into()
        .expect("conversion should succeed");
    let json = serde_json::to_value(&converted).expect("serialize");

    assert_eq!(json["type"], "file");
    assert_eq!(json["file"]["file_id"], "file_abc");
    assert!(json["file"].get("file_data").is_none());
}

// Guards against `OneOrMany::many` flattening at the User content site:
// a mixed text + PDF message must produce one User message with both parts.
#[test]
fn mixed_text_and_pdf_user_message_produces_two_content_parts() {
    let user = message::Message::User {
        content: OneOrMany::many(vec![
            message::UserContent::text("What is in this PDF?"),
            message::UserContent::Document(message::Document {
                data: DocumentSourceKind::Base64("JVBERi0K".into()),
                media_type: Some(message::DocumentMediaType::PDF),
                additional_params: None,
            }),
        ])
        .expect("non-empty content"),
    };
    let converted: Vec<Message> = user.try_into().expect("conversion should succeed");
    assert_eq!(converted.len(), 1);
    let Message::User { content, .. } = &converted[0] else {
        panic!("expected user message");
    };
    let parts: Vec<&UserContent> = content.iter().collect();
    assert_eq!(parts.len(), 2);
    assert!(matches!(parts[0], UserContent::Text { .. }));
    assert!(matches!(parts[1], UserContent::File { .. }));
}

#[tokio::test]
async fn completion_preserves_raw_provider_error_json_on_api_error_envelope() {
    use crate::client::CompletionClient;
    use crate::completion::CompletionModel;
    use crate::providers::openai::CompletionsClient;
    use crate::test_utils::RecordingHttpClient;

    let body = r#"{"message":"slow down","type":"rate_limit","code":"rate_limit_exceeded"}"#;
    let http_client = RecordingHttpClient::with_error_response(http::StatusCode::ACCEPTED, body);
    let client = CompletionsClient::builder()
        .api_key("test-key")
        .http_client(http_client)
        .build()
        .expect("build client");
    let model = client.completion_model("gpt-4o-mini");
    let request = model.completion_request("hello").build();

    let error = model
        .completion(request)
        .await
        .expect_err("completion should fail with provider error envelope");

    match &error {
        CompletionError::ProviderResponse(stored) => {
            assert_eq!(stored.body, body);
            assert_eq!(stored.status, Some(http::StatusCode::ACCEPTED));
            assert_eq!(error.provider_response_body(), Some(body));
            assert_eq!(
                error.provider_response_status(),
                Some(http::StatusCode::ACCEPTED)
            );
            let json = error
                .provider_response_json()
                .expect("raw body should be valid JSON")
                .expect("parsed JSON should be present");
            assert_eq!(json["code"], "rate_limit_exceeded");
            assert_eq!(json["type"], "rate_limit");
        }
        other => panic!("expected ProviderResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn completion_http_non_success_preserves_status_and_body() {
    use crate::client::CompletionClient;
    use crate::completion::CompletionModel;
    use crate::providers::openai::CompletionsClient;
    use crate::test_utils::RecordingHttpClient;

    let body = r#"{"error":{"message":"rate limited","type":"rate_limit_error"}}"#;
    let http_client =
        RecordingHttpClient::with_error_response(http::StatusCode::TOO_MANY_REQUESTS, body);
    let client = CompletionsClient::builder()
        .api_key("test-key")
        .http_client(http_client)
        .build()
        .expect("build client");
    let model = client.completion_model("gpt-4o-mini");
    let request = model.completion_request("hello").build();

    let error = model
        .completion(request)
        .await
        .expect_err("completion should fail with non-success status");

    assert!(matches!(error, CompletionError::HttpError(_)));
    assert_eq!(
        error.provider_response_status(),
        Some(http::StatusCode::TOO_MANY_REQUESTS)
    );
    assert_eq!(error.provider_response_body(), Some(body));
    let json = error
        .provider_response_json()
        .expect("raw body should be valid JSON")
        .expect("parsed JSON should be present");
    assert_eq!(json["error"]["type"], "rate_limit_error");
}

// ================================================================
// Coverage additions: conversions, tool choice, finalize paths
// ================================================================

fn rig_image(data: DocumentSourceKind) -> message::UserContent {
    message::UserContent::Image(message::Image {
        data,
        media_type: Some(message::ImageMediaType::PNG),
        detail: None,
        additional_params: None,
    })
}

fn rig_audio(data: DocumentSourceKind) -> message::UserContent {
    message::UserContent::Audio(message::Audio {
        data,
        media_type: None,
        additional_params: None,
    })
}

fn rig_video(
    data: DocumentSourceKind,
    media_type: Option<message::VideoMediaType>,
) -> message::UserContent {
    message::UserContent::Video(message::Video {
        data,
        media_type,
        additional_params: None,
    })
}

fn wire_assistant(content: Vec<AssistantContent>) -> Message {
    Message::Assistant {
        content,
        reasoning: None,
        refusal: None,
        audio: None,
        name: None,
        tool_calls: vec![],
        reasoning_details: vec![],
        images: vec![],
    }
}

fn completion_response_with_message(message: Message) -> CompletionResponse {
    CompletionResponse {
        id: "chatcmpl-1".to_owned(),
        object: "chat.completion".to_owned(),
        created: 0,
        model: "gpt-4o-mini".to_owned(),
        system_fingerprint: None,
        choices: vec![Choice {
            index: 0,
            message,
            logprobs: None,
            finish_reason: "stop".to_owned(),
        }],
        usage: None,
    }
}

fn weather_output_schema() -> serde_json::Value {
    serde_json::json!({
        "title": "WeatherResponse",
        "type": "object",
        "properties": {
            "city": { "type": "string" }
        }
    })
}

fn core_request(history: Vec<message::Message>) -> CoreCompletionRequest {
    CoreCompletionRequest {
        model: None,
        preamble: None,
        chat_history: OneOrMany::many(history).expect("history should be non-empty"),
        documents: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    }
}

#[test]
fn assistant_content_text_and_refusal_convert_to_rig_text() {
    let text: completion::AssistantContent = AssistantContent::Text {
        text: "hello".to_string(),
    }
    .into();
    assert_eq!(text, completion::AssistantContent::text("hello"));

    let refusal: completion::AssistantContent = AssistantContent::Refusal {
        refusal: "no can do".to_string(),
    }
    .into();
    assert_eq!(refusal, completion::AssistantContent::text("no can do"));
}

#[test]
fn tool_result_content_parses_from_str() {
    let parsed: ToolResultContent = "tool output".parse().expect("parse should be infallible");
    assert_eq!(parsed, ToolResultContent::from("tool output".to_string()));
}

#[test]
fn tool_result_content_value_from_string_and_to_array() {
    let string_form = ToolResultContentValue::from_string("one".to_string(), false);
    assert_eq!(
        string_form,
        ToolResultContentValue::String("one".to_string())
    );
    assert_eq!(string_form.as_text(), "one");

    let array_form = ToolResultContentValue::from_string("two".to_string(), true);
    assert_eq!(
        array_form,
        ToolResultContentValue::Array(vec![ToolResultContent::from("two".to_string())])
    );
    assert_eq!(array_form.as_text(), "two");

    // `to_array` is idempotent on arrays and wraps strings.
    assert_eq!(array_form.to_array(), array_form);
    assert_eq!(
        string_form.to_array(),
        ToolResultContentValue::Array(vec![ToolResultContent::from("one".to_string())])
    );
}

#[test]
fn tool_definition_with_strict_sets_flag_and_sanitizes_schema() {
    let def = ToolDefinition::from(completion::ToolDefinition {
        name: "get_weather".to_string(),
        description: "Get the weather".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "city": { "type": "string" } }
        }),
    })
    .with_strict();

    let json = serde_json::to_value(&def).expect("serialize tool definition");
    assert_eq!(json["function"]["strict"], true);
    assert_eq!(
        json["function"]["parameters"]["additionalProperties"],
        false
    );
    assert_eq!(
        json["function"]["parameters"]["required"],
        serde_json::json!(["city"])
    );
}

#[test]
fn tool_choice_serializes_modes_and_function_form() {
    assert_eq!(
        serde_json::to_value(ToolChoice::Auto).expect("serialize auto"),
        serde_json::json!("auto")
    );
    assert_eq!(
        serde_json::to_value(ToolChoice::None).expect("serialize none"),
        serde_json::json!("none")
    );
    assert_eq!(
        serde_json::to_value(ToolChoice::Required).expect("serialize required"),
        serde_json::json!("required")
    );
    assert_eq!(
        serde_json::to_value(ToolChoice::Function {
            name: "get_weather".to_string()
        })
        .expect("serialize function"),
        serde_json::json!({ "type": "function", "function": { "name": "get_weather" } })
    );
}

#[test]
fn tool_choice_deserializes_modes_and_function_form() {
    assert_eq!(
        serde_json::from_str::<ToolChoice>("\"auto\"").expect("deserialize auto"),
        ToolChoice::Auto
    );
    assert_eq!(
        serde_json::from_str::<ToolChoice>("\"none\"").expect("deserialize none"),
        ToolChoice::None
    );
    assert_eq!(
        serde_json::from_str::<ToolChoice>("\"required\"").expect("deserialize required"),
        ToolChoice::Required
    );
    assert_eq!(
        serde_json::from_str::<ToolChoice>(
            r#"{"type":"function","function":{"name":"get_weather"}}"#
        )
        .expect("deserialize function"),
        ToolChoice::function("get_weather")
    );

    let err =
        serde_json::from_str::<ToolChoice>("\"bananas\"").expect_err("unknown mode should fail");
    assert!(err.to_string().contains("unknown tool_choice mode"));
}

#[test]
fn tool_choice_function_constructor_builds_function_variant() {
    assert_eq!(
        ToolChoice::function("subtract"),
        ToolChoice::Function {
            name: "subtract".to_string()
        }
    );
}

#[test]
fn tool_choice_converts_from_rig_tool_choice() {
    assert_eq!(
        ToolChoice::try_from(message::ToolChoice::Auto).expect("auto converts"),
        ToolChoice::Auto
    );
    assert_eq!(
        ToolChoice::try_from(message::ToolChoice::None).expect("none converts"),
        ToolChoice::None
    );
    assert_eq!(
        ToolChoice::try_from(message::ToolChoice::Required).expect("required converts"),
        ToolChoice::Required
    );
    assert_eq!(
        ToolChoice::try_from(message::ToolChoice::Specific {
            function_names: vec!["get_weather".to_string()],
        })
        .expect("single specific tool converts"),
        ToolChoice::Function {
            name: "get_weather".to_string()
        }
    );
}

#[test]
fn tool_choice_specific_with_multiple_names_errors() {
    let err = ToolChoice::try_from(message::ToolChoice::Specific {
        function_names: vec!["first".to_string(), "second".to_string()],
    })
    .expect_err("only exactly one specific tool is supported");
    assert!(err.to_string().contains("exactly one specific tool"));
}

#[test]
fn tool_result_with_image_content_errors() {
    let result = message::ToolResult {
        id: "result-id".to_string(),
        call_id: None,
        content: OneOrMany::one(message::ToolResultContent::image_base64(
            "AAAA",
            Some(message::ImageMediaType::PNG),
            None,
        )),
    };

    let res: Result<Message, _> = result.try_into();
    assert!(
        matches!(&res, Err(message::MessageError::ConversionError(msg)) if msg.contains("images in tool results")),
        "expected image-in-tool-result error, got: {res:?}"
    );
}

#[test]
fn image_url_source_converts_with_default_and_explicit_detail() {
    let converted: UserContent =
        message::UserContent::image_url("https://example.com/cat.png", None, None)
            .try_into()
            .expect("url image converts");
    assert_eq!(
        serde_json::to_value(&converted).expect("serialize"),
        serde_json::json!({
            "type": "image_url",
            "image_url": { "url": "https://example.com/cat.png", "detail": "auto" }
        })
    );

    let low: UserContent = message::UserContent::image_url(
        "https://example.com/cat.png",
        None,
        Some(ImageDetail::Low),
    )
    .try_into()
    .expect("url image converts");
    assert_eq!(
        serde_json::to_value(&low).expect("serialize")["image_url"]["detail"],
        "low"
    );
}

#[test]
fn base64_image_without_media_type_errors() {
    let image = message::UserContent::Image(message::Image {
        data: DocumentSourceKind::Base64("AAAA".into()),
        media_type: None,
        detail: None,
        additional_params: None,
    });

    let res: Result<UserContent, _> = image.try_into();
    assert!(
        matches!(&res, Err(message::MessageError::ConversionError(msg)) if msg.contains("media type")),
        "expected missing-media-type error, got: {res:?}"
    );
}

#[test]
fn image_unsupported_sources_error() {
    let sources = [
        DocumentSourceKind::Raw(vec![0, 1]),
        DocumentSourceKind::FileId("file_1".into()),
        DocumentSourceKind::String("not-an-image".into()),
        DocumentSourceKind::Unknown,
    ];

    for source in sources {
        let res: Result<UserContent, _> = rig_image(source).try_into();
        assert!(
            matches!(res, Err(message::MessageError::ConversionError(_))),
            "expected conversion error"
        );
    }
}

#[test]
fn pdf_string_and_unknown_sources_error() {
    let string_doc = message::UserContent::Document(message::Document {
        data: DocumentSourceKind::String("raw pdf text".into()),
        media_type: Some(message::DocumentMediaType::PDF),
        additional_params: None,
    });
    let res: Result<UserContent, _> = string_doc.try_into();
    assert!(
        matches!(&res, Err(message::MessageError::ConversionError(msg)) if msg.contains("base64-encoded")),
        "expected raw-string PDF error, got: {res:?}"
    );

    let unknown_doc = message::UserContent::Document(message::Document {
        data: DocumentSourceKind::Unknown,
        media_type: Some(message::DocumentMediaType::PDF),
        additional_params: None,
    });
    let res: Result<UserContent, _> = unknown_doc.try_into();
    assert!(
        matches!(&res, Err(message::MessageError::ConversionError(msg)) if msg.contains("no body")),
        "expected missing-body error, got: {res:?}"
    );
}

#[test]
fn url_document_without_media_type_errors() {
    let doc = message::UserContent::document_url("https://example.com/doc.pdf", None);
    let res: Result<UserContent, _> = doc.try_into();
    assert!(
        matches!(&res, Err(message::MessageError::ConversionError(msg)) if msg.contains("base64 or a string")),
        "expected non-base64 document error, got: {res:?}"
    );
}

#[test]
fn base64_audio_converts_to_input_audio() {
    let wav: UserContent = message::UserContent::audio("QUJD", Some(message::AudioMediaType::WAV))
        .try_into()
        .expect("audio converts");
    assert_eq!(
        serde_json::to_value(&wav).expect("serialize"),
        serde_json::json!({
            "type": "input_audio",
            "input_audio": { "data": "QUJD", "format": "wav" }
        })
    );

    // Absent media type falls back to MP3 on the wire.
    let default: UserContent = message::UserContent::audio("QUJD", None)
        .try_into()
        .expect("audio converts");
    assert_eq!(
        serde_json::to_value(&default).expect("serialize")["input_audio"]["format"],
        "mp3"
    );
}

#[test]
fn audio_unsupported_sources_error() {
    let sources = [
        DocumentSourceKind::Url("https://example.com/a.wav".into()),
        DocumentSourceKind::Raw(vec![0, 1]),
        DocumentSourceKind::FileId("file_1".into()),
        DocumentSourceKind::String("not-audio".into()),
        DocumentSourceKind::Unknown,
    ];

    for source in sources {
        let res: Result<UserContent, _> = rig_audio(source).try_into();
        assert!(
            matches!(res, Err(message::MessageError::ConversionError(_))),
            "expected conversion error"
        );
    }
}

#[test]
fn user_tool_result_content_errors() {
    let content = message::UserContent::ToolResult(message::ToolResult {
        id: "call_1".to_string(),
        call_id: None,
        content: OneOrMany::one(message::ToolResultContent::text("tool output")),
    });
    let res: Result<UserContent, _> = content.try_into();
    assert!(
        matches!(&res, Err(message::MessageError::ConversionError(msg)) if msg.contains("unsupported format")),
        "expected tool-result unsupported-format error, got: {res:?}"
    );
}

#[test]
fn base64_video_converts_to_data_uri() {
    let converted: UserContent =
        message::UserContent::video("QUJD", Some(message::VideoMediaType::MP4))
            .try_into()
            .expect("base64 video converts");
    assert_eq!(
        serde_json::to_value(&converted).expect("serialize"),
        serde_json::json!({
            "type": "video_url",
            "video_url": { "url": "data:video/mp4;base64,QUJD" }
        })
    );
}

#[test]
fn video_unsupported_sources_error() {
    let sources = [
        DocumentSourceKind::Raw(vec![0, 1]),
        DocumentSourceKind::FileId("file_1".into()),
        DocumentSourceKind::String("not-video".into()),
        DocumentSourceKind::Unknown,
    ];

    for source in sources {
        let res: Result<UserContent, _> =
            rig_video(source, Some(message::VideoMediaType::MP4)).try_into();
        assert!(
            matches!(res, Err(message::MessageError::ConversionError(_))),
            "expected conversion error"
        );
    }

    // Base64 without a media type cannot build a data URI.
    let res: Result<UserContent, _> =
        rig_video(DocumentSourceKind::Base64("QUJD".into()), None).try_into();
    assert!(
        matches!(&res, Err(message::MessageError::ConversionError(msg)) if msg.contains("media type required")),
        "expected missing-media-type error, got: {res:?}"
    );
}

#[test]
fn assistant_image_content_errors() {
    let content = OneOrMany::one(message::AssistantContent::image_base64(
        "AAAA",
        Some(message::ImageMediaType::PNG),
        None,
    ));

    let res: Result<Vec<Message>, _> = content.try_into();
    assert!(
        matches!(&res, Err(message::MessageError::ConversionError(msg)) if msg.contains("image content")),
        "expected assistant-image error, got: {res:?}"
    );
}

#[test]
fn openai_tool_call_converts_to_rig_tool_call() {
    let call = ToolCall {
        id: "call_9".to_string(),
        r#type: ToolType::Function,
        function: Function {
            name: "get_weather".to_string(),
            arguments: serde_json::json!({ "city": "Paris" }),
        },
    };

    let rig: message::ToolCall = call.into();
    assert_eq!(rig.id, "call_9");
    assert_eq!(rig.call_id, None);
    assert_eq!(rig.function.name, "get_weather");
    assert_eq!(
        rig.function.arguments,
        serde_json::json!({ "city": "Paris" })
    );
    assert_eq!(rig.signature, None);
    assert_eq!(rig.additional_params, None);
}

#[test]
fn refusal_assistant_content_maps_to_rig_text() {
    let assistant = wire_assistant(vec![AssistantContent::Refusal {
        refusal: "blocked".to_string(),
    }]);

    let rig: message::Message = assistant.try_into().expect("refusal converts");
    let message::Message::Assistant { content, .. } = rig else {
        panic!("expected assistant message");
    };
    let items: Vec<_> = content.into_iter().collect();
    assert_eq!(items.len(), 1);
    assert!(
        matches!(&items[0], message::AssistantContent::Text(text) if text.text == "blocked"),
        "expected refusal to map to text, got: {items:?}"
    );
}

#[test]
fn empty_assistant_message_errors_on_rig_conversion() {
    let res: Result<message::Message, _> = wire_assistant(vec![]).try_into();
    assert!(
        matches!(&res, Err(message::MessageError::ConversionError(msg)) if msg.contains("Neither `content` nor `tool_calls`")),
        "expected empty-assistant error, got: {res:?}"
    );
}

#[test]
fn tool_result_message_maps_back_to_rig_user_tool_result() {
    let tool = Message::ToolResult {
        tool_call_id: "call_1".to_string(),
        content: ToolResultContentValue::Array(vec![
            ToolResultContent::from("first".to_string()),
            ToolResultContent::from("second".to_string()),
        ]),
    };

    let rig: message::Message = tool.try_into().expect("tool result converts");
    let message::Message::User { content } = rig else {
        panic!("expected user message");
    };
    let items: Vec<_> = content.into_iter().collect();
    assert_eq!(items.len(), 1);
    assert!(
        matches!(&items[0], message::UserContent::ToolResult(result) if result.id == "call_1"),
        "expected tool result content, got: {items:?}"
    );
    // The array content is flattened into a single joined text block.
    let message::UserContent::ToolResult(result) = &items[0] else {
        panic!("expected tool result content");
    };
    let blocks: Vec<_> = result.content.iter().collect();
    assert_eq!(blocks.len(), 1);
    assert!(
        matches!(&blocks[0], message::ToolResultContent::Text(text) if text.text == "first\nsecond"),
        "expected joined text, got: {blocks:?}"
    );
}

#[test]
fn system_message_maps_to_rig_user_text() {
    let system = Message::System {
        content: OneOrMany::one(SystemContent::from("sys prompt".to_string())),
        name: None,
    };

    let rig: message::Message = system.try_into().expect("system converts");
    let message::Message::User { content } = rig else {
        panic!("expected user message");
    };
    assert!(
        matches!(content.first(), message::UserContent::Text(text) if text.text == "sys prompt"),
        "expected system text to survive, got: {content:?}"
    );
}

#[test]
fn wire_image_and_audio_map_back_to_rig_content() {
    let image = UserContent::Image {
        image_url: ImageUrl {
            url: "https://example.com/cat.png".to_string(),
            detail: Some(ImageDetail::Low),
        },
    };
    let rig: message::UserContent = image.into();
    assert!(
        matches!(&rig, message::UserContent::Image(img)
                if matches!(&img.data, DocumentSourceKind::Url(url) if url == "https://example.com/cat.png")
                    && img.detail == Some(ImageDetail::Low)
                    && img.media_type.is_none()),
        "expected url-backed rig image, got: {rig:?}"
    );

    let audio = UserContent::Audio {
        input_audio: InputAudio {
            data: "QUJD".to_string(),
            format: AudioMediaType::WAV,
        },
    };
    let rig: message::UserContent = audio.into();
    assert!(
        matches!(&rig, message::UserContent::Audio(audio)
                if matches!(&audio.data, DocumentSourceKind::Base64(data) if data == "QUJD")
                    && audio.media_type == Some(AudioMediaType::WAV)),
        "expected base64 rig audio, got: {rig:?}"
    );
}

#[test]
fn file_with_non_pdf_data_url_maps_to_string_document() {
    let wire = UserContent::File {
        file: FileData {
            file_data: Some("data:text/plain;base64,QUJD".to_string()),
            file_id: None,
            filename: None,
        },
    };

    let rig: message::UserContent = wire.into();
    let message::UserContent::Document(doc) = rig else {
        panic!("expected document");
    };
    assert_eq!(doc.media_type, Some(message::DocumentMediaType::PDF));
    assert!(
        matches!(&doc.data, DocumentSourceKind::String(data) if data == "data:text/plain;base64,QUJD"),
        "expected the unrecognized data URI kept as a string, got: {:?}",
        doc.data
    );
}

#[test]
fn file_with_neither_data_nor_id_maps_to_empty_text() {
    let wire = UserContent::File {
        file: FileData {
            file_data: None,
            file_id: None,
            filename: None,
        },
    };

    let rig: message::UserContent = wire.into();
    assert!(
        matches!(rig, message::UserContent::Text(ref text) if text.text.is_empty()),
        "expected empty text fallback, got: {rig:?}"
    );
}

#[test]
fn user_content_from_string_str_and_parse() {
    assert_eq!(
        UserContent::from("hello".to_string()),
        UserContent::Text {
            text: "hello".to_string()
        }
    );
    assert_eq!(
        UserContent::from("hi"),
        UserContent::Text {
            text: "hi".to_string()
        }
    );
    let parsed: UserContent = "parsed".parse().expect("parse should be infallible");
    assert_eq!(
        parsed,
        UserContent::Text {
            text: "parsed".to_string()
        }
    );
}

#[test]
fn system_content_parses_from_str() {
    let parsed: SystemContent = "sys".parse().expect("parse should be infallible");
    assert_eq!(parsed, SystemContent::from("sys".to_string()));
}

#[test]
fn normalize_without_choices_errors() {
    let mut response =
        completion_response_with_message(wire_assistant(vec![AssistantContent::Text {
            text: "hello".to_string(),
        }]));
    response.choices.clear();

    let err = response
        .normalize("openai")
        .expect_err("response without choices should fail");
    assert!(
        err.to_string().contains("no choices"),
        "expected no-choices error, got: {err}"
    );
}

#[test]
fn normalize_non_assistant_choice_errors() {
    let response = completion_response_with_message(Message::User {
        content: OneOrMany::one(UserContent::Text {
            text: "hi".to_string(),
        }),
        name: None,
    });

    let err = response
        .normalize("openai")
        .expect_err("non-assistant choice should fail");
    assert!(
        err.to_string().contains("valid message"),
        "expected invalid-message error, got: {err}"
    );
}

#[test]
fn normalize_empty_assistant_choice_errors() {
    let response = completion_response_with_message(wire_assistant(vec![]));

    let err = response
        .normalize("openai")
        .expect_err("assistant without content or tool calls should fail");
    assert!(
        err.to_string().contains("empty"),
        "expected empty-message error, got: {err}"
    );
}

#[test]
fn normalize_maps_refusal_content_to_text() {
    let mut response =
        completion_response_with_message(wire_assistant(vec![AssistantContent::Refusal {
            refusal: "blocked".to_string(),
        }]));
    response.choices[0].finish_reason = String::new();

    let normalized = response.normalize("openai").expect("refusal normalizes");
    assert_eq!(normalized.choice.len(), 1);
    assert!(
        matches!(
            normalized.choice.first(),
            completion::message::AssistantContent::Text(text) if text.text == "blocked"
        ),
        "expected refusal text, got: {:?}",
        normalized.choice.first()
    );
}

#[test]
fn provider_response_ext_reports_id_model_messages_and_usage() {
    let response = completion_response_with_message(wire_assistant(vec![AssistantContent::Text {
        text: "hello".to_string(),
    }]));

    assert_eq!(response.get_response_id(), Some("chatcmpl-1".to_string()));
    assert_eq!(
        response.get_response_model_name(),
        Some("gpt-4o-mini".to_string())
    );
    assert_eq!(response.get_output_messages().len(), 1);
    assert!(response.get_usage().is_none());
}

#[test]
fn text_response_is_none_without_assistant_choices() {
    let mut response =
        completion_response_with_message(wire_assistant(vec![AssistantContent::Text {
            text: "hello".to_string(),
        }]));
    response.choices.clear();
    assert_eq!(response.get_text_response(), None);

    // Non-assistant messages contribute nothing either.
    let tool_only = completion_response_with_message(Message::ToolResult {
        tool_call_id: "call_1".to_string(),
        content: ToolResultContentValue::String("done".to_string()),
    });
    assert_eq!(tool_only.get_text_response(), None);
}

#[test]
fn text_response_is_none_for_empty_assistant_without_refusal() {
    let response = completion_response_with_message(wire_assistant(vec![]));
    assert_eq!(response.get_text_response(), None);
}

#[test]
fn usage_display_formats_prompt_and_total_tokens() {
    let usage = Usage {
        prompt_tokens: 12,
        completion_tokens: Some(34),
        total_tokens: 46,
        ..Usage::default()
    };
    assert_eq!(usage.to_string(), "Prompt tokens: 12 Total tokens: 46");
}

#[test]
fn default_streaming_detail_hooks_return_none() {
    use crate::providers::openai::OpenAICompletionsExt;

    let ext = OpenAICompletionsExt::default();
    assert!(
        <OpenAICompletionsExt as OpenAICompatibleProvider>::streaming_detail_reasoning(
            &ext,
            &serde_json::json!({ "type": "reasoning.encrypted" }),
        )
        .is_none()
    );
    assert!(
        <OpenAICompletionsExt as OpenAICompatibleProvider>::decorate_streaming_tool_call(
            &ext,
            &serde_json::json!({ "type": "reasoning.encrypted" }),
        )
        .is_none()
    );
}

#[test]
fn completion_model_builders_toggle_flags() {
    use crate::client::CompletionClient;
    use crate::providers::openai::CompletionsClient;
    use crate::test_utils::RecordingHttpClient;

    let client = CompletionsClient::builder()
        .api_key("test-key")
        .http_client(RecordingHttpClient::new("{}"))
        .build()
        .expect("build client");
    let model = client.completion_model("gpt-4o-mini");
    assert!(!model.strict_tools);
    assert!(!model.tool_result_array_content);

    let model = model.with_strict_tools().with_tool_result_array_content();
    assert!(model.strict_tools);
    assert!(model.tool_result_array_content);
}

#[test]
fn joined_text_parts_concatenates_text_parts_in_order() {
    let parts = serde_json::json!([
        { "type": "text", "text": "a" },
        { "type": "image_url", "image_url": { "url": "https://example.com/x.png" } },
        { "type": "text", "text": "b" },
        { "type": "text" },
    ]);
    assert_eq!(
        joined_text_parts(parts.as_array().expect("parts should be an array")),
        "ab"
    );
    assert_eq!(joined_text_parts(&[]), "");
}

#[test]
fn request_conversion_drops_tools_and_schema_for_unsupported_provider() {
    let request = CoreCompletionRequest {
        tools: vec![completion::ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get the weather".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "city": { "type": "string" } }
            }),
        }],
        tool_choice: Some(message::ToolChoice::Required),
        output_schema: Some(
            serde_json::from_value(weather_output_schema()).expect("schema should deserialize"),
        ),
        ..core_request(vec![message::Message::user("hello")])
    };

    let converted = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: false,
        supports_tools: false,
    })
    .expect("request conversion should succeed");

    let serialized = serde_json::to_value(converted).expect("serialization should succeed");
    assert!(
        serialized.get("tools").is_none(),
        "tools should be dropped: {serialized:?}"
    );
    assert!(
        serialized.get("tool_choice").is_none(),
        "tool_choice should be dropped: {serialized:?}"
    );
    assert!(
        serialized.get("response_format").is_none(),
        "response_format should be dropped: {serialized:?}"
    );
}

#[test]
fn response_format_merges_with_existing_additional_params() {
    let request = CoreCompletionRequest {
        additional_params: Some(serde_json::json!({ "top_p": 0.5 })),
        output_schema: Some(
            serde_json::from_value(weather_output_schema()).expect("schema should deserialize"),
        ),
        ..core_request(vec![message::Message::user("hello")])
    };

    let converted = CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    })
    .expect("request conversion should succeed");

    let serialized = serde_json::to_value(converted).expect("serialization should succeed");
    assert_eq!(
        serialized["top_p"], 0.5,
        "existing params survive: {serialized:?}"
    );
    assert_eq!(
        serialized["response_format"]["json_schema"]["name"], "WeatherResponse",
        "schema name comes from the schema title: {serialized:?}"
    );
    assert_eq!(serialized["response_format"]["json_schema"]["strict"], true);
}

#[test]
fn empty_assistant_content_is_omitted_from_the_wire() {
    // `content` carries `skip_serializing_if = "Vec::is_empty"`: an
    // assistant turn with no content (pure tool-call scaffolding)
    // serializes without a `content` field at all.
    let json = serde_json::to_value(wire_assistant(vec![])).expect("serialize");
    assert!(json.get("content").is_none(), "got: {json:?}");
}

#[tokio::test]
async fn completion_logs_request_and_response_bodies_at_trace_level() {
    use crate::client::CompletionClient;
    use crate::completion::CompletionModel;
    use crate::providers::openai::CompletionsClient;
    use crate::test_utils::{RecordingHttpClient, scoped_tracing_subscriber_guard};

    // Serialize against tests that install scoped tracing subscribers.
    let _guard = scoped_tracing_subscriber_guard().await;

    let body = r#"{
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 0,
            "model": "gpt-4o-mini",
            "system_fingerprint": null,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hello" },
                "logprobs": null,
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        }"#;
    let http_client = RecordingHttpClient::new(body);
    let client = CompletionsClient::builder()
        .api_key("test-key")
        .http_client(http_client)
        .build()
        .expect("build client");
    let model = client.completion_model("gpt-4o-mini");
    let request = model.completion_request("hello").build();

    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_test_writer()
        .finish();
    let response =
        tracing::subscriber::with_default(subscriber, || async { model.completion(request).await })
            .await
            .expect("completion should succeed with trace subscriber installed");

    assert_eq!(response.choice.len(), 1);
}

/// Malformed `additional_params.tools` is user config (external): the
/// conversion fails as a typed error naming the offending payload.
#[test]
fn malformed_additional_params_tools_fail_loudly() {
    let request = crate::completion::CompletionRequest {
        additional_params: Some(serde_json::json!({"tools": "not-an-array"})),
        model: None,
        preamble: None,
        chat_history: crate::OneOrMany::one("Hello".into()),
        documents: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        output_schema: None,
        record_telemetry_content: false,
    };
    let error = match CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    }) {
        Err(error) => error,
        Ok(_) => panic!("a non-array tools payload must fail the conversion"),
    };
    let message = error.to_string();
    assert!(
        message.contains("`additional_params.tools`"),
        "the error names the payload: {message}"
    );

    let request = crate::completion::CompletionRequest {
        additional_params: Some(serde_json::json!({
            "tools": [
                {"type": "function", "function": {"description": "no name"}}
            ]
        })),
        model: None,
        preamble: None,
        chat_history: crate::OneOrMany::one("Hello".into()),
        documents: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        output_schema: None,
        record_telemetry_content: false,
    };
    let error = match CompletionRequest::try_from(OpenAIRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        strict_tools: false,
        tool_result_array_content: false,
        supports_response_format: true,
        supports_tools: true,
    }) {
        Err(error) => error,
        Ok(_) => panic!("a function tool missing its name must fail the conversion"),
    };
    assert!(
        error.to_string().contains("Invalid function tool"),
        "the error names the invalid entry: {error}"
    );
}

/// Upstream's truncated-turn rule (adopted 2026-08): a turn the provider
/// cut short — output budget (`length`) or content filter — may carry no
/// content; the finish reason and usage are the story. A *completed*
/// empty turn stays the shared provider defect.
#[test]
fn a_cut_short_turn_may_be_contentless_but_a_completed_empty_turn_may_not() {
    use crate::completion::FinishReason;

    fn empty_choice_response(finish_reason: &str) -> CompletionResponse {
        serde_json::from_value(serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 0,
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": null},
                "finish_reason": finish_reason
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 30, "total_tokens": 40}
        }))
        .expect("wire shape decodes")
    }

    // Cut short by the output budget: normalizes with the reason and usage.
    let parsed = empty_choice_response("length")
        .normalize("openai")
        .expect("a budget-cut turn is not a provider defect");
    assert_eq!(parsed.finish_reason(), Some(FinishReason::Length));
    assert_eq!(parsed.usage.total_tokens, 40);
    assert!(matches!(
        parsed.choice.first(),
        crate::completion::AssistantContent::Text(text) if text.text.is_empty()
    ));

    // Cut short by the content filter: same rule.
    let parsed = empty_choice_response("content_filter")
        .normalize("openai")
        .expect("a filtered turn is not a provider defect");
    assert_eq!(parsed.finish_reason(), Some(FinishReason::ContentFilter));

    // A completed empty turn stays the shared defect.
    let error = empty_choice_response("stop")
        .normalize("openai")
        .expect_err("a completed empty turn is a defect");
    assert!(
        error.to_string().contains("(empty)"),
        "the empty-response guard stays: {error}"
    );
}
