//! Shared helpers for the tabit-gate test corpus (ported from
//! pi-sanity's tests).
//!
//! Every assumed name of the config data model sits HERE, in one
//! place: the checkers, the walker, and `default_config()` are the
//! frozen API; the inner shape of `SanityConfig` mirrors pi-sanity's
//! `config-types.ts` (snake_cased). If a name differs, this file is
//! the only place to touch.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;

use tabit_gate::config::{
    CommandFlag, CommandsConfig, OverrideRule, PermissionSection, PermissionsConfig, PreCheck,
    Rule, RuleConfig, SanityConfig, ToolParamCheck, ToolsConfig,
};
use tabit_gate::types::{Action, CheckResult};

// ---------------------------------------------------------------------------
// Environment anchors (the same values the checker's default context uses)
// ---------------------------------------------------------------------------

/// The user's home directory, as the checker's default context sees it.
pub fn home() -> String {
    std::env::home_dir()
        .unwrap_or_else(|| {
            PathBuf::from(if cfg!(windows) {
                "C:\\Users\\nobody"
            } else {
                "/home/nobody"
            })
        })
        .to_string_lossy()
        .into_owned()
}

/// The temp directory, as the checker's default context sees it.
pub fn tmpdir() -> String {
    std::env::temp_dir().to_string_lossy().into_owned()
}

/// The process working directory (cargo runs test binaries at the
/// package root).
pub fn cwd() -> String {
    std::env::current_dir()
        .expect("process cwd")
        .to_string_lossy()
        .into_owned()
}

/// Native win32 path -> git-bash drive form: `C:\Users\x` -> `/c/Users/x`.
pub fn to_git_bash(path: &str) -> String {
    let bytes: Vec<char> = path.chars().collect();
    let drive = bytes[0].to_ascii_lowercase();
    let rest: String = bytes[2..].iter().collect();
    let rest = rest.replace('\\', "/");
    format!("/{drive}{rest}")
}

// ---------------------------------------------------------------------------
// Config construction (mirrors pi-sanity config-types.ts)
// ---------------------------------------------------------------------------

/// pi-sanity's `createEmptyConfig()`: everything allows, no rules.
pub fn empty_config() -> SanityConfig {
    SanityConfig {
        permissions: PermissionsConfig {
            read: section(Action::Allow, None, vec![]),
            write: section(Action::Allow, None, vec![]),
        },
        commands: CommandsConfig {
            default_action: Action::Allow,
            reason: Some("Unknown commands default to allow (low-friction)".into()),
            rules: vec![],
        },
        tools: ToolsConfig {
            rules: HashMap::new(),
        },
        ask_timeout: None,
    }
}

/// A config differing only in its two permission sections.
pub fn config_with_permissions(read: PermissionSection, write: PermissionSection) -> SanityConfig {
    let mut config = empty_config();
    config.permissions = PermissionsConfig { read, write };
    config
}

/// A config with a single command rule (pi-sanity's `makeConfig`).
pub fn config_with_command_rule(
    name: &str,
    fallback: Action,
    rule_config: RuleConfig,
) -> SanityConfig {
    let mut config = empty_config();
    config
        .commands
        .rules
        .push(command_rule(name, fallback, rule_config));
    config
}

/// A permission section: default action plus ordered overrides.
pub fn section(
    default: Action,
    reason: Option<&str>,
    overrides: Vec<OverrideRule>,
) -> PermissionSection {
    PermissionSection {
        default,
        reason: reason.map(str::to_string),
        overrides,
    }
}

/// An allow override over one or more patterns.
pub fn allow_rule(patterns: &[&str]) -> OverrideRule {
    action_rule(patterns, Action::Allow)
}

/// An override with no reason.
pub fn action_rule(patterns: &[&str], action: Action) -> OverrideRule {
    OverrideRule {
        path: patterns.iter().map(|p| (*p).to_string()).collect(),
        action,
        reason: None,
    }
}

/// An override carrying its reason.
pub fn action_rule_reason(patterns: &[&str], action: Action, reason: &str) -> OverrideRule {
    OverrideRule {
        path: patterns.iter().map(|p| (*p).to_string()).collect(),
        action,
        reason: Some(reason.to_string()),
    }
}

/// An empty rule body.
pub fn rule_config() -> RuleConfig {
    RuleConfig {
        reason: None,
        pre_checks: vec![],
        positionals: None,
        options: HashMap::new(),
        flags: vec![],
    }
}

/// A flattened command rule: one name prefix, a fallback action, a body.
pub fn command_rule(name: &str, fallback: Action, config: RuleConfig) -> Rule {
    Rule {
        name: name.to_string(),
        action: fallback,
        reason: None,
        config,
    }
}

/// `positionals = { default_perm = [...], overrides = { ... } }`.
pub fn positionals(
    default_perm: &[&str],
    overrides: &[(&str, &[&str])],
) -> tabit_gate::config::PositionalConfig {
    tabit_gate::config::PositionalConfig {
        default_perm: default_perm.iter().map(|p| (*p).to_string()).collect(),
        overrides: overrides
            .iter()
            .map(|(index, perms)| {
                (
                    (*index).to_string(),
                    perms.iter().map(|p| (*p).to_string()).collect(),
                )
            })
            .collect(),
    }
}

/// Option permission entries: `options = { "-o" = ["write"] }`.
pub fn option_perms(permissions: &[&str]) -> Vec<String> {
    permissions.iter().map(|p| (*p).to_string()).collect()
}

/// Flag entries without reasons.
pub fn flags(entries: &[(&str, Action)]) -> Vec<CommandFlag> {
    entries
        .iter()
        .map(|(flag, action)| CommandFlag {
            flag: (*flag).to_string(),
            action: *action,
            reason: None,
        })
        .collect()
}

/// Flag entries with reasons.
pub fn flags_with_reason(entries: &[(&str, Action, &str)]) -> Vec<CommandFlag> {
    entries
        .iter()
        .map(|(flag, action, reason)| CommandFlag {
            flag: (*flag).to_string(),
            action: *action,
            reason: Some((*reason).to_string()),
        })
        .collect()
}

/// Pre-check entries without reasons: `(env, match, action)`.
pub fn pre_checks(entries: &[(&str, &str, Action)]) -> Vec<PreCheck> {
    entries
        .iter()
        .map(|(env, pattern, action)| PreCheck {
            env: (*env).to_string(),
            match_: (*pattern).to_string(),
            action: *action,
            reason: None,
        })
        .collect()
}

/// A single pre-check carrying its reason.
pub fn pre_check_reason(env: &str, pattern: &str, action: Action, reason: &str) -> PreCheck {
    PreCheck {
        env: env.to_string(),
        match_: pattern.to_string(),
        action,
        reason: Some(reason.to_string()),
    }
}

/// Tool-rule entries: `(tool name, [(param, check)])`.
pub fn tool_rules(entries: &[(&str, Vec<ToolParamCheck>)]) -> ToolsConfig {
    ToolsConfig {
        rules: entries
            .iter()
            .map(|(name, checks)| ((*name).to_string(), checks.clone()))
            .collect(),
    }
}

/// One tool parameter check.
pub fn param_check(param: &str, check: tabit_gate::config::CheckKind) -> ToolParamCheck {
    ToolParamCheck {
        param: param.to_string(),
        check,
    }
}

// ---------------------------------------------------------------------------
// Tool-call inputs and assertions
// ---------------------------------------------------------------------------

/// A tool-call input map from string pairs.
pub fn input(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

/// Assert the action with context.
pub fn assert_action(result: &CheckResult, expected: Action, context: &str) {
    assert_eq!(result.action, expected, "{context}");
}
