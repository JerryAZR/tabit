#![cfg_attr(docsrs, feature(doc_cfg))]
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
//! Rig's classic agent runtime.
//!
//! This crate owns the mature builder, the driving loop, the tool-phase hook
//! pair, contextual tool registry, memory orchestration, and the shared
//! blocking/streaming driver. Portable provider, message, tool, and storage
//! contracts remain in [`tabit_providers`] and are reachable here through the
//! explicit [`core`] namespace; this crate's root deliberately exports only
//! runtime-owned items. The comprehensive end-user facade is the root `rig`
//! crate.
//!
//! # Target support
//!
//! Native targets are fully supported. `wasm32-unknown-unknown` (browser) is
//! supported with no feature flags to set — the relaxed async bounds follow
//! from the target alone.
//!
//! The `rmcp` feature is unavailable on wasm: rmcp's `ClientHandler` requires
//! `Send + Sync` unconditionally, which this crate's wasm tool registry cannot
//! satisfy, so asking for it there raises a targeted `compile_error!`. WASI
//! (`wasm32-wasip1`/`wasip2`) is **not supported**: the dependency graph does
//! not build for it. See the crate README for the full matrix and the
//! reasoning.

/// Direct access to portable provider, data, memory, and tool contracts.
///
/// This explicit namespace is also the stable expansion root for portable
/// `#[rig_tool]` functions in crates that depend on `tabit-engine` without a
/// separate direct `tabit-providers` dependency.
///
/// Portable `tabit-providers` root items are reachable here, but deliberately *not*
/// at the `tabit_engine` crate root — adding a root export to `tabit-providers` must not
/// silently add one to `tabit-engine`. A stable `tabit-providers` root export
/// ([`tabit_providers::OneOrMany`]) demonstrates both halves of that invariant (the
/// two doctests below enforce it):
///
/// ```
/// // Reachable through the explicit `core` namespace.
/// use tabit_engine::core::OneOrMany;
/// let _reachable: Option<OneOrMany<u8>> = None;
/// ```
///
/// ```compile_fail
/// // NOT reachable at the `tabit_engine` crate root.
/// use tabit_engine::OneOrMany as _;
/// ```
pub mod core {
    pub use tabit_providers::*;
}

pub mod agent;
pub mod client;
pub mod completion;
// Shared JSON helpers live in tabit-providers; re-export so call sites stay
// `json_utils::merge` / `json_utils::serialize_json_value`.
pub(crate) use tabit_providers::json_utils;
pub mod prelude;
pub mod streaming;
#[cfg(any(test, feature = "test-utils"))]
#[cfg_attr(docsrs, doc(cfg(feature = "test-utils")))]
pub mod test_utils;
pub mod tool;

pub use agent::{
    Agent, AgentBuilder, AgentHook, AgentRunner, HookContext, ModelHandle, SteeringSource,
    TurnIdSource,
};

#[cfg(feature = "derive")]
#[cfg_attr(docsrs, doc(cfg(feature = "derive")))]
pub use tabit_derive::rig_tool;
#[cfg(feature = "derive")]
#[cfg_attr(docsrs, doc(cfg(feature = "derive")))]
pub use tabit_derive::rig_tool as tool_macro;
