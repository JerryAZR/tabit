use crate::agent::{
    InvalidToolCallAction, InvalidToolCallContext, ObservationAction, StreamResponseFinish,
    TextDelta, ToolCall, ToolCallAction, ToolCallDelta,
};

use super::*;
use crate::agent::AgentBuilder;
use crate::agent::hook::{AgentHook, HookContext};
use crate::agent::prompt_request::{TOOL_NOT_EXECUTED_DUE_TO_INVALID_PEER, tool_result_output};
use crate::agent::run::AgentRunStep;
use crate::agent::run::streamed::merge_reasoning_blocks;
use crate::client::AgentClientExt;
use crate::completion::{CompletionRequest, Prompt, PromptError, ToolDefinition, Usage};
use crate::streaming::{StreamingPrompt, ToolCallDeltaContent};
use crate::test_utils::{
    AppendFailingMemory, FailingMemory, MockAddTool, MockBarrierTool, MockCompletionModel,
    MockContextProbeTool, MockStreamEvent, MockSubtractTool, MockToolError, MockTurn, SessionId,
};
use crate::tool::{Tool, ToolContext};
use futures::{StreamExt, TryStreamExt};
use rig_core::client::ProviderClient;
use rig_core::message::{
    AssistantContent, DocumentSourceKind, ImageMediaType, Message, ReasoningContent, ToolChoice,
    ToolResultContent, UserContent,
};
use rig_core::providers::anthropic;
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::field::{Field, Visit};
use tracing::{Id, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry, registry::LookupSpan};

fn reasoning(
    id: Option<&str>,
    content: impl IntoIterator<Item = ReasoningContent>,
) -> rig_core::message::Reasoning {
    let mut reasoning = rig_core::message::Reasoning::new("");
    reasoning.id = id.map(str::to_string);
    reasoning.content = content.into_iter().collect();
    reasoning
}

struct StopAgentStreamingBeforeCompletion;

impl AgentHook for StopAgentStreamingBeforeCompletion {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        _event: crate::agent::CompletionCallEvent<'_>,
    ) -> crate::agent::CompletionCallAction {
        crate::agent::CompletionCallAction::stop("agent streaming stopped")
    }
}

#[tokio::test]
async fn public_streaming_request_constructor_preserves_agent_hooks() {
    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("should not run"),
        MockStreamEvent::final_response(Usage::new()),
    ]]);
    let agent = Arc::new(
        AgentBuilder::new(model.clone())
            .add_hook(StopAgentStreamingBeforeCompletion)
            .build(),
    );

    let mut stream = StreamingPromptRequest::new(agent, "go").await;
    let error = stream
        .try_next()
        .await
        .expect_err("the configured agent hook should terminate the stream");

    assert!(matches!(
        error,
        StreamingError::Prompt(error)
            if matches!(*error, PromptError::PromptCancelled { ref reason, .. }
                if reason == "agent streaming stopped")
    ));
    assert_eq!(model.request_count(), 0);
}

#[tokio::test]
async fn stream_chat_sends_the_whole_conversation_with_the_final_message_as_the_turn() {
    use crate::streaming::StreamingChat;

    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("done"),
        MockStreamEvent::final_response(Usage::new()),
    ]]);
    let agent = Arc::new(AgentBuilder::new(model.clone()).build());

    let conversation = vec![
        Message::user("first"),
        Message::user("second"),
        Message::user("third"),
    ];
    let mut stream = agent.stream_chat(conversation).await;
    let mut finished = false;
    while let Some(item) = stream.next().await {
        if let Ok(crate::agent::MultiTurnStreamItem::FinalResponse(_)) = item {
            finished = true;
        }
    }
    assert!(finished, "the run completed");

    // Every conversation message went out, in order, none duplicated: the
    // final message is the turn's prompt (the request's own rule), the
    // rest precede it.
    assert_eq!(model.request_count(), 1);
    let history = &model.requests()[0].chat_history;
    let texts: Vec<&str> = history
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => content.iter().find_map(|part| match part {
                UserContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["first", "second", "third"]);
}

#[tokio::test]
async fn an_empty_conversation_fails_loudly_at_send() {
    use crate::streaming::StreamingChat;

    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("should not run"),
        MockStreamEvent::final_response(Usage::new()),
    ]]);
    let agent = Arc::new(AgentBuilder::new(model.clone()).build());

    let mut stream = agent.stream_chat(Vec::<Message>::new()).await;
    let error = stream
        .try_next()
        .await
        .expect_err("an empty conversation cannot be sent");
    assert!(
        error.to_string().contains("empty conversation"),
        "got: {error}"
    );
    assert_eq!(
        model.request_count(),
        0,
        "no model call is made for an empty conversation"
    );
}

#[tokio::test]
async fn text_only_stream_without_terminal_record_is_rejected_as_truncated() {
    let model = MockCompletionModel::from_stream_turns([[MockStreamEvent::text("partial answer")]]);
    let agent = Arc::new(AgentBuilder::new(model.clone()).build());

    let mut stream = StreamingPromptRequest::new(agent, "go").await;
    let mut saw_error = false;
    while let Some(item) = stream.next().await {
        if let Err(error) = item {
            assert!(
                error.to_string().contains("terminal record"),
                "truncation should surface as a terminal-record error, got: {error}"
            );
            saw_error = true;
            break;
        }
    }
    assert!(
        saw_error,
        "a stream ending without a terminal record must be rejected, not \
             treated as a successful completion"
    );
}

#[tokio::test]
async fn tool_call_stream_without_terminal_record_dispatches_no_tools() {
    let calls = Arc::new(AtomicU32::new(0));
    let add_tool = CountingAddTool {
        calls: calls.clone(),
    };
    let model = MockCompletionModel::from_stream_turns([[MockStreamEvent::tool_call(
        "tool_call_1",
        "add",
        serde_json::json!({"x": 1, "y": 2}),
    )]]);
    let agent = AgentBuilder::new(model.clone()).tool(add_tool).build();

    let mut stream = agent.stream_prompt("go").max_turns(3).await;
    let mut saw_error = false;
    while let Some(item) = stream.next().await {
        if item.is_err() {
            saw_error = true;
            break;
        }
    }
    assert!(
        saw_error,
        "a truncated tool-call turn must error rather than complete"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a tool call from a stream the provider never confirmed complete \
             must not be dispatched"
    );
}

#[test]
fn finalize_streamed_choice_surfaces_output_over_tool_call_and_prose() {
    use rig_core::message::{ToolCall, ToolFunction};

    let output_call = AssistantContent::ToolCall(ToolCall::new(
        "c1".to_string(),
        ToolFunction::new(
            "final_result".to_string(),
            serde_json::json!({"city": "Tokyo"}),
        ),
    ));

    // Prose + output-tool call (#1928): the streamed response text must be
    // the structured output, not the prose, with no orphan tool_use.
    let with_prose = OneOrMany::many(vec![
        AssistantContent::text("Sure, here is the weather:"),
        output_call.clone(),
    ])
    .expect("two items");
    let final_choice = finalize_streamed_choice(&with_prose, r#"{"city":"Tokyo"}"#)
        .expect("a turn with the output-tool call is finalized via it");
    assert_eq!(
        assistant_text_from_choice(&final_choice),
        r#"{"city":"Tokyo"}"#
    );
    assert!(
        !final_choice
            .iter()
            .any(|item| matches!(item, AssistantContent::ToolCall(_))),
        "no unanswered tool_use should remain in the final content"
    );

    // Output-tool call only.
    let only_call = OneOrMany::one(output_call);
    let final_choice = finalize_streamed_choice(&only_call, r#"{"city":"Tokyo"}"#)
        .expect("finalized via output tool");
    assert_eq!(
        assistant_text_from_choice(&final_choice),
        r#"{"city":"Tokyo"}"#
    );

    // A plain-text finalize (no tool call) is left to the caller.
    let text_only = OneOrMany::one(AssistantContent::text(r#"{"city":"Tokyo"}"#));
    assert!(finalize_streamed_choice(&text_only, r#"{"city":"Tokyo"}"#).is_none());
}

#[test]
fn merge_reasoning_blocks_preserves_order_and_signatures() {
    let mut accumulated = Vec::new();
    let first = reasoning(
        Some("rs_1"),
        [ReasoningContent::Text {
            text: "step-1".to_string(),
            signature: Some("sig-1".to_string()),
        }],
    );
    let second = reasoning(
        Some("rs_1"),
        [
            ReasoningContent::Text {
                text: "step-2".to_string(),
                signature: Some("sig-2".to_string()),
            },
            ReasoningContent::Summary("summary".to_string()),
        ],
    );

    merge_reasoning_blocks(&mut accumulated, &first);
    merge_reasoning_blocks(&mut accumulated, &second);

    assert_eq!(accumulated.len(), 1);
    let merged = accumulated.first().expect("expected accumulated reasoning");
    assert_eq!(merged.id.as_deref(), Some("rs_1"));
    assert_eq!(merged.content.len(), 3);
    assert!(matches!(
        merged.content.first(),
        Some(ReasoningContent::Text { text, signature: Some(sig) })
            if text == "step-1" && sig == "sig-1"
    ));
    assert!(matches!(
        merged.content.get(1),
        Some(ReasoningContent::Text { text, signature: Some(sig) })
            if text == "step-2" && sig == "sig-2"
    ));
}

#[test]
fn merge_reasoning_blocks_keeps_distinct_ids_as_separate_items() {
    let mut accumulated = vec![reasoning(
        Some("rs_a"),
        [ReasoningContent::Text {
            text: "step-1".to_string(),
            signature: None,
        }],
    )];
    let incoming = reasoning(
        Some("rs_b"),
        [ReasoningContent::Text {
            text: "step-2".to_string(),
            signature: None,
        }],
    );

    merge_reasoning_blocks(&mut accumulated, &incoming);
    assert_eq!(accumulated.len(), 2);
    assert_eq!(
        accumulated.first().and_then(|r| r.id.as_deref()),
        Some("rs_a")
    );
    assert_eq!(
        accumulated.get(1).and_then(|r| r.id.as_deref()),
        Some("rs_b")
    );
}

#[test]
fn merge_reasoning_blocks_keeps_none_ids_separate_items() {
    let mut accumulated = vec![reasoning(
        None,
        [ReasoningContent::Text {
            text: "first".to_string(),
            signature: None,
        }],
    )];
    let incoming = reasoning(
        None,
        [ReasoningContent::Text {
            text: "second".to_string(),
            signature: None,
        }],
    );

    merge_reasoning_blocks(&mut accumulated, &incoming);
    assert_eq!(accumulated.len(), 2);
    assert!(accumulated.first().is_some_and(|reasoning| {
        reasoning.id.is_none()
            && matches!(
                reasoning.content.first(),
                Some(ReasoningContent::Text { text, .. }) if text == "first"
            )
    }));
    assert!(accumulated.get(1).is_some_and(|reasoning| {
        reasoning.id.is_none()
            && matches!(
                reasoning.content.first(),
                Some(ReasoningContent::Text { text, .. }) if text == "second"
            )
    }));
}

#[test]
fn tool_result_output_preserves_multimodal_tool_output() {
    let instruction = serde_json::json!({
        "instruction": "Use the image part to answer."
    });
    let mut content = rig_core::OneOrMany::one(ToolResultContent::json(instruction.clone()));
    content.push(ToolResultContent::image_base64(
        "base64data==",
        Some(ImageMediaType::PNG),
        None,
    ));
    let user_content = tool_result_output(
        "tool_call_1".to_string(),
        Some("call_1".to_string()),
        crate::tool::ToolOutput::content(content),
    );

    let tool_result = match user_content {
        UserContent::ToolResult(tool_result) => tool_result,
        other => panic!("expected tool result content, got {other:?}"),
    };

    assert_eq!(tool_result.id, "tool_call_1");
    assert_eq!(tool_result.call_id.as_deref(), Some("call_1"));
    assert_eq!(tool_result.content.len(), 2);

    let mut items = tool_result.content.iter();
    match items.next() {
        Some(ToolResultContent::Json { value }) => {
            assert_eq!(value, &instruction);
        }
        other => panic!("expected structured JSON payload first, got {other:?}"),
    }

    match items.next() {
        Some(ToolResultContent::Image(image)) => {
            assert_eq!(image.media_type, Some(ImageMediaType::PNG));
            assert!(matches!(
                image.data,
                DocumentSourceKind::Base64(ref data) if data == "base64data=="
            ));
        }
        other => panic!("expected image payload second, got {other:?}"),
    }
}

fn validate_follow_up_tool_history(request: &CompletionRequest) -> Result<(), String> {
    let history = request.chat_history.iter().cloned().collect::<Vec<_>>();
    if history.len() != 3 {
        return Err(format!(
            "follow-up request should contain [original user prompt, assistant tool call, user tool result]: {history:?}"
        ));
    }

    if !matches!(
        history.first(),
        Some(Message::User { content })
            if matches!(
                content.first(),
                UserContent::Text(text) if text.text == "do tool work"
            )
    ) {
        return Err(format!(
            "follow-up request should begin with the original user prompt: {history:?}"
        ));
    }

    if !matches!(
        history.get(1),
        Some(Message::Assistant { content, .. })
            if matches!(
                content.first(),
                AssistantContent::ToolCall(tool_call)
                    if tool_call.id == "tool_call_1"
                        && tool_call.call_id.as_deref() == Some("call_1")
            )
    ) {
        return Err(format!(
            "follow-up request is missing the assistant tool call in position 2: {history:?}"
        ));
    }

    if !matches!(
        history.get(2),
        Some(Message::User { content })
            if matches!(
                content.first(),
                UserContent::ToolResult(tool_result)
                    if tool_result.id == "tool_call_1"
                        && tool_result.call_id.as_deref() == Some("call_1")
            )
    ) {
        return Err(format!(
            "follow-up request should end with the user tool result: {history:?}"
        ));
    }

    Ok(())
}

fn history_contains_tool_call(history: &[Message], tool_name: &str) -> bool {
    history.iter().any(|message| {
        matches!(
            message,
            Message::Assistant { content, .. }
                if content.iter().any(|item| matches!(
                    item,
                    AssistantContent::ToolCall(tool_call)
                        if tool_call.function.name == tool_name
                ))
        )
    })
}

fn history_contains_text(history: &[Message], expected: &str) -> bool {
    history.iter().any(|message| {
        matches!(
            message,
            Message::Assistant { content, .. }
                if content.iter().any(|item| matches!(
                    item,
                    AssistantContent::Text(text) if text.text == expected
                ))
        )
    })
}

fn assistant_reasoning_precedes_tool_call(
    history: &[Message],
    expected_reasoning: &str,
    tool_name: &str,
) -> bool {
    history.iter().any(|message| {
        let Message::Assistant { content, .. } = message else {
            return false;
        };

        let reasoning_index = content.iter().position(|item| {
            matches!(
                item,
                AssistantContent::Reasoning(reasoning)
                    if reasoning.content.iter().any(|content| matches!(
                        content,
                        ReasoningContent::Text { text, .. }
                            if text == expected_reasoning
                    ))
            )
        });
        let tool_index = content.iter().position(|item| {
            matches!(
                item,
                AssistantContent::ToolCall(tool_call)
                    if tool_call.function.name == tool_name
            )
        });

        matches!((reasoning_index, tool_index), (Some(reasoning), Some(tool)) if reasoning < tool)
    })
}

fn assistant_reasoning_precedes_text_and_tool_call(
    history: &[Message],
    expected_reasoning: &str,
    expected_text: &str,
    tool_name: &str,
) -> bool {
    history.iter().any(|message| {
        let Message::Assistant { content, .. } = message else {
            return false;
        };

        let reasoning_index = content.iter().position(|item| {
            matches!(
                item,
                AssistantContent::Reasoning(reasoning)
                    if reasoning.content.iter().any(|content| matches!(
                        content,
                        ReasoningContent::Text { text, .. }
                            if text == expected_reasoning
                    ))
            )
        });
        let text_index = content.iter().position(|item| {
            matches!(
                item,
                AssistantContent::Text(text) if text.text == expected_text
            )
        });
        let tool_index = content.iter().position(|item| {
            matches!(
                item,
                AssistantContent::ToolCall(tool_call)
                    if tool_call.function.name == tool_name
            )
        });

        matches!(
            (reasoning_index, text_index, tool_index),
            (Some(reasoning), Some(text), Some(tool))
                if reasoning < text && text < tool
        )
    })
}

#[derive(Clone)]
struct PanicOnUnknownToolHook;

impl AgentHook for PanicOnUnknownToolHook {
    async fn on_tool_call_delta(&self, _: &HookContext, _: ToolCallDelta<'_>) -> ObservationAction {
        panic!("unknown tool call delta should fail before delta hooks run")
    }
    async fn on_tool_call(&self, _: &HookContext, _: ToolCall<'_>) -> ToolCallAction {
        panic!("unknown tool call should fail before tool hooks run")
    }
    async fn on_stream_response_finish(
        &self,
        _: &HookContext,
        _: StreamResponseFinish<'_>,
    ) -> ObservationAction {
        panic!("unknown tool call should fail before stream finish hooks run")
    }
}

#[derive(Clone)]
struct CountingAddTool {
    calls: Arc<AtomicU32>,
}

#[derive(Clone)]
struct CountingSubtractTool {
    calls: Arc<AtomicU32>,
}

#[derive(Deserialize)]
struct CountingOperationArgs {
    x: i32,
    y: i32,
}

fn arithmetic_tool_definition(name: &str, description: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "x": {
                    "type": "number",
                    "description": "The first operand"
                },
                "y": {
                    "type": "number",
                    "description": "The second operand"
                }
            },
            "required": ["x", "y"],
        }),
    }
}

impl Tool for CountingAddTool {
    const NAME: &'static str = "add";
    type Error = MockToolError;
    type Args = CountingOperationArgs;
    type Output = i32;

    fn description(&self) -> String {
        "Add x and y together".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        arithmetic_tool_definition(Self::NAME, "Add x and y together").parameters
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(args.x + args.y)
    }
}

impl Tool for CountingSubtractTool {
    const NAME: &'static str = "subtract";
    type Error = MockToolError;
    type Args = CountingOperationArgs;
    type Output = i32;

    fn description(&self) -> String {
        "Subtract y from x".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        arithmetic_tool_definition(Self::NAME, "Subtract y from x").parameters
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(args.x - args.y)
    }
}

fn streaming_tool_then_text_model() -> MockCompletionModel {
    MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tool_call_1", "add", serde_json::json!({"x": 1, "y": 2}))
                .with_call_id("call_1"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ])
}

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

#[tokio::test]
async fn execution_commit_items_are_not_emitted_when_run_commit_fails() {
    let runner = AgentBuilder::new(MockCompletionModel::default())
        .build()
        .runner("go");
    let tool_snapshot = Arc::new(
        runner
            .tool_server_handle
            .snapshot_tool_defs(None)
            .await
            .expect("empty tool snapshot should build"),
    );

    let mut run = AgentRun::new("go").max_turns(2);
    assert!(matches!(
        run.next_step().expect("initial model step"),
        AgentRunStep::CallModel { .. }
    ));

    let tool_name = "missing".to_string();
    let advertised = BTreeSet::from([tool_name.clone()]);
    let turn = crate::agent::run::ModelTurn::new(
        None,
        OneOrMany::one(AssistantContent::ToolCall(
            rig_core::message::ToolCall::new(
                "expected_call".to_string(),
                rig_core::message::ToolFunction::new(tool_name, serde_json::json!({})),
            ),
        )),
        Usage::new(),
        advertised.clone(),
        advertised,
    );
    assert!(matches!(
        run.model_response(turn)
            .expect("tool turn should be accepted"),
        crate::agent::run::ModelTurnOutcome::Continue { .. }
    ));

    let mut calls = match run.next_step().expect("tool step") {
        AgentRunStep::CallTools { calls } => calls,
        other => panic!("expected tool step, got {other:?}"),
    };
    // Corrupt only the driver's copy so execution settles successfully but
    // `AgentRun` rejects the result before any commit-labelled item escapes.
    calls[0].tool_call.id = "mismatched_call".to_string();

    let hook_context = HookContext::new(true, None);
    hook_context.set_turn(1);
    let mut stream = drive_tool_calls(
        &runner,
        &hook_context,
        &mut run,
        calls,
        tool_snapshot,
        |span| span,
        true,
    );

    let mut saw_commit = false;
    let mut saw_result = false;
    let mut saw_error = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::ToolExecutionCommitted { .. }) => saw_commit = true,
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult { .. })) => {
                saw_result = true
            }
            Err(_) => saw_error = true,
            _ => {}
        }
    }

    assert!(
        saw_error,
        "the mismatched result must fail run-state commit"
    );
    assert!(!saw_commit, "a failed run-state commit cannot be announced");
    assert!(!saw_result, "an uncommitted result cannot be surfaced");
}

