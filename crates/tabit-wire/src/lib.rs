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
//! The frozen wire's runtimes for spawned tabit-core children (the
//! extraction the extension-SDK round builds on — ruled 2026-09:
//! share what is the same).
//!
//! Two modules, one concern — being the client end of the frontend
//! protocol to a tabit-core process:
//!
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

pub mod client;
pub mod process;
