//! The edit tool's presentation cargo: the unified diff of an applied
//! edit call plus per-edit outcomes, serialized as the `details` object
//! that rides `tool_result` (the protocol's derived-presentation rule —
//! `content` stays the model-facing faithful copy; `details` is the
//! same facts, structured, computed once where the file is). Hunk shape
//! mirrors `similar`'s change model (context/removed/added lines with
//! old/new start+count) so the frontend renders through the same crate
//! the ROADMAP already picked for its viewer.

use serde::Serialize;

/// Context lines carried on each side of a change (pi uses 4).
const CONTEXT_LINES: usize = 4;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct EditDetails {
    pub(crate) diff: DiffDetails,
    pub(crate) outcomes: Vec<OutcomeDetails>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct DiffDetails {
    /// 1-indexed first changed line in the new file (`None` when nothing
    /// changed — the all-fail case emits no details).
    pub(crate) first_changed_line: Option<usize>,
    pub(crate) hunks: Vec<Hunk>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct Hunk {
    pub(crate) old_start: usize,
    pub(crate) old_lines: usize,
    pub(crate) new_start: usize,
    pub(crate) new_lines: usize,
    pub(crate) lines: Vec<DiffLine>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct DiffLine {
    pub(crate) kind: LineKind,
    pub(crate) text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LineKind {
    Context,
    Removed,
    Added,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) struct OutcomeDetails {
    /// The edit's index in the call.
    pub(crate) index: usize,
    pub(crate) applied: bool,
    /// Why a rejected edit was rejected (the same string `content`
    /// reports — one production site).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

/// One edit's matching outcome, handed in from the core.
pub(crate) enum Outcome {
    Applied,
    Rejected(String),
}

/// Compute the presentation cargo for one edit call: the unified diff of
/// `before` → `after` (LF space — the same space matching ran in, so
/// line numbers agree with read's paging), plus every edit's outcome.
pub(crate) fn edit_details(before: &str, after: &str, outcomes: Vec<Outcome>) -> serde_json::Value {
    let diff = unified(before, after);
    let outcomes = outcomes
        .into_iter()
        .enumerate()
        .map(|(index, outcome)| match outcome {
            Outcome::Applied => OutcomeDetails {
                index,
                applied: true,
                reason: None,
            },
            Outcome::Rejected(reason) => OutcomeDetails {
                index,
                applied: false,
                reason: Some(reason),
            },
        })
        .collect();
    serde_json::to_value(EditDetails { diff, outcomes })
        .unwrap_or_else(|e| serde_json::json!({ "serialization_error": e.to_string() }))
}

/// The unified diff: hunks of change runs with CONTEXT_LINES of context
/// on each side, adjacent hunks merged when their context would overlap.
fn unified(before: &str, after: &str) -> DiffDetails {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(before, after);
    let mut first_changed_line: Option<usize> = None;
    // (tag, text) runs; newline-stripped.
    let changes: Vec<(ChangeTag, String)> = diff
        .iter_all_changes()
        .map(|c| (c.tag(), c.value().trim_end_matches('\n').to_string()))
        .collect();

    // Indices of changed (non-equal) entries.
    let changed: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, (tag, _))| *tag != ChangeTag::Equal)
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return DiffDetails {
            first_changed_line: None,
            hunks: Vec::new(),
        };
    }

    // Group changed indices into hunk ranges with context, merging ranges
    // whose context windows touch.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &i in &changed {
        let start = i.saturating_sub(CONTEXT_LINES);
        let end = (i + CONTEXT_LINES + 1).min(changes.len());
        match ranges.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => ranges.push((start, end)),
        }
    }

    // Old/new line counters walk the changes; each hunk records its
    // start at the point its range begins.
    let mut hunks = Vec::new();
    let mut old_line = 1usize;
    let mut new_line = 1usize;
    let mut cursor = 0usize;
    for (start, end) in ranges {
        // Advance counters through the gap before this hunk.
        for (tag, _) in changes.get(cursor..start).unwrap_or(&[]) {
            match tag {
                ChangeTag::Equal => {
                    old_line += 1;
                    new_line += 1;
                }
                ChangeTag::Delete => old_line += 1,
                ChangeTag::Insert => new_line += 1,
            }
        }
        let hunk_old_start = old_line;
        let hunk_new_start = new_line;
        let mut lines = Vec::new();
        for (tag, text) in changes.get(start..end).unwrap_or(&[]) {
            match tag {
                ChangeTag::Equal => {
                    lines.push(DiffLine {
                        kind: LineKind::Context,
                        text: text.clone(),
                    });
                    old_line += 1;
                    new_line += 1;
                }
                ChangeTag::Delete => {
                    lines.push(DiffLine {
                        kind: LineKind::Removed,
                        text: text.clone(),
                    });
                    old_line += 1;
                }
                ChangeTag::Insert => {
                    lines.push(DiffLine {
                        kind: LineKind::Added,
                        text: text.clone(),
                    });
                    new_line += 1;
                    if first_changed_line.is_none() {
                        first_changed_line = Some(new_line - 1);
                    }
                }
            }
        }
        hunks.push(Hunk {
            old_start: hunk_old_start,
            old_lines: lines
                .iter()
                .filter(|l| !matches!(l.kind, LineKind::Added))
                .count(),
            new_start: hunk_new_start,
            new_lines: lines
                .iter()
                .filter(|l| !matches!(l.kind, LineKind::Removed))
                .count(),
            lines,
        });
        cursor = end;
    }

    // A deletion-only diff has a first change too: the line after which
    // content vanished (the new-file line the hunk starts at).
    if first_changed_line.is_none() {
        first_changed_line = hunks.first().map(|h| h.new_start);
    }

    DiffDetails {
        first_changed_line,
        hunks,
    }
}
