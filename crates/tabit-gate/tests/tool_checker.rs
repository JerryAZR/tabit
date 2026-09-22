//! The generic tool-call checker (ported from pi-sanity
//! `tests/unit/config/tools.test.ts`, describes `checkToolCall` and
//! `buildToolDetails`, plus the routing essence of
//! `tests/integration/extension/tool-interception.test.ts`).
//!
//! The source file builds configs through the TOML loader, which is
//! not part of the v1 frozen API, so each case constructs the same
//! config directly. The extension file's tool-routing cases run here
//! against `default_config()` (the extension ships exactly that rule
//! set); its UI-interaction, registration, and /tmp-rewrite cases are
//! the pi extension layer and stay behind (see the porting report).

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

use common::{assert_action, empty_config, input, loaded_rule, param_check, tool_rules};
use tabit_gate::config::{CheckKind, SanityConfig};
use tabit_gate::tool_checker::{build_tool_details, check_tool_call};
use tabit_gate::types::Action;

/// The source file's `makeConfig(toolsToml)`: read defaults allow,
/// write defaults deny, plus the given tool rules.
fn make_config(rules: Vec<(&str, Vec<tabit_gate::config::ToolParamCheck>)>) -> SanityConfig {
    let mut config = empty_config();
    config.permissions.write.default = Action::Deny;
    config.tools = tool_rules(&rules);
    config
}

// --- checkToolCall ------------------------------------------------------------

#[test]
fn returns_none_for_unlisted_tools() {
    let config = make_config(vec![]);
    let result = check_tool_call("unknown", &input(&[]), &config);
    assert!(result.is_none(), "unlisted tools are ignored entirely");
}

#[test]
fn allows_read_of_allowed_files() {
    let config = make_config(vec![("read", vec![param_check("path", CheckKind::Read)])]);
    let result = check_tool_call("read", &input(&[("path", "package.json".into())]), &config);
    let result = result.expect("listed tool always yields a result");
    assert_action(&result, Action::Allow, "read default is allow");
}

#[test]
fn asks_for_read_of_sensitive_files() {
    let mut config = make_config(vec![("read", vec![param_check("path", CheckKind::Read)])]);
    config.permissions.read.overrides.push(loaded_rule(
        "read",
        &["{{HOME}}/.ssh/*"],
        Action::Ask,
        "May contain credentials or secrets",
    ));
    let result = check_tool_call("read", &input(&[("path", "~/.ssh/id_rsa".into())]), &config);
    let result = result.expect("listed tool always yields a result");
    assert_action(
        &result,
        Action::Ask,
        "the tilde path expands into the sensitive override",
    );
}

#[test]
fn denies_write_to_protected_locations() {
    let config = make_config(vec![("write", vec![param_check("path", CheckKind::Write)])]);
    let result = check_tool_call("write", &input(&[("path", "/etc/passwd".into())]), &config);
    let result = result.expect("listed tool always yields a result");
    assert_action(&result, Action::Deny, "write default is deny");
}

#[test]
fn passes_through_when_required_param_is_missing() {
    let config = make_config(vec![("read", vec![param_check("path", CheckKind::Read)])]);
    let result = check_tool_call("read", &input(&[]), &config);
    let result = result.expect("listed tool always yields a result");
    assert_action(
        &result,
        Action::Allow,
        "missing params are skipped for the framework to validate",
    );
}

#[test]
fn aggregates_multiple_checks_with_deny_winning() {
    let config = make_config(vec![(
        "copy",
        vec![
            param_check("src", CheckKind::Read),
            param_check("dst", CheckKind::Write),
        ],
    )]);
    let result = check_tool_call(
        "copy",
        &input(&[
            ("src", "package.json".into()),
            ("dst", "/etc/passwd".into()),
        ]),
        &config,
    );
    let result = result.expect("listed tool always yields a result");
    assert_action(&result, Action::Deny, "the write deny dominates");
}

#[test]
fn aggregates_array_valued_parameters() {
    let config = make_config(vec![(
        "delete_many",
        vec![param_check("paths", CheckKind::Write)],
    )]);
    let result = check_tool_call(
        "delete_many",
        &input(&[("paths", serde_json::json!(["package.json", "/etc/passwd"]))]),
        &config,
    );
    let result = result.expect("listed tool always yields a result");
    assert_action(&result, Action::Deny, "each array element is checked");
}

#[test]
fn ignores_non_string_values_in_arrays() {
    let config = make_config(vec![("mixed", vec![param_check("paths", CheckKind::Read)])]);
    let result = check_tool_call(
        "mixed",
        &input(&[("paths", serde_json::json!(["package.json", 123, null]))]),
        &config,
    );
    let result = result.expect("listed tool always yields a result");
    assert_action(&result, Action::Allow, "numbers and nulls are not paths");
}

