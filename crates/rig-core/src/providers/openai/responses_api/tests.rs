
use super::*;
use crate::completion::CompletionRequestBuilder;
use crate::message;
use crate::test_utils::MockCompletionModel;
use serde_json::json;
use std::collections::HashMap;

fn test_document(id: &str, text: &str) -> crate::completion::Document {
    crate::completion::Document {
        id: id.to_string(),
        text: text.to_string(),
        additional_props: HashMap::new(),
    }
}

fn weather_tool_definition() -> completion::ToolDefinition {
    completion::ToolDefinition {
        name: "get_weather".to_string(),
        description: "Get the weather".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "location": {"type": "string"},
                "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]}
            },
            "required": ["location"]
        }),
    }
}

fn rig_tool_result(content: message::ToolResultContent) -> message::Message {
    message::Message::User {
        content: OneOrMany::one(message::UserContent::ToolResult(message::ToolResult {
            id: "result-id".to_string(),
            call_id: Some("call-id".to_string()),
            content: OneOrMany::one(content),
        })),
    }
}

#[test]
fn mixed_user_content_preserves_order_around_tool_results() {
    let input = message::Message::User {
        content: OneOrMany::many(vec![
            message::UserContent::text("before"),
            message::UserContent::tool_result_with_call_id(
                "result-id",
                "call-id".to_string(),
                OneOrMany::one(message::ToolResultContent::text("tool output")),
            ),
            message::UserContent::text("after"),
        ])
        .expect("mixed content should be non-empty"),
    };

    let messages = Vec::<Message>::try_from(input).expect("message conversion");

    assert!(matches!(
        messages.as_slice(),
        [
            Message::User { content: before, .. },
            Message::ToolResult { tool_call_id, .. },
            Message::User { content: after, .. },
        ] if matches!(before.first(), UserContent::InputText { text } if text == "before")
            && tool_call_id == "call-id"
            && matches!(after.first(), UserContent::InputText { text } if text == "after")
    ));
}

fn reasoning_input_items(items: &[InputItem]) -> Vec<serde_json::Value> {
    items
        .iter()
        .map(|item| serde_json::to_value(item).expect("input item should serialize"))
        .filter(|value| value["type"] == "reasoning")
        .collect()
}

/// F7 leak route (a): reasoning replayed cross-provider — another
/// provider's stream aggregated under a boundary-minted id and swapped
/// onto a Responses model — must not serialize the fabricated id
/// upstream; the item is dropped like main dropped id-less reasoning.
/// A wire-plausible id keeps round-tripping.
#[tokio::test]
async fn cross_provider_minted_reasoning_ids_are_not_serialized_upstream() {
    use crate::completion::CompletionModel as _;
    use crate::test_utils::MockStreamEvent;
    use futures::StreamExt as _;

    // The constant-id shape gemini/ollama/chat-compat streams leave in
    // history, via the mock model's streaming pipeline.
    let model = MockCompletionModel::from_stream_turns([vec![
        MockStreamEvent::reasoning_delta("thinking hard"),
        MockStreamEvent::text("answer"),
        MockStreamEvent::final_response_with_default_usage(),
    ]]);
    let request = CompletionRequestBuilder::new(model.clone(), "hi").build();
    let mut stream = model.stream(request).await.expect("mock stream");
    while stream.next().await.is_some() {}
    let choice = stream.choice.clone();
    // The provenance funnel: a minted stream identity never becomes the
    // durable `Reasoning::id`, so the replayed history carries no id at
    // all — there is nothing for a serializer gate to filter, and no
    // gate exists.
    assert!(
        choice.iter().any(
            |content| matches!(content, message::AssistantContent::Reasoning(reasoning)
                    if reasoning.id.is_none())
        ),
        "a minted stream identity must aggregate as an id-less reasoning part"
    );

    let items = Vec::<InputItem>::try_from(crate::completion::Message::Assistant {
        id: None,
        content: choice,
    })
    .expect("history should convert");
    assert!(
        reasoning_input_items(&items).is_empty(),
        "an id-less reasoning part must not reach the request input"
    );

    // A wire-plausible id is provider-issued and must round-trip.
    let items = Vec::<InputItem>::try_from(crate::completion::Message::Assistant {
        id: None,
        content: OneOrMany::one(message::AssistantContent::Reasoning(message::Reasoning {
            id: Some("rs_0123".to_string()),
            content: vec![message::ReasoningContent::Text {
                text: "real item".to_string(),
                signature: None,
            }],
        })),
    })
    .expect("history should convert");
    let reasoning = reasoning_input_items(&items);
    assert_eq!(reasoning.len(), 1);
    assert_eq!(reasoning[0]["id"], "rs_0123");
}

/// F7 leak route (b), closed structurally: a same-provider delta-only
/// Responses stream whose reasoning deltas lack `item_id` keys
/// accumulation by a minted identity that never becomes a durable id, so
/// the next request carries no fabricated `output-{index}` item.
#[tokio::test]
async fn delta_only_stream_minted_output_ids_are_not_serialized_upstream() {
    use crate::test_utils::streaming_conformance::{fixtures, ok_chunks};
    use bytes::Bytes;

    let sse = |frame: &serde_json::Value| Bytes::from(format!("data: {frame}\n\n"));
    let frames = vec![
        // No `item_id`: the streaming adapter mints `output-0`.
        sse(&json!({
            "type": "response.reasoning_text.delta",
            "output_index": 0,
            "content_index": 0,
            "sequence_number": 1,
            "delta": "unattributed thought",
        })),
        sse(&json!({
            "type": "response.completed",
            "sequence_number": 2,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 0,
                "status": "completed",
                "model": "gpt-5.4",
                "output": [],
                "tools": [],
                "usage": null,
            },
        })),
    ];
    let drained = fixtures::openai_responses::driver()
        .drive(ok_chunks(frames))
        .await
        .expect("stream should complete");
    // The minted `output_index` identity keys accumulation only; the
    // aggregated part carries no durable id, so nothing can go upstream.
    assert!(
        drained.choice.iter().any(
            |content| matches!(content, message::AssistantContent::Reasoning(reasoning)
                    if reasoning.id.is_none())
        ),
        "an id-less delta stream must aggregate as an id-less reasoning part"
    );

    let items = Vec::<InputItem>::try_from(crate::completion::Message::Assistant {
        id: None,
        content: drained.choice.clone(),
    })
    .expect("history should convert");
    assert!(
        reasoning_input_items(&items).is_empty(),
        "an id-less reasoning part must not reach the request input"
    );
}

#[test]
fn tool_result_literal_text_and_structured_json_render_without_reparsing() {
    let cases = [
        (
            message::ToolResultContent::text(r#"{"status":"ok"}"#),
            r#"{"status":"ok"}"#.to_string(),
        ),
        (
            message::ToolResultContent::json(json!({ "status": "ok" })),
            r#"{"status":"ok"}"#.to_string(),
        ),
    ];

    for (content, expected) in cases {
        let input = rig_tool_result(content);

        let messages: Vec<Message> = input.clone().try_into().expect("message conversion");
        assert!(matches!(
            messages.as_slice(),
            [Message::ToolResult {
                output: ToolResultOutput::Text(output),
                ..
            }] if output == &expected
        ));

        let items: Vec<InputItem> = input.try_into().expect("input item conversion");
        assert!(matches!(
            items.as_slice(),
            [InputItem {
                input: InputContent::FunctionCallOutput(ToolResult {
                    output: ToolResultOutput::Text(output),
                    ..
                }),
                ..
            }] if output == &expected
        ));
    }
}

#[test]
fn multiple_text_tool_result_blocks_preserve_order_as_rich_function_output() {
    let content = OneOrMany::many(vec![
        message::ToolResultContent::text("first"),
        message::ToolResultContent::text("second"),
    ])
    .expect("multiple tool-result blocks should be non-empty");

    let input = message::Message::User {
        content: OneOrMany::one(message::UserContent::ToolResult(message::ToolResult {
            id: "result-id".to_string(),
            call_id: Some("call-id".to_string()),
            content,
        })),
    };

    let expected = ToolResultOutput::Content(vec![
        ToolResultOutputContent::InputText {
            text: "first".to_string(),
        },
        ToolResultOutputContent::InputText {
            text: "second".to_string(),
        },
    ]);

    let messages: Vec<Message> = input.clone().try_into().expect("message conversion");

    match messages.as_slice() {
        [Message::ToolResult { output, .. }] => {
            assert_eq!(output, &expected);
        }
        other => panic!("expected one tool result, got {other:?}"),
    }

    let items: Vec<InputItem> = input.try_into().expect("input item conversion");

    match items.as_slice() {
        [
            InputItem {
                input: InputContent::FunctionCallOutput(ToolResult { output, .. }),
                ..
            },
        ] => {
            assert_eq!(output, &expected);
        }
        other => panic!("expected one function-call output, got {other:?}"),
    }

    let wire = serde_json::to_value(&items[0]).expect("input item should serialize");

    assert_eq!(
        wire,
        json!({
            "type": "function_call_output",
            "call_id": "call-id",
            "output": [
                {
                    "type": "input_text",
                    "text": "first"
                },
                {
                    "type": "input_text",
                    "text": "second"
                }
            ],
            "status": "completed"
        })
    );
}

#[test]
fn multiple_text_and_json_tool_result_blocks_preserve_boundaries() {
    let content = OneOrMany::many(vec![
        message::ToolResultContent::text("before"),
        message::ToolResultContent::json(json!({
            "status": "ok"
        })),
        message::ToolResultContent::text("after"),
    ])
    .expect("multiple tool-result blocks should be non-empty");

    let output =
        responses_tool_result_output(content).expect("tool-result conversion should succeed");

    assert_eq!(
        output,
        ToolResultOutput::Content(vec![
            ToolResultOutputContent::InputText {
                text: "before".to_string(),
            },
            ToolResultOutputContent::InputText {
                text: r#"{"status":"ok"}"#.to_string(),
            },
            ToolResultOutputContent::InputText {
                text: "after".to_string(),
            },
        ])
    );
}

#[test]
fn tool_result_images_and_text_preserve_order_as_rich_function_output() {
    let content = OneOrMany::many(vec![
        message::ToolResultContent::text("before"),
        message::ToolResultContent::image_base64(
            "aW1hZ2U=",
            Some(message::ImageMediaType::PNG),
            None,
        ),
        message::ToolResultContent::json(json!({ "after": true })),
    ])
    .expect("mixed tool output is non-empty");
    let input = message::Message::User {
        content: OneOrMany::one(message::UserContent::ToolResult(message::ToolResult {
            id: "result-id".to_string(),
            call_id: Some("call-id".to_string()),
            content,
        })),
    };

    let assert_output = |output: &ToolResultOutput| {
        assert!(matches!(
            output,
            ToolResultOutput::Content(content)
                if matches!(content.as_slice(), [
                    ToolResultOutputContent::InputText { text: before },
                    ToolResultOutputContent::InputImage { image_url, .. },
                    ToolResultOutputContent::InputText { text: after },
                ] if before == "before"
                    && image_url.as_deref() == Some("data:image/png;base64,aW1hZ2U=")
                    && after == r#"{"after":true}"#)
        ));
    };

    let messages: Vec<Message> = input.clone().try_into().expect("message conversion");
    match messages.as_slice() {
        [Message::ToolResult { output, .. }] => assert_output(output),
        other => panic!("expected one rich tool result, got {other:?}"),
    }

    let items: Vec<InputItem> = input.try_into().expect("input item conversion");
    match items.as_slice() {
        [
            InputItem {
                input: InputContent::FunctionCallOutput(ToolResult { output, .. }),
                ..
            },
        ] => assert_output(output),
        other => panic!("expected one rich function output, got {other:?}"),
    }
}

#[test]
fn tool_result_file_id_image_uses_the_native_wire_field() {
    let input = rig_tool_result(message::ToolResultContent::Image(message::Image {
        data: message::DocumentSourceKind::FileId("file-image-123".to_string()),
        media_type: None,
        detail: None,
        additional_params: None,
    }));

    let items: Vec<InputItem> = input.try_into().expect("input item conversion");
    let wire = serde_json::to_value(&items[0]).expect("serialize input item");
    assert_eq!(
        wire,
        json!({
            "type": "function_call_output",
            "call_id": "call-id",
            "output": [{
                "type": "input_image",
                "file_id": "file-image-123",
                "detail": "auto"
            }],
            "status": "completed"
        })
    );
}

fn weather_tool_request() -> completion::CompletionRequest {
    completion::CompletionRequest {
        model: None,
        preamble: None,
        chat_history: crate::OneOrMany::one(message::Message::user("what's the weather?")),
        documents: Vec::new(),
        tools: vec![weather_tool_definition()],
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    }
}

#[test]
fn responses_tool_choice_modes_serialize_as_plain_strings() {
    for (choice, expected) in [
        (message::ToolChoice::Auto, json!("auto")),
        (message::ToolChoice::None, json!("none")),
        (message::ToolChoice::Required, json!("required")),
    ] {
        let converted = ToolChoice::try_from(choice).expect("mode should convert");
        assert_eq!(
            serde_json::to_value(&converted).expect("serialize tool choice"),
            expected
        );
    }
}

#[test]
fn responses_tool_choice_specific_single_name_serializes_as_named_function() {
    let converted = ToolChoice::try_from(message::ToolChoice::Specific {
        function_names: vec!["get_weather".to_string()],
    })
    .expect("single specific tool should convert");

    assert_eq!(
        serde_json::to_value(&converted).expect("serialize tool choice"),
        json!({"type": "function", "name": "get_weather"})
    );
}

#[test]
fn responses_tool_choice_specific_multiple_names_serialize_as_allowed_tools() {
    let converted = ToolChoice::try_from(message::ToolChoice::Specific {
        function_names: vec!["add".to_string(), "subtract".to_string()],
    })
    .expect("multiple specific tools should convert");

    assert_eq!(
        serde_json::to_value(&converted).expect("serialize tool choice"),
        json!({
            "type": "allowed_tools",
            "mode": "required",
            "tools": [
                {"type": "function", "name": "add"},
                {"type": "function", "name": "subtract"}
            ]
        })
    );
}

#[test]
fn responses_tool_choice_specific_empty_names_error() {
    let converted = ToolChoice::try_from(message::ToolChoice::Specific {
        function_names: vec![],
    });

    assert!(matches!(
        converted,
        Err(CompletionError::RequestError(error))
            if error.to_string().contains("at least one function name")
    ));
}

#[test]
fn responses_request_with_specific_tool_choice_serializes_named_function() {
    let mut request = weather_tool_request();
    request.tool_choice = Some(message::ToolChoice::Specific {
        function_names: vec!["get_weather".to_string()],
    });

    let request = CompletionRequest::try_from(("gpt-test".to_string(), request)).expect("convert");
    let request_json = serde_json::to_value(&request).expect("serialize request");

    assert_eq!(
        request_json.get("tool_choice"),
        Some(&json!({"type": "function", "name": "get_weather"}))
    );
}

#[test]
fn responses_function_tools_are_non_strict_by_default() {
    let tool = ResponsesToolDefinition::function(
        "get_weather",
        "Get the weather",
        weather_tool_definition().parameters,
    );

    assert!(!tool.strict);
    assert_eq!(tool.parameters["required"], json!(["location"]));
    assert!(tool.parameters.get("additionalProperties").is_none());

    let serialized = serde_json::to_value(tool).expect("tool should serialize");
    assert!(serialized.get("strict").is_none());
}

#[test]
fn responses_tool_definitions_accept_nullable_strict() {
    let cases = [
        (
            json!({
                "type": "function",
                "name": "get_weather",
                "parameters": {}
            }),
            false,
        ),
        (
            json!({
                "type": "function",
                "name": "get_weather",
                "parameters": {},
                "strict": null
            }),
            false,
        ),
        (
            json!({
                "type": "function",
                "name": "get_weather",
                "parameters": {},
                "strict": false
            }),
            false,
        ),
        (
            json!({
                "type": "function",
                "name": "get_weather",
                "parameters": {},
                "strict": true
            }),
            true,
        ),
    ];

    for (value, expected) in cases {
        let tool: ResponsesToolDefinition =
            serde_json::from_value(value).expect("tool definition should deserialize");
        assert_eq!(tool.strict, expected);
    }
}

#[test]
fn responses_strict_function_tools_sanitize_schema() {
    let tool = ResponsesToolDefinition::strict_function(
        "get_weather",
        "Get the weather",
        weather_tool_definition().parameters,
    );

    assert!(tool.strict);
    assert_eq!(tool.parameters["additionalProperties"], json!(false));
    assert_eq!(tool.parameters["required"], json!(["location", "unit"]));
}

fn request_with_preamble(preamble: &str) -> completion::CompletionRequest {
    completion::CompletionRequest {
        model: None,
        preamble: Some(preamble.to_string()),
        chat_history: crate::OneOrMany::one(message::Message::user("Hello")),
        documents: Vec::new(),
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    }
}

fn system_only_request(system_text: &str) -> completion::CompletionRequest {
    completion::CompletionRequest {
        model: None,
        preamble: None,
        chat_history: crate::OneOrMany::one(completion::Message::system(system_text)),
        documents: Vec::new(),
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    }
}

#[test]
fn responses_request_uses_top_level_instructions_for_preamble_by_default() {
    let req = CompletionRequest::try_from((
        "gpt-4o-mini".to_string(),
        request_with_preamble("You are concise."),
    ))
    .expect("request should convert");
    let serialized = serde_json::to_value(&req).expect("request should serialize");
    let input = serialized["input"]
        .as_array()
        .expect("input should be array");

    assert_eq!(serialized["instructions"], json!("You are concise."));
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["role"], "user");
}

#[test]
fn responses_request_drops_whitespace_only_preamble() {
    let req =
        CompletionRequest::try_from(("gpt-4o-mini".to_string(), request_with_preamble("  \n ")))
            .expect("request should convert");
    let serialized = serde_json::to_value(&req).expect("request should serialize");
    let input = serialized["input"]
        .as_array()
        .expect("input should be array");

    assert!(
        serialized.get("instructions").is_none(),
        "a whitespace-only preamble carries no content and is dropped"
    );
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["role"], "user");
}

