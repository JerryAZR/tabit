#![allow(clippy::expect_used, clippy::indexing_slicing)]

use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use futures::{Stream, StreamExt, stream};
use rig_agent::{
    Agent, AgentBuilder, ModelHandle,
    agent::{
        AgentHook, CompletionCallAction, HookContext, ModelSelection, ModelSelectionAction,
        ModelTurnFinished, NoToolConfig, PromptRequest, RequestPatch, Standard, StreamingError,
        StreamingResult, ToolCall as ToolCallEvent, ToolCallAction, ToolResultAction,
        ToolResultEvent,
    },
    completion::{
        CompletionError, CompletionModel, CompletionRequest, CompletionResponse, Message, Prompt,
        PromptError, Usage,
    },
    streaming::{
        RawStreamingChoice, StreamFinal, StreamedAssistantContent, StreamingCompletionResponse,
        StreamingPrompt,
    },
    tool::{Tool, ToolContext, ToolExecutionError},
};
use rig_core::{
    OneOrMany,
    message::{AssistantContent, ReasoningContent, ToolCall, ToolFunction, UserContent},
};
use tokio::sync::Notify;

async fn wait_for_notification(notify: &Notify) {
    tokio::time::timeout(Duration::from_secs(5), notify.notified())
        .await
        .expect("model was polled before timeout");
}

struct SelectWith<F>(F);

impl<F> AgentHook for SelectWith<F>
where
    F: for<'a> Fn(&HookContext, ModelSelection<'a>) -> ModelSelectionAction + Send + Sync,
{
    fn on_model_select(
        &self,
        context: &HookContext,
        event: ModelSelection<'_>,
    ) -> ModelSelectionAction {
        (self.0)(context, event)
    }
}

#[derive(Clone)]
struct StopSelection {
    completion_calls: Arc<AtomicUsize>,
}

impl AgentHook for StopSelection {
    fn on_model_select(
        &self,
        _context: &HookContext,
        _event: ModelSelection<'_>,
    ) -> ModelSelectionAction {
        ModelSelectionAction::continue_run()
    }

    async fn on_completion_call(
        &self,
        _context: &HookContext,
        _event: rig_agent::agent::CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        self.completion_calls.fetch_add(1, Ordering::SeqCst);
        CompletionCallAction::continue_run()
    }
}

fn usage(total_tokens: u64) -> Usage {
    Usage {
        total_tokens,
        ..Usage::new()
    }
}

#[derive(Clone)]
enum Turn {
    // Matched by the mock's arms; no current test constructs it (its
    // constructor left with the invalid-retry tests).
    #[allow(dead_code)]
    Error(String),
    Text {
        text: String,
        usage: Usage,
        message_id: String,
    },
    Tool {
        id: String,
        name: String,
        arguments: serde_json::Value,
        usage: Usage,
        message_id: String,
    },
    Rich {
        text: String,
        usage: Usage,
        message_id: String,
    },
}

impl Turn {
    fn text(text: &str, total_tokens: u64, message_id: &str) -> Self {
        Self::Text {
            text: text.to_owned(),
            usage: usage(total_tokens),
            message_id: message_id.to_owned(),
        }
    }

    fn tool(name: &str, total_tokens: u64, message_id: &str) -> Self {
        Self::Tool {
            id: format!("{name}-call"),
            name: name.to_owned(),
            arguments: serde_json::json!({"query": "rust"}),
            usage: usage(total_tokens),
            message_id: message_id.to_owned(),
        }
    }

    fn rich(text: &str, total_tokens: u64, message_id: &str) -> Self {
        Self::Rich {
            text: text.to_owned(),
            usage: usage(total_tokens),
            message_id: message_id.to_owned(),
        }
    }

    fn usage(&self) -> Usage {
        match self {
            Self::Text { usage, .. } | Self::Tool { usage, .. } | Self::Rich { usage, .. } => {
                *usage
            }
            Self::Error(_) => Usage::new(),
        }
    }

    fn message_id(&self) -> String {
        match self {
            Self::Text { message_id, .. }
            | Self::Tool { message_id, .. }
            | Self::Rich { message_id, .. } => message_id.clone(),
            Self::Error(_) => String::new(),
        }
    }

    fn choice(&self) -> OneOrMany<AssistantContent> {
        match self {
            Self::Text { text, .. } => OneOrMany::one(AssistantContent::text(text)),
            Self::Tool {
                id,
                name,
                arguments,
                ..
            } => OneOrMany::one(AssistantContent::ToolCall(ToolCall::new(
                id.clone(),
                ToolFunction::new(name.clone(), arguments.clone()),
            ))),
            Self::Rich { text, .. } => OneOrMany::many(vec![
                AssistantContent::reasoning("considering the evidence"),
                AssistantContent::text(text),
            ])
            .expect("rich turn contains two items"),
            Self::Error(_) => OneOrMany::one(AssistantContent::text("unreachable")),
        }
    }
}

