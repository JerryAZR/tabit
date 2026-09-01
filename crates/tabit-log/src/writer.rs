//! The session file's write buffer: the contract, and the file-backed
//! implementor.
//!
//! [`WriteBuffer`] is the contract — the one interface callers have:
//! enqueue a batch. Everything else about file writing is the buffer's
//! private business. A batch enters the outbox as one all-or-nothing
//! unit (a failure anywhere rolls the whole batch back out — a partial
//! blob never queues), then the buffer attempts to write everything
//! queued. The write behavior is one thing, not a family of verbs:
//! **flush; on failure revert the file to its clean prefix and keep
//! the lines queued — every later enqueue retries them; on success
//! drop the lines.** The `Err` an enqueue returns is a report, not an
//! undo.
//!
//! Initialization lines are pre-populated, not special-cased: a fresh
//! session queues its header at construction, and the **first write
//! attempt materializes the file** — the no-orphan gate. A session
//! that never enqueues drains nothing and leaves nothing on disk; a
//! session that committed at least once tries a best-effort final
//! flush at drop.

use crate::entry::{FileRecord, SessionHeader};
use crate::error::LogError;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The buffer contract: the one interface callers have. See the module
/// docs for the behavior an implementation owns.
pub trait WriteBuffer: Send {
    /// Enqueue a batch of records as one all-or-nothing unit, then
    /// attempt the write of everything queued. On failure the batch
    /// stays queued and the file is reverted to its clean prefix — the
    /// `Err` is a report, not an undo.
    fn enqueue(&mut self, records: &[FileRecord]) -> Result<(), LogError>;

    /// Queue a record WITHOUT the write attempt — pre-population (the
    /// deferred opening `model_change` riding the first real commit:
    /// a session that never commits materializes nothing, so the
    /// register must not be the orphan-maker either).
    fn prequeue(&mut self, record: &FileRecord);

    /// How many lines sit in the outbox (the persist-degraded count,
    /// and the `durable` verdict: zero means every commit reached the
    /// disk).
    fn pending(&self) -> usize;

    /// The pending persist transition, if any (true = entered
    /// degraded, false = recovered), taken once — the session emits
    /// its notices from these.
    fn take_degraded_transition(&mut self) -> Option<bool>;
}

/// A handle to a session's shared write buffer — what the
/// [`ContextManager`](crate::context_manager::ContextManager) holds,
/// and what the session holds for its side records. One buffer per
/// session file.
pub type SharedBuffer = std::sync::Arc<std::sync::Mutex<dyn WriteBuffer + Send>>;

/// The file-backed write buffer for one session file. See the module
/// docs.
#[derive(Debug)]
pub struct SessionWriter {
    path: PathBuf,
    id: String,
    /// The append handle, opened at the first write attempt. `None`
    /// until then: nothing has touched the disk.
    file: Option<File>,
    /// Serialized lines whose bytes have not reached the disk yet, in
    /// commit order.
    outbox: VecDeque<String>,
    /// The file offset just past the last flushed byte — the rollback
    /// point when a write tears, and the boundary of the clean prefix.
    durable_offset: u64,
    /// The persist-degraded state: set while the outbox holds records
    /// a flush could not place, cleared when it drains. The
    /// transitions ride the notice channel so a frontend can nag about
    /// disk space instead of string-matching run failures.
    degraded: bool,
    /// One pending transition notice, if any (true = entered
    /// degraded, false = recovered). The session takes them at its
    /// emission points; at most one is pending (a transition that has
    /// not been observed still happened).
    transition: Option<bool>,
    /// The sticky born bit: set when a user message is first
    /// enqueued (the session's first real commit) and never cleared.
    /// The no-orphan gate is exactly this bit plus the file handle:
    /// a valid session has a user message, in the queue or in the
    /// file — a drain with neither writes nothing. Since the first
    /// commit's enqueue always attempts a drain, bit set implies the
    /// file exists from then on.
    born: bool,
}

