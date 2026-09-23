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
//! - [`router`]: THE event router — register by kind or wildcard,
//!   dispatch, retract by owner; each callback owns its own dispatch
//!   (a thread, a pipe, a channel — the router never queues).
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
pub mod process;
pub mod router;
pub mod routing;
