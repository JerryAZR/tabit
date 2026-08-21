//! Selection validation — the `(provider, model, thinking level)`
//! shape itself is protocol vocabulary ([`ModelSelection`] in
//! tabit-protocol); construction lives in the [`crate::ModelRegistry`],
//! resolution against tabit config here.

use crate::error::SessionError;
use tabit_config::TabitConfig;
use tabit_protocol::ModelSelection;

/// Validate that the selection resolves in the config (provider,
/// model, and — when set — the thinking level name).
pub fn validate_selection(
    selection: &ModelSelection,
    config: &TabitConfig,
) -> Result<(), SessionError> {
    let provider = config
        .provider(&selection.provider)
        .ok_or_else(|| SessionError::Config {
            message: format!("provider `{}` (check providers.toml)", selection.provider),
        })?;
    let model = provider
        .model(&selection.model)
        .ok_or_else(|| SessionError::Config {
            message: format!(
                "model `{}` for provider `{}`",
                selection.model, selection.provider
            ),
        })?;
    if let Some(level) = &selection.thinking_level
        && model.thinking_level(level).is_none()
    {
        return Err(SessionError::Config {
            message: format!(
                "thinking level `{level}` for model `{}` (defined levels: {})",
                selection.model,
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

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
