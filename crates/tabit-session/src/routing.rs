//! The child router: routing's second table (owner ruling 2026-09).
//!
//! The host's worker map resolves the sessions the *host* owns; every
//! other session address belongs to a subagent child. Routing is
//! uniform and filter-free (**route all commands** — the owner
//! ruling: the router never decides): an address that resolves to a
//! child is forwarded there, an address that resolves to nothing is
//! the host's existing `error { kind: session }`.
//!
//! Children are subprocess session hosts (the one substrate): every
//! session command works on a child **structurally** — the router
//! forwards the wire line and the child's own host consumes it, the
//! same host every user session gets. No child-specific consumption
//! code exists, by design.
//!
//! Deep trees route by **learning** — the Ethernet-switch model: the
//! subprocess bridge snoops every frame it forwards, and a frame's
//! stamp teaches which child subtree owns that id. A command
//! addressed to a grandchild walks hop by hop, each process's router
//! forwarding to the child pipe it learned the id from. No wire
//! change, no id rewriting; an id that never emitted anything is
//! unroutable — and a child that never announced is dead on arrival,
//! because `session_opened` is the first unconditional emission.
//!
//! Abort is the one command with tree semantics (owner ruling):
//! consumption at the target is *cancel + broadcast to my children*,
//! recursively — abort stops all work in a subtree without destroying
//! the session instances. The broadcast crosses into each child as
//! one routed command; the child's own host cascades inside its
//! process, through this same router.

use crate::lock::lock;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tabit_protocol::SessionCommand;

#[cfg(test)]
#[path = "routing_tests.rs"]
mod tests;

/// One registered child: its parent (abort's tree walk) and its
/// command pipe — line-form commands into the subprocess child's
/// stdin writer. Every command crosses unfiltered; the child's own
/// host consumes it (and cascades aborts to its own children), which
/// is the structural guarantee that every session command works on a
/// child with zero child-specific code: a child IS a session host.
#[derive(Clone)]
struct Child {
    parent: String,
    commands: tokio::sync::mpsc::UnboundedSender<String>,
}

/// Router state: children by their own id, plus the learned table —
/// every routable id (the children themselves and every descendant a
/// bridge saw emit) mapping to the owning child.
#[derive(Default)]
struct RouterState {
    children: HashMap<String, Child>,
    routes: HashMap<String, String>,
}

/// The process-wide child registry — one per host; the binary shares
/// it with [`crate::subagent::SubagentParts`] so spawns register and
/// routing sees the same table.
#[derive(Default)]
pub struct ChildRouter {
    state: Mutex<RouterState>,
}

impl ChildRouter {
    /// One shared router (the assembly hands the same `Arc` to the
    /// host wiring and the subagent parts).
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a child at spawn. Routing begins here: the child's
    /// own id resolves to itself.
    pub(crate) fn register(
        &self,
        child_id: &str,
        parent_id: &str,
        commands: tokio::sync::mpsc::UnboundedSender<String>,
    ) {
        let mut state = lock(&self.state);
        state.children.insert(
            child_id.to_string(),
            Child {
                parent: parent_id.to_string(),
                commands,
            },
        );
        state
            .routes
            .insert(child_id.to_string(), child_id.to_string());
    }

    /// Learn a routable id: `descendant` emitted through `child`'s
    /// subtree (the bridge's snoop at its forwarding site). Idempotent.
    pub(crate) fn learn(&self, descendant: &str, child: &str) {
        lock(&self.state)
            .routes
            .insert(descendant.to_string(), child.to_string());
    }

    /// Unregister a child and everything learned through it (the
    /// process exit's cleanup).
    pub(crate) fn unregister(&self, child_id: &str) {
        let mut state = lock(&self.state);
        state.children.remove(child_id);
        state.routes.retain(|_, owner| owner != child_id);
    }

    /// Route-and-forward one command addressed to `session`. Returns
    /// whether a child took it — `false` means no such session
    /// anywhere, and the host's unknown-session error follows.
    /// Forwarding is all the router does; the child's host consumes.
    pub(crate) fn deliver(&self, session: &str, command: SessionCommand) -> bool {
        let owner = lock(&self.state).routes.get(session).cloned();
        let Some(owner) = owner else {
            return false;
        };
        let child = lock(&self.state).children.get(&owner).cloned();
        let Some(child) = child else {
            // A learned route outliving its child (the unregister
            // retains away exactly these; a racing frame can re-learn
            // one) — treat as unroutable.
            return false;
        };
        if let Ok(line) = serde_json::to_string(&command) {
            // A closed writer is a child mid-exit: the command's fate
            // is the process's, visible on its stream ending — nothing
            // to say here.
            let _ = child.commands.send(line);
        }
        true
    }

    /// Abort every child of `parent` — the abort-consumption rule
    /// (cancel my work, then stop my children's, recursively). The
    /// leash cascade (run tokens) already covers children inside a
    /// live run; this walk is the session-tree semantic, and also
    /// reaches anything a finished run left registered.
    pub(crate) fn broadcast_abort(&self, parent: &str) {
        let children: Vec<(String, Child)> = lock(&self.state)
            .children
            .iter()
            .filter(|(_, child)| child.parent == parent)
            .map(|(id, child)| (id.clone(), child.clone()))
            .collect();
        for (id, child) in children {
            // The routed command is consumed by the child's own host:
            // cancel + its own recursive broadcast, through this same
            // router inside its process.
            if let Ok(line) = serde_json::to_string(&SessionCommand::Abort { session: id }) {
                let _ = child.commands.send(line);
            }
        }
    }
}
