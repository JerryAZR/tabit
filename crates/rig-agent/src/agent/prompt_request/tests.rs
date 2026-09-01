use super::{CompletionCall, PromptResponse, PromptResponseRepr};

use crate::agent::prompt_request::streaming::fold_stream;
use crate::streaming::StreamingPrompt;

/// Fold a streaming request to its outcome — the suite drives the one
/// execution surface.
/// A mock model whose streaming surface replays the given unary-turn
/// scenario — the suite drives the one execution surface.
fn stream_model(
    turns: impl IntoIterator<Item = crate::test_utils::MockTurn>,
) -> crate::test_utils::MockCompletionModel {
    crate::test_utils::MockCompletionModel::from_stream_turns(
        turns
            .into_iter()
            .map(crate::test_utils::MockTurn::into_stream_events),
    )
}

async fn fold(
    request: crate::agent::prompt_request::streaming::StreamingPromptRequest,
) -> Result<PromptResponse, crate::completion::PromptError> {
    fold_stream(&mut request.await).await
}
use crate::{
    agent::AgentBuilder,
    completion::{
        AssistantContent, CompletionError, CompletionRequest, Message, PromptError, Usage,
    },
    test_utils::{MockAddTool, MockCompletionModel, MockContextProbeTool, MockTurn, SessionId},
    tool::{Tool, ToolContext},
};
use rig_core::message::{Text, ToolChoice, UserContent};
use serde_json::json;

fn usage(input_tokens: u64, output_tokens: u64) -> Usage {
    Usage {
        input_tokens,
        output_tokens,
        total_tokens: input_tokens + output_tokens,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_creation_1h_input_tokens: 0,
        tool_use_prompt_tokens: 0,
        reasoning_tokens: 0,
    }
}

#[test]
fn prompt_response_serializes_completion_calls_with_missing_usage() {
    let reported_usage = usage(3, 4);
    let response = PromptResponse::new("ok", reported_usage).with_completion_calls(vec![
        CompletionCall::new(0, Usage::new(), None),
        CompletionCall::new(1, reported_usage, None),
    ]);

    let value = serde_json::to_value(&response).expect("serialize prompt response");

    // Unreported usage serializes as a plain zero-valued object: zero is
    // Usage's documented sentinel for missing provider metrics, so there
    // is no null encoding to keep in sync.
    assert_eq!(
        value.get("completion_calls"),
        Some(&json!([
            {
                "call_index": 0,
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "total_tokens": 0,
                    "cached_input_tokens": 0,
                    "cache_creation_input_tokens": 0,"cache_creation_1h_input_tokens": 0,
                    "tool_use_prompt_tokens": 0,
                    "reasoning_tokens": 0,
                }
            },
            {
                "call_index": 1,
                "usage": {
                    "input_tokens": 3,
                    "output_tokens": 4,
                    "total_tokens": 7,
                    "cached_input_tokens": 0,
                    "cache_creation_input_tokens": 0,"cache_creation_1h_input_tokens": 0,
                    "tool_use_prompt_tokens": 0,
                    "reasoning_tokens": 0,
                }
            }
        ]))
    );

    let response: PromptResponse =
        serde_json::from_value(value).expect("deserialize prompt response");
    assert_eq!(
        response.completion_calls(),
        &[
            CompletionCall::new(0, Usage::new(), None),
            CompletionCall::new(1, reported_usage, None)
        ]
    );
    assert_eq!(response.requests(), 2);
}

