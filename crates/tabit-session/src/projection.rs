//! Replaying session entries into model-visible context.
//!
//! Projection is pure: a function from the active branch of the
//! conversation tree to messages. Only the three node kinds reach here
//! — side records (`model_change`, `checkout`, `aborted`, …) are
//! session state, not context, and never enter the tree (format v3).
//! The model selection is a session preference — a register read over
//! the record stream ([`last_model_change_in_file`]) — folded at load
//! and never moved by a checkout.

use crate::entry::{EntryKind, FileRecord, SessionEntry};
use rig_core::OneOrMany;
use rig_core::completion::Message;
use rig_core::message::{ToolCall, ToolResult, ToolResultContent};
use std::collections::HashSet;

/// A dangling assistant turn found during projection: the assistant called
/// tools, and the branch ends before every call received a result — a crash
/// or abort mid tool-use roundtrip, or a branch point landing mid-batch.
/// Carries everything needed to synthesize honest "interrupted" results so
/// the branch replays cleanly.
#[derive(Debug, Clone, PartialEq)]
pub struct DanglingToolCalls {
    /// The tool calls that never received results.
    pub calls: Vec<ToolCall>,
}

/// The incremental context fold — the live projection of the active
/// branch. One implementation serves both users: the whole-branch
/// [`project`] (load, checkout rebuild) and the record-time fold (the
/// resident context grows one node at a time, so nothing re-derives
/// mid-session).
#[derive(Default)]
pub struct Projector {
    messages: Vec<Message>,
    pending_results: Vec<ToolResult>,
    // The trailing assistant turn's calls and the call ids its results
    // have answered so far. A branch point can land mid-batch, so
    // "some results arrived" no longer implies "all calls answered".
    trailing_calls: Vec<ToolCall>,
    answered: HashSet<String>,
}

impl Projector {
    /// Fold one conversation node into the projection.
    pub fn fold(&mut self, entry: &SessionEntry) {
        match &entry.kind {
            EntryKind::UserMessage { message } => {
                self.flush_results();
                self.trailing_calls.clear();
                self.answered.clear();
                self.messages.push(message.clone());
            }
            EntryKind::AssistantMessage { message, .. } => {
                self.flush_results();
                self.trailing_calls = calls_of(message);
                self.answered.clear();
                self.messages.push(message.clone());
            }
            EntryKind::ToolResult { result } => {
                self.answered.insert(result.id.clone());
                if let Some(call_id) = &result.call_id {
                    self.answered.insert(call_id.clone());
                }
                self.pending_results.push(result.clone());
            }
        }
    }

    /// The folded messages so far (pending tool results held back
    /// until flushed by the next turn or `finish`).
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// The context snapshot: the folded messages with any pending tool
    /// batch flushed into place. Reads happen at run boundaries — turn
    /// boundaries by construction — so closing the batch here cannot
    /// merge batches that a turn kept apart. The fold keeps its state;
    /// a snapshot never consumes it.
    pub fn snapshot(&mut self) -> Vec<Message> {
        self.flush_results();
        self.messages.clone()
    }

    /// The trailing turn's unanswered calls, when any — the dangling
    /// detection the repair paths peek at, without consuming the fold.
    pub fn dangling(&self) -> Option<DanglingToolCalls> {
        unanswered_calls(&self.trailing_calls, &self.answered)
    }

    /// The folded messages, flushing any pending tool batch, and the
    /// final dangling report.
    pub fn finish(mut self) -> (Vec<Message>, Option<DanglingToolCalls>) {
        self.flush_results();
        let dangling = unanswered_calls(&self.trailing_calls, &self.answered);
        (self.messages, dangling)
    }

    fn flush_results(&mut self) {
        if self.pending_results.is_empty() {
            return;
        }
        let results = std::mem::take(&mut self.pending_results);
        self.messages.push(tool_results_message(results));
    }
}

/// Project the active branch of the conversation tree into the message
/// list the next outer loop should see, and report the trailing
/// assistant turn's unanswered tool calls, if any.
///
/// Consecutive `tool_result` entries merge into one user message — the
/// shape providers expect for a tool batch.
pub fn project(entries: &[SessionEntry]) -> (Vec<Message>, Option<DanglingToolCalls>) {
    let mut projector = Projector::default();
    for entry in entries {
        projector.fold(entry);
    }
    projector.finish()
}

/// The branch's `user_message` entries in root→head order — the valid
/// user-facing checkout targets. A branch before any of them leaves the
/// branch replayable: it ends on a completed turn, a closed tool batch,
/// never mid-batch.
pub fn user_message_boundaries(entries: &[SessionEntry]) -> Vec<&SessionEntry> {
    entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::UserMessage { .. }))
        .collect()
}

/// The calls of `calls` that no result on the branch answered, as a
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

/// The file's last `model_change` side record, read backwards in file
/// (append) order and stopping at the first encounter — the **session
/// preference register** (owner ruling 2026-08): model selection is
/// present-tense state, so the latest choice in time wins regardless of
/// which branch the conversation is on, and a checkout (a head-pointer
/// move) never rolls it back. `None` when the file records no model
/// change.
pub fn last_model_change_in_file(records: &[FileRecord]) -> Option<(&str, &str, Option<&str>)> {
    records.iter().rev().find_map(|record| match record {
        FileRecord::Side(crate::entry::SideRecord {
            kind:
                crate::entry::SideKind::ModelChange {
                    provider,
                    model,
                    thinking_level,
                },
            ..
        }) => Some((provider.as_str(), model.as_str(), thinking_level.as_deref())),
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
