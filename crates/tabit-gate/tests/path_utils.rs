//! Path preprocessing semantics, observed through the checkers
//! (ported from pi-sanity `tests/unit/permissions/path-utils.test.ts`).
//!
//! The source suite calls `preprocessConfigPattern`,
//! `preprocessRuntimePath`, and `canonicalizeDrive` as pure string
//! functions; those are not part of the frozen API, so every case here
//! drives the same preprocessing through `check_read` with a config
//! whose pattern (or probe path) exercises the transformation.
//! Not ported: the exact-string `canonicalizeDrive` unit cases
//! (idempotency, UNC passthrough) — they assert the string function's
//! output directly; their behavioral consequences are pinned by
//! windows_paths.rs at the checker level.

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

use common::{allow_rule, assert_action, config_with_permissions, cwd, home, section, tmpdir};
use tabit_gate::checker_read::check_read;
use tabit_gate::types::Action;

/// A read config that allows only paths matching `patterns`.
fn only_matches(patterns: &[&str]) -> tabit_gate::config::SanityConfig {
    config_with_permissions(
        section(Action::Deny, None, vec![allow_rule(patterns)]),
        section(Action::Allow, None, vec![]),
    )
}

/// The repo root as the default context detects it (`git rev-parse
/// --show-toplevel`), or None when git is unavailable / not a repo.
fn repo_root() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() { None } else { Some(root) }
}

// --- {{VAR}} expansion in config patterns ------------------------------------

#[test]
fn home_placeholder_expands_to_the_home_directory() {
    let config = only_matches(&["{{HOME}}/.ssh/**"]);
    let inside = check_read(format!("{}/.ssh/id_rsa", home()), &config);
    let outside = check_read("/etc/hosts", &config);
    assert_action(
        &inside,
        Action::Allow,
        "{{HOME}} expands so the pattern matches",
    );
    assert_action(&outside, Action::Deny, "the pattern stays anchored at HOME");
}

#[test]
fn cwd_placeholder_expands_to_the_working_directory() {
    let config = only_matches(&["{{CWD}}/file.txt"]);
    let inside = check_read(format!("{}/file.txt", cwd()), &config);
    let outside = check_read(format!("{}/other.txt", cwd()), &config);
    assert_action(
        &inside,
        Action::Allow,
        "{{CWD}} expands so the pattern matches",
    );
    assert_action(&outside, Action::Deny, "the pattern stays anchored at CWD");
}

#[test]
fn tmpdir_placeholder_expands_to_the_temp_directory() {
    let config = only_matches(&["{{TMPDIR}}/temp/**"]);
    let inside = check_read(format!("{}/temp/file", tmpdir()), &config);
    let outside = check_read(format!("{}/elsewhere", tmpdir()), &config);
    assert_action(
        &inside,
        Action::Allow,
        "{{TMPDIR}} expands so the pattern matches",
    );
    assert_action(
        &outside,
        Action::Deny,
        "the pattern stays anchored at TMPDIR",
    );
}

#[test]
fn repo_placeholder_expands_to_the_repo_root_or_cwd() {
    // The default context auto-detects the repo root via git and falls
    // back to cwd; compute the same anchor the way the context does.
    let config = only_matches(&["{{REPO}}/file.txt"]);
    match repo_root() {
        Some(repo) => {
            let inside = check_read(format!("{repo}/file.txt"), &config);
            assert_action(
                &inside,
                Action::Allow,
                "{{REPO}} expands to the detected repo root",
            );
        }
        None => {
            let inside = check_read(format!("{}/file.txt", cwd()), &config);
            assert_action(
                &inside,
                Action::Allow,
                "{{REPO}} falls back to cwd outside a repo",
            );
        }
    }
}

// --- $ENV_VAR expansion in config patterns --------------------------------------

#[test]
fn env_var_placeholder_expands_when_set() {
    const VAR: &str = "TABIT_GATE_PATTERN_ENV";
    // SAFETY: test-private variable, no other test reads it.
    unsafe { std::env::set_var(VAR, "/test/value") };
    let pattern = format!("${VAR}/file");
    let config = only_matches(&[pattern.as_str()]);
    let result = check_read("/test/value/file", &config);
    // SAFETY: see above.
    unsafe { std::env::remove_var(VAR) };
    assert_action(&result, Action::Allow, "$VAR expands to the env value");
}

#[test]
fn unset_env_var_stays_literal() {
    // The variable is deliberately never set: the pattern keeps its
    // literal `$...` text, so no path matches it.
    let config = only_matches(&["$TABIT_GATE_UNDEFINED_VAR/file"]);
    let result = check_read("/test/value/file", &config);
    assert_action(
        &result,
        Action::Deny,
        "an unset variable cannot widen the pattern",
    );
}

// --- pattern normalization -------------------------------------------------------

#[test]
fn trailing_slash_in_a_pattern_is_normalized_away() {
    // ".../Temp/" normalizes to the tmpdir itself, so the tmpdir
    // exactly matches; an unnormalized pattern would not.
    let pattern = format!("{}/", tmpdir());
    let config = only_matches(&[pattern.as_str()]);
    let result = check_read(tmpdir(), &config);
    assert_action(
        &result,
        Action::Allow,
        "the pattern's trailing slash is stripped",
    );
}

#[test]
fn simple_patterns_pass_through_unchanged() {
    // "/simple/path" has nothing to expand: only the exact path matches.
    let config = only_matches(&["/simple/path"]);
    assert_action(
        &check_read("/simple/path", &config),
        Action::Allow,
        "exact passthrough",
    );
    assert_action(
        &check_read("/simple/path/child", &config),
        Action::Deny,
        "exact means exact",
    );
}

// --- expanded patterns at work (the picomatch block) ------------------------------

#[test]
fn expanded_cwd_glob_matches_within_cwd_only() {
    let config = only_matches(&["{{CWD}}/**"]);
    let inside = check_read(format!("{}/file.txt", cwd()), &config);
    let outside = check_read("/other/file.txt", &config);
    assert_action(
        &inside,
        Action::Allow,
        "{{CWD}}/** matches files under the cwd",
    );
    assert_action(
        &outside,
        Action::Deny,
        "{{CWD}}/** does not match foreign absolute paths",
    );
}

#[test]
fn expanded_repo_glob_matches_git_directories_at_any_depth() {
    let Some(repo) = repo_root() else {
        return; // outside a repo the {{REPO}} anchor is cwd; covered above
    };
    let config = only_matches(&["{{REPO}}/**/.git/**"]);
    let top = check_read(format!("{repo}/.git/config"), &config);
    let nested = check_read(format!("{repo}/submodule/.git/HEAD"), &config);
    let foreign = check_read("/other/.git/config", &config);
    assert_action(&top, Action::Allow, "the repo's own .git matches");
    assert_action(&nested, Action::Allow, "nested .git directories match");
    assert_action(&foreign, Action::Deny, "foreign .git paths do not match");
}

#[test]
fn drive_prefixes_canonicalize_case_insensitively_on_windows() {
    if !cfg!(windows) {
        return; // win32-only handling, like the source's pinned platform
    }
    // C:\Users\file canonicalizes to /c/Users/file, so a pattern in the
    // canonical spelling matches the native input (win32: case-insensitive).
    let config = only_matches(&["/c/users/file"]);
    let native = check_read("C:\\Users\\file", &config);
    assert_action(
        &native,
        Action::Allow,
        "the drive spelling canonicalizes to /c/...",
    );
    let other_drive = check_read("D:\\Users\\file", &config);
    assert_action(&other_drive, Action::Deny, "distinct drives stay distinct");
}