#[test]
fn prompt_response_deserializes_pre_monoid_null_usage_format() {
    // Fixture captured from rig before CompletionCall.usage dropped its
    // Option encoding; `"usage": null` must map to zero-valued usage.
    let fixture = r#"{"output":"ok","usage":{"input_tokens":3,"output_tokens":4,"total_tokens":7,"cached_input_tokens":0,"cache_creation_input_tokens":0,"cache_creation_1h_input_tokens":0,"tool_use_prompt_tokens":0,"reasoning_tokens":0},"completion_calls":[{"call_index":0,"usage":null},{"call_index":1,"usage":{"input_tokens":3,"output_tokens":4,"total_tokens":7,"cached_input_tokens":0,"cache_creation_input_tokens":0,"cache_creation_1h_input_tokens":0,"tool_use_prompt_tokens":0,"reasoning_tokens":0}}],"messages":[{"role":"user","content":[{"type":"text","text":"add things"}]}]}"#;

    let response: PromptResponse =
        serde_json::from_str(fixture).expect("old-format response should deserialize");
    assert_eq!(
        response.completion_calls(),
        &[
            CompletionCall::new(0, Usage::new(), None),
            CompletionCall::new(1, usage(3, 4), None)
        ]
    );
}

#[test]
fn prompt_response_missing_content_reconstructs_from_output() {
    // Runs serialized before `content` existed must not deserialize to empty
    // text: the structured final turn is reconstructed from `output`, so
    // `output()` and `content()` stay consistent for legacy data.
    let mut value = serde_json::to_value(PromptResponse::new("hello", Usage::new()))
        .expect("serialize prompt response");
    value
        .as_object_mut()
        .expect("prompt response serializes to a JSON object")
        .remove("content");
    assert!(
        value.get("content").is_none(),
        "fixture must omit the content field to model legacy data"
    );

    let response: PromptResponse =
        serde_json::from_value(value).expect("legacy response without content should deserialize");

    assert_eq!(response.output(), "hello");
    assert_eq!(response.content().iter().count(), 1);
    assert_eq!(response.content().first(), AssistantContent::text("hello"));
}

#[test]
fn prompt_response_missing_content_empty_output_stays_empty_text() {
    let mut value =
        serde_json::to_value(PromptResponse::empty()).expect("serialize prompt response");
    value
        .as_object_mut()
        .expect("prompt response serializes to a JSON object")
        .remove("content");

    let response: PromptResponse = serde_json::from_value(value)
        .expect("legacy empty response without content should deserialize");

    assert_eq!(response.output(), "");
    assert_eq!(response.content().first(), AssistantContent::text(""));
}

#[test]
fn prompt_response_roundtrip_preserves_explicit_content() {
    // An explicitly-set `content` (e.g. the streaming surface's structured
    // final turn) must survive a serialize/deserialize round-trip and is not
    // clobbered by the output-derived fallback.
    let response = PromptResponse::new("visible text", Usage::new()).with_content(
        rig_core::OneOrMany::one(AssistantContent::text("structured")),
    );

    let value = serde_json::to_value(&response).expect("serialize prompt response");
    assert!(
        value.get("content").is_some(),
        "content is part of the serialized shape"
    );

    let round: PromptResponse = serde_json::from_value(value).expect("deserialize prompt response");
    assert_eq!(round.output(), "visible text");
    // The stored content is "structured" — distinct from `output` — proving the
    // output-derived fallback only fills a genuinely absent `content`. (Compare
    // the text directly to sidestep the unrelated `Text::additional_params`
    // serde round-trip asymmetry.)
    let AssistantContent::Text(text) = round.content().first() else {
        panic!("expected text content, got {:?}", round.content().first());
    };
    assert_eq!(text.text, "structured");
}

#[test]
fn prompt_response_serialize_and_deserialize_agree_on_wire_shape() {
    // Serialize *and* deserialize both route through `PromptResponseRepr`, so
    // the two directions agree on `content`'s wire shape (an `Option`).
    // Routing only deserialize through the shadow would make serialize write a
    // bare `OneOrMany` while deserialize expects an `Option`, breaking
    // round-trips for positional / non-self-describing formats. Assert this
    // structurally: the message content types use `#[serde(flatten)]`, which no
    // length-prefixed binary format can encode, and self-describing formats
    // (JSON) collapse `Some(x)` and `x` to identical bytes, hiding the mismatch.
    let response = PromptResponse::new("hi", usage(1, 2))
        .with_completion_calls(vec![CompletionCall::new(0, usage(1, 2), None)]);

    let from_response = serde_json::to_value(&response).expect("serialize response");
    let from_shadow =
        serde_json::to_value(PromptResponseRepr::from(response.clone())).expect("serialize shadow");
    assert_eq!(
        from_response, from_shadow,
        "serialize must route through the same shadow as deserialize"
    );

    // ...and the value still round-trips back to an equivalent response.
    let round: PromptResponse =
        serde_json::from_value(from_response).expect("deserialize response");
    assert_eq!(round.output(), "hi");
    assert_eq!(round.usage(), usage(1, 2));
    assert_eq!(
        round.completion_calls(),
        &[CompletionCall::new(0, usage(1, 2), None)]
    );
}

