//! The POSIX /tmp rewrite: pure-function pins for the path rewrite
//! and the tool-param rewrite over configured rules (the integration
//! through the permission-gate hook is pinned in tabit-core).

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

use std::collections::HashMap;

use tabit_gate::config::{CheckKind, SanityConfig, ToolParamCheck, ToolsConfig};
use tabit_gate::path_utils::Platform;
use tabit_gate::tmp_rewrite::{rewrite_posix_tmp_path, rewrite_tool_path_param};
use tabit_gate::types::Action;

const WIN: Platform = Platform::Win32;
const TMP: &str = "C:\\Users\\u\\Temp";

#[test]
fn exact_and_nested_tmp_prefixes_rewrite_with_posix_separators_collapsed() {
    assert_eq!(rewrite_posix_tmp_path("/tmp", TMP, WIN), TMP);
    assert_eq!(
        rewrite_posix_tmp_path("/tmp/a/b.txt", TMP, WIN),
        "C:\\Users\\u\\Temp\\a\\b.txt"
    );
    // "." and "//" collapse before the prefix match.
    assert_eq!(
        rewrite_posix_tmp_path("/tmp//x/./y", TMP, WIN),
        "C:\\Users\\u\\Temp\\x\\y"
    );
}

#[test]
fn lookalikes_and_non_tmp_roots_are_left_alone() {
    for untouched in ["/tmpfoo", "/tmpfoo/x", "C:/tmp/a", "tmp/a", "/var/x", ""] {
        assert_eq!(rewrite_posix_tmp_path(untouched, TMP, WIN), untouched);
    }
}

#[test]
fn traversal_out_of_tmp_returns_the_original_for_the_checker_to_deny() {
    // Normalization moves the path out of /tmp, so no rewrite — the
    // permission check must see (and deny) the original path.
    assert_eq!(
        rewrite_posix_tmp_path("/tmp/../etc/x", TMP, WIN),
        "/tmp/../etc/x"
    );
}

#[test]
fn a_non_windows_platform_never_rewrites() {
    assert_eq!(
        rewrite_posix_tmp_path("/tmp/a", TMP, Platform::Other),
        "/tmp/a"
    );
}

/// write→path (write check), bash→command (bash check), edit→paths
/// (an array-valued write param) — the shapes the shipped rules use
/// plus one array case.
fn rules_config() -> SanityConfig {
    let mut rules = HashMap::new();
    rules.insert(
        "write".to_string(),
        vec![ToolParamCheck {
            param: "path".to_string(),
            check: CheckKind::Write,
        }],
    );
    rules.insert(
        "bash".to_string(),
        vec![ToolParamCheck {
            param: "command".to_string(),
            check: CheckKind::Bash,
        }],
    );
    rules.insert(
        "edit".to_string(),
        vec![ToolParamCheck {
            param: "paths".to_string(),
            check: CheckKind::Write,
        }],
    );
    SanityConfig {
        tools: ToolsConfig { rules },
        ..SanityConfig::default()
    }
}

#[test]
fn configured_write_params_rewrite_and_report_the_change() {
    let config = rules_config();
    let mut input = serde_json::json!({
        "path": "/tmp/a.txt",
        "note": "/tmp/b.txt",
    });
    let object = input.as_object_mut().expect("object");
    let changed = rewrite_tool_path_param("write", object, TMP, &config, WIN);
    assert!(changed, "the configured param was rewritten");
    assert_eq!(object["path"], "C:\\Users\\u\\Temp\\a.txt");
    assert_eq!(
        object["note"], "/tmp/b.txt",
        "an unconfigured param is never rewritten"
    );
}

#[test]
fn bash_params_unknown_tools_and_other_platforms_change_nothing() {
    let config = rules_config();

    let mut input = serde_json::json!({"command": "/tmp/x"});
    let object = input.as_object_mut().expect("object");
    assert!(!rewrite_tool_path_param("bash", object, TMP, &config, WIN));
    assert_eq!(
        object["command"], "/tmp/x",
        "the shell translates /tmp itself"
    );

    let mut input = serde_json::json!({"path": "/tmp/x"});
    let object = input.as_object_mut().expect("object");
    assert!(!rewrite_tool_path_param(
        "mystery", object, TMP, &config, WIN
    ));

    let mut input = serde_json::json!({"path": "/tmp/x"});
    let object = input.as_object_mut().expect("object");
    assert!(!rewrite_tool_path_param(
        "write",
        object,
        TMP,
        &config,
        Platform::Other
    ));
    assert_eq!(object["path"], "/tmp/x");
}

#[test]
fn array_params_rewrite_only_their_string_members() {
    let config = rules_config();
    let mut input = serde_json::json!({"paths": ["/tmp/a", "/keep", 7]});
    let object = input.as_object_mut().expect("object");
    let changed = rewrite_tool_path_param("edit", object, TMP, &config, WIN);
    assert!(changed);
    assert_eq!(
        object["paths"],
        serde_json::json!(["C:\\Users\\u\\Temp\\a", "/keep", 7]),
        "non-tmp strings and non-strings pass through untouched"
    );
}

#[test]
fn non_string_values_in_a_configured_param_are_skipped() {
    let config = rules_config();
    let mut input = serde_json::json!({"path": 7});
    let object = input.as_object_mut().expect("object");
    assert!(!rewrite_tool_path_param("write", object, TMP, &config, WIN));
    assert_eq!(object["path"], 7);
}

#[test]
fn the_empty_config_is_allow_everything_with_no_rules() {
    // TS `createEmptyConfig`: the merge base starts permissive; the
    // shipped defaults (and user TOML) layer the policy on top.
    let empty = SanityConfig::default();
    assert_eq!(empty.permissions.read.default, Action::Allow);
    assert_eq!(empty.permissions.read.reason, None);
    assert!(empty.permissions.read.overrides.is_empty());
    assert_eq!(empty.permissions.write.default, Action::Allow);
    assert!(empty.permissions.write.overrides.is_empty());
    assert_eq!(empty.commands.default_action, Action::Allow);
    assert_eq!(
        empty.commands.reason.as_deref(),
        Some("Unknown commands default to allow (low-friction)")
    );
    assert!(empty.commands.rules.is_empty());
    assert!(empty.tools.rules.is_empty());
    assert_eq!(empty.ask_timeout, None);
}
