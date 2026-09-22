//! Glob matching for path permission patterns — the picomatch
//! replacement (the parser-adjacent dependency substitution this port
//! is allowed, alongside the brush-parser adapter). Matching is the
//! battle-tested [`glob`] crate with the picomatch-equivalent options
//! (`require_literal_separator`, dots matched); braces — which `glob`
//! does not expand — are expanded here first (preprocessing, not
//! matching).
//!
//! One picomatch corner the crate does not have, carried by a small
//! documented adapter: a **trailing `/**` matches the directory
//! itself** (`/test/**` matches `/test`) — load-bearing for rules
//! like `{{CWD}}/**`, where the checked path may be the directory
//! exactly (`rm -rf <the project root>`). Every other pinned corner
//! (leading/middle `**/` zero segments, `*` never crossing `/`,
//! `[abc]` classes, dots matched by wildcards) is the crate's own
//! behavior with these options — verified by the pinned tests below.
//!
//! Extglobs (`@(…)`) are not interpreted — they stay literal, the one
//! known divergence from picomatch (no default or corpus pattern uses
//! them).

/// Match `path` against a preprocessed glob `pattern` (the TS
/// `matchesGlob` body: picomatch with `dot: true`). Every brace
/// alternative is tried; a variant that fails to compile counts as
/// non-matching.
pub(crate) fn matches(path: &str, pattern: &str, case_insensitive: bool) -> bool {
    expand_braces(pattern)
        .iter()
        .any(|variant| matches_variant(path, variant, case_insensitive))
}

/// One brace-free variant through the `glob` crate, plus the
/// trailing-`/**` dir-itself adapter.
fn matches_variant(path: &str, pattern: &str, case_insensitive: bool) -> bool {
    let Ok(compiled) = glob::Pattern::new(pattern) else {
        return false;
    };
    let options = glob::MatchOptions {
        case_sensitive: !case_insensitive,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    if compiled.matches_with(path, options) {
        return true;
    }
    // picomatch: a trailing `/**` also matches the directory itself.
    // The glob crate requires the globstar to consume something, so
    // the dir-exact match is checked here.
    pattern
        .strip_suffix("/**")
        .is_some_and(|dir| path == dir || path == format!("{dir}/"))
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
    /// port; see the module doc. These are the acceptance criteria for
    /// the `glob`-crate substitution — every corner must hold.
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
