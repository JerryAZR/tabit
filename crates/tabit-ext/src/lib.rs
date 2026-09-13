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
//! Tabit extension host (ROADMAP item 9): subprocess executables over
//! a frozen JSONL pipe — the subagent substrate, generalized.
//!
//! One supervisor per backend process. The `tabit` binary owns it:
//! extensions are backend machinery, and their contributions (tools,
//! hooks) reach sessions through the binary's assembly, never through
//! tabit-session. This crate is the leaf below that wiring — it knows
//! processes and frames, nothing about sessions or models.
//!
//! Task-1 scope (the implementation checklist in ROADMAP item 9):
//! discovery, the handshake, supervision, and the death policy. The
//! frames for tool calls, hook events, and host services land with
//! the tasks that exercise them.

pub mod manifest;
pub mod process;
pub mod protocol;
pub mod supervisor;
