//! The tabit composition root — the opinionated assembly as a
//! library. Everything an agent app needs ruled into one place: the
//! default toolset ([`core_tools`]), the invocation's tool filter
//! (include/exclude-if-it-exists), the extension world
//! ([`world_scan`] + [`build_registry`] + [`mount_world`]: the scan,
//! the catalog refresh, the providers-fragment merge, the skills
//! tables, the supervisor boot, the tool mount), the built-in permission gate
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
pub(crate) mod catalog;
pub mod extensions;
pub mod gate;
pub mod options;
pub mod serve;

pub use assemble::{
    ContinueMiss, Launchable, WorldBoot, assemble, build_registry, core_tools, extension_root,
    host_data, host_node, install_root, mount_world, world_scan,
};

/// Serializes unit tests that stage process-global environment
/// (`TABIT_SETTINGS`, `USERPROFILE`/`HOME`): one test's staged home
/// is another test's race.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
pub use gate::PermissionGate;
pub use options::{AppOptions, parse_model};
pub use serve::{serve_json_stdio, setup_guide, startup_banner};

/// The binary's one exit path. `std::process::exit` on Windows is
/// `ExitProcess`, which never runs the CRT `atexit` chain — and the
/// LLVM coverage runtime registers its profile flush there, so an
/// instrumented child exiting via `process::exit` writes no profraw
/// (the spawned-binary coverage attribution silently collapsed to
/// zero until this flush was added, 2026-10). Flush explicitly under
/// `cfg(coverage)` (cargo-llvm-cov sets it), then exit. Every
/// terminal exit in the binary routes through here — never around.
pub fn exit(code: i32) -> ! {
    #[cfg(coverage)]
    // SAFETY: `__llvm_profile_write_file` is the compiler-rt profile
    // runtime's public flush entry point, linked exactly when the
    // build is instrumented (which is exactly when `cfg(coverage)`
    // is set). It is idempotent and takes no state.
    unsafe {
        unsafe extern "C" {
            fn __llvm_profile_write_file() -> std::ffi::c_int;
        }
        __llvm_profile_write_file();
    }
    std::process::exit(code)
}
