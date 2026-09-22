//! cd tracking through compound commands (ported from pi-sanity
//! `tests/integration/scenarios/cd-tracking.test.ts`): relative paths
//! resolve where they will actually land, not against the process CWD.
//! By-design simplifications: `||` and branches are followed
//! sequentially; subshells, substitutions, pipeline segments,
//! background statements, and coproc bodies are isolated; untrackable
//! cd falls back to the process CWD until an absolute cd re-anchors.

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

use common::assert_action;
use tabit_gate::checker_bash::check_bash;
use tabit_gate::config;
use tabit_gate::types::Action;

// --- sequential cd ---------------------------------------------------------

#[test]
fn denies_relative_write_after_cd_into_restricted_dir() {
    let result = check_bash("cd /etc && rm conf", &config::default_config());
    assert_action(&result, Action::Deny, "conf resolves under /etc");
}

#[test]
fn denies_relative_redirect_after_cd_into_restricted_dir() {
    let result = check_bash("cd /etc && echo x > conf", &config::default_config());
    assert_action(
        &result,
        Action::Deny,
        "the redirect target resolves under /etc",
    );
}

#[test]
fn asks_for_relative_write_after_cd_into_home() {
    let result = check_bash("cd ~ && rm scratch.txt", &config::default_config());
    assert_action(&result, Action::Ask, "scratch.txt resolves under HOME");
}

#[test]
fn resolves_dotdot_against_the_tracked_cwd() {
    let result = check_bash("cd /etc; cd ..; rm rootfile", &config::default_config());
    assert_action(
        &result,
        Action::Deny,
        ".. lands at /, not at the process CWD",
    );
}

#[test]
fn allows_relative_write_after_cd_into_allowed_temp() {
    let result = check_bash("cd /tmp && echo hi > log.txt", &config::default_config());
    assert_action(&result, Action::Allow, "log.txt resolves under /tmp");
}

#[cfg(windows)]
#[test]
fn handles_cd_with_explicit_windows_path() {
    let result = check_bash("cd C:/Windows && rm sysfile", &config::default_config());
    assert_action(
        &result,
        Action::Deny,
        "C:/Windows is a restricted system directory",
    );
}

// --- scope isolation -------------------------------------------------------

#[test]
fn applies_cd_inside_a_subshell_to_inner_commands_but_not_after() {
    let result = check_bash(
        "(cd /etc && rm x); echo done > log.txt",
        &config::default_config(),
    );
    // rm /etc/x is the strictest result: deny. log.txt resolves to CWD.
    assert_action(
        &result,
        Action::Deny,
        "the inner rm is denied; the outer redirect is irrelevant",
    );
}

#[test]
fn isolates_coproc_bodies() {
    let result = check_bash(
        "coproc { cd /etc && rm inner; }; rm outer.txt",
        &config::default_config(),
    );
    assert_action(&result, Action::Deny, "the inner rm resolves under /etc");
    assert!(
        result
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("outside allowed"),
        "deny must come from the path check, not a parse error: {result:?}"
    );
}

#[test]
fn does_not_leak_subshell_cd_to_following_relative_writes() {
    let result = check_bash("(cd /etc); echo done > log.txt", &config::default_config());
    assert_action(&result, Action::Allow, "the subshell cd never leaks out");
}

#[test]
fn applies_cd_inside_command_substitutions_to_inner_commands() {
    let result = check_bash(
        "echo $(cd /etc && rm x) > log.txt",
        &config::default_config(),
    );
    assert_action(
        &result,
        Action::Deny,
        "the substitution's rm resolves under /etc",
    );
}

#[test]
fn flows_cd_through_brace_groups() {
    let result = check_bash("{ cd /etc; rm x; }", &config::default_config());
    assert_action(
        &result,
        Action::Deny,
        "brace groups run in the current shell",
    );
}

// --- cd argument parsing ----------------------------------------------------

#[test]
fn skips_cd_flags_when_finding_the_target() {
    let result = check_bash("cd -L /etc && rm conf", &config::default_config());
    assert_action(&result, Action::Deny, "-L is a flag, /etc is the target");
}

#[test]
fn treats_the_argument_after_dashdash_as_the_target() {
    let result = check_bash("cd -- /etc && rm conf", &config::default_config());
    assert_action(&result, Action::Deny, "-- ends options, /etc is the target");
}

#[test]
fn treats_flag_only_cd_as_bare_cd_going_home() {
    let result = check_bash("cd -L; rm x.txt", &config::default_config());
    assert_action(
        &result,
        Action::Ask,
        "bare cd goes HOME, so x.txt is a HOME write",
    );
}

#[test]
fn treats_single_quoted_cd_targets_literally() {
    // bash does not expand $VAR inside single quotes: the target is a
    // literal directory named $PI_SANITY_CD_TEST_DIR, relative to CWD.
    // The env var is set so an implementation that (wrongly) expands
    // the target would cd into /etc and fail this test.
    const VAR: &str = "PI_SANITY_CD_TEST_DIR";
    // SAFETY: single-threaded mutation of a test-private variable that
    // no other test reads; the workspace tests set env vars the same way.
    unsafe { std::env::set_var(VAR, "/etc") };
    let result = check_bash(
        "cd '$PI_SANITY_CD_TEST_DIR'; rm x.txt",
        &config::default_config(),
    );
    // SAFETY: see above.
    unsafe { std::env::remove_var(VAR) };
    assert_action(
        &result,
        Action::Allow,
        "the quoted target stays a literal relative directory",
    );
}

// --- untrackable cd fallback --------------------------------------------------

#[test]
fn falls_back_to_process_cwd_for_dynamic_cd_targets() {
    let result = check_bash("cd $DIR && rm file.txt", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "untrackable cd falls back to CWD, where writes are allowed",
    );
}

#[test]
fn falls_back_to_process_cwd_for_cd_dash() {
    let result = check_bash("cd -; touch f.txt", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "cd - is untrackable; f.txt resolves to CWD",
    );
}

#[test]
fn reanchors_tracking_on_a_later_absolute_cd() {
    let result = check_bash("cd $DIR; cd /etc; rm conf", &config::default_config());
    assert_action(
        &result,
        Action::Deny,
        "the absolute cd re-anchors tracking at /etc",
    );
}
