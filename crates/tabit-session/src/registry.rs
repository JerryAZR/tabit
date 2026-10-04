//! The model registry: the single construction site for models the
//! session layer uses (ROADMAP item 2).
//!
//! It owns the loaded config and auth, caches one HTTP client per
//! provider so switching models mid-session reuses the provider's
//! connection pool, and resolves the default model selection with the
//! precedence: an explicit caller choice, then the resumed session's
//! last model, then the configured `default_model` preference, then the
//! first configured model — and, when nothing anywhere is usable, no
//! selection at all (the first-run ruling reversal, 2026-10: the
//! zero-config boot opens selection-less with a teaching note; it is
//! never a startup error).
//!
//! The login/logout world refresh (protocol v21) mints a fresh
//! registry over the same config with the new auth and swaps the
//! host's current-world cell ([`CurrentWorld`]); config reload
//! (re-reading the providers.toml layers) reuses that path when it
//! lands. Dynamic model listing from endpoints stays deferred until
//! a consumer exists.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::lock::lock;
use crate::model::validate_selection;
use tabit_config::{AuthConfig, InputModality, Provider, TabitConfig, WireApi};
use tabit_engine::agent::ModelHandle;
use tabit_providers::client::CompletionClient;
use tabit_providers::providers::{anthropic, openai};

use crate::SessionError;
use crate::session::ModelFactory;
use tabit_protocol::{AvailableModel, AvailableProvider, MissingKeyProvider, ModelSelection};

/// The host's current-world cell: the ONE registry every session
/// builder reads AT CALL TIME (the create/open closures capture the
/// cell, never a registry) and the login/logout handler swaps whole
/// — mint a fresh registry over the same config with the new auth,
/// swap, re-fold, re-announce. Lock via [`crate::lock::lock`]; no
/// guard crosses an await.
pub type CurrentWorld = Arc<Mutex<ModelRegistry>>;

/// Mint the current-world cell over the boot registry.
pub fn current_world(registry: ModelRegistry) -> CurrentWorld {
    Arc::new(Mutex::new(registry))
}

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

