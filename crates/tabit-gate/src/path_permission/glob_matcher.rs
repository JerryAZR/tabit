//! Glob matching for path permission patterns — the picomatch
//! replacement (the parser-adjacent dependency substitution this port
//! is allowed, alongside the brush-parser adapter). Implements the
//! picomatch semantics the gate's patterns rely on, pinned against
//! picomatch itself during the port:
//!
//! - `*` matches any run of non-separator characters (`dot: true`, so
//!   leading dots are ordinary);
//! - `?` matches one non-separator character;
//! - a full `**` segment matches any number of segments: leading
//!   `**/` (zero or more leading segments, e.g. `**/.git/**` matches
//!   `.git/config`), middle `/**/` (zero or more segments, e.g.
//!   `/deep/**/*.js` matches `/deep/test.js`), and trailing `/**`
//!   matches the directory itself too (`/test/**` matches `/test`);
//! - `[abc]` / `[!abc]` character classes;
//! - `{a,b}` brace alternatives (expanded before matching);
//! - `nocase` (case-insensitive) matching for win32.
//!
//! Implemented as a glob-to-regex compilation over the `regex`
//! crate. Extglobs (`@(…)`) are NOT interpreted — they stay literal,
//! the one known divergence from picomatch (no default or corpus
//! pattern uses them).

/// Match `path` against a preprocessed glob `pattern` (the TS
/// `matchesGlob` body: picomatch with `dot: true`). Every brace
/// alternative is tried; a variant that fails to compile counts as
/// non-matching.
pub(crate) fn matches(path: &str, pattern: &str, case_insensitive: bool) -> bool {
    expand_braces(pattern).iter().any(|variant| {
        let anchored = format!("^{}$", glob_to_regex(variant));
        regex::RegexBuilder::new(&anchored)
            .case_insensitive(case_insensitive)
            .build()
            .is_ok_and(|re| re.is_match(path))
    })
}

