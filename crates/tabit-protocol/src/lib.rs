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
//! The tabit frontend protocol: the one vocabulary every frontend and
//! transport shares (FRONTEND.md is the contract; PROTOCOL.md the
//! design record).
//!
//! Commands are fire-and-forget with total semantics — outcomes arrive
//! as events, never as responses. Events are stamped with the stream
//! that produced them and serialize flat next to their stamp, so one
//! line on the wire is one [`EventFrame`]. The crate owns its shapes:
//! nothing here depends on the engine, so engine refactors cannot
//! churn the wire silently (the reason this vocabulary left
//! tabit-session).
//!
//! Consumers: tabit-session (the backend mints and emits), the `tabit`
//! binary's stdio bridge (serialization edge), and frontends (the egui
//! GUI and any future transport client) — all against these types, no
//! codegen, no persistence internals.

mod events;
mod model;
mod protocol;
mod usage;

pub use events::{DiscardedMessage, ErrorKind, InteractionOption, SessionEvent, ToolResultStatus};
pub use model::ModelSelection;
pub use protocol::{
    ClientFrame, EventFrame, PROTOCOL_VERSION, ServerControlFrame, ServerFrame, SessionCommand,
    StreamId, to_wire_line,
};
pub use usage::Usage;
