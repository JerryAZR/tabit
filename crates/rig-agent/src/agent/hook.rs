//! Event-specific hooks for observing and steering an agent run.
//!
//! [`AgentHook`] replaces the old universal event/action pair with one lifecycle
//! method and one action type per event. Unsupported combinations are therefore
//! rejected by the compiler instead of being interpreted at runtime.
//! Hooks are independent of the agent's [`CompletionModel`](crate::completion::CompletionModel):
//! managed response events carry canonical Rig messages, content, usage, and
//! message IDs. Use the direct completion or streaming APIs when a hook-like
//! integration needs the provider's typed raw response.
//!
//! Hooks run in registration order through [`HookStack`]. Model selections,
//! tool-call argument rewrites, and tool-result presentation rewrites chain into
//! later hooks; completion-call [`RequestPatch`] values accumulate and merge.
//! A [`ModelTurnAction::Retry`] or stop action short-circuits the remaining
//! hooks for that event. Nested stacks obey the same rules as flat stacks,
//! including preserving an argument rewrite when an inner stack later skips or
//! stops.
//!
//! Register observe-only hooks before steering hooks when every observation is
//! required: a steering stop intentionally prevents later observers from
//! running. Tool-result rewrites change the effective `presentation` sent to
//! the model and recorded as result-content telemetry. The
//! [`ToolResultEvent::raw_result`] and its [`ToolResultEvent::tool_context`]
//! remain unchanged for policy decisions and execution-outcome metadata. A
//! tool-result stop omits result content from telemetry.
//!
//! Blocking and streaming agents share model-turn, request, tool-call, and
//! tool-result resolution. Streaming adds delta-specific observations, but
//! shared lifecycle actions have identical semantics on both surfaces. Streamed
//! deltas are provisional until the model turn is accepted; a retry is surfaced
//! as [`MultiTurnStreamItem::ModelTurnRetried`](crate::agent::MultiTurnStreamItem::ModelTurnRetried)
//! so consumers can discard the rejected turn's deltas.
//!
//! # Example
//!
//! ```
//! use rig_agent::agent::{
//!     AgentHook, CompletionResponseEvent, HookContext, ObservationAction,
//! };
//!
//! struct ResponseLogger;
//!
//! impl AgentHook for ResponseLogger {
//!     async fn on_completion_response(
//!         &self,
//!         _ctx: &HookContext,
//!         event: CompletionResponseEvent<'_>,
//!     ) -> ObservationAction {
//!         println!(
//!             "message {:?}: {:?} ({:?})",
//!             event.message_id, event.content, event.usage
//!         );
//!         ObservationAction::continue_run()
//!     }
//! }
//! ```
//!
//! # Retrying a completed model turn
//!
//! A hook can reject a tool-free turn and either reuse the same prompt and
//! preceding history with fresh request preparation, or preserve the rejected
//! response and append corrective feedback. Retries use the run's existing
//! total model-call budget. A narrower policy limit belongs to the hook and can
//! be stored in the run-scoped [`Scratchpad`]:
//!
//! ```
//! use std::{collections::HashMap, sync::atomic::{AtomicUsize, Ordering}};
//! use rig_agent::agent::{AgentHook, HookContext, ModelTurnAction, ModelTurnFinished};
//! use rig_core::message::AssistantContent;
//!
//! static NEXT_HOOK_ID: AtomicUsize = AtomicUsize::new(1);
//!
//! #[derive(Clone, Default)]
//! struct RetryCounts(HashMap<usize, usize>);
//!
//! struct RetryOnMarker {
//!     id: usize,
//!     max_retries: usize,
//! }
//!
//! impl RetryOnMarker {
//!     fn new(max_retries: usize) -> Self {
//!         Self {
//!             id: NEXT_HOOK_ID.fetch_add(1, Ordering::Relaxed),
//!             max_retries,
//!         }
//!     }
//! }
//!
//! impl AgentHook for RetryOnMarker {
//!     async fn on_model_turn_finished(
//!         &self,
//!         ctx: &HookContext,
//!         event: ModelTurnFinished<'_>,
//!     ) -> ModelTurnAction {
//!         let rejected = event.content.iter().any(|content| {
//!             matches!(content, AssistantContent::Text(text) if text.text.contains("RETRY"))
//!         });
//!         if !rejected {
//!             return ModelTurnAction::continue_run();
//!         }
//!
//!         let attempt = ctx.scratchpad().update::<RetryCounts, _>(|counts| {
//!             let attempt = counts.0.entry(self.id).or_default();
//!             *attempt += 1;
//!             *attempt
//!         });
//!         if attempt <= self.max_retries {
//!             ModelTurnAction::retry_with_feedback("Return a complete answer.")
//!         } else {
//!             ModelTurnAction::stop("response retry limit exceeded")
//!         }
//!     }
//! }
//! # let _hook = RetryOnMarker::new(2);
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{future::Future, sync::Arc, sync::Mutex};

use crate::tool::extensions::TypeMap;
use rig_core::{
    OneOrMany,
    message::{AssistantContent, Message, ToolChoice},
    wasm_compat::{WasmBoxedFuture, WasmCompatSend, WasmCompatSync},
};

use crate::{
    agent::model::ModelHandle,
    completion::{Document, Usage},
    json_utils,
    tool::{ToolContext, ToolOutput, ToolResult},
};

/// Opaque process-scoped identifier for one agent run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunId(String);

impl RunId {
    pub(crate) fn generate() -> Self {
        Self(rig_core::id::generate())
    }

