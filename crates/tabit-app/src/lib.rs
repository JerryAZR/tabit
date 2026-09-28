//! The tabit composition root — the opinionated assembly as a
//! library. Everything an agent app needs ruled into one place: the
//! default toolset ([`core_tools`]), the invocation's tool filter
//! (include/exclude-if-it-exists), the extension world
//! ([`world_registry`] + [`mount_world`]: the scan, the
//! providers-fragment merge, the skills tables, the supervisor
//! boot, the tool mount), the built-in permission gate
//! ([`gate::PermissionGate`]), the session builders ([`assemble`],
//! [`host_data`]), and the process's node ([`host_node`]). The
//! `tabit-core` binary is one consumer — argv in, two I/O arms out;
//! an embedder is any other: construct [`AppOptions`], call the same
//! functions, own the I/O.
//!
//! The layering law holds: tabit-session stays mechanism with no
//! policy; this crate is the policy (the binary's former assembly,
//! extracted 2026-09 so the stack is embeddable above Session
//! without copying a thousand lines of glue).

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

pub mod assemble;
pub mod extensions;
pub mod gate;
pub mod options;
pub mod serve;

pub use assemble::{
    ContinueMiss, Launchable, assemble, core_tools, extension_root, host_data, host_node,
    install_root, mount_world, world_registry,
};
pub use gate::PermissionGate;
pub use options::{AppOptions, parse_model};
pub use serve::{serve_json_stdio, setup_guide, startup_banner};
