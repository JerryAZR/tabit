//! Shared tool-output truncation: one limit kind — bytes, whole lines
//! only — and two mechanisms with their own dials, one per tool family
//! (owner ruling 2026-09: read and bash are *different* mechanisms, and
//! "lines" mean nothing to a model — a newline is just another byte, so
//! there is no line cap):
//!
//! - [`truncate_head`] — read: the beginning of the file plus a
//!   continuation offset; the rest is paged, never spilled. 50 KiB —
//!   a read's budget is rarely wasted, so it can be generous.
//! - [`truncate_head_tail`] — bash: the first lines *and* the last
//!   lines with the middle omitted; the rest is unrecoverable without
//!   re-running, so the caller spills the full output to a file. 16 KiB —
//!   legitimate command output past that is mostly noise (owner ruling).
//!
//! One policy site for every tool; pi's `truncate.ts` was the
//! reference for the limits, not the mechanisms. Image results (when
//! they arrive) bypass this module — it is text-only by design.

/// Byte limit for one `read` result (~12k tokens at 4 bytes/token). The
/// caps are dials, not doctrine: raising them as contexts grow is
/// sanctioned (owner ruling).
pub(crate) const READ_MAX_BYTES: usize = 50 * 1024;

/// Byte limit for one shell result (~4k tokens): command output is
/// mostly noise past this, and the full text survives in the spill
/// file, so the visible window can be tighter than read's.
pub(crate) const SHELL_MAX_BYTES: usize = 16 * 1024;

#[derive(Debug)]
pub(crate) struct Truncation {
    /// The truncated content (whole lines, except where a single line
    /// alone exceeds the budget — see the flags).
    pub(crate) content: String,
    pub(crate) truncated: bool,
    pub(crate) total_lines: usize,
    pub(crate) output_lines: usize,
    pub(crate) total_bytes: usize,
    pub(crate) output_bytes: usize,
    /// Head policy only: the first line alone exceeds the budget (e.g.
    /// a minified file) — no whole line fits, the caller points at a
    /// shell fallback.
    pub(crate) first_line_exceeds_limit: bool,
    /// Head-tail policy only: the output is one line over the budget
    /// and both ends of the returned content are partial cuts of that
    /// line, made on char boundaries.
    pub(crate) single_line_split: bool,
}

/// Split into lines the way the counters see them: a trailing newline
/// does not open a phantom empty line, and empty content is no lines.
pub(crate) fn split_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn fits(content: &str, max_bytes: usize) -> Option<Truncation> {
    let total_lines = split_lines(content).len();
    let total_bytes = content.len();
    (total_bytes <= max_bytes).then(|| Truncation {
        content: content.to_string(),
        truncated: false,
        total_lines,
        output_lines: total_lines,
        total_bytes,
        output_bytes: total_bytes,
        first_line_exceeds_limit: false,
        single_line_split: false,
    })
}

/// Keep the first whole lines that fit the byte budget — the read
/// policy: the beginning of a file plus a continuation offset is the
/// recoverable paging unit.
pub(crate) fn truncate_head(content: &str) -> Truncation {
    if let Some(fit) = fits(content, READ_MAX_BYTES) {
        return fit;
    }
    let lines = split_lines(content);

    // A single line over the byte budget (minified files): no whole line
    // fits; report it so the caller can point at a shell fallback.
    if lines.first().is_some_and(|l| l.len() > READ_MAX_BYTES) {
        return Truncation {
            content: String::new(),
            truncated: true,
            total_lines: lines.len(),
            output_lines: 0,
            total_bytes: content.len(),
            output_bytes: 0,
            first_line_exceeds_limit: true,
            single_line_split: false,
        };
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let line_bytes = line.len() + usize::from(i > 0); // +1 newline
        if bytes + line_bytes > READ_MAX_BYTES {
            break;
        }
        kept.push(line);
        bytes += line_bytes;
    }
    let total_bytes = content.len();
    let content = kept.join("\n");
    Truncation {
        output_bytes: content.len(),
        content,
        truncated: true,
        total_lines: lines.len(),
        output_lines: kept.len(),
        total_bytes,
        first_line_exceeds_limit: false,
        single_line_split: false,
    }
}

/// Keep the first *and* last whole lines, each up to half the byte
/// budget, with the middle omitted — the bash policy: a command's
/// output starts with context and ends with results, and both ends
/// beat a one-ended window. The marker between the halves names the
/// omitted span; the caller attaches the spill path for the full
/// output.
pub(crate) fn truncate_head_tail(content: &str) -> Truncation {
    if let Some(fit) = fits(content, SHELL_MAX_BYTES) {
        return fit;
    }
    let lines = split_lines(content);
    let total_lines = lines.len();
    let total_bytes = content.len();
    let half = SHELL_MAX_BYTES / 2;

    // One line over the whole budget (minified output): both halves are
    // cuts of that line, on char boundaries.
    if let Some(line) = lines.first().filter(|l| l.len() > SHELL_MAX_BYTES) {
        let head_end = floor_char_boundary_from_start(line, half);
        let tail_start = ceil_char_boundary_from_end(line, half);
        let split = format!(
            "{}\n\n[... middle of the line omitted ...]\n\n{}",
            &line[..head_end],
            &line[tail_start..]
        );
        return Truncation {
            output_bytes: split.len(),
            content: split,
            truncated: true,
            total_lines,
            output_lines: 1,
            total_bytes,
            first_line_exceeds_limit: false,
            single_line_split: true,
        };
    }

    let mut head: Vec<&str> = Vec::new();
    let mut head_bytes = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let line_bytes = line.len() + usize::from(i > 0);
        if head_bytes + line_bytes > half {
            break;
        }
        head.push(line);
        head_bytes += line_bytes;
    }

    let mut tail: Vec<&str> = Vec::new(); // reversed; fixed below
    let mut tail_bytes = 0usize;
    for line in lines.iter().skip(head.len()).rev() {
        let line_bytes = line.len() + usize::from(!tail.is_empty());
        if tail_bytes + line_bytes > half {
            break;
        }
        tail.push(line);
        tail_bytes += line_bytes;
    }
    tail.reverse();

    let omitted = total_lines - head.len() - tail.len();
    let mut out = head.join("\n");
    out.push_str(&format!("\n\n[... {omitted} lines omitted ...]\n\n"));
    out.push_str(&tail.join("\n"));
    Truncation {
        output_bytes: out.len(),
        output_lines: head.len() + tail.len(),
        content: out,
        truncated: true,
        total_lines,
        total_bytes,
        first_line_exceeds_limit: false,
        single_line_split: false,
    }
}

