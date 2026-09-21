//! Environment pre-check evaluator — a faithful port of pi-sanity's
//! `pre-check.ts`.
//!
//! Syntax for the `match` pattern:
//! - `"pattern" | ":pattern"`  → exact match (colon optional, stripped)
//! - `"!pattern"`              → exact `"!pattern"` (literal — no colon!)
//! - `"!:pattern"`             → NOT pattern (negated exact — has colon!)
//! - `"::pattern"`             → exact `":pattern"` (second colon is part
//!   of the pattern)
//! - `"glob:pattern"`          → glob match
//! - `"!glob:pattern"`         → NOT glob match
//! - `"re:pattern"`            → regex match
//! - `"!re:pattern"`           → NOT regex match
//!
//! RULE: Colon is REQUIRED for prefix parsing. No colon = positive
//! exact. Misspelled types (`"typo:pattern"`) fall back to exact match
//! of the literal string — safe but potentially confusing.

use crate::path_utils::normalize_separators;
use crate::types::Action;

/// The outcome of one pre-check condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreCheckResult {
    pub action: Action,
    pub reason: Option<String>,
    pub matched: bool,
}

/// The parsed shape of a match pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternKind {
    Exact,
    Glob,
    Regex,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedPattern {
    pub kind: PatternKind,
    pub negated: bool,
    pub pattern: String,
}

/// Parse a match pattern (TS `parseMatchPattern`).
///
/// Priority:
/// 1. No colon at all → positive exact, literal (including any `!`).
/// 2. `glob:` / `re:` after an optional `!` → typed.
/// 3. Otherwise, an optional leading `!` is negation and the (optional)
///    leading `:` is stripped for the exact pattern.
pub fn parse_match_pattern(raw_pattern: &str) -> ParsedPattern {
    // Empty pattern.
    if raw_pattern.is_empty() {
        return ParsedPattern {
            kind: PatternKind::Exact,
            negated: false,
            pattern: String::new(),
        };
    }

    // Try to match the prefix pattern: MUST have a colon. Types are
    // only recognized verbatim; anything else before the colon (a
    // typo) falls back to exact match of the literal.
    let after_bang = raw_pattern.strip_prefix('!');
    let negated = after_bang.is_some();
    let after_bang = after_bang.unwrap_or(raw_pattern);

    for (prefix, kind) in [("glob:", PatternKind::Glob), ("re:", PatternKind::Regex)] {
        if let Some(content) = after_bang.strip_prefix(prefix) {
            return ParsedPattern {
                kind,
                negated,
                pattern: content.to_string(),
            };
        }
    }

    // Has colon but no type: exact match (negated if `!` present).
    // "!:" → negated exact with empty pattern; ":foo" → exact "foo".
    if let Some(content) = after_bang.strip_prefix(':') {
        return ParsedPattern {
            kind: PatternKind::Exact,
            negated,
            pattern: content.to_string(),
        };
    }

    // NO COLON — positive exact match, literal string. This includes
    // "!prod" (literal), "glob" (literal), "re" (literal).
    ParsedPattern {
        kind: PatternKind::Exact,
        negated: false,
        pattern: raw_pattern.to_string(),
    }
}

/// Check if an env value matches the pattern (TS `matchesPattern`).
pub fn matches_pattern(value: &str, pattern: &str) -> bool {
    let parsed = parse_match_pattern(pattern);
    let matches = match parsed.kind {
        PatternKind::Exact => value == parsed.pattern,
        PatternKind::Glob => {
            let normalized_value = normalize_separators(value);
            let normalized_pattern = normalize_separators(&parsed.pattern);
            // TS calls Node's `path.matchesGlob` here — no nocase
            // option, so case-sensitive even on win32.
            crate::path_permission::matches_glob(
                &normalized_value,
                &normalized_pattern,
                crate::path_utils::Platform::Other,
            )
        }
        PatternKind::Regex => regex::Regex::new(&parsed.pattern)
            // TS compiles `new RegExp(pattern)` eagerly, which throws
            // on invalid patterns; here an invalid config regex fails
            // gracefully as a non-match (external error, never a
            // crash).
            .is_ok_and(|re| re.is_match(value)),
    };

    if parsed.negated { !matches } else { matches }
}

