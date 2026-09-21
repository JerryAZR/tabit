//! Configuration types and loader — ported from pi-sanity's
//! `config-types.ts`, `config-loader.ts` and `config-manager.ts`.
//!
//! v1 scope (per the porting map): rules-as-data and the parsed
//! default rule set only — the multi-source `loadConfig` merge and the
//! mtime-tracking `ConfigManager` arrive with the settings layers.
//! What is kept from the loader: parsing rules-as-data out of TOML,
//! the backwards rule parse (later source rules first, first-match
//! wins at check time), the `names = [""]` catch-all, the tool-rules
//! table, and load-time pattern preprocessing.
//!
//! The shipped default rule set is `default-config.toml` next to this
//! module — extracted verbatim from pi-sanity's
//! `generated/default-config.ts` (itself generated from
//! `default-config.toml`), embedded with `include_str!` and parsed at
//! load, exactly as TS parses `DEFAULT_CONFIG_CONTENT`.

use std::collections::HashMap;

use crate::path_utils::{PathContext, preprocess_config_pattern};
use crate::types::Action;

/// Sink for non-fatal config warnings (TS `WarningSink`); TS falls
/// back to `console.warn`, the loader here to `eprintln!`.
pub type WarningSink<'a> = &'a mut dyn FnMut(&str);

fn default_sink(msg: &str) {
    eprintln!("{msg}");
}

// ─────────────────────────────────────────────────────────────────────────────
// Config vocabulary (config-types.ts)
// ─────────────────────────────────────────────────────────────────────────────

/// One path override within a permission section.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverrideRule {
    pub path: Vec<String>,
    pub action: Action,
    pub reason: Option<String>,
}

/// A read/write permission section.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionSection {
    pub default: Action,
    pub reason: Option<String>,
    pub overrides: Vec<OverrideRule>,
}

/// Environment pre-check (the only implemented pre-check type; user
/// config uses `pre_checks` for future types too).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreCheck {
    pub env: String,
    /// The `match` key in TOML.
    pub match_: String,
    pub action: Action,
    pub reason: Option<String>,
}

/// Positional configuration: a default permission for all positionals
/// plus index overrides (`"-1"` = last, `"0"`-based otherwise). An
/// empty permission list means the positional is not path-checked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PositionalConfig {
    /// e.g. `["read"]`, `["read", "write"]`, or `[]` for none.
    pub default_perm: Vec<String>,
    pub overrides: HashMap<String, Vec<String>>,
}

/// One declared flag with its action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandFlag {
    pub flag: String,
    pub action: Action,
    pub reason: Option<String>,
}

/// The body of a command rule: positionals, options, flags,
/// pre_checks. The rule's fallback action lives at [`Rule::action`],
/// not here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleConfig {
    pub reason: Option<String>,
    pub pre_checks: Vec<PreCheck>,
    pub positionals: Option<PositionalConfig>,
    pub options: HashMap<String, Vec<String>>,
    pub flags: Vec<CommandFlag>,
}

/// A single flattened rule with one name, created from a
/// `[[commands.rules]]` `names` entry at parse time. Rules are stored
/// in check order: later source rules first (last-match-wins via
/// array order, no priority numbers needed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub name: String,
    /// Fallback when no checks trigger.
    pub action: Action,
    pub reason: Option<String>,
    pub config: RuleConfig,
}

/// The commands domain: default action + ordered rule list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandsConfig {
    pub default_action: Action,
    pub reason: Option<String>,
    pub rules: Vec<Rule>,
}

/// A permission domain (read/write).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionsConfig {
    pub read: PermissionSection,
    pub write: PermissionSection,
}

/// Which permission domain a tool parameter check runs against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CheckKind {
    Read,
    Write,
    Bash,
}

impl CheckKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckKind::Read => "read",
            CheckKind::Write => "write",
            CheckKind::Bash => "bash",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(CheckKind::Read),
            "write" => Some(CheckKind::Write),
            "bash" => Some(CheckKind::Bash),
            _ => None,
        }
    }
}

/// One parameter check inside a tool rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolParamCheck {
    pub param: String,
    pub check: CheckKind,
}

/// Tool rules map a tool name to the parameter checks that should run.
/// Exact tool-name matching; later TOML rules overwrite earlier ones
/// (last-match-wins, TS `Map` semantics).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolsConfig {
    pub rules: HashMap<String, Vec<ToolParamCheck>>,
}

