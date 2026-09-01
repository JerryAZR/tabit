//! File mutation for the text tools: a per-path lock registry and an
//! atomic store. The engine's tool phase is concurrency-bounded by
//! design (ENGINE.md: chains run in call order at `tool_concurrency` 1,
//! bounded-concurrent above it), so two calls mutating one path can
//! genuinely interleave — write and edit serialize through this module.
//! Write paths only: readers never lock (the atomic store means a reader
//! only ever sees a complete old or complete new file), and bash never
//! touches it (its spill files are unique by construction).
//!
//! Tool bodies poll on the engine's sidecar runtime; the locks are
//! async so a contended path parks the body instead of blocking a
//! worker.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// The per-path lock registry. Keyed by the path as given — process-wide,
/// so `a/../b` and `b` would lock separately; in practice paths come from
/// one cwd-relative model and exact-path aliasing is rare enough that a
/// canonicalization layer (which also forces path-exists semantics) is
/// not worth it.
static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> = OnceLock::new();

fn lock_for(path: &Path) -> Arc<AsyncMutex<()>> {
    let registry = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    // The registry lock is a short claim over a HashMap lookup — it never
    // crosses an await, so a std Mutex is the right primitive here.
    let mut map = registry.lock().unwrap_or_else(|poison| poison.into_inner());
    map.entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

/// Serialize a mutation to `path`: existence checks, content compares,
/// and the store itself all happen while the returned guard is alive.
pub(crate) async fn lock(path: &Path) -> OwnedMutexGuard<()> {
    lock_for(path).lock_owned().await
}

/// The outcome of a store, for the tool's result line.
pub(crate) struct StoreOutcome {
    /// Byte length of the replaced content (`None` on creation — its
    /// presence is the overwrite signal).
    pub(crate) previous_len: Option<usize>,
    /// Parent-directory levels this store created (named in the result so
    /// a nesting typo surfaces visually).
    pub(crate) parents_created: usize,
}

/// Synchronous store of already-decided bytes over an existing file:
/// temp-file + rename, no existence policy (the caller — edit — has
/// already established what the file is inside its lock). The plain
/// counterpart of [`store`].
pub(crate) fn store_sync(path: &Path, content: &[u8]) -> Result<(), crate::ToolExecutionError> {
    if std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        atomic_replace(path, content)
    } else {
        std::fs::write(path, content).map_err(|e| {
            crate::ToolExecutionError::other(format!("cannot write `{}`: {e}", path.display()))
        })
    }
}

/// Store `content` at `path`, creating parents as needed. Non-atomic in
/// the filesystem sense only for one case: this is a plain create when
/// the path does not exist; when it does, the bytes go through a
/// temp-file + rename (`NamedTempFile::persist` — retry-over-delete on
/// Windows) so readers never see a torn file. Callers must hold
/// [`lock`] — existence is observed inside their critical section.
pub(crate) async fn store(
    path: &Path,
    content: &[u8],
) -> Result<StoreOutcome, crate::ToolExecutionError> {
    use crate::ToolExecutionError;

    let previous = std::fs::metadata(path).ok().filter(|m| m.is_file());
    let previous_len = previous.map(|m| m.len() as usize);

    let mut parents_created = 0usize;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        match std::fs::metadata(parent) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(ToolExecutionError::other(format!(
                    "cannot write `{}`: `{}` exists and is not a directory",
                    path.display(),
                    parent.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                parents_created = count_missing_ancestors(parent);
                std::fs::create_dir_all(parent).map_err(|e| {
                    ToolExecutionError::other(format!(
                        "cannot create directories for `{}`: {e}",
                        path.display()
                    ))
                })?;
            }
            Err(e) => {
                return Err(ToolExecutionError::other(format!(
                    "cannot access `{}`: {e}",
                    parent.display()
                )));
            }
        }
    }

    if previous_len.is_some() {
        atomic_replace(path, content)?;
    } else {
        std::fs::write(path, content).map_err(|e| {
            ToolExecutionError::other(format!("cannot write `{}`: {e}", path.display()))
        })?;
    }
    Ok(StoreOutcome {
        previous_len,
        parents_created,
    })
}

/// Temp-file + rename over an existing file: same-directory temp (same
/// volume, so the rename is atomic), then `persist` — tempfile's
/// battle-tested retry-over-delete path for Windows, where an open
/// rename target cannot be replaced.
fn atomic_replace(path: &Path, content: &[u8]) -> Result<(), crate::ToolExecutionError> {
    use crate::ToolExecutionError;
    use std::io::Write as _;

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| {
        ToolExecutionError::other(format!("cannot stage `{}`: {e}", path.display()))
    })?;
    tmp.write_all(content).map_err(|e| {
        ToolExecutionError::other(format!("cannot stage `{}`: {e}", path.display()))
    })?;
    tmp.persist(path).map(|_| ()).map_err(|e| {
        ToolExecutionError::other(format!("cannot replace `{}`: {}", path.display(), e.error))
    })
}

/// Ancestor levels that do not yet exist — walked without touching the
/// filesystem more than once per level.
fn count_missing_ancestors(parent: &Path) -> usize {
    let mut count = 0;
    let mut cur = Some(parent);
    while let Some(dir) = cur {
        if dir.as_os_str().is_empty() || dir.exists() {
            break;
        }
        count += 1;
        cur = dir.parent();
    }
    count
}