    /// Identifier as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Run-scoped typed storage shared by hooks.
#[derive(Clone, Default)]
pub struct Scratchpad {
    inner: Arc<std::sync::Mutex<TypeMap>>,
}

impl Scratchpad {
    fn lock(&self) -> std::sync::MutexGuard<'_, TypeMap> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Insert a value.
    pub fn insert<T>(&self, value: T) -> Option<T>
    where
        T: Clone + WasmCompatSend + WasmCompatSync + 'static,
    {
        self.lock().insert(value)
    }

    /// Get a cloned value.
    pub fn get<T>(&self) -> Option<T>
    where
        T: Clone + WasmCompatSend + WasmCompatSync + 'static,
    {
        self.lock().get::<T>().cloned()
    }

    /// Whether a type is present.
    pub fn contains<T>(&self) -> bool
    where
        T: WasmCompatSend + WasmCompatSync + 'static,
    {
        self.lock().contains::<T>()
    }

    /// Remove a value.
    pub fn remove<T>(&self) -> Option<T>
    where
        T: Clone + WasmCompatSend + WasmCompatSync + 'static,
    {
        self.lock().remove::<T>()
    }

    /// Atomically update a value, starting at `Default`.
    pub fn update<T, R>(&self, update: impl FnOnce(&mut T) -> R) -> R
    where
        T: Clone + Default + WasmCompatSend + WasmCompatSync + 'static,
    {
        let mut guard = self.lock();
        let mut value = guard.remove::<T>().unwrap_or_default();
        let result = update(&mut value);
        guard.insert(value);
        result
    }
}

impl std::fmt::Debug for Scratchpad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scratchpad")
            .field("entries", &self.lock().len())
            .finish()
    }
}

type ToolCallRewriteFrameMap = HashMap<String, Vec<Option<serde_json::Value>>>;

// A nested `HookStack` can terminate after rewriting arguments, but the public
// action only carries the terminal reason. Resolution frames transfer that
// rewrite across the private erased-hook boundary. Call IDs keep concurrently
// executing tool chains isolated, and the frame stack supports arbitrary nesting.
#[derive(Default)]
struct ToolCallRewriteFrames {
    inner: std::sync::Mutex<ToolCallRewriteFrameMap>,
}

impl ToolCallRewriteFrames {
    fn lock(&self) -> std::sync::MutexGuard<'_, ToolCallRewriteFrameMap> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn begin(&self, internal_call_id: &str) -> ToolCallResolutionFrame<'_> {
        self.lock()
            .entry(internal_call_id.to_owned())
            .or_default()
            .push(None);
        ToolCallResolutionFrame {
            frames: self,
            internal_call_id: internal_call_id.to_owned(),
            active: true,
        }
    }

    fn record(&self, internal_call_id: &str, rewrite: serde_json::Value) {
        if let Some(frame) = self
            .lock()
            .get_mut(internal_call_id)
            .and_then(|frames| frames.last_mut())
        {
            *frame = Some(rewrite);
        }
    }

    fn finish(&self, internal_call_id: &str) -> Option<serde_json::Value> {
        let mut frames = self.lock();
        let (rewrite, remove_entry) = frames
            .get_mut(internal_call_id)
            .map(|frames| {
                let rewrite = frames.pop().flatten();
                (rewrite, frames.is_empty())
            })
            .unwrap_or((None, false));
        if remove_entry {
            frames.remove(internal_call_id);
        }
        rewrite
    }
}

impl std::fmt::Debug for ToolCallRewriteFrames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallRewriteFrames")
            .finish_non_exhaustive()
    }
}

struct ToolCallResolutionFrame<'a> {
    frames: &'a ToolCallRewriteFrames,
    internal_call_id: String,
    active: bool,
}

impl ToolCallResolutionFrame<'_> {
    fn finish(mut self) -> Option<serde_json::Value> {
        self.active = false;
        self.frames.finish(&self.internal_call_id)
    }
}

impl Drop for ToolCallResolutionFrame<'_> {
    fn drop(&mut self) {
        if self.active {
            self.frames.finish(&self.internal_call_id);
        }
    }
}

/// Run-scoped context supplied to hooks.
#[derive(Debug)]
pub struct HookContext {
    run_id: RunId,
    turn: AtomicUsize,
    /// The announced id of the turn in flight (ENGINE.md behavior delta
    /// 10): set when a model-call attempt commits, read by hooks for the
    /// rest of that attempt. `None` only before the first attempt or on
    /// surfaces that never announce.
    turn_id: Mutex<Option<String>>,
    is_streaming: bool,
    agent_name: Option<String>,
    scratchpad: Scratchpad,
    tool_call_rewrite_frames: ToolCallRewriteFrames,
}

impl HookContext {
    pub(crate) fn new(is_streaming: bool, agent_name: Option<String>) -> Self {
        Self {
            run_id: RunId::generate(),
            turn: AtomicUsize::new(0),
            turn_id: Mutex::new(None),
            is_streaming,
            agent_name,
            scratchpad: Scratchpad::default(),
            tool_call_rewrite_frames: ToolCallRewriteFrames::default(),
        }
    }

    pub(crate) fn set_turn(&self, turn: usize) {
        self.turn.store(turn, Ordering::Relaxed);
    }

    pub(crate) fn set_turn_id(&self, id: String) {
        *self
            .turn_id
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(id);
    }

    /// Stable run identifier.
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Current one-based model-call index.
    pub fn turn(&self) -> usize {
        self.turn.load(Ordering::Relaxed)
    }

    /// The announced id of the turn in flight, when one has been
    /// announced. Stable for the whole attempt; a retried attempt
    /// announces a fresh id (ids are never reused).
    pub fn turn_id(&self) -> Option<String> {
        self.turn_id
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Whether the streaming surface is driving this run.
    pub fn is_streaming(&self) -> bool {
        self.is_streaming
    }

    /// Configured agent name.
    pub fn agent_name(&self) -> Option<&str> {
        self.agent_name.as_deref()
    }

    /// Shared run scratchpad.
    pub fn scratchpad(&self) -> &Scratchpad {
        &self.scratchpad
    }

    fn begin_tool_call_resolution(&self, internal_call_id: &str) -> ToolCallResolutionFrame<'_> {
        self.tool_call_rewrite_frames.begin(internal_call_id)
    }

