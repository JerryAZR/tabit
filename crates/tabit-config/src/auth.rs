//! Credentials for providers, kept in a separate file from provider config.
//!
//! `auth.toml` maps provider ids to API keys:
//!
//! ```toml
//! [providers.anthropic]
//! api_key = "sk-ant-..."
//! ```
//!
//! Keeping secrets out of `providers.toml` means the provider config is
//! safe to display, share, and edit with agent assistance; the auth file is
//! the one place key material lives (aside from environment variables named
//! by `api_key_env`). The file is created and permissioned by the user —
//! tabit never writes it.

use crate::ConfigError;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Provider credentials loaded from `auth.toml`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Provider id -> API key.
    #[serde(default)]
    pub providers: BTreeMap<String, AuthEntry>,
}

/// The credential for one provider.
#[derive(Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthEntry {
    /// The API key.
    pub api_key: String,
}

impl std::fmt::Debug for AuthEntry {
    /// Redacts the key material — a Debug of a credential must be safe
    /// to print (a test panic once surfaced real keys through the
    /// derived rendering). Provider names and key shapes stay visible:
    /// `AuthConfig`'s derived Debug nests this impl.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthEntry")
            .field(
                "api_key",
                &format!("<redacted: {} chars>", self.api_key.chars().count()),
            )
            .finish()
    }
}

impl AuthConfig {
    /// Look up the key configured for a provider.
    pub fn api_key(&self, provider_id: &str) -> Option<&str> {
        self.providers
            .get(provider_id)
            .map(|entry| entry.api_key.as_str())
    }

    /// Parse an auth config from a TOML string, attributing any error to
    /// `path`.
    pub fn from_toml_str(raw: &str, path: &Path) -> Result<Self, ConfigError> {
        toml::from_str(raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Load an auth config file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&raw, path)
    }

    /// Load the default auth config: the file named by `$TABIT_AUTH`, else
    /// `<home>/.tabit/auth.toml`. Unlike provider config, a missing file is
    /// **not** an error — auth is optional (local endpoints run keyless, and
    /// `api_key_env` may carry the key instead); an empty [`AuthConfig`]
    /// is returned.
    pub fn load_default() -> Result<Self, ConfigError> {
        // An explicit override is authoritative: when `$TABIT_AUTH` is set,
        // the home file is never consulted — a missing override file is an
        // empty config, not a silent fallback (the search-list version read
        // the developer's real auth file and broke hermeticity).
        if let Some(from_env) = std::env::var_os("TABIT_AUTH") {
            let path = PathBuf::from(from_env);
            if !path.is_file() {
                return Ok(Self::default());
            }
            return Self::load(&path);
        }
        if let Some(home) = crate::home_dir() {
            let path = home.join(".tabit").join("auth.toml");
            if path.is_file() {
                return Self::load(&path);
            }
        }
        Ok(Self::default())
    }
}
