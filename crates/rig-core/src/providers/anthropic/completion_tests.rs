use super::*;
use serde_json::json;
use serde_path_to_error::deserialize;

#[test]
fn missing_max_tokens_defaults_to_64k() {
    let request = CompletionRequest {
        model: None,
        preamble: None,
        chat_history: OneOrMany::one("hi".into()),
        documents: vec![],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    };

    let converted = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: false,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .expect("request without max_tokens should convert using the provider default");

    assert_eq!(converted.max_tokens, DEFAULT_MAX_TOKENS);
}

#[test]
fn system_role_message_deserializes_and_round_trips() {
    let message: Message = serde_json::from_str(
        r#"
        {
            "role": "system",
            "content": "From now on, require explicit type annotations."
        }
        "#,
    )
    .unwrap();

    assert_eq!(message.role, Role::System);

    let generic: message::Message = message.try_into().unwrap();
    assert_eq!(
        generic,
        message::Message::System {
            content: "From now on, require explicit type annotations.".to_string()
        }
    );

    let provider: Message = generic.try_into().unwrap();
    assert_eq!(provider.role, Role::System);
}

#[test]
fn test_deserialize_message() {
    let assistant_message_json = r#"
        {
            "role": "assistant",
            "content": "\n\nHello there, how may I assist you today?"
        }
        "#;

    let assistant_message_json2 = r#"
        {
            "role": "assistant",
            "content": [
                {
                    "type": "text",
                    "text": "\n\nHello there, how may I assist you today?"
                },
                {
                    "type": "tool_use",
                    "id": "toolu_01A09q90qw90lq917835lq9",
                    "name": "get_weather",
                    "input": {"location": "San Francisco, CA"}
                }
            ]
        }
        "#;

    let user_message_json = r#"
        {
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/jpeg",
                        "data": "/9j/4AAQSkZJRg..."
                    }
                },
                {
                    "type": "text",
                    "text": "What is in this image?"
                },
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_01A09q90qw90lq917835lq9",
                    "content": "15 degrees"
                }
            ]
        }
        "#;

    let assistant_message: Message = {
        let jd = &mut serde_json::Deserializer::from_str(assistant_message_json);
        deserialize(jd).unwrap_or_else(|err| {
            panic!("Deserialization error at {}: {}", err.path(), err);
        })
    };

    let assistant_message2: Message = {
        let jd = &mut serde_json::Deserializer::from_str(assistant_message_json2);
        deserialize(jd).unwrap_or_else(|err| {
            panic!("Deserialization error at {}: {}", err.path(), err);
        })
    };

    let user_message: Message = {
        let jd = &mut serde_json::Deserializer::from_str(user_message_json);
        deserialize(jd).unwrap_or_else(|err| {
            panic!("Deserialization error at {}: {}", err.path(), err);
        })
    };

    let Message { role, content } = assistant_message;
    assert_eq!(role, Role::Assistant);
    assert_eq!(
        content.first(),
        Content::Text {
            text: "\n\nHello there, how may I assist you today?".to_owned(),
            citations: Vec::new(),
            cache_control: None,
        }
    );

    let Message { role, content } = assistant_message2;
    {
        assert_eq!(role, Role::Assistant);
        assert_eq!(content.len(), 2);

        let mut iter = content.into_iter();

        match iter.next().unwrap() {
            Content::Text { text, .. } => {
                assert_eq!(text, "\n\nHello there, how may I assist you today?");
            }
            _ => panic!("Expected text content"),
        }

        match iter.next().unwrap() {
            Content::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_01A09q90qw90lq917835lq9");
                assert_eq!(name, "get_weather");
                assert_eq!(input, json!({"location": "San Francisco, CA"}));
            }
            _ => panic!("Expected tool use content"),
        }

        assert_eq!(iter.next(), None);
    }

    let Message { role, content } = user_message;
    {
        assert_eq!(role, Role::User);
        assert_eq!(content.len(), 3);

        let mut iter = content.into_iter();

        match iter.next().unwrap() {
            Content::Image { source, .. } => {
                assert_eq!(
                    source,
                    ImageSource::Base64 {
                        data: "/9j/4AAQSkZJRg...".to_owned(),
                        media_type: ImageFormat::JPEG,
                    }
                );
            }
            _ => panic!("Expected image content"),
        }

        match iter.next().unwrap() {
            Content::Text { text, .. } => {
                assert_eq!(text, "What is in this image?");
            }
            _ => panic!("Expected text content"),
        }

        match iter.next().unwrap() {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_01A09q90qw90lq917835lq9");
                assert_eq!(
                    content.first(),
                    ToolResultContent::Text {
                        text: "15 degrees".to_owned()
                    }
                );
                assert_eq!(is_error, None);
            }
            _ => panic!("Expected tool result content"),
        }

        assert_eq!(iter.next(), None);
    }
}

#[test]
fn test_message_to_message_conversion() {
    let user_message: Message = serde_json::from_str(
        r#"
        {
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/jpeg",
                        "data": "/9j/4AAQSkZJRg..."
                    }
                },
                {
                    "type": "text",
                    "text": "What is in this image?"
                },
                {
                    "type": "document",
                    "source": {
                        "type": "base64",
                        "data": "base64_encoded_pdf_data",
                        "media_type": "application/pdf"
                    }
                }
            ]
        }
        "#,
    )
    .unwrap();

    let assistant_message = Message {
        role: Role::Assistant,
        content: OneOrMany::one(Content::ToolUse {
            id: "toolu_01A09q90qw90lq917835lq9".to_string(),
            name: "get_weather".to_string(),
            input: json!({"location": "San Francisco, CA"}),
        }),
    };

    let tool_message = Message {
        role: Role::User,
        content: OneOrMany::one(Content::ToolResult {
            tool_use_id: "toolu_01A09q90qw90lq917835lq9".to_string(),
            content: OneOrMany::one(ToolResultContent::Text {
                text: "15 degrees".to_string(),
            }),
            is_error: None,
            cache_control: None,
        }),
    };

    let converted_user_message: message::Message = user_message.clone().try_into().unwrap();
    let converted_assistant_message: message::Message =
        assistant_message.clone().try_into().unwrap();
    let converted_tool_message: message::Message = tool_message.clone().try_into().unwrap();

    match converted_user_message.clone() {
        message::Message::User { content } => {
            assert_eq!(content.len(), 3);

            let mut iter = content.into_iter();

            match iter.next().unwrap() {
                message::UserContent::Image(message::Image {
                    data, media_type, ..
                }) => {
                    assert_eq!(data, DocumentSourceKind::base64("/9j/4AAQSkZJRg..."));
                    assert_eq!(media_type, Some(message::ImageMediaType::JPEG));
                }
                _ => panic!("Expected image content"),
            }

            match iter.next().unwrap() {
                message::UserContent::Text(message::Text { text, .. }) => {
                    assert_eq!(text, "What is in this image?");
                }
                _ => panic!("Expected text content"),
            }

            match iter.next().unwrap() {
                message::UserContent::Document(message::Document {
                    data, media_type, ..
                }) => {
                    assert_eq!(
                        data,
                        DocumentSourceKind::String("base64_encoded_pdf_data".into())
                    );
                    assert_eq!(media_type, Some(message::DocumentMediaType::PDF));
                }
                _ => panic!("Expected document content"),
            }

            assert_eq!(iter.next(), None);
        }
        _ => panic!("Expected user message"),
    }

    match converted_tool_message.clone() {
        message::Message::User { content } => {
            let message::ToolResult { id, content, .. } = match content.first() {
                message::UserContent::ToolResult(tool_result) => tool_result,
                _ => panic!("Expected tool result content"),
            };
            assert_eq!(id, "toolu_01A09q90qw90lq917835lq9");
            match content.first() {
                message::ToolResultContent::Text(message::Text { text, .. }) => {
                    assert_eq!(text, "15 degrees");
                }
                _ => panic!("Expected text content"),
            }
        }
        _ => panic!("Expected tool result content"),
    }

    match converted_assistant_message.clone() {
        message::Message::Assistant { content, .. } => {
            assert_eq!(content.len(), 1);

            match content.first() {
                message::AssistantContent::ToolCall(message::ToolCall { id, function, .. }) => {
                    assert_eq!(id, "toolu_01A09q90qw90lq917835lq9");
                    assert_eq!(function.name, "get_weather");
                    assert_eq!(function.arguments, json!({"location": "San Francisco, CA"}));
                }
                _ => panic!("Expected tool call content"),
            }
        }
        _ => panic!("Expected assistant message"),
    }

    let original_user_message: Message = converted_user_message.try_into().unwrap();
    let original_assistant_message: Message = converted_assistant_message.try_into().unwrap();
    let original_tool_message: Message = converted_tool_message.try_into().unwrap();

    assert_eq!(user_message, original_user_message);
    assert_eq!(assistant_message, original_assistant_message);
    assert_eq!(tool_message, original_tool_message);
}

#[test]
fn test_content_format_conversion() {
    use crate::completion::message::ContentFormat;

    let source_type: SourceType = ContentFormat::Url.try_into().unwrap();
    assert_eq!(source_type, SourceType::URL);

    let content_format: ContentFormat = SourceType::URL.into();
    assert_eq!(content_format, ContentFormat::Url);

    let source_type: SourceType = ContentFormat::Base64.try_into().unwrap();
    assert_eq!(source_type, SourceType::BASE64);

    let content_format: ContentFormat = SourceType::BASE64.into();
    assert_eq!(content_format, ContentFormat::Base64);

    let source_type: SourceType = ContentFormat::String.try_into().unwrap();
    assert_eq!(source_type, SourceType::TEXT);

    let content_format: ContentFormat = SourceType::TEXT.into();
    assert_eq!(content_format, ContentFormat::String);
}

