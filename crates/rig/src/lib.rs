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
//! Public facade for Rig.
//!
//! The `rig` crate is the user-facing entry point for Rig. It re-exports the
//! portable contracts from `rig_core` at their familiar `rig::...` paths and the
//! classic runtime from `rig_agent` under `rig::agent`.
//!
//! `rig::tool` keeps the classic contextual tool API (`Tool`, `ToolContext`,
//! …) with the default `agent` feature — the same surface as before the runtime
//! split — and always exposes the runtime-independent contracts explicitly as
//! `PortableTool`, `PortableToolEmbedding`, and `PortableDynamicTool`. The
//! classic API also lives at [`crate::agent::tool`]. Classic construction
//! methods such as `client.agent(...)` come from
//! [`crate::client::AgentClientExt`]; `use rig::prelude::*;` brings it in
//! alongside the canonical `CompletionClient`, the same surface as before the
//! split.
//!
//! # When to use `rig-core` directly
//!
//! Depend on the `rig-core` package directly when you only need the core Rig
//! implementation crate, including provider abstractions, built-in core
//! providers, tools, and vector-store traits, without the root
//! facade's companion integration feature surface.

pub use rig_core::*;

#[cfg(feature = "agent")]
#[cfg_attr(docsrs, doc(cfg(feature = "agent")))]
pub use rig_agent::{Agent, AgentBuilder, AgentRunner};

/// Direct access to the portable provider and data contracts.
pub mod core {
    pub use rig_core::*;
}

/// Classic agent orchestration and lifecycle APIs.
#[cfg(feature = "agent")]
#[cfg_attr(docsrs, doc(cfg(feature = "agent")))]
pub mod agent {
    pub use rig_agent::agent::*;

    /// Contextual tools for the classic agent runtime.
    pub mod tool {
        pub use rig_agent::tool::*;
    }
}

/// Provider clients plus classic agent constructors.
pub mod client {
    // Classic-runtime construction extensions: `agent()` on any completion
    // client (`AgentClientExt`) and `into_agent_builder()` on any completion
    // model (`AgentModelExt`).
    #[cfg(feature = "agent")]
    pub use rig_agent::client::{AgentClientExt, AgentModelExt};

    // The full portable provider-client surface, including the canonical
    // `CompletionClient`. `AgentClientExt` is a distinct name, so there is no
    // shadow — just one canonical completion-client trait plus the classic
    // construction extension.
    pub use rig_core::client::*;
}

/// Low-level completion contracts plus classic prompting traits and errors.
pub mod completion {
    #[cfg(feature = "agent")]
    pub use rig_agent::completion::{Prompt, PromptError};
    pub use rig_core::completion::*;
}

/// Common portable imports plus additive classic-runtime conveniences.
pub mod prelude {
    // The classic contextual `Tool` and its mutable `ToolContext` — the same
    // prelude surface as before the runtime split, so `use rig::prelude::*;
    // impl Tool for X {…}` keeps working.
    #[cfg(feature = "agent")]
    pub use crate::tool::{Tool, ToolContext};
    // The classic construction extension `AgentClientExt` (adding `agent()`)
    // sits alongside the canonical `CompletionClient` brought in by the
    // `rig_core::prelude::*` glob below. The two traits share no method
    // names, so both resolve without ambiguity and together restore the
    // pre-split `client.completion_model(m)` / `client.agent(m)` surface.
    #[cfg(feature = "agent")]
    pub use rig_agent::prelude::{
        Agent, AgentClientExt, AgentModelExt, MultiTurnStreamItem, Prompt, PromptError,
        StreamingChat, StreamingPrompt, StreamingResult, ToolSet,
    };
    pub use rig_core::prelude::*;
}

/// Low-level streaming values plus classic streaming traits.
pub mod streaming {
    #[cfg(feature = "agent")]
    pub use rig_agent::streaming::{StreamingChat, StreamingPrompt};
    pub use rig_core::streaming::*;
}

/// Tools for the default (classic) runtime.
///
/// With the `agent` feature (on by default), `Tool`, `ToolContext`, and friends
/// here are the classic *contextual* tool API — the same surface as before the
/// runtime split, so `use rig::tool::{Tool, ToolContext};` keeps working. The
/// runtime-independent portable contracts are always exposed explicitly as
/// [`crate::tool::PortableTool`], [`crate::tool::PortableToolEmbedding`], and
/// [`crate::tool::PortableDynamicTool`] (and in full under
/// [`crate::tool::portable`]). The classic API also lives at
/// [`crate::agent::tool`] for code that prefers the explicit runtime path.
pub mod tool {
    // Canonical execution values — portable, always available.
    pub use rig_core::tool::{
        IntoToolOutput, ToolErrorKind, ToolExecutionError, ToolOutput, ToolResult,
    };
    // Runtime-independent portable contracts — explicit, always available.
    pub use rig_core::tool::{
        PortableDynamicTool, PortableTool, PortableToolEmbedding, portable_tool_definition,
    };
    // Built-in portable tools (e.g. `ThinkTool`), always available.
    pub use rig_core::tool::builtin;

    // Classic contextual tool API (default runtime). `Tool`/`ToolContext` are
    // the classic contextual trait and its mutable context; none of these
    // collide with the portable exports above.
    // NOTE: the upstream `rmcp` re-export (`rig_agent::tool::rmcp`) is omitted —
    // the `rmcp` companion feature is deferred in this vendored facade.
    #[cfg(feature = "agent")]
    #[cfg_attr(docsrs, doc(cfg(feature = "agent")))]
    pub use rig_agent::tool::{
        DynamicTool, MissingToolContext, Tool, ToolContext, ToolEmbedding, ToolSet, ToolSetBuilder,
        server, tool_definition,
    };

    /// The complete portable `rig-core` tool surface, under one explicit path.
    pub mod portable {
        pub use rig_core::tool::*;
    }
}

#[cfg(all(feature = "agent", any(test, feature = "test-utils")))]
#[cfg_attr(docsrs, doc(cfg(feature = "test-utils")))]
pub mod test_utils {
    pub use rig_agent::test_utils::*;
}

#[cfg(feature = "derive")]
#[cfg_attr(docsrs, doc(cfg(feature = "derive")))]
pub use rig_derive::rig_tool;
#[cfg(feature = "derive")]
#[cfg_attr(docsrs, doc(cfg(feature = "derive")))]
pub use rig_derive::rig_tool as tool_macro;
