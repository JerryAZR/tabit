//! Replaying session entries into model-visible context.
//!
//! Projection is pure: a function from entries to messages. `label`,
//! `custom`, and `model_change` entries are state, not context, and are
//! skipped here; `model_change` state is folded separately by the session
//! on resume.

use crate::entry::{EntryKind, SessionEntry};
use rig_core::OneOrMany;
use rig_core::completion::Message;
use rig_core::message::{ToolCall, ToolResult, ToolResultContent};

/// A dangling assistant turn found during projection: the assistant called
/// tools, but the log ends before any results were recorded (the process
/// died mid tool-use roundtrip). Carries everything needed to synthesize
/// honest "interrupted" results so the log replays cleanly.
#[derive(Debug, Clone, PartialEq)]
pub struct DanglingToolCalls {
    /// The tool calls that never received results.
    pub calls: Vec<ToolCall>,
}

/// Project a linear entry log into the message list the next outer loop
/// should see, and report a dangling trailing assistant turn if present.
///
/// Consecutive `tool_result` entries merge into one user message — the
/// shape providers expect for a tool batch.
pub fn project(entries: &[SessionEntry]) -> (Vec<Message>, Option<DanglingToolCalls>) {
    let mut messages = Vec::new();
    let mut pending_results: Vec<ToolResult> = Vec::new();
    let mut dangling: Option<DanglingToolCalls> = None;

    let flush_results = |pending: &mut Vec<ToolResult>, messages: &mut Vec<Message>| {
        if pending.is_empty() {
            return;
        }
        let results: Vec<rig_core::message::UserContent> = pending
            .drain(..)
            .map(rig_core::message::UserContent::ToolResult)
            .collect();
        let content =
            OneOrMany::many(results).unwrap_or_else(|_| OneOrMany::one(user_placeholder()));
        messages.push(Message::User { content });
    };

    for entry in entries {
        match &entry.kind {
            EntryKind::UserMessage { message } => {
                flush_results(&mut pending_results, &mut messages);
                dangling = None;
                messages.push(message.clone());
            }
            EntryKind::AssistantMessage { message, .. } => {
                flush_results(&mut pending_results, &mut messages);
                dangling = tool_calls_of(message);
                messages.push(message.clone());
            }
            EntryKind::ToolResult { result } => {
                dangling = None;
                pending_results.push(result.clone());
            }
            EntryKind::ModelChange { .. } | EntryKind::Label { .. } | EntryKind::Custom { .. } => {
                continue;
            }
        }
    }
    flush_results(&mut pending_results, &mut messages);

    (messages, dangling)
}

/// The last `model_change` entry in the log, if any — the provider/model a
/// resumed session should continue with.
pub fn last_model_change(entries: &[SessionEntry]) -> Option<(&str, &str, Option<&str>)> {
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
                "[tool execution was interrupted before completing; the session \
                 was resumed and no result was recorded — treat this call as failed]",
            )),
        })
        .collect()
}

/// The tool calls carried by an assistant message.
fn tool_calls_of(message: &Message) -> Option<DanglingToolCalls> {
    let Message::Assistant { content, .. } = message else {
        return None;
    };
    let calls: Vec<ToolCall> = content
        .iter()
        .filter_map(|part| match part {
            rig_core::message::AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect();
    if calls.is_empty() {
        None
    } else {
        Some(DanglingToolCalls { calls })
    }
}

/// A neutral single result for the `OneOrMany::many` fallback — unreachable
/// in practice (only called with a non-empty vec), but `OneOrMany` has no
/// empty constructor.
fn user_placeholder() -> rig_core::message::UserContent {
    rig_core::message::UserContent::ToolResult(ToolResult {
        id: String::new(),
        call_id: None,
        content: OneOrMany::one(ToolResultContent::text("")),
    })
}

#[cfg(test)]
#[path = "projection_tests.rs"]
mod tests;
