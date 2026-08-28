//! The durable layer's one commit door.
//!
//! The recorder owns the **resident state** — the conversation tree
//! (every branch), the head pointer, the incrementally folded context
//! (the one context builder, the same fold the engine holds per-run),
//! and the cumulative stats ledger — and every change to it passes
//! through one door:
//!
//! **validate → write → grow.** Validation (parent linking, paired
//! tool calls) runs before any state-modifying action; the write
//! queues the records and drains the outbox as one blob; the grow
//! step (tree, context, stats) happens only after the write's verdict,
//! in one step under one lock. An engine contract violation at the
//! door panics loud (AGENTS.md doctrine); a disk that refuses degrades
//! through the persist notices (flag 8) and never blocks the
//! conversation — except at the prompt barrier, the one gated write.
//!
//! The **roundtrip is atomic** (ENGINE.md, the durable roundtrip): an
//! assistant turn stages with its results in the pending slot and
//! commits all-or-none at [`SessionRecorder::close_roundtrip`]. Abort
//! simply drops the slot — nothing half-open ever lands, which is why
//! there is no repair pass. Raw records are never retained; stats grow
//! incrementally as records commit.

use crate::entry::{EntryKind, FileRecord, SessionEntry, SideKind, SideRecord};
use crate::error::SessionError;
use crate::ids;
use crate::parser::Parsed;
use crate::projection;
use crate::stats::UsageLedger;
use crate::tree::SessionTree;
use crate::writer::SessionWriter;
use rig_agent::agent::context::Context;
use rig_agent::agent::hook::{AgentHook, HookContext, ModelTurnAction, ModelTurnFinished};
use rig_core::completion::{Message, Usage};
use rig_core::message::ToolResult;
use rig_core::wasm_compat::WasmCompatSend;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use tabit_protocol::ModelSelection;
use tabit_protocol::{EventFrame, SessionEvent, StreamId};

/// The in-flight roundtrip: the staged assistant turn plus its results
/// as they complete. Single-occupancy — one turn's roundtrip closes
/// (or is discarded) before the next stages.
struct PendingRoundtrip {
    turn_id: String,
    message: Message,
    usage: Usage,
    /// Staged results with the entry ids their events announced.
    results: Vec<(String, ToolResult)>,
}

/// The resident state. Lock order discipline: a path that holds this
/// and needs the writer takes the writer **second** — no path takes
/// them the other way around.
struct Resident {
    /// Every conversation node the session holds, by id — all
    /// branches. Abandoned branches are the checkout targets of the
    /// future.
    tree: SessionTree,
    /// The live projection of the active branch — the one context
    /// builder, the same fold the engine holds per-run. It grows only
    /// through the door, so no run ever re-derives it.
    context: Context,
    /// The model usage currently attributes to (the last
    /// `model_change` through the door), when the session records one.
    attribution: Option<ModelSelection>,
    /// The opening `model_change`, held back so a session that never
    /// runs materializes nothing: it rides the first commit's write.
    pending_register: Option<ModelSelection>,
    /// Cumulative token usage — committed turns and discarded attempts,
    /// every branch.
    stats: UsageLedger,
    /// The roundtrip in flight, when one is.
    pending_roundtrip: Option<PendingRoundtrip>,
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
        crate::lock::lock(&self.resident).tree.contains(id)
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
    /// resumed session fills it through [`Self::adopt`].
    pub fn new(writer: SessionWriter) -> Self {
        Self {
            resident: Arc::new(Mutex::new(Resident {
                tree: SessionTree::empty(),
                context: Context::new(),
                attribution: None,
                pending_register: None,
                stats: UsageLedger::new(),
                pending_roundtrip: None,
            })),
            writer: Mutex::new(writer),
            degraded: Mutex::new(false),
            notices: OnceLock::new(),
            notice_stream: OnceLock::new(),
        }
    }