/// The largest index <= `budget` that ends on a char boundary, so a
/// head cut never splits a multi-byte character.
fn floor_char_boundary_from_start(text: &str, budget: usize) -> usize {
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// The smallest index >= `text.len() - budget` that starts on a char
/// boundary, so a tail cut never splits a multi-byte character.
fn ceil_char_boundary_from_end(text: &str, budget: usize) -> usize {
    let mut start = text.len().saturating_sub(budget);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_content_passes_through() {
        let t = truncate_head("one\ntwo\n");
        assert!(!t.truncated);
        assert_eq!(t.content, "one\ntwo\n");
        let t = truncate_head_tail("one\ntwo\n");
        assert!(!t.truncated);
        assert_eq!(t.content, "one\ntwo\n");
    }

    #[test]
    fn there_is_no_line_cap() {
        // 3,000 tiny lines (~14 KiB): under both byte caps, far over
        // the old 2000-line dial — the line count must not truncate.
        let many: String = (0..3_000)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let t = truncate_head(&many);
        assert!(!t.truncated, "bytes are the only cap");
        let t = truncate_head_tail(&many);
        assert!(!t.truncated);
    }

    #[test]
    fn head_stops_on_whole_lines() {
        // Few lines, huge bytes: whole lines only, from the top.
        let lines: Vec<String> = (0..10)
            .map(|i| "x".repeat(READ_MAX_BYTES / 4) + &i.to_string())
            .collect();
        let content = lines.join("\n");
        let t = truncate_head(&content);
        assert!(t.truncated);
        assert_eq!(t.output_lines, 3, "three whole lines fit the budget");
        assert!(content.starts_with(&t.content));
        assert!(!t.content.ends_with('\n'));
    }

    #[test]
    fn head_flags_a_first_line_over_the_budget() {
        let minified = "x".repeat(READ_MAX_BYTES + 1);
        let t = truncate_head(&minified);
        assert!(t.first_line_exceeds_limit);
        assert!(t.content.is_empty());
    }

    #[test]
    fn head_tail_keeps_both_ends_and_marks_the_omission() {
        let lines: Vec<String> = (0..10)
            .map(|i| "x".repeat(SHELL_MAX_BYTES / 6) + &format!("row-{i}"))
            .collect();
        let content = lines.join("\n");
        let t = truncate_head_tail(&content);
        assert!(t.truncated);
        assert!(t.content.starts_with(&lines[0]), "head kept");
        assert!(t.content.ends_with("row-9"), "tail kept");
        assert!(
            t.content.contains("lines omitted"),
            "the omission is marked: {}",
            &t.content[t.content.len() / 2..t.content.len() / 2 + 60]
        );
        // Whole lines at both ends.
        assert!(t.content.contains("row-0"));
        assert!(!t.content.contains("row-5\n"), "the middle is gone");
    }

    #[test]
    fn head_tail_splits_one_huge_line_on_char_boundaries() {
        let mut huge = "é".repeat(SHELL_MAX_BYTES); // 2-byte chars: over budget
        huge.push('x');
        let t = truncate_head_tail(&huge);
        assert!(t.truncated);
        assert!(t.single_line_split);
        assert!(t.content.starts_with('é'));
        assert!(t.content.ends_with('x'));
        assert!(t.content.contains("middle of the line omitted"));
        assert!(
            t.content
                .chars()
                .all(|c| c == 'é' || c == 'x' || c.is_ascii())
        );
    }

    #[test]
    fn head_tail_backs_the_head_cut_off_a_mid_char_budget() {
        // 3-byte chars: the half budget (8192) lands mid-character, so
        // the head cut must walk back to a boundary — no replacement
        // chars at either cut.
        let huge = "€".repeat(SHELL_MAX_BYTES);
        let t = truncate_head_tail(&huge);
        assert!(t.truncated && t.single_line_split);
        assert!(
            t.content.chars().all(|c| c == '€' || c.is_ascii()),
            "both cuts land on char boundaries"
        );
    }

    #[test]
    fn trailing_newline_does_not_open_a_phantom_line() {
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines("a\nb"), vec!["a", "b"]);
        assert_eq!(split_lines(""), Vec::<&str>::new());
    }

    #[test]
    fn the_caps_are_dials_not_doctrine() {
        // 50/16 KiB today; growth is sanctioned (owner ruling).
        assert_eq!(READ_MAX_BYTES, 50 * 1024);
        assert_eq!(SHELL_MAX_BYTES, 16 * 1024);
    }
}
