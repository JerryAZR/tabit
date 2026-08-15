//! Provider-level configuration: endpoint, auth, wire protocol, and models.

use crate::model::Model;
use crate::wire::WireApi;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// A provider entry: everything needed to construct a wire client for it.
///
/// Auth is intentionally two-sourced: `api_key` (inline, convenient for local
/// endpoints like LM Studio where the key is a placeholder) and `api_key_env`
/// (the *name* of an environment variable, for real secrets). When both are
/// set the inline key wins, as the more specific setting.
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
    /// Inline API key. Wins over `api_key_env` when both are set.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Name of the environment variable holding the API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Extra headers sent with every request to this provider.
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    /// Fields merged into the top level of every request body to this
    /// provider (overridden by model- and thinking-level `extra_body`).
    /// This is tabit's compat escape hatch: provider quirks are expressed
    /// as plain request fields, never as named compat flags.
    #[serde(default)]
    pub extra_body: Option<Map<String, Value>>,
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

    /// Resolve the API key for this provider: the inline `api_key` wins;
    /// otherwise the environment variable named by `api_key_env` is read.
    ///
    /// Returns `Ok(None)` when neither source is configured or the
    /// environment variable is unset — local endpoints legitimately need no
    /// key, so the decision to require one belongs to the consumer, which
    /// can fail loudly with its own context.
    pub fn resolve_api_key(&self) -> Option<String> {
        if let Some(key) = &self.api_key {
            return Some(key.clone());
        }
        self.api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
    }
}