    /// Fill the resident state from a parsed session (process start,
    /// `open_session`): from here on memory is authoritative and the
    /// file is the write-behind mirror.
    pub fn adopt(&self, parsed: Parsed) {
        let mut resident = crate::lock::lock(&self.resident);
        resident.attribution = parsed.register.clone();
        resident.stats = parsed.stats;
        resident.context = parsed.context;
        resident.tree = parsed.tree;
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

    // === The door: validate → write → grow =================================

    /// The prompt barrier (the outer loop's Draining edge): commit the
    /// opening user batch and flush **through** it — the one gated
    /// write. `Ok` (the ids, real and probe-visible) means every node
    /// is durable and the turn may start; `Err` means the flush failed
    /// and the batch was **never accepted** — no tree, no context, no
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
        // after the gated write proves out.
        let mut parent = resident.tree.head().map(str::to_string);
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
            resident.attribution = Some(selection.clone());
            records.push(register_record(&selection));
        }
        records.extend(entries.iter().map(|entry| FileRecord::Node(entry.clone())));
        let ids: Vec<String> = entries.iter().map(|entry| entry.id.clone()).collect();
        let outcome = {
            let mut writer = crate::lock::lock(&self.writer);
            let outcome = writer.write_gated(&records);
            let message = outcome.as_ref().err().map(|error| error.to_string());
            self.observe(&mut writer, message);
            outcome
        };
        match outcome {
            Ok(()) => {
                for entry in &entries {
                    resident.tree.append(entry.clone());
                    if let EntryKind::UserMessage { message } = &entry.kind {
                        resident.context.fold(message.clone());
                    }
                }
                Ok(ids)
            }
            Err(error) => {
                // The batch (and the register record it carried) was
                // never accepted — put the stash back.
                if let Some(record) = records.first() {
                    resident.pending_register = register_back(record);
                }
                Err(error.to_string())
            }
        }
    }

    /// Stage the completed assistant turn (the recorder hook, fired
    /// while the turn is parked — before hooks' verdicts, before any
    /// fold): it enters the pending slot and commits with its results
    /// at [`Self::close_roundtrip`], or dies with them at
    /// [`Self::discard_roundtrip`]. Nothing touches the tree, the
    /// context, or the file yet.
    #[allow(clippy::panic)] // sanctioned crash: an engine wiring bug, failed loud (AGENTS.md doctrine)
    pub fn stage_assistant(&self, turn_id: &str, message: Message, usage: Usage) {
        let mut resident = crate::lock::lock(&self.resident);
        if let Some(pending) = &resident.pending_roundtrip
            && pending.turn_id != turn_id
        {
            panic!(
                "recorder: turn `{turn_id}` staged while turn `{}`'s roundtrip is still \
                 open — every roundtrip closes or is discarded before the next stages",
                pending.turn_id
            );
        }
        resident.pending_roundtrip = Some(PendingRoundtrip {
            turn_id: turn_id.to_string(),
            message,
            usage,
            results: Vec::new(),
        });
    }

    /// Stage one executed tool result into the open roundtrip and
    /// return its entry id (the id its event announced). The result is
    /// real in memory the moment this returns; it is durable when the
    /// roundtrip closes.
    pub fn stage_result(&self, turn_id: &str, result: ToolResult) -> String {
        let mut resident = crate::lock::lock(&self.resident);
        let entry_id = ids::new_entry_id();
        let pending = open_roundtrip(&mut resident, turn_id);
        pending.results.push((entry_id.clone(), result));
        entry_id
    }

    /// Close the roundtrip: the staged assistant plus its complete
    /// batch commit **as one unit** — all-or-none in the file (one
    /// blob) and in memory (one grow step). Write-behind: a refusing
    /// disk degrades, it never blocks the conversation.
    #[allow(clippy::panic)] // sanctioned crash: an engine wiring bug, failed loud (AGENTS.md doctrine)
    pub fn close_roundtrip(&self, turn_id: &str) {
        let mut resident = crate::lock::lock(&self.resident);
        let Some(pending) = resident.pending_roundtrip.take() else {
            panic!("recorder: roundtrip `{turn_id}` closed without a staged turn");
        };
        if pending.turn_id != turn_id {
            panic!(
                "recorder: roundtrip `{turn_id}` closed while `{}` was staged",
                pending.turn_id
            );
        }
        // Validation before any state-modifying action: the assistant's
        // calls are answered exactly once by the staged results. An
        // unpaired batch is an engine contract violation — internal,
        // fail loud.
        let closing: Vec<ToolResult> = pending
            .results
            .iter()
            .map(|(_, result)| result.clone())
            .collect();
        validate_paired(&pending.message, &closing);

        let mut records = Vec::new();
        let mut nodes = Vec::new();
        let assistant = SessionEntry::with_id(
            pending.turn_id.clone(),
            resident.tree.head().map(str::to_string),
            ids::now_rfc3339(),
            EntryKind::AssistantMessage {
                message: pending.message.clone(),
                usage: pending.usage,
            },
        );
        records.push(FileRecord::Node(assistant.clone()));
        nodes.push((assistant, None));
        for (entry_id, result) in &pending.results {
            let entry = SessionEntry::with_id(
                entry_id.clone(),
                nodes.last().map(|(last, _)| last.id.clone()),
                ids::now_rfc3339(),
                EntryKind::ToolResult {
                    result: result.clone(),
                },
            );
            records.push(FileRecord::Node(entry.clone()));
            nodes.push((entry, Some(result.clone())));
        }
        // The grow step folds the batch's results as ONE user message —
        // the same shape the engine folds at settlement — between the
        // assistant and whatever follows.
        let mut batch_results: Vec<ToolResult> = Vec::new();
        self.write_and_grow(&mut resident, &records, |resident| {
            for (entry, result) in &nodes {
                resident.tree.append(entry.clone());
                match result {
                    Some(result) => batch_results.push(result.clone()),
                    None => {
                        flush_batch(resident, &mut batch_results);
                        if let EntryKind::AssistantMessage { message, .. } = &entry.kind {
                            resident.context.fold(message.clone());
                        }
                    }
                }
            }
            flush_batch(resident, &mut batch_results);
            let (provider, model, level) = attribution_of(resident);
            resident
                .stats
                .add(&provider, &model, level.as_deref(), pending.usage);
        });
    }

    /// Discard the open roundtrip (a hook veto or a malformed-tool-call
    /// defect, retried): the tokens were spent, so the attempt's usage
    /// commits as a `discarded` side record (flag 22) — the log stays
    /// the cost source of truth — and the slot dies without landing.
    /// `usage` is the attempt's completion-call usage.
    #[allow(clippy::panic)] // sanctioned crash: an engine wiring bug, failed loud (AGENTS.md doctrine)
    pub fn discard_roundtrip(&self, turn_id: &str, usage: Usage) {
        let mut resident = crate::lock::lock(&self.resident);
        if let Some(pending) = &resident.pending_roundtrip
            && pending.turn_id != turn_id
        {
            panic!(
                "recorder: roundtrip `{turn_id}` discarded while `{}` was staged",
                pending.turn_id
            );
        }
        // A matched discard (or nothing staged — the defect path dies
        // before its turn parks): the slot dies either way.
        resident.pending_roundtrip = None;
        let record = FileRecord::Side(SideRecord {
            timestamp: ids::now_rfc3339(),
            kind: SideKind::Discarded { usage },
        });
        self.write_and_grow(&mut resident, &[record], |resident| {
            let (provider, model, level) = attribution_of(resident);
            resident
                .stats
                .add(&provider, &model, level.as_deref(), usage);
        });
    }

    /// Drop the open roundtrip without a trace (the abort epilogue):
    /// nothing half-open ever lands, so there is nothing to repair.
    pub fn drop_open_roundtrip(&self) {
        crate::lock::lock(&self.resident).pending_roundtrip = None;
    }

    /// A drained steering batch: one user node per message, in drain
    /// order, under the id its `message_queued` announced. Write-behind.
    pub fn commit_steer(&self, entry_id: &str, message: Message) {
        let mut resident = crate::lock::lock(&self.resident);
        let entry = SessionEntry::with_id(
            entry_id.to_string(),
            resident.tree.head().map(str::to_string),
            ids::now_rfc3339(),
            EntryKind::UserMessage { message },
        );
        let record = FileRecord::Node(entry.clone());
        self.write_and_grow(&mut resident, &[record], |resident| {
            if let EntryKind::UserMessage { message } = &entry.kind {
                resident.context.fold(message.clone());
            }
            resident.tree.append(entry.clone());
        });
    }

    /// Commit one side record — session state outside the tree. The
    /// record's semantics (the register cell, the discard's ledger
    /// entry) are applied by the door as part of the grow step; this is
    /// the durable trace. Write-behind.
    pub fn record_side(&self, kind: SideKind) {
        let mut resident = crate::lock::lock(&self.resident);
        let record = FileRecord::Side(SideRecord {
            timestamp: ids::now_rfc3339(),
            kind,
        });
        self.write_and_grow(&mut resident, std::slice::from_ref(&record), |resident| {
            apply_side(resident, &record);
        });
    }

    /// Hold the opening `model_change` back until the session's first
    /// commit — the deferred-creation contract (a session that never
    /// runs materializes nothing).
    pub fn defer_register(&self, selection: ModelSelection) {
        crate::lock::lock(&self.resident).pending_register = Some(selection);
    }

    /// Move the head to `to` (checkout): validate the target against
    /// the resident tree and the closed-path rule, write the `checkout`
    /// side record, re-project the context from the new branch. No file
    /// read — abandoned branches live in the tree. Returns the new
    /// branch's length.
    ///
    /// A target inside an open tool roundtrip panics (the owner's
    /// flag-23 ruling: not supported, revisited later) — `rewind(n)`
    /// targets user messages and never trips this.
    #[allow(clippy::panic, clippy::panic_in_result_fn, clippy::expect_used)] // sanctioned crash: the ruled flag-23 panic; the target was validated against the same tree (AGENTS.md doctrine)
    pub fn checkout(
        &self,
        to: Option<&str>,
        path: &std::path::Path,
    ) -> Result<usize, SessionError> {
        let mut resident = crate::lock::lock(&self.resident);
        let branch = match to {
            None => Vec::new(), // the root: an empty conversation
            Some(to) => {
                if !resident.tree.contains(to) {
                    return Err(SessionError::Corrupt {
                        path: path.to_path_buf(),
                        message: format!("checkout target `{to}` is not in this session"),
                    });
                }
                resident
                    .tree
                    .path_to(Some(to))
                    .map_err(|fault| SessionError::Corrupt {
                        path: path.to_path_buf(),
                        message: fault.0,
                    })?
            }
        };
        if let Err(fault) = projection::path_is_closed(&branch) {
            panic!(
                "checkout target `{}` sits inside an open tool roundtrip: {fault} \
                 — mid-roundtrip checkouts are not supported (revisit later)",
                to.unwrap_or_default()
            );
        }
        let record = FileRecord::Side(SideRecord {
            timestamp: ids::now_rfc3339(),
            kind: SideKind::Checkout {
                to: to.map(str::to_string),
            },
        });
        self.write_and_grow(&mut resident, &[record], |resident| {
            resident
                .tree
                .move_head(to)
                .expect("the target was validated against the same tree");
            resident.context = projection::fold_branch(&branch);
        });
        Ok(branch.len())
    }

    // === Reads =============================================================

    /// The projected model-visible context (what the next outer loop
    /// sees) — a snapshot of the live projection.
    pub fn context(&self) -> Vec<Message> {
        crate::lock::lock(&self.resident)
            .context
            .messages()
            .to_vec()
    }

    /// The active branch, root → head — the temporary path container,
    /// materialized on demand for replay and checkout reporting. Never
    /// maintained as state.
    pub fn active_branch(&self) -> Vec<SessionEntry> {
        crate::lock::lock(&self.resident).tree.path_to_head()
    }

    /// The cumulative stats ledger (a snapshot; costs are derived at
    /// the session facade).
    pub fn stats(&self) -> UsageLedger {
        crate::lock::lock(&self.resident).stats.clone()
    }

    /// The clean-exit flush attempt (flag 8): drain whatever the outbox
    /// still holds. A failure keeps the buffer and the degraded state
    /// (nothing is left in-process to surface it, but the state is not
    /// silently swallowed either).
    pub fn flush(&self) {
        let mut writer = crate::lock::lock(&self.writer);
        let result = writer.flush();
        self.observe(&mut writer, result.err().map(|error| error.to_string()));
    }

    // === The door's shared core ============================================

    /// The write-then-grow half of the door: one write-behind attempt
    /// (records as one blob, degraded on failure), then the grow step —
    /// one closure, under the resident lock, after the verdict. The
    /// write is behind memory: a failure keeps the lines queued for the
    /// retry and the verdict rides the notices.
    fn write_and_grow(
        &self,
        resident: &mut Resident,
        records: &[FileRecord],
        grow: impl FnOnce(&mut Resident),
    ) {
        {
            let mut writer = crate::lock::lock(&self.writer);
            let error = writer.write_behind(records).map(|error| error.to_string());
            self.observe(&mut writer, error);
        }
        grow(resident);
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

/// The open roundtrip for `turn_id`, or a loud crash — staging into a
/// turn that never staged its assistant is an engine wiring bug.
#[allow(clippy::panic)] // sanctioned crash: an engine wiring bug, failed loud (AGENTS.md doctrine)
fn open_roundtrip<'a>(resident: &'a mut Resident, turn_id: &str) -> &'a mut PendingRoundtrip {
    match &mut resident.pending_roundtrip {
        Some(pending) if pending.turn_id == turn_id => pending,
        _ => panic!("recorder: result staged for turn `{turn_id}` without its staged assistant"),
    }
}