#[derive(Clone, Debug, Default)]
struct CapturedSpan {
    id: u64,
    name: String,
    parent_id: Option<u64>,
    fields: HashMap<String, u64>,
    string_fields: HashMap<String, String>,
    record_counts: HashMap<String, usize>,
}

#[derive(Clone, Default)]
struct CapturedSpans(Arc<Mutex<Vec<CapturedSpan>>>);

impl CapturedSpans {
    fn clear(&self) {
        if let Ok(mut spans) = self.0.lock() {
            spans.clear();
        }
    }

    fn insert(&self, id: &Id, name: &str, parent_id: Option<u64>) {
        let id = id.into_u64();
        if let Ok(mut spans) = self.0.lock() {
            spans.push(CapturedSpan {
                id,
                name: name.to_string(),
                parent_id,
                fields: HashMap::new(),
                string_fields: HashMap::new(),
                record_counts: HashMap::new(),
            });
        }
    }

    fn record(&self, id: &Id, fields: Vec<CapturedField>) {
        if let Ok(mut spans) = self.0.lock()
            && let Some(span) = spans.iter_mut().rev().find(|span| span.id == id.into_u64())
        {
            for field in fields {
                match field {
                    CapturedField::Number(name, value) => {
                        *span.record_counts.entry(name.clone()).or_insert(0) += 1;
                        span.fields.insert(name, value);
                    }
                    CapturedField::Text(name, value) => {
                        *span.record_counts.entry(name.clone()).or_insert(0) += 1;
                        span.fields.insert(name.clone(), 0);
                        span.string_fields.insert(name, value);
                    }
                }
            }
        }
    }

    fn record_strings(&self, id: &Id, fields: Vec<(String, String)>) {
        if let Ok(mut spans) = self.0.lock()
            && let Some(span) = spans.iter_mut().rev().find(|span| span.id == id.into_u64())
        {
            span.string_fields.extend(fields);
        }
    }

    fn snapshot(&self) -> Vec<CapturedSpan> {
        self.0.lock().map(|spans| spans.clone()).unwrap_or_default()
    }
}

struct SpanCaptureLayer {
    spans: CapturedSpans,
}

impl<S> Layer<S> for SpanCaptureLayer
where
    S: Subscriber,
    S: for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &tracing::span::Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let parent_id = attrs
            .parent()
            .map(Id::into_u64)
            .or_else(|| ctx.current_span().id().map(Id::into_u64));
        self.spans.insert(id, attrs.metadata().name(), parent_id);
        let mut string_fields = Vec::new();
        attrs.record(&mut SpanStringCaptureVisitor {
            fields: &mut string_fields,
        });
        self.spans.record_strings(id, string_fields);
    }

    fn on_record(&self, span: &Id, values: &tracing::span::Record<'_>, _ctx: Context<'_, S>) {
        let mut fields = Vec::new();
        values.record(&mut SpanFieldCaptureVisitor {
            fields: &mut fields,
        });
        self.spans.record(span, fields);
        let mut string_fields = Vec::new();
        values.record(&mut SpanStringCaptureVisitor {
            fields: &mut string_fields,
        });
        self.spans.record_strings(span, string_fields);
    }
}

enum CapturedField {
    Number(String, u64),
    Text(String, String),
}

struct SpanFieldCaptureVisitor<'a> {
    fields: &'a mut Vec<CapturedField>,
}

struct SpanStringCaptureVisitor<'a> {
    fields: &'a mut Vec<(String, String)>,
}

impl Visit for SpanStringCaptureVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .push((field.name().to_string(), format!("{value:?}")));
    }
}

impl Visit for SpanFieldCaptureVisitor<'_> {
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .push(CapturedField::Number(field.name().to_string(), value));
    }

    // Capture the *presence* of non-numeric fields (e.g. `gen_ai.completion`)
    // with a placeholder value so tests can assert whether they were recorded.
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.push(CapturedField::Text(
            field.name().to_string(),
            value.to_string(),
        ));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.push(CapturedField::Text(
            field.name().to_string(),
            format!("{value:?}"),
        ));
    }
}

async fn assert_stream_usage_recorded_on_chat_spans(
    agent: crate::agent::Agent,
    prompt: &str,
    max_turns: usize,
    expected_usages: &[Usage],
) {
    // Scoped-subscriber tests must not run concurrently; the warm-up
    // below explains the callsite-interest hazard this guards against.
    let _isolation = crate::test_utils::scoped_tracing_subscriber_guard().await;
    let spans = CapturedSpans::default();
    let subscriber = Registry::default().with(SpanCaptureLayer {
        spans: spans.clone(),
    });
    let _default = tracing::subscriber::set_default(subscriber);

    // Span callsites in the driver are shared with every other test in
    // this binary. The FIRST thread to hit a callsite caches its interest
    // from that thread's dispatcher (`Dispatchers::Rebuilder::JustOne`
    // consults `dispatcher::get_default`), so a parallel test without a
    // subscriber can permanently cache `Interest::never` for the very
    // spans this harness asserts on. Defend in two steps, both under the
    // isolation guard: (1) warm the whole driver path from THIS thread so
    // unregistered callsites first-register against this subscriber, then
    // (2) rebuild the interest cache to heal callsites a foreign thread
    // already poisoned.
    let warmup_model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("warmup"),
        MockStreamEvent::final_response(Usage::default()),
    ]]);
    let warmup_agent = crate::agent::AgentBuilder::new(warmup_model).build();
    let mut warmup_stream = warmup_agent.stream_prompt("warmup").max_turns(1).await;
    while let Some(item) = warmup_stream
        .try_next()
        .await
        .expect("warmup stream should not error")
    {
        if matches!(item, MultiTurnStreamItem::FinalResponse(_)) {
            break;
        }
    }
    tracing::callsite::rebuild_interest_cache();
    spans.clear();

    let empty_history: &[Message] = &[];
    // Declare the fields the guard protects so a regression (recording onto
    // a caller span) is actually observable, not silently a no-op.
    let outer_span = tracing::info_span!("outer", gen_ai.completion = tracing::field::Empty);

    async {
        let mut stream = agent
            .stream_prompt(prompt)
            .history(empty_history)
            .max_turns(max_turns)
            .await;

        while let Some(item) = stream.try_next().await.expect("stream should not error") {
            if matches!(item, MultiTurnStreamItem::FinalResponse(_)) {
                break;
            }
        }
    }
    .instrument(outer_span)
    .await;

    let span_snapshot = spans.snapshot();
    let outer_span_id = span_snapshot
        .iter()
        .find(|span| span.name == "outer")
        .map(|span| span.id)
        .expect("outer span should be captured");
    let chat_spans = span_snapshot
        .iter()
        .filter(|span| span.name == "chat_streaming")
        .collect::<Vec<_>>();

    assert_eq!(chat_spans.len(), expected_usages.len());
    assert!(
        span_snapshot.iter().all(|span| span.name != "invoke_agent"),
        "outer span path should not create invoke_agent"
    );

    for (chat_span, expected_usage) in chat_spans.into_iter().zip(expected_usages) {
        assert_eq!(chat_span.parent_id, Some(outer_span_id));
        assert_eq!(
            chat_span
                .string_fields
                .get("gen_ai.operation.name")
                .map(String::as_str),
            Some("chat")
        );
        assert_eq!(
            chat_span.fields.get("gen_ai.usage.input_tokens"),
            Some(&expected_usage.input_tokens)
        );
        assert_eq!(
            chat_span.fields.get("gen_ai.usage.output_tokens"),
            Some(&expected_usage.output_tokens)
        );
        assert_eq!(
            chat_span.fields.get("gen_ai.usage.cache_read.input_tokens"),
            Some(&expected_usage.cached_input_tokens)
        );
        assert_eq!(
            chat_span
                .fields
                .get("gen_ai.usage.cache_creation.input_tokens"),
            Some(&expected_usage.cache_creation_input_tokens)
        );
        assert_eq!(
            chat_span.fields.get("gen_ai.usage.tool_use_prompt_tokens"),
            Some(&expected_usage.tool_use_prompt_tokens)
        );
        assert_eq!(
            chat_span.fields.get("gen_ai.usage.reasoning_tokens"),
            Some(&expected_usage.reasoning_tokens)
        );
    }

    let outer_span = span_snapshot
        .iter()
        .find(|span| span.id == outer_span_id)
        .expect("outer span should be present");
    assert!(
        outer_span
            .fields
            .keys()
            .all(|field| !field.starts_with("gen_ai.usage.")),
        "usage should not be recorded onto the caller's outer span"
    );
    assert!(
        !outer_span.fields.contains_key("gen_ai.completion"),
        "gen_ai.completion should not be recorded onto the caller's outer span \
             (parity with the blocking driver)"
    );
}

async fn capture_stream_message_telemetry(
    record_telemetry_content: bool,
) -> (CapturedSpan, Vec<CompletionRequest>) {
    let _isolation = crate::test_utils::scoped_tracing_subscriber_guard().await;
    let spans = CapturedSpans::default();
    let subscriber = Registry::default().with(SpanCaptureLayer {
        spans: spans.clone(),
    });
    let _default = tracing::subscriber::set_default(subscriber);

    let warmup_model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("warmup"),
        MockStreamEvent::final_response(Usage::default()),
    ]]);
    let warmup_agent = crate::agent::AgentBuilder::new(warmup_model).build();
    let mut warmup_stream = warmup_agent.stream_prompt("warmup").max_turns(1).await;
    while let Some(item) = warmup_stream
        .try_next()
        .await
        .expect("warmup stream should not error")
    {
        if matches!(item, MultiTurnStreamItem::FinalResponse(_)) {
            break;
        }
    }
    tracing::callsite::rebuild_interest_cache();
    spans.clear();

    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("stream response secret"),
        MockStreamEvent::final_response(Usage::default()),
    ]]);
    let recorded_model = model.clone();
    let builder = AgentBuilder::new(model);
    let agent = if record_telemetry_content {
        builder
            .record_content_telemetry(true)
            .context("static stream context secret")
            .build()
    } else {
        builder.context("static stream context secret").build()
    };

    let mut stream = agent
        .stream_prompt("stream prompt secret")
        .max_turns(1)
        .await;
    while let Some(item) = stream.try_next().await.expect("stream should not error") {
        if matches!(item, MultiTurnStreamItem::FinalResponse(_)) {
            break;
        }
    }

    let span = spans
        .snapshot()
        .into_iter()
        .find(|span| span.name == "chat_streaming")
        .expect("chat_streaming span should be captured");
    (span, recorded_model.requests())
}

async fn capture_unary_message_telemetry(
    record_telemetry_content: bool,
) -> (CapturedSpan, CapturedSpan, Vec<CompletionRequest>) {
    let _isolation = crate::test_utils::scoped_tracing_subscriber_guard().await;
    let spans = CapturedSpans::default();
    let subscriber = Registry::default().with(SpanCaptureLayer {
        spans: spans.clone(),
    });
    let _default = tracing::subscriber::set_default(subscriber);

    let warmup_agent = crate::agent::AgentBuilder::new(MockCompletionModel::text("warmup")).build();
    warmup_agent
        .prompt("warmup")
        .await
        .expect("warmup prompt should not error");
    tracing::callsite::rebuild_interest_cache();
    spans.clear();

    let model = MockCompletionModel::text("blocking response secret");
    let recorded_model = model.clone();
    let builder = AgentBuilder::new(model).preamble("blocking system secret");
    let agent = if record_telemetry_content {
        builder.record_content_telemetry(true).build()
    } else {
        builder.build()
    };

    agent
        .prompt("blocking prompt secret")
        .await
        .expect("prompt should not error");

    let snapshot = spans.snapshot();
    let chat_span = snapshot
        .iter()
        .find(|span| span.name == "chat")
        .cloned()
        .expect("chat span should be captured");
    let agent_span = snapshot
        .into_iter()
        .find(|span| span.name == "invoke_agent")
        .expect("invoke_agent span should be captured");
    (chat_span, agent_span, recorded_model.requests())
}

#[tokio::test]
async fn stream_prompt_message_telemetry_is_opt_in() {
    let (default_span, default_requests) = capture_stream_message_telemetry(false).await;
    assert!(
        !default_span.fields.contains_key("gen_ai.input.messages"),
        "default streaming prompt should not record input message contents"
    );
    assert!(
        !default_span.fields.contains_key("gen_ai.output.messages"),
        "default streaming prompt should not record output message contents"
    );

    assert_eq!(default_requests.len(), 1);
    assert!(
        !default_requests[0].record_telemetry_content,
        "default agent stream should keep provider request message telemetry disabled"
    );

    let (opt_in_span, opt_in_requests) = capture_stream_message_telemetry(true).await;
    let input = opt_in_span
        .string_fields
        .get("gen_ai.input.messages")
        .expect("opt-in should record input messages");
    assert!(input.contains("stream prompt secret"));
    assert!(input.contains("static stream context secret"));
    let output = opt_in_span
        .string_fields
        .get("gen_ai.output.messages")
        .expect("opt-in should record output messages");
    assert!(output.contains("stream response secret"));
    assert_eq!(
        opt_in_span
            .record_counts
            .get("gen_ai.input.messages")
            .copied(),
        Some(1),
        "agent-owned input message telemetry should be recorded once"
    );
    assert_eq!(
        opt_in_span
            .record_counts
            .get("gen_ai.output.messages")
            .copied(),
        Some(1),
        "agent-owned output message telemetry should be recorded once"
    );
    assert_eq!(opt_in_requests.len(), 1);
    assert!(
        !opt_in_requests[0].record_telemetry_content,
        "agent-owned stream telemetry should clear the provider request flag"
    );
}

