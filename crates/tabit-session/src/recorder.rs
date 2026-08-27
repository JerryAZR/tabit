//! The persistence hook: records completed assistant turns into the
//! session log as the run produces them.
//!
//! Tool results are recorded from the engine's item stream by the session
//! (they arrive there as message-level results); this hook covers the one
//! record the stream does not itemize per turn - the canonical assistant
//! message with its usage. Commits are memory-first (flag 8): entries
//! chain and take their ids at buffer time, the writer's outbox drains on
//! every commit, and a flush failure flips the persist state to degraded
//! (announced on the notice channel; `run_finished.durable` carries the
//! verdict per run) until the buffer drains again.

use crate::entry::{EntryKind, SessionEntry};
use crate::error::SessionError;
use crate::store::SessionWriter;
use rig_agent::agent::hook::{AgentHook, HookContext, ModelTurnAction, ModelTurnFinished};
use rig_core::completion::Message;
use rig_core::wasm_compat::WasmCompatSend;
use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use tabit_protocol::{EventFrame, SessionEvent, StreamId};

/// Read-only probe over every entry id the session holds — buffered
/// and flushed alike (ids are real at buffer time), plus everything
/// earlier processes wrote (the seed at open). This is what checkout
/// **verification** reads at route time (host-side, synchronous,
/// loop-independent — the same class as the lifecycle builders): a
/// read-only id lookup, never a file re-parse and never a wait on the
/// worker. Insertion happens in the recorder's append path.
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
    /// Every entry id this session holds (see [`EntryIdProbe`]).
    ids: Arc<Mutex<HashSet<String>>>,
    /// The persist-degraded state (flag 8): set while the outbox holds
    /// entries a flush could not place, cleared when it drains. The
    /// transitions ride the notice channel so a frontend can nag about
    /// disk space instead of string-matching run failures.
    degraded: Mutex<bool>,
    /// The persist-state notice channel, attached by the session
    /// worker at spawn (the mailbox-notices pattern: weak, so the
    /// stream ends with the frontend).
    notices: OnceLock<tokio::sync::mpsc::WeakUnboundedSender<EventFrame>>,
    notice_stream: OnceLock<StreamId>,
}

impl SessionRecorder {
    /// Wrap a session writer.
    pub fn new(writer: SessionWriter) -> Self {
        Self {
            writer: Mutex::new(writer),
            ids: Arc::new(Mutex::new(HashSet::new())),
            degraded: Mutex::new(false),
            notices: OnceLock::new(),
            notice_stream: OnceLock::new(),
        }
    }