/// The pairing law at the door: every call of the assistant message is
/// answered exactly once by the closing results, and nothing answers a
/// call that was never made. An engine contract violation — panic.
#[allow(clippy::panic)] // sanctioned crash: an engine wiring bug, failed loud (AGENTS.md doctrine)
fn validate_paired(message: &Message, closing: &[ToolResult]) {
    let mut unanswered: Vec<String> = projection::calls_of(message)
        .iter()
        .map(|call| call.id.clone())
        .collect();
    for result in closing {
        let Some(index) = unanswered.iter().position(|id| *id == result.id) else {
            panic!(
                "recorder: roundtrip closed with a result answering no call (`{}`)",
                result.id
            );
        };
        unanswered.swap_remove(index);
    }
    if !unanswered.is_empty() {
        panic!(
            "recorder: roundtrip closed with unanswered call(s) {unanswered:?} — \
             the engine commits a batch only when every call is answered"
        );
    }
}

/// The attribution triple for the ledger (owned — the caller mutates
/// the resident right after), from the resident register.
fn attribution_of(resident: &Resident) -> (String, String, Option<String>) {
    match &resident.attribution {
        Some(selection) => (
            selection.provider.clone(),
            selection.model.clone(),
            selection.thinking_level.clone(),
        ),
        None => (String::new(), String::new(), None),
    }
}

