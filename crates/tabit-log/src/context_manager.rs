//! The conversation's single source of truth.
//!
//! [`ContextManager`] owns the tree privately and commits through a
//! shared [`WriteBuffer`](crate::writer::WriteBuffer) handle — the
//! buffer is not owned, because non-context records (side facts) ride
//! the same one from the session side. The model-facing context is a
//! **view**: `messages` folds the active branch on every call through
//! the one context builder ([`fold_branch`](crate::fold::fold_branch)),
//! so live and reload are literally the same derivation and there is no
//! stored context to drift.
//!
//! The interface is the engine context's old one — `fold`, `fold_all`,
//! `messages` — with the unified commit underneath: **verify first,
//! then commit**. A message (or roundtrip batch) is checked whole, and
//! only then do its records enter the buffer and the tree grow, in the
//! same operation. Tool calls never enter the context without results:
//! a tool-carrying assistant folds only as the head of a complete
//! [`fold_all`](ContextManager::fold_all) batch, so nothing half-landed
//! is ever representable and nothing needs un-folding — a turn that
//! dies before its batch completes never touches the manager at all.
//!
//! Design-set 2026-08 (owner-ruled): wired into the reload path only;
//! the live agent loop consumes it after the rewiring discussion.

use crate::entry::{EntryKind, FileRecord, SessionEntry};
use crate::fold::{calls_of, fold_branch, tail_is_closed};
use crate::ids;
use crate::lock;
use crate::tree::{SessionTree, TreeFault};
use crate::writer::SharedBuffer;
use rig_core::completion::{Message, Usage};
use rig_core::message::{AssistantContent, ToolResult, UserContent};

/// A refused checkout: the target names no node the tree holds. User
/// input — graceful. A target *inside* an open roundtrip is not this
/// error: it panics (PROTOCOL.md flag 23 — not a representable
/// conversation state).
#[derive(Debug, thiserror::Error)]
#[error("checkout target `{0}` is not in this session")]
pub struct CheckoutError(pub String);

/// The conversation as a shared cell: the one manager behind the
/// session's `RwLock`. Writers are enumerated and mutually exclusive
/// in time — during a run the engine's folds (brief, synchronous
/// `write()` holds through [`lock::write`](crate::lock::write));
/// between runs the session beat — and readers take `read()` holds
/// (the receive-time probe, the engine's turn-history reads). No
/// await is ever held under a guard.
pub type ConversationCell = std::sync::Arc<std::sync::RwLock<ContextManager>>;

/// The source of truth for the conversation. Whoever holds the manager
/// is the conversation — the session idle, the run live.
///
/// Laws:
///
/// 1. No stored context. `messages` derives the view from the tree on
///    every call.
/// 2. The tree never grows without its records entering the buffer in
///    the same operation — one batch enqueue, one grow step.
/// 3. Birth is the only preloaded state ([`from_tree`](Self::from_tree));
///    there is no mid-life adoption.
/// 4. The roundtrip is atomic end to end: verified whole
///    ([`fold_all`](Self::fold_all)), enqueued whole (one
///    all-or-nothing batch, written whole or retried) — an assistant
///    and its results land together or not at all, so a file with tool
///    calls but no results is unrepresentable.
pub struct ContextManager {
    /// Every node ever held, all branches; the head names the grown one.
    tree: SessionTree,
    /// The shared write buffer: commits queue here, the session drains.
    buffer: SharedBuffer,
}

impl std::fmt::Debug for ContextManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextManager")
            .field("tree", &self.tree)
            .finish_non_exhaustive()
    }
}

impl ContextManager {
    /// A fresh conversation over a shared buffer. The writer's `create`
    /// pre-queues the header; nothing is written until a drain.
    pub fn empty(buffer: SharedBuffer) -> Self {
        Self {
            tree: SessionTree::empty(),
            buffer,
        }
    }

    /// Reload: born from a parsed file's tree, with a buffer positioned
    /// at the file's end. The only way existing state enters.
    pub fn from_tree(tree: SessionTree, buffer: SharedBuffer) -> Self {
        Self { tree, buffer }
    }

