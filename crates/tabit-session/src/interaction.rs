//! The interaction hub — the generic ask pattern (ENGINE.md's tool
//! phase; FRONTEND.md §8 is the wire contract). This module knows
//! nothing about any particular asker: hooks and tools (permission
//! gates, ask-the-user tools) are dev-time or extension policy that
//! lives elsewhere and consumes the capability; their vocabulary and
//! state never leak here.
//!
//! One hub per session worker, the actor's third shared leaf beside
//! the mailbox and abort: [`InteractionHub::ask`] is called from tool
//! bodies and hooks (many producers), registers its delivery closure
//! on the shared ask registry, emits `interaction_request` on the
//! event channel, and awaits; [`InteractionHub::respond`] routes an
//! arriving answer by id to the one awaiting asker — a sync leaf call
//! needing no worker attention. Total semantics: an unknown id or a
//! dead asker is a logged no-op. Every settle site — the first answer
//! (the rest race onto a gone id and drop), the run-terminal
//! retraction, the dead-channel dismissal at registration — emits
//! `interaction_settled` fire-and-forget (the closure's settle arm),
//! so every channel holding the card can close it; run terminals
//! clear the pending map (questions die with their chains — drop is
//! the cancellation).

use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::{mpsc, oneshot};

use rig_agent::tool::interaction::{InteractionOutcome, UserInteraction};
use tabit_protocol::{EventFrame, SessionEvent, StreamId};
use tabit_wire::asks::Outcome;

use crate::ids::new_entry_id;
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
    /// Open questions by id: where the answer payload goes — the
    /// shared registry ([`tabit_wire::asks`]), one law for every
    /// node. The owner key is the session's stream.
    owner: String,
    pending: tabit_wire::asks::PendingAsks,
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
        let owner = stream.as_str().to_string();
        Self {
            inner: Arc::new(Inner {
                notices: NoticeSink::new(&events, stream),
                owner,
                pending: tabit_wire::asks::PendingAsks::default(),
            }),
        }
    }

    /// The capability tools consume: `Arc<dyn UserInteraction>` for
    /// [`rig_agent::tool::ToolContext`]'s typed map.
    pub fn capability(&self) -> Arc<dyn UserInteraction> {
        Arc::new(self.clone())
    }

    /// Deliver an answer. Returns whether the id was ours to answer (a
    /// miss is the total-semantics no-op — the question went away with
    /// its run). The first answer settles the id: the entry is claimed
    /// atomically with the lookup, so a racing second answer finds
    /// nothing and drops, and the settlement is announced to every
    /// channel still holding the card.
    pub fn respond(&self, id: &str, payload: serde_json::Value) -> bool {
        let claimed = self.inner.pending.respond(id, Box::new(payload));
        if !claimed {
            tracing::debug!(
                interaction_id = id,
                "interaction response for an unknown or closed request — dropped"
            );
        }
        claimed
    }

    /// Retract every open question. Called at run terminals: the askers
    /// died with the run, and the senders must not linger. Each
    /// retraction settles its id — a channel that missed whatever
    /// ended the run still learns its card is dead.
    pub fn clear_pending(&self) {
        self.inner.pending.retract_all("the run ended");
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
        let notices = self.inner.notices.clone();
        let settled_id = id.clone();
        self.inner.pending.insert(
            id.clone(),
            &self.inner.owner,
            "interaction",
            move |outcome| {
                // The delivery: resolve the awaiting asker, then
                // announce the settlement — whichever way it settled.
                if let Outcome::Answered(answer) = outcome {
                    let _ = sender.send(tabit_wire::asks::unanswer::<serde_json::Value>(answer));
                }
                let _ = notices.emit(SessionEvent::InteractionSettled {
                    id: settled_id.clone(),
                });
            },
        );
        let sent = self.inner.notices.emit(SessionEvent::InteractionRequest {
            id: id.clone(),
            ui_type: ui_type.to_string(),
            payload,
        });
        if !sent {
            // No pump in flight, or the frontend is already gone: no one
            // will ever answer. The question settles at registration —
            // announced for whoever still consumes the channel, though
            // a dead channel makes that nobody (fail-soft either way).
            self.inner
                .pending
                .orphan(&id, "the event channel is dead at registration");
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
            SessionEvent::InteractionRequest {
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
                    tabit_protocol::templates::ui::SELECT_ANY,
                    serde_json::json!({"prompt": "which file?"}),
                )
                .await
        });

        // The hub is payload-blind: ui_type and payload pass through
        // verbatim, stamped with the session's stream.
        let frame = rx.recv().await.expect("request emitted");
        assert_eq!(frame.stream.as_ref().map(StreamId::as_str), Some("s"));
        let (id, ui_type, payload) = request_from(&frame);
        assert_eq!(ui_type, tabit_protocol::templates::ui::SELECT_ANY);
        assert_eq!(payload, serde_json::json!({"prompt": "which file?"}));

        assert!(hub.respond(&id, serde_json::json!({"text": "main.rs"})));
        assert_eq!(
            asker.await.expect("asker finished"),
            InteractionOutcome::Answered(serde_json::json!({"text": "main.rs"}))
        );
        // Settling is announced: the next frame on the channel closes
        // the card for every holder.
        let settled = rx.recv().await.expect("settlement emitted");
        assert_eq!(
            settled.event,
            SessionEvent::InteractionSettled { id: id.clone() }
        );
        // First answer wins; a racing second answer finds a gone id,
        // drops, and re-announces nothing.
        assert!(!hub.respond(&id, serde_json::json!({"text": "lib.rs"})));
        assert!(rx.try_recv().is_err(), "no second settlement");
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
                .request(
                    tabit_protocol::templates::ui::SELECT_ANY,
                    serde_json::json!({})
                )
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
                .request(
                    tabit_protocol::templates::ui::SELECT_ANY,
                    serde_json::json!({}),
                )
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
        // The retraction settled the id — announced, like every settle
        // site.
        let settled = rx.recv().await.expect("retraction settlement emitted");
        assert_eq!(
            settled.event,
            SessionEvent::InteractionSettled { id: id.clone() }
        );
    }
}
