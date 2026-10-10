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
//! Tabit extension host: subprocess executables over
//! a frozen JSONL pipe — the subagent substrate, generalized.
//!
//! One supervisor per backend process. The `tabit-core` binary owns it:
//! extensions are backend machinery, and their contributions (tools,
//! hooks) reach sessions through the binary's assembly, never through
//! tabit-session. This crate is the leaf below that wiring — it knows
//! processes and frames, nothing about sessions or models.
//!
//! Scope: discovery, the report-first
//! handshake, supervision, and the death policy — plus the lanes the
//! frames ride: the tool lane (`tool_call` out, `tool_result` back),
//! the hook lane, and the host-service envelope the SDK's asks
//! travel in.

pub mod manifest;
pub mod protocol;
pub mod supervisor;

pub use supervisor::LaunchContext;
