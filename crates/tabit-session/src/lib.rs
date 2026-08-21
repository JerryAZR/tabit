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
//! Tabit sessions: persistent, resumable conversations over the rig-agent
//! outer loop.
//!
//! A session is one JSONL file (header + append-only entries) under a
//! caller-chosen directory — project-local by default, because a path
//! relative to the project survives renames and moves. The session layer
//! is the *policy owner* around the rig-agent engine: it selects the model
//! for each outer loop (from `tabit-config`), replays the log into model
//! context, persists every completed record as it happens (assistant
//! turns, tool results, model switches), and folds the engine's item
//! stream into the serializable [`SessionEvent`] list a frontend
//! consumes.
//!
//! Native targets only: sessions are filesystem-backed (and UUIDv7 ids
//! need OS entropy), so this crate deliberately does not build for
//! `wasm32-unknown-unknown`. The browser-compatible surface remains the
//! rig crates.
//!
//! Terminology (see AGENTS.md): one [`Session::prompt`] is one **outer
//! loop**; a **turn** is one model call within it; the **tool-use
//! roundtrip** is the boundary between a turn's tool calls and the next
//! model call. Steering, permission checks, and the extension framework
//! are future insertions at that boundary.
//!
//! # Example
//!
//! ```no_run
//! # async fn example() -> Result<(), tabit_session::SessionError> {
//! use std::sync::Arc;
//! use tabit_config::{AuthConfig, TabitConfig};
//! use tabit_session::{ModelSelection, SessionBuilder, SessionStore};
//!
//! let config = Arc::new(TabitConfig::load_default()?);
//! let auth = Arc::new(AuthConfig::load_default()?);
//! let store = SessionStore::project_default();
//! let selection = ModelSelection::new("lmstudio", "openai/gpt-oss-20b");
//!
//! let mut session = SessionBuilder::new(store, config, auth, selection)?
//!     .create("C:/work/project")?;
//! let run = session.prompt("explain this repository").await;
//! println!("{}", run.output);
//!
//! // Resuming is the same builder with `.resume(path)` instead of
//! // `.create(cwd)` — the log is the source of truth. A fresh session
//! // leaves no file behind until its first user message.
//! # Ok(())
//! # }
//! ```
//!
//! [`Session::prompt`]: crate::Session::prompt

mod endpoint;
mod entry;
mod error;
mod ids;
mod lock;
mod model;
mod projection;
mod prompt;
mod recorder;
mod registry;
mod session;
mod store;

pub use endpoint::{SessionCommandLink, SessionHandle, SessionInfo};
pub use entry::{EntryKind, SESSION_FORMAT_VERSION, SessionEntry, SessionHeader};
pub use error::SessionError;
pub use model::validate_selection;
pub use projection::DanglingToolCalls;
pub use prompt::build_system_prompt;
pub use registry::ModelRegistry;
pub use session::{
    AbortHandle, DEFAULT_MAX_TURNS, MailboxHandle, ModelStats, RewindSummary, RunOutcome,
    RunSummary, Session, SessionBuilder, SessionStats,
};
pub use store::{LoadedSession, Repair, SessionStore, SessionSummary, SessionWriter};
pub use tabit_protocol::{
    ClientFrame, EventFrame, ModelSelection, PROTOCOL_VERSION, ServerControlFrame, ServerFrame,
    SessionCommand, SessionEvent, StreamId,
};

#[cfg(test)]
mod tests;
