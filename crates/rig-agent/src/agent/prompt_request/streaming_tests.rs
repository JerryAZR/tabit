use super::*;
use crate::agent::AgentBuilder;
use crate::agent::hook::{AgentHook, HookContext, ToolCall as ToolCallEvent, ToolCallAction};
use crate::agent::prompt_request::tool_result_output;
use crate::agent::run::streamed::merge_reasoning_blocks;
use crate::client::AgentClientExt;
use crate::completion::{CompletionRequest, Prompt, ToolDefinition, Usage};
use crate::streaming::{StreamingPrompt, ToolCallDeltaContent};
use crate::test_utils::{
    MockAddTool, MockBarrierTool, MockCompletionModel, MockContextProbeTool, MockError,
    MockStreamEvent, MockToolError, MockTurn, SessionId,
};
use crate::tool::{Tool, ToolContext};
use futures::{StreamExt, TryStreamExt};
use rig_core::client::ProviderClient;
use rig_core::message::{
    AssistantContent, DocumentSourceKind, ImageMediaType, Message, ReasoningContent,
    ToolResultContent, UserContent,
};
use rig_core::providers::anthropic;
use serde::Deserialize;
use std::collections::HashMap;
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

struct CountingToolCallHook(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl AgentHook for CountingToolCallHook {
    async fn on_tool_call(&self, _ctx: &HookContext, _event: ToolCallEvent<'_>) -> ToolCallAction {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ToolCallAction::run()
    }
}

#[tokio::test]
async fn public_streaming_request_constructor_preserves_agent_hooks() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tool_call_1", "add", serde_json::json!({"x": 1, "y": 2}))
                .with_call_id("call_1"),
            MockStreamEvent::final_response(Usage::new()),
        ],
        vec![
            MockStreamEvent::text("answer"),
            MockStreamEvent::final_response(Usage::new()),
        ],
    ]);
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let agent = Arc::new(
        AgentBuilder::new(model.clone())
            .tool(MockAddTool)
            .add_hook(CountingToolCallHook(seen.clone()))
            .build(),
    );

    let mut stream = StreamingPromptRequest::new(agent, "go").max_turns(2).await;
    let mut finished = false;
    while let Some(item) = stream.next().await {
        if matches!(
            item.expect("stream item"),
            crate::agent::MultiTurnStreamItem::FinalResponse(_)
        ) {
            finished = true;
        }
    }
    assert!(finished, "the run completes");
    assert_eq!(model.request_count(), 2);
    assert_eq!(
        seen.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the constructor preserved the agent's hook stack"
    );
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

/// A scripted [`SteeringSource`]: messages queued up front, drained once.
#[derive(Default)]
struct ScriptedSteers {
    queue: Mutex<std::collections::VecDeque<Message>>,
}

impl ScriptedSteers {
    fn with_texts(texts: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(
                texts
                    .iter()
                    .map(|text| Message::user(text.to_string()))
                    .collect(),
            ),
        })
    }
}

impl crate::agent::runner::SteeringSource for ScriptedSteers {
    fn drain(&self) -> Vec<(String, Message)> {
        self.queue
            .lock()
            .expect("steer queue")
            .drain(..)
            .map(|message| (rig_core::id::generate(), message))
            .collect()
    }
}

