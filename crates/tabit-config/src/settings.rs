//! Settings beyond providers/models: the user/workspace layers that
//! govern host-level behavior. Today that is one fact — the extension
//! disable list (EXTENSIONS.md's enablement ruling: an installed
//! package mounts by default — installing was the consent — and
//! `disabled` is the opt-out).
//!
//! Layers (union — disabling is the one explicit act, and any layer
//! naming a package disables it): the user file (`$TABIT_SETTINGS`,
//! else `<home>/.tabit/settings.toml` — the `$TABIT_CONFIG`
//! debug-override pattern) and the workspace file
//! (`<cwd>/.tabit/settings.toml`). A missing file at either layer is
//! the normal case (nothing disabled); a file that exists but does
//! not parse is a loud external error.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::ConfigError;

/// Parsed settings, merged over the layers.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SettingsConfig {
    /// Extension host settings.
    pub extensions: ExtensionsSettings,
}

/// Extension host settings.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExtensionsSettings {
    /// The disable list: a discovered package is skipped when its
    /// name is listed. Everything else mounts — install was the
    /// consent.
    pub disabled: Vec<String>,
}

impl SettingsConfig {
    /// Parse and validate a settings file, attributing errors to `path`.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Load the layers and union them: the user file (`$TABIT_SETTINGS`,
    /// else `<home>/.tabit/settings.toml`) plus the workspace file
    /// (`<cwd>/.tabit/settings.toml`). A missing file is not an error —
    /// a bare machine has settings, they just disable nothing.
    pub fn load_default() -> Result<Self, ConfigError> {
        let mut merged = Self::default();
        for path in default_settings_paths() {
            if path.is_file() {
                merged.absorb(Self::load(&path)?);
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            let workspace = cwd.join(".tabit").join("settings.toml");
            if workspace.is_file() {
                merged.absorb(Self::load(&workspace)?);
            }
        }
        Ok(merged)
    }

    /// Union another layer's disable list in (any layer naming a
    /// package disables it — there is no re-enable override, by
    /// design: disabling is the one explicit act).
    fn absorb(&mut self, other: Self) {
        self.extensions.disabled.extend(other.extensions.disabled);
    }

    /// The resolved disable list.
    pub fn disabled_extensions(&self) -> HashSet<String> {
        self.extensions.disabled.iter().cloned().collect()
    }
}

/// The user-layer candidates: `$TABIT_SETTINGS`, then
/// `<home>/.tabit/settings.toml`. The env var replaces the default
/// location (the `$TABIT_CONFIG` pattern — point it at a scratch file
/// instead of touching the real one).
fn default_settings_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(from_env) = std::env::var_os("TABIT_SETTINGS") {
        paths.push(PathBuf::from(from_env));
    }
    if let Some(home) = crate::home_dir() {
        paths.push(home.join(".tabit").join("settings.toml"));
    }
    paths
}
