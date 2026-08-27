//! Session file storage: one JSONL file per session under a caller-chosen
//! directory.
//!
//! The default location is project-local — `<project-root>/.tabit/sessions`
//! with the project root discovered from the working directory (git root,
//! else the working directory itself) — because a path relative to the
//! project survives renames and moves, unlike a home-dir layout keyed by
//! the absolute cwd. The directory is always a constructor argument, so a
//! future config option can point it anywhere without touching this module.

use crate::entry::{FileRecord, SESSION_FORMAT_VERSION, SessionHeader};
use crate::error::SessionError;
use crate::ids;
use std::collections::VecDeque;
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
///
/// The writer is the write-behind sink (flag 8): records are
/// constructed by the recorder (which owns the resident tree and head)
/// and handed over already shaped, so this struct knows nothing about
/// the conversation — it serializes lines into a FIFO outbox and drains
/// it as one write.
#[derive(Debug)]
pub struct SessionWriter {
    path: PathBuf,
    /// `None` until the first append materializes the file.
    file: Option<fs::File>,
    /// The header, written when the file materializes.
    header: SessionHeader,
    /// The outbox: committed-in-memory records whose lines have not
    /// reached the disk yet, in commit order. FIFO — the file is
    /// always a clean prefix of commit order, so an unflushed record
    /// can never be skipped past (the prompt barrier relies on this).
    outbox: VecDeque<String>,
    /// The file offset just past the last flushed byte — the rollback
    /// point when a write tears, and the boundary of the clean
    /// prefix. It only ever advances on a fully successful drain, so
    /// a failed batch cannot leave part of itself durable.
    durable_offset: u64,
    id: String,
}