#[tokio::test]
async fn unary_prompt_message_telemetry_records_accepted_output_when_opted_in() {
    let (default_span, default_agent_span, default_requests) =
        capture_unary_message_telemetry(false).await;
    assert!(
        !default_span.fields.contains_key("gen_ai.input.messages"),
        "default blocking prompt should not record input message contents"
    );
    assert!(
        !default_span.fields.contains_key("gen_ai.output.messages"),
        "default blocking prompt should not record output message contents"
    );
    assert!(
        !default_span
            .string_fields
            .contains_key("gen_ai.system_instructions"),
        "default blocking prompt should not record system instructions"
    );
    assert!(
        !default_agent_span
            .string_fields
            .contains_key("gen_ai.prompt")
    );
    assert!(
        !default_agent_span
            .string_fields
            .contains_key("gen_ai.completion")
    );
    assert_eq!(default_requests.len(), 1);
    assert!(
        !default_requests[0].record_telemetry_content,
        "default blocking prompt should keep provider request message telemetry disabled"
    );

    let (opt_in_span, opt_in_agent_span, opt_in_requests) =
        capture_unary_message_telemetry(true).await;
    let input = opt_in_span
        .string_fields
        .get("gen_ai.input.messages")
        .expect("opt-in should record blocking input messages");
    assert!(input.contains("blocking prompt secret"));
    let output = opt_in_span
        .string_fields
        .get("gen_ai.output.messages")
        .expect("opt-in should record blocking output messages");
    assert!(output.contains("blocking response secret"));
    assert_eq!(
        opt_in_span
            .string_fields
            .get("gen_ai.system_instructions")
            .map(String::as_str),
        Some(r#"[{"type":"text","content":"blocking system secret"}]"#)
    );
    assert_eq!(
        opt_in_agent_span
            .string_fields
            .get("gen_ai.prompt")
            .map(String::as_str),
        Some("blocking prompt secret")
    );
    assert_eq!(
        opt_in_agent_span
            .string_fields
            .get("gen_ai.completion")
            .map(String::as_str),
        Some("blocking response secret")
    );
    assert_eq!(opt_in_requests.len(), 1);
    assert!(
        !opt_in_requests[0].record_telemetry_content,
        "agent-owned blocking telemetry should clear the provider request flag"
    );
}

async fn capture_tool_content_telemetry(record_telemetry_content: bool) -> CapturedSpan {
    let _isolation = crate::test_utils::scoped_tracing_subscriber_guard().await;
    let spans = CapturedSpans::default();
    let subscriber = Registry::default().with(SpanCaptureLayer {
        spans: spans.clone(),
    });
    let _default = tracing::subscriber::set_default(subscriber);

    let warmup = AgentBuilder::new(MockCompletionModel::from_turns([
        MockTurn::tool_call("warmup", "add", serde_json::json!({"x": 1, "y": 2})),
        MockTurn::text("done"),
    ]))
    .tool(MockAddTool)
    .build();
    warmup
        .runner("warmup")
        .max_turns(2)
        .run()
        .await
        .expect("warmup tool run should succeed");
    tracing::callsite::rebuild_interest_cache();
    spans.clear();

    let builder = AgentBuilder::new(MockCompletionModel::from_turns([
        MockTurn::tool_call(
            "secret-tool-call",
            "add",
            serde_json::json!({"x": 12345, "y": 67890}),
        ),
        MockTurn::text("done"),
    ]))
    .tool(MockAddTool);
    let agent = if record_telemetry_content {
        builder.record_content_telemetry(true).build()
    } else {
        builder.build()
    };
    agent
        .runner("use the tool")
        .max_turns(2)
        .run()
        .await
        .expect("tool run should succeed");

    spans
        .snapshot()
        .into_iter()
        .find(|span| span.name == "execute_tool")
        .expect("execute_tool span should be captured")
}

#[tokio::test]
async fn tool_arguments_and_results_follow_content_telemetry_toggle() {
    let default_span = capture_tool_content_telemetry(false).await;
    assert!(
        !default_span
            .string_fields
            .contains_key("gen_ai.tool.call.arguments")
    );
    assert!(
        !default_span
            .string_fields
            .contains_key("gen_ai.tool.call.result")
    );
    assert_eq!(
        default_span
            .string_fields
            .get("gen_ai.tool.name")
            .map(String::as_str),
        Some("add"),
        "structural tool metadata should remain available"
    );

    let opt_in_span = capture_tool_content_telemetry(true).await;
    assert!(
        opt_in_span
            .string_fields
            .get("gen_ai.tool.call.arguments")
            .is_some_and(|args| args.contains("12345") && args.contains("67890"))
    );
    assert!(
        opt_in_span
            .string_fields
            .get("gen_ai.tool.call.result")
            .is_some_and(|result| result.contains("80235"))
    );
}

#[tokio::test]
async fn streaming_rejected_message_telemetry_does_not_record_output() {
    let _isolation = crate::test_utils::scoped_tracing_subscriber_guard().await;
    let spans = CapturedSpans::default();
    let subscriber = Registry::default().with(SpanCaptureLayer {
        spans: spans.clone(),
    });
    let _default = tracing::subscriber::set_default(subscriber);

    let warmup_model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("warmup"),
        MockStreamEvent::final_response(Usage::default()),
    ]]);
    let warmup_agent = crate::agent::AgentBuilder::new(warmup_model).build();
    let mut warmup_stream = warmup_agent.stream_prompt("warmup").max_turns(1).await;
    while let Some(item) = warmup_stream
        .try_next()
        .await
        .expect("warmup stream should not error")
    {
        if matches!(item, MultiTurnStreamItem::FinalResponse(_)) {
            break;
        }
    }
    tracing::callsite::rebuild_interest_cache();
    spans.clear();

    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("rejected stream output secret"),
        MockStreamEvent::tool_call(
            "tool_call_1",
            "default_api",
            serde_json::json!({"x": 2, "y": 3}),
        ),
        MockStreamEvent::final_response(Usage::default()),
    ]]);
    let agent = AgentBuilder::new(model)
        .record_content_telemetry(true)
        .build();

    let mut stream = agent
        .stream_prompt("stream rejection prompt")
        .max_turns(1)
        .await;
    let err = loop {
        match stream.try_next().await {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("rejected stream should error"),
            Err(err) => break err,
        }
    };
    assert!(
        err.to_string().contains("default_api"),
        "expected invalid tool error, got {err}"
    );

    let chat_span = spans
        .snapshot()
        .into_iter()
        .find(|span| span.name == "chat_streaming")
        .expect("chat_streaming span should be captured");
    assert!(
        chat_span.fields.contains_key("gen_ai.input.messages"),
        "opt-in rejected stream should still record input messages"
    );
    assert!(
        !chat_span.fields.contains_key("gen_ai.output.messages"),
        "rejected streaming turn must not record output message contents"
    );
}

#[tokio::test]
async fn unary_repaired_message_telemetry_records_canonical_output() {
    let _isolation = crate::test_utils::scoped_tracing_subscriber_guard().await;
    let spans = CapturedSpans::default();
    let subscriber = Registry::default().with(SpanCaptureLayer {
        spans: spans.clone(),
    });
    let _default = tracing::subscriber::set_default(subscriber);

    let warmup_agent = crate::agent::AgentBuilder::new(MockCompletionModel::text("warmup")).build();
    warmup_agent
        .prompt("warmup")
        .await
        .expect("warmup prompt should not error");
    tracing::callsite::rebuild_interest_cache();
    spans.clear();

    let model = MockCompletionModel::new([
        MockTurn::tool_call(
            "tool_call_1",
            "default_api",
            serde_json::json!({"x": 2, "y": 3}),
        ),
        MockTurn::text("done"),
    ]);
    let recorded_model = model.clone();
    let agent = AgentBuilder::new(model)
        .record_content_telemetry(true)
        .tool(MockAddTool)
        .build();

    let output = agent
        .prompt("repair tool call")
        .add_hook(RepairDefaultApiHook)
        .max_turns(3)
        .await
        .expect("repaired tool call should complete");
    assert_eq!(output, "done");

    let output_messages: Vec<String> = spans
        .snapshot()
        .into_iter()
        .filter(|span| span.name == "chat")
        .filter_map(|span| span.string_fields.get("gen_ai.output.messages").cloned())
        .collect();
    assert!(
        output_messages.iter().any(|output| output.contains("add")),
        "repaired accepted output should include canonical tool name: {output_messages:?}"
    );
    assert!(
        !output_messages
            .iter()
            .any(|output| output.contains("default_api")),
        "repaired output telemetry must not serialize stale raw tool name: {output_messages:?}"
    );

    let requests = recorded_model.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| !request.record_telemetry_content),
        "agent-owned repaired telemetry should clear provider request flags"
    );
}

#[test]
fn completion_calls_stream_item_serializes_and_deserializes_expected_shape() {
    let item: MultiTurnStreamItem =
        MultiTurnStreamItem::CompletionCall(CompletionCall::new(2, usage(3, 4)));

    let value = serde_json::to_value(&item).expect("serialize completion call event");

    assert_eq!(
        value,
        serde_json::json!({
            "type": "completionCall",
            "call_index": 2,
            "usage": {
                "input_tokens": 3,
                "output_tokens": 4,
                "total_tokens": 7,
                "cached_input_tokens": 0,
                "cache_creation_input_tokens": 0,"cache_creation_1h_input_tokens": 0,
                "tool_use_prompt_tokens": 0,
                "reasoning_tokens": 0,
            }
        })
    );

    let item: MultiTurnStreamItem =
        serde_json::from_value(value).expect("deserialize completion call event");
    match item {
        MultiTurnStreamItem::CompletionCall(call_usage) => {
            assert_eq!(call_usage, CompletionCall::new(2, usage(3, 4)));
        }
        other => panic!("expected completion call event, got {other:?}"),
    }

    let item: MultiTurnStreamItem =
        MultiTurnStreamItem::CompletionCall(CompletionCall::new(3, Usage::new()));
    let value = serde_json::to_value(&item).expect("serialize missing usage event");

    // Unreported usage serializes as a plain zero-valued object (Usage's
    // documented sentinel for missing provider metrics).
    assert_eq!(
        value,
        serde_json::json!({
            "type": "completionCall",
            "call_index": 3,
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "total_tokens": 0,
                "cached_input_tokens": 0,
                "cache_creation_input_tokens": 0,"cache_creation_1h_input_tokens": 0,
                "tool_use_prompt_tokens": 0,
                "reasoning_tokens": 0,
            }
        })
    );

    // Stream items serialized before the Option encoding was dropped used
    // `"usage": null`; they must still deserialize.
    let legacy: MultiTurnStreamItem = serde_json::from_value(serde_json::json!({
        "type": "completionCall",
        "call_index": 3,
        "usage": null
    }))
    .expect("legacy null-usage event should deserialize");
    match legacy {
        MultiTurnStreamItem::CompletionCall(call) => {
            assert_eq!(call, CompletionCall::new(3, Usage::new()));
        }
        other => panic!("expected completion call event, got {other:?}"),
    }
}

#[test]
fn final_response_serializes_completion_calls_with_missing_usage() {
    let item: MultiTurnStreamItem = MultiTurnStreamItem::final_response_with_completion_calls(
        OneOrMany::one(AssistantContent::text("done")),
        usage(3, 4),
        vec![
            CompletionCall::new(0, Usage::new()),
            CompletionCall::new(1, usage(3, 4)),
        ],
        None,
    );

    if let MultiTurnStreamItem::FinalResponse(response) = &item {
        assert_eq!(response.requests(), 2);
    }

    let value = serde_json::to_value(&item).expect("serialize final response");

    assert_eq!(
        value.get("completion_calls"),
        Some(&serde_json::json!([
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
}

fn streaming_text_then_final_model() -> MockCompletionModel {
    MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("hello"),
        MockStreamEvent::text(" world"),
        MockStreamEvent::final_response_with_total_tokens(3),
    ]])
}

fn citation_metadata() -> serde_json::Value {
    serde_json::json!({
        "citations": [{
            "type": "web_search_result_location",
            "cited_text": "Claude Shannon was born in 1916.",
            "url": "https://example.com/shannon",
            "title": "Claude Shannon",
            "encrypted_index": "encrypted-reference"
        }]
    })
}

fn streaming_cited_text_then_final_model() -> MockCompletionModel {
    MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text_start("block-0", Some(citation_metadata())),
        MockStreamEvent::text("cited "),
        MockStreamEvent::text_start("block-1", None),
        MockStreamEvent::text("answer"),
        MockStreamEvent::final_response_with_total_tokens(3),
    ]])
}

fn streaming_cited_text_then_tool_model() -> MockCompletionModel {
    MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text_start("block-0", Some(citation_metadata())),
            MockStreamEvent::text("I need a tool. "),
            MockStreamEvent::tool_call("tool_call_1", "add", serde_json::json!({"x": 1, "y": 2}))
                .with_call_id("call_1"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ])
}

fn streaming_final_only_model() -> MockCompletionModel {
    MockCompletionModel::from_stream_turns([[MockStreamEvent::final_response_with_total_tokens(1)]])
}

#[derive(Clone)]
struct TerminateOnStreamFinish;

impl AgentHook for TerminateOnStreamFinish {
    async fn on_stream_response_finish(
        &self,
        _ctx: &HookContext,
        event: StreamResponseFinish<'_>,
    ) -> ObservationAction {
        match event {
            StreamResponseFinish { .. } => ObservationAction::stop("stop after completion call"),
            _ => ObservationAction::continue_run(),
        }
    }
}

type RecordedToolCallDelta = (String, String, Option<String>, String);

#[derive(Clone)]
struct RepairDefaultApiHook;

impl AgentHook for RepairDefaultApiHook {
    async fn on_invalid_tool_call(
        &self,
        _ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        Some(match event {
            context => {
                assert_eq!(context.tool_name, "default_api");
                InvalidToolCallAction::repair("add")
            }
            _ => InvalidToolCallAction::fail(),
        })
    }
}

#[derive(Clone)]
struct RetryDefaultApiHook;

impl AgentHook for RetryDefaultApiHook {
    async fn on_invalid_tool_call(
        &self,
        _ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        Some(match event {
            context => {
                assert_eq!(context.tool_name, "default_api");
                if let Some(args) = context.args.as_deref() {
                    assert!(!args.is_empty());
                }
                InvalidToolCallAction::retry("Use the add tool instead")
            }
            _ => InvalidToolCallAction::fail(),
        })
    }
}

#[derive(Clone)]
struct SkipDefaultApiHook;

impl AgentHook for SkipDefaultApiHook {
    async fn on_invalid_tool_call(
        &self,
        _ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        Some(match event {
            context => {
                assert_eq!(context.tool_name, "default_api");
                InvalidToolCallAction::skip("default_api was skipped")
            }
            _ => InvalidToolCallAction::fail(),
        })
    }
}

#[derive(Clone, Default)]
struct RecordingInvalidToolCallHook {
    contexts: Arc<Mutex<Vec<InvalidToolCallContext>>>,
}

impl RecordingInvalidToolCallHook {
    fn observed(&self) -> Vec<InvalidToolCallContext> {
        self.contexts
            .lock()
            .expect("invalid tool context records mutex was poisoned")
            .clone()
    }
}

impl AgentHook for RecordingInvalidToolCallHook {
    async fn on_invalid_tool_call(
        &self,
        _ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        Some(match event {
            context => {
                self.contexts
                    .lock()
                    .expect("invalid tool context records mutex was poisoned")
                    .push(context.clone());
                InvalidToolCallAction::fail()
            }
            _ => InvalidToolCallAction::fail(),
        })
    }
}

#[derive(Clone, Default)]
struct RecordingToolCallDeltaHook {
    deltas: Arc<Mutex<Vec<RecordedToolCallDelta>>>,
}

impl RecordingToolCallDeltaHook {
    fn observed(&self) -> Vec<RecordedToolCallDelta> {
        self.deltas
            .lock()
            .expect("tool call delta hook records mutex was poisoned")
            .clone()
    }
}

impl AgentHook for RecordingToolCallDeltaHook {
    async fn on_tool_call_delta(
        &self,
        _ctx: &HookContext,
        event: ToolCallDelta<'_>,
    ) -> ObservationAction {
        match event {
            ToolCallDelta {
                tool_call_id,
                internal_call_id,
                tool_name,
                delta,
            } => {
                let record = (
                    tool_call_id.to_string(),
                    internal_call_id.to_string(),
                    tool_name.map(str::to_string),
                    delta.to_string(),
                );
                self.deltas
                    .lock()
                    .expect("tool call delta hook records mutex was poisoned")
                    .push(record);
                ObservationAction::continue_run()
            }
            _ => ObservationAction::continue_run(),
        }
    }
}

#[derive(Clone, Default)]
struct RecordingTextDeltaHook {
    deltas: Arc<Mutex<Vec<(String, String)>>>,
}

impl RecordingTextDeltaHook {
    fn observed(&self) -> Vec<(String, String)> {
        self.deltas
            .lock()
            .expect("text delta hook records mutex was poisoned")
            .clone()
    }
}

impl AgentHook for RecordingTextDeltaHook {
    async fn on_text_delta(&self, _ctx: &HookContext, event: TextDelta<'_>) -> ObservationAction {
        match event {
            TextDelta { delta, aggregated } => {
                let record = (delta.to_string(), aggregated.to_string());
                self.deltas
                    .lock()
                    .expect("text delta hook records mutex was poisoned")
                    .push(record);
                ObservationAction::continue_run()
            }
            _ => ObservationAction::continue_run(),
        }
    }
}

#[derive(Clone)]
struct RecordingTextAndSkipInvalidToolHook {
    text: RecordingTextDeltaHook,
}

impl AgentHook for RecordingTextAndSkipInvalidToolHook {
    async fn on_text_delta(&self, ctx: &HookContext, event: TextDelta<'_>) -> ObservationAction {
        self.text.on_text_delta(ctx, event).await
    }
    async fn on_invalid_tool_call(
        &self,
        ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        SkipDefaultApiHook.on_invalid_tool_call(ctx, event).await
    }
}

#[derive(Clone)]
struct RecordingTextAndRetryInvalidToolHook {
    text: RecordingTextDeltaHook,
}

impl AgentHook for RecordingTextAndRetryInvalidToolHook {
    async fn on_text_delta(&self, ctx: &HookContext, event: TextDelta<'_>) -> ObservationAction {
        self.text.on_text_delta(ctx, event).await
    }
    async fn on_invalid_tool_call(
        &self,
        ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        RetryDefaultApiHook.on_invalid_tool_call(ctx, event).await
    }
}

#[derive(Clone)]
struct RecordingDeltaAndRetryInvalidToolHook {
    delta: RecordingToolCallDeltaHook,
}

impl AgentHook for RecordingDeltaAndRetryInvalidToolHook {
    async fn on_tool_call_delta(
        &self,
        ctx: &HookContext,
        event: ToolCallDelta<'_>,
    ) -> ObservationAction {
        self.delta.on_tool_call_delta(ctx, event).await
    }
    async fn on_invalid_tool_call(
        &self,
        ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        RetryDefaultApiHook.on_invalid_tool_call(ctx, event).await
    }
}

#[derive(Clone)]
struct RecordingDeltaAndSkipInvalidToolHook {
    delta: RecordingToolCallDeltaHook,
}

