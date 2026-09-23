#![cfg_attr(
    test,
    allow(
        clippy::err_expect,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::panic_in_result_fn,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]
//! The frozen wire's shared node mechanisms and the runtimes for
//! spawned tabit-core children (the extraction the extension-SDK
//! round builds on — ruled 2026-09: share what is the same):
//!
//! - [`node`]: the node runtime — the ruled architecture assembled:
//!   one routing layer (the [`node::Channel`] primitive, the
//!   learning table, the ask table, the command-by-type handlers)
//!   with functional layers mounted on top; the net tests
//!   (`node_tests`, `tests/net.rs`) are its acceptance suite.
//!   dispatch, retract by owner, over any routed vocabulary (events
//!   by tag, commands by tag — one mechanism, no sibling tables);
//!   each callback owns its own dispatch (a thread, a pipe, a
//!   channel — the router never queues).
//! - [`asks`]: THE pending-question registry — one entry per open
//!   round-trip (an owner key plus a delivery closure over
//!   answered-or-orphaned); answers are races, the first arrival
//!   claims, late arrivals drop.
//! - [`routing`]: the ChildRouter — route-all line forwarding with
//!   learned grandchild tables (the Ethernet-switch model).
//! - [`process`]: the child-process substrate every spawning site
//!   shares (tree-kill wrapping, the stderr ring, the command writer
//!   whose close is the stdin drop, the grace reaper). Moved from
//!   `tabit-ext` (its first homes were the extension supervisor and
//!   the subagent bridge; the SDK's owned children join them).
//! - [`client`]: the frontend-role runtime — shape a child (the
//!   child-role CLI knobs), spawn it, run the bounded handshake, and
//!   hold the handle (commands out, stamped frames in, the exit
//!   machinery). The subagent bridge is the first consumer; the
//!   extension SDK's owned-session wrapper is the second.
//!
//! The wire's serve side lives with the host it drives
//! (`tabit-session`'s edge module) — one server, no sharing need;
//! this crate is the many-clients half.

pub mod asks;
pub mod client;
pub mod node;
pub mod process;
pub mod router;
pub mod routing;
