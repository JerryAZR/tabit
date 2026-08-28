//! Folding conversation nodes into the model-visible view, and the
//! structural checks over folded branches.
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

/// Validate that a branch is **roundtrip-closed**: every assistant turn
/// on it that called tools is fully answered by the tool results that
/// follow it on the branch. `Err` names the violation — the checkout
/// rule (a mid-roundtrip target refuses at the door) and the parser's
/// final-head check both route through here.
pub fn path_is_closed(entries: &[SessionEntry]) -> Result<(), String> {
    let mut pending: Vec<String> = Vec::new();
    for entry in entries {
        match &entry.kind {
            EntryKind::UserMessage { .. } => {
                if pending.is_empty() {
                    continue;
                }
                return Err(format!(
                    "user message interrupts the open tool batch of entry `{}`",
                    entry.id
                ));
            }
            EntryKind::AssistantMessage { message, .. } => {
                if !pending.is_empty() {
                    return Err(format!(
                        "branch ends the batch of a previous turn open at entry `{}`",
                        entry.id
                    ));
                }
                pending = calls_of(message)
                    .iter()
                    .map(|call| call.id.clone())
                    .collect();
            }
            EntryKind::ToolResult { result } => {
                let Some(index) = pending.iter().position(|id| *id == result.id) else {
                    return Err(format!(
                        "tool result `{}` answers no open call at entry `{}`",
                        result.id, entry.id
                    ));
                };
                pending.swap_remove(index);
            }
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "branch ends mid-roundtrip: {} call(s) unanswered",
            pending.len()
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
