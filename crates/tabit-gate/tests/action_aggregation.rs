//! Stricter-action aggregation, observed through the bash checker
//! (ported from pi-sanity `tests/unit/action-utils.test.ts`).
//!
//! The source suite calls `stricterAction` / `aggregateResults`
//! directly; those helpers are folded into the checker, so each case
//! here builds a command rule whose declared flags fire known results
//! and asserts the aggregated action and reason. Flags are the cleanest
//! lever: every declared flag present in the command contributes its
//! action (and reason) to the aggregate, in declaration order.

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

use common::{assert_action, config_with_command_rule, flags, flags_with_reason, rule_config};
use tabit_gate::checker_bash::check_bash;
use tabit_gate::config;
use tabit_gate::types::{Action, CheckResult};

/// check_bash over a one-rule config for `cmd` with the given flags.
fn check_with_flags(command: &str, flag_set: Vec<config::CommandFlag>) -> CheckResult {
    let mut rule = rule_config();
    rule.flags = flag_set;
    let cfg = config_with_command_rule("cmd", Action::Allow, rule);
    check_bash(command, &cfg)
}

// --- stricterAction ---------------------------------------------------------

#[test]
fn deny_is_stricter_than_ask_and_allow() {
    // (allow, deny) then (deny, allow): both orders pick deny.
    let result = check_with_flags(
        "cmd -a -b",
        flags_with_reason(&[
            ("-a", Action::Allow, "keep going"),
            ("-b", Action::Deny, "stop"),
        ]),
    );
    assert_action(&result, Action::Deny, "deny wins over allow");
    let result = check_with_flags(
        "cmd -a -b",
        flags(&[("-a", Action::Deny), ("-b", Action::Allow)]),
    );
    assert_action(&result, Action::Deny, "deny wins when it comes first");
    // (ask, deny) then (deny, ask).
    let result = check_with_flags(
        "cmd -a -b",
        flags(&[("-a", Action::Ask), ("-b", Action::Deny)]),
    );
    assert_action(&result, Action::Deny, "deny wins over ask");
    let result = check_with_flags(
        "cmd -a -b",
        flags(&[("-a", Action::Deny), ("-b", Action::Ask)]),
    );
    assert_action(
        &result,
        Action::Deny,
        "deny wins over ask when it comes first",
    );
}

#[test]
fn ask_is_stricter_than_allow() {
    // (allow, ask) then (ask, allow): both orders pick ask.
    let result = check_with_flags(
        "cmd -a -b",
        flags(&[("-a", Action::Allow), ("-b", Action::Ask)]),
    );
    assert_action(&result, Action::Ask, "ask wins over allow");
    let result = check_with_flags(
        "cmd -a -b",
        flags(&[("-a", Action::Ask), ("-b", Action::Allow)]),
    );
    assert_action(
        &result,
        Action::Ask,
        "ask wins over allow when it comes first",
    );
}

#[test]
fn equal_actions_are_kept() {
    let result = check_with_flags("cmd -a", flags(&[("-a", Action::Ask)]));
    assert_action(&result, Action::Ask, "a single ask stays ask");
    let result = check_with_flags("cmd -a", flags(&[("-a", Action::Deny)]));
    assert_action(&result, Action::Deny, "a single deny stays deny");
}

// --- aggregateResults ---------------------------------------------------------

#[test]
fn no_results_fall_back_to_the_configured_action() {
    // TS: aggregateResults([]) is allow — observable through the
    // checker as the fallback when no check fires.
    let result = check_bash("unmatched_command", &config::default_config());
    assert_action(&result, Action::Allow, "default commands allow");
    assert_eq!(result.reason, None);
}

#[test]
fn a_single_result_passes_through() {
    let result = check_with_flags(
        "cmd -a",
        flags_with_reason(&[("-a", Action::Ask, "sensitive")]),
    );
    assert_action(&result, Action::Ask, "the single action passes through");
    assert_eq!(result.reason.as_deref(), Some("sensitive"));
}

