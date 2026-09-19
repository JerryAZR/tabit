//! The model selection shape: the `(provider, model, thinking
//! level)` triple carried on the wire (initialize facts, model
//! commands, model-change events). Validation against tabit config
//! lives in tabit-session — this crate knows shapes, not policy.

use serde::{Deserialize, Serialize};

/// A `(provider, model)` pair with an optional thinking level — the
/// unit of model selection on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelection {
    /// Provider id from tabit config.
    pub provider: String,
    /// Model id within the provider.
    pub model: String,
    /// Active thinking level name, when the model defines levels.
    pub thinking_level: Option<String>,
}

impl ModelSelection {
    /// A selection without a thinking level.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            thinking_level: None,
        }
    }
}

/// Per-million-token pricing for a model, in USD — the wire mirror of
/// tabit-config's cost record (this crate stays serde-only; frontends
/// link it without the config stack). Same convention as config: all
/// rates are stated, `0.0` means free or unknown.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    /// Input token rate, USD per million tokens.
    pub input: f64,
    /// Output token rate, USD per million tokens.
    pub output: f64,
    /// Cached-input token rate, USD per million tokens.
    pub cache_read: f64,
    /// Cache-write token rate, USD per million tokens.
    pub cache_write: f64,
}

/// The resolved model-record facts a `model_changed` announcement
/// carries (v11): the capacity, display, and pricing facts a frontend
/// renders next to the selection ids — a context meter's denominator,
/// a human name, a per-turn cost. Every field is optional: the config
/// states what it knows, and an unresolved record (a register left
/// stale by a config edit) is all-None — the next validated write
/// repairs it. Constructor input only; the event's fields are flat.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelFacts {
    /// The model's context window in tokens, when known.
    pub context_window: Option<u64>,
    /// The model's display name, when configured (frontends fall back
    /// to the model id).
    pub name: Option<String>,
    /// Per-million-token pricing, when configured.
    pub cost: Option<Cost>,
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