    /// Seed a standalone (in-memory) conversation from an existing
    /// message list: the same verification as live folding — user
    /// messages and tool-free assistants fold; an assistant carrying
    /// tool calls folds with the results message that follows it, as
    /// one roundtrip. Nothing persists (a [`NullBuffer`] underneath).
    #[allow(clippy::panic)] // sanctioned crash: an invalid seed, failed loud (AGENTS.md doctrine)
    pub fn seeded(messages: Vec<Message>) -> Self {
        let mut seeded = Self::empty(std::sync::Arc::new(std::sync::Mutex::new(
            crate::writer::NullBuffer,
        )));
        let mut batch: Vec<Message> = Vec::new();
        for message in messages {
            let opens_roundtrip = matches!(&message, Message::Assistant { content, .. }
                if content.iter().any(|part| matches!(part, AssistantContent::ToolCall(_))));
            if opens_roundtrip || !batch.is_empty() {
                batch.push(message);
                let roundtrip_ready =
                    !batch.is_empty() && matches!(batch.last(), Some(Message::User { .. }));
                if roundtrip_ready {
                    let batch = std::mem::take(&mut batch);
                    seeded.fold_all(batch);
                }
            } else {
                seeded.fold(message);
            }
        }
        if !batch.is_empty() {
            panic!(
                "ContextManager::seeded: the message list ends inside an open tool \
                 roundtrip — tool calls never enter the context without results"
            );
        }
        seeded
    }

    /// The conversation so far, as the model sees it: the **history
    /// view** (the newest compaction's summary leading its tail plus
    /// newer entries) folded through the one context builder. Derived
    /// on every call — the only read, and nothing stores it.
    pub fn messages(&self) -> Vec<Message> {
        fold_branch(&self.tree.history_to_head())
    }

    /// The history view (root → head with the newest compaction
    /// spliced at its cut): the model-facing entry order — the **cut
    /// surface** (owner ruling 2026-09: cut points and folds consume
    /// the view; totals come from [`Self::measured_total`], never
    /// from walking this). Materialized on demand — never stored.
    pub fn history(&self) -> Vec<SessionEntry> {
        self.tree.history_to_head()
    }

    /// The branch's measured context size — the total of the nearest
    /// measurement-bearing node at-or-before the head. **The one
    /// public read of the raw walk** (the tree walk serves this total
    /// and the history view's own construction — the surfaces are not
    /// interchangeable, and the choice lives here, never at call
    /// sites). `None` is an unmeasured context.
    pub fn measured_total(&self) -> Option<u64> {
        crate::fold::regime_total(&self.tree.path_to_head())
    }

    /// The active branch as entries (root → head, the raw tree walk,
    /// compactions at their leaf positions), for session-side
    /// projections that read the whole grown branch — rewind targets,
    /// replay. Materialized on demand — never stored. Never a
    /// measurement or cut surface: totals come from
    /// [`Self::measured_total`], cut points from [`Self::history`].
    pub fn active_branch(&self) -> Vec<SessionEntry> {
        self.tree.path_to_head()
    }

    /// Whether `id` names a node in the tree (any branch) — the
    /// receive-time checkout validation.
    pub fn contains(&self, id: &str) -> bool {
        self.tree.contains(id)
    }

    /// Fold one message from a seeded history (or a test double): a
    /// user message, a system note, or a tool-free assistant turn no
    /// server measured — its usage records the type's not-reported
    /// zeros. An assistant carrying tool calls is refused loud: tool
    /// calls commit only through [`fold_all`](Self::fold_all), never
    /// without their results.
    pub fn fold(&mut self, message: Message) {
        self.fold_entry(message, None, None);
    }

    /// Fold a user message under its born-early id (its `message_queued`
    /// id) — live and replay name the same node. A settled assistant
    /// commits through [`fold_turn_with_id`](Self::fold_turn_with_id)
    /// instead: its usage rides the commit, and folding one here would
    /// silently record an unmeasured turn.
    #[allow(clippy::panic)] // sanctioned crash: an engine wiring bug, failed loud (AGENTS.md doctrine)
    pub fn fold_with_id(&mut self, message: Message, id: String) {
        if matches!(message, Message::Assistant { .. }) {
            panic!(
                "ContextManager::fold_with_id: an assistant turn commits through \
                 fold_turn_with_id — its reported usage rides the commit"
            );
        }
        self.fold_entry(message, None, Some(id));
    }

