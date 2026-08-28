//! Event hooks for gating and shaping an agent run's tool phase.
//!
//! [`AgentHook`] is one lifecycle method per tool-phase event; unsupported
//! combinations are rejected by the compiler instead of being interpreted at
//! runtime. The surface is the tool pair (PROTOCOL.md flag 31, ruled
//! 2026-08): everything else inherited from the rig 0.41.0 vendoring —
//! model-selection routing, completion-call request patches, and every
//! observation point — was deleted as surface without a consumer or a
//! ruling. New hook points are designed with their consumers in the
//! extension discussion, not re-added by inertia.
//!
//! Hooks run in registration order through [`HookStack`]; tool-call
//! argument rewrites chain into later hooks. Nested stacks obey the same
//! rules as flat stacks, including preserving an argument rewrite when an
//! inner stack later skips.
//!
//! Tool-result rewrites change the effective `presentation` sent to
//! the model and recorded as result-content telemetry. The
//! [`ToolResultEvent::raw_result`] and its [`ToolResultEvent::tool_context`]
//! remain unchanged for policy decisions and execution-outcome metadata.
//!
//! Blocking and streaming agents share tool-call and tool-result
//! resolution, so the pair has identical semantics on both surfaces.
//!
//! # Example
//!
//! ```
//! use rig_agent::agent::{hook::ToolCallAction, AgentHook, HookContext, ToolCall};
//!
//! struct BashGuard;
//!
//! impl AgentHook for BashGuard {
//!     async fn on_tool_call(&self, _ctx: &HookContext, call: ToolCall<'_>) -> ToolCallAction {
//!         if call.tool_name == "bash" && call.args.contains("rm -rf") {
//!             ToolCallAction::skip("destructive command denied — the call did not run")
//!         } else {
//!             ToolCallAction::run()
//!         }
//!     }
//! }
//! ```

use std::collections::HashMap;
use std::{future::Future, sync::Arc, sync::Mutex};

use rig_core::wasm_compat::{WasmBoxedFuture, WasmCompatSend, WasmCompatSync};

use crate::{
    json_utils,
    tool::{ToolContext, ToolOutput, ToolResult},
};

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

/// Run-scoped context supplied to hooks: the announced turn id, the
/// unified capability map, and the rewrite-chaining frames. Identity
/// accessors (run id, turn counter, surface, agent name) are
/// deliberately absent — those identities live where their consumers
/// are (announced turn ids on events, run/agent names on telemetry
/// spans), and unconsumed surface is deleted, not kept by inertia
/// (PROTOCOL.md flag 31 and its follow-up).
#[derive(Debug)]
pub struct HookContext {
    /// The announced id of the turn in flight (ENGINE.md behavior delta
    /// 10): set when a model-call attempt commits, read by hooks for the
    /// rest of that attempt. `None` only before the first attempt or on
    /// surfaces that never announce.
    turn_id: Mutex<Option<String>>,
    tool_call_rewrite_frames: ToolCallRewriteFrames,
    /// The run's capability map — the same [`ToolContext`] the tool
    /// bodies read (snapshot at run start), so hooks and tools see one
    /// set of capabilities: "why could my tool ask but not my hook" is
    /// removed, not documented (the unified run context).
    capabilities: crate::tool::ToolContext,
}

impl HookContext {
    pub(crate) fn new(capabilities: crate::tool::ToolContext) -> Self {
        Self {
            turn_id: Mutex::new(None),
            capabilities,
            tool_call_rewrite_frames: ToolCallRewriteFrames::default(),
        }
    }

    pub(crate) fn set_turn_id(&self, id: String) {
        *self
            .turn_id
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(id);
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

    /// The run's [`UserInteraction`](crate::tool::interaction::UserInteraction)
    /// capability, when the host inserted one — hooks and tools ask
    /// the same way (the unified run context).
    pub fn interaction(
        &self,
    ) -> Option<std::sync::Arc<dyn crate::tool::interaction::UserInteraction>> {
        self.capabilities
            .get::<std::sync::Arc<dyn crate::tool::interaction::UserInteraction>>()
            .cloned()
    }

    fn begin_tool_call_resolution(&self, internal_call_id: &str) -> ToolCallResolutionFrame<'_> {
        self.tool_call_rewrite_frames.begin(internal_call_id)
    }

