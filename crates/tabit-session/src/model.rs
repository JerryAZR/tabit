//! The session's model selection type: the `(provider, model,
//! thinking level)` triple the session layer switches between.
//! Construction lives in the [`crate::ModelRegistry`].

use crate::error::SessionError;
use serde::{Deserialize, Serialize};
use tabit_config::TabitConfig;

/// A `(provider, model)` pair the session can switch between.
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

    /// Validate the selection resolves in the config (provider, model, and
    /// — when set — the thinking level name).
    pub fn validate(&self, config: &TabitConfig) -> Result<(), SessionError> {
        let provider = config
            .provider(&self.provider)
            .ok_or_else(|| SessionError::Config {
                message: format!("provider `{}` (check providers.toml)", self.provider),
            })?;
        let model = provider
            .model(&self.model)
            .ok_or_else(|| SessionError::Config {
                message: format!("model `{}` for provider `{}`", self.model, self.provider),
            })?;
        if let Some(level) = &self.thinking_level
            && model.thinking_level(level).is_none()
        {
            return Err(SessionError::Config {
                message: format!(
                    "thinking level `{level}` for model `{}` (defined levels: {})",
                    self.model,
                    model
                        .thinking_levels
                        .iter()
                        .map(|l| l.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