    /// Commit a settled, tool-free assistant turn under its announced
    /// turn id, with the usage the provider reported for it — the
    /// measurement rides the entry (the compaction trigger reads it
    /// back; reloaded stats count it). The engine's FINAL fold.
    pub fn fold_turn_with_id(&mut self, message: Message, id: String, usage: Usage) {
        self.fold_entry(message, Some(usage), Some(id));
    }

    /// The committing turn's measured context growth (owner ruling
    /// 2026-09): `total_tokens` minus the predecessor measurement's
    /// total — the previous measured assistant on the history view,
    /// the leading compaction node's `tokens_after`, or 0 at session
    /// start (the system prompt folds into the first turn's delta,
    /// measured). `None` when the turn is unmeasured (no usage, or
    /// the zero sentinel) — the turn is then uncounted and the next
    /// measured delta telescopes over it. A total below its
    /// predecessor (a mid-regime model switch re-tokenizing the same
    /// history, or a misreport) is also `None` — a debug line, never
    /// a warning (nobody can act on it): external world, graceful —
    /// the chain re-anchors at this turn's total.
    fn turn_delta(&self, usage: Option<&Usage>) -> Option<u64> {
        // `None` is the seed door only (no server measured it —
        // expected, silent). A live commit (this method's `Some`
        // paths) carries the provider's report; the zero sentinel
        // there means the provider answered without one.
        let usage = usage?;
        if usage.total_tokens == 0 {
            // The assumption this design builds on: every valid
            // provider reports usage (owner ruling 2026-09) — an
            // unreported live turn disables compaction for the whole
            // context (measurement, never estimation). A warning: the
            // user can act on it (their provider/model choice).
            tracing::warn!(
                "a provider completed a turn without reporting usage — compaction \
                 measures and never estimates, so it cannot run on this conversation \
                 while turns go unreported (check the provider/model)"
            );
            return None;
        }
        let predecessor = crate::fold::regime_total(&self.tree.path_to_head()).unwrap_or(0);
        match usage.total_tokens.checked_sub(predecessor) {
            Some(delta) => Some(delta),
            None => {
                // Expected on a mid-regime model/tokenizer change (the
                // user switched models — their need) or a flaky report:
                // nobody can act on it, so it is a debug line, never a
                // warning. The span is uncounted; the next delta
                // anchors at this turn's total.
                tracing::debug!(
                    total = usage.total_tokens,
                    predecessor,
                    "a turn total below its predecessor — delta uncounted, the chain \
                     re-anchors at this total"
                );
                None
            }
        }
    }

    #[allow(clippy::panic)] // sanctioned crash: an engine wiring bug, failed loud (AGENTS.md doctrine)
    fn fold_entry(&mut self, message: Message, usage: Option<Usage>, id: Option<String>) {
        let kind = match message {
            Message::User { .. } => EntryKind::UserMessage { message },
            Message::Assistant { id, content } => {
                if content
                    .iter()
                    .any(|part| matches!(part, AssistantContent::ToolCall(_)))
                {
                    panic!(
                        "ContextManager::fold: a tool-carrying assistant commits only through \
                         fold_all — tool calls never enter the context without results"
                    );
                }
                EntryKind::AssistantMessage {
                    message: Message::Assistant { id, content },
                    // `None` only on the seed path — no server measured
                    // a seeded turn, and zeros are the type's
                    // not-reported sentinel.
                    usage: usage.unwrap_or_default(),
                    delta_tokens: self.turn_delta(usage.as_ref()),
                }
            }
            // A System message carries verbatim as its own message
            // (mid-conversation system items hoist at request build —
            // unsupported by design as a mid-run injection, but a
            // seeded standalone history carries them and the provider
            // must see the same list). The view reproduces the
            // verbatim message; the tree holds it as a user node.
            Message::System { content } => EntryKind::UserMessage {
                message: Message::System { content },
            },
        };
        self.commit_with_ids([(kind, id)]);
    }