    fn record_tool_call_rewrite(&self, internal_call_id: &str, rewrite: serde_json::Value) {
        self.tool_call_rewrite_frames
            .record(internal_call_id, rewrite);
    }
}

/// Completion-call event.
///
/// Per `CallModel` step, hook resolution is ordered: completion-call hooks run
/// **first** and their [`RequestPatch`]es merge in registration order. Only
/// when every completion-call hook proceeds does [`ModelSelection`] run
/// (receiving the merged patch), after which request preparation inspects the
/// selected model's captured
/// [`ProviderCapabilities`](crate::completion::ProviderCapabilities) and the
/// attempt is issued. A completion-call stop therefore suppresses model
/// selection entirely and does not advance
/// [`ModelSelection::previous_model`].
#[derive(Clone, Copy)]
pub struct CompletionCall<'a> {
    /// Prompt for this turn.
    pub prompt: &'a Message,
    /// History preceding the prompt.
    pub history: &'a [Message],
    /// One-based model-call index.
    pub turn: usize,
}

/// Model-selection event resolved after completion-call hooks and before
/// request preparation.
///
/// The runner default is the first candidate. A [`HookStack`] threads every
/// [`ModelSelectionAction::Select`] into later hooks in registration order, so
/// `selected_model` always reflects all earlier decisions for this event.
///
/// Ordering per `CallModel` step: completion-call hooks resolve first; only if
/// they proceed does this event fire, carrying the merged [`RequestPatch`] in
/// [`request_patch`](Self::request_patch); only after selection resolves does
/// request preparation run against the selected model's captured
/// [`ProviderCapabilities`](crate::completion::ProviderCapabilities), and only
/// then is the attempt issued. Selection therefore runs once per `CallModel`
/// step whose completion-call hooks proceed — including model-turn retries and
/// post-tool calls — and never after a completion-call stop.
///
/// Selection is synchronous, local, and non-blocking: a hook may read and
/// write the run [`Scratchpad`], but must not perform blocking I/O. In-flight
/// attempts never rebind — the selected handle is cloned into the prepared
/// attempt and executes it to completion.
///
/// `previous_model` reflects **issued attempts** only: it advances immediately
/// before the selected model's unary or streaming operation is invoked, so a
/// provider attempt that returns an error still counts, while a
/// completion-call stop, a selection stop, or a request-preparation failure
/// does not. An extraction or run default set via `using_model(...)` is the
/// default candidate for every retry, not a hard pin: selection hooks may
/// override it on each retry.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct ModelSelection<'a> {
    /// Prompt for the pending model call.
    pub prompt: &'a Message,
    /// Canonical history visible to the pending model call.
    pub history: &'a [Message],
    /// Merged per-turn request patch from this step's completion-call hooks
    /// (in hook registration order), when any hook patched the request.
    pub request_patch: Option<&'a RequestPatch>,
    /// Model that executed the preceding issued attempt in this run, if any.
    pub previous_model: Option<&'a ModelHandle>,
    /// Runner default used as the initial candidate for this call.
    pub default_model: &'a ModelHandle,
    /// Candidate after all earlier model-selection hooks.
    pub selected_model: &'a ModelHandle,
}

impl<'a> ModelSelection<'a> {
    /// Construct a `ModelSelection` event from its parts.
    ///
    /// The struct is `#[non_exhaustive]`, so external code cannot build it
    /// with a struct literal; this constructor exists so that custom
    /// model-selection routers can be unit-tested outside this crate.
    pub fn new(
        prompt: &'a Message,
        history: &'a [Message],
        request_patch: Option<&'a RequestPatch>,
        previous_model: Option<&'a ModelHandle>,
        default_model: &'a ModelHandle,
        selected_model: &'a ModelHandle,
    ) -> Self {
        Self {
            prompt,
            history,
            request_patch,
            previous_model,
            default_model,
            selected_model,
        }
    }
}

/// Canonical non-streaming completion response event.
#[derive(Clone, Copy)]
pub struct CompletionResponse<'a> {
    /// Prompt sent for this turn.
    pub prompt: &'a Message,
    /// Canonical assistant content returned for this turn.
    pub content: &'a OneOrMany<AssistantContent>,
    /// Usage reported for this turn.
    pub usage: Usage,
    /// Provider-assigned message ID, when available.
    pub message_id: Option<&'a str>,
}

/// Medium-neutral accepted model-turn event.
///
/// The turn is canonicalized and parked in the run state, but has not yet been
/// advanced into tool execution or finalization. A hook may therefore reject a
/// tool-free turn with [`ModelTurnAction::Retry`].
#[derive(Clone, Copy)]
pub struct ModelTurnFinished<'a> {
    /// One-based model-call index.
    pub turn: usize,
    /// Canonical assistant content parked for hook acceptance.
    pub content: &'a OneOrMany<AssistantContent>,
    /// Usage reported for the turn.
    pub usage: Usage,
}

/// How an accepted, tool-free model turn should be retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryRequest {
    /// Discard the rejected response and reuse the same prompt and preceding
    /// history with fresh request preparation.
    ///
    /// Completion-call hooks, retrieval, and dynamic tool resolution run again,
    /// so the resulting provider request may differ from the rejected attempt.
    Repeat,
    /// Preserve the rejected assistant response and append corrective feedback.
    Feedback(String),
}