#[test]
fn the_strictest_action_wins_across_results() {
    let result = check_with_flags(
        "cmd -a -b -c",
        flags(&[
            ("-a", Action::Allow),
            ("-b", Action::Ask),
            ("-c", Action::Allow),
        ]),
    );
    assert_action(&result, Action::Ask, "the lone ask dominates two allows");
}

#[test]
fn deny_beats_ask_across_results() {
    let result = check_with_flags(
        "cmd -a -b",
        flags(&[("-a", Action::Ask), ("-b", Action::Deny)]),
    );
    assert_action(&result, Action::Deny, "the lone deny dominates the ask");
}

#[test]
fn reasons_join_with_semicolons_in_first_appearance_order() {
    let result = check_with_flags(
        "cmd -a -b",
        flags_with_reason(&[("-a", Action::Ask, "first"), ("-b", Action::Deny, "second")]),
    );
    assert_action(&result, Action::Deny, "deny wins");
    assert_eq!(
        result.reason.as_deref(),
        Some("first; second"),
        "order preserved, '; ' joined"
    );
}

#[test]
fn identical_reasons_deduplicate() {
    let result = check_with_flags(
        "cmd -a -b -c",
        flags_with_reason(&[
            ("-a", Action::Deny, "same reason"),
            ("-b", Action::Deny, "same reason"),
            ("-c", Action::Deny, "same reason"),
        ]),
    );
    assert_action(&result, Action::Deny, "deny wins");
    assert_eq!(
        result.reason.as_deref(),
        Some("same reason"),
        "three identical reasons collapse"
    );
}

#[test]
fn distinct_reasons_keep_first_appearance_and_drop_later_duplicates() {
    let result = check_with_flags(
        "cmd -a -b -c",
        flags_with_reason(&[
            ("-a", Action::Deny, "a"),
            ("-b", Action::Deny, "b"),
            ("-c", Action::Ask, "b"),
        ]),
    );
    assert_action(&result, Action::Deny, "deny wins");
    assert_eq!(
        result.reason.as_deref(),
        Some("a; b"),
        "the later duplicate of 'b' is dropped"
    );
}

#[test]
fn all_allow_results_carry_no_reason() {
    let result = check_with_flags(
        "cmd -a -b",
        flags(&[("-a", Action::Allow), ("-b", Action::Allow)]),
    );
    assert_action(&result, Action::Allow, "allows aggregate to allow");
    assert_eq!(result.reason, None, "no reason means no reason");
}

#[test]
fn a_reason_survives_when_only_one_result_carries_it() {
    // One flag without a reason, one with: the aggregate keeps the reason.
    let mut rule = rule_config();
    rule.flags = vec![
        config::CommandFlag {
            flag: "-a".into(),
            action: Action::Allow,
            reason: None,
        },
        config::CommandFlag {
            flag: "-b".into(),
            action: Action::Allow,
            reason: Some("just checking".into()),
        },
    ];
    let cfg = config_with_command_rule("cmd", Action::Allow, rule);
    let result = check_bash("cmd -a -b", &cfg);
    assert_action(&result, Action::Allow, "allows aggregate to allow");
    assert_eq!(result.reason.as_deref(), Some("just checking"));
}

#[test]
fn undefined_reasons_are_omitted_from_the_join() {
    // Same shape as the source case: one ask with a reason, one without.
    let mut rule = rule_config();
    rule.flags = vec![
        config::CommandFlag {
            flag: "-a".into(),
            action: Action::Ask,
            reason: Some("only".into()),
        },
        config::CommandFlag {
            flag: "-b".into(),
            action: Action::Ask,
            reason: None,
        },
    ];
    let cfg = config_with_command_rule("cmd", Action::Allow, rule);
    let result = check_bash("cmd -a -b", &cfg);
    assert_action(&result, Action::Ask, "ask wins");
    assert_eq!(
        result.reason.as_deref(),
        Some("only"),
        "the reason-less result contributes nothing, not even a separator"
    );
}
