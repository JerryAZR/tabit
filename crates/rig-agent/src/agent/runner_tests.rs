use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use futures::StreamExt;
use serde_json::json;

use crate::{
    agent::{
        AgentBuilder, AgentHook, HookContext, ToolCall, ToolCallAction, ToolResultAction,
        ToolResultEvent,
    },
    completion::{CompletionModel, Document, PromptError},
    test_utils::{MockAddTool, MockCompletionModel, MockStreamEvent, MockTurn},
    tool::{Tool, ToolContext, ToolExecutionError},
};
use rig_core::message::ToolChoice;

/// A mock model whose streaming surface replays the given unary-turn
/// scenario: each `MockTurn` becomes one scripted streaming turn
/// ([`MockTurn::into_stream_events`]). The suite drives the one
/// execution surface.
fn stream_model(turns: impl IntoIterator<Item = MockTurn>) -> MockCompletionModel {
    MockCompletionModel::from_stream_turns(turns.into_iter().map(MockTurn::into_stream_events))
}
fn three_tools_first_terminates_streaming_model() -> MockCompletionModel {
    MockCompletionModel::from_stream_turns([
        vec![
            // tc0 (x==0) terminates on its ToolCall hook after the in-flight
            // sibling starts; tc1 (x==1) is the in-flight sibling (drains);
            // tc2 (x==2) is beyond the concurrency-2 window (not yet started)
            // and must be dropped once tc0 terminates.
            MockStreamEvent::tool_call("tc0", "add", json!({"x": 0, "y": 0})),
            MockStreamEvent::tool_call("tc1", "add", json!({"x": 1, "y": 1})),
            MockStreamEvent::tool_call("tc2", "add", json!({"x": 2, "y": 2})),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
        vec![
            MockStreamEvent::text("unreachable"),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
    ])
}

/// Deterministic announced-id mint for parity runs: both scenarios
/// restart the same sequence, so assistant entries (which carry the
/// announced ids — the one-value rule) compare equal across surfaces.
async fn drive_to_final_response(
    mut stream: crate::agent::prompt_request::streaming::StreamingResult,
) -> crate::agent::prompt_request::PromptResponse {
    let mut final_response = None;
    while let Some(item) = stream.next().await {
        if let MultiTurnStreamItem::FinalResponse(resp) =
            item.unwrap_or_else(|err| panic!("stream item errored: {err}"))
        {
            final_response = Some(resp);
        }
    }
    final_response.expect("stream should yield a final response")
}

fn parity_turn_ids() -> crate::agent::TurnIdSource {
    let counter = Arc::new(AtomicU32::new(0));
    Arc::new(move || {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        format!("turn-{n}")
    })
}

fn parity_conversation(cell: &tabit_log::ConversationCell) -> Vec<Message> {
    tabit_log::lock::read(cell).messages()
}

/// A standalone cell seeded with the scenario prompt — the parity
/// harness is a cell-supplying caller (the conversation, not the
/// response, is the transcript it compares).
fn parity_cell(prompt: &str) -> tabit_log::ConversationCell {
    std::sync::Arc::new(std::sync::RwLock::new(tabit_log::ContextManager::seeded(
        vec![Message::user(prompt)],
    )))
}

struct MetadataFailingTool;

struct SnapshotValue {
    value: usize,
    clones: Arc<AtomicUsize>,
}

impl Clone for SnapshotValue {
    fn clone(&self) -> Self {
        self.clones.fetch_add(1, Ordering::SeqCst);
        Self {
            value: self.value,
            clones: self.clones.clone(),
        }
    }
}

#[derive(Clone, Default)]
struct SnapshotMutatingTool(Arc<Mutex<Vec<usize>>>);

impl Tool for SnapshotMutatingTool {
    const NAME: &'static str = "snapshot_mutator";
    type Error = rig::tool::ToolExecutionError;
    type Args = serde_json::Value;
    type Output = String;

    fn description(&self) -> String {
        "Mutates its per-dispatch context snapshot".into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let initial = context.require::<SnapshotValue>()?.value;
        self.0.lock().expect("observed values").push(initial);
        let updated = {
            let value = context
                .get_mut::<SnapshotValue>()
                .expect("required snapshot value");
            value.value += 1;
            value.value
        };
        context.insert_result(updated);
        Ok(updated.to_string())
    }
}

#[derive(Clone, Default)]
struct SnapshotResults(Arc<Mutex<Vec<usize>>>);

impl AgentHook for SnapshotResults {
    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        self.0.lock().expect("result values").push(
            *event
                .tool_context
                .require_result::<usize>()
                .expect("per-dispatch result metadata"),
        );
        ToolResultAction::keep()
    }
}

impl Tool for MetadataFailingTool {
    const NAME: &'static str = "flaky_tool";
    type Error = rig::tool::ToolExecutionError;
    type Args = serde_json::Value;
    type Output = String;

    fn description(&self) -> String {
        "Fails after attaching result metadata".into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        context.insert_result("shared-result-metadata".to_string());
        Err(ToolExecutionError::timeout("raw timeout failure"))
    }
}

#[test]
fn agent_exposes_read_only_name_and_description() {
    let named = AgentBuilder::new(stream_model([MockTurn::text("done")]))
        .name("researcher")
        .description("Finds evidence")
        .build();
    assert_eq!(named.name(), Some("researcher"));
    assert_eq!(named.description(), Some("Finds evidence"));

    let unnamed = AgentBuilder::new(stream_model([MockTurn::text("done")])).build();
    assert_eq!(unnamed.name(), None);
    assert_eq!(unnamed.description(), None);
}

#[tokio::test]
async fn runner_applies_per_run_request_overrides() {
    let model = stream_model([MockTurn::text("done")]);
    AgentBuilder::new(model.clone())
        .preamble("baseline preamble")
        .context("baseline document")
        .temperature(0.1)
        .max_tokens(10)
        .additional_params(json!({"baseline": true}))
        .build()
        .runner("go")
        .preamble("run preamble")
        .document(Document {
            id: "run-one".into(),
            text: "first run document".into(),
            additional_props: Default::default(),
        })
        .documents([Document {
            id: "run-two".into(),
            text: "second run document".into(),
            additional_props: Default::default(),
        }])
        .temperature(0.7)
        .max_tokens(42)
        .replace_additional_params(json!({"override": true}))
        .tool_choice(ToolChoice::None)
        .run()
        .await
        .expect("runner request should succeed");

    let requests = model.requests();
    let request = requests.first().expect("one request");
    assert!(request.chat_history.iter().any(
            |message| matches!(message, crate::completion::Message::System { content } if content == "run preamble")
        ));
    assert!(
        request
            .documents
            .iter()
            .any(|document| document.text == "baseline document")
    );
    assert!(
        request
            .documents
            .iter()
            .any(|document| document.id == "run-one")
    );
    assert!(
        request
            .documents
            .iter()
            .any(|document| document.id == "run-two")
    );
    assert_eq!(request.temperature, Some(0.7));
    assert_eq!(request.max_tokens, Some(42));
    assert_eq!(request.additional_params, Some(json!({"override": true})));
    assert_eq!(request.tool_choice, Some(ToolChoice::None));
}

#[tokio::test]
async fn runner_can_merge_additional_params_into_the_baseline() {
    let model = stream_model([MockTurn::text("done")]);
    AgentBuilder::new(model.clone())
        .additional_params(json!({"baseline": true, "winner": "baseline"}))
        .build()
        .runner("go")
        .merge_additional_params(
            json!({"override": true, "winner": "runner"})
                .as_object()
                .expect("object")
                .clone(),
        )
        .run()
        .await
        .expect("runner request should succeed");

    assert_eq!(
        model
            .requests()
            .first()
            .expect("one request")
            .additional_params,
        Some(json!({"baseline": true, "override": true, "winner": "runner"}))
    );
}

#[tokio::test]
async fn runner_can_replace_additional_params_wholesale() {
    let model = stream_model([MockTurn::text("done")]);
    AgentBuilder::new(model.clone())
        .additional_params(json!({"baseline": true}))
        .build()
        .runner("go")
        .replace_additional_params(json!({"replacement": true}))
        .run()
        .await
        .expect("runner request should succeed");

    let requests = model.requests();
    let request = requests.first().expect("one request");
    assert_eq!(
        request.additional_params,
        Some(json!({"replacement": true}))
    );
}

#[tokio::test]
async fn runner_can_clear_configured_request_defaults() {
    let model = stream_model([MockTurn::text("done")]);
    AgentBuilder::new(model.clone())
        .preamble("baseline")
        .temperature(0.1)
        .max_tokens(10)
        .additional_params(json!({"baseline": true}))
        .tool_choice(ToolChoice::Required)
        .build()
        .runner("go")
        .without_preamble()
        .without_temperature()
        .without_max_tokens()
        .without_additional_params()
        .without_tool_choice()
        .run()
        .await
        .expect("runner request should succeed");

    let requests = model.requests();
    let request = requests.first().expect("one request");
    assert!(
        !request
            .chat_history
            .iter()
            .any(|message| matches!(message, crate::completion::Message::System { .. }))
    );
    assert_eq!(request.temperature, None);
    assert_eq!(request.max_tokens, None);
    assert_eq!(request.additional_params, None);
    assert_eq!(request.tool_choice, None);
}

#[tokio::test]
async fn direct_completion_model_requests_are_intentionally_hook_free() {
    #[derive(Clone)]
    struct CountToolCalls(Arc<AtomicUsize>);

    impl AgentHook for CountToolCalls {
        async fn on_tool_call(&self, _ctx: &HookContext, _event: ToolCall<'_>) -> ToolCallAction {
            self.0.fetch_add(1, Ordering::SeqCst);
            ToolCallAction::run()
        }
    }

    let model = MockCompletionModel::from_turns([MockTurn::text("raw response")]);
    let calls = Arc::new(AtomicUsize::new(0));
    let _agent = AgentBuilder::new(model.clone())
        .add_hook(CountToolCalls(calls.clone()))
        .build();

    model
        .completion_request("raw request")
        .send()
        .await
        .expect("direct model request should succeed");

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(model.request_count(), 1);
}

/// A user tool whose body panics with an out-of-bounds index — the classic
/// uncaught bug the dispatch boundary must contain.
struct IndexPanickingTool;

impl Tool for IndexPanickingTool {
    const NAME: &'static str = "index_panicking_tool";
    type Error = ToolExecutionError;
    type Args = serde_json::Value;
    type Output = String;

    fn description(&self) -> String {
        "Panics with an out-of-bounds index mid-execution".into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }

    // The out-of-bounds index in `call` is deliberate: this tool exists
    // to panic mid-execution and exercise the run's panic containment.
    #[allow(clippy::out_of_bounds_indexing)]
    async fn call(
        &self,
        _context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let empty: [u32; 0] = [];
        Ok(empty[0].to_string())
    }
}

#[tokio::test]
async fn a_panicking_tool_yields_an_error_result_the_model_sees_and_the_run_recovers() {
    let model = stream_model([
        MockTurn::tool_call("tc1", IndexPanickingTool::NAME, json!({})),
        MockTurn::text("recovered"),
    ]);
    let response = AgentBuilder::new(model.clone())
        .tool(IndexPanickingTool)
        .build()
        .runner("go")
        .max_turns(3)
        .run()
        .await
        .expect("a panicking tool must not kill the run");

    // The run continued past the panic and completed on the next turn.
    assert_eq!(response.output, "recovered");
    assert_eq!(
        model.request_count(),
        2,
        "the panic must not terminate the loop before the second model turn"
    );

    // The panic message reached the model as the turn-1 tool result.
    let history = serde_json::to_value(
        &model
            .requests()
            .get(1)
            .expect("second request")
            .chat_history,
    )
    .unwrap()
    .to_string();
    // The JSON-serialized history escapes the quotes around the tool name, so
    // match on the stable, quote-free tail of the panic message.
    assert!(
        history.contains("panicked: index out of bounds: the len is 0 but the index is 0")
            && history.contains(IndexPanickingTool::NAME),
        "the model must see the panic as tool feedback; history was: {history}"
    );
}

#[tokio::test]
async fn a_normal_tool_error_still_flows_to_the_model_unchanged() {
    let model = stream_model([
        MockTurn::tool_call("tc1", "flaky_tool", json!({})),
        MockTurn::text("done"),
    ]);
    let response = AgentBuilder::new(model.clone())
        .tool(MetadataFailingTool)
        .build()
        .runner("go")
        .max_turns(3)
        .run()
        .await
        .expect("a normal tool error must not kill the run");

    assert_eq!(response.output, "done");
    let history = serde_json::to_value(
        &model
            .requests()
            .get(1)
            .expect("second request")
            .chat_history,
    )
    .unwrap()
    .to_string();
    assert!(
        history.contains("raw timeout failure"),
        "an ordinary tool error keeps its model-visible message; history was: {history}"
    );
    assert!(!history.contains("panicked"));
}

#[tokio::test]
async fn agent_dispatch_snapshot_clones_once_and_isolates_tool_mutations() {
    let clones = Arc::new(AtomicUsize::new(0));
    let mut context = ToolContext::new();
    context.insert(SnapshotValue {
        value: 0,
        clones: clones.clone(),
    });
    let tool = SnapshotMutatingTool::default();
    let results = SnapshotResults::default();

    AgentBuilder::new(stream_model([
        MockTurn::tool_call("tc1", SnapshotMutatingTool::NAME, json!({})),
        MockTurn::tool_call("tc2", SnapshotMutatingTool::NAME, json!({})),
        MockTurn::text("done"),
    ]))
    .tool(tool.clone())
    .add_hook(results.clone())
    .build()
    .runner("go")
    .tool_context(context)
    .max_turns(4)
    .run()
    .await
    .expect("agent run");

    assert_eq!(*tool.0.lock().expect("observed values"), vec![0, 0]);
    assert_eq!(*results.0.lock().expect("result values"), vec![1, 1]);
    assert_eq!(
        clones.load(Ordering::SeqCst),
        3,
        "two dispatch clones (one per tool) plus the run-context snapshot \n         (hooks read the same capability map tools do, snapshotted once \n         per run)"
    );
}

#[tokio::test]
async fn using_model_value_replaces_the_run_model() {
    let default_model = stream_model([MockTurn::text("default answer")]);
    let override_model = stream_model([MockTurn::text("override answer")]);

    let response = AgentBuilder::new(default_model.clone())
        .build()
        .runner("question")
        .using_model_value(override_model.clone())
        .run()
        .await
        .expect("run should succeed with the run-local model");

    assert_eq!(response.output, "override answer");
    assert_eq!(default_model.request_count(), 0);
    assert_eq!(override_model.request_count(), 1);
}

#[tokio::test]
async fn merge_additional_params_replaces_a_non_object_baseline() {
    let model = stream_model([MockTurn::text("done")]);
    AgentBuilder::new(model.clone())
        .build()
        .runner("question")
        .replace_additional_params(json!("scalar-baseline"))
        .merge_additional_params(json!({"keep": 1}).as_object().expect("object").clone())
        .run()
        .await
        .expect("run should succeed");

    let request = &model.requests()[0];
    assert_eq!(
        request.additional_params,
        Some(json!({"keep": 1})),
        "merging an object into a non-object baseline must replace it wholesale"
    );
}

use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

use tokio::sync::{Barrier, Notify};

use crate::agent::hook::ToolCall as ToolCallEvent;
use crate::agent::prompt_request::streaming::{MultiTurnStreamItem, StreamingError};
use crate::completion::{CompletionError, Message, Usage};
use crate::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingPrompt};
use crate::test_utils::{MockBarrierTool, MockOperationArgs, MockSubtractTool, MockToolError};
use crate::tool::server::{ToolServer, ToolServerHandle};
use rig_core::message::{AssistantContent, ToolCall as MessageToolCall, ToolFunction, UserContent};