/// Action for the medium-neutral [`ModelTurnFinished`] event.
///
/// Every retry consumes the run's existing total model-call budget. Rig does
/// not impose a separate response-retry limit; hooks that need one should keep
/// run-scoped state in [`HookContext::scratchpad`]. Retrying a turn containing
/// tool calls is rejected so provider-visible history never contains unanswered
/// calls. Use tool-call hooks to steer those turns instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelTurnAction {
    /// Accept the turn and continue the run.
    Continue,
    /// Reject the turn and request another model call.
    Retry(RetryRequest),
    /// Stop the run with a reason.
    Stop(String),
}

impl ModelTurnAction {
    /// Accepts the completed model turn.
    pub fn continue_run() -> Self {
        Self::Continue
    }

    /// Discards the response and reuses the same prompt and preceding history
    /// with fresh request preparation.
    pub fn repeat() -> Self {
        Self::Retry(RetryRequest::Repeat)
    }

    /// Preserves the response, appends corrective feedback, and retries.
    pub fn retry_with_feedback(feedback: impl Into<String>) -> Self {
        Self::Retry(RetryRequest::Feedback(feedback.into()))
    }

    /// Stops the run with the supplied reason.
    pub fn stop(reason: impl Into<String>) -> Self {
        Self::Stop(reason.into())
    }
}

/// Pre-execution tool event.
#[derive(Clone, Copy)]
pub struct ToolCall<'a> {
    /// Tool name.
    pub tool_name: &'a str,
    /// Provider tool-call id.
    pub tool_call_id: Option<&'a str>,
    /// Rig correlation id.
    pub internal_call_id: &'a str,
    /// Effective JSON arguments, including earlier rewrites.
    pub args: &'a str,
}

/// Post-execution tool event.
///
/// `presentation` contains the running presentation rewrite. `raw_result` and
/// `tool_context` always contain the original execution data.
#[derive(Clone, Copy)]
pub struct ToolResultEvent<'a> {
    /// Tool name.
    pub tool_name: &'a str,
    /// Provider tool-call id.
    pub tool_call_id: Option<&'a str>,
    /// Rig correlation id.
    pub internal_call_id: &'a str,
    /// Effective arguments used for execution.
    pub args: &'a str,
    /// Current model-visible presentation, including earlier rewrites.
    pub presentation: &'a ToolOutput,
    /// Immutable raw execution result.
    pub raw_result: &'a ToolResult,
    /// Per-dispatch context containing inbound data and result metadata.
    pub tool_context: &'a ToolContext,
}

/// Streaming text delta.
#[derive(Clone, Copy)]
pub struct TextDelta<'a> {
    /// Newly received text.
    pub delta: &'a str,
    /// Text accumulated for the turn.
    pub aggregated: &'a str,
}

/// Streaming tool-call delta.
#[derive(Clone, Copy)]
pub struct ToolCallDelta<'a> {
    /// Provider tool-call id.
    pub tool_call_id: &'a str,
    /// Rig correlation id.
    pub internal_call_id: &'a str,
    /// Tool name on the first delta.
    pub tool_name: Option<&'a str>,
    /// Newly received argument fragment.
    pub delta: &'a str,
}

/// Canonical streaming response-finish event.
#[derive(Clone, Copy)]
pub struct StreamResponseFinish<'a> {
    /// Prompt sent for this turn.
    pub prompt: &'a Message,
    /// Canonical assistant content aggregated for this turn.
    pub content: &'a OneOrMany<AssistantContent>,
    /// Usage reported for this turn.
    pub usage: Usage,
    /// Provider-assigned message ID, when available.
    pub message_id: Option<&'a str>,
}

/// Hook event kind used only as an observation performance hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StepEventKind {
    CompletionCall,
    CompletionResponse,
    ModelTurnFinished,
    ToolCall,
    ToolResult,
    TextDelta,
    ToolCallDelta,
    StreamResponseFinish,
}

/// A non-sticky patch applied only to the current turn's completion request.
///
/// A [`HookStack`] merges patches in hook registration order according to these
/// rules:
///
/// - `extra_context` documents are appended in order.
/// - JSON-object `additional_params` values are shallow-merged, with later
///   top-level keys winning; a later non-object value replaces an earlier value.
/// - `active_tools` allow-lists are intersected.
/// - Scalar fields and `history` use last-writer-wins semantics, with a warning
///   when multiple hooks set the same field.
///
/// The merged patch does not mutate the agent's configured baseline and is not
/// carried into subsequent turns.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct RequestPatch {
    /// Preamble to use instead of the agent's configured preamble for this turn.
    pub preamble: Option<String>,
    /// Sampling temperature to use for this turn.
    pub temperature: Option<f64>,
    /// Maximum output-token count to use for this turn.
    pub max_tokens: Option<u64>,
    /// Tool-choice policy to use for this turn.
    pub tool_choice: Option<ToolChoice>,
    /// Allow-list used to narrow the tools advertised for this turn.
    pub active_tools: Option<Vec<String>>,
    /// Provider-specific request parameters to apply for this turn.
    pub additional_params: Option<serde_json::Value>,
    /// Context documents appended to the request for this turn.
    pub extra_context: Vec<Document>,
    /// Conversation history to use instead of the current history for this turn.
    pub history: Option<Vec<Message>>,
}

fn merge_last_wins<T>(earlier: Option<T>, later: Option<T>, field: &str) -> Option<T> {
    match (earlier, later) {
        (Some(_), Some(later)) => {
            tracing::warn!(
                patch_field = field,
                "two hooks set the same request field; later wins"
            );
            Some(later)
        }
        (earlier, later) => later.or(earlier),
    }
}