#[test]
fn ignores_empty_strings() {
    let config = make_config(vec![("read", vec![param_check("path", CheckKind::Read)])]);
    let result = check_tool_call("read", &input(&[("path", "".into())]), &config);
    let result = result.expect("listed tool always yields a result");
    assert_action(&result, Action::Allow, "an empty string is a missing param");
}

#[test]
fn aggregates_reasons_from_multiple_checks() {
    let mut config = make_config(vec![(
        "copy",
        vec![
            param_check("src", CheckKind::Read),
            param_check("dst", CheckKind::Write),
        ],
    )]);
    config.permissions.read.overrides.push(loaded_rule(
        "read",
        &["{{HOME}}/.ssh/*"],
        Action::Ask,
        "May contain credentials or secrets",
    ));
    config.permissions.write.overrides.push(loaded_rule(
        "write",
        &["{{HOME}}/**"],
        Action::Ask,
        "Writing to home directory requires confirmation",
    ));
    let result = check_tool_call(
        "copy",
        &input(&[("src", "~/.ssh/id_rsa".into()), ("dst", "~/.bashrc".into())]),
        &config,
    );
    let result = result.expect("listed tool always yields a result");
    assert_action(&result, Action::Ask, "both paths ask");
    let reason = result.reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains("credentials"),
        "the read reason must appear: {reason:?}"
    );
    assert!(
        reason.contains("home directory"),
        "the write reason must appear: {reason:?}"
    );
}

#[test]
fn aggregates_mixed_actions_with_deny_winning_and_keeps_reasons() {
    let mut config = make_config(vec![(
        "copy",
        vec![
            param_check("src", CheckKind::Read),
            param_check("dst", CheckKind::Write),
        ],
    )]);
    config.permissions.read.overrides.push(loaded_rule(
        "read",
        &["{{HOME}}/.ssh/*"],
        Action::Ask,
        "May contain credentials or secrets",
    ));
    let result = check_tool_call(
        "copy",
        &input(&[
            ("src", "~/.ssh/id_rsa".into()),
            ("dst", "/etc/passwd".into()),
        ]),
        &config,
    );
    let result = result.expect("listed tool always yields a result");
    assert_action(
        &result,
        Action::Deny,
        "the write deny dominates the read ask",
    );
    let reason = result.reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains("credentials"),
        "the ask's reason survives the deny: {reason:?}"
    );
}

#[test]
fn allows_safe_bash_commands() {
    let config = make_config(vec![(
        "bash",
        vec![param_check("command", CheckKind::Bash)],
    )]);
    let result = check_tool_call("bash", &input(&[("command", "ls -la".into())]), &config);
    let result = result.expect("listed tool always yields a result");
    assert_action(&result, Action::Allow, "safe commands allow");
}

#[test]
fn checks_bash_commands() {
    let mut config = make_config(vec![(
        "bash",
        vec![param_check("command", CheckKind::Bash)],
    )]);
    config.commands.rules.push(common::command_rule(
        "dd",
        Action::Deny,
        common::rule_config(),
    ));
    let result = check_tool_call(
        "bash",
        &input(&[("command", "dd if=/dev/zero of=/dev/sda".into())]),
        &config,
    );
    let result = result.expect("listed tool always yields a result");
    assert_action(
        &result,
        Action::Deny,
        "the bash check routes through the command rules",
    );
}

// --- buildToolDetails -----------------------------------------------------------

#[test]
fn details_fall_back_for_unknown_tools() {
    let config = make_config(vec![]);
    let details = build_tool_details("unknown", &input(&[("path", "x".into())]), &config);
    assert_eq!(details, "Tool: unknown");
}

#[test]
fn details_skip_empty_string_parameter_values() {
    let config = make_config(vec![("read", vec![param_check("path", CheckKind::Read)])]);
    let details = build_tool_details("read", &input(&[("path", "".into())]), &config);
    assert!(
        details.contains("Tool: read"),
        "the tool line stays: {details:?}"
    );
    assert!(
        !details.contains("path"),
        "empty params are not described: {details:?}"
    );
}

#[test]
fn details_describe_checked_parameters() {
    let config = make_config(vec![(
        "copy",
        vec![
            param_check("src", CheckKind::Read),
            param_check("dst", CheckKind::Write),
        ],
    )]);
    let details = build_tool_details(
        "copy",
        &input(&[("src", "a.txt".into()), ("dst", "b.txt".into())]),
        &config,
    );
    assert!(details.contains("Tool: copy"), "{details:?}");
    assert!(details.contains("src (read): a.txt"), "{details:?}");
    assert!(details.contains("dst (write): b.txt"), "{details:?}");
}

