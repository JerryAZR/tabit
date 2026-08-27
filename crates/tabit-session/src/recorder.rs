//! The persistence and residency hook: the session's in-memory truth
//! and its write-behind mirror.
//!
//! The recorder owns the **resident state** — the conversation tree
//! (every branch), the head pointer (the branch being grown), the live
//! projection of that branch into model-facing context, and the
//! file-order record sequence (stats attribution) — because every
//! producing site already passes through it: the run loop's tool
//! results, the engine hook's assistant turns, the prompt barrier's
//! user batch, the model register, and checkout. Records are
//! committed memory-first (flag 8): the node enters the tree and the
//! context the moment it is recorded, its line joins the writer's
//! outbox, and the drain retries until the disk accepts it — a
//! refusing disk degrades (announced on the notice channel;
//! `run_finished.durable` carries the verdict per run), it never
//! blocks the conversation.
//!
//! Nothing re-reads the file mid-session. The file is the handoff
//! between processes and the archive of abandoned branches; the
//! loader ([`SessionRecorder::load`]) parses it once, folds the tree,
//! the head, and the selection register, and from then on memory is
//! authoritative. The path array exists only as a temporary container
//! inside load and checkout (walk head→root, reverse, project).

use crate::entry::{EntryKind, FileRecord, SessionEntry, SideKind, SideRecord};
use crate::error::SessionError;
use crate::ids;
use crate::projection::fold_node;
use crate::store::{LoadedSession, SessionWriter};
use rig_agent::agent::conversation::{Conversation, interrupted_results};
use rig_agent::agent::hook::{AgentHook, HookContext, ModelTurnAction, ModelTurnFinished};
use rig_core::completion::Message;
use rig_core::wasm_compat::WasmCompatSend;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use tabit_protocol::ModelSelection;
use tabit_protocol::{EventFrame, SessionEvent, StreamId};

/// The in-memory session truth. Lock order discipline: a path that
/// holds this and needs the writer takes the writer **second** — no
/// path takes them the other way around.
struct Resident {
    /// Every conversation node the session holds, by id — all
    /// branches, not just the active one. Abandoned branches are the
    /// checkout targets of the future.
    tree: HashMap<String, SessionEntry>,
    /// The opening `model_change`, held back so a session that never
    /// runs materializes nothing: it rides the first barrier's drain
    /// (superseded and cleared the moment any register write lands).
    pending_register: Option<ModelSelection>,
    /// The node the conversation currently ends at — the branch being
    /// grown. `None` is the root (an empty conversation). Appends
    /// attach as children of the head; checkout moves the pointer.
    head: Option<String>,
    /// The live projection of the active branch — the **one context
    /// builder** (`rig_agent::agent::conversation::Conversation`), the
    /// same fold the engine holds per-run. Context grows one node at a
    /// time here, so no run ever re-derives it.
    conversation: Conversation,
    /// The file-order sequence of every record (nodes and side
    /// records alike) — what the file will hold once drained; stats
    /// attribution walks it.
    records: Vec<FileRecord>,
}

/// What one load produced: the folded selection register, the active
/// branch's context, and how many dangling tool calls were repaired.
#[derive(Debug)]
pub struct Loaded {
    /// The last `model_change` side record, when the file has one.
    pub selection: Option<ModelSelection>,
    /// The projected context of the active branch (repairs included).
    pub context: Vec<Message>,
    /// How many interrupted tool calls received synthesized results.
    pub repaired_tool_calls: usize,
}

/// Read-only probe over the resident tree — what checkout
/// **verification** reads at route time (host-side, synchronous,
/// loop-independent — the same class as the lifecycle builders): an
/// O(1) node lookup, never a file re-parse and never a wait on the
/// worker.
#[derive(Clone)]
pub(crate) struct EntryIdProbe {
    resident: Arc<Mutex<Resident>>,
}