struct Script {
    provider: &'static str,
    /// Never read by design: it exists to prove the handle's Debug output
    /// never leaks it (the assert below).
    #[allow(dead_code)]
    secret: String,
    turns: Mutex<VecDeque<Turn>>,
    fallback: Turn,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl Script {
    fn new(
        provider: &'static str,
        turns: impl IntoIterator<Item = Turn>,
        fallback: Turn,
    ) -> Arc<Self> {
        Arc::new(Self {
            provider,
            secret: format!("{provider}-credential-must-not-leak"),
            turns: Mutex::new(turns.into_iter().collect()),
            fallback,
            requests: Mutex::new(Vec::new()),
        })
    }

    fn next_turn(&self) -> Turn {
        self.turns
            .lock()
            .expect("script turn lock")
            .pop_front()
            .unwrap_or_else(|| self.fallback.clone())
    }

    fn record(&self, request: CompletionRequest) {
        self.requests
            .lock()
            .expect("script request lock")
            .push(request);
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("script request lock").clone()
    }
}

fn completion_from_script(
    script: &Script,
    request: CompletionRequest,
) -> Result<CompletionResponse, CompletionError> {
    script.record(request);
    let turn = script.next_turn();
    if let Turn::Error(message) = &turn {
        return Err(CompletionError::ProviderError(message.clone()));
    }
    Ok(
        CompletionResponse::new(turn.choice(), turn.usage(), script.provider)
            .with_message_id(turn.message_id()),
    )
}

fn stream_from_script(
    script: &Script,
    request: CompletionRequest,
) -> Result<StreamingCompletionResponse, CompletionError> {
    script.record(request);
    let turn = script.next_turn();
    if let Turn::Error(message) = &turn {
        return Err(CompletionError::ProviderError(message.clone()));
    }
    let mut events = vec![Ok(RawStreamingChoice::MessageId(turn.message_id()))];
    match &turn {
        Turn::Text { text, .. } => {
            events.push(Ok(RawStreamingChoice::Message(text.clone())));
        }
        Turn::Tool {
            id,
            name,
            arguments,
            ..
        } => {
            // Canonical fragmenting-wire shape: name/args fragments closed by
            // a tool-input end; the shared accumulator assembles the call and
            // mints the correlation id at the first fragment.
            events.push(Ok(RawStreamingChoice::ToolCallDelta {
                id: rig_agent::streaming::PartId::wire(id.clone()),
                content: rig_agent::streaming::ToolCallDeltaContent::Name(name.clone()),
            }));
            events.push(Ok(RawStreamingChoice::ToolCallDelta {
                id: rig_agent::streaming::PartId::wire(id.clone()),
                content: rig_agent::streaming::ToolCallDeltaContent::Delta(arguments.to_string()),
            }));
            events.push(Ok(RawStreamingChoice::ToolInputEnd(
                rig_agent::streaming::ToolInputEnd::new(
                    id.clone(),
                    rig_agent::streaming::UnparseableToolInput::Drop,
                ),
            )));
        }
        Turn::Rich { text, .. } => {
            events.push(Ok(RawStreamingChoice::Reasoning {
                id: rig_agent::streaming::MintKind::Reasoning.for_wire_index(1),
                content: ReasoningContent::Summary("summary".to_owned()),
            }));
            events.push(Ok(RawStreamingChoice::ReasoningDelta {
                id: rig_agent::streaming::MintKind::Reasoning.for_wire_index(2),
                reasoning: "reasoning delta".to_owned(),
            }));
            events.push(Ok(RawStreamingChoice::Unknown(serde_json::json!({
                "type": "provider_native_event",
                "provider": script.provider,
            }))));
            events.push(Ok(RawStreamingChoice::Message(text.clone())));
        }
        // Handled by the early return above.
        Turn::Error(_) => return Err(CompletionError::ProviderError("unreachable".to_owned())),
    }
    events.push(Ok(RawStreamingChoice::FinalResponse(
        StreamFinal::new(script.provider, turn.usage()).with_message_id(turn.message_id()),
    )));

    Ok(StreamingCompletionResponse::stream(
        script.provider,
        Box::pin(stream::iter(events)),
    ))
}

#[derive(Clone)]
struct AlphaModel(Arc<Script>);

#[derive(Clone)]
struct BetaModel(Arc<Script>);

macro_rules! impl_test_model {
    ($model:ty) => {
        impl CompletionModel for $model {
            async fn completion(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, CompletionError> {
                completion_from_script(&self.0, request)
            }

            async fn stream(
                &self,
                request: CompletionRequest,
            ) -> Result<StreamingCompletionResponse, CompletionError> {
                stream_from_script(&self.0, request)
            }
        }
    };
}

impl_test_model!(AlphaModel);
impl_test_model!(BetaModel);

fn alpha_static(text: &str) -> AlphaModel {
    AlphaModel(Script::new(
        "alpha",
        [],
        Turn::text(text, 1, "alpha-message"),
    ))
}

fn beta_static(text: &str) -> BetaModel {
    BetaModel(Script::new("beta", [], Turn::text(text, 2, "beta-message")))
}

fn request(prompt: &str) -> CompletionRequest {
    CompletionRequest {
        model: None,
        preamble: None,
        chat_history: OneOrMany::one(Message::user(prompt)),
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

fn assert_agent(_: Agent) {}
fn assert_builder(_: AgentBuilder<NoToolConfig>) {}
fn assert_prompt_request(_: PromptRequest<Standard>) {}
fn assert_agent_stream(_: StreamingResult) {}

#[tokio::test]
async fn downstream_models_keep_typed_low_level_apis_and_share_a_concrete_agent_type() {
    let alpha = alpha_static("alpha");
    let beta = beta_static("beta");

    let alpha_agent = AgentBuilder::new(alpha.clone()).build();
    let beta_agent = AgentBuilder::new(beta.clone()).build();
    let agents: Vec<Agent> = vec![alpha_agent.clone(), beta_agent];
    assert_eq!(agents.len(), 2);

    assert_agent(alpha_agent.clone());
    assert_builder(AgentBuilder::new(alpha.clone()));
    assert_prompt_request(alpha_agent.prompt("typed request"));
    assert_agent_stream(alpha_agent.stream_prompt("stream type").await);

    let unary = alpha
        .completion(request("low-level unary"))
        .await
        .expect("direct unary response");
    assert_eq!(unary.provider, "alpha");

    let mut low_level_stream = beta
        .stream(request("low-level stream"))
        .await
        .expect("direct provider stream");
    let mut stream_final: Option<StreamFinal> = None;
    while let Some(item) = low_level_stream.next().await {
        if let StreamedAssistantContent::Final(final_) = item.expect("stream item") {
            stream_final = Some(final_);
        }
    }
    assert_eq!(
        stream_final.expect("stream final").provider,
        "beta",
        "direct model streams report their provider on the terminal record"
    );

    let handle = ModelHandle::named("diagnostic-alpha", alpha);
    let debug = format!("{handle:?}");
    assert!(debug.contains("diagnostic-alpha"));
    assert!(!debug.contains("credential-must-not-leak"));
}

#[tokio::test]
async fn replacement_and_override_scopes_have_value_semantics() {
    let alpha = alpha_static("alpha");
    let beta = beta_static("beta");
    let beta_handle = ModelHandle::named("beta", beta.clone());
    let alpha_handle = ModelHandle::named("alpha", alpha.clone());

    let mut agent = AgentBuilder::new(alpha.clone()).build();
    let runner_before_replacement = agent.runner("runner snapshot");
    agent.set_model(beta.clone());

    assert_eq!(
        runner_before_replacement
            .run()
            .await
            .expect("old runner")
            .output,
        "alpha"
    );
    assert_eq!(
        agent.prompt("new runner").await.expect("new default"),
        "beta"
    );

    let original = AgentBuilder::new(alpha.clone()).build();
    let changed_clone = original.clone().with_model_handle(beta_handle.clone());
    assert_eq!(
        original.prompt("original").await.expect("original"),
        "alpha"
    );
    assert_eq!(
        changed_clone.prompt("clone").await.expect("changed clone"),
        "beta"
    );

    assert_eq!(
        original
            .prompt("one run")
            .using_model(beta_handle.clone())
            .await
            .expect("fixed override"),
        "beta"
    );
    assert_eq!(
        original.prompt("default remains").await.expect("default"),
        "alpha"
    );

    let observed_candidates = Arc::new(Mutex::new(Vec::new()));
    let observed_for_hook = observed_candidates.clone();
    assert_eq!(
        original
            .prompt("hook after run default")
            .using_model(alpha_handle)
            .add_hook(SelectWith(
                move |_context: &HookContext, event: ModelSelection<'_>| {
                    observed_for_hook
                        .lock()
                        .expect("candidate observations")
                        .push((
                            event.default_model.label().map(str::to_owned),
                            event.selected_model.label().map(str::to_owned),
                        ));
                    ModelSelectionAction::select(beta_handle.clone())
                }
            ))
            .await
            .expect("hook overrides run default"),
        "beta"
    );
    assert_eq!(
        observed_candidates
            .lock()
            .expect("candidate observations")
            .as_slice(),
        &[(Some("alpha".to_owned()), Some("alpha".to_owned()))]
    );
}

#[tokio::test]
async fn agent_and_request_model_selection_hooks_have_expected_scope() {
    let default = ModelHandle::named("default", alpha_static("default"));
    let routed = ModelHandle::named("routed", beta_static("agent routed"));
    let request = ModelHandle::named("request", alpha_static("request routed"));
    let routed_for_hook = routed.clone();
    let agent = AgentBuilder::from_model_handle(default)
        .add_hook(SelectWith(
            move |_context: &HookContext, _event: ModelSelection<'_>| {
                ModelSelectionAction::select(routed_for_hook.clone())
            },
        ))
        .build();

    assert_eq!(
        agent.prompt("agent hook").await.expect("agent hook route"),
        "agent routed"
    );

    assert_eq!(
        agent
            .prompt("request hook")
            .add_hook(SelectWith(
                move |_context: &HookContext, event: ModelSelection<'_>| {
                    assert_eq!(event.selected_model.label(), Some("routed"));
                    ModelSelectionAction::select(request.clone())
                }
            ))
            .await
            .expect("request hook route"),
        "request routed"
    );

    assert_eq!(
        agent
            .prompt("agent hook remains")
            .await
            .expect("agent hook remains"),
        "agent routed"
    );
}

#[derive(Clone)]
struct LookupTool {
    calls: Arc<AtomicUsize>,
}

impl Tool for LookupTool {
    const NAME: &'static str = "lookup";
    type Error = ToolExecutionError;
    type Args = serde_json::Value;
    type Output = String;

