//! The persistence hook: records completed assistant turns into the
//! session log as the run produces them.
//!
//! Tool results are recorded from the engine's item stream by the session
//! (they arrive there as message-level results); this hook covers the one
//! record the stream does not itemize per turn - the canonical assistant
//! message with its usage. Hook methods cannot propagate errors, so the
//! first persistence failure is captured and the session surfaces it
//! loudly when the run returns.

use crate::entry::EntryKind;
use crate::store::SessionWriter;
use rig_agent::agent::hook::{AgentHook, HookContext, ModelTurnAction, ModelTurnFinished};
use rig_core::completion::Message;
use rig_core::wasm_compat::WasmCompatSend;
use std::future::Future;
use std::sync::{Arc, Mutex};

/// Appends records to the session log.
pub struct SessionRecorder {
    writer: Mutex<SessionWriter>,
    /// The first persistence failure, if any; checked by the session when
    /// the run returns.
    first_error: Mutex<Option<String>>,
}

impl SessionRecorder {
    /// Wrap a session writer.
    pub fn new(writer: SessionWriter) -> Self {
        Self {
            writer: Mutex::new(writer),
            first_error: Mutex::new(None),
        }
    }

    /// The first persistence failure, if one occurred. `None` means every
    /// record reached the log.
    pub fn first_error(&self) -> Option<String> {
        match self.first_error.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Append one record to the session log. Persistence failures are
    /// captured (first one wins) and surfaced by the session after the
    /// run - see [`SessionRecorder::first_error`].
    pub fn record(&self, kind: EntryKind) {
        let result = match self.writer.lock() {
            Ok(mut writer) => writer.append(kind),
            Err(poisoned) => poisoned.into_inner().append(kind),
        };
        if let Err(error) = result {
            self.note_error(error);
        }
    }

    /// Record a rewind: a `rewound` marker plus the leaf move, under the
    /// same single writer. Persistence failures are captured like
    /// [`SessionRecorder::record`].
    pub fn rewind_to(&self, to: Option<&str>) {
        let result = match self.writer.lock() {
            Ok(mut writer) => writer.rewind_to(to),
            Err(poisoned) => poisoned.into_inner().rewind_to(to),
        };
        if let Err(error) = result {
            self.note_error(error);
        }
    }

    /// Remember the first persistence failure for the session to surface.
    fn note_error(&self, error: crate::error::SessionError) {
        let message = error.to_string();
        let mut slot = match self.first_error.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if slot.is_none() {
            *slot = Some(message);
        }
    }
}

/// The hook mounted into each request: the session's recorder behind an
/// `Arc`, so the mounted hook and the session's handle are the same object.
/// (A newtype rather than a bare `Arc` impl: rig-agent owns `AgentHook`,
/// so implementing it for `Arc<..>` directly would hit the orphan rule.)
pub struct RecorderHook(pub Arc<SessionRecorder>);

impl AgentHook for RecorderHook {
    fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> impl Future<Output = ModelTurnAction> + WasmCompatSend {
        self.0.record(EntryKind::AssistantMessage {
            message: Message::Assistant {
                id: None,
                content: event.content.clone(),
            },
            usage: event.usage,
        });
        async { ModelTurnAction::Continue }
    }
}