/// Records the kind of every hook event (and every tool-result payload) so a
/// run() and a stream() of the same scenario can be compared. The kinds are
/// labels, not `StepEventKind` — that hint machinery died with the
/// observation hooks (PROTOCOL.md flag 31).
#[derive(Clone, Default)]
struct RecordingHook {
    events: Arc<Mutex<Vec<&'static str>>>,
    tool_results: Arc<Mutex<Vec<String>>>,
}

impl RecordingHook {
    fn tool_results(&self) -> Vec<String> {
        self.tool_results.lock().expect("results lock").clone()
    }

    /// Count of a single event label across the whole run.
    fn count(&self, kind: &'static str) -> usize {
        self.events
            .lock()
            .expect("events lock")
            .iter()
            .filter(|recorded| **recorded == kind)
            .count()
    }

    fn record(&self, kind: &'static str) {
        self.events.lock().expect("events lock").push(kind);
    }
}

impl AgentHook for RecordingHook {
    async fn on_tool_call(&self, _: &HookContext, _: ToolCallEvent<'_>) -> ToolCallAction {
        self.record("tool_call");
        ToolCallAction::run()
    }
    async fn on_tool_result(
        &self,
        _: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        self.record("tool_result");
        self.tool_results
            .lock()
            .expect("results lock")
            .push(event.presentation.render());
        ToolResultAction::keep()
    }
}

fn canonical_usage() -> Usage {
    Usage {
        input_tokens: 11,
        output_tokens: 7,
        total_tokens: 18,
        ..Usage::new()
    }
}

#[tokio::test]
async fn assistant_entries_carry_announced_turn_ids_not_provider_ids() {
    // The one-value rule (ENGINE.md): the announced turn id is THE
    // entry id; provider-assigned message/response ids never enter
    // the history (telemetry metadata only). A provider that assigns
    // either id shape changes nothing here.
    let assistant_ids_of = |turn: MockTurn| async move {
        let cell = parity_cell("prompt");
        AgentBuilder::new(stream_model([turn]))
            .build()
            .runner_over(cell.clone())
            .turn_id_source(parity_turn_ids())
            .run()
            .await
            .expect("blocking response");
        let messages = parity_conversation(&cell);
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Assistant { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let with_message_id =
        assistant_ids_of(MockTurn::text("reply").with_message_id("msg_abc")).await;
    let with_response_id =
        assistant_ids_of(MockTurn::text("reply").with_response_id("chatcmpl-123")).await;
    assert_eq!(with_message_id, [Some("turn-1".to_string())]);
    assert_eq!(
        with_response_id,
        [Some("turn-1".to_string())],
        "a response-scoped id is not the entry id either"
    );
}

#[tokio::test]
async fn provider_error_after_final_hides_the_buffered_final() {
    let mut stream = AgentBuilder::new(MockCompletionModel::from_stream_turns([[
        MockStreamEvent::text("canonical response"),
        MockStreamEvent::final_response(canonical_usage()),
        MockStreamEvent::error("post-final failure"),
    ]]))
    .build()
    .runner("canonical prompt")
    .stream()
    .await;
    let mut saw_provider_final = false;
    let mut error = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Final(_))) => {
                saw_provider_final = true
            }
            Ok(_) => {}
            Err(err) => error = Some(err),
        }
    }

    assert!(!saw_provider_final, "the buffered final must remain hidden");
    assert!(matches!(
        error,
        Some(StreamingError::Completion(CompletionError::ProviderError(message)))
            if message == "post-final failure"
    ));
}

#[tokio::test]
async fn visible_assistant_items_after_final_are_rejected() {
    let cases = [
        ("text", MockStreamEvent::text("late text")),
        ("reasoning", MockStreamEvent::reasoning("late reasoning")),
        (
            "reasoning delta",
            MockStreamEvent::reasoning_delta("late reasoning"),
        ),
        (
            "tool call",
            MockStreamEvent::tool_call("late", "add", json!({"x": 1, "y": 2})),
        ),
        (
            "tool-call delta",
            MockStreamEvent::tool_call_name_delta("late", "add"),
        ),
        ("unknown", MockStreamEvent::unknown(json!({"type": "late"}))),
    ];

    for (case, visible_item) in cases {
        let mut stream = AgentBuilder::new(MockCompletionModel::from_stream_turns([vec![
            MockStreamEvent::text("canonical response"),
            MockStreamEvent::final_response(canonical_usage()),
            visible_item,
        ]]))
        .build()
        .runner("canonical prompt")
        .stream()
        .await;
        let mut saw_provider_final = false;
        let mut error = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Final(
                    _,
                ))) => saw_provider_final = true,
                Ok(_) => {}
                Err(err) => error = Some(err),
            }
        }

        assert!(
            !saw_provider_final,
            "{case}: buffered final must remain hidden"
        );
        assert!(
            matches!(
                error,
                Some(StreamingError::Completion(CompletionError::ResponseError(ref message)))
                    if message.contains("visible assistant content after its final response")
            ),
            "{case}: expected malformed-response error, got {error:?}"
        );
    }
}

#[tokio::test]
async fn visible_item_after_non_emittable_final_is_rejected() {
    let mut stream = AgentBuilder::new(MockCompletionModel::from_stream_turns([[
        MockStreamEvent::reasoning("think"),
        MockStreamEvent::final_response(canonical_usage()),
        MockStreamEvent::text("late text"),
    ]]))
    .build()
    .runner("canonical prompt")
    .stream()
    .await;
    let mut error = None;
    while let Some(item) = stream.next().await {
        if let Err(err) = item {
            error = Some(err);
        }
    }

    assert!(matches!(
        error,
        Some(StreamingError::Completion(CompletionError::ResponseError(message)))
            if message.contains("visible assistant content after its final response")
    ));
}

fn blocking_model() -> MockCompletionModel {
    stream_model([
        MockTurn::tool_call("tc1", "add", json!({"x": 2, "y": 3})),
        MockTurn::text("the answer is 5"),
    ])
}

/// Note the shape of turn one: the call's input streams as fragments
/// (`tc1`) *and* the wire restates it as one complete `ToolCall`. See
/// [`streamed_tool_call_items_share_one_internal_call_id`] for the
/// correlation contract this pins.
fn streaming_model() -> MockCompletionModel {
    MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call_name_delta("tc1", "add"),
            MockStreamEvent::tool_call_arguments_delta("tc1", "{\"x\":2,\"y\":3}"),
            MockStreamEvent::tool_call("tc1", "add", json!({"x": 2, "y": 3})),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
        vec![
            MockStreamEvent::text("the answer is 5"),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
    ])
}

/// #2258 F1, end to end: every stream item for one tool call carries the
/// same `internal_call_id` — the deltas, the completed call, the
/// execution confirmation, and the tool result. This mock has always
/// emitted deltas followed by a full `ToolCall` for `tc1`; before the
/// accumulator adopted the assembly's id, the completed call (and
/// therefore the execution and result items) carried a fresh id no delta
/// ever mentioned, and the mismatch passed silently here.
///
/// Not inducible from a recorded provider turn: no in-tree wire mixes
/// fragments with a full restatement of the same call.
#[tokio::test]
async fn streamed_tool_call_items_share_one_internal_call_id() {
    let mut stream = AgentBuilder::new(streaming_model())
        .tool(MockAddTool)
        .build()
        .runner("add 2 and 3")
        .max_turns(2)
        .stream()
        .await;

    let mut delta_ids = Vec::new();
    let mut completed_ids = Vec::new();
    let mut executed_ids = Vec::new();
    let mut result_ids = Vec::new();
    while let Some(item) = stream.next().await {
        match item.expect("stream item") {
            MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCallDelta {
                internal_call_id,
                ..
            }) => delta_ids.push(internal_call_id),
            MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                internal_call_id,
                ..
            }) => completed_ids.push(internal_call_id),
            MultiTurnStreamItem::ToolExecutionCommitted {
                internal_call_id, ..
            } => executed_ids.push(internal_call_id),
            MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                internal_call_id,
                ..
            }) => result_ids.push(internal_call_id),
            _ => {}
        }
    }

    assert_eq!(delta_ids.len(), 2, "one name delta and one argument delta");
    let correlated = delta_ids.first().expect("a delta id").clone();
    assert!(
        delta_ids.iter().all(|id| *id == correlated),
        "the fragments of one call share one id: {delta_ids:?}"
    );
    assert_eq!(completed_ids, vec![correlated.clone()]);
    assert_eq!(executed_ids, vec![correlated.clone()]);
    assert_eq!(result_ids, vec![correlated]);
}

/// `AgentRunner::from_agent` preserves the distinction between an absent
/// agent default (the implicit one-call budget) and an explicit zero budget.
#[tokio::test]
async fn from_agent_preserves_implicit_one_and_explicit_zero_budgets() {
    let implicit_model = blocking_model();
    let implicit_recorded = implicit_model.clone();
    let implicit_agent = AgentBuilder::new(implicit_model).tool(MockAddTool).build();
    let implicit_runner = super::AgentRunner::from_agent(&implicit_agent, "add 2 and 3");
    assert_eq!(implicit_runner.max_turns, 1);

    let implicit_err = implicit_runner
        .run()
        .await
        .expect_err("implicit budget should reject the second model call");
    assert!(matches!(
        implicit_err,
        PromptError::MaxTurnsError { max_turns: 1, .. }
    ));
    assert_eq!(implicit_recorded.request_count(), 1);

    let zero_model = stream_model([MockTurn::text("should not be requested")]);
    let zero_recorded = zero_model.clone();
    let zero_agent = AgentBuilder::new(zero_model).default_max_turns(0).build();
    let zero_runner = super::AgentRunner::from_agent(&zero_agent, "do not call");
    assert_eq!(zero_runner.max_turns, 0);

    let zero_err = zero_runner
        .run()
        .await
        .expect_err("explicit zero budget should reject the initial model call");
    // The entry contract (ENGINE.md): a run always executes one turn, so a
    // zero budget is a configuration error, not a run shape.
    assert!(
        matches!(&zero_err, PromptError::PromptCancelled { reason, .. }
            if reason.contains("max_turns must be at least 1")),
        "got: {zero_err:?}"
    );
    assert_eq!(zero_recorded.request_count(), 0);
}

/// The public blocking and streaming prompt surfaces enforce the one-call
/// boundary identically after executing a tool-producing first turn.
#[tokio::test]
async fn prompt_surfaces_reject_second_tool_roundtrip_request_at_budget_one() {
    let streaming_model = streaming_model();
    let streaming_recorded = streaming_model.clone();
    let streaming_agent = AgentBuilder::new(streaming_model).tool(MockAddTool).build();
    let mut stream = streaming_agent
        .stream_prompt("add 2 and 3")
        .max_turns(1)
        .await;
    let mut streaming_err = None;
    while let Some(item) = stream.next().await {
        if let Err(err) = item {
            streaming_err = Some(err);
            break;
        }
    }
    match streaming_err {
        Some(StreamingError::Prompt(err)) => assert!(matches!(
            *err,
            PromptError::MaxTurnsError { max_turns: 1, .. }
        )),
        other => panic!("expected streaming max-turns error, got {other:?}"),
    }
    assert_eq!(streaming_recorded.request_count(), 1);
}

