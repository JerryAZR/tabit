//! The interaction hub — the ask pattern (ENGINE.md's tool phase;
//! FRONTEND.md §8 is the wire contract).
//!
//! One hub per session worker, the actor's third shared leaf beside
//! the mailbox and abort: [`InteractionHub::ask`] is called from tool
//! bodies and hooks (many producers), registers a oneshot in the
//! pending map, emits `interaction_request` on the event channel, and
//! awaits; [`InteractionHub::respond`] routes an arriving answer by id
//! to the one awaiting asker — a sync leaf call needing no worker
//! attention. Total semantics: an unknown id or a dead asker is a
//! logged no-op. Run terminals clear the pending map (questions die
//! with their chains — drop is the cancellation), and the frontend
//! closes cards on terminals, so no close event exists.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::{mpsc, oneshot};

use rig_agent::tool::interaction::{
    InteractionChoice, InteractionPrompt, InteractionReply, UserInteraction,
};
use tabit_protocol::{EventFrame, InteractionOption, SessionEvent, StreamId};

use crate::ids::new_entry_id;
use crate::lock::lock;

/// The hub's shared state.
struct Inner {
    /// Where requests surface: the worker's event channel (the same one
    /// every other event rides). **Weak** on purpose: the hub
    /// participates in the channel but must not own it — a strong
    /// clone would keep the channel open past worker wind-down and the
    /// event stream would never end (the termination contract). Strong
    /// senders exist exactly while a pump is in flight, which is the
    /// only time an ask can happen.
    events: mpsc::WeakUnboundedSender<EventFrame>,
    /// Open questions by id: where the answer goes.
    pending: std::sync::Mutex<HashMap<String, oneshot::Sender<InteractionReply>>>,
    /// Tool names granted "Always allow" — session memory, never
    /// persisted (the test-the-path policy; EXTENSIONS.md).
    always_allowed: std::sync::Mutex<HashSet<String>>,
}

/// The session's interaction router. Cheap to clone (one `Arc`).
#[derive(Clone)]
pub struct InteractionHub {
    inner: Arc<Inner>,
}

impl InteractionHub {
    /// Build the hub over the worker's event channel.
    pub fn new(events: mpsc::UnboundedSender<EventFrame>) -> Self {
        Self {
            inner: Arc::new(Inner {
                events: events.downgrade(),
                pending: std::sync::Mutex::new(HashMap::new()),
                always_allowed: std::sync::Mutex::new(HashSet::new()),
            }),
        }
    }

    /// The capability tools consume: `Arc<dyn UserInteraction>` for
    /// [`rig_agent::tool::ToolContext`]'s typed map.
    pub fn capability(&self) -> Arc<dyn UserInteraction> {
        Arc::new(self.clone())
    }

    /// Deliver an answer. Returns whether it reached a live asker (a
    /// miss is the total-semantics no-op — the question went away with
    /// its run).
    pub fn respond(&self, id: &str, option: Option<String>, text: Option<String>) -> bool {
        let sender = lock(&self.inner.pending).remove(id);
        match sender {
            Some(sender) => sender.send(InteractionReply { option, text }).is_ok(),
            None => {
                tracing::debug!(
                    interaction_id = id,
                    "interaction response for an unknown or closed request — dropped"
                );
                false
            }
        }
    }

    /// Retract every open question. Called at run terminals: the askers
    /// died with the run, and the senders must not linger.
    pub fn clear_pending(&self) {
        lock(&self.inner.pending).clear();
    }

    /// Whether "Always allow" covers this tool (session memory).
    pub fn is_always_allowed(&self, tool: &str) -> bool {
        lock(&self.inner.always_allowed).contains(tool)
    }

    /// Remember "Always allow" for this tool (session memory).
    pub fn always_allow(&self, tool: &str) {
        lock(&self.inner.always_allowed).insert(tool.to_string());
    }

    /// Register the question, surface it, and await the answer. Drop is
    /// the cancellation: aborting the run (the user, or the frontend
    /// dying — the endpoint's death watcher aborts) drops the asking
    /// future and the question goes with it; run terminals clear the
    /// map. A dead event channel at registration (frontend already
    /// gone, no pump in flight) resolves as unanswered.
    async fn ask_once(&self, prompt: InteractionPrompt) -> InteractionReply {
        let (sender, receiver) = oneshot::channel();
        let id = new_entry_id();
        lock(&self.inner.pending).insert(id.clone(), sender);
        let event = EventFrame {
            stream: StreamId::main(),
            event: SessionEvent::InteractionRequested {
                id: id.clone(),
                title: prompt.title,
                body: prompt.body,
                options: prompt
                    .options
                    .into_iter()
                    .map(|choice| InteractionOption {
                        label: choice.label,
                        description: choice.description,
                    })
                    .collect(),
                free_text: prompt.free_text,
            },
        };
        let sent = self
            .inner
            .events
            .upgrade()
            .is_some_and(|channel| channel.send(event).is_ok());
        if !sent {
            // No pump in flight, or the frontend is already gone: no one
            // will ever answer.
            lock(&self.inner.pending).remove(&id);
            return InteractionReply::unanswered();
        }
        match receiver.await {
            Ok(reply) => reply,
            // The sender was dropped without sending — the run ended
            // under the question (terminal retraction or the death
            // watcher's abort dropping the asker).
            Err(_) => InteractionReply::unanswered(),
        }
    }
}

