//! The pre-check pattern grammar, observed through the bash checker
//! (ported from pi-sanity `tests/unit/bash/pre-check.test.ts`).
//!
//! The source suite calls `parseMatchPattern` / `matchesPattern` /
//! `evaluatePreCheck(s)` directly; those are not part of the frozen
//! API, so every case here drives the same grammar through a command
//! rule carrying `pre_checks` in `check_bash`: the probe variable is
//! set to the input, and a match fires the pre-check's deny while a
//! miss falls through to the rule's allow fallback. The nine
//! `parseMatchPattern` structural assertions (the parsed shape
//! `{ type, negated, pattern }`) test an internal representation with
//! no frozen surface and are not ported; their behavior is fully
//! covered by the match table below.

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

mod common;

use common::{assert_action, config_with_command_rule, pre_check_reason, pre_checks, rule_config};
use tabit_gate::checker_bash::check_bash;
use tabit_gate::config::SanityConfig;
use tabit_gate::types::Action;

/// A test-private probe variable name, unique per `matches` call —
/// concurrent test threads each probe their own variable, so the
/// process-global env is never contended.
fn probe_var() -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("TABIT_GATE_PRECHECK_PROBE_{id}")
}

/// Set the probe to `env_value` and run `check_bash("cmd")` against a
/// rule whose single pre-check matches the probe against `pattern`
/// with a deny action: match -> Deny, miss -> the rule's Allow
/// fallback.
fn matches(env_value: &str, pattern: &str) -> bool {
    let probe = probe_var();
    // SAFETY: a variable name minted for this call alone; no other
    // test can read or write it.
    unsafe { std::env::set_var(&probe, env_value) };
    let mut rule = rule_config();
    rule.pre_checks = vec![pre_check_reason(&probe, pattern, Action::Deny, "matched")];
    let config = config_with_command_rule("cmd", Action::Allow, rule);
    let result = check_bash("cmd", &config);
    // SAFETY: see above.
    unsafe { std::env::remove_var(&probe) };
    result.action == Action::Deny
}

fn assert_match(input: &str, pattern: &str, expect: bool) {
    assert_eq!(
        matches(input, pattern),
        expect,
        "pattern={pattern:?} input={input:?}: expected {expect}"
    );
}

// --- exact matching ---------------------------------------------------------

#[test]
fn exact_pattern_matches_equal_value() {
    assert_match("root", "root", true);
}

#[test]
fn exact_pattern_rejects_different_value() {
    assert_match("admin", "root", false);
}

#[test]
fn empty_pattern_matches_empty_value() {
    assert_match("", "", true);
}

#[test]
fn empty_pattern_rejects_non_empty_value() {
    assert_match("value", "", false);
}

#[test]
fn optional_colon_prefix_strips_to_exact() {
    assert_match("root", ":root", true);
}

#[test]
fn escaped_double_colon_keeps_literal_colon() {
    assert_match(":root", "::root", true);
}

#[test]
fn leading_bang_without_colon_is_literal() {
    assert_match("!root", "!root", true);
}

#[test]
fn negated_exact_rejects_the_named_value() {
    assert_match("root", "!:root", false);
}

#[test]
fn negated_exact_accepts_other_values() {
    assert_match("admin", "!:root", true);
}

// --- glob matching ------------------------------------------------------------

#[test]
fn glob_star_matches_within_a_segment() {
    assert_match("file.txt", "glob:*.txt", true);
    assert_match("file.log", "glob:*.txt", false);
}

#[test]
fn glob_doublestar_matches_across_segments() {
    assert_match("/home/user/project/src", "glob:**/src", true);
    assert_match("/home/user/project", "glob:**/src", false);
}

#[test]
fn negated_glob_inverts_the_match() {
    assert_match("file.txt", "!glob:*.log", true);
    assert_match("file.log", "!glob:*.log", false);
}

// --- regex matching --------------------------------------------------------------

#[test]
fn regex_anchored_prefix_matches() {
    assert_match("test123", "re:^test", true);
    assert_match("mytest", "re:^test", false);
    assert_match("/dev/sda1", "re:^/dev", true);
    assert_match("/sys/dev", "re:^/dev", false);
}

