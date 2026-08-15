//! Session file storage: one JSONL file per session under a caller-chosen
//! directory.
//!
//! The default location is project-local — `<project-root>/.tabit/sessions`
//! with the project root discovered from the working directory (git root,
//! else the working directory itself) — because a path relative to the
//! project survives renames and moves, unlike a home-dir layout keyed by
//! the absolute cwd. The directory is always a constructor argument, so a
//! future config option can point it anywhere without touching this module.

use crate::entry::{EntryKind, SESSION_FORMAT_VERSION, SessionEntry, SessionHeader};
use crate::error::SessionError;
use crate::ids;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Creates and loads session files under one directory.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

/// A session file opened for appending.
#[derive(Debug)]
pub struct SessionWriter {
    path: PathBuf,
    file: fs::File,
    leaf: Option<String>,
    id: String,
}

/// A session file loaded from disk.
#[derive(Debug)]
pub struct LoadedSession {
    /// The header line.
    pub header: SessionHeader,
    /// Every parseable entry, in file order.
    pub entries: Vec<SessionEntry>,
    /// The file the session was loaded from.
    pub path: PathBuf,
    /// Repairs applied while loading (e.g. a torn tail line dropped).
    pub repairs: Vec<Repair>,
}

/// A destructive fixup applied to a session file during load, reported so
/// the user knows what happened to their data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repair {
    /// The final line was a torn write (crash mid-append). The file was
    /// truncated to the end of the last complete record; the partial bytes
    /// are carried verbatim.
    TornTail {
        /// The dropped partial line.
        dropped: String,
    },
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

    /// The store at `<project-root>/.tabit/sessions`, where the project
    /// root is discovered from the current working directory: the nearest
    /// ancestor containing `.git`, else the working directory itself.
    pub fn project_default() -> Self {
        Self::project_default_from(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    /// [`SessionStore::project_default`] with an explicit starting
    /// directory (testable variant).
    pub fn project_default_from(start: &Path) -> Self {
        Self::new(project_root_from(start).join(".tabit").join("sessions"))
    }

    /// The sessions directory this store manages.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Create a new session file (header only) and return its writer.
    pub fn create(&self, cwd: &str) -> Result<SessionWriter, SessionError> {
        fs::create_dir_all(&self.dir).map_err(|source| SessionError::Io {
            path: self.dir.clone(),
            source,
        })?;
        let header = SessionHeader {
            version: SESSION_FORMAT_VERSION,
            id: ids::new_session_id(),
            created_at: ids::now_rfc3339(),
            cwd: cwd.to_string(),
            parent_session: None,
        };
        let file_name = format!("{}_{}.jsonl", ids::filename_timestamp(), header.id.as_str());
        let path = self.dir.join(file_name);
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .map_err(|source| SessionError::Io {
                path: path.clone(),
                source,
            })?;
        let line = serde_json::to_string(&header).map_err(|source| SessionError::Io {
            path: path.clone(),
            source: source.into(),
        })?;
        let mut file = file;
        writeln!(file, "{line}").map_err(|source| SessionError::Io {
            path: path.clone(),
            source,
        })?;
        file.flush().map_err(|source| SessionError::Io {
            path: path.clone(),
            source,
        })?;
        let id = header.id.clone();
        Ok(SessionWriter {
            path,
            file,
            leaf: None,
            id,
        })
    }

    /// Load the session file with the given session id.
    pub fn open(&self, session_id: &str) -> Result<LoadedSession, SessionError> {
        for summary in self.list()? {
            if summary.id == session_id {
                return self.open_path(&summary.path);
            }
        }
        Err(SessionError::NotFound {
            path: self.dir.join(format!("{session_id}.jsonl")),
        })
    }

    /// Load a session file by path.
    pub fn open_path(&self, path: &Path) -> Result<LoadedSession, SessionError> {
        let raw = fs::read_to_string(path).map_err(|source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse_and_repair(raw, path)
    }

    /// Every stored session, newest first (by creation timestamp in the
    /// header, file-name order as tiebreak).
    pub fn list(&self) -> Result<Vec<SessionSummary>, SessionError> {
        let dir = fs::read_dir(&self.dir).map_err(|source| SessionError::Io {
            path: self.dir.clone(),
            source,
        })?;
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

    /// Parse file contents, repairing a torn tail (crash mid-append) by
    /// truncating the file to the last complete record. Any other malformed
    /// line is corruption and fails loudly.
    fn parse_and_repair(raw: String, path: &Path) -> Result<LoadedSession, SessionError> {
        let lines: Vec<&str> = raw.split('\n').collect();
        let header_line = lines.first().copied().unwrap_or("").trim_end();
        let header: SessionHeader =
            serde_json::from_str(header_line).map_err(|source| SessionError::Parse {
                path: path.to_path_buf(),
                line: 1,
                source,
            })?;
        if header.version != SESSION_FORMAT_VERSION {
            return Err(SessionError::Corrupt {
                path: path.to_path_buf(),
                message: format!(
                    "unsupported session format version {} (this tabit reads version {})",
                    header.version, SESSION_FORMAT_VERSION
                ),
            });
        }

        let mut entries = Vec::new();
        let mut seen_ids = std::collections::BTreeSet::new();
        let mut repairs = Vec::new();
        // Byte offset of the start of the current line, for tail truncation.
        let mut offset = lines.first().map_or(0, |l| l.len() + 1);
        let last_index = lines.len().saturating_sub(1);
        for (index, line) in lines.iter().enumerate().skip(1) {
            let line_start = offset;
            offset += line.len() + 1;
            let trimmed = line.trim_end_matches(['\r']);
            if trimmed.is_empty() {
                continue;
            }
            let attempt = serde_json::from_str::<SessionEntry>(trimmed);
            match attempt {
                Ok(entry) => {
                    if !seen_ids.insert(entry.id.clone()) {
                        return Err(SessionError::Corrupt {
                            path: path.to_path_buf(),
                            message: format!("duplicate entry id `{}`", entry.id),
                        });
                    }
                    if let Some(parent) = &entry.parent_id
                        && !seen_ids.contains(parent)
                    {
                        return Err(SessionError::Corrupt {
                            path: path.to_path_buf(),
                            message: format!(
                                "entry `{}` references unknown parent `{}`",
                                entry.id, parent
                            ),
                        });
                    }
                    entries.push(entry);
                }
                Err(source) => {
                    // A torn write can only ever be the final line (an
                    // append in progress when the process died). A malformed
                    // line anywhere else is real corruption.
                    if index == last_index {
                        let dropped = trimmed.to_string();
                        truncate_to(path, line_start as u64)?;
                        repairs.push(Repair::TornTail { dropped });
                        break;
                    }
                    return Err(SessionError::Parse {
                        path: path.to_path_buf(),
                        line: index + 1,
                        source,
                    });
                }
            }
        }

        Ok(LoadedSession {
            header,
            entries,
            path: path.to_path_buf(),
            repairs,
        })
    }
}

impl SessionWriter {
    /// Re-open an existing session file for appending, resuming the parent
    /// chain from its last complete record.
    pub fn open_existing(path: &Path) -> Result<SessionWriter, SessionError> {
        let loaded = SessionStore::new(path.parent().unwrap_or(Path::new("."))).open_path(path)?;
        let header = loaded.header;
        let leaf = loaded.entries.last().map(|entry| entry.id.clone());
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|source| SessionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(SessionWriter {
            path: path.to_path_buf(),
            file,
            leaf,
            id: header.id,
        })
    }

    /// Append one record and return the persisted entry.
    pub fn append(&mut self, kind: EntryKind) -> Result<SessionEntry, SessionError> {
        let entry = SessionEntry::new(self.leaf.clone(), ids::now_rfc3339(), kind);
        let line = serde_json::to_string(&entry).map_err(|source| SessionError::Io {
            path: self.path.clone(),
            source: source.into(),
        })?;
        writeln!(self.file, "{line}").map_err(|source| SessionError::Io {
            path: self.path.clone(),
            source,
        })?;
        self.file.flush().map_err(|source| SessionError::Io {
            path: self.path.clone(),
            source,
        })?;
        self.leaf = Some(entry.id.clone());
        Ok(entry)
    }

    /// The id of the last appended entry.
    pub fn leaf(&self) -> Option<&str> {
        self.leaf.as_deref()
    }

    /// The session file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The session id from the header.
    pub fn session_id(&self) -> &str {
        &self.id
    }
}

/// The nearest ancestor of `start` containing a `.git` entry, else `start`.
fn project_root_from(start: &Path) -> PathBuf {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        current = dir.parent();
    }
    start.to_path_buf()
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

fn truncate_to(path: &Path, len: u64) -> Result<(), SessionError> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    file.set_len(len).map_err(|source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
