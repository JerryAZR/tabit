//! The child router: routing's second table (owner ruling 2026-09).
//!
//! The host's worker map resolves the sessions the *host* owns; every
//! other session address belongs to a subagent child — in-process
//! children (registered by [`crate::subagent::SpawnContext`] at
//! `announce`, unregistered at `drive`'s end) or subprocess children
//! (registered by the bridge at spawn). Routing is uniform and
//! filter-free (**route all commands** — the owner ruling: the router
//! never decides): an address that resolves to a child is delivered
//! there, an address that resolves to nothing is the host's existing
//! `error { kind: session }`.
//!
//! Consumption is **the session's own** ([`crate::session::SessionCommands`]
//! — one implementation, shared with the worker's dequeue point): an
//! in-process child consumes every command exactly as a worker-owned
//! session does; a subprocess child's own host consumes across the
//! pipe. No subsets, no rejections.
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
//! the session instances. In-process consumption cascades through
//! [`SessionCommands::abort`] into this table; subprocess children
//! receive the routed command and cascade inside their own process,
//! through this same router.

use crate::lock::lock;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tabit_protocol::SessionCommand;

#[cfg(test)]
#[path = "routing_tests.rs"]
mod tests;

/// One registered child's delivery surface.
#[derive(Clone)]
pub(crate) enum ChildTarget {
    /// An in-process child: the session's own command consumption —
    /// every arm, the same semantics a worker-owned session gets.
    InProcess {
        commands: crate::session::SessionCommands,
    },
    /// A subprocess child: line-form commands into the child's stdin
    /// writer. Every command crosses unfiltered — the child's own
    /// host consumes it (and cascades aborts to its own children).
    Process {
        commands: tokio::sync::mpsc::UnboundedSender<String>,
    },
}

/// One registered child: its parent (abort's tree walk) and its
/// delivery surface.
#[derive(Clone)]
struct Child {
    parent: String,
    target: ChildTarget,
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

    /// Register a child (at `announce` for in-process children, at
    /// spawn for subprocess ones). Routing begins here: the child's
    /// own id resolves to itself.
    pub(crate) fn register(&self, child_id: &str, parent_id: &str, target: ChildTarget) {
        let mut state = lock(&self.state);
        state.children.insert(
            child_id.to_string(),
            Child {
                parent: parent_id.to_string(),
                target,
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

    /// Unregister a child and everything learned through it (drive's
    /// end for in-process children; process exit for subprocess ones).
    pub(crate) fn unregister(&self, child_id: &str) {
        let mut state = lock(&self.state);
        state.children.remove(child_id);
        state.routes.retain(|_, owner| owner != child_id);
    }

    /// Route-and-deliver one command addressed to `session`. Returns
    /// whether a child took it — `false` means no such session
    /// anywhere, and the host's unknown-session error follows.
    /// Delivery is the target's consumption: the session's own arm
    /// set in-process, the child host's across the pipe.
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
        match &child.target {
            ChildTarget::Process { commands } => {
                if let Ok(line) = serde_json::to_string(&command) {
                    // A closed writer is a child mid-exit: the
                    // command's fate is the process's, visible on its
                    // stream ending — nothing to say here.
                    let _ = commands.send(line);
                }
            }
            ChildTarget::InProcess { commands } => commands.deliver(command),
        }
        true
    }

    /// Abort every child of `parent` — the abort-consumption rule
    /// (cancel my work, then stop my children's, recursively). The
    /// leash cascade (run tokens) already covers children inside a
    /// live run; this walk is the session-tree semantic, and also
    /// reaches anything a finished run left registered.
    pub(crate) fn broadcast_abort(&self, parent: &str) {
        let children: Vec<(String, ChildTarget)> = lock(&self.state)
            .children
            .iter()
            .filter(|(_, child)| child.parent == parent)
            .map(|(id, child)| (id.clone(), child.target.clone()))
            .collect();
        for (id, target) in children {
            match target {
                // Cancel + cascade — the consumption is the session's
                // own, including its recursive broadcast into this
                // table.
                ChildTarget::InProcess { commands } => commands.abort(),
                ChildTarget::Process { commands } => {
                    // The routed command is consumed by the child's own
                    // host: cancel + its own recursive broadcast.
                    if let Ok(line) = serde_json::to_string(&SessionCommand::Abort { session: id })
                    {
                        let _ = commands.send(line);
                    }
                }
            }
        }
    }
}
