//! Glob matching for path permission patterns — the picomatch
//! replacement (the parser-adjacent dependency substitution this port
//! is allowed, alongside the brush-parser adapter). Matching is the
//! battle-tested [`glob`] crate with the picomatch-equivalent options
//! (`require_literal_separator`, dots matched); win32
//! case-insensitivity is the crate's `case_sensitive: false`.
//!
//! Two deliberate divergences from picomatch, both owner-ruled
//! 2026-09 (default behavior over ported extras):
//!
//! - **Brace alternatives (`{a,b}`) are not a feature** — no shipped
//!   rule uses one (the `{{VAR}}` config placeholders expand during
//!   preprocessing, before matching) and an undeclared feature that
//!   silently widens a pattern is worse than a literal that matches
//!   nothing. Braces stay literal. The "one of" construct IS
//!   supported: `[abc]` character classes, the gitignore grammar.
//! - **A trailing `/**` matches everything under the directory, not
//!   the directory itself** (the `glob`/gitignore default; picomatch
//!   also matches the dir). No corpus rule relies on the dir-exact
//!   match, and the flip is fail-safe: writing a directory exactly
//!   (e.g. the project root) falls through to the write default
//!   (deny) instead of the `{{CWD}}/**` allow.
//!
//! Extglobs (`@(…)`) are likewise not interpreted — they stay
//! literal.

/// Match `path` against a preprocessed glob `pattern` (the TS
/// `matchesGlob` body: picomatch with `dot: true`). A pattern that
/// fails to compile counts as non-matching.
pub(crate) fn matches(path: &str, pattern: &str, case_insensitive: bool) -> bool {
    let Ok(compiled) = glob::Pattern::new(pattern) else {
        return false;
    };
    compiled.matches_with(
        path,
        glob::MatchOptions {
            case_sensitive: !case_insensitive,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        },
    )
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
    /// port, minus the two owner-ruled divergences in the module doc
    /// — these are the acceptance criteria for the `glob`-crate
    /// substitution; every remaining corner must hold.
    #[test]
    fn picomatch_pinned_corners() {
        // A trailing /** matches everything under the directory —
        // and, by the ruled divergence, NOT the directory itself.
        assert!(matches("/test/file.txt", "/test/**", false));
        assert!(matches("/test/a/b", "/test/**", false));
        assert!(!matches("/test", "/test/**", false));
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
        assert!(!matches("/c/Users/Jerry", "/c/Users/Jerry/**", false));
        assert!(matches("/tmp/x/y", "/tmp/**", false));
        // Character classes — the "one of" construct.
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
    fn braces_and_extglobs_stay_literal() {
        // The ruled non-features: a `{a,b}` or `@(…)` in a pattern is
        // literal text, matching nothing ordinary.
        assert!(!matches("a.txt", "{a,b}.txt", false));
        assert!(!matches("a.txt", "@(a|b).txt", false));
        // (and a file literally named "{a,b}.txt" does match)
        assert!(matches("{a,b}.txt", "{a,b}.txt", false));
    }
}