#[tokio::test]
async fn prompt_response_records_completion_call_without_reported_usage() {
    let model = stream_model([MockTurn::text("ok")]);
    let agent = AgentBuilder::new(model).build();

    let response = fold(agent.stream_prompt("say ok"))
        .await
        .expect("prompt should succeed");

    assert_eq!(response.output, "ok");
    assert_eq!(response.usage, Usage::new());
    assert_eq!(
        response.completion_calls(),
        &[CompletionCall::new(0, Usage::new(), None)]
    );
}

fn validate_follow_up_tool_history(request: &CompletionRequest) {
    let history = request.chat_history.iter().cloned().collect::<Vec<_>>();
    assert_eq!(
        history.len(),
        3,
        "follow-up request should contain the prompt, assistant tool call, and user tool result: {history:?}"
    );

    assert!(matches!(
        history.first(),
        Some(Message::User { content })
            if matches!(
                content.first(),
                UserContent::Text(text) if text.text == "do tool work"
            )
    ));

    assert!(matches!(
        history.get(1),
        Some(Message::Assistant { content, .. })
            if matches!(
                content.first(),
                AssistantContent::ToolCall(tool_call)
                    if tool_call.id == "tool_call_1"
                        && tool_call.call_id.as_deref() == Some("call_1")
            )
    ));

    assert!(matches!(
        history.get(2),
        Some(Message::User { content })
            if matches!(
                content.first(),
                UserContent::ToolResult(tool_result)
                    if tool_result.id == "tool_call_1"
                        && tool_result.call_id.as_deref() == Some("call_1")
            )
    ));
}

/// The motivating use-case: a `ToolContext` set on the prompt request is
/// threaded all the way to the tool the agent loop executes.
#[tokio::test]
async fn tool_context_reaches_tool_through_agent_loop() {
    let model = stream_model([
        MockTurn::tool_call("tool_call_1", "context_probe", json!({})),
        MockTurn::text("done"),
    ]);
    let probe = MockContextProbeTool::default();
    let agent = AgentBuilder::new(model).tool(probe.clone()).build();

    let mut context = ToolContext::new();
    context.insert(SessionId("abc-123".to_string()));

    let out = fold(
        agent
            .stream_prompt("use the tool")
            .tool_context(context)
            .max_turns(3),
    )
    .await
    .map(|response| response.output)
    .expect("run succeeds");

    assert_eq!(out, "done");
    assert_eq!(probe.observed().as_deref(), Some("session:abc-123"));
}

/// Context values persist for the whole run, across *multiple* tool-call rounds
/// (the headline value prop). The model calls the probe in two consecutive
/// rounds; both must observe the same injected value, not just the first.
#[tokio::test]
async fn tool_context_persists_across_multiple_rounds() {
    let model = stream_model([
        MockTurn::tool_call("c1", "context_probe", json!({})),
        MockTurn::tool_call("c2", "context_probe", json!({})),
        MockTurn::text("done"),
    ]);
    let probe = MockContextProbeTool::default();
    let agent = AgentBuilder::new(model).tool(probe.clone()).build();

    let mut context = ToolContext::new();
    context.insert(SessionId("abc-123".to_string()));

    let out = fold(
        agent
            .stream_prompt("use the tool twice")
            .tool_context(context)
            .max_turns(5),
    )
    .await
    .map(|response| response.output)
    .expect("run succeeds");

    assert_eq!(out, "done");
    assert_eq!(
        probe.observations(),
        vec!["session:abc-123".to_string(), "session:abc-123".to_string()],
    );
}

