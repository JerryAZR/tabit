//! The one context builder.
//!
//! A [`Conversation`] folds the events that extend a conversation —
//! user messages, committed assistant turns, individual tool results,
//! steered batches — into the message list a provider request carries.
//! There is exactly one implementation: the engine's state machine
//! holds a run-scoped instance (seeded from the session's history at
//! run open, extended as turns commit and tools answer), and a session
//! layer holds the durable instance (folded from its records at load
//! and grown at record time). Two instances of one builder, fed by the
//! same events in the same order — they cannot drift, because there is
//! nothing separate to drift.
//!
//! Two feed shapes arrive through the same doors. The engine commits a
//! turn's tool results as one validated batch ([`Conversation::user`]
//! with a pre-shaped message); a log's records arrive one result at a
//! time ([`Conversation::tool_result`], deferred into `pending_results`
//! until the next boundary flushes them into the single user message
//! providers expect). Reads ([`Conversation::messages`]) flush first,
//! so a pending batch can never be silently split or dropped.

use crate::completion::Message;
use rig_core::OneOrMany;
use rig_core::message::{ToolCall, ToolResult, ToolResultContent};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A dangling assistant turn: the assistant called tools, and the
/// conversation ends before every call received a result — a crash or
/// abort mid tool-use roundtrip, or a branch point landing mid-batch.
/// Carries everything needed to synthesize honest "interrupted"
/// results so the conversation replays cleanly.
#[derive(Debug, Clone, PartialEq)]
pub struct DanglingToolCalls {
    /// The tool calls that never received results.
    pub calls: Vec<ToolCall>,
}

/// The one context builder. See the module docs.
#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    messages: Vec<Message>,
    pending_results: Vec<ToolResult>,
    // The trailing assistant turn's calls and the call ids its results
    // have answered so far. A branch point can land mid-batch, so
    // "some results arrived" no longer implies "all calls answered".
    trailing_calls: Vec<ToolCall>,
    answered: HashSet<String>,
}

impl Conversation {
    /// An empty conversation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adopt an existing message list as the starting state (the
    /// engine's run-open seed; a resumed session's load).
    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self {
            messages,
            ..Self::default()
        }
    }

    /// One user message — a recorded user node, a steered message, or
    /// an engine-authored corrective. Closes any pending tool batch
    /// into place and resets the roundtrip bookkeeping.
    pub fn user(&mut self, message: Message) {
        self.flush_results();
        self.trailing_calls.clear();
        self.answered.clear();
        self.messages.push(message);
    }

    /// One committed assistant turn (the announced turn id rides the
    /// message). Closes any pending tool batch and re-arms the
    /// roundtrip bookkeeping from the turn's calls.
    pub fn assistant(&mut self, message: Message) {
        self.flush_results();
        self.trailing_calls = calls_of(&message);
        self.answered.clear();
        self.messages.push(message);
    }

    /// One tool result, deferred: a turn's results form a single user
    /// message, so results accumulate until the next boundary (a user
    /// or assistant message, or a read) flushes the batch.
    pub fn tool_result(&mut self, result: ToolResult) {
        self.answered.insert(result.id.clone());
        if let Some(call_id) = &result.call_id {
            self.answered.insert(call_id.clone());
        }
        self.pending_results.push(result);
    }

    /// A drained steering batch (the machine's steer point): user
    /// messages, in drain order.
    pub fn extend_users(&mut self, messages: Vec<Message>) {
        for message in messages {
            self.user(message);
        }
    }

    /// The retry mechanic: drop the last assistant turn, returning the
    /// conversation to the state before it ([`RetryRequest::Repeat`]).
    pub fn pop_last_assistant(&mut self) {
        if matches!(self.messages.last(), Some(Message::Assistant { .. })) {
            self.messages.pop();
        }
    }

    /// The committed message prefix, without flushing — the engine's
    /// read (its feeds arrive pre-batched, so nothing is ever pending
    /// in a run-scoped instance). The log-side feeds defer results;
    /// those readers use [`Conversation::messages`].
    pub fn committed(&self) -> &[Message] {
        &self.messages
    }

    /// The message list, any pending tool batch flushed into place.
    /// This is what a provider request carries.
    pub fn messages(&mut self) -> &[Message] {
        self.flush_results();
        &self.messages
    }

    /// The message list as an owned snapshot (flushed).
    pub fn messages_vec(&mut self) -> Vec<Message> {
        self.messages().to_vec()
    }

    /// The messages appended after the entry boundary
    /// (`split_at(entry_len).1`, flushed).
    pub fn new_since(&mut self, entry_len: usize) -> &[Message] {
        self.flush_results();
        &self.messages[entry_len.min(self.messages.len())..]
    }

    /// How many messages (flushed).
    pub fn len(&mut self) -> usize {
        self.messages().len()
    }

    /// Whether empty (flushed).
    pub fn is_empty(&mut self) -> bool {
        self.len() == 0
    }

    /// The trailing turn's unanswered calls, when any — the crash /
    /// abort / mid-batch-branch shape a repair pass synthesizes
    /// results for ([`interrupted_results`]).
    pub fn dangling(&self) -> Option<DanglingToolCalls> {
        let dangling: Vec<ToolCall> = self
            .trailing_calls
            .iter()
            .filter(|call| !self.is_answered(call))
            .cloned()
            .collect();
        (!dangling.is_empty()).then_some(DanglingToolCalls { calls: dangling })
    }

    fn flush_results(&mut self) {
        if self.pending_results.is_empty() {
            return;
        }
        let results = std::mem::take(&mut self.pending_results);
        let content: Vec<rig_core::message::UserContent> = results
            .into_iter()
            .map(rig_core::message::UserContent::ToolResult)
            .collect();
        self.messages.push(Message::User {
            content: OneOrMany::many(content)
                .unwrap_or_else(|_| OneOrMany::one(user_placeholder())),
        });
    }

    fn is_answered(&self, call: &ToolCall) -> bool {
        self.answered.contains(&call.id)
            || call
                .call_id
                .as_ref()
                .is_some_and(|call_id| self.answered.contains(call_id))
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

#[cfg(test)]
#[path = "conversation_tests.rs"]
mod tests;