/// Resolve the `default_model` preference into a selection. Every
/// failure is a message (the caller warns and falls back), never a
/// hard error.
fn preferred_selection(
    default: &tabit_config::DefaultModel,
    config: &TabitConfig,
) -> Result<ModelSelection, String> {
    let (provider, model) = match &default.provider {
        Some(provider) => (provider.clone(), default.model.clone()),
        None => config.resolve_model_ref(&default.model)?,
    };
    let selection = ModelSelection {
        provider,
        model,
        thinking_level: default.thinking_level.clone(),
    };
    validate_selection(&selection, config).map_err(|error| error.to_string())?;
    Ok(selection)
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
        Arc::new(move |provider, model, cache_key| registry.build(provider, model, cache_key))
    }

    /// Resolve the default selection for a new outer loop.
    ///
    /// Precedence: `explicit` (a caller-provided choice, e.g. `--model`),
    /// then `resumed` (the session log's last model), then the configured
    /// `default_model`, then the first **usable** model. A `resumed`
    /// reference that no longer resolves — gone from config, or its
    /// provider lacking the key material it needs — degrades with a note
    /// (it is a preference, like `default_model`); only an explicit
    /// selection fails loudly. The terminal arm **degrades instead of
    /// erroring** (the first-run ruling reversal, 2026-10): nothing
    /// usable means `Ok((None, notes))` with a teaching note — a fresh
    /// install is normal, the session boots selection-less, and the
    /// run-open failure is the carrier. The notes are data — the
    /// session worker surfaces them to the frontend as
    /// `error { kind: model }` frames (stderr printing at construction
    /// is ruled out: events are the only thing a frontend can see).
    pub fn default_selection(
        &self,
        explicit: Option<ModelSelection>,
        resumed: Option<ModelSelection>,
    ) -> Result<(Option<ModelSelection>, Vec<String>), SessionError> {
        let mut notes = Vec::new();
        if let Some(explicit) = explicit {
            validate_selection(&explicit, &self.inner.config)?;
            return Ok((Some(explicit), notes));
        }
        // A resumed selection is a preference too (owner ruling, pi
        // precedent): the session's last model may be gone from
        // config — degrade with a note instead of blocking. Explicit
        // selections (the arm above) stay loud; the user asked for
        // exactly that model.
        if let Some(resumed) = resumed {
            match self.preference_error(&resumed) {
                None => return Ok((Some(resumed), notes)),
                Some(error) => notes.push(format!(
                    "the resumed session's model `{}/{}` is not usable ({}); \
                     the default selection resolves without it",
                    resumed.provider, resumed.model, error
                )),
            }
        }
        // `default_model` is a preference, not a hard reference: a
        // stale or ambiguous entry degrades to the first usable
        // model with a note (owner ruling — it must never block
        // startup).
        if let Some(default) = &self.inner.config.default_model {
            match preferred_selection(default, &self.inner.config) {
                Ok(selection) if self.usable(&selection.provider) => {
                    return Ok((Some(selection), notes));
                }
                Ok(_) => notes.push(format!(
                    "default_model `{}` is not usable (its provider has no key and \
                     is not declared keyless); the default selection falls through it",
                    default.model
                )),
                Err(message) => notes.push(format!(
                    "default_model `{}` is not usable ({message}); the default selection \
                     falls through it",
                    default.model
                )),
            }
        }
        match self.first_usable_model() {
            Some((provider, model)) => Ok((Some(ModelSelection::new(provider, model)), notes)),
            // The terminal arm degrades (the ruling reversal): teach,
            // never scare — point at the fix, name the in-app path.
            None => {
                notes.push(
                    "no usable model at this backend — every configured provider lacks a key \
                     (declare local servers `keyless = true`, or add one via the `login` command \
                     or auth.toml / `api_key_env`), or there is no providers.toml at all, which \
                     is the normal fresh-install state. Create ~/.tabit/providers.toml (a `login` \
                     writes ~/.tabit/auth.toml and refreshes the world — no restart); the session \
                     runs selection-less until then, and a `model` command can name any \
                     configured ref at any time"
                        .to_string(),
                );
                Ok((None, notes))
            }
        }
    }

    /// Is this provider runnable — does it have the key material it
    /// needs? A provider with neither a key nor the `keyless`
    /// declaration is not a usable model provider.
    fn usable(&self, provider_id: &str) -> bool {
        let Some(provider) = self.inner.config.provider(provider_id) else {
            return false;
        };
        provider.keyless
            || self
                .inner
                .config
                .resolve_api_key(provider_id, &self.inner.auth)
                .is_some()
    }

    /// Why a preference (resumed selection) cannot run, if it cannot.
    fn preference_error(&self, selection: &ModelSelection) -> Option<String> {
        match validate_selection(selection, &self.inner.config) {
            Err(error) => Some(error.to_string()),
            Ok(()) if self.usable(&selection.provider) => None,
            Ok(()) => Some(format!(
                "provider `{}` has no key and is not declared keyless",
                selection.provider
            )),
        }
    }

    /// The boot catalog for the wire's `models_available`
    /// announcement (protocol v21): every USABLE provider — the same
    /// `usable` predicate `default_selection`'s rungs walk, never a
    /// sibling — with its models, folded into the protocol's wire
    /// types. The fold lives here because the registry owns config +
    /// auth; the endpoint just emits. Providers walk in the config
    /// map's alphabetical order (the same walk `first_usable_model`
    /// uses), models in config-file order; display sorting is the
    /// frontend's business.
    pub fn available_catalog(&self) -> Vec<AvailableProvider> {
        self.inner
            .config
            .providers
            .iter()
            .filter(|(id, _)| self.usable(id))
            .map(|(id, provider)| AvailableProvider {
                id: id.clone(),
                name: provider.name.clone(),
                models: provider
                    .models
                    .iter()
                    .map(|model| AvailableModel {
                        id: model.id.clone(),
                        name: model.name.clone(),
                        context_window: model.context_window,
                        max_tokens: model.max_tokens,
                        cost: model.cost.map(crate::model::wire_cost),
                        reasoning: model.reasoning,
                        input: model
                            .input
                            .iter()
                            .map(|modality| {
                                match modality {
                                    InputModality::Text => "text",
                                    InputModality::Image => "image",
                                }
                                .to_string()
                            })
                            .collect(),
                        // Names only — the dial's request-merge maps
                        // never cross the wire.
                        thinking_levels: model
                            .thinking_levels
                            .iter()
                            .map(|level| level.name.clone())
                            .collect(),
                    })
                    .collect(),
            })
            .collect()
    }

    /// The login widget's targets (`models_available.missing_keys`,
    /// v21 amended): the configured providers FAILING the `usable`
    /// predicate — no resolvable key and no `keyless = true`
    /// declaration — as identity-only entries. The complement of
    /// [`Self::available_catalog`]'s fold over the same predicate;
    /// alphabetical id order, like the catalog.
    pub fn missing_keys(&self) -> Vec<MissingKeyProvider> {
        self.inner
            .config
            .providers
            .iter()
            .filter(|(id, _)| !self.usable(id))
            .map(|(id, provider)| MissingKeyProvider {
                id: id.clone(),
                name: provider.name.clone(),
            })
            .collect()
    }

    /// The last-resort pick: the first usable provider's first model
    /// (alphabetical provider order), skipping providers that lack the
    /// key material they need.
    fn first_usable_model(&self) -> Option<(String, String)> {
        for (provider_id, provider) in &self.inner.config.providers {
            if !self.usable(provider_id) {
                continue;
            }
            if let Some(model) = provider.models.first() {
                return Some((provider_id.clone(), model.id.clone()));
            }
        }
        None
    }

    /// Build a model handle for `(provider, model)` through the cached
    /// provider client. `cache_key` is the session's stable id — the
    /// prompt-cache routing hint where the wire API has one.
    fn build(
        &self,
        provider_id: &str,
        model_id: &str,
        cache_key: &str,
    ) -> Result<ModelHandle, SessionError> {
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
        // Keyless is a supported state (ROADMAP item 1: local endpoints
        // run keyless) — but only when DECLARED. A provider with
        // neither a key nor `keyless = true` is not a usable model
        // provider (owner ruling 2026-09): the selection fails here,
        // loudly, naming both fixes, instead of surfacing a bare 401
        // at request time. Declared keyless, the stubbed empty
        // credential rides the same builders as everyone else.
        let api_key = match self
            .inner
            .config
            .resolve_api_key(provider_id, &self.inner.auth)
        {
            Some(key) => key,
            None if provider.keyless => String::new(),
            None => {
                return Err(SessionError::Config {
                    message: format!(
                        "provider `{provider_id}` requires a key — add one to \
                         auth.toml, set `api_key_env`, or declare local servers \
                         `keyless = true`"
                    ),
                });
            }
        };
        let label = format!("{provider_id}/{}", model.id);
        // Tabit's prompt-cache policy, in full (owner ruling 2026-08: keep
        // it simple, all 1h; a modeled policy — breakpoint cadence, mixed
        // TTLs — lands here as a contained edit).
        //
        // Anthropic: automatic caching with the 1h TTL. The API owns
        // breakpoint placement; we pay the 2x write premium so entries
        // survive interactive gaps and >5m tool turns (5m entries lapse;
        // hits refresh free and read at 0.1x under either TTL). Sent on
        // the wire protocol as-is — an anthropic-compatible server that
        // rejects the field fails loudly at the first request, per the
        // external-error doctrine.
        //
        // OpenAI Responses: caching itself is automatic; we only pin
        // routing with the session's key so a conversation's requests
        // land on one cache shard (the codex/pi pattern). The chat
        // completions gateway sends no key — third parties vary in what
        // they accept.
        let handle = match self.client_for(provider_id, provider, &api_key)? {
            ProviderClient::Anthropic(client) => ModelHandle::named(
                label,
                client
                    .completion_model(&model.id)
                    .with_automatic_caching_1h(),
            ),
            ProviderClient::Responses(client) => ModelHandle::named(
                label,
                client.completion_model(&model.id).with_cache_key(cache_key),
            ),
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
                    .http_headers(configured_headers(provider_id, provider)?)
                    .build()
                    .map_err(|source| build_error(provider_id, source))?,
            ),
            WireApi::OpenaiResponses => ProviderClient::Responses(
                openai::Client::builder()
                    .base_url(provider.base_url.clone())
                    .api_key(api_key)
                    .http_headers(configured_headers(provider_id, provider)?)
                    .build()
                    .map_err(|source| build_error(provider_id, source))?,
            ),
            WireApi::OpenaiCompletions => ProviderClient::Completions(
                openai::CompletionsClient::builder()
                    .base_url(provider.base_url.clone())
                    .api_key(api_key)
                    .http_headers(configured_headers(provider_id, provider)?)
                    .build()
                    .map_err(|source| build_error(provider_id, source))?,
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

/// The request parameters a session forwards for a selection: the model's
/// pass-through knobs, with the active thinking level's `extra_body`
/// overlaid last (level over model over provider).
///
/// Pure forwarding — nothing here interprets the values. Unknown ids
/// contribute nothing (the model factory is the loud check for those).
pub(crate) fn request_params(
    config: &TabitConfig,
    selection: &ModelSelection,
) -> ModelRequestParams {
    let Some(provider) = config.provider(&selection.provider) else {
        return ModelRequestParams::default();
    };
    let Some(model) = provider.model(&selection.model) else {
        return ModelRequestParams::default();
    };
    let level = selection
        .thinking_level
        .as_deref()
        .and_then(|name| model.thinking_level(name));
    ModelRequestParams {
        max_tokens: model.max_tokens,
        temperature: model.sampling_params.and_then(|params| params.temperature),
        top_p: model.sampling_params.and_then(|params| params.top_p),
        top_k: model.sampling_params.and_then(|params| params.top_k),
        extra_body: model.merged_extra_body(provider.extra_body.as_ref(), level),
    }
}

/// The request parameters resolved for one selection. Carried separately
/// from the model handle so the factory (tests, custom constructors) stays
/// untouched.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ModelRequestParams {
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u64>,
    pub extra_body: Option<serde_json::Map<String, serde_json::Value>>,
}

/// The provider's configured headers as an HTTP header map. Provider
/// protocol headers (e.g. `anthropic-version`) are inserted at client build
/// time and coexist with these.
fn configured_headers(
    provider_id: &str,
    provider: &Provider,
) -> Result<http::HeaderMap, SessionError> {
    let mut headers = http::HeaderMap::new();
    let Some(configured) = &provider.headers else {
        return Ok(headers);
    };
    for (name, value) in configured {
        let name = http::HeaderName::from_bytes(name.as_bytes()).map_err(|source| {
            SessionError::Config {
                message: format!(
                    "provider `{provider_id}` header name `{name}` is not valid HTTP: {source}"
                ),
            }
        })?;
        let value = http::HeaderValue::from_str(value).map_err(|source| SessionError::Config {
            message: format!(
                "provider `{provider_id}` header `{name}` has a value that is not valid \
                     HTTP: {source}"
            ),
        })?;
        headers.insert(name, value);
    }
    Ok(headers)
}

/// Wrap a client-construction failure with the provider id.
fn build_error(provider_id: &str, source: impl std::error::Error) -> SessionError {
    SessionError::ClientBuild {
        provider: provider_id.to_string(),
        message: source.to_string(),
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
