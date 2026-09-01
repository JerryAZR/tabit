//! Shared tool-output truncation: dual limits (lines and bytes, whichever
//! hits first), whole lines only, two policies — [`truncate_head`] (read:
//! the beginning plus a continuation offset) and [`truncate_tail`] (bash:
//! the end, where errors and final results live). One policy site for
//! every tool; pi's `truncate.ts` is the reference. Image results (when
//! they arrive) bypass this module — it is text-only by design.

/// Line limit per tool result. The caps are a dial, not doctrine: raising
/// them to 64/128 KiB as contexts grow is sanctioned (owner ruling).
pub(crate) const MAX_LINES: usize = 2000;
/// Byte limit per tool result (~12k tokens at 4 bytes/token).
pub(crate) const MAX_BYTES: usize = 50 * 1024;

/// Which limit ended the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TruncatedBy {
    Lines,
    Bytes,
}

#[derive(Debug)]
pub(crate) struct Truncation {
    /// The truncated content (whole lines; the tail policy may carry a
    /// partial final line — see `last_line_partial`).
    pub(crate) content: String,
    pub(crate) truncated: bool,
    pub(crate) truncated_by: Option<TruncatedBy>,
    pub(crate) total_lines: usize,
    pub(crate) output_lines: usize,
    pub(crate) total_bytes: usize,
    pub(crate) output_bytes: usize,
    /// Head policy only: the first line alone exceeds [`MAX_BYTES`] (e.g.
    /// a minified file) — no whole line fits, the caller points at a
    /// shell fallback.
    pub(crate) first_line_exceeds_limit: bool,
    /// Tail policy only: the output is one line over [`MAX_BYTES`] and the
    /// returned content is its final [`MAX_BYTES`] bytes, cut on a char
    /// boundary.
    pub(crate) last_line_partial: bool,
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

fn fits(content: &str) -> Option<Truncation> {
    let total_lines = split_lines(content).len();
    let total_bytes = content.len();
    (total_lines <= MAX_LINES && total_bytes <= MAX_BYTES).then(|| Truncation {
        content: content.to_string(),
        truncated: false,
        truncated_by: None,
        total_lines,
        output_lines: total_lines,
        total_bytes,
        output_bytes: total_bytes,
        first_line_exceeds_limit: false,
        last_line_partial: false,
    })
}

/// Keep the first lines/bytes that fit — the read policy: the beginning of
/// a file plus a continuation offset is the recoverable paging unit.
pub(crate) fn truncate_head(content: &str) -> Truncation {
    if let Some(fit) = fits(content) {
        return fit;
    }
    let lines = split_lines(content);
    let total_lines = lines.len();

    // A single line over the byte budget (minified files): no whole line
    // fits; report it so the caller can point at a shell fallback.
    if lines.first().is_some_and(|l| l.len() > MAX_BYTES) {
        return Truncation {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            output_lines: 0,
            output_bytes: 0,
            total_bytes: content.len(),
            first_line_exceeds_limit: true,
            last_line_partial: false,
        };
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    for (i, line) in lines.iter().enumerate().take(MAX_LINES) {
        let line_bytes = line.len() + usize::from(i > 0); // +1 newline
        if bytes + line_bytes > MAX_BYTES {
            truncated_by = TruncatedBy::Bytes;
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
        truncated_by: Some(truncated_by),
        total_lines,
        output_lines: kept.len(),
        total_bytes,
        first_line_exceeds_limit: false,
        last_line_partial: false,
    }
}

/// Keep the last lines/bytes that fit — the bash policy: errors and final
/// results live at the end. One edge: a final line alone over
/// [`MAX_BYTES`] yields its final bytes, cut on a char boundary.
pub(crate) fn truncate_tail(content: &str) -> Truncation {
    if let Some(fit) = fits(content) {
        return fit;
    }
    let lines = split_lines(content);
    let total_lines = lines.len();
    let total_bytes = content.len();

    let mut kept: Vec<&str> = Vec::new(); // reversed; fixed below
    let mut bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;
    for line in lines.iter().rev() {
        if kept.len() >= MAX_LINES {
            break;
        }
        let line_bytes = line.len() + usize::from(!kept.is_empty()); // +1 newline
        if bytes + line_bytes > MAX_BYTES {
            truncated_by = TruncatedBy::Bytes;
            if kept.is_empty() {
                // The final line alone is over the budget: its final
                // MAX_BYTES bytes, on a char boundary.
                let start = floor_char_boundary(line, MAX_BYTES);
                kept.push(&line[start..]);
                last_line_partial = true;
            }
            break;
        }
        kept.push(line);
        bytes += line_bytes;
    }
    kept.reverse();
    let content = kept.join("\n");
    Truncation {
        output_bytes: content.len(),
        content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        output_lines: kept.len(),
        total_bytes,
        first_line_exceeds_limit: false,
        last_line_partial,
    }
}

/// The largest index <= `from_bytes_back` that starts a char, so a slice
/// `[start..]` never cuts a multi-byte character. `from_bytes_back` counts
/// from the end of `text`.
fn floor_char_boundary(text: &str, from_bytes_back: usize) -> usize {
    let mut start = text.len().saturating_sub(from_bytes_back);
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
        assert_eq!(t.output_lines, 2);
        let t = truncate_tail("one\ntwo\n");
        assert!(!t.truncated);
        assert_eq!(t.content, "one\ntwo\n");
    }

    #[test]
    fn head_stops_on_whole_lines() {
        let many: String = (0..MAX_LINES + 500)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let t = truncate_head(&many);
        assert!(t.truncated);
        assert_eq!(t.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(t.output_lines, MAX_LINES);
        assert_eq!(t.total_lines, MAX_LINES + 500);
        // Whole lines only: the output is a prefix of the input lines.
        assert!(many.starts_with(&t.content));
        assert!(!t.content.ends_with('\n'));
    }

    #[test]
    fn head_stops_on_bytes_without_splitting_a_line() {
        // Few lines, huge bytes: the byte limit must win, on a line edge.
        let mut lines: Vec<String> = Vec::new();
        for i in 0..10 {
            lines.push("x".repeat(MAX_BYTES / 8) + &i.to_string());
        }
        let content = lines.join("\n");
        let t = truncate_head(&content);
        assert!(t.truncated);
        assert_eq!(t.truncated_by, Some(TruncatedBy::Bytes));
        assert!(t.output_lines < 10);
        assert!(t.content.lines().all(|l| content.contains(l)));
    }

    #[test]
    fn head_flags_a_first_line_over_the_budget() {
        let minified = "x".repeat(MAX_BYTES + 1);
        let t = truncate_head(&minified);
        assert!(t.first_line_exceeds_limit);
        assert!(t.content.is_empty());
    }

    #[test]
    fn tail_keeps_the_end() {
        let many: String = (0..MAX_LINES + 500)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let t = truncate_tail(&many);
        assert!(t.truncated);
        assert_eq!(t.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(t.output_lines, MAX_LINES);
        assert!(many.ends_with(&t.content));
    }

    #[test]
    fn tail_cuts_one_huge_line_on_a_char_boundary() {
        let mut huge = "é".repeat(MAX_BYTES); // 2-byte chars: over budget
        huge.push('x');
        let t = truncate_tail(&huge);
        assert!(t.truncated);
        assert!(t.last_line_partial);
        assert!(t.output_bytes <= MAX_BYTES);
        // Valid UTF-8 by construction — and the cut kept whole chars.
        assert!(t.content.ends_with('x'));
        assert!(t.content.chars().all(|c| c == 'é' || c == 'x'));
    }

    #[test]
    fn trailing_newline_does_not_open_a_phantom_line() {
        assert_eq!(split_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines("a\nb"), vec!["a", "b"]);
        assert_eq!(split_lines(""), Vec::<&str>::new());
    }

    #[test]
    fn limits_are_the_dial_not_the_doctrine() {
        // 50 KiB today; 64/128 KiB is sanctioned growth (owner ruling).
        assert_eq!(MAX_BYTES, 50 * 1024);
    }
}