impl SessionWriter {
    /// A fresh session: queue the header line, touch nothing. The file
    /// materializes at the first write attempt (with whatever else the
    /// outbox holds by then — the no-orphan gate: a session that never
    /// commits leaves nothing behind).
    pub fn create(path: PathBuf, header: SessionHeader) -> Self {
        let id = header.id.clone();
        // A `SessionHeader` is plain strings and numbers; a failure here
        // is unconstructible. Sanctioned crash (AGENTS.md doctrine).
        #[allow(clippy::expect_used)]
        let header_line =
            serde_json::to_string(&header).expect("a session header always serializes");
        Self {
            path,
            id,
            file: None,
            outbox: VecDeque::from([header_line]),
            durable_offset: 0,
            degraded: false,
            transition: None,
            born: false,
        }
    }

    /// Queue a record WITHOUT the write attempt — the deferred
    /// opening `model_change` rides the first real commit's drain (a
    /// session that never commits materializes nothing: the
    /// no-orphan gate holds even for the register).
    pub fn prequeue(&mut self, record: &FileRecord) {
        // A `FileRecord` of plain strings and numbers serializes;
        // a failure here is unconstructible (same stance as the
        // header). Sanctioned crash.
        #[allow(clippy::expect_used)]
        let line = serde_json::to_string(record).expect("a session record always serializes");
        self.outbox.push_back(line);
    }

    /// Re-open an existing session file for appending. The durable
    /// prefix is everything already in the file (the caller holds the
    /// parsed state; there is deliberately no second parse here).
    pub fn append_to(path: &Path, id: String, durable_offset: u64) -> Result<Self, LogError> {
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|source| LogError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(Self {
            path: path.to_path_buf(),
            id,
            file: Some(file),
            outbox: VecDeque::new(),
            durable_offset,
            degraded: false,
            transition: None,
            born: true,
        })
    }

    /// How many lines sit in the outbox (the persist-degraded `pending`
    /// count, and the `durable` verdict: zero means every commit
    /// reached the disk).
    pub fn pending(&self) -> usize {
        self.outbox.len()
    }

    /// Whether the outbox holds records a flush could not place.
    pub fn degraded(&self) -> bool {
        self.degraded
    }

    /// The pending persist transition, if any (true = entered
    /// degraded, false = recovered), taken once.
    pub fn take_degraded_transition(&mut self) -> Option<bool> {
        self.transition.take()
    }

    /// The session file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The session id (from the header; carried so the writer need not
    /// re-read its file).
    pub fn session_id(&self) -> &str {
        &self.id
    }

    /// Serialize one record into the outbox. These record types are
    /// plain strings and numbers — a serialization failure cannot
    /// happen; the `Err` exists so the enqueue rollback and this stay
    /// one mechanism.
    fn serialize(&mut self, record: &FileRecord) -> Option<LogError> {
        match serde_json::to_string(record) {
            Ok(line) => {
                self.outbox.push_back(line);
                None
            }
            Err(source) => Some(LogError::Io {
                path: self.path.clone(),
                source: source.into(),
            }),
        }
    }

    /// Un-queue everything a failed enqueue serialized.
    fn rollback_to_mark(&mut self, mark: usize) {
        while self.outbox.len() > mark {
            self.outbox.pop_back();
        }
    }

