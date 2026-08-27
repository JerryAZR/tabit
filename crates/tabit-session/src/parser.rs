//! The one-pass load: a session file's bytes to the resident state.
//!
//! [`parse`] walks the file exactly once and produces everything the
//! session runs on afterwards — the header, the tree (with its head),
//! the active branch's context (folded through the one context builder,
//! tool batches merged), the selection register, and the computed
//! cumulative stats. Raw records are **not** retained: the file stays
//! on disk for the rare need; memory keeps only what it means.
//!
//! Every structural invariant is enforced here, loudly — there is no
//! repair pass and no tail tolerance: a file written only at commit
//! boundaries (atomic roundtrips, one-blob drains) cannot contain a
//! half-open roundtrip or a torn line, so seeing one means corruption
//! or a foreign writer, and the honest answer is a named error, not a
//! guess. A violation anywhere — a torn final line included — fails
//! the open.

use crate::entry::{
    EntryKind, FileRecord, SESSION_FORMAT_VERSION, SessionEntry, SessionHeader, SideKind,
};
use crate::error::SessionError;
use crate::projection;
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
    /// The conversation tree, head included.
    pub tree: SessionTree,
    /// The active branch's model-visible context.
    pub context: rig_agent::agent::context::Context,
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
    // The open tool batch in file order: the assistant's unanswered
    // call ids, drained by its results (or a feedback close).
    let mut pending_calls: Vec<String> = Vec::new();

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
                validate_node_order(&entry, &mut pending_calls).map_err(corrupt)?;
                if let EntryKind::AssistantMessage { usage, .. } = &entry.kind {
                    let (provider, model, level) = &attribution;
                    stats.add(provider, model, level.as_deref(), *usage);
                }
                tree.load_append(entry).map_err(tree_fault)?;
            }
            FileRecord::Side(record) => {
                if !pending_calls.is_empty() {
                    return Err(corrupt(format!(
                        "side record (`{}`) inside the open tool batch at `{}`",
                        record.timestamp,
                        record.kind.kind_name()
                    )));
                }
                match record.kind {
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
                        let branch = tree.path_to(to.as_deref()).map_err(tree_fault)?;
                        projection::path_is_closed(&branch)
                            .map_err(|message| corrupt(format!("checkout target: {message}")))?;
                        tree.move_head(to.as_deref()).map_err(tree_fault)?;
                    }
                    SideKind::Aborted | SideKind::Label { .. } | SideKind::Custom { .. } => {}
                    SideKind::Discarded { usage } => {
                        let (provider, model, level) = &attribution;
                        stats.add(provider, model, level.as_deref(), usage);
                    }
                }
            }
        }
    }
    if !pending_calls.is_empty() {
        return Err(corrupt(
            "the file ends inside an open tool batch — a batch written only at \
             commit boundaries cannot look like this"
                .to_string(),
        ));
    }
    let branch = tree.path_to_head();
    projection::path_is_closed(&branch)
        .map_err(|message| corrupt(format!("head branch: {message}")))?;
    let context = projection::fold_branch(&branch);

    Ok(Parsed {
        header,
        tree,
        context,
        register,
        stats,
        path: path.to_path_buf(),
        file_len: raw.len() as u64,
    })
}

/// The file-order pairing law: an assistant's calls are answered by the
/// immediately following `tool_result` nodes, or by one user message
/// carrying their results (an engine-authored feedback close) — nothing
/// else may intervene.
fn validate_node_order(
    entry: &SessionEntry,
    pending_calls: &mut Vec<String>,
) -> Result<(), String> {
    match &entry.kind {
        EntryKind::UserMessage { message } => {
            if pending_calls.is_empty() {
                return Ok(());
            }
            let answered = projection::results_of(message);
            for call in pending_calls.iter() {
                if !answered.iter().any(|result| result.id == *call) {
                    return Err(format!(
                        "user entry `{}` interrupts the open tool batch (call `{call}` \
                         unanswered)",
                        entry.id
                    ));
                }
            }
            pending_calls.clear();
            Ok(())
        }
        EntryKind::AssistantMessage { message, .. } => {
            if !pending_calls.is_empty() {
                return Err(format!(
                    "assistant entry `{}` follows an unanswered tool batch",
                    entry.id
                ));
            }
            *pending_calls = projection::calls_of(message)
                .iter()
                .map(|call| call.id.clone())
                .collect();
            Ok(())
        }
        EntryKind::ToolResult { result } => {
            let Some(index) = pending_calls.iter().position(|id| *id == result.id) else {
                return Err(format!(
                    "tool result entry `{}` answers no open call",
                    entry.id
                ));
            };
            pending_calls.swap_remove(index);
            Ok(())
        }
    }
}

impl SideKind {
    /// The side kind's tag name, for error messages.
    fn kind_name(&self) -> &'static str {
        match self {
            SideKind::ModelChange { .. } => "model_change",
            SideKind::Checkout { .. } => "checkout",
            SideKind::Aborted => "aborted",
            SideKind::Discarded { .. } => "discarded",
            SideKind::Label { .. } => "label",
            SideKind::Custom { .. } => "custom",
        }
    }
}

#[cfg(test)]
#[path = "parser_tests.rs"]
mod tests;
