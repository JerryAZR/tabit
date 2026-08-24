//! The interaction hub — the generic ask pattern (ENGINE.md's tool
//! phase; FRONTEND.md §8 is the wire contract). This module knows
//! nothing about any particular asker: hooks and tools (permission
//! gates, ask-the-user tools) are dev-time or extension policy that
//! lives elsewhere and consumes the capability; their vocabulary and
//! state never leak here.
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

use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::{mpsc, oneshot};

use rig_agent::tool::interaction::{InteractionPrompt, InteractionReply, UserInteraction};
use tabit_protocol::{EventFrame, InteractionOption, SessionEvent, StreamId};

use crate::ids::new_entry_id;
use crate::lock::lock;

/// The hub's shared state.
struct Inner {
    /// Where requests surface: the worker's event channel (the same one
    /// every other event rides). **Weak** on purpose: the hub
    /// participates in the channel but must not own it — the handle and
    /// command links outlive the worker, so a strong clone would keep
    /// the channel open past wind-down and the event stream would never
    /// end (the termination contract). Safety does not rest on sender
    /// lifetime: a dead receiver surfaces as a failed `send`, and
    /// post-wind-down upgrades fail, dismissing the asker.
    ///
    /// Note: asks bypass `run_one`'s event fold by design — they
    /// originate on tool-chain tasks, not the worker, and reach the
    /// channel directly; ordering with run events is channel send order.
    events: mpsc::WeakUnboundedSender<EventFrame>,
    /// Open questions by id: where the answer goes.
    pending: std::sync::Mutex<HashMap<String, oneshot::Sender<InteractionReply>>>,
    /// The stream stamp for requests (the session's id).
    stream: StreamId,
}

/// The session's interaction router. Cheap to clone (one `Arc`).
#[derive(Clone)]
pub struct InteractionHub {
    inner: Arc<Inner>,
}

impl InteractionHub {
    /// Build the hub over the worker's event channel, stamped with the
    /// session's stream.
    pub fn new(events: mpsc::UnboundedSender<EventFrame>, stream: StreamId) -> Self {
        Self {
            inner: Arc::new(Inner {
                events: events.downgrade(),
                pending: std::sync::Mutex::new(HashMap::new()),
                stream,
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
            stream: self.inner.stream.clone(),
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
        (InteractionHub::new(tx.clone(), StreamId::new("s")), rx, tx)
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
        let hub = InteractionHub::new(tx, StreamId::new("s")); // the only strong sender drops here
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
}