    fn description(&self) -> String {
        "Look up deterministic evidence".to_owned()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}}})
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok("durable evidence".to_owned())
    }
}

#[derive(Clone, Default)]
struct LifecycleLog(Arc<Mutex<Vec<String>>>);

impl LifecycleLog {
    fn entries(&self) -> Vec<String> {
        self.0.lock().expect("lifecycle lock").clone()
    }

    fn push(&self, entry: impl Into<String>) {
        self.0.lock().expect("lifecycle lock").push(entry.into());
    }
}

impl AgentHook for LifecycleLog {
    async fn on_completion_call(
        &self,
        context: &HookContext,
        _event: rig_agent::agent::CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        self.push(format!("completion:{}", context.turn()));
        CompletionCallAction::continue_run()
    }

    async fn on_model_turn_finished(&self, context: &HookContext, _event: ModelTurnFinished<'_>) {
        self.push(format!("model:{}", context.turn()));
    }

    async fn on_tool_call(
        &self,
        _context: &HookContext,
        event: ToolCallEvent<'_>,
    ) -> ToolCallAction {
        self.push(format!("tool-call:{}", event.tool_name));
        ToolCallAction::run()
    }

    async fn on_tool_result(
        &self,
        _context: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        self.push(format!("tool-result:{}", event.tool_name));
        ToolResultAction::keep()
    }
}