    /// The one write attempt over the outbox: materialize the file if
    /// this is the first attempt (the only open site), then **one
    /// write** for everything queued. On success the offset advances
    /// past the blob and the lines are dropped; on failure the file is
    /// truncated back to the durable offset (whatever bytes a partial
    /// write left are a torn tail, gone) and the lines stay queued, so
    /// a retried write can never splice into torn bytes.
    ///
    /// The no-orphan gate is universal: no file and nothing but birth
    /// lines (no user message queued) means no flush — every call
    /// site, no exceptions; a session that never gets a user message
    /// leaves nothing behind, not even a model change. The first
    /// commit's enqueue sets the sticky `born` bit, so the check
    /// costs no file-existence probe after that.
    fn drain(&mut self) -> Result<(), LogError> {
        if self.outbox.is_empty() {
            return Ok(());
        }
        // The no-orphan gate, universal: no user message ever
        // enqueued and no file — nothing to write, ever. (The
        // guard's probe of an unborn session skips the same way:
        // there is no stuck content to recover.)
        if self.file.is_none() && !self.born {
            return Ok(());
        }
        self.materialize()?;
        let file = self.file.as_mut().ok_or_else(|| LogError::Io {
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
            // The truncation goes through a separate write handle — an
            // append-mode handle cannot set_len on Windows — and the
            // append handle's next write lands at the new end regardless.
            let _ = truncate_to(&self.path, self.durable_offset);
            return Err(LogError::Io {
                path: self.path.clone(),
                source,
            });
        }
        self.durable_offset += blob.len() as u64;
        self.outbox.clear();
        Ok(())
    }

    /// Materialize the file (idempotent): create the directory and an
    /// empty file. The header is an ordinary queued line, so the drain's
    /// blob writes it — there is no header special case here. A previous
    /// materialize that died mid-blob may have left a partial file the
    /// `create_new` open refuses to replace: remove the orphan so this
    /// attempt (and the next) can proceed — the outbox still holds every
    /// line, so nothing is lost.
    fn materialize(&mut self) -> Result<(), LogError> {
        if self.file.is_some() {
            return Ok(());
        }
        let dir = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        fs::create_dir_all(&dir).map_err(|source| LogError::Io { path: dir, source })?;
        match OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => {
                self.file = Some(file);
                Ok(())
            }
            Err(source) => {
                let _ = fs::remove_file(&self.path);
                Err(LogError::Io {
                    path: self.path.clone(),
                    source,
                })
            }
        }
    }
}

impl WriteBuffer for SessionWriter {
    #[allow(clippy::panic, clippy::panic_in_result_fn)] // sanctioned crash: unconstructible failure (AGENTS.md doctrine)
    fn enqueue(&mut self, records: &[FileRecord]) -> Result<(), LogError> {
        let mark = self.outbox.len();
        for record in records {
            if let Some(error) = self.serialize(record) {
                // Roll the partial batch back out first — even the
                // crash path leaves no half blob queued — then fail
                // loud: a record that cannot serialize is an internal
                // bug, not an external condition.
                self.rollback_to_mark(mark);
                panic!("session record failed to serialize: {error}");
            }
        }
        // The first non-empty enqueue is the session's first real
        // commit — a user message is now enqueued: set the sticky
        // bit. Its drain always attempts (the gate passes with
        // content), so the bit implies the file from here on.
        if !records.is_empty() {
            self.born = true;
        }
        let outcome = self.drain();
        match &outcome {
            Ok(()) if self.degraded => {
                self.degraded = false;
                self.transition = Some(false);
            }
            Err(_) if !self.degraded => {
                self.degraded = true;
                self.transition = Some(true);
            }
            _ => {}
        }
        outcome
    }

    fn prequeue(&mut self, record: &FileRecord) {
        SessionWriter::prequeue(self, record);
    }

    fn pending(&self) -> usize {
        SessionWriter::pending(self)
    }

    fn take_degraded_transition(&mut self) -> Option<bool> {
        SessionWriter::take_degraded_transition(self)
    }
}

impl Drop for SessionWriter {
    /// Best-effort final flush — only for a session that already
    /// materialized its file (the no-orphan gate: a session that never
    /// committed leaves nothing behind, not even by dropping).
    fn drop(&mut self) {
        if self.file.is_some() {
            let _ = self.drain();
        }
    }
}

/// The no-op buffer: the same contract with the disk unplugged — the
/// conversation owner for standalone and wasm consumers. Everything
/// folds and grows; nothing persists.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullBuffer;

impl WriteBuffer for NullBuffer {
    fn enqueue(&mut self, _records: &[FileRecord]) -> Result<(), LogError> {
        Ok(())
    }

    fn prequeue(&mut self, _record: &FileRecord) {}

    fn pending(&self) -> usize {
        0
    }

    fn take_degraded_transition(&mut self) -> Option<bool> {
        None
    }
}

/// Truncate `path` to `len` through a separate write handle (Windows:
/// append-mode handles cannot `set_len`).
fn truncate_to(path: &Path, len: u64) -> Result<(), LogError> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|source| LogError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    file.set_len(len).map_err(|source| LogError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
