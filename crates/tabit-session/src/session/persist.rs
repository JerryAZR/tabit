//! Write-behind durability: the cleanliness verdict, the degraded/
//! recovered notices, and the clean-exit flush.

use super::Session;
use crate::lock::lock;
use std::sync::{Arc, Mutex};

/// The persist-notice cell (flag 8): the event channel's weak sender
/// plus the stream id every persist notice is stamped with. Attached by
/// the worker at spawn; `None` before that or after it left.
pub type PersistNotices = Arc<
    Mutex<
        Option<(
            tokio::sync::mpsc::WeakUnboundedSender<tabit_protocol::EventFrame>,
            tabit_protocol::StreamId,
        )>,
    >,
>;

impl Session {
    /// Every commit reached the disk — the `durable` verdict. The
    /// manager's folds enqueue through the same buffer; a clean buffer
    /// means every record landed.
    pub(super) fn buffer_is_clean(&self) -> bool {
        crate::lock::lock(&self.buffer).pending() == 0
    }

    /// Drain any pending persist transitions into the notice events:
    /// a failed enqueue set the writer's degraded flag (true), a
    /// successful retry cleared it (false). Called at the emission
    /// points — the entry guard, conclude — so the notices ride the
    /// same channel the run's events do.
    pub(super) fn drain_persist_transitions(&self) {
        let (transition, pending) = {
            let mut buffer = crate::lock::lock(&self.buffer);
            (buffer.take_degraded_transition(), buffer.pending() as u64)
        };
        let Some(degraded) = transition else {
            return;
        };
        let notices = lock(&self.persist_notices).clone();
        if let Some((sender, stream)) = notices
            && let Some(sender) = sender.upgrade()
        {
            let event = if degraded {
                tabit_protocol::SessionEvent::error_persist_degraded(
                    pending,
                    "the session log refuses to flush; records stay queued and retry",
                )
            } else {
                tabit_protocol::SessionEvent::error_persist_recovered()
            };
            let _ = sender.send(tabit_protocol::EventFrame {
                stream: Some(stream),
                event,
            });
        }
    }

    /// The clean-exit flush attempt: one more enqueue retry of
    /// anything queued (the writer's Drop also flushes; this rides the
    /// endpoint's explicit close path).
    pub(crate) fn flush_log(&self) {
        if let Err(error) = crate::lock::lock(&self.buffer).enqueue(&[]) {
            tracing::warn!(%error, "the clean-exit flush failed; lines stay queued");
        }
    }

    /// Attach the persist-state notice channel (flag 8's degraded /
    /// recovered events): the entry guard emits them through here —
    /// the mailbox-notices pattern: weak, so the channel ends with the
    /// frontend.
    pub(crate) fn attach_persist_notices(
        &self,
        sender: tokio::sync::mpsc::WeakUnboundedSender<tabit_protocol::EventFrame>,
        stream: tabit_protocol::StreamId,
    ) {
        *crate::lock::lock(&self.persist_notices) = Some((sender, stream));
    }
}
