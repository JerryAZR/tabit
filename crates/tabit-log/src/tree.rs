//! The session's conversation tree: memory-only, no I/O.
//!
//! Every conversation node the session has ever held lives here, by id
//! — all branches, not just the active one; abandoned branches are the
//! checkout targets of the future. The **head** names the branch being
//! grown (git-style): appends attach as children of the head and
//! advance it, a checkout moves it. The tree knows nothing about the
//! file, the context, or the writer — it is the history structure the
//! durable layer's door grows and the parser fills.

use crate::entry::SessionEntry;
use std::collections::HashMap;

/// A structural fault the tree refuses to paper over. Memory-only: the
/// caller attaches the file path (or the panic) it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFault(pub String);

/// The conversation tree plus its head pointer.
#[derive(Debug, Clone, Default)]
pub struct SessionTree {
    nodes: HashMap<String, SessionEntry>,
    head: Option<String>,
}

impl SessionTree {
    /// An empty tree (the root; the head points nowhere).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Insert a node loaded from a session file under its recorded
    /// parent and advance the head to it. The parent must equal the
    /// current head — the append invariant every writer preserves, so a
    /// violation is a corrupt file, named as such.
    pub fn load_append(&mut self, entry: SessionEntry) -> Result<(), TreeFault> {
        if entry.parent_id != self.head {
            return Err(TreeFault(format!(
                "entry `{}` parents `{:?}` but the head at that point was `{:?}`",
                entry.id, entry.parent_id, self.head
            )));
        }
        if self.nodes.contains_key(&entry.id) {
            return Err(TreeFault(format!("duplicate entry id `{}`", entry.id)));
        }
        let id = entry.id.clone();
        self.head = Some(id.clone());
        self.nodes.insert(id, entry);
        Ok(())
    }

    /// Append a node at the current head — the door's grow step. The
    /// node's parent **is** the head by construction (the door chains
    /// its batches); anything else is an internal wiring bug and fails
    /// loud (AGENTS.md error doctrine).
    #[allow(clippy::panic)] // sanctioned crash: an engine wiring bug, failed loud (AGENTS.md doctrine)
    pub fn append(&mut self, entry: SessionEntry) -> String {
        if entry.parent_id != self.head {
            panic!(
                "session tree: node `{}` parents `{:?}` but the head is `{:?}` — \
                 the commit door constructs parents against the head",
                entry.id, entry.parent_id, self.head
            );
        }
        let id = entry.id.clone();
        self.head = Some(id.clone());
        self.nodes.insert(id.clone(), entry);
        id
    }

    /// Move the head (a checkout). The target must name a node the tree
    /// holds; `None` moves it to the root (an empty conversation).
    pub fn move_head(&mut self, to: Option<&str>) -> Result<(), TreeFault> {
        if let Some(to) = to
            && !self.nodes.contains_key(to)
        {
            return Err(TreeFault(format!(
                "checkout target `{to}` is not in this session"
            )));
        }
        self.head = to.map(str::to_string);
        Ok(())
    }

    /// Insert a compaction node between the cut-point parent and the cut
    /// child (v4) — the one insert that is not a head-append. The
    /// entry's parent must be an existing node, `cut_child` must be an
    /// existing node whose recorded parent IS the entry's parent (the
    /// derivation's consistency check — a mismatched record is
    /// corruption, named as such), and the entry's id must be fresh. The
    /// cut child is re-parented through the compaction node **in the
    /// resident tree only** (its file record keeps the original parent;
    /// every load re-derives the insertion from this record). The head
    /// does not move: the conversation tip is unchanged, later appends
    /// chain through the insertion by walking the re-parented links.
    pub fn insert_compaction(&mut self, entry: SessionEntry) -> Result<(), TreeFault> {
        let cut_child_id = match &entry.kind {
            crate::entry::EntryKind::Compaction { cut_child, .. } => cut_child.clone(),
            _ => {
                return Err(TreeFault(format!(
                    "tree.insert_compaction: entry `{}` is not a compaction node",
                    entry.id
                )));
            }
        };
        let parent = entry.parent_id.clone().ok_or_else(|| {
            TreeFault(format!(
                "compaction entry `{}` has no parent — the node before the cut is required",
                entry.id
            ))
        })?;
        if !self.nodes.contains_key(&parent) {
            return Err(TreeFault(format!(
                "compaction entry `{}` parents unknown node `{parent}`",
                entry.id
            )));
        }
        if self.nodes.contains_key(&entry.id) {
            return Err(TreeFault(format!("duplicate entry id `{}`", entry.id)));
        }
        let cut_child = self.nodes.get_mut(&cut_child_id).ok_or_else(|| {
            TreeFault(format!(
                "compaction entry `{}` names unknown cut child `{cut_child_id}`",
                entry.id
            ))
        })?;
        if cut_child.parent_id.as_deref() != Some(parent.as_str()) {
            return Err(TreeFault(format!(
                "compaction entry `{}` parents `{parent}` but its cut child `{cut_child_id}` \
                 records parent `{:?}` — the insertion must name an existing edge",
                entry.id, cut_child.parent_id
            )));
        }
        cut_child.parent_id = Some(entry.id.clone());
        self.nodes.insert(entry.id.clone(), entry);
        Ok(())
    }

    /// The node the head points at, when the conversation is non-empty.
    pub fn head(&self) -> Option<&str> {
        self.head.as_deref()
    }

    /// Whether `id` names a node in the tree (any branch).
    pub fn contains(&self, id: &str) -> bool {
        self.nodes.contains_key(id)
    }

    /// One node by id, when held.
    pub fn node(&self, id: &str) -> Option<&SessionEntry> {
        self.nodes.get(id)
    }

    /// The branch ending at `to` (default: the head), root → `to`. A
    /// broken parent link is a fault — the tree's only inserts are
    /// head-appends and validated loads, so a broken walk names real
    /// corruption.
    pub fn path_to(&self, to: Option<&str>) -> Result<Vec<SessionEntry>, TreeFault> {
        let Some(mut current) = to.or(self.head.as_deref()).map(str::to_string) else {
            return Ok(Vec::new());
        };
        let mut branch = Vec::new();
        loop {
            let entry = self.nodes.get(&current).ok_or_else(|| {
                TreeFault(format!("branch walks through missing node `{current}`"))
            })?;
            branch.push(entry.clone());
            match &entry.parent_id {
                Some(parent) => current = parent.clone(),
                None => return Ok(branch.into_iter().rev().collect()),
            }
        }
    }

    /// The active branch (root → head) — the temporary path container,
    /// materialized on demand for replay and checkout reporting. Never
    /// maintained as state.
    pub fn path_to_head(&self) -> Vec<SessionEntry> {
        self.path_to(None).unwrap_or_default()
    }
}

#[cfg(test)]
#[path = "tree_tests.rs"]
mod tests;