#[test]
fn responses_request_lifts_system_messages_to_top_level_instructions_by_default() {
    let request = CompletionRequestBuilder::new(MockCompletionModel::default(), "Hello")
        .preamble("System one".to_string())
        .message(completion::Message::system("System two"))
        .build();

    let req = CompletionRequest::try_from(("gpt-4o-mini".to_string(), request))
        .expect("request should convert");
    let serialized = serde_json::to_value(&req).expect("request should serialize");
    let input = serialized["input"]
        .as_array()
        .expect("input should be array");

    assert_eq!(
        serialized["instructions"],
        json!("System one\n\nSystem two")
    );
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["role"], "user");
}

#[test]
fn responses_request_with_only_system_messages_keeps_them_in_input() {
    let req = CompletionRequest::try_from((
        "gpt-4o-mini".to_string(),
        system_only_request("System only"),
    ))
    .expect("request conversion should succeed");
    let serialized = serde_json::to_value(&req).expect("request should serialize");
    let input = serialized["input"]
        .as_array()
        .expect("input should be array");

    assert!(
        serialized.get("instructions").is_none(),
        "lifting a system-only history would leave input empty, so it stays in input"
    );
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["role"], "system");
    assert!(input[0].to_string().contains("System only"));
}

#[test]
fn responses_model_can_fallback_to_system_messages_in_input() {
    let client = crate::providers::openai::Client::new("dummy-key").expect("client");
    let model =
        ResponsesCompletionModel::new(client, "gpt-4o-mini").with_system_instructions_as_messages();

    let req = model
        .create_completion_request(request_with_preamble("You are concise."))
        .expect("request should convert");
    let serialized = serde_json::to_value(&req).expect("request should serialize");
    let input = serialized["input"]
        .as_array()
        .expect("input should be array");

    assert!(serialized.get("instructions").is_none());
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["role"], "system");
    assert!(input[0].to_string().contains("You are concise."));
    assert_eq!(input[1]["role"], "user");
}

#[test]
fn responses_client_can_fallback_to_system_messages_in_input() {
    use crate::prelude::CompletionClient;

    let client = crate::providers::openai::Client::new("dummy-key")
        .expect("client")
        .with_system_instructions_as_messages();
    let model = client.completion_model("gpt-4o-mini");

    let req = model
        .create_completion_request(request_with_preamble("You are concise."))
        .expect("request should convert");
    let serialized = serde_json::to_value(&req).expect("request should serialize");
    let input = serialized["input"]
        .as_array()
        .expect("input should be array");

    assert!(serialized.get("instructions").is_none());
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["role"], "system");
    assert!(input[0].to_string().contains("You are concise."));
    assert_eq!(input[1]["role"], "user");
}

#[test]
fn responses_model_can_lift_all_system_messages_via_placement() {
    let client = crate::providers::openai::Client::new("dummy-key").expect("client");
    let model = ResponsesCompletionModel::new(client, "gpt-4o-mini")
        .with_system_instructions_placement(SystemInstructionsPlacement::AllInstructions);

    let request = CompletionRequestBuilder::new(MockCompletionModel::default(), "again")
        .preamble("System one".to_string())
        .message(completion::Message::user("hi"))
        .message(completion::Message::system("Mid-conversation instruction"))
        .build();

    let req = model
        .create_completion_request(request)
        .expect("request should convert");
    let serialized = serde_json::to_value(&req).expect("request should serialize");
    let input = serialized["input"]
        .as_array()
        .expect("input should be array");

    assert_eq!(
        serialized["instructions"],
        json!("System one\n\nMid-conversation instruction")
    );
    assert!(
        input.iter().all(|item| item["role"] != "system"),
        "AllInstructions should leave no system items in input: {input:?}"
    );
}

#[test]
fn responses_client_placement_survives_completions_api_round_trip() {
    use crate::prelude::CompletionClient;

    let client = crate::providers::openai::Client::new("dummy-key")
        .expect("client")
        .with_system_instructions_placement(SystemInstructionsPlacement::InputSystemMessages)
        .completions_api()
        .responses_api();
    let model = client.completion_model("gpt-4o-mini");

    let req = model
        .create_completion_request(request_with_preamble("You are concise."))
        .expect("request should convert");
    let serialized = serde_json::to_value(&req).expect("request should serialize");

    assert!(
        serialized.get("instructions").is_none(),
        "placement configured before completions_api() should survive responses_api()"
    );
    assert_eq!(serialized["input"][0]["role"], "system");
}

/// A tool result whose correlation key matches no prior assistant
/// function call is an orphan: the conversion fails loudly, naming the id
/// and the history index, instead of forwarding a request the Responses
/// API would reject.
#[test]
fn orphan_tool_result_history_fails_request_conversion() {
    let request = crate::completion::CompletionRequest {
        model: None,
        preamble: None,
        chat_history: OneOrMany::many(vec![
            crate::completion::Message::user("Run the report."),
            rig_tool_result(message::ToolResultContent::text("output")),
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

    let err = CompletionRequest::try_from(ResponsesRequestParams {
        model: "gpt-4o-mini".to_string(),
        request,
        system_instructions_placement: SystemInstructionsPlacement::Instructions,
    })
    .expect_err("an orphan tool result must fail request conversion");

    assert!(
        err.to_string().contains(
            "tool result \"call-id\" has no matching tool call in the conversation history"
        ),
        "unexpected error: {err}"
    );
    assert!(
        err.to_string().contains("history index 1"),
        "the error must name the message index: {err}"
    );
}

#[test]
fn all_instructions_system_only_input_reports_non_system_requirement() {
    let err = CompletionRequest::try_from(ResponsesRequestParams {
        model: "gpt-4o-mini".to_string(),
        request: system_only_request("System only"),
        system_instructions_placement: SystemInstructionsPlacement::AllInstructions,
    })
    .expect_err("system-only input should fail once every item is lifted");

    assert!(
        err.to_string().contains("non-system item"),
        "error should explain that lifted system messages left input empty: {err}"
    );
}

#[test]
fn all_instructions_whitespace_only_system_input_reports_non_system_requirement() {
    let err = CompletionRequest::try_from(ResponsesRequestParams {
        model: "gpt-4o-mini".to_string(),
        request: system_only_request("   "),
        system_instructions_placement: SystemInstructionsPlacement::AllInstructions,
    })
    .expect_err("whitespace-only system input should fail once every item is lifted");

    assert!(
        err.to_string().contains("non-system item"),
        "even when lifted system text is whitespace-only (so no `instructions` field is \
             produced), the error should explain that system messages were lifted: {err}"
    );
}

#[test]
fn responses_request_conversion_keeps_tools_non_strict_by_default() {
    let req = CompletionRequest::try_from(("gpt-4o-mini".to_string(), weather_tool_request()))
        .expect("request should convert");

    let tool = &req.tools[0];
    assert!(!tool.strict);
    assert_eq!(tool.parameters["required"], json!(["location"]));
    assert!(tool.parameters.get("additionalProperties").is_none());
}

#[test]
fn responses_model_strict_tools_opt_in_sanitizes_all_function_tools() {
    let client = crate::providers::openai::Client::new("dummy-key").expect("client");
    let model = ResponsesCompletionModel::new(client, "gpt-4o-mini")
        .with_strict_tools()
        .with_tool(completion::ToolDefinition {
            name: "lookup".to_string(),
            description: "Look something up".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"q": {"type": "string"}}
            }),
        });

    let mut request = weather_tool_request();
    request.additional_params = Some(json!({
        "tools": [{
            "type": "function",
            "name": "extra",
            "description": "An additional_params tool",
            "parameters": {"type": "object", "properties": {"x": {"type": "string"}}}
        }]
    }));

    let req = model
        .create_completion_request(request)
        .expect("request should convert");

    assert_eq!(req.tools.len(), 3);
    for tool in &req.tools {
        assert!(tool.strict, "{} should be strict", tool.name);
        assert_eq!(tool.parameters["additionalProperties"], json!(false));
    }
}

