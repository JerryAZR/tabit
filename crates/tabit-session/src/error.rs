//! Errors produced by the session layer.

use std::path::PathBuf;

/// A session-layer error, with enough context to point at the exact file
/// and setting at fault.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// An I/O error on a session file.
    #[error("session I/O error on `{path}`: {source}")]
    Io {
        /// The file being accessed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// A session file line is not valid JSON, or does not match the entry
    /// schema. `line` is one-based.
    #[error("session file `{path}` line {line} is not a valid session record: {source}")]
    Parse {
        /// The file being read.
        path: PathBuf,
        /// One-based line number.
        line: usize,
        /// The underlying deserialization error.
        #[source]
        source: serde_json::Error,
    },

    /// A session file violates its structural invariants (missing header,
    /// duplicate entry id, broken parent chain, wrong first line).
    #[error("session file `{path}` is corrupt: {message}")]
    Corrupt {
        /// The file being read.
        path: PathBuf,
        /// What invariant was violated.
        message: String,
    },

    /// The session log could not be found.
    #[error("session file `{path}` does not exist")]
    NotFound {
        /// The path that was attempted.
        path: PathBuf,
    },

    /// A tabit config file could not be loaded or validated.
    #[error(transparent)]
    ConfigFile(#[from] tabit_config::ConfigError),

    /// A provider/model reference does not resolve in the loaded config.
    #[error("config does not define {message}")]
    Config {
        /// What is missing, phrased to complete the sentence
        /// "config does not define ...".
        message: String,
    },

    /// A provider client could not be constructed from the provider
    /// config (bad endpoint, missing credential, ...). Client
    /// construction is provider-scoped — no model is involved yet —
    /// so the error names the provider alone.
    #[error("cannot build the provider client for `{provider}`: {message}")]
    ClientBuild {
        /// The provider id from config.
        provider: String,
        /// What failed.
        message: String,
    },

    /// A session record failed to reach the log during a run. The run's
    /// outcome may be correct but the session is no longer durable, so the
    /// failure is surfaced loudly instead of swallowed.
    #[error("session persistence failed during the run: {0}")]
    Persist(String),

    /// The agent outer loop failed.
    #[error("agent run failed: {0}")]
    Prompt(#[from] rig_agent::completion::PromptError),
}