impl UserInteraction for InteractionHub {
    fn ask(&self, prompt: InteractionPrompt) -> BoxFuture<'static, InteractionReply> {
        let hub = self.clone();
        Box::pin(async move { hub.ask_once(prompt).await })
    }
}

/// The permission gate's wire vocabulary: the option labels a frontend
/// renders as the card's buttons (FRONTEND.md §8). Shared with the
/// permission module.
pub mod permission_labels {
    pub const ALLOW: &str = "Allow";
    pub const ALWAYS_ALLOW: &str = "Always allow";
    pub const DENY: &str = "Deny";
}

/// Build the permission prompt for a gated tool call: the three-button
/// card with free text on (a denial reason is delivered to the model).
pub(crate) fn permission_prompt(tool: &str, args: &str) -> InteractionPrompt {
    InteractionPrompt {
        title: format!("Allow `{tool}` to run?"),
        body: args.to_string(),
        options: vec![
            InteractionChoice::new(permission_labels::ALLOW),
            InteractionChoice {
                label: permission_labels::ALWAYS_ALLOW.to_string(),
                description: Some("skip prompts for this tool until the session ends".to_string()),
            },
            InteractionChoice::new(permission_labels::DENY),
        ],
        free_text: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hub plus its channel ends. The returned **strong** sender
    /// stands in for the worker's pump callback — the only strong
    /// sender in production, alive exactly while a run is in flight.
    fn hub_with_channel() -> (
        InteractionHub,
        mpsc::UnboundedReceiver<EventFrame>,
        mpsc::UnboundedSender<EventFrame>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        (InteractionHub::new(tx.clone()), rx, tx)
    }

    fn request_from(frame: &EventFrame) -> (String, Vec<String>) {
        match &frame.event {
            SessionEvent::InteractionRequested { id, options, .. } => (
                id.clone(),
                options.iter().map(|o| o.label.clone()).collect(),
            ),
            other => panic!("expected an interaction request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_answer_routes_to_its_awaiting_asker() {
        let (hub, mut rx, _tx) = hub_with_channel();
        let capability = hub.capability();
        let asker =
            tokio::spawn(
                async move { capability.ask(InteractionPrompt::ask("which file?")).await },
            );

        let frame = rx.recv().await.expect("request emitted");
        let (id, options) = request_from(&frame);
        assert!(options.is_empty(), "free-text ask carries no buttons");

        assert!(hub.respond(&id, None, Some("main.rs".to_string())));
        let reply = asker.await.expect("asker finished");
        assert_eq!(
            reply,
            InteractionReply {
                option: None,
                text: Some("main.rs".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn a_response_for_an_unknown_id_is_a_total_no_op() {
        let (hub, _rx, _tx) = hub_with_channel();
        assert!(!hub.respond("no-such-id", Some("Allow".to_string()), None));
    }

    #[tokio::test]
    async fn an_ask_without_a_strong_sender_reports_the_dismissal() {
        // No pump in flight (or the frontend gone): the weak upgrade
        // fails and the ask resolves unanswered instead of hanging.
        let (tx, _rx) = mpsc::unbounded_channel();
        let hub = InteractionHub::new(tx); // the only strong sender drops here
        let reply = hub
            .capability()
            .ask(InteractionPrompt::ask("anyone?"))
            .await;
        assert_eq!(reply, InteractionReply::unanswered());
    }

    #[tokio::test]
    async fn clearing_pending_retracts_open_questions_as_unanswered() {
        let (hub, mut rx, _tx) = hub_with_channel();
        let capability = hub.capability();
        let asker =
            tokio::spawn(async move { capability.ask(InteractionPrompt::ask("staying?")).await });
        let frame = rx.recv().await.expect("request emitted");
        let (id, _) = request_from(&frame);
        hub.clear_pending();
        // The retracted response is a no-op, and the asker resolves
        // unanswered.
        assert!(!hub.respond(&id, Some("Allow".to_string()), None));
        assert_eq!(
            asker.await.expect("asker finished"),
            InteractionReply::unanswered()
        );
    }

    #[test]
    fn always_allow_is_session_memory() {
        let (hub, _rx, _tx) = hub_with_channel();
        assert!(!hub.is_always_allowed("bash"));
        hub.always_allow("bash");
        assert!(hub.is_always_allowed("bash"));
    }

    #[test]
    fn permission_prompts_carry_the_three_buttons_and_free_text() {
        let prompt = permission_prompt("bash", "{\"command\":\"ls\"}");
        assert!(prompt.free_text);
        assert_eq!(
            prompt
                .options
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            ["Allow", "Always allow", "Deny"]
        );
        assert_eq!(prompt.body, "{\"command\":\"ls\"}");
    }
}