/// Structured tool-execution results reach `ToolResultEvent` as machine
/// metadata (error/refusal state plus result context), on both the blocking and streaming paths,
/// so hooks can steer on a classified failure without parsing the result
/// string.
mod structured_tool_results {
    use std::sync::{Arc, Mutex};

    use super::stream_model;

    use futures::StreamExt;
    use serde_json::json;

    use crate::agent::{
        AgentBuilder, AgentHook, HookContext, HookStack, ToolCall, ToolCallAction,
        ToolResultAction, ToolResultEvent,
    };
    use crate::test_utils::{
        MockAddTool, MockCompletionModel, MockDeniedTool, MockFailingTool, MockHandledFailureTool,
        MockMetadataTool, MockRequestId, MockStreamEvent, MockTurn,
    };
    use crate::tool::{ToolErrorKind, ToolResult};

    /// Records, for every `ToolResult` event, a compact outcome label and the
    /// model-visible result string — the machine metadata a policy reads.
    #[derive(Clone, Default)]
    struct OutcomeHook {
        outcomes: Arc<Mutex<Vec<String>>>,
        results: Arc<Mutex<Vec<String>>>,
    }

    impl OutcomeHook {
        fn outcomes(&self) -> Vec<String> {
            self.outcomes.lock().expect("outcomes").clone()
        }

        fn results(&self) -> Vec<String> {
            self.results.lock().expect("results").clone()
        }
    }

    /// A compact string label for an outcome, e.g. `error:timeout`.
    fn outcome_label(result: &ToolResult) -> String {
        if result.is_skipped() {
            "skipped".to_string()
        } else if result.is_refused() {
            "denied".to_string()
        } else if let Some(error) = result.error() {
            format!("error:{}", error.kind().as_str())
        } else {
            "success".to_string()
        }
    }

    impl AgentHook for OutcomeHook {
        async fn on_tool_result(
            &self,
            _ctx: &HookContext,
            event: ToolResultEvent<'_>,
        ) -> ToolResultAction {
            if let ToolResultEvent {
                presentation,
                raw_result,
                ..
            } = event
            {
                self.outcomes
                    .lock()
                    .expect("outcomes")
                    .push(outcome_label(raw_result));
                self.results
                    .lock()
                    .expect("results")
                    .push(presentation.render());
            }
            ToolResultAction::keep()
        }
    }

    /// A blocking model that calls `tool` once, then answers.
    fn model_one_tool_then_text(tool: &str) -> MockCompletionModel {
        stream_model([
            MockTurn::tool_call("tc1", tool, json!({})),
            MockTurn::text("done"),
        ])
    }