/// Evaluate a single pre-check condition (TS `evaluatePreCheck`).
/// An unset env value is treated as the empty string.
pub fn evaluate_pre_check(
    _env_name: &str,
    match_pattern: &str,
    env_value: Option<&str>,
    action: Action,
    reason: Option<String>,
) -> PreCheckResult {
    let value = env_value.unwrap_or("");
    let matched = matches_pattern(value, match_pattern);
    PreCheckResult {
        matched,
        action,
        reason,
    }
}

/// The strictest action across matching checks, with the matching
/// checks' reasons in order (TS `evaluatePreChecks`).
pub fn evaluate_pre_checks(checks: &[crate::config::PreCheck]) -> Option<(Action, Vec<String>)> {
    if checks.is_empty() {
        return None;
    }

    let mut matching_results: Vec<PreCheckResult> = vec![];

    for check in checks {
        let env_value = std::env::var(&check.env).ok();
        let result = evaluate_pre_check(
            &check.env,
            &check.match_,
            env_value.as_deref(),
            check.action,
            check.reason.clone(),
        );

        if result.matched {
            matching_results.push(result);
        }
    }

    if matching_results.is_empty() {
        return None;
    }

    let mut strictest_action = matching_results[0].action;
    let mut reasons: Vec<String> = vec![];

    for result in &matching_results {
        strictest_action = Action::stricter(strictest_action, result.action);
        if let Some(reason) = &result.reason {
            reasons.push(reason.clone());
        }
    }

    Some((strictest_action, reasons))
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

    fn kind(pattern: &str) -> (PatternKind, bool, String) {
        let parsed = parse_match_pattern(pattern);
        (parsed.kind, parsed.negated, parsed.pattern)
    }

    #[test]
    fn parse_match_pattern_cases() {
        assert_eq!(kind("root"), (PatternKind::Exact, false, "root".into()));
        assert_eq!(
            kind("glob:**/project/*"),
            (PatternKind::Glob, false, "**/project/*".into())
        );
        assert_eq!(
            kind("re:^/dev"),
            (PatternKind::Regex, false, "^/dev".into())
        );
        assert_eq!(kind(":root"), (PatternKind::Exact, false, "root".into()));
        assert_eq!(kind("::root"), (PatternKind::Exact, false, ":root".into()));
        assert_eq!(kind("!root"), (PatternKind::Exact, false, "!root".into()));
        assert_eq!(kind("!:root"), (PatternKind::Exact, true, "root".into()));
        assert_eq!(
            kind("!glob:*/prod/*"),
            (PatternKind::Glob, true, "*/prod/*".into())
        );
        assert_eq!(
            kind("!re:^/etc"),
            (PatternKind::Regex, true, "^/etc".into())
        );
        assert_eq!(kind(":"), (PatternKind::Exact, false, "".into()));
        assert_eq!(kind("::"), (PatternKind::Exact, false, ":".into()));
        assert_eq!(kind(":::"), (PatternKind::Exact, false, "::".into()));
        assert_eq!(kind("glob"), (PatternKind::Exact, false, "glob".into()));
        assert_eq!(kind("!glob"), (PatternKind::Exact, false, "!glob".into()));
    }

    #[test]
    fn matches_pattern_cases() {
        assert!(matches_pattern("root", "root"));
        assert!(!matches_pattern("admin", "root"));
        assert!(matches_pattern("", ""));
        assert!(!matches_pattern("value", ""));
        assert!(matches_pattern("root", ":root"));
        assert!(matches_pattern(":root", "::root"));
        assert!(matches_pattern("!root", "!root"));
        assert!(!matches_pattern("root", "!:root"));
        assert!(matches_pattern("admin", "!:root"));
        assert!(matches_pattern("file.txt", "glob:*.txt"));
        assert!(!matches_pattern("file.log", "glob:*.txt"));
        assert!(matches_pattern("/home/user/project/src", "glob:**/src"));
        assert!(!matches_pattern("/home/user/project", "glob:**/src"));
        assert!(matches_pattern("file.txt", "!glob:*.log"));
        assert!(!matches_pattern("file.log", "!glob:*.log"));
        assert!(matches_pattern("test123", "re:^test"));
        assert!(!matches_pattern("mytest", "re:^test"));
        assert!(matches_pattern("/home/user", "!re:^/etc"));
        assert!(!matches_pattern("/etc/passwd", "!re:^/etc"));
        assert!(matches_pattern("!", "!"));
        assert!(!matches_pattern("", "!"));
        assert!(matches_pattern("x", "!:"));
        assert!(!matches_pattern("", "!:"));
    }
}
