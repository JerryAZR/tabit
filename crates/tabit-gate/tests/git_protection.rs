//! Git protection scenarios (ported from pi-sanity
//! `tests/integration/scenarios/git-protection.test.ts`): writes into
//! .git directories ask at any depth, and force-pushing asks.

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

#[test]
fn asks_for_writing_to_git_config() {
    let result = check_bash("echo 'evil' > .git/config", &config::default_config());
    assert_action(&result, Action::Ask, ".git/config is git-protected");
}

#[test]
fn asks_for_writing_to_git_head() {
    let result = check_bash("cp file.txt .git/HEAD", &config::default_config());
    assert_action(&result, Action::Ask, ".git/HEAD is git-protected");
}

#[test]
fn allows_writing_to_normal_files() {
    let result = check_bash("echo 'hello' > file.txt", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "plain CWD files are not git-protected",
    );
}

#[test]
fn asks_for_submodule_git_directories() {
    let result = check_bash(
        "echo 'evil' > libs/submodule/.git/config",
        &config::default_config(),
    );
    assert_action(
        &result,
        Action::Ask,
        "submodule .git directories are protected",
    );
}

#[test]
fn asks_for_nested_git_directories() {
    let result = check_bash(
        "cp file.txt vendor/lib/.git/HEAD",
        &config::default_config(),
    );
    assert_action(
        &result,
        Action::Ask,
        "nested .git directories are protected",
    );
}

// --- force push protection ------------------------------------------------

#[test]
fn asks_for_git_push_force_flag() {
    let result = check_bash("git push --force origin main", &config::default_config());
    assert_action(&result, Action::Ask, "--force rewrites history");
    assert_eq!(
        result.reason.as_deref(),
        Some("Force push rewrites history")
    );
}

#[test]
fn asks_for_git_push_short_force_flag() {
    let result = check_bash("git push -f origin main", &config::default_config());
    assert_action(&result, Action::Ask, "-f rewrites history");
    assert_eq!(
        result.reason.as_deref(),
        Some("Force push rewrites history")
    );
}

#[test]
fn asks_for_git_push_force_with_lease() {
    let result = check_bash(
        "git push --force-with-lease origin main",
        &config::default_config(),
    );
    assert_action(&result, Action::Ask, "--force-with-lease rewrites history");
    assert_eq!(
        result.reason.as_deref(),
        Some("Force push rewrites history")
    );
}

#[test]
fn asks_for_bare_git_push_force_with_lease() {
    let result = check_bash("git push --force-with-lease", &config::default_config());
    assert_action(
        &result,
        Action::Ask,
        "--force-with-lease rewrites history even without refs",
    );
    assert_eq!(
        result.reason.as_deref(),
        Some("Force push rewrites history")
    );
}

#[test]
fn allows_normal_git_push() {
    let result = check_bash("git push origin main", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "a plain push never rewrites history",
    );
}
