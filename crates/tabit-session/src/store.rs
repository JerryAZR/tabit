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
use crate::projection;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use tabit_protocol::ModelSelection;

/// Creates and loads session files under one directory.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

/// A session file opened for appending. Creation is deferred: a writer
/// from [`SessionStore::create`] holds its header in memory and the file
/// materializes on the first append — a session that never records a
/// user message leaves nothing on disk.
#[derive(Debug)]
pub struct SessionWriter {
    path: PathBuf,
    /// `None` until the first append materializes the file.
    file: Option<fs::File>,
    /// The header, written when the file materializes.
    header: SessionHeader,
    /// An entry written immediately after the header on materialization
    /// (the opening `model_change`).
    opening: Option<EntryKind>,
    leaf: Option<String>,
    id: String,
}

/// A session file loaded from disk.
#[derive(Debug)]
pub struct LoadedSession {
    /// The header line.
    pub header: SessionHeader,
    /// Every parseable entry, in file order (all branches, plus
    /// bookkeeping markers).
    pub entries: Vec<SessionEntry>,
    /// The active chain: root → effective leaf, the entries the next
    /// outer loop sees. Diverges from file order once the log holds
    /// branches.
    pub chain: Vec<SessionEntry>,
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

    /// Prepare a new session. Nothing touches the disk: the file (and
    /// the sessions directory) materialize on the writer's first append,
    /// so a session that never records — no user message — leaves no
    /// orphan behind.
    pub fn create(&self, cwd: &str) -> SessionWriter {
        let header = SessionHeader {
            version: SESSION_FORMAT_VERSION,
            id: ids::new_session_id(),
            created_at: ids::now_rfc3339(),
            cwd: cwd.to_string(),
            parent_session: None,
        };
        let file_name = format!("{}_{}.jsonl", ids::filename_timestamp(), header.id.as_str());
        let id = header.id.clone();
        SessionWriter {
            path: self.dir.join(file_name),
            file: None,
            header,
            opening: None,
            leaf: None,
            id,
        }
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

    /// The model a session file last used, for default-selection hints
    /// (see [`crate::ModelRegistry::default_selection`]). Follows the
    /// active chain, so a rewind past a model switch rolls the hint back
    /// with it. `None` when the chain records no model change.
    pub fn last_model(&self, path: &Path) -> Result<Option<ModelSelection>, SessionError> {
        let loaded = self.open_path(path)?;
        Ok(
            projection::last_model_change(&loaded.chain).map(|(provider, model, level)| {
                ModelSelection {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    thinking_level: level.map(str::to_string),
                }
            }),
        )
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

        let leaf = effective_leaf(&entries, path)?;
        let chain = chain_from(&entries, leaf.as_deref(), path)?;

        Ok(LoadedSession {
            header,
            entries,
            chain,
            path: path.to_path_buf(),
            repairs,
        })
    }
}

impl SessionWriter {
    /// Re-open an existing session file for appending, resuming the parent
    /// chain from the entry the log's rewind state points at.
    pub fn open_existing(path: &Path) -> Result<SessionWriter, SessionError> {
        let loaded = SessionStore::new(path.parent().unwrap_or(Path::new("."))).open_path(path)?;
        let header = loaded.header;
        let leaf = effective_leaf(&loaded.entries, path)?;
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|source| SessionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        let id = header.id.clone();
        Ok(SessionWriter {
            path: path.to_path_buf(),
            file: Some(file),
            header,
            opening: None,
            leaf,
            id,
        })
    }

    /// An entry written immediately after the header when a deferred
    /// session materializes (the opening `model_change`); meaningless on
    /// a writer that is already on disk.
    pub fn set_opening_entry(&mut self, kind: EntryKind) {
        self.opening = Some(kind);
    }

    /// Materialize the file: create the directory, write the header, and
    /// flush the opening entry. Idempotent.
    fn ensure_open(&mut self) -> Result<(), SessionError> {
        if self.file.is_some() {
            return Ok(());
        }
        let dir = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        fs::create_dir_all(&dir).map_err(|source| SessionError::Io { path: dir, source })?;
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| SessionError::Io {
                path: self.path.clone(),
                source,
            })?;
        let line = serde_json::to_string(&self.header).map_err(|source| SessionError::Io {
            path: self.path.clone(),
            source: source.into(),
        })?;
        let mut file = file;
        writeln!(file, "{line}").map_err(|source| SessionError::Io {
            path: self.path.clone(),
            source,
        })?;
        file.flush().map_err(|source| SessionError::Io {
            path: self.path.clone(),
            source,
        })?;
        self.file = Some(file);
        if let Some(opening) = self.opening.take() {
            // Already inside `ensure_open` — write directly, not through
            // `append_with_id` (which re-runs this setup).
            self.append_entry(None, opening)?;
        }
        Ok(())
    }

