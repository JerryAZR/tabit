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

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
