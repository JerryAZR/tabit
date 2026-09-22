//! Windows path representations (ported from pi-sanity
//! `tests/integration/scenarios/windows-paths.test.ts`; win32-only,
//! like the source's `skip: !isWindows`).
//!
//! Design: native drive form (`C:\...`) is the internal standard;
//! git-bash input (`/c/...`) converts at the boundary; matching
//! compares a canonical drive-preserving form (`/c/...`)
//! case-insensitively on win32. Consequences pinned here:
//! `/c/...` and `C:\...` are the SAME path, `/c/...` and `/d/...` are
//! DIFFERENT paths, and case variants are the SAME path.
//!
//! The TypeScript suite passes an explicit `PathContext`; the Rust
//! checkers assemble the same context from the process
//! (cwd, home, tmpdir, actual platform) per the frozen API.

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
#![cfg(windows)]

mod common;

use common::{assert_action, cwd, home, to_git_bash};
use tabit_gate::checker_bash::check_bash;
use tabit_gate::checker_read::check_read;
use tabit_gate::checker_write::check_write;
use tabit_gate::config;
use tabit_gate::types::Action;

// --- git-bash drive paths are the same as native paths ----------------------

#[test]
fn asks_for_credential_read_via_git_bash_path() {
    let gb_cred = format!("{}/.aws/credentials", to_git_bash(&home()));
    let result = check_read(&gb_cred, &config::default_config());
    assert_action(
        &result,
        Action::Ask,
        "/c/-style input reaches the same protected path",
    );
}

#[test]
fn asks_for_credential_write_via_git_bash_path_same_as_native() {
    let cred = format!("{}\\.aws\\credentials", home());
    let gb_cred = format!("{}/.aws/credentials", to_git_bash(&home()));
    let via_gb = check_write(&gb_cred, &config::default_config());
    let native = check_write(&cred, &config::default_config());
    assert_eq!(
        via_gb.action, native.action,
        "git-bash and native spellings must produce the same verdict"
    );
}

#[test]
fn allows_writes_inside_cwd_via_git_bash_path() {
    let target = format!("{}/new-file.txt", to_git_bash(&cwd()));
    let result = check_write(&target, &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "the /c/ spelling of CWD keeps the CWD allow",
    );
}

// --- different drives are different paths -------------------------------------

/// The drive letter that is NOT `path`'s — the mirror tests need a
/// genuinely different drive, and CI checks out onto whatever drive
/// the runner hands it (`D:\a\...` on windows-latest), so a hardcoded
/// counterpart would be the SAME drive there.
fn other_drive(path: &str) -> char {
    if path
        .chars()
        .next()
        .is_some_and(|d| d.eq_ignore_ascii_case(&'D'))
    {
        'C'
    } else {
        'D'
    }
}

#[test]
fn does_not_extend_cwd_write_allow_to_a_mirrored_path_on_another_drive() {
    let cwd = cwd();
    let mirrored = format!("{}:{}\\evil.txt", other_drive(&cwd), &cwd[2..]);
    let result = check_write(&mirrored, &config::default_config());
    assert_action(
        &result,
        Action::Deny,
        "a mirrored path on another drive is not the CWD allow",
    );
}

#[test]
fn does_not_extend_home_ask_to_a_mirrored_path_on_another_drive() {
    let home = home();
    let mirrored = format!("{}:{}\\.aws\\credentials", other_drive(&home), &home[2..]);
    let result = check_read(&mirrored, &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "read default is allow; the other-drive path is not HOME",
    );
}

// --- case variants are the same path --------------------------------------------

#[test]
fn asks_for_credential_read_via_upper_cased_path() {
    let cred = format!("{}\\.aws\\credentials", home());
    let result = check_read(cred.to_uppercase(), &config::default_config());
    assert_action(&result, Action::Ask, "win32 matching is case-insensitive");
}

#[test]
fn asks_for_home_write_via_lower_cased_path() {
    let target = format!("{}\\secret.txt", home()).to_lowercase();
    let result = check_write(&target, &config::default_config());
    assert_action(&result, Action::Ask, "win32 matching is case-insensitive");
}

// --- bash cd tracking with git-bash paths -----------------------------------------

#[test]
fn tracks_cd_into_a_git_bash_home_directory() {
    let command = format!("cd {} && rm secret.txt", to_git_bash(&home()));
    let result = check_bash(&command, &config::default_config());
    assert_action(&result, Action::Ask, "the /c/ home keeps the HOME ask");
}

#[test]
fn tracks_cd_into_a_git_bash_project_directory() {
    let command = format!("cd {} && rm scratch-file.txt", to_git_bash(&cwd()));
    let result = check_bash(&command, &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "the /c/ spelling of CWD keeps the CWD allow",
    );
}