fn routing_models() -> (AlphaModel, BetaModel) {
    let alpha_turn = Turn::tool("lookup", 3, "alpha-tool-message");
    let beta_turn = Turn::text("synthesized answer", 5, "beta-answer-message");
    (
        AlphaModel(Script::new("alpha", [alpha_turn.clone()], alpha_turn)),
        BetaModel(Script::new("beta", [beta_turn.clone()], beta_turn)),
    )
}

fn history_has_tool_result(request: &CompletionRequest) -> bool {
    request.chat_history.iter().any(|message| {
        matches!(
            message,
            Message::User { content }
                if content.iter().any(|item| matches!(item, UserContent::ToolResult(_)))
        )
    })
}

#[tokio::test]
async fn runner_default_is_used_for_every_attempt_without_a_selecting_hook() {
    let first = Turn::tool("lookup", 2, "default-tool-message");
    let second = Turn::text("default final", 3, "default-final-message");
    let model = AlphaModel(Script::new("default", [first, second.clone()], second));
    let script = model.0.clone();

    let output = AgentBuilder::new(model)
        .tool(LookupTool {
            calls: Arc::new(AtomicUsize::new(0)),
        })
        .build()
        .prompt("use the default twice")
        .max_turns(2)
        .await
        .expect("default multi-turn run");

    assert_eq!(output, "default final");
    assert_eq!(script.requests().len(), 2);
}