    /// As [`fold_all`](Self::fold_all), but the committed entries carry
    /// the provider-reported usage of the batch's assistant turn, and
    /// the result entries reuse their born-early ids (minted at
    /// settlement, announced by the result events) — live and replay
    /// name the same nodes. `result_ids` pairs 1:1 with the batch's
    /// results, in order. The engine's roundtrip fold.
    pub fn fold_all_with_ids(
        &mut self,
        batch: Vec<Message>,
        usage: Usage,
        result_ids: Vec<String>,
    ) {
        self.fold_all_entry(batch, usage, result_ids);
    }

    /// The roundtrip commit for a seeded history (or a test double) —
    /// no server measured it, so the assistant's usage records the
    /// type's not-reported zeros. The batch must be exactly one
    /// tool-carrying assistant turn followed by user messages of tool
    /// results, every call answered exactly once, nothing unpaired.
    /// Verified whole, then committed whole: one buffer blob, one tree
    /// grow, all-or-none — tool calls enter the context only with
    /// their results, or never.
    pub fn fold_all(&mut self, batch: Vec<Message>) {
        self.fold_all_entry(batch, Usage::new(), Vec::new());
    }

    #[allow(clippy::panic)] // sanctioned crash: an engine wiring bug, failed loud (AGENTS.md doctrine)
    fn fold_all_entry(&mut self, batch: Vec<Message>, usage: Usage, result_ids: Vec<String>) {
        let mut messages = batch.into_iter();
        let assistant = match messages.next() {
            Some(message @ Message::Assistant { .. }) => message,
            _ => panic!("ContextManager::fold_all: the batch must lead with an assistant turn"),
        };
        let calls = calls_of(&assistant);
        if calls.is_empty() {
            panic!(
                "ContextManager::fold_all: the head turn carries no tool calls — a tool-free \
                 turn folds through fold, and fold_all is strictly the roundtrip commit"
            );
        }
        let mut open: Vec<&str> = calls.iter().map(|call| call.id.as_str()).collect();
        let mut results: Vec<ToolResult> = Vec::new();
        for message in messages {
            let Message::User { content } = message else {
                panic!(
                    "ContextManager::fold_all: after the assistant the batch carries only \
                     result messages, got `{message:?}`"
                );
            };
            for part in content {
                let UserContent::ToolResult(result) = part else {
                    panic!(
                        "ContextManager::fold_all: result messages carry only tool results, \
                         got `{part:?}`"
                    );
                };
                let Some(index) = open.iter().position(|id| *id == result.id) else {
                    panic!(
                        "ContextManager::fold_all: tool result `{}` answers no open call of \
                         the batch (unknown or already answered)",
                        result.id
                    );
                };
                open.remove(index);
                results.push(result);
            }
        }
        if !open.is_empty() {
            panic!(
                "ContextManager::fold_all: the roundtrip is incomplete — {} call(s) \
                 unanswered; an assistant and its results commit whole or not at all",
                open.len()
            );
        }
        let assistant_id = match &assistant {
            Message::Assistant { id, .. } => id.clone(),
            _ => None,
        };
        let mut kinds: Vec<(EntryKind, Option<String>)> = Vec::with_capacity(results.len() + 1);
        kinds.push((
            EntryKind::AssistantMessage {
                message: assistant,
                delta_tokens: self.turn_delta(Some(&usage)),
                usage,
            },
            assistant_id,
        ));
        kinds.extend(results.into_iter().enumerate().map(|(index, result)| {
            let entry_id = result_ids.get(index).cloned();
            (EntryKind::ToolResult { result }, entry_id)
        }));
        self.commit_with_ids(kinds);
    }

