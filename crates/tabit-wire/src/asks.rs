//! Pending asks — THE registry for "an id awaiting an answer,"
//! instantiated by every node (the 2026-09 unification: five homes
//! of the same mutex-map-with-first-wins-removal collapsed into one
//! type; concern identity is the output artifact, and they all
//! produce "an answer delivered to whoever asked, exactly once").
//!
//! The law lives here once: **answers are races** — the first
//! arrival takes the entry (the `take` is atomic with the lookup),
//! every later answer finds a gone id and is a tolerated no-op, and
//! death retracts what its filter matches. Settlement *announcements*
//! (telling every channel holding the card) are the caller's: only
//! grammar-facing registries announce, and their sinks differ. What
//! is shared is the bookkeeping; what is local is the delivery.

use std::collections::HashMap;
use std::sync::Mutex;

use tabit_log::lock::lock;

/// Id-keyed pending entries, one per node per vocabulary. `V` is the
/// delivery — a oneshot, a channel, a lane writer: a transport fact
/// of the asking hop, not a different semantic.
pub struct PendingAsks<V> {
    pending: Mutex<HashMap<String, V>>,
}

impl<V> Default for PendingAsks<V> {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }
}

impl<V> PendingAsks<V> {
    /// Register one awaiting ask under its id. A colliding id (the
    /// asker reused a live id — its bug, not a routing event)
    /// replaces the earlier entry.
    pub fn insert(&self, id: String, delivery: V) {
        lock(&self.pending).insert(id, delivery);
    }

    /// Claim one answer: the atomic first-wins remove. `None` is the
    /// dead id — unknown, already answered, or retracted — the
    /// caller's tolerated no-op.
    pub fn take(&self, id: &str) -> Option<V> {
        lock(&self.pending).remove(id)
    }

    /// Retract every entry the filter matches (a death's sweep), in
    /// registration order. Callers announce settlements for the
    /// returned ids as their surface demands.
    pub fn retract_where<F: Fn(&V) -> bool>(&self, matches: F) -> Vec<(String, V)> {
        let matching: Vec<String> = lock(&self.pending)
            .iter()
            .filter(|(_, value)| matches(value))
            .map(|(id, _)| id.clone())
            .collect();
        matching
            .into_iter()
            .filter_map(|id| lock(&self.pending).remove(&id).map(|value| (id, value)))
            .collect()
    }

    /// Read one entry without claiming it (a dispatcher peeking the
    /// correlation's context). The entry stays; only `take` settles.
    pub fn peek<R>(&self, id: &str, read: impl FnOnce(&V) -> R) -> Option<R> {
        lock(&self.pending).get(id).map(read)
    }

    /// Retract everything (a terminal's sweep — the askers died with
    /// their run).
    pub fn retract_all(&self) -> Vec<(String, V)> {
        lock(&self.pending).drain().collect()
    }
}