#[test]
fn test_cache_control_serialization() {
    // Test SystemContent with cache_control
    let system = SystemContent::Text {
        text: "You are a helpful assistant.".to_string(),
        cache_control: Some(CacheControl::ephemeral()),
    };
    let json = serde_json::to_string(&system).unwrap();
    assert!(json.contains(r#""cache_control":{"type":"ephemeral"}"#));
    assert!(json.contains(r#""type":"text""#));

    // Test SystemContent without cache_control (should not have cache_control field)
    let system_no_cache = SystemContent::Text {
        text: "Hello".to_string(),
        cache_control: None,
    };
    let json_no_cache = serde_json::to_string(&system_no_cache).unwrap();
    assert!(!json_no_cache.contains("cache_control"));

    // Test Content::Text with cache_control
    let content = Content::Text {
        text: "Test message".to_string(),
        citations: Vec::new(),
        cache_control: Some(CacheControl::ephemeral()),
    };
    let json_content = serde_json::to_string(&content).unwrap();
    assert!(json_content.contains(r#""cache_control":{"type":"ephemeral"}"#));

    // Test apply_cache_control function
    let mut system_vec = vec![SystemContent::Text {
        text: "System prompt".to_string(),
        cache_control: None,
    }];
    let mut messages = vec![
        Message {
            role: Role::User,
            content: OneOrMany::one(Content::Text {
                text: "First message".to_string(),
                citations: Vec::new(),
                cache_control: None,
            }),
        },
        Message {
            role: Role::Assistant,
            content: OneOrMany::one(Content::Text {
                text: "Response".to_string(),
                citations: Vec::new(),
                cache_control: None,
            }),
        },
    ];

    apply_cache_control(&mut system_vec, &mut messages);

    // System should have cache_control
    match &system_vec[0] {
        SystemContent::Text { cache_control, .. } => {
            assert!(cache_control.is_some());
        }
    }

    // Only the last content block of last message should have cache_control
    // First message should NOT have cache_control
    for content in messages[0].content.iter() {
        if let Content::Text { cache_control, .. } = content {
            assert!(cache_control.is_none());
        }
    }

    // Last message SHOULD have cache_control
    for content in messages[1].content.iter() {
        if let Content::Text { cache_control, .. } = content {
            assert!(cache_control.is_some());
        }
    }
}

fn generic_tool(name: &str) -> completion::ToolDefinition {
    completion::ToolDefinition {
        name: name.to_string(),
        description: format!("{name} description"),
        parameters: json!({
            "type": "object",
            "properties": {}
        }),
    }
}

fn completion_request_with_tools(
    tools: Vec<completion::ToolDefinition>,
    additional_params: Option<serde_json::Value>,
) -> CompletionRequest {
    CompletionRequest {
        model: None,
        preamble: Some("System prompt".to_string()),
        chat_history: OneOrMany::one(message::Message::from("Hello")),
        documents: Vec::new(),
        tools,
        temperature: None,
        max_tokens: Some(64),
        tool_choice: None,
        additional_params,
        output_schema: None,
        record_telemetry_content: false,
    }
}

fn completion_request_with_history(
    chat_history: Vec<message::Message>,
    preamble: Option<String>,
) -> CompletionRequest {
    CompletionRequest {
        model: None,
        preamble,
        chat_history: OneOrMany::many(chat_history).unwrap(),
        documents: Vec::new(),
        tools: Vec::new(),
        temperature: None,
        max_tokens: Some(64),
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    }
}

/// A tool result whose id matches no prior assistant `tool_use` is an
/// orphan: the conversion fails loudly, naming the id and the history
/// index, instead of forwarding a request Anthropic would reject (or
/// worse, attribute to the wrong call).
#[test]
fn orphan_tool_result_history_fails_request_conversion() {
    let request = completion_request_with_history(
        vec![
            message::Message::user("What is the weather in London?"),
            message::Message::tool_result("toolu_orphan", "15 degrees"),
        ],
        None,
    );

    let error = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: false,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .expect_err("an orphan tool result must fail request conversion");
    assert!(
        error.to_string().contains(
            "tool result \"toolu_orphan\" has no matching tool call in the conversation \
                 history"
        ),
        "unexpected error: {error}"
    );
    assert!(
        error.to_string().contains("history index 1"),
        "the error must name the message index: {error}"
    );

    // A correlated history (assistant tool call before the result) passes.
    let request = completion_request_with_history(
        vec![
            message::Message::user("What is the weather in London?"),
            message::Message::Assistant {
                id: None,
                content: OneOrMany::one(message::AssistantContent::tool_call(
                    "toolu_ok",
                    "get_weather",
                    json!({"city": "London"}),
                )),
            },
            message::Message::tool_result("toolu_ok", "15 degrees"),
        ],
        None,
    );
    let converted = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: false,
        automatic_caching: false,
        automatic_caching_ttl: None,
    });
    assert!(converted.is_ok(), "a correlated history must convert");
}

fn system_has_cache_control(value: &serde_json::Value) -> bool {
    value["system"]
        .as_array()
        .and_then(|blocks| blocks.last())
        .and_then(|block| block.get("cache_control"))
        .is_some()
}

fn last_message_has_cache_control(value: &serde_json::Value) -> bool {
    value["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .and_then(|message| message["content"].as_array())
        .and_then(|content| content.last())
        .and_then(|content| content.get("cache_control"))
        .is_some()
}

#[test]
fn documents_hoist_leading_and_mid_conversation_system_messages() {
    let mut request = completion_request_with_history(
        vec![
            message::Message::System {
                content: "Global history instruction.".to_string(),
            },
            message::Message::assistant("Acknowledged."),
            message::Message::System {
                content: "Mid-conversation instruction.".to_string(),
            },
            message::Message::user("Answer from the document."),
        ],
        None,
    );
    request.documents = vec![completion::Document {
        id: "doc".to_string(),
        text: "Document context.".to_string(),
        additional_props: Default::default(),
    }];

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-opus-4-8",
        request,
        prompt_caching: false,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    assert_eq!(value["system"][0]["text"], "Global history instruction.");
    assert_eq!(value["system"][1]["text"], "Mid-conversation instruction.");

    let messages = value["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[2]["role"], "user");
    assert!(
        messages[0].to_string().contains("<file id: doc>"),
        "document message should follow top-level system: {messages:?}"
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.to_string().contains("<file id: doc>"))
            .count(),
        1,
        "document message should appear exactly once: {messages:?}"
    );
    assert!(
        messages
            .iter()
            .all(|message| message["role"].as_str() != Some("system"))
    );
}

#[test]
fn older_anthropic_models_hoist_mid_conversation_system_message() {
    let request = completion_request_with_history(
        vec![
            message::Message::from("Review this code."),
            message::Message::System {
                content: "From now on, require explicit type annotations.".to_string(),
            },
        ],
        None,
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-opus-4-7",
        request,
        prompt_caching: false,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    assert_eq!(
        value["system"][0]["text"],
        "From now on, require explicit type annotations."
    );

    let messages = value["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
}

#[test]
fn test_tool_definition_cache_control_serialization() {
    let tool = ToolDefinition {
        name: "cached_tool".to_string(),
        description: Some("Cached tool".to_string()),
        input_schema: json!({"type": "object"}),
        cache_control: Some(CacheControl::ephemeral()),
    };

    let value = serde_json::to_value(tool).unwrap();
    assert_eq!(value["cache_control"]["type"], "ephemeral");

    let tool_without_cache = ToolDefinition {
        name: "uncached_tool".to_string(),
        description: Some("Uncached tool".to_string()),
        input_schema: json!({"type": "object"}),
        cache_control: None,
    };

    let value = serde_json::to_value(tool_without_cache).unwrap();
    assert!(value.get("cache_control").is_none());
}

#[test]
fn test_apply_tool_cache_control_marks_only_final_tool() {
    let mut tools = vec![
        json!({
            "name": "first_tool",
            "description": "First tool",
            "input_schema": {"type": "object"}
        }),
        json!({
            "name": "second_tool",
            "description": "Second tool",
            "input_schema": {"type": "object"}
        }),
    ];

    let mut remaining_cache_markers = 4;
    apply_tool_cache_control(
        &mut tools,
        &mut remaining_cache_markers,
        &CacheControl::ephemeral(),
    )
    .unwrap();

    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
    assert_eq!(remaining_cache_markers, 3);
}

#[test]
fn test_prompt_caching_skips_final_deferred_tool_in_request() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [
                {
                    "name": "regular_tool",
                    "description": "Regular tool",
                    "input_schema": {"type": "object"}
                },
                {
                    "name": "deferred_tool",
                    "description": "Deferred tool",
                    "input_schema": {"type": "object"},
                    "defer_loading": true
                }
            ]
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools[0]["name"], "regular_tool");
    assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(tools[1]["name"], "deferred_tool");
    assert!(tools[1].get("cache_control").is_none());
}

#[test]
fn test_prompt_caching_preserves_existing_final_tool_cache_control() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [{
                "name": "cached_tool",
                "description": "Cached tool",
                "input_schema": {"type": "object"},
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }]
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(tools[0]["cache_control"]["ttl"], "1h");
}

#[test]
fn test_prompt_caching_all_deferred_tools_do_not_receive_cache_control() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [
                {
                    "name": "first_deferred_tool",
                    "description": "First deferred tool",
                    "input_schema": {"type": "object"},
                    "defer_loading": true
                },
                {
                    "name": "second_deferred_tool",
                    "description": "Second deferred tool",
                    "input_schema": {"type": "object"},
                    "defer_loading": true
                }
            ]
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert!(tools[0].get("cache_control").is_none());
    assert!(tools[1].get("cache_control").is_none());
}

#[test]
fn test_prompt_caching_preserves_earlier_tool_cache_control() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [
                {
                    "name": "earlier_tool",
                    "description": "Earlier tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                },
                {
                    "name": "later_tool",
                    "description": "Later tool",
                    "input_schema": {"type": "object"}
                }
            ]
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(tools[0]["cache_control"]["ttl"], "1h");
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
}

#[test]
fn test_prompt_caching_deferred_marker_does_not_suppress_loaded_tool_marker() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [
                {
                    "name": "regular_tool",
                    "description": "Regular tool",
                    "input_schema": {"type": "object"}
                },
                {
                    "name": "deferred_cached_tool",
                    "description": "Deferred cached tool",
                    "input_schema": {"type": "object"},
                    "defer_loading": true,
                    "cache_control": {"type": "ephemeral"}
                }
            ]
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
}

#[test]
fn test_prompt_caching_errors_when_tool_cache_control_ttl_order_is_invalid() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [
                {
                    "name": "first_cached_tool",
                    "description": "First cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "second_cached_tool",
                    "description": "Second cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                }
            ]
        })),
    );

    let err = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap_err();

    assert!(err.to_string().contains("ttl `1h`"));
}

#[test]
fn test_prompt_caching_preserves_valid_mixed_ttl_tool_cache_controls() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [
                {
                    "name": "first_cached_tool",
                    "description": "First cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                },
                {
                    "name": "second_cached_tool",
                    "description": "Second cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                }
            ]
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(tools[0]["cache_control"]["ttl"], "1h");
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
    assert!(tools[1]["cache_control"].get("ttl").is_none());
}

#[test]
fn test_prompt_caching_preserves_deferred_tool_cache_control() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [{
                "name": "deferred_cached_tool",
                "description": "Deferred cached tool",
                "input_schema": {"type": "object"},
                "defer_loading": true,
                "cache_control": {"type": "ephemeral"}
            }]
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
}

#[test]
fn test_prompt_caching_budget_preserves_three_tool_markers_and_skips_message() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [
                {
                    "name": "first_cached_tool",
                    "description": "First cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "second_cached_tool",
                    "description": "Second cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "third_cached_tool",
                    "description": "Third cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                }
            ]
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
    assert_eq!(tools[2]["cache_control"]["type"], "ephemeral");
    assert!(system_has_cache_control(&value));
    assert!(!last_message_has_cache_control(&value));
}

#[test]
fn test_prompt_caching_errors_when_explicit_tool_markers_exceed_budget() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [
                {
                    "name": "first_cached_tool",
                    "description": "First cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "second_cached_tool",
                    "description": "Second cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "third_cached_tool",
                    "description": "Third cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "fourth_cached_tool",
                    "description": "Fourth cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "fifth_cached_tool",
                    "description": "Fifth cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                }
            ]
        })),
    );

    let err = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap_err();

    assert!(err.to_string().contains("Too many Anthropic tool"));
}

#[test]
fn test_prompt_caching_errors_when_final_tool_marker_has_no_budget() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [
                {
                    "name": "first_cached_tool",
                    "description": "First cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "second_cached_tool",
                    "description": "Second cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "third_cached_tool",
                    "description": "Third cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "fourth_cached_tool",
                    "description": "Fourth cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "final_uncached_tool",
                    "description": "Final uncached tool",
                    "input_schema": {"type": "object"}
                }
            ]
        })),
    );

    let err = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap_err();

    assert!(err.to_string().contains("final non-deferred tool"));
}

#[test]
fn test_prompt_caching_replaces_null_final_tool_cache_control() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [{
                "name": "final_tool",
                "description": "Final tool",
                "input_schema": {"type": "object"},
                "cache_control": null
            }]
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
}

#[test]
fn test_prompt_caching_ignores_null_tool_cache_control_when_budgeting() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [
                {
                    "name": "first_null_tool",
                    "description": "First null tool",
                    "input_schema": {"type": "object"},
                    "cache_control": null
                },
                {
                    "name": "second_null_tool",
                    "description": "Second null tool",
                    "input_schema": {"type": "object"},
                    "cache_control": null
                },
                {
                    "name": "third_null_tool",
                    "description": "Third null tool",
                    "input_schema": {"type": "object"},
                    "cache_control": null
                },
                {
                    "name": "fourth_null_tool",
                    "description": "Fourth null tool",
                    "input_schema": {"type": "object"},
                    "cache_control": null
                },
                {
                    "name": "final_uncached_tool",
                    "description": "Final uncached tool",
                    "input_schema": {"type": "object"}
                }
            ]
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert!(tools[0].get("cache_control").is_none());
    assert!(tools[1].get("cache_control").is_none());
    assert!(tools[2].get("cache_control").is_none());
    assert!(tools[3].get("cache_control").is_none());
    assert_eq!(tools[4]["cache_control"]["type"], "ephemeral");
}

#[test]
fn test_prompt_caching_preserves_non_null_provider_tool_cache_control_escape_hatch() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [{
                "name": "provider_tool",
                "description": "Provider tool",
                "input_schema": {"type": "object"},
                "cache_control": {"type": "provider_specific"}
            }]
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools[0]["cache_control"]["type"], "provider_specific");
}

#[test]
fn test_prompt_caching_automatic_mode_uses_reduced_marker_budget() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [
                {
                    "name": "first_cached_tool",
                    "description": "First cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "second_cached_tool",
                    "description": "Second cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "third_cached_tool",
                    "description": "Third cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                }
            ]
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: true,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
    assert_eq!(tools[2]["cache_control"]["type"], "ephemeral");
    assert_eq!(value["cache_control"]["type"], "ephemeral");
    assert!(!system_has_cache_control(&value));
    assert!(!last_message_has_cache_control(&value));
}

#[test]
fn test_prompt_caching_automatic_mode_errors_when_final_tool_marker_has_no_budget() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [
                {
                    "name": "first_cached_tool",
                    "description": "First cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "second_cached_tool",
                    "description": "Second cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "third_cached_tool",
                    "description": "Third cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "final_uncached_tool",
                    "description": "Final uncached tool",
                    "input_schema": {"type": "object"}
                }
            ]
        })),
    );

    let err = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: true,
        automatic_caching_ttl: None,
    })
    .unwrap_err();

    assert!(err.to_string().contains("final non-deferred tool"));
}

#[test]
fn test_automatic_caching_errors_when_explicit_tool_markers_exhaust_budget() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [
                {
                    "name": "first_cached_tool",
                    "description": "First cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "second_cached_tool",
                    "description": "Second cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "third_cached_tool",
                    "description": "Third cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "fourth_cached_tool",
                    "description": "Fourth cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                }
            ]
        })),
    );

    let err = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: false,
        automatic_caching: true,
        automatic_caching_ttl: None,
    })
    .unwrap_err();

    assert!(err.to_string().contains("Too many Anthropic tool"));
}

#[test]
fn test_automatic_caching_1h_errors_with_explicit_five_minute_tool_marker() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "tools": [{
                "name": "cached_tool",
                "description": "Cached tool",
                "input_schema": {"type": "object"},
                "cache_control": {"type": "ephemeral"}
            }]
        })),
    );

    let err = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: false,
        automatic_caching: true,
        automatic_caching_ttl: Some(CacheTtl::OneHour),
    })
    .unwrap_err();

    assert!(err.to_string().contains("ttl `1h`"));
}

#[test]
fn test_prompt_and_automatic_caching_1h_uses_1h_generated_markers() {
    let request = completion_request_with_tools(vec![generic_tool("cached_tool")], None);

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: true,
        automatic_caching_ttl: Some(CacheTtl::OneHour),
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(tools[0]["cache_control"]["ttl"], "1h");
    assert_eq!(
        value["system"]
            .as_array()
            .and_then(|blocks| blocks.last())
            .and_then(|block| block["cache_control"].get("ttl")),
        Some(&json!("1h"))
    );
    assert_eq!(value["cache_control"]["ttl"], "1h");
    assert!(!last_message_has_cache_control(&value));
}