    /// A streaming model that calls `tool` once, then answers.
    fn stream_model_one_tool_then_text(tool: &str) -> MockCompletionModel {
        MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::tool_call_name_delta("tc1", tool),
                MockStreamEvent::tool_call_arguments_delta("tc1", "{}"),
                MockStreamEvent::tool_call("tc1", tool, json!({})),
                MockStreamEvent::final_response_with_total_tokens(0),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(0),
            ],
        ])
    }

    // (1) A `Timeout` failure reaches `ToolResultEvent` as structured
    // metadata (not just a string), with the model-visible feedback intact.
    #[tokio::test]
    async fn timeout_failure_surfaces_structured_outcome() {
        let hook = OutcomeHook::default();
        AgentBuilder::new(model_one_tool_then_text("flaky_tool"))
            .tool(MockFailingTool::new(ToolErrorKind::Timeout))
            .add_hook(hook.clone())
            .build()
            .runner("go")
            .max_turns(3)
            .run()
            .await
            .expect("run should succeed; a tool timeout is model-visible feedback, not fatal");

        assert_eq!(hook.outcomes(), vec!["error:timeout".to_string()]);
        // (4) The model still receives useful text for the handled failure.
        assert_eq!(hook.results(), vec!["mock tool call failed".to_string()]);
    }

    // (2) A hook counts timeout failures in its own state and terminates
    // the run after a threshold — the motivating use case. (The count is
    // hook-local: the run Scratchpad died with the observation hooks,
    // PROTOCOL.md flag 31.)
    #[tokio::test]
    async fn hook_terminates_after_repeated_timeouts() {
        #[derive(Clone, Default)]
        struct TimeoutTerminator {
            timeouts: Arc<Mutex<usize>>,
        }
        impl AgentHook for TimeoutTerminator {
            async fn on_tool_result(
                &self,
                _ctx: &HookContext,
                event: ToolResultEvent<'_>,
            ) -> ToolResultAction {
                if let ToolResultEvent { raw_result, .. } = event
                    && raw_result.is_error_kind(ToolErrorKind::Timeout)
                {
                    let mut count = self.timeouts.lock().expect("timeout count");
                    *count += 1;
                    if *count >= 2 {
                        return ToolResultAction::stop("aborting after repeated tool timeouts");
                    }
                }
                ToolResultAction::keep()
            }
        }

        let observer = OutcomeHook::default();
        let err = AgentBuilder::new(stream_model([
            MockTurn::tool_call("tc1", "flaky_tool", json!({})),
            MockTurn::tool_call("tc2", "flaky_tool", json!({})),
            MockTurn::text("unreachable"),
        ]))
        .tool(MockFailingTool::new(ToolErrorKind::Timeout))
        // Observer first so it records both timeouts before the terminator fires.
        .add_hook(observer.clone())
        .add_hook(TimeoutTerminator::default())
        .build()
        .runner("go")
        .max_turns(5)
        .run()
        .await
        .expect_err("the run must terminate after two timeouts");

        assert!(
            err.to_string()
                .contains("aborting after repeated tool timeouts"),
            "unexpected error: {err}"
        );
        assert_eq!(
            observer.outcomes(),
            vec!["error:timeout".to_string(), "error:timeout".to_string()],
            "both timeout outcomes must be observed before termination"
        );
    }

    // (3) A not-found (404) failure surfaces as structured `NotFound` metadata
    // but does not terminate the run by default — the model may try another path.
    #[tokio::test]
    async fn not_found_outcome_is_structured_and_non_fatal() {
        let hook = OutcomeHook::default();
        let status: Arc<Mutex<Option<u16>>> = Arc::new(Mutex::new(None));

        struct StatusProbe(Arc<Mutex<Option<u16>>>);
        impl AgentHook for StatusProbe {
            async fn on_tool_result(
                &self,
                _ctx: &HookContext,
                event: ToolResultEvent<'_>,
            ) -> ToolResultAction {
                if let Some(error) = event.raw_result.error() {
                    *self.0.lock().expect("status") = error.http_status();
                }
                ToolResultAction::keep()
            }
        }

        AgentBuilder::new(model_one_tool_then_text("flaky_tool"))
            .tool(MockFailingTool::new(ToolErrorKind::NotFound))
            .add_hook(hook.clone())
            .add_hook(StatusProbe(status.clone()))
            .build()
            .runner("go")
            .max_turns(3)
            .run()
            .await
            .expect("a 404 must not terminate the run by default");

        assert_eq!(hook.outcomes(), vec!["error:not_found".to_string()]);
        assert_eq!(
            *status.lock().expect("status"),
            Some(404),
            "the structured failure must carry the HTTP status"
        );
    }

    // (4) A tool that returns a handled failure via ordinary `Result` shows the
    // model useful output while the outcome is a classified error.
    #[tokio::test]
    async fn handled_failure_delivers_model_output_and_error_outcome() {
        let hook = OutcomeHook::default();
        AgentBuilder::new(model_one_tool_then_text("lookup"))
            .tool(MockHandledFailureTool)
            .add_hook(hook.clone())
            .build()
            .runner("go")
            .max_turns(3)
            .run()
            .await
            .expect("a handled failure is not fatal");

        assert_eq!(hook.outcomes(), vec!["error:not_found".to_string()]);
        assert_eq!(
            hook.results(),
            vec!["no record found for id 42; try a different id".to_string()],
            "the tool's model-visible output must survive alongside the error outcome"
        );
    }

    // (7) `ToolCallAction::Skip` on the tool-call produces a structured `Skipped`
    // outcome that the result hook observes.
    #[tokio::test]
    async fn flow_skip_produces_skipped_outcome() {
        struct SkipHook;
        impl AgentHook for SkipHook {
            async fn on_tool_call(
                &self,
                _ctx: &HookContext,
                event: ToolCall<'_>,
            ) -> ToolCallAction {
                if let ToolCall { .. } = event {
                    ToolCallAction::skip("not executed (denied by policy); do not retry")
                } else {
                    ToolCallAction::run()
                }
            }
        }

        let observer = OutcomeHook::default();
        AgentBuilder::new(model_one_tool_then_text("flaky_tool"))
            .tool(MockFailingTool::new(ToolErrorKind::Timeout))
            .add_hook(SkipHook)
            .add_hook(observer.clone())
            .build()
            .runner("go")
            .max_turns(3)
            .run()
            .await
            .expect("run should succeed after skipping the tool");

        assert_eq!(observer.outcomes(), vec!["skipped".to_string()]);
        assert_eq!(
            observer.results(),
            vec!["not executed (denied by policy); do not retry".to_string()]
        );
    }

    // A *tool-authored* refusal surfaces as a `Denied`
    // outcome — distinct from a hook `ToolCallAction::Skip`, which is `Skipped`. This
    // pins the documented `Skipped` vs `Denied` split: `Denied` comes only
    // from the tool, never from a hook skip.
    #[tokio::test]
    async fn tool_authored_denial_produces_denied_outcome() {
        let hook = OutcomeHook::default();
        AgentBuilder::new(model_one_tool_then_text("guarded"))
            .tool(MockDeniedTool)
            .add_hook(hook.clone())
            .build()
            .runner("go")
            .max_turns(3)
            .run()
            .await
            .expect("a tool-authored denial is not fatal");

        assert_eq!(hook.outcomes(), vec!["denied".to_string()]);
        assert_eq!(
            hook.results(),
            vec!["access to this resource is not permitted".to_string()],
            "the model still receives the tool's denial message"
        );
    }

    #[tokio::test]
    async fn permission_denied_failure_is_not_a_tool_refusal() {
        let hook = OutcomeHook::default();
        AgentBuilder::new(model_one_tool_then_text("flaky_tool"))
            .tool(MockFailingTool::new(ToolErrorKind::PermissionDenied))
            .add_hook(hook.clone())
            .build()
            .runner("go")
            .max_turns(3)
            .run()
            .await
            .expect("a permission failure is model-visible feedback, not fatal");

        assert_eq!(hook.outcomes(), vec!["error:permission_denied".to_string()]);
        assert_eq!(hook.results(), vec!["mock tool call failed".to_string()]);
    }

    // A `ToolCallAction::Rewrite` hook followed by a `Skip` hook: the tool must not run,
    // the `ToolResult` reports the *rewritten* args (not the model's
    // original), and the outcome is `Skipped` — the rewrite (e.g. a
    // redaction) is not lost when a later hook short-circuits. Verified on
    // both the blocking and streaming surfaces.
    #[tokio::test]
    async fn rewrite_args_then_skip_reports_rewritten_args() {
        // Rewrites the tool args, replacing whatever the model emitted.
        struct RewriteHook;
        impl AgentHook for RewriteHook {
            async fn on_tool_call(
                &self,
                _ctx: &HookContext,
                event: ToolCall<'_>,
            ) -> ToolCallAction {
                if let ToolCall { .. } = event {
                    ToolCallAction::rewrite(json!({ "x": 41, "y": 1 }))
                } else {
                    ToolCallAction::run()
                }
            }
        }
        // Skips *after* the rewrite (registered second).
        struct SkipHook;
        impl AgentHook for SkipHook {
            async fn on_tool_call(
                &self,
                _ctx: &HookContext,
                event: ToolCall<'_>,
            ) -> ToolCallAction {
                if let ToolCall { .. } = event {
                    ToolCallAction::skip("denied after rewrite")
                } else {
                    ToolCallAction::run()
                }
            }
        }
        // Records the args + outcome seen on the `ToolResult` event.
        #[derive(Clone, Default)]
        struct ArgsProbe {
            args: Arc<Mutex<Option<String>>>,
            outcome: Arc<Mutex<Option<String>>>,
        }
        impl AgentHook for ArgsProbe {
            async fn on_tool_result(
                &self,
                _ctx: &HookContext,
                event: ToolResultEvent<'_>,
            ) -> ToolResultAction {
                if let ToolResultEvent {
                    args, raw_result, ..
                } = event
                {
                    *self.args.lock().expect("args") = Some(args.to_string());
                    *self.outcome.lock().expect("outcome") = Some(outcome_label(raw_result));
                }
                ToolResultAction::keep()
            }
        }

        async fn run_surface(streaming: bool) -> (String, String) {
            let probe = ArgsProbe::default();
            // The tool must never execute; `MockAddTool` would produce a
            // `Success` outcome with result "42" if it (wrongly) ran.
            if streaming {
                let mut stream = AgentBuilder::new(stream_model_one_tool_then_text("add"))
                    .tool(MockAddTool)
                    .add_hook(RewriteHook)
                    .add_hook(SkipHook)
                    .add_hook(probe.clone())
                    .build()
                    .runner("go")
                    .max_turns(3)
                    .stream()
                    .await;
                while let Some(item) = stream.next().await {
                    if let Err(err) = item {
                        panic!("stream item errored: {err}");
                    }
                }
            } else {
                AgentBuilder::new(model_one_tool_then_text("add"))
                    .tool(MockAddTool)
                    .add_hook(RewriteHook)
                    .add_hook(SkipHook)
                    .add_hook(probe.clone())
                    .build()
                    .runner("go")
                    .max_turns(3)
                    .run()
                    .await
                    .expect("run should succeed after skipping the tool");
            }
            let args = probe.args.lock().expect("args").clone().expect("args seen");
            let outcome = probe
                .outcome
                .lock()
                .expect("outcome")
                .clone()
                .expect("outcome seen");
            (args, outcome)
        }

        for streaming in [false, true] {
            let (args, outcome) = run_surface(streaming).await;
            assert_eq!(
                outcome, "skipped",
                "the skipped tool must produce a Skipped outcome (streaming={streaming})"
            );
            let parsed: serde_json::Value =
                serde_json::from_str(&args).expect("ToolResult args are valid JSON");
            assert_eq!(
                parsed,
                json!({ "x": 41, "y": 1 }),
                "the skipped ToolResult must report the rewritten args, not the model's \
                     original {{}} (streaming={streaming}); got {args}"
            );
        }
    }

    // End-to-end nesting: a *nested* `HookStack` that rewrites args then skips
    // must still report the rewritten args on the skipped `ToolResult` — the
    // inner rewrite is not lost behind the inner skip when the stack is added
    // as a single composed hook. Guards the nested-composition fix.
    #[tokio::test]
    async fn nested_hook_stack_rewrite_then_skip_reports_rewritten_args() {
        struct RewriteHook;
        impl AgentHook for RewriteHook {
            async fn on_tool_call(
                &self,
                _ctx: &HookContext,
                event: ToolCall<'_>,
            ) -> ToolCallAction {
                if let ToolCall { .. } = event {
                    ToolCallAction::rewrite(json!({ "x": 41, "y": 1 }))
                } else {
                    ToolCallAction::run()
                }
            }
        }
        struct SkipHook;
        impl AgentHook for SkipHook {
            async fn on_tool_call(
                &self,
                _ctx: &HookContext,
                event: ToolCall<'_>,
            ) -> ToolCallAction {
                if let ToolCall { .. } = event {
                    ToolCallAction::skip("denied after nested rewrite")
                } else {
                    ToolCallAction::run()
                }
            }
        }
        #[derive(Clone, Default)]
        struct ArgsProbe {
            args: Arc<Mutex<Option<String>>>,
            outcome: Arc<Mutex<Option<String>>>,
        }
        impl AgentHook for ArgsProbe {
            async fn on_tool_result(
                &self,
                _ctx: &HookContext,
                event: ToolResultEvent<'_>,
            ) -> ToolResultAction {
                if let ToolResultEvent {
                    args, raw_result, ..
                } = event
                {
                    *self.args.lock().expect("args") = Some(args.to_string());
                    *self.outcome.lock().expect("outcome") = Some(outcome_label(raw_result));
                }
                ToolResultAction::keep()
            }
        }

        // The rewrite + skip live inside a *nested* stack added as one hook.
        fn nested_stack() -> HookStack {
            let mut nested = HookStack::new();
            nested.push(RewriteHook);
            nested.push(SkipHook);
            nested
        }

        // Verified on both surfaces: run_single_tool (shared) drives the same
        // nested resolution, so blocking and streaming must agree.
        for streaming in [false, true] {
            let probe = ArgsProbe::default();
            if streaming {
                let mut stream = AgentBuilder::new(stream_model_one_tool_then_text("add"))
                    .tool(MockAddTool)
                    .add_hook(nested_stack())
                    .add_hook(probe.clone())
                    .build()
                    .runner("go")
                    .max_turns(3)
                    .stream()
                    .await;
                while let Some(item) = stream.next().await {
                    if let Err(err) = item {
                        panic!("stream item errored: {err}");
                    }
                }
            } else {
                AgentBuilder::new(model_one_tool_then_text("add"))
                    .tool(MockAddTool)
                    .add_hook(nested_stack())
                    .add_hook(probe.clone())
                    .build()
                    .runner("go")
                    .max_turns(3)
                    .run()
                    .await
                    .expect("run should succeed after the nested stack skips the tool");
            }

            assert_eq!(
                probe.outcome.lock().expect("outcome").clone(),
                Some("skipped".to_string()),
                "streaming={streaming}"
            );
            let args = probe.args.lock().expect("args").clone().expect("args seen");
            let parsed: serde_json::Value = serde_json::from_str(&args).expect("valid JSON args");
            assert_eq!(
                parsed,
                json!({ "x": 41, "y": 1 }),
                "the nested stack's rewrite must survive its skip and reach the ToolResult \
                     (streaming={streaming}); got {args}"
            );
        }
    }

    // (8) Invalid JSON arguments are classified as a structured `InvalidArgs`
    // failure rather than surfacing as an opaque string.
    #[tokio::test]
    async fn invalid_args_are_classified_as_invalid_args() {
        let hook = OutcomeHook::default();
        AgentBuilder::new(stream_model([
            // `add` needs integers; a string is a hard parse failure.
            MockTurn::tool_call("tc1", "add", json!({ "x": "not-a-number", "y": 1 })),
            MockTurn::text("done"),
        ]))
        .tool(MockAddTool)
        .add_hook(hook.clone())
        .build()
        .runner("go")
        .max_turns(3)
        .run()
        .await
        .expect("an invalid-args failure is model-visible feedback, not fatal");

        assert_eq!(hook.outcomes(), vec!["error:invalid_args".to_string()]);
    }

    // Result metadata a tool attaches reaches the hook but never appears in the
    // model-visible output on either execution surface.
    #[tokio::test]
    async fn success_result_metadata_reaches_hook_but_not_model() {
        struct MetadataProbe {
            seen: Arc<Mutex<Option<String>>>,
            model_output: Arc<Mutex<Option<String>>>,
        }
        impl AgentHook for MetadataProbe {
            async fn on_tool_result(
                &self,
                _ctx: &HookContext,
                event: ToolResultEvent<'_>,
            ) -> ToolResultAction {
                if let ToolResultEvent {
                    presentation,
                    tool_context,
                    ..
                } = event
                {
                    *self.seen.lock().expect("seen") = tool_context
                        .result::<MockRequestId>()
                        .map(|id| id.0.clone());
                    *self.model_output.lock().expect("model_output") = Some(presentation.render());
                }
                ToolResultAction::keep()
            }
        }

        async fn run_surface(streaming: bool) -> (Option<String>, String) {
            let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let model_output: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let probe = MetadataProbe {
                seen: seen.clone(),
                model_output: model_output.clone(),
            };

            if streaming {
                let mut stream = AgentBuilder::new(stream_model_one_tool_then_text("with_meta"))
                    .tool(MockMetadataTool)
                    .add_hook(probe)
                    .build()
                    .runner("go")
                    .max_turns(3)
                    .stream()
                    .await;
                while let Some(item) = stream.next().await {
                    if let Err(error) = item {
                        panic!("stream item errored: {error}");
                    }
                }
            } else {
                AgentBuilder::new(model_one_tool_then_text("with_meta"))
                    .tool(MockMetadataTool)
                    .add_hook(probe)
                    .build()
                    .runner("go")
                    .max_turns(3)
                    .run()
                    .await
                    .expect("run should succeed");
            }

            let seen_value = seen.lock().expect("seen").clone();
            let output = model_output
                .lock()
                .expect("model_output")
                .clone()
                .expect("output");
            (seen_value, output)
        }

        for streaming in [false, true] {
            let (seen, output) = run_surface(streaming).await;
            assert_eq!(
                seen,
                Some("req-7".to_string()),
                "the tool's result metadata must reach the hook (streaming={streaming})"
            );
            assert_eq!(output, "done");
            assert!(
                !output.contains("req-7"),
                "result metadata must never leak into model output (streaming={streaming})"
            );
        }
    }

    // (6) A `ToolResultAction::Rewrite` hook redacts the model-visible text, but a later
    // policy hook still sees the tool's *raw* structured outcome — a rewrite
    // changes only what the model sees, not the classification.
    #[tokio::test]
    async fn rewrite_result_does_not_mask_the_structured_outcome() {
        struct Redact;
        impl AgentHook for Redact {
            async fn on_tool_result(
                &self,
                _ctx: &HookContext,
                event: ToolResultEvent<'_>,
            ) -> ToolResultAction {
                if let ToolResultEvent { .. } = event {
                    ToolResultAction::rewrite("[REDACTED]")
                } else {
                    ToolResultAction::keep()
                }
            }
        }

        let observer = OutcomeHook::default();
        AgentBuilder::new(model_one_tool_then_text("flaky_tool"))
            .tool(MockFailingTool::new(ToolErrorKind::NotFound))
            // Observer AFTER the redactor: it still sees the true outcome, and
            // the chained (redacted) model-visible result.
            .add_hook(Redact)
            .add_hook(observer.clone())
            .build()
            .runner("go")
            .max_turns(3)
            .run()
            .await
            .expect("run should succeed");

        assert_eq!(observer.outcomes(), vec!["error:not_found".to_string()]);
        assert_eq!(observer.results(), vec!["[REDACTED]".to_string()]);
    }

    // (10) With two tools in one turn at `concurrency > 1`, both structured
    // outcomes are observed and the persisted tool results keep call order.
    #[tokio::test]
    async fn concurrent_tools_preserve_order_and_both_outcomes() {
        use rig_core::message::{
            AssistantContent, ToolCall as MessageToolCall, ToolFunction, UserContent,
        };

        let turn = MockTurn::from_contents([
            AssistantContent::ToolCall(MessageToolCall::new(
                "tc_add".to_string(),
                ToolFunction::new("add".to_string(), json!({ "x": 2, "y": 3 })),
            )),
            AssistantContent::ToolCall(MessageToolCall::new(
                "tc_flaky".to_string(),
                ToolFunction::new("flaky_tool".to_string(), json!({})),
            )),
        ])
        .expect("two tool calls");

        let observer = OutcomeHook::default();
        let concurrent_cell = super::parity_cell("go");
        let _response = AgentBuilder::new(stream_model([turn, MockTurn::text("done")]))
            .tool(MockAddTool)
            .tool(MockFailingTool::new(ToolErrorKind::Timeout))
            .add_hook(observer.clone())
            .build()
            .runner_over(concurrent_cell.clone())
            .max_turns(3)
            .tool_concurrency(2)
            .run()
            .await
            .expect("run should succeed");

        // Hook order may interleave under concurrency, so compare as a set.
        let mut outcomes = observer.outcomes();
        outcomes.sort();
        assert_eq!(
            outcomes,
            vec!["error:timeout".to_string(), "success".to_string()]
        );

        // The persisted tool results must keep tool-call order regardless of
        // completion timing: `add` (tc_add) before `flaky_tool` (tc_flaky).
        let messages = super::parity_conversation(&concurrent_cell);
        let tool_result_ids: Vec<String> = messages
            .iter()
            .flat_map(|message| match message {
                crate::completion::Message::User { content } => content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::ToolResult(result) => Some(result.id.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        assert_eq!(
            tool_result_ids,
            vec!["tc_add".to_string(), "tc_flaky".to_string()],
            "tool results must be persisted in call order"
        );
    }
}

/// Safety net for the streaming/non-streaming unification: pins the blocking
/// driver's span topology (span name, `invoke_agent` creation, the
/// `follows_from` chain, and `created_agent_span`-gated run-level usage) so a
/// later refactor onto a shared engine cannot silently drift it. The
/// streaming side is already pinned by `assert_stream_usage_recorded_on_chat_spans`.
mod span_safety_net {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Record};
    use tracing::{Id, Subscriber};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::{Layer, Registry, registry::LookupSpan};

    use super::stream_model;
    use crate::agent::AgentBuilder;
    use crate::test_utils::MockTurn;

    #[derive(Clone)]
    struct CapturedSpan {
        id: u64,
        name: String,
        field_names: HashSet<String>,
        u64_fields: HashMap<String, u64>,
        string_fields: HashMap<String, Vec<String>>,
    }

    #[derive(Clone, Default)]
    struct Captured {
        spans: Arc<Mutex<Vec<CapturedSpan>>>,
        /// `(span, follows_from)` pairs recorded via `Span::follows_from`.
        follows: Arc<Mutex<Vec<(u64, u64)>>>,
    }

    impl Captured {
        fn insert(&self, id: &Id, name: &str) {
            self.spans.lock().expect("spans").push(CapturedSpan {
                id: id.into_u64(),
                name: name.to_string(),
                field_names: HashSet::new(),
                u64_fields: HashMap::new(),
                string_fields: HashMap::new(),
            });
        }

        fn record(
            &self,
            id: &Id,
            names: HashSet<String>,
            u64s: HashMap<String, u64>,
            strings: HashMap<String, String>,
        ) {
            let id = id.into_u64();
            if let Ok(mut spans) = self.spans.lock()
                && let Some(span) = spans.iter_mut().find(|s| s.id == id)
            {
                span.field_names.extend(names);
                span.u64_fields.extend(u64s);
                for (name, value) in strings {
                    span.string_fields.entry(name).or_default().push(value);
                }
            }
        }

        fn follows_from(&self, span: &Id, follows: &Id) {
            self.follows
                .lock()
                .expect("follows")
                .push((span.into_u64(), follows.into_u64()));
        }

        fn clear(&self) {
            self.spans.lock().expect("spans").clear();
            self.follows.lock().expect("follows").clear();
        }

        fn snapshot(&self) -> Vec<CapturedSpan> {
            self.spans.lock().expect("spans").clone()
        }

        fn follows_edges(&self) -> Vec<(u64, u64)> {
            self.follows.lock().expect("follows").clone()
        }
    }

    struct CaptureLayer {
        captured: Captured,
    }

    impl<S> Layer<S> for CaptureLayer
    where
        S: Subscriber + for<'l> LookupSpan<'l>,
    {
        fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
            self.captured.insert(id, attrs.metadata().name());
        }

        fn on_record(&self, span: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
            let mut visitor = FieldVisitor::default();
            values.record(&mut visitor);
            self.captured
                .record(span, visitor.names, visitor.u64s, visitor.strings);
        }

        fn on_follows_from(&self, span: &Id, follows: &Id, _ctx: Context<'_, S>) {
            self.captured.follows_from(span, follows);
        }
    }

    #[derive(Default)]
    struct FieldVisitor {
        names: HashSet<String>,
        u64s: HashMap<String, u64>,
        strings: HashMap<String, String>,
    }

    impl Visit for FieldVisitor {
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.names.insert(field.name().to_string());
            self.u64s.insert(field.name().to_string(), value);
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.names.insert(field.name().to_string());
            self.strings
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.names.insert(field.name().to_string());
            self.strings
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }

    #[tokio::test]
    async fn run_adopts_a_caller_supplied_outer_span() {
        let _isolation = crate::test_utils::scoped_tracing_subscriber_guard().await;
        let captured = Captured::default();
        let subscriber = Registry::default().with(CaptureLayer {
            captured: captured.clone(),
        });
        let _default = tracing::subscriber::set_default(subscriber);

        let outer = tracing::info_span!("outer");
        let agent = AgentBuilder::new(stream_model([MockTurn::text("done")])).build();
        let run = agent.runner("hello").run();
        outer.in_scope(|| {
            let _ = futures::executor::block_on(run);
        });

        let spans = captured.snapshot();
        assert!(
            spans.iter().all(|s| s.name != "invoke_agent"),
            "an ambient outer span should be adopted, not wrapped in invoke_agent"
        );
        assert!(
            spans.iter().any(|s| s.name == "outer"),
            "the ambient outer span stays the run's parent"
        );
    }
}

