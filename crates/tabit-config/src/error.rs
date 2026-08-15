//! Errors produced while loading, parsing, or validating a tabit config file.

use std::path::PathBuf;

/// A configuration error, with enough context to point the user at the exact
/// file and setting to fix.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The config file could not be read.
    #[error("failed to read config file `{path}`: {source}")]
    Io {
        /// The file that was being read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The config file could not be parsed as TOML or did not match the
    /// schema.
    #[error("failed to parse config file `{path}`: {source}")]
    Parse {
        /// The file that was being parsed.
        path: PathBuf,
        /// The underlying deserialization error (includes line/column).
        #[source]
        source: toml::de::Error,
    },

    /// The config parsed but failed semantic validation. All issues are
    /// reported at once so the user can fix the file in one pass.
    #[error(
        "config file `{path}` is invalid:\n{}",
        issues
            .iter()
            .map(|issue| format!("  - {issue}"))
            .collect::<Vec<_>>()
            .join("\n")
    )]
    Validation {
        /// The file that failed validation.
        path: PathBuf,
        /// Every validation issue found, each with its config key path.
        issues: Vec<String>,
    },

    /// No config file exists at any of the candidate locations.
    #[error(
        "no config file found; tried:\n{}",
        paths
            .iter()
            .map(|path| format!("  - {}", path.display()))
            .collect::<Vec<_>>()
            .join("\n")
    )]
    NotFound {
        /// Every location that was checked, in order.
        paths: Vec<PathBuf>,
    },
}