impl AgentHook for RecordingDeltaAndSkipInvalidToolHook {
    async fn on_tool_call_delta(
        &self,
        ctx: &HookContext,
        event: ToolCallDelta<'_>,
    ) -> ObservationAction {
        self.delta.on_tool_call_delta(ctx, event).await
    }
    async fn on_invalid_tool_call(
        &self,
        ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        SkipDefaultApiHook.on_invalid_tool_call(ctx, event).await
    }
}

#[derive(Clone, Default)]
struct TerminatingToolCallDeltaHook {
    deltas: Arc<Mutex<Vec<RecordedToolCallDelta>>>,
}

impl TerminatingToolCallDeltaHook {
    fn observed(&self) -> Vec<RecordedToolCallDelta> {
        self.deltas
            .lock()
            .expect("tool call delta hook records mutex was poisoned")
            .clone()
    }
}

impl AgentHook for TerminatingToolCallDeltaHook {
    async fn on_tool_call_delta(
        &self,
        _ctx: &HookContext,
        event: ToolCallDelta<'_>,
    ) -> ObservationAction {
        match event {
            ToolCallDelta {
                tool_call_id,
                internal_call_id,
                tool_name,
                delta,
            } => {
                let record = (
                    tool_call_id.to_string(),
                    internal_call_id.to_string(),
                    tool_name.map(str::to_string),
                    delta.to_string(),
                );
                self.deltas
                    .lock()
                    .expect("tool call delta hook records mutex was poisoned")
                    .push(record);
                ObservationAction::stop("stop on tool call delta")
            }
            _ => ObservationAction::continue_run(),
        }
    }
}

fn text_metadata(content: &OneOrMany<AssistantContent>) -> Option<&serde_json::Value> {
    content.iter().find_map(|item| match item {
        AssistantContent::Text(text) => text.additional_params.as_ref(),
        _ => None,
    })
}

#[tokio::test]
async fn stream_prompt_continues_after_tool_call_turn() {
    let model = streaming_tool_then_text_model();
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();
    let empty_history: &[Message] = &[];

    let mut stream = agent
        .stream_prompt("do tool work")
        .history(empty_history)
        .max_turns(3)
        .await;
    let mut saw_tool_call = false;
    let mut saw_tool_result = false;
    let mut saw_final_response = false;
    let mut final_text = String::new();
    let mut final_response_text = None;
    let mut final_history = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                ..
            })) => {
                saw_tool_call = true;
            }
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult { .. })) => {
                saw_tool_result = true;
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                final_text.push_str(&text.text);
            }
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                saw_final_response = true;
                final_response_text = Some(res.output().to_owned());
                final_history = res.messages().map(|history| history.to_vec());
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    assert!(saw_tool_call);
    assert!(saw_tool_result);
    assert!(saw_final_response);
    assert_eq!(final_text, "done");
    assert_eq!(final_response_text.as_deref(), Some("done"));
    let history = final_history.expect("expected final response history");
    assert!(history.iter().any(|message| matches!(
        message,
        Message::Assistant { content, .. }
            if content.iter().any(|item| matches!(
                item,
                AssistantContent::Text(text) if text.text == "done"
            ))
    )));
    let requests = recorded.requests();
    assert_eq!(requests.len(), 2);
    assert!(validate_follow_up_tool_history(&requests[1]).is_ok());
}

/// `StreamingPromptRequest::tool_concurrency` reaches the runner: two
/// barrier-synchronized tools in a streamed turn only finish if they run
/// concurrently. At `tool_concurrency(2)` the stream completes; sequential
/// execution would block on the first tool forever, so the timeout asserts
/// the public builder actually enables concurrency on the streaming path.
#[tokio::test]
async fn streaming_prompt_request_tool_concurrency_runs_tools_concurrently() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("b1", "barrier_tool", serde_json::json!({})),
            MockStreamEvent::tool_call("b2", "barrier_tool", serde_json::json!({})),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
    ]);
    let agent = AgentBuilder::new(model)
        .tool(MockBarrierTool::new(barrier))
        .build();

    let drive = async {
        let mut stream = agent
            .stream_prompt("hit the barrier twice")
            .max_turns(3)
            .tool_concurrency(2)
            .await;
        while let Some(item) = stream.next().await {
            item.unwrap_or_else(|err| panic!("unexpected streaming error: {err:?}"));
        }
    };

    tokio::time::timeout(Duration::from_secs(5), drive)
        .await
        .expect("streamed tools must run concurrently, not deadlock at the barrier");
}

/// The streaming driver threads the per-call `ToolContext` to executed
/// tools, exactly like the blocking path.
#[tokio::test]
async fn tool_context_reaches_tool_through_streaming_loop() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tool_call_1", "context_probe", serde_json::json!({}))
                .with_call_id("call_1"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let probe = MockContextProbeTool::default();
    let agent = AgentBuilder::new(model).tool(probe.clone()).build();
    let empty_history: &[Message] = &[];

    let mut tool_context = ToolContext::new();
    tool_context.insert(SessionId("xyz-789".to_string()));

    let mut stream = agent
        .stream_prompt("do tool work")
        .tool_context(tool_context)
        .history(empty_history)
        .max_turns(3)
        .await;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::FinalResponse(_)) => break,
            Err(err) => panic!("unexpected streaming error: {err:?}"),
            Ok(_) => {}
        }
    }

    assert_eq!(probe.observed().as_deref(), Some("session:xyz-789"));
}

/// Streaming counterpart of the blocking empty-context default: when no
/// [`ToolContext`] is supplied, the tool still receives a fresh empty
/// context (observing `no-session`), not a stale value.
#[tokio::test]
async fn streaming_tool_runs_with_empty_context_when_none_supplied() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tool_call_1", "context_probe", serde_json::json!({}))
                .with_call_id("call_1"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let probe = MockContextProbeTool::default();
    let agent = AgentBuilder::new(model).tool(probe.clone()).build();
    let empty_history: &[Message] = &[];

    let mut stream = agent
        .stream_prompt("do tool work")
        .history(empty_history)
        .max_turns(3)
        .await;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::FinalResponse(_)) => break,
            Err(err) => panic!("unexpected streaming error: {err:?}"),
            Ok(_) => {}
        }
    }

    assert_eq!(probe.observed().as_deref(), Some("no-session"));
}

#[tokio::test]
async fn unknown_tool_call_fails_before_streaming_second_request() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call(
                "tool_call_1",
                "default_api",
                serde_json::json!({"x": 1, "y": 2}),
            ),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(PanicOnUnknownToolHook)
        .max_turns(3)
        .await;
    let mut saw_tool_call = false;
    let mut error = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                ..
            })) => {
                saw_tool_call = true;
            }
            Ok(_) => {}
            Err(err) => {
                error = Some(err);
                break;
            }
        }
    }

    assert!(!saw_tool_call);
    let error = error.expect("unknown model-emitted tool should fail");
    match error {
        StreamingError::Prompt(err) => match *err {
            PromptError::UnknownToolCall {
                tool_name,
                available_tools,
                allowed_tools,
                chat_history,
            } => {
                assert_eq!(tool_name, "default_api");
                assert_eq!(available_tools, vec!["add".to_string()]);
                assert_eq!(allowed_tools, vec!["add".to_string()]);
                assert!(history_contains_tool_call(&chat_history, "default_api"));
            }
            other => panic!("expected UnknownToolCall, got {other:?}"),
        },
        other => panic!("expected prompt streaming error, got {other:?}"),
    }
    assert_eq!(recorded.request_count(), 1);
}

#[tokio::test]
async fn invalid_tool_call_hook_can_repair_streaming_tool_name() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call(
                "tool_call_1",
                "default_api",
                serde_json::json!({"x": 2, "y": 3}),
            ),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(RepairDefaultApiHook)
        .max_turns(3)
        .history(Vec::<Message>::new())
        .await;
    let mut saw_repaired_tool_call = false;
    let mut saw_tool_result = false;
    let mut final_response_text = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                tool_call,
                ..
            })) => {
                assert_eq!(tool_call.function.name, "add");
                saw_repaired_tool_call = true;
            }
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                tool_result,
                ..
            })) => {
                assert!(tool_result.content.iter().any(|content| {
                    matches!(
                        content,
                        ToolResultContent::Json { value }
                            if value == &serde_json::json!(5)
                    )
                }));
                saw_tool_result = true;
            }
            Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                final_response_text = Some(response.output().to_string());
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    assert!(saw_repaired_tool_call);
    assert!(saw_tool_result);
    assert_eq!(final_response_text.as_deref(), Some("done"));
    assert_eq!(recorded.request_count(), 2);
}

#[tokio::test]
async fn invalid_tool_call_context_uses_completed_streaming_tool_call_provider_id() {
    let invalid_hook = RecordingInvalidToolCallHook::default();
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call(
                "tool_call_1",
                "default_api",
                serde_json::json!({"x": 2, "y": 3}),
            )
            .with_call_id("provider_call_1"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(invalid_hook.clone())
        .max_turns(3)
        .await;
    let mut error = None;

    while let Some(item) = stream.next().await {
        if let Err(err) = item {
            error = Some(err);
            break;
        }
    }

    assert!(error.is_some(), "invalid tool should fail");
    assert_eq!(recorded.request_count(), 1);
    let contexts = invalid_hook.observed();
    assert_eq!(contexts.len(), 1);
    let context = &contexts[0];
    assert_eq!(context.tool_name, "default_api");
    assert_eq!(context.tool_call_id.as_deref(), Some("tool_call_1"));
    assert!(context.internal_call_id.is_some());
    assert!(context.is_streaming);
}

#[tokio::test]
async fn invalid_tool_call_hook_skip_emits_streaming_tool_result() {
    let add_calls = Arc::new(AtomicU32::new(0));
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call(
                "tool_call_1",
                "default_api",
                serde_json::json!({"x": 2, "y": 3}),
            )
            .with_call_id("call_1"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("continued"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(CountingAddTool {
            calls: add_calls.clone(),
        })
        .build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(SkipDefaultApiHook)
        .max_turns(3)
        .history(Vec::<Message>::new())
        .await;
    let mut skipped_tool_result = None;
    let mut final_response_text = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                tool_result,
                internal_call_id,
            })) => {
                assert!(!internal_call_id.is_empty());
                skipped_tool_result = Some(tool_result);
            }
            Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                final_response_text = Some(response.output().to_string());
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    let skipped_tool_result =
        skipped_tool_result.expect("skip recovery should emit a synthetic tool result");
    assert_eq!(skipped_tool_result.id, "tool_call_1");
    assert_eq!(skipped_tool_result.call_id.as_deref(), Some("call_1"));
    assert!(skipped_tool_result.content.iter().any(|content| matches!(
        content,
        ToolResultContent::Text(text) if text.text == "default_api was skipped"
    )));
    assert_eq!(final_response_text.as_deref(), Some("continued"));
    assert_eq!(add_calls.load(Ordering::SeqCst), 0);

    let requests = recorded.requests();
    assert_eq!(requests.len(), 2);
    let follow_up_history = requests[1].chat_history.iter().cloned().collect::<Vec<_>>();
    assert!(matches!(
        follow_up_history.get(2),
        Some(Message::User { content })
            if content.iter().any(|item| matches!(
                item,
                UserContent::ToolResult(result)
                    if result.id == "tool_call_1"
                        && result.content.iter().any(|content| matches!(
                            content,
                            ToolResultContent::Text(text)
                                if text.text == "default_api was skipped"
                        ))
            ))
    ));
}

#[tokio::test]
async fn invalid_tool_call_hook_retries_mixed_streaming_turn_without_executing_valid_call() {
    let add_calls = Arc::new(AtomicU32::new(0));
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("checking "),
            MockStreamEvent::tool_call("tool_call_1", "add", serde_json::json!({"x": 2, "y": 3}))
                .with_call_id("call_1"),
            MockStreamEvent::tool_call(
                "tool_call_2",
                "default_api",
                serde_json::json!({"x": 4, "y": 5}),
            )
            .with_call_id("call_2"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("retried"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(CountingAddTool {
            calls: add_calls.clone(),
        })
        .build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(RetryDefaultApiHook)
        .max_turns(3)
        .history(Vec::<Message>::new())
        .max_invalid_tool_call_retries(1)
        .await;
    let mut completion_call_events = Vec::new();
    let mut final_response_text = None;
    let mut final_response_usage = Usage::new();
    let mut final_completion_calls = Vec::new();

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::CompletionCall(completion_call)) => {
                completion_call_events.push(completion_call);
            }
            Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                final_response_text = Some(response.output().to_string());
                final_response_usage = response.usage();
                final_completion_calls = response.completion_calls().to_vec();
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    assert_eq!(final_response_text.as_deref(), Some("retried"));
    assert_eq!(add_calls.load(Ordering::SeqCst), 0);
    let mut first_usage = Usage::new();
    first_usage.total_tokens = 4;
    let mut second_usage = Usage::new();
    second_usage.total_tokens = 6;
    let expected_completion_calls = vec![
        CompletionCall::new(0, first_usage),
        CompletionCall::new(1, second_usage),
    ];
    assert_eq!(completion_call_events, expected_completion_calls);
    assert_eq!(final_completion_calls, expected_completion_calls);
    assert_eq!(final_response_usage.total_tokens, 10);

    let requests = recorded.requests();
    assert_eq!(requests.len(), 2);
    let retry_history = requests[1].chat_history.iter().cloned().collect::<Vec<_>>();
    assert_eq!(retry_history.len(), 3);
    assert!(matches!(
        retry_history.get(1),
        Some(Message::Assistant { content, .. })
            if content.iter().any(|item| matches!(
                item,
                AssistantContent::Text(text) if text.text == "checking "
            ))
                && content.iter().any(|item| matches!(
                    item,
                    AssistantContent::ToolCall(tool_call)
                        if tool_call.id == "tool_call_1"
                            && tool_call.function.name == "add"
                ))
                && content.iter().any(|item| matches!(
                    item,
                    AssistantContent::ToolCall(tool_call)
                        if tool_call.id == "tool_call_2"
                            && tool_call.function.name == "default_api"
                ))
    ));
    assert!(matches!(
        retry_history.get(2),
        Some(Message::User { content })
            if content.iter().filter(|item| matches!(item, UserContent::ToolResult(_))).count() == 2
                && content.iter().any(|item| matches!(
                    item,
                    UserContent::ToolResult(result)
                        if result.id == "tool_call_1"
                            && result.content.iter().any(|content| matches!(
                                content,
                                ToolResultContent::Text(text)
                                    if text.text == TOOL_NOT_EXECUTED_DUE_TO_INVALID_PEER
                            ))
                ))
                && content.iter().any(|item| matches!(
                    item,
                    UserContent::ToolResult(result)
                        if result.id == "tool_call_2"
                            && result.content.iter().any(|content| matches!(
                                content,
                                ToolResultContent::Text(text)
                                    if text.text == "Use the add tool instead"
                            ))
                ))
    ));
}

#[tokio::test]
async fn invalid_tool_call_hook_skips_mixed_streaming_turn_without_executing_valid_call() {
    let add_calls = Arc::new(AtomicU32::new(0));
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("checking "),
            MockStreamEvent::tool_call("tool_call_1", "add", serde_json::json!({"x": 2, "y": 3}))
                .with_call_id("call_1"),
            MockStreamEvent::tool_call(
                "tool_call_2",
                "default_api",
                serde_json::json!({"x": 4, "y": 5}),
            )
            .with_call_id("call_2"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("continued"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(CountingAddTool {
            calls: add_calls.clone(),
        })
        .build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(SkipDefaultApiHook)
        .max_turns(3)
        .history(Vec::<Message>::new())
        .await;
    let mut skipped_tool_result = None;
    let mut final_response_text = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                tool_result,
                ..
            })) => {
                skipped_tool_result = Some(tool_result);
            }
            Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                final_response_text = Some(response.output().to_string());
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    let skipped_tool_result =
        skipped_tool_result.expect("skip recovery should emit a synthetic tool result");
    assert_eq!(skipped_tool_result.id, "tool_call_2");
    assert_eq!(skipped_tool_result.call_id.as_deref(), Some("call_2"));
    assert_eq!(final_response_text.as_deref(), Some("continued"));
    assert_eq!(add_calls.load(Ordering::SeqCst), 0);

    let requests = recorded.requests();
    assert_eq!(requests.len(), 2);
    let follow_up_history = requests[1].chat_history.iter().cloned().collect::<Vec<_>>();
    assert_eq!(follow_up_history.len(), 3);
    assert!(matches!(
        follow_up_history.get(1),
        Some(Message::Assistant { content, .. })
            if content.iter().any(|item| matches!(
                item,
                AssistantContent::Text(text) if text.text == "checking "
            ))
                && content.iter().any(|item| matches!(
                    item,
                    AssistantContent::ToolCall(tool_call)
                        if tool_call.id == "tool_call_1"
                            && tool_call.function.name == "add"
                ))
                && content.iter().any(|item| matches!(
                    item,
                    AssistantContent::ToolCall(tool_call)
                        if tool_call.id == "tool_call_2"
                            && tool_call.function.name == "default_api"
                ))
    ));
    assert!(matches!(
        follow_up_history.get(2),
        Some(Message::User { content })
            if content.iter().filter(|item| matches!(item, UserContent::ToolResult(_))).count() == 2
                && content.iter().any(|item| matches!(
                    item,
                    UserContent::ToolResult(result)
                        if result.id == "tool_call_1"
                            && result.call_id.as_deref() == Some("call_1")
                            && result.content.iter().any(|content| matches!(
                                content,
                                ToolResultContent::Text(text)
                                    if text.text == TOOL_NOT_EXECUTED_DUE_TO_INVALID_PEER
                            ))
                ))
                && content.iter().any(|item| matches!(
                    item,
                    UserContent::ToolResult(result)
                        if result.id == "tool_call_2"
                            && result.call_id.as_deref() == Some("call_2")
                            && result.content.iter().any(|content| matches!(
                                content,
                                ToolResultContent::Text(text)
                                    if text.text == "default_api was skipped"
                            ))
        ))
    ));
}