impl EntryIdProbe {
    /// Whether `id` names a node in the session (any branch).
    pub(crate) fn contains(&self, id: &str) -> bool {
        crate::lock::lock(&self.resident).tree.contains_key(id)
    }
}

/// The session's resident state and its write-behind mirror.
pub struct SessionRecorder {
    resident: Arc<Mutex<Resident>>,
    writer: Mutex<SessionWriter>,
    /// The persist-degraded state (flag 8): set while the outbox holds
    /// records a flush could not place, cleared when it drains. The
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
    /// Wrap a session writer. The resident state starts empty — a
    /// resumed session fills it through [`Self::load`].
    pub fn new(writer: SessionWriter) -> Self {
        Self {
            resident: Arc::new(Mutex::new(Resident {
                tree: HashMap::new(),
                pending_register: None,
                head: None,
                conversation: Conversation::new(),
                records: Vec::new(),
            })),
            writer: Mutex::new(writer),
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

    /// The read-only node probe (checkout verification at route time).
    pub fn id_probe(&self) -> EntryIdProbe {
        EntryIdProbe {
            resident: self.resident.clone(),
        }
    }

    /// Whether every commit reached the disk — the `durable` verdict.
    /// Sound once the file is open (and a finishing run implies that:
    /// its prompt went through the barrier): every post-open failure
    /// leaves its record buffered, so zero pending means the disk holds
    /// everything.
    pub fn is_clean(&self) -> bool {
        crate::lock::lock(&self.writer).pending() == 0
    }

    /// The one-pass load fold (process start, `open_session`): build
    /// the tree from the parsed records, walk the head (appends
    /// advance it, `checkout` side records move it), fold the
    /// selection register (last `model_change` wins), project the
    /// active branch, and repair any dangling tool-use roundtrip the
    /// previous process left behind — a crash and an abort leave the
    /// same shape.
    pub fn load(&self, loaded: LoadedSession) -> Result<Loaded, SessionError> {
        let mut resident = crate::lock::lock(&self.resident);
        let mut selection: Option<ModelSelection> = None;
        for record in &loaded.records {
            match record {
                FileRecord::Node(entry) => {
                    // The append invariant: every node's parent is the
                    // head at the time it was appended. A violation is
                    // corruption — fail loudly, never guess.
                    if entry.parent_id != resident.head {
                        return Err(SessionError::Corrupt {
                            path: loaded.path.clone(),
                            message: format!(
                                "entry `{}` parents `{:?}` but the head at that point was `{:?}`",
                                entry.id, entry.parent_id, resident.head
                            ),
                        });
                    }
                    resident.head = Some(entry.id.clone());
                    resident.tree.insert(entry.id.clone(), entry.clone());
                }
                FileRecord::Side(SideRecord { kind, .. }) => match kind {
                    SideKind::ModelChange {
                        provider,
                        model,
                        thinking_level,
                    } => {
                        selection = Some(ModelSelection {
                            provider: provider.clone(),
                            model: model.clone(),
                            thinking_level: thinking_level.clone(),
                        });
                    }
                    SideKind::Checkout { to } => {
                        if let Some(to) = to
                            && !resident.tree.contains_key(to)
                        {
                            return Err(SessionError::Corrupt {
                                path: loaded.path.clone(),
                                message: format!("checkout targets unknown or later node `{to}`"),
                            });
                        }
                        resident.head = to.clone();
                    }
                    SideKind::Aborted | SideKind::Label { .. } | SideKind::Custom { .. } => {}
                },
            }
        }
        resident.records = loaded.records;
        let branch = walk(&resident, resident.head.clone(), &loaded.path)?;
        for entry in &branch {
            fold_node(&mut resident.conversation, entry);
        }
        let repaired = self.repair_dangling_locked(&mut resident, &loaded.path);
        let context = resident.conversation.messages_vec();
        Ok(Loaded {
            selection,
            context,
            repaired_tool_calls: repaired,
        })
    }

    /// Append one conversation node under a minted id and return it.
    /// The node is real the moment this returns — tree, head, context,
    /// outbox — whether or not the line reached the disk.
    pub fn record(&self, kind: EntryKind) -> String {
        self.node(None, kind)
    }

    /// Append one conversation node under an announced id (the entry
    /// keeps that id verbatim) and return it. Failure handling as
    /// [`Self::record`].
    pub fn record_as(&self, id: &str, kind: EntryKind) -> String {
        self.node(Some(id.to_string()), kind)
    }

    /// Append one side record — session state outside the tree. The
    /// register's `model_change`, checkout's pointer move, the abort
    /// marker: all bookkeeping, all riding the same write-behind
    /// outbox. The record's semantics (the register cell, the head
    /// move) were applied by the caller; this is the durable trace.
    pub fn record_side(&self, kind: SideKind) {
        let record = FileRecord::Side(SideRecord {
            timestamp: ids::now_rfc3339(),
            kind,
        });
        {
            let mut writer = crate::lock::lock(&self.writer);
            let error = writer.append_record(&record);
            self.observe(&mut writer, error.map(|error| error.to_string()));
        }
        let mut resident = crate::lock::lock(&self.resident);
        resident.pending_register = None;
        resident.records.push(record);
    }

    /// Hold the opening `model_change` back until the session's first
    /// barrier — the deferred-creation contract (a session that never
    /// runs materializes nothing).
    pub fn defer_register(&self, selection: ModelSelection) {
        crate::lock::lock(&self.resident).pending_register = Some(selection);
    }

    /// The prompt barrier (flag 8): commit the batch and flush through
    /// it, under one resident lock — nothing interleaves. `Ok` (the
    /// ids, real and probe-visible) means every node is durable and
    /// the turn may start; `Err` means the flush failed and the batch
    /// was **never accepted** — no tree, no head move, no context, no
    /// line (the force-stop equivalent) — the caller hands the texts
    /// back as drafts and runs nothing.
    pub fn commit_barrier(
        &self,
        batch: Vec<(Option<String>, EntryKind)>,
    ) -> Result<Vec<String>, String> {
        let mut resident = crate::lock::lock(&self.resident);
        // Construct against the running head — the batch chains: each
        // node's parent is the one before it, the first parented to the
        // current head. Mutate nothing yet — the batch is accepted only
        // after the flush proves out (validate-then-commit, the
        // checkout pattern).
        let mut parent = resident.head.clone();
        let entries: Vec<SessionEntry> = batch
            .into_iter()
            .map(|(id, kind)| {
                let entry = SessionEntry::with_id(
                    id.unwrap_or_else(ids::new_entry_id),
                    parent.clone(),
                    ids::now_rfc3339(),
                    kind,
                );
                parent = Some(entry.id.clone());
                entry
            })
            .collect();
        let mut records: Vec<FileRecord> = Vec::new();
        if let Some(selection) = resident.pending_register.take() {
            records.push(FileRecord::Side(SideRecord {
                timestamp: ids::now_rfc3339(),
                kind: SideKind::ModelChange {
                    provider: selection.provider.clone(),
                    model: selection.model.clone(),
                    thinking_level: selection.thinking_level.clone(),
                },
            }));
        }
        records.extend(entries.iter().map(|entry| FileRecord::Node(entry.clone())));
        let ids: Vec<String> = entries.iter().map(|entry| entry.id.clone()).collect();
        let outcome = {
            let mut writer = crate::lock::lock(&self.writer);
            let outcome = writer.commit_records(&records);
            let message = outcome.as_ref().err().map(|error| error.to_string());
            self.observe(&mut writer, message);
            outcome
        };
        match outcome {
            Ok(()) => {
                for (entry, record) in entries.iter().zip(records) {
                    resident.tree.insert(entry.id.clone(), entry.clone());
                    resident.head = Some(entry.id.clone());
                    fold_node(&mut resident.conversation, entry);
                    resident.records.push(record);
                }
                Ok(ids)
            }
            Err(error) => {
                // The batch (and the register record it carried) was
                // never accepted — put the stash back.
                if let Some(record) = records.first() {
                    resident.pending_register = lock_selection_back(record);
                }
                Err(error.to_string())
            }
        }
    }

    /// Move the head to `to` (checkout): validate the target against
    /// the resident tree, write the `checkout` side record, re-project
    /// the context from the new branch, and repair a mid-batch landing
    /// point the same way load does. Returns the new branch's length.
    /// No file read — abandoned branches live in the tree.
    pub fn checkout(
        &self,
        to: Option<&str>,
        path: &std::path::Path,
    ) -> Result<usize, SessionError> {
        let mut resident = crate::lock::lock(&self.resident);
        if let Some(to) = to
            && !resident.tree.contains_key(to)
        {
            return Err(SessionError::Corrupt {
                path: path.to_path_buf(),
                message: format!("checkout target `{to}` is not in this session"),
            });
        }
        let branch = walk(&resident, to.map(str::to_string), path)?;
        let mut conversation = Conversation::new();
        for entry in &branch {
            fold_node(&mut conversation, entry);
        }
        let record = FileRecord::Side(SideRecord {
            timestamp: ids::now_rfc3339(),
            kind: SideKind::Checkout {
                to: to.map(str::to_string),
            },
        });
        {
            let mut writer = crate::lock::lock(&self.writer);
            let error = writer.append_record(&record);
            self.observe(&mut writer, error.map(|error| error.to_string()));
        }
        resident.head = to.map(str::to_string);
        resident.conversation = conversation;
        resident.records.push(record);
        self.repair_dangling_locked(&mut resident, path);
        Ok(branch.len())
    }

    /// The repair pass for a dangling tool-use roundtrip (an aborted
    /// or crashed run): synthesize one "interrupted" result per
    /// unanswered call and append it as a node, so the branch replays
    /// cleanly. Returns how many calls were repaired.
    pub fn repair_dangling(&self, path: &std::path::Path) -> usize {
        let mut resident = crate::lock::lock(&self.resident);
        self.repair_dangling_locked(&mut resident, path)
    }

    fn repair_dangling_locked(&self, resident: &mut Resident, path: &std::path::Path) -> usize {
        let Some(dangling) = resident.conversation.dangling() else {
            return 0;
        };
        let results = interrupted_results(&dangling);
        let repaired = results.len();
        for result in results {
            let entry = SessionEntry::new(
                resident.head.clone(),
                ids::now_rfc3339(),
                EntryKind::ToolResult { result },
            );
            let record = FileRecord::Node(entry.clone());
            {
                let mut writer = crate::lock::lock(&self.writer);
                let error = writer.append_record(&record);
                self.observe(&mut writer, error.map(|error| error.to_string()));
            }
            resident.tree.insert(entry.id.clone(), entry.clone());
            resident.head = Some(entry.id.clone());
            fold_node(&mut resident.conversation, &entry);
            resident.records.push(record);
        }
        let _ = path;
        repaired
    }

    /// The projected model-visible context (what the next outer loop
    /// sees) — a snapshot of the live projection.
    pub fn context(&self) -> Vec<Message> {
        crate::lock::lock(&self.resident)
            .conversation
            .messages_vec()
    }

    /// The active branch, root → head — the temporary path container,
    /// materialized on demand for replay and checkout reporting. Never
    /// maintained as state.
    pub fn active_branch(&self) -> Vec<SessionEntry> {
        let resident = crate::lock::lock(&self.resident);
        walk(&resident, resident.head.clone(), std::path::Path::new("")).unwrap_or_default()
    }

    /// The file-order record sequence — what stats attribution walks.
    pub fn records(&self) -> Vec<FileRecord> {
        crate::lock::lock(&self.resident).records.clone()
    }

    /// The clean-exit flush attempt (flag 8): drain whatever the
    /// outbox still holds. A failure keeps the buffer and the
    /// degraded state (nothing is left in-process to surface it, but
    /// the state is not silently swallowed either).
    pub fn flush(&self) {
        let mut writer = crate::lock::lock(&self.writer);
        let result = writer.flush();
        self.observe(&mut writer, result.err().map(|error| error.to_string()));
    }

    fn node(&self, id: Option<String>, kind: EntryKind) -> String {
        let mut resident = crate::lock::lock(&self.resident);
        let entry = SessionEntry::with_id(
            id.unwrap_or_else(ids::new_entry_id),
            resident.head.clone(),
            ids::now_rfc3339(),
            kind,
        );
        let record = FileRecord::Node(entry.clone());
        {
            let mut writer = crate::lock::lock(&self.writer);
            let error = writer.append_record(&record);
            self.observe(&mut writer, error.map(|error| error.to_string()));
        }
        resident.tree.insert(entry.id.clone(), entry.clone());
        resident.head = Some(entry.id.clone());
        fold_node(&mut resident.conversation, &entry);
        resident.records.push(record);
        entry.id
    }

    /// Fold a flush outcome into the degraded state machine and
    /// announce the transitions. Called with the writer lock held
    /// (the pending count is the writer's). A drain that reports no
    /// error has emptied the outbox, so the error alone decides the
    /// state — a nonzero `pending` always rides an error, and the
    /// degraded message always names its cause.
    fn observe(&self, writer: &mut SessionWriter, message: Option<String>) {
        let pending = writer.pending() as u64;
        let degraded_now = message.is_some();
        let mut state = crate::lock::lock(&self.degraded);
        if *state == degraded_now {
            return;
        }
        *state = degraded_now;
        drop(state);
        let event = match message {
            Some(message) => SessionEvent::error_persist_degraded(pending, message),
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

/// Rebuild the stashed selection from a barrier record list's leading
/// model_change (the validate-then-commit rollback).
fn lock_selection_back(record: &FileRecord) -> Option<ModelSelection> {
    let FileRecord::Side(SideRecord {
        kind:
            SideKind::ModelChange {
                provider,
                model,
                thinking_level,
            },
        ..
    }) = record
    else {
        return None;
    };
    Some(ModelSelection {
        provider: provider.clone(),
        model: model.clone(),
        thinking_level: thinking_level.clone(),
    })
}

/// Walk the branch ending at `to` (default: the resident head) back to
/// the root, reversed into root→head order. A broken parent link is
/// corruption — load-time parsing guarantees every link resolves, and
/// in-memory inserts never break one.
fn walk(
    resident: &Resident,
    to: Option<String>,
    path: &std::path::Path,
) -> Result<Vec<SessionEntry>, SessionError> {
    let Some(mut current) = to else {
        return Ok(Vec::new());
    };
    let mut branch = Vec::new();
    loop {
        let entry = resident
            .tree
            .get(&current)
            .ok_or_else(|| SessionError::Corrupt {
                path: path.to_path_buf(),
                message: format!("branch walks through missing node `{current}`"),
            })?;
        branch.push(entry.clone());
        match &entry.parent_id {
            Some(parent) => current = parent.clone(),
            None => return Ok(branch.into_iter().rev().collect()),
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
        // The turn's node keeps the id the engine announced for it
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
                // The announced turn id rides the message — the same
                // shape the engine's conversation carries, so the
                // durable fold and the in-run builder agree on every
                // field.
                message: Message::Assistant {
                    id: Some(turn_id.clone()),
                    content: event.content.clone(),
                },
                usage: event.usage,
            },
        );
        async { ModelTurnAction::Continue }
    }
}

#[cfg(test)]
#[path = "recorder_tests.rs"]
mod tests;
