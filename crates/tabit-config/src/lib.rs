#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used,
        clippy::unreachable
    )
)]
//! Tabit's provider/model configuration.
//!
//! This crate answers two questions and nothing else:
//!
//! 1. **What are the providers and how do I talk to them?** —
//!    [`Provider`]: endpoint (`base_url`), wire protocol (`api`), auth
//!    (`api_key` inline or `api_key_env`), extra headers, and a shared
//!    `extra_body` merge.
//! 2. **What models does each provider have and what can they do?** —
//!    [`Model`]: id, display name, input modalities, reasoning flag,
//!    context/output limits, pricing, default sampling params, and an
//!    ordered list of [`ThinkingLevel`]s a UI can cycle through.
//!
//! There is no built-in model catalog and no compat-flag taxonomy: every
//! provider quirk is expressed as plain request fields through `extra_body`,
//! and every capability value is supplied by the user's config. Parsing and
//! validation fail loudly, naming the exact file and setting at fault, and
//! validation reports *all* issues in one pass.
//!
//! Provider config is secret-free by construction: keys live in a separate
//! [`AuthConfig`] file (`~/.tabit/auth.toml`) or in environment variables
//! named by `api_key_env`, so `providers.toml` is safe to display, share,
//! and edit with agent assistance.
//!
//! # Example
//!
//! ```toml
//! [providers.lmstudio]
//! name = "LM Studio"
//! base_url = "http://127.0.0.1:1234/v1"
//! api = "openai-completions"
//!
//! [[providers.lmstudio.models]]
//! id = "openai/gpt-oss-20b"
//! reasoning = true
//! input = ["text", "image"]
//! context_window = 131_072
//! max_tokens = 65_536
//!
//! [providers.deepseek]
//! base_url = "https://api.deepseek.com"
//! api = "openai-completions"
//! api_key_env = "DEEPSEEK_API_KEY"
//! extra_body = { requires = "nothing" }  # top-level request-body merge
//!
//! [[providers.deepseek.models]]
//! id = "deepseek-chat"
//! context_window = 131_072
//!
//! [[providers.deepseek.models.thinking_levels]]
//! name = "off"
//! extra_body = { thinking = { type = "disabled" } }
//!
//! [[providers.deepseek.models.thinking_levels]]
//! name = "high"
//! extra_body = { thinking = { type = "enabled" } }
//! ```
//!
//! Loading uses [`TabitConfig::load`] with an explicit path, or
//! [`TabitConfig::load_default`], which checks `$TABIT_CONFIG` (the
//! debugging/local override), then `<home>/.tabit/providers.toml` (the
//! single canonical location). Keys are resolved separately through
//! [`AuthConfig::load_default`] (`$TABIT_AUTH`, then
//! `<home>/.tabit/auth.toml`; a missing auth file is not an error).
//!
//! ```
//! use tabit_config::TabitConfig;
//!
//! let raw = r#"
//! [providers.lmstudio]
//! base_url = "http://127.0.0.1:1234/v1"
//! api = "openai-completions"
//! "#;
//! let config = TabitConfig::from_toml_str(raw, "tabit.toml".as_ref())?;
//! assert!(config.provider("lmstudio").is_some());
//! # Ok::<(), tabit_config::ConfigError>(())
//! ```
//!
//! [`Provider`]: crate::Provider
//! [`Model`]: crate::Model
//! [`ThinkingLevel`]: crate::ThinkingLevel

mod auth;
mod error;
mod model;
mod provider;
mod wire;

pub use auth::{AuthConfig, AuthEntry};
pub use error::ConfigError;
pub use model::{Cost, InputModality, Model, SamplingParams, ThinkingLevel};
pub use provider::Provider;
pub use wire::WireApi;

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// The model a session uses when the caller does not pick one.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultModel {
    /// Provider id. Optional: when absent, `model` must resolve to exactly
    /// one provider's model — qualify with the provider only when the same
    /// model id is configured under more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model id within the provider (may itself contain `/`, as in
    /// `openai/gpt-oss-20b`).
    pub model: String,
    /// Thinking level name, when the model defines levels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