#[tokio::test]
async fn invalid_completed_tool_call_skip_preserves_streaming_reasoning_history() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("checking "),
            MockStreamEvent::reasoning("reasoned step").with_reasoning_id("rs_1"),
            MockStreamEvent::tool_call(
                "tool_call_1",
                "default_api",
                serde_json::json!({"x": 2, "y": 3}),
            ),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("continued"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(SkipDefaultApiHook)
        .max_turns(3)
        .history(Vec::<Message>::new())
        .await;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::FinalResponse(_)) => break,
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    let requests = recorded.requests();
    assert_eq!(requests.len(), 2);
    let follow_up_history = requests[1].chat_history.iter().cloned().collect::<Vec<_>>();
    assert!(history_contains_text(&follow_up_history, "checking "));
    assert!(assistant_reasoning_precedes_tool_call(
        &follow_up_history,
        "reasoned step",
        "default_api"
    ));
    assert!(
        assistant_reasoning_precedes_text_and_tool_call(
            &follow_up_history,
            "reasoned step",
            "checking ",
            "default_api"
        ),
        "{follow_up_history:?}"
    );
}

#[tokio::test]
async fn invalid_name_delta_retry_preserves_streaming_reasoning_history() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::reasoning_delta_with_id("rs_1", "delta reason"),
            MockStreamEvent::tool_call_arguments_delta("tool_call_1", r#"{"x":2,"y":3}"#),
            MockStreamEvent::tool_call_name_delta("tool_call_1", "default_api"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("retried"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(RetryDefaultApiHook)
        .max_turns(3)
        .history(Vec::<Message>::new())
        .max_invalid_tool_call_retries(1)
        .await;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::FinalResponse(_)) => break,
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    let requests = recorded.requests();
    assert_eq!(requests.len(), 2);
    let retry_history = requests[1].chat_history.iter().cloned().collect::<Vec<_>>();
    assert!(assistant_reasoning_precedes_tool_call(
        &retry_history,
        "delta reason",
        "default_api"
    ));
}

#[tokio::test]
async fn invalid_tool_call_hook_skip_resets_streaming_text_delta_state() {
    let text_hook = RecordingTextDeltaHook::default();
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("stale "),
            MockStreamEvent::tool_call(
                "tool_call_1",
                "default_api",
                serde_json::json!({"x": 2, "y": 3}),
            ),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("fresh"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(RecordingTextAndSkipInvalidToolHook {
            text: text_hook.clone(),
        })
        .max_turns(3)
        .history(Vec::<Message>::new())
        .await;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::FinalResponse(_)) => break,
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    assert_eq!(
        text_hook.observed(),
        vec![
            ("stale ".to_string(), "stale ".to_string()),
            ("fresh".to_string(), "fresh".to_string()),
        ]
    );
}

#[tokio::test]
async fn invalid_tool_call_delta_retry_uses_structured_tool_feedback() {
    let delta_hook = RecordingToolCallDeltaHook::default();
    let add_calls = Arc::new(AtomicU32::new(0));
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("checking "),
            MockStreamEvent::reasoning_delta_with_id("rs_1", "diagnostic reason"),
            MockStreamEvent::tool_call("tool_call_0", "add", serde_json::json!({"x": 1, "y": 2}))
                .with_call_id("call_0"),
            MockStreamEvent::tool_call_arguments_delta("tool_call_1", r#"{"x":2,"y":3}"#),
            MockStreamEvent::tool_call_name_delta("tool_call_1", "default_api"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("retried"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(CountingAddTool {
            calls: add_calls.clone(),
        })
        .build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(RecordingDeltaAndRetryInvalidToolHook {
            delta: delta_hook.clone(),
        })
        .max_turns(3)
        .history(Vec::<Message>::new())
        .max_invalid_tool_call_retries(1)
        .await;
    let mut completion_call_events = Vec::new();
    let mut final_response_text = None;
    let mut final_response_usage = Usage::new();
    let mut final_completion_calls = Vec::new();

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::CompletionCall(completion_call)) => {
                completion_call_events.push(completion_call);
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::ToolCallDelta { .. },
            )) => panic!("invalid tool-call delta should not be emitted"),
            Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                final_response_text = Some(response.output().to_string());
                final_response_usage = response.usage();
                final_completion_calls = response.completion_calls().to_vec();
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    assert_eq!(final_response_text.as_deref(), Some("retried"));
    assert!(delta_hook.observed().is_empty());
    assert_eq!(add_calls.load(Ordering::SeqCst), 0);
    let mut first_usage = Usage::new();
    first_usage.total_tokens = 4;
    let mut second_usage = Usage::new();
    second_usage.total_tokens = 6;
    let expected_completion_calls = vec![
        CompletionCall::new(0, first_usage),
        CompletionCall::new(1, second_usage),
    ];
    assert_eq!(completion_call_events, expected_completion_calls);
    assert_eq!(final_completion_calls, expected_completion_calls);
    assert_eq!(final_response_usage.total_tokens, 10);

    let requests = recorded.requests();
    assert_eq!(requests.len(), 2);
    let retry_history = requests[1].chat_history.iter().cloned().collect::<Vec<_>>();
    assert!(matches!(
        retry_history.get(1),
        Some(Message::Assistant { content, .. })
            if content.iter().any(|item| matches!(
                item,
                AssistantContent::Text(text) if text.text == "checking "
            ))
                && content.iter().any(|item| matches!(
                    item,
                    AssistantContent::ToolCall(tool_call)
                        if tool_call.id == "tool_call_0"
                            && tool_call.function.name == "add"
                ))
                && content.iter().any(|item| matches!(
                item,
                AssistantContent::ToolCall(tool_call)
                    if tool_call.id == "tool_call_1"
                        && tool_call.function.name == "default_api"
                        && tool_call.function.arguments == serde_json::json!({"x": 2, "y": 3})
            ))
    ));
    assert!(matches!(
        retry_history.get(2),
        Some(Message::User { content })
            if content.iter().filter(|item| matches!(item, UserContent::ToolResult(_))).count() == 2
                && content.iter().any(|item| matches!(
                    item,
                    UserContent::ToolResult(result)
                        if result.id == "tool_call_0"
                            && result.call_id.as_deref() == Some("call_0")
                            && result.content.iter().any(|content| matches!(
                                content,
                                ToolResultContent::Text(text)
                                    if text.text == TOOL_NOT_EXECUTED_DUE_TO_INVALID_PEER
                            ))
                ))
                && content.iter().any(|item| matches!(
                item,
                UserContent::ToolResult(result)
                    if result.id == "tool_call_1"
                        && result.content.iter().any(|content| matches!(
                            content,
                            ToolResultContent::Text(text)
                                if text.text == "Use the add tool instead"
                        ))
            ))
    ));
}

#[tokio::test]
async fn invalid_tool_call_delta_context_includes_same_turn_history_and_tool_call_id() {
    let invalid_hook = RecordingInvalidToolCallHook::default();
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("checking "),
            MockStreamEvent::reasoning_delta_with_id("rs_1", "diagnostic reason"),
            MockStreamEvent::tool_call("tool_call_0", "add", serde_json::json!({"x": 1, "y": 2}))
                .with_call_id("call_0"),
            MockStreamEvent::tool_call_arguments_delta("tool_call_1", r#"{"x":2,"y":3}"#),
            MockStreamEvent::tool_call_name_delta("tool_call_1", "default_api"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(invalid_hook.clone())
        .max_turns(3)
        .await;
    let mut error = None;

    while let Some(item) = stream.next().await {
        if let Err(err) = item {
            error = Some(err);
            break;
        }
    }

    assert!(error.is_some(), "invalid name delta should fail");
    assert_eq!(recorded.request_count(), 1);
    let contexts = invalid_hook.observed();
    assert_eq!(contexts.len(), 1);
    let context = &contexts[0];
    assert_eq!(context.tool_name, "default_api");
    assert_eq!(context.tool_call_id.as_deref(), Some("tool_call_1"));
    assert!(
        context
            .internal_call_id
            .as_deref()
            .is_some_and(|id| !id.is_empty()),
        "internal call id is minted by the shared accumulator"
    );
    assert!(context.is_streaming);
    assert!(history_contains_text(&context.chat_history, "checking "));
    assert!(
        assistant_reasoning_precedes_tool_call(&context.chat_history, "diagnostic reason", "add"),
        "{:?}",
        context.chat_history
    );
    assert!(history_contains_tool_call(&context.chat_history, "add"));
    assert!(history_contains_tool_call(
        &context.chat_history,
        "default_api"
    ));
}

#[tokio::test]
async fn invalid_tool_call_delta_retry_resets_streaming_text_delta_state() {
    let text_hook = RecordingTextDeltaHook::default();
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("stale "),
            MockStreamEvent::tool_call_arguments_delta("tool_call_1", r#"{"x":2,"y":3}"#),
            MockStreamEvent::tool_call_name_delta("tool_call_1", "default_api"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("fresh"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(RecordingTextAndRetryInvalidToolHook {
            text: text_hook.clone(),
        })
        .max_turns(3)
        .history(Vec::<Message>::new())
        .max_invalid_tool_call_retries(1)
        .await;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::FinalResponse(_)) => break,
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    assert_eq!(
        text_hook.observed(),
        vec![
            ("stale ".to_string(), "stale ".to_string()),
            ("fresh".to_string(), "fresh".to_string()),
        ]
    );
}

#[tokio::test]
async fn invalid_tool_call_delta_skip_uses_structured_tool_feedback() {
    let delta_hook = RecordingToolCallDeltaHook::default();
    let add_calls = Arc::new(AtomicU32::new(0));
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("checking "),
            MockStreamEvent::tool_call("tool_call_0", "add", serde_json::json!({"x": 1, "y": 2}))
                .with_call_id("call_0"),
            MockStreamEvent::tool_call_arguments_delta("tool_call_1", r#"{"x":2,"y":3}"#),
            MockStreamEvent::tool_call_name_delta("tool_call_1", "default_api"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("continued"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(CountingAddTool {
            calls: add_calls.clone(),
        })
        .build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(RecordingDeltaAndSkipInvalidToolHook {
            delta: delta_hook.clone(),
        })
        .max_turns(3)
        .history(Vec::<Message>::new())
        .await;
    let mut skipped_tool_result = None;
    let mut final_response_text = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::ToolCallDelta { .. },
            )) => panic!("invalid tool-call delta should not be emitted"),
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                tool_result,
                internal_call_id,
            })) => {
                assert!(
                    !internal_call_id.is_empty(),
                    "internal call id is minted by the shared accumulator"
                );
                skipped_tool_result = Some(tool_result);
            }
            Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                final_response_text = Some(response.output().to_string());
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    let skipped_tool_result =
        skipped_tool_result.expect("skip recovery should emit a synthetic tool result");
    assert_eq!(skipped_tool_result.id, "tool_call_1");
    assert!(skipped_tool_result.call_id.is_none());
    assert!(skipped_tool_result.content.iter().any(|content| matches!(
        content,
        ToolResultContent::Text(text) if text.text == "default_api was skipped"
    )));
    assert_eq!(final_response_text.as_deref(), Some("continued"));
    assert!(delta_hook.observed().is_empty());
    assert_eq!(add_calls.load(Ordering::SeqCst), 0);

    let requests = recorded.requests();
    assert_eq!(requests.len(), 2);
    let follow_up_history = requests[1].chat_history.iter().cloned().collect::<Vec<_>>();
    assert!(matches!(
        follow_up_history.get(1),
        Some(Message::Assistant { content, .. })
            if content.iter().any(|item| matches!(
                item,
                AssistantContent::Text(text) if text.text == "checking "
            ))
                && content.iter().any(|item| matches!(
                    item,
                    AssistantContent::ToolCall(tool_call)
                        if tool_call.id == "tool_call_0"
                            && tool_call.function.name == "add"
                ))
                && content.iter().any(|item| matches!(
                item,
                AssistantContent::ToolCall(tool_call)
                    if tool_call.id == "tool_call_1"
                        && tool_call.function.name == "default_api"
                        && tool_call.function.arguments == serde_json::json!({"x": 2, "y": 3})
            ))
    ));
    assert!(matches!(
        follow_up_history.get(2),
        Some(Message::User { content })
            if content.iter().filter(|item| matches!(item, UserContent::ToolResult(_))).count() == 2
                && content.iter().any(|item| matches!(
                    item,
                    UserContent::ToolResult(result)
                        if result.id == "tool_call_0"
                            && result.call_id.as_deref() == Some("call_0")
                            && result.content.iter().any(|content| matches!(
                                content,
                                ToolResultContent::Text(text)
                                    if text.text == TOOL_NOT_EXECUTED_DUE_TO_INVALID_PEER
                            ))
                ))
                && content.iter().any(|item| matches!(
                item,
                UserContent::ToolResult(result)
                    if result.id == "tool_call_1"
                        && result.content.iter().any(|content| matches!(
                            content,
                            ToolResultContent::Text(text)
                                if text.text == "default_api was skipped"
                        ))
            ))
    ));
}

#[tokio::test]
async fn streaming_retry_budget_exhaustion_history_contains_invalid_tool_call() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call(
                "tool_call_1",
                "default_api",
                serde_json::json!({"x": 1, "y": 2}),
            ),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(RetryDefaultApiHook)
        .max_turns(3)
        .max_invalid_tool_call_retries(0)
        .await;
    let mut error = None;

    while let Some(item) = stream.next().await {
        if let Err(err) = item {
            error = Some(err);
            break;
        }
    }

    let error = error.expect("retry budget exhaustion should fail");
    match error {
        StreamingError::Prompt(err) => match *err {
            PromptError::UnknownToolCall {
                tool_name,
                chat_history,
                ..
            } => {
                assert_eq!(tool_name, "default_api");
                assert!(history_contains_tool_call(&chat_history, "default_api"));
            }
            other => panic!("expected UnknownToolCall, got {other:?}"),
        },
        other => panic!("expected prompt streaming error, got {other:?}"),
    }
    assert_eq!(recorded.request_count(), 1);
}

#[tokio::test]
async fn streaming_name_delta_retry_budget_exhaustion_history_includes_same_turn_context() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("checking "),
            MockStreamEvent::tool_call("tool_call_0", "add", serde_json::json!({"x": 1, "y": 2}))
                .with_call_id("call_0"),
            MockStreamEvent::tool_call_arguments_delta("tool_call_1", r#"{"x":2,"y":3}"#),
            MockStreamEvent::tool_call_name_delta("tool_call_1", "default_api"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(RetryDefaultApiHook)
        .max_turns(3)
        .max_invalid_tool_call_retries(0)
        .await;
    let mut error = None;

    while let Some(item) = stream.next().await {
        if let Err(err) = item {
            error = Some(err);
            break;
        }
    }

    let error = error.expect("retry budget exhaustion should fail");
    match error {
        StreamingError::Prompt(err) => match *err {
            PromptError::UnknownToolCall {
                tool_name,
                chat_history,
                ..
            } => {
                assert_eq!(tool_name, "default_api");
                assert!(history_contains_text(&chat_history, "checking "));
                assert!(history_contains_tool_call(&chat_history, "add"));
                assert!(history_contains_tool_call(&chat_history, "default_api"));
            }
            other => panic!("expected UnknownToolCall, got {other:?}"),
        },
        other => panic!("expected prompt streaming error, got {other:?}"),
    }
    assert_eq!(recorded.request_count(), 1);
}

#[tokio::test]
async fn completed_unknown_tool_call_after_text_fails_before_finish_hook_or_later_emit() {
    let add_calls = Arc::new(AtomicU32::new(0));
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("thinking "),
            MockStreamEvent::tool_call(
                "tool_call_1",
                "default_api",
                serde_json::json!({"x": 1, "y": 2}),
            ),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(CountingAddTool {
            calls: add_calls.clone(),
        })
        .build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(PanicOnUnknownToolHook)
        .max_turns(3)
        .await;
    let mut saw_text = false;
    let mut saw_completion_call = false;
    let mut saw_final_response = false;
    let mut saw_tool_call = false;
    let mut saw_tool_result = false;
    let mut error = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(_))) => {
                saw_text = true;
            }
            Ok(MultiTurnStreamItem::CompletionCall(_)) => {
                saw_completion_call = true;
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Final(_)))
            | Ok(MultiTurnStreamItem::FinalResponse(_)) => {
                saw_final_response = true;
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                ..
            })) => {
                saw_tool_call = true;
            }
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult { .. })) => {
                saw_tool_result = true;
            }
            Ok(_) => {}
            Err(err) => {
                error = Some(err);
                break;
            }
        }
    }

    assert!(saw_text);
    assert!(!saw_completion_call);
    assert!(!saw_final_response);
    assert!(!saw_tool_call);
    assert!(!saw_tool_result);
    assert_eq!(add_calls.load(Ordering::SeqCst), 0);
    let error = error.expect("completed unknown tool call should fail immediately");
    match error {
        StreamingError::Prompt(err) => match *err {
            PromptError::UnknownToolCall {
                tool_name,
                available_tools,
                allowed_tools,
                chat_history,
            } => {
                assert_eq!(tool_name, "default_api");
                assert_eq!(available_tools, vec!["add".to_string()]);
                assert_eq!(allowed_tools, vec!["add".to_string()]);
                assert!(history_contains_tool_call(&chat_history, "default_api"));
            }
            other => panic!("expected UnknownToolCall, got {other:?}"),
        },
        other => panic!("expected prompt streaming error, got {other:?}"),
    }
    assert_eq!(recorded.request_count(), 1);
}

