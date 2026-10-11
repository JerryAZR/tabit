//! Provider-level configuration: endpoint, auth wiring, wire protocol, and
//! models.

use crate::auth::AuthConfig;
use crate::model::Model;
use crate::wire::WireApi;
use serde::Deserialize;
use std::collections::BTreeMap;

/// A provider entry: everything needed to construct a wire client for it.
///
/// Key material is deliberately absent — this file is safe to display,
/// share, and edit with agent assistance. Auth comes from [`AuthConfig`]
/// (`~/.tabit/auth.toml`) or from the environment variable named by
/// `api_key_env`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// Display name; defaults to the provider's config key.
    pub name: Option<String>,
    /// The provider's API root (e.g. `https://api.anthropic.com`, or
    /// `http://127.0.0.1:1234/v1` for an OpenAI-format local server).
    /// Required — there is no default endpoint.
    pub base_url: String,
    /// The wire protocol this provider speaks.
    pub api: WireApi,
    /// Name of the environment variable holding the API key. Loses to an
    /// `auth.toml` entry for the same provider.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// This provider needs no API key — a local server (LM Studio,
    /// llama.cpp, an OpenAI-compatible relay). A provider with neither
    /// a key nor this declaration is **not a usable model provider**:
    /// selection fails loudly instead of surfacing a bare 401 at
    /// request time. Declared keyless, requests run with a stubbed
    /// empty credential through the same builders as everyone else.
    #[serde(default)]
    pub keyless: bool,
    /// Extra headers sent with every request to this provider.
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    /// Provider-level defaults for every model entry: any `Model`
    /// field, inherited field-by-field (a model's own fields win).
    /// Merged into each model at load time (the value-level
    /// normalization in `TabitConfig::from_toml_str`, which validates
    /// the table against the `Model` shape) — the field is carried
    /// for provenance but read by nothing after the merge.
    #[serde(default)]
    pub model_defaults: Option<toml::Table>,
    /// Engine quirk values for this provider (PROVIDERS.md's seam):
    /// a raw table, parsed into the engine's typed `Quirks` struct at
    /// client construction — an unknown key is a loud config error
    /// there. The initial vocabulary is empty, so any key errors.
    #[serde(default)]
    pub quirks: Option<toml::Table>,
    /// The models this provider offers.
    #[serde(default)]
    pub models: Vec<Model>,
}

impl Provider {
    /// The provider's display name, falling back to its config key.
    pub fn display_name<'a>(&'a self, id: &'a str) -> &'a str {
        self.name.as_deref().unwrap_or(id)
    }

    /// Look up a model by id.
    pub fn model(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|model| model.id == id)
    }

    /// Resolve the API key for this provider: an `auth.toml` entry wins;
    /// otherwise the environment variable named by `api_key_env` is read.
    ///
    /// Returns `None` when neither source is configured or the environment
    /// variable is unset — local endpoints legitimately need no key, so the
    /// decision to require one belongs to the consumer, which can fail
    /// loudly with its own context.
    pub fn resolve_api_key(&self, id: &str, auth: &AuthConfig) -> Option<String> {
        if let Some(key) = auth.api_key(id) {
            return Some(key.to_string());
        }
        self.api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
    }
}
