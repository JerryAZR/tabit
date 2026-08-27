//! Session file storage: one JSONL file per session under a caller-chosen
//! directory.
//!
//! The default location is project-local — `<project-root>/.tabit/sessions`
//! with the project root discovered from the working directory (git root,
//! else the working directory itself) — because a path relative to the
//! project survives renames and moves, unlike a home-dir layout keyed by
//! the absolute cwd. The directory is always a constructor argument, so a
//! future config option can point it anywhere without touching this module.
//!
//! The store is directory management and nothing else: it names files,
//! lists them, and hands their bytes to the parser. What a session file
//! means — the tree, the register, the stats — is the parser's one pass;
//! how it grows is the recorder's door; how it drains is the writer's
//! queue.

use crate::entry::{SESSION_FORMAT_VERSION, SessionHeader};
use crate::error::SessionError;
use crate::ids;
use crate::parser::{self, Parsed};
use crate::writer::SessionWriter;
use std::fs;
use std::path::{Path, PathBuf};

/// Creates and loads session files under one directory.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

/// Header-level facts about a stored session, for listing.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    /// Session id.
    pub id: String,
    /// Creation time (RFC 3339).
    pub created_at: String,
    /// Working directory recorded at creation.
    pub cwd: String,
    /// Number of entries in the file.
    pub entry_count: usize,
    /// The session file.
    pub path: PathBuf,
}

impl SessionStore {
    /// Open a store over an explicit sessions directory. The directory is
    /// created on first write.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The store at `<cwd>/.tabit/sessions` — the working directory
    /// the backend was started in. There is no project-root
    /// discovery: do not assume a git repo (owner ruling).
    pub fn project_default() -> Self {
        Self::project_default_from(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    /// [`SessionStore::project_default`] with an explicit starting
    /// directory (testable variant).
    pub fn project_default_from(start: &Path) -> Self {
        Self::new(start.join(".tabit").join("sessions"))
    }

    /// The sessions directory this store manages.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Prepare a new session. Nothing touches the disk: the writer holds
    /// its header in its queue and the file materializes on the first
    /// drain, so a session that never commits — no user message — leaves
    /// no orphan behind.
    pub fn create(&self, cwd: &str) -> SessionWriter {
        let header = SessionHeader {
            version: SESSION_FORMAT_VERSION,
            id: ids::new_session_id(),
            created_at: ids::now_rfc3339(),
            cwd: cwd.to_string(),
            parent_session: None,
        };
        let file_name = format!("{}_{}.jsonl", ids::filename_timestamp(), header.id.as_str());
        SessionWriter::create(self.dir.join(file_name), header)
    }

    /// Load the session file with the given session id.
    pub fn open(&self, session_id: &str) -> Result<Parsed, SessionError> {
        for summary in self.list()? {
            if summary.id == session_id {
                return self.open_path(&summary.path);
            }
        }
        Err(SessionError::NotFound {
            path: self.dir.join(format!("{session_id}.jsonl")),
        })
    }

    /// Load and parse a session file by path (the one pass).
    pub fn open_path(&self, path: &Path) -> Result<Parsed, SessionError> {
        parser::parse_file(path)
    }

    /// Every stored session, newest first (by creation timestamp in the
    /// header, file-name order as tiebreak).
    pub fn list(&self) -> Result<Vec<SessionSummary>, SessionError> {
        let dir = match fs::read_dir(&self.dir) {
            Ok(dir) => dir,
            // No sessions directory yet means no sessions (deferred
            // creation materializes it with the first session file);
            // anything else is a real read failure.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(source) => {
                return Err(SessionError::Io {
                    path: self.dir.clone(),
                    source,
                });
            }
        };
        let mut summaries = Vec::new();
        for entry in dir {
            let entry = entry.map_err(|source| SessionError::Io {
                path: self.dir.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let raw = fs::read_to_string(&path).map_err(|source| SessionError::Io {
                path: path.clone(),
                source,
            })?;
            let (header, entry_count) = parse_header_and_count(&raw, &path)?;
            summaries.push(SessionSummary {
                id: header.id,
                created_at: header.created_at,
                cwd: header.cwd,
                entry_count,
                path,
            });
        }
        summaries.sort_by(|a, b| {
            (b.created_at.clone(), b.path.clone()).cmp(&(a.created_at.clone(), a.path.clone()))
        });
        Ok(summaries)
    }
}

/// Read the header and count entries without full validation (listing fast
/// path).
fn parse_header_and_count(raw: &str, path: &Path) -> Result<(SessionHeader, usize), SessionError> {
    let mut lines = raw.split('\n').map(|l| l.trim_end_matches(['\r']));
    let header_line = lines.next().unwrap_or("");
    if header_line.is_empty() {
        return Err(SessionError::Corrupt {
            path: path.to_path_buf(),
            message: "file is empty; expected a header line".to_string(),
        });
    }
    let header: SessionHeader =
        serde_json::from_str(header_line).map_err(|source| SessionError::Parse {
            path: path.to_path_buf(),
            line: 1,
            source,
        })?;
    let count = lines.filter(|l| !l.is_empty()).count();
    Ok((header, count))
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
