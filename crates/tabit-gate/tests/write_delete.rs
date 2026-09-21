//! Deletion is a write (ported from pi-sanity
//! `tests/integration/checker/delete.test.ts`). That file previously
//! tested `checkDelete`; the delete permission has been merged into
//! write, so deletion is checked against `permissions.write` — these
//! cases pin the same behavior through `check_write`.

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
fn asks_when_deleting_regular_files_in_home() {
    let path = format!("{}/documents/file.txt", home());
    let result = check_write(&path, &config::default_config());
    assert_action(&result, Action::Ask, "deleting in HOME needs confirmation");
}

#[test]
fn asks_when_deleting_hidden_files_in_home() {
    let path = format!("{}/.bashrc", home());
    let result = check_write(&path, &config::default_config());
    assert_action(
        &result,
        Action::Ask,
        "deleting hidden HOME files needs confirmation",
    );
}

// --- CWD (allowed) --------------------------------------------------------

#[test]
fn allows_deleting_regular_files_in_cwd() {
    let result = check_write("file.txt", &config::default_config());
    assert_action(&result, Action::Allow, "deleting in CWD is allowed");
}

#[test]
fn allows_deleting_hidden_files_in_cwd() {
    let result = check_write(".env", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "deleting hidden CWD files is allowed",
    );
}

// --- TMPDIR (allowed) -----------------------------------------------------

#[test]
fn allows_deleting_regular_files_in_tmpdir() {
    let path = format!("{}/file.txt", tmpdir());
    let result = check_write(&path, &config::default_config());
    assert_action(&result, Action::Allow, "deleting in TMPDIR is allowed");
}

#[test]
fn allows_deleting_hidden_files_in_tmpdir() {
    let path = format!("{}/.hidden", tmpdir());
    let result = check_write(&path, &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "deleting hidden TMPDIR files is allowed",
    );
}

// --- git protection -------------------------------------------------------

#[test]
fn asks_when_deleting_git_config() {
    let result = check_write(".git/config", &config::default_config());
    assert_action(
        &result,
        Action::Ask,
        "git internals are protected from deletion",
    );
}

#[test]
fn asks_when_deleting_inside_git_objects() {
    let result = check_write(".git/objects/abc", &config::default_config());
    assert_action(
        &result,
        Action::Ask,
        "git internals are protected from deletion",
    );
}

// --- system directories (deny) --------------------------------------------

#[test]
fn denies_deleting_etc_file() {
    let result = check_write("/etc/file", &config::default_config());
    assert_action(&result, Action::Deny, "system directories deny deletion");
}

#[test]
fn denies_deleting_usr_bin_app() {
    let result = check_write("/usr/bin/app", &config::default_config());
    assert_action(&result, Action::Deny, "system directories deny deletion");
}