impl RequestPatch {
    /// Creates an empty request patch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the agent's configured preamble for this turn.
    pub fn preamble(mut self, value: impl Into<String>) -> Self {
        self.preamble = Some(value.into());
        self
    }

    /// Sets the sampling temperature for this turn.
    pub fn temperature(mut self, value: f64) -> Self {
        self.temperature = Some(value);
        self
    }

    /// Sets the maximum output-token count for this turn.
    pub fn max_tokens(mut self, value: u64) -> Self {
        self.max_tokens = Some(value);
        self
    }

    /// Sets the tool-choice policy for this turn.
    pub fn tool_choice(mut self, value: ToolChoice) -> Self {
        self.tool_choice = Some(value);
        self
    }

    /// Sets the allow-list used to narrow the tools advertised for this turn.
    pub fn active_tools<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.active_tools = Some(values.into_iter().map(Into::into).collect());
        self
    }

    /// Sets provider-specific request parameters for this turn.
    ///
    /// When multiple patches provide JSON objects, their top-level keys are
    /// shallow-merged and values from later hooks win.
    pub fn additional_params(mut self, value: serde_json::Value) -> Self {
        self.additional_params = Some(value);
        self
    }

    /// Appends context documents to the request for this turn.
    pub fn extra_context<I>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = Document>,
    {
        self.extra_context.extend(values);
        self
    }

    /// Appends one context document to the request for this turn.
    pub fn context(mut self, value: Document) -> Self {
        self.extra_context.push(value);
        self
    }

    /// Replaces the conversation history for this turn.
    pub fn history<I>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = Message>,
    {
        self.history = Some(values.into_iter().collect());
        self
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.preamble.is_none()
            && self.temperature.is_none()
            && self.max_tokens.is_none()
            && self.tool_choice.is_none()
            && self.active_tools.is_none()
            && self.additional_params.is_none()
            && self.extra_context.is_empty()
            && self.history.is_none()
    }

    pub(crate) fn merge(mut self, later: Self) -> Self {
        self.extra_context.extend(later.extra_context);
        self.additional_params = match (self.additional_params.take(), later.additional_params) {
            (Some(base), Some(patch)) if base.is_object() && patch.is_object() => {
                Some(json_utils::merge(base, patch))
            }
            (base, patch) => patch.or(base),
        };
        self.preamble = merge_last_wins(self.preamble, later.preamble, "preamble");
        self.temperature = merge_last_wins(self.temperature, later.temperature, "temperature");
        self.max_tokens = merge_last_wins(self.max_tokens, later.max_tokens, "max_tokens");
        self.tool_choice = merge_last_wins(self.tool_choice, later.tool_choice, "tool_choice");
        self.history = merge_last_wins(self.history, later.history, "history");
        self.active_tools = match (self.active_tools.take(), later.active_tools) {
            (Some(earlier), Some(later)) => {
                let later: std::collections::BTreeSet<_> = later.iter().collect();
                Some(
                    earlier
                        .into_iter()
                        .filter(|name| later.contains(name))
                        .collect(),
                )
            }
            (earlier, later) => earlier.or(later),
        };
        self
    }
}

/// Action for model-selection hooks.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ModelSelectionAction {
    /// Keep the candidate supplied to this hook.
    Continue,
    /// Replace the candidate and pass it to later hooks.
    Select(ModelHandle),
    /// Stop the run before request preparation or model execution.
    Stop(String),
}

impl ModelSelectionAction {
    /// Keeps the current model candidate.
    pub fn continue_run() -> Self {
        Self::Continue
    }

    /// Selects `model` and passes it to later hooks.
    pub fn select(model: ModelHandle) -> Self {
        Self::Select(model)
    }

    /// Stops the run before the pending model attempt.
    ///
    /// A selection stop happens before the attempt is issued, so it does not
    /// advance [`ModelSelection::previous_model`].
    pub fn stop(reason: impl Into<String>) -> Self {
        Self::Stop(reason.into())
    }
}

/// Action for completion-call hooks.
#[derive(Debug, Clone, PartialEq)]
pub enum CompletionCallAction {
    /// Send the baseline request.
    Continue,
    /// Merge this per-turn patch into the request.
    Patch(RequestPatch),
    /// Stop the run with a reason.
    Stop(String),
}

impl CompletionCallAction {
    /// Creates an action that sends the request without adding a patch.
    pub fn continue_run() -> Self {
        Self::Continue
    }

    /// Creates an action that applies a per-turn request patch.
    pub fn patch(patch: RequestPatch) -> Self {
        Self::Patch(patch)
    }

    /// Creates an action that stops the run with the supplied reason.
    pub fn stop(reason: impl Into<String>) -> Self {
        Self::Stop(reason.into())
    }
}

/// Action for pre-tool hooks. There is deliberately no stop variant:
/// nothing may kill a batch (ENGINE.md, stop taxonomy) — a hook that
/// wants this call not to run skips it, and one that wants the run
/// over now holds the abort leaf.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallAction {
    /// Execute with the current arguments.
    Run,
    /// Execute with replacement arguments.
    Rewrite(serde_json::Value),
    /// Do not execute; return this feedback to the model.
    Skip(String),
}

impl ToolCallAction {
    /// Creates an action that executes the tool with the current arguments.
    pub fn run() -> Self {
        Self::Run
    }

    /// Creates an action that replaces the arguments passed to the tool.
    pub fn rewrite(args: impl Into<serde_json::Value>) -> Self {
        Self::Rewrite(args.into())
    }

    /// Serializes replacement arguments and creates a rewrite action.
    ///
    /// Returns an error when `args` cannot be represented as JSON.
    pub fn try_rewrite<T: serde::Serialize>(args: &T) -> Result<Self, serde_json::Error> {
        Ok(Self::Rewrite(serde_json::to_value(args)?))
    }

