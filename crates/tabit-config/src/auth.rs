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
//! by `api_key_env`). Writes are the `login`/`logout` commands' one
//! surgical path ([`AuthConfig::set_api_key`]/[`AuthConfig::remove_api_key`]
//! — toml_edit, so comments, order, and other entries survive); the file
//! is otherwise the user's own.

use crate::ConfigError;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

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

    /// Store a provider's key in the auth file at `path` — the
    /// `login` command's write. Surgical (toml_edit): comments,
    /// order, and other providers' entries survive; a missing file is
    /// created (mode 0600 on unix — key material). Returns the
    /// resulting config, parsed from the written document so memory
    /// and file never disagree.
    pub fn set_api_key(path: &Path, provider: &str, api_key: &str) -> Result<Self, ConfigError> {
        let mut doc = read_document(path)?;
        {
            let providers = doc
                .as_table_mut()
                .entry("providers")
                .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
            let providers = providers
                .as_table_like_mut()
                .ok_or_else(|| not_a_table(path, "providers"))?;
            let entry = providers
                .entry(provider)
                .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
            entry
                .as_table_like_mut()
                .ok_or_else(|| not_a_table(path, provider))?
                .insert("api_key", toml_edit::Item::Value(api_key.into()));
        }
        // Parse before write: a document the schema rejects never
        // reaches the disk (the file and the reported state never
        // disagree).
        let auth = Self::from_toml_str(&doc.to_string(), path)?;
        write_document(path, &doc)?;
        Ok(auth)
    }

    /// Remove a provider's key from the auth file at `path` — the
    /// `logout` command's write. Idempotent: a missing file or an
    /// absent key is a no-op (a removal never creates the file, and
    /// an unchanged file is not rewritten). The emptied provider
    /// table goes with its key (`api_key` is the entry's only field —
    /// an empty table would not parse back). Returns the resulting
    /// config, parsed from the written document.
    pub fn remove_api_key(path: &Path, provider: &str) -> Result<Self, ConfigError> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let mut doc = read_document(path)?;
        let changed = {
            let mut changed = false;
            if let Some(providers) = doc
                .as_table_mut()
                .get_mut("providers")
                .and_then(toml_edit::Item::as_table_like_mut)
                && let Some(entry) = providers
                    .get_mut(provider)
                    .and_then(toml_edit::Item::as_table_like_mut)
                && entry.remove("api_key").is_some()
            {
                let emptied = providers
                    .get(provider)
                    .and_then(toml_edit::Item::as_table_like)
                    .is_some_and(toml_edit::TableLike::is_empty);
                if emptied {
                    providers.remove(provider);
                }
                changed = true;
            }
            changed
        };
        let auth = Self::from_toml_str(&doc.to_string(), path)?;
        if changed {
            write_document(path, &doc)?;
        }
        Ok(auth)
    }
}

/// The path `login`/`logout` write and [`AuthConfig::load_default`]
/// reads — one resolution for both so the written file is the one the
/// next boot reads: `$TABIT_AUTH` when set, else
/// `<home>/.tabit/auth.toml`. `None` when neither resolves.
pub fn default_path() -> Option<PathBuf> {
    if let Some(from_env) = std::env::var_os("TABIT_AUTH") {
        return Some(PathBuf::from(from_env));
    }
    crate::home_dir().map(|home| home.join(".tabit").join("auth.toml"))
}

/// Read and parse the auth file into an editable document; a missing
/// file is an empty document (the write creates it). A file that
/// exists but does not parse fails loudly with the same error the
/// load path reports.
fn read_document(path: &Path) -> Result<toml_edit::DocumentMut, ConfigError> {
    if !path.is_file() {
        return Ok(toml_edit::DocumentMut::new());
    }
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml_edit::DocumentMut::from_str(&raw).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source: <toml::de::Error as serde::de::Error>::custom(source.to_string()),
    })
}

/// The surgical edit met a key that exists but holds no table — the
/// file is malformed for auth purposes; say so, never clobber it.
fn not_a_table(path: &Path, key: &str) -> ConfigError {
    ConfigError::Parse {
        path: path.to_path_buf(),
        source: <toml::de::Error as serde::de::Error>::custom(format!(
            "`{key}` exists but is not a table"
        )),
    }
}

/// Write the document back as one atomic act: the content lands in a
/// sibling temp file first and is renamed over the target, so a crash
/// mid-write never leaves a truncated auth.toml for the next boot to
/// trip on. A missing file (and parent directory) is created — mode
/// 0600 on unix at creation, and an existing file's permissions
/// tighten to owner-only through the rename (key material).
fn write_document(path: &Path, doc: &toml_edit::DocumentMut) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut temp = path.as_os_str().to_os_string();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|source| ConfigError::Io {
            path: temp.clone(),
            source,
        })?;
        use std::io::Write as _;
        write!(file, "{doc}").map_err(|source| ConfigError::Io {
            path: temp.clone(),
            source,
        })?;
        std::fs::rename(&temp, path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