#[test]
fn responses_model_default_preserves_all_function_tools_as_constructed() {
    let client = crate::providers::openai::Client::new("dummy-key").expect("client");
    let model =
        ResponsesCompletionModel::new(client, "gpt-4o-mini").with_tool(weather_tool_definition());

    let mut request = weather_tool_request();
    request.additional_params = Some(json!({
        "tools": [{
            "type": "function",
            "name": "extra",
            "description": "An additional_params tool",
            "parameters": {"type": "object", "properties": {"x": {"type": "string"}}}
        }]
    }));

    let req = model
        .create_completion_request(request)
        .expect("request should convert");

    assert_eq!(req.tools.len(), 3);
    for tool in &req.tools {
        assert!(!tool.strict, "{} should not be strict", tool.name);
        assert!(tool.parameters.get("additionalProperties").is_none());
    }
}

#[test]
fn responses_explicit_strict_tool_stays_strict_on_default_model() {
    let client = crate::providers::openai::Client::new("dummy-key").expect("client");
    let model = ResponsesCompletionModel::new(client, "gpt-4o-mini").with_tool(
        ResponsesToolDefinition::strict_function(
            "lookup",
            "Look something up",
            json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        ),
    );

    let req = model
        .create_completion_request(weather_tool_request())
        .expect("request should convert");

    assert!(!req.tools[0].strict);
    assert!(req.tools[1].strict);
    assert_eq!(
        req.tools[1].parameters["additionalProperties"],
        json!(false)
    );
}

fn response_with_service_tier(service_tier: &str) -> Value {
    json!({
        "id": "resp_123",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "gpt-5.4",
        "output": [],
        "service_tier": service_tier,
    })
}

#[test]
fn completion_response_deserializes_standard_service_tier() {
    let response: CompletionResponse =
        serde_json::from_value(response_with_service_tier("standard"))
            .expect("response should deserialize");

    assert!(matches!(
        response.additional_parameters.service_tier,
        Some(OpenAIServiceTier::Standard)
    ));
}

#[test]
fn completion_response_deserializes_priority_service_tier() {
    let response: CompletionResponse =
        serde_json::from_value(response_with_service_tier("priority"))
            .expect("response should deserialize");

    assert!(matches!(
        response.additional_parameters.service_tier,
        Some(OpenAIServiceTier::Priority)
    ));
}

#[test]
fn completion_response_preserves_unknown_service_tier() {
    let response: CompletionResponse =
        serde_json::from_value(response_with_service_tier("provider_experimental"))
            .expect("response should deserialize");

    let Some(OpenAIServiceTier::Other(service_tier)) = response.additional_parameters.service_tier
    else {
        panic!("expected provider-specific service tier");
    };

    assert_eq!(service_tier, "provider_experimental");
}

#[test]
fn responses_request_keeps_documents_after_lifted_system_messages() {
    let request = CompletionRequestBuilder::new(MockCompletionModel::default(), "Prompt")
        .message(completion::Message::system("System prompt"))
        .message(completion::Message::user("Earlier user turn"))
        .message(completion::Message::assistant("Earlier assistant turn"))
        .document(test_document("doc1", "Document text."))
        .build();

    let responses_request = CompletionRequest::try_from(("gpt-4o-mini".to_string(), request))
        .expect("request conversion should succeed");

    let serialized = serde_json::to_value(&responses_request).expect("request should serialize");
    let input = serialized["input"]
        .as_array()
        .expect("input should be an array");

    assert_eq!(serialized["instructions"], json!("System prompt"));
    assert_eq!(input.len(), 4);
    assert_eq!(input[0]["role"], "user");
    assert!(
        input[0].to_string().contains("<file id: doc1>"),
        "document input should be first after system instructions are lifted: {input:?}"
    );
    assert_eq!(input[1]["role"], "user");
    assert!(
        input[1].to_string().contains("Earlier user turn"),
        "prior user history should follow document input: {input:?}"
    );
    assert_eq!(input[2]["role"], "assistant");
    assert!(
        input[2].to_string().contains("Earlier assistant turn"),
        "prior assistant history should follow prior user history: {input:?}"
    );
    assert_eq!(input[3]["role"], "user");
    assert!(
        input[3].to_string().contains("Prompt"),
        "prompt should remain last: {input:?}"
    );
}

#[test]
fn responses_direct_request_keeps_mid_conversation_system_messages_in_input() {
    let request = crate::completion::CompletionRequest {
        model: None,
        preamble: None,
        chat_history: crate::OneOrMany::many(vec![
            completion::Message::system("System prompt"),
            completion::Message::assistant("Earlier assistant turn"),
            completion::Message::system("Mid-conversation instruction"),
            completion::Message::user("Prompt"),
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

    let responses_request = CompletionRequest::try_from(("gpt-4o-mini".to_string(), request))
        .expect("request conversion should succeed");

    let serialized = serde_json::to_value(&responses_request).expect("request should serialize");
    let input = serialized["input"]
        .as_array()
        .expect("input should be an array");

    assert_eq!(
        serialized["instructions"],
        json!("System prompt"),
        "only the leading run of system messages should be lifted"
    );
    assert_eq!(input.len(), 4);
    assert_eq!(input[0]["role"], "user");
    assert!(
        input[0].to_string().contains("<file id: doc1>"),
        "document input should follow lifted system instructions: {input:?}"
    );
    assert_eq!(input[1]["role"], "assistant");
    assert_eq!(input[2]["role"], "system");
    assert!(
        input[2]
            .to_string()
            .contains("Mid-conversation instruction"),
        "mid-conversation system messages should keep their position: {input:?}"
    );
    assert_eq!(input[3]["role"], "user");
    assert_eq!(
        input
            .iter()
            .filter(|message| message.to_string().contains("<file id: doc1>"))
            .count(),
        1,
        "document input should appear exactly once: {input:?}"
    );
}

#[test]
fn service_tier_serializes_expected_strings() {
    let cases = [
        (OpenAIServiceTier::Auto, "auto"),
        (OpenAIServiceTier::Default, "default"),
        (OpenAIServiceTier::Flex, "flex"),
        (OpenAIServiceTier::Priority, "priority"),
        (OpenAIServiceTier::Standard, "standard"),
    ];

    for (service_tier, expected) in cases {
        assert_eq!(
            serde_json::to_value(service_tier).expect("service tier should serialize"),
            json!(expected)
        );
    }

    assert_eq!(
        serde_json::to_value(OpenAIServiceTier::Other(
            "provider_experimental".to_string()
        ))
        .expect("provider-specific service tier should serialize"),
        json!("provider_experimental")
    );
}

#[test]
fn responses_usage_token_usage_preserves_reasoning_tokens() {
    let usage = ResponsesUsage {
        input_tokens: 100,
        input_tokens_details: Some(InputTokensDetails { cached_tokens: 25 }),
        output_tokens: 50,
        output_tokens_details: Some(OutputTokensDetails {
            reasoning_tokens: 15,
        }),
        total_tokens: 150,
    };

    let token_usage = crate::completion::Usage::from(&usage);

    assert_eq!(token_usage.input_tokens, 100);
    assert_eq!(token_usage.cached_input_tokens, 25);
    assert_eq!(token_usage.output_tokens, 50);
    assert_eq!(token_usage.reasoning_tokens, 15);
    assert_eq!(token_usage.total_tokens, 150);
}

#[test]
fn responses_usage_deserializes_without_output_token_details() {
    let usage: ResponsesUsage = serde_json::from_value(json!({
        "input_tokens": 100,
        "input_tokens_details": {
            "cached_tokens": 25
        },
        "output_tokens": 50,
        "total_tokens": 150
    }))
    .expect("usage should deserialize when output token details are omitted");

    assert!(usage.output_tokens_details.is_none());

    let token_usage = crate::completion::Usage::from(&usage);

    assert_eq!(token_usage.input_tokens, 100);
    assert_eq!(token_usage.cached_input_tokens, 25);
    assert_eq!(token_usage.output_tokens, 50);
    assert_eq!(token_usage.reasoning_tokens, 0);
    assert_eq!(token_usage.total_tokens, 150);
}

#[test]
fn completion_response_accepts_top_level_reasoning_string() {
    let response: CompletionResponse = serde_json::from_value(json!({
        "id": "resp_123",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "Qwen/Qwen3-4B",
        "reasoning": "thinking through the answer",
        "usage": {
            "input_tokens": 1,
            "output_tokens": 2,
            "total_tokens": 3
        },
        "output": [{
            "type": "message",
            "id": "msg_123",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "annotations": [],
                "text": "done"
            }]
        }],
        "tools": []
    }))
    .expect("mistral.rs-style reasoning string should deserialize");

    assert_eq!(
        response.provider_reasoning.as_deref(),
        Some("thinking through the answer")
    );
    assert_eq!(response.reasoning_metadata, None);
    assert_eq!(response.reasoning_context, None);
    assert_eq!(
        serde_json::to_value(&response).expect("response should serialize")["reasoning"],
        json!("thinking through the answer")
    );

    let completion: completion::CompletionResponse = response
        .normalize("openai")
        .expect("response should convert");
    let items = completion.choice.iter().collect::<Vec<_>>();
    assert!(matches!(
        items[0],
        completion::AssistantContent::Reasoning(_)
    ));
    assert!(matches!(items[1], completion::AssistantContent::Text(_)));
}

#[test]
fn completion_response_accepts_null_metadata() {
    let response: CompletionResponse = serde_json::from_value(json!({
        "id": "resp_123",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "openai-compatible-model",
        "metadata": null,
        "output": [{
            "type": "message",
            "id": "msg_123",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "annotations": [],
                "text": "done"
            }]
        }],
        "tools": []
    }))
    .expect("response with null metadata should deserialize");

    assert!(response.additional_parameters.metadata.is_empty());
}

#[test]
fn completion_response_accepts_reasoning_only_response() {
    let response: CompletionResponse = serde_json::from_value(json!({
        "id": "resp_123",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "Qwen/Qwen3-4B",
        "reasoning": "thinking only",
        "usage": {
            "input_tokens": 1,
            "output_tokens": 2,
            "total_tokens": 3
        },
        "output": [],
        "tools": []
    }))
    .expect("reasoning-only response should deserialize");

    let completion: completion::CompletionResponse = response
        .normalize("openai")
        .expect("reasoning-only response should convert");
    let items = completion.choice.iter().collect::<Vec<_>>();

    assert_eq!(items.len(), 1);
    assert!(matches!(
        items[0],
        completion::AssistantContent::Reasoning(_)
    ));
}

#[test]
fn completion_response_rejects_empty_response_without_reasoning() {
    let response: CompletionResponse = serde_json::from_value(json!({
        "id": "resp_123",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "Qwen/Qwen3-4B",
        "output": [],
        "tools": []
    }))
    .expect("empty response shape should deserialize");

    let err = response
        .normalize("openai")
        .expect_err("empty response without reasoning should be rejected");

    assert!(
        err.to_string()
            .contains("Response contained no message or tool call")
    );
}

fn incomplete_because(reason: &str) -> IncompleteDetailsReason {
    IncompleteDetailsReason {
        reason: reason.to_string(),
    }
}

#[test]
fn finish_reason_maps_every_documented_terminal_state() {
    assert_eq!(
        map_finish_reason(&ResponseStatus::Completed, None),
        Some(completion::FinishReason::Stop)
    );
    assert_eq!(
        map_finish_reason(
            &ResponseStatus::Incomplete,
            Some(&incomplete_because("max_output_tokens"))
        ),
        Some(completion::FinishReason::Length)
    );
    assert_eq!(
        map_finish_reason(
            &ResponseStatus::Incomplete,
            Some(&incomplete_because("content_filter"))
        ),
        Some(completion::FinishReason::ContentFilter)
    );
    // `incomplete_details` on a completed turn is not a termination reason.
    assert_eq!(
        map_finish_reason(
            &ResponseStatus::Completed,
            Some(&incomplete_because("noise"))
        ),
        Some(completion::FinishReason::Stop)
    );
    // In-flight statuses are not terminations at all.
    assert_eq!(map_finish_reason(&ResponseStatus::InProgress, None), None);
    assert_eq!(map_finish_reason(&ResponseStatus::Queued, None), None);
}

#[test]
fn finish_reason_preserves_unknown_values_verbatim() {
    // A reason OpenAI adds later must survive in OpenAI's own spelling
    // rather than being smoothed into a natural stop.
    assert_eq!(
        map_finish_reason(
            &ResponseStatus::Incomplete,
            Some(&incomplete_because("MAX_TOOL_CALLS"))
        ),
        Some(completion::FinishReason::Other(
            "MAX_TOOL_CALLS".to_string()
        ))
    );
    // So must a terminal status with no normalized counterpart, and an
    // `incomplete` that states no reason.
    assert_eq!(
        map_finish_reason(&ResponseStatus::Failed, None),
        Some(completion::FinishReason::Other("failed".to_string()))
    );
    assert_eq!(
        map_finish_reason(&ResponseStatus::Cancelled, None),
        Some(completion::FinishReason::Other("cancelled".to_string()))
    );
    assert_eq!(
        map_finish_reason(&ResponseStatus::Incomplete, None),
        Some(completion::FinishReason::Other("incomplete".to_string()))
    );
    assert_eq!(
        map_finish_reason(&ResponseStatus::Incomplete, Some(&incomplete_because(""))),
        Some(completion::FinishReason::Other("incomplete".to_string()))
    );
}

