//! Path-permission semantics, observed through the read and write
//! checkers (ported from pi-sanity
//! `tests/unit/permissions/path-permission.test.ts`).
//!
//! The source suite calls `checkPathPermission(permission, context)`
//! directly; the frozen API exposes the same decision through
//! `check_read`/`check_write` with a custom `PermissionSection`, which
//! is what the checkRead/checkWrite halves of the source file do too.
//! Not ported: the `matchedPattern` result field (an internal return
//! shape the frozen `CheckResult` does not carry) and
//! `getDefaultContext` (its cwd/home/tmpdir anchors are pinned
//! indirectly by checker_read.rs / checker_write.rs, which run against
//! the real default context).

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

use common::{action_rule, action_rule_reason, assert_action, config_with_permissions, section};
use tabit_gate::checker_read::check_read;
use tabit_gate::checker_write::check_write;
use tabit_gate::types::Action;

// --- permission section semantics (via check_read) --------------------------

#[test]
fn returns_default_action_and_reason_when_no_overrides_match() {
    let permission = section(Action::Ask, Some("Default reason"), vec![]);
    let config = config_with_permissions(permission.clone(), permission);
    let result = check_read("/some/path", &config);
    assert_action(&result, Action::Ask, "no override matches");
    assert_eq!(result.reason.as_deref(), Some("Default reason"));
}

#[test]
fn returns_matching_override_action_and_reason() {
    let permission = section(
        Action::Allow,
        None,
        vec![action_rule_reason(
            &["/secret/**"],
            Action::Deny,
            "Secret area",
        )],
    );
    let config = config_with_permissions(permission.clone(), permission);
    let result = check_read("/secret/file.txt", &config);
    assert_action(&result, Action::Deny, "the override matches");
    assert_eq!(result.reason.as_deref(), Some("Secret area"));
}

#[test]
fn uses_last_matching_override() {
    let permission = section(
        Action::Allow,
        None,
        vec![
            action_rule(&["/**"], Action::Deny),
            action_rule_reason(&["/public/**"], Action::Allow, "Public area"),
        ],
    );
    let config = config_with_permissions(permission.clone(), permission);
    let result = check_read("/public/file.txt", &config);
    assert_action(&result, Action::Allow, "later overrides win");
    assert_eq!(result.reason.as_deref(), Some("Public area"));
}

#[test]
fn matches_preprocessed_literal_patterns() {
    let permission = section(
        Action::Ask,
        None,
        vec![action_rule_reason(
            &["/home/user/.ssh/**"],
            Action::Deny,
            "SSH keys",
        )],
    );
    let config = config_with_permissions(permission.clone(), permission);
    let result = check_read("/home/user/.ssh/id_rsa", &config);
    assert_action(
        &result,
        Action::Deny,
        "the literal pattern matches the nested path",
    );
}

#[test]
fn matches_any_pattern_in_a_single_override() {
    let permission = section(
        Action::Allow,
        None,
        vec![action_rule(&["/a/**", "/b/**"], Action::Deny)],
    );
    let config = config_with_permissions(permission.clone(), permission);
    assert_action(
        &check_read("/a/file", &config),
        Action::Deny,
        "first pattern in the override matches",
    );
    assert_action(
        &check_read("/b/file", &config),
        Action::Deny,
        "second pattern in the override matches",
    );
    assert_action(
        &check_read("/c/file", &config),
        Action::Allow,
        "no pattern in the override matches",
    );
}

#[test]
fn matches_glob_wildcards_star_question_and_doublestar() {
    let permission = section(
        Action::Allow,
        None,
        vec![
            action_rule_reason(&["**/*.txt"], Action::Ask, "Text files anywhere"),
            action_rule_reason(&["/project/file?.log"], Action::Deny, "Log files"),
            action_rule_reason(&["/deep/**/*.js"], Action::Ask, "JS files anywhere"),
        ],
    );
    let config = config_with_permissions(permission.clone(), permission);
    // * wildcard with **/
    assert_action(
        &check_read("/project/readme.txt", &config),
        Action::Ask,
        "**/*.txt matches a nested txt file",
    );
    assert_action(
        &check_read("/project/readme.md", &config),
        Action::Allow,
        "**/*.txt does not match other extensions",
    );
    // ? wildcard
    assert_action(
        &check_read("/project/file1.log", &config),
        Action::Deny,
        "file? matches one character",
    );
    assert_action(
        &check_read("/project/file12.log", &config),
        Action::Allow,
        "file? does not match two characters",
    );
    // ** wildcard
    assert_action(
        &check_read("/deep/a/b/c/test.js", &config),
        Action::Ask,
        "/deep/**/*.js matches deeply nested js",
    );
    assert_action(
        &check_read("/deep/test.js", &config),
        Action::Ask,
        "/deep/**/*.js matches shallow js too",
    );
    assert_action(
        &check_read("/deep/a/b/c/test.ts", &config),
        Action::Allow,
        "/deep/**/*.js does not match other extensions",
    );
}

