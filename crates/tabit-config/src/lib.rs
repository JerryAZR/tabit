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
//! # Example
//!
//! ```toml
//! [providers.lmstudio]
//! name = "LM Studio"
//! base_url = "http://127.0.0.1:1234/v1"
//! api = "openai-completions"
//! api_key = "lm-studio"
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
//! debugging/local override), then `<home>/.tabit/tabit.toml` (the single
//! canonical location).
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

mod error;
mod model;
mod provider;
mod wire;

pub use error::ConfigError;
pub use model::{Cost, InputModality, Model, SamplingParams, ThinkingLevel};
pub use provider::Provider;
pub use wire::WireApi;

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A parsed and validated tabit configuration.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabitConfig {
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
    /// `<home>/.tabit/tabit.toml`. `$TABIT_CONFIG` is the debugging/local
    /// override — point it at a scratch config instead of touching the real
    /// one. (A future CLI flag will outrank the env var; more specific
    /// scopes win.) Fails with [`ConfigError::NotFound`] listing every
    /// candidate when none exists.
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

    /// All semantic validation issues, each prefixed with its config key
    /// path. Empty means valid.
    pub fn validation_issues(&self) -> Vec<String> {
        let mut issues = Vec::new();
        for (provider_id, provider) in &self.providers {
            validate_provider(provider_id, provider, &mut issues);
        }
        issues
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

    if provider.api_key.as_deref() == Some("") {
        issues.push(format!("{provider_key}.api_key: must not be empty"));
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
/// `<home>/.tabit/tabit.toml`.
fn default_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(from_env) = std::env::var_os("TABIT_CONFIG") {
        paths.push(PathBuf::from(from_env));
    }
    if let Some(home) = home_dir() {
        paths.push(home.join(".tabit").join("tabit.toml"));
    }
    paths
}

/// The user's home directory (`$USERPROFILE` on Windows, `$HOME` elsewhere).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests;
