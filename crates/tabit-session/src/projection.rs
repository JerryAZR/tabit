//! Replaying session records into model-visible context.
//!
//! The context builder itself is shared with the engine
//! ([`rig_agent::agent::conversation`]) — one implementation
//! everywhere. What lives here is the node side of it: unwrapping the
//! log's records into the builder's doors (`fold_node`), the
//! whole-branch fold for load and checkout (`project`), the valid
//! checkout targets (`user_message_boundaries`), and the session
//! preference register (`last_model_change_in_file`). Side records
//! (`model_change`, `checkout`, `aborted`, …) are session state, not
//! context, and never fold.

use crate::entry::{EntryKind, FileRecord, SessionEntry};
use rig_agent::agent::conversation::Conversation;

/// Fold one conversation node into the builder.
pub fn fold_node(conversation: &mut Conversation, entry: &SessionEntry) {
    match &entry.kind {
        EntryKind::UserMessage { message } => conversation.user(message.clone()),
        EntryKind::AssistantMessage { message, .. } => conversation.assistant(message.clone()),
        EntryKind::ToolResult { result } => conversation.tool_result(result.clone()),
    }
}

/// Fold a whole branch (root → head) into a fresh builder and return
/// the flushed message list plus the dangling report — the load and
/// checkout rebuild shape.
pub fn project(
    entries: &[SessionEntry],
) -> (
    Vec<rig_core::completion::Message>,
    Option<rig_agent::agent::conversation::DanglingToolCalls>,
) {
    let mut conversation = Conversation::new();
    for entry in entries {
        fold_node(&mut conversation, entry);
    }
    let dangling = conversation.dangling();
    (conversation.messages_vec(), dangling)
}

/// The branch's `user_message` nodes in root→head order — the valid
/// user-facing checkout targets. A branch before any of them leaves the
/// branch replayable: it ends on a completed turn, a closed tool batch,
/// never mid-batch.
pub fn user_message_boundaries(entries: &[SessionEntry]) -> Vec<&SessionEntry> {
    entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::UserMessage { .. }))
        .collect()
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

#[cfg(test)]
#[path = "projection_tests.rs"]
mod tests;
