//! Write-behind durability: the cleanliness verdict, the degraded/
//! recovered notices, and the clean-exit flush.

use super::Session;
use crate::notice::NoticeSink;

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
        let Some(sink) = self.persist_notices.get() else {
            return;
        };
        let event = if degraded {
            tabit_protocol::SessionEvent::error_persist_degraded(
                pending,
                "the session log refuses to flush; records stay queued and retry",
            )
        } else {
            tabit_protocol::SessionEvent::error_persist_recovered()
        };
        sink.emit(event);
    }

    /// The clean-exit flush attempt: one more enqueue retry of
    /// anything queued (the writer's Drop also flushes; this rides the
    /// endpoint's explicit close path).
    pub(crate) fn flush_log(&self) {
        if let Err(error) = crate::lock::lock(&self.buffer).flush_on_exit() {
            tracing::warn!(%error, "the clean-exit flush failed; lines stay queued");
        }
    }

    /// Attach the persist-state notice sink (flag 8's degraded /
    /// recovered events): the entry guard emits them through here.
    /// Called by the session worker at spawn; see [`crate::notice`] for
    /// the channel discipline.
    pub(crate) fn attach_persist_notices(
        &self,
        events: &tokio::sync::mpsc::UnboundedSender<tabit_protocol::EventFrame>,
        stream: tabit_protocol::StreamId,
    ) {
        let _ = self.persist_notices.set(NoticeSink::new(events, stream));
    }
}