// --- checkRead uses permissions.read ---------------------------------------

#[test]
fn read_uses_the_read_section() {
    let read = section(
        Action::Ask,
        None,
        vec![action_rule(&["/safe/**"], Action::Allow)],
    );
    let write = section(Action::Allow, None, vec![]);
    let config = config_with_permissions(read, write);
    assert_action(
        &check_read("/safe/file.txt", &config),
        Action::Allow,
        "the read override matches",
    );
    assert_action(
        &check_read("/other/file.txt", &config),
        Action::Ask,
        "the read default applies",
    );
}

#[test]
fn read_allows_when_config_allows() {
    let config = config_with_permissions(
        section(Action::Allow, None, vec![]),
        section(Action::Allow, None, vec![]),
    );
    assert_action(
        &check_read("/any/path", &config),
        Action::Allow,
        "allow default",
    );
}

#[test]
fn read_denies_with_reason_when_config_denies() {
    let config = config_with_permissions(
        section(Action::Deny, Some("Reads are denied"), vec![]),
        section(Action::Allow, None, vec![]),
    );
    let result = check_read("/secret/file", &config);
    assert_action(&result, Action::Deny, "deny default");
    assert_eq!(result.reason.as_deref(), Some("Reads are denied"));
}

#[test]
fn read_asks_with_reason_when_config_asks() {
    let config = config_with_permissions(
        section(Action::Ask, Some("Please confirm read"), vec![]),
        section(Action::Allow, None, vec![]),
    );
    let result = check_read("/some/path", &config);
    assert_action(&result, Action::Ask, "ask default");
    assert_eq!(result.reason.as_deref(), Some("Please confirm read"));
}

#[test]
fn read_respects_override_rules_last_match_wins() {
    let read = section(
        Action::Deny,
        None,
        vec![
            action_rule(&["/public/**"], Action::Allow),
            action_rule(&["/public/secret/**"], Action::Deny),
        ],
    );
    let config = config_with_permissions(read, section(Action::Allow, None, vec![]));
    assert_action(
        &check_read("/public/file.txt", &config),
        Action::Allow,
        "public path is allowed",
    );
    assert_action(
        &check_read("/public/secret/file.txt", &config),
        Action::Deny,
        "the later override denies secret-in-public",
    );
    assert_action(
        &check_read("/private/file.txt", &config),
        Action::Deny,
        "everything else takes the default",
    );
}

// --- checkWrite uses permissions.write --------------------------------------

#[test]
fn write_uses_the_write_section() {
    let write = section(
        Action::Ask,
        None,
        vec![action_rule(&["/project/**"], Action::Allow)],
    );
    let config = config_with_permissions(section(Action::Allow, None, vec![]), write);
    assert_action(
        &check_write("/project/file.txt", &config),
        Action::Allow,
        "the write override matches",
    );
    assert_action(
        &check_write("/outside/file.txt", &config),
        Action::Ask,
        "the write default applies",
    );
}

#[test]
fn write_denies_with_reason_when_config_denies() {
    let config = config_with_permissions(
        section(Action::Allow, None, vec![]),
        section(Action::Deny, Some("Writes are denied"), vec![]),
    );
    let result = check_write("/etc/passwd", &config);
    assert_action(&result, Action::Deny, "deny default");
    assert_eq!(result.reason.as_deref(), Some("Writes are denied"));
}

#[test]
fn write_protects_system_directories() {
    let write = section(
        Action::Allow,
        None,
        vec![action_rule_reason(
            &["/etc/**", "/usr/**", "/bin/**"],
            Action::Deny,
            "System directories are protected",
        )],
    );
    let config = config_with_permissions(section(Action::Allow, None, vec![]), write);
    assert_action(
        &check_write("/etc/config", &config),
        Action::Deny,
        "/etc protected",
    );
    assert_action(
        &check_write("/usr/bin/app", &config),
        Action::Deny,
        "/usr protected",
    );
    assert_action(
        &check_write("/bin/ls", &config),
        Action::Deny,
        "/bin protected",
    );
    assert_action(
        &check_write("/home/user/file", &config),
        Action::Allow,
        "outside system dirs",
    );
}

// The source file's PermissionSection fixtures are reused above through
// `PermissionSection: Clone`; delete is covered by write (see
// write_delete.rs), and the source file itself ends with that note.