#[test]
fn completion_response_carries_the_message_id_not_the_response_id() {
    let response: CompletionResponse = serde_json::from_value(json!({
        "id": "resp_123",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "gpt-5.4",
        "output": [{
            "type": "message",
            "id": "msg_456",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "annotations": [],
                "text": "done"
            }]
        }],
        "tools": []
    }))
    .expect("response should deserialize");

    let completion: completion::CompletionResponse = response
        .normalize("openai")
        .expect("response should convert");

    // The two IDs are distinct in this API: `resp_...` names the response,
    // `msg_...` names the assistant message.
    assert_eq!(completion.message_id.as_deref(), Some("msg_456"));
    assert_eq!(completion.provider, "openai");
    assert_eq!(completion.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(
        completion.finish_reason(),
        Some(completion::FinishReason::Stop)
    );
}

#[test]
fn completion_response_provider_name_is_an_input() {
    let response: CompletionResponse = serde_json::from_value(json!({
        "id": "resp_123",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "gpt-5.3-codex",
        "output": [{
            "type": "message",
            "id": "msg_456",
            "status": "completed",
            "role": "assistant",
            "content": [{ "type": "output_text", "annotations": [], "text": "done" }]
        }],
        "tools": []
    }))
    .expect("response should deserialize");

    let completion: completion::CompletionResponse = response
        .normalize("chatgpt")
        .expect("response should convert");

    assert_eq!(completion.provider, "chatgpt");
}

#[test]
fn completion_response_completed_with_tool_call_reports_tool_calls() {
    let response: CompletionResponse = serde_json::from_value(json!({
        "id": "resp_123",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "gpt-5.4",
        "output": [{
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "get_weather",
            "arguments": "{\"city\":\"London\"}",
            "status": "completed"
        }],
        "tools": []
    }))
    .expect("response should deserialize");

    let completion: completion::CompletionResponse = response
        .normalize("openai")
        .expect("response should convert");

    // `completed` is reconciled up to `ToolCalls` because the turn carried
    // a function call.
    assert_eq!(
        completion.finish_reason(),
        Some(completion::FinishReason::ToolCalls)
    );
}

#[test]
fn completion_response_incomplete_reports_the_truncation_reason() {
    let response: CompletionResponse = serde_json::from_value(json!({
        "id": "resp_123",
        "object": "response",
        "created_at": 0,
        "status": "incomplete",
        "incomplete_details": { "reason": "max_output_tokens" },
        "model": "gpt-5.4",
        "output": [{
            "type": "message",
            "id": "msg_456",
            "status": "incomplete",
            "role": "assistant",
            "content": [{ "type": "output_text", "annotations": [], "text": "half an ans" }]
        }],
        "tools": []
    }))
    .expect("response should deserialize");

    let completion: completion::CompletionResponse = response
        .normalize("openai")
        .expect("response should convert");

    assert_eq!(
        completion.finish_reason(),
        Some(completion::FinishReason::Length)
    );
}

#[test]
fn completion_response_preserves_context_without_treating_config_as_text() {
    let response: CompletionResponse = serde_json::from_value(json!({
        "id": "resp_123",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "Qwen/Qwen3-4B",
        "reasoning": {
            "context": "all_turns",
            "effort": "high",
            "mode": "standard",
            "summary": null
        },
        "output": [{
            "type": "message",
            "id": "msg_123",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "annotations": [],
                "text": "done"
            }]
        }],
        "tools": []
    }))
    .expect("object-shaped reasoning should be tolerated");

    assert!(response.provider_reasoning.is_none());
    assert_eq!(response.reasoning_context.as_deref(), Some("all_turns"));
    assert_eq!(
        response.reasoning_metadata.as_ref(),
        json!({
            "context": "all_turns",
            "effort": "high",
            "mode": "standard",
            "summary": null
        })
        .as_object()
    );
    assert_eq!(
        serde_json::to_value(&response).expect("response should serialize")["reasoning"],
        json!({
            "context": "all_turns",
            "effort": "high",
            "mode": "standard",
            "summary": null
        })
    );

    let completion: completion::CompletionResponse = response
        .normalize("openai")
        .expect("response should convert");
    let items = completion.choice.iter().collect::<Vec<_>>();
    assert_eq!(items.len(), 1);
    assert!(matches!(items[0], completion::AssistantContent::Text(_)));
}

#[test]
fn completion_response_preserves_unknown_reasoning_metadata_and_nulls() {
    let metadata = json!({
        "context": "future_context",
        "effort": "ultra",
        "summary": null,
        "future_control": { "depth": 3 }
    });
    let response: CompletionResponse = serde_json::from_value(json!({
        "id": "resp_123",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "gpt-future",
        "reasoning": metadata.clone(),
        "output": [],
        "tools": []
    }))
    .expect("unknown reasoning metadata should deserialize");

    assert_eq!(
        response.reasoning_context.as_deref(),
        Some("future_context")
    );
    assert_eq!(response.reasoning_metadata.as_ref(), metadata.as_object());
    assert_eq!(
        serde_json::to_value(&response).expect("response should serialize")["reasoning"],
        metadata
    );
}

#[test]
fn completion_response_ignores_unsupported_reasoning_shapes() {
    for reasoning in [Value::Null, json!(["unexpected"]), json!(42), json!(true)] {
        let response: CompletionResponse = serde_json::from_value(json!({
            "id": "resp_123",
            "object": "response",
            "created_at": 0,
            "status": "completed",
            "model": "openai-compatible-model",
            "reasoning": reasoning,
            "output": [],
            "tools": []
        }))
        .expect("unsupported reasoning shapes should remain non-fatal");

        assert_eq!(response.provider_reasoning, None);
        assert_eq!(response.reasoning_metadata, None);
        assert_eq!(response.reasoning_context, None);
        let serialized = serde_json::to_value(&response).expect("response should serialize");
        assert!(
            !serialized
                .as_object()
                .expect("response should serialize as an object")
                .contains_key("reasoning")
        );
    }
}

#[test]
fn completion_response_reasoning_serialization_precedence_is_stable() {
    let mut response: CompletionResponse = serde_json::from_value(json!({
        "id": "resp_123",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "gpt-5.6",
        "reasoning": { "context": "all_turns", "effort": "max" },
        "output": [],
        "tools": []
    }))
    .expect("reasoning metadata should deserialize");

    response.reasoning_context = Some("current_turn".to_owned());
    response.additional_parameters.reasoning =
        Some(Reasoning::new().with_effort(ReasoningEffort::Low));
    let serialized = serde_json::to_string(&response).expect("response should serialize");
    assert_eq!(serialized.matches("\"reasoning\":").count(), 1);
    assert_eq!(
        serde_json::to_value(&response).expect("response should serialize")["reasoning"],
        json!({ "context": "all_turns", "effort": "max" })
    );

    let metadata = response.reasoning_metadata.take();
    assert_eq!(
        serde_json::to_value(&response).expect("response should serialize")["reasoning"],
        json!({ "context": "current_turn" })
    );

    response.reasoning_metadata = metadata;
    response.provider_reasoning = Some("compatible-provider text".to_owned());
    assert_eq!(
        serde_json::to_value(&response).expect("response should serialize")["reasoning"],
        json!("compatible-provider text")
    );
}

fn request_with_reasoning_params(reasoning: Value) -> CompletionRequest {
    let mut request = request_with_preamble("You are concise.");
    request.additional_params = Some(json!({ "reasoning": reasoning }));

    CompletionRequest::try_from(("gpt-5.6".to_string(), request))
        .expect("request with reasoning params should convert")
}

#[test]
fn reasoning_effort_max_survives_request_conversion() {
    let request = request_with_reasoning_params(json!({ "effort": "max" }));
    let serialized = serde_json::to_value(&request).expect("request should serialize");

    assert_eq!(serialized["reasoning"], json!({ "effort": "max" }));
}

#[test]
fn reasoning_mode_pro_composes_with_independent_effort() {
    let request = request_with_reasoning_params(json!({ "effort": "high", "mode": "pro" }));
    let serialized = serde_json::to_value(&request).expect("request should serialize");

    assert_eq!(
        serialized["reasoning"],
        json!({ "effort": "high", "mode": "pro" })
    );
}

#[test]
fn reasoning_context_values_survive_request_conversion() {
    for (context, wire_value) in [
        (ReasoningContext::Auto, "auto"),
        (ReasoningContext::AllTurns, "all_turns"),
        (ReasoningContext::CurrentTurn, "current_turn"),
    ] {
        let typed = serde_json::to_value(Reasoning::new().with_context(context))
            .expect("typed reasoning should serialize");
        assert_eq!(typed, json!({ "context": wire_value }));

        let request = request_with_reasoning_params(json!({ "context": wire_value }));
        let serialized = serde_json::to_value(&request).expect("request should serialize");
        assert_eq!(serialized["reasoning"], json!({ "context": wire_value }));
    }
}

#[test]
fn reasoning_omits_unset_optional_fields() {
    let reasoning = serde_json::to_value(Reasoning::new().with_mode(ReasoningMode::Pro))
        .expect("reasoning should serialize");

    assert_eq!(reasoning, json!({ "mode": "pro" }));

    let reasoning = serde_json::to_value(
        Reasoning::new()
            .with_effort(ReasoningEffort::Max)
            .with_mode(ReasoningMode::Pro)
            .with_context(ReasoningContext::CurrentTurn)
            .with_summary_level(ReasoningSummaryLevel::Detailed),
    )
    .expect("reasoning should serialize");

    assert_eq!(
        reasoning,
        json!({
            "effort": "max",
            "mode": "pro",
            "context": "current_turn",
            "summary": "detailed"
        })
    );
}

#[test]
fn completion_response_does_not_duplicate_structured_reasoning() {
    let response: CompletionResponse = serde_json::from_value(json!({
        "id": "resp_123",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "gpt-5.4",
        "reasoning": "provider top-level text",
        "output": [{
            "type": "reasoning",
            "id": "rs_123",
            "summary": [{
                "type": "summary_text",
                "text": "structured summary"
            }]
        }, {
            "type": "message",
            "id": "msg_123",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "annotations": [],
                "text": "done"
            }]
        }],
        "tools": []
    }))
    .expect("response should deserialize");

    let completion: completion::CompletionResponse = response
        .normalize("openai")
        .expect("response should convert");
    let reasoning_count = completion
        .choice
        .iter()
        .filter(|item| matches!(item, completion::AssistantContent::Reasoning(_)))
        .count();

    assert_eq!(reasoning_count, 1);
}

#[test]
fn idless_reasoning_is_skipped_when_converting_responses_history() {
    let assistant = message::Message::Assistant {
        id: Some("msg_123".to_string()),
        content: OneOrMany::one(message::AssistantContent::Reasoning(
            message::Reasoning::new("provider reasoning"),
        )),
    };

    let converted =
        Vec::<Message>::try_from(assistant).expect("idless reasoning should degrade gracefully");

    assert!(converted.is_empty());
}

#[test]
fn idless_reasoning_only_is_skipped_without_empty_input_item() {
    let assistant = completion::Message::Assistant {
        id: None,
        content: OneOrMany::one(message::AssistantContent::Reasoning(
            message::Reasoning::new("provider reasoning"),
        )),
    };

    let converted =
        Vec::<InputItem>::try_from(assistant).expect("idless reasoning should degrade gracefully");

    assert!(converted.is_empty());
}

#[test]
fn idless_reasoning_plus_text_preserves_text_for_responses_history() {
    let assistant = message::Message::Assistant {
        id: Some("msg_123".to_string()),
        content: OneOrMany::many(vec![
            message::AssistantContent::Reasoning(message::Reasoning::new("provider reasoning")),
            message::AssistantContent::Text(Text::new("final answer")),
        ])
        .expect("assistant content should be non-empty"),
    };

    let converted = Vec::<Message>::try_from(assistant).expect("assistant history should convert");

    assert_eq!(converted.len(), 1);
    let Message::Assistant { content, .. } = &converted[0] else {
        panic!("expected assistant message");
    };
    assert!(matches!(
        content.first_ref(),
        AssistantContentType::Text(AssistantContent::OutputText(Text { text, .. })) if text == "final answer"
    ));
}

#[test]
fn completion_history_idless_reasoning_plus_text_preserves_text_input_item() {
    let assistant = completion::Message::Assistant {
        id: Some("msg_123".to_string()),
        content: OneOrMany::many(vec![
            message::AssistantContent::Reasoning(message::Reasoning::new("provider reasoning")),
            message::AssistantContent::Text(Text::new("final answer")),
        ])
        .expect("assistant content should be non-empty"),
    };

    let converted =
        Vec::<InputItem>::try_from(assistant).expect("assistant history should convert");

    assert_eq!(converted.len(), 1);
    assert!(matches!(converted[0].role, Some(Role::Assistant)));
    let InputContent::Message(Message::Assistant { content, .. }) = &converted[0].input else {
        panic!("expected assistant message input item");
    };
    assert!(matches!(
        content.first_ref(),
        AssistantContentType::Text(AssistantContent::OutputText(Text { text, .. })) if text == "final answer"
    ));
}