    /// Creates an action that skips execution and returns feedback to the model.
    pub fn skip(reason: impl Into<String>) -> Self {
        Self::Skip(reason.into())
    }
}

/// Action for post-tool hooks.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolResultAction {
    /// Keep the current presentation.
    Keep,
    /// Replace the effective presentation sent to the model and result-content
    /// telemetry.
    Rewrite(ToolOutput),
    /// Do not continue the run after this batch. The current batch is
    /// unaffected — chains not yet started still run and every result
    /// commits; the reason is fed to the machine at settle and the run
    /// ends `failed(reason)` at the decision (ENGINE.md, stop taxonomy).
    Stop(String),
}

impl ToolResultAction {
    /// Creates an action that preserves the current model-visible presentation.
    pub fn keep() -> Self {
        Self::Keep
    }

    /// Creates an action that replaces the effective presentation sent to the
    /// model and result-content telemetry.
    ///
    /// The tool's raw structured result remains unchanged.
    pub fn rewrite(result: impl Into<String>) -> Self {
        Self::Rewrite(ToolOutput::text(result))
    }

    /// Creates an action that replaces the effective model and telemetry
    /// presentation with explicit structured or multimodal output.
    pub fn rewrite_output(output: ToolOutput) -> Self {
        Self::Rewrite(output)
    }

    /// Creates an action that ends the run after the current batch
    /// settles (see [`ToolResultAction::Stop`]).
    pub fn stop(reason: impl Into<String>) -> Self {
        Self::Stop(reason.into())
    }
}

/// Action for observe-only lifecycle events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationAction {
    /// Continue the run.
    Continue,
    /// Stop the run.
    Stop(String),
}

impl ObservationAction {
    /// Creates an action that continues the run.
    pub fn continue_run() -> Self {
        Self::Continue
    }

    /// Creates an action that stops the run with the supplied reason.
    pub fn stop(reason: impl Into<String>) -> Self {
        Self::Stop(reason.into())
    }
}

/// Per-run lifecycle observer and steerer.
pub trait AgentHook: WasmCompatSend + WasmCompatSync {
    /// Selects the model for the pending model-call boundary.
    ///
    /// Selection is synchronous, local, and non-blocking: it operates only on
    /// already-constructed [`ModelHandle`] values and may read or write the
    /// run [`Scratchpad`], but must not perform blocking I/O. It runs once per
    /// `CallModel` step whose completion-call hooks proceed — including
    /// retries and post-tool calls — never after a completion-call stop, and
    /// in-flight attempts never rebind. In a [`HookStack`], selections are
    /// passed to later hooks in registration order; the last selection wins
    /// and a stop is terminal. The default action keeps the current candidate.
    /// See [`ModelSelection`] for the full ordering contract.
    fn on_model_select(
        &self,
        _ctx: &HookContext,
        _event: ModelSelection<'_>,
    ) -> ModelSelectionAction {
        ModelSelectionAction::Continue
    }

    /// Runs before a completion request is sent.
    ///
    /// Return a per-turn patch, continue without one, or stop the run. Patches
    /// from a [`HookStack`] are merged in hook registration order.
    fn on_completion_call(
        &self,
        _ctx: &HookContext,
        _event: CompletionCall<'_>,
    ) -> impl Future<Output = CompletionCallAction> + WasmCompatSend {
        async { CompletionCallAction::Continue }
    }

    /// Observes a completed model response.
    ///
    /// The default action continues the run.
    fn on_completion_response(
        &self,
        _ctx: &HookContext,
        _event: CompletionResponse<'_>,
    ) -> impl Future<Output = ObservationAction> + WasmCompatSend {
        async { ObservationAction::Continue }
    }

    /// Observes or rejects the content produced at the end of a model turn.
    ///
    /// A retry is valid only for a tool-free turn and consumes the existing
    /// total model-call budget. The default action accepts the turn.
    fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        _event: ModelTurnFinished<'_>,
    ) -> impl Future<Output = ModelTurnAction> + WasmCompatSend {
        async { ModelTurnAction::Continue }
    }

    /// Resolves a model-emitted tool call before its body runs: the gate
    /// seam (permission, rewrites, skips).
    ///
    /// The hook may rewrite the current arguments or skip execution (the
    /// model sees an in-band skip result, never a silent drop). Rewrites in
    /// a [`HookStack`] are passed to subsequent hooks. A hook that wants
    /// the run over holds the abort leaf — nothing a hook does here may
    /// stop or kill the tool batch (ENGINE.md's stop taxonomy: a batch is
    /// sealed; only `on_tool_result` may set the don't-continue flag, read
    /// after the batch settles). The default action executes the call as
    /// written.
    fn on_tool_call(
        &self,
        _ctx: &HookContext,
        _event: ToolCall<'_>,
    ) -> impl Future<Output = ToolCallAction> + WasmCompatSend {
        async { ToolCallAction::Run }
    }

    /// Runs after a tool call resolves and before its presentation is sent to the model.
    ///
    /// This includes framework-skipped calls whose tool body did not execute.
    /// Rewrites affect the model-visible presentation and result-content
    /// telemetry, but not the raw structured result or execution-outcome
    /// metadata. A stop omits result content from telemetry. The default action
    /// keeps the current presentation.
    fn on_tool_result(
        &self,
        _ctx: &HookContext,
        _event: ToolResultEvent<'_>,
    ) -> impl Future<Output = ToolResultAction> + WasmCompatSend {
        async { ToolResultAction::Keep }
    }

    /// Observes a text delta from a streaming response.
    ///
    /// The default action continues the run.
    fn on_text_delta(
        &self,
        _ctx: &HookContext,
        _event: TextDelta<'_>,
    ) -> impl Future<Output = ObservationAction> + WasmCompatSend {
        async { ObservationAction::Continue }
    }

    /// Observes an argument delta for a streaming tool call.
    ///
    /// The default action continues the run.
    fn on_tool_call_delta(
        &self,
        _ctx: &HookContext,
        _event: ToolCallDelta<'_>,
    ) -> impl Future<Output = ObservationAction> + WasmCompatSend {
        async { ObservationAction::Continue }
    }

    /// Observes a completed streaming response in canonical Rig form.
    ///
    /// The default action continues the run.
    fn on_stream_response_finish(
        &self,
        _ctx: &HookContext,
        _event: StreamResponseFinish<'_>,
    ) -> impl Future<Output = ObservationAction> + WasmCompatSend {
        async { ObservationAction::Continue }
    }

    /// Observation interest hint, primarily for high-frequency deltas.
    fn observes(&self, _kind: StepEventKind) -> bool {
        true
    }
}