#[test]
fn test_prompt_and_raw_top_level_automatic_caching_1h_uses_1h_generated_markers() {
    let request = completion_request_with_tools(
        vec![generic_tool("cached_tool")],
        Some(json!({
            "cache_control": {"type": "ephemeral", "ttl": "1h"},
            "metadata": {"source": "test"}
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: true,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(tools[0]["cache_control"]["ttl"], "1h");
    assert_eq!(
        value["system"]
            .as_array()
            .and_then(|blocks| blocks.last())
            .and_then(|block| block["cache_control"].get("ttl")),
        Some(&json!("1h"))
    );
    assert_eq!(value["cache_control"]["ttl"], "1h");
    assert_eq!(value["metadata"]["source"], "test");
    assert!(!last_message_has_cache_control(&value));
}

#[test]
fn test_prompt_caching_uses_raw_top_level_cache_control_ttl() {
    let request = completion_request_with_tools(
        vec![generic_tool("cached_tool")],
        Some(json!({
            "cache_control": {"type": "ephemeral", "ttl": "1h"},
            "metadata": {"source": "raw-cache-control"}
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(tools[0]["cache_control"]["ttl"], "1h");
    assert_eq!(
        value["system"]
            .as_array()
            .and_then(|blocks| blocks.last())
            .and_then(|block| block["cache_control"].get("ttl")),
        Some(&json!("1h"))
    );
    assert_eq!(value["cache_control"]["ttl"], "1h");
    assert_eq!(value["metadata"]["source"], "raw-cache-control");
    assert!(!last_message_has_cache_control(&value));
}

#[test]
fn test_raw_top_level_automatic_caching_reduces_marker_budget() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "cache_control": {"type": "ephemeral"},
            "tools": [
                {
                    "name": "first_cached_tool",
                    "description": "First cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "second_cached_tool",
                    "description": "Second cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "third_cached_tool",
                    "description": "Third cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                },
                {
                    "name": "fourth_cached_tool",
                    "description": "Fourth cached tool",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral"}
                }
            ]
        })),
    );

    let err = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: false,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap_err();

    assert!(err.to_string().contains("Too many Anthropic tool"));
}

#[test]
fn test_raw_top_level_automatic_caching_1h_errors_after_explicit_five_minute_tool_marker() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "cache_control": {"type": "ephemeral", "ttl": "1h"},
            "tools": [{
                "name": "cached_tool",
                "description": "Cached tool",
                "input_schema": {"type": "object"},
                "cache_control": {"type": "ephemeral"}
            }]
        })),
    );

    let err = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: false,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap_err();

    assert!(err.to_string().contains("ttl `1h`"));
}

#[test]
fn test_typed_automatic_caching_ttl_errors_on_conflicting_raw_top_level_ttl() {
    let request = completion_request_with_tools(
        Vec::new(),
        Some(json!({
            "cache_control": {"type": "ephemeral"}
        })),
    );

    let err = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: false,
        automatic_caching: true,
        automatic_caching_ttl: Some(CacheTtl::OneHour),
    })
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("conflicts with the typed automatic caching TTL")
    );
}

#[test]
fn test_prompt_caching_marks_final_tool_in_request() {
    let request = completion_request_with_tools(
        vec![generic_tool("first_tool"), generic_tool("second_tool")],
        None,
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
}

#[test]
fn test_prompt_caching_marks_final_additional_tool_in_request() {
    let request = completion_request_with_tools(
        vec![generic_tool("rig_tool")],
        Some(json!({
            "tools": [{
                "name": "provider_tool",
                "description": "Provider tool",
                "input_schema": {"type": "object"}
            }],
            "metadata": {"source": "test"}
        })),
    );

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(tools[1]["name"], "provider_tool");
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
    assert_eq!(value["metadata"]["source"], "test");
}

#[test]
fn test_prompt_caching_without_tools_omits_tools() {
    let request = completion_request_with_tools(Vec::new(), None);

    let request = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: true,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(request).unwrap();
    assert!(value.get("tools").is_none());
}

#[test]
fn test_plaintext_document_serialization() {
    let content = Content::Document {
        source: DocumentSource::Text {
            data: "Hello, world!".to_string(),
            media_type: PlainTextMediaType::Plain,
        },
        title: None,
        context: None,
        citations: None,
        cache_control: None,
    };

    let json = serde_json::to_value(&content).unwrap();
    assert_eq!(json["type"], "document");
    assert_eq!(json["source"]["type"], "text");
    assert_eq!(json["source"]["media_type"], "text/plain");
    assert_eq!(json["source"]["data"], "Hello, world!");
}

#[test]
fn test_plaintext_document_deserialization() {
    let json = r#"
        {
            "type": "document",
            "source": {
                "type": "text",
                "media_type": "text/plain",
                "data": "Hello, world!"
            }
        }
        "#;

    let content: Content = serde_json::from_str(json).unwrap();
    match content {
        Content::Document {
            source,
            cache_control,
            ..
        } => {
            assert_eq!(
                source,
                DocumentSource::Text {
                    data: "Hello, world!".to_string(),
                    media_type: PlainTextMediaType::Plain,
                }
            );
            assert_eq!(cache_control, None);
        }
        _ => panic!("Expected Document content"),
    }
}

#[test]
fn test_base64_pdf_document_serialization() {
    let content = Content::Document {
        source: DocumentSource::Base64 {
            data: "base64data".to_string(),
            media_type: DocumentFormat::PDF,
        },
        title: None,
        context: None,
        citations: None,
        cache_control: None,
    };

    let json = serde_json::to_value(&content).unwrap();
    assert_eq!(json["type"], "document");
    assert_eq!(json["source"]["type"], "base64");
    assert_eq!(json["source"]["media_type"], "application/pdf");
    assert_eq!(json["source"]["data"], "base64data");
}

#[test]
fn test_base64_pdf_document_deserialization() {
    let json = r#"
        {
            "type": "document",
            "source": {
                "type": "base64",
                "media_type": "application/pdf",
                "data": "base64data"
            }
        }
        "#;

    let content: Content = serde_json::from_str(json).unwrap();
    match content {
        Content::Document { source, .. } => {
            assert_eq!(
                source,
                DocumentSource::Base64 {
                    data: "base64data".to_string(),
                    media_type: DocumentFormat::PDF,
                }
            );
        }
        _ => panic!("Expected Document content"),
    }
}

#[test]
fn test_file_id_document_serialization() {
    let content = Content::Document {
        source: DocumentSource::File {
            file_id: "file_abc".to_string(),
        },
        title: None,
        context: None,
        citations: None,
        cache_control: None,
    };

    let json = serde_json::to_value(&content).unwrap();
    assert_eq!(json["type"], "document");
    assert_eq!(json["source"]["type"], "file");
    assert_eq!(json["source"]["file_id"], "file_abc");
}

#[test]
fn test_file_id_document_deserialization() {
    let json = r#"
        {
            "type": "document",
            "source": {
                "type": "file",
                "file_id": "file_abc"
            }
        }
        "#;

    let content: Content = serde_json::from_str(json).unwrap();
    match content {
        Content::Document { source, .. } => {
            assert_eq!(
                source,
                DocumentSource::File {
                    file_id: "file_abc".to_string(),
                }
            );
        }
        _ => panic!("Expected Document content"),
    }
}

#[test]
fn test_file_id_rig_to_anthropic_conversion() {
    use crate::completion::message as msg;

    let rig_message = msg::Message::User {
        content: OneOrMany::one(msg::UserContent::Document(msg::Document {
            data: DocumentSourceKind::FileId("file_abc".to_string()),
            media_type: None,
            additional_params: None,
        })),
    };

    let anthropic_message: Message = rig_message.try_into().unwrap();
    assert_eq!(anthropic_message.role, Role::User);

    let mut iter = anthropic_message.content.into_iter();
    match iter.next().unwrap() {
        Content::Document { source, .. } => {
            assert_eq!(
                source,
                DocumentSource::File {
                    file_id: "file_abc".to_string(),
                }
            );
        }
        other => panic!("Expected Document content, got: {other:?}"),
    }
}

#[test]
fn test_file_id_anthropic_to_rig_conversion() {
    use crate::completion::message as msg;

    let anthropic_message = Message {
        role: Role::User,
        content: OneOrMany::one(Content::Document {
            source: DocumentSource::File {
                file_id: "file_abc".to_string(),
            },
            title: None,
            context: None,
            citations: None,
            cache_control: None,
        }),
    };

    let rig_message: msg::Message = anthropic_message.try_into().unwrap();
    match rig_message {
        msg::Message::User { content } => {
            let mut iter = content.into_iter();
            match iter.next().unwrap() {
                msg::UserContent::Document(msg::Document {
                    data, media_type, ..
                }) => {
                    assert_eq!(data, DocumentSourceKind::FileId("file_abc".to_string()));
                    assert_eq!(media_type, None);
                }
                other => panic!("Expected Document content, got: {other:?}"),
            }
        }
        _ => panic!("Expected User message"),
    }
}

#[test]
fn test_plaintext_rig_to_anthropic_conversion() {
    use crate::completion::message as msg;

    let rig_message = msg::Message::User {
        content: OneOrMany::one(msg::UserContent::document(
            "Some plain text content".to_string(),
            Some(msg::DocumentMediaType::TXT),
        )),
    };

    let anthropic_message: Message = rig_message.try_into().unwrap();
    assert_eq!(anthropic_message.role, Role::User);

    let mut iter = anthropic_message.content.into_iter();
    match iter.next().unwrap() {
        Content::Document { source, .. } => {
            assert_eq!(
                source,
                DocumentSource::Text {
                    data: "Some plain text content".to_string(),
                    media_type: PlainTextMediaType::Plain,
                }
            );
        }
        other => panic!("Expected Document content, got: {other:?}"),
    }
}

#[test]
fn test_plaintext_anthropic_to_rig_conversion() {
    use crate::completion::message as msg;

    let anthropic_message = Message {
        role: Role::User,
        content: OneOrMany::one(Content::Document {
            source: DocumentSource::Text {
                data: "Some plain text content".to_string(),
                media_type: PlainTextMediaType::Plain,
            },
            title: None,
            context: None,
            citations: None,
            cache_control: None,
        }),
    };

    let rig_message: msg::Message = anthropic_message.try_into().unwrap();
    match rig_message {
        msg::Message::User { content } => {
            let mut iter = content.into_iter();
            match iter.next().unwrap() {
                msg::UserContent::Document(msg::Document {
                    data, media_type, ..
                }) => {
                    assert_eq!(
                        data,
                        DocumentSourceKind::String("Some plain text content".into())
                    );
                    assert_eq!(media_type, Some(msg::DocumentMediaType::TXT));
                }
                other => panic!("Expected Document content, got: {other:?}"),
            }
        }
        _ => panic!("Expected User message"),
    }
}

#[test]
fn test_plaintext_roundtrip_rig_to_anthropic_and_back() {
    use crate::completion::message as msg;

    let original = msg::Message::User {
        content: OneOrMany::one(msg::UserContent::document(
            "Round trip text".to_string(),
            Some(msg::DocumentMediaType::TXT),
        )),
    };

    let anthropic: Message = original.clone().try_into().unwrap();
    let back: msg::Message = anthropic.try_into().unwrap();

    match (&original, &back) {
        (
            msg::Message::User {
                content: orig_content,
            },
            msg::Message::User {
                content: back_content,
            },
        ) => match (orig_content.first(), back_content.first()) {
            (
                msg::UserContent::Document(msg::Document {
                    media_type: orig_mt,
                    ..
                }),
                msg::UserContent::Document(msg::Document {
                    media_type: back_mt,
                    ..
                }),
            ) => {
                assert_eq!(orig_mt, back_mt);
            }
            _ => panic!("Expected Document content in both"),
        },
        _ => panic!("Expected User messages"),
    }
}

#[test]
fn test_unsupported_document_type_returns_error() {
    use crate::completion::message as msg;

    let rig_message = msg::Message::User {
        content: OneOrMany::one(msg::UserContent::Document(msg::Document {
            data: DocumentSourceKind::String("data".into()),
            media_type: Some(msg::DocumentMediaType::HTML),
            additional_params: None,
        })),
    };

    let result: Result<Message, _> = rig_message.try_into();
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Anthropic only supports PDF and plain text documents"),
        "Unexpected error: {err}"
    );
}

#[test]
fn test_plaintext_document_url_source_returns_error() {
    use crate::completion::message as msg;

    let rig_message = msg::Message::User {
        content: OneOrMany::one(msg::UserContent::Document(msg::Document {
            data: DocumentSourceKind::Url("https://example.com/doc.txt".into()),
            media_type: Some(msg::DocumentMediaType::TXT),
            additional_params: None,
        })),
    };

    let result: Result<Message, _> = rig_message.try_into();
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Only string or base64 data is supported for plain text documents"),
        "Unexpected error: {err}"
    );
}

#[test]
fn test_plaintext_document_with_cache_control() {
    let content = Content::Document {
        source: DocumentSource::Text {
            data: "cached text".to_string(),
            media_type: PlainTextMediaType::Plain,
        },
        title: None,
        context: None,
        citations: None,
        cache_control: Some(CacheControl::ephemeral()),
    };

    let json = serde_json::to_value(&content).unwrap();
    assert_eq!(json["source"]["type"], "text");
    assert_eq!(json["source"]["media_type"], "text/plain");
    assert_eq!(json["cache_control"]["type"], "ephemeral");
}

#[test]
fn test_message_with_plaintext_document_deserialization() {
    let json = r#"
        {
            "role": "user",
            "content": [
                {
                    "type": "document",
                    "source": {
                        "type": "text",
                        "media_type": "text/plain",
                        "data": "Hello from a text file"
                    }
                },
                {
                    "type": "text",
                    "text": "Summarize this document."
                }
            ]
        }
        "#;

    let message: Message = serde_json::from_str(json).unwrap();
    assert_eq!(message.role, Role::User);
    assert_eq!(message.content.len(), 2);

    let mut iter = message.content.into_iter();

    match iter.next().unwrap() {
        Content::Document { source, .. } => {
            assert_eq!(
                source,
                DocumentSource::Text {
                    data: "Hello from a text file".to_string(),
                    media_type: PlainTextMediaType::Plain,
                }
            );
        }
        _ => panic!("Expected Document content"),
    }

    match iter.next().unwrap() {
        Content::Text { text, .. } => {
            assert_eq!(text, "Summarize this document.");
        }
        _ => panic!("Expected Text content"),
    }
}

