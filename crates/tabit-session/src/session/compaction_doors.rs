//! The session-side compaction doors: the surfaces that call into
//! the box ([`crate::compaction`]) with what the session holds. The
//! pre-request leaf is minted at run open; the beat doors run in the
//! worker loop; the overflow intercept runs in the run epilogue
//! (ENGINE.md's compaction amendment).

use super::Session;
use crate::compaction::{self as box_module, Door, Outcome};
use rig_agent::agent::PreRequestSource;
use rig_core::completion::ContextOverflow;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

impl Session {
    /// The pre-request leaf for one run — attached to the engine
    /// request at open (see [`super::run`]'s `open_run`). The engine
    /// awaits it blindly between DECIDE and PREPARE.
    pub(crate) fn pre_request_door(
        &self,
        run_token: &CancellationToken,
    ) -> Arc<dyn PreRequestSource> {
        Arc::new(box_module::PreRequestDoor {
            cell: self.conversation.clone(),
            state: self.compaction.clone(),
            agent: self.agent.clone(),
            config: self.config.clone(),
            selection: self.selection(),
            preamble_chars: self.preamble_chars(),
            token: run_token.clone(),
            notice: self.event_tap.get().cloned(),
        })
    }

    /// The idle door (the beat, after the pump arm): A ∨ B, the
    /// mailbox-empty requirement carried by A. A fresh token in the
    /// abort slot — the abort command does its usual discard plus
    /// terminating the box's stream.
    pub async fn compact_idle(&mut self) {
        let mailbox_empty = !self.mailbox.has_queued();
        self.run_box(Door::Idle, mailbox_empty).await;
    }

    /// The manual door (the `compact` command, parked and served at
    /// the beat): forced; the short-history skip is its only guard.
    pub async fn compact_manual(&mut self) {
        self.run_box(Door::Manual, true).await;
    }

    /// The overflow intercept (the run epilogue): forced, with the
    /// window the error itself reported — the wall teaches the
    /// window. Returns whether the context was compacted (the run
    /// then sets a continue intent and the pump retries the turn).
    /// Only `Compacted` parks the retry — happened **and** fits the
    /// urgent bound. `Oversized` (the guard or the pass cap stopped
    /// with the context still over the bound) must not: the retry
    /// would re-hit the wall.
    pub(crate) async fn compact_after_overflow(&mut self, overflow: &ContextOverflow) -> bool {
        if let Some(window) = overflow.window_tokens {
            self.compaction.note_window(window);
        }
        matches!(
            self.run_box(Door::Overflow, true).await,
            Outcome::Compacted { passes: 1.., .. }
        )
    }

    /// One box invocation over the session's state, under a fresh
    /// token in the abort slot, emitting through the event tap. A
    /// dead tap (no host attached) drops the bracket — the compaction
    /// still runs.
    async fn run_box(&mut self, door: Door, mailbox_empty: bool) -> Outcome {
        let token = {
            let mut slot = crate::lock::lock(&self.abort);
            *slot = CancellationToken::new();
            slot.clone()
        };
        // The agent freshness check: the beat doors serve whatever
        // selection is current, and a stale agent cannot serve a
        // request (the same point-of-use rule as run open).
        if let Err(error) = self.ensure_agent() {
            return Outcome::Failed {
                message: error.to_string(),
                passes: 0,
            };
        }
        let notice = self.event_tap.get().cloned();
        let mut emit = move |event: tabit_protocol::SessionEvent| {
            if let Some(notice) = &notice {
                notice.emit(event);
            }
        };
        box_module::run(
            door,
            &self.conversation,
            &self.compaction,
            &self.agent,
            &token,
            &self.config,
            &self.selection(),
            self.preamble_chars(),
            mailbox_empty,
            &mut emit,
        )
        .await
    }
}
