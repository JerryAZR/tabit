//! Quoted-path security (ported from pi-sanity
//! `tests/integration/scenarios/quoted-paths.test.ts`): quotes are
//! shell syntax, not path content — `rm "/etc/passwd"` must be treated
//! exactly like `rm /etc/passwd`. Historically, quoted paths resolved
//! against CWD and bypassed checks.

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

#[test]
fn denies_double_quoted_absolute_path() {
    let result = check_bash("rm \"/etc/passwd\"", &config::default_config());
    assert_action(
        &result,
        Action::Deny,
        "double quotes must not hide the absolute path",
    );
}

#[test]
fn denies_single_quoted_absolute_path() {
    let result = check_bash("rm '/etc/passwd'", &config::default_config());
    assert_action(
        &result,
        Action::Deny,
        "single quotes must not hide the absolute path",
    );
}

#[test]
fn denies_quoted_destination_in_cp() {
    let result = check_bash("cp \"file.txt\" \"/etc/evil\"", &config::default_config());
    assert_action(
        &result,
        Action::Deny,
        "the quoted destination is still write-checked",
    );
}

#[test]
fn denies_quoted_redirect_target() {
    let result = check_bash("echo x > \"/etc/passwd\"", &config::default_config());
    assert_action(
        &result,
        Action::Deny,
        "the quoted redirect target is still write-checked",
    );
}

#[test]
fn detects_quoted_flags() {
    let result = check_bash(
        "git push \"--force\" origin main",
        &config::default_config(),
    );
    assert_action(
        &result,
        Action::Ask,
        "a quoted --force is still the force flag",
    );
}

#[test]
fn follows_quoted_cd_targets() {
    let result = check_bash("cd \"/etc\" && rm conf", &config::default_config());
    assert_action(&result, Action::Deny, "a quoted cd target is still tracked");
}

#[test]
fn still_allows_quoted_relative_paths_in_cwd() {
    let result = check_bash("rm \"my notes.txt\"", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "a quoted CWD path is still a CWD path",
    );
}

#[test]
fn denies_windows_style_quoted_system_path() {
    let result = check_bash(
        "rm \"C:/Windows/system32/config\"",
        &config::default_config(),
    );
    assert_action(
        &result,
        Action::Deny,
        "a quoted drive path is still the system directory",
    );
}
mod common;
