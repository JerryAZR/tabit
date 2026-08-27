//! The context: the in-memory committed conversation state visible to
//! models.
//!
//! One job, one law. [`Context`] holds the message list a provider
//! request consumes (the rig currency: `Vec<Message>`) and grows only
//! through its doors, one **committed** message at a time or one
//! committed batch at a time. What "committed" means is the callers'
//! contract, and the callers own it:
//!
//! - A turn folds when it is accepted — never mid-stream (streamed
//!   text is frontend-visible only), never after a veto (rejection
//!   happens before the commit, so there is nothing to undo).
//! - A tool batch folds once complete: every call of the turn answered
//!   by a real result or a synthesized one. The buffering until
//!   completeness lives at the commit site, not here.
//! - A steer drain folds as one batch.
//!
//! [`Context`] does not validate the sequence, does not inspect tool
//! calls, does not group, reorder, or synthesize — it receives valid
//! history and keeps it. The engine holds a run-scoped instance
//! (seeded at run open, folded at turn acceptance) and the session
//! layer holds the durable one (folded at load and at every commit
//! through its one door); two instances of one implementation, never
//! two builders.

use crate::completion::Message;
use serde::{Deserialize, Serialize};

/// The committed conversation state visible to models.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Context {
    messages: Vec<Message>,
}

impl Context {
    /// An empty context.
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    /// Fold one committed message.
    pub fn fold(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// Fold one committed batch — a run's seed, a loaded branch, a
    /// steer drain.
    pub fn fold_all(&mut self, messages: Vec<Message>) {
        self.messages.extend(messages);
    }

    /// The committed message list — exactly what a provider request
    /// carries.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Hand the message list over (the request's `chat_history`).
    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
