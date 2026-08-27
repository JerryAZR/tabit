//! The session file's write queue: a plain FIFO of serialized lines and
//! the one drain that puts them on disk.
//!
//! The writer is structure-blind — records arrive pre-constructed (the
//! recorder owns the tree, the context, the validation), join the outbox
//! as lines, and drain as **one write** per attempt, so the file is
//! always a clean prefix of commit order. Initialization lines are
//! pre-populated, not special-cased: a fresh session queues its header
//! (and the recorder queues the opening `model_change`) before anything
//! happens, and the **first drain materializes the file** — the
//! no-orphan gate. A session that never commits drains nothing and
//! leaves nothing on disk.
//!
//! A failed drain rolls the file back to the durable offset (the
//! boundary of the clean prefix — only a fully successful write advances
//! it); whether the failed batch's lines stay queued or pop back out is
//! the caller's policy, expressed by the two write verbs.

use crate::entry::{FileRecord, SessionHeader};
use crate::error::SessionError;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The write-behind queue for one session file. See the module docs.
#[derive(Debug)]
pub struct SessionWriter {
    path: PathBuf,
    id: String,
    /// The append handle, opened at the first drain. `None` until then:
    /// nothing has touched the disk.
    file: Option<File>,
    /// Serialized lines whose bytes have not reached the disk yet, in
    /// commit order.
    outbox: VecDeque<String>,
    /// The file offset just past the last flushed byte — the rollback
    /// point when a write tears, and the boundary of the clean prefix.
    durable_offset: u64,
}

impl SessionWriter {
    /// A fresh session: queue the header line, touch nothing. The file
    /// materializes at the first drain (with whatever else the outbox
    /// holds by then — the no-orphan gate: a session that never commits
    /// leaves nothing behind).
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
        }
    }

    /// Re-open an existing session file for appending. The durable
    /// prefix is everything already in the file (the caller holds the
    /// parsed state; there is deliberately no second parse here).
    pub fn append_to(path: &Path, id: String, durable_offset: u64) -> Result<Self, SessionError> {
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|source| SessionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(Self {
            path: path.to_path_buf(),
            id,
            file: Some(file),
            outbox: VecDeque::new(),
            durable_offset,
        })
    }

    /// Queue one record without draining — pre-population (a deferred
    /// opening `model_change` riding the first commit) and nothing else:
    /// every real commit uses a write verb.
    pub fn buffer(&mut self, record: &FileRecord) -> Option<SessionError> {
        self.buffer_line(record)
    }

    /// The gated write (the prompt barrier): queue the records and drain
    /// as one blob. On failure the batch pops back out — it exists
    /// nowhere, and the caller treats the `Err` as "never accepted".
    pub fn write_gated(&mut self, records: &[FileRecord]) -> Result<(), SessionError> {
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

    /// The write-behind verb (roundtrips, steers, side records): queue
    /// the records and drain as one blob. On failure the lines **stay**
    /// queued — the caller already accepted them into memory, the file
    /// is rolled back to its clean prefix, and every later write (and
    /// the clean-exit flush) retries. Returns the drain error, if any.
    pub fn write_behind(&mut self, records: &[FileRecord]) -> Option<SessionError> {
        for record in records {
            if let Some(error) = self.buffer_line(record) {
                return Some(error);
            }
        }
        self.drain().err()
    }

    /// One flush attempt over the outbox — the clean-exit retry (the
    /// same drain every write attempts).
    pub fn flush(&mut self) -> Result<(), SessionError> {
        self.drain()
    }

    /// How many lines sit in the outbox (the persist-degraded `pending`
    /// count, and the `durable` verdict: zero means every commit reached
    /// the disk).
    pub fn pending(&self) -> usize {
        self.outbox.len()
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

    /// Un-queue everything a failed gated write buffered.
    fn rollback_to_mark(&mut self, mark: usize) {
        while self.outbox.len() > mark {
            self.outbox.pop_back();
        }
    }

    /// Drain the outbox: materialize the file if this is the first
    /// attempt (the only open site), then **one write** for everything
    /// queued. On success the offset advances past the blob; on failure
    /// the file is truncated back to the durable offset (whatever bytes
    /// a partial write left are a torn tail, gone), so a retried write
    /// can never splice into torn bytes.
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
            // The truncation goes through a separate write handle — an
            // append-mode handle cannot set_len on Windows — and the
            // append handle's next write lands at the new end regardless.
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

    /// Materialize the file (idempotent): create the directory and an
    /// empty file. The header is an ordinary queued line, so the drain's
    /// blob writes it — there is no header special case here. A previous
    /// materialize that died mid-blob may have left a partial file the
    /// `create_new` open refuses to replace: remove the orphan so this
    /// attempt (and the next) can proceed — the outbox still holds every
    /// line, so nothing is lost.
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
                Err(SessionError::Io {
                    path: self.path.clone(),
                    source,
                })
            }
        }
    }
}

/// Truncate `path` to `len` through a separate write handle (Windows:
/// append-mode handles cannot `set_len`).
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
#[path = "writer_tests.rs"]
mod tests;
