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

/// One usable provider in the boot `models_available` catalog (v21):
/// its identity, its display name when configured, and its models.
/// Only runnable providers cross — a provider with no resolvable key
/// and no `keyless = true` declaration is dropped wholesale (the fold
/// lives in tabit-session's registry, the config+auth owner); there is
/// no `usable` flag on the wire. Providers arrive in alphabetical id
/// order, models in config-file order; display sorting is the
/// frontend's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvailableProvider {
    /// The provider's config key (the id `model` commands address).
    pub id: String,
    /// The provider's display name, when configured (frontends fall
    /// back to the id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The provider's models, in config-file order.
    pub models: Vec<AvailableModel>,
}

/// One configured provider in the `providers_available` catalog
/// (v22): identity plus the winning key source. Unlike
/// [`AvailableProvider`] this covers EVERY configured provider,
/// usable or not — the login/logout view's data (the models catalog
/// stays the picker's). Providers arrive in alphabetical id order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderStatus {
    /// The provider's config key (the id `login`/`logout` address).
    pub id: String,
    /// The provider's display name, when configured (frontends fall
    /// back to the id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The winning key source — the resolution order's outcome.
    pub auth: ProviderAuth,
}

/// Where a provider's key material comes from (v22) — the winning
/// source in resolution order: a stored auth.toml key beats the
/// `api_key_env` variable, which beats the declared `keyless = true`
/// fallback, which is all that remains before `None`. The order is
/// the law: a keyless provider WITH a stored key reports `Stored` —
/// the key genuinely rides requests (`keyless` is a fallback
/// declaration, not a prohibition; the explicit user act wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuth {
    /// auth.toml holds a key for the provider (the `login` command's
    /// write — the explicit user act).
    Stored,
    /// The provider's `api_key_env` environment variable is set.
    /// Display-only for a frontend: the app cannot unset a
    /// persistent environment variable, so no logout is offered.
    Env,
    /// No key from any source, but the provider declares
    /// `keyless = true` (a local server).
    Keyless,
    /// No key from any source and not keyless — the provider is not
    /// runnable; this is `login`'s target.
    None,
}

/// One model in [`AvailableProvider`]: the id a `model` command
/// addresses plus the facts config states — display name, capacity,
/// pricing, capability flags, and the thinking dial's ordered level
/// names. Every optional field follows the v11 rule: absent means the
/// config does not state it (never zero).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvailableModel {
    /// The model id within the provider.
    pub id: String,
    /// The model's display name, when configured (frontends fall back
    /// to the id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The context window in tokens, when config states one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// The maximum output tokens, when config states one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Per-million-token pricing, when config states it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    /// Whether the model produces reasoning output.
    pub reasoning: bool,
    /// The input modalities the model accepts, as lowercase strings
    /// (`"text"`, `"image"`).
    pub input: Vec<String>,
    /// The thinking dial's ordered level NAMES (never the levels'
    /// request-merge maps); empty when the model has no dial.
    /// `thinking_level: null` is always a legal selection on top —
    /// the provider/model default — so a picker cycles null → these.
    pub thinking_levels: Vec<String>,
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