#[test]
fn assistant_text_without_idless_reasoning_replays_as_output_text() {
    let assistant = completion::Message::Assistant {
        id: Some("msg_123".to_string()),
        content: OneOrMany::one(message::AssistantContent::Text(Text::new("final answer"))),
    };

    let converted =
        Vec::<InputItem>::try_from(assistant).expect("assistant history should convert");

    assert_eq!(converted.len(), 1);
    let InputContent::Message(Message::Assistant { content, .. }) = &converted[0].input else {
        panic!("expected assistant message input item");
    };
    assert!(matches!(
        content.first_ref(),
        AssistantContentType::Text(AssistantContent::OutputText(Text { text, .. })) if text == "final answer"
    ));
}

#[test]
fn idless_completion_assistant_text_replays_as_easy_input_message() {
    let assistant = completion::Message::Assistant {
        id: None,
        content: OneOrMany::one(message::AssistantContent::Text(Text::new("final answer"))),
    };

    let converted =
        Vec::<InputItem>::try_from(assistant).expect("assistant history should convert");

    assert_eq!(converted.len(), 1);
    assert!(matches!(converted[0].role, Some(Role::Assistant)));
    let InputContent::Message(Message::AssistantInput { content, .. }) = &converted[0].input else {
        panic!("expected assistant input message item");
    };
    assert_eq!(content, "final answer");

    let serialized =
        serde_json::to_value(&converted[0]).expect("input item should serialize to JSON");
    assert_eq!(serialized["type"], json!("message"));
    assert_eq!(serialized["role"], json!("assistant"));
    assert_eq!(serialized["content"], json!("final answer"));
    assert!(serialized.get("id").is_none());
    assert!(serialized.get("status").is_none());
}

#[test]
fn idless_message_assistant_text_replays_as_easy_input_message() {
    let assistant = message::Message::Assistant {
        id: None,
        content: OneOrMany::one(message::AssistantContent::Text(Text::new("final answer"))),
    };

    let converted = Vec::<Message>::try_from(assistant).expect("assistant history should convert");

    assert_eq!(converted.len(), 1);
    let Message::AssistantInput { content, .. } = &converted[0] else {
        panic!("expected assistant input message");
    };
    assert_eq!(content, "final answer");

    let serialized =
        serde_json::to_value(&converted[0]).expect("assistant message should serialize to JSON");
    assert_eq!(serialized["role"], json!("assistant"));
    assert_eq!(serialized["content"], json!("final answer"));
    assert!(serialized.get("id").is_none());
    assert!(serialized.get("status").is_none());
}

#[test]
fn structured_reasoning_with_id_still_converts_for_responses_history() {
    let assistant = message::Message::Assistant {
        id: Some("msg_123".to_string()),
        content: OneOrMany::one(message::AssistantContent::Reasoning(message::Reasoning {
            id: Some("rs_123".to_string()),
            content: vec![message::ReasoningContent::Summary(
                "structured summary".to_string(),
            )],
        })),
    };

    let converted =
        Vec::<Message>::try_from(assistant).expect("structured reasoning should still convert");

    assert_eq!(converted.len(), 1);
    let Message::Assistant { content, .. } = &converted[0] else {
        panic!("expected assistant message");
    };
    assert!(matches!(
        content.first_ref(),
        AssistantContentType::Reasoning(OpenAIReasoning { id, .. }) if id == "rs_123"
    ));
}

#[test]
fn structured_reasoning_with_id_still_converts_to_input_item() {
    let assistant = completion::Message::Assistant {
        id: Some("msg_123".to_string()),
        content: OneOrMany::one(message::AssistantContent::Reasoning(message::Reasoning {
            id: Some("rs_123".to_string()),
            content: vec![message::ReasoningContent::Summary(
                "structured summary".to_string(),
            )],
        })),
    };

    let converted =
        Vec::<InputItem>::try_from(assistant).expect("structured reasoning should convert");

    assert_eq!(converted.len(), 1);
    assert!(converted[0].role.is_none());
    assert!(matches!(
        &converted[0].input,
        InputContent::Reasoning(OpenAIReasoning { id, .. }) if id == "rs_123"
    ));
}

#[test]
fn assistant_reasoning_text_tool_call_convert_in_responses_replay_order() {
    let assistant = completion::Message::Assistant {
        id: Some("msg_123".to_string()),
        content: OneOrMany::many(vec![
            message::AssistantContent::Reasoning(message::Reasoning {
                id: Some("rs_123".to_string()),
                content: vec![message::ReasoningContent::Summary(
                    "structured summary".to_string(),
                )],
            }),
            message::AssistantContent::Text(Text::new("final answer")),
            message::AssistantContent::tool_call_with_call_id(
                "fc_123",
                "call_123".to_string(),
                "lookup",
                json!({"query": "rig"}),
            ),
        ])
        .expect("assistant content should be non-empty"),
    };

    let converted =
        Vec::<InputItem>::try_from(assistant).expect("assistant history should convert");

    assert_eq!(converted.len(), 3);
    assert!(converted[0].role.is_none());
    assert!(matches!(
        &converted[0].input,
        InputContent::Reasoning(OpenAIReasoning { id, .. }) if id == "rs_123"
    ));

    assert!(matches!(converted[1].role, Some(Role::Assistant)));
    let InputContent::Message(Message::Assistant { content, id, .. }) = &converted[1].input else {
        panic!("expected assistant output message");
    };
    assert_eq!(id, "msg_123");
    assert!(matches!(
        content.first_ref(),
        AssistantContentType::Text(AssistantContent::OutputText(Text { text, .. }))
            if text == "final answer"
    ));

    assert!(converted[2].role.is_none());
    let InputContent::FunctionCall(OutputFunctionCall {
        id, call_id, name, ..
    }) = &converted[2].input
    else {
        panic!("expected function call input item");
    };
    assert_eq!(id, "fc_123");
    assert_eq!(call_id, "call_123");
    assert_eq!(name, "lookup");
}

#[test]
fn mocked_second_turn_request_omits_unreplayable_reasoning() {
    let request = crate::completion::CompletionRequest {
        model: None,
        preamble: Some("You are concise.".to_string()),
        chat_history: OneOrMany::many(vec![
            completion::Message::User {
                content: OneOrMany::one(message::UserContent::Text(Text::new(
                    "Think briefly, then answer.",
                ))),
            },
            completion::Message::Assistant {
                id: Some("msg_123".to_string()),
                content: OneOrMany::many(vec![
                    message::AssistantContent::Reasoning(message::Reasoning::new(
                        "provider reasoning",
                    )),
                    message::AssistantContent::Text(Text::new("final answer")),
                ])
                .expect("assistant content should be non-empty"),
            },
            completion::Message::Assistant {
                id: None,
                content: OneOrMany::many(vec![
                    message::AssistantContent::Reasoning(message::Reasoning::new(
                        "provider reasoning only",
                    )),
                    message::AssistantContent::Text(Text::new("")),
                ])
                .expect("assistant content should be non-empty"),
            },
            completion::Message::User {
                content: OneOrMany::one(message::UserContent::Text(Text::new(
                    "/no_think Reply with exactly: OK",
                ))),
            },
        ])
        .expect("history should be non-empty"),
        documents: Vec::new(),
        tools: Vec::new(),
        temperature: None,
        max_tokens: Some(64),
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    };

    let request = CompletionRequest::try_from(("Qwen/Qwen3-4B".to_string(), request))
        .expect("request should convert");
    let value = serde_json::to_value(&request).expect("request should serialize");
    let input = value["input"]
        .as_array()
        .expect("mocked multi-turn request should serialize input as an array");

    assert!(
        !input.iter().any(|item| {
            item.get("type") == Some(&json!("reasoning")) && item.get("id").is_none()
        })
    );
    assert!(!input.iter().any(|item| {
        item.get("role") == Some(&json!("assistant"))
            && item
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
    }));

    let assistant_items = input
        .iter()
        .filter(|item| item.get("role") == Some(&json!("assistant")))
        .collect::<Vec<_>>();

    assert_eq!(assistant_items.len(), 1);
    assert_eq!(assistant_items[0]["content"][0]["type"], "output_text");
    assert_eq!(assistant_items[0]["content"][0]["text"], "final answer");
}

#[test]
fn responses_usage_add_preserves_rhs_details_when_lhs_details_are_absent() {
    let lhs = ResponsesUsage {
        input_tokens: 10,
        input_tokens_details: None,
        output_tokens: 20,
        output_tokens_details: None,
        total_tokens: 30,
    };
    let rhs = ResponsesUsage {
        input_tokens: 3,
        input_tokens_details: Some(InputTokensDetails { cached_tokens: 2 }),
        output_tokens: 5,
        output_tokens_details: Some(OutputTokensDetails {
            reasoning_tokens: 4,
        }),
        total_tokens: 8,
    };

    let usage = lhs + rhs;
    let token_usage = crate::completion::Usage::from(&usage);

    assert_eq!(token_usage.input_tokens, 13);
    assert_eq!(token_usage.cached_input_tokens, 2);
    assert_eq!(token_usage.output_tokens, 25);
    assert_eq!(token_usage.reasoning_tokens, 4);
    assert_eq!(token_usage.total_tokens, 38);
}

#[test]
fn file_id_document_serializes_as_input_file_content() {
    let message = message::Message::User {
        content: OneOrMany::one(message::UserContent::Document(message::Document {
            data: DocumentSourceKind::FileId("file_abc".to_string()),
            media_type: None,
            additional_params: None,
        })),
    };

    let converted: Vec<Message> = message.try_into().expect("conversion should succeed");
    let Message::User { content, .. } = &converted[0] else {
        panic!("expected user message");
    };

    let json = serde_json::to_value(content.first_ref()).expect("serialize content");

    assert_eq!(json["type"], "input_file");
    assert_eq!(json["file_id"], "file_abc");
    assert!(json.get("file_data").is_none());
    assert!(json.get("file_url").is_none());
}

#[test]
fn file_id_document_serializes_as_input_item_content() {
    let message = completion::Message::User {
        content: OneOrMany::one(message::UserContent::Document(message::Document {
            data: DocumentSourceKind::FileId("file_abc".to_string()),
            media_type: None,
            additional_params: None,
        })),
    };

    let converted: Vec<InputItem> = message.try_into().expect("conversion should succeed");
    let json = serde_json::to_value(&converted[0]).expect("serialize input item");

    assert_eq!(json["type"], "message");
    assert_eq!(json["role"], "user");
    assert_eq!(json["content"][0]["type"], "input_file");
    assert_eq!(json["content"][0]["file_id"], "file_abc");
    assert!(json["content"][0].get("file_data").is_none());
    assert!(json["content"][0].get("file_url").is_none());
}

#[tokio::test]
async fn responses_completion_http_non_success_preserves_status_and_body() {
    use crate::client::CompletionClient;
    use crate::completion::CompletionModel;
    use crate::providers::openai::Client;
    use crate::test_utils::RecordingHttpClient;

    let body = r#"{"error":{"message":"bad image","type":"invalid_request_error","code":"invalid_value"}}"#;
    let http_client = RecordingHttpClient::with_error_response(http::StatusCode::BAD_REQUEST, body);
    let client = Client::builder()
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
        Some(http::StatusCode::BAD_REQUEST)
    );
    assert_eq!(error.provider_response_body(), Some(body));
    let json = error
        .provider_response_json()
        .expect("raw body should be valid JSON")
        .expect("parsed JSON should be present");
    assert_eq!(json["error"]["code"], "invalid_value");
}

#[test]
fn output_unknown_preserves_hosted_tool_payload() {
    let item = json!({
        "type": "web_search_call",
        "id": "ws_001",
        "status": "completed",
        "action": { "type": "search", "queries": ["rig framework"] },
    });

    let output: Output =
        serde_json::from_value(item.clone()).expect("unknown output should deserialize");

    let Output::Unknown(value) = output else {
        panic!("expected Output::Unknown for an unmodeled item type");
    };
    assert_eq!(value, item);
}

#[test]
fn output_unknown_round_trips_value_equal() {
    let item = json!({
        "type": "file_search_call",
        "id": "fs_007",
        "status": "in_progress",
        "queries": ["lifecycle"],
    });

    let output: Output =
        serde_json::from_value(item.clone()).expect("unknown output should deserialize");
    let serialized = serde_json::to_value(&output).expect("unknown output should serialize");

    assert_eq!(serialized, item);
}

#[test]
fn output_known_variant_with_bad_body_errors() {
    // A recognized `type` tag with a malformed body must still error rather
    // than silently degrading to `Output::Unknown`.
    let malformed = json!({
        "type": "function_call",
        "id": "call_1",
        // missing `arguments`, `call_id`, `name`
    });

    let result: Result<Output, _> = serde_json::from_value(malformed);
    assert!(result.is_err());
}

