//! The interaction hub — the generic ask pattern (ENGINE.md's tool
//! phase; FRONTEND.md §8 is the wire contract). This module knows
//! nothing about any particular asker: hooks and tools (permission
//! gates, ask-the-user tools) are dev-time or extension policy that
//! lives elsewhere and consumes the capability; their vocabulary and
//! state never leak here.
//!
//! The hub is the session's thin face over the node's ask law: a
//! question is [`Node::ask`] (the id minted, the promise held, the
//! request fanned to whoever displays cards — the routing layer's
//! business), an arriving answer claims the ask table wherever it
//! lands (law 5, kind-checked), and settling — the answer, the run's
//! end, the frontend's death — is the closure's arms: the promise
//! resolves or reads dismissal, and the settle announcement is the
//! node's single-producer discipline. What is left for the hub is
//! the vocabulary mapping: [`InteractionOutcome`] is what tool
//! bodies and hooks consume, and "questions die with their run" is
//! [`Self::clear_pending`]'s owner sweep.

use std::sync::Arc;

use futures::future::BoxFuture;
use tabit_protocol::StreamId;
use tabit_wire::node::Node;

use rig_agent::tool::interaction::{InteractionOutcome, UserInteraction};

/// The session's interaction face over the node. Cheap to clone (one
/// `Arc`); one hub per session worker, attached when the worker takes
/// ownership.
#[derive(Clone)]
pub struct InteractionHub {
    inner: Arc<Inner>,
}

struct Inner {
    node: Arc<Node>,
    /// The session's stream — the ask's owner key (the death sweep)
    /// and the stamp the request carries (the card's home stream).
    stream: StreamId,
}

impl InteractionHub {
    /// Build the hub over the node, speaking as the session's stream.
    pub fn new(node: Arc<Node>, stream: StreamId) -> Self {
        Self {
            inner: Arc::new(Inner { node, stream }),
        }
    }

    /// The capability tools consume: `Arc<dyn UserInteraction>` for
    /// [`rig_agent::tool::ToolContext`]'s typed map.
    pub fn capability(&self) -> Arc<dyn UserInteraction> {
        Arc::new(self.clone())
    }

    /// Retract every open question. Called at run terminals: the
    /// askers died with their run, and the senders must not linger.
    /// The sweep settles each id — a channel that missed whatever
    /// ended the run still learns its card is dead.
    pub fn clear_pending(&self) {
        self.inner
            .node
            .retract_asks(self.inner.stream.as_str(), "the run ended");
    }

    /// Register the question, surface it, await the answer. Drop is
    /// the cancellation: aborting the run (the user, or the frontend
    /// dying — the endpoint's death door aborts) drops the asking
    /// future and the question goes with it; run terminals clear the
    /// rest. A dismissed promise (the sender dropped unresolved)
    /// reads as [`InteractionOutcome::Dismissed`].
    async fn ask_once(&self, ui_type: &str, payload: serde_json::Value) -> InteractionOutcome {
        let awaiter = self.inner.node.ask(
            self.inner.stream.as_str(),
            Some(&self.inner.stream),
            ui_type,
            payload,
        );
        match awaiter.await {
            Ok(answer) => InteractionOutcome::Answered(answer),
            Err(_) => InteractionOutcome::Dismissed,
        }
    }
}

impl UserInteraction for InteractionHub {
    fn request(
        &self,
        ui_type: &str,
        payload: serde_json::Value,
    ) -> BoxFuture<'static, InteractionOutcome> {
        let hub = self.clone();
        let ui_type = ui_type.to_string();
        Box::pin(async move { hub.ask_once(&ui_type, payload).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tabit_protocol::{EventFrame, SessionCommand, SessionEvent};
    use tabit_wire::node::Locality;

    /// The hub over a bare node: one recorder subscription is the
    /// "frontend" — the test drives the card lifecycle the way the
    /// net does.
    fn hub_and_recorder() -> (
        InteractionHub,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        Arc<Node>,
    ) {
        let node = Arc::new(Node::new("test"));
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        node.subscribe_all("recorder", Locality::Both, move |frame: &EventFrame| {
            let note = match &frame.event {
                SessionEvent::InteractionRequest { id, ui_type, .. } => {
                    format!("request:{id}:{ui_type}")
                }
                SessionEvent::InteractionSettled { id } => format!("settled:{id}"),
                event => event.tag().to_string(),
            };
            sink.lock().expect("test lock").push(note);
        });
        (
            InteractionHub::new(node.clone(), StreamId::new("s")),
            seen,
            node,
        )
    }

    /// Wait (bounded) until the request has surfaced — the asker task
    /// runs concurrently — and return its id (the note is
    /// `request:{id}:{ui_type}`; the id ends at the first colon).
    async fn the_request(seen: &std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(rest) = seen
                    .lock()
                    .expect("test lock")
                    .iter()
                    .find_map(|note| note.strip_prefix("request:"))
                {
                    return rest.split(':').next().unwrap_or_default().to_string();
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the request surfaced")
    }

    #[tokio::test]
    async fn an_answer_resolves_the_asker_and_settles_the_card() {
        let (hub, seen, node) = hub_and_recorder();
        let asking = hub.clone();
        let asker = tokio::spawn(async move {
            asking
                .capability()
                .request(
                    tabit_protocol::templates::ui::SELECT_ANY,
                    json!({"prompt": "which file?"}),
                )
                .await
        });

        let id = the_request(&seen).await;
        // The answer arrives from anywhere — an intake on the node.
        node.intake(
            &tabit_wire::node::Channel::local("frontend", |_| {}, |_| {}),
            tabit_wire::node::Inbound::Command(SessionCommand::InteractionResponse {
                session: None,
                id,
                payload: json!({"text": "main.rs"}),
            }),
        );
        assert_eq!(
            asker.await.expect("asker finished"),
            InteractionOutcome::Answered(json!({"text": "main.rs"}))
        );
        let seen = seen.lock().expect("test lock");
        assert!(
            seen.iter().any(|note| note.starts_with("settled:")),
            "the settle announced: {seen:?}"
        );
    }

    #[tokio::test]
    async fn clearing_pending_dismisses_the_open_question() {
        let (hub, seen, node) = hub_and_recorder();
        let asking = hub.clone();
        let asker = tokio::spawn(async move {
            asking
                .capability()
                .request(tabit_protocol::templates::ui::SELECT_ANY, json!({}))
                .await
        });

        let id = the_request(&seen).await;
        hub.clear_pending();
        assert_eq!(
            asker.await.expect("asker finished"),
            InteractionOutcome::Dismissed
        );
        // The retracted response is a no-op — the entry is gone.
        node.intake(
            &tabit_wire::node::Channel::local("frontend", |_| {}, |_| {}),
            tabit_wire::node::Inbound::Command(SessionCommand::InteractionResponse {
                session: None,
                id,
                payload: json!({"text": "too late"}),
            }),
        );
        let seen = seen.lock().expect("test lock");
        assert_eq!(
            seen.iter()
                .filter(|note| note.starts_with("settled:"))
                .count(),
            1,
            "exactly one settle — the sweep's: {seen:?}"
        );
    }
}
