//! The one-pass load: a session file's bytes to the resident state.
//!
//! [`parse`] walks the file exactly once and produces everything the
//! session runs on afterwards — the header, the tree (with its head),
//! the active branch's context (folded through the one context builder,
//! tool batches merged), the selection register, and the computed
//! cumulative stats. Raw records are **not** retained: the file stays
//! on disk for the rare need; memory keeps only what it means.
//!
//! Structural integrity is enforced loudly, under a stated threat
//! model. **Detected, and fatal at open:** a torn or unparseable line,
//! an unsupported format version, a dangling parent, an unknown
//! checkout target, and a tail left inside an open tool batch — the
//! realistic shape of a torn write, because the writer flushes a
//! roundtrip as one blob, so only the tail can tear mid-batch (the
//! check walks back from the last node over one batch's span, never
//! the file). **Trusted, by policy:** mid-file records that remain
//! valid JSON with valid parentage — split batches, interleaved side
//! records, edited checkout targets — because no app-level failure
//! produces them (one-blob commits), and below-app damage severe
//! enough to create them breaks the JSON or the parent links first,
//! which IS caught. There is no repair pass: a detected violation
//! fails the open with a named error, not a guess.

use crate::entry::{EntryKind, FileRecord, SESSION_FORMAT_VERSION, SessionHeader, SideKind};
use crate::error::SessionError;
use crate::stats::UsageLedger;
use crate::tree::{SessionTree, TreeFault};
use std::path::{Path, PathBuf};
use tabit_protocol::ModelSelection;

/// What one pass over a session file produced: the resident state,
/// ready to adopt.
#[derive(Debug)]
pub struct Parsed {
    /// The header line.
    pub header: SessionHeader,
    /// The conversation tree, head included. The model-visible context
    /// is NOT parsed or stored — it is derived on every read
    /// (`ContextManager::messages`); a reload and a live run are the
    /// same fold by construction.
    pub tree: SessionTree,
    /// The last `model_change` (the session preference register), when
    /// the file records one.
    pub register: Option<ModelSelection>,
    /// Cumulative token usage (every branch, discarded attempts
    /// included).
    pub stats: UsageLedger,
    /// The file this came from.
    pub path: PathBuf,
    /// The file's length in bytes — the append writer's durable offset.
    pub file_len: u64,
}

/// Read and parse the session file at `path`.
pub fn parse_file(path: &Path) -> Result<Parsed, SessionError> {
    let raw = std::fs::read_to_string(path).map_err(|source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&raw, path)
}

/// Parse session file contents. See the module docs for the contract.
pub fn parse(raw: &str, path: &Path) -> Result<Parsed, SessionError> {
    let corrupt = |message: String| SessionError::Corrupt {
        path: path.to_path_buf(),
        message,
    };
    let tree_fault = |fault: TreeFault| SessionError::Corrupt {
        path: path.to_path_buf(),
        message: fault.0,
    };

    let mut lines = raw.split('\n');
    let header_line = lines.next().unwrap_or("").trim_end();
    let header: SessionHeader =
        serde_json::from_str(header_line).map_err(|source| SessionError::Parse {
            path: path.to_path_buf(),
            line: 1,
            source,
        })?;
    if header.version != SESSION_FORMAT_VERSION {
        return Err(corrupt(format!(
            "unsupported session format version {} (this tabit reads version {})",
            header.version, SESSION_FORMAT_VERSION
        )));
    }

    let mut tree = SessionTree::empty();
    let mut register: Option<ModelSelection> = None;
    let mut stats = UsageLedger::new();
    // The model usage attributes to (empty ids before any change —
    // uncosted), mirroring the record stream's own sequence.
    let mut attribution = (String::new(), String::new(), None);
    // The last node the file appended — the tail check's tip (a torn
    // write shows as its batch left open).
    let mut last_node: Option<String> = None;

    for (offset, line) in lines.enumerate() {
        let line = line.trim_end_matches(['\r']);
        if line.is_empty() {
            continue;
        }
        let record: FileRecord =
            serde_json::from_str(line).map_err(|source| SessionError::Parse {
                path: path.to_path_buf(),
                line: offset + 2,
                source,
            })?;
        match record {
            FileRecord::Node(entry) => {
                if let EntryKind::AssistantMessage { usage, .. } = &entry.kind {
                    let (provider, model, level) = &attribution;
                    stats.add(provider, model, level.as_deref(), *usage);
                }
                last_node = Some(entry.id.clone());
                tree.load_append(entry).map_err(tree_fault)?;
            }
            FileRecord::Side(record) => match record.kind {
                SideKind::ModelChange {
                    provider,
                    model,
                    thinking_level,
                } => {
                    attribution = (provider.clone(), model.clone(), thinking_level.clone());
                    register = Some(ModelSelection {
                        provider,
                        model,
                        thinking_level,
                    });
                }
                SideKind::Checkout { to } => {
                    if let Some(to) = &to
                        && !tree.contains(to)
                    {
                        return Err(corrupt(format!(
                            "checkout targets unknown or later node `{to}`"
                        )));
                    }
                    tree.move_head(to.as_deref()).map_err(tree_fault)?;
                }
                SideKind::Aborted | SideKind::Label { .. } | SideKind::Custom { .. } => {}
                SideKind::Discarded { usage } => {
                    let (provider, model, level) = &attribution;
                    stats.add(provider, model, level.as_deref(), usage);
                }
            },
        }
    }
    // The tail check — the load's one pairing validation (see the
    // module docs' threat model): a torn write left the trailing batch
    // open. Bounded: one walk back over the last batch's span.
    if let Some(tip) = last_node {
        let tail = tree.path_to(Some(&tip)).map_err(tree_fault)?;
        tabit_log::tail_is_closed(&tail).map_err(corrupt)?;
    }

    Ok(Parsed {
        header,
        tree,
        register,
        stats,
        path: path.to_path_buf(),
        file_len: raw.len() as u64,
    })
}

#[cfg(test)]
#[path = "parser_tests.rs"]
mod tests;