#[test]
fn completion_response_with_unknown_output_keeps_usage() {
    // Guards the original reason the catch-all exists: an unknown item must
    // not break decoding of the whole response or drop token usage.
    let response = json!({
        "id": "resp_123",
        "object": "response",
        "created_at": 0,
        "status": "completed",
        "model": "gpt-5.4",
        "output": [
            {
                "type": "web_search_call",
                "id": "ws_001",
                "status": "completed",
            },
            {
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "status": "completed",
                "content": [ { "type": "output_text", "text": "hi", "annotations": [] } ],
            },
        ],
        "usage": {
            "input_tokens": 100,
            "input_tokens_details": { "cached_tokens": 25 },
            "output_tokens": 50,
            "output_tokens_details": { "reasoning_tokens": 15 },
            "total_tokens": 150,
        },
    });

    let response: CompletionResponse =
        serde_json::from_value(response).expect("response should deserialize");

    assert!(matches!(response.output.first(), Some(Output::Unknown(_))));
    let usage = response.usage.expect("usage should be present");
    assert_eq!(usage.total_tokens, 150);
}

#[test]
fn output_known_variant_round_trips_value_equal() {
    // The hand-written Serialize must reproduce the modeled wire shape, so a
    // decoded known item re-serializes value-equal to what it came from
    // (guards the `function_call` arm, including its stringified `arguments`).
    // The item ID uses the provider-native `fc_` prefix; other IDs are
    // intentionally dropped on serialization (see `OutputFunctionCall::id`).
    let item = json!({
        "type": "function_call",
        "id": "fc_1",
        "arguments": "{}",
        "call_id": "c1",
        "name": "search",
        "status": "completed",
    });

    let output: Output =
        serde_json::from_value(item.clone()).expect("known output should deserialize");
    assert!(matches!(output, Output::FunctionCall(_)));

    let serialized = serde_json::to_value(&output).expect("known output should serialize");
    assert_eq!(serialized, item);
}

#[test]
fn output_reasoning_round_trips_value_equal() {
    // Highest-value parity guard: the `Reasoning` struct variant threads its
    // fields by hand in *both* directions. Populated `encrypted_content` /
    // `status` (the `#[serde(default)]` optionals) must survive
    // serialize -> deserialize unchanged — catching a dropped field or a
    // forgotten `reasoning` dispatch arm (which would degrade to `Unknown`).
    let original = Output::Reasoning {
        id: "reasoning_1".to_string(),
        summary: vec![ReasoningSummary::SummaryText {
            text: "weighing options".to_string(),
        }],
        content: vec!["private reasoning".to_string()],
        encrypted_content: Some("ENCRYPTED".to_string()),
        status: Some(ToolStatus::Completed),
    };

    let value = serde_json::to_value(&original).expect("reasoning should serialize");
    let round_tripped: Output =
        serde_json::from_value(value).expect("reasoning should deserialize");

    assert_eq!(round_tripped, original);
}

#[test]
fn output_reasoning_conversion_omits_empty_encrypted_content() {
    let output = Output::Reasoning {
        id: "reasoning_1".to_string(),
        summary: vec![],
        content: vec!["visible reasoning".to_string()],
        encrypted_content: Some(String::new()),
        status: Some(ToolStatus::Completed),
    };

    let converted = Vec::<completion::AssistantContent>::from(output);

    assert_eq!(converted.len(), 1);
    let completion::AssistantContent::Reasoning(reasoning) = &converted[0] else {
        panic!("expected reasoning output");
    };
    assert_eq!(reasoning.id.as_deref(), Some("reasoning_1"));
    assert_eq!(reasoning.content.len(), 1);
    assert!(matches!(
        reasoning.content.first(),
        Some(message::ReasoningContent::Text { text, .. })
            if text == "visible reasoning"
    ));
}

#[test]
fn output_reasoning_conversion_preserves_non_empty_encrypted_content() {
    let output = Output::Reasoning {
        id: "reasoning_1".to_string(),
        summary: vec![],
        content: vec![],
        encrypted_content: Some("ciphertext".to_string()),
        status: Some(ToolStatus::Completed),
    };

    let converted = Vec::<completion::AssistantContent>::from(output);

    assert_eq!(converted.len(), 1);
    let completion::AssistantContent::Reasoning(reasoning) = &converted[0] else {
        panic!("expected reasoning output");
    };
    assert_eq!(
        reasoning.content,
        vec![message::ReasoningContent::Encrypted(
            "ciphertext".to_string()
        )]
    );
}

#[test]
fn output_reasoning_none_optionals_serialize_as_explicit_null() {
    // Wire-anchored complement to the round-trip test: with `None`
    // optionals, the keys must still be emitted as explicit `null` (the
    // derived behavior this hand-written serde replaced has no
    // `skip_serializing_if`). Guards against a future refactor silently
    // dropping the keys and changing the wire shape.
    let value = serde_json::to_value(Output::Reasoning {
        id: "reasoning_1".to_string(),
        summary: vec![],
        content: vec![],
        encrypted_content: None,
        status: None,
    })
    .expect("reasoning should serialize");

    assert_eq!(value["type"], "reasoning");
    assert_eq!(value["encrypted_content"], Value::Null);
    assert_eq!(value["status"], Value::Null);
    assert!(value.get("encrypted_content").is_some());
    assert!(value.get("status").is_some());
}

#[test]
fn output_message_round_trips_value_equal() {
    // Wire-anchored serialize check for the `message` arm (only
    // `function_call` was anchored): a decoded message item re-serializes
    // value-equal to the input, tag included.
    let item = json!({
        "type": "message",
        "id": "msg_1",
        "role": "assistant",
        "status": "completed",
        "content": [ { "type": "output_text", "text": "hello", "annotations": [] } ],
    });

    let output: Output =
        serde_json::from_value(item.clone()).expect("message item should deserialize");
    assert!(matches!(output, Output::Message(_)));

    let serialized = serde_json::to_value(&output).expect("message should serialize");
    assert_eq!(serialized, item);
}

#[test]
fn each_known_tag_decodes_to_its_modeled_variant() {
    // Guards every modeled dispatch arm: a well-formed item for each known
    // `type` must decode to its specific variant, never to `Unknown`. Adding
    // an `Output` variant without a matching deserialize arm fails here
    // instead of silently routing real items to `Unknown`.
    let message: Output = serde_json::from_value(json!({
        "type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
        "content": [ { "type": "output_text", "text": "hi", "annotations": [] } ],
    }))
    .expect("message item should decode");
    assert!(matches!(message, Output::Message(_)));

    let function_call: Output = serde_json::from_value(json!({
        "type": "function_call", "id": "call_1", "arguments": "{}",
        "call_id": "c1", "name": "f", "status": "completed",
    }))
    .expect("function_call item should decode");
    assert!(matches!(function_call, Output::FunctionCall(_)));

    let reasoning: Output =
        serde_json::from_value(json!({ "type": "reasoning", "id": "r1", "summary": [] }))
            .expect("reasoning item should decode");
    assert!(matches!(reasoning, Output::Reasoning { .. }));
}

#[test]
fn output_without_usable_type_tag_decodes_to_unknown() {
    // An absent or non-string `type` is itself unmodeled, so it is captured
    // verbatim as `Unknown` rather than erroring.
    for item in [
        json!({ "id": "x", "note": "no type field" }),
        json!({ "type": 7, "id": "x" }),
    ] {
        let output: Output =
            serde_json::from_value(item.clone()).expect("should decode to Unknown");
        assert_eq!(output, Output::Unknown(item));
    }
}

// Regression tests for issue #1429: `file_url` and `filename` are mutually
// exclusive on OpenAI's Responses API (400 `mutually_exclusive_parameters`),
// so URL-backed PDFs must not carry the hardcoded `filename`. PR #1432
// fixed the `TryFrom<message::Message> for Vec<Message>` conversion; these
// tests also cover the `TryFrom<crate::completion::Message> for
// Vec<InputItem>` path that `CompletionModel::completion()` requests
// actually go through.
//
// See <https://platform.openai.com/docs/guides/pdf-files> for the
// `input_file` content part and its `file_url` / `file_data` / `file_id`
// input variants.

const PDF_URL: &str = "https://example.com/resume.pdf";

fn url_pdf_message() -> message::Message {
    message::Message::User {
        content: OneOrMany::one(message::UserContent::document_url(
            PDF_URL,
            Some(message::DocumentMediaType::PDF),
        )),
    }
}

/// Recursively collect every JSON object with `"type": "input_file"`.
fn find_input_files(value: &serde_json::Value, out: &mut Vec<serde_json::Value>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("type").and_then(|t| t.as_str()) == Some("input_file") {
                out.push(value.clone());
            }
            map.values().for_each(|v| find_input_files(v, out));
        }
        serde_json::Value::Array(items) => {
            items.iter().for_each(|v| find_input_files(v, out));
        }
        _ => {}
    }
}

fn sole_input_file(value: &serde_json::Value) -> serde_json::Value {
    let mut found = Vec::new();
    find_input_files(value, &mut found);
    assert_eq!(
        found.len(),
        1,
        "expected exactly one input_file item in {value:#}"
    );
    found.pop().unwrap()
}

fn assert_url_only_input_file(input_file: &serde_json::Value) {
    assert_eq!(
        input_file.get("file_url").and_then(|v| v.as_str()),
        Some(PDF_URL),
        "URL PDF should carry file_url: {input_file:#}"
    );
    assert_eq!(
        input_file.get("filename"),
        None,
        "filename must be absent for URL PDFs (issue #1429): {input_file:#}"
    );
    assert_eq!(
        input_file.get("file_data"),
        None,
        "file_data must be absent for URL PDFs: {input_file:#}"
    );
}

#[test]
fn url_pdf_via_input_item_path_omits_filename() {
    let items = Vec::<InputItem>::try_from(url_pdf_message())
        .expect("URL PDF should convert to input items");
    let json = serde_json::to_value(&items).expect("input items should serialize");
    assert_url_only_input_file(&sole_input_file(&json));
}

#[test]
fn url_pdf_in_full_completion_request_omits_filename() {
    let core_request = crate::completion::CompletionRequest {
        model: None,
        preamble: None,
        chat_history: OneOrMany::one(url_pdf_message()),
        documents: Vec::new(),
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    };

    let request = CompletionRequest::try_from(("gpt-4o".to_string(), core_request))
        .expect("request should convert");
    let json = serde_json::to_value(&request).expect("request should serialize");
    assert_url_only_input_file(&sole_input_file(&json));
}

#[test]
fn url_pdf_via_vec_message_path_omits_filename() {
    let messages =
        Vec::<Message>::try_from(url_pdf_message()).expect("URL PDF should convert to messages");
    let json = serde_json::to_value(&messages).expect("messages should serialize");
    assert_url_only_input_file(&sole_input_file(&json));
}

#[test]
fn base64_pdf_via_input_item_path_keeps_filename() {
    let input = message::Message::User {
        content: OneOrMany::one(message::UserContent::Document(message::Document {
            data: DocumentSourceKind::base64("dGVzdA=="),
            media_type: Some(message::DocumentMediaType::PDF),
            additional_params: None,
        })),
    };

    let items =
        Vec::<InputItem>::try_from(input).expect("base64 PDF should convert to input items");
    let json = serde_json::to_value(&items).expect("input items should serialize");
    let input_file = sole_input_file(&json);

    assert_eq!(
        input_file.get("file_data").and_then(|v| v.as_str()),
        Some("data:application/pdf;base64,dGVzdA=="),
        "base64 PDF should carry file_data: {input_file:#}"
    );
    assert_eq!(
        input_file.get("filename").and_then(|v| v.as_str()),
        Some("document.pdf"),
        "base64 PDF should keep the default filename: {input_file:#}"
    );
    assert_eq!(
        input_file.get("file_url"),
        None,
        "base64 PDF should not carry file_url: {input_file:#}"
    );
}

fn image_content(
    data: DocumentSourceKind,
    media_type: Option<message::ImageMediaType>,
) -> message::UserContent {
    message::UserContent::Image(message::Image {
        data,
        media_type,
        detail: None,
        additional_params: None,
    })
}

fn user_message_with(content: message::UserContent) -> completion::Message {
    completion::Message::User {
        content: OneOrMany::one(content),
    }
}

/// `ReasoningSummary::text` and the `OneOrMany<String>` conversion used by
/// providers that lift summaries into a reasoning item.
#[test]
fn reasoning_summary_accessors_round_trip() {
    let summaries = Vec::<ReasoningSummary>::from(
        OneOrMany::many(["first".to_string(), "second".to_string()]).expect("non-empty"),
    );

    assert_eq!(
        summaries
            .iter()
            .map(ReasoningSummary::text)
            .collect::<Vec<_>>(),
        vec!["first".to_string(), "second".to_string()]
    );
}

