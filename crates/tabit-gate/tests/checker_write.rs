//! The write checker over the shipped default config (ported from
//! pi-sanity `tests/integration/checker/write.test.ts`): HOME asks,
//! CWD and TMPDIR allow, git protection asks, system directories deny,
//! and POSIX-/tmp handling.

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

use common::{assert_action, home, tmpdir};
use tabit_gate::checker_write::check_write;
use tabit_gate::config;
use tabit_gate::types::Action;

// --- HOME directory -------------------------------------------------------

#[test]
fn asks_for_regular_files_in_home() {
    let path = format!("{}/documents/file.txt", home());
    let result = check_write(&path, &config::default_config());
    assert_action(&result, Action::Ask, "HOME writes need confirmation");
}

#[test]
fn asks_for_hidden_files_in_home() {
    let path = format!("{}/.bashrc", home());
    let result = check_write(&path, &config::default_config());
    assert_action(
        &result,
        Action::Ask,
        "hidden HOME writes need confirmation too",
    );
}

// --- CWD (allowed) --------------------------------------------------------

#[test]
fn allows_regular_files_in_cwd() {
    let result = check_write("file.txt", &config::default_config());
    assert_action(&result, Action::Allow, "CWD writes are allowed");
}

#[test]
fn allows_hidden_files_in_cwd() {
    let result = check_write(".env", &config::default_config());
    assert_action(&result, Action::Allow, "hidden CWD writes are allowed");
}

#[test]
fn allows_gitignore_in_cwd() {
    let result = check_write(".gitignore", &config::default_config());
    assert_action(&result, Action::Allow, "hidden CWD writes are allowed");
}

// --- TMPDIR (allowed) -----------------------------------------------------

#[test]
fn allows_regular_files_in_tmpdir() {
    let path = format!("{}/file.txt", tmpdir());
    let result = check_write(&path, &config::default_config());
    assert_action(&result, Action::Allow, "TMPDIR writes are allowed");
}

#[test]
fn allows_hidden_files_in_tmpdir() {
    let path = format!("{}/.hidden", tmpdir());
    let result = check_write(&path, &config::default_config());
    assert_action(&result, Action::Allow, "hidden TMPDIR writes are allowed");
}

#[test]
fn allows_posix_style_tmp_paths() {
    // Git Bash maps /tmp to TMPDIR on Windows; it is the real temp dir
    // elsewhere.
    let result = check_write("/tmp/output.log", &config::default_config());
    assert_action(&result, Action::Allow, "/tmp writes are allowed");
}

#[test]
fn denies_traversal_out_of_tmp() {
    let result = check_write("/tmp/../etc/passwd", &config::default_config());
    assert_action(&result, Action::Deny, "traversal out of /tmp lands in /etc");
}

// --- git protection -------------------------------------------------------

#[test]
fn asks_for_git_config() {
    let result = check_write(".git/config", &config::default_config());
    assert_action(&result, Action::Ask, ".git internals are protected");
}

#[test]
fn asks_for_git_head() {
    let result = check_write(".git/HEAD", &config::default_config());
    assert_action(&result, Action::Ask, ".git internals are protected");
}

// --- system directories (deny) --------------------------------------------

#[test]
fn denies_etc_file() {
    let result = check_write("/etc/file", &config::default_config());
    assert_action(&result, Action::Deny, "system directories are denied");
}

#[test]
fn denies_usr_bin_app() {
    let result = check_write("/usr/bin/app", &config::default_config());
    assert_action(&result, Action::Deny, "system directories are denied");
}

#[test]
fn denies_var_log_file() {
    let result = check_write("/var/log/file", &config::default_config());
    assert_action(&result, Action::Deny, "system directories are denied");
}