/// Fold any staged tool results into the context as one user message.
#[allow(clippy::expect_used)] // unreachable: guarded by the empty check above (AGENTS.md doctrine)
fn flush_batch(resident: &mut Resident, batch_results: &mut Vec<ToolResult>) {
    if batch_results.is_empty() {
        return;
    }
    let results = std::mem::take(batch_results);
    let content = rig_core::OneOrMany::from_iter_optional(
        results
            .into_iter()
            .map(rig_core::message::UserContent::ToolResult)
            .collect::<Vec<_>>(),
    )
    .expect("non-empty by the guard above");
    resident.context.fold(Message::User { content });
}

/// A side record's semantics inside the grow step (the register cell,
/// the discard's ledger entry); `checkout` and `aborted` carry none —
/// their meaning lives in the caller's state changes.
fn apply_side(resident: &mut Resident, record: &FileRecord) {
    let FileRecord::Side(side) = record else {
        return;
    };
    match &side.kind {
        SideKind::ModelChange {
            provider,
            model,
            thinking_level,
        } => {
            resident.attribution = Some(ModelSelection {
                provider: provider.clone(),
                model: model.clone(),
                thinking_level: thinking_level.clone(),
            });
            resident.pending_register = None;
        }
        SideKind::Discarded { usage } => {
            let (provider, model, level) = attribution_of(resident);
            resident
                .stats
                .add(&provider, &model, level.as_deref(), *usage);
        }
        SideKind::Checkout { .. }
        | SideKind::Aborted
        | SideKind::Label { .. }
        | SideKind::Custom { .. } => {}
    }
}

/// The register's side-record shape.
fn register_record(selection: &ModelSelection) -> FileRecord {
    FileRecord::Side(SideRecord {
        timestamp: ids::now_rfc3339(),
        kind: SideKind::ModelChange {
            provider: selection.provider.clone(),
            model: selection.model.clone(),
            thinking_level: selection.thinking_level.clone(),
        },
    })
}

/// Rebuild the stashed selection from a barrier record list's leading
/// model_change (the gated write's rollback).
fn register_back(record: &FileRecord) -> Option<ModelSelection> {
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
        // The staged turn keeps the id the engine announced for it
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
        // The turn is parked (folded nowhere, hooks observing): staging
        // here means the roundtrip commits — or dies — with its results
        // at close, atomically. The usage rides the assistant node; the
        // same shape the engine's fold carries.
        self.0.stage_assistant(
            &turn_id,
            Message::Assistant {
                id: Some(turn_id.clone()),
                content: event.content.clone(),
            },
            event.usage,
        );
        async { ModelTurnAction::Continue }
    }
}

#[cfg(test)]
#[path = "recorder_tests.rs"]
mod tests;
