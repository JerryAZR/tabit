//! Building rig models from tabit config: the config crate's provider and
//! model entries become a [`ModelHandle`] the session can switch at
//! runtime.
//!
//! v1 wiring covers endpoint, credential, and model id. The config crate's
//! `extra_body`, `thinking_levels`, `sampling_params`, and per-model
//! headers are parsed and validated there but are not yet merged into
//! requests — the engine gains that merge when compaction/overflow work
//! lands (see ROADMAP.md item 6).

use crate::error::SessionError;
use rig_agent::agent::ModelHandle;
use rig_core::client::CompletionClient;
use rig_core::providers::{anthropic, openai};
use tabit_config::{AuthConfig, Provider, TabitConfig, WireApi};

/// Resolve `(provider, model)` ids against the config into a labeled,
/// type-erased model handle.
pub fn build_model(
    config: &TabitConfig,
    auth: &AuthConfig,
    provider_id: &str,
    model_id: &str,
) -> Result<ModelHandle, SessionError> {
    let provider = config
        .provider(provider_id)
        .ok_or_else(|| SessionError::Config {
            message: format!("provider `{provider_id}` (check providers.toml)"),
        })?;
    let model = provider
        .model(model_id)
        .ok_or_else(|| SessionError::Config {
            message: format!("model `{model_id}` for provider `{provider_id}`"),
        })?;
    let api_key =
        config
            .resolve_api_key(provider_id, auth)
            .ok_or_else(|| SessionError::ModelBuild {
                provider: provider_id.to_string(),
                model: model_id.to_string(),
                message: format!(
                    "no API key for provider `{provider_id}`: set one in auth.toml \
                 ([providers.{provider_id}] api_key = ...) or point the provider's \
                 api_key_env at an environment variable (a placeholder key is fine \
                 for local servers)"
                ),
            })?;
    let handle = match provider.api {
        WireApi::AnthropicMessages => ModelHandle::named(
            format!("{provider_id}/{model_id}"),
            anthropic_client(provider, &api_key)?.completion_model(&model.id),
        ),
        WireApi::OpenaiResponses => ModelHandle::named(
            format!("{provider_id}/{model_id}"),
            openai_client(provider, &api_key)?.completion_model(&model.id),
        ),
        WireApi::OpenaiCompletions => ModelHandle::named(
            format!("{provider_id}/{model_id}"),
            openai::CompletionsClient::builder()
                .base_url(provider.base_url.clone())
                .api_key(api_key.clone())
                .build()
                .map_err(|source| build_error(provider_id, model_id, source))?
                .completion_model(&model.id),
        ),
    };
    Ok(handle)
}

/// A `(provider, model)` pair the session can switch between.
#[derive(Debug, Clone, PartialEq, Eq)]
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

fn anthropic_client(provider: &Provider, api_key: &str) -> Result<anthropic::Client, SessionError> {
    anthropic::Client::builder()
        .base_url(provider.base_url.clone())
        .api_key(api_key)
        .build()
        .map_err(|source| build_error(&provider.base_url, "", source))
}

fn openai_client(provider: &Provider, api_key: &str) -> Result<openai::Client, SessionError> {
    openai::Client::builder()
        .base_url(provider.base_url.clone())
        .api_key(api_key)
        .build()
        .map_err(|source| build_error(&provider.base_url, "", source))
}

fn build_error(provider_id: &str, model_id: &str, source: impl std::error::Error) -> SessionError {
    SessionError::ModelBuild {
        provider: provider_id.to_string(),
        model: model_id.to_string(),
        message: source.to_string(),
    }
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