/// The `content` field of a reasoning item accepts every documented wire
/// spelling: a plain string, an array of strings, and the tagged
/// `reasoning_text` object array (which also round-trips on serialize).
#[test]
fn reasoning_content_accepts_every_wire_spelling() {
    let cases = [
        (json!("step one"), vec!["step one".to_string()]),
        (
            json!(["step one", "step two"]),
            vec!["step one".to_string(), "step two".to_string()],
        ),
        (
            json!([
                {"type": "reasoning_text", "text": "step one"},
                {"type": "reasoning_text", "text": "step two"}
            ]),
            vec!["step one".to_string(), "step two".to_string()],
        ),
    ];

    for (wire, expected) in cases {
        let value = json!({
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "summary"}],
            "content": wire,
        });
        let reasoning: OpenAIReasoning =
            serde_json::from_value(value).expect("reasoning should deserialize");
        assert_eq!(reasoning.content, expected, "spelling {wire}");

        let encoded = serde_json::to_value(&reasoning).expect("reasoning should serialize");
        let expected_wire: Vec<_> = expected
            .iter()
            .map(|text| json!({"type": "reasoning_text", "text": text}))
            .collect();
        assert_eq!(
            encoded["content"],
            serde_json::Value::Array(expected_wire),
            "serialization always emits the tagged object array"
        );
    }
}

/// A base64 tool-result image without a media type cannot form a data URL
/// and must fail loudly rather than fabricate one.
#[test]
fn tool_result_base64_image_without_media_type_errors() {
    let input = rig_tool_result(message::ToolResultContent::Image(message::Image {
        data: DocumentSourceKind::base64("dGVzdA=="),
        media_type: None,
        detail: None,
        additional_params: None,
    }));

    let err = Vec::<Message>::try_from(input)
        .expect_err("a media type is required for base64 tool-result images");
    assert!(err.to_string().contains("media type is required"));
}

/// A URL-backed tool-result image passes the URL through as `image_url`.
#[test]
fn tool_result_url_image_uses_the_url_directly() {
    let input = rig_tool_result(message::ToolResultContent::Image(message::Image {
        data: DocumentSourceKind::url("https://example.com/pic.png"),
        media_type: None,
        detail: None,
        additional_params: None,
    }));

    let items = Vec::<InputItem>::try_from(input).expect("URL image should convert");
    match &items[0].input {
        InputContent::FunctionCallOutput(ToolResult {
            output: ToolResultOutput::Content(blocks),
            ..
        }) => match &blocks[0] {
            ToolResultOutputContent::InputImage { image_url, .. } => {
                assert_eq!(image_url.as_deref(), Some("https://example.com/pic.png"));
            }
            other => panic!("expected an input image block, got {other:?}"),
        },
        other => panic!("expected a function-call output item, got {other:?}"),
    }
}

/// Source kinds the Responses API cannot express as a tool-result image
/// (raw bytes, string payloads) surface as conversion errors.
#[test]
fn tool_result_unsupported_image_sources_error() {
    for data in [
        DocumentSourceKind::raw(vec![1, 2, 3]),
        DocumentSourceKind::string("not an image"),
    ] {
        let input = rig_tool_result(message::ToolResultContent::Image(message::Image {
            data,
            media_type: None,
            detail: None,
            additional_params: None,
        }));

        let err = Vec::<InputItem>::try_from(input).expect_err("unsupported source should error");
        assert!(
            err.to_string()
                .contains("Unsupported tool-result image source"),
            "got: {err}"
        );
    }
}

/// `From<Message> for InputItem` maps every Responses message shape onto
/// an input item, including the reasoning-aware assistant role elision.
#[test]
fn from_message_converts_every_responses_message_shape() {
    let text = || {
        OneOrMany::one(AssistantContentType::Text(AssistantContent::OutputText(
            Text::new("hi"),
        )))
    };
    let reasoning = || {
        OneOrMany::one(AssistantContentType::Reasoning(OpenAIReasoning {
            id: "rs_1".to_string(),
            summary: Vec::new(),
            content: Vec::new(),
            encrypted_content: None,
            status: None,
        }))
    };

    let user = InputItem::from(Message::User {
        content: OneOrMany::one(UserContent::InputText {
            text: "hi".to_string(),
        }),
        name: None,
    });
    assert!(matches!(user.role, Some(Role::User)));

    let assistant_text = InputItem::from(Message::Assistant {
        content: text(),
        id: "msg_1".to_string(),
        name: None,
        status: ToolStatus::Completed,
    });
    assert!(matches!(assistant_text.role, Some(Role::Assistant)));

    let assistant_reasoning = InputItem::from(Message::Assistant {
        content: reasoning(),
        id: "msg_1".to_string(),
        name: None,
        status: ToolStatus::Completed,
    });
    assert!(
        assistant_reasoning.role.is_none(),
        "a reasoning item replays without a message role"
    );

    let assistant_input = InputItem::from(Message::AssistantInput {
        content: "hi".to_string(),
        name: None,
    });
    assert!(matches!(assistant_input.role, Some(Role::Assistant)));

    let system = InputItem::from(Message::system("you are terse"));
    assert!(matches!(system.role, Some(Role::System)));

    let tool_result = InputItem::from(Message::ToolResult {
        tool_call_id: "call_1".to_string(),
        output: ToolResultOutput::Text("ok".to_string()),
    });
    assert!(tool_result.role.is_none());
    assert!(matches!(
        tool_result.input,
        InputContent::FunctionCallOutput(_)
    ));
}

/// A hand-built input item whose content has no `role` key of its own
/// (a function-call output) still serializes the stored role alongside it.
#[test]
fn input_item_serializes_a_role_for_non_message_content() {
    let item = InputItem {
        role: Some(Role::User),
        input: InputContent::FunctionCallOutput(ToolResult {
            call_id: "call_1".to_string(),
            output: ToolResultOutput::Text("ok".to_string()),
            status: ToolStatus::Completed,
        }),
    };

    let encoded = serde_json::to_value(&item).expect("input item should serialize");
    assert_eq!(encoded["role"], json!("user"));
    assert_eq!(encoded["call_id"], json!("call_1"));
    assert_eq!(encoded["output"], json!("ok"));
}

/// User image content converts through the full completion-request path:
/// base64 (with and without a media type) and URL sources succeed; raw and
/// string sources fail loudly.
#[test]
fn user_image_content_converts_for_every_source_kind() {
    let base64_with_type = Vec::<InputItem>::try_from(user_message_with(image_content(
        DocumentSourceKind::base64("dGVzdA=="),
        Some(message::ImageMediaType::PNG),
    )))
    .expect("base64 image with media type should convert");
    let encoded = serde_json::to_value(&base64_with_type).expect("items should serialize");
    assert_eq!(
        encoded[0]["content"][0]["image_url"],
        json!("data:image/png;base64,dGVzdA==")
    );

    let base64_without_type = Vec::<InputItem>::try_from(user_message_with(image_content(
        DocumentSourceKind::base64("dGVzdA=="),
        None,
    )))
    .expect("a media-type-less base64 image still converts");
    let encoded = serde_json::to_value(&base64_without_type).expect("items should serialize");
    assert_eq!(
        encoded[0]["content"][0]["image_url"],
        json!("data:;base64,dGVzdA==")
    );

    let url = Vec::<InputItem>::try_from(user_message_with(image_content(
        DocumentSourceKind::url("https://example.com/pic.png"),
        None,
    )))
    .expect("URL image should convert");
    let encoded = serde_json::to_value(&url).expect("items should serialize");
    assert_eq!(
        encoded[0]["content"][0]["image_url"],
        json!("https://example.com/pic.png")
    );

    for data in [
        DocumentSourceKind::raw(vec![1, 2, 3]),
        DocumentSourceKind::string("not an image"),
    ] {
        let err = Vec::<InputItem>::try_from(user_message_with(image_content(data, None)))
            .expect_err("unsupported image source should error");
        assert!(
            err.to_string().contains("not supported")
                || err.to_string().contains("Unsupported document type"),
            "got: {err}"
        );
    }
}

/// PDF documents from raw or string sources cannot become Responses
/// `file_data`/`file_url` and error instead.
#[test]
fn pdf_documents_from_unsupported_sources_error() {
    for data in [
        DocumentSourceKind::raw(vec![1, 2, 3]),
        DocumentSourceKind::string("not really a pdf"),
    ] {
        let input = user_message_with(message::UserContent::Document(message::Document {
            data,
            media_type: Some(message::DocumentMediaType::PDF),
            additional_params: None,
        }));

        let err =
            Vec::<InputItem>::try_from(input).expect_err("unsupported PDF source should error");
        assert!(
            err.to_string().contains("not supported")
                || err.to_string().contains("Unsupported document type"),
            "got: {err}"
        );
    }
}

/// Assistant image content has no Responses representation and must error
/// rather than silently dropping content.
#[test]
fn assistant_image_content_errors_on_request_conversion() {
    let input = completion::Message::Assistant {
        id: Some("msg_1".to_string()),
        content: OneOrMany::one(message::AssistantContent::Image(message::Image {
            data: DocumentSourceKind::url("https://example.com/pic.png"),
            media_type: None,
            detail: None,
            additional_params: None,
        })),
    };

    let err = Vec::<InputItem>::try_from(input).expect_err("assistant images should not convert");
    assert!(
        err.to_string()
            .contains("Assistant image content is not supported"),
        "got: {err}"
    );
}

/// Hosted tool constructors and their config extension.
#[test]
fn hosted_tool_constructors_set_their_kind() {
    assert_eq!(
        ResponsesToolDefinition::hosted("custom_tool").kind,
        "custom_tool"
    );
    assert_eq!(ResponsesToolDefinition::web_search().kind, "web_search");
    assert_eq!(ResponsesToolDefinition::file_search().kind, "file_search");
    assert_eq!(ResponsesToolDefinition::computer_use().kind, "computer_use");

    let tool = ResponsesToolDefinition::web_search()
        .with_config("search_context_size", serde_json::json!("low"));
    assert_eq!(tool.config["search_context_size"], json!("low"));
}

/// The request-level builder extensions for structured outputs, reasoning,
/// and hosted tools.
#[test]
fn completion_request_builder_extensions_apply() {
    let base = CompletionRequest::try_from(("gpt-4o".to_string(), weather_tool_request()))
        .expect("request should convert");

    let structured = base.clone().with_structured_outputs(
        "weather",
        json!({"type": "object", "properties": {"city": {"type": "string"}}}),
    );
    let TextConfig {
        format: TextFormat::JsonSchema(StructuredOutputsInput { name, strict, .. }),
    } = structured
        .additional_parameters
        .text
        .expect("structured output should be configured")
    else {
        panic!("expected a JSON schema text format");
    };
    assert_eq!(name, "weather");
    assert!(strict);

    let reasoned = base
        .clone()
        .with_reasoning(Reasoning::new().with_effort(ReasoningEffort::High));
    assert!(reasoned.additional_parameters.reasoning.is_some());

    let with_one = base
        .clone()
        .with_tool(ResponsesToolDefinition::web_search());
    assert_eq!(with_one.tools.len(), 2);
    assert_eq!(with_one.tools[1].kind, "web_search");

    let with_many = base.with_tools([
        ResponsesToolDefinition::file_search(),
        ResponsesToolDefinition::computer_use(),
    ]);
    assert_eq!(with_many.tools.len(), 3);
    assert_eq!(with_many.tools[1].kind, "file_search");
    assert_eq!(with_many.tools[2].kind, "computer_use");
}

fn usage_with(
    details: Option<InputTokensDetails>,
    output: Option<OutputTokensDetails>,
) -> ResponsesUsage {
    ResponsesUsage {
        input_tokens: 1,
        input_tokens_details: details,
        output_tokens: 2,
        output_tokens_details: output,
        total_tokens: 3,
    }
}

/// `Add` keeps whichever side carries details and drops to `None` only
/// when neither does — and `From<ResponsesUsage>` carries the detail
/// fields onto the normalized usage.
#[test]
fn responses_usage_add_and_from_cover_every_details_combination() {
    let details = InputTokensDetails { cached_tokens: 5 };
    let output_details = OutputTokensDetails {
        reasoning_tokens: 7,
    };

    let lhs_only =
        usage_with(Some(details.clone()), Some(output_details.clone())) + usage_with(None, None);
    assert_eq!(
        lhs_only
            .input_tokens_details
            .map(|details| details.cached_tokens),
        Some(5)
    );
    assert_eq!(
        lhs_only
            .output_tokens_details
            .map(|details| details.reasoning_tokens),
        Some(7)
    );

    let neither = usage_with(None, None) + usage_with(None, None);
    assert!(neither.input_tokens_details.is_none());
    assert!(neither.output_tokens_details.is_none());

    let input_sum = details.clone() + InputTokensDetails { cached_tokens: 2 };
    assert_eq!(input_sum.cached_tokens, 7);
    let output_sum = output_details.clone()
        + OutputTokensDetails {
            reasoning_tokens: 3,
        };
    assert_eq!(output_sum.reasoning_tokens, 10);

    let normalized =
        crate::completion::Usage::from(usage_with(Some(details), Some(output_details)));
    assert_eq!(normalized.cached_input_tokens, 5);
    assert_eq!(normalized.reasoning_tokens, 7);
    assert_eq!(normalized.total_tokens, 3);
}