/// The user-text sequence one request carried, for asserting what the model
/// actually saw.
fn request_user_texts(request: &CompletionRequest) -> Vec<String> {
    request
        .chat_history
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => content.iter().find_map(|part| match part {
                UserContent::Text(text) => Some(text.text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect()
}

/// A scripted [`SteeringSource`] releasing one message per drain — the
/// engine-visible shape of a user steering once per turn boundary.
struct OneSteerAtATime {
    released: Mutex<usize>,
    remaining: Mutex<usize>,
}

impl crate::agent::runner::SteeringSource for OneSteerAtATime {
    fn drain(&self) -> Vec<(String, Message)> {
        let mut remaining = self.remaining.lock().expect("steer remaining");
        if *remaining == 0 {
            return Vec::new();
        }
        *remaining -= 1;
        let mut released = self.released.lock().expect("steer released");
        *released += 1;
        vec![(
            rig_core::id::generate(),
            Message::user(format!("steer {}", *released)),
        )]
    }
}

/// A steer drained at a defect boundary resets the streak: each steering
/// message buys the model a fresh retry, so an actively steered run never
/// exhausts — exhaustion bounds runs the user has gone silent on.
#[tokio::test]
async fn steers_reset_the_defect_streak_each_buying_another_retry() {
    use crate::streaming::StreamingChat;

    let model = MockCompletionModel::from_stream_turns([
        vec![MockStreamEvent::Error(MockError::malformed_tool_call(
            "lookup",
            "arguments arrived truncated",
        ))],
        vec![MockStreamEvent::Error(MockError::malformed_tool_call(
            "lookup",
            "arguments arrived truncated again",
        ))],
        vec![
            MockStreamEvent::text("recovered"),
            MockStreamEvent::final_response(Usage::new()),
        ],
    ]);
    let agent = Arc::new(AgentBuilder::new(model.clone()).build());

    let mut stream = agent
        .stream_chat(vec![Message::user("go")])
        .steering(Arc::new(OneSteerAtATime {
            released: Mutex::new(0),
            remaining: Mutex::new(2),
        }))
        .await;
    let mut finished = false;
    while let Some(item) = stream.next().await {
        if let Ok(crate::agent::MultiTurnStreamItem::FinalResponse(_)) = item {
            finished = true;
        }
    }
    assert!(
        finished,
        "each steer resets the streak; the third attempt recovers"
    );
    assert_eq!(model.request_count(), 3, "defect, defect, recovery");
    assert_eq!(
        request_user_texts(&model.requests()[2]),
        ["go", "steer 1", "steer 2"],
        "each drained steer rides along and stays in the retried history"
    );
}

/// A malformed tool call discards the turn and retries the identical
/// request — and the discarded attempt does not consume the turn budget
/// (max_turns 1 still allows the retry to complete the run).
#[tokio::test]
async fn malformed_tool_call_discards_the_turn_and_retries_within_budget() {
    use crate::streaming::StreamingChat;

    let model = MockCompletionModel::from_stream_turns([
        vec![MockStreamEvent::Error(MockError::malformed_tool_call(
            "lookup",
            "arguments arrived truncated",
        ))],
        vec![
            MockStreamEvent::text("recovered"),
            MockStreamEvent::final_response(Usage::new()),
        ],
    ]);
    let agent = Arc::new(AgentBuilder::new(model.clone()).build());

    let mut stream = agent
        .stream_chat(vec![Message::user("go")])
        .max_turns(1)
        .await;
    let mut finished = false;
    while let Some(item) = stream.next().await {
        if let Ok(crate::agent::MultiTurnStreamItem::FinalResponse(_)) = item {
            finished = true;
        }
    }
    assert!(finished, "the retried turn should complete the run");

    assert_eq!(model.request_count(), 2, "one discard, one retry");
    assert_eq!(
        request_user_texts(&model.requests()[0]),
        ["go"],
        "the first attempt sent the conversation"
    );
    assert_eq!(
        request_user_texts(&model.requests()[1]),
        ["go"],
        "the retry resent the identical conversation — the defective turn \
         never entered history"
    );
}

/// Two consecutive malformed turns exhaust the retry: the run fails with a
/// message naming the defect and the levers, and the history stays clean.
#[tokio::test]
async fn two_consecutive_malformed_turns_fail_the_run() {
    use crate::streaming::StreamingChat;

    let model = MockCompletionModel::from_stream_turns([
        vec![MockStreamEvent::Error(MockError::malformed_tool_call(
            "lookup",
            "arguments arrived truncated",
        ))],
        vec![MockStreamEvent::Error(MockError::malformed_tool_call(
            "lookup",
            "arguments arrived truncated again",
        ))],
    ]);
    let agent = Arc::new(AgentBuilder::new(model.clone()).build());

    let mut stream = agent.stream_chat(vec![Message::user("go")]).await;
    let mut failure = None;
    while let Some(item) = stream.next().await {
        if let Err(error) = item {
            failure = Some(error.to_string());
        }
    }
    let failure = failure.expect("the exhausted defect should fail the run");
    assert!(
        failure.contains("repeatedly emitted tool calls with malformed arguments"),
        "got: {failure}"
    );
    assert!(
        failure.contains("resend the prompt"),
        "the message should name the recovery: {failure}"
    );
    assert_eq!(model.request_count(), 2, "two attempts, then failure");
}

/// A steer queued behind a defective turn rides along in the retry request:
/// the drain at the discard boundary is a turn boundary like any other.
#[tokio::test]
async fn a_steer_queued_during_a_defective_turn_rides_along_in_the_retry() {
    use crate::streaming::StreamingChat;

    let model = MockCompletionModel::from_stream_turns([
        vec![MockStreamEvent::Error(MockError::malformed_tool_call(
            "lookup",
            "arguments arrived truncated",
        ))],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response(Usage::new()),
        ],
    ]);
    let agent = Arc::new(AgentBuilder::new(model.clone()).build());
    let steers = ScriptedSteers::with_texts(&["wait, use python"]);

    let mut stream = agent
        .stream_chat(vec![Message::user("go")])
        .steering(steers)
        .await;
    let mut finished = false;
    while let Some(item) = stream.next().await {
        if let Ok(crate::agent::MultiTurnStreamItem::FinalResponse(_)) = item {
            finished = true;
        }
    }
    assert!(finished, "the retried turn should complete the run");

    assert_eq!(
        request_user_texts(&model.requests()[1]),
        ["go", "wait, use python"],
        "the retry carries the drained steer after the original prompt"
    );
}

/// The defect streak resets on any committed turn: a run may hit (and
/// recover from) an independent defect per turn without ever exhausting.
/// The middle turn carries a tool call so the run continues past it.
#[tokio::test]
async fn the_defect_streak_resets_on_a_committed_turn() {
    use crate::streaming::StreamingChat;

    let model = MockCompletionModel::from_stream_turns([
        vec![MockStreamEvent::Error(MockError::malformed_tool_call(
            "lookup",
            "arguments arrived truncated",
        ))],
        vec![
            MockStreamEvent::ToolCall {
                id: "call_1".to_string(),
                name: "add".to_string(),
                arguments: serde_json::json!({ "x": 1, "y": 2 }),
                call_id: Some("call_1".to_string()),
            },
            MockStreamEvent::final_response(Usage::new()),
        ],
        vec![MockStreamEvent::Error(MockError::malformed_tool_call(
            "lookup",
            "arguments arrived truncated",
        ))],
        vec![
            MockStreamEvent::text("final"),
            MockStreamEvent::final_response(Usage::new()),
        ],
    ]);
    let agent = Arc::new(AgentBuilder::new(model.clone()).tool(MockAddTool).build());

    let mut stream = agent
        .stream_chat(vec![Message::user("go")])
        .max_turns(2)
        .await;
    let mut finished = false;
    while let Some(item) = stream.next().await {
        if let Ok(crate::agent::MultiTurnStreamItem::FinalResponse(_)) = item {
            finished = true;
        }
    }
    assert!(
        finished,
        "two independent defects, each retried once, should both recover"
    );
    assert_eq!(model.request_count(), 4, "defect, retry, defect, retry");
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

#[test]
fn completion_calls_stream_item_serializes_and_deserializes_expected_shape() {
    let item: MultiTurnStreamItem =
        MultiTurnStreamItem::CompletionCall(CompletionCall::new(2, usage(3, 4), None));

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
            assert_eq!(call_usage, CompletionCall::new(2, usage(3, 4), None));
        }
        other => panic!("expected completion call event, got {other:?}"),
    }

    let item: MultiTurnStreamItem =
        MultiTurnStreamItem::CompletionCall(CompletionCall::new(3, Usage::new(), None));
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
            assert_eq!(call, CompletionCall::new(3, Usage::new(), None));
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
            CompletionCall::new(0, Usage::new(), None),
            CompletionCall::new(1, usage(3, 4), None),
        ],
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
struct SkipDefaultApiHook;

impl AgentHook for SkipDefaultApiHook {}

fn test_cell(prompt: &str) -> tabit_log::ConversationCell {
    std::sync::Arc::new(std::sync::RwLock::new(tabit_log::ContextManager::seeded(
        vec![Message::user(prompt)],
    )))
}

fn cell_conversation(cell: &tabit_log::ConversationCell) -> Vec<Message> {
    tabit_log::lock::read(cell).messages()
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
    let cell = test_cell("do tool work");

    let mut stream = agent
        .stream_prompt("do tool work")
        .history(empty_history)
        .max_turns(3)
        .conversation_cell(cell.clone())
        .await;
    let mut saw_tool_call = false;
    let mut saw_tool_result = false;
    let mut saw_final_response = false;
    let mut final_text = String::new();
    let mut final_response_text = None;

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
    let history = cell_conversation(&cell);
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
async fn invalid_tool_call_skip_resets_streaming_text_delta_state() {
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
        .max_turns(3)
        .history(Vec::<Message>::new())
        .await;

    // The delta state resets at the turn boundary: turn one streams
    // "stale ", turn two streams "fresh" alone — never an aggregate
    // carrying the prior turn's text forward.
    let mut text_deltas = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                text_deltas.push(text.text.clone())
            }
            Ok(MultiTurnStreamItem::FinalResponse(_)) => break,
            Ok(_) => {}
            Err(err) => panic!("unexpected streaming error: {err:?}"),
        }
    }

    assert_eq!(text_deltas, vec!["stale ".to_string(), "fresh".to_string()]);
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
async fn tool_call_args_delta_before_valid_name_buffers_then_emits_in_safe_order() {
    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::tool_call_arguments_delta("tool_1", "{\"x\":"),
        MockStreamEvent::tool_call_name_delta("tool_1", "add"),
        MockStreamEvent::tool_call_arguments_delta("tool_1", "1}"),
        MockStreamEvent::final_response_with_total_tokens(3),
    ]]);
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent.stream_prompt("stream a tool call").await;
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
async fn stream_prompt_emits_tool_call_deltas_with_a_hook_registered() {
    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::tool_call_name_delta("tool_1", "add"),
        MockStreamEvent::tool_call_arguments_delta("tool_1", "{\"x\":"),
        MockStreamEvent::tool_call_arguments_delta("tool_1", "1}"),
        MockStreamEvent::final_response_with_total_tokens(3),
    ]]);
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("stream a tool call")
        .add_hook(SkipDefaultApiHook)
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
            CompletionCall::new(0, first_call_usage, None),
            CompletionCall::new(1, second_call_usage, None)
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
            CompletionCall::new(0, first_call_usage, None),
            CompletionCall::new(1, second_call_usage, None)
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
async fn stream_prompt_emits_completion_call_before_the_final_response() {
    let call_usage = usage(10, 2);
    let model = MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("done"),
        MockStreamEvent::final_response(call_usage),
    ]]);
    let agent = AgentBuilder::new(model).build();

    let mut stream = agent.stream_prompt("say done").await;
    let mut completion_calls = Vec::new();
    let mut completion_call_index = None;
    let mut final_index = None;
    let mut index = 0usize;
    while let Some(item) = stream.next().await {
        match item.expect("stream item") {
            MultiTurnStreamItem::CompletionCall(call) => {
                completion_calls.push(call);
                completion_call_index = Some(index);
            }
            MultiTurnStreamItem::FinalResponse(_) => final_index = Some(index),
            _ => {}
        }
        index += 1;
    }

    assert_eq!(
        completion_calls,
        vec![CompletionCall::new(0, call_usage, None)]
    );
    let completion_at = completion_call_index.expect("completion call emitted");
    let final_at = final_index.expect("final response emitted");
    assert!(
        completion_at < final_at,
        "the CompletionCall precedes the final response"
    );
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
        CompletionCall::new(0, Usage::new(), None),
        CompletionCall::new(1, second_call_usage, None),
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
    let cell = test_cell("answer with citations");
    let mut stream = agent
        .stream_prompt("answer with citations")
        .history(empty_history)
        .conversation_cell(cell.clone())
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
    let _ = final_response;
    let history = cell_conversation(&cell);
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

