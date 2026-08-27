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
    /// The parent-chain cursor, advanced at **buffer time** — entries
    /// chain in commit order the moment they are committed to memory,
    /// so the disk can lag without forking the chain (flag 8's two
    /// cursors: this one, and the durable offset below).
    leaf: Option<String>,
    /// The outbox: committed-in-memory entries whose lines have not
    /// reached the disk yet, in commit order. FIFO — the file is
    /// always a clean prefix of commit order, so an unflushed entry
    /// can never be skipped past (the prompt barrier relies on this).
    outbox: VecDeque<String>,
    /// The file offset just past the last flushed byte — the rollback
    /// point when a write tears, and the boundary of the clean
    /// prefix.
    durable_offset: u64,
    id: String,
}

/// One committed entry: it exists in the session's memory (chained,
/// id-minted, probe-visible) regardless of the flush outcome.
/// `flush_error` set means the line is still in the outbox — retried
/// on every subsequent write and at clean exit (flag 8's memory-first
/// commit).
#[derive(Debug)]
pub struct Committed {
    /// The entry as committed.
    pub entry: SessionEntry,
    /// The flush attempt's failure, if the outbox could not drain.
    pub flush_error: Option<SessionError>,
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
        Ok(projection::last_model_change_in_file(&loaded.entries).map(
            |(provider, model, level)| ModelSelection {
                provider: provider.to_string(),
                model: model.to_string(),
                thinking_level: level.map(str::to_string),
            },
        ))
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
            opening: None,
            leaf,
            outbox: VecDeque::new(),
            durable_offset,
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
        // The header is the durable prefix's first byte range.
        self.durable_offset = self
            .file
            .as_ref()
            .map(|file| file.metadata().map(|meta| meta.len()).unwrap_or(0))
            .unwrap_or(0);
        if let Some(opening) = self.opening.take() {
            // Already inside `ensure_open` — commit through the normal
            // path (enqueue + drain), not through `append_with_id`
            // (which re-runs this setup). A flush failure here is an
            // open failure: the file is not usable yet.
            let committed = self.append_entry(None, opening)?;
            if let Some(error) = committed.flush_error {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Append one record and return the committed entry.
    pub fn append(&mut self, kind: EntryKind) -> Result<Committed, SessionError> {
        self.append_with_id(None, kind)
    }

    /// Append one record under a caller-provided entry id — for records
    /// whose id was announced before the record existed (turns, born-early
    /// messages). The id is trusted unique; it becomes the entry's own id
    /// verbatim, so announced ids and log ids are the same value.
    pub fn append_as(&mut self, id: &str, kind: EntryKind) -> Result<Committed, SessionError> {
        self.append_with_id(Some(id.to_string()), kind)
    }

    pub(crate) fn append_with_id(
        &mut self,
        id: Option<String>,
        kind: EntryKind,
    ) -> Result<Committed, SessionError> {
        self.ensure_open()?;
        self.append_entry(id, kind)
    }

    /// Commit one entry — no open-check; callers must have opened the
    /// file (`append_with_id` and `ensure_open`'s opening write).
    /// The commit is memory-first: the entry chains (leaf advances)
    /// and its line joins the outbox, then the flush is *attempted* —
    /// a failure leaves the entry buffered, not lost.
    fn append_entry(
        &mut self,
        id: Option<String>,
        kind: EntryKind,
    ) -> Result<Committed, SessionError> {
        let entry = self.buffer_entry(id, kind)?;
        let flush_error = self.drain().err();
        Ok(Committed { entry, flush_error })
    }

    /// Buffer one entry without touching the disk: serialize, advance
    /// the chain, queue the line. The memory half of every commit —
    /// the barrier buffers a whole batch this way and flushes once, so
    /// the batch occupies one contiguous region of the outbox (and,
    /// once flushed, of the file) that a failure can roll back over.
    fn buffer_entry(
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
        let line = serde_json::to_string(&entry).map_err(|source| SessionError::Io {
            path: self.path.clone(),
            source: source.into(),
        })?;
        // Memory-first commit: the chain advances here, at buffer
        // time, so commit order and chain order can never disagree
        // no matter how the disk lags.
        self.leaf = Some(entry.id.clone());
        self.outbox.push_back(line);
        Ok(entry)
    }

    /// Drain the outbox: write every buffered line, in order, and
    /// flush. A torn or failed write rolls the file back to the
    /// durable offset (`set_len` + seek), so a retried entry can
    /// never splice a torn line — the file is a clean prefix of
    /// commit order at every instant. Buffered entries stay for the
    /// next attempt (every subsequent commit retries, plus one at
    /// clean exit).
    fn drain(&mut self) -> Result<(), SessionError> {
        while let Some(line) = self.outbox.front() {
            let file = self.file.as_mut().ok_or_else(|| SessionError::Io {
                path: self.path.clone(),
                source: std::io::Error::other(
                    "internal invariant violated: drain before ensure_open",
                ),
            })?;
            let written = writeln!(file, "{line}").and_then(|()| file.flush());
            match written {
                Ok(()) => {
                    self.durable_offset += (line.len() + 1) as u64;
                    self.outbox.pop_front();
                }
                Err(source) => {
                    // Roll back to the clean prefix: whatever bytes the
                    // torn write left are gone, and the buffered line
                    // will be rewritten whole on the next attempt. The
                    // truncation goes through a separate write handle —
                    // an append-only handle cannot set_len on Windows —
                    // and the append handle's next write lands at the
                    // new end regardless.
                    let _ = truncate_to(&self.path, self.durable_offset);
                    return Err(SessionError::Io {
                        path: self.path.clone(),
                        source,
                    });
                }
            }
        }
        Ok(())
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
    /// even if nothing follows. Returns the marker's own committed entry.
    pub fn rewind_to(&mut self, to: Option<&str>) -> Result<Committed, SessionError> {
        let committed = self.append(EntryKind::Rewound {
            to: to.map(str::to_string),
        })?;
        self.leaf = to.map(str::to_string);
        Ok(committed)
    }

    /// One flush attempt over the outbox — the clean-exit retry (the
    /// same drain every commit attempts; failures are reported, the
    /// buffer keeps its contents for a process that outlives it).
    pub fn flush(&mut self) -> Result<(), SessionError> {
        self.drain()
    }

    /// The prompt barrier's atomic core (flag 8): buffer every entry,
    /// then drain the outbox through them — all under this one call,
    /// so nothing interleaves and the batch is one contiguous region
    /// of the outbox and the file. `Ok` = every entry is durable; the
    /// turn may start. `Err` = a failure anywhere un-commits the whole
    /// batch — outbox, chain cursor, **and the file** (the region is
    /// truncated back to the pre-barrier offset; without that, a
    /// partially flushed batch would resurrect as history at reload,
    /// because the load leaf is the last entry in file order). The
    /// batch exists nowhere — the force-stop equivalent — and the
    /// caller hands the texts back as drafts and runs nothing.
    pub fn commit_barrier(
        &mut self,
        entries: Vec<(Option<String>, EntryKind)>,
    ) -> Result<Vec<SessionEntry>, SessionError> {
        self.ensure_open()?;
        let mark = self.outbox.len();
        let leaf_mark = self.leaf.clone();
        let offset_mark = self.durable_offset;
        let mut committed = Vec::with_capacity(entries.len());
        for (id, kind) in entries {
            match self.buffer_entry(id, kind) {
                Ok(entry) => committed.push(entry),
                // Unreachable through ensure_open's success (the
                // kinds always serialize), but the rollback
                // discipline is the same: leave nothing half-committed.
                Err(error) => {
                    self.rollback_barrier(mark, leaf_mark, offset_mark);
                    return Err(error);
                }
            }
        }
        if let Err(error) = self.drain() {
            self.rollback_barrier(mark, leaf_mark, offset_mark);
            return Err(error);
        }
        Ok(committed)
    }

    /// Un-commit a failed barrier: restore the outbox mark, the chain
    /// cursor, and the file prefix. `drain` already truncated any torn
    /// write; this also removes batch entries that made it to the disk
    /// before the failure — the truncation rides a separate write
    /// handle (`truncate_to`), because an append-only handle cannot
    /// `set_len` on Windows. Best-effort by necessity: if the
    /// truncation itself fails, the file keeps orphaned lines — a dead
    /// branch nothing chains through once a later entry appends — and
    /// the offset is left untouched so later writes never splice into
    /// them. (A hard crash between the failed drain and this rollback
    /// is the one window where a partial batch can still surface at
    /// reload; that window is the force-stop class, narrowed to the
    /// instant between two flushes by the single-drain barrier.)
    fn rollback_barrier(&mut self, mark: usize, leaf: Option<String>, offset: u64) {
        while self.outbox.len() > mark {
            self.outbox.pop_back();
        }
        self.leaf = leaf;
        if self.durable_offset <= offset {
            return;
        }
        if truncate_to(&self.path, offset).is_ok() {
            self.durable_offset = offset;
        }
    }

    /// How many entries sit in the outbox (the persist-degraded
    /// `pending` count, and the `durable` verdict: zero means every
    /// commit reached the disk).
    pub fn pending(&self) -> usize {
        self.outbox.len()
    }

    /// The outbox's lines, in commit order — the resident view's
    /// buffered tail (flag 8: a reload spans file + buffer when the
    /// disk is degraded).
    pub fn buffered_lines(&self) -> Vec<String> {
        self.outbox.iter().cloned().collect()
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
