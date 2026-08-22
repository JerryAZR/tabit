//! The permission gate — the core's basic, test-the-path policy
//! (EXTENSIONS.md): ask before the listed tools run, remember "Always
//! allow" for the session, deny maps to the engine's existing `Skip`
//! (an in-band synthetic result — the model is told, siblings are
//! unaffected, nothing kills a batch). The real permission system is
//! an extension over the same seams; this module is the deletable
//! placeholder that proves the interaction path.

use rig_agent::agent::hook::{AgentHook, HookContext, ToolCall, ToolCallAction};

use crate::interaction::{InteractionHub, permission_labels, permission_prompt};

/// Tools the core gate asks about. Everything else passes the gate
/// silently.
pub const PERMISSION_ASK_TOOLS: &[&str] = &["bash"];

/// The pre-body permission gate, installed on every session's runs like
/// `RecorderHook` is. Without an interaction frontend (a direct
/// [`crate::Session`] consumer rather than the actor) the gate fails
/// closed: the call does not run and the model is told why.
pub struct PermissionHook {
    hub: Option<InteractionHub>,
}

impl PermissionHook {
    /// Build the gate over the session's hub (`None` = no frontend:
    /// fail closed).
    pub fn new(hub: Option<InteractionHub>) -> Self {
        Self { hub }
    }
}

impl AgentHook for PermissionHook {
    async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        let ToolCall {
            tool_name, args, ..
        } = event;
        if !PERMISSION_ASK_TOOLS.contains(&tool_name) {
            return ToolCallAction::run();
        }
        let Some(hub) = &self.hub else {
            return ToolCallAction::skip(format!(
                "`{tool_name}` requires permission, but this session has no interactive \
                 frontend to grant it — the call did not run"
            ));
        };
        if hub.is_always_allowed(tool_name) {
            return ToolCallAction::run();
        }
        let reply = hub
            .capability()
            .ask(permission_prompt(tool_name, args))
            .await;
        match reply.option.as_deref() {
            Some(permission_labels::ALLOW) => ToolCallAction::run(),
            Some(permission_labels::ALWAYS_ALLOW) => {
                hub.always_allow(tool_name);
                ToolCallAction::run()
            }
            // Deny — including a dismissed, retracted, or unrecognized
            // answer: the gate fails closed. A free-text reason (when
            // given) is delivered to the model with the denial.
            _ => {
                let reason = match reply.text.as_deref() {
                    Some(text) if !text.trim().is_empty() => format!(": {text}"),
                    _ => String::new(),
                };
                ToolCallAction::skip(format!(
                    "the user denied `{tool_name}`{reason} — the call did not run"
                ))
            }
        }
    }
}