#[test]
fn negated_regex_inverts_the_match() {
    assert_match("/home/user", "!re:^/etc", true);
    assert_match("/etc/passwd", "!re:^/etc", false);
}

// --- edge cases ---------------------------------------------------------------------

#[test]
fn lone_bang_is_a_literal_exact_pattern() {
    assert_match("!", "!", true);
    assert_match("", "!", false);
}

#[test]
fn negated_empty_pattern_inverts_empty_match() {
    assert_match("x", "!:", true);
    assert_match("", "!:", false);
}

#[test]
fn colon_edge_cases_follow_the_grammar() {
    assert_match("", ":", true);
    assert_match(":", "::", true);
    assert_match("::", ":::", true);
    assert_match("test", ":test", true);
    assert_match(":test", "::test", true);
}

#[test]
fn ambiguous_type_words_without_colon_are_literals() {
    assert_match("glob", "glob", true);
    assert_match("!glob", "!glob", true);
    assert_match("re", "re", true);
    assert_match("!re", "!re", true);
}

// --- evaluatePreChecks: aggregation over multiple checks -----------------------------

#[test]
fn unmatched_checks_leave_the_rule_fallback_in_place() {
    // TS: evaluatePreChecks returns undefined when nothing matches —
    // observable here as the fallback action with no pre-check result.
    const VAR: &str = "TABIT_GATE_PRECHECK_NO_MATCH";
    // SAFETY: test-private variable, no other test reads it.
    unsafe { std::env::set_var(VAR, "admin") };
    let mut rule = rule_config();
    rule.pre_checks = pre_checks(&[(VAR, "root", Action::Deny)]);
    let config = config_with_command_rule("cmd", Action::Allow, rule);
    let result = check_bash("cmd", &config);
    // SAFETY: see above.
    unsafe { std::env::remove_var(VAR) };
    assert_action(
        &result,
        Action::Allow,
        "no matching check means no pre-check result",
    );
    assert_eq!(
        result.reason, None,
        "no matching check contributes no reason"
    );
}

#[test]
fn multiple_matching_checks_take_the_strictest_action() {
    const VAR: &str = "TABIT_GATE_PRECHECK_MULTI";
    // SAFETY: test-private variable, no other test reads it.
    unsafe { std::env::set_var(VAR, "value") };
    let mut rule = rule_config();
    rule.pre_checks = pre_checks(&[
        (VAR, "value", Action::Allow),
        (VAR, "value", Action::Ask),
        (VAR, "value", Action::Deny),
    ]);
    let config = config_with_command_rule("cmd", Action::Allow, rule);
    let result = check_bash("cmd", &config);
    // SAFETY: see above.
    unsafe { std::env::remove_var(VAR) };
    assert_action(
        &result,
        Action::Deny,
        "deny > ask > allow across matching checks",
    );
}

#[test]
fn matching_checks_collect_their_reasons_in_order() {
    let var1 = "TABIT_GATE_PRECHECK_REASONS_1";
    let var2 = "TABIT_GATE_PRECHECK_REASONS_2";
    // SAFETY: test-private variables, no other test reads them.
    unsafe { std::env::set_var(var1, "x") };
    unsafe { std::env::set_var(var2, "y") };
    let mut rule = rule_config();
    rule.pre_checks = vec![
        pre_check_reason(var1, "x", Action::Ask, "Reason 1"),
        pre_check_reason(var2, "y", Action::Ask, "Reason 2"),
    ];
    let config: SanityConfig = config_with_command_rule("cmd", Action::Allow, rule);
    let result = check_bash("cmd", &config);
    // SAFETY: see above.
    unsafe { std::env::remove_var(var1) };
    unsafe { std::env::remove_var(var2) };
    assert_action(&result, Action::Ask, "both checks match with ask");
    assert_eq!(
        result.reason.as_deref(),
        Some("Reason 1; Reason 2"),
        "reasons join with the standard separator"
    );
}
