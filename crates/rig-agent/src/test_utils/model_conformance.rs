//! Provider-neutral behavioral scenarios for completion-model conformance.
//!
//! These helpers test the model/agent contract only. Provider wire formats,
//! authentication, HTTP streaming, and cassette matching remain provider-suite
//! responsibilities.

use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use futures::StreamExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    agent::{
        AgentBuilder, AgentHook, HookContext, MultiTurnStreamItem, NoToolConfig, StreamingError,
        ToolCall as ToolCallEvent, ToolCallAction, ToolResultAction, ToolResultEvent,
    },
    completion::{
        AssistantContent, CompletionError, CompletionModel, Message, Prompt, PromptError,
        ToolDefinition,
    },
    streaming::StreamingPrompt,
    tool::{Tool, ToolContext},
};
use rig_core::message::{ToolChoice, UserContent};

/// Typed failure from a portable model-conformance scenario.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScenarioError {
    /// A buffered agent run failed.
    #[error(transparent)]
    Prompt(#[from] PromptError),
    /// A direct model completion failed.
    #[error(transparent)]
    Completion(#[from] CompletionError),
    /// A streaming agent run failed.
    #[error(transparent)]
    Streaming(#[from] StreamingError),
    /// Structured content could not be decoded.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The model or agent violated the portable behavioral contract.
    #[error("{scenario} conformance failed: {details}")]
    Contract {
        /// Stable scenario name.
        scenario: &'static str,
        /// Actionable observation explaining the failure.
        details: String,
    },
}

impl ScenarioError {
    fn contract(scenario: &'static str, details: impl Into<String>) -> Self {
        Self::Contract {
            scenario,
            details: details.into(),
        }
    }
}

/// Validate cancellation diagnostics, including the exact reason and retained
/// assistant tool-call history.
pub fn validate_cancelled_failure(
    error: &PromptError,
    expected_reason: &str,
    expected_tool: &str,
) -> Result<(), ScenarioError> {
    const SCENARIO: &str = "cancelled_failure";
    let PromptError::PromptCancelled {
        chat_history,
        reason,
    } = error
    else {
        return Err(ScenarioError::contract(
            SCENARIO,
            format!("expected PromptCancelled, observed {error:?}"),
        ));
    };
    let history_has_call = chat_history.iter().any(|message| {
        matches!(
            message,
            Message::Assistant { content, .. }
                if content.iter().any(|item| matches!(
                    item,
                    AssistantContent::ToolCall(call) if call.function.name == expected_tool
                ))
        )
    });
    if reason != expected_reason || !history_has_call {
        return Err(ScenarioError::contract(
            SCENARIO,
            format!(
                "reason={reason:?}, expected={expected_reason:?}, history_has_call={history_has_call}, history={chat_history:?}"
            ),
        ));
    }
    Ok(())
}

/// Validate max-turn diagnostics, including the exact configured budget and a
/// retained pending prompt.
pub fn validate_max_turns_failure(
    error: &PromptError,
    expected_max_turns: usize,
) -> Result<(), ScenarioError> {
    const SCENARIO: &str = "max_turns_failure";
    let PromptError::MaxTurnsError {
        max_turns,
        chat_history,
        prompt,
    } = error
    else {
        return Err(ScenarioError::contract(
            SCENARIO,
            format!("expected MaxTurnsError, observed {error:?}"),
        ));
    };
    let pending_prompt_retained = matches!(
        prompt.as_ref(),
        Message::User { content } if content.iter().next().is_some()
    );
    if *max_turns != expected_max_turns || chat_history.is_empty() || !pending_prompt_retained {
        return Err(ScenarioError::contract(
            SCENARIO,
            format!(
                "max_turns={max_turns}, expected={expected_max_turns}, history={chat_history:?}, pending_prompt_retained={pending_prompt_retained}, pending_prompt={prompt:?}"
            ),
        ));
    }
    Ok(())
}

/// Decode a structured-output response with a typed conformance failure that
/// retains the scenario name and raw response.
pub fn decode_structured_output<T>(
    scenario: &'static str,
    response: &str,
) -> Result<T, ScenarioError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_str(response).map_err(|error| {
        ScenarioError::contract(
            scenario,
            format!("structured output did not decode: {error}; response={response:?}"),
        )
    })
}

/// Validate that model-family control markers are absent from user-visible
/// output and persisted history.
pub fn validate_protocol_hygiene(
    scenario: &'static str,
    visible_output: &str,
    messages: &[Message],
    forbidden_markers: &[&str],
) -> Result<(), ScenarioError> {
    let serialized = serde_json::to_string(messages)?;
    let leaked = forbidden_markers
        .iter()
        .filter(|marker| visible_output.contains(**marker) || serialized.contains(**marker))
        .copied()
        .collect::<Vec<_>>();
    if !leaked.is_empty() {
        return Err(ScenarioError::contract(
            scenario,
            format!(
                "protocol markers leaked: {leaked:?}; output={visible_output:?}, history={messages:?}"
            ),
        ));
    }
    Ok(())
}

/// Validate that every observed tool invocation contains the expected rewritten
/// argument fields while allowing unrelated original fields to remain.
pub fn validate_rewritten_arguments(
    scenario: &'static str,
    observations: &[serde_json::Value],
    expected_fields: &serde_json::Value,
) -> Result<(), ScenarioError> {
    let Some(expected) = expected_fields.as_object() else {
        return Err(ScenarioError::contract(
            scenario,
            "expected rewritten fields must be a JSON object",
        ));
    };
    if observations.is_empty() {
        return Err(ScenarioError::contract(
            scenario,
            "the rewritten tool was never invoked",
        ));
    }
    for observation in observations {
        let Some(actual) = observation.as_object() else {
            return Err(ScenarioError::contract(
                scenario,
                format!("observed rewritten arguments were not an object: {observation:?}"),
            ));
        };
        for (key, expected_value) in expected {
            if actual.get(key) != Some(expected_value) {
                return Err(ScenarioError::contract(
                    scenario,
                    format!(
                        "rewritten field {key:?} expected {expected_value:?}, observed {observation:?}"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Validate that a sensitive raw tool result was produced but did not reach the
/// model's user-visible response after result hooks ran.
pub fn validate_result_redaction(
    scenario: &'static str,
    tool_produced_secret: bool,
    visible_output: &str,
    secret: &str,
) -> Result<(), ScenarioError> {
    if !tool_produced_secret || visible_output.is_empty() || visible_output.contains(secret) {
        return Err(ScenarioError::contract(
            scenario,
            format!(
                "produced_secret={tool_produced_secret}, secret_visible={}, output={visible_output:?}",
                visible_output.contains(secret)
            ),
        ));
    }
    Ok(())
}

/// Validate a provider-neutral person extraction and its usage accounting.
pub fn validate_extraction_fields(
    scenario: &'static str,
    first_name: Option<&str>,
    last_name: Option<&str>,
    job: Option<&str>,
    usage: crate::completion::Usage,
) -> Result<(), ScenarioError> {
    let fields_match = first_name.is_some_and(|value| value.eq_ignore_ascii_case("Ada"))
        && last_name.is_some_and(|value| value.eq_ignore_ascii_case("Lovelace"))
        && job.is_some_and(|value| value.to_ascii_lowercase().contains("mathematician"));
    if !fields_match || !usage.has_values() {
        return Err(ScenarioError::contract(
            scenario,
            format!(
                "first_name={first_name:?}, last_name={last_name:?}, job={job:?}, usage={usage:?}"
            ),
        ));
    }
    Ok(())
}

/// Summary emitted by a portable model-conformance scenario.
#[derive(Debug, Clone)]
pub struct ScenarioReport {
    /// Stable scenario name.
    pub name: &'static str,
    /// Number of model-invoked tool calls observed by the scenario.
    pub tool_calls: usize,
    /// Aggregated prompt tokens reported by the model across all turns.
    pub prompt_tokens: u64,
    /// Aggregated generated tokens reported by the model across all turns.
    pub generated_tokens: u64,
    /// Number of messages retained in the completed run history.
    pub history_messages: usize,
    /// End-to-end scenario duration.
    pub duration: Duration,
    /// The model's final user-visible response.
    pub response: String,
}

/// Error used by the deterministic conformance tools.
#[derive(Debug, thiserror::Error)]
#[error("model-conformance tool failed")]
pub struct ConformanceToolError;

const FORCE_TOOLS_PREAMBLE: &str = "You are a calculator assistant. You MUST use the provided tools for every arithmetic operation instead of computing results yourself. Once you have all the tool results you need, reply with the final numeric answer in plain text.";
const PARALLEL_PROMPT: &str = "Compute 3 + 4 and 10 - 2. You MUST call the add tool and the subtract tool together in your first response, as two parallel function calls, then report both results.";
const PING_OUTPUT: &str = "pong-crimson-7423";
const MOTTO_OUTPUT: &str = "steady hands\ncalm waters";

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn tool_result_values(message: &Message) -> Vec<serde_json::Value> {
    let Message::User { content } = message else {
        return Vec::new();
    };
    content
        .iter()
        .filter_map(|item| match item {
            UserContent::ToolResult(result) => Some(result),
            _ => None,
        })
        .flat_map(|result| result.content.iter())
        .filter_map(|content| match content {
            rig_core::message::ToolResultContent::Text(text) => {
                Some(serde_json::Value::String(text.text.clone()))
            }
            rig_core::message::ToolResultContent::Json { value } => Some(value.clone()),
            rig_core::message::ToolResultContent::Image(_) => None,
        })
        .collect()
}

fn validate_tool_correlation(
    scenario: &'static str,
    messages: &[Message],
) -> Result<(), ScenarioError> {
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for message in messages {
        match message {
            Message::Assistant { content, .. } => {
                calls.extend(content.iter().filter_map(|item| match item {
                    AssistantContent::ToolCall(call) => {
                        Some((call.id.as_str(), call.call_id.as_deref()))
                    }
                    _ => None,
                }));
            }
            Message::User { content } => {
                results.extend(content.iter().filter_map(|item| match item {
                    UserContent::ToolResult(result) => {
                        Some((result.id.as_str(), result.call_id.as_deref()))
                    }
                    _ => None,
                }));
            }
            Message::System { .. } => {}
        }
    }
    if calls.is_empty() {
        return Err(ScenarioError::contract(
            scenario,
            format!("history has no assistant tool calls: {messages:?}"),
        ));
    }
    for (id, call_id) in &calls {
        let matches = results
            .iter()
            .filter(|(result_id, result_call_id)| result_id == id && call_id == result_call_id)
            .count();
        if matches != 1 {
            return Err(ScenarioError::contract(
                scenario,
                format!(
                    "tool call id={id:?} call_id={call_id:?} has {matches} correlated results; calls={calls:?}, results={results:?}"
                ),
            ));
        }
    }
    if results.len() != calls.len() {
        return Err(ScenarioError::contract(
            scenario,
            format!(
                "history contains dangling calls or results: calls={calls:?}, results={results:?}"
            ),
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize, JsonSchema)]
struct OperationArgs {
    x: i64,
    y: i64,
}

#[derive(Clone)]
struct CountingAdd(Arc<AtomicUsize>);

impl Tool for CountingAdd {
    const NAME: &'static str = "add";
    type Error = ConformanceToolError;
    type Args = OperationArgs;
    type Output = i64;

    fn description(&self) -> String {
        "Add x and y together".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "x": { "type": "number", "description": "The first operand" },
                "y": { "type": "number", "description": "The second operand" }
            },
            "required": ["x", "y"]
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(args.x + args.y)
    }
}

#[derive(Clone)]
struct CountingSum(Arc<AtomicUsize>);

impl Tool for CountingSum {
    const NAME: &'static str = "sum";
    type Error = ConformanceToolError;
    type Args = OperationArgs;
    type Output = i64;

    fn description(&self) -> String {
        "Add x and y together (alias of add)".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        CountingAdd(Arc::new(AtomicUsize::new(0))).parameters()
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(args.x + args.y)
    }
}

#[derive(Clone)]
struct CountingSubtract(Arc<AtomicUsize>);

impl Tool for CountingSubtract {
    const NAME: &'static str = "subtract";
    type Error = ConformanceToolError;
    type Args = OperationArgs;
    type Output = i64;

    fn description(&self) -> String {
        "Subtract y from x (i.e. x - y)".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "x": { "type": "number", "description": "The first operand" },
                "y": { "type": "number", "description": "The second operand" }
            },
            "required": ["x", "y"]
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(args.x - args.y)
    }
}

#[derive(Clone)]
struct RewriteArgument {
    key: &'static str,
    value: serde_json::Value,
}

impl AgentHook for RewriteArgument {
    async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCallEvent<'_>) -> ToolCallAction {
        if event.tool_name != CountingAdd::NAME {
            return ToolCallAction::run();
        }
        let Ok(mut arguments) = serde_json::from_str::<serde_json::Value>(event.args) else {
            return ToolCallAction::run();
        };
        let Some(object) = arguments.as_object_mut() else {
            return ToolCallAction::run();
        };
        object.insert(self.key.to_string(), self.value.clone());
        ToolCallAction::rewrite(arguments)
    }
}

#[derive(Clone, Default)]
struct ObserveArguments(Arc<Mutex<Vec<serde_json::Value>>>);

impl AgentHook for ObserveArguments {
    async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCallEvent<'_>) -> ToolCallAction {
        let value = serde_json::from_str(event.args)
            .unwrap_or_else(|_| serde_json::Value::String(event.args.to_string()));
        lock_recover(&self.0).push(value);
        ToolCallAction::run()
    }
}

#[derive(Clone)]
struct ReplaceResult(&'static str);

impl AgentHook for ReplaceResult {
    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        if event.tool_name == CountingAdd::NAME {
            ToolResultAction::rewrite(self.0)
        } else {
            ToolResultAction::keep()
        }
    }
}

#[derive(Clone)]
struct WrapResult;

impl AgentHook for WrapResult {
    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        if event.tool_name == CountingAdd::NAME {
            ToolResultAction::rewrite(format!("[{}]", event.presentation.render()))
        } else {
            ToolResultAction::keep()
        }
    }
}

#[derive(Clone)]
struct StopAfterResult(&'static str);

impl AgentHook for StopAfterResult {
    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        if event.tool_name == CountingAdd::NAME {
            ToolResultAction::stop(self.0)
        } else {
            ToolResultAction::keep()
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EmptyArgs {}

#[derive(Clone)]
struct PingTool(Arc<AtomicUsize>);

impl Tool for PingTool {
    const NAME: &'static str = "ping";
    type Error = ConformanceToolError;
    type Args = EmptyArgs;
    type Output = String;

    fn description(&self) -> String {
        "Return the current ping marker. Takes no arguments.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "required": [] })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(PING_OUTPUT.to_string())
    }
}

#[derive(Clone)]
struct MottoTool(Arc<AtomicUsize>);

impl Tool for MottoTool {
    const NAME: &'static str = "fetch_motto";
    type Error = ConformanceToolError;
    type Args = EmptyArgs;
    type Output = String;

    fn description(&self) -> String {
        "Fetch the two-line workshop motto.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "required": [] })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(MOTTO_OUTPUT.to_string())
    }
}

#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
struct ConfigOutput {
    service: String,
    max_retries: u64,
}

#[derive(Clone)]
struct ConfigTool(Arc<AtomicUsize>);

impl Tool for ConfigTool {
    const NAME: &'static str = "fetch_config";
    type Error = ConformanceToolError;
    type Args = EmptyArgs;
    type Output = ConfigOutput;

    fn description(&self) -> String {
        "Fetch the service configuration object.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "required": [] })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        _args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ConfigOutput {
            service: "cassette-lab".to_string(),
            max_retries: 3,
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ComplexMode {
    Careful,
    Fast,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
struct ComplexProfile {
    name: String,
    tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
struct ComplexArgs {
    profile: ComplexProfile,
    mode: ComplexMode,
    note: Option<String>,
    quote: String,
}

#[derive(Clone)]
struct CaptureComplexTool {
    calls: Arc<AtomicUsize>,
    captured: Arc<Mutex<Option<ComplexArgs>>>,
}

impl Tool for CaptureComplexTool {
    const NAME: &'static str = "store_profile";
    type Error = ConformanceToolError;
    type Args = ComplexArgs;
    type Output = ComplexArgs;

    fn description(&self) -> String {
        "Store one profile with its nested tags, mode, optional note, and exact quoted text."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ComplexArgs)).unwrap_or_default()
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *lock_recover(&self.captured) = Some(args.clone());
        Ok(args)
    }
}

fn has_tool_roundtrip(messages: Option<&[Message]>) -> bool {
    let saw_call = messages.is_some_and(|messages| {
        messages.iter().any(|message| {
            matches!(
                message,
                Message::Assistant { content, .. }
                    if content.iter().any(|item| matches!(item, AssistantContent::ToolCall(_)))
            )
        })
    });
    let saw_result = messages.is_some_and(|messages| {
        messages.iter().any(|message| {
            matches!(
                message,
                Message::User { content }
                    if content.iter().any(|item| matches!(item, UserContent::ToolResult(_)))
            )
        })
    });
    saw_call && saw_result
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RepeatArgs {
    /// The text to repeat.
    text: String,
    /// Number of repetitions; defaults to 2 when omitted.
    times: Option<u32>,
}

#[derive(Clone)]
struct RepeatTool {
    calls: Arc<AtomicUsize>,
}

impl Tool for RepeatTool {
    const NAME: &'static str = "repeat_text";
    type Error = ConformanceToolError;
    type Args = RepeatArgs;
    type Output = String;

    fn description(&self) -> String {
        "Repeat `text`. `times` is optional and defaults to 2.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(RepeatArgs)).unwrap_or_default()
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![args.text.as_str(); args.times.unwrap_or(2) as usize].join(" "))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct BinOpArgs {
    a: i64,
    b: i64,
}

#[derive(Clone)]
struct AddTool(Arc<AtomicUsize>);

impl Tool for AddTool {
    const NAME: &'static str = "add";
    type Error = ConformanceToolError;
    type Args = BinOpArgs;
    type Output = i64;

    fn description(&self) -> String {
        "Add two integers a and b.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(BinOpArgs)).unwrap_or_default()
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(args.a + args.b)
    }
}

#[derive(Clone)]
struct MultiplyTool(Arc<AtomicUsize>);

impl Tool for MultiplyTool {
    const NAME: &'static str = "multiply";
    type Error = ConformanceToolError;
    type Args = BinOpArgs;
    type Output = i64;

    fn description(&self) -> String {
        "Multiply two integers a and b.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(BinOpArgs)).unwrap_or_default()
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(args.a * args.b)
    }
}

fn report_from_response(
    name: &'static str,
    started: Instant,
    tool_calls: usize,
    response: crate::agent::PromptResponse,
) -> Result<ScenarioReport, ScenarioError> {
    if let Some(messages) = response.messages.as_deref() {
        validate_protocol_hygiene(
            name,
            &response.output,
            messages,
            &[
                "<tool_call>",
                "</tool_call>",
                "<tool_response>",
                "</tool_response>",
                "<|im_start|>",
                "<|im_end|>",
                "<think>",
                "</think>",
            ],
        )?;
    }
    Ok(ScenarioReport {
        name,
        tool_calls,
        prompt_tokens: response.usage.input_tokens,
        generated_tokens: response.usage.output_tokens,
        history_messages: response.messages.as_ref().map_or(0, Vec::len),
        duration: started.elapsed(),
        response: response.output,
    })
}

fn value_matches_integer(value: &serde_json::Value, expected: i64) -> bool {
    value.as_i64() == Some(expected)
        || value
            .as_str()
            .and_then(|text| text.trim().parse::<i64>().ok())
            == Some(expected)
}

/// Runs two independent calls in one assistant turn and validates canonical
/// call/result history correlation.
///
/// Set `tool_concurrency` to `Some(1)` to prove that serial host execution does
/// not split or drop the model's parallel call batch.
pub async fn parallel_tools<M, F>(
    model: M,
    configure: F,
    tool_concurrency: Option<usize>,
) -> Result<ScenarioReport, ScenarioError>
where
    M: CompletionModel + 'static,
    F: FnOnce(AgentBuilder<NoToolConfig>) -> AgentBuilder<NoToolConfig>,
{
    let add_calls = Arc::new(AtomicUsize::new(0));
    let subtract_calls = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let agent = configure(AgentBuilder::new(model))
        .preamble(FORCE_TOOLS_PREAMBLE)
        .temperature(0.0)
        .tool(CountingAdd(add_calls.clone()))
        .tool(CountingSubtract(subtract_calls.clone()))
        .default_max_turns(3)
        .build();
    let request = agent.prompt(PARALLEL_PROMPT).max_turns(3);
    let response = match tool_concurrency {
        Some(concurrency) => {
            request
                .tool_concurrency(concurrency)
                .extended_details()
                .await?
        }
        None => request.extended_details().await?,
    };
    let scenario = if tool_concurrency == Some(1) {
        "parallel_tools_serial_execution"
    } else {
        "parallel_tools"
    };
    let messages = response.messages.as_deref().ok_or_else(|| {
        ScenarioError::contract(scenario, "extended run omitted accumulated message history")
    })?;
    validate_tool_correlation(scenario, messages)?;

    let Some((call_index, calls)) = messages.iter().enumerate().find_map(|(index, message)| {
        let Message::Assistant { content, .. } = message else {
            return None;
        };
        let calls = content
            .iter()
            .filter_map(|item| match item {
                AssistantContent::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect::<Vec<_>>();
        (calls.len() == 2).then_some((index, calls))
    }) else {
        return Err(ScenarioError::contract(
            scenario,
            format!("no assistant turn contained exactly two tool calls: {messages:?}"),
        ));
    };
    let mut names = calls
        .iter()
        .map(|call| call.function.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    if names != ["add", "subtract"] {
        return Err(ScenarioError::contract(
            scenario,
            format!("parallel turn called {names:?}, expected add and subtract"),
        ));
    }
    let results_message = messages.get(call_index + 1).ok_or_else(|| {
        ScenarioError::contract(
            scenario,
            "parallel call turn has no following result message",
        )
    })?;
    let values = tool_result_values(results_message);
    if !values.iter().any(|value| value_matches_integer(value, 7))
        || !values.iter().any(|value| value_matches_integer(value, 8))
        || values.len() != 2
    {
        return Err(ScenarioError::contract(
            scenario,
            format!("parallel result message did not contain exactly 7 and 8: {values:?}"),
        ));
    }
    let add = add_calls.load(Ordering::SeqCst);
    let subtract = subtract_calls.load(Ordering::SeqCst);
    if add != 1 || subtract != 1 {
        return Err(ScenarioError::contract(
            scenario,
            format!("execution counts were add={add}, subtract={subtract}, expected one each"),
        ));
    }
    report_from_response(scenario, started, add + subtract, response)
}

/// Runs a zero-argument tool and validates verbatim string-result handling.
pub async fn zero_argument_tool<M, F>(
    model: M,
    configure: F,
) -> Result<ScenarioReport, ScenarioError>
where
    M: CompletionModel + 'static,
    F: FnOnce(AgentBuilder<NoToolConfig>) -> AgentBuilder<NoToolConfig>,
{
    const SCENARIO: &str = "zero_argument_tool";
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let agent = configure(AgentBuilder::new(model))
        .preamble("You must use the provided tools. Report tool outputs exactly as returned.")
        .temperature(0.0)
        .tool(PingTool(calls.clone()))
        .default_max_turns(2)
        .build();
    let response = agent
        .prompt("Call the ping tool, then report the exact marker it returns.")
        .max_turns(2)
        .extended_details()
        .await?;
    let messages = response.messages.as_deref().ok_or_else(|| {
        ScenarioError::contract(SCENARIO, "extended run omitted accumulated message history")
    })?;
    validate_tool_correlation(SCENARIO, messages)?;
    let values = messages
        .iter()
        .flat_map(tool_result_values)
        .collect::<Vec<_>>();
    if calls.load(Ordering::SeqCst) != 1
        || !values
            .iter()
            .any(|value| value.as_str() == Some(PING_OUTPUT))
        || !response.output.contains(PING_OUTPUT)
    {
        return Err(ScenarioError::contract(
            SCENARIO,
            format!(
                "calls={}, results={values:?}, response={:?}",
                calls.load(Ordering::SeqCst),
                response.output
            ),
        ));
    }
    report_from_response(SCENARIO, started, 1, response)
}

/// Runs string- and JSON-returning tools and validates that neither output is
/// double encoded.
pub async fn tool_output_serialization<M, F>(
    model: M,
    configure: F,
) -> Result<ScenarioReport, ScenarioError>
where
    M: CompletionModel + 'static,
    F: FnOnce(AgentBuilder<NoToolConfig>) -> AgentBuilder<NoToolConfig>,
{
    const SCENARIO: &str = "tool_output_serialization";
    let started = Instant::now();
    let motto_calls = Arc::new(AtomicUsize::new(0));
    let config_calls = Arc::new(AtomicUsize::new(0));
    let agent = configure(AgentBuilder::new(model))
        .preamble("You must use the provided tools before answering.")
        .temperature(0.0)
        .tool(MottoTool(motto_calls.clone()))
        .tool(ConfigTool(config_calls.clone()))
        .default_max_turns(3)
        .build();
    let response = agent
        .prompt("Call fetch_motto and fetch_config, then summarize both outputs in one sentence.")
        .max_turns(3)
        .extended_details()
        .await?;
    let messages = response.messages.as_deref().ok_or_else(|| {
        ScenarioError::contract(SCENARIO, "extended run omitted accumulated message history")
    })?;
    validate_tool_correlation(SCENARIO, messages)?;
    let values = messages
        .iter()
        .flat_map(tool_result_values)
        .collect::<Vec<_>>();
    let expected_config = serde_json::to_value(ConfigOutput {
        service: "cassette-lab".to_string(),
        max_retries: 3,
    })?;
    let motto_ok = values
        .iter()
        .any(|value| value.as_str() == Some(MOTTO_OUTPUT));
    let config_ok = values.iter().any(|value| {
        value == &expected_config
            || value
                .as_str()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                .as_ref()
                == Some(&expected_config)
    });
    let motto_count = motto_calls.load(Ordering::SeqCst);
    let config_count = config_calls.load(Ordering::SeqCst);
    if !motto_ok || !config_ok || motto_count != 1 || config_count != 1 {
        return Err(ScenarioError::contract(
            SCENARIO,
            format!(
                "expected one verbatim motto and one semantic config JSON; motto_calls={motto_count}, config_calls={config_count}, values={values:?}"
            ),
        ));
    }
    report_from_response(SCENARIO, started, motto_count + config_count, response)
}

/// Runs a nested, escaped, Unicode-bearing argument payload through typed tool
/// deserialization and validates the exact semantic value received by the tool.
pub async fn complex_tool_arguments<M, F>(
    model: M,
    configure: F,
) -> Result<ScenarioReport, ScenarioError>
where
    M: CompletionModel + 'static,
    F: FnOnce(AgentBuilder<NoToolConfig>) -> AgentBuilder<NoToolConfig>,
{
    const SCENARIO: &str = "complex_tool_arguments";
    let expected = ComplexArgs {
        profile: ComplexProfile {
            name: "Zoë \"Z\"".to_string(),
            tags: vec!["rust".to_string(), "東京".to_string()],
        },
        mode: ComplexMode::Careful,
        note: Some("line one\nline two".to_string()),
        quote: "path C:\\tmp and \"quoted\"".to_string(),
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let captured = Arc::new(Mutex::new(None));
    let started = Instant::now();
    let agent = configure(AgentBuilder::new(model))
        .preamble("Use store_profile exactly once with every value supplied by the user.")
        .temperature(0.0)
        .tool(CaptureComplexTool {
            calls: calls.clone(),
            captured: captured.clone(),
        })
        .default_max_turns(3)
        .build();
    let response = agent
        .prompt(
            "Call store_profile with profile.name exactly `Zoë \\\"Z\\\"`, profile.tags exactly [`rust`, `東京`], mode `careful`, note containing the two lines `line one` and `line two` separated by a newline, and quote exactly `path C:\\\\tmp and \\\"quoted\\\"`. Then confirm it was stored.",
        )
        .max_turns(3)
        .extended_details()
        .await?;
    let observed = lock_recover(&captured).clone();
    if calls.load(Ordering::SeqCst) != 1 || observed.as_ref() != Some(&expected) {
        return Err(ScenarioError::contract(
            SCENARIO,
            format!(
                "calls={}, expected={expected:?}, observed={observed:?}, response={:?}",
                calls.load(Ordering::SeqCst),
                response.output
            ),
        ));
    }
    let messages = response.messages.as_deref().ok_or_else(|| {
        ScenarioError::contract(SCENARIO, "extended run omitted accumulated message history")
    })?;
    validate_tool_correlation(SCENARIO, messages)?;
    report_from_response(SCENARIO, started, 1, response)
}

/// Runs the same deterministic text request through buffered and raw streaming
/// completion surfaces and validates equivalent visible content and usage.
pub async fn buffered_streaming_text_parity<M>(model: M) -> Result<ScenarioReport, ScenarioError>
where
    M: CompletionModel + Clone + 'static,
{
    const SCENARIO: &str = "buffered_streaming_text_parity";
    const PROMPT: &str = "Answer with exactly the single word Paris.";
    let started = Instant::now();
    let request = || {
        model
            .completion_request(PROMPT)
            .temperature(0.0)
            .max_tokens(32)
            .build()
    };
    let buffered = model.completion(request()).await?;
    let buffered_text = buffered
        .choice
        .iter()
        .filter_map(|item| match item {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>();

    let mut stream = model.stream(request()).await?;
    let mut streamed_text = String::new();
    let mut streamed_usage = None;
    while let Some(item) = stream.next().await {
        match item? {
            crate::streaming::StreamedAssistantContent::Text(text) => {
                streamed_text.push_str(&text.text);
            }
            crate::streaming::StreamedAssistantContent::Final(response) => {
                streamed_usage = Some(response.usage);
            }
            crate::streaming::StreamedAssistantContent::ToolCall { .. }
            | crate::streaming::StreamedAssistantContent::ToolCallDelta { .. }
            | crate::streaming::StreamedAssistantContent::Reasoning(_)
            | crate::streaming::StreamedAssistantContent::ReasoningDelta { .. }
            | crate::streaming::StreamedAssistantContent::Unknown(_) => {}
        }
    }
    let usage = streamed_usage.ok_or_else(|| {
        ScenarioError::contract(SCENARIO, "raw stream omitted its final response metadata")
    })?;
    let normalize = |text: &str| {
        text.trim()
            .trim_matches(|character: char| !character.is_alphanumeric())
            .to_string()
    };
    let buffered_answer = normalize(&buffered_text);
    let streamed_answer = normalize(&streamed_text);
    if !buffered_answer.eq_ignore_ascii_case("Paris")
        || !streamed_answer.eq_ignore_ascii_case("Paris")
        || !buffered.usage.has_values()
        || !usage.has_values()
    {
        return Err(ScenarioError::contract(
            SCENARIO,
            format!(
                "buffered={buffered_text:?}, streamed={streamed_text:?}, buffered_usage={:?}, streamed_usage={usage:?}",
                buffered.usage
            ),
        ));
    }
    Ok(ScenarioReport {
        name: SCENARIO,
        tool_calls: 0,
        prompt_tokens: usage.input_tokens,
        generated_tokens: usage.output_tokens,
        history_messages: 0,
        duration: started.elapsed(),
        response: streamed_text,
    })
}

/// Uses the conformance tool set to exercise in-band admission: a tool
/// name the model calls but the agent never registered returns a
/// synthetic result naming the problem — the run continues, nothing is
/// executed, nothing fails.
pub async fn invalid_tool_recovery<M, F>(
    model: M,
    configure: F,
) -> Result<ScenarioReport, ScenarioError>
where
    M: CompletionModel + 'static,
    F: FnOnce(AgentBuilder<NoToolConfig>) -> AgentBuilder<NoToolConfig>,
{
    const SCENARIO: &str = "invalid_tool_recovery";
    const PROMPT: &str = "Call the add tool exactly once with x=2 and y=3. Do not call sum.";
    let started = Instant::now();
    let sum_calls = Arc::new(AtomicUsize::new(0));
    // Drive the loop through the public surface: the model emits a call
    // to `add`, which the agent never registered (only `sum` exists), so
    // admission answers in-band — the model is told, nothing executes,
    // the run finishes.
    let agent = configure(AgentBuilder::new(model))
        .preamble(FORCE_TOOLS_PREAMBLE)
        .temperature(0.0)
        .tool(CountingSum(sum_calls.clone()))
        .tool_choice(ToolChoice::Required)
        .build();
    let response = agent.runner(PROMPT).max_turns(4).run().await?;
    // Nothing executed: the unregistered name never ran a body.
    if sum_calls.load(Ordering::SeqCst) != 0 {
        return Err(ScenarioError::contract(
            SCENARIO,
            "an unregistered tool name executed instead of being told in-band",
        ));
    }
    // The model saw the in-band synthetic result in the run's history.
    let rendered = serde_json::to_string(&response.messages).unwrap_or_default();
    if !rendered.contains("unknown or disallowed tool") {
        return Err(ScenarioError::contract(
            SCENARIO,
            format!("synthetic result did not reach the history: {rendered}"),
        ));
    }

    Ok(ScenarioReport {
        name: SCENARIO,
        tool_calls: 0,
        prompt_tokens: response.usage.input_tokens,
        generated_tokens: response.usage.output_tokens,
        history_messages: response
            .messages
            .as_ref()
            .map(|messages| messages.len())
            .unwrap_or(0),
        duration: started.elapsed(),
        response: response.output.clone(),
    })
}

/// Exercises chained argument/result rewrites through the public agent
/// surface: two argument rewrites compose, the result rewrites wrap in
/// registration order.
pub async fn hook_rewrites<M, F>(model: M, configure: F) -> Result<ScenarioReport, ScenarioError>
where
    M: CompletionModel + 'static,
    F: FnOnce(AgentBuilder<NoToolConfig>) -> AgentBuilder<NoToolConfig>,
{
    const SCENARIO: &str = "hook_rewrites";
    let started = Instant::now();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = ObserveArguments::default();
    let observed_probe = observed.clone();
    let agent = configure(AgentBuilder::new(model))
        .preamble("Use add for arithmetic and report only the tool result.")
        .temperature(0.0)
        .tool(CountingAdd(calls.clone()))
        .default_max_turns(3)
        .build();
    let response = agent
        .prompt("Use add once for x=1 and y=1, then report what the tool returns.")
        .max_turns(3)
        .add_hook(RewriteArgument {
            key: "x",
            value: serde_json::json!(7),
        })
        .add_hook(RewriteArgument {
            key: "y",
            value: serde_json::json!(8),
        })
        .add_hook(observed)
        .add_hook(ReplaceResult("portable-redacted"))
        .add_hook(WrapResult)
        .extended_details()
        .await?;
    let observations = lock_recover(&observed_probe.0).clone();
    validate_rewritten_arguments(
        SCENARIO,
        &observations,
        &serde_json::json!({ "x": 7, "y": 8 }),
    )?;
    let messages = response.messages.as_deref().ok_or_else(|| {
        ScenarioError::contract(SCENARIO, "extended hook run omitted message history")
    })?;
    let results = messages
        .iter()
        .flat_map(tool_result_values)
        .collect::<Vec<_>>();
    if calls.load(Ordering::SeqCst) != 1
        || !results
            .iter()
            .any(|value| value == &serde_json::json!("[portable-redacted]"))
        || response.completion_calls.len() != 2
    {
        return Err(ScenarioError::contract(
            SCENARIO,
            format!(
                "calls={}, completion_calls={}, results={results:?}, output={:?}",
                calls.load(Ordering::SeqCst),
                response.completion_calls.len(),
                response.output
            ),
        ));
    }
    report_from_response(SCENARIO, started, 1, response)
}

/// Exercises post-execution cancellation and max-turn diagnostics through the
/// public agent driver using real model-emitted calls.
pub async fn cancellation_and_max_turns<M, F>(
    model: M,
    configure: F,
) -> Result<ScenarioReport, ScenarioError>
where
    M: CompletionModel + Clone + 'static,
    F: Fn(AgentBuilder<NoToolConfig>) -> AgentBuilder<NoToolConfig>,
{
    const SCENARIO: &str = "cancellation_and_max_turns";
    const REASON: &str = "portable result veto";
    let started = Instant::now();
    let cancelled_calls = Arc::new(AtomicUsize::new(0));
    let cancelled_agent = configure(AgentBuilder::new(model.clone()))
        .preamble("Use add for arithmetic; never calculate by hand.")
        .temperature(0.0)
        .tool(CountingAdd(cancelled_calls.clone()))
        .build();
    let cancelled = match cancelled_agent
        .prompt("Use add once to compute x=20 plus y=22.")
        .max_turns(2)
        .add_hook(StopAfterResult(REASON))
        .await
    {
        Err(error) => error,
        Ok(output) => {
            return Err(ScenarioError::contract(
                SCENARIO,
                format!("result cancellation unexpectedly completed: {output:?}"),
            ));
        }
    };
    validate_cancelled_failure(&cancelled, REASON, CountingAdd::NAME)?;

    let max_turn_calls = Arc::new(AtomicUsize::new(0));
    let max_turn_agent = configure(AgentBuilder::new(model))
        .preamble("Use add for arithmetic; never calculate by hand.")
        .temperature(0.0)
        .tool(CountingAdd(max_turn_calls.clone()))
        .build();
    let max_turn = match max_turn_agent
        .prompt("Use add once to compute x=20 plus y=22, then report the result.")
        .max_turns(1)
        .await
    {
        Err(error) => error,
        Ok(output) => {
            return Err(ScenarioError::contract(
                SCENARIO,
                format!("one-turn budget unexpectedly completed: {output:?}"),
            ));
        }
    };
    validate_max_turns_failure(&max_turn, 1)?;
    let cancelled_count = cancelled_calls.load(Ordering::SeqCst);
    let max_turn_count = max_turn_calls.load(Ordering::SeqCst);
    if cancelled_count != 1 || max_turn_count != 1 {
        return Err(ScenarioError::contract(
            SCENARIO,
            format!("cancelled executions={cancelled_count}, max-turn executions={max_turn_count}"),
        ));
    }
    Ok(ScenarioReport {
        name: SCENARIO,
        tool_calls: cancelled_count + max_turn_count,
        prompt_tokens: 0,
        generated_tokens: 0,
        history_messages: 4,
        duration: started.elapsed(),
        response: "post-result cancellation and max-turn diagnostics passed".to_string(),
    })
}

/// Runs the portable optional-argument tool scenario.
///
/// `configure` is deliberately outside the scenario so a provider suite can
/// attach transport-only settings without putting them into the shared model
/// contract.
pub async fn optional_argument<M, F>(
    model: M,
    configure: F,
) -> Result<ScenarioReport, ScenarioError>
where
    M: CompletionModel + 'static,
    F: FnOnce(AgentBuilder<NoToolConfig>) -> AgentBuilder<NoToolConfig>,
{
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let agent = configure(AgentBuilder::new(model))
        .preamble("Use the repeat_text tool whenever asked to repeat text.")
        .tool(RepeatTool {
            calls: calls.clone(),
        })
        .default_max_turns(4)
        .build();
    let result = agent
        .prompt(
            "Use the repeat_text tool to repeat the word \"banana\" 3 times, then show me the exact result.",
        )
        .extended_details()
        .await?;
    let response = result.output.clone();
    let tool_calls = calls.load(Ordering::SeqCst);
    if tool_calls == 0
        || response.matches("banana").count() < 1
        || !has_tool_roundtrip(result.messages.as_deref())
    {
        return Err(ScenarioError::contract(
            "optional_argument",
            format!("calls={tool_calls}, response={response:?}"),
        ));
    }
    report_from_response("optional_argument", started, tool_calls, result)
}

/// Runs a portable two-tool sequential arithmetic scenario.
pub async fn sequential_tools<M, F>(model: M, configure: F) -> Result<ScenarioReport, ScenarioError>
where
    M: CompletionModel + 'static,
    F: FnOnce(AgentBuilder<NoToolConfig>) -> AgentBuilder<NoToolConfig>,
{
    let add_calls = Arc::new(AtomicUsize::new(0));
    let multiply_calls = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let agent = configure(AgentBuilder::new(model))
        .preamble(
            "You are a calculator. Use the add and multiply tools for arithmetic; never compute by hand.",
        )
        .tool(AddTool(add_calls.clone()))
        .tool(MultiplyTool(multiply_calls.clone()))
        .default_max_turns(6)
        .build();
    let result = agent
        .prompt(
            "Compute (4 + 6) * 2. First call the add tool, then call the multiply tool on the result. Tell me the final number.",
        )
        .extended_details()
        .await?;
    let response = result.output.clone();
    let add = add_calls.load(Ordering::SeqCst);
    let multiply = multiply_calls.load(Ordering::SeqCst);
    if add == 0
        || multiply == 0
        || !response.contains("20")
        || !has_tool_roundtrip(result.messages.as_deref())
    {
        return Err(ScenarioError::contract(
            "sequential_tools",
            format!("add={add}, multiply={multiply}, response={response:?}"),
        ));
    }
    report_from_response("sequential_tools", started, add + multiply, result)
}

/// Runs a tool through Rig's multi-turn streaming agent driver.
pub async fn streaming_tool<M, F>(model: M, configure: F) -> Result<ScenarioReport, ScenarioError>
where
    M: CompletionModel + 'static,
    F: FnOnce(AgentBuilder<NoToolConfig>) -> AgentBuilder<NoToolConfig>,
{
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let agent = configure(AgentBuilder::new(model))
        .preamble("Use the add tool for arithmetic; do not calculate by hand.")
        .tool(AddTool(calls.clone()))
        .default_max_turns(4)
        .build();
    let mut stream = agent
        .stream_prompt("Use add to calculate 17 + 25, then state the final number.")
        .max_turns(4)
        .await;
    let mut final_response = None;
    let mut final_count = 0_usize;
    let mut completion_usage = crate::completion::Usage::new();
    let mut streamed_call_ids = Vec::new();
    let mut streamed_result_ids = Vec::new();
    while let Some(item) = stream.next().await {
        match item? {
            MultiTurnStreamItem::StreamAssistantItem(
                crate::streaming::StreamedAssistantContent::ToolCall {
                    internal_call_id, ..
                },
            ) => streamed_call_ids.push(internal_call_id),
            MultiTurnStreamItem::StreamUserItem(
                crate::streaming::StreamedUserContent::ToolResult {
                    internal_call_id, ..
                },
            ) => streamed_result_ids.push(internal_call_id),
            MultiTurnStreamItem::CompletionCall(call) => completion_usage += call.usage,
            MultiTurnStreamItem::FinalResponse(response) => {
                final_count += 1;
                final_response = Some(response);
            }
            MultiTurnStreamItem::StreamAssistantItem(_)
            | MultiTurnStreamItem::TurnStarted { .. }
            | MultiTurnStreamItem::TurnCommitted { .. }
            | MultiTurnStreamItem::BatchResults { .. }
            | MultiTurnStreamItem::ToolExecutionCommitted { .. }
            | MultiTurnStreamItem::ModelTurnRetried { .. }
            | MultiTurnStreamItem::Steer { .. } => {}
        }
    }
    let result = final_response.ok_or_else(|| {
        ScenarioError::contract("streaming_tool", "stream produced no final response")
    })?;
    let response = result.output.clone();
    let history_messages = result.messages.as_ref().map_or(0, Vec::len);
    let tool_calls = calls.load(Ordering::SeqCst);
    streamed_call_ids.sort();
    streamed_result_ids.sort();
    let correlated_stream =
        !streamed_call_ids.is_empty() && streamed_call_ids == streamed_result_ids;
    let correlated_history = result
        .messages
        .as_deref()
        .is_some_and(|messages| validate_tool_correlation("streaming_tool", messages).is_ok());
    if tool_calls == 0
        || !response.contains("42")
        || history_messages < 4
        || final_count != 1
        || !correlated_stream
        || !correlated_history
        || completion_usage != result.usage
        || result.completion_calls.is_empty()
    {
        return Err(ScenarioError::contract(
            "streaming_tool",
            format!(
                "calls={tool_calls}, final_count={final_count}, streamed_call_ids={streamed_call_ids:?}, streamed_result_ids={streamed_result_ids:?}, completion_usage={completion_usage:?}, final_usage={:?}, history_messages={history_messages}, response={response:?}",
                result.usage
            ),
        ));
    }
    if let Some(messages) = result.messages.as_deref() {
        validate_protocol_hygiene(
            "streaming_tool",
            &response,
            messages,
            &["<tool_call>", "</tool_call>", "<think>", "</think>"],
        )?;
    }
    Ok(ScenarioReport {
        name: "streaming_tool",
        tool_calls,
        prompt_tokens: result.usage.input_tokens,
        generated_tokens: result.usage.output_tokens,
        history_messages,
        duration: started.elapsed(),
        response,
    })
}

/// Runs all portable tool-choice modes directly against a completion model.
pub async fn tool_choice_modes<M>(model: M) -> Result<ScenarioReport, ScenarioError>
where
    M: CompletionModel + Clone + 'static,
{
    let definition = |name: &str| ToolDefinition {
        name: name.to_string(),
        description: format!("Return the supplied integer using {name}."),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {"value": {"type": "integer"}},
            "required": ["value"]
        }),
    };
    let tools = vec![definition("alpha"), definition("beta")];
    let started = Instant::now();
    let none = model
        .completion(
            model
                .completion_request("Answer with only the number 4. Do not call a function.")
                .tools(tools.clone())
                .tool_choice(ToolChoice::None)
                .temperature(0.0)
                .max_tokens(64)
                .build(),
        )
        .await?;
    if none
        .choice
        .iter()
        .any(|item| matches!(item, AssistantContent::ToolCall(_)))
    {
        return Err(ScenarioError::contract(
            "tool_choice_modes",
            "tool_choice none emitted a tool call",
        ));
    }

    let required = model
        .completion(
            model
                .completion_request("Call alpha with value 7.")
                .tools(tools.clone())
                .tool_choice(ToolChoice::Required)
                .temperature(0.0)
                .max_tokens(96)
                .build(),
        )
        .await?;
    let required_calls = required
        .choice
        .iter()
        .filter(|item| matches!(item, AssistantContent::ToolCall(_)))
        .count();
    if required_calls == 0 {
        return Err(ScenarioError::contract(
            "tool_choice_modes",
            "tool_choice required emitted no tool call",
        ));
    }

    let specific = model
        .completion(
            model
                .completion_request("Call beta with value 9.")
                .tools(tools)
                .tool_choice(ToolChoice::Specific {
                    function_names: vec!["beta".to_string()],
                })
                .temperature(0.0)
                .max_tokens(96)
                .build(),
        )
        .await?;
    let specific_calls = specific
        .choice
        .iter()
        .filter_map(|item| match item {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect::<Vec<_>>();
    if specific_calls.is_empty()
        || specific_calls
            .iter()
            .any(|call| call.function.name != "beta")
    {
        return Err(ScenarioError::contract(
            "tool_choice_modes",
            "specific tool choice did not select only beta",
        ));
    }

    Ok(ScenarioReport {
        name: "tool_choice_modes",
        tool_calls: required_calls + specific_calls.len(),
        prompt_tokens: none.usage.input_tokens
            + required.usage.input_tokens
            + specific.usage.input_tokens,
        generated_tokens: none.usage.output_tokens
            + required.usage.output_tokens
            + specific.usage.output_tokens,
        history_messages: 0,
        duration: started.elapsed(),
        response: "none, required, and specific modes passed".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolOutput;
    use crate::{
        completion::Usage,
        test_utils::{MockCompletionModel, MockStreamEvent, MockTurn, mock_final},
    };
    use rig_core::{
        OneOrMany,
        completion::{CompletionRequest, CompletionResponse},
        message::{
            DocumentSourceKind, Image, ToolCall, ToolFunction, ToolResult, ToolResultContent,
        },
        streaming::StreamingCompletionResponse,
        tool::ToolResult as ExecutionResult,
    };

    fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> AssistantContent {
        AssistantContent::ToolCall(ToolCall::new(
            id.to_string(),
            ToolFunction::new(name.to_string(), arguments),
        ))
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            ..Usage::new()
        }
    }

    fn fixture_contract(condition: bool, details: &str) -> Result<(), ScenarioError> {
        if condition {
            Ok(())
        } else {
            Err(ScenarioError::contract("test_fixture", details))
        }
    }

    #[tokio::test]
    async fn parallel_contract_validates_batch_and_correlation() -> Result<(), ScenarioError> {
        let first = MockTurn::from_contents([
            tool_call("call_add", "add", serde_json::json!({"x": 3, "y": 4})),
            tool_call(
                "call_subtract",
                "subtract",
                serde_json::json!({"x": 10, "y": 2}),
            ),
        ])
        .map_err(|error| ScenarioError::contract("test_fixture", error.to_string()))?;
        let report = parallel_tools(
            MockCompletionModel::new([first, MockTurn::text("7 and 8")]),
            |builder| builder,
            Some(1),
        )
        .await?;
        fixture_contract(report.tool_calls == 2, "parallel tool-call count")?;
        fixture_contract(report.history_messages >= 4, "parallel history length")?;
        Ok(())
    }

    #[tokio::test]
    async fn zero_argument_and_output_serialization_contracts_pass() -> Result<(), ScenarioError> {
        let zero = zero_argument_tool(
            MockCompletionModel::new([
                MockTurn::tool_call("ping_call", "ping", serde_json::json!({})),
                MockTurn::text(PING_OUTPUT),
            ]),
            |builder| builder,
        )
        .await?;
        fixture_contract(zero.tool_calls == 1, "zero-argument call count")?;

        let first = MockTurn::from_contents([
            tool_call("motto_call", "fetch_motto", serde_json::json!({})),
            tool_call("config_call", "fetch_config", serde_json::json!({})),
        ])
        .map_err(|error| ScenarioError::contract("test_fixture", error.to_string()))?;
        let serialized = tool_output_serialization(
            MockCompletionModel::new([first, MockTurn::text("summary")]),
            |builder| builder,
        )
        .await?;
        fixture_contract(serialized.tool_calls == 2, "serialized-output call count")?;
        Ok(())
    }

    #[tokio::test]
    async fn complex_arguments_preserve_nested_unicode_and_escapes() -> Result<(), ScenarioError> {
        let arguments = serde_json::json!({
            "profile": {"name": "Zoë \"Z\"", "tags": ["rust", "東京"]},
            "mode": "careful",
            "note": "line one\nline two",
            "quote": "path C:\\tmp and \"quoted\""
        });
        let report = complex_tool_arguments(
            MockCompletionModel::new([
                MockTurn::tool_call("profile_call", "store_profile", arguments),
                MockTurn::text("stored"),
            ]),
            |builder| builder,
        )
        .await?;
        fixture_contract(report.tool_calls == 1, "complex-argument call count")?;
        Ok(())
    }

    #[tokio::test]
    async fn streaming_contract_checks_events_history_and_usage() -> Result<(), ScenarioError> {
        let model = MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::tool_call(
                    "add_call",
                    "add",
                    serde_json::json!({"a": 17, "b": 25}),
                ),
                MockStreamEvent::FinalResponse(mock_final(usage(10, 2))),
            ],
            vec![
                MockStreamEvent::text("42"),
                MockStreamEvent::FinalResponse(mock_final(usage(14, 1))),
            ],
        ]);
        let report = streaming_tool(model, |builder| builder).await?;
        fixture_contract(report.prompt_tokens == 24, "streaming input usage")?;
        fixture_contract(report.generated_tokens == 3, "streaming output usage")?;
        Ok(())
    }

    #[tokio::test]
    async fn invalid_recovery_paths_do_not_execute_tools() -> Result<(), ScenarioError> {
        let report = invalid_tool_recovery(
            MockCompletionModel::new([
                MockTurn::tool_call("invalid-add", "add", serde_json::json!({ "x": 2, "y": 3 })),
                MockTurn::text("understood, done"),
            ]),
            |builder| builder,
        )
        .await?;
        fixture_contract(report.tool_calls == 0, "recovery executes nothing")?;
        Ok(())
    }

    #[tokio::test]
    async fn hook_rewrites_chain_across_hooks() -> Result<(), ScenarioError> {
        let report = hook_rewrites(
            MockCompletionModel::new([
                MockTurn::tool_call("hook-add", "add", serde_json::json!({ "x": 1, "y": 1 })),
                MockTurn::text("[portable-redacted]"),
            ]),
            |builder| builder,
        )
        .await?;
        fixture_contract(report.tool_calls == 1, "hook execution count")?;
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_and_max_turn_controls_retain_diagnostics() -> Result<(), ScenarioError> {
        let report = cancellation_and_max_turns(
            MockCompletionModel::new([
                MockTurn::tool_call("cancel-add", "add", serde_json::json!({ "x": 20, "y": 22 })),
                MockTurn::tool_call("budget-add", "add", serde_json::json!({ "x": 20, "y": 22 })),
            ]),
            |builder| builder,
        )
        .await?;
        fixture_contract(report.tool_calls == 2, "run-control execution count")?;
        Ok(())
    }

    #[test]
    fn typed_validators_reject_bad_structured_output_and_protocol_leaks() {
        let invalid = decode_structured_output::<ConfigOutput>("invalid_json", "not json");
        assert!(matches!(invalid, Err(ScenarioError::Contract { .. })));

        let messages = vec![Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::text("visible <tool_call>")),
        }];
        let hygiene = validate_protocol_hygiene(
            "protocol_hygiene",
            "visible <tool_call>",
            &messages,
            &["<tool_call>"],
        );
        assert!(matches!(hygiene, Err(ScenarioError::Contract { .. })));
    }

    // ------------------------------------------------------------------
    // Validator failure branches
    // ------------------------------------------------------------------

    #[test]
    fn validators_reject_wrong_error_shapes() {
        let wrong =
            PromptError::CompletionError(CompletionError::ProviderError("wrong shape".to_string()));
        assert!(matches!(
            validate_cancelled_failure(&wrong, "reason", "add"),
            Err(ScenarioError::Contract { .. })
        ));
        assert!(matches!(
            validate_max_turns_failure(&wrong, 1),
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[test]
    fn cancelled_validation_rejects_reason_and_history_mismatches() {
        let no_calls = vec![Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::text("no tool calls")),
        }];
        let mismatched_reason = PromptError::PromptCancelled {
            chat_history: no_calls.clone(),
            reason: "other reason".to_string(),
        };
        assert!(matches!(
            validate_cancelled_failure(&mismatched_reason, "expected", "add"),
            Err(ScenarioError::Contract { .. })
        ));

        let matching_reason_missing_call = PromptError::PromptCancelled {
            chat_history: no_calls,
            reason: "expected".to_string(),
        };
        assert!(matches!(
            validate_cancelled_failure(&matching_reason_missing_call, "expected", "add"),
            Err(ScenarioError::Contract { .. })
        ));

        let with_call = vec![
            Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::text("turn")),
            },
            Message::Assistant {
                id: None,
                content: OneOrMany::one(tool_call(
                    "c1",
                    "add",
                    serde_json::json!({"x": 1, "y": 2}),
                )),
            },
        ];
        let matching = PromptError::PromptCancelled {
            chat_history: with_call,
            reason: "expected".to_string(),
        };
        assert!(validate_cancelled_failure(&matching, "expected", "add").is_ok());
    }

    #[test]
    fn max_turns_validation_rejects_budget_and_history_mismatches() {
        let prompt = Message::User {
            content: OneOrMany::one(UserContent::text("pending")),
        };
        let wrong_budget = PromptError::MaxTurnsError {
            max_turns: 2,
            chat_history: Box::new(Vec::new()),
            prompt: Box::new(prompt.clone()),
        };
        assert!(matches!(
            validate_max_turns_failure(&wrong_budget, 1),
            Err(ScenarioError::Contract { .. })
        ));

        let right_budget_empty_history = PromptError::MaxTurnsError {
            max_turns: 1,
            chat_history: Box::new(Vec::new()),
            prompt: Box::new(prompt),
        };
        assert!(matches!(
            validate_max_turns_failure(&right_budget_empty_history, 1),
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[test]
    fn rewritten_argument_validation_rejects_malformed_observations() {
        const SCENARIO: &str = "validate_rewritten_arguments";
        assert!(matches!(
            validate_rewritten_arguments(
                SCENARIO,
                &[serde_json::json!({"x": 7})],
                &serde_json::json!([1]),
            ),
            Err(ScenarioError::Contract { .. })
        ));
        assert!(matches!(
            validate_rewritten_arguments(SCENARIO, &[], &serde_json::json!({"x": 7})),
            Err(ScenarioError::Contract { .. })
        ));
        assert!(matches!(
            validate_rewritten_arguments(
                SCENARIO,
                &[serde_json::json!(3)],
                &serde_json::json!({"x": 7}),
            ),
            Err(ScenarioError::Contract { .. })
        ));
        assert!(matches!(
            validate_rewritten_arguments(
                SCENARIO,
                &[serde_json::json!({"x": 8})],
                &serde_json::json!({"x": 7}),
            ),
            Err(ScenarioError::Contract { .. })
        ));
        assert!(
            validate_rewritten_arguments(
                SCENARIO,
                &[serde_json::json!({"x": 7, "z": 9})],
                &serde_json::json!({"x": 7}),
            )
            .is_ok()
        );
    }

    #[test]
    fn result_redaction_validation_covers_every_failure_mode() {
        const SCENARIO: &str = "result_redaction";
        assert!(matches!(
            validate_result_redaction(SCENARIO, false, "clean output", "s3cr3t"),
            Err(ScenarioError::Contract { .. })
        ));
        assert!(matches!(
            validate_result_redaction(SCENARIO, true, "", "s3cr3t"),
            Err(ScenarioError::Contract { .. })
        ));
        assert!(matches!(
            validate_result_redaction(SCENARIO, true, "leaked s3cr3t", "s3cr3t"),
            Err(ScenarioError::Contract { .. })
        ));
        assert!(validate_result_redaction(SCENARIO, true, "clean output", "s3cr3t").is_ok());
    }

    #[test]
    fn extraction_validation_rejects_wrong_people_and_missing_usage() {
        assert!(matches!(
            validate_extraction_fields(
                "extraction",
                Some("Grace"),
                Some("Hopper"),
                Some("mathematician"),
                usage(1, 1),
            ),
            Err(ScenarioError::Contract { .. })
        ));
        assert!(matches!(
            validate_extraction_fields(
                "extraction",
                Some("Ada"),
                Some("Lovelace"),
                Some("mathematician"),
                Usage::new(),
            ),
            Err(ScenarioError::Contract { .. })
        ));
    }

    // ------------------------------------------------------------------
    // History helpers
    // ------------------------------------------------------------------

    #[test]
    fn tool_result_values_skip_images_and_non_user_messages() {
        let image_result = ToolResult {
            id: "img".to_string(),
            call_id: None,
            content: OneOrMany::many(vec![
                ToolResultContent::text("label"),
                ToolResultContent::Image(Image {
                    data: DocumentSourceKind::url("https://example.test/i.png"),
                    ..Image::default()
                }),
            ])
            .expect("non-empty content"),
            status: None,
        };
        let user = Message::User {
            content: OneOrMany::one(UserContent::ToolResult(image_result)),
        };
        assert_eq!(
            tool_result_values(&user),
            vec![serde_json::Value::String("label".to_string())]
        );
        let assistant = Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::text("not a result")),
        };
        assert!(tool_result_values(&assistant).is_empty());
    }

    #[test]
    fn tool_correlation_validation_rejects_uncorrelated_histories() {
        let call = |id: &str| Message::Assistant {
            id: None,
            content: OneOrMany::one(tool_call(id, "add", serde_json::json!({"x": 1, "y": 2}))),
        };
        let result = |id: &str| {
            UserContent::ToolResult(ToolResult {
                id: id.to_string(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::text("ok")),
                status: None,
            })
        };
        // Correlated pass, including a skipped system message.
        let correlated = vec![
            Message::System {
                content: "preamble".to_string(),
            },
            call("c1"),
            Message::User {
                content: OneOrMany::one(result("c1")),
            },
        ];
        assert!(validate_tool_correlation("t", &correlated).is_ok());

        // History without any assistant tool calls.
        let no_calls = vec![Message::User {
            content: OneOrMany::one(result("r1")),
        }];
        assert!(matches!(
            validate_tool_correlation("t", &no_calls),
            Err(ScenarioError::Contract { .. })
        ));

        // Result id does not correlate with the call.
        let uncorrelated = vec![
            call("c1"),
            Message::User {
                content: OneOrMany::one(result("other")),
            },
        ];
        assert!(matches!(
            validate_tool_correlation("t", &uncorrelated),
            Err(ScenarioError::Contract { .. })
        ));

        // Dangling extra result.
        let dangling = vec![
            call("c1"),
            Message::User {
                content: OneOrMany::many(vec![result("c1"), result("extra")]).expect("non-empty"),
            },
        ];
        assert!(matches!(
            validate_tool_correlation("t", &dangling),
            Err(ScenarioError::Contract { .. })
        ));
    }

    // ------------------------------------------------------------------
    // Hook branches
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn rewrite_and_result_hooks_leave_non_add_tools_untouched() {
        let context = HookContext::new(Default::default());
        let rewrite = RewriteArgument {
            key: "x",
            value: serde_json::json!(7),
        };
        let other_tool = ToolCallEvent {
            tool_name: "subtract",
            tool_call_id: Some("p1"),
            internal_call_id: "i1",
            args: r#"{"x":1,"y":2}"#,
        };
        assert!(matches!(
            rewrite.on_tool_call(&context, other_tool).await,
            ToolCallAction::Run
        ));
        let bad_json = ToolCallEvent {
            tool_name: "add",
            tool_call_id: Some("p2"),
            internal_call_id: "i2",
            args: "not json",
        };
        assert!(matches!(
            rewrite.on_tool_call(&context, bad_json).await,
            ToolCallAction::Run
        ));
        let scalar_args = ToolCallEvent {
            tool_name: "add",
            tool_call_id: Some("p3"),
            internal_call_id: "i3",
            args: "3",
        };
        assert!(matches!(
            rewrite.on_tool_call(&context, scalar_args).await,
            ToolCallAction::Run
        ));
        let add_call = ToolCallEvent {
            tool_name: "add",
            tool_call_id: Some("p4"),
            internal_call_id: "i4",
            args: r#"{"y":2}"#,
        };
        assert!(matches!(
            rewrite.on_tool_call(&context, add_call).await,
            ToolCallAction::Rewrite(_)
        ));

        let presentation = ToolOutput::text("raw");
        let raw = ExecutionResult::success(ToolOutput::text("raw"));
        let result_event = ToolResultEvent {
            tool_name: "subtract",
            tool_call_id: Some("p5"),
            internal_call_id: "i5",
            args: "{}",
            presentation: &presentation,
            raw_result: &raw,
            tool_context: &ToolContext::new(),
        };
        assert!(matches!(
            ReplaceResult("redacted")
                .on_tool_result(&context, result_event)
                .await,
            ToolResultAction::Keep
        ));
        assert!(matches!(
            WrapResult.on_tool_result(&context, result_event).await,
            ToolResultAction::Keep
        ));
        assert!(matches!(
            StopAfterResult("reason")
                .on_tool_result(&context, result_event)
                .await,
            ToolResultAction::Keep
        ));
    }

    #[tokio::test]
    async fn sum_alias_tool_executes_and_counts() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut context = ToolContext::new();
        let output = Tool::call(
            &CountingSum(Arc::clone(&calls)),
            &mut context,
            OperationArgs { x: 2, y: 3 },
        )
        .await
        .expect("sum tool succeeds");
        assert_eq!(output, 5);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // ------------------------------------------------------------------
    // Scenario success paths
    // ------------------------------------------------------------------

    /// Serves one buffered text completion and one scripted stream from the
    /// same model instance, for scenarios that compare both surfaces.
    #[derive(Clone)]
    struct BufferedAndStreamMock {
        buffered: MockCompletionModel,
        streaming: MockCompletionModel,
    }

    impl CompletionModel for BufferedAndStreamMock {
        async fn completion(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, CompletionError> {
            self.buffered.completion(request).await
        }

        async fn stream(
            &self,
            request: CompletionRequest,
        ) -> Result<StreamingCompletionResponse, CompletionError> {
            self.streaming.stream(request).await
        }
    }

    #[tokio::test]
    async fn buffered_streaming_parity_contract_passes_with_mocks() -> Result<(), ScenarioError> {
        let model = BufferedAndStreamMock {
            buffered: MockCompletionModel::new([MockTurn::text("Paris").with_usage(usage(3, 1))]),
            streaming: MockCompletionModel::from_stream_turns([vec![
                MockStreamEvent::text("Paris"),
                MockStreamEvent::FinalResponse(mock_final(usage(3, 1))),
            ]]),
        };
        let report = buffered_streaming_text_parity(model).await?;
        fixture_contract(report.response.contains("Paris"), "parity streamed text")?;
        // Probe the helper itself: a satisfied contract passes and an
        // unsatisfied one reports the failure as a contract error.
        fixture_contract(true, "probe")?;
        match fixture_contract(false, "probe") {
            Err(ScenarioError::Contract { .. }) => {}
            Err(other) => return Err(other),
            Ok(()) => {
                return Err(ScenarioError::contract(
                    "test_fixture",
                    "fixture_contract accepted an unsatisfied contract",
                ));
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn parallel_contract_passes_with_default_concurrency() -> Result<(), ScenarioError> {
        let first = MockTurn::from_contents([
            tool_call("call_add", "add", serde_json::json!({"x": 3, "y": 4})),
            tool_call(
                "call_subtract",
                "subtract",
                serde_json::json!({"x": 10, "y": 2}),
            ),
        ])
        .map_err(|error| ScenarioError::contract("test_fixture", error.to_string()))?;
        let report = parallel_tools(
            MockCompletionModel::new([first, MockTurn::text("7 and 8")]),
            |builder| builder,
            None,
        )
        .await?;
        fixture_contract(report.tool_calls == 2, "default-concurrency call count")?;
        Ok(())
    }

    #[tokio::test]
    async fn optional_argument_contract_defaults_repetitions() -> Result<(), ScenarioError> {
        let report = optional_argument(
            MockCompletionModel::new([
                MockTurn::tool_call(
                    "repeat_call",
                    "repeat_text",
                    serde_json::json!({"text": "banana"}),
                ),
                MockTurn::text("banana banana"),
            ]),
            |builder| builder,
        )
        .await?;
        fixture_contract(report.tool_calls == 1, "optional-argument call count")?;
        Ok(())
    }

    #[tokio::test]
    async fn sequential_tools_contract_computes_chained_arithmetic() -> Result<(), ScenarioError> {
        let report = sequential_tools(
            MockCompletionModel::new([
                MockTurn::tool_call("add_call", "add", serde_json::json!({"a": 4, "b": 6})),
                MockTurn::tool_call(
                    "multiply_call",
                    "multiply",
                    serde_json::json!({"a": 10, "b": 2}),
                ),
                MockTurn::text("20"),
            ]),
            |builder| builder,
        )
        .await?;
        fixture_contract(report.tool_calls == 2, "sequential call count")?;
        Ok(())
    }

    #[tokio::test]
    async fn tool_choice_mode_contract_accepts_scripted_choices() -> Result<(), ScenarioError> {
        let report = tool_choice_modes(MockCompletionModel::new([
            MockTurn::text("4"),
            MockTurn::tool_call("alpha_call", "alpha", serde_json::json!({"value": 7})),
            MockTurn::tool_call("beta_call", "beta", serde_json::json!({"value": 9})),
        ]))
        .await?;
        fixture_contract(report.tool_calls == 2, "tool-choice call count")?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Scenario contract-failure diagnostics: a misbehaving model must be
    // rejected with a typed ScenarioError, never silently accepted.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn parallel_contract_rejects_non_conforming_batches() {
        // Single-call batch: no assistant turn contains exactly two calls.
        let single = MockTurn::tool_call("only_add", "add", serde_json::json!({"x": 3, "y": 4}));
        assert!(matches!(
            parallel_tools(
                MockCompletionModel::new([single, MockTurn::text("7")]),
                |builder| builder,
                None,
            )
            .await,
            Err(ScenarioError::Contract { .. })
        ));

        // Two-call batch with the wrong tool names.
        let wrong_names = MockTurn::from_contents([
            tool_call("a", "add", serde_json::json!({"x": 3, "y": 4})),
            tool_call("b", "add", serde_json::json!({"x": 1, "y": 1})),
        ])
        .expect("fixture");
        assert!(matches!(
            parallel_tools(
                MockCompletionModel::new([wrong_names, MockTurn::text("done")]),
                |builder| builder,
                None,
            )
            .await,
            Err(ScenarioError::Contract { .. })
        ));

        // Batch whose results are neither 7 nor 8.
        let wrong_results = MockTurn::from_contents([
            tool_call("a", "add", serde_json::json!({"x": 1, "y": 1})),
            tool_call("b", "subtract", serde_json::json!({"x": 1, "y": 1})),
        ])
        .expect("fixture");
        assert!(matches!(
            parallel_tools(
                MockCompletionModel::new([wrong_results, MockTurn::text("done")]),
                |builder| builder,
                None,
            )
            .await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn zero_argument_contract_rejects_models_that_skip_the_tool() {
        // The tool is never invoked: history has no correlated call/result
        // round trip to validate.
        assert!(matches!(
            zero_argument_tool(MockCompletionModel::text("answered directly"), |builder| {
                builder
            },)
            .await,
            Err(ScenarioError::Contract { .. })
        ));

        // Correlated round trip, but the model invoked the tool more than once.
        let twice = MockTurn::from_contents([
            tool_call("ping_1", "ping", serde_json::json!({})),
            tool_call("ping_2", "ping", serde_json::json!({})),
        ])
        .expect("fixture");
        assert!(matches!(
            zero_argument_tool(
                MockCompletionModel::new([twice, MockTurn::text(PING_OUTPUT)]),
                |builder| builder,
            )
            .await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn serialization_contract_rejects_missing_config_call() {
        let first = MockTurn::tool_call("motto_call", "fetch_motto", serde_json::json!({}));
        assert!(matches!(
            tool_output_serialization(
                MockCompletionModel::new([first, MockTurn::text("motto only")]),
                |builder| builder,
            )
            .await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn complex_argument_contract_rejects_wrong_payloads() {
        let wrong = MockTurn::tool_call(
            "profile_call",
            "store_profile",
            serde_json::json!({
                "profile": {"name": "Wrong", "tags": []},
                "mode": "fast",
                "quote": "no"
            }),
        );
        assert!(matches!(
            complex_tool_arguments(
                MockCompletionModel::new([wrong, MockTurn::text("stored")]),
                |builder| builder,
            )
            .await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn hook_rewrite_contract_rejects_models_that_skip_the_tool() {
        // The tool is never invoked, so there are no rewritten arguments to
        // observe.
        assert!(matches!(
            hook_rewrites(MockCompletionModel::text("no tool used"), |builder| {
                builder
            },)
            .await,
            Err(ScenarioError::Contract { .. })
        ));

        // Rewrites are observed, but the extra call breaks the single-call and
        // two-completion-call invariants.
        let twice = MockCompletionModel::new([
            MockTurn::tool_call("hook-add-1", "add", serde_json::json!({"x": 1, "y": 1})),
            MockTurn::tool_call("hook-add-2", "add", serde_json::json!({"x": 1, "y": 1})),
            MockTurn::text("[portable-redacted]"),
        ]);
        assert!(matches!(
            hook_rewrites(twice, |builder| builder).await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn cancellation_contract_rejects_unexpectedly_completing_runs() {
        // The stop hook never fires: the cancelled sub-run answers directly.
        assert!(matches!(
            cancellation_and_max_turns(MockCompletionModel::text("42"), |builder| builder).await,
            Err(ScenarioError::Contract { .. })
        ));
        // Cancellation works, but the one-turn budget completes because the
        // second sub-run's model also answers immediately.
        let mixed = MockCompletionModel::new([
            MockTurn::tool_call("cancel-add", "add", serde_json::json!({"x": 20, "y": 22})),
            MockTurn::text("42"),
        ]);
        assert!(matches!(
            cancellation_and_max_turns(mixed, |builder| builder).await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn streaming_contract_rejects_wrong_final_answers() {
        let model = MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::tool_call(
                    "add_call",
                    "add",
                    serde_json::json!({"a": 17, "b": 25}),
                ),
                MockStreamEvent::FinalResponse(mock_final(usage(10, 2))),
            ],
            vec![
                MockStreamEvent::text("41"),
                MockStreamEvent::FinalResponse(mock_final(usage(14, 1))),
            ],
        ]);
        assert!(matches!(
            streaming_tool(model, |builder| builder).await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn streaming_contract_rejects_leaked_protocol_markers() {
        let model = MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::tool_call(
                    "add_call",
                    "add",
                    serde_json::json!({"a": 17, "b": 25}),
                ),
                MockStreamEvent::FinalResponse(mock_final(usage(10, 2))),
            ],
            vec![
                MockStreamEvent::text("42 <tool_call>"),
                MockStreamEvent::FinalResponse(mock_final(usage(14, 1))),
            ],
        ]);
        assert!(matches!(
            streaming_tool(model, |builder| builder).await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn parity_contract_rejects_disagreeing_surfaces() {
        let model = BufferedAndStreamMock {
            buffered: MockCompletionModel::new([MockTurn::text("London").with_usage(usage(3, 1))]),
            streaming: MockCompletionModel::from_stream_turns([vec![
                MockStreamEvent::text("London"),
                MockStreamEvent::FinalResponse(mock_final(usage(3, 1))),
            ]]),
        };
        assert!(matches!(
            buffered_streaming_text_parity(model).await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn optional_argument_contract_rejects_direct_answers() {
        assert!(matches!(
            optional_argument(MockCompletionModel::text("banana"), |builder| builder).await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn sequential_tools_contract_rejects_direct_answers() {
        assert!(matches!(
            sequential_tools(MockCompletionModel::text("20"), |builder| builder).await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn tool_choice_mode_contract_rejects_violating_choices() {
        // `none` mode must not emit a tool call.
        let none_emits_call = MockCompletionModel::new([MockTurn::tool_call(
            "alpha_call",
            "alpha",
            serde_json::json!({"value": 7}),
        )]);
        assert!(matches!(
            tool_choice_modes(none_emits_call).await,
            Err(ScenarioError::Contract { .. })
        ));

        // `required` mode must emit at least one call.
        let required_silent =
            MockCompletionModel::new([MockTurn::text("4"), MockTurn::text("still no call")]);
        assert!(matches!(
            tool_choice_modes(required_silent).await,
            Err(ScenarioError::Contract { .. })
        ));

        // `specific` mode must select only the named function.
        let specific_wrong_tool = MockCompletionModel::new([
            MockTurn::text("4"),
            MockTurn::tool_call("alpha_call", "alpha", serde_json::json!({"value": 7})),
            MockTurn::tool_call("alpha_call", "alpha", serde_json::json!({"value": 9})),
        ]);
        assert!(matches!(
            tool_choice_modes(specific_wrong_tool).await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn parallel_contract_rejects_protocol_marker_leaks_in_summary() {
        let batch = MockTurn::from_contents([
            tool_call("call_add", "add", serde_json::json!({"x": 3, "y": 4})),
            tool_call(
                "call_subtract",
                "subtract",
                serde_json::json!({"x": 10, "y": 2}),
            ),
        ])
        .expect("fixture");
        assert!(matches!(
            parallel_tools(
                MockCompletionModel::new([batch, MockTurn::text("7 and 8 <tool_call>")]),
                |builder| builder,
                None,
            )
            .await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn parallel_contract_rejects_unexpected_extra_executions() {
        let batch = MockTurn::from_contents([
            tool_call("call_add", "add", serde_json::json!({"x": 3, "y": 4})),
            tool_call(
                "call_subtract",
                "subtract",
                serde_json::json!({"x": 10, "y": 2}),
            ),
        ])
        .expect("fixture");
        assert!(matches!(
            parallel_tools(
                MockCompletionModel::new([
                    batch,
                    MockTurn::tool_call("extra_add", "add", serde_json::json!({"x": 1, "y": 1})),
                    MockTurn::text("done"),
                ]),
                |builder| builder,
                None,
            )
            .await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn buffered_streaming_parity_ignores_non_text_surfaces() -> Result<(), ScenarioError> {
        let buffered_turn = MockTurn::from_contents([
            tool_call("parity_call", "alpha", serde_json::json!({"value": 1})),
            AssistantContent::text("Paris"),
        ])
        .expect("fixture");
        let model = BufferedAndStreamMock {
            buffered: MockCompletionModel::new([buffered_turn.with_usage(usage(3, 1))]),
            streaming: MockCompletionModel::from_stream_turns([vec![
                MockStreamEvent::tool_call("parity_call", "alpha", serde_json::json!({"value": 1})),
                MockStreamEvent::text("Paris"),
                MockStreamEvent::FinalResponse(mock_final(usage(3, 1))),
            ]]),
        };
        let report = buffered_streaming_text_parity(model).await?;
        fixture_contract(
            report.response.contains("Paris"),
            "parity ignores non-text items",
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn parity_contract_rejects_streams_without_final_metadata() {
        let model = BufferedAndStreamMock {
            buffered: MockCompletionModel::new([MockTurn::text("Paris").with_usage(usage(3, 1))]),
            streaming: MockCompletionModel::from_stream_turns([vec![MockStreamEvent::text(
                "Paris",
            )]]),
        };
        assert!(matches!(
            buffered_streaming_text_parity(model).await,
            Err(ScenarioError::Contract { .. })
        ));
    }

    #[tokio::test]
    async fn tool_choice_specific_mode_tolerates_mixed_content() -> Result<(), ScenarioError> {
        let specific_turn = MockTurn::from_contents([
            AssistantContent::text("calling beta"),
            tool_call("beta_call", "beta", serde_json::json!({"value": 9})),
        ])
        .expect("fixture");
        let report = tool_choice_modes(MockCompletionModel::new([
            MockTurn::text("4"),
            MockTurn::tool_call("alpha_call", "alpha", serde_json::json!({"value": 7})),
            specific_turn,
        ]))
        .await?;
        fixture_contract(report.tool_calls == 2, "mixed-content specific choice")?;
        Ok(())
    }
}