/// The parsed gate configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SanityConfig {
    pub permissions: PermissionsConfig,
    pub commands: CommandsConfig,
    pub tools: ToolsConfig,
    /// Timeout in seconds for "ask" prompts (default: 30).
    pub ask_timeout: Option<u64>,
}

impl Default for SanityConfig {
    /// The empty config structure (TS `createEmptyConfig`).
    fn default() -> Self {
        SanityConfig {
            permissions: PermissionsConfig {
                read: PermissionSection {
                    default: Action::Allow,
                    reason: None,
                    overrides: vec![],
                },
                write: PermissionSection {
                    default: Action::Allow,
                    reason: None,
                    overrides: vec![],
                },
            },
            commands: CommandsConfig {
                default_action: Action::Allow,
                reason: Some("Unknown commands default to allow (low-friction)".to_string()),
                rules: vec![],
            },
            tools: ToolsConfig {
                rules: HashMap::new(),
            },
            ask_timeout: None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TOML → runtime config (config-loader.ts)
// ─────────────────────────────────────────────────────────────────────────────

/// Config-load context for pattern preprocessing (TS
/// `createConfigContext`): patterns are preprocessed before any repo
/// is known, so `{{REPO}}` falls back to cwd at load time.
fn create_config_context() -> PathContext {
    PathContext {
        cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string()),
        home: home::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_string()),
        tmpdir: std::env::temp_dir().to_string_lossy().into_owned(),
        repo: None,
        platform: crate::path_utils::Platform::native(),
    }
}

fn value_str<'a>(value: &'a toml::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(|v| v.as_str())
}

fn string_array(value: &toml::Value, key: &str) -> Option<Vec<String>> {
    value.get(key).and_then(|v| v.as_array()).map(|items| {
        items
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    })
}

/// Parse an action string, warning and falling back to `allow` on
/// invalid values. TS carries invalid strings through untyped (and
/// they misbehave in comparisons); this typed boundary warns instead.
fn parse_action_or_warn(value: Option<&str>, sink: &mut dyn FnMut(&str)) -> Action {
    match value {
        Some(s) => match Action::parse(s) {
            Some(action) => action,
            None => {
                sink(&format!(
                    "[pi-sanity] Invalid action \"{s}\", using \"allow\""
                ));
                Action::Allow
            }
        },
        None => Action::Allow,
    }
}

/// Filter and preprocess one section's overrides at load time (TS
/// `filterValidOverrides`), keeping TS's skip-and-warn behavior and
/// message texts.
fn filter_valid_overrides(
    overrides: &[toml::Value],
    section_name: &str,
    ctx: &PathContext,
    sink: &mut dyn FnMut(&str),
) -> Vec<OverrideRule> {
    let mut valid = vec![];
    for (i, raw) in overrides.iter().enumerate() {
        let Some(table) = raw.as_table() else {
            sink(&format!(
                "[pi-sanity] Skipping invalid override #{i} in [permissions.{section_name}]: not an object"
            ));
            continue;
        };
        let path = table.get("path").and_then(|v| v.as_array());
        let Some(path_items) = path else {
            sink(&format!(
                "[pi-sanity] Skipping invalid override #{i} in [permissions.{section_name}]: missing or invalid 'path' (expected array)"
            ));
            continue;
        };
        let action = table.get("action").and_then(|v| v.as_str());
        let Some(action_str) = action else {
            sink(&format!(
                "[pi-sanity] Skipping invalid override #{i} in [permissions.{section_name}]: missing or invalid 'action' (got: {})",
                table
                    .get("action")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "undefined".into())
            ));
            continue;
        };
        let Some(action) = Action::parse(action_str) else {
            sink(&format!(
                "[pi-sanity] Skipping invalid override #{i} in [permissions.{section_name}]: missing or invalid 'action' (got: {action_str})"
            ));
            continue;
        };

        valid.push(OverrideRule {
            path: path_items
                .iter()
                .filter_map(|p| p.as_str().map(|p| preprocess_config_pattern(p, ctx)))
                .filter(|p| !p.is_empty())
                .collect(),
            action,
            reason: table
                .get("reason")
                .and_then(|v| v.as_str())
                .map(String::from),
        });
    }
    valid
}