impl AgentHook for () {
    fn observes(&self, _kind: StepEventKind) -> bool {
        false
    }
}

trait DynAgentHook: WasmCompatSend + WasmCompatSync {
    fn model_select(&self, ctx: &HookContext, event: ModelSelection<'_>) -> ModelSelectionAction;
    fn completion_call<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: CompletionCall<'a>,
    ) -> WasmBoxedFuture<'a, CompletionCallAction>;
    fn completion_response<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: CompletionResponse<'a>,
    ) -> WasmBoxedFuture<'a, ObservationAction>;
    fn model_turn_finished<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: ModelTurnFinished<'a>,
    ) -> WasmBoxedFuture<'a, ModelTurnAction>;
    fn tool_call<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: ToolCall<'a>,
    ) -> WasmBoxedFuture<'a, (ToolCallAction, Option<serde_json::Value>)>;
    fn tool_result<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: ToolResultEvent<'a>,
    ) -> WasmBoxedFuture<'a, ToolResultAction>;
    fn text_delta<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: TextDelta<'a>,
    ) -> WasmBoxedFuture<'a, ObservationAction>;
    fn tool_call_delta<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: ToolCallDelta<'a>,
    ) -> WasmBoxedFuture<'a, ObservationAction>;
    fn stream_response_finish<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: StreamResponseFinish<'a>,
    ) -> WasmBoxedFuture<'a, ObservationAction>;
    fn observes(&self, kind: StepEventKind) -> bool;
}

impl<H> DynAgentHook for H
where
    H: AgentHook,
{
    fn model_select(&self, ctx: &HookContext, event: ModelSelection<'_>) -> ModelSelectionAction {
        self.on_model_select(ctx, event)
    }

    fn completion_call<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: CompletionCall<'a>,
    ) -> WasmBoxedFuture<'a, CompletionCallAction> {
        Box::pin(self.on_completion_call(ctx, event))
    }
    fn completion_response<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: CompletionResponse<'a>,
    ) -> WasmBoxedFuture<'a, ObservationAction> {
        Box::pin(self.on_completion_response(ctx, event))
    }
    fn model_turn_finished<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: ModelTurnFinished<'a>,
    ) -> WasmBoxedFuture<'a, ModelTurnAction> {
        Box::pin(self.on_model_turn_finished(ctx, event))
    }
    fn tool_call<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: ToolCall<'a>,
    ) -> WasmBoxedFuture<'a, (ToolCallAction, Option<serde_json::Value>)> {
        Box::pin(async move {
            // Only `on_tool_call` is public dispatch. A nested `HookStack`
            // records terminal-path rewrite state into this private frame.
            let frame = ctx.begin_tool_call_resolution(event.internal_call_id);
            let action = self.on_tool_call(ctx, event).await;
            (action, frame.finish())
        })
    }
    fn tool_result<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: ToolResultEvent<'a>,
    ) -> WasmBoxedFuture<'a, ToolResultAction> {
        Box::pin(self.on_tool_result(ctx, event))
    }
    fn text_delta<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: TextDelta<'a>,
    ) -> WasmBoxedFuture<'a, ObservationAction> {
        Box::pin(self.on_text_delta(ctx, event))
    }
    fn tool_call_delta<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: ToolCallDelta<'a>,
    ) -> WasmBoxedFuture<'a, ObservationAction> {
        Box::pin(self.on_tool_call_delta(ctx, event))
    }
    fn stream_response_finish<'a>(
        &'a self,
        ctx: &'a HookContext,
        event: StreamResponseFinish<'a>,
    ) -> WasmBoxedFuture<'a, ObservationAction> {
        Box::pin(self.on_stream_response_finish(ctx, event))
    }
    fn observes(&self, kind: StepEventKind) -> bool {
        AgentHook::observes(self, kind)
    }
}

/// Ordered composable hook stack.
///
/// Model selections chain in registration order: each hook sees the candidate
/// selected by earlier hooks, the last selection wins, and a stop is terminal.
/// Nested stacks preserve the same composition semantics.
#[derive(Clone, Default)]
pub struct HookStack {
    hooks: Vec<Arc<dyn DynAgentHook>>,
}

impl std::fmt::Debug for HookStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookStack")
            .field("len", &self.hooks.len())
            .finish()
    }
}