#[tokio::test]
async fn blocking_and_streaming_switch_after_tools_with_equivalent_semantics() {
    async fn run_blocking() -> (
        rig_agent::agent::PromptResponse,
        Vec<String>,
        usize,
        Vec<CompletionRequest>,
        Vec<String>,
    ) {
        let (alpha, beta) = routing_models();
        let beta_script = beta.0.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let lifecycle = LifecycleLog::default();
        let selected = Arc::new(Mutex::new(Vec::new()));
        let selected_for_router = selected.clone();
        let alpha_handle = ModelHandle::named("alpha", alpha);
        let beta_handle = ModelHandle::named("beta", beta);
        let response = AgentBuilder::from_model_handle(alpha_handle.clone())
            .tool(LookupTool {
                calls: calls.clone(),
            })
            .add_hook(lifecycle.clone())
            .build()
            .prompt("research then synthesize")
            .max_turns(3)
            .add_hook(SelectWith(
                move |context: &HookContext, _event: ModelSelection<'_>| {
                    selected_for_router
                        .lock()
                        .expect("selection lock")
                        .push(context.turn());
                    ModelSelectionAction::select(if context.turn() == 1 {
                        alpha_handle.clone()
                    } else {
                        beta_handle.clone()
                    })
                },
            ))
            .extended_details()
            .await
            .expect("blocking routed run");
        let selections = selected.lock().expect("selection lock").clone();
        (
            response,
            lifecycle.entries(),
            calls.load(Ordering::SeqCst),
            beta_script.requests(),
            selections
                .into_iter()
                .map(|turn| turn.to_string())
                .collect(),
        )
    }

    async fn run_streaming() -> (
        rig_agent::agent::PromptResponse,
        Vec<String>,
        usize,
        Vec<CompletionRequest>,
        Vec<usize>,
        Vec<&'static str>,
        Vec<String>,
    ) {
        let (alpha, beta) = routing_models();
        let beta_script = beta.0.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let lifecycle = LifecycleLog::default();
        let selected = Arc::new(Mutex::new(Vec::new()));
        let selected_for_router = selected.clone();
        let alpha_handle = ModelHandle::named("alpha", alpha);
        let beta_handle = ModelHandle::named("beta", beta);
        let agent = AgentBuilder::from_model_handle(alpha_handle.clone())
            .tool(LookupTool {
                calls: calls.clone(),
            })
            .add_hook(lifecycle.clone())
            .build();
        let mut stream = agent
            .stream_prompt("research then synthesize")
            .max_turns(3)
            .add_hook(SelectWith(
                move |context: &HookContext, _event: ModelSelection<'_>| {
                    selected_for_router
                        .lock()
                        .expect("selection lock")
                        .push(context.turn());
                    ModelSelectionAction::select(if context.turn() == 1 {
                        alpha_handle.clone()
                    } else {
                        beta_handle.clone()
                    })
                },
            ))
            .await;
        let mut final_response = None;
        let mut events = Vec::new();
        let mut internal_call_ids = Vec::new();
        while let Some(item) = stream.next().await {
            match item.expect("stream item") {
                rig_agent::agent::MultiTurnStreamItem::StreamAssistantItem(
                    StreamedAssistantContent::ToolCallDelta {
                        internal_call_id, ..
                    },
                ) => {
                    events.push("tool-delta");
                    internal_call_ids.push(internal_call_id);
                }
                rig_agent::agent::MultiTurnStreamItem::StreamAssistantItem(
                    StreamedAssistantContent::ToolCall {
                        internal_call_id, ..
                    },
                ) => {
                    events.push("tool-call");
                    internal_call_ids.push(internal_call_id);
                }
                rig_agent::agent::MultiTurnStreamItem::ToolExecutionCommitted {
                    internal_call_id,
                    ..
                } => {
                    events.push("tool-commit");
                    internal_call_ids.push(internal_call_id);
                }
                rig_agent::agent::MultiTurnStreamItem::StreamUserItem(
                    rig_agent::streaming::StreamedUserContent::ToolResult {
                        internal_call_id, ..
                    },
                ) => {
                    events.push("tool-result");
                    internal_call_ids.push(internal_call_id);
                }
                rig_agent::agent::MultiTurnStreamItem::FinalResponse(response) => {
                    final_response = Some(response)
                }
                _ => {}
            }
        }
        (
            final_response.expect("stream final response"),
            lifecycle.entries(),
            calls.load(Ordering::SeqCst),
            beta_script.requests(),
            selected.lock().expect("selection lock").clone(),
            events,
            internal_call_ids,
        )
    }

    let (blocking, blocking_hooks, blocking_tool_calls, blocking_beta, blocking_selected) =
        run_blocking().await;
    let (
        streaming,
        streaming_hooks,
        streaming_tool_calls,
        streaming_beta,
        streaming_selected,
        stream_events,
        stream_internal_call_ids,
    ) = run_streaming().await;

    assert_eq!(blocking.output, "synthesized answer");
    assert_eq!(blocking.output, streaming.output);
    assert_eq!(blocking.usage, usage(8));
    assert_eq!(blocking.usage, streaming.usage);
    assert_eq!(blocking.messages, streaming.messages);
    assert_eq!(blocking_hooks, streaming_hooks);
    assert_eq!(blocking_tool_calls, 1);
    assert_eq!(streaming_tool_calls, 1);
    assert_eq!(blocking_selected, vec!["1", "2"]);
    assert_eq!(streaming_selected, vec![1, 2]);
    assert!(blocking_beta.first().is_some_and(history_has_tool_result));
    assert!(streaming_beta.first().is_some_and(history_has_tool_result));
    assert_eq!(
        stream_events,
        vec![
            "tool-delta",
            "tool-delta",
            "tool-call",
            "tool-commit",
            "tool-result"
        ]
    );
    // The correlation id is minted by the shared accumulator when the call's
    // first fragment arrives; every downstream stage must carry that one id.
    let correlation = stream_internal_call_ids
        .first()
        .expect("at least one correlated event")
        .clone();
    assert!(!correlation.is_empty());
    assert_eq!(
        stream_internal_call_ids,
        vec![correlation; 5],
        "deltas, the completed call, execution, and result retain one correlation id"
    );
}