#[test]
fn test_assistant_reasoning_multiblock_to_anthropic_content() {
    let reasoning = message::Reasoning {
        id: None,
        content: vec![
            message::ReasoningContent::Text {
                text: "step one".to_string(),
                signature: Some("sig-1".to_string()),
            },
            message::ReasoningContent::Summary("summary".to_string()),
            message::ReasoningContent::Text {
                text: "step two".to_string(),
                signature: Some("sig-2".to_string()),
            },
            message::ReasoningContent::Redacted {
                data: "redacted block".to_string(),
            },
        ],
    };

    let msg = message::Message::Assistant {
        id: None,
        content: OneOrMany::one(message::AssistantContent::Reasoning(reasoning)),
    };
    let converted: Message = msg.try_into().expect("convert assistant message");
    let converted_content = converted.content.iter().cloned().collect::<Vec<_>>();

    assert_eq!(converted.role, Role::Assistant);
    assert_eq!(converted_content.len(), 4);
    assert!(matches!(
        converted_content.first(),
        Some(Content::Thinking { thinking, signature: Some(signature) })
            if thinking == "step one" && signature == "sig-1"
    ));
    assert!(matches!(
        converted_content.get(1),
        Some(Content::Thinking { thinking, signature: None }) if thinking == "summary"
    ));
    assert!(matches!(
        converted_content.get(2),
        Some(Content::Thinking { thinking, signature: Some(signature) })
            if thinking == "step two" && signature == "sig-2"
    ));
    assert!(matches!(
        converted_content.get(3),
        Some(Content::RedactedThinking { data }) if data == "redacted block"
    ));
}

#[test]
fn test_redacted_thinking_content_to_assistant_reasoning() {
    let content = Content::RedactedThinking {
        data: "opaque-redacted".to_string(),
    };
    let converted: message::AssistantContent =
        content.try_into().expect("convert redacted thinking");

    assert!(matches!(
        converted,
        message::AssistantContent::Reasoning(message::Reasoning { content, .. })
            if matches!(
                content.first(),
                Some(message::ReasoningContent::Redacted { data }) if data == "opaque-redacted"
            )
    ));
}

#[test]
fn test_assistant_encrypted_reasoning_maps_to_redacted_thinking() {
    let reasoning = message::Reasoning {
        id: None,
        content: vec![message::ReasoningContent::Encrypted(
            "ciphertext".to_string(),
        )],
    };
    let msg = message::Message::Assistant {
        id: None,
        content: OneOrMany::one(message::AssistantContent::Reasoning(reasoning)),
    };

    let converted: Message = msg.try_into().expect("convert assistant message");
    let converted_content = converted.content.iter().cloned().collect::<Vec<_>>();

    assert_eq!(converted_content.len(), 1);
    assert!(matches!(
        converted_content.first(),
        Some(Content::RedactedThinking { data }) if data == "ciphertext"
    ));
}

#[test]
fn empty_end_turn_response_normalizes_to_empty_text_choice() {
    let response = CompletionResponse {
        content: vec![],
        id: "msg_123".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        role: "assistant".to_string(),
        stop_reason: Some("end_turn".to_string()),
        stop_sequence: None,
        usage: Usage {
            input_tokens: 7,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            cache_creation: None,
            output_tokens: 2,
        },
    };

    let parsed: completion::CompletionResponse = response
        .normalize("anthropic")
        .expect("empty end_turn should not error");

    assert_eq!(parsed.choice.len(), 1);
    assert!(matches!(
        parsed.choice.first(),
        completion::AssistantContent::Text(text) if text.text.is_empty()
    ));
    assert_eq!(parsed.provider, "anthropic");
    assert_eq!(parsed.message_id.as_deref(), Some("msg_123"));
    assert_eq!(parsed.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(parsed.finish_reason(), Some(completion::FinishReason::Stop));
}

#[test]
fn empty_non_end_turn_response_still_errors() {
    let response = CompletionResponse {
        content: vec![],
        id: "msg_123".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        role: "assistant".to_string(),
        stop_reason: Some("tool_use".to_string()),
        stop_sequence: None,
        usage: Usage {
            input_tokens: 7,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            cache_creation: None,
            output_tokens: 2,
        },
    };

    let err = response
        .normalize("anthropic")
        .expect_err("empty non-end_turn should remain an error");

    assert!(matches!(
        err,
        CompletionError::ResponseError(message) if message == EMPTY_RESPONSE_ERROR
    ));
}

#[test]
fn stop_reason_maps_onto_the_normalized_vocabulary() {
    assert_eq!(
        map_finish_reason("end_turn"),
        completion::FinishReason::Stop
    );
    assert_eq!(
        map_finish_reason("stop_sequence"),
        completion::FinishReason::Stop
    );
    assert_eq!(
        map_finish_reason("max_tokens"),
        completion::FinishReason::Length
    );
    assert_eq!(
        map_finish_reason("tool_use"),
        completion::FinishReason::ToolCalls
    );
    assert_eq!(
        map_finish_reason("refusal"),
        completion::FinishReason::ContentFilter
    );
}

#[test]
fn unknown_stop_reason_is_preserved_verbatim() {
    // Anthropic's own spelling survives, so a reason this crate does not yet
    // model never reads as a natural stop.
    assert_eq!(
        map_finish_reason("pause_turn"),
        completion::FinishReason::Other("pause_turn".to_owned())
    );
    assert_eq!(
        map_finish_reason("model_context_window_exceeded"),
        completion::FinishReason::Other("model_context_window_exceeded".to_owned())
    );
}

#[test]
fn end_turn_with_a_tool_call_is_reconciled_to_tool_calls() {
    // Anthropic reports `tool_use`, but the reconciliation the response
    // builder applies must hold for any provider that reports a plain stop
    // alongside a tool call.
    let response = CompletionResponse {
        content: vec![Content::ToolUse {
            id: "toolu_1".to_string(),
            name: "add".to_string(),
            input: json!({"x": 1}),
        }],
        id: "msg_123".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        role: "assistant".to_string(),
        stop_reason: Some("end_turn".to_string()),
        stop_sequence: None,
        usage: Usage {
            input_tokens: 7,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            cache_creation: None,
            output_tokens: 2,
        },
    };

    let parsed = response
        .normalize("anthropic")
        .expect("tool-use response should normalize");

    assert_eq!(
        parsed.finish_reason(),
        Some(completion::FinishReason::ToolCalls)
    );
}

#[test]
fn test_tool_result_content_in_message_roundtrip() {
    let message_json = r#"{
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_01A09q90qw90lq917835lq9",
                    "content": [
                        {
                            "type": "text",
                            "text": "Here is the screenshot:"
                        },
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": "iVBORw0KGgo..."
                            }
                        }
                    ]
                }
            ]
        }"#;

    let message: Message = serde_json::from_str(message_json).unwrap();
    let serialized = serde_json::to_value(&message).unwrap();

    let tool_result = &serialized["content"][0];
    assert_eq!(tool_result["type"], "tool_result");

    let image_content = &tool_result["content"][1];
    assert_eq!(image_content["type"], "image");
    assert_eq!(image_content["source"]["type"], "base64");
    assert_eq!(image_content["source"]["media_type"], "image/png");
    assert_eq!(image_content["source"]["data"], "iVBORw0KGgo...");
}

// -------------------------------------------------------------------
// Citations (#1767)
// -------------------------------------------------------------------

#[test]
fn document_serializes_citations_and_metadata() {
    let doc = Content::Document {
        source: DocumentSource::Text {
            data: "hello".into(),
            media_type: PlainTextMediaType::Plain,
        },
        title: Some("My Doc".into()),
        context: None,
        citations: Some(CitationsConfig { enabled: true }),
        cache_control: None,
    };
    let value = serde_json::to_value(&doc).unwrap();
    assert_eq!(value["citations"]["enabled"], true);
    assert_eq!(value["title"], "My Doc");
    assert!(
        value.get("context").is_none(),
        "context should be skipped when None"
    );
}

#[test]
fn text_serializes_without_citations_when_empty() {
    let content = Content::Text {
        text: "hello".into(),
        citations: Vec::new(),
        cache_control: None,
    };
    let value = serde_json::to_value(&content).unwrap();
    assert!(
        value.get("citations").is_none(),
        "empty citations vec must be skipped"
    );
}

#[test]
fn text_deserializes_char_location_citation() {
    let value = json!({
        "type": "text",
        "text": "the grass is green",
        "citations": [{
            "type": "char_location",
            "cited_text": "The grass is green.",
            "document_index": 0,
            "document_title": "Example",
            "start_char_index": 0,
            "end_char_index": 20
        }]
    });
    let parsed: Content = serde_json::from_value(value).unwrap();
    let Content::Text { citations, .. } = parsed else {
        panic!("expected Content::Text");
    };
    assert_eq!(citations.len(), 1);
    let Citation::CharLocation {
        start_char_index,
        end_char_index,
        ..
    } = &citations[0]
    else {
        panic!("expected CharLocation");
    };
    assert_eq!(*start_char_index, 0);
    assert_eq!(*end_char_index, 20);
}

#[test]
fn text_deserializes_search_result_location_citation() {
    let value = json!({
        "type": "text",
        "text": "API keys are required.",
        "citations": [{
            "type": "search_result_location",
            "cited_text": "All API requests must include an API key.",
            "source": "https://docs.example.com/api-reference",
            "title": "API Reference",
            "search_result_index": 0,
            "start_block_index": 0,
            "end_block_index": 1
        }]
    });

    let parsed: Content = serde_json::from_value(value).unwrap();
    let Content::Text { citations, .. } = parsed else {
        panic!("expected Content::Text");
    };

    assert!(matches!(
        &citations[0],
        Citation::SearchResultLocation {
            source,
            title: Some(title),
            search_result_index: 0,
            start_block_index: 0,
            end_block_index: 1,
            ..
        } if source == "https://docs.example.com/api-reference" && title == "API Reference"
    ));
}

#[test]
fn text_deserializes_web_search_result_location_citation() {
    let value = json!({
        "type": "text",
        "text": "Claude Shannon worked at Bell Labs.",
        "citations": [{
            "type": "web_search_result_location",
            "cited_text": "Claude Shannon was a mathematician.",
            "url": "https://example.com/shannon",
            "title": "Claude Shannon",
            "encrypted_index": "encrypted-reference"
        }]
    });

    let parsed: Content = serde_json::from_value(value).unwrap();
    let Content::Text { citations, .. } = parsed else {
        panic!("expected Content::Text");
    };

    assert!(matches!(
        &citations[0],
        Citation::WebSearchResultLocation {
            url,
            title,
            encrypted_index,
            ..
        } if url == "https://example.com/shannon"
            && title.as_deref() == Some("Claude Shannon")
            && encrypted_index == "encrypted-reference"
    ));
}

#[test]
fn text_deserializes_web_search_result_location_citation_with_null_title() {
    let value = json!({
        "type": "text",
        "text": "Claude Shannon worked at Bell Labs.",
        "citations": [{
            "type": "web_search_result_location",
            "cited_text": "Claude Shannon was a mathematician.",
            "url": "https://example.com/shannon",
            "title": null,
            "encrypted_index": "encrypted-reference"
        }]
    });

    let parsed: Content = serde_json::from_value(value).unwrap();
    let Content::Text { citations, .. } = parsed else {
        panic!("expected Content::Text");
    };

    let Citation::WebSearchResultLocation { title, .. } = &citations[0] else {
        panic!("expected WebSearchResultLocation");
    };
    assert_eq!(title, &None);

    let serialized = serde_json::to_value(&citations[0]).unwrap();
    assert!(serialized.get("title").is_some());
    assert!(serialized["title"].is_null());
}

#[test]
fn web_search_response_preserves_raw_blocks_and_citations() {
    let value = json!({
        "id": "msg_web_search",
        "model": "claude-sonnet-4-6",
        "role": "assistant",
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": 10,
            "output_tokens": 20
        },
        "content": [
            {
                "type": "server_tool_use",
                "id": "srvtoolu_01",
                "name": "web_search",
                "input": {
                    "query": "claude shannon birth date"
                }
            },
            {
                "type": "web_search_tool_result",
                "tool_use_id": "srvtoolu_01",
                "content": [
                    {
                        "type": "web_search_result",
                        "url": "https://example.com/shannon",
                        "title": "Claude Shannon",
                        "encrypted_content": "encrypted-content",
                        "page_age": "April 30, 2025"
                    }
                ]
            },
            {
                "type": "text",
                "text": "Claude Shannon was born on April 30, 1916.",
                "citations": [{
                    "type": "web_search_result_location",
                    "cited_text": "Claude Shannon was born on April 30, 1916.",
                    "url": "https://example.com/shannon",
                    "title": "Claude Shannon",
                    "encrypted_index": "encrypted-index"
                }]
            }
        ]
    });

    let response: CompletionResponse = serde_json::from_value(value).unwrap();
    // The wire response is consumed by the conversion, so read the
    // provider-native text off it first.
    let raw_text_response = response.get_text_response();
    let converted = response.normalize("anthropic").unwrap();
    assert_eq!(converted.choice.len(), 3);
    assert_eq!(
        raw_text_response.as_deref(),
        Some("Claude Shannon was born on April 30, 1916.")
    );

    let items = converted.choice.iter().collect::<Vec<_>>();
    let message::AssistantContent::Text(server_tool_use) = items[0] else {
        panic!("expected raw server_tool_use metadata");
    };
    assert_eq!(server_tool_use.text, "");
    assert_eq!(
        server_tool_use.additional_params.as_ref().unwrap()[ANTHROPIC_RAW_CONTENT_KEY]["type"],
        "server_tool_use"
    );

    let message::AssistantContent::Text(web_search_result) = items[1] else {
        panic!("expected raw web_search_tool_result metadata");
    };
    assert_eq!(
        web_search_result.additional_params.as_ref().unwrap()[ANTHROPIC_RAW_CONTENT_KEY]["content"]
            [0]["encrypted_content"],
        "encrypted-content"
    );

    let message::AssistantContent::Text(answer) = items[2] else {
        panic!("expected text answer");
    };
    let citations = anthropic_citations(answer).unwrap();
    assert!(matches!(
        citations.first(),
        Some(Citation::WebSearchResultLocation {
            encrypted_index,
            ..
        }) if encrypted_index == "encrypted-index"
    ));

    let round_trip: Message = message::Message::Assistant {
        id: converted.message_id.clone(),
        content: converted.choice,
    }
    .try_into()
    .unwrap();

    let round_trip_items = round_trip.content.iter().collect::<Vec<_>>();
    assert!(matches!(
        round_trip_items.first(),
        Some(Content::ServerToolUse { id, name, input })
            if id == "srvtoolu_01"
                && name == "web_search"
                && input["query"] == "claude shannon birth date"
    ));
    assert!(matches!(
        round_trip_items.get(1),
        Some(Content::WebSearchToolResult {
            tool_use_id,
            content
        }) if tool_use_id == "srvtoolu_01"
            && content[0]["encrypted_content"] == "encrypted-content"
    ));
}

