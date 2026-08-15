//! Session record types: the header line and the append-only entry log.
//!
//! One session is one JSONL file. The first line is a
//! [`SessionHeader`]; every following line is a [`SessionEntry`] whose
//! `parent_id` links it into a tree rooted at the header (in practice the
//! log is appended linearly, so the tree is a path; the link exists so
//! rewinding to an earlier entry — moving the leaf — stays possible without
//! a format change).
//!
//! Unknown `kind` tags fail deserialization loudly: the file was written by
//! a newer tabit, and silently dropping records would corrupt the
//! conversation.

use rig_core::completion::{Message, Usage};
use rig_core::message::ToolResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The current session file format version.
pub const SESSION_FORMAT_VERSION: u32 = 1;

/// The first line of a session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionHeader {
    /// Format version; see [`SESSION_FORMAT_VERSION`].
    pub version: u32,
    /// Session id (UUIDv7 string).
    pub id: String,
    /// Creation time (RFC 3339).
    pub created_at: String,
    /// The working directory the session was created in.
    pub cwd: String,
    /// The id of the session this one was forked from, if any. Reserved;
    /// forking is not implemented yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

/// One append-only record in a session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEntry {
    /// Entry id (UUIDv7 string).
    pub id: String,
    /// Parent entry id; `None` only for the first entry after the header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// When the entry was appended (RFC 3339).
    pub timestamp: String,
    /// The record payload.
    pub kind: EntryKind,
}

impl SessionEntry {
    /// Construct an entry linking to `parent_id` at the given time.
    pub fn new(parent_id: Option<String>, timestamp: String, kind: EntryKind) -> Self {
        Self {
            id: crate::ids::new_entry_id(),
            parent_id,
            timestamp,
            kind,
        }
    }
}

/// The payload of a session entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryKind {
    /// A user-authored message (the prompt that started an outer loop, or a
    /// message injected on the user's behalf).
    UserMessage {
        /// The message; must be [`Message::User`].
        message: Message,
    },
    /// A completed assistant turn, with the usage the provider reported for
    /// it.
    AssistantMessage {
        /// The message; must be [`Message::Assistant`].
        message: Message,
        /// Provider-reported token usage for the turn.
        usage: Usage,
    },
    /// The result of one executed tool call. Consecutive `tool_result`
    /// entries after an `assistant_message` form that turn's tool batch;
    /// the projection merges them into a single user message when the log
    /// is replayed into model context.
    ToolResult {
        /// The result, carrying the tool call id it answers.
        result: ToolResult,
    },
    /// The session switched provider/model/thinking level. Applied from the
    /// next outer loop on.
    ModelChange {
        /// Provider id from tabit config.
        provider: String,
        /// Model id within the provider.
        model: String,
        /// Active thinking level name, when the model defines levels.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_level: Option<String>,
    },
    /// A human-facing bookmark. Reserved; not part of model context.
    Label {
        /// Bookmark name.
        name: String,
    },
    /// Extension-owned state. Reserved; not part of model context until the
    /// extension framework defines its projection.
    Custom {
        /// Opaque extension payload.
        data: Value,
    },
}

impl EntryKind {
    /// Whether the entry contributes to the model-visible context on
    /// replay.
    pub fn is_context_entry(&self) -> bool {
        matches!(
            self,
            Self::UserMessage { .. } | Self::AssistantMessage { .. } | Self::ToolResult { .. }
        )
    }
}

#[cfg(test)]
#[path = "entry_tests.rs"]
mod tests;