#[tokio::test]
async fn normalized_stream_preserves_events_message_id_and_usage() {
    let rich = Turn::rich("final text", 13, "rich-message-id");
    let alpha = AlphaModel(Script::new("alpha", [rich.clone()], rich));
    let agent = AgentBuilder::new(alpha).build();
    let mut stream = agent.stream_prompt("rich stream").await;
    let mut saw_reasoning = false;
    let mut saw_reasoning_delta = false;
    let mut saw_unknown = false;
    let mut provider_final: Option<StreamFinal> = None;
    let mut final_response = None;

    while let Some(item) = stream.next().await {
        match item.expect("normalized stream item") {
            rig_agent::agent::MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::Reasoning(_),
            ) => saw_reasoning = true,
            rig_agent::agent::MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::ReasoningDelta { .. },
            ) => saw_reasoning_delta = true,
            rig_agent::agent::MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::Unknown(value),
            ) => {
                saw_unknown = value["type"] == "provider_native_event";
            }
            rig_agent::agent::MultiTurnStreamItem::StreamAssistantItem(
                StreamedAssistantContent::Final(final_),
            ) => provider_final = Some(final_),
            rig_agent::agent::MultiTurnStreamItem::FinalResponse(response) => {
                final_response = Some(response)
            }
            _ => {}
        }
    }

    assert!(saw_reasoning);
    assert!(saw_reasoning_delta);
    assert!(saw_unknown);
    let provider_final = provider_final.expect("normalized provider final");
    assert_eq!(provider_final.usage, usage(13));
    assert_eq!(provider_final.provider, "alpha");
    let final_response = final_response.expect("agent final response");
    assert_eq!(final_response.output, "final text");
    assert_eq!(final_response.usage, usage(13));
    assert!(final_response.messages.is_some_and(|messages| {
        messages.iter().any(|message| {
            matches!(message, Message::Assistant { id: Some(id), .. } if id == "rich-message-id")
        })
    }));
}

#[derive(Clone)]
struct GatedToolModel {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl CompletionModel for GatedToolModel {
    async fn completion(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        self.started.notify_one();
        self.release.notified().await;
        let turn = Turn::tool("lookup", 3, "gated-tool-message");
        Ok(
            CompletionResponse::new(turn.choice(), turn.usage(), "gated")
                .with_message_id(turn.message_id()),
        )
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse, CompletionError> {
        Ok(StreamingCompletionResponse::stream(
            "gated",
            Box::pin(stream::iter([
                Ok(RawStreamingChoice::Message("unused".to_owned())),
                Ok(RawStreamingChoice::FinalResponse(StreamFinal::new(
                    "gated",
                    usage(1),
                ))),
            ])),
        ))
    }
}

#[tokio::test]
async fn routing_changes_cannot_rebind_an_in_flight_attempt_but_affect_the_next_call() {
    let gated = GatedToolModel {
        started: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    };
    let started = gated.started.clone();
    let release = gated.release.clone();
    let gated_handle = ModelHandle::named("gated", gated);
    let beta_handle = ModelHandle::named("beta", beta_static("after tool"));
    let use_beta = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let use_beta_for_router = use_beta.clone();
    let tool_calls = Arc::new(AtomicUsize::new(0));

    let agent = AgentBuilder::from_model_handle(gated_handle.clone())
        .tool(LookupTool {
            calls: tool_calls.clone(),
        })
        .build();
    let task = tokio::spawn(async move {
        agent
            .prompt("bind the attempt")
            .max_turns(2)
            .add_hook(SelectWith(
                move |_context: &HookContext, _event: ModelSelection<'_>| {
                    ModelSelectionAction::select(if use_beta_for_router.load(Ordering::SeqCst) {
                        beta_handle.clone()
                    } else {
                        gated_handle.clone()
                    })
                },
            ))
            .await
    });

    wait_for_notification(&started).await;
    use_beta.store(true, Ordering::SeqCst);
    release.notify_one();

