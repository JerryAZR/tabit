//! Folding conversation nodes into the model-visible view, and the
//! structural check over a path's tail.
//!
//! The context builder here is the one implementation everywhere
//! ([`crate::ContextManager::messages`] calls it; the parser's reload
//! checks route through the same fold). Consecutive `tool_result`
//! nodes merge into one user message per batch — the same shape the
//! engine commits through `fold_all` — so a loaded view and a live one
//! are the same list. Side records (`model_change`, `checkout`,
//! `aborted`, …) are session state, not context, and never fold.

use crate::entry::{EntryKind, SessionEntry};
use rig_core::OneOrMany;
use rig_core::completion::Message;
use rig_core::message::{ToolCall, UserContent};

/// How the summary enters the model-visible context: a user-role
/// message wrapping the summary text (the references' pattern — codex's
/// "another language model" prefix, pi's `<summary>` tags). User-role,
/// never system: mid-conversation system messages are unsupported by
/// design (AGENTS.md), and the wrapper must read as conversation
/// history, not instruction. Everything before the compaction node on
/// the walked path is dropped from the view — the fold truncates what
/// it accumulated when it reaches a compaction entry, so only the
/// LAST compaction on the path and the entries after it survive.
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
/// The closing tag of [`COMPACTION_SUMMARY_PREFIX`].
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Fold a whole branch (root → head) into the model-visible message
/// list. Consecutive `tool_result` nodes merge into one user message
/// per batch. A `compaction` node truncates everything before it and
/// enters as the wrapped summary message — walkers stop at the
/// compaction, included.
pub fn fold_branch(entries: &[SessionEntry]) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::new();
    let mut pending_results: Vec<UserContent> = Vec::new();
    for entry in entries {
        match &entry.kind {
            EntryKind::UserMessage { message } => {
                flush_results(&mut messages, &mut pending_results);
                messages.push(message.clone());
            }
            EntryKind::AssistantMessage { message, .. } => {
                flush_results(&mut messages, &mut pending_results);
                messages.push(message.clone());
            }
            EntryKind::ToolResult { result } => {
                pending_results.push(UserContent::ToolResult(result.clone()));
            }
            EntryKind::Compaction { summary, .. } => {
                // The stop rule (v4): the summary replaces the walked
                // prefix wholesale — clear the accumulated view (and any
                // staged results, which belong to that prefix) and start
                // from the summary. A later compaction on the same path
                // repeats this, so only the last one survives.
                pending_results.clear();
                messages.clear();
                messages.push(Message::user(format!(
                    "{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"
                )));
            }
        }
    }
    flush_results(&mut messages, &mut pending_results);
    messages
}

/// Fold one accumulated tool batch into place (no-op when empty).
fn flush_results(messages: &mut Vec<Message>, pending: &mut Vec<UserContent>) {
    if pending.is_empty() {
        return;
    }
    let results = std::mem::take(pending);
    if let Some(content) = OneOrMany::from_iter_optional(results) {
        messages.push(Message::User { content });
    }
}