/// Without a context, the same tool runs with an empty one (no panic, no
/// stale value) — the backward-compatible default path.
#[tokio::test]
async fn tool_runs_with_empty_context_when_none_supplied() {
    let model = stream_model([
        MockTurn::tool_call("tool_call_1", "context_probe", json!({})),
        MockTurn::text("done"),
    ]);
    let probe = MockContextProbeTool::default();
    let agent = AgentBuilder::new(model).tool(probe.clone()).build();

    let out = fold(agent.stream_prompt("use the tool").max_turns(3))
        .await
        .map(|response| response.output)
        .expect("run succeeds");

    assert_eq!(out, "done");
    // The single call path receives an empty context and observes no session.
    assert_eq!(probe.observed().as_deref(), Some("no-session"));
}

/// Direct typed calls use the same context contract as dispatched calls.
#[tokio::test]
async fn probe_direct_call_uses_context() {
    let probe = MockContextProbeTool::default();
    let out = probe
        .call(&mut ToolContext::new(), json!({}))
        .await
        .expect("call succeeds");
    assert_eq!(out, "no-session");
    assert_eq!(probe.observed().as_deref(), Some("no-session"));
}

#[tokio::test]
async fn invalid_specific_tool_choice_fails_before_non_streaming_provider_request() {
    let model = MockCompletionModel::text("should not be requested");
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(MockAddTool)
        .tool_choice(ToolChoice::Specific {
            function_names: vec!["missing".to_string()],
        })
        .build();

    let err = fold(agent.stream_prompt("use the missing tool"))
        .await
        .expect_err("invalid ToolChoice::Specific should fail before provider request");

    match err {
        PromptError::CompletionError(CompletionError::RequestError(err)) => {
            let msg = err.to_string();
            assert!(msg.contains("missing"), "got: {msg}");
            assert!(msg.contains("add"), "got: {msg}");
        }
        other => panic!("expected CompletionError::RequestError, got {other:?}"),
    }
    assert_eq!(recorded.request_count(), 0);
}

#[tokio::test]
async fn allowed_specific_tool_call_executes_normally() {
    let model = stream_model([
        MockTurn::tool_call("tool_call_1", "add", json!({"x": 1, "y": 2})),
        MockTurn::text("done"),
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(MockAddTool)
        .tool_choice(ToolChoice::Specific {
            function_names: vec!["add".to_string()],
        })
        .build();

    let response = fold(agent.stream_prompt("use the allowed tool").max_turns(3))
        .await
        .map(|response| response.output)
        .expect("allowed specific tool should execute");

    assert_eq!(response, "done");
    assert_eq!(recorded.request_count(), 2);
}

#[tokio::test]
async fn prompt_request_stops_cleanly_on_empty_terminal_turn() {
    let first_call_usage = Usage {
        input_tokens: 1,
        output_tokens: 1,
        total_tokens: 2,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_creation_1h_input_tokens: 0,
        tool_use_prompt_tokens: 0,
        reasoning_tokens: 0,
    };
    let second_call_usage = Usage {
        input_tokens: 1,
        output_tokens: 1,
        total_tokens: 2,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_creation_1h_input_tokens: 0,
        tool_use_prompt_tokens: 0,
        reasoning_tokens: 0,
    };
    let model = stream_model([
        MockTurn::tool_call("tool_call_1", "add", json!({"x": 1, "y": 2}))
            .with_call_id("call_1")
            .with_usage(first_call_usage),
        MockTurn::text("").with_usage(second_call_usage),
    ]);
    let agent = AgentBuilder::new(model.clone()).tool(MockAddTool).build();

    let cell = test_cell("do tool work");
    let response = fold(
        crate::agent::prompt_request::streaming::StreamingPromptRequest::from_agent_cell(
            &agent,
            cell.clone(),
        )
        .max_turns(3),
    )
    .await
    .expect("empty terminal turn should not error");

    assert!(response.output.is_empty());
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 2,
            output_tokens: 2,
            total_tokens: 4,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
            tool_use_prompt_tokens: 0,
            reasoning_tokens: 0,
        }
    );
    assert_eq!(
        response.completion_calls(),
        &[
            CompletionCall::new(0, first_call_usage, None),
            CompletionCall::new(1, second_call_usage, None)
        ]
    );

    let history = cell_conversation(&cell);
    assert_eq!(history.len(), 3);
    assert!(matches!(
        history.first(),
        Some(Message::User { content })
            if matches!(
                content.first(),
                UserContent::Text(text) if text.text == "do tool work"
            )
    ));
    assert!(history.iter().any(|message| matches!(
        message,
        Message::Assistant { content, .. }
            if matches!(
                content.first(),
                AssistantContent::ToolCall(tool_call)
                    if tool_call.id == "tool_call_1"
                        && tool_call.call_id.as_deref() == Some("call_1")
            )
    )));
    assert!(history.iter().any(|message| matches!(
        message,
        Message::User { content }
            if matches!(
                content.first(),
                UserContent::ToolResult(tool_result)
                    if tool_result.id == "tool_call_1"
                        && tool_result.call_id.as_deref() == Some("call_1")
            )
    )));
    assert!(!history.iter().any(|message| matches!(
        message,
        Message::Assistant { content, .. }
            if content.iter().any(|item| matches!(
                item,
                AssistantContent::Text(text) if text.text.is_empty()
            ))
    )));
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    validate_follow_up_tool_history(&requests[1]);
}