#[test]
fn web_search_tool_result_error_object_is_preserved_raw() {
    let value = json!({
        "id": "msg_web_search_error",
        "model": "claude-sonnet-4-6",
        "role": "assistant",
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": 10,
            "output_tokens": 2
        },
        "content": [{
            "type": "web_search_tool_result",
            "tool_use_id": "srvtoolu_01",
            "content": {
                "type": "web_search_tool_result_error",
                "error_code": "max_uses_exceeded"
            }
        }]
    });

    let response: CompletionResponse = serde_json::from_value(value).unwrap();
    let converted = response.normalize("anthropic").unwrap();
    let message::AssistantContent::Text(web_search_result) = converted.choice.first() else {
        panic!("expected raw web_search_tool_result metadata");
    };

    let raw_content =
        &web_search_result.additional_params.as_ref().unwrap()[ANTHROPIC_RAW_CONTENT_KEY];
    assert_eq!(raw_content["type"], "web_search_tool_result");
    assert_eq!(raw_content["content"]["error_code"], "max_uses_exceeded");
    assert_eq!(
        raw_content["content"]["type"],
        "web_search_tool_result_error"
    );

    let round_trip: Message = message::Message::Assistant {
        id: converted.message_id,
        content: converted.choice,
    }
    .try_into()
    .unwrap();

    assert!(matches!(
        round_trip.content.first(),
        Content::WebSearchToolResult {
            tool_use_id,
            content
        } if tool_use_id == "srvtoolu_01"
            && content["error_code"] == "max_uses_exceeded"
    ));
}

#[test]
fn code_execution_tool_result_variants_deserialize() {
    let normal: Content = serde_json::from_value(json!({
        "type": "code_execution_tool_result",
        "tool_use_id": "srvtoolu_normal",
        "content": {
            "type": "code_execution_result",
            "return_code": 0,
            "stdout": "42\n",
            "stderr": "",
            "content": []
        }
    }))
    .unwrap();
    assert!(matches!(
        normal,
        Content::CodeExecutionToolResult {
            ref tool_use_id,
            ref content
        } if tool_use_id == "srvtoolu_normal"
            && content["type"] == "code_execution_result"
            && content["stdout"] == "42\n"
    ));

    let encrypted: Content = serde_json::from_value(json!({
        "type": "code_execution_tool_result",
        "tool_use_id": "srvtoolu_encrypted",
        "content": {
            "type": "encrypted_code_execution_result",
            "return_code": 1,
            "stderr": "failure",
            "encrypted_stdout": "encrypted-output",
            "content": []
        }
    }))
    .unwrap();
    assert!(matches!(
        encrypted,
        Content::CodeExecutionToolResult {
            ref tool_use_id,
            ref content
        } if tool_use_id == "srvtoolu_encrypted"
            && content["type"] == "encrypted_code_execution_result"
            && content["encrypted_stdout"] == "encrypted-output"
    ));
}

#[test]
fn code_execution_tool_result_is_preserved_and_round_trips() {
    let raw_block = json!({
        "type": "code_execution_tool_result",
        "tool_use_id": "srvtoolu_01",
        "content": {
            "type": "code_execution_result",
            "return_code": 0,
            "stdout": "42\n",
            "stderr": "",
            "content": []
        }
    });
    let value = json!({
        "id": "msg_code_execution",
        "model": "claude-opus-4-8",
        "role": "assistant",
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": 10,
            "output_tokens": 20
        },
        "content": [raw_block.clone()]
    });

    let response: CompletionResponse = serde_json::from_value(value).unwrap();
    let converted = response.normalize("anthropic").unwrap();
    let message::AssistantContent::Text(code_execution_result) = converted.choice.first() else {
        panic!("expected raw code_execution_tool_result metadata");
    };
    assert_eq!(
        code_execution_result.additional_params.as_ref().unwrap()[ANTHROPIC_RAW_CONTENT_KEY],
        raw_block
    );

    let round_trip: Message = message::Message::Assistant {
        id: converted.message_id,
        content: converted.choice,
    }
    .try_into()
    .unwrap();
    assert!(matches!(
        round_trip.content.first(),
        Content::CodeExecutionToolResult {
            tool_use_id,
            content
        } if tool_use_id == "srvtoolu_01"
            && content["type"] == "code_execution_result"
            && content["stdout"] == "42\n"
    ));
}

#[test]
fn text_deserializes_unknown_citation_without_failing() {
    let value = json!({
        "type": "text",
        "text": "future citation",
        "citations": [{
            "type": "future_location",
            "cited_text": "future text",
            "new_field": "kept"
        }]
    });

    let parsed: Content = serde_json::from_value(value).unwrap();
    let Content::Text { citations, .. } = parsed else {
        panic!("expected Content::Text");
    };

    assert!(matches!(
        &citations[0],
        Citation::Unknown(raw)
            if raw["type"] == "future_location" && raw["new_field"] == "kept"
    ));
}

#[test]
fn page_location_citation_roundtrips() {
    let citation = Citation::PageLocation {
        cited_text: "Water is essential for life.".into(),
        document_index: 1,
        document_title: Some("PDF Doc".into()),
        start_page_number: 5,
        end_page_number: 6,
    };
    let value = serde_json::to_value(&citation).unwrap();
    assert_eq!(value["type"], "page_location");
    assert_eq!(value["start_page_number"], 5);
    let back: Citation = serde_json::from_value(value).unwrap();
    assert_eq!(back, citation);
}

#[test]
fn content_block_location_citation_roundtrips() {
    let citation = Citation::ContentBlockLocation {
        cited_text: "These are important findings.".into(),
        document_index: 2,
        document_title: None,
        start_block_index: 0,
        end_block_index: 1,
    };
    let value = serde_json::to_value(&citation).unwrap();
    assert_eq!(value["type"], "content_block_location");
    assert!(value.get("document_title").is_none());
    let back: Citation = serde_json::from_value(value).unwrap();
    assert_eq!(back, citation);
}

#[test]
fn anthropic_citations_extracts_from_additional_params() {
    let text = message::Text {
        text: "the grass is green".into(),
        additional_params: Some(json!({
            "citations": [{
                "type": "char_location",
                "cited_text": "The grass is green.",
                "document_index": 0,
                "start_char_index": 0,
                "end_char_index": 20
            }]
        })),
    };
    let citations = anthropic_citations(&text).unwrap();
    assert_eq!(citations.len(), 1);
}

#[test]
fn anthropic_citations_returns_empty_when_absent() {
    let text = message::Text::new("hello".to_string());
    assert!(anthropic_citations(&text).unwrap().is_empty());
}

#[test]
fn content_text_with_citations_survives_assistant_conversion() {
    let content = Content::Text {
        text: "the grass is green".into(),
        citations: vec![Citation::CharLocation {
            cited_text: "The grass is green.".into(),
            document_index: 0,
            document_title: None,
            start_char_index: 0,
            end_char_index: 20,
        }],
        cache_control: None,
    };
    let assistant: message::AssistantContent = content.try_into().unwrap();
    let message::AssistantContent::Text(text) = assistant else {
        panic!("expected text variant");
    };
    let recovered = anthropic_citations(&text).unwrap();
    assert_eq!(recovered.len(), 1);
}

#[test]
fn provider_text_response_concatenates_text_blocks_without_inserted_newlines() {
    let response = CompletionResponse {
        content: vec![
            Content::Text {
                text: "According to the document, ".into(),
                citations: Vec::new(),
                cache_control: None,
            },
            Content::Text {
                text: "the grass is green".into(),
                citations: Vec::new(),
                cache_control: None,
            },
            Content::Text {
                text: " and the sky is blue.".into(),
                citations: Vec::new(),
                cache_control: None,
            },
        ],
        id: "msg_1".into(),
        model: "claude-test".into(),
        role: "assistant".into(),
        stop_reason: Some("end_turn".into()),
        stop_sequence: None,
        usage: Usage {
            input_tokens: 1,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            cache_creation: None,
            output_tokens: 1,
        },
    };

    assert_eq!(
        response.get_text_response().as_deref(),
        Some("According to the document, the grass is green and the sky is blue.")
    );
}

#[test]
fn assistant_text_citations_survive_anthropic_request_conversion() {
    let assistant = message::Message::Assistant {
        id: None,
        content: OneOrMany::one(message::AssistantContent::Text(message::Text {
            text: "the grass is green".into(),
            additional_params: Some(json!({
                "citations": [{
                    "type": "char_location",
                    "cited_text": "The grass is green.",
                    "document_index": 0,
                    "start_char_index": 0,
                    "end_char_index": 20
                }]
            })),
        })),
    };

    let converted: Message = assistant.try_into().unwrap();
    let Content::Text {
        citations, text, ..
    } = converted.content.first()
    else {
        panic!("expected assistant text content");
    };

    assert_eq!(text, "the grass is green");
    assert_eq!(
        citations,
        vec![Citation::CharLocation {
            cited_text: "The grass is green.".into(),
            document_index: 0,
            document_title: None,
            start_char_index: 0,
            end_char_index: 20,
        }]
    );
}

#[test]
fn assistant_text_invalid_known_citations_are_rejected_for_anthropic_request_conversion() {
    let text = message::AssistantContent::Text(message::Text {
        text: "bad citation".into(),
        additional_params: Some(json!({
            "citations": [{
                "type": "char_location",
                "cited_text": "bad"
            }]
        })),
    });

    let result = Content::try_from(text);

    assert!(
        result.is_err(),
        "invalid Anthropic citation metadata should not be silently dropped"
    );
}

#[test]
fn document_additional_params_forward_to_anthropic_document() {
    let doc = message::UserContent::Document(message::Document {
        data: message::DocumentSourceKind::String("Hello world.".into()),
        media_type: Some(message::DocumentMediaType::TXT),
        additional_params: Some(json!({
            "title": "Doc1",
            "context": "ctx",
            "citations": { "enabled": true }
        })),
    });
    let msg = message::Message::User {
        content: OneOrMany::one(doc),
    };
    let converted: Message = msg.try_into().unwrap();
    let block = converted.content.first();
    let Content::Document {
        title,
        context,
        citations,
        ..
    } = block
    else {
        panic!("expected Content::Document");
    };
    assert_eq!(title.as_deref(), Some("Doc1"));
    assert_eq!(context.as_deref(), Some("ctx"));
    assert_eq!(citations, Some(CitationsConfig { enabled: true }));
}

fn assert_reverse_document_metadata(
    source: DocumentSource,
    expected_data: DocumentSourceKind,
    expected_media_type: Option<message::DocumentMediaType>,
) -> message::Message {
    let provider_message = Message {
        role: Role::User,
        content: OneOrMany::one(Content::Document {
            source,
            title: Some("Doc1".into()),
            context: Some("ctx".into()),
            citations: Some(CitationsConfig { enabled: true }),
            cache_control: None,
        }),
    };

    let generic: message::Message = provider_message.try_into().unwrap();
    let message::Message::User { content } = &generic else {
        panic!("expected generic user message");
    };
    let message::UserContent::Document(document) = content.first() else {
        panic!("expected generic document");
    };

    assert_eq!(document.data, expected_data);
    assert_eq!(document.media_type, expected_media_type);
    let additional_params = document
        .additional_params
        .as_ref()
        .expect("expected Anthropic document metadata");
    assert_eq!(additional_params["title"], "Doc1");
    assert_eq!(additional_params["context"], "ctx");
    assert_eq!(additional_params["citations"]["enabled"], true);

    generic
}

#[test]
fn anthropic_document_metadata_survives_reverse_conversion_for_all_sources() {
    assert_reverse_document_metadata(
        DocumentSource::Text {
            data: "Hello world.".into(),
            media_type: PlainTextMediaType::Plain,
        },
        DocumentSourceKind::String("Hello world.".into()),
        Some(message::DocumentMediaType::TXT),
    );
    assert_reverse_document_metadata(
        DocumentSource::Base64 {
            data: "base64-pdf".into(),
            media_type: DocumentFormat::PDF,
        },
        DocumentSourceKind::String("base64-pdf".into()),
        Some(message::DocumentMediaType::PDF),
    );
    assert_reverse_document_metadata(
        DocumentSource::Url {
            url: "https://example.com/doc.pdf".into(),
        },
        DocumentSourceKind::Url("https://example.com/doc.pdf".into()),
        None,
    );
    assert_reverse_document_metadata(
        DocumentSource::File {
            file_id: "file_abc".into(),
        },
        DocumentSourceKind::FileId("file_abc".into()),
        None,
    );
}

#[test]
fn anthropic_document_metadata_survives_reverse_round_trip() {
    let provider_message = Message {
        role: Role::User,
        content: OneOrMany::one(Content::Document {
            source: DocumentSource::Text {
                data: "Hello world.".into(),
                media_type: PlainTextMediaType::Plain,
            },
            title: Some("Doc1".into()),
            context: Some("ctx".into()),
            citations: Some(CitationsConfig { enabled: true }),
            cache_control: None,
        }),
    };

    let generic: message::Message = provider_message.try_into().unwrap();
    let message::Message::User { content } = &generic else {
        panic!("expected generic user message");
    };
    let message::UserContent::Document(document) = content.first() else {
        panic!("expected generic document");
    };
    let additional_params = document
        .additional_params
        .as_ref()
        .expect("expected Anthropic document metadata");
    assert_eq!(additional_params["title"], "Doc1");
    assert_eq!(additional_params["context"], "ctx");
    assert_eq!(additional_params["citations"]["enabled"], true);

    let round_trip: Message = generic.try_into().unwrap();
    let Content::Document {
        title,
        context,
        citations,
        ..
    } = round_trip.content.first()
    else {
        panic!("expected Anthropic document");
    };
    assert_eq!(title.as_deref(), Some("Doc1"));
    assert_eq!(context.as_deref(), Some("ctx"));
    assert_eq!(citations, Some(CitationsConfig { enabled: true }));
}