/// Build the tool-rules table (TS `buildToolsConfig`, messages kept).
fn build_tools_config(raw_tools: &toml::Value, sink: &mut dyn FnMut(&str)) -> ToolsConfig {
    let mut rules: HashMap<String, Vec<ToolParamCheck>> = HashMap::new();
    let raw_rules = raw_tools
        .get("rules")
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);

    for (i, raw_rule) in raw_rules.iter().enumerate() {
        if raw_rule.as_table().is_none() {
            sink(&format!(
                "[pi-sanity] Skipping invalid tool rule #{i}: not an object"
            ));
            continue;
        }
        let names = string_array(raw_rule, "names");
        if names.as_ref().is_none_or(|n| n.is_empty()) {
            sink(&format!(
                "[pi-sanity] Skipping invalid tool rule #{i}: missing or invalid 'names' (expected array)"
            ));
            continue;
        }
        if names
            .as_ref()
            .is_some_and(|n| n.iter().any(|name| name.is_empty()))
        {
            sink(&format!(
                "[pi-sanity] Skipping invalid tool rule #{i}: \"\" is not allowed in tool names"
            ));
            continue;
        }
        let raw_checks = raw_rule.get("checks").and_then(|v| v.as_array());
        let Some(raw_checks) = raw_checks else {
            sink(&format!(
                "[pi-sanity] Skipping invalid tool rule #{i}: missing or invalid 'checks' (expected array)"
            ));
            continue;
        };
        if raw_checks.is_empty() {
            sink(&format!(
                "[pi-sanity] Skipping invalid tool rule #{i}: missing or invalid 'checks' (expected array)"
            ));
            continue;
        }

        let mut checks: Vec<ToolParamCheck> = vec![];
        for (j, raw_check) in raw_checks.iter().enumerate() {
            let Some(check_table) = raw_check.as_table() else {
                sink(&format!(
                    "[pi-sanity] Skipping invalid check #{j} in tool rule #{i}: not an object"
                ));
                continue;
            };
            let Some(param) = check_table.get("param").and_then(|v| v.as_str()) else {
                sink(&format!(
                    "[pi-sanity] Skipping invalid check #{j} in tool rule #{i}: missing or invalid 'param'"
                ));
                continue;
            };
            if param.is_empty() {
                sink(&format!(
                    "[pi-sanity] Skipping invalid check #{j} in tool rule #{i}: missing or invalid 'param'"
                ));
                continue;
            }
            let Some(check) = check_table.get("check").and_then(|v| v.as_str()) else {
                sink(&format!(
                    "[pi-sanity] Skipping invalid check #{j} in tool rule #{i}: unsupported check \"{}\" (expected read, write, or bash)",
                    check_table
                        .get("check")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "undefined".into())
                ));
                continue;
            };
            let Some(kind) = CheckKind::parse(check) else {
                sink(&format!(
                    "[pi-sanity] Skipping invalid check #{j} in tool rule #{i}: unsupported check \"{check}\" (expected read, write, or bash)"
                ));
                continue;
            };
            checks.push(ToolParamCheck {
                param: param.to_string(),
                check: kind,
            });
        }

        if checks.is_empty() {
            sink(&format!(
                "[pi-sanity] Skipping tool rule #{i}: no valid checks remain"
            ));
            continue;
        }

        for name in names.unwrap_or_default() {
            if name.is_empty() {
                sink(&format!(
                    "[pi-sanity] Skipping invalid name in tool rule #{i}: expected non-empty string"
                ));
                continue;
            }
            rules.insert(name, checks.clone());
        }
    }

    ToolsConfig { rules }
}

/// Build one permission section (defaults + filtered, preprocessed
/// overrides).
fn build_permission_section(
    raw: &toml::Value,
    section_name: &str,
    ctx: &PathContext,
    sink: &mut dyn FnMut(&str),
) -> PermissionSection {
    // Missing default falls back to "allow" silently (TS `?? "allow"`).
    let default = match raw.get("default").and_then(|v| v.as_str()) {
        Some(s) => parse_action_or_warn(Some(s), sink),
        None => Action::Allow,
    };
    let overrides = raw
        .get("overrides")
        .and_then(|v| v.as_array())
        .map(|a| filter_valid_overrides(a, section_name, ctx, sink))
        .unwrap_or_default();
    PermissionSection {
        default,
        reason: value_str(raw, "reason").map(String::from),
        overrides,
    }
}