    /// Append one record and return the persisted entry.
    pub fn append(&mut self, kind: EntryKind) -> Result<SessionEntry, SessionError> {
        self.append_with_id(None, kind)
    }

    /// Append one record under a caller-provided entry id — for records
    /// whose id was announced before the record existed (turns, born-early
    /// messages). The id is trusted unique; it becomes the entry's own id
    /// verbatim, so announced ids and log ids are the same value.
    pub fn append_as(&mut self, id: &str, kind: EntryKind) -> Result<SessionEntry, SessionError> {
        self.append_with_id(Some(id.to_string()), kind)
    }

    pub(crate) fn append_with_id(
        &mut self,
        id: Option<String>,
        kind: EntryKind,
    ) -> Result<SessionEntry, SessionError> {
        self.ensure_open()?;
        self.append_entry(id, kind)
    }

    /// Write one entry — no open-check; callers must have opened the file
    /// (`append_with_id` and `ensure_open`'s opening write).
    fn append_entry(
        &mut self,
        id: Option<String>,
        kind: EntryKind,
    ) -> Result<SessionEntry, SessionError> {
        let entry = SessionEntry::with_id(
            id.unwrap_or_else(ids::new_entry_id),
            self.leaf.clone(),
            ids::now_rfc3339(),
            kind,
        );
        self.write_entry(entry)
    }

    fn write_entry(&mut self, entry: SessionEntry) -> Result<SessionEntry, SessionError> {
        let line = serde_json::to_string(&entry).map_err(|source| SessionError::Io {
            path: self.path.clone(),
            source: source.into(),
        })?;
        let Some(file) = self.file.as_mut() else {
            // Unreachable through `append`/`append_as` (ensure_open runs
            // first); loud rather than silently dropping the record.
            return Err(SessionError::Io {
                path: self.path.clone(),
                source: std::io::Error::other(
                    "internal invariant violated: append_entry before ensure_open",
                ),
            });
        };
        writeln!(file, "{line}").map_err(|source| SessionError::Io {
            path: self.path.clone(),
            source,
        })?;
        file.flush().map_err(|source| SessionError::Io {
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

    /// Record a rewind: append a `rewound` marker — parented to the
    /// current leaf, the honest record of where the abandoned chain
    /// ended — then move the leaf to `to`, so the next append branches
    /// from there (`None` branches from the root). The caller proves `to`
    /// names an entry in this log; the marker alone makes the move durable
    /// even if nothing follows. Returns the marker's own entry id (the
    /// recorder tracks it — every id the file holds is a probe answer).
    pub fn rewind_to(&mut self, to: Option<&str>) -> Result<String, SessionError> {
        let marker = self.append(EntryKind::Rewound {
            to: to.map(str::to_string),
        })?;
        self.leaf = to.map(str::to_string);
        Ok(marker.id)
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

/// The entry the log's active chain ends at: normally the last entry in
/// file order, but a `rewound` marker moves it to the entry the marker
/// names. A marker pointing at an entry that does not precede it is
/// corruption (rewind targets can only be entries that already existed).
fn effective_leaf(entries: &[SessionEntry], path: &Path) -> Result<Option<String>, SessionError> {
    let mut leaf = None;
    let mut seen = std::collections::BTreeSet::new();
    for entry in entries {
        if let EntryKind::Rewound { to } = &entry.kind {
            if let Some(target) = to
                && !seen.contains(target)
            {
                return Err(SessionError::Corrupt {
                    path: path.to_path_buf(),
                    message: format!(
                        "rewound marker `{}` targets unknown or later entry `{target}`",
                        entry.id
                    ),
                });
            }
            leaf = to.clone();
        } else {
            leaf = Some(entry.id.clone());
        }
        seen.insert(entry.id.clone());
    }
    Ok(leaf)
}

/// The active chain of `entries`: `leaf` walked to the root via parent
/// links, reversed into root→leaf order. Parent validation during parsing
/// guarantees every link resolves; a miss is unreachable corruption and
/// still fails loudly rather than producing a partial chain.
pub(crate) fn chain_from(
    entries: &[SessionEntry],
    leaf: Option<&str>,
    path: &Path,
) -> Result<Vec<SessionEntry>, SessionError> {
    let Some(leaf) = leaf else {
        return Ok(Vec::new());
    };
    let by_id: std::collections::HashMap<&str, &SessionEntry> = entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect();
    let mut chain = Vec::new();
    let mut current = leaf;
    loop {
        let entry = by_id.get(current).ok_or_else(|| SessionError::Corrupt {
            path: path.to_path_buf(),
            message: format!("chain walks through missing entry `{current}`"),
        })?;
        chain.push((*entry).clone());
        match &entry.parent_id {
            Some(parent) => current = parent.as_str(),
            None => return Ok(chain.into_iter().rev().collect()),
        }
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
