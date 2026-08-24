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
use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex};

/// Read-only probe over every entry id the file has ever held — the
/// append-only log never removes ids, so the set only grows. This is
/// what checkout **verification** reads at route time (host-side,
/// synchronous, loop-independent — the same class as the lifecycle
/// builders): a read-only id lookup, never a file re-parse and never
/// a wait on the worker. Insertion happens in the recorder's append
/// path; the seed at open covers ids written by earlier processes.
#[derive(Clone)]
pub(crate) struct EntryIdProbe {
    ids: Arc<Mutex<HashSet<String>>>,
}

impl EntryIdProbe {
    /// Whether `id` names an entry in the session file.
    pub(crate) fn contains(&self, id: &str) -> bool {
        crate::lock::lock(&self.ids).contains(id)
    }
}

/// Appends records to the session log.
pub struct SessionRecorder {
    writer: Mutex<SessionWriter>,
    /// The first persistence failure, if any; checked by the session when
    /// the run returns.
    first_error: Mutex<Option<String>>,
    /// Every entry id this file has ever held (see [`EntryIdProbe`]).
    ids: Arc<Mutex<HashSet<String>>>,
}

impl SessionRecorder {
    /// Wrap a session writer.
    pub fn new(writer: SessionWriter) -> Self {
        Self {
            writer: Mutex::new(writer),
            first_error: Mutex::new(None),
            ids: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Seed the id set with entries already in the file (the resume
    /// path — ids written by earlier processes). Ids recorded from
    /// here on insert themselves in [`Self::append`].
    pub fn seed_ids(&self, ids: impl IntoIterator<Item = String>) {
        crate::lock::lock(&self.ids).extend(ids);
    }

    /// The read-only id probe (checkout verification at route time).
    pub fn id_probe(&self) -> EntryIdProbe {
        EntryIdProbe {
            ids: self.ids.clone(),
        }
    }

    /// The first persistence failure, if one occurred. `None` means every
    /// record reached the log.
    pub fn first_error(&self) -> Option<String> {
        crate::lock::lock(&self.first_error).clone()
    }

    /// Append one record to the session log and return its entry id.
    /// Persistence failures are captured (first one wins) and surfaced by
    /// the session after the run - see
    /// [`SessionRecorder::first_error`]; a failed append has no id to
    /// return, so the empty string stands in for it there.
    pub fn record(&self, kind: EntryKind) -> String {
        self.append(None, kind)
    }

    /// Append one record under an announced id (the entry keeps that id
    /// verbatim) and return it. Failure handling as [`Self::record`].
    pub fn record_as(&self, id: &str, kind: EntryKind) -> String {
        self.append(Some(id.to_string()), kind)
    }

    fn append(&self, id: Option<String>, kind: EntryKind) -> String {
        let appended = crate::lock::lock(&self.writer).append_with_id(id, kind);
        match appended {
            Ok(entry) => {
                crate::lock::lock(&self.ids).insert(entry.id.clone());
                entry.id
            }
            Err(error) => {
                self.note_error(error);
                String::new()
            }
        }
    }

    /// Record a rewind: a `rewound` marker plus the leaf move, under the
    /// same single writer. Persistence failures are captured like
    /// [`Self::record`]. The marker's id joins the set — the probe
    /// answers for every id the file holds, exactly like the file.
    pub fn rewind_to(&self, to: Option<&str>) {
        match crate::lock::lock(&self.writer).rewind_to(to) {
            Ok(marker_id) => {
                crate::lock::lock(&self.ids).insert(marker_id);
            }
            Err(error) => self.note_error(error),
        }
    }

    /// Remember the first persistence failure for the session to surface.
    fn note_error(&self, error: crate::error::SessionError) {
        let message = error.to_string();
        let mut slot = crate::lock::lock(&self.first_error);
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
        ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> impl Future<Output = ModelTurnAction> + WasmCompatSend {
        // The turn's entry keeps the id the engine announced for it
        // (ENGINE.md behavior delta 10): live events and the log name the
        // same turn by the same value. Tabit only drives announced runs,
        // so a missing id here is an internal wiring bug, not a state to
        // paper over with a fresh mint (that would silently split the
        // turn's identity in two). Sanctioned crash: see the error
        // doctrine in AGENTS.md.
        #[allow(clippy::expect_used)]
        let turn_id = ctx.turn_id().expect(
            "recorder: model turn finished without an announced turn id - \
             the run was driven without turn announcements",
        );
        self.0.record_as(
            &turn_id,
            EntryKind::AssistantMessage {
                message: Message::Assistant {
                    id: None,
                    content: event.content.clone(),
                },
                usage: event.usage,
            },
        );
        async { ModelTurnAction::Continue }
    }
}