#[tokio::test]
async fn mixed_streaming_tool_calls_fail_before_any_tool_execution() {
    let add_calls = Arc::new(AtomicU32::new(0));
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tool_call_1", "add", serde_json::json!({"x": 1, "y": 2}))
                .with_call_id("call_1"),
            MockStreamEvent::tool_call(
                "tool_call_2",
                "default_api",
                serde_json::json!({"x": 3, "y": 4}),
            ),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(CountingAddTool {
            calls: add_calls.clone(),
        })
        .build();

    let mut stream = agent
        .stream_prompt("use tools")
        .add_hook(PanicOnUnknownToolHook)
        .max_turns(3)
        .await;
    let mut saw_completion_call = false;
    let mut saw_tool_call = false;
    let mut saw_tool_result = false;
    let mut error = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::CompletionCall(_)) => {
                saw_completion_call = true;
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                ..
            })) => {
                saw_tool_call = true;
            }
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult { .. })) => {
                saw_tool_result = true;
            }
            Ok(_) => {}
            Err(err) => {
                error = Some(err);
                break;
            }
        }
    }

    assert!(!saw_completion_call);
    assert!(!saw_tool_call);
    assert!(!saw_tool_result);
    assert_eq!(add_calls.load(Ordering::SeqCst), 0);
    let error = error.expect("mixed unknown streamed tool call should fail");
    match error {
        StreamingError::Prompt(err) => match *err {
            PromptError::UnknownToolCall {
                tool_name,
                available_tools,
                allowed_tools,
                chat_history,
            } => {
                assert_eq!(tool_name, "default_api");
                assert_eq!(available_tools, vec!["add".to_string()]);
                assert_eq!(allowed_tools, vec!["add".to_string()]);
                assert!(history_contains_tool_call(&chat_history, "default_api"));
            }
            other => panic!("expected UnknownToolCall, got {other:?}"),
        },
        other => panic!("expected prompt streaming error, got {other:?}"),
    }
    assert_eq!(recorded.request_count(), 1);
}

#[tokio::test]
async fn multiple_valid_streaming_tool_calls_execute_after_batch_validation() {
    let add_calls = Arc::new(AtomicU32::new(0));
    let subtract_calls = Arc::new(AtomicU32::new(0));
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tool_call_1", "add", serde_json::json!({"x": 1, "y": 2}))
                .with_call_id("call_1"),
            MockStreamEvent::tool_call(
                "tool_call_2",
                "subtract",
                serde_json::json!({"x": 8, "y": 3}),
            )
            .with_call_id("call_2"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(CountingAddTool {
            calls: add_calls.clone(),
        })
        .tool(CountingSubtractTool {
            calls: subtract_calls.clone(),
        })
        .build();

    let mut stream = agent.stream_prompt("use tools").max_turns(3).await;
    let mut tool_call_names = Vec::new();
    let mut tool_result_ids = Vec::new();
    let mut final_response_text = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                tool_call,
                ..
            })) => {
                tool_call_names.push(tool_call.function.name);
            }
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                tool_result,
                ..
            })) => {
                tool_result_ids.push(tool_result.id);
            }
            Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                final_response_text = Some(response.output().to_owned());
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    assert_eq!(
        tool_call_names,
        vec!["add".to_string(), "subtract".to_string()]
    );
    assert_eq!(
        tool_result_ids,
        vec!["tool_call_1".to_string(), "tool_call_2".to_string()]
    );
    assert_eq!(add_calls.load(Ordering::SeqCst), 1);
    assert_eq!(subtract_calls.load(Ordering::SeqCst), 1);
    assert_eq!(final_response_text.as_deref(), Some("done"));
    assert_eq!(recorded.request_count(), 2);
}

#[tokio::test]
async fn disallowed_specific_tool_call_fails_before_streaming_second_request() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call(
                "tool_call_1",
                "subtract",
                serde_json::json!({"x": 3, "y": 1}),
            ),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(MockAddTool)
        .tool(MockSubtractTool)
        .tool_choice(ToolChoice::Specific {
            function_names: vec!["add".to_string()],
        })
        .build();

    let mut stream = agent
        .stream_prompt("use the allowed tool")
        .add_hook(PanicOnUnknownToolHook)
        .max_turns(3)
        .await;
    let mut saw_tool_call = false;
    let mut error = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                ..
            })) => {
                saw_tool_call = true;
            }
            Ok(_) => {}
            Err(err) => {
                error = Some(err);
                break;
            }
        }
    }

    assert!(!saw_tool_call);
    let error = error.expect("disallowed model-emitted tool should fail");
    match error {
        StreamingError::Prompt(err) => match *err {
            PromptError::UnknownToolCall {
                tool_name,
                available_tools,
                allowed_tools,
                chat_history,
            } => {
                assert_eq!(tool_name, "subtract");
                assert_eq!(
                    available_tools,
                    vec!["add".to_string(), "subtract".to_string()]
                );
                assert_eq!(allowed_tools, vec!["add".to_string()]);
                assert!(history_contains_tool_call(&chat_history, "subtract"));
            }
            other => panic!("expected UnknownToolCall, got {other:?}"),
        },
        other => panic!("expected prompt streaming error, got {other:?}"),
    }
    assert_eq!(recorded.request_count(), 1);
}

#[tokio::test]
async fn mixed_specific_tool_calls_fail_before_any_tool_execution() {
    let add_calls = Arc::new(AtomicU32::new(0));
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tool_call_1", "add", serde_json::json!({"x": 1, "y": 2})),
            MockStreamEvent::tool_call(
                "tool_call_2",
                "subtract",
                serde_json::json!({"x": 3, "y": 1}),
            ),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(CountingAddTool {
            calls: add_calls.clone(),
        })
        .tool(MockSubtractTool)
        .tool_choice(ToolChoice::Specific {
            function_names: vec!["add".to_string()],
        })
        .build();

    let mut stream = agent
        .stream_prompt("use the allowed tool")
        .add_hook(PanicOnUnknownToolHook)
        .max_turns(3)
        .await;
    let mut saw_tool_call = false;
    let mut saw_tool_result = false;
    let mut error = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                ..
            })) => {
                saw_tool_call = true;
            }
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult { .. })) => {
                saw_tool_result = true;
            }
            Ok(_) => {}
            Err(err) => {
                error = Some(err);
                break;
            }
        }
    }

    assert!(!saw_tool_call);
    assert!(!saw_tool_result);
    assert_eq!(add_calls.load(Ordering::SeqCst), 0);
    let error = error.expect("mixed disallowed streamed tool call should fail");
    match error {
        StreamingError::Prompt(err) => match *err {
            PromptError::UnknownToolCall {
                tool_name,
                available_tools,
                allowed_tools,
                chat_history,
            } => {
                assert_eq!(tool_name, "subtract");
                assert_eq!(
                    available_tools,
                    vec!["add".to_string(), "subtract".to_string()]
                );
                assert_eq!(allowed_tools, vec!["add".to_string()]);
                assert!(history_contains_tool_call(&chat_history, "subtract"));
            }
            other => panic!("expected UnknownToolCall, got {other:?}"),
        },
        other => panic!("expected prompt streaming error, got {other:?}"),
    }
    assert_eq!(recorded.request_count(), 1);
}

#[tokio::test]
async fn tool_choice_none_rejects_streaming_tool_call() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tool_call_1", "add", serde_json::json!({"x": 1, "y": 2})),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(MockAddTool)
        .tool_choice(ToolChoice::None)
        .build();

    let mut stream = agent
        .stream_prompt("do not use tools")
        .add_hook(PanicOnUnknownToolHook)
        .max_turns(3)
        .await;
    let mut saw_tool_call = false;
    let mut error = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                ..
            })) => {
                saw_tool_call = true;
            }
            Ok(_) => {}
            Err(err) => {
                error = Some(err);
                break;
            }
        }
    }

    assert!(!saw_tool_call);
    let error = error.expect("ToolChoice::None should reject returned tool calls");
    match error {
        StreamingError::Prompt(err) => match *err {
            PromptError::UnknownToolCall {
                tool_name,
                available_tools,
                allowed_tools,
                chat_history,
            } => {
                assert_eq!(tool_name, "add");
                assert_eq!(available_tools, vec!["add".to_string()]);
                assert!(allowed_tools.is_empty());
                assert!(history_contains_tool_call(&chat_history, "add"));
            }
            other => panic!("expected UnknownToolCall, got {other:?}"),
        },
        other => panic!("expected prompt streaming error, got {other:?}"),
    }
    assert_eq!(recorded.request_count(), 1);
}

#[tokio::test]
async fn tool_choice_none_rejects_streaming_tool_call_name_delta_before_hook_or_emit() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call_name_delta("tool_1", "add"),
            MockStreamEvent::tool_call_arguments_delta("tool_1", "{\"x\":1}"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(MockAddTool)
        .tool_choice(ToolChoice::None)
        .build();

    let mut stream = agent
        .stream_prompt("do not use tools")
        .add_hook(PanicOnUnknownToolHook)
        .max_turns(3)
        .await;
    let mut saw_delta = false;
    let mut error = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::ToolCallDelta { .. },
            )) => {
                saw_delta = true;
            }
            Ok(_) => {}
            Err(err) => {
                error = Some(err);
                break;
            }
        }
    }

    assert!(!saw_delta);
    let error = error.expect("ToolChoice::None should reject returned tool-call deltas");
    match error {
        StreamingError::Prompt(err) => match *err {
            PromptError::UnknownToolCall {
                tool_name,
                available_tools,
                allowed_tools,
                chat_history,
            } => {
                assert_eq!(tool_name, "add");
                assert_eq!(available_tools, vec!["add".to_string()]);
                assert!(allowed_tools.is_empty());
                assert!(history_contains_tool_call(&chat_history, "add"));
            }
            other => panic!("expected UnknownToolCall, got {other:?}"),
        },
        other => panic!("expected prompt streaming error, got {other:?}"),
    }
    assert_eq!(recorded.request_count(), 1);
}

#[tokio::test]
async fn unknown_tool_call_name_delta_fails_before_streaming_delta_hook_or_emit() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call_name_delta("tool_1", "default_api"),
            MockStreamEvent::tool_call_arguments_delta("tool_1", "{\"x\":1}"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("stream a bad tool call")
        .add_hook(PanicOnUnknownToolHook)
        .max_turns(3)
        .await;
    let mut saw_delta = false;
    let mut error = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::ToolCallDelta { .. },
            )) => {
                saw_delta = true;
            }
            Ok(_) => {}
            Err(err) => {
                error = Some(err);
                break;
            }
        }
    }

    assert!(!saw_delta);
    let error = error.expect("unknown tool-call name delta should fail");
    match error {
        StreamingError::Prompt(err) => match *err {
            PromptError::UnknownToolCall {
                tool_name,
                available_tools,
                allowed_tools,
                chat_history,
            } => {
                assert_eq!(tool_name, "default_api");
                assert_eq!(available_tools, vec!["add".to_string()]);
                assert_eq!(allowed_tools, vec!["add".to_string()]);
                assert!(history_contains_tool_call(&chat_history, "default_api"));
            }
            other => panic!("expected UnknownToolCall, got {other:?}"),
        },
        other => panic!("expected prompt streaming error, got {other:?}"),
    }
    assert_eq!(recorded.request_count(), 1);
}

#[tokio::test]
async fn tool_call_args_delta_before_unknown_name_fails_before_hook_or_emit() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call_arguments_delta("tool_1", "{\"x\":1}"),
            MockStreamEvent::tool_call_name_delta("tool_1", "default_api"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("stream a bad tool call")
        .add_hook(PanicOnUnknownToolHook)
        .max_turns(3)
        .await;
    let mut saw_delta = false;
    let mut error = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::ToolCallDelta { .. },
            )) => {
                saw_delta = true;
            }
            Ok(_) => {}
            Err(err) => {
                error = Some(err);
                break;
            }
        }
    }

    assert!(!saw_delta);
    let error = error.expect("unknown tool-call name should reject buffered args");
    match error {
        StreamingError::Prompt(err) => match *err {
            PromptError::UnknownToolCall {
                tool_name,
                available_tools,
                allowed_tools,
                chat_history,
            } => {
                assert_eq!(tool_name, "default_api");
                assert_eq!(available_tools, vec!["add".to_string()]);
                assert_eq!(allowed_tools, vec!["add".to_string()]);
                assert!(history_contains_tool_call(&chat_history, "default_api"));
            }
            other => panic!("expected UnknownToolCall, got {other:?}"),
        },
        other => panic!("expected prompt streaming error, got {other:?}"),
    }
    assert_eq!(recorded.request_count(), 1);
}

#[tokio::test]
async fn tool_call_args_delta_before_valid_name_buffers_then_emits_in_safe_order() {
    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::tool_call_arguments_delta("tool_1", "{\"x\":"),
        MockStreamEvent::tool_call_name_delta("tool_1", "add"),
        MockStreamEvent::tool_call_arguments_delta("tool_1", "1}"),
        MockStreamEvent::final_response_with_total_tokens(3),
    ]]);
    let hook = RecordingToolCallDeltaHook::default();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("stream a tool call")
        .add_hook(hook.clone())
        .await;
    let mut stream_deltas = Vec::new();

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::ToolCallDelta {
                    id,
                    internal_call_id,
                    content,
                },
            )) => {
                stream_deltas.push((id, internal_call_id, content));
            }
            Ok(MultiTurnStreamItem::FinalResponse(_)) => break,
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    // The internal call id is minted by the shared accumulator when the
    // call opens; assert correlation (one stable id across every delta)
    // rather than a scripted literal.
    let internal = stream_deltas
        .first()
        .map(|delta| delta.1.clone())
        .expect("at least one delta");
    assert!(!internal.is_empty());
    assert_eq!(
        hook.observed(),
        vec![
            (
                "tool_1".to_string(),
                internal.clone(),
                Some("add".to_string()),
                String::new()
            ),
            (
                "tool_1".to_string(),
                internal.clone(),
                None,
                "{\"x\":".to_string()
            ),
            (
                "tool_1".to_string(),
                internal.clone(),
                None,
                "1}".to_string()
            ),
        ]
    );
    assert_eq!(
        stream_deltas,
        vec![
            (
                "tool_1".to_string(),
                internal.clone(),
                ToolCallDeltaContent::Name("add".to_string())
            ),
            (
                "tool_1".to_string(),
                internal.clone(),
                ToolCallDeltaContent::Delta("{\"x\":".to_string())
            ),
            (
                "tool_1".to_string(),
                internal.clone(),
                ToolCallDeltaContent::Delta("1}".to_string())
            ),
        ]
    );
}

#[tokio::test]
async fn tool_call_args_delta_without_name_errors_at_stream_end() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call_arguments_delta("tool_1", "{\"x\":1}"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("stream an incomplete tool call")
        .add_hook(PanicOnUnknownToolHook)
        .max_turns(3)
        .await;
    let mut saw_delta = false;
    let mut saw_completion_call = false;
    let mut saw_final_response = false;
    let mut error = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::ToolCallDelta { .. },
            )) => {
                saw_delta = true;
            }
            Ok(MultiTurnStreamItem::CompletionCall(_)) => {
                saw_completion_call = true;
            }
            Ok(MultiTurnStreamItem::FinalResponse(_)) => {
                saw_final_response = true;
            }
            Ok(_) => {}
            Err(err) => {
                error = Some(err);
                break;
            }
        }
    }

    assert!(!saw_delta);
    assert!(!saw_completion_call);
    assert!(!saw_final_response);
    let error = error.expect("unterminated tool-call args delta should fail");
    match error {
        StreamingError::Completion(CompletionError::ResponseError(message)) => {
            assert!(
                message.contains("streamed tool call arguments"),
                "{message}"
            );
            assert!(message.contains("tool_1"), "{message}");
        }
        other => panic!("expected completion response error, got {other:?}"),
    }
    assert_eq!(recorded.request_count(), 1);
}

#[tokio::test]
async fn tool_choice_none_buffers_args_then_rejects_name_without_emit() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call_arguments_delta("tool_1", "{\"x\":1}"),
            MockStreamEvent::tool_call_name_delta("tool_1", "add"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model)
        .tool(MockAddTool)
        .tool_choice(ToolChoice::None)
        .build();

    let mut stream = agent
        .stream_prompt("do not use tools")
        .add_hook(PanicOnUnknownToolHook)
        .max_turns(3)
        .await;
    let mut saw_delta = false;
    let mut error = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::ToolCallDelta { .. },
            )) => {
                saw_delta = true;
            }
            Ok(_) => {}
            Err(err) => {
                error = Some(err);
                break;
            }
        }
    }

    assert!(!saw_delta);
    let error = error.expect("ToolChoice::None should reject buffered tool-call deltas");
    match error {
        StreamingError::Prompt(err) => match *err {
            PromptError::UnknownToolCall {
                tool_name,
                available_tools,
                allowed_tools,
                chat_history,
            } => {
                assert_eq!(tool_name, "add");
                assert_eq!(available_tools, vec!["add".to_string()]);
                assert!(allowed_tools.is_empty());
                assert!(history_contains_tool_call(&chat_history, "add"));
            }
            other => panic!("expected UnknownToolCall, got {other:?}"),
        },
        other => panic!("expected prompt streaming error, got {other:?}"),
    }
    assert_eq!(recorded.request_count(), 1);
}

#[tokio::test]
async fn stream_prompt_emits_tool_call_deltas_without_hook() {
    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::tool_call_name_delta("tool_1", "add"),
        MockStreamEvent::tool_call_arguments_delta("tool_1", "{\"x\":"),
        MockStreamEvent::tool_call_arguments_delta("tool_1", "1}"),
        MockStreamEvent::final_response_with_total_tokens(3),
    ]]);
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent.stream_prompt("stream a tool call").await;
    let mut deltas = Vec::new();

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::ToolCallDelta {
                    id,
                    internal_call_id,
                    content,
                },
            )) => {
                deltas.push((id, internal_call_id, content));
            }
            Ok(MultiTurnStreamItem::FinalResponse(_)) => break,
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    // The internal call id is minted by the shared accumulator when the
    // call opens; assert correlation (one stable id across every delta)
    // rather than a scripted literal.
    let internal = deltas
        .first()
        .map(|delta| delta.1.clone())
        .expect("at least one delta");
    assert!(!internal.is_empty());
    assert_eq!(
        deltas,
        vec![
            (
                "tool_1".to_string(),
                internal.clone(),
                ToolCallDeltaContent::Name("add".to_string())
            ),
            (
                "tool_1".to_string(),
                internal.clone(),
                ToolCallDeltaContent::Delta("{\"x\":".to_string())
            ),
            (
                "tool_1".to_string(),
                internal.clone(),
                ToolCallDeltaContent::Delta("1}".to_string())
            ),
        ]
    );
}