/// The tool calls an assistant message carries.
pub fn calls_of(message: &Message) -> Vec<&ToolCall> {
    let Message::Assistant { content, .. } = message else {
        return Vec::new();
    };
    content
        .iter()
        .filter_map(|part| match part {
            rig_core::message::AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

/// Validate that a path **ends roundtrip-closed**: the tip must not sit
/// inside a tool roundtrip. Walks back from the tip only as far as the
/// trailing result run and its assistant — one batch's span. Under the
/// one-commit-door invariant (a roundtrip enters the tree whole or not
/// at all) everything further back is closed by construction, so the
/// check is a bounded lookback, never a branch walk. The live checkout
/// door (a mid-roundtrip target refuses) and the parser's torn-tail
/// check both route through here.
#[allow(clippy::panic_in_result_fn)] // the crash inside is sanctioned (AGENTS.md doctrine), annotated below
pub fn tail_is_closed(path: &[SessionEntry]) -> Result<(), String> {
    // The trailing run of tool results, walking back from the tip.
    let batch_start = path
        .iter()
        .rposition(|entry| !matches!(entry.kind, EntryKind::ToolResult { .. }))
        .map(|pos| pos + 1)
        .unwrap_or(0);
    let (before, trailing) = path.split_at(batch_start);
    let Some(boundary) = before.last() else {
        return match trailing.first() {
            None => Ok(()),
            Some(entry) => Err(format!(
                "the tail's tool results (from entry `{}`) have no assistant behind them",
                entry.id
            )),
        };
    };
    if trailing.is_empty() {
        // A tip that is not a tool result: illegal only when it is a
        // call-carrying assistant (its roundtrip never landed).
        if let EntryKind::AssistantMessage { message, .. } = &boundary.kind {
            let calls = calls_of(message);
            if !calls.is_empty() {
                return Err(format!(
                    "the tail ends at assistant entry `{}` with {} unanswered call(s)",
                    boundary.id,
                    calls.len()
                ));
            }
        }
        return Ok(());
    }
    // A trailing result run: its assistant sits right before it (the
    // whole-roundtrip shape), and the run must answer every call once.
    let EntryKind::AssistantMessage { message, .. } = &boundary.kind else {
        return Err(format!(
            "the tail's tool results follow entry `{}`, not their assistant",
            boundary.id
        ));
    };
    let mut open: Vec<String> = calls_of(message)
        .iter()
        .map(|call| call.id.clone())
        .collect();
    for entry in trailing {
        #[allow(clippy::panic)]
        // sanctioned crash: unreachable by the split above — an internal invariant break, failed loud
        let EntryKind::ToolResult { result } = &entry.kind else {
            panic!(
                "tail_is_closed: a non-result entry `{}` rode the trailing result run",
                entry.id
            );
        };
        let Some(index) = open.iter().position(|id| *id == result.id) else {
            return Err(format!(
                "tool result `{}` answers no open call at entry `{}`",
                result.id, entry.id
            ));
        };
        open.swap_remove(index);
    }
    if open.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the tail ends mid-roundtrip: {} call(s) unanswered",
            open.len()
        ))
    }
}

/// The branch's measured context size: the total of the nearest
/// measurement-bearing node at-or-before the head, walking the **raw
/// branch** (owner ruling 2026-09 — deltas are facts, nothing is
/// estimated). An assistant's `total_tokens` already measures the
/// whole request it rode (`P + history through the turn`,
/// partition-correct per provider); a compaction node's
/// `tokens_after` is the regime's base. The raw walk matters: the
/// leaf-append geometry meets the live compaction **before** any
/// retained-tail entry, so an old-regime total (stale — its prefix
/// was replaced) is structurally unreachable, and the walk returns
/// at the compaction's base instead of inheriting across it.
/// Zero-sentinel assistants pass by — the read inherits the previous
/// valid total (their content rides the next measured turn's delta).
/// `None` when nothing on the branch ever measured: an unmeasured
/// context (a fresh session, a provider that never reports usage) —
/// callers treat absence as absence, never an estimate.
pub fn regime_total(branch: &[SessionEntry]) -> Option<u64> {
    branch.iter().rev().find_map(|entry| match &entry.kind {
        EntryKind::AssistantMessage { usage, .. } if usage.total_tokens > 0 => {
            Some(usage.total_tokens)
        }
        EntryKind::Compaction { tokens_after, .. } => Some(*tokens_after),
        _ => None,
    })
}

/// The branch's `user_message` nodes in root→head order — the valid
/// user-facing checkout targets (`rewind(n)` resolves through these).
pub fn user_message_boundaries(entries: &[SessionEntry]) -> Vec<&SessionEntry> {
    entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::UserMessage { .. }))
        .collect()
}

#[cfg(test)]
#[path = "fold_tests.rs"]
mod tests;