fn tool_call_content(id: &str, args: serde_json::Value) -> AssistantContent {
    AssistantContent::ToolCall(MessageToolCall::new(
        id.to_string(),
        ToolFunction::new("add".to_string(), args),
    ))
}

/// Whether any tool result in `messages` carries `expected` as verbatim text.
/// Used to pin a skip reason's actual value (a reason dropped or altered on
/// both drivers would still satisfy a blocking == streaming equality check).
fn tool_result_text_in_history(messages: &[Message], expected: &str) -> bool {
    messages.iter().any(|message| {
        matches!(
            message,
            Message::User { content }
                if content.iter().any(|item| matches!(
                    item,
                    UserContent::ToolResult(result)
                        if result.content.iter().any(|c| matches!(
                            c,
                            rig_core::message::ToolResultContent::Text(text)
                                if text.text == expected
                        ))
                ))
        )
    })
}

/// A tool whose first-*called* invocation completes *after* the second, so
/// `buffer_unordered` yields the results in completion order — yet the
/// persisted history stays in call order because each result is written into
/// its original call-index slot. The first call (in poll/call order) waits on
/// a gate the second call releases.
#[derive(Clone)]
struct OutOfOrderTool {
    gate: Arc<tokio::sync::Notify>,
    order: Arc<AtomicU32>,
}

impl Tool for OutOfOrderTool {
    const NAME: &'static str = "add";
    type Error = MockToolError;
    type Args = MockOperationArgs;
    type Output = i32;

    fn description(&self) -> String {
        MockAddTool.description()
    }

    fn parameters(&self) -> serde_json::Value {
        MockAddTool.parameters()
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let nth = self.order.fetch_add(1, SeqCst);
        if nth == 0 {
            // First call: cannot finish until a later call releases us.
            self.gate.notified().await;
        } else {
            // Later call: finishes immediately and releases the first.
            self.gate.notify_one();
        }
        Ok(nth as i32)
    }
}

/// `run()` must persist tool results in tool-call (emission) order even when
/// tools complete out of order under concurrency — it runs them with
/// `buffer_unordered` but reindexes each result into its original call-index
/// slot. (This is what keeps its message history identical to the sequential
/// streaming driver.)
#[tokio::test]
async fn run_preserves_tool_call_order_under_out_of_order_completion() {
    let model = stream_model([
        MockTurn::from_contents([
            tool_call_content("tc1", json!({"x": 1, "y": 0})),
            tool_call_content("tc2", json!({"x": 2, "y": 0})),
        ])
        .expect("two tool calls is a valid turn"),
        MockTurn::text("done"),
    ]);
    let cell = parity_cell("go");
    let _response = AgentBuilder::new(model)
        .tool(OutOfOrderTool {
            gate: Arc::new(tokio::sync::Notify::new()),
            order: Arc::new(AtomicU32::new(0)),
        })
        .build()
        .runner_over(cell.clone())
        .max_turns(3)
        .tool_concurrency(4)
        .run()
        .await
        .expect("run should succeed");

    let messages = parity_conversation(&cell);
    let result_ids: Vec<String> = messages
        .iter()
        .flat_map(|message| match message {
            Message::User { content } => content
                .iter()
                .filter_map(|item| match item {
                    UserContent::ToolResult(result) => Some(result.id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
    // Call order (tc1 then tc2), even though tc2 finished first.
    assert_eq!(result_ids, vec!["tc1".to_string(), "tc2".to_string()]);
}

/// Tool-result ids, in history order, across a run's message history.
fn tool_result_ids(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| match message {
            Message::User { content } => content
                .iter()
                .filter_map(|item| match item {
                    UserContent::ToolResult(result) => Some(result.id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

/// The streaming driver under concurrency persists tool results in **call
/// order** even when tools complete out of order. `OutOfOrderTool`'s
/// first-called invocation only finishes once the second runs, so this also
/// proves the tools run concurrently: sequential execution would deadlock on
/// the first call.
#[tokio::test]
async fn stream_preserves_history_order_under_out_of_order_completion() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tc1", "add", json!({"x": 1, "y": 0})),
            MockStreamEvent::tool_call("tc2", "add", json!({"x": 2, "y": 0})),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
    ]);
    let cell = parity_cell("go");
    let stream = AgentBuilder::new(model)
        .tool(OutOfOrderTool {
            gate: Arc::new(tokio::sync::Notify::new()),
            order: Arc::new(AtomicU32::new(0)),
        })
        .build()
        .runner_over(cell.clone())
        .max_turns(3)
        .turn_id_source(parity_turn_ids())
        .tool_concurrency(4)
        .stream()
        .await;
    // Timeout so a regression to sequential execution fails cleanly instead
    // of hanging (the first call only completes once the second runs).
    let _final_response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        drive_to_final_response(stream),
    )
    .await
    .expect("streamed tools must run concurrently, not deadlock on the first call");

    let messages = parity_conversation(&cell);
    // History stays in call order (tc1 then tc2), even though tc2 finished first.
    assert_eq!(
        tool_result_ids(&messages),
        vec!["tc1".to_string(), "tc2".to_string()]
    );
}

/// Under concurrency the streaming driver surfaces tool results **atomically
/// after the whole batch settles**, in **call order** — not as each tool
/// completes. The second call completes first (via the gate), yet its result
/// is still surfaced second, matching persisted history order.
#[tokio::test]
async fn stream_emits_tool_results_in_call_order_after_batch_settles_under_concurrency() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tc1", "add", json!({"x": 1, "y": 0})),
            MockStreamEvent::tool_call("tc2", "add", json!({"x": 2, "y": 0})),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
    ]);
    let cell = parity_cell("go");
    let mut stream = AgentBuilder::new(model)
        .tool(OutOfOrderTool {
            gate: Arc::new(tokio::sync::Notify::new()),
            order: Arc::new(AtomicU32::new(0)),
        })
        .build()
        .runner_over(cell.clone())
        .max_turns(3)
        .tool_concurrency(4)
        .stream()
        .await;

    let mut streamed_result_ids = Vec::new();
    let mut final_response = None;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(item) = stream.next().await {
            match item.unwrap_or_else(|err| panic!("stream item errored: {err}")) {
                MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                    tool_result,
                    ..
                }) => streamed_result_ids.push(tool_result.id),
                MultiTurnStreamItem::FinalResponse(resp) => final_response = Some(resp),
                _ => {}
            }
        }
    })
    .await
    .expect("streamed tools must run concurrently, not deadlock on the first call");

    // Call order, even though tc2 completed first — results are surfaced only
    // after the whole batch settles.
    assert_eq!(
        streamed_result_ids,
        vec!["tc1".to_string(), "tc2".to_string()]
    );
    final_response.expect("stream should yield a final response");
    assert_eq!(
        tool_result_ids(&parity_conversation(&cell)),
        vec!["tc1".to_string(), "tc2".to_string()]
    );
}

/// Two barrier-synchronized tools in one streamed turn finish only if they
/// run concurrently — each waits at the barrier for the other. At
/// `tool_concurrency(2)` the streamed turn completes; sequential execution
/// would block on the first call forever, so the timeout asserts genuine
/// concurrency on the streaming path.
#[tokio::test]
async fn stream_executes_tools_concurrently_under_concurrency() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("b1", "barrier_tool", json!({})),
            MockStreamEvent::tool_call("b2", "barrier_tool", json!({})),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
    ]);
    let stream = AgentBuilder::new(model)
        .tool(MockBarrierTool::new(barrier))
        .build()
        .runner("hit the barrier twice")
        .max_turns(3)
        .tool_concurrency(2)
        .stream()
        .await;

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        drive_to_final_response(stream),
    )
    .await
    .expect("streamed tools must run concurrently, not deadlock at the barrier");
}

/// The stream-item taxonomy and ordering: the driver emits *all* of a turn's
/// **model** tool-call items ([`StreamedAssistantContent::ToolCall`], one per
/// call the model made) first, then — after the whole tool batch settles —
/// the per-tool **execution** items (`ToolExecutionCommitted` then the
/// `ToolResult`) in call order. This holds identically at every concurrency
/// (the batch is atomic on both the sequential and concurrent paths).
#[tokio::test]
async fn stream_emits_model_tool_calls_then_atomic_execution_items() {
    async fn markers(concurrency: usize) -> Vec<&'static str> {
        let model = MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::tool_call("tc1", "add", json!({"x": 1, "y": 1})),
                MockStreamEvent::tool_call("tc2", "add", json!({"x": 2, "y": 2})),
                MockStreamEvent::final_response_with_total_tokens(0),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(0),
            ],
        ]);
        let mut stream = AgentBuilder::new(model)
            .tool(MockAddTool)
            .build()
            .runner("add two pairs")
            .max_turns(3)
            .tool_concurrency(concurrency)
            .stream()
            .await;
        let mut markers = Vec::new();
        while let Some(item) = stream.next().await {
            match item.unwrap_or_else(|err| panic!("stream item errored: {err}")) {
                MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                    ..
                }) => markers.push("model-call"),
                MultiTurnStreamItem::ToolExecutionCommitted { .. } => markers.push("exec-commit"),
                MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult { .. }) => {
                    markers.push("result")
                }
                _ => {}
            }
        }
        markers
    }

    // Both surfaces: all model tool calls first, then per-tool (start, result)
    // in call order, surfaced atomically after the batch settles.
    let expected = vec![
        "model-call",
        "model-call",
        "exec-commit",
        "result",
        "exec-commit",
        "result",
    ];
    assert_eq!(markers(1).await, expected);
    assert_eq!(markers(4).await, expected);
}

/// Records the `x` arg of every tool call that reaches its body. The `x == 1`
/// sibling signals it has started (via `sibling_started`) and then stays
/// pending across several polls, so it is genuinely in flight when the
/// terminator (`x == 0`) fires — while a sibling beyond the concurrency
/// window is not yet started and must be dropped.
#[derive(Clone)]
struct RecordingArgsTool {
    called: Arc<Mutex<Vec<i64>>>,
    sibling_started: Arc<tokio::sync::Notify>,
}

impl Tool for RecordingArgsTool {
    const NAME: &'static str = "add";
    type Error = MockToolError;
    type Args = serde_json::Value;
    type Output = i32;

    fn description(&self) -> String {
        MockAddTool.description()
    }

    fn parameters(&self) -> serde_json::Value {
        MockAddTool.parameters()
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let x = args.get("x").and_then(serde_json::Value::as_i64);
        if let Some(x) = x {
            self.called.lock().expect("called").push(x);
        }
        if x == Some(1) {
            // Signal that the in-flight sibling has started, then stay pending
            // so it is still executing when the terminator fires.
            self.sibling_started.notify_one();
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
        }
        Ok(0)
    }
}

/// Stops after the `x == 0` tool's result, but only after the `x == 1`
/// sibling has signalled it started executing — so tc1 is genuinely in
/// flight (not merely not-yet-started) when the stop decision lands.
struct TerminateOnArgZeroAfterSiblingHook {
    sibling_started: Arc<tokio::sync::Notify>,
}
impl AgentHook for TerminateOnArgZeroAfterSiblingHook {
    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        if let ToolResultEvent { args, .. } = event
            && serde_json::from_str::<serde_json::Value>(args)
                .ok()
                .and_then(|v| v.get("x").and_then(serde_json::Value::as_i64))
                == Some(0)
        {
            self.sibling_started.notified().await;
            return ToolResultAction::stop("stop");
        }
        ToolResultAction::keep()
    }
}

