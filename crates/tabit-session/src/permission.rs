//! The permission gate — dev-time policy, mounted by the assembly
//! through the builder's interaction-hook seam (EXTENSIONS.md): ask
//! before the listed tools run, remember "Always allow" for the
//! session, deny maps to the engine's existing `Skip` (an in-band
//! synthetic result — the model is told, siblings are unaffected,
//! nothing kills a batch). The core's generic interaction path (the
//! hub) carries none of this: deleting this module and the one
//! assembly line that mounts it removes every trace of permission
//! from the backend. The real permission system is an extension over
//! the same seam.

use std::collections::HashSet;
use std::sync::Arc;

use rig_agent::agent::hook::ToolCallAction;

use crate::lock::lock;

/// Tools the dev-time gate asks about. Everything else passes the
/// gate silently.
pub const PERMISSION_ASK_TOOLS: &[&str] = &["bash"];

/// Session-scoped "Always allow" memory — the gate's own state, never
/// persisted (the test-the-path policy; EXTENSIONS.md). The assembly
/// creates one per session and threads it to every run's hook, which
/// is what makes the memory outlive runs without the hub (generic
/// routing) ever knowing it exists.
#[derive(Clone, Default)]
pub struct PermissionMemory {
    tools: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl PermissionMemory {
    /// Whether "Always allow" covers this tool.
    pub fn contains(&self, tool: &str) -> bool {
        lock(&self.tools).contains(tool)
    }

    /// Remember "Always allow" for this tool.
    pub fn insert(&self, tool: &str) {
        lock(&self.tools).insert(tool.to_string());
    }
}

/// The labels this gate puts in its confirm payload's options (what a
/// frontend renders as the card's buttons) and reads back from the
/// answer.
mod permission_labels {
    pub const ALLOW: &str = "Allow";
    pub const ALWAYS_ALLOW: &str = "Always allow";
    pub const DENY: &str = "Deny";
}

/// Build the permission ask: the `native:select_one` template with this
/// gate's three options and free text on (a denial reason is delivered
/// to the model). An ordinary template consumer — the core
/// never knows these labels.
#[allow(clippy::expect_used)] // sanctioned crash: pure-data serialization (AGENTS.md doctrine)
pub(crate) fn permission_ask(tool: &str, args: &str) -> (&'static str, serde_json::Value) {
    let card = tabit_protocol::templates::SelectOneCard {
        title: format!("Allow `{tool}` to run?"),
        body: args.to_string(),
        options: vec![
            tabit_protocol::templates::SelectOption::new(permission_labels::ALLOW),
            tabit_protocol::templates::SelectOption {
                label: permission_labels::ALWAYS_ALLOW.to_string(),
                description: Some("skip prompts for this tool until the session ends".to_string()),
            },
            tabit_protocol::templates::SelectOption::new(permission_labels::DENY),
        ],
        free_text: true,
    };
    (
        tabit_protocol::templates::ui::SELECT_ONE,
        serde_json::to_value(card).expect("template payloads always serialize"),
    )
}

/// The gate as a mounted hook stack: one `on_tool_call` closure over
/// the session-scoped memory, asking through the run context (the
/// unified capability map — hooks and tools ask the same way).
#[allow(clippy::expect_used)] // sanctioned crash: pure-data serialization
pub fn permission_gate(memory: PermissionMemory) -> rig_agent::agent::HookStack {
    rig_agent::agent::HookStack::new().hook(
        ("permission", 0),
        rig_agent::agent::on::tool_call(move |ctx, call| {
            let memory = memory.clone();
            let tool_name = call.tool_name.to_string();
            let args = call.args.to_string();
            let interaction = ctx.interaction();
            Box::pin(async move { gate(&tool_name, &args, interaction.as_deref(), &memory).await })
        }),
    )
}

/// The gate's decision for one call — the whole policy, extracted so the
/// decision table is directly testable (`on_tool_call` is a thin adapter).
async fn gate(
    tool_name: &str,
    args: &str,
    hub: Option<&dyn rig_agent::tool::interaction::UserInteraction>,
    memory: &PermissionMemory,
) -> ToolCallAction {
    if !PERMISSION_ASK_TOOLS.contains(&tool_name) {
        return ToolCallAction::run();
    }
    let Some(hub) = hub else {
        return ToolCallAction::skip(format!(
            "`{tool_name}` requires permission, but this session has no interactive \
             frontend to grant it — the call did not run"
        ));
    };
    if memory.contains(tool_name) {
        return ToolCallAction::run();
    }
    let (ui_type, payload) = permission_ask(tool_name, args);
    let reply = match hub.request(ui_type, payload).await {
        rig_agent::tool::interaction::InteractionOutcome::Answered(payload) => {
            match serde_json::from_value::<tabit_protocol::templates::SelectAnswer>(payload) {
                Ok(answer) => answer,
                // A malformed answer is a frontend defect, not a user
                // decision — fail closed, but leave the trace so the
                // protocol break is not recast silently as a denial.
                Err(error) => {
                    tracing::warn!(%error, tool = tool_name, "malformed select answer — failing closed");
                    tabit_protocol::templates::SelectAnswer::default()
                }
            }
        }
        // Dismissed — the gate fails closed.
        rig_agent::tool::interaction::InteractionOutcome::Dismissed => {
            tabit_protocol::templates::SelectAnswer::default()
        }
    };
    match reply.selected.first().map(String::as_str) {
        Some(permission_labels::ALLOW) => ToolCallAction::run(),
        Some(permission_labels::ALWAYS_ALLOW) => {
            memory.insert(tool_name);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction::InteractionHub;
    use std::time::Duration;
    use tabit_protocol::{EventFrame, SessionEvent};

    type Rx = tokio::sync::mpsc::UnboundedReceiver<EventFrame>;

    /// Drive `gate` to its decision for `bash`, answering the card it opens
    /// with `respond(id)`; `None` retracts the ask instead (a run terminal).
    /// Returns the decision, the memory (for follow-up checks), and the
    /// still-open channel. Fails — never hangs — if the gate stalls.
    async fn decide_with(
        respond: impl FnOnce(String) -> Option<(Option<String>, Option<String>)>,
    ) -> (ToolCallAction, PermissionMemory, Rx) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = InteractionHub::new(tx.clone(), tabit_protocol::StreamId::new("s"));
        // The hub holds a weak sender by design (the termination
        // contract); this harness stands in for the worker's pump, so the
        // strong sender must live as long as the ask.
        let _worker_sender = tx;
        let asker = hub.clone();
        let memory = PermissionMemory::default();
        let gate_memory = memory.clone();
        let decision = tokio::spawn(async move {
            gate("bash", "{\"command\":\"ls\"}", Some(&asker), &gate_memory).await
        });
        let frame = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the gate must open its card within 5s");
        let SessionEvent::InteractionRequested { id, .. } = frame.expect("a card").event else {
            panic!("expected an interaction request");
        };
        match respond(id.clone()) {
            Some((option, reason)) => {
                hub.respond(
                    &id,
                    serde_json::json!(tabit_protocol::templates::SelectAnswer {
                        selected: option.into_iter().collect(),
                        text: reason,
                    }),
                );
                let action = decision.await.expect("the asker resolves after an answer");
                (action, memory, rx)
            }
            None => {
                hub.clear_pending();
                let action = decision.await.expect("the asker resolves after retraction");
                (action, memory, rx)
            }
        }
    }

    #[tokio::test]
    async fn tools_outside_the_ask_list_run_without_a_card() {
        let action = gate("read", "{}", None, &PermissionMemory::default()).await;
        assert_eq!(action, ToolCallAction::run());
    }

    #[tokio::test]
    async fn without_a_frontend_the_gate_fails_closed_and_says_why() {
        let action = gate(
            "bash",
            "{\"command\":\"ls\"}",
            None,
            &PermissionMemory::default(),
        )
        .await;
        let ToolCallAction::Skip(message) = action else {
            panic!("a gated call without a frontend must not run");
        };
        assert!(
            message.contains("no interactive frontend"),
            "the skip must name the missing frontend: {message}"
        );
    }

    #[tokio::test]
    async fn allow_runs_the_call() {
        let (action, memory, mut rx) =
            decide_with(|_id| Some((Some("Allow".to_string()), None))).await;
        assert_eq!(action, ToolCallAction::run());
        assert!(!memory.contains("bash"), "allow remembers nothing");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn always_allow_runs_and_makes_the_next_call_cardless() {
        let (first, memory, _rx) =
            decide_with(|_id| Some((Some("Always allow".to_string()), None))).await;
        assert_eq!(first, ToolCallAction::run());
        // Session memory: the next gated call runs with no card opened.
        assert!(memory.contains("bash"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = InteractionHub::new(tx, tabit_protocol::StreamId::new("s"));
        let action = gate("bash", "{}", Some(&hub), &memory).await;
        assert_eq!(action, ToolCallAction::run());
        assert!(rx.try_recv().is_err(), "a remembered tool asks nothing");
    }

    #[tokio::test]
    async fn deny_delivers_the_reason_with_the_skip() {
        let (action, _memory, _rx) =
            decide_with(|_id| Some((Some("Deny".to_string()), Some("too risky".to_string()))))
                .await;
        let ToolCallAction::Skip(message) = action else {
            panic!("a denied call must not run");
        };
        assert_eq!(
            message, "the user denied `bash`: too risky — the call did not run",
            "the model sees the denial and its reason, verbatim"
        );
    }

    #[tokio::test]
    async fn an_unanswered_card_retracted_by_a_terminal_fails_closed() {
        let (action, _memory, _rx) = decide_with(|_id| None).await;
        let ToolCallAction::Skip(message) = action else {
            panic!("a dismissed ask must not run the call");
        };
        assert_eq!(
            message, "the user denied `bash` — the call did not run",
            "dismissal is a denial without a reason"
        );
    }

    #[test]
    fn permission_asks_with_the_confirm_template() {
        let (ui_type, payload) = permission_ask("bash", "{\"command\":\"ls\"}");
        assert_eq!(ui_type, tabit_protocol::templates::ui::SELECT_ONE);
        let card: tabit_protocol::templates::SelectOneCard =
            serde_json::from_value(payload).expect("the template payload parses");
        assert!(card.free_text);
        assert_eq!(
            card.options
                .iter()
                .map(|o| o.label.as_str())
                .collect::<Vec<_>>(),
            ["Allow", "Always allow", "Deny"]
        );
        assert_eq!(card.body, "{\"command\":\"ls\"}");
    }

    #[test]
    fn the_memory_is_the_gate_state_not_the_hubs() {
        let memory = PermissionMemory::default();
        assert!(!memory.contains("bash"));
        memory.insert("bash");
        assert!(memory.contains("bash"));
        // A clone shares the session's memory (the factory threads one
        // memory to every run's hook).
        assert!(memory.clone().contains("bash"));
    }
}