#[tokio::test]
async fn prompt_request_concatenates_text_blocks_without_inserted_newlines() {
    let model = stream_model([MockTurn::from_contents([
        AssistantContent::Text(Text::new("According to the document, ")),
        AssistantContent::Text(Text::new("the grass is green")),
        AssistantContent::Text(Text::new(" and the sky is blue.")),
    ])
    .expect("mock response should contain text blocks")]);
    let agent = AgentBuilder::new(model).build();

    let response = fold(agent.stream_prompt("answer with cited spans"))
        .await
        .expect("prompt should succeed")
        .output;

    assert_eq!(
        response,
        "According to the document, the grass is green and the sky is blue."
    );
}

#[tokio::test]
async fn prompt_request_preserves_metadata_only_text_turn_in_history() {
    let metadata = json!({
        "citations": [{
            "type": "web_search_result_location",
            "cited_text": "Claude Shannon was born in 1916.",
            "url": "https://example.com/shannon",
            "title": null,
            "encrypted_index": "encrypted-reference"
        }]
    });
    let model = stream_model([MockTurn::from_content(AssistantContent::Text(Text {
        text: String::new(),
        additional_params: Some(metadata.clone()),
    }))]);
    let agent = AgentBuilder::new(model).build();

    let cell = test_cell("answer with cited metadata");
    let response = fold(
        crate::agent::prompt_request::streaming::StreamingPromptRequest::from_agent_cell(
            &agent,
            cell.clone(),
        ),
    )
    .await
    .expect("metadata-only text turn should succeed");

    assert!(response.output.is_empty());
    let history = cell_conversation(&cell);
    assert!(history.iter().any(|message| matches!(
        message,
        Message::Assistant { content, .. }
            if matches!(
                content.first(),
                AssistantContent::Text(text)
                    if text.text.is_empty()
                        && text.additional_params.as_ref() == Some(&metadata)
            )
    )));
}

fn test_cell(prompt: &str) -> tabit_log::ConversationCell {
    std::sync::Arc::new(std::sync::RwLock::new(tabit_log::ContextManager::seeded(
        vec![Message::user(prompt)],
    )))
}

fn cell_conversation(cell: &tabit_log::ConversationCell) -> Vec<Message> {
    tabit_log::lock::read(cell).messages()
}

fn static_document(id: &str, text: &str) -> crate::completion::Document {
    crate::completion::Document {
        id: id.to_string(),
        text: text.to_string(),
        additional_props: Default::default(),
    }
}