    /// Attach the persist-state notice channel (the worker, at spawn).
    /// Late or repeated attachment is ignored — one channel per
    /// recorder, the first wins.
    pub fn attach_notices(
        &self,
        sender: tokio::sync::mpsc::WeakUnboundedSender<EventFrame>,
        stream: StreamId,
    ) {
        let _ = self.notices.set(sender);
        let _ = self.notice_stream.set(stream);
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

    /// Whether every commit reached the disk — the `durable` verdict.
    /// Sound once the file is open (and a finishing run implies that:
    /// its prompt went through the barrier): every post-open failure
    /// leaves its entry buffered, so zero pending means the disk holds
    /// everything. Before the file first opens, a failed bootstrap can
    /// lose a record without buffering one — that state announces
    /// `persist_degraded` with `pending: 0` and never coincides with a
    /// `run_finished`.
    pub fn is_clean(&self) -> bool {
        crate::lock::lock(&self.writer).pending() == 0
    }

    /// Append one record to the session log and return its entry id.
    /// The commit is memory-first (flag 8): the entry chains and its
    /// id is real the moment this returns, whether or not the line
    /// reached the disk — a failed flush leaves it buffered, retried
    /// on every subsequent write and at clean exit, and announced
    /// through the degraded notice. Only a failure to even
    /// open/bootstrap the file loses the record (no entry exists to
    /// keep); the empty string stands in for its absent id.
    pub fn record(&self, kind: EntryKind) -> String {
        self.append(None, kind)
    }

    /// Append one record under an announced id (the entry keeps that id
    /// verbatim) and return it. Failure handling as [`Self::record`].
    pub fn record_as(&self, id: &str, kind: EntryKind) -> String {
        self.append(Some(id.to_string()), kind)
    }

    fn append(&self, id: Option<String>, kind: EntryKind) -> String {
        let mut writer = crate::lock::lock(&self.writer);
        match writer.append_with_id(id, kind) {
            Ok(committed) => {
                crate::lock::lock(&self.ids).insert(committed.entry.id.clone());
                self.observe(&mut writer, committed.flush_error);
                committed.entry.id
            }
            Err(error) => {
                self.observe(&mut writer, Some(error));
                String::new()
            }
        }
    }

    /// The prompt barrier (flag 8): record the batch and flush through
    /// it, under one writer lock — nothing interleaves, the batch is
    /// the outbox tail. `Ok` (the ids, already probe-visible) means
    /// every entry is durable and the turn may start; `Err` means the
    /// flush failed and the batch was un-committed (it exists nowhere,
    /// the force-stop equivalent) — the caller hands the texts back as
    /// drafts and runs nothing.
    pub fn commit_barrier(
        &self,
        entries: Vec<(Option<String>, EntryKind)>,
    ) -> Result<Vec<String>, String> {
        let mut writer = crate::lock::lock(&self.writer);
        match writer.commit_barrier(entries) {
            Ok(committed) => {
                let ids: Vec<String> = committed.iter().map(|entry| entry.id.clone()).collect();
                crate::lock::lock(&self.ids).extend(ids.iter().cloned());
                self.observe(&mut writer, None);
                Ok(ids)
            }
            Err(error) => {
                let message = error.to_string();
                self.observe(&mut writer, Some(error));
                Err(message)
            }
        }
    }

    /// Record a rewind: a `rewound` marker plus the leaf move, under
    /// the same single writer. The marker commits to memory like any
    /// entry (its id joins the set — the probe answers for every id
    /// the session holds); a failed flush is announced through the
    /// degraded notice like any other.
    pub fn rewind_to(&self, to: Option<&str>) {
        let mut writer = crate::lock::lock(&self.writer);
        let outcome = writer.rewind_to(to);
        let flush_error = match outcome {
            Ok(committed) => {
                crate::lock::lock(&self.ids).insert(committed.entry.id.clone());
                committed.flush_error
            }
            Err(error) => Some(error),
        };
        self.observe(&mut writer, flush_error);
    }

    /// The clean-exit flush attempt (flag 8): drain whatever the
    /// outbox still holds. A failure keeps the buffer and the
    /// degraded state (nothing is left in-process to surface it, but
    /// the state is not silently swallowed either).
    pub fn flush(&self) {
        let mut writer = crate::lock::lock(&self.writer);
        let result = writer.flush();
        self.observe(&mut writer, result.err());
    }

    /// The resident view for context re-derivation: the durable file
    /// plus the buffered tail (flag 8: under degrade, buffered entries
    /// are conversation truth too). The chain is recomputed from the
    /// writer's buffer-time leaf over the merged entries — appended
    /// tails can branch (a buffered rewind), so file order alone is
    /// not the chain. Repairs belong to the file load alone.
    pub fn load_resident(
        &self,
        durable: crate::store::LoadedSession,
    ) -> Result<(Vec<SessionEntry>, Vec<SessionEntry>), SessionError> {
        let writer = crate::lock::lock(&self.writer);
        if writer.pending() == 0 {
            return Ok((durable.entries, durable.chain));
        }
        let mut entries = durable.entries;
        for line in writer.buffered_lines() {
            // Our own serialization round-tripped through the outbox;
            // a line that cannot parse back is an internal invariant
            // break. Sanctioned crash: see the error doctrine in
            // AGENTS.md.
            #[allow(clippy::expect_used)]
            let entry = serde_json::from_str::<SessionEntry>(&line)
                .expect("outbox lines are entries this process serialized");
            entries.push(entry);
        }
        let path = durable.path.clone();
        let leaf = writer.leaf().map(str::to_string);
        let chain = crate::store::chain_from(&entries, leaf.as_deref(), &path)?;
        Ok((entries, chain))
    }

    /// Fold a flush outcome into the degraded state machine and
    /// announce the transitions. Called with the writer lock held
    /// (the pending count is the writer's). A drain that reports no
    /// error has emptied the outbox, so the error alone decides the
    /// state — a nonzero `pending` always rides an error, and the
    /// degraded message always names its cause.
    fn observe(&self, writer: &mut SessionWriter, error: Option<SessionError>) {
        let pending = writer.pending() as u64;
        let degraded_now = error.is_some();
        let mut state = crate::lock::lock(&self.degraded);
        if *state == degraded_now {
            return;
        }
        *state = degraded_now;
        drop(state);
        let event = match error {
            Some(error) => SessionEvent::error_persist_degraded(pending, error.to_string()),
            None => SessionEvent::error_persist_recovered(),
        };
        self.send_notice(event);
    }

    /// One emission path for the persist-state transitions. A dead or
    /// absent channel is a no-op — the frontend is gone or was never
    /// attached (direct [`Session`] consumers).
    fn send_notice(&self, event: SessionEvent) {
        let Some(sender) = self
            .notices
            .get()
            .and_then(tokio::sync::mpsc::WeakUnboundedSender::upgrade)
        else {
            return;
        };
        let Some(stream) = self.notice_stream.get() else {
            return;
        };
        let _ = sender.send(EventFrame {
            stream: Some(stream.clone()),
            event,
        });
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