/// Parse a `[[commands.rules]]` entry body into a [`RuleConfig`].
/// TS passes these fields through untyped; the typed boundary here
/// skips malformed entries with a warning.
fn build_rule_config(
    raw_rule: &toml::Value,
    index: usize,
    sink: &mut dyn FnMut(&str),
) -> RuleConfig {
    let mut pre_checks: Vec<PreCheck> = vec![];
    if let Some(raw_checks) = raw_rule.get("pre_checks").and_then(|v| v.as_array()) {
        for raw_check in raw_checks {
            if raw_check.as_table().is_none() {
                sink(&format!(
                    "[pi-sanity] Skipping invalid pre-check in rule #{index}: not an object"
                ));
                continue;
            }
            let Some(action) = value_str(raw_check, "action").and_then(Action::parse) else {
                sink(&format!(
                    "[pi-sanity] Skipping invalid pre-check in rule #{index}: missing or invalid 'action'"
                ));
                continue;
            };
            pre_checks.push(PreCheck {
                env: value_str(raw_check, "env").unwrap_or_default().to_string(),
                match_: value_str(raw_check, "match")
                    .unwrap_or_default()
                    .to_string(),
                action,
                reason: value_str(raw_check, "reason").map(String::from),
            });
        }
    }

    let mut positionals: Option<PositionalConfig> = None;
    if let Some(raw_positionals) = raw_rule.get("positionals") {
        let mut config = PositionalConfig::default();
        if let Some(perm) = raw_positionals
            .get("default_perm")
            .and_then(|v| v.as_array())
        {
            config.default_perm = perm
                .iter()
                .filter_map(|p| p.as_str().map(String::from))
                .collect();
        }
        if let Some(overrides) = raw_positionals.get("overrides").and_then(|v| v.as_table()) {
            let mut map = HashMap::new();
            for (key, value) in overrides {
                if let Some(perms) = value.as_array() {
                    map.insert(
                        key.clone(),
                        perms
                            .iter()
                            .filter_map(|p| p.as_str().map(String::from))
                            .collect(),
                    );
                }
            }
            config.overrides = map;
        }
        positionals = Some(config);
    }

    let mut options: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(raw_options) = raw_rule.get("options").and_then(|v| v.as_table()) {
        for (key, value) in raw_options {
            if let Some(perms) = value.as_array() {
                options.insert(
                    key.clone(),
                    perms
                        .iter()
                        .filter_map(|p| p.as_str().map(String::from))
                        .collect(),
                );
            }
        }
    }

    let mut flags: Vec<CommandFlag> = vec![];
    if let Some(raw_flags) = raw_rule.get("flags").and_then(|v| v.as_array()) {
        for raw_flag in raw_flags {
            if raw_flag.as_table().is_none() {
                sink(&format!(
                    "[pi-sanity] Skipping invalid flag in rule #{index}: not an object"
                ));
                continue;
            }
            let Some(flag) = value_str(raw_flag, "flag") else {
                sink(&format!(
                    "[pi-sanity] Skipping invalid flag in rule #{index}: missing 'flag'"
                ));
                continue;
            };
            let Some(action) = value_str(raw_flag, "action").and_then(Action::parse) else {
                sink(&format!(
                    "[pi-sanity] Skipping invalid flag in rule #{index}: missing or invalid 'action'"
                ));
                continue;
            };
            flags.push(CommandFlag {
                flag: flag.to_string(),
                action,
                reason: value_str(raw_flag, "reason").map(String::from),
            });
        }
    }

    RuleConfig {
        reason: value_str(raw_rule, "reason").map(String::from),
        pre_checks,
        positionals,
        options,
        flags,
    }
}

