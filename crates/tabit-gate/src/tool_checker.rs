//! Generic tool-call checker — a faithful port of pi-sanity's
//! `tool-checker.ts`. Looks up a tool name in the configured tool
//! rules, runs the requested checks (read/write/bash) against the
//! named parameters, and returns the aggregated result.

use serde_json::{Map, Value};

use crate::checker_bash::check_bash;
use crate::checker_read::check_read;
use crate::checker_write::check_write;
use crate::config::SanityConfig;
use crate::types::{Action, CheckResult, aggregate_results};

/// The string values of one tool-call parameter (TS
/// `normalizeParamValue`): a non-empty string, or the non-empty
/// strings of an array.
fn normalize_param_value(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        _ => vec![],
    }
}

/// Check one tool call. `None` when no tool rule matches the tool
/// name; `Some(allow)` when rules matched but nothing tripped.
pub fn check_tool_call(
    tool_name: &str,
    input: &Map<String, Value>,
    config: &SanityConfig,
) -> Option<CheckResult> {
    let checks = config.tools.rules.get(tool_name)?;
    let checks = checks.as_slice();

    let mut results: Vec<CheckResult> = vec![];

    for check in checks {
        let values = normalize_param_value(input.get(&check.param));
        if values.is_empty() {
            // Missing/empty param: pass through for this check. The
            // framework should validate required arguments before
            // invoking the tool.
            continue;
        }

        for value in values {
            let check_result = match check.check {
                crate::config::CheckKind::Read => check_read(&value, config),
                crate::config::CheckKind::Write => check_write(&value, config),
                crate::config::CheckKind::Bash => check_bash(&value, config),
            };
            if check_result.action != Action::Allow {
                results.push(check_result);
            }
        }
    }

    if results.is_empty() {
        return Some(CheckResult::allow());
    }

    Some(aggregate_results(results))
}

/// Build a human-readable description of the parameters being checked.
/// Used in the confirmation dialog title (TS `buildToolDetails`).
pub fn build_tool_details(
    tool_name: &str,
    input: &Map<String, Value>,
    config: &SanityConfig,
) -> String {
    let Some(checks) = config.tools.rules.get(tool_name) else {
        return format!("Tool: {tool_name}");
    };

    // Group checks by parameter so a single param checked as both read
    // and write is shown once with all its check types.
    let mut grouped: Vec<(String, Vec<&str>)> = vec![];
    for check in checks.iter() {
        if let Some((_, list)) = grouped.iter_mut().find(|(param, _)| *param == check.param) {
            if !list.contains(&check.check.as_str()) {
                list.push(check.check.as_str());
            }
        } else {
            grouped.push((check.param.clone(), vec![check.check.as_str()]));
        }
    }

    let mut lines: Vec<String> = vec![format!("Tool: {tool_name}")];
    for (param, check_types) in &grouped {
        let Some(value) = input.get(param) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        let display = match value {
            Value::String(s) if !s.is_empty() => s.clone(),
            Value::Array(items) => {
                if items.is_empty() || items.iter().any(|v| !v.is_string()) {
                    continue;
                }
                items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
            Value::String(_) => continue, // empty string, skipped like TS
            _ => continue,                // non-string, skipped like TS
        };
        lines.push(format!("  {param} ({}): {display}", check_types.join(", ")));
    }

    lines.join("\n")
}
