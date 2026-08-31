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

/// Fold a whole branch (root → head) into the model-visible message
/// list. Consecutive `tool_result` nodes merge into one user message
/// per batch.
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
        let EntryKind::ToolResult { result } = &entry.kind else {
            continue; // the trailing run holds only results by construction
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