/// Expand `{a,b}` alternatives into concrete patterns (leftmost,
/// outermost first; nesting supported, no escapes inside braces). A
/// brace group without a comma stays literal.
fn expand_braces(pattern: &str) -> Vec<String> {
    let bytes = pattern.as_bytes();
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    // Find the brace group's closing bracket, tracking nesting.
    let mut depth = 0usize;
    let mut close: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        // No matching brace: the rest is literal.
        return vec![pattern.to_string()];
    };
    let prefix = &pattern[..open];
    let body = &pattern[open + 1..close];
    let suffix = &pattern[close + 1..];

    // Split the body on top-level commas.
    let mut parts: Vec<&str> = vec![];
    let mut part_depth = 0usize;
    let mut start = 0usize;
    for (i, &b) in body.as_bytes().iter().enumerate() {
        match b {
            b'{' => part_depth += 1,
            b'}' => part_depth -= 1,
            b',' if part_depth == 0 => {
                parts.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&body[start..]);
    if parts.len() == 1 {
        // No alternatives: literal brace group.
        return vec![pattern.to_string()];
    }

    let mut out = vec![];
    for part in parts {
        for alternative in expand_braces(part) {
            out.extend(expand_braces(&format!("{prefix}{alternative}{suffix}")));
        }
    }
    out
}

/// Convert one brace-free glob to regex source (unanchored).
fn glob_to_regex(pattern: &str) -> String {
    let segments: Vec<&str> = pattern.split('/').collect();
    match_segments(&segments)
}

/// Build regex source for a slash-delimited segment list. The `/**`
/// globstar folds the separating slash into its optional group so a
/// trailing `/**` matches the directory itself (picomatch behavior).
fn match_segments(segments: &[&str]) -> String {
    match segments {
        [] => String::new(),
        // A lone `**` pattern matches anything.
        ["**"] => "(?:.*)?".to_string(),
        // Leading/middle `**/`: zero or more segments, tolerating the
        // root slash (`**/src` matches `/home/u/p/src` and
        // `**/.git/**` matches `.git/config`).
        ["**", rest @ ..] => format!("(?:/?[^/]+/)*{}", match_segments(rest)),
        [segment, rest @ ..] => {
            let head = segment_to_regex(segment);
            match rest {
                [] => head,
                // `dir/**`: the directory itself and everything under
                // it (`/test/**` matches `/test`).
                ["**"] => format!("{head}(?:/.*)?"),
                _ => format!("{head}/{}", match_segments(rest)),
            }
        }
    }
}

/// Convert one segment (no `/` inside) to regex source.
fn segment_to_regex(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'*' => {
                while i < bytes.len() && bytes[i] == b'*' {
                    i += 1;
                }
                out.push_str("[^/]*");
            }
            b'?' => {
                out.push_str("[^/]");
                i += 1;
            }
            b'[' => match class_to_regex(&segment[i..]) {
                Some((source, consumed)) => {
                    out.push_str(&source);
                    i += consumed;
                }
                None => {
                    out.push_str("\\[");
                    i += 1;
                }
            },
            b'\\' => {
                if i + 1 < bytes.len() {
                    // Escaped character: literal.
                    out.push_str(&regex_escape(&segment[i + 1..i + 2]));
                    i += 2;
                } else {
                    out.push_str("\\\\");
                    i += 1;
                }
            }
            _ => {
                let ch = segment[i..].chars().next().unwrap_or('?');
                out.push_str(&regex_escape(ch.encode_utf8(&mut [0u8; 4])));
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// Compile a `[...]` class starting at `rest[0] == '['`. Returns the
/// regex source and the consumed byte length; `None` when there is no
/// closing bracket (then `[` is literal, as in picomatch).
fn class_to_regex(rest: &str) -> Option<(String, usize)> {
    let bytes = rest.as_bytes();
    let mut i = 1;
    let mut body = String::new();
    if i < bytes.len() && (bytes[i] == b'!' || bytes[i] == b'^') {
        body.push('^');
        i += 1;
    }
    // A leading `]` is a literal member.
    if i < bytes.len() && bytes[i] == b']' {
        body.push_str("\\]");
        i += 1;
    }
    while i < bytes.len() && bytes[i] != b']' {
        // Pass POSIX classes through untouched.
        if bytes[i] == b'['
            && rest[i..].starts_with("[:")
            && let Some(end) = rest[i..].find("]")
        {
            body.push_str(&rest[i..=i + end]);
            i += end + 1;
            continue;
        }
        let ch = rest[i..].chars().next()?;
        body.push_str(&regex_escape_class_member(ch.encode_utf8(&mut [0u8; 4])));
        i += ch.len_utf8();
    }
    if i >= bytes.len() {
        return None;
    }
    Some((format!("[{body}]"), i + 1))
}

/// Escape regex metacharacters in a literal.
fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if matches!(
            ch,
            '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Escape a character-class member; `^` only matters leading (already
/// handled), `-` and `]` are escaped to stay literal.
fn regex_escape_class_member(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if matches!(ch, ']' | '\\' | '^' | '-') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    #![cfg_attr(
        test,
        allow(
            clippy::err_expect,
            clippy::expect_used,
            clippy::indexing_slicing,
            clippy::panic,
            clippy::panic_in_result_fn,
            clippy::unreachable,
            clippy::unwrap_used
        )
    )]

    use super::*;

    /// Behaviors pinned against picomatch (`dot: true`) during the
    /// port; see the module doc.
    #[test]
    fn picomatch_pinned_corners() {
        assert!(matches("/test", "/test/**", false));
        assert!(matches("/test/file.txt", "/test/**", false));
        assert!(matches("/test/a/b", "/test/**", false));
        assert!(matches(".git/config", "**/.git/**", false));
        assert!(matches("a/.git/config", "**/.git/**", false));
        assert!(matches("a/b/.git/config", "**/.git/**", false));
        assert!(matches("readme.txt", "**/*.txt", false));
        assert!(matches("/deep/test.js", "/deep/**/*.js", false));
        assert!(matches("/deep/a/b/c/test.js", "/deep/**/*.js", false));
        assert!(matches("/public/file.txt", "/**", false));
        assert!(matches("/", "/**", false));
        assert!(matches("/dev/null", "/dev/null", false));
        assert!(matches(
            "/c/Users/Jerry/.ssh/id_rsa",
            "/c/Users/Jerry/.ssh/*",
            false
        ));
        assert!(matches(
            "/c/Users/Jerry/.ssh/id_rsa.pub",
            "/c/Users/Jerry/.ssh/*.pub",
            false
        ));
        assert!(matches(
            "/c/Users/Jerry/.aws/credentials",
            "/c/Users/Jerry/**",
            false
        ));
        assert!(matches("/c/Users/Jerry", "/c/Users/Jerry/**", false));
        assert!(matches("/tmp/x/y", "/tmp/**", false));
        assert!(matches("/a/b.txt", "/a/[abc].txt", false));
        assert!(!matches("/a/d.txt", "/a/[abc].txt", false));
        assert!(matches("/project/file1.log", "/project/file?.log", false));
        assert!(!matches("/project/file12.log", "/project/file?.log", false));
        assert!(!matches("/a/b/c.conf", "/a/*.conf", false));
        assert!(matches("/home/user/project/src", "**/src", false));
        assert!(!matches("/home/user/project", "**/src", false));
    }

    #[test]
    fn case_insensitivity_is_opt_in() {
        assert!(matches("/c/a/b", "/c/A/**", true));
        assert!(!matches("/c/a/b", "/c/A/**", false));
        assert!(matches("c:/a/b", "C:/a/**", true));
    }

    #[test]
    fn braces_expand() {
        assert!(matches("a.txt", "{a,b}.txt", false));
        assert!(matches("b.txt", "{a,b}.txt", false));
        assert!(!matches("c.txt", "{a,b}.txt", false));
        // x{1{a,b},2} expands to x1a, x1b, x2.
        assert!(matches("x1a", "x{1{a,b},2}", false), "nested braces");
        assert!(matches("x2", "x{1{a,b},2}", false), "nested braces");
        assert!(!matches("x1", "x{1{a,b},2}", false), "nested braces");
    }
}