#[test]
fn final_response_constructors_surface_content_and_usage() {
    let item = MultiTurnStreamItem::final_response(
        OneOrMany::one(AssistantContent::text("done")),
        usage(1, 2),
    );
    let MultiTurnStreamItem::FinalResponse(response) = item else {
        panic!("expected a final response item, got {item:?}");
    };
    assert_eq!(response.output(), "done");
    assert_eq!(response.usage(), usage(1, 2));
    assert!(response.completion_calls().is_empty());
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
            None,
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
    fn drain(&self) -> Vec<(String, Message)> {
        let taken: Vec<Message> = self.pending.lock().expect("steer lock").drain(..).collect();
        taken
            .into_iter()
            .map(|message| (rig_core::id::generate(), message))
            .collect()
    }
}

#[tokio::test]
async fn a_steer_during_the_final_turn_exits_and_leaves_the_queue() {
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

    // The ruling (2026-08): a steer arriving during the final turn exits
    // the run — the steer opens the NEXT run at the work signal. In-run:
    // one model call, no drain, the first turn's answer is final.
    assert_eq!(
        model.request_count(),
        1,
        "the steer must not drive a second in-run call"
    );
    let steered: Vec<&String> = items
        .iter()
        .filter_map(|item| match item {
            MultiTurnStreamItem::Steer { batch } => batch.first().map(|(_, text)| text),
            _ => None,
        })
        .collect();
    assert!(
        steered.is_empty(),
        "the final-turn steer is not drained into this run"
    );
    let final_text = items
        .iter()
        .find_map(|item| match item {
            MultiTurnStreamItem::FinalResponse(response) => Some(response.output.clone()),
            _ => None,
        })
        .expect("final response");
    assert_eq!(final_text, "first answer");
}