    fn record_tool_call_rewrite(&self, internal_call_id: &str, rewrite: serde_json::Value) {
        self.tool_call_rewrite_frames
            .record(internal_call_id, rewrite);
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

/// Per-run lifecycle gate for the tool phase.
pub trait AgentHook: WasmCompatSend + WasmCompatSync {
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
}

trait DynAgentHook: WasmCompatSend + WasmCompatSync {
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
}

impl<H> DynAgentHook for H
where
    H: AgentHook,
{
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
}

/// How a closure registration identifies and orders itself: the id
/// names the subscription (author-chosen, stable across runs — attribution,
/// introspection, replace-by-id later), the priority orders it against
/// other registrations (stable sort; equal priorities fall back to
/// registration order; reference bands are extension docs, not
/// type-level). `From<&str>`/`From<String>` give the bare-closure
/// sugar: an id alone, priority 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSpec {
    pub id: String,
    pub priority: i32,
}

impl From<&str> for HookSpec {
    fn from(id: &str) -> Self {
        Self {
            id: id.to_string(),
            priority: 0,
        }
    }
}

impl From<String> for HookSpec {
    fn from(id: String) -> Self {
        Self { id, priority: 0 }
    }
}

impl From<(&str, i32)> for HookSpec {
    fn from((id, priority): (&str, i32)) -> Self {
        Self {
            id: id.to_string(),
            priority,
        }
    }
}

/// The per-event boxed closure shape (copy-out: the future owns its
/// captures; take what you need from the event by value before
/// awaiting).
pub type ToolCallFn = Box<
    dyn Fn(&HookContext, ToolCall<'_>) -> WasmBoxedFuture<'static, ToolCallAction> + Send + Sync,
>;

/// One registered closure, tagged by its event point (built by the
/// [`on`] constructors).
pub enum OnEvent {
    ToolCall(ToolCallFn),
}

/// The registration constructors: `on::tool_call(|ctx, call| async move { ... })`.
pub mod on {
    use super::*;

    /// The pre-call gate point (`ToolCallAction`; Skip absorbing).
    pub fn tool_call(
        f: impl Fn(&HookContext, ToolCall<'_>) -> WasmBoxedFuture<'static, ToolCallAction>
        + Send
        + Sync
        + 'static,
    ) -> OnEvent {
        OnEvent::ToolCall(Box::new(f))
    }
}

/// One closure registration: the gate point this round's consumers
/// exist for (the permission seam). More event points join as
/// consumers do — "not registered" is the filter.
pub(crate) struct ClosureHook {
    tool_call: Option<ToolCallFn>,
}

impl AgentHook for ClosureHook {
    async fn on_tool_call(&self, ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        match &self.tool_call {
            Some(f) => f(ctx, event).await,
            None => ToolCallAction::run(),
        }
    }
}

impl HookStack {
    /// Register a closure at its event point: `.hook(spec, on::tool_call(|ctx, call| ...))`.
    /// `on_tool_call` is the pre-call gate point — Skip is absorbing
    /// (the first deny in priority order wins; later registrations do
    /// not see the call), Run is neutral.
    pub fn hook(mut self, spec: impl Into<HookSpec>, on: OnEvent) -> Self {
        let spec = spec.into();
        let closure = match on {
            OnEvent::ToolCall(f) => ClosureHook { tool_call: Some(f) },
        };
        self.push_prioritized(spec.id, spec.priority, std::sync::Arc::new(closure));
        self
    }
}

/// Ordered composable hook stack.
///
/// Nested stacks preserve the same composition semantics.
#[derive(Clone, Default)]
pub struct HookStack {
    // Registered records, stably sorted by priority on every push
    // (stacks are tiny), equal priorities preserving registration
    // order — the order law (ENGINE.md's hook surface).
    hooks: Vec<StackedHook>,
}

/// One registration inside a [`HookStack`] — the record the hook
/// surface is built on (ENGINE.md): the author-chosen id (attribution
/// and replace-by-id fall out of it when consumers appear), the
/// priority, the registration sequence (the stable-sort tiebreak),
/// and the hook itself.
#[derive(Clone)]
struct StackedHook {
    id: String,
    priority: i32,
    seq: u64,
    hook: Arc<dyn DynAgentHook>,
}

impl std::fmt::Debug for HookStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookStack")
            .field("hooks", &self.ids())
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

    /// The registration ids in resolution order (priority, then
    /// registration) — a stack's identity for debugging and the
    /// foundation for attribution surfaces.
    pub fn ids(&self) -> Vec<&str> {
        self.hooks.iter().map(|record| record.id.as_str()).collect()
    }

    /// Appends a trait hook at the default priority (0): registration
    /// order among trait hooks is preserved exactly.
    pub fn push<H: AgentHook + 'static>(&mut self, hook: H) {
        // Trait hooks carry no author id; the sequence keeps each
        // synthetic name distinct.
        let id = format!("trait-{}", self.hooks.len());
        self.push_prioritized(id, 0, Arc::new(hook));
    }

    /// Appends a hook entry at `priority` and restores the sort
    /// (stable: equal priorities keep insertion order).
    fn push_prioritized(&mut self, id: String, priority: i32, hook: Arc<dyn DynAgentHook>) {
        let seq = self.hooks.len() as u64;
        self.hooks.push(StackedHook {
            id,
            priority,
            seq,
            hook,
        });
        self.hooks
            .sort_by_key(|record| (record.priority, record.seq));
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
        for record in &self.hooks {
            let hook = &record.hook;
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

impl AgentHook for HookStack {
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
        for record in &self.hooks {
            let current = ToolResultEvent {
                presentation: effective.as_ref().unwrap_or(event.presentation),
                ..event
            };
            match record.hook.tool_result(ctx, current).await {
                ToolResultAction::Keep => {}
                ToolResultAction::Rewrite(value) => effective = Some(value),
                stop @ ToolResultAction::Stop(_) => return stop,
            }
        }
        effective.map_or(ToolResultAction::Keep, ToolResultAction::Rewrite)
    }
}

#[cfg(test)]
#[path = "hook_tests.rs"]
mod hook_tests;
