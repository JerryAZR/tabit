//! Chain checkouts: rewind by count or to an entry, over the one shared
//! checkout mechanics.

use super::Session;
use crate::entry::{FileRecord, SideKind, SideRecord};
use crate::error::SessionError;

/// What a rewind did: how many user messages left the active chain, and
/// the entry the chain now ends at (the branch point; empty for a branch
/// from the root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindSummary {
    /// How many trailing user messages the rewind dropped from the chain.
    pub dropped: usize,
    /// The entry the active chain now ends at.
    pub to_entry: String,
}

impl Session {
    /// Rewind the active chain by `turns` user messages: the leaf moves to
    /// the parent of the `turns`-th-most-recent `user_message` entry (a
    /// prompt or a steer — both are valid "I should have said something
    /// else here" points), and the next prompt branches from there. The
    /// dropped entries stay in the file as a sibling branch.
    ///
    /// Idle only — `&mut self` cannot alias a run in flight. The rewind is
    /// durable on its own: a `rewound` marker lands in the log even if no
    /// prompt follows.
    pub fn rewind(&mut self, turns: usize) -> Result<RewindSummary, SessionError> {
        let branch = crate::lock::read(&self.conversation).active_branch();
        let boundaries = tabit_log::user_message_boundaries(&branch);
        if turns == 0 {
            return Err(SessionError::Config {
                message: "rewind needs at least 1 user message to drop".to_string(),
            });
        }
        let Some(target) = turns
            .checked_sub(1)
            .and_then(|offset| boundaries.len().checked_sub(1 + offset))
            .and_then(|index| boundaries.get(index))
        else {
            return Err(SessionError::Config {
                message: format!(
                    "cannot rewind {turns} user message(s): the active branch holds {}",
                    boundaries.len()
                ),
            });
        };
        // The branch point is the boundary's parent; the conversation
        // continues from there.
        self.apply_checkout(target.parent_id.as_deref())
    }

    /// Rewind to an exact entry: the active branch will end at that
    /// entry. Any **roundtrip-closed** node in the tree is a valid
    /// target, on or off the active branch (this is also how a branch
    /// switch happens); a target inside an open tool roundtrip panics
    /// (the flag-23 ruling: unsupported, revisited later). The library
    /// primitive for tree-picking frontends — [`Session::rewind`] is the
    /// user-facing form.
    pub fn rewind_to_entry(&mut self, entry_id: &str) -> Result<RewindSummary, SessionError> {
        self.apply_checkout(Some(entry_id))
    }

    /// Shared checkout mechanics: move the recorder's head (closed-path
    /// rule enforced at the door) and re-project the context from the new
    /// branch. The selection is a session preference (owner ruling
    /// 2026-08): a checkout moves the head, never the register — the
    /// model that answers next is unchanged by this move. The
    /// `checkout` side record rides the outbox like any record (flag 8
    /// — degraded notices announce a failed flush; the ruling keeps it
    /// non-barrier).
    fn apply_checkout(&mut self, to: Option<&str>) -> Result<RewindSummary, SessionError> {
        let before = {
            let branch = crate::lock::read(&self.conversation).active_branch();
            tabit_log::user_message_boundaries(&branch).len()
        };
        crate::lock::write(&self.conversation)
            .checkout(to)
            .map_err(
                |crate::context_manager::CheckoutError(target)| SessionError::Config {
                    message: format!("checkout target `{target}` is not in this session"),
                },
            )?;
        if let Err(error) =
            crate::lock::lock(&self.buffer).enqueue(&[FileRecord::Side(SideRecord {
                timestamp: crate::ids::now_rfc3339(),
                kind: SideKind::Checkout {
                    to: to.map(str::to_string),
                },
            })])
        {
            tracing::warn!(%error, "checkout record failed to flush; queued for retry");
        }
        let after = {
            let branch = crate::lock::read(&self.conversation).active_branch();
            tabit_log::user_message_boundaries(&branch).len()
        };
        Ok(RewindSummary {
            dropped: before.saturating_sub(after),
            to_entry: to.unwrap_or_default().to_string(),
        })
    }
}
