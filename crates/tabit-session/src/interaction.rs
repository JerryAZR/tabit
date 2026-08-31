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

use rig_agent::tool::interaction::{InteractionOutcome, UserInteraction};
use tabit_protocol::{EventFrame, SessionEvent, StreamId};

use crate::ids::new_entry_id;
use crate::lock::lock;
use crate::notice::NoticeSink;

/// The hub's shared state.
struct Inner {
    /// Where requests surface: the worker's event channel (the same one
    /// every other event rides), held as the notice sink — the handle
    /// and command links outlive the worker, so the weak discipline of
    /// [`crate::notice`] is what lets the stream end. A dead channel
    /// fails the emit, dismissing the asker.
    ///
    /// Note: asks bypass `run_one`'s event fold by design — they
    /// originate on tool-chain tasks, not the worker, and reach the
    /// channel directly; ordering with run events is channel send order.
    notices: NoticeSink,
    /// Open questions by id: where the answer payload goes.
    pending: std::sync::Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>,
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
                notices: NoticeSink::new(&events, stream),
                pending: std::sync::Mutex::new(HashMap::new()),
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
    pub fn respond(&self, id: &str, payload: serde_json::Value) -> bool {
        let sender = lock(&self.inner.pending).remove(id);
        match sender {
            Some(sender) => sender.send(payload).is_ok(),
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

    /// Register the question, surface it, await the answer. Drop is
    /// the cancellation: aborting the run (the user, or the frontend
    /// dying — the endpoint's death watcher aborts) drops the asking
    /// future and the question goes with it; run terminals clear the
    /// map. A dead event channel at registration (frontend already
    /// gone, no pump in flight) resolves as dismissed.
    async fn ask_once(&self, ui_type: &str, payload: serde_json::Value) -> InteractionOutcome {
        let (sender, receiver) = oneshot::channel();
        let id = new_entry_id();
        lock(&self.inner.pending).insert(id.clone(), sender);
        let sent = self.inner.notices.emit(SessionEvent::InteractionRequested {
            id: id.clone(),
            ui_type: ui_type.to_string(),
            payload,
        });
        if !sent {
            // No pump in flight, or the frontend is already gone: no one
            // will ever answer.
            lock(&self.inner.pending).remove(&id);
            return InteractionOutcome::Dismissed;
        }
        match receiver.await {
            Ok(payload) => InteractionOutcome::Answered(payload),
            // The sender was dropped without sending — the run ended
            // under the question (terminal retraction or the death
            // watcher's abort dropping the asker).
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

    fn request_from(frame: &EventFrame) -> (String, String, serde_json::Value) {
        match &frame.event {
            SessionEvent::InteractionRequested {
                id,
                ui_type,
                payload,
                ..
            } => (id.clone(), ui_type.clone(), payload.clone()),
            other => panic!("expected an interaction request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_answer_routes_to_its_awaiting_asker() {
        let (hub, mut rx, _tx) = hub_with_channel();
        let capability = hub.capability();
        let asker = tokio::spawn(async move {
            capability
                .request(
                    tabit_protocol::templates::ui::ASK,
                    serde_json::json!({"prompt": "which file?"}),
                )
                .await
        });

        // The hub is payload-blind: ui_type and payload pass through
        // verbatim, stamped with the session's stream.
        let frame = rx.recv().await.expect("request emitted");
        assert_eq!(frame.stream.as_ref().map(StreamId::as_str), Some("s"));
        let (id, ui_type, payload) = request_from(&frame);
        assert_eq!(ui_type, tabit_protocol::templates::ui::ASK);
        assert_eq!(payload, serde_json::json!({"prompt": "which file?"}));

        assert!(hub.respond(&id, serde_json::json!({"text": "main.rs"})));
        assert_eq!(
            asker.await.expect("asker finished"),
            InteractionOutcome::Answered(serde_json::json!({"text": "main.rs"}))
        );
    }

    #[tokio::test]
    async fn a_response_for_an_unknown_id_is_a_total_no_op() {
        let (hub, _rx, _tx) = hub_with_channel();
        assert!(!hub.respond("no-such-id", serde_json::json!({"option": "Allow"})));
    }

    #[tokio::test]
    async fn an_ask_without_a_strong_sender_reports_the_dismissal() {
        // No pump in flight (or the frontend gone): the weak upgrade
        // fails and the ask resolves dismissed instead of hanging.
        let (tx, _rx) = mpsc::unbounded_channel();
        let hub = InteractionHub::new(tx, StreamId::new("s")); // the only strong sender drops here
        assert_eq!(
            hub.capability()
                .request(tabit_protocol::templates::ui::ASK, serde_json::json!({}))
                .await,
            InteractionOutcome::Dismissed
        );
    }

    #[tokio::test]
    async fn clearing_pending_retracts_open_questions_as_dismissed() {
        let (hub, mut rx, _tx) = hub_with_channel();
        let capability = hub.capability();
        let asker = tokio::spawn(async move {
            capability
                .request(tabit_protocol::templates::ui::ASK, serde_json::json!({}))
                .await
        });
        let frame = rx.recv().await.expect("request emitted");
        let (id, _, _) = request_from(&frame);
        hub.clear_pending();
        // The retracted response is a no-op, and the asker resolves
        // dismissed.
        assert!(!hub.respond(&id, serde_json::json!({"option": "Allow"})));
        assert_eq!(
            asker.await.expect("asker finished"),
            InteractionOutcome::Dismissed
        );
    }
}