#[test]
fn anthropic_document_empty_metadata_stays_none_on_reverse_conversion() {
    let provider_message = Message {
        role: Role::User,
        content: OneOrMany::one(Content::Document {
            source: DocumentSource::Text {
                data: "Hello world.".into(),
                media_type: PlainTextMediaType::Plain,
            },
            title: None,
            context: None,
            citations: None,
            cache_control: None,
        }),
    };

    let generic: message::Message = provider_message.try_into().unwrap();
    let message::Message::User { content } = &generic else {
        panic!("expected generic user message");
    };
    let message::UserContent::Document(document) = content.first() else {
        panic!("expected generic document");
    };

    assert_eq!(document.additional_params, None);
}

#[tokio::test]
async fn completion_http_non_success_preserves_status_and_body() {
    use crate::client::CompletionClient;
    use crate::completion::CompletionModel as _;
    use crate::providers::anthropic::Client;
    use crate::test_utils::RecordingHttpClient;

    let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"slow down"}}"#;
    let http_client =
        RecordingHttpClient::with_error_response(http::StatusCode::TOO_MANY_REQUESTS, body);
    let client = Client::builder()
        .api_key("test-key")
        .http_client(http_client)
        .build()
        .expect("build client");
    let model = client.completion_model("claude-sonnet-4-6");
    let request = model.completion_request("hello").max_tokens(1024).build();

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
}

#[tokio::test]
async fn completion_2xx_error_envelope_preserves_status_and_body() {
    use crate::client::CompletionClient;
    use crate::completion::CompletionModel as _;
    use crate::providers::anthropic::Client;
    use crate::test_utils::RecordingHttpClient;

    // Anthropic's `ApiResponse` is internally tagged on `type`; the `Error`
    // arm flattens `ApiErrorResponse { message }`, so a 200-OK error envelope
    // deserializes from `{"type":"error","message":"..."}` and routes through
    // `from_http_response(OK, ..)` into `ProviderResponse`.
    let body = r#"{"type":"error","message":"model overloaded"}"#;
    let http_client = RecordingHttpClient::new(body); // 200 OK
    let client = Client::builder()
        .api_key("test-key")
        .http_client(http_client)
        .build()
        .expect("build client");
    let model = client.completion_model("claude-sonnet-4-6");
    let request = model.completion_request("hello").max_tokens(1024).build();

    let error = model
        .completion(request)
        .await
        .expect_err("completion should fail with provider error envelope");

    match &error {
        CompletionError::ProviderResponse(stored) => {
            assert_eq!(stored.body, body);
            assert_eq!(stored.status, Some(http::StatusCode::OK));
            assert_eq!(error.provider_response_body(), Some(body));
            assert_eq!(error.provider_response_status(), Some(http::StatusCode::OK));
        }
        other => panic!("expected ProviderResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn completion_streaming_http_non_success_preserves_status_and_body() {
    use crate::client::CompletionClient;
    use crate::completion::CompletionModel as _;
    use crate::providers::anthropic::Client;
    use crate::test_utils::HttpErrorStreamingClient;
    use futures::StreamExt;

    let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"slow down"}}"#;
    let http_client = HttpErrorStreamingClient::new(http::StatusCode::SERVICE_UNAVAILABLE, body);
    let client = Client::builder()
        .api_key("test-key")
        .http_client(http_client)
        .build()
        .expect("build client");
    let model = client.completion_model("claude-sonnet-4-6");
    let request = model.completion_request("hello").max_tokens(1024).build();

    let mut stream = model.stream(request).await.expect("stream should start");

    // The transport failure surfaces as the first error item yielded by the stream.
    let error = loop {
        match stream.next().await {
            Some(Ok(_)) => continue,
            Some(Err(error)) => break error,
            None => panic!("stream ended without yielding the transport error"),
        }
    };

    assert!(matches!(error, CompletionError::HttpError(_)));
    assert_eq!(
        error.provider_response_status(),
        Some(http::StatusCode::SERVICE_UNAVAILABLE)
    );
    assert_eq!(error.provider_response_body(), Some(body));

    // The transport failure ends the stream: nothing may follow it that
    // would read as a successfully completed turn.
    assert!(stream.next().await.is_none());
    assert!(
        stream.response.is_none(),
        "a stream cut short by a transport error must not synthesize a terminal record"
    );
}

#[test]
fn coerce_tool_input_normalizes_non_object_arguments() {
    use serde_json::json;

    // Object passes through untouched.
    assert_eq!(
        coerce_tool_input(json!({"q": "rust", "n": 3})),
        json!({"q": "rust", "n": 3})
    );

    // A JSON string that encodes an object is parsed into that object.
    assert_eq!(
        coerce_tool_input(json!("{\"q\":\"rust\"}")),
        json!({"q": "rust"})
    );

    // A non-JSON string, a JSON string that is not an object, null, arrays,
    // numbers and bools all collapse to an empty object: the only shape the
    // Anthropic API accepts for tool_use.input.
    assert_eq!(coerce_tool_input(json!("not json")), json!({}));
    assert_eq!(coerce_tool_input(json!("[1,2,3]")), json!({}));
    assert_eq!(coerce_tool_input(json!(null)), json!({}));
    assert_eq!(coerce_tool_input(json!([1, 2, 3])), json!({}));
    assert_eq!(coerce_tool_input(json!(42)), json!({}));
    assert_eq!(coerce_tool_input(json!(true)), json!({}));
}

// Regression test for issue #1429: PR #1431 added the `DocumentSource::Url`
// wire variant and response-side parsing, but the request-side
// `UserContent::Document` conversion still rejected URL-backed PDFs even
// though the Anthropic Messages API supports
// `"source": {"type": "url", ...}` for PDFs.
// The media type is optional because Anthropic's URL source is implicitly a
// PDF and does not include a media-type field on the wire.
//
// See <https://docs.anthropic.com/en/docs/build-with-claude/pdf-support>
// for URL-sourced PDF documents.
#[test]
fn url_pdf_with_or_without_media_type_converts_to_url_document_source() {
    let pdf_url = "https://example.com/resume.pdf";

    for media_type in [Some(message::DocumentMediaType::PDF), None] {
        let msg = message::Message::User {
            content: OneOrMany::one(message::UserContent::document_url(pdf_url, media_type)),
        };

        let converted = Message::try_from(msg).expect("URL PDF should convert");
        let json = serde_json::to_value(&converted).expect("message should serialize");

        assert_eq!(
            json.pointer("/content/0/source"),
            Some(&json!({ "type": "url", "url": pdf_url })),
            "URL PDF should map to a url document source: {json:#}"
        );
    }
}

// -------------------------------------------------------------------
// Coverage additions: response accessors, usage, citations, content
// conversions, cache control, schema sanitization, and request-builder
// edge cases.
// -------------------------------------------------------------------

fn sample_usage() -> Usage {
    Usage {
        input_tokens: 3,
        cache_read_input_tokens: Some(5),
        cache_creation_input_tokens: Some(7),
        cache_creation: Some(CacheCreationDetail {
            ephemeral_5m_input_tokens: Some(3),
            ephemeral_1h_input_tokens: Some(4),
        }),
        output_tokens: 11,
    }
}

fn sample_completion_response() -> CompletionResponse {
    CompletionResponse {
        content: vec![Content::Text {
            text: "hello".to_string(),
            citations: Vec::new(),
            cache_control: None,
        }],
        id: "msg_9".to_string(),
        model: "claude-test".to_string(),
        role: "assistant".to_string(),
        stop_reason: Some("end_turn".to_string()),
        stop_sequence: None,
        usage: sample_usage(),
    }
}

#[test]
fn provider_response_ext_accessors_expose_id_model_messages_and_usage() {
    let response = sample_completion_response();
    assert_eq!(response.get_response_id(), Some("msg_9".to_string()));
    assert_eq!(
        response.get_response_model_name(),
        Some("claude-test".to_string())
    );
    assert_eq!(response.get_output_messages(), response.content);
    let usage = response.get_usage().expect("usage should be reported");
    assert_eq!(usage.input_tokens, sample_usage().input_tokens);
    assert_eq!(usage.output_tokens, sample_usage().output_tokens);
    assert_eq!(
        usage.cache_read_input_tokens,
        sample_usage().cache_read_input_tokens
    );
    assert_eq!(
        usage.cache_creation_input_tokens,
        sample_usage().cache_creation_input_tokens
    );
}

#[test]
fn usage_display_and_conversion_to_generic_usage() {
    let usage = sample_usage();
    let display = usage.to_string();
    assert!(display.contains("Input tokens: 3"));
    assert!(display.contains("Cache read input tokens: 5"));
    assert!(display.contains("Cache creation input tokens: 7"));
    assert!(display.contains("1h cache creation input tokens: 4"));
    assert!(display.contains("Output tokens: 11"));

    // Absent cache counts render as `n/a`.
    let bare = Usage {
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        cache_creation: None,
        ..sample_usage()
    };
    assert!(bare.to_string().contains("Cache read input tokens: n/a"));
    assert!(
        bare.to_string()
            .contains("Cache creation input tokens: n/a")
    );
    assert!(
        bare.to_string()
            .contains("1h cache creation input tokens: n/a")
    );

    let generic = crate::completion::Usage::from(usage);
    assert_eq!(generic.input_tokens, 3);
    assert_eq!(generic.output_tokens, 11);
    assert_eq!(generic.cached_input_tokens, 5);
    assert_eq!(generic.cache_creation_input_tokens, 7);
    assert_eq!(generic.cache_creation_1h_input_tokens, 4);
    // The 1h figure is a breakdown of the aggregate, not an addition:
    // the total stays computed from the aggregate alone.
    assert_eq!(generic.total_tokens, 3 + 5 + 7 + 11);
}

/// The recorded Messages wire always carries the TTL breakdown on the
/// usage payload (`"cache_creation":{"ephemeral_5m_input_tokens":0,
/// "ephemeral_1h_input_tokens":0}`); it must decode, and a payload
/// without it (older relays) must still decode via `#[serde(default)]`.
#[test]
fn usage_decodes_the_wire_cache_creation_breakdown() {
    let usage: Usage = serde_json::from_value(json!({
        "input_tokens": 37,
        "output_tokens": 75,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
        "cache_creation": {
            "ephemeral_1h_input_tokens": 0,
            "ephemeral_5m_input_tokens": 0
        }
    }))
    .expect("the recorded wire shape should decode");
    assert_eq!(
        usage.cache_creation,
        Some(CacheCreationDetail {
            ephemeral_5m_input_tokens: Some(0),
            ephemeral_1h_input_tokens: Some(0),
        })
    );

    let without_breakdown: Usage = serde_json::from_value(json!({
        "input_tokens": 37,
        "output_tokens": 75
    }))
    .expect("a usage payload without the breakdown should still decode");
    assert_eq!(without_breakdown.cache_creation, None);
}

#[test]
fn cache_control_1h_constructor_serializes_extended_ttl() {
    assert_eq!(
        CacheControl::ephemeral_1h(),
        CacheControl::Ephemeral {
            ttl: Some(CacheTtl::OneHour)
        }
    );
    assert_eq!(
        serde_json::to_value(CacheControl::ephemeral_1h()).unwrap(),
        json!({"type": "ephemeral", "ttl": "1h"})
    );
}

#[test]
fn search_result_location_citation_roundtrips() {
    let citation = Citation::SearchResultLocation {
        cited_text: "Quoted text.".into(),
        source: "https://example.com/search".into(),
        title: Some("Example".into()),
        search_result_index: 2,
        start_block_index: 0,
        end_block_index: 3,
    };
    let value = serde_json::to_value(&citation).unwrap();
    assert_eq!(value["type"], "search_result_location");
    assert_eq!(value["cited_text"], "Quoted text.");
    assert_eq!(value["source"], "https://example.com/search");
    assert_eq!(value["title"], "Example");
    assert_eq!(value["search_result_index"], 2);
    assert_eq!(value["start_block_index"], 0);
    assert_eq!(value["end_block_index"], 3);
    let back: Citation = serde_json::from_value(value).unwrap();
    assert_eq!(back, citation);

    let no_title = Citation::SearchResultLocation {
        cited_text: "Quoted text.".into(),
        source: "https://example.com/search".into(),
        title: None,
        search_result_index: 2,
        start_block_index: 0,
        end_block_index: 3,
    };
    let value = serde_json::to_value(&no_title).unwrap();
    assert!(value.get("title").is_none());
    let back: Citation = serde_json::from_value(value).unwrap();
    assert_eq!(back, no_title);
}

#[test]
fn content_block_location_citation_serializes_document_title() {
    let citation = Citation::ContentBlockLocation {
        cited_text: "Findings.".into(),
        document_index: 1,
        document_title: Some("Custom Doc".into()),
        start_block_index: 0,
        end_block_index: 1,
    };
    let value = serde_json::to_value(&citation).unwrap();
    assert_eq!(value["type"], "content_block_location");
    assert_eq!(value["document_title"], "Custom Doc");
}

#[test]
fn unknown_citation_serializes_raw_and_typeless_payload_deserializes_as_unknown() {
    let raw = json!({"type": "future_location", "field": "kept"});
    let citation = Citation::Unknown(raw.clone());
    assert_eq!(serde_json::to_value(&citation).unwrap(), raw);

    // A citation payload without a `type` field stays raw.
    let parsed: Citation = serde_json::from_value(json!({"cited_text": "x"})).unwrap();
    assert!(matches!(parsed, Citation::Unknown(value) if value["cited_text"] == "x"));
}

#[test]
fn document_additional_params_with_invalid_citations_config_errors() {
    let doc = message::UserContent::Document(message::Document {
        data: DocumentSourceKind::String("doc".into()),
        media_type: Some(message::DocumentMediaType::TXT),
        additional_params: Some(json!({"citations": {"enabled": "not-a-bool"}})),
    });
    let msg = message::Message::User {
        content: OneOrMany::one(doc),
    };

    let err = Message::try_from(msg)
        .expect_err("invalid citations metadata must not be silently dropped");
    assert!(err.to_string().contains("not a valid CitationsConfig"));
}

