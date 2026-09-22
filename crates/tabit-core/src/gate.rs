//! The built-in permission gate's integration: pi-sanity's ported
//! policy (crates/tabit-gate) as one in-process `AgentHook` member,
//! assembled into the binary's hook stack (owner ruling 2026-09 — a
//! default safety feature must not fail open on a dead extension
//! process). The card flow follows pi-sanity's integration layer:
//! `allow` runs, `deny` skips with the reason, `ask` opens one
//! `native:select_one` card (Allow / Block, free text carries a
//! block reason) — an answer of Block, a dismissal, or no UI
//! available (print mode, a headless host) all skip, never silently
//! run. There is deliberately no "Always allow" in v1 — pi-sanity
//! has none, and the default rule set is shaped to ask rarely.

use rig_agent::agent::hook::{AgentHook, HookContext, ToolCall, ToolCallAction};
use rig_agent::tool::interaction::InteractionOutcome;
use serde_json::json;

/// The gate: the default rule set plus the ask. Shared across
/// sessions like every hook member (stateless per call — the policy
/// is pure).
#[derive(Clone)]
pub struct PermissionGate {
    config: std::sync::Arc<tabit_gate::config::SanityConfig>,
}

impl PermissionGate {
    /// The default-policy gate (settings carry only the opt-out, so
    /// the rules themselves are the shipped defaults, one instance
    /// per process).
    pub fn mount() -> Self {
        Self {
            config: std::sync::Arc::new(tabit_gate::config::default_config()),
        }
    }

    /// The hook stack with the gate mounted — priority 0, ahead of
    /// whatever the extension mount carries (the cheap in-process
    /// checks run first; extensions see what survived).
    pub fn stack() -> rig_agent::agent::HookStack {
        let mut stack = rig_agent::agent::HookStack::new();
        stack.push(Self::mount());
        stack
    }
}

impl AgentHook for PermissionGate {
    async fn on_tool_call(&self, ctx: &HookContext, call: ToolCall<'_>) -> ToolCallAction {
        // The engine hands the effective arguments as a raw JSON
        // string (post-rewrite). Unparseable arguments are nothing
        // the gate can check — the strict-arguments doctrine errors
        // the call before execution anyway.
        let Ok(serde_json::Value::Object(args)) = serde_json::from_str(call.args) else {
            return ToolCallAction::run();
        };
        let Some(result) = tabit_gate::check_tool_call(call.tool_name, &args, &self.config) else {
            // No rule names this tool: pass through silently.
            return ToolCallAction::run();
        };
        match result.action {
            tabit_gate::types::Action::Allow => ToolCallAction::run(),
            tabit_gate::types::Action::Deny => skip(result.reason, call),
            tabit_gate::types::Action::Ask => {
                let Some(interaction) = ctx.interaction() else {
                    return skip(
                        Some(format!(
                            "{} (no UI available — the gate treats an unanswerable ask as a block)",
                            result
                                .reason
                                .as_deref()
                                .unwrap_or("this operation requires confirmation")
                        )),
                        call,
                    );
                };
                let reason = result
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("`{}` requires confirmation", call.tool_name));
                let payload = json!({
                    "title": reason,
                    "body": tabit_gate::build_tool_details(call.tool_name, &args, &self.config),
                    "options": [
                        {"label": "Allow"},
                        {"label": "Block", "description": "the call does not run"},
                    ],
                    "free_text": true,
                });
                match interaction.request("native:select_one", payload).await {
                    InteractionOutcome::Answered(answer) => {
                        let allowed = answer["selected"][0]
                            .as_str()
                            .is_some_and(|label| label == "Allow");
                        if allowed {
                            ToolCallAction::run()
                        } else {
                            let custom = answer["text"]
                                .as_str()
                                .map(str::trim)
                                .filter(|t| !t.is_empty());
                            skip(
                                Some(match custom {
                                    Some(text) => format!("{reason}: {text}"),
                                    None => format!("{reason} (blocked by user)"),
                                }),
                                call,
                            )
                        }
                    }
                    // A dismissal is a block, never a silent run —
                    // the run ending under the card drops the ask and
                    // the call must not sneak through.
                    InteractionOutcome::Dismissed => {
                        skip(Some(format!("{reason} (blocked by user)")), call)
                    }
                }
            }
        }
    }
}

/// The skip: pi-sanity's block shape — the call did not run, the
/// agent is told why, and the turn continues.
fn skip(reason: Option<String>, call: ToolCall<'_>) -> ToolCallAction {
    ToolCallAction::skip(format!(
        "the permission gate blocked `{}` — {}: the call did not run",
        call.tool_name,
        reason.as_deref().unwrap_or("not permitted by policy")
    ))
}