/// Concurrent post-result stop: the batch is sealed (ENGINE.md, stop
/// taxonomy), so every chain — in flight, beyond the concurrency window, and
/// the stopper itself — runs to completion and its result is collected.
/// With concurrency 2 and three tools: tc0 (`x == 0`) stops only after tc1
/// (`x == 1`) has started; tc2 (`x == 2`) starts when a slot frees and runs
/// anyway. The run then ends with the stop reason instead of another turn.
#[tokio::test]
async fn concurrent_post_result_stop_runs_the_whole_batch() {
    let called = Arc::new(Mutex::new(Vec::new()));
    let sibling_started = Arc::new(tokio::sync::Notify::new());
    let mut stream = AgentBuilder::new(three_tools_first_terminates_streaming_model())
        .tool(RecordingArgsTool {
            called: called.clone(),
            sibling_started: sibling_started.clone(),
        })
        .build()
        .runner("go")
        .max_turns(3)
        .tool_concurrency(2)
        .add_hook(TerminateOnArgZeroAfterSiblingHook { sibling_started })
        .stream()
        .await;

    let (saw_error, saw_final) =
        tokio::time::timeout(std::time::Duration::from_secs(5), async move {
            let mut saw_error = false;
            let mut saw_final = false;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(MultiTurnStreamItem::FinalResponse(_)) => saw_final = true,
                    Ok(_) => {}
                    Err(_) => saw_error = true,
                }
            }
            (saw_error, saw_final)
        })
        .await
        .expect("the concurrent tool drive must not hang");

    assert!(saw_error, "the stopped run must surface an error");
    assert!(!saw_final, "a stopped run must not yield a final response");
    let called = called.lock().expect("called").clone();
    assert!(
        called.contains(&1),
        "the in-flight sibling (x==1) completes; called args: {called:?}"
    );
    assert!(
        called.contains(&2),
        "the beyond-window sibling (x==2) starts once a slot frees and runs anyway; \
             called args: {called:?}"
    );
    assert!(
        called.contains(&0),
        "the stopper's own body runs — a post-result stop never un-executes anything; \
             called args: {called:?}"
    );
}

/// A tool that, for the `x == 1` call, records it ran and signals a gate; the
/// terminating sibling waits on that gate so the `x == 1` call completes
/// *before* the batch terminates.
#[derive(Clone)]
struct SignalOnRunTool {
    a_ran: Arc<AtomicU32>,
    a_done: Arc<tokio::sync::Notify>,
}
impl Tool for SignalOnRunTool {
    const NAME: &'static str = "add";
    type Error = MockToolError;
    type Args = serde_json::Value;
    type Output = i32;
    fn description(&self) -> String {
        MockAddTool.description()
    }

    fn parameters(&self) -> serde_json::Value {
        MockAddTool.parameters()
    }
    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        if args.get("x").and_then(serde_json::Value::as_i64) == Some(1) {
            self.a_ran.fetch_add(1, SeqCst);
            self.a_done.notify_one();
        }
        Ok(0)
    }
}

/// The `x == 2` tool's result hook stops, but only after the `x == 1`
/// sibling has finished (via the gate), so a *completed* sibling's result
/// sits collected next to the stop decision.
struct TerminateAfterSiblingDoneHook {
    a_done: Arc<tokio::sync::Notify>,
}
impl AgentHook for TerminateAfterSiblingDoneHook {
    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        if let ToolResultEvent { args, .. } = event
            && serde_json::from_str::<serde_json::Value>(args)
                .ok()
                .and_then(|v| v.get("x").and_then(serde_json::Value::as_i64))
                == Some(2)
        {
            self.a_done.notified().await;
            return ToolResultAction::stop("stop");
        }
        ToolResultAction::keep()
    }
}

/// Atomic concurrent batch with a post-result stop: the batch settles fully,
/// so every chain surfaces its `ToolExecutionCommitted` and `ToolResult`
/// items — a stop decision suppresses nothing, it only ends the run at the
/// next decision. The `x == 1` tool runs to completion and signals; the
/// `x == 2` tool's result hook then stops.
#[tokio::test]
async fn concurrent_post_result_stop_surfaces_every_execution_item() {
    let a_ran = Arc::new(AtomicU32::new(0));
    let a_done = Arc::new(tokio::sync::Notify::new());
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tc1", "add", json!({"x": 1, "y": 1})),
            MockStreamEvent::tool_call("tc2", "add", json!({"x": 2, "y": 2})),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
        vec![
            MockStreamEvent::text("unreachable"),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
    ]);
    let mut stream = AgentBuilder::new(model)
        .tool(SignalOnRunTool {
            a_ran: a_ran.clone(),
            a_done: a_done.clone(),
        })
        .build()
        .runner("go")
        .max_turns(3)
        .tool_concurrency(2)
        .add_hook(TerminateAfterSiblingDoneHook {
            a_done: a_done.clone(),
        })
        .stream()
        .await;

    let (exec_commits, results, saw_error, saw_final) =
        tokio::time::timeout(std::time::Duration::from_secs(5), async move {
            let (mut exec_commits, mut results, mut saw_error, mut saw_final) =
                (0, 0, false, false);
            while let Some(item) = stream.next().await {
                match item {
                    Ok(MultiTurnStreamItem::ToolExecutionCommitted { .. }) => exec_commits += 1,
                    Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                        ..
                    })) => results += 1,
                    Ok(MultiTurnStreamItem::FinalResponse(_)) => saw_final = true,
                    Ok(_) => {}
                    Err(_) => saw_error = true,
                }
            }
            (exec_commits, results, saw_error, saw_final)
        })
        .await
        .expect("the concurrent tool drive must not hang");

    assert!(saw_error, "the stopped run must surface an error");
    assert!(!saw_final, "a stopped run must not yield a final response");
    assert_eq!(
        exec_commits, 2,
        "a settled batch surfaces every ToolExecutionCommitted event"
    );
    assert_eq!(results, 2, "a settled batch surfaces every ToolResult");
    assert_eq!(a_ran.load(SeqCst), 1, "the fast sibling ran exactly once");
}

/// The model tool-call event carries the model's **original** arguments; the
/// execution-commit event carries the **effective** (hook-rewritten) arguments
/// — so a `ToolCallAction::Rewrite` (e.g. a redaction) is reflected in what
/// actually ran, not leaked as the original.
#[tokio::test]
async fn stream_tool_execution_committed_carries_effective_rewritten_args() {
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tc1", "add", json!({"x": 2, "y": 3})),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
    ]);
    let mut stream = AgentBuilder::new(model)
        .tool(MockAddTool)
        .add_hook(RewriteToolArgsHook(json!({"x": 2, "y": 40})))
        .build()
        .runner("go")
        .max_turns(3)
        .stream()
        .await;

    let mut model_args = None;
    let mut exec_args = None;
    while let Some(item) = stream.next().await {
        match item.unwrap_or_else(|err| panic!("stream item errored: {err}")) {
            MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                tool_call,
                ..
            }) => model_args = Some(tool_call.function.arguments),
            MultiTurnStreamItem::ToolExecutionCommitted { tool_call, .. } => {
                exec_args = Some(tool_call.function.arguments)
            }
            _ => {}
        }
    }
    assert_eq!(
        model_args,
        Some(json!({"x": 2, "y": 3})),
        "the model tool-call event carries the model's original arguments"
    );
    assert_eq!(
        exec_args,
        Some(json!({"x": 2, "y": 40})),
        "the execution-commit event carries the hook-rewritten (effective) arguments"
    );
}

/// A `ToolCall` hook `ToolCallAction::Skip` surfaces the skip result as a `ToolResult`
/// (the model sees it, and it is committed to history) but produces **no**
/// `ToolExecutionCommitted` — nothing actually ran.
#[tokio::test]
async fn stream_hook_skip_surfaces_result_without_execution_commit() {
    struct SkipHook;
    impl AgentHook for SkipHook {
        async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
            if let ToolCall { .. } = event {
                ToolCallAction::skip("blocked by policy")
            } else {
                ToolCallAction::run()
            }
        }
    }

    let calls = Arc::new(AtomicU32::new(0));
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("tc1", "add", json!({"x": 1, "y": 2})),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(0),
        ],
    ]);
    let cell = parity_cell("go");
    let stream = AgentBuilder::new(model)
        .tool(CountingAddTool {
            calls: calls.clone(),
        })
        .add_hook(SkipHook)
        .build()
        .runner_over(cell.clone())
        .max_turns(3)
        .stream()
        .await;

    let mut exec_commits = 0;
    let mut results = 0;
    let mut final_response = None;
    let mut stream = stream;
    while let Some(item) = stream.next().await {
        match item.unwrap_or_else(|err| panic!("stream item errored: {err}")) {
            MultiTurnStreamItem::ToolExecutionCommitted { .. } => exec_commits += 1,
            MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult { .. }) => {
                results += 1
            }
            MultiTurnStreamItem::FinalResponse(resp) => final_response = Some(resp),
            _ => {}
        }
    }

    assert_eq!(calls.load(SeqCst), 0, "a skipped tool's body never runs");
    assert_eq!(
        exec_commits, 0,
        "a hook-skipped tool produces no execution-commit"
    );
    assert_eq!(
        results, 1,
        "the skip result is still surfaced to the consumer"
    );
    final_response.expect("stream should yield a final response");
    // The skip result is committed to history (the model sees the reason).
    let history = parity_conversation(&cell);
    assert!(
        history.iter().any(|m| serde_json::to_string(m)
            .map(|s| s.contains("blocked by policy"))
            .unwrap_or(false)),
        "the skip result is committed to history"
    );
}

/// Concurrent tool execution is bounded on *both* sides: real parallelism
/// occurs (lower bound) and the configured `tool_concurrency` cap is never
/// exceeded (upper bound). Four parallel calls run under a cap of two; the
/// barrier is sized to the cap, so it only releases when `cap` calls are in
/// flight together — a serial runtime would deadlock, while an over-eager one
/// (ignoring the cap) would let `max_active` exceed it.
#[tokio::test]
async fn concurrent_tool_execution_stays_within_the_configured_bound() {
    #[derive(Clone)]
    struct ConcurrencyProbe {
        barrier: Arc<Barrier>,
        active: Arc<AtomicU32>,
        max_active: Arc<AtomicU32>,
    }

    impl Tool for ConcurrencyProbe {
        const NAME: &'static str = "add";
        type Error = MockToolError;
        type Args = serde_json::Value;
        type Output = String;

        fn description(&self) -> String {
            "concurrency probe".to_string()
        }

        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }

        async fn call(
            &self,
            _context: &mut ToolContext,
            _args: Self::Args,
        ) -> Result<Self::Output, Self::Error> {
            let now = self.active.fetch_add(1, SeqCst) + 1;
            self.max_active.fetch_max(now, SeqCst);
            self.barrier.wait().await;
            self.active.fetch_sub(1, SeqCst);
            Ok("ok".to_string())
        }
    }

    let cap = 2usize;
    let probe = ConcurrencyProbe {
        barrier: Arc::new(Barrier::new(cap)),
        active: Arc::new(AtomicU32::new(0)),
        max_active: Arc::new(AtomicU32::new(0)),
    };
    let max_active = probe.max_active.clone();

    // One turn issues four parallel calls to the probe (registered as `add`).
    let model = stream_model([
        MockTurn::from_contents([
            tool_call_content("c1", json!({})),
            tool_call_content("c2", json!({})),
            tool_call_content("c3", json!({})),
            tool_call_content("c4", json!({})),
        ])
        .expect("four tool calls is a valid turn"),
        MockTurn::text("done"),
    ]);

    let _ = AgentBuilder::new(model)
        .tool(probe)
        .build()
        .runner("probe concurrency")
        .max_turns(3)
        .tool_concurrency(cap)
        .run()
        .await
        .expect("run should succeed");

    let observed = max_active.load(SeqCst);
    assert!(
        observed > 1,
        "tools actually ran concurrently (lower bound): max_active={observed}"
    );
    assert!(
        observed <= cap as u32,
        "in-flight never exceeded the configured bound {cap} (upper bound): max_active={observed}"
    );
}

/// `tool_concurrency(0)` is clamped to 1 and runs to completion. The timeout
/// guards against a regression that lets `concurrency == 0` reach a
/// `buffer_unordered(0)` (which never makes progress) instead of the
/// sequential `concurrency <= 1` path.
#[tokio::test]
async fn tool_concurrency_zero_is_clamped_and_does_not_hang() {
    let model = stream_model([
        MockTurn::tool_call("tc1", "add", json!({"x": 1, "y": 2})),
        MockTurn::text("done"),
    ]);
    let run = AgentBuilder::new(model)
        .tool(MockAddTool)
        .build()
        .runner("add")
        .max_turns(3)
        .tool_concurrency(0)
        .run();

    let response = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("tool_concurrency(0) must clamp to 1, not hang on buffer_unordered(0)")
        .expect("run should succeed");
    assert_eq!(response.output, "done");
}

