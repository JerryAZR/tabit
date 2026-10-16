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
//!    (`api_key` inline or `api_key_env`), extra headers.
//! 2. **What models does each provider have and what can they do?** —
//!    [`Model`]: id, display name, input modalities, reasoning flag,
//!    context/output limits, pricing, default sampling params, and an
//!    ordered list of thinking-level **names** (plain strings; each
//!    dialect engine realizes them natively — PROVIDERS.md) a UI can
//!    cycle through.
//!
//! There is no built-in model catalog in this crate and no compat-flag
//! taxonomy: every capability value is supplied by config (the user's,
//! an extension fragment's, or the downloaded catalog's — `merge_catalog`
//! folds it in at boot), parsing and validation fail loudly, naming the
//! exact file and setting at fault, and validation reports *all* issues
//! in one pass. The one seam for observed provider deviations is
//! `quirks` (PROVIDERS.md) — empty today, loud on any key.
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
//! keyless = true  # a local server: usable with a stubbed credential
//!
//! [[providers.lmstudio.models]]
//! id = "openai/gpt-oss-20b"
//! reasoning = true
//! input = ["text", "image"]
//! context_window = 131_072
//! max_tokens = 65_536
//!
//! [[providers.deepseek.models]]
//! id = "deepseek-chat"
//! context_window = 131_072
//! thinking_levels = ["low", "high"]  # plain names; the dialect
//!                                    # engines realize them natively
//! ```
//!
//! Loading uses [`TabitConfig::load`] with an explicit path, or
//! [`TabitConfig::load_default`], which checks `$TABIT_CONFIG` (the
//! debugging/local override), then `<home>/.tabit/providers.toml` (the
//! single canonical location; a missing file there is the empty
//! config, not an error — the zero-config boot). Keys are resolved
//! separately through
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
mod settings;
mod wire;

pub use auth::{AuthConfig, AuthEntry, default_path as auth_default_path};
pub use error::ConfigError;
pub use model::{Cost, InputModality, Model, SamplingParams};
pub use provider::Provider;
pub use settings::{CatalogSettings, ExtensionsSettings, SettingsConfig};
pub use wire::WireApi;

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// The model a session uses when the caller does not pick one.
///
/// A **preference, not a hard reference**: it accepts a bare model id
/// string or the full table, and an entry that no longer resolves is
/// not a config error — the registry warns and falls back to the
/// first configured model (owner ruling: stale `default_model` must
/// never block startup).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(from = "DefaultModelRaw")]
pub struct DefaultModel {
    /// Provider id. Optional: when absent, `model` must resolve to exactly
    /// one provider's model — qualify with the provider only when the same
    /// model id is configured under more than one.
    pub provider: Option<String>,
    /// Model id within the provider (may itself contain `/`, as in
    /// `openai/gpt-oss-20b`).
    pub model: String,
    /// Thinking level name, when the model defines levels.
    pub thinking_level: Option<String>,
}

/// The wire forms of [`DefaultModel`]: a bare model id (must be
/// unambiguous across providers) or the full table.
#[derive(Deserialize)]
#[serde(untagged)]
enum DefaultModelRaw {
    Bare(String),
    Full {
        provider: Option<String>,
        model: String,
        thinking_level: Option<String>,
    },
}

