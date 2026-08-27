//! Replaying session entries into model-visible context.
//!
//! Projection is pure: a function from the active chain of entries to
//! messages. `label`, `custom`, `aborted`, `rewound`, and `model_change`
//! entries are state, not context, and are skipped here; the model
//! selection is a session preference — a register read over the whole
//! file, not the chain ([`last_model_change_in_file`]) — folded by the
//! session on resume and never moved by a rewind.

use crate::entry::{EntryKind, SessionEntry};
use rig_core::OneOrMany;
use rig_core::completion::Message;
use rig_core::message::{ToolCall, ToolResult, ToolResultContent};
use std::collections::HashSet;

/// A dangling assistant turn found during projection: the assistant called
/// tools, and the chain ends before every call received a result — a crash
/// or abort mid tool-use roundtrip, or a branch point landing mid-batch.
/// Carries everything needed to synthesize honest "interrupted" results so
/// the chain replays cleanly.
#[derive(Debug, Clone, PartialEq)]
pub struct DanglingToolCalls {
    /// The tool calls that never received results.
    pub calls: Vec<ToolCall>,
}

/// Project the active chain of a session log into the message list the
/// next outer loop should see, and report the trailing assistant turn's
/// unanswered tool calls, if any.
///
/// Consecutive `tool_result` entries merge into one user message — the
/// shape providers expect for a tool batch.
pub fn project(entries: &[SessionEntry]) -> (Vec<Message>, Option<DanglingToolCalls>) {
    let mut messages = Vec::new();
    let mut pending_results: Vec<ToolResult> = Vec::new();
    // The trailing assistant turn's calls and the call ids its results
    // have answered so far. A rewind can branch mid-batch, so "some
    // results arrived" no longer implies "all calls answered".
    let mut trailing_calls: Vec<ToolCall> = Vec::new();
    let mut answered: HashSet<String> = HashSet::new();

    let flush_results = |pending: &mut Vec<ToolResult>, messages: &mut Vec<Message>| {
        if pending.is_empty() {
            return;
        }
        messages.push(tool_results_message(std::mem::take(pending)));
    };

    for entry in entries {
        match &entry.kind {
            EntryKind::UserMessage { message } => {
                flush_results(&mut pending_results, &mut messages);
                trailing_calls.clear();
                answered.clear();
                messages.push(message.clone());
            }
            EntryKind::AssistantMessage { message, .. } => {
                flush_results(&mut pending_results, &mut messages);
                trailing_calls = calls_of(message);
                answered.clear();
                messages.push(message.clone());
            }
            EntryKind::ToolResult { result } => {
                answered.insert(result.id.clone());
                if let Some(call_id) = &result.call_id {
                    answered.insert(call_id.clone());
                }
                pending_results.push(result.clone());
            }
            EntryKind::ModelChange { .. }
            | EntryKind::Aborted
            | EntryKind::Rewound { .. }
            | EntryKind::Label { .. }
            | EntryKind::Custom { .. } => {
                continue;
            }
        }
    }
    flush_results(&mut pending_results, &mut messages);
    let dangling = unanswered_calls(&trailing_calls, &answered);

    (messages, dangling)
}

/// The chain's `user_message` entries in root→leaf order — the valid
/// user-facing rewind targets. A branch before any of them leaves the
/// chain replayable: it ends on a completed turn, a closed tool batch, or
/// bookkeeping, never mid-batch.
pub fn user_message_boundaries(entries: &[SessionEntry]) -> Vec<&SessionEntry> {
    entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::UserMessage { .. }))
        .collect()
}

/// The calls of `calls` that no result on the chain answered, as a
/// [`DanglingToolCalls`] when any remain.
fn unanswered_calls(calls: &[ToolCall], answered: &HashSet<String>) -> Option<DanglingToolCalls> {
    let dangling: Vec<ToolCall> = calls
        .iter()
        .filter(|call| !is_answered(call, answered))
        .cloned()
        .collect();
    (!dangling.is_empty()).then(|| DanglingToolCalls { calls: dangling })
}

/// Whether a tool result answered `call`: by the canonical call id, or by
/// the provider-specific call id when both carry one.
fn is_answered(call: &ToolCall, answered: &HashSet<String>) -> bool {
    answered.contains(&call.id)
        || call
            .call_id
            .as_ref()
            .is_some_and(|call_id| answered.contains(call_id))
}

/// The file's last `model_change` entry, read backwards in file (append)
/// order and stopping at the first encounter — the **session preference
/// register** (owner ruling 2026-08): model selection is present-tense
/// state, so the latest choice in time wins regardless of which branch it
/// was recorded on, and a rewind (a chain-pointer move) never rolls it
/// back. Parent links are deliberately ignored — the file's append order
/// is the time order. `None` when the file records no model change.
pub fn last_model_change_in_file(entries: &[SessionEntry]) -> Option<(&str, &str, Option<&str>)> {
    entries.iter().rev().find_map(|entry| match &entry.kind {
        EntryKind::ModelChange {
            provider,
            model,
            thinking_level,
        } => Some((provider.as_str(), model.as_str(), thinking_level.as_deref())),
        _ => None,
    })
}

/// Synthesize the tool results that repair a dangling turn: one per
/// unanswered call, with an explicit "interrupted" payload so the model
/// knows the call never completed.
pub fn interrupted_results(dangling: &DanglingToolCalls) -> Vec<ToolResult> {
    dangling
        .calls
        .iter()
        .map(|call| ToolResult {
            id: call.id.clone(),
            call_id: call.call_id.clone(),
            content: OneOrMany::one(ToolResultContent::text(
                "[tool execution was interrupted before completing — the call \"
                 may have had partial effects; verify them before relying on \"
                 anything it did]",
            )),
            // Interrupted is a failure shape: no body completed.
            status: Some(rig_core::completion::ToolResultStatus::Failed { code: None }),
        })
        .collect()
}

/// The user message carrying one tool batch's results — the single shape
/// providers expect, shared by projection and the dangling-roundtrip
/// repair. Only called with a non-empty batch; the placeholder arm exists
/// because `OneOrMany` has no empty constructor.
pub(crate) fn tool_results_message(results: Vec<ToolResult>) -> Message {
    let content: Vec<rig_core::message::UserContent> = results
        .into_iter()
        .map(rig_core::message::UserContent::ToolResult)
        .collect();
    Message::User {
        content: OneOrMany::many(content).unwrap_or_else(|_| OneOrMany::one(user_placeholder())),
    }
}

/// The tool calls carried by an assistant message.
fn calls_of(message: &Message) -> Vec<ToolCall> {
    let Message::Assistant { content, .. } = message else {
        return Vec::new();
    };
    content
        .iter()
        .filter_map(|part| match part {
            rig_core::message::AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

/// A neutral single result for the `OneOrMany::many` fallback — unreachable
/// in practice (only called with a non-empty vec), but `OneOrMany` has no
/// empty constructor.
fn user_placeholder() -> rig_core::message::UserContent {
    rig_core::message::UserContent::ToolResult(ToolResult {
        id: String::new(),
        call_id: None,
        content: OneOrMany::one(ToolResultContent::text("")),
        status: None,
    })
}

#[cfg(test)]
#[path = "projection_tests.rs"]
mod tests;