fn raw_content_params(raw: serde_json::Value, text: &str) -> message::Text {
    let mut params = serde_json::Map::new();
    params.insert(ANTHROPIC_RAW_CONTENT_KEY.to_string(), raw);
    message::Text {
        text: text.to_string(),
        additional_params: Some(serde_json::Value::Object(params)),
    }
}

#[test]
fn raw_content_metadata_paths_are_validated() {
    // Valid server-tool block combined with non-empty text is rejected.
    let assistant_text = message::AssistantContent::Text(raw_content_params(
        json!({
            "type": "server_tool_use",
            "id": "srvtoolu_01",
            "name": "web_search",
            "input": {"query": "claude"}
        }),
        "leftover text",
    ));
    let err = Content::try_from(assistant_text)
        .expect_err("raw content metadata cannot combine with text");
    assert!(
        err.to_string()
            .contains("cannot be combined with non-empty text")
    );

    // A valid server-tool block with empty text converts.
    let assistant_text = message::AssistantContent::Text(raw_content_params(
        json!({
            "type": "web_search_tool_result",
            "tool_use_id": "srvtoolu_01",
            "content": []
        }),
        "",
    ));
    let content = Content::try_from(assistant_text).expect("raw content should convert to a block");
    assert!(matches!(content, Content::WebSearchToolResult { .. }));

    // Payloads that are not Anthropic content blocks at all are rejected.
    let err = extract_anthropic_raw_content(&raw_content_params(json!("not-a-block"), ""))
        .expect_err("non-object payloads are invalid Anthropic content");
    assert!(err.to_string().contains("is not valid Anthropic content"));

    // Client-writable block types are rejected: only server tool blocks may
    // travel through the escape hatch.
    let err = extract_anthropic_raw_content(&raw_content_params(
        json!({"type": "text", "text": "nope"}),
        "",
    ))
    .expect_err("only server tool blocks are supported");
    assert!(err.to_string().contains("only supports"));
}

#[test]
fn from_string_conversions_produce_text_variants() {
    let content: Content = "hello".to_string().into();
    assert_eq!(
        content,
        Content::Text {
            text: "hello".to_string(),
            citations: Vec::new(),
            cache_control: None,
        }
    );

    let tool_result_content: ToolResultContent = "result".to_string().into();
    assert_eq!(
        tool_result_content,
        ToolResultContent::Text {
            text: "result".to_string()
        }
    );
}

#[test]
fn image_format_conversions_cover_supported_and_unsupported_types() {
    let pairs = [
        (ImageFormat::JPEG, message::ImageMediaType::JPEG),
        (ImageFormat::PNG, message::ImageMediaType::PNG),
        (ImageFormat::GIF, message::ImageMediaType::GIF),
        (ImageFormat::WEBP, message::ImageMediaType::WEBP),
    ];
    for (format, media_type) in pairs {
        let converted: ImageFormat = media_type.clone().try_into().unwrap();
        assert_eq!(converted, format);
        let back: message::ImageMediaType = format.into();
        assert_eq!(back, media_type);
    }

    let err = ImageFormat::try_from(message::ImageMediaType::SVG)
        .expect_err("SVG is not supported by the Anthropic API");
    assert!(err.to_string().contains("Unsupported image media type"));
}

#[test]
fn document_format_conversions_require_pdf_for_base64_sources() {
    assert_eq!(
        DocumentFormat::try_from(DocumentMediaType::PDF).unwrap(),
        DocumentFormat::PDF
    );

    let err = DocumentFormat::try_from(DocumentMediaType::HTML)
        .expect_err("only PDF converts to an Anthropic document format");
    assert!(err.to_string().contains("DocumentFormat only supports PDF"));
}

#[test]
fn assistant_content_variants_convert_to_anthropic_content() {
    // Assistant images are not supported by the Anthropic API.
    let err = Content::try_from(message::AssistantContent::image_base64(
        "dg==",
        Some(message::ImageMediaType::PNG),
        None,
    ))
    .expect_err("assistant images are unsupported");
    assert!(err.to_string().contains("doesn't support images"));

    // Tool calls map onto tool_use blocks with coerced object input.
    let content = Content::try_from(message::AssistantContent::tool_call(
        "toolu_01",
        "get_weather",
        json!({"city": "SF"}),
    ))
    .unwrap();
    assert!(matches!(
        &content,
        Content::ToolUse { id, name, input }
            if id == "toolu_01" && name == "get_weather" && input["city"] == "SF"
    ));

    // Reasoning maps onto a thinking block with its signature.
    let content = Content::try_from(message::AssistantContent::Reasoning(
        message::Reasoning::new_with_signature("thinking hard", Some("sig-1".to_string())),
    ))
    .unwrap();
    assert!(matches!(
        content,
        Content::Thinking {
            thinking,
            signature: Some(signature),
        } if thinking == "thinking hard" && signature == "sig-1"
    ));
}

#[test]
fn assistant_message_image_and_empty_reasoning_are_rejected() {
    let image_message = message::Message::Assistant {
        id: None,
        content: OneOrMany::one(message::AssistantContent::image_base64(
            "dg==",
            Some(message::ImageMediaType::PNG),
            None,
        )),
    };
    let err = Message::try_from(image_message).expect_err("assistant images are unsupported");
    assert!(err.to_string().contains("doesn't support images"));

    let empty_reasoning = message::Message::Assistant {
        id: None,
        content: OneOrMany::one(message::AssistantContent::Reasoning(message::Reasoning {
            id: None,
            content: Vec::new(),
        })),
    };
    let err = Message::try_from(empty_reasoning).expect_err("empty reasoning is rejected");
    assert!(err.to_string().contains("empty reasoning content"));
}

#[test]
fn user_tool_result_image_content_converts_to_anthropic_tool_result_image() {
    let msg = message::Message::User {
        content: OneOrMany::one(message::UserContent::tool_result(
            "toolu_01",
            OneOrMany::one(message::ToolResultContent::image_base64(
                "aVpv",
                Some(message::ImageMediaType::PNG),
                None,
            )),
        )),
    };
    let converted: Message = msg.try_into().unwrap();
    let Content::ToolResult { content, .. } = converted.content.first() else {
        panic!("expected tool result");
    };
    assert_eq!(
        content.first(),
        ToolResultContent::Image {
            source: ImageSource::Base64 {
                data: "aVpv".to_string(),
                media_type: ImageFormat::PNG,
            }
        }
    );

    // URL-backed tool result images are rejected: only base64 is accepted.
    let msg = message::Message::User {
        content: OneOrMany::one(message::UserContent::tool_result(
            "toolu_01",
            OneOrMany::one(message::ToolResultContent::image_url(
                "https://example.com/shot.png",
                None,
                None,
            )),
        )),
    };
    let err = Message::try_from(msg).expect_err("URL tool result images are rejected");
    assert!(err.to_string().contains("Only base64 strings"));

    // Base64 tool result images require a media type.
    let msg = message::Message::User {
        content: OneOrMany::one(message::UserContent::tool_result(
            "toolu_01",
            OneOrMany::one(message::ToolResultContent::image_base64("aVpv", None, None)),
        )),
    };
    let err = Message::try_from(msg).expect_err("media type is required");
    assert!(err.to_string().contains("Image media type is required"));
}

#[test]
fn user_image_url_converts_and_unknown_or_raw_sources_error() {
    let url_msg = message::Message::User {
        content: OneOrMany::one(message::UserContent::image_url(
            "https://example.com/cat.png",
            None,
            None,
        )),
    };
    let converted: Message = url_msg.try_into().unwrap();
    assert_eq!(
        converted.content.first(),
        Content::Image {
            source: ImageSource::Url {
                url: "https://example.com/cat.png".to_string()
            },
            cache_control: None,
        }
    );

    let unknown_msg = message::Message::User {
        content: OneOrMany::one(message::UserContent::Image(message::Image::default())),
    };
    let err = Message::try_from(unknown_msg).expect_err("empty image bodies are rejected");
    assert!(err.to_string().contains("Image content has no body"));

    let raw_msg = message::Message::User {
        content: OneOrMany::one(message::UserContent::Image(message::Image {
            data: DocumentSourceKind::Raw(vec![1, 2, 3]),
            media_type: None,
            detail: None,
            additional_params: None,
        })),
    };
    let err = Message::try_from(raw_msg).expect_err("raw image bytes are rejected");
    assert!(err.to_string().contains("Unsupported document type"));
}

#[test]
fn user_document_conversions_cover_pdf_and_plaintext_source_kinds() {
    // String-backed PDF payloads map onto the base64 source kind.
    let string_pdf = message::Message::User {
        content: OneOrMany::one(message::UserContent::document(
            "cGRm",
            Some(message::DocumentMediaType::PDF),
        )),
    };
    let converted: Message = string_pdf.try_into().unwrap();
    assert!(matches!(
        converted.content.first(),
        Content::Document {
            source: DocumentSource::Base64 {
                data,
                media_type: DocumentFormat::PDF,
            },
            ..
        } if data == "cGRm"
    ));

    // Base64-backed PDF payloads map onto the same source kind.
    let base64_pdf = message::Message::User {
        content: OneOrMany::one(message::UserContent::Document(message::Document {
            data: DocumentSourceKind::Base64("cGRm".into()),
            media_type: Some(message::DocumentMediaType::PDF),
            additional_params: None,
        })),
    };
    let converted: Message = base64_pdf.try_into().unwrap();
    assert!(matches!(
        converted.content.first(),
        Content::Document {
            source: DocumentSource::Base64 { .. },
            ..
        }
    ));

    // Base64-backed plain text documents map onto the text source kind.
    let base64_txt = message::Message::User {
        content: OneOrMany::one(message::UserContent::Document(message::Document {
            data: DocumentSourceKind::Base64("aGVsbG8=".into()),
            media_type: Some(message::DocumentMediaType::TXT),
            additional_params: None,
        })),
    };
    let converted: Message = base64_txt.try_into().unwrap();
    assert!(matches!(
        converted.content.first(),
        Content::Document {
            source: DocumentSource::Text {
                data,
                media_type: PlainTextMediaType::Plain,
            },
            ..
        } if data == "aGVsbG8="
    ));

    // A document with neither media type nor URL source is rejected.
    let no_media_type = message::Message::User {
        content: OneOrMany::one(message::UserContent::Document(message::Document {
            data: DocumentSourceKind::String("doc".into()),
            media_type: None,
            additional_params: None,
        })),
    };
    let err = Message::try_from(no_media_type).expect_err("media type is required");
    assert!(err.to_string().contains("Document media type is required"));

    // PDF documents cannot be backed by opaque/unknown sources.
    let unknown_pdf = message::Message::User {
        content: OneOrMany::one(message::UserContent::Document(message::Document {
            data: DocumentSourceKind::Unknown,
            media_type: Some(message::DocumentMediaType::PDF),
            additional_params: None,
        })),
    };
    let err = Message::try_from(unknown_pdf).expect_err("unknown PDF sources are rejected");
    assert!(
        err.to_string()
            .contains("Only base64 encoded data or URLs are supported for PDF")
    );
}

#[test]
fn audio_and_video_user_content_are_rejected() {
    let audio = message::Message::User {
        content: OneOrMany::one(message::UserContent::audio(
            "data",
            Some(message::AudioMediaType::MP3),
        )),
    };
    let err = Message::try_from(audio).expect_err("audio is unsupported");
    assert!(err.to_string().contains("Audio is not supported"));

    let video = message::Message::User {
        content: OneOrMany::one(message::UserContent::video(
            "data",
            Some(message::VideoMediaType::MP4),
        )),
    };
    let err = Message::try_from(video).expect_err("video is unsupported");
    assert!(err.to_string().contains("Video is not supported"));
}

#[test]
fn content_to_assistant_content_rejects_non_assistant_variants() {
    let err = message::AssistantContent::try_from(Content::Image {
        source: ImageSource::Url {
            url: "https://example.com/cat.png".to_string(),
        },
        cache_control: None,
    })
    .expect_err("images cannot become assistant content");
    assert!(
        err.to_string()
            .contains("did not contain a message, tool call, or reasoning")
    );
}

#[test]
fn tool_result_content_image_sources_convert_back_to_generic_content() {
    let base64: message::ToolResultContent = ToolResultContent::Image {
        source: ImageSource::Base64 {
            data: "aVpv".to_string(),
            media_type: ImageFormat::PNG,
        },
    }
    .into();
    assert_eq!(
        base64,
        message::ToolResultContent::image_base64("aVpv", Some(message::ImageMediaType::PNG), None)
    );

    let url: message::ToolResultContent = ToolResultContent::Image {
        source: ImageSource::Url {
            url: "https://example.com/shot.png".to_string(),
        },
    }
    .into();
    assert_eq!(
        url,
        message::ToolResultContent::image_url("https://example.com/shot.png", None, None)
    );
}

#[test]
fn url_image_message_converts_back_to_generic_url_image() {
    let provider_message = Message {
        role: Role::User,
        content: OneOrMany::one(Content::Image {
            source: ImageSource::Url {
                url: "https://example.com/cat.png".to_string(),
            },
            cache_control: None,
        }),
    };
    let generic: message::Message = provider_message.try_into().unwrap();
    let message::Message::User { content } = generic else {
        panic!("expected user message");
    };
    assert!(matches!(
        content.first(),
        message::UserContent::Image(message::Image {
            data: DocumentSourceKind::Url(url),
            media_type: None,
            ..
        }) if url == "https://example.com/cat.png"
    ));
}

#[test]
fn unsupported_variants_error_for_user_and_system_roles() {
    let user_tool_use = Message {
        role: Role::User,
        content: OneOrMany::one(Content::ToolUse {
            id: "toolu_01".to_string(),
            name: "get_weather".to_string(),
            input: json!({}),
        }),
    };
    let err = message::Message::try_from(user_tool_use)
        .expect_err("tool_use cannot appear in a user message");
    assert!(
        err.to_string()
            .contains("Unsupported content type for User role")
    );

    let system_tool_use = Message {
        role: Role::System,
        content: OneOrMany::one(Content::ToolUse {
            id: "toolu_01".to_string(),
            name: "get_weather".to_string(),
            input: json!({}),
        }),
    };
    let err = message::Message::try_from(system_tool_use)
        .expect_err("tool_use cannot appear in a system message");
    assert!(
        err.to_string()
            .contains("Unsupported content type for System role")
    );
}