    assert_eq!(
        task.await
            .expect("routing task join")
            .expect("routing task result"),
        "after tool"
    );
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
}

struct DropGuard(Arc<AtomicUsize>);

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct PendingUnaryModel {
    started: Arc<Notify>,
    dropped: Arc<AtomicUsize>,
}

impl CompletionModel for PendingUnaryModel {
    async fn completion(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        let _guard = DropGuard(self.dropped.clone());
        self.started.notify_one();
        std::future::pending::<Result<CompletionResponse, CompletionError>>().await
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse, CompletionError> {
        Ok(StreamingCompletionResponse::stream(
            "pending",
            Box::pin(stream::empty()),
        ))
    }
}

struct PendingRawStream {
    started: Arc<Notify>,
    dropped: Arc<AtomicUsize>,
    notified: bool,
}

impl Stream for PendingRawStream {
    type Item = Result<RawStreamingChoice, CompletionError>;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if !self.notified {
            self.notified = true;
            self.started.notify_one();
        }
        Poll::Pending
    }
}

impl Drop for PendingRawStream {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct PendingStreamingModel {
    started: Arc<Notify>,
    dropped: Arc<AtomicUsize>,
}

impl CompletionModel for PendingStreamingModel {
    async fn completion(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        Ok(CompletionResponse::new(
            OneOrMany::one(AssistantContent::text("unused")),
            Usage::new(),
            "pending",
        ))
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse, CompletionError> {
        let raw: rig_agent::streaming::StreamingResult = Box::pin(PendingRawStream {
            started: self.started.clone(),
            dropped: self.dropped.clone(),
            notified: false,
        });
        Ok(StreamingCompletionResponse::stream("pending", raw))
    }
}

#[tokio::test]
async fn dropping_pending_unary_and_streaming_attempts_cancels_by_drop() {
    let unary_started = Arc::new(Notify::new());
    let unary_dropped = Arc::new(AtomicUsize::new(0));
    let unary_agent = AgentBuilder::new(PendingUnaryModel {
        started: unary_started.clone(),
        dropped: unary_dropped.clone(),
    })
    .build();
    let unary_task = tokio::spawn(async move { unary_agent.prompt("pending unary").await });
    wait_for_notification(&unary_started).await;
    unary_task.abort();
    let _join_error = unary_task.await.expect_err("unary task was aborted");
    assert_eq!(unary_dropped.load(Ordering::SeqCst), 1);

    let stream_started = Arc::new(Notify::new());
    let stream_dropped = Arc::new(AtomicUsize::new(0));
    let stream_agent = AgentBuilder::new(PendingStreamingModel {
        started: stream_started.clone(),
        dropped: stream_dropped.clone(),
    })
    .build();
    let pending_stream = stream_agent.stream_prompt("pending stream").await;
    let stream_task = tokio::spawn(async move {
        let mut pending_stream = pending_stream;
        // The first poll returns the turn announcement (ENGINE.md behavior
        // delta 10); the provider's own stream is reached on the next poll,
        // where this run parks forever.
        while pending_stream.next().await.is_some() {}
    });
    wait_for_notification(&stream_started).await;
    stream_task.abort();
    let _join_error = stream_task.await.expect_err("stream task was aborted");
    assert_eq!(stream_dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_runs_and_handle_calls_are_independent() {
    let alpha_handle = ModelHandle::named("alpha", alpha_static("alpha concurrent"));
    let beta_handle = ModelHandle::named("beta", beta_static("beta concurrent"));
    let agent = AgentBuilder::from_model_handle(alpha_handle.clone()).build();

    let alpha_run = agent.prompt("alpha run").add_hook(SelectWith(
        move |_context: &HookContext, _event: ModelSelection<'_>| {
            ModelSelectionAction::select(alpha_handle.clone())
        },
    ));
    let beta_run = agent.prompt("beta run").add_hook(SelectWith(
        move |_context: &HookContext, _event: ModelSelection<'_>| {
            ModelSelectionAction::select(beta_handle.clone())
        },
    ));
    let (alpha, beta) = tokio::join!(alpha_run, beta_run);
    assert_eq!(alpha.expect("alpha concurrent run"), "alpha concurrent");
    assert_eq!(beta.expect("beta concurrent run"), "beta concurrent");

    let shared = ModelHandle::new(alpha_static("shared handle"));
    let first = agent.prompt("first shared").using_model(shared.clone());
    let second = agent.prompt("second shared").using_model(shared);
    let (first, second) = tokio::join!(first, second);
    assert_eq!(first.expect("first shared call"), "shared handle");
    assert_eq!(second.expect("second shared call"), "shared handle");
}

// ---------------------------------------------------------------------------
// Ordering parity tests: completion-call hooks -> merged RequestPatch ->
// ModelSelection -> preparation -> issue attempt; previous_model reflects
// issued attempts only. Each scenario runs on both surfaces.
// ---------------------------------------------------------------------------

struct PatchWith(RequestPatch);

impl AgentHook for PatchWith {
    async fn on_completion_call(
        &self,
        _context: &HookContext,
        _event: rig_agent::agent::CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        CompletionCallAction::patch(self.0.clone())
    }
}

struct StopCompletionCall;

impl AgentHook for StopCompletionCall {
    async fn on_completion_call(
        &self,
        _context: &HookContext,
        _event: rig_agent::agent::CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        CompletionCallAction::continue_run()
    }
}

/// Records what every selection event observed: (turn, previous_model label,
/// merged-patch temperature, merged-patch preamble).
type SelectionObservations = Arc<Mutex<Vec<(usize, Option<String>, Option<f64>, Option<String>)>>>;

fn observing_selector(
    observations: SelectionObservations,
) -> SelectWith<impl for<'a> Fn(&HookContext, ModelSelection<'a>) -> ModelSelectionAction> {
    SelectWith(move |context: &HookContext, event: ModelSelection<'_>| {
        observations.lock().expect("observation lock").push((
            context.turn(),
            event
                .previous_model
                .and_then(ModelHandle::label)
                .map(str::to_owned),
            event.request_patch.and_then(|patch| patch.temperature),
            event.request_patch.and_then(|patch| patch.preamble.clone()),
        ));
        ModelSelectionAction::continue_run()
    })
}

/// Drive a streaming run to its terminal item, returning the first error if
/// the stream yields one.
async fn drain_stream(mut stream: StreamingResult) -> Result<(), StreamingError> {
    while let Some(item) = stream.next().await {
        item?;
    }
    Ok(())
}

#[tokio::test]
async fn model_selection_hooks_observe_the_merged_request_patch_on_both_surfaces() {
    for streaming in [false, true] {
        let model = alpha_static("patched");
        let script = model.0.clone();
        let observations: SelectionObservations = Arc::new(Mutex::new(Vec::new()));
        // Two patching hooks: the selection event must observe their MERGED
        // patch (temperature from the first, preamble from the second).
        let agent = AgentBuilder::new(model)
            .add_hook(PatchWith(RequestPatch::new().temperature(0.25)))
            .add_hook(PatchWith(RequestPatch::new().preamble("patched preamble")))
            .add_hook(observing_selector(observations.clone()))
            .build();

        if streaming {
            drain_stream(agent.stream_prompt("merged patch").await)
                .await
                .expect("streaming patched run");
        } else {
            agent
                .prompt("merged patch")
                .await
                .expect("blocking patched run");
        }

        assert_eq!(
            observations.lock().expect("observation lock").as_slice(),
            &[(1, None, Some(0.25), Some("patched preamble".to_owned()))],
            "streaming={streaming}: selection must see the merged completion-call patch"
        );
        // The merged patch reached the issued request too.
        let requests = script.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].temperature, Some(0.25));
    }
}

