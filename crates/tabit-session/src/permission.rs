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
        gate(tool_name, args, self.hub.as_ref()).await
    }
}

/// The gate's decision for one call — the whole policy, extracted so the
/// decision table is directly testable (`on_tool_call` is a thin adapter).
async fn gate(tool_name: &str, args: &str, hub: Option<&InteractionHub>) -> ToolCallAction {
    if !PERMISSION_ASK_TOOLS.contains(&tool_name) {
        return ToolCallAction::run();
    }
    let Some(hub) = hub else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tabit_protocol::{EventFrame, SessionEvent};

    type Rx = tokio::sync::mpsc::UnboundedReceiver<EventFrame>;

    /// Drive `gate` to its decision for `bash`, answering the card it opens
    /// with `respond(id)`; `None` retracts the ask instead (a run terminal).
    /// Returns the decision, the hub (for follow-up memory checks), and the
    /// still-open channel. Fails — never hangs — if the gate stalls.
    async fn decide_with(
        respond: impl FnOnce(String) -> Option<(Option<String>, Option<String>)>,
    ) -> (ToolCallAction, InteractionHub, Rx) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = InteractionHub::new(tx.clone(), tabit_protocol::StreamId::new("s"));
        // The hub holds a weak sender by design (the termination
        // contract); this harness stands in for the worker's pump, so the
        // strong sender must live as long as the ask.
        let _worker_sender = tx;
        let asker = hub.clone();
        let decision =
            tokio::spawn(async move { gate("bash", "{\"command\":\"ls\"}", Some(&asker)).await });
        let frame = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the gate must open its card within 5s");
        let SessionEvent::InteractionRequested { id, .. } = frame.expect("a card").event else {
            panic!("expected an interaction request");
        };
        match respond(id.clone()) {
            Some((option, text)) => {
                hub.respond(&id, option, text);
                let action = decision.await.expect("the asker resolves after an answer");
                (action, hub, rx)
            }
            None => {
                hub.clear_pending();
                let action = decision.await.expect("the asker resolves after retraction");
                (action, hub, rx)
            }
        }
    }

    #[tokio::test]
    async fn tools_outside_the_ask_list_run_without_a_card() {
        let action = gate("read", "{}", None).await;
        assert_eq!(action, ToolCallAction::run());
    }

    #[tokio::test]
    async fn without_a_frontend_the_gate_fails_closed_and_says_why() {
        let action = gate("bash", "{\"command\":\"ls\"}", None).await;
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
        let (action, _hub, mut rx) =
            decide_with(|_id| Some((Some("Allow".to_string()), None))).await;
        assert_eq!(action, ToolCallAction::run());
        assert!(rx.try_recv().is_err(), "allow remembers nothing");
    }

    #[tokio::test]
    async fn always_allow_runs_and_makes_the_next_call_cardless() {
        let (first, hub, _rx) =
            decide_with(|_id| Some((Some("Always allow".to_string()), None))).await;
        assert_eq!(first, ToolCallAction::run());
        // Session memory: the next gated call runs with no card opened.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let remembering_hub = InteractionHub::new(tx, tabit_protocol::StreamId::new("s"));
        remembering_hub.always_allow("bash");
        let action = gate("bash", "{}", Some(&remembering_hub)).await;
        assert_eq!(action, ToolCallAction::run());
        assert!(rx.try_recv().is_err(), "a remembered tool asks nothing");
        let _ = hub;
    }

    #[tokio::test]
    async fn deny_delivers_the_reason_with_the_skip() {
        let (action, _hub, _rx) =
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
        let (action, _hub, _rx) = decide_with(|_id| None).await;
        let ToolCallAction::Skip(message) = action else {
            panic!("a dismissed ask must not run the call");
        };
        assert_eq!(
            message, "the user denied `bash` — the call did not run",
            "dismissal is a denial without a reason"
        );
    }
}