/// A tool that counts how many times it executes.
#[derive(Clone)]
struct CountingAddTool {
    calls: Arc<AtomicU32>,
}
impl Tool for CountingAddTool {
    const NAME: &'static str = "add";
    type Error = MockToolError;
    type Args = MockOperationArgs;
    type Output = i32;
    fn description(&self) -> String {
        MockAddTool.description()
    }
    fn parameters(&self) -> serde_json::Value {
        MockAddTool.parameters()
    }
    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.calls.fetch_add(1, SeqCst);
        MockAddTool.call(_context, args).await
    }
}

// ----------------------------------------------------------------------
// Single-source-of-truth parity harness
// ----------------------------------------------------------------------
//
// `run()` and `stream()` are two implementations of one agent loop; testing
// they agree on the same input is *differential testing*, with each driver
// acting as the other's oracle. The hazard such tests have (and that bit the
// invalid-tool-repair test above) is *fixture drift*: when the blocking
// `MockTurn` list and the streaming `MockStreamEvent` list are hand-written
// separately, they can silently encode different model behavior, so a
// passing test proves nothing.
//
// The fix — the single-source-of-truth / data-driven principle, embodied by
// pydantic-ai's `TestModel` (one scripted response replayed as a stream) and
// litellm's `stream_chunk_builder` (reassemble the stream, compare to the
// whole) — is to derive *both* encodings from one canonical `ScriptedTurn`
// list. The two drivers are then provably fed identical model behavior and
// can be asserted equal on the medium-independent projection (final output,
// message history, tool-result content, shared hook-event sequence).

/// One tool call inside a scripted turn.
#[derive(Clone)]
struct ScriptedToolCall {
    id: &'static str,
    name: &'static str,
    args: serde_json::Value,
}

/// One scripted model turn, described once and rendered into both a blocking
/// `MockTurn` and a streaming `Vec<MockStreamEvent>`.
#[derive(Clone)]
enum ScriptedTurn {
    /// A final text answer.
    Text(&'static str),
    /// One or more tool calls emitted in a single turn.
    ToolCalls(Vec<ScriptedToolCall>),
}

/// How a tool call is rendered onto the wire for the streaming driver. Both
/// shapes must yield the *same* canonical turn ("chunked-input invariance",
/// the `tokio-util` `LengthDelimitedCodec` lesson): the assembled message
/// history and tool results may not depend on whether a provider sends a
/// complete tool call or streams it as deltas.
#[derive(Clone, Copy)]
enum StreamShape {
    /// One complete tool-call event per call (mirrors the blocking turn).
    Complete,
    /// Name + argument deltas followed by the complete call, additionally
    /// exercising the delta-hook path and the assembler's delta buffering.
    Chunked,
}

impl ScriptedTurn {
    fn as_blocking_turn(&self) -> MockTurn {
        match self {
            ScriptedTurn::Text(text) => MockTurn::text(*text),
            ScriptedTurn::ToolCalls(calls) => MockTurn::from_contents(calls.iter().map(|call| {
                AssistantContent::ToolCall(MessageToolCall::new(
                    call.id.to_string(),
                    ToolFunction::new(call.name.to_string(), call.args.clone()),
                ))
            }))
            .expect("a scripted tool-call turn has at least one call"),
        }
    }

    fn as_stream_events(&self, shape: StreamShape) -> Vec<MockStreamEvent> {
        let mut events = Vec::new();
        match self {
            ScriptedTurn::Text(text) => events.push(MockStreamEvent::text(*text)),
            ScriptedTurn::ToolCalls(calls) => {
                for call in calls {
                    if let StreamShape::Chunked = shape {
                        // The canonical args still come from the complete
                        // event below, so this exercises the delta path
                        // without changing the turn.
                        let args = serde_json::to_string(&call.args)
                            .expect("scripted args serialize to json");
                        events.push(MockStreamEvent::tool_call_name_delta(call.id, call.name));
                        events.push(MockStreamEvent::tool_call_arguments_delta(call.id, &args));
                    }
                    events.push(MockStreamEvent::tool_call(
                        call.id,
                        call.name,
                        call.args.clone(),
                    ));
                }
            }
        }
        events.push(MockStreamEvent::final_response_with_total_tokens(0));
        events
    }
}

fn add_call(id: &'static str, x: i64, y: i64) -> ScriptedToolCall {
    ScriptedToolCall {
        id,
        name: "add",
        args: json!({ "x": x, "y": y }),
    }
}

/// A prompt/runner-level `add_hook` APPENDS to the agent's default hooks
/// rather than replacing them (the `with_hook` → `add_hook` semantic change):
/// a hook registered on the builder and a hook registered on the runner both
/// observe the same run.
#[tokio::test]
async fn runner_add_hook_appends_to_agent_default_hooks() {
    let agent_hook = RecordingHook::default();
    let runner_hook = RecordingHook::default();

    // `agent_hook` is registered on the builder; `runner_hook` is registered
    // on the runner obtained from that agent. `AgentRunner::from_agent` clones
    // the agent's hook stack and `add_hook` pushes on top, so both must fire.
    AgentBuilder::new(blocking_model())
        .tool(MockAddTool)
        .add_hook(agent_hook.clone())
        .build()
        .runner("add 2 and 3")
        .max_turns(3)
        .add_hook(runner_hook.clone())
        .run()
        .await
        .expect("run should succeed");

    assert!(
        agent_hook.count("tool_call") >= 1,
        "the agent-default hook must still observe the run after a runner-level add_hook"
    );
    assert!(
        runner_hook.count("tool_call") >= 1,
        "the runner-level hook must also observe the run"
    );
    // Both saw the same number of tool calls — the runner-level hook
    // appended to the agent stack; it did not replace it.
    assert_eq!(
        agent_hook.count("tool_call"),
        runner_hook.count("tool_call"),
        "add_hook appends (both hooks observe every turn); it does not replace"
    );
}

/// A hook that rewrites a valid tool call's arguments (`ToolCallAction::Rewrite` on
/// `ToolCall`) so the tool executes with the replacement instead of what the
/// model emitted.
struct RewriteToolArgsHook(serde_json::Value);

impl AgentHook for RewriteToolArgsHook {
    async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        if let ToolCall { .. } = event {
            ToolCallAction::rewrite(self.0.clone())
        } else {
            ToolCallAction::run()
        }
    }
}

struct EchoStringArgs;

impl Tool for EchoStringArgs {
    const NAME: &'static str = "echo_string_args";
    type Error = rig::tool::ToolExecutionError;
    type Args = String;
    type Output = String;

    fn description(&self) -> String {
        "Echo a JSON string argument".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "string"})
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        Ok(args)
    }
}

#[derive(serde::Deserialize)]
struct FirstGenerationArgs {
    old: String,
}

struct FirstGenerationTool(Arc<AtomicU32>);

impl Tool for FirstGenerationTool {
    const NAME: &'static str = "generation_pinned";
    type Error = rig::tool::ToolExecutionError;
    type Args = FirstGenerationArgs;
    type Output = String;

    fn description(&self) -> String {
        "first generation schema".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {"old": {"type": "string"}},
            "required": ["old"]
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        self.0.fetch_add(1, SeqCst);
        Ok(format!("first:{}", args.old))
    }
}

#[derive(serde::Deserialize)]
struct SecondGenerationArgs {
    new: String,
}

struct SecondGenerationTool(Arc<AtomicU32>);

impl Tool for SecondGenerationTool {
    const NAME: &'static str = FirstGenerationTool::NAME;
    type Error = rig::tool::ToolExecutionError;
    type Args = SecondGenerationArgs;
    type Output = String;

    fn description(&self) -> String {
        "second generation schema".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {"new": {"type": "string"}},
            "required": ["new"]
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        self.0.fetch_add(1, SeqCst);
        Ok(format!("second:{}", args.new))
    }
}

/// Pauses the first provider call after its request has been built. Tests
/// replace the live registry while that request is in flight, then let the
/// model return a call that is valid only for the advertised generation.
#[derive(Clone)]
struct PausingCompletionModel {
    inner: MockCompletionModel,
    request_started: Arc<Notify>,
    release_response: Arc<Notify>,
    requests: Arc<AtomicU32>,
}

impl PausingCompletionModel {
    fn new(inner: MockCompletionModel) -> (Self, Arc<Notify>, Arc<Notify>) {
        let request_started = Arc::new(Notify::new());
        let release_response = Arc::new(Notify::new());
        (
            Self {
                inner,
                request_started: request_started.clone(),
                release_response: release_response.clone(),
                requests: Arc::new(AtomicU32::new(0)),
            },
            request_started,
            release_response,
        )
    }

    async fn inspect_and_pause(&self, request: &crate::completion::CompletionRequest) {
        let request_index = self.requests.fetch_add(1, SeqCst);
        let definition = request
            .tools
            .iter()
            .find(|definition| definition.name == FirstGenerationTool::NAME)
            .expect("generation tool must be advertised");
        if request_index == 0 {
            assert_eq!(definition.description, "first generation schema");
            self.request_started.notify_one();
            self.release_response.notified().await;
        } else {
            assert_eq!(definition.description, "second generation schema");
        }
    }
}

impl CompletionModel for PausingCompletionModel {
    async fn completion(
        &self,
        request: crate::completion::CompletionRequest,
    ) -> Result<crate::completion::CompletionResponse, crate::completion::CompletionError> {
        self.inspect_and_pause(&request).await;
        self.inner.completion(request).await
    }

    async fn stream(
        &self,
        request: crate::completion::CompletionRequest,
    ) -> Result<crate::streaming::StreamingCompletionResponse, crate::completion::CompletionError>
    {
        self.inspect_and_pause(&request).await;
        self.inner.stream(request).await
    }
}

/// `ToolCallAction::Rewrite` resolves to a `ProceedWith` tool-call decision that
/// carries the replacement arguments, and is named for fail-closed
/// diagnostics.
#[test]
fn rewrite_args_resolves_to_proceed_with_for_tool_call() {
    let args = json!({"x": 1, "y": 2});
    match super::tool_call_decision(ToolCallAction::rewrite(args.clone())) {
        super::ToolCallDecision::ProceedWith(replacement) => assert_eq!(replacement, args),
        _ => panic!("ToolCallAction::Rewrite should resolve to ProceedWith"),
    }
    // The typed convenience builds the same variant as the value constructor.
    assert_eq!(
        ToolCallAction::try_rewrite(&json!({"x": 1, "y": 2})).expect("serializes"),
        ToolCallAction::rewrite(json!({"x": 1, "y": 2})),
    );
}

#[tokio::test]
async fn streaming_turn_dispatches_the_registry_generation_it_advertised() {
    let first_calls = Arc::new(AtomicU32::new(0));
    let second_calls = Arc::new(AtomicU32::new(0));
    let handle: ToolServerHandle = ToolServer::new()
        .tool(FirstGenerationTool(first_calls.clone()))
        .run();
    let turns = [
        ScriptedTurn::ToolCalls(vec![ScriptedToolCall {
            id: "tc-generation",
            name: FirstGenerationTool::NAME,
            args: json!({"old": "payload"}),
        }]),
        ScriptedTurn::Text("done"),
    ];
    let inner = MockCompletionModel::from_stream_turns(
        turns
            .iter()
            .map(|turn| turn.as_stream_events(StreamShape::Complete)),
    );
    let (model, request_started, release_response) = PausingCompletionModel::new(inner);
    let runner = AgentBuilder::new(model)
        .tool_server_handle(handle.clone())
        .build()
        .runner("use the generation tool")
        .max_turns(3);

    let drive = async {
        let mut stream = runner.stream().await;
        let mut final_output = None;
        while let Some(item) = stream.next().await {
            if let MultiTurnStreamItem::FinalResponse(response) =
                item.expect("streaming run should use its pinned tool generation")
            {
                final_output = Some(response.output().to_string());
            }
        }
        final_output
    };
    let replace = async {
        request_started.notified().await;
        handle
            .add_tool(SecondGenerationTool(second_calls.clone()))
            .await;
        release_response.notify_one();
    };
    let (final_output, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::join!(drive, replace)
    })
    .await
    .expect("in-flight streaming replacement must not hang");

    assert_eq!(final_output.as_deref(), Some("done"));
    assert_eq!(first_calls.load(SeqCst), 1);
    assert_eq!(second_calls.load(SeqCst), 0);
}

/// A hook that rewrites a tool's result (`ToolResultAction::Rewrite` on
/// `ToolResult`) so the model sees the replacement instead of the tool's
/// actual output.
struct RewriteToolResultHook(&'static str);

impl AgentHook for RewriteToolResultHook {
    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        if let ToolResultEvent { .. } = event {
            ToolResultAction::rewrite(self.0)
        } else {
            ToolResultAction::keep()
        }
    }
}

/// `ToolResultAction::Rewrite` resolves to a `Replace` tool-result decision carrying
/// the replacement, and is named for fail-closed diagnostics.
#[test]
fn rewrite_result_resolves_to_replace_for_tool_result() {
    match super::tool_result_decision(ToolResultAction::rewrite("redacted")) {
        super::ToolResultDecision::Replace(result) => {
            assert_eq!(result.as_text(), Some("redacted"))
        }
        _ => panic!("ToolResultAction::Rewrite should resolve to Replace"),
    }
}