#[tokio::test]
async fn a_request_patch_can_influence_the_selected_model_on_both_surfaces() {
    for streaming in [false, true] {
        let alpha = alpha_static("alpha answer");
        let beta = beta_static("beta answer");
        let beta_script = beta.0.clone();
        let beta_handle = ModelHandle::named("beta", beta);
        // The completion-call hook escalates via a patch; the selection hook
        // routes to beta exactly when it observes the escalation marker.
        let agent = AgentBuilder::new(alpha)
            .add_hook(PatchWith(RequestPatch::new().temperature(0.9)))
            .add_hook(SelectWith(
                move |_context: &HookContext, event: ModelSelection<'_>| {
                    let escalated = event
                        .request_patch
                        .and_then(|patch| patch.temperature)
                        .is_some_and(|temperature| temperature > 0.5);
                    if escalated {
                        ModelSelectionAction::select(beta_handle.clone())
                    } else {
                        ModelSelectionAction::continue_run()
                    }
                },
            ))
            .build();

        if streaming {
            drain_stream(agent.stream_prompt("route by patch").await)
                .await
                .expect("streaming patch-routed run");
            assert_eq!(
                beta_script.requests().len(),
                1,
                "streaming: the patch must route the attempt to beta"
            );
        } else {
            assert_eq!(
                agent
                    .prompt("route by patch")
                    .await
                    .expect("blocking patch-routed run"),
                "beta answer"
            );
        }
    }
}

#[tokio::test]
async fn failed_preparation_follows_selection_and_does_not_issue_an_attempt() {
    for streaming in [false, true] {
        // Turn 1 issues a tool-call attempt on alpha; turn 2's completion-call
        // patch names a tool that does not exist, so preparation fails after
        // model selection resolves.
        let alpha_turn = Turn::tool("lookup", 3, "alpha-tool-message");
        let model = AlphaModel(Script::new("alpha", [alpha_turn.clone()], alpha_turn));
        let script = model.0.clone();
        let observations: SelectionObservations = Arc::new(Mutex::new(Vec::new()));
        let alpha_handle = ModelHandle::named("alpha", model.clone());
        let bad_patch = BadSecondTurnPatch;
        let agent = AgentBuilder::from_model_handle(alpha_handle)
            .tool(LookupTool {
                calls: Arc::new(AtomicUsize::new(0)),
            })
            .add_hook(bad_patch)
            .add_hook(observing_selector(observations.clone()))
            .build();

        let failed = if streaming {
            drain_stream(agent.stream_prompt("prepare fails").max_turns(2).await)
                .await
                .is_err()
        } else {
            agent.prompt("prepare fails").max_turns(2).await.is_err()
        };
        assert!(failed, "streaming={streaming}: preparation must fail");

        // Selection ran on both turns; only turn 1's attempt was issued, so
        // turn 2 observes previous_model == alpha, and the failed preparation
        // never reached the provider.
        let observed = observations.lock().expect("observation lock").clone();
        assert_eq!(observed.len(), 2, "streaming={streaming}");
        assert_eq!(observed[0].0, 1);
        assert_eq!(observed[0].1, None);
        assert_eq!(observed[1].0, 2);
        assert_eq!(observed[1].1, Some("alpha".to_owned()));
        assert_eq!(
            script.requests().len(),
            1,
            "streaming={streaming}: the failed turn must not reach the provider"
        );
    }
}

/// Patches turn 2 with an `active_tools` allow-list naming a missing tool, so
/// request preparation fails locally on that turn.
#[derive(Clone)]
struct BadSecondTurnPatch;

impl AgentHook for BadSecondTurnPatch {
    async fn on_completion_call(
        &self,
        context: &HookContext,
        _event: rig_agent::agent::CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        if context.turn() == 2 {
            CompletionCallAction::patch(
                RequestPatch::new().active_tools(["no_such_tool".to_owned()]),
            )
        } else {
            CompletionCallAction::continue_run()
        }
    }
}
