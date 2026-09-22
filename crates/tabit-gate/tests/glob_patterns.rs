//! Glob patterns in bash paths (ported from pi-sanity
//! `tests/integration/scenarios/glob-patterns.test.ts`): globs in CWD
//! are allowed, globs into system directories are denied, and special
//! characters in filenames stay paths.
//!
//! The source file's closing "pattern matching behavior" describe is
//! four `assert.ok(true)` no-ops documenting that exact patterns match
//! only exact paths and `/**` is needed for subpaths; they assert
//! nothing and are recorded here as comments instead of fake tests.

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

use tabit_gate::checker_bash::check_bash;
use tabit_gate::config;
use tabit_gate::types::Action;

fn action_of(command: &str) -> Action {
    check_bash(command, &config::default_config()).action
}

// --- glob patterns in CWD (allowed) ----------------------------------------

#[test]
fn allows_rm_with_star_glob_in_cwd() {
    assert_eq!(
        action_of("rm *.txt"),
        Action::Allow,
        "delete in CWD is allowed"
    );
}

#[test]
fn allows_rm_with_doublestar_glob_in_cwd() {
    assert_eq!(
        action_of("rm **/*.log"),
        Action::Allow,
        "delete in CWD is allowed"
    );
}

#[test]
fn allows_cat_with_glob_in_cwd() {
    assert_eq!(
        action_of("cat *.log"),
        Action::Allow,
        "read default is allow"
    );
}

#[test]
fn allows_cp_with_glob_within_cwd() {
    assert_eq!(
        action_of("cp *.txt backup/"),
        Action::Allow,
        "read+write in CWD is allowed"
    );
}

#[test]
fn allows_mv_with_glob_within_cwd() {
    assert_eq!(
        action_of("mv *.txt archive/"),
        Action::Allow,
        "all ops in CWD are allowed"
    );
}

// --- special characters in filenames ---------------------------------------

#[test]
fn allows_hash_in_filename() {
    assert_eq!(
        action_of("rm file#1.txt"),
        Action::Allow,
        "special chars in CWD are paths"
    );
}

#[test]
fn allows_at_sign_in_filename() {
    assert_eq!(
        action_of("cat file@2.txt"),
        Action::Allow,
        "special chars in CWD are paths"
    );
}

#[test]
fn allows_bracket_glob_in_cwd() {
    assert_eq!(
        action_of("rm [abc].txt"),
        Action::Allow,
        "a bracket glob is still a CWD path"
    );
}

// --- glob patterns in system directories (deny) ------------------------------

#[test]
fn denies_rm_glob_in_etc() {
    assert_eq!(
        action_of("rm -rf /etc/*.conf"),
        Action::Deny,
        "/etc is a system directory"
    );
}

#[test]
fn denies_rm_glob_in_var_log() {
    assert_eq!(
        action_of("rm -rf /var/log/*.log"),
        Action::Deny,
        "/var/log is a system directory"
    );
}

#[test]
fn denies_rm_glob_in_usr_share() {
    assert_eq!(
        action_of("rm -rf /usr/share/*.txt"),
        Action::Deny,
        "/usr/share is a system directory"
    );
}

#[test]
fn denies_rm_glob_at_root() {
    assert_eq!(
        action_of("rm -rf /*"),
        Action::Deny,
        "root-level globs hit everything"
    );
}

// --- write outside CWD (deny) -------------------------------------------------

#[test]
fn denies_cp_glob_into_etc() {
    assert_eq!(
        action_of("cp *.txt /etc/"),
        Action::Deny,
        "writing outside CWD is denied"
    );
}

#[test]
fn denies_mv_glob_into_var() {
    assert_eq!(
        action_of("mv *.txt /var/"),
        Action::Deny,
        "writing outside CWD is denied"
    );
}

// The source file's final describe ("pattern matching behavior") is four
// assert.ok(true) documentation no-ops:
//   - exact patterns match only exact paths (use /** for subpaths)
//   - glob patterns with /** match subpaths
//   - the default config uses {{HOME}}/** for the home directory
//   - the default config uses **/.git/** for git protection
// They assert nothing; the behaviors they document are pinned by the
// concrete cases in checker_bash.rs, git_protection.rs, and
// path_utils.rs.
