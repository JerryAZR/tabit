//! Session record types: the header line and the append-only entry log.
//!
//! One session is one JSONL file. The first line is a
//! [`SessionHeader`]; every following line is a [`SessionEntry`] whose
//! `parent_id` links it into a tree rooted at the header. Appends always
//! extend the current leaf, so a never-rewound session is one path;
//! rewinding moves the leaf to an earlier entry and the next append starts
//! a branch. Abandoned branches stay in the file, reachable through their
//! parent links.
//!
//! Unknown `kind` tags fail deserialization loudly: the file was written by
//! a newer tabit, and silently dropping records would corrupt the
//! conversation.

use rig_core::completion::{Message, Usage};
use rig_core::message::ToolResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The current session file format version. v2: tool results may carry
/// a structured `status`, and entries may reuse engine-announced turn
/// ids (both additive; the bump keeps old readers from misparsing new
/// files instead of silently dropping the new fields).
pub const SESSION_FORMAT_VERSION: u32 = 2;

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
    /// Construct an entry linking to `parent_id` at the given time, with a
    /// freshly minted id.
    pub fn new(parent_id: Option<String>, timestamp: String, kind: EntryKind) -> Self {
        Self::with_id(crate::ids::new_entry_id(), parent_id, timestamp, kind)
    }

    /// Construct an entry under a caller-provided id — the shape behind
    /// announced ids (a turn's entry reuses the id the engine announced).
    pub fn with_id(
        id: String,
        parent_id: Option<String>,
        timestamp: String,
        kind: EntryKind,
    ) -> Self {
        Self {
            id,
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
    /// The user aborted the outer loop mid-run. Bookkeeping only: the
    /// turns that completed before the abort are already recorded; calls
    /// the abort interrupted are repaired on the next open, exactly like a
    /// crash. Not part of model context.
    Aborted,
    /// The leaf moved to the entry `to` — a rewind (branch point). The
    /// marker's own parent records where the previous chain ended, and
    /// entries appended after it extend from `to` instead. Bookkeeping
    /// only: not part of model context. A marker as the final line makes
    /// the rewind durable even when nothing is appended after it.
    Rewound {
        /// The entry the active chain now ends at; `None` branches from
        /// the root (an empty chain).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
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