/// Build the runtime config from the parsed TOML (TS
/// `buildSanityConfig`): backwards rules parse with catch-all
/// handling, tool rules, and load-time pattern preprocessing.
fn build_sanity_config(
    raw: &toml::Value,
    on_warning: Option<&mut dyn FnMut(&str)>,
) -> SanityConfig {
    let mut eprint_sink = default_sink;
    let mut sink: &mut dyn FnMut(&str) = match on_warning {
        Some(w) => w,
        None => &mut eprint_sink,
    };
    let empty = toml::Value::Boolean(false); // Stand-in never read as a table.
    let raw_commands = raw.get("commands").unwrap_or(&empty);
    let raw_tools = raw.get("tools").unwrap_or(&empty);

    // Warn about unknown keys in [commands] (e.g. old format
    // [commands.NAME]).
    if let Some(table) = raw_commands.as_table() {
        for key in table.keys() {
            if !["default", "default_action", "reason", "rules"].contains(&key.as_str()) {
                sink(&format!(
                    "[pi-sanity] Ignoring unsupported key \"{key}\" in [commands]. \
                     If you meant to define a command rule, use [[commands.rules]] with \
                     names = [\"{key}\"]. Use /skill:sanity-config for assistance."
                ));
            }
        }
    }

    // Parse commands.rules backwards. Later rules in the source array
    // win over earlier ones. A catch-all (names = [""]) discards all
    // rules that came before it and may change the default action for
    // rules after it.
    let raw_rules = raw_commands
        .get("rules")
        .and_then(|v| v.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);
    let mut rules: Vec<Rule> = vec![];
    let mut catch_all_seen = false;
    let mut default_action = parse_action_or_warn(value_str(raw_commands, "default"), &mut sink);
    let mut reason = value_str(raw_commands, "reason").map(String::from);

    for (i, raw_rule) in raw_rules.iter().enumerate().rev() {
        let Some(names) = string_array(raw_rule, "names") else {
            continue;
        };
        if names.is_empty() {
            continue;
        }

        // Reject mixed names arrays containing "" — user almost
        // certainly made a mistake. Only exact names = [""] is valid
        // catch-all syntax.
        if names.iter().any(|name| name.is_empty()) {
            if names.len() > 1 {
                sink(&format!(
                    "[pi-sanity] Skipping invalid rule #{i}: \"\" must be the only element in names. \
                     Use separate [[commands.rules]] entries for catch-all and named rules."
                ));
            } else {
                // Exact names = [""] → catch-all.
                catch_all_seen = true;
                if let Some(action) = raw_rule.get("action").and_then(|v| v.as_str()) {
                    match Action::parse(action) {
                        Some(parsed) => default_action = parsed,
                        None => sink(&format!(
                            "[pi-sanity] Invalid catch-all action \"{action}\" in rule #{i}, keeping previous default"
                        )),
                    }
                }
                if let Some(catch_all_reason) = value_str(raw_rule, "reason") {
                    reason = Some(catch_all_reason.to_string());
                }
            }
            continue;
        }

        // Rules before the catch-all are discarded.
        if catch_all_seen {
            continue;
        }

        let rule_config = build_rule_config(raw_rule, i, &mut sink);
        let rule_action = match value_str(raw_rule, "action") {
            Some(action_str) => match Action::parse(action_str) {
                Some(parsed) => parsed,
                None => {
                    sink(&format!(
                        "[pi-sanity] Invalid action \"{action_str}\" in rule #{i}, using the default"
                    ));
                    default_action
                }
            },
            None => default_action,
        };
        let rule_reason = value_str(raw_rule, "reason").map(String::from);

        for name in names {
            rules.push(Rule {
                name,
                action: rule_action,
                reason: rule_reason.clone(),
                config: rule_config.clone(),
            });
        }
    }

    // Rules are already in check order: later source rules first.
    let tools = build_tools_config(raw_tools, &mut sink);

    let raw_permissions = raw.get("permissions").unwrap_or(&empty);
    let read = raw_permissions.get("read").unwrap_or(&empty);
    let write = raw_permissions.get("write").unwrap_or(&empty);

    let ctx = create_config_context();
    let permissions = PermissionsConfig {
        read: build_permission_section(read, "read", &ctx, &mut sink),
        write: build_permission_section(write, "write", &ctx, &mut sink),
    };

    SanityConfig {
        permissions,
        commands: CommandsConfig {
            default_action,
            reason,
            rules,
        },
        tools,
        ask_timeout: raw
            .get("ask_timeout")
            .and_then(|v| v.as_integer())
            .and_then(|i| u64::try_from(i).ok()),
    }
}

/// The shipped default rule set, verbatim from pi-sanity's
/// `generated/default-config.ts` (see the module doc).
pub(crate) const DEFAULT_CONFIG_CONTENT: &str = include_str!("default_config.toml");

/// Parse the embedded default config (TS `loadDefaultConfig`).
///
/// # Panics
///
/// Only if the compiled-in TOML is malformed — a build-time invariant;
/// the content is extracted verbatim from the tested upstream file.
pub fn default_config() -> SanityConfig {
    match load_from_string(DEFAULT_CONFIG_CONTENT, None) {
        Ok(config) => config,
        Err(err) => {
            // The embedded TOML is compiled in and verified by the
            // upstream test corpus; a parse failure is a build bug —
            // the sanctioned loud crash.
            #[allow(clippy::panic)]
            {
                panic!("built-in default gate config failed to parse: {err}");
            }
        }
    }
}

/// Parse config from a TOML string (TS `loadConfigFromString`).
/// Invalid TOML is a typed error; invalid entries inside valid TOML
/// are skipped with warnings through the sink.
pub fn load_from_string(
    toml_content: &str,
    on_warning: Option<&mut dyn FnMut(&str)>,
) -> Result<SanityConfig, String> {
    let parsed: toml::Value =
        toml::from_str(toml_content).map_err(|err| format!("invalid TOML: {err}"))?;
    Ok(build_sanity_config(&parsed, on_warning))
}
