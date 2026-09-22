//! Special device files (ported from pi-sanity
//! `tests/integration/scenarios/special-files.test.ts`): /dev/null,
//! /dev/stdout, and /dev/stderr are safe write targets; real system
//! writes stay blocked.

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

use common::assert_action;
use tabit_gate::checker_bash::check_bash;
use tabit_gate::config;
use tabit_gate::types::Action;

// --- /dev/null redirections -------------------------------------------------

#[test]
fn allows_stderr_redirection_to_dev_null() {
    let result = check_bash("rm -f test.txt 2>/dev/null", &config::default_config());
    assert_action(&result, Action::Allow, "2>/dev/null is a safe write target");
}

#[test]
fn allows_stdout_redirection_to_dev_null() {
    let result = check_bash("echo 'test' >/dev/null", &config::default_config());
    assert_action(&result, Action::Allow, ">/dev/null is a safe write target");
}

#[test]
fn allows_both_stdout_and_stderr_to_dev_null() {
    let result = check_bash("some_command >/dev/null 2>&1", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "the classic silencing idiom is safe",
    );
}

// --- /dev/stdout and /dev/stderr ----------------------------------------------

#[test]
fn allows_redirect_to_dev_stdout() {
    let result = check_bash("echo test >/dev/stdout", &config::default_config());
    assert_action(&result, Action::Allow, "/dev/stdout is a safe write target");
}

#[test]
fn allows_redirect_to_dev_stderr() {
    let result = check_bash("echo error >/dev/stderr", &config::default_config());
    assert_action(&result, Action::Allow, "/dev/stderr is a safe write target");
}

// --- actual system writes should still be blocked --------------------------------

#[test]
fn denies_write_to_etc() {
    let result = check_bash("echo 'test' >/etc/test_file", &config::default_config());
    assert_action(&result, Action::Deny, "a real system write stays blocked");
}
mod common;
