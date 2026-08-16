//! The model registry: the single construction site for models the
//! session layer uses (ROADMAP item 2).
//!
//! It owns the loaded config and auth, caches one HTTP client per
//! provider so switching models mid-session reuses the provider's
//! connection pool, and resolves the default model selection with the
//! precedence: an explicit caller choice, then the resumed session's
//! last model, then the configured `default_model` preference, then the
//! first configured model.
//!
//! Reload (re-reading config for future resolutions) and dynamic model
//! listing from endpoints are deferred until a consumer exists.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::lock::lock;
use rig_agent::agent::ModelHandle;
use rig_core::client::CompletionClient;
use rig_core::providers::{anthropic, openai};
use tabit_config::{AuthConfig, Provider, TabitConfig, WireApi};

use crate::SessionError;
use crate::model::ModelSelection;
use crate::session::ModelFactory;

/// One constructed provider client. Clients clone cheaply and share
/// their HTTP connection pool; models built from them are thin wrappers,
/// so only the client is worth caching.
#[derive(Clone)]
enum ProviderClient {
    Anthropic(anthropic::Client),
    Responses(openai::Client),
    Completions(openai::CompletionsClient),
}

struct RegistryInner {
    config: Arc<TabitConfig>,
    auth: Arc<AuthConfig>,
    clients: Mutex<HashMap<String, ProviderClient>>,
}

/// The model registry. Cheap to clone: every copy shares the cached
/// clients.
#[derive(Clone)]
pub struct ModelRegistry {
    inner: Arc<RegistryInner>,
}

impl ModelRegistry {
    /// A registry over the loaded config and auth.
    pub fn new(config: Arc<TabitConfig>, auth: Arc<AuthConfig>) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                config,
                auth,
                clients: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The loaded config behind the registry.
    pub fn config(&self) -> &Arc<TabitConfig> {
        &self.inner.config
    }

    /// The loaded auth behind the registry.
    pub fn auth(&self) -> &Arc<AuthConfig> {
        &self.inner.auth
    }

    /// The session-layer model factory: every build goes through the
    /// registry, so model switches reuse the cached provider client.
    pub fn factory(&self) -> ModelFactory {
        let registry = self.clone();
        Arc::new(move |provider, model| registry.build(provider, model))
    }

    /// Resolve the default selection for a new outer loop.
    ///
    /// Precedence: `explicit` (a caller-provided choice, e.g. `--model`),
    /// then `resumed` (the session log's last model), then the configured
    /// `default_model`, then the first configured model. A `resumed`
    /// reference that no longer resolves is a loud error — the session
    /// was last used with a provider that is no longer configured.
    pub fn default_selection(
        &self,
        explicit: Option<ModelSelection>,
        resumed: Option<ModelSelection>,
    ) -> Result<ModelSelection, SessionError> {
        if let Some(explicit) = explicit {
            explicit.validate(&self.inner.config)?;
            return Ok(explicit);
        }
        if let Some(resumed) = resumed {
            resumed
                .validate(&self.inner.config)
                .map_err(|error| match error {
                    SessionError::Config { message } => SessionError::Config {
                        message: format!(
                            "{message} — the resumed session last used it; pass \
                             --model or restore the provider"
                        ),
                    },
                    other => other,
                })?;
            return Ok(resumed);
        }
        if let Some(default) = &self.inner.config.default_model {
            let (provider, model) = match &default.provider {
                Some(provider) => (provider.clone(), default.model.clone()),
                None => self
                    .inner
                    .config
                    .resolve_model_ref(&default.model)
                    .map_err(|message| SessionError::Config { message })?,
            };
            let selection = ModelSelection {
                provider,
                model,
                thinking_level: default.thinking_level.clone(),
            };
            selection.validate(&self.inner.config)?;
            return Ok(selection);
        }
        self.inner
            .config
            .first_model()
            .map(|(provider, model)| ModelSelection::new(provider, model))
            .ok_or_else(|| SessionError::Config {
                message: "any model to run with (providers.toml defines no models)".to_string(),
            })
    }

    /// Build a model handle for `(provider, model)` through the cached
    /// provider client.
    fn build(&self, provider_id: &str, model_id: &str) -> Result<ModelHandle, SessionError> {
        let provider =
            self.inner
                .config
                .provider(provider_id)
                .ok_or_else(|| SessionError::Config {
                    message: format!("provider `{provider_id}` (check providers.toml)"),
                })?;
        let model = provider
            .model(model_id)
            .ok_or_else(|| SessionError::Config {
                message: format!("model `{model_id}` for provider `{provider_id}`"),
            })?;
        let api_key = self
            .inner
            .config
            .resolve_api_key(provider_id, &self.inner.auth)
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
        let label = format!("{provider_id}/{}", model.id);
        let handle = match self.client_for(provider_id, provider, &api_key)? {
            ProviderClient::Anthropic(client) => {
                ModelHandle::named(label, client.completion_model(&model.id))
            }
            ProviderClient::Responses(client) => {
                ModelHandle::named(label, client.completion_model(&model.id))
            }
            ProviderClient::Completions(client) => {
                ModelHandle::named(label, client.completion_model(&model.id))
            }
        };
        Ok(handle)
    }

    /// The cached client for a provider, constructed on first use. A
    /// poisoned lock carries intact data (no code panics while holding
    /// it), so recovering beats failing.
    fn client_for(
        &self,
        provider_id: &str,
        provider: &Provider,
        api_key: &str,
    ) -> Result<ProviderClient, SessionError> {
        if let Some(client) = lock(&self.inner.clients).get(provider_id) {
            return Ok(client.clone());
        }
        let client = match provider.api {
            WireApi::AnthropicMessages => ProviderClient::Anthropic(
                anthropic::Client::builder()
                    .base_url(provider.base_url.clone())
                    .api_key(api_key)
                    .build()
                    .map_err(|source| build_error(provider_id, "", source))?,
            ),
            WireApi::OpenaiResponses => ProviderClient::Responses(
                openai::Client::builder()
                    .base_url(provider.base_url.clone())
                    .api_key(api_key)
                    .build()
                    .map_err(|source| build_error(provider_id, "", source))?,
            ),
            WireApi::OpenaiCompletions => ProviderClient::Completions(
                openai::CompletionsClient::builder()
                    .base_url(provider.base_url.clone())
                    .api_key(api_key)
                    .build()
                    .map_err(|source| build_error(provider_id, "", source))?,
            ),
        };
        Ok(lock(&self.inner.clients)
            .entry(provider_id.to_string())
            .or_insert(client)
            .clone())
    }

    /// How many provider clients are cached (diagnostics and tests).
    pub fn cached_provider_count(&self) -> usize {
        lock(&self.inner.clients).len()
    }
}

/// Lock, recovering from poisoning (see [`ModelRegistry::client_for`]).
fn build_error(provider_id: &str, model_id: &str, source: impl std::error::Error) -> SessionError {
    SessionError::ModelBuild {
        provider: provider_id.to_string(),
        model: model_id.to_string(),
        message: source.to_string(),
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
