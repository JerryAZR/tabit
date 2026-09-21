//! Bash checker - validates bash commands against config (a faithful
//! port of pi-sanity's `checker-bash.ts`)
//!
//! 1. Parses the command (brush-parser, via the walker adapter)
//! 2. Runs pre-checks (env conditions)
//! 3. Finds matching rule for the command
//! 4. Parses arguments (flags, options, positionals)
//! 5. Checks each path against appropriate permissions (read/write)
//! 6. Returns the strictest action

use crate::arg_parser::parse_args;
use crate::bash_walker::{FoundCommand, walk_in_context};
use crate::config::{Rule, SanityConfig};
use crate::path_permission::{PathContext, check_read, check_write, default_context};
use crate::path_utils::clearly_not_a_path;
use crate::pre_check::evaluate_pre_checks;
use crate::types::{Action, CheckResult, aggregate_results};

/// Check if a bash command is allowed (TS `checkBash`).
pub fn check_bash(command: impl AsRef<str>, config: &SanityConfig) -> CheckResult {
    let command = command.as_ref();
    if command.trim().is_empty() {
        return CheckResult::allow();
    }

    let base_ctx = default_context();
    let walk_result = walk_in_context(command, &base_ctx);

    if !walk_result.errors.is_empty() {
        let error_messages = walk_result.errors.join("; ");
        return CheckResult {
            action: Action::Deny,
            reason: Some(format!("Invalid bash syntax: {error_messages}")),
        };
    }

    let results: Vec<CheckResult> = walk_result
        .commands
        .iter()
        .map(|cmd| check_single_command(cmd, config, &base_ctx))
        .collect();

    if results.is_empty() {
        return CheckResult::allow();
    }

    aggregate_results(results)
}

/// Check if a normalized command matches a rule name prefix. Word
/// boundary: "git" matches "git push" but not "github".
fn matches_prefix(normalized: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if !normalized.starts_with(prefix) {
        return false;
    }
    match normalized[prefix.len()..].chars().next() {
        None => true,
        Some(next_char) => next_char == ' ',
    }
}

/// Find the first matching rule. Rules are stored so that later source
/// rules come first: the first match wins.
fn find_matching_rule<'a>(normalized: &str, rules: &'a [Rule]) -> Option<&'a Rule> {
    rules
        .iter()
        .find(|rule| matches_prefix(normalized, &rule.name))
}

/// Check a single parsed command. The effective path context uses the
/// command's tracked cwd (followed through cd by the walker) instead
/// of the process cwd.
fn check_single_command(
    cmd: &FoundCommand,
    config: &SanityConfig,
    base_ctx: &PathContext,
) -> CheckResult {
    let command_name = cmd.name.clone().unwrap_or_default();
    let normalized = if cmd.args.is_empty() {
        command_name
    } else {
        format!("{command_name} {}", cmd.args.join(" "))
    };
    let rule = find_matching_rule(&normalized, &config.commands.rules);

    let ctx = PathContext {
        cwd: if cmd.cwd.is_empty() {
            base_ctx.cwd.clone()
        } else {
            cmd.cwd.clone()
        },
        ..base_ctx.clone()
    };

    let mut results: Vec<CheckResult> = vec![];

    // If no rule matches, still check redirects (they have their own
    // path permissions).
    let Some(rule) = rule else {
        results.extend(check_redirects(cmd, config, &ctx));
        if results.is_empty() {
            return CheckResult::new(config.commands.default_action, None);
        }
        return aggregate_results(results);
    };

    // 1. Evaluate pre-checks.
    if let Some((action, reasons)) = evaluate_pre_checks(&rule.config.pre_checks) {
        results.push(CheckResult {
            action,
            reason: if reasons.is_empty() {
                None
            } else {
                Some(reasons.join("; "))
            },
        });
    }

    // 2. Parse args (pure).
    let parsed = parse_args(&cmd.args, Some(&rule.config), &cmd.dynamic_indices);

    // 3. Flag actions.
    for flag_config in &rule.config.flags {
        if parsed.flags.contains(&flag_config.flag) {
            results.push(CheckResult {
                action: flag_config.action,
                reason: flag_config.reason.clone(),
            });
        }
    }

    // 4. Options — check consumed values against path permissions.
    for (opt_name, entry) in &parsed.options {
        if cmd.dynamic_indices.contains(&entry.original_index) {
            continue;
        }
        let Some(perms) = rule.config.options.get(opt_name) else {
            continue;
        };
        for perm in perms {
            let res = check_path_with_permission(&entry.value, perm, config, &ctx);
            if res.action != Action::Allow {
                results.push(res);
            }
        }
    }

    // 5. Positionals — check against index-based overrides.
    if let Some(positionals_config) = &rule.config.positionals {
        let count = parsed.positionals.len();
        for (i, (value, original_index)) in parsed.positionals.iter().enumerate() {
            let index_str = i.to_string();
            let neg_index_str = (i as isize - count as isize).to_string();

            let override_perm = positionals_config
                .overrides
                .get(&neg_index_str)
                .or_else(|| positionals_config.overrides.get(&index_str));
            let perm: &[String] = match override_perm {
                Some(perm) => perm,
                None => &positionals_config.default_perm,
            };

            if perm.is_empty() {
                continue;
            }
            if cmd.dynamic_indices.contains(original_index) {
                continue;
            }

            for p in perm {
                let res = check_path_with_permission(value, p, config, &ctx);
                if res.action != Action::Allow {
                    results.push(res);
                }
            }
        }
    }

    // 6. Redirects.
    results.extend(check_redirects(cmd, config, &ctx));

    // 7. If no specific checks applied, use rule's fallback action.
    if results.is_empty() {
        return CheckResult {
            action: rule.action,
            reason: rule.reason.clone(),
        };
    }

    aggregate_results(results)
}

/// Check a path with a specific permission type.
fn check_path_with_permission(
    path: &str,
    perm: &str,
    config: &SanityConfig,
    ctx: &PathContext,
) -> CheckResult {
    if clearly_not_a_path(path) {
        return CheckResult::allow();
    }

    match perm {
        "read" => {
            let result = check_read(path, config, Some(ctx));
            CheckResult {
                action: result.action,
                reason: result.reason,
            }
        }
        "write" | "delete" => {
            // "delete" is a write operation (modifies the parent
            // directory): an alias, as in TS.
            let result = check_write(path, config, Some(ctx));
            CheckResult {
                action: result.action,
                reason: result.reason,
            }
        }
        _ => CheckResult::allow(),
    }
}

/// Check redirects.
fn check_redirects(
    cmd: &FoundCommand,
    config: &SanityConfig,
    ctx: &PathContext,
) -> Vec<CheckResult> {
    let mut results: Vec<CheckResult> = vec![];

    for redirect in &cmd.redirects {
        if redirect.is_input {
            let result = check_read(&redirect.target, config, Some(ctx));
            if result.action != Action::Allow {
                results.push(CheckResult {
                    action: result.action,
                    reason: result.reason,
                });
            }
        } else if redirect.is_output {
            let result = check_write(&redirect.target, config, Some(ctx));
            if result.action != Action::Allow {
                results.push(CheckResult {
                    action: result.action,
                    reason: result.reason,
                });
            }
        }
    }

    results
}