/// Every status keeps OpenAI's own wire spelling when rendered into a
/// finish reason, including the in-flight ones `map_finish_reason` itself
/// never renders.
#[test]
fn response_status_wire_name_matches_the_documented_spellings() {
    for (status, expected) in [
        (ResponseStatus::InProgress, "in_progress"),
        (ResponseStatus::Completed, "completed"),
        (ResponseStatus::Failed, "failed"),
        (ResponseStatus::Cancelled, "cancelled"),
        (ResponseStatus::Queued, "queued"),
        (ResponseStatus::Incomplete, "incomplete"),
    ] {
        assert_eq!(response_status_wire_name(&status), expected);
    }
}

/// A boolean `additional_params` payload is the `stream` toggle and must
/// not leak into the typed additional-parameters object.
#[test]
fn boolean_additional_params_become_the_stream_flag() {
    let mut request = weather_tool_request();
    request.additional_params = Some(json!(true));

    let converted = CompletionRequest::try_from(("gpt-4o".to_string(), request)).expect("convert");
    assert_eq!(converted.stream, Some(true));
    assert_eq!(
        serde_json::to_value(&converted).expect("serialize")["background"],
        Value::Null,
        "the boolean payload itself must not survive as a typed parameter"
    );
}

/// An `additional_params.tools` payload that is not a valid tool list
/// errors with the payload context rather than a bare serde message.
#[test]
fn invalid_additional_params_tools_payload_errors() {
    let mut request = weather_tool_request();
    request.additional_params = Some(json!({"tools": "not-a-tool-list"}));

    let err = CompletionRequest::try_from(("gpt-4o".to_string(), request))
        .expect_err("invalid tools payload should error");
    assert!(
        err.to_string()
            .contains("Invalid OpenAI Responses tools payload"),
        "got: {err}"
    );
}

/// `with_model` matches `new`, and `provider_name` reports the ext's
/// provider descriptor.
#[test]
fn with_model_matches_new_and_reports_provider_name() {
    let client = crate::providers::openai::Client::new("dummy-key").expect("client");
    let model = ResponsesCompletionModel::with_model(client, "gpt-4o-mini");

    assert_eq!(model.model, "gpt-4o-mini");
    assert_eq!(model.provider_name(), "openai");
}

/// Model-level default tools append after the request's own tools, and
/// `completions_api` hands the same model id to the Chat Completions model.
#[test]
fn model_with_tools_appends_and_completions_api_keeps_the_model() {
    let client = crate::providers::openai::Client::new("dummy-key").expect("client");
    let model = ResponsesCompletionModel::new(client, "gpt-4o-mini").with_tools([
        ResponsesToolDefinition::web_search(),
        ResponsesToolDefinition::file_search(),
    ]);

    let req = model
        .create_completion_request(weather_tool_request())
        .expect("request should convert");
    assert_eq!(req.tools.len(), 3);
    assert_eq!(req.tools[1].kind, "web_search");
    assert_eq!(req.tools[2].kind, "file_search");

    let chat_model = ResponsesCompletionModel::new(
        crate::providers::openai::Client::new("dummy-key").expect("client"),
        "gpt-4o-mini",
    )
    .completions_api();
    assert_eq!(chat_model.model, "gpt-4o-mini");
}

/// `AdditionalParameters::to_json` renders the typed struct as a JSON
/// object.
#[test]
fn additional_parameters_to_json_is_an_object() {
    let params = AdditionalParameters {
        background: Some(true),
        user: Some("jane".to_string()),
        ..Default::default()
    };

    let encoded = params.to_json();
    assert_eq!(encoded["background"], json!(true));
    assert_eq!(encoded["user"], json!("jane"));
}

/// The `Message::system` helper and the refusal-content conversion.
#[test]
fn message_system_helper_and_refusal_conversion() {
    let encoded =
        serde_json::to_value(Message::system("be terse")).expect("system message should serialize");
    assert_eq!(encoded["role"], json!("system"));
    assert_eq!(encoded["content"][0]["type"], json!("input_text"));
    assert_eq!(encoded["content"][0]["text"], json!("be terse"));

    let converted = completion::AssistantContent::from(AssistantContent::Refusal {
        refusal: "no can do".to_string(),
    });
    assert!(matches!(
        converted,
        completion::AssistantContent::Text(Text { text, .. }) if text == "no can do"
    ));
}

/// `FromStr` infallibly produces the plain-text content variants.
#[test]
fn content_from_str_produces_plain_text() {
    let system: SystemContent = "hello".parse().expect("system content parses");
    assert_eq!(
        system,
        SystemContent::InputText {
            text: "hello".into()
        }
    );

    let user: UserContent = "hello".parse().expect("user content parses");
    assert_eq!(
        user,
        UserContent::InputText {
            text: "hello".into()
        }
    );
}

fn core_user_message(content: message::UserContent) -> message::Message {
    message::Message::User {
        content: OneOrMany::one(content),
    }
}

/// The `Vec<Message>` path converts user image content for every source
/// kind, erroring on the ones the Responses wire cannot express.
#[test]
fn responses_message_image_content_converts_for_every_source_kind() {
    let messages = Vec::<Message>::try_from(core_user_message(image_content(
        DocumentSourceKind::base64("dGVzdA=="),
        Some(message::ImageMediaType::PNG),
    )))
    .expect("base64 image should convert");
    let encoded = serde_json::to_value(&messages).expect("messages should serialize");
    assert_eq!(
        encoded[0]["content"][0]["image_url"],
        json!("data:image/png;base64,dGVzdA==")
    );

    let messages = Vec::<Message>::try_from(core_user_message(image_content(
        DocumentSourceKind::base64("dGVzdA=="),
        None,
    )))
    .expect("a media-type-less base64 image still converts");
    let encoded = serde_json::to_value(&messages).expect("messages should serialize");
    assert_eq!(
        encoded[0]["content"][0]["image_url"],
        json!("data:;base64,dGVzdA==")
    );

    let messages = Vec::<Message>::try_from(core_user_message(image_content(
        DocumentSourceKind::url("https://example.com/pic.png"),
        None,
    )))
    .expect("URL image should convert");
    let encoded = serde_json::to_value(&messages).expect("messages should serialize");
    assert_eq!(
        encoded[0]["content"][0]["image_url"],
        json!("https://example.com/pic.png")
    );

    for data in [
        DocumentSourceKind::raw(vec![1, 2, 3]),
        DocumentSourceKind::string("not an image"),
    ] {
        let err = Vec::<Message>::try_from(core_user_message(image_content(data, None)))
            .expect_err("unsupported image source should error");
        assert!(
            err.to_string().contains("not supported")
                || err.to_string().contains("Unsupported document type"),
            "got: {err}"
        );
    }
}

/// Documents and audio through the `Vec<Message>` path: base64 PDFs carry
/// `file_data` plus the default filename, plain base64 documents become
/// text, base64 audio is passed through, and non-base64 audio errors.
#[test]
fn responses_message_document_and_audio_content_convert() {
    let messages = Vec::<Message>::try_from(core_user_message(message::UserContent::Document(
        message::Document {
            data: DocumentSourceKind::base64("dGVzdA=="),
            media_type: Some(message::DocumentMediaType::PDF),
            additional_params: None,
        },
    )))
    .expect("base64 PDF should convert");
    let encoded = serde_json::to_value(&messages).expect("messages should serialize");
    assert_eq!(
        encoded[0]["content"][0]["file_data"],
        json!("data:application/pdf;base64,dGVzdA==")
    );
    assert_eq!(encoded[0]["content"][0]["filename"], json!("document.pdf"));

    for data in [
        DocumentSourceKind::raw(vec![1, 2, 3]),
        DocumentSourceKind::string("not really a pdf"),
    ] {
        let err = Vec::<Message>::try_from(core_user_message(message::UserContent::Document(
            message::Document {
                data,
                media_type: Some(message::DocumentMediaType::PDF),
                additional_params: None,
            },
        )))
        .expect_err("unsupported PDF source should error");
        assert!(
            err.to_string().contains("not supported")
                || err.to_string().contains("Unsupported document type"),
            "got: {err}"
        );
    }

    let messages = Vec::<Message>::try_from(core_user_message(message::UserContent::Document(
        message::Document {
            data: DocumentSourceKind::base64("aGk="),
            media_type: None,
            additional_params: None,
        },
    )))
    .expect("plain base64 document should convert");
    let encoded = serde_json::to_value(&messages).expect("messages should serialize");
    assert_eq!(encoded[0]["content"][0]["type"], json!("input_text"));

    let messages = Vec::<Message>::try_from(core_user_message(message::UserContent::Audio(
        message::Audio {
            data: DocumentSourceKind::base64("//uQx"),
            media_type: Some(message::AudioMediaType::WAV),
            additional_params: None,
        },
    )))
    .expect("base64 audio should convert");
    let encoded = serde_json::to_value(&messages).expect("messages should serialize");
    assert_eq!(
        encoded[0]["content"][0]["input_audio"]["format"],
        json!("wav")
    );

    let err = Vec::<Message>::try_from(core_user_message(message::UserContent::Audio(
        message::Audio {
            data: DocumentSourceKind::url("https://example.com/clip.wav"),
            media_type: None,
            additional_params: None,
        },
    )))
    .expect_err("non-base64 audio should error");
    assert!(
        err.to_string().contains("Audio must be base64"),
        "got: {err}"
    );
}

/// A tool result without a `call_id` cannot be correlated on the wire and
/// errors rather than serializing an unpaired output.
#[test]
fn tool_result_without_call_id_errors_on_message_conversion() {
    let input = message::Message::User {
        content: OneOrMany::one(message::UserContent::ToolResult(message::ToolResult {
            id: "result-id".to_string(),
            call_id: None,
            content: OneOrMany::one(message::ToolResultContent::text("tool output")),
        })),
    };

    let err = Vec::<Message>::try_from(input).expect_err("call_id is required");
    assert!(
        err.to_string().contains("`call_id` is required"),
        "got: {err}"
    );
}

/// A core system message converts to a Responses system message, and an
/// empty-text assistant fragment is skipped entirely.
#[test]
fn system_messages_convert_and_empty_assistant_text_is_skipped() {
    let messages = Vec::<Message>::try_from(message::Message::system("be terse"))
        .expect("system message should convert");
    assert!(matches!(messages.as_slice(), [Message::System { .. }]));

    let messages = Vec::<Message>::try_from(message::Message::Assistant {
        id: None,
        content: OneOrMany::one(message::AssistantContent::Text(Text::new(""))),
    })
    .expect("empty assistant text should convert to nothing");
    assert!(messages.is_empty());
}

/// Assistant edge cases through the `Vec<Message>` path: a tool call
/// without a `call_id` errors, a valid tool call converts, and assistant
/// image content is rejected.
#[test]
fn assistant_tool_call_edges_on_message_conversion() {
    let tool_call = |call_id: Option<String>| message::Message::Assistant {
        id: Some("msg_1".to_string()),
        content: OneOrMany::one(message::AssistantContent::ToolCall(message::ToolCall {
            id: "fc_1".to_string(),
            call_id,
            function: message::ToolFunction {
                name: "lookup".to_string(),
                arguments: json!({"q": "rig"}),
            },
            signature: None,
            additional_params: None,
        })),
    };

    let err = Vec::<Message>::try_from(tool_call(None))
        .expect_err("tool call without call_id should error");
    assert!(
        err.to_string().contains("`call_id` is required"),
        "got: {err}"
    );

    let messages = Vec::<Message>::try_from(tool_call(Some("call_1".to_string())))
        .expect("tool call with call_id should convert");
    assert!(matches!(messages.as_slice(), [Message::Assistant { .. }]));

    let err = Vec::<Message>::try_from(message::Message::Assistant {
        id: Some("msg_1".to_string()),
        content: OneOrMany::one(message::AssistantContent::Image(message::Image {
            data: DocumentSourceKind::url("https://example.com/pic.png"),
            media_type: None,
            detail: None,
            additional_params: None,
        })),
    })
    .expect_err("assistant images should not convert");
    assert!(
        err.to_string()
            .contains("Assistant image content is not supported"),
        "got: {err}"
    );
}

/// Truncation policy on the output path: a function call whose arguments
/// never parsed into JSON is dropped (not fabricated), and an unmodeled
/// output item contributes no assistant content.
#[test]
fn truncated_arguments_and_unknown_outputs_contribute_no_content() {
    let truncated: Output = serde_json::from_value(json!({
        "type": "function_call",
        "id": "fc_1",
        "call_id": "call_1",
        "name": "lookup",
        "arguments": "{\"q\": \"tru",
        "status": "completed",
    }))
    .expect("truncated function call should decode");
    let converted: Vec<completion::AssistantContent> = truncated.into();
    assert!(
        converted.is_empty(),
        "a truncated call must not fabricate a tool call"
    );

    let unknown = Output::Unknown(json!({"type": "web_search_call", "id": "ws_1"}));
    let converted: Vec<completion::AssistantContent> = unknown.into();
    assert!(converted.is_empty());
}
