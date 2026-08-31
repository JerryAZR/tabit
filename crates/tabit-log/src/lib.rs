//! The durable conversation — the layer between providers and agents.
//!
//! This crate owns the conversation and its file, and nothing else: the
//! record vocabulary ([`entry`]), the branch [`SessionTree`], the
//! conversation's single source of truth ([`ContextManager`]), and the
//! write buffer ([`SessionWriter`] + the [`WriteBuffer`] contract it
//! defines in the same module). It depends on rig-core only for the
//! message shapes — it does not know agents, hooks, tools, sessions, or
//! frontends exist. The engine (rig-agent) drives a `ContextManager`
//! through a run; the session layer (tabit-session) hosts the lifetime
//! (open/resume/checkout) and writes its side records through the same
//! shared buffer handle.
//!
//! The model-facing context is a **view**: `ContextManager::messages`
//! folds the active branch on every call and nothing stores it, so live
//! and reload are literally the same derivation. Commits are atomic —
//! a batch is verified whole, enqueued whole (one all-or-nothing
//! outbox unit), and the tree grows in the same operation — so a file
//! holding tool calls without their results is unrepresentable by
//! construction.
//!
//! The file half (`SessionWriter`) is native-in-practice: it compiles
//! everywhere but means something only where a filesystem exists. All
//! pure logic (tree, manager, records, the buffer contract, and a
//! [`NullBuffer`]) is usable anywhere, including wasm.

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

pub mod context_manager;
pub mod entry;
pub mod fold;
pub mod ids;
pub mod lock;
pub mod tree;
pub mod writer;

pub use context_manager::{CheckoutError, ContextManager, ConversationCell};
pub use entry::{
    EntryKind, FileRecord, SESSION_FORMAT_VERSION, SessionEntry, SessionHeader, SideKind,
    SideRecord,
};
pub use fold::{calls_of, fold_branch, tail_is_closed, user_message_boundaries};
pub use ids::{filename_timestamp, new_entry_id, new_session_id, now_rfc3339};
pub use tree::{SessionTree, TreeFault};
pub use writer::{NullBuffer, SessionWriter, SharedBuffer, WriteBuffer};

mod error;
pub use error::LogError;