/// An id source counting attempts, so announced ids are deterministic.
fn counting_turn_ids() -> (
    crate::agent::TurnIdSource,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let source = {
        let counter = counter.clone();
        Arc::new(move || {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            format!("attempt-{n}")
        }) as crate::agent::TurnIdSource
    };
    (source, counter)
}

/// Every model-call attempt is announced as the first item of its turn,
/// with an id from the injected source (ENGINE.md behavior delta 10): a
/// two-turn run (tool call, then final text) announces exactly twice, the
/// first announcement precedes all content, and the second precedes the
/// second turn's text.
#[tokio::test]
async fn each_model_call_attempt_is_announced_before_its_content() {
    let (ids, calls) = counting_turn_ids();
    let model = streaming_tool_then_text_model();
    let agent = AgentBuilder::new(model).tool(MockAddTool).build();

    let mut stream = agent
        .stream_prompt("do tool work")
        .max_turns(3)
        .turn_id_source(ids)
        .await;

    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item.expect("unexpected streaming error"));
    }

    // The very first item of the run is the first turn's announcement.
    assert!(
        matches!(&items.first(), Some(MultiTurnStreamItem::TurnStarted { id }) if id == "attempt-0"),
        "the run must open with the first turn's announcement, got {:?}",
        items.first()
    );
    let mut announcement_positions = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if matches!(item, MultiTurnStreamItem::TurnStarted { .. }) {
            announcement_positions.push(index);
        }
    }
    assert_eq!(
        announcement_positions.len(),
        2,
        "exactly two announcements in a two-turn run, got {announcement_positions:?}"
    );
    assert_eq!(
        announcement_positions[0], 0,
        "the first announcement precedes all content"
    );
    assert!(
        matches!(&items[announcement_positions[1]], MultiTurnStreamItem::TurnStarted { id } if id == "attempt-1"),
        "the second attempt announces a fresh id"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2, "one mint per attempt");

    // The first turn's content (tool call, result, completion call) sits
    // between the two announcements; the second turn's text follows the
    // second announcement.
    let second = announcement_positions[1];
    let between = &items[1..second];
    assert!(
        between.iter().any(|item| matches!(
            item,
            MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall { .. })
        )),
        "the tool call belongs to the first announced turn"
    );
    assert!(
        between
            .iter()
            .any(|item| matches!(item, MultiTurnStreamItem::CompletionCall(_))),
        "the first turn's completion call precedes the second announcement"
    );
    assert!(
        items[second + 1..].iter().any(|item| matches!(
            item,
            MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text)) if text.text == "done"
        )),
        "the final text belongs to the second announced turn"
    );

    // Each accepted turn closes with its own commit bracket, before the
    // next turn's announcement (turn 1) / the terminal (turn 2).
    assert!(
        between.iter().any(
            |item| matches!(item, MultiTurnStreamItem::TurnCommitted { id, .. } if id == "attempt-0")
        ),
        "the first turn's commit precedes the second announcement"
    );
    assert!(
        items[second + 1..].iter().any(
            |item| matches!(item, MultiTurnStreamItem::TurnCommitted { id, .. } if id == "attempt-1")
        ),
        "the second turn commits before the run ends"
    );
}