impl HookStack {
    /// Creates an empty hook stack.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a hook stack containing `hook`.
    pub fn with<H: AgentHook + 'static>(hook: H) -> Self {
        let mut stack = Self::new();
        stack.push(hook);
        stack
    }

    /// Appends a hook to the end of the stack's registration order.
    pub fn push<H: AgentHook + 'static>(&mut self, hook: H) {
        self.hooks.push(Arc::new(hook));
    }

    /// Returns `true` when the stack contains no hooks.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Returns the number of hooks in the stack.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Resolve the hook chain while retaining a rewrite accumulated before a
    /// terminal action so the runner can report the effective arguments.
    pub(crate) async fn resolve_tool_call(
        &self,
        ctx: &HookContext,
        event: ToolCall<'_>,
    ) -> (ToolCallAction, Option<serde_json::Value>) {
        let mut effective = None;
        for hook in &self.hooks {
            let rewritten = effective.as_ref().map(json_utils::serialize_json_value);
            let current = ToolCall {
                args: rewritten.as_deref().unwrap_or(event.args),
                ..event
            };
            let (action, salvaged) = hook.tool_call(ctx, current).await;
            if let Some(value) = salvaged {
                effective = Some(value);
            }
            match action {
                ToolCallAction::Run => {}
                ToolCallAction::Rewrite(value) => effective = Some(value),
                other => return (other, effective),
            }
        }
        match effective {
            Some(value) => (ToolCallAction::Rewrite(value), None),
            None => (ToolCallAction::Run, None),
        }
    }
}

async fn first_stop<I>(futures: I) -> ObservationAction
where
    I: IntoIterator<Item = ObservationAction>,
{
    for action in futures {
        if !matches!(action, ObservationAction::Continue) {
            return action;
        }
    }
    ObservationAction::Continue
}

impl AgentHook for HookStack {
    fn on_model_select(
        &self,
        ctx: &HookContext,
        event: ModelSelection<'_>,
    ) -> ModelSelectionAction {
        let mut selected = None;
        for hook in &self.hooks {
            let action = {
                let selected_model = selected.as_ref().unwrap_or(event.selected_model);
                hook.model_select(
                    ctx,
                    ModelSelection {
                        selected_model,
                        ..event
                    },
                )
            };
            match action {
                ModelSelectionAction::Continue => {}
                ModelSelectionAction::Select(model) => selected = Some(model),
                stop @ ModelSelectionAction::Stop(_) => return stop,
            }
        }
        selected.map_or(ModelSelectionAction::Continue, ModelSelectionAction::Select)
    }

    async fn on_completion_call(
        &self,
        ctx: &HookContext,
        event: CompletionCall<'_>,
    ) -> CompletionCallAction {
        let mut merged: Option<RequestPatch> = None;
        for hook in &self.hooks {
            match hook.completion_call(ctx, event).await {
                CompletionCallAction::Continue => {}
                CompletionCallAction::Patch(patch) => {
                    merged = Some(merged.map_or(patch.clone(), |value| value.merge(patch)))
                }
                stop @ CompletionCallAction::Stop(_) => return stop,
            }
        }
        match merged {
            Some(patch) if !patch.is_empty() => CompletionCallAction::Patch(patch),
            _ => CompletionCallAction::Continue,
        }
    }

    async fn on_completion_response(
        &self,
        ctx: &HookContext,
        event: CompletionResponse<'_>,
    ) -> ObservationAction {
        let mut actions = Vec::new();
        for hook in &self.hooks {
            let action = hook.completion_response(ctx, event).await;
            let stop = !matches!(action, ObservationAction::Continue);
            actions.push(action);
            if stop {
                break;
            }
        }
        first_stop(actions).await
    }
    async fn on_model_turn_finished(
        &self,
        ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        for hook in &self.hooks {
            let action = hook.model_turn_finished(ctx, event).await;
            if !matches!(action, ModelTurnAction::Continue) {
                return action;
            }
        }
        ModelTurnAction::Continue
    }
    async fn on_tool_call(&self, ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        let internal_call_id = event.internal_call_id;
        let (action, salvaged) = self.resolve_tool_call(ctx, event).await;
        // This is a no-op for direct calls. Under private erased dispatch it
        // returns a nested stack's terminal-path rewrite to its parent stack.
        if let Some(rewrite) = salvaged {
            ctx.record_tool_call_rewrite(internal_call_id, rewrite);
        }
        action
    }
    async fn on_tool_result(
        &self,
        ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        let mut effective: Option<ToolOutput> = None;
        for hook in &self.hooks {
            let current = ToolResultEvent {
                presentation: effective.as_ref().unwrap_or(event.presentation),
                ..event
            };
            match hook.tool_result(ctx, current).await {
                ToolResultAction::Keep => {}
                ToolResultAction::Rewrite(value) => effective = Some(value),
                stop @ ToolResultAction::Stop(_) => return stop,
            }
        }
        effective.map_or(ToolResultAction::Keep, ToolResultAction::Rewrite)
    }
    async fn on_text_delta(&self, ctx: &HookContext, event: TextDelta<'_>) -> ObservationAction {
        for hook in &self.hooks {
            let action = hook.text_delta(ctx, event).await;
            if !matches!(action, ObservationAction::Continue) {
                return action;
            }
        }
        ObservationAction::Continue
    }
    async fn on_tool_call_delta(
        &self,
        ctx: &HookContext,
        event: ToolCallDelta<'_>,
    ) -> ObservationAction {
        for hook in &self.hooks {
            let action = hook.tool_call_delta(ctx, event).await;
            if !matches!(action, ObservationAction::Continue) {
                return action;
            }
        }
        ObservationAction::Continue
    }
    async fn on_stream_response_finish(
        &self,
        ctx: &HookContext,
        event: StreamResponseFinish<'_>,
    ) -> ObservationAction {
        for hook in &self.hooks {
            let action = hook.stream_response_finish(ctx, event).await;
            if !matches!(action, ObservationAction::Continue) {
                return action;
            }
        }
        ObservationAction::Continue
    }
    fn observes(&self, kind: StepEventKind) -> bool {
        self.hooks.iter().any(|hook| hook.observes(kind))
    }
}

#[cfg(test)]
#[path = "hook_tests.rs"]
mod hook_tests;