#[tokio::test]
async fn stream_prompt_emits_tool_call_deltas_after_hook_continue() {
    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::tool_call_name_delta("tool_1", "add"),
        MockStreamEvent::tool_call_arguments_delta("tool_1", "{\"x\":"),
        MockStreamEvent::tool_call_arguments_delta("tool_1", "1}"),
        MockStreamEvent::final_response_with_total_tokens(3),
    ]]);
    let hook = RecordingToolCallDeltaHook::default();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("stream a tool call")
        .add_hook(hook.clone())
        .await;
    let mut stream_deltas = Vec::new();

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::ToolCallDelta {
                    id,
                    internal_call_id,
                    content,
                },
            )) => {
                stream_deltas.push((id, internal_call_id, content));
            }
            Ok(MultiTurnStreamItem::FinalResponse(_)) => break,
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    // The internal call id is minted by the shared accumulator when the
    // call opens; assert correlation (one stable id across every delta)
    // rather than a scripted literal.
    let internal = stream_deltas
        .first()
        .map(|delta| delta.1.clone())
        .expect("at least one delta");
    assert!(!internal.is_empty());
    assert_eq!(
        hook.observed(),
        vec![
            (
                "tool_1".to_string(),
                internal.clone(),
                Some("add".to_string()),
                String::new()
            ),
            (
                "tool_1".to_string(),
                internal.clone(),
                None,
                "{\"x\":".to_string()
            ),
            (
                "tool_1".to_string(),
                internal.clone(),
                None,
                "1}".to_string()
            ),
        ]
    );
    assert_eq!(
        stream_deltas,
        vec![
            (
                "tool_1".to_string(),
                internal.clone(),
                ToolCallDeltaContent::Name("add".to_string())
            ),
            (
                "tool_1".to_string(),
                internal.clone(),
                ToolCallDeltaContent::Delta("{\"x\":".to_string())
            ),
            (
                "tool_1".to_string(),
                internal.clone(),
                ToolCallDeltaContent::Delta("1}".to_string())
            ),
        ]
    );
}

#[tokio::test]
async fn stream_prompt_tool_call_deltas_hook_termination_prevents_delta_emit() {
    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::tool_call_name_delta("tool_1", "add"),
        MockStreamEvent::tool_call_arguments_delta("tool_1", "{\"x\":"),
        MockStreamEvent::final_response_with_total_tokens(3),
    ]]);
    let hook = TerminatingToolCallDeltaHook::default();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("stream a tool call")
        .add_hook(hook.clone())
        .await;
    let mut saw_delta = false;
    let mut saw_final_response = false;
    let mut error_message = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::ToolCallDelta { .. },
            )) => {
                saw_delta = true;
            }
            Ok(MultiTurnStreamItem::FinalResponse(_)) => {
                saw_final_response = true;
            }
            Ok(_) => {}
            Err(err) => {
                error_message = Some(err.to_string());
                break;
            }
        }
    }

    // Internal ids are minted by the shared accumulator; assert presence,
    // not a scripted literal.
    let observed = hook.observed();
    assert_eq!(observed.len(), 1);
    let first = observed.first().expect("one observed delta");
    assert_eq!(first.0, "tool_1");
    assert!(!first.1.is_empty());
    assert_eq!(first.2, Some("add".to_string()));
    assert_eq!(first.3, String::new());
    assert!(!saw_delta);
    assert!(!saw_final_response);
    assert!(
        error_message
            .as_deref()
            .is_some_and(|message| message.contains("PromptCancelled: stop on tool call delta")),
        "expected hook termination error, got {error_message:?}"
    );
}

#[tokio::test]
async fn stream_prompt_exposes_completion_calls() {
    let first_call_usage = usage(10, 2);
    let second_call_usage = usage(25, 5);
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tool_call_1", "add", serde_json::json!({"x": 1, "y": 2}))
                .with_call_id("call_1"),
            MockStreamEvent::final_response(first_call_usage),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response(second_call_usage),
        ],
    ]);
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();
    let empty_history: &[Message] = &[];

    let mut stream = agent
        .stream_prompt("do tool work")
        .history(empty_history)
        .max_turns(3)
        .await;
    let mut completion_calls_events = Vec::new();
    let mut final_response = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::CompletionCall(call_usage)) => {
                completion_calls_events.push(call_usage);
            }
            Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                final_response = Some(response);
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    assert_eq!(
        completion_calls_events,
        vec![
            CompletionCall::new(0, first_call_usage),
            CompletionCall::new(1, second_call_usage)
        ]
    );

    let final_response = final_response.expect("expected final response");
    assert_eq!(
        final_response.usage(),
        Usage {
            input_tokens: 35,
            output_tokens: 7,
            total_tokens: 42,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_creation_1h_input_tokens: 0,
            tool_use_prompt_tokens: 0,
            reasoning_tokens: 0,
        }
    );
    assert_eq!(
        final_response.completion_calls(),
        &[
            CompletionCall::new(0, first_call_usage),
            CompletionCall::new(1, second_call_usage)
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn stream_prompt_records_single_call_usage_on_chat_span_under_outer_span() {
    let call_usage = usage(10, 2);
    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("done"),
        MockStreamEvent::final_response(call_usage),
    ]]);
    let agent = AgentBuilder::new(model).build();

    assert_stream_usage_recorded_on_chat_spans(agent, "say done", 1, &[call_usage]).await;
}

#[tokio::test(flavor = "current_thread")]
async fn stream_prompt_records_multi_turn_usage_on_chat_spans_under_outer_span() {
    let first_call_usage = usage(10, 2);
    let second_call_usage = usage(25, 5);
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tool_call_1", "add", serde_json::json!({"x": 1, "y": 2}))
                .with_call_id("call_1"),
            MockStreamEvent::final_response(first_call_usage),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response(second_call_usage),
        ],
    ]);
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    assert_stream_usage_recorded_on_chat_spans(
        agent,
        "do tool work",
        3,
        &[first_call_usage, second_call_usage],
    )
    .await;
}

#[tokio::test]
async fn stream_prompt_emits_completion_call_before_finish_hook_termination() {
    let call_usage = usage(10, 2);
    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("done"),
        MockStreamEvent::final_response(call_usage),
    ]]);
    let agent = AgentBuilder::new(model).build();

    let mut stream = agent
        .stream_prompt("say done")
        .add_hook(TerminateOnStreamFinish)
        .await;
    let mut completion_calls = Vec::new();
    let mut saw_error = false;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::CompletionCall(completion_call)) => {
                completion_calls.push(completion_call);
            }
            Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                panic!("unexpected final response after hook termination: {response:?}");
            }
            Ok(_) => {}
            Err(_) => {
                saw_error = true;
                break;
            }
        }
    }

    assert_eq!(completion_calls, vec![CompletionCall::new(0, call_usage)]);
    assert!(saw_error);
}

#[tokio::test]
async fn stream_prompt_completion_calls_records_unreported_usage() {
    let second_call_usage = usage(25, 5);
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tool_call_1", "add", serde_json::json!({"x": 1, "y": 2}))
                .with_call_id("call_1"),
            // A genuine terminal whose usage is unreported: the completion
            // call records the zero-usage sentinel. (A turn with no
            // terminal at all is rejected as truncation instead.)
            MockStreamEvent::final_response(Usage::new()),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response(second_call_usage),
        ],
    ]);
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();
    let empty_history: &[Message] = &[];

    let mut stream = agent
        .stream_prompt("do tool work")
        .history(empty_history)
        .max_turns(3)
        .await;
    let mut completion_calls_events = Vec::new();
    let mut final_response = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::CompletionCall(call_usage)) => {
                completion_calls_events.push(call_usage);
            }
            Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                final_response = Some(response);
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    let expected_usage = vec![
        CompletionCall::new(0, Usage::new()),
        CompletionCall::new(1, second_call_usage),
    ];
    assert_eq!(completion_calls_events, expected_usage);

    let final_response = final_response.expect("expected final response");
    assert_eq!(final_response.completion_calls(), expected_usage.as_slice());
}

#[tokio::test]
async fn final_response_matches_streamed_text_when_provider_final_is_textless() {
    let agent = AgentBuilder::new(streaming_text_then_final_model()).build();

    let mut stream = agent.stream_prompt("say hello").await;
    let mut streamed_text = String::new();
    let mut final_response_text = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                streamed_text.push_str(&text.text)
            }
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                final_response_text = Some(res.output().to_owned());
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    assert_eq!(streamed_text, "hello world");
    assert_eq!(final_response_text.as_deref(), Some("hello world"));
}

#[tokio::test]
async fn final_response_preserves_structured_text_metadata() {
    let agent = AgentBuilder::new(streaming_cited_text_then_final_model()).build();

    let mut stream = agent.stream_prompt("answer with citations").await;
    let mut final_response = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                final_response = Some(res);
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    let final_response = final_response.expect("expected final response");
    assert_eq!(final_response.output(), "cited answer");
    let metadata =
        text_metadata(final_response.content()).expect("expected text metadata in final content");
    assert_eq!(
        metadata["citations"][0]["encrypted_index"],
        "encrypted-reference"
    );
}

#[tokio::test]
async fn final_response_history_preserves_structured_text_metadata() {
    let agent = AgentBuilder::new(streaming_cited_text_then_final_model()).build();

    let empty_history: &[Message] = &[];
    let mut stream = agent
        .stream_prompt("answer with citations")
        .history(empty_history)
        .await;
    let mut final_response = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                final_response = Some(res);
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    let final_response = final_response.expect("expected final response");
    let history = final_response
        .messages()
        .expect("with_history should include final history");
    let assistant_content = history
        .iter()
        .find_map(|message| match message {
            Message::Assistant { content, .. } => Some(content),
            _ => None,
        })
        .expect("expected assistant message in history");
    let metadata =
        text_metadata(assistant_content).expect("expected text metadata in assistant history");
    assert_eq!(
        metadata["citations"][0]["encrypted_index"],
        "encrypted-reference"
    );
}

#[tokio::test]
async fn tool_follow_up_history_preserves_structured_text_metadata() {
    let model = streaming_cited_text_then_tool_model();
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();
    let empty_history: &[Message] = &[];

    let mut stream = agent
        .stream_prompt("use a tool with citations")
        .history(empty_history)
        .max_turns(3)
        .await;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::FinalResponse(_)) => break,
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    let requests = recorded.requests();
    assert_eq!(requests.len(), 2);
    let follow_up_history = requests[1].chat_history.iter().collect::<Vec<_>>();
    let assistant_content = follow_up_history
        .iter()
        .find_map(|message| match message {
            Message::Assistant { content, .. } => Some(content),
            _ => None,
        })
        .expect("expected assistant message in follow-up history");
    let metadata = text_metadata(assistant_content)
        .expect("expected citation metadata in follow-up assistant history");
    assert_eq!(
        metadata["citations"][0]["encrypted_index"],
        "encrypted-reference"
    );
}

#[tokio::test]
async fn final_response_can_remain_empty_for_truly_textless_turns() {
    let agent = AgentBuilder::new(streaming_final_only_model()).build();

    let mut stream = agent.stream_prompt("say nothing").await;
    let mut streamed_text = String::new();
    let mut final_response_text = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                streamed_text.push_str(&text.text)
            }
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                final_response_text = Some(res.output().to_owned());
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    assert!(streamed_text.is_empty());
    assert_eq!(final_response_text.as_deref(), Some(""));
}

/// Background task that logs periodically to detect span leakage.
/// If span leakage occurs, these logs will be prefixed with `invoke_agent{...}`.
async fn background_logger(stop: Arc<AtomicBool>, leak_count: Arc<AtomicU32>) {
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    let mut count = 0u32;

    while !stop.load(Ordering::Relaxed) {
        interval.tick().await;
        count += 1;

        tracing::event!(
            target: "background_logger",
            tracing::Level::INFO,
            count = count,
            "Background tick"
        );

        // Check if we're inside an unexpected span
        let current = tracing::Span::current();
        if !current.is_disabled() && !current.is_none() {
            leak_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    tracing::info!(target: "background_logger", total_ticks = count, "Background logger stopped");
}

/// Test that span context doesn't leak to concurrent tasks during streaming.
///
/// This test verifies that using `.instrument()` instead of `span.enter()` in
/// async_stream prevents thread-local span context from leaking to other tasks.
///
/// Uses single-threaded runtime to force all tasks onto the same thread,
/// making the span leak deterministic (it only occurs when tasks share a thread).
#[tokio::test(flavor = "current_thread")]
#[ignore = "This requires an API key"]
async fn test_span_context_isolation() -> anyhow::Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let leak_count = Arc::new(AtomicU32::new(0));

    // Start background logger
    let bg_stop = stop.clone();
    let bg_leak = leak_count.clone();
    let bg_handle = tokio::spawn(async move {
        background_logger(bg_stop, bg_leak).await;
    });

    // Small delay to let background logger start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Make streaming request WITHOUT an outer span so rig creates its own invoke_agent span
    // (rig reuses current span if one exists, so we need to ensure there's no current span)
    let client = anthropic::Client::from_env()?;
    let agent = client
        .agent("claude-haiku-4-5")
        .preamble("You are a helpful assistant.")
        .temperature(0.1)
        .max_tokens(100)
        .build();

    let mut stream = agent
        .stream_prompt("Say 'hello world' and nothing else.")
        .await;

    let mut full_content = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                full_content.push_str(&text.text);
            }
            Ok(MultiTurnStreamItem::FinalResponse(_)) => {
                break;
            }
            Err(e) => {
                tracing::warn!("Error: {:?}", e);
                break;
            }
            _ => {}
        }
    }

    tracing::info!("Got response: {:?}", full_content);

    // Stop background logger
    stop.store(true, Ordering::Relaxed);
    bg_handle.await?;

    let leaks = leak_count.load(Ordering::Relaxed);
    anyhow::ensure!(
        leaks == 0,
        "SPAN LEAK DETECTED: Background logger was inside unexpected spans {leaks} times. \
             This indicates that span.enter() is being used inside async_stream instead of .instrument()"
    );

    Ok(())
}

/// Test that FinalResponse contains the updated chat history when a starting
/// history is provided via `.history(..)`.
///
/// This verifies that:
/// 1. PromptResponse.messages() returns Some when a starting history was provided
/// 2. The history contains both the user prompt and assistant response
#[tokio::test]
#[ignore = "This requires an API key"]
async fn test_chat_history_in_final_response() -> anyhow::Result<()> {
    use rig_core::message::Message;

    let client = anthropic::Client::from_env()?;
    let agent = client
        .agent("claude-haiku-4-5")
        .preamble("You are a helpful assistant. Keep responses brief.")
        .temperature(0.1)
        .max_tokens(50)
        .build();

    // Send streaming request with history
    let empty_history: &[Message] = &[];
    let mut stream = agent
        .stream_prompt("Say 'hello' and nothing else.")
        .history(empty_history)
        .await;

    // Consume the stream and collect FinalResponse
    let mut response_text = String::new();
    let mut final_history = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                response_text.push_str(&text.text);
            }
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                final_history = res.messages().map(|h| h.to_vec());
                break;
            }
            Err(e) => {
                return Err(e.into());
            }
            _ => {}
        }
    }

    let history =
        final_history.ok_or_else(|| anyhow::anyhow!("final response should include history"))?;

    // Should contain at least the user message
    anyhow::ensure!(
        history.iter().any(|m| matches!(m, Message::User { .. })),
        "History should contain the user message"
    );

    // Should contain the assistant response
    anyhow::ensure!(
        history
            .iter()
            .any(|m| matches!(m, Message::Assistant { .. })),
        "History should contain the assistant response"
    );

    tracing::info!(
        "History after streaming: {} messages, response: {:?}",
        history.len(),
        response_text
    );

    Ok(())
}

#[tokio::test]
async fn streaming_appends_to_memory_after_final_response() {
    use rig_core::memory::{ConversationMemory, InMemoryConversationMemory};

    let memory = InMemoryConversationMemory::new();
    let agent = AgentBuilder::new(streaming_text_then_final_model())
        .memory(memory.clone())
        .build();

    let mut stream = agent
        .stream_prompt("hi there")
        .conversation("stream-thread")
        .await;

    let mut history_in_final = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                history_in_final = res.messages().map(|h| h.to_vec());
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    let final_history = history_in_final
        .expect("PromptResponse.messages should be populated when memory is configured");
    assert_eq!(
        final_history.len(),
        2,
        "user prompt + assistant response in final history: {final_history:?}"
    );

    let stored = memory.load("stream-thread").await.unwrap();
    assert_eq!(stored.len(), 2, "memory should contain user + assistant");
}