impl From<DefaultModelRaw> for DefaultModel {
    fn from(raw: DefaultModelRaw) -> Self {
        match raw {
            DefaultModelRaw::Bare(model) => Self {
                provider: None,
                model,
                thinking_level: None,
            },
            DefaultModelRaw::Full {
                provider,
                model,
                thinking_level,
            } => Self {
                provider,
                model,
                thinking_level,
            },
        }
    }
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
    ///
    /// Before deserializing, each provider's `model_defaults` table is
    /// merged into its model entries at the VALUE level (a model's own
    /// keys win; absent keys inherit) — the only level where "unset"
    /// is distinguishable from an explicit value (`reasoning = false`
    /// vs absent). The defaults table itself is validated against the
    /// `Model` shape, so a typo there errors as loudly as one in a
    /// model entry.
    pub fn from_toml_str(raw: &str, path: &Path) -> Result<Self, ConfigError> {
        let mut value: toml::Value = toml::from_str(raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        apply_model_defaults(&mut value).map_err(|issue| ConfigError::Validation {
            path: path.to_path_buf(),
            issues: vec![issue],
        })?;
        let config: Self =
            value
                .try_into()
                .map_err(|source: toml::de::Error| ConfigError::Parse {
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

    /// Load the default config: the file named by `$TABIT_CONFIG`
    /// (which **replaces** the home location — set but missing is
    /// [`ConfigError::NotFound`], no fallthrough), else
    /// `<home>/.tabit/providers.toml`; `$TABIT_CONFIG_EXTRA` appends
    /// one more candidate. The env vars are the debugging/local
    /// override — point at a scratch config instead of touching the
    /// real one. (A future CLI flag will outrank the env vars; more
    /// specific scopes win.) A missing file at the DEFAULT location
    /// is **not** an error (the first-run ruling reversal, 2026-10:
    /// zero config boots; the user fixes it in-app) — an empty
    /// config is returned. What stays loud: an explicit pointer
    /// (`$TABIT_CONFIG` / `$TABIT_CONFIG_EXTRA`) set but missing is
    /// [`ConfigError::NotFound`] listing every candidate — never a
    /// silent fallthrough — and a parse error in an existing file is
    /// an error either way.
    pub fn load_default() -> Result<Self, ConfigError> {
        Self::load_default_located().map(|(config, _path)| config)
    }

    /// [`load_default`](Self::load_default), plus the path of the file
    /// actually read (`None` on a bare machine) — the catalog
    /// manager's auto file lands next to it (PROVIDERS.md).
    pub fn load_default_located() -> Result<(Self, Option<PathBuf>), ConfigError> {
        let candidates = default_config_paths();
        for path in &candidates {
            if path.is_file() {
                return Self::load(path).map(|config| (config, Some(path.clone())));
            }
        }
        // Nothing exists. A bare machine (no explicit pointer set)
        // gets the empty config — a fresh install is normal; an
        // explicit pointer that misses is the loud case.
        if std::env::var_os("TABIT_CONFIG").is_none()
            && std::env::var_os("TABIT_CONFIG_EXTRA").is_none()
        {
            return Ok((Self::default(), None));
        }
        Err(ConfigError::NotFound { paths: candidates })
    }

    /// Merge one providers fragment under this config — the
    /// extension-package contribution path (EXTENSIONS.md: fragments
    /// are merged at config load, never copied into the user's file).
    /// **The user's own provider wins on id collision, silently** —
    /// that is exactly what a user with an override expects, not a
    /// condition worth a warning; `user_ids` is the config's
    /// pre-merge provider ids (the user's own), so a fragment key the
    /// user already defined drops quietly while a key an *earlier
    /// fragment* contributed collides loudly (the user never chose
    /// that one — the warning names the overstepping origin). A
    /// fragment cannot set `default_model` (an extension steering the
    /// default model is not the package's call) — present ones warn
    /// and are ignored. Returns the provider ids the fragment
    /// contributed, in the fragment's own (alphabetical) order.
    pub fn merge_fragment(
        &mut self,
        fragment: TabitConfig,
        origin: &str,
        user_ids: &std::collections::HashSet<String>,
        warnings: &mut Vec<String>,
    ) -> Vec<String> {
        if fragment.default_model.is_some() {
            warnings.push(format!(
                "{origin}: providers fragment sets default_model — ignored (fragments carry providers only)"
            ));
        }
        let mut contributed = Vec::new();
        for (id, provider) in fragment.providers {
            if self.providers.contains_key(&id) {
                if !user_ids.contains(&id) {
                    warnings.push(format!(
                        "{origin}: provider `{id}` ignored — an earlier fragment already contributed it (scan order decides; disable one of the packages to resolve)"
                    ));
                }
                continue;
            }
            self.providers.insert(id.clone(), provider);
            contributed.push(id);
        }
        contributed
    }

    /// Merge the downloaded catalog (`providers.auto.toml`,
    /// PROVIDERS.md) under this config. Like fragment merge, an
    /// unknown provider id inserts whole; unlike fragments, a provider
    /// the config already has merges at MODEL granularity — the
    /// catalog's models fill in under the configured provider, and a
    /// model id the config already declares wins whole (provider-level
    /// fields of an existing provider are never touched). The catalog
    /// is one generated source, so no collision warnings exist here.
    pub fn merge_catalog(&mut self, catalog: TabitConfig) {
        for (id, catalog_provider) in catalog.providers {
            match self.providers.get_mut(&id) {
                None => {
                    self.providers.insert(id, catalog_provider);
                }
                Some(existing) => {
                    for model in catalog_provider.models {
                        if existing.model(&model.id).is_none() {
                            existing.models.push(model);
                        }
                    }
                }
            }
        }
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
        // `default_model` is deliberately absent: it is a preference,
        // and a stale one degrades at resolution (the registry warns
        // and falls back to the first configured model) rather than
        // failing the whole config.
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
}

/// Merge every provider's `model_defaults` table into its model
/// entries (the `from_toml_str` normalization — see its docs). A
/// model's own keys win; the defaults table is validated against the
/// `Model` shape with a placeholder id. Returns the one issue on
/// failure (the walk stops at the first malformed spot — config
/// errors are fixed one at a time anyway).
fn apply_model_defaults(value: &mut toml::Value) -> Result<(), String> {
    let Some(providers) = value
        .get_mut("providers")
        .and_then(toml::Value::as_table_mut)
    else {
        return Ok(());
    };
    for (provider_id, provider) in providers.iter_mut() {
        let Some(provider_table) = provider.as_table_mut() else {
            continue; // a non-table provider errors at deserialize
        };
        let Some(defaults) = provider_table.get("model_defaults") else {
            continue;
        };
        let Some(defaults) = defaults.as_table() else {
            return Err(format!(
                "providers.{provider_id}.model_defaults: must be a table"
            ));
        };
        // Validate the defaults against the Model shape: a typo here
        // must error as loudly as one in a model entry. The
        // placeholder id satisfies Model's one required field.
        let mut probe = toml::map::Map::new();
        probe.insert("id".to_string(), toml::Value::String("\0".to_string()));
        probe.extend(defaults.clone());
        if let Err(source) = crate::model::Model::deserialize(toml::Value::Table(probe)) {
            return Err(format!(
                "providers.{provider_id}.model_defaults: not a valid model-defaults table: {source}"
            ));
        }
        let defaults = defaults.clone();
        if let Some(models) = provider_table
            .get_mut("models")
            .and_then(toml::Value::as_array_mut)
        {
            for model in models.iter_mut() {
                let Some(model_table) = model.as_table_mut() else {
                    continue; // a non-table model errors at deserialize
                };
                let mut merged = defaults.clone();
                merged.extend(model_table.clone());
                *model_table = merged;
            }
        }
    }
    Ok(())
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
            if level.is_empty() {
                issues.push(format!(
                    "{model_key}: thinking level name must not be empty"
                ));
            }
            if !seen_level_names.insert(level.clone()) {
                issues.push(format!(
                    "{model_key}: duplicate thinking level name `{level}`",
                ));
            }
        }
    }
}

/// The default config search path. `$TABIT_CONFIG` **replaces** the
/// home file (the `$TABIT_SETTINGS`/`$TABIT_AUTH` pattern — a set
/// but missing path is [`ConfigError::NotFound`], never a silent
/// fallthrough to the user's real config); `$TABIT_CONFIG_EXTRA`
/// appends one more candidate after whatever the base is (the
/// additional-layer knob).
fn default_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(from_env) = std::env::var_os("TABIT_CONFIG") {
        paths.push(PathBuf::from(from_env));
    } else if let Some(home) = home_dir() {
        paths.push(home.join(".tabit").join("providers.toml"));
    }
    if let Some(extra) = std::env::var_os("TABIT_CONFIG_EXTRA") {
        paths.push(PathBuf::from(extra));
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