/// A `ToolResultAction::Rewrite` replacement is delivered to the model verbatim, not
/// re-parsed as structured/multimodal tool output. A JSON-shaped replacement
/// (here, an image payload that `tool_result_output` would turn into an image
/// content block for *real* tool output) reaches history as literal text —
/// so a redaction hook returning JSON cannot be silently restructured.
#[tokio::test]
async fn rewrite_result_is_delivered_verbatim_not_reparsed() {
    const IMAGE_JSON: &str = r#"{"type":"image","data":"abc","mimeType":"image/png"}"#;

    let turns = [
        ScriptedTurn::ToolCalls(vec![add_call("tc1", 2, 3)]),
        ScriptedTurn::Text("done"),
    ];
    let model = stream_model(turns.iter().map(ScriptedTurn::as_blocking_turn));
    let cell = parity_cell("add 2 and 3");
    let _result = AgentBuilder::new(model)
        .tool(MockAddTool)
        .build()
        .runner_over(cell.clone())
        .max_turns(3)
        .add_hook(RewriteToolResultHook(IMAGE_JSON))
        .run()
        .await
        .expect("run should succeed with a JSON-shaped rewritten result");

    let messages = parity_conversation(&cell);
    assert!(
        tool_result_text_in_history(&messages, IMAGE_JSON),
        "the JSON-shaped replacement must reach history verbatim as text, not be \
             re-parsed into a structured/image content block"
    );
}

/// `ToolCallAction::Rewrite` and `ToolResultAction::Rewrite` chain across hooks: a later hook observes
/// (and further rewrites) the value produced by earlier hooks.
#[tokio::test]
async fn chained_rewrites_compose_across_hooks() {
    /// Sets one key of the tool arguments, preserving the rest.
    struct SetArg {
        key: &'static str,
        value: i64,
    }
    impl AgentHook for SetArg {
        async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
            if let ToolCall { args, .. } = event {
                let mut parsed: serde_json::Value =
                    serde_json::from_str(args).unwrap_or_else(|_| json!({}));
                parsed[self.key] = json!(self.value);
                ToolCallAction::rewrite(parsed)
            } else {
                ToolCallAction::run()
            }
        }
    }

    /// Wraps the tool result in `label(...)`.
    struct WrapResult(&'static str);
    impl AgentHook for WrapResult {
        async fn on_tool_result(
            &self,
            _ctx: &HookContext,
            event: ToolResultEvent<'_>,
        ) -> ToolResultAction {
            if let ToolResultEvent { presentation, .. } = event {
                ToolResultAction::rewrite(format!("{}({})", self.0, presentation.render()))
            } else {
                ToolResultAction::keep()
            }
        }
    }

    // The model asks add(2, 3). SetArg{y:40} then SetArg{x:100} chain, so the
    // tool runs with (100, 40) = 140 — proving arg rewrites compose. Then
    // WrapResult "A" and "B" chain, and a trailing recorder observes the fully
    // chained result "B(A(140))".
    let recorder = RecordingHook::default();
    let blocking = AgentBuilder::new(blocking_model())
        .tool(MockAddTool)
        .add_hook(SetArg {
            key: "y",
            value: 40,
        })
        .add_hook(SetArg {
            key: "x",
            value: 100,
        })
        .add_hook(WrapResult("A"))
        .add_hook(WrapResult("B"))
        .add_hook(recorder.clone())
        .build()
        .runner("add 2 and 3")
        .max_turns(3)
        .run()
        .await
        .expect("blocking run should succeed");
    assert_eq!(blocking.output, "the answer is 5");
    assert_eq!(
        recorder.tool_results(),
        vec!["B(A(140))".to_string()],
        "arg rewrites compose (100+40=140) and result rewrites nest B(A(...))"
    );

    // Same on the streaming surface.
    let stream_recorder = RecordingHook::default();
    let mut stream = AgentBuilder::new(streaming_model())
        .tool(MockAddTool)
        .add_hook(SetArg {
            key: "y",
            value: 40,
        })
        .add_hook(SetArg {
            key: "x",
            value: 100,
        })
        .add_hook(WrapResult("A"))
        .add_hook(WrapResult("B"))
        .add_hook(stream_recorder.clone())
        .build()
        .runner("add 2 and 3")
        .max_turns(3)
        .stream()
        .await;
    while let Some(item) = stream.next().await {
        let _ = item.map_err(|err| panic!("stream item errored: {err}"));
    }
    assert_eq!(
        stream_recorder.tool_results(),
        vec!["B(A(140))".to_string()],
        "chained rewrites compose identically on the streaming surface"
    );
}

// -----------------------------------------------------------------------
// Human-in-the-loop (HITL): one hook gates each tool call behind a human
// decision, mapping approve/deny/edit/abort onto the event-specific actions
// (cont / skip / rewrite_args / terminate). The runnable interactive
// version lives in `examples/agent_with_human_in_the_loop`.
// -----------------------------------------------------------------------

/// A human reviewer's decision for a pending tool call.
enum Decision {
    /// Run the tool as the model requested.
    Approve,
    /// Don't run the tool; feed `reason` back to the model as the result.
    Deny(&'static str),
    /// Run the tool with these arguments instead of the model's.
    Edit(serde_json::Value),
    /// Abort the whole run with this reason.
    Abort(&'static str),
}

/// Simulates a human reviewer by popping a scripted decision per `ToolCall`
/// and mapping it to the matching event-specific action. A real reviewer would `.await`
/// interactive input here (the hook is async) rather than read a queue.
#[derive(Clone)]
struct HumanApprovalHook {
    decisions: Arc<Mutex<std::collections::VecDeque<Decision>>>,
    reviewed: Arc<Mutex<Vec<String>>>,
    /// An `Abort` decision stashed by the `ToolCall` hook; the run ends with
    /// this reason after the current batch settles (the action surface has
    /// no kill-a-batch escape — ENGINE.md, stop taxonomy; a true stop-now
    /// would hold the abort leaf instead).
    stop_after: Arc<Mutex<Option<&'static str>>>,
}

impl HumanApprovalHook {
    fn new(decisions: impl IntoIterator<Item = Decision>) -> Self {
        Self {
            decisions: Arc::new(Mutex::new(decisions.into_iter().collect())),
            reviewed: Arc::new(Mutex::new(Vec::new())),
            stop_after: Arc::new(Mutex::new(None)),
        }
    }

    /// `"name(args)"` for each call presented for review, in order.
    fn reviewed(&self) -> Vec<String> {
        self.reviewed.lock().unwrap().clone()
    }
}

impl AgentHook for HumanApprovalHook {
    async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        let ToolCall {
            tool_name, args, ..
        } = event
        else {
            return ToolCallAction::run();
        };
        self.reviewed
            .lock()
            .unwrap()
            .push(format!("{tool_name}({args})"));
        let decision = self.decisions.lock().unwrap().pop_front();
        match decision {
            Some(Decision::Approve) => ToolCallAction::run(),
            Some(Decision::Deny(reason)) => ToolCallAction::skip(reason),
            Some(Decision::Edit(args)) => ToolCallAction::rewrite(args),
            // The call runs; the stashed reason ends the run after its batch.
            Some(Decision::Abort(reason)) => {
                *self.stop_after.lock().unwrap() = Some(reason);
                ToolCallAction::run()
            }
            // Fail closed if the script is exhausted (it shouldn't be) — deny
            // rather than silently approve, matching the example's contract.
            None => ToolCallAction::skip("denied: no scripted decision (fail-closed)"),
        }
    }

    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        _event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        match self.stop_after.lock().unwrap().take() {
            Some(reason) => ToolResultAction::stop(reason),
            None => ToolResultAction::keep(),
        }
    }
}

/// A HITL hook that aborts a tool call (`Decision::Abort` — the call runs,
/// then the run ends with the reason at the batch's settle) surfaces the
/// reason as a `PromptCancelled` error — on both the blocking and streaming
/// drivers.
#[tokio::test]
async fn human_in_the_loop_abort_terminates_the_run() {
    let turns = [
        ScriptedTurn::ToolCalls(vec![add_call("tc1", 2, 3)]),
        ScriptedTurn::Text("unreachable"),
    ];
    const ABORT_REASON: &str = "aborted by the human reviewer";

    // Blocking driver: the run resolves to a PromptCancelled error.
    let blocking_model = stream_model(turns.iter().map(ScriptedTurn::as_blocking_turn));
    let err = AgentBuilder::new(blocking_model)
        .tool(MockAddTool)
        .build()
        .runner("do the sensitive thing")
        .max_turns(3)
        .add_hook(HumanApprovalHook::new([Decision::Abort(ABORT_REASON)]))
        .run()
        .await
        .expect_err("an aborted tool call should terminate the blocking run");
    assert!(
        format!("{err}").contains(ABORT_REASON),
        "the abort reason should surface in the blocking error, got: {err}"
    );

    // Streaming driver: the stream yields an error carrying the same reason and
    // never reaches the "unreachable" final text.
    let streaming_model = MockCompletionModel::from_stream_turns(
        turns
            .iter()
            .map(|turn| turn.as_stream_events(StreamShape::Complete)),
    );
    let mut stream = AgentBuilder::new(streaming_model)
        .tool(MockAddTool)
        .build()
        .runner("do the sensitive thing")
        .max_turns(3)
        .add_hook(HumanApprovalHook::new([Decision::Abort(ABORT_REASON)]))
        .stream()
        .await;
    let mut stream_error = None;
    while let Some(item) = stream.next().await {
        match item {
            Err(err) => stream_error = Some(format!("{err}")),
            Ok(MultiTurnStreamItem::FinalResponse(resp)) => {
                panic!("aborted stream must not finalize, got: {}", resp.output())
            }
            Ok(_) => {}
        }
    }
    let stream_error = stream_error.expect("an aborted tool call should error the stream");
    assert!(
        stream_error.contains(ABORT_REASON),
        "the abort reason should surface in the streaming error, got: {stream_error}"
    );
}

/// A non-interactive *policy* HITL hook: auto-approve an allow-list, deny
/// everything else (fail-closed), and cache each decision so a repeated tool
/// is not re-evaluated ("sticky", like the OpenAI Agents SDK's
/// `always_approve`). Backs `examples/agent_with_approval_policy`.
#[derive(Clone)]
struct PolicyHook {
    auto_approve: std::collections::HashSet<&'static str>,
    /// Tool names the policy actually evaluated (cache misses), in order.
    evaluated: Arc<Mutex<Vec<String>>>,
    /// Sticky cache of prior decisions, keyed by tool name.
    cache: Arc<Mutex<std::collections::HashMap<String, bool>>>,
}

impl PolicyHook {
    fn new(auto_approve: impl IntoIterator<Item = &'static str>) -> Self {
        Self {
            auto_approve: auto_approve.into_iter().collect(),
            evaluated: Arc::new(Mutex::new(Vec::new())),
            cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    fn evaluated(&self) -> Vec<String> {
        self.evaluated.lock().unwrap().clone()
    }
}

impl AgentHook for PolicyHook {
    async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        let ToolCall { tool_name, .. } = event else {
            return ToolCallAction::run();
        };
        let cached = self.cache.lock().unwrap().get(tool_name).copied();
        let approved = match cached {
            Some(decision) => decision, // sticky: reuse without re-evaluating
            None => {
                self.evaluated.lock().unwrap().push(tool_name.to_string());
                let decision = self.auto_approve.contains(tool_name);
                self.cache
                    .lock()
                    .unwrap()
                    .insert(tool_name.to_string(), decision);
                decision
            }
        };
        if approved {
            ToolCallAction::run()
        } else {
            ToolCallAction::skip(format!("denied by policy: `{tool_name}` not allowed"))
        }
    }
}

/// The policy hook auto-approves `add` and denies `subtract`, and its decision
/// is sticky: a second `add` call reuses the cached approval instead of being
/// re-evaluated. The denied call never runs and its reason reaches the model.
#[tokio::test]
async fn approval_policy_allow_list_with_sticky_decisions() {
    // One turn issues three calls: add, subtract (denied), add again (sticky).
    let turns = [
        ScriptedTurn::ToolCalls(vec![
            add_call("c1", 2, 3),
            ScriptedToolCall {
                id: "c2",
                name: "subtract",
                args: json!({ "x": 10, "y": 4 }),
            },
            add_call("c3", 2, 3),
        ]),
        ScriptedTurn::Text("done"),
    ];

    let model = stream_model(turns.iter().map(ScriptedTurn::as_blocking_turn));
    let recorder = RecordingHook::default();
    let policy = PolicyHook::new(["add"]);
    let policy_cell = parity_cell("go");
    let out = AgentBuilder::new(model)
        .tool(MockAddTool)
        .tool(MockSubtractTool)
        .build()
        .runner_over(policy_cell.clone())
        .max_turns(3)
        .add_hook(recorder.clone())
        .add_hook(policy.clone())
        .run()
        .await
        .expect("policy run should succeed");

    assert_eq!(out.output, "done");
    // `add` ran twice (auto-approved, then sticky-reused); `subtract` was denied
    // and executed nothing, but its denial reason now surfaces as a ToolResult
    // (structured `Skipped` outcome) between the two `add` results.
    assert_eq!(
        recorder.tool_results(),
        vec![
            "5".to_string(),
            "denied by policy: `subtract` not allowed".to_string(),
            "5".to_string()
        ]
    );
    // The policy evaluated each distinct tool once; the second `add` reused the
    // cached decision rather than being re-evaluated.
    assert_eq!(
        policy.evaluated(),
        vec!["add".to_string(), "subtract".to_string()]
    );
    let messages = parity_conversation(&policy_cell);
    assert!(
        tool_result_text_in_history(&messages, "denied by policy: `subtract` not allowed"),
        "the policy denial reason must reach the model as the subtract tool result"
    );
}