/// A parsed and validated tabit configuration.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabitConfig {
    /// The model a session uses when the caller does not pick one.
    #[serde(default)]
    pub default_model: Option<DefaultModel>,
    /// All configured providers, keyed by their config id.
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
}

impl TabitConfig {
    /// Parse and validate a config from a TOML string, attributing any
    /// error to `path`.
    pub fn from_toml_str(raw: &str, path: &Path) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        let issues = config.validation_issues();
        if issues.is_empty() {
            Ok(config)
        } else {
            Err(ConfigError::Validation {
                path: path.to_path_buf(),
                issues,
            })
        }
    }

    /// Load and validate a config file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&raw, path)
    }

    /// Load the default config: the file named by `$TABIT_CONFIG`, else
    /// `<home>/.tabit/providers.toml`. `$TABIT_CONFIG` is the
    /// debugging/local override — point it at a scratch config instead of
    /// touching the real one. (A future CLI flag will outrank the env var;
    /// more specific scopes win.) Fails with [`ConfigError::NotFound`]
    /// listing every candidate when none exists.
    pub fn load_default() -> Result<Self, ConfigError> {
        let candidates = default_config_paths();
        for path in &candidates {
            if path.is_file() {
                return Self::load(path);
            }
        }
        Err(ConfigError::NotFound { paths: candidates })
    }

    /// Look up a provider by its config id.
    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.get(id)
    }

    /// Look up a provider/model pair as `("<provider>", "<model>")`.
    pub fn model(&self, provider_id: &str, model_id: &str) -> Option<(&Provider, &Model)> {
        self.provider(provider_id)
            .and_then(|provider| provider.model(model_id).map(|model| (provider, model)))
    }

    /// Resolve a provider's API key: an `auth.toml` entry for it wins, else
    /// the environment variable named by its `api_key_env`. See
    /// [`Provider::resolve_api_key`].
    pub fn resolve_api_key(&self, provider_id: &str, auth: &AuthConfig) -> Option<String> {
        self.provider(provider_id)?
            .resolve_api_key(provider_id, auth)
    }

    /// All semantic validation issues, each prefixed with its config key
    /// path. Empty means valid.
    pub fn validation_issues(&self) -> Vec<String> {
        let mut issues = Vec::new();
        if let Some(default) = &self.default_model {
            match &default.provider {
                Some(provider) => validate_model_reference(
                    "default_model",
                    provider,
                    &default.model,
                    default.thinking_level.as_deref(),
                    self,
                    &mut issues,
                ),
                None => match self.resolve_model_ref(&default.model) {
                    Ok((provider, _)) => validate_model_reference(
                        "default_model",
                        &provider,
                        &default.model,
                        default.thinking_level.as_deref(),
                        self,
                        &mut issues,
                    ),
                    Err(message) => {
                        issues.push(format!("default_model.model: {message}"));
                    }
                },
            }
        }
        for (provider_id, provider) in &self.providers {
            validate_provider(provider_id, provider, &mut issues);
        }
        issues
    }

    /// Resolve a model reference to `(provider id, model id)`.
    ///
    /// Resolution is one exact-match lookup over an index that registers
    /// every model under two keys: its bare model id and its qualified
    /// `provider/model` id (model ids may themselves contain `/`, as in
    /// `openai/gpt-oss-20b` under a `lmstudio` provider). A key matching
    /// exactly one model resolves. A key matching several — the same id
    /// under multiple providers, or a bare id colliding with another
    /// provider's qualified id — is an ambiguity error listing the
    /// candidates, so the reference must be qualified. An unknown key is
    /// an error naming the reference.
    pub fn resolve_model_ref(&self, reference: &str) -> Result<(String, String), String> {
        let index = self.model_index();
        let matches = match index.get(reference) {
            Some(matches) => matches,
            None => return Err(format!("no configured model matches `{reference}`")),
        };
        if let Some((provider, model)) = matches.first()
            && matches.len() == 1
        {
            return Ok((provider.clone(), model.clone()));
        }
        Err(format!(
            "model reference `{reference}` is ambiguous (matches: {}); \
             qualify as `provider/model`",
            matches
                .iter()
                .map(|(provider, model)| format!("{provider}/{model}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    /// The resolution index: every model registered under both its bare
    /// id and its qualified `provider/model` key. Registration order
    /// (alphabetical providers, file-order models) keeps candidate lists
    /// in the config's own order.
    fn model_index(&self) -> HashMap<String, Vec<(String, String)>> {
        let mut index: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for (provider_id, provider) in &self.providers {
            for model in &provider.models {
                let entry = (provider_id.clone(), model.id.clone());
                index
                    .entry(model.id.clone())
                    .or_default()
                    .push(entry.clone());
                index
                    .entry(format!("{provider_id}/{}", model.id))
                    .or_default()
                    .push(entry);
            }
        }
        index
    }

    /// The first model the loader sees: the alphabetically-first provider's
    /// first model (`providers` is a `BTreeMap`; model arrays keep file
    /// order). The fallback default when no preference exists.
    pub fn first_model(&self) -> Option<(String, String)> {
        let (provider_id, provider) = self.providers.iter().next()?;
        let model = provider.models.first()?;
        Some((provider_id.clone(), model.id.clone()))
    }
}

/// Validate a `provider`/`model`(/`thinking_level`) reference against the
/// configured providers; shared by `default_model` validation.
fn validate_model_reference(
    key: &str,
    provider_id: &str,
    model_id: &str,
    thinking_level: Option<&str>,
    config: &TabitConfig,
    issues: &mut Vec<String>,
) {
    let Some(provider) = config.providers.get(provider_id) else {
        issues.push(format!(
            "{key}.provider: `{provider_id}` is not defined under [providers]"
        ));
        return;
    };
    let Some(model) = provider.models.iter().find(|m| m.id == model_id) else {
        issues.push(format!(
            "{key}.model: `{model_id}` is not defined for provider `{provider_id}`"
        ));
        return;
    };
    if let Some(level) = thinking_level
        && model.thinking_level(level).is_none()
    {
        issues.push(format!(
            "{key}.thinking_level: `{level}` is not one of the levels defined for              provider `{provider_id}` model `{model_id}`"
        ));
    }
}

fn validate_provider(provider_id: &str, provider: &Provider, issues: &mut Vec<String>) {
    let provider_key = format!("providers.{provider_id}");

    match url::Url::parse(&provider.base_url) {
        Ok(url) => {
            if !matches!(url.scheme(), "http" | "https") {
                issues.push(format!(
                    "{provider_key}.base_url: scheme must be http or https, got `{}`",
                    url.scheme()
                ));
            }
        }
        Err(source) => issues.push(format!(
            "{provider_key}.base_url: not a valid URL (`{}`): {source}",
            provider.base_url
        )),
    }

    if provider.api_key_env.as_deref() == Some("") {
        issues.push(format!("{provider_key}.api_key_env: must not be empty"));
    }

    let mut seen_model_ids = std::collections::BTreeSet::new();
    for model in &provider.models {
        let model_key = format!("{provider_key}.models[{}]", model.id);
        if model.id.is_empty() {
            issues.push(format!("{model_key}: id must not be empty"));
        }
        if !seen_model_ids.insert(model.id.clone()) {
            issues.push(format!("{model_key}: duplicate model id `{}`", model.id));
        }

        let mut seen_level_names = std::collections::BTreeSet::new();
        for level in &model.thinking_levels {
            if level.name.is_empty() {
                issues.push(format!(
                    "{model_key}: thinking level name must not be empty"
                ));
            }
            if !seen_level_names.insert(level.name.clone()) {
                issues.push(format!(
                    "{model_key}: duplicate thinking level name `{}`",
                    level.name
                ));
            }
        }
    }
}

/// The default config search path: `$TABIT_CONFIG`, then
/// `<home>/.tabit/providers.toml`.
fn default_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(from_env) = std::env::var_os("TABIT_CONFIG") {
        paths.push(PathBuf::from(from_env));
    }
    if let Some(home) = home_dir() {
        paths.push(home.join(".tabit").join("providers.toml"));
    }
    paths
}

/// The user's home directory (`$USERPROFILE` on Windows, `$HOME` elsewhere).
///
/// Shared by every home-relative tabit location (config files, the
/// home-level AGENTS.md), so there is exactly one home-resolution rule.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests;