#[test]
fn completion_model_constructors_and_automatic_caching_flags() {
    use crate::client::CompletionClient;
    use crate::providers::anthropic::Client;
    use crate::test_utils::RecordingHttpClient;

    let client = Client::builder()
        .api_key("test-key")
        .http_client(RecordingHttpClient::new("{}"))
        .build()
        .expect("build client");

    let model = client.completion_model("claude-a");
    assert_eq!(model.model, "claude-a");
    assert!(!model.prompt_caching);
    assert!(!model.automatic_caching);
    assert_eq!(model.automatic_caching_ttl, None);

    let model = client
        .completion_model("claude-b")
        .with_prompt_caching()
        .with_automatic_caching()
        .with_automatic_caching_1h();
    assert_eq!(model.model, "claude-b");
    assert!(model.prompt_caching);
    assert!(model.automatic_caching);
    assert_eq!(model.automatic_caching_ttl, Some(CacheTtl::OneHour));

    let model = CompletionModel::with_model(client, "claude-c");
    assert_eq!(model.model, "claude-c");
    assert!(!model.prompt_caching);
    assert!(!model.automatic_caching);
    assert_eq!(model.automatic_caching_ttl, None);
}

#[test]
fn tool_choice_specific_requires_exactly_one_function() {
    assert!(matches!(
        ToolChoice::try_from(message::ToolChoice::Auto).unwrap(),
        ToolChoice::Auto
    ));
    assert!(matches!(
        ToolChoice::try_from(message::ToolChoice::None).unwrap(),
        ToolChoice::None
    ));
    assert!(matches!(
        ToolChoice::try_from(message::ToolChoice::Required).unwrap(),
        ToolChoice::Any
    ));
    assert!(matches!(
        ToolChoice::try_from(message::ToolChoice::Specific {
            function_names: vec!["get_weather".to_string()],
        })
        .unwrap(),
        ToolChoice::Tool { name } if name == "get_weather"
    ));

    let err = ToolChoice::try_from(message::ToolChoice::Specific {
        function_names: Vec::new(),
    })
    .expect_err("zero tool names are rejected");
    assert!(err.to_string().contains("Only one tool may be specified"));

    let err = ToolChoice::try_from(message::ToolChoice::Specific {
        function_names: vec!["a".to_string(), "b".to_string()],
    })
    .expect_err("multiple tool names are rejected");
    assert!(err.to_string().contains("Only one tool may be specified"));
}

#[test]
fn sanitize_schema_strips_numeric_constraints_and_enforces_strict_objects() {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "count": {
                "type": "integer",
                "minimum": 0,
                "maximum": 10,
                "exclusiveMinimum": -1,
                "exclusiveMaximum": 5,
                "multipleOf": 2
            },
            "ratio": {"type": "number", "minimum": 0.5}
        }
    });
    sanitize_schema(&mut schema);

    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(schema["required"], json!(["name", "count", "ratio"]));

    let count = &schema["properties"]["count"];
    for key in [
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
    ] {
        assert!(
            count.get(key).is_none(),
            "{key} should be stripped: {count}"
        );
    }

    assert!(schema["properties"]["ratio"].get("minimum").is_none());
}

#[test]
fn sanitize_schema_recurses_into_defs_properties_items_and_variants() {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "nested": {
                "type": "object",
                "properties": {"inner": {"type": "integer", "minimum": 1}}
            },
            "list": {
                "type": "array",
                "items": {"properties": {"item": {"type": "number", "maximum": 2}}}
            },
            "pick": {"oneOf": [{"type": "string"}, {"oneOf": [{"type": "boolean"}]}]},
            "merged": {
                "anyOf": [{"type": "null"}],
                "oneOf": [{"type": "string"}]
            },
            "combo": {
                "anyOf": [{"type": "integer", "minimum": 3}],
                "allOf": [{"type": "object", "properties": {"x": {"type": "string"}}}]
            }
        },
        "$defs": {
            "definition": {"properties": {"y": {"type": "integer", "exclusiveMinimum": 4}}}
        }
    });
    sanitize_schema(&mut schema);

    // Nested properties recurse.
    assert!(
        schema["properties"]["nested"]["properties"]["inner"]
            .get("minimum")
            .is_none()
    );
    assert_eq!(schema["properties"]["nested"]["required"], json!(["inner"]));

    // Array items recurse.
    let item = &schema["properties"]["list"]["items"];
    assert_eq!(item["additionalProperties"], json!(false));
    assert_eq!(item["required"], json!(["item"]));
    assert!(item["properties"]["item"].get("maximum").is_none());

    // oneOf is rewritten to anyOf and recursed into: the nested oneOf
    // inside the second variant is itself rewritten.
    let pick = &schema["properties"]["pick"];
    assert!(pick.get("oneOf").is_none());
    assert_eq!(pick["anyOf"][1]["anyOf"][0]["type"], "boolean");

    // An existing anyOf absorbs the oneOf variants.
    let merged = &schema["properties"]["merged"];
    assert!(merged.get("oneOf").is_none());
    assert_eq!(merged["anyOf"].as_array().unwrap().len(), 2);

    // anyOf/allOf variants recurse (numeric constraints stripped, strict
    // objects completed).
    assert!(
        schema["properties"]["combo"]["anyOf"][0]
            .get("minimum")
            .is_none()
    );
    assert_eq!(
        schema["properties"]["combo"]["allOf"][0]["required"],
        json!(["x"])
    );

    // $defs recurse.
    assert!(
        schema["$defs"]["definition"]["properties"]["y"]
            .get("exclusiveMinimum")
            .is_none()
    );
    assert_eq!(schema["$defs"]["definition"]["required"], json!(["y"]));
}

#[test]
fn apply_cache_control_marks_tool_result_image_and_document_blocks() {
    let tool_result_message = Message {
        role: Role::User,
        content: OneOrMany::one(Content::ToolResult {
            tool_use_id: "toolu_01".to_string(),
            content: OneOrMany::one(ToolResultContent::Text {
                text: "15 degrees".to_string(),
            }),
            is_error: None,
            cache_control: None,
        }),
    };
    let image_message = Message {
        role: Role::User,
        content: OneOrMany::one(Content::Image {
            source: ImageSource::Url {
                url: "https://example.com/cat.png".to_string(),
            },
            cache_control: None,
        }),
    };
    let document_message = Message {
        role: Role::User,
        content: OneOrMany::one(Content::Document {
            source: DocumentSource::Text {
                data: "doc".to_string(),
                media_type: PlainTextMediaType::Plain,
            },
            title: None,
            context: None,
            citations: None,
            cache_control: None,
        }),
    };

    for message in [tool_result_message, image_message, document_message] {
        let mut messages = vec![message];
        apply_cache_control(&mut [], &mut messages);
        let block = messages[0].content.first();
        let cache_control =
            content_cache_control(&block).expect("last content block should be marked for caching");
        assert_eq!(*cache_control, CacheControl::ephemeral());
    }
}

#[test]
fn top_level_cache_control_null_is_removed_and_invalid_payloads_error() {
    let mut params = json!({"cache_control": null, "metadata": {"source": "kept"}});
    assert_eq!(extract_top_level_cache_control(&mut params).unwrap(), None);
    assert!(params.get("cache_control").is_none());
    assert_eq!(params["metadata"]["source"], "kept");

    let mut invalid = json!({"cache_control": {"type": "definitely-not-ephemeral"}});
    let err = extract_top_level_cache_control(&mut invalid)
        .expect_err("unknown cache control types are rejected");
    assert!(
        err.to_string()
            .contains("Invalid Anthropic `additional_params.cache_control` payload")
    );

    let mut absent = json!({"metadata": {"source": "no-cache-control"}});
    assert_eq!(extract_top_level_cache_control(&mut absent).unwrap(), None);
}

#[test]
fn empty_preamble_produces_no_system_blocks() {
    let request =
        completion_request_with_history(vec![message::Message::user("hi")], Some(String::new()));
    let converted = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: false,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .unwrap();

    let value = serde_json::to_value(&converted).unwrap();
    // An empty system array is skipped entirely during serialization.
    assert!(
        value
            .get("system")
            .and_then(|system| system.as_array())
            .is_none_or(Vec::is_empty)
    );
}

#[test]
fn invalid_additional_params_tools_payload_errors() {
    let request = completion_request_with_tools(Vec::new(), Some(json!({"tools": "not-an-array"})));

    let err = AnthropicCompletionRequest::try_from(AnthropicRequestParams {
        model: "claude-sonnet-4-6",
        request,
        prompt_caching: false,
        automatic_caching: false,
        automatic_caching_ttl: None,
    })
    .expect_err("invalid tools payload must error loudly");

    assert!(
        err.to_string()
            .contains("Invalid Anthropic `additional_params.tools` payload")
    );
}

#[tokio::test]
async fn raw_completion_emits_request_and_response_trace_logs() {
    use crate::client::CompletionClient;
    use crate::completion::CompletionModel as _;
    use crate::providers::anthropic::Client;
    use crate::test_utils::RecordingHttpClient;

    // Scoped-subscriber tests must not run concurrently; see
    // `test_utils::scoped_tracing_subscriber_guard`.
    let _isolation = crate::test_utils::scoped_tracing_subscriber_guard().await;
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .without_time()
        .with_writer(std::io::sink)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let body = r#"{
            "type": "message",
            "id": "msg_trace",
            "model": "claude-sonnet-4-6",
            "role": "assistant",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 1, "output_tokens": 1},
            "content": [{"type": "text", "text": "hi"}]
        }"#;
    let http_client = RecordingHttpClient::new(body);
    let client = Client::builder()
        .api_key("test-key")
        .http_client(http_client)
        .build()
        .expect("build client");
    let model = client.completion_model("claude-sonnet-4-6");
    let request = model.completion_request("hello").max_tokens(64).build();

    let response = model
        .raw_completion(request)
        .await
        .expect("trace-enabled completion should succeed");

    assert_eq!(response.id, "msg_trace");
    assert_eq!(response.get_text_response().as_deref(), Some("hi"));
}

#[test]
fn user_tool_result_json_content_serializes_as_text() {
    let msg = message::Message::User {
        content: OneOrMany::one(message::UserContent::tool_result(
            "toolu_01",
            OneOrMany::one(message::ToolResultContent::json(json!({"answer": 42}))),
        )),
    };
    let converted: Message = msg.try_into().unwrap();
    let Content::ToolResult { content, .. } = converted.content.first() else {
        panic!("expected tool result");
    };
    assert_eq!(
        content.first(),
        ToolResultContent::Text {
            text: json!({"answer": 42}).to_string()
        }
    );
}

#[test]
fn thinking_content_converts_back_to_assistant_reasoning() {
    let content = Content::Thinking {
        thinking: "step by step".to_string(),
        signature: Some("sig-1".to_string()),
    };
    let converted: message::AssistantContent = content.try_into().unwrap();
    assert!(matches!(
        converted,
        message::AssistantContent::Reasoning(message::Reasoning { content, .. })
            if matches!(
                content.first(),
                Some(message::ReasoningContent::Text { text, signature: Some(signature) })
                    if text == "step by step" && signature == "sig-1"
            )
    ));
}

#[test]
fn cache_control_helpers_ignore_non_cacheable_variants() {
    // The catch-all arms: thinking blocks carry no cache breakpoint, so
    // setting one is a no-op and reading one yields `None`.
    let mut content = Content::Thinking {
        thinking: "thought".to_string(),
        signature: None,
    };
    set_content_cache_control(&mut content, Some(CacheControl::ephemeral()));
    assert!(content_cache_control(&content).is_none());

    // apply_cache_control tolerates a tool_use block as the final block:
    // the marker is simply not applied.
    let mut messages = vec![Message {
        role: Role::Assistant,
        content: OneOrMany::one(Content::ToolUse {
            id: "toolu_01".to_string(),
            name: "get_weather".to_string(),
            input: json!({}),
        }),
    }];
    apply_cache_control(&mut [], &mut messages);
    assert!(matches!(
        messages[0].content.first(),
        Content::ToolUse { .. }
    ));
}

#[test]
fn completion_model_capabilities_and_construct() {
    use crate::client::CompletionClient;
    use crate::client::ConstructCompletionModel as _;
    use crate::completion::CompletionModel as _;
    use crate::providers::anthropic::Client;
    use crate::test_utils::RecordingHttpClient;

    let client = Client::builder()
        .api_key("test-key")
        .http_client(RecordingHttpClient::new("{}"))
        .build()
        .expect("build client");
    let model = client.completion_model("claude-a");

    // Anthropic's structured outputs compose with strict tool use.
    let capabilities = model.capabilities();
    assert!(capabilities.composes_native_output_with_tools);

    let constructed = CompletionModel::construct(&client, "claude-b".to_string());
    assert_eq!(constructed.model, "claude-b");
}

#[tokio::test]
async fn completion_normalizes_a_successful_wire_response() {
    use crate::client::CompletionClient;
    use crate::completion::CompletionModel as _;
    use crate::providers::anthropic::Client;
    use crate::test_utils::RecordingHttpClient;

    let body = r#"{
            "type": "message",
            "id": "msg_normalized",
            "model": "claude-sonnet-4-6",
            "role": "assistant",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 4, "output_tokens": 6},
            "content": [{"type": "text", "text": "the answer"}]
        }"#;
    let http_client = RecordingHttpClient::new(body);
    let client = Client::builder()
        .api_key("test-key")
        .http_client(http_client)
        .build()
        .expect("build client");
    let model = client.completion_model("claude-sonnet-4-6");
    let request = model.completion_request("hello").max_tokens(64).build();

    let response = model
        .completion(request)
        .await
        .expect("completion should succeed and normalize");

    assert_eq!(response.provider, "anthropic");
    assert_eq!(response.message_id.as_deref(), Some("msg_normalized"));
    assert_eq!(response.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(
        response.finish_reason(),
        Some(completion::FinishReason::Stop)
    );
    assert!(matches!(
        response.choice.first(),
        completion::AssistantContent::Text(text) if text.text == "the answer"
    ));
}