#[tokio::test]
async fn prompt_request_setters_reach_the_prepared_completion_request() {
    let model = stream_model([MockTurn::text("done"), MockTurn::text("done")]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).preamble("agent preamble").build();

    let mut params = serde_json::Map::new();
    params.insert("merged".to_string(), json!(1));
    let out = fold(
        agent
            .stream_prompt("configured run")
            .preamble("request preamble")
            .document(static_document("d1", "doc one"))
            .documents([static_document("d2", "doc two")])
            .temperature(0.7)
            .max_tokens(128)
            .merge_additional_params(params)
            .tool_choice(ToolChoice::Auto)
            .max_turns(1),
    )
    .await
    .map(|response| response.output)
    .expect("run with request-level setters should succeed");

    assert_eq!(out, "done");
    let request = &recorded.requests()[0];
    // The request preamble is hoisted into a leading system message.
    assert!(request.chat_history.iter().any(
        |message| matches!(message, Message::System { content } if content == "request preamble")
    ));
    assert_eq!(request.documents.len(), 2);
    assert_eq!(request.temperature, Some(0.7));
    assert_eq!(request.max_tokens, Some(128));
    assert_eq!(request.additional_params, Some(json!({"merged": 1})));
    assert_eq!(request.tool_choice, Some(ToolChoice::Auto));

    // A later `replace_additional_params` swaps the whole map.
    let out = fold(
        agent
            .stream_prompt("replaced params run")
            .replace_additional_params(json!({"replaced": true}))
            .max_turns(1),
    )
    .await
    .map(|response| response.output)
    .expect("run with replaced params should succeed");
    assert_eq!(out, "done");
    let request = &recorded.requests()[1];
    assert_eq!(request.additional_params, Some(json!({"replaced": true})));
}

#[tokio::test]
async fn prompt_request_without_setters_clear_overrides_back_to_none() {
    let model = stream_model([MockTurn::text("done")]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).preamble("agent preamble").build();

    let out = fold(
        agent
            .stream_prompt("cleared run")
            .preamble("temporary")
            .without_preamble()
            .temperature(0.7)
            .without_temperature()
            .max_tokens(128)
            .without_max_tokens()
            .replace_additional_params(json!({"k": 1}))
            .without_additional_params()
            .tool_choice(ToolChoice::Auto)
            .without_tool_choice()
            .max_turns(1),
    )
    .await
    .map(|response| response.output)
    .expect("run with cleared setters should succeed");

    assert_eq!(out, "done");
    let request = &recorded.requests()[0];
    assert_eq!(request.preamble, None);
    assert_eq!(request.temperature, None);
    assert_eq!(request.max_tokens, None);
    assert_eq!(request.additional_params, None);
    assert_eq!(request.tool_choice, None);
}

#[tokio::test]
async fn prompt_request_using_model_value_swaps_the_run_model() {
    let model = stream_model([MockTurn::text("from agent model")]);
    let replacement = stream_model([MockTurn::text("from request model")]);
    let agent = AgentBuilder::new(model).build();

    let out = fold(
        agent
            .stream_prompt("swap the model")
            .using_model_value(replacement)
            .max_turns(1),
    )
    .await
    .map(|response| response.output)
    .expect("request-level model swap should run");

    assert_eq!(out, "from request model");
}

#[tokio::test]
async fn prompt_request_using_model_handle_swaps_the_run_model() {
    let model = stream_model([MockTurn::text("from agent model")]);
    let replacement = stream_model([MockTurn::text("from handle model")]);
    let agent = AgentBuilder::new(model).build();

    let out = fold(
        agent
            .stream_prompt("swap the model via handle")
            .using_model(crate::agent::ModelHandle::new(replacement))
            .max_turns(1),
    )
    .await
    .map(|response| response.output)
    .expect("request-level model-handle swap should run");

    assert_eq!(out, "from handle model");
}

#[test]
fn prompt_response_display_formats_the_output_text() {
    let response = PromptResponse::new("hello display", usage(1, 2));
    assert_eq!(format!("{response}"), "hello display");
}
