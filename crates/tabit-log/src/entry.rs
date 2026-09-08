//! Session record types: the header line and the append-only log.
//!
//! One session is one JSONL file. The first line is a
//! [`SessionHeader`]; every following line is a [`FileRecord`], one of
//! two planes with different jobs (format v3):
//!
//! - **Conversation nodes** ([`SessionEntry`]) — `user_message`,
//!   `assistant_message`, `tool_result` — carry `id` and `parent_id`
//!   and form the conversation tree. Appends attach as children of the
//!   current head; a checkout moves the head to an existing node and
//!   the next append branches from there. Abandoned branches stay in
//!   the file, reachable through their parent links.
//! - **Side records** ([`SideRecord`]) — session-level facts that are
//!   not conversation: `model_change` (the selection register),
//!   `checkout` (a head move), `aborted`, `label`, `custom`. They have
//!   no id and no parent: they never enter the tree, only a linear,
//!   order-significant stream (last `model_change` wins; the last
//!   `checkout` names the active branch).
//!
//! In memory the planes stay separated: the tree plus a head pointer
//! is the history, the selection register is its own cell, and the
//! model-facing context is the live projection of the active branch —
//! nothing re-reads the file mid-session. The file mixes the planes
//! because it is the single append-only write path; the loader splits
//! them in one pass.
//!
//! Unknown `kind` tags fail deserialization loudly: the file was written by
//! a newer tabit, and silently dropping records would corrupt the
//! conversation.

use rig_core::completion::{Message, Usage};
use rig_core::message::ToolResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The current session file format version. v4: the `compaction` tree
/// node (the insertion record — its `cut_child` names the first
/// retained-tail entry; the loader re-parents that child through the
/// compaction node, so a v4 tree can hold an inserted node whose
/// parent is not the head at load time). v4 readers also accept v3
/// files (no compaction entries exist in them by construction). v3:
/// the log splits into conversation nodes (id + parent, the tree) and
/// parentless side records (`model_change`, `checkout`, `aborted`,
/// `label`, `custom`) — bookkeeping stops chaining into the tree.
/// Pre-release break: v2 files are rejected loudly, there is no
/// migration.
pub const SESSION_FORMAT_VERSION: u32 = 4;
/// Session file versions this build can read. Writers always write
/// [`SESSION_FORMAT_VERSION`]; older versions stay readable while
/// their records are a subset of the current vocabulary (a v3 file
/// cannot contain a `compaction` node).
pub const READABLE_FORMAT_VERSIONS: [u32; 2] = [3, SESSION_FORMAT_VERSION];

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

/// One append-only line of a session file: a tree node or a side
/// record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FileRecord {
    /// A conversation node.
    Node(SessionEntry),
    /// A session-level fact outside the tree.
    Side(SideRecord),
}

impl FileRecord {
    /// Whether this record is a user message — the record class that
    /// makes a session real (the no-orphan gate: a file materializes
    /// only once one of these has been enqueued).
    pub fn is_user_message(&self) -> bool {
        matches!(
            self,
            FileRecord::Node(SessionEntry {
                kind: EntryKind::UserMessage { .. },
                ..
            })
        )
    }
}

/// One conversation node: a parent-linked entry in the tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEntry {
    /// Entry id (UUIDv7 string).
    pub id: String,
    /// Parent entry id — the node the head pointed at when this node
    /// was appended; `None` only for the first node after the header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// When the entry was appended (RFC 3339).
    pub timestamp: String,
    /// The record payload.
    pub kind: EntryKind,
}

impl SessionEntry {
    /// Construct a node linking to `parent_id` at the given time, with a
    /// freshly minted id.
    pub fn new(parent_id: Option<String>, timestamp: String, kind: EntryKind) -> Self {
        Self::with_id(crate::ids::new_entry_id(), parent_id, timestamp, kind)
    }

    /// Construct a node under a caller-provided id — the shape behind
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

/// The payload of a conversation node. These three kinds are the whole
/// tree — everything the model sees.
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
    /// A compaction insertion (v4): the history before the cut, replaced
    /// by this node's summary. The node's `parent_id` is the node before
    /// the cut point; `cut_child` names the first retained-tail entry —
    /// the loader re-parents that child through this node, so the walked
    /// chain routes `[... before-cut, compaction, cut child, ... tail]`
    /// and history walkers stop here (included): the model-visible
    /// context becomes `[summary] + retained tail`. Everything before
    /// the insertion stays in the file and the tree — checkout to a
    /// pre-compaction node yields the full-history branch (compaction
    /// never deletes). The head does not move at insertion time.
    Compaction {
        /// The summary text, verbatim as the summarizer produced it.
        summary: String,
        /// The id of the first retained-tail entry (the message that
        /// immediately follows the cut point).
        cut_child: String,
        /// The estimated context tokens before this pass ran (the
        /// trigger's measurement, recorded for audit).
        tokens_before: u64,
        /// The summarization call's provider-reported usage.
        usage: Usage,
    },
}

/// A session-level fact recorded outside the tree: no id, no parent —
/// order alone carries meaning (latest in time wins).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SideRecord {
    /// When the record was appended (RFC 3339).
    pub timestamp: String,
    /// The record payload.
    pub kind: SideKind,
}

/// The payload of a side record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SideKind {
    /// The session switched provider/model/thinking level — the
    /// selection register. Applied from the next outer loop on.
    ModelChange {
        /// Provider id from tabit config.
        provider: String,
        /// Model id within the provider.
        model: String,
        /// Active thinking level name, when the model defines levels.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_level: Option<String>,
    },
    /// The head moved to the node `to` — a checkout (branch point).
    /// The nodes appended after it extend from `to`; the branch
    /// abandoned by the move stays in the tree. `None` moves the head
    /// to the root (an empty conversation).
    Checkout {
        /// The node the head now points at.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
    },
    /// The user aborted the outer loop mid-run. Bookkeeping only: the
    /// roundtrip the abort interrupted never landed (roundtrips commit
    /// atomically — there is nothing dangling to repair). Not part of
    /// model context.
    Aborted,
    /// An attempt the engine discarded — a hook veto or a malformed
    /// tool-call defect retried. The tokens were spent, so the usage is
    /// recorded here (PROTOCOL.md flag 22): stats count it, the log stays
    /// the cost source of truth. Not part of model context.
    Discarded {
        /// The provider-reported usage of the discarded attempt.
        usage: Usage,
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

#[cfg(test)]
#[path = "entry_tests.rs"]
mod tests;
