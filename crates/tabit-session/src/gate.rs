//! The tool-gate seam — the assembly's mount point for tool-call
//! policy (the dev-time permission gate today, the real permission
//! system and other extension policy later; EXTENSIONS.md).
//!
//! The engine's `AgentHook` trait is not dyn-compatible (RPITIT
//! futures), so the seam is its own small trait — [`ToolGate`] — and
//! [`GateHook`] adapts it into a hook at run assembly. The core
//! mounts whatever the assembly's factory builds and never names a
//! concrete policy: deleting the policy module and the one assembly
//! line that mounts it removes every trace.

use std::sync::Arc;

use futures::future::BoxFuture;
use rig_agent::agent::hook::{AgentHook, HookContext, ToolCall, ToolCallAction};

use crate::interaction::InteractionHub;

/// Decides whether a tool call runs. A gate may ask the user through
/// the hub (permission cards) or decide on its own (static policy);
/// returning [`ToolCallAction::Skip`] with an explanatory message
/// tells the model, in-band, why the call did not run — the batch's
/// siblings are unaffected.
pub trait ToolGate: Send + Sync {
    /// The decision for one admitted call. The future must own its
    /// captures (`'static`): it runs inside the engine's tool phase.
    fn on_tool_call(&self, tool_name: &str, args: &str) -> BoxFuture<'static, ToolCallAction>;
}

/// Builds a session's gate per run: the assembly (the binary) provides
/// the factory; `None` means the session has no interaction frontend
/// (a direct [`crate::Session`] consumer) — a gate mounts anyway and
/// decides for itself, typically failing closed.
pub type ToolGateFactory = Arc<dyn Fn(Option<&InteractionHub>) -> Arc<dyn ToolGate> + Send + Sync>;

/// The engine-side mounting of a [`ToolGate`] — the adapter that keeps
/// the seam dyn-compatible while the engine's hook chain stays
/// concrete.
pub(crate) struct GateHook(pub Arc<dyn ToolGate>);

impl AgentHook for GateHook {
    async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        let ToolCall {
            tool_name, args, ..
        } = event;
        self.0.on_tool_call(tool_name, args).await
    }
}