/// A session file loaded from disk.
#[derive(Debug)]
pub struct LoadedSession {
    /// The header line.
    pub header: SessionHeader,
    /// Every parseable record, in file order: conversation nodes of
    /// every branch interleaved with side records. Deriving the tree,
    /// the active head, and the selection register from this sequence
    /// is the loader's one-pass fold (the recorder's load).
    pub records: Vec<FileRecord>,
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
            outbox: VecDeque::new(),
            durable_offset: 0,
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
    /// (see [`crate::ModelRegistry::default_selection`]). Reads the
    /// file's last `model_change` in append order — the session
    /// preference register — so a rewind does not roll the hint back:
    /// the user's latest model choice is the hint, whichever branch it
    /// was recorded on. `None` when the file records no model change.
    pub fn last_model(&self, path: &Path) -> Result<Option<ModelSelection>, SessionError> {
        let loaded = self.open_path(path)?;
        Ok(
            crate::projection::last_model_change_in_file(&loaded.records).map(
                |(provider, model, level)| ModelSelection {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    thinking_level: level.map(str::to_string),
                },
            ),
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

        let mut records = Vec::new();
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
            let attempt = serde_json::from_str::<FileRecord>(trimmed);
            match attempt {
                Ok(FileRecord::Node(entry)) => {
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
                    records.push(FileRecord::Node(entry));
                }
                Ok(record @ FileRecord::Side(_)) => {
                    records.push(record);
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
            records,
            path: path.to_path_buf(),
            repairs,
        })
    }
}

impl SessionWriter {
    /// Re-open an existing session file for appending. The resident
    /// state (tree, head, selection) is rebuilt by the recorder's load
    /// fold over the parsed records, not here — the writer only owns
    /// the file handle and the outbox.
    pub fn open_existing(path: &Path) -> Result<SessionWriter, SessionError> {
        let loaded = SessionStore::new(path.parent().unwrap_or(Path::new("."))).open_path(path)?;
        let header = loaded.header;
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|source| SessionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        // The durable prefix starts at the file's end: everything on
        // disk is clean (a torn tail was repaired at load).
        let durable_offset = file.metadata().map_err(|source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let durable_offset = durable_offset.len();
        let id = header.id.clone();
        Ok(SessionWriter {
            path: path.to_path_buf(),
            file: Some(file),
            header,
            outbox: VecDeque::new(),
            durable_offset,
            id,
        })
    }

    /// Append one record: buffer its line, then attempt the drain. The
    /// commit is memory-first — the caller's resident state has already
    /// accepted the record, and a flush failure (including a file that
    /// cannot materialize yet) leaves the line buffered, not lost:
    /// retried on every subsequent write and at clean exit.
    pub fn append_record(&mut self, record: &FileRecord) -> Option<SessionError> {
        if let Some(error) = self.buffer_line(record) {
            return Some(error);
        }
        self.drain().err()
    }

    /// The prompt barrier's atomic core (flag 8): buffer every record's
    /// line, then drain the outbox as **one write** — all under this
    /// one call, so nothing interleaves. `Ok` = every line is durable;
    /// the turn may start. `Err` = a failure anywhere un-commits the
    /// batch in the file sense: the drain's failure path already
    /// truncated the torn bytes back to the durable offset (which only
    /// advances on a fully successful write), and this pops the batch
    /// back out of the outbox — the batch exists nowhere, and the
    /// caller (who constructed the records but held off accepting them
    /// into resident state) discards them and hands the texts back as
    /// drafts.
    pub fn commit_records(&mut self, records: &[FileRecord]) -> Result<(), SessionError> {
        let mark = self.outbox.len();
        for record in records {
            if let Some(error) = self.buffer_line(record) {
                self.rollback_to_mark(mark);
                return Err(error);
            }
        }
        if let Err(error) = self.drain() {
            self.rollback_to_mark(mark);
            return Err(error);
        }
        Ok(())
    }

    /// Serialize one record into the outbox — the memory half of every
    /// commit. No open-check; callers have run `ensure_open`.
    fn buffer_line(&mut self, record: &FileRecord) -> Option<SessionError> {
        match serde_json::to_string(record) {
            Ok(line) => {
                self.outbox.push_back(line);
                None
            }
            Err(source) => Some(SessionError::Io {
                path: self.path.clone(),
                source: source.into(),
            }),
        }
    }

    /// Un-commit a failed barrier: everything the batch buffered is
    /// popped back out. The file needs no separate rollback — the
    /// drain's failure path already truncated to the durable offset,
    /// which only advances on a fully successful write.
    fn rollback_to_mark(&mut self, mark: usize) {
        while self.outbox.len() > mark {
            self.outbox.pop_back();
        }
    }

    /// Drain the outbox: materialize the file if this is the first
    /// attempt, then **one write** for everything buffered; the offset
    /// advances past it. A failure rolls the file back to the durable
    /// offset — whatever bytes a partial write left are a torn tail,
    /// gone — so the file is always a clean prefix of commit order and
    /// a retried write can never splice into torn bytes. Buffered
    /// records stay for the next attempt (every subsequent commit
    /// retries, plus one at clean exit).
    fn drain(&mut self) -> Result<(), SessionError> {
        if self.outbox.is_empty() {
            return Ok(());
        }
        self.materialize()?;
        let file = self.file.as_mut().ok_or_else(|| SessionError::Io {
            path: self.path.clone(),
            source: std::io::Error::other("internal invariant violated: drain without a file"),
        })?;
        let mut blob = String::new();
        for line in &self.outbox {
            blob.push_str(line);
            blob.push('\n');
        }
        // A plain `File` is unbuffered — `write_all` (which retries
        // short writes itself) is the whole flush.
        if let Err(source) = file.write_all(blob.as_bytes()) {
            // Roll back to the clean prefix. The truncation goes
            // through a separate write handle — an append-mode handle
            // cannot set_len on Windows — and the append handle's next
            // write lands at the new end regardless.
            let _ = truncate_to(&self.path, self.durable_offset);
            return Err(SessionError::Io {
                path: self.path.clone(),
                source,
            });
        }
        self.durable_offset += blob.len() as u64;
        self.outbox.clear();
        Ok(())
    }

    /// Materialize the file (idempotent): create the directory, create
    /// the file, write the header, and queue the opening record at the
    /// front of the outbox so it drains as the first line after the
    /// header. Called from [`Self::drain`] — a record committed before
    /// the disk would accept the file simply waits in the outbox with
    /// everything else.
    fn materialize(&mut self) -> Result<(), SessionError> {
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
            .open(&self.path);
        let file = match file {
            Ok(file) => file,
            Err(source) => {
                // A previous materialize may have died mid-header,
                // leaving a partial file `create_new` now refuses to
                // replace. Remove the orphan so this attempt (and the
                // next) can proceed — the outbox still holds every
                // record, so nothing is lost.
                let _ = fs::remove_file(&self.path);
                return Err(SessionError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let line = serde_json::to_string(&self.header).map_err(|source| SessionError::Io {
            path: self.path.clone(),
            source: source.into(),
        })?;
        let mut file = file;
        let wrote = file.write_all(format!("{line}\n").as_bytes());
        if let Err(source) = wrote {
            self.file = None;
            let _ = fs::remove_file(&self.path);
            return Err(SessionError::Io {
                path: self.path.clone(),
                source,
            });
        }
        self.file = Some(file);
        // The header is the durable prefix's first byte range.
        self.durable_offset = line.len() as u64 + 1;
        Ok(())
    }

    /// One flush attempt over the outbox — the clean-exit retry (the
    /// same drain every commit attempts; failures are reported, the
    /// buffer keeps its contents for a process that outlives it).
    pub fn flush(&mut self) -> Result<(), SessionError> {
        self.drain()
    }

    /// How many records sit in the outbox (the persist-degraded
    /// `pending` count, and the `durable` verdict: zero means every
    /// commit reached the disk).
    pub fn pending(&self) -> usize {
        self.outbox.len()
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