#[tokio::test]
async fn streaming_reasoning_without_tools_does_not_duplicate_final_history() {
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("final answer"),
        MockStreamEvent::reasoning("reasoned step").with_reasoning_id("rs_1"),
        MockStreamEvent::final_response_with_total_tokens(3),
    ]]))
    .build();

    let mut stream = agent
        .stream_prompt("think before answering")
        .history(Vec::<Message>::new())
        .await;

    let mut history_in_final = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                history_in_final = res.messages().map(|h| h.to_vec());
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    let final_history = history_in_final
        .expect("PromptResponse.messages should be populated when with_history is used");
    assert_eq!(
        final_history.len(),
        2,
        "user prompt + one assistant response in final history: {final_history:?}"
    );

    assert!(matches!(
        final_history.first(),
        Some(Message::User { content })
            if matches!(
                content.first(),
                UserContent::Text(text) if text.text == "think before answering"
            )
    ));

    let assistant_messages = final_history
        .iter()
        .filter_map(|message| match message {
            Message::Assistant { content, .. } => Some(content),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        assistant_messages.len(),
        1,
        "reasoning turn should produce exactly one assistant history message: {final_history:?}"
    );
    let assistant_content = assistant_messages
        .first()
        .expect("expected assistant history message");
    assert!(assistant_content.iter().any(|item| matches!(
        item,
        AssistantContent::Text(text) if text.text == "final answer"
    )));
    assert!(assistant_content.iter().any(|item| matches!(
        item,
        AssistantContent::Reasoning(reasoning)
            if reasoning.id.as_deref() == Some("rs_1")
                && reasoning.content.iter().any(|content| matches!(
                    content,
                    ReasoningContent::Text { text, .. } if text == "reasoned step"
                ))
    )));
    let reasoning_index = assistant_content
        .iter()
        .position(|item| matches!(item, AssistantContent::Reasoning(_)))
        .expect("assistant history should contain reasoning");
    let text_index = assistant_content
        .iter()
        .position(|item| matches!(item, AssistantContent::Text(_)))
        .expect("assistant history should contain text");
    assert!(
        reasoning_index < text_index,
        "assistant reasoning must be stored before assistant text: {assistant_content:?}"
    );
}

#[tokio::test]
async fn streaming_with_history_overrides_memory() {
    use rig_core::memory::{ConversationMemory, InMemoryConversationMemory};

    let memory = InMemoryConversationMemory::new();
    memory
        .append("t1", vec![Message::user("from-memory")])
        .await
        .unwrap();

    let agent = AgentBuilder::new(streaming_text_then_final_model())
        .memory(memory.clone())
        .build();

    let mut stream = agent
        .stream_prompt("hi")
        .conversation("t1")
        .history(vec![Message::user("from-caller")])
        .await;

    while let Some(item) = stream.next().await {
        if let Ok(MultiTurnStreamItem::FinalResponse(_)) = item {
            break;
        }
    }

    let stored = memory.load("t1").await.unwrap();
    assert_eq!(
        stored.len(),
        1,
        "with_history bypasses memory; only the pre-seeded entry remains: {stored:?}"
    );
}

#[tokio::test]
async fn streaming_without_memory_disables_for_request() {
    use rig_core::memory::{ConversationMemory, InMemoryConversationMemory};

    let memory = InMemoryConversationMemory::new();
    let agent = AgentBuilder::new(streaming_text_then_final_model())
        .memory(memory.clone())
        .conversation("default")
        .build();

    let mut stream = agent.stream_prompt("hi").without_memory().await;

    while let Some(item) = stream.next().await {
        if let Ok(MultiTurnStreamItem::FinalResponse(_)) = item {
            break;
        }
    }

    let stored = memory.load("default").await.unwrap();
    assert!(stored.is_empty(), "without_memory disables save");
}

#[tokio::test]
async fn streaming_load_error_yields_memory_error() {
    let agent = AgentBuilder::new(streaming_text_then_final_model())
        .memory(FailingMemory::default())
        .build();

    let mut stream = agent.stream_prompt("hi").conversation("t1").await;

    let first = stream.next().await.expect("at least one item");
    match first {
        Err(StreamingError::Prompt(err)) => match *err {
            PromptError::MemoryError(err) => {
                assert!(err.to_string().contains("load boom"));
            }
            other => panic!("expected PromptError::MemoryError, got {other:?}"),
        },
        other => panic!("expected StreamingError::Prompt, got {other:?}"),
    }
}

#[tokio::test]
async fn streaming_with_filter_shapes_loaded_history() {
    use rig_core::memory::{ConversationMemory, InMemoryConversationMemory};

    let memory = InMemoryConversationMemory::new()
        .with_filter(|msgs: Vec<Message>| msgs.into_iter().rev().take(2).rev().collect());
    memory
        .append(
            "t1",
            vec![
                Message::user("1"),
                Message::assistant("2"),
                Message::user("3"),
                Message::assistant("4"),
            ],
        )
        .await
        .unwrap();

    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("ok"),
        MockStreamEvent::final_response_with_total_tokens(1),
    ]]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).memory(memory).build();

    let mut stream = agent.stream_prompt("ping").conversation("t1").await;
    while let Some(item) = stream.next().await {
        if let Ok(MultiTurnStreamItem::FinalResponse(_)) = item {
            break;
        }
    }

    let received = recorded.requests()[0]
        .chat_history
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        received.len(),
        3,
        "window-truncated history (2) + current prompt: {received:?}"
    );
}

#[tokio::test]
async fn streaming_append_error_does_not_suppress_final_response() {
    let agent = AgentBuilder::new(streaming_text_then_final_model())
        .memory(AppendFailingMemory::default())
        .build();

    let mut stream = agent.stream_prompt("hi").conversation("t1").await;

    let mut saw_final = false;
    while let Some(item) = stream.next().await {
        if let Ok(MultiTurnStreamItem::FinalResponse(_)) = item {
            saw_final = true;
            break;
        }
    }
    assert!(
        saw_final,
        "FinalResponse must be yielded even when memory.append fails"
    );
}

#[test]
fn final_response_constructors_surface_content_usage_and_history() {
    let item = MultiTurnStreamItem::final_response(
        OneOrMany::one(AssistantContent::text("done")),
        usage(1, 2),
    );
    let MultiTurnStreamItem::FinalResponse(response) = item else {
        panic!("expected a final response item, got {item:?}");
    };
    assert_eq!(response.output(), "done");
    assert_eq!(response.usage(), usage(1, 2));
    assert_eq!(response.messages(), None);
    assert!(response.completion_calls().is_empty());

    let history = vec![Message::user("hi"), Message::assistant("hello")];
    let item = MultiTurnStreamItem::final_response_with_history(
        OneOrMany::one(AssistantContent::text("done")),
        usage(3, 4),
        Some(history.clone()),
    );
    let MultiTurnStreamItem::FinalResponse(response) = item else {
        panic!("expected a final response item, got {item:?}");
    };
    assert_eq!(response.usage(), usage(3, 4));
    assert_eq!(response.messages(), Some(&history[..]));

    let item = MultiTurnStreamItem::final_response_with_history(
        OneOrMany::one(AssistantContent::text("done")),
        usage(3, 4),
        None,
    );
    let MultiTurnStreamItem::FinalResponse(response) = item else {
        panic!("expected a final response item, got {item:?}");
    };
    assert_eq!(response.messages(), None);
}

#[tokio::test]
async fn stream_to_stdout_handles_reasoning_retries_and_errors() {
    let mut reasoning = rig_core::message::Reasoning::new("");
    reasoning.content = vec![ReasoningContent::Text {
        text: "thinking".to_string(),
        signature: None,
    }];
    let items = vec![
        Ok(MultiTurnStreamItem::StreamAssistantItem(
            StreamedAssistantContent::Text(rig_core::message::Text {
                text: "answer".to_string(),
                additional_params: None,
            }),
        )),
        Ok(MultiTurnStreamItem::StreamAssistantItem(
            StreamedAssistantContent::Reasoning(reasoning),
        )),
        Ok(MultiTurnStreamItem::ModelTurnRetried { turn: 1 }),
        Ok(MultiTurnStreamItem::CompletionCall(CompletionCall::new(
            0,
            Usage::new(),
        ))),
        Err(StreamingError::Completion(CompletionError::ResponseError(
            "boom".to_string(),
        ))),
        Ok(MultiTurnStreamItem::FinalResponse(PromptResponse::new(
            "done",
            Usage::new(),
        ))),
    ];
    let mut stream: StreamingResult = futures::stream::iter(items).boxed();

    let response = stream_to_stdout(&mut stream)
        .await
        .expect("stdout writes should succeed");

    assert_eq!(response.output(), "done");
}

#[tokio::test]
async fn streaming_skip_drains_trailing_events_before_final_usage() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("checking "),
            MockStreamEvent::tool_call(
                "tool_call_1",
                "default_api",
                serde_json::json!({"x": 2, "y": 3}),
            ),
            // Trailing events after the abandoned tool call: draining them
            // must still surface the terminal record's usage rather than
            // dropping it.
            MockStreamEvent::text("trailing "),
            MockStreamEvent::reasoning("more thinking"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("continued"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(SkipDefaultApiHook)
        .max_turns(3)
        .history(Vec::<Message>::new())
        .await;
    let mut completion_calls = Vec::new();
    let mut final_output = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::CompletionCall(call)) => completion_calls.push(call),
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                final_output = Some(res.output().to_string());
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    assert_eq!(final_output.as_deref(), Some("continued"));
    assert_eq!(
        completion_calls.len(),
        2,
        "one completion call per model turn"
    );
    assert_eq!(
        completion_calls[0].usage.total_tokens, 4,
        "the abandoned turn's terminal-record usage must survive the drain"
    );
    assert_eq!(completion_calls[1].usage.total_tokens, 6);
    assert_eq!(recorded.request_count(), 2);
}

#[tokio::test]
async fn streaming_skip_drain_surfaces_provider_error_after_abandon() {
    let model = MockCompletionModel::from_stream_turns([
        // The provider errors after the abandoned tool call: the drain
        // must propagate that error instead of reporting a silently
        // zero-usage completion.
        vec![
            MockStreamEvent::tool_call(
                "tool_call_1",
                "default_api",
                serde_json::json!({"x": 2, "y": 3}),
            ),
            MockStreamEvent::error("post-abandon boom"),
        ],
        vec![
            MockStreamEvent::text("should not be requested"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(SkipDefaultApiHook)
        .max_turns(3)
        .history(Vec::<Message>::new())
        .await;
    let mut error = None;
    while let Some(item) = stream.next().await {
        if let Err(err) = item {
            error = Some(err);
            break;
        }
    }

    let error = error.expect("an error during the post-abandon drain must surface");
    assert!(
        error.to_string().contains("post-abandon boom"),
        "expected the drained provider error, got: {error}"
    );
    assert_eq!(recorded.request_count(), 1);
}

#[tokio::test]
async fn streaming_skip_after_truncated_stream_reports_zero_drained_usage() {
    let model = MockCompletionModel::from_stream_turns([
        // The provider stream truncates right after the invalid tool call:
        // no terminal record ever arrives, so the drain must fall back to
        // zero usage instead of hanging or misreporting.
        vec![MockStreamEvent::tool_call(
            "tool_call_1",
            "default_api",
            serde_json::json!({"x": 2, "y": 3}),
        )],
        vec![
            MockStreamEvent::text("continued"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let recorded = model.clone();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("use the tool")
        .add_hook(SkipDefaultApiHook)
        .max_turns(3)
        .history(Vec::<Message>::new())
        .await;
    let mut completion_calls = Vec::new();
    let mut skipped_tool_result = false;
    let mut final_output = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::CompletionCall(call)) => completion_calls.push(call),
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult { .. })) => {
                skipped_tool_result = true
            }
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                final_output = Some(res.output().to_string());
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    assert_eq!(final_output.as_deref(), Some("continued"));
    assert!(
        skipped_tool_result,
        "skip recovery emits the synthetic result"
    );
    assert_eq!(completion_calls.len(), 2);
    assert_eq!(
        completion_calls[0].usage,
        Usage::new(),
        "a truncated drain reports zero usage for the abandoned turn"
    );
    assert_eq!(completion_calls[1].usage.total_tokens, 6);
    assert_eq!(recorded.request_count(), 2);
}

#[tokio::test]
async fn streaming_skip_mixed_turn_at_higher_concurrency_preresolves_without_execution() {
    let add_calls = Arc::new(AtomicU32::new(0));
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tool_call_1", "add", serde_json::json!({"x": 1, "y": 2}))
                .with_call_id("call_1"),
            MockStreamEvent::tool_call(
                "tool_call_2",
                "default_api",
                serde_json::json!({"x": 3, "y": 4}),
            )
            .with_call_id("call_2"),
            MockStreamEvent::final_response_with_total_tokens(4),
        ],
        vec![
            MockStreamEvent::text("skipped"),
            MockStreamEvent::final_response_with_total_tokens(6),
        ],
    ]);
    let agent = AgentBuilder::new(model)
        .tool(CountingAddTool {
            calls: add_calls.clone(),
        })
        .build();

    let mut stream = agent
        .stream_prompt("use tools")
        .add_hook(SkipDefaultApiHook)
        .tool_concurrency(2)
        .max_turns(3)
        .history(Vec::<Message>::new())
        .await;
    let mut skipped_results = Vec::new();
    let mut final_output = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                tool_result,
                ..
            })) => skipped_results.push(tool_result),
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                final_output = Some(res.output().to_string());
                break;
            }
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    assert_eq!(final_output.as_deref(), Some("skipped"));
    assert_eq!(
        add_calls.load(Ordering::SeqCst),
        0,
        "the valid peer must not execute during skip recovery, at any concurrency"
    );
    // The invalid call's synthetic result is surfaced during the model
    // turn; the valid peer is preresolved with a synthetic result that is
    // committed to history only, so exactly one result is streamed.
    assert_eq!(
        skipped_results.len(),
        1,
        "only the invalid call's skip feedback is streamed: {skipped_results:?}"
    );
    assert!(matches!(
        skipped_results.first(),
        Some(result) if result.id == "tool_call_2"
            && result.content.iter().any(|content| matches!(
                content,
                ToolResultContent::Text(text) if text.text == "default_api was skipped"
            ))
    ));
}

/// Shared sink for [`streaming_empty_final_turn_logs_a_warning_under_a_subscriber`].
#[derive(Clone, Default)]
struct SharedLogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for SharedLogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer mutex was poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLogBuffer {
    type Writer = SharedLogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn streaming_empty_final_turn_logs_a_warning_under_a_subscriber() {
    let _isolation = crate::test_utils::scoped_tracing_subscriber_guard().await;
    let buffer = SharedLogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(buffer.clone())
        .finish();
    let _default = tracing::subscriber::set_default(subscriber);

    // Same callsite-interest hazard the span-capture harness documents:
    // warm the textless-turn path from THIS thread (registering the warn
    // callsite against this subscriber), then heal any cache a foreign
    // thread poisoned, then clear the buffer before the observed run.
    let warmup = AgentBuilder::new(streaming_final_only_model()).build();
    let mut warmup_stream = warmup.stream_prompt("warmup").max_turns(1).await;
    while let Some(item) = warmup_stream
        .try_next()
        .await
        .expect("warmup stream should not error")
    {
        if matches!(item, MultiTurnStreamItem::FinalResponse(_)) {
            break;
        }
    }
    tracing::callsite::rebuild_interest_cache();
    buffer
        .0
        .lock()
        .expect("log buffer mutex was poisoned")
        .clear();

    let agent = AgentBuilder::new(streaming_final_only_model()).build();
    let mut stream = agent.stream_prompt("say nothing").max_turns(1).await;
    while let Some(item) = stream
        .try_next()
        .await
        .expect("textless stream should not error")
    {
        if matches!(item, MultiTurnStreamItem::FinalResponse(_)) {
            break;
        }
    }

    let logged = String::from_utf8_lossy(&buffer.0.lock().expect("log buffer mutex was poisoned"))
        .to_string();
    assert!(
        logged.contains("Streaming turn completed without assistant text"),
        "expected the textless-turn warning to be logged, got: {logged}"
    );
}

/// A steering source that becomes pending when the test arms it — steering
/// is only ever submitted while a run is in flight, so the queue starts
/// empty and the test flips `armed` mid-run (deterministically, from the
/// item stream the test itself drives).
struct ArmedSteers {
    armed: std::sync::atomic::AtomicBool,
    pending: std::sync::Mutex<Vec<Message>>,
}

impl ArmedSteers {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            armed: std::sync::atomic::AtomicBool::new(false),
            pending: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn submit(&self, text: &str) {
        self.pending
            .lock()
            .expect("steer lock")
            .push(Message::user(text));
    }

    fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl crate::agent::SteeringSource for ArmedSteers {
    fn has_pending(&self) -> bool {
        self.armed.load(std::sync::atomic::Ordering::SeqCst)
            && !self.pending.lock().expect("steer lock").is_empty()
    }

    fn drain(&self) -> Vec<Message> {
        std::mem::take(&mut self.pending.lock().expect("steer lock"))
    }
}

#[tokio::test]
async fn steering_after_a_final_turn_gets_another_model_call() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("first answer"),
            MockStreamEvent::final_response(Usage::new()),
        ],
        vec![
            MockStreamEvent::text("steered answer"),
            MockStreamEvent::final_response(Usage::new()),
        ],
    ]);
    let agent = Arc::new(AgentBuilder::new(model.clone()).build());
    let steers = ArmedSteers::new();

    let stream = StreamingPromptRequest::new(agent, "go")
        .max_turns(4)
        .steering(steers.clone())
        .await;
    let mut items = Vec::new();
    let mut submitted = false;
    let mut stream = Box::pin(stream);
    while let Some(item) = stream.next().await {
        let item = item.expect("stream item");
        // Submit the steer while the first turn is streaming — before its
        // turn end can drain it.
        if !submitted
            && matches!(
                &item,
                MultiTurnStreamItem::StreamAssistantItem(
                    crate::streaming::StreamedAssistantContent::Text(_),
                )
            )
        {
            steers.submit("and also?");
            steers.arm();
            submitted = true;
        }
        items.push(item);
    }

    assert_eq!(model.request_count(), 2, "the steer drove a second call");
    let steered: Vec<&String> = items
        .iter()
        .filter_map(|item| match item {
            MultiTurnStreamItem::Steer { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(steered, [&"and also?".to_string()]);
    let final_text = items
        .iter()
        .find_map(|item| match item {
            MultiTurnStreamItem::FinalResponse(response) => Some(response.output.clone()),
            _ => None,
        })
        .expect("final response");
    assert_eq!(final_text, "steered answer");
}
