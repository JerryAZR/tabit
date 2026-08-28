//! Errors produced by the durable-conversation layer.

use std::path::PathBuf;

/// A durable-conversation error, with enough context to point at the
/// exact file at fault.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    /// An I/O error on a session file.
    #[error("session I/O error on `{path}`: {source}")]
    Io {
        /// The file being accessed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}
