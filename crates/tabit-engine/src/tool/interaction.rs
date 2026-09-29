//! The interaction capability: ask the human a question and await the
//! answer (ENGINE.md's tool phase; FRONTEND.md §8 for the wire view).
//!
//! [`ToolContext`](super::ToolContext) carries it as
//! `Arc<dyn UserInteraction>` — the same typed-map pattern as the
//! `CancellationToken` — and a tool body may ask any number of times.
//! Hosts back it with their own routing: tabit's hub emits an
//! `interaction_request` on the event channel and routes the response
//! back by id. Pause points stay enumerable — contexts are the only
//! carriers.
//!
//! The vocabulary is routing-generic (owner ruling 2026-08): a
//! `ui_type` names the widget the frontend should render, and the
//! `payload` is opaque to the engine and to the core — askers
//! construct requests from templates (`native:*`) or their own
//! custom shapes, frontends render what they know and report what
//! they don't. A retracted question (the asker's run ended before
//! the user answered) resolves as [`InteractionOutcome::Dismissed`]
//! rather than an error — the asker is being torn down with the run
//! either way, and every stop-shaped need has its own mechanism
//! (ENGINE.md, stop taxonomy).

use futures::future::BoxFuture;
use serde_json::Value;

/// The outcome of one ask.
#[derive(Debug, Clone, PartialEq)]
pub enum InteractionOutcome {
    /// The human answered; the payload is the answer, shaped by the
    /// asking template's convention (opaque to the engine).
    Answered(Value),
    /// Nobody will ever answer: the asker's run ended under the
    /// question (terminal retraction, dropped asker, gone frontend).
    /// Consumers treat it as their fail-closed case (a permission
    /// gate denies; an ask-the-user tool reports the dismissal).
    Dismissed,
}

/// The capability: ask the user, await the answer. Object-safe — held
/// as `Arc<dyn UserInteraction>` in
/// [`ToolContext`](super::ToolContext) and by hooks that gate on it.
pub trait UserInteraction: Send + Sync {
    /// Ask: `ui_type` names the widget, `payload` is opaque cargo
    /// carried verbatim to the frontend. The interaction id is the
    /// host's to mint (born at acknowledgment). The future resolves
    /// when answered or retracted; dropping it abandons the question
    /// (drop is the cancellation).
    fn request(&self, ui_type: &str, payload: Value) -> BoxFuture<'static, InteractionOutcome>;
}