    /// Move the head (a checkout / rewind). The branch ending at the
    /// target must be roundtrip-closed: a target inside an open tool
    /// batch names an unrepresentable conversation state and panics
    /// loud (PROTOCOL.md flag 23). Under the one-commit-door invariant
    /// only the path's tail can be open, so the check is a bounded
    /// walk-back, never a branch walk. An unknown target is user input
    /// — a graceful [`CheckoutError`]. The manager records nothing
    /// here; the `checkout` side record is session business through
    /// its own buffer handle.
    #[allow(clippy::panic, clippy::panic_in_result_fn)] // sanctioned crashes: corruption / contract violations, loud (AGENTS.md doctrine)
    pub fn checkout(&mut self, target: Option<&str>) -> Result<(), CheckoutError> {
        if let Some(id) = target
            && !self.tree.contains(id)
        {
            return Err(CheckoutError(id.to_string()));
        }
        // Validate before mutating anything: the walk and the
        // closed-path rule are read-only over the tree.
        let path = self
            .tree
            .path_to(target)
            .unwrap_or_else(|TreeFault(fault)| {
                panic!("ContextManager::checkout: {fault}");
            });
        if let Err(reason) = tail_is_closed(&path) {
            panic!(
                "ContextManager::checkout to `{target:?}` refused: the target is inside an \
                 open tool roundtrip ({reason}) — a mid-roundtrip checkout is unsupported"
            );
        }
        self.tree
            .move_head(target)
            .unwrap_or_else(|TreeFault(fault)| {
                panic!("ContextManager::checkout: target validated, then refused: {fault}");
            });
        Ok(())
    }

    /// Commit one compaction pass (v5): a **leaf-append at the head**
    /// — the entry's parent is the current head, the head advances to
    /// it, and `cut_child` names where the history view splices it
    /// in. The cut child must sit on the active branch (the box
    /// selected it there). The regime's base (`tokens_after`) is the
    /// box's suffix-delta arithmetic, persisted here once — every
    /// later read is a field access. Like every commit: the record
    /// enqueues into the buffer as one batch and the tree grows in
    /// the same operation. Validation failures are internal wiring
    /// bugs (the box checks cut viability before committing) and
    /// fail loud.
    #[allow(clippy::panic)] // sanctioned crash: an engine wiring bug, failed loud (AGENTS.md doctrine)
    pub fn commit_compaction(
        &mut self,
        id: String,
        summary: String,
        cut_child: String,
        tokens_before: u64,
        tokens_after: u64,
        usage: Usage,
    ) {
        let on_branch = self
            .tree
            .path_to_head()
            .iter()
            .any(|entry| entry.id == cut_child);
        if !on_branch {
            panic!(
                "ContextManager::commit_compaction: cut child `{cut_child}` is not on the \
                 active branch — the compaction box selected a stale cut"
            );
        }
        let entry = SessionEntry::with_id(
            id,
            self.tree.head().map(str::to_string),
            ids::now_rfc3339(),
            EntryKind::Compaction {
                summary,
                cut_child,
                tokens_before,
                tokens_after,
                usage,
            },
        );
        // The buffer's one interface, as every commit: the record enters
        // the outbox and the write attempt happens inside. The tree
        // append moves the head — later turns chain through the
        // compaction node, exactly the walked order.
        let _ = lock::lock(&self.buffer).enqueue(&[FileRecord::Node(entry.clone())]);
        self.tree.append(entry);
    }

    /// The unified commit: chain the entries under the head, enqueue
    /// their records as **one batch** (one outbox unit, all-or-nothing),
    /// then grow the tree — never one without the other, and never a
    /// partial blob: a roundtrip enters the buffer whole or not at all,
    /// so a file with tool calls but no results is unrepresentable.
    fn commit_with_ids(&mut self, kinds: impl IntoIterator<Item = (EntryKind, Option<String>)>) {
        let mut parent = self.tree.head().map(str::to_string);
        let entries: Vec<SessionEntry> = kinds
            .into_iter()
            .map(|(kind, id)| {
                let entry = match id {
                    Some(id) => SessionEntry::with_id(id, parent.clone(), ids::now_rfc3339(), kind),
                    None => SessionEntry::new(parent.clone(), ids::now_rfc3339(), kind),
                };
                parent = Some(entry.id.clone());
                entry
            })
            .collect();
        let records: Vec<FileRecord> = entries
            .iter()
            .map(|entry| FileRecord::Node(entry.clone()))
            .collect();
        // The buffer's one interface: the batch enters the outbox whole
        // and the write attempt happens inside. A failure keeps the
        // lines queued (every later enqueue retries them) — the
        // conversation never stalls on the disk, and the report's
        // consumer wiring (degradation notices) lands with the loop.
        let _ = lock::lock(&self.buffer).enqueue(&records);
        for entry in entries {
            self.tree.append(entry);
        }
    }
}

#[cfg(test)]
#[path = "context_manager_tests.rs"]
mod tests;