#[test]
fn details_group_multiple_checks_on_the_same_parameter() {
    let config = make_config(vec![(
        "move",
        vec![
            param_check("path", CheckKind::Read),
            param_check("path", CheckKind::Write),
        ],
    )]);
    let details = build_tool_details("move", &input(&[("path", "a.txt".into())]), &config);
    assert!(details.contains("path (read, write): a.txt"), "{details:?}");
}

#[test]
fn details_skip_missing_parameters() {
    let config = make_config(vec![(
        "copy",
        vec![
            param_check("src", CheckKind::Read),
            param_check("dst", CheckKind::Write),
        ],
    )]);
    let details = build_tool_details("copy", &input(&[("src", "a.txt".into())]), &config);
    assert!(details.contains("src (read): a.txt"), "{details:?}");
    assert!(
        !details.contains("dst"),
        "absent params are not described: {details:?}"
    );
}

#[test]
fn details_handle_array_values() {
    let config = make_config(vec![("multi", vec![param_check("paths", CheckKind::Read)])]);
    let details = build_tool_details(
        "multi",
        &input(&[("paths", serde_json::json!(["a.txt", "b.txt"]))]),
        &config,
    );
    assert!(
        details.contains("paths (read): a.txt, b.txt"),
        "{details:?}"
    );
}

// --- extension routing essence (over default_config) ------------------------------

/// The extension ships the default config: read/write/edit route their
/// `path` param, bash routes `command`, anything else is ignored.
mod extension_routing {
    use super::*;

    #[test]
    fn allows_reading_project_files() {
        let result = check_tool_call(
            "read",
            &input(&[("path", "package.json".into())]),
            &tabit_gate::config::default_config(),
        );
        let result = result.expect("read is a listed tool");
        assert_action(&result, Action::Allow, "the extension lets this through");
    }

    #[test]
    fn blocks_reading_protected_files() {
        let result = check_tool_call(
            "read",
            &input(&[("path", "~/.ssh/id_rsa".into())]),
            &tabit_gate::config::default_config(),
        );
        let result = result.expect("read is a listed tool");
        assert_action(
            &result,
            Action::Ask,
            "the extension turns this into a UI prompt/block",
        );
        assert!(result.reason.is_some(), "a reason backs the prompt");
    }

    #[test]
    fn passes_through_missing_params_for_all_path_tools() {
        let default = tabit_gate::config::default_config();
        for tool in ["read", "write", "edit"] {
            let result = check_tool_call(tool, &input(&[]), &default);
            let result = result.expect("all three are listed tools");
            assert_action(
                &result,
                Action::Allow,
                "missing path is the framework's problem",
            );
        }
    }

    #[test]
    fn allows_writing_to_allowed_locations() {
        let result = check_tool_call(
            "write",
            &input(&[("path", "test-file.txt".into())]),
            &tabit_gate::config::default_config(),
        );
        let result = result.expect("write is a listed tool");
        assert_action(&result, Action::Allow, "CWD writes pass");
    }

    #[test]
    fn blocks_writing_to_protected_locations() {
        let result = check_tool_call(
            "write",
            &input(&[("path", "~/.ssh/id_rsa".into())]),
            &tabit_gate::config::default_config(),
        );
        let result = result.expect("write is a listed tool");
        assert_action(&result, Action::Ask, "HOME writes prompt");
    }

    #[test]
    fn allows_safe_bash_and_blocks_dangerous_and_unparseable() {
        let default = tabit_gate::config::default_config();
        let safe = check_tool_call("bash", &input(&[("command", "ls -la".into())]), &default);
        let safe = safe.expect("bash is a listed tool");
        assert_action(&safe, Action::Allow, "ls is safe");

        let dd = check_tool_call(
            "bash",
            &input(&[("command", "dd if=/dev/zero of=/tmp/test".into())]),
            &default,
        );
        let dd = dd.expect("bash is a listed tool");
        assert_action(&dd, Action::Deny, "dd is denied");

        let parse_error = check_tool_call(
            "bash",
            &input(&[("command", "echo \"unclosed".into())]),
            &default,
        );
        let parse_error = parse_error.expect("bash is a listed tool");
        assert_action(&parse_error, Action::Deny, "parse errors deny");

        let missing = check_tool_call("bash", &input(&[]), &default);
        let missing = missing.expect("bash is a listed tool");
        assert_action(&missing, Action::Allow, "missing command passes through");
    }

    #[test]
    fn ignores_unknown_tools() {
        let result = check_tool_call(
            "unknown_tool",
            &input(&[("something", "value".into())]),
            &tabit_gate::config::default_config(),
        );
        assert!(result.is_none(), "unhandled tools are ignored");
    }
}

// The source config-parsing describe (tool rules from TOML, warnings)
// and the extension's UI/notification/tmp-rewrite layers are not
// portable to the pure core: config loading is out of the v1 API, and
// the UI lift lives in the pi integration layer.
