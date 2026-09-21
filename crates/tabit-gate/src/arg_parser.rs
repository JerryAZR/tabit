//! Pure argument parser for command-line arguments — a faithful port
//! of pi-sanity's `arg-parser.ts`.
//!
//! Separates argument parsing from permission checking. Given a rule
//! configuration and raw args, produces:
//! - flags: declared flags found in the command line
//! - options: declared option → consumed value
//! - positionals: positional arguments with original indices
//!
//! Single-pass left-to-right scan. Handles: exact flag match,
//! combined short flags (`-rf`), option with space separator, option
//! with equals separator, combined short string containing an option
//! (`-xzf`), atomic declared multi-char flags (`-Wall`), the
//! end-of-options marker (`--`), and dynamic args — tracked but still
//! participating in positional counting.

use std::collections::{BTreeSet, HashSet};

use crate::config::RuleConfig;

/// A consumed option value with the argument index it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionValue {
    pub value: String,
    pub original_index: usize,
}

/// The structured parse of one command's arguments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedArgs {
    /// Declared flags found (exact match or via combined-short
    /// decomposition).
    pub flags: BTreeSet<String>,
    /// Declared options found → consumed value, in first-appearance
    /// order (TS `Map` semantics: re-setting keeps the position).
    pub options: Vec<(String, OptionValue)>,
    /// Positional arguments with their original arg indices.
    pub positionals: Vec<(String, usize)>,
}

impl ParsedArgs {
    /// Record an option value; a repeat of the same option replaces
    /// the value in place (TS `Map.set`).
    fn set_option(&mut self, name: String, entry: OptionValue) {
        if let Some(existing) = self.options.iter_mut().find(|(n, _)| *n == name) {
            existing.1 = entry;
        } else {
            self.options.push((name, entry));
        }
    }
}

/// Parse command-line arguments according to a rule configuration.
///
/// Pure function: no side effects, no permission checking. It only
/// decides which tokens are flags, options, or positionals.
pub fn parse_args(
    args: &[String],
    cmd_config: Option<&RuleConfig>,
    _dynamic_indices: &HashSet<usize>, // In the TS signature; never consulted (the checker skips dynamic indices itself).
) -> ParsedArgs {
    let mut result = ParsedArgs::default();

    let declared_flags: BTreeSet<String> = cmd_config
        .map(|c| c.flags.iter().map(|f| f.flag.clone()).collect())
        .unwrap_or_default();
    let declared_options: BTreeSet<String> = cmd_config
        .map(|c| c.options.keys().cloned().collect())
        .unwrap_or_default();

    let mut pending_option: Option<String> = None;

    for (i, arg) in args.iter().enumerate() {
        // 1. Consume pending option value.
        if let Some(option) = pending_option.take() {
            result.set_option(
                option,
                OptionValue {
                    value: arg.clone(),
                    original_index: i,
                },
            );
            continue;
        }

        // 2. End-of-options marker: everything after -- is positional.
        if arg == "--" {
            for (j, rest) in args.iter().enumerate().skip(i + 1) {
                result.positionals.push((rest.clone(), j));
            }
            break;
        }

        // 3. Exact declared flag match.
        if declared_flags.contains(arg) {
            result.flags.insert(arg.clone());
            continue;
        }

        // 4. Exact declared option match.
        if declared_options.contains(arg) {
            pending_option = Some(arg.clone());
            continue;
        }

        // 5. -o=value and --option=value forms.
        if arg.starts_with('-') && arg.contains('=') {
            let eq_index = arg.find('=').unwrap_or(0);
            let key = &arg[..eq_index];
            let value = &arg[eq_index + 1..];
            if declared_options.contains(key) {
                result.set_option(
                    key.to_string(),
                    OptionValue {
                        value: value.to_string(),
                        original_index: i,
                    },
                );
                continue;
            }
            // If key is not a declared option, fall through:
            // - short form may be combined-short (-xzf) handled below
            // - long form is unknown, skip
            if arg.starts_with("--") {
                continue;
            }
            // short form with = but unknown option: fall through to
            // combined-short.
        }

        // 6. Long flag/option (unknown) — skip.
        if arg.starts_with("--") {
            continue;
        }

        // 7. Combined short string (-xzf).
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 2 {
            // If it's a declared multi-char flag, it's atomic.
            if declared_flags.contains(arg) {
                result.flags.insert(arg.clone());
                continue;
            }

            // Scan characters left-to-right. If a declared option
            // appears before later flags in the same combined token
            // (e.g. -fr where -f is an option), we break at the
            // option and consume the next argument as its value; the
            // trailing flags are NOT processed. The common convention
            // is to write the option last (e.g. -rf value).
            let chars: Vec<char> = arg[1..].chars().collect();
            let mut consumed_next = false;
            for ch in chars {
                let short = format!("-{ch}");

                if declared_options.contains(&short) {
                    // Option in combined string: consume next arg as
                    // value. If there is no next arg, the option has
                    // no value (edge case, dropped like in TS).
                    pending_option = Some(short);
                    consumed_next = true;
                    break;
                }

                if declared_flags.contains(&short) {
                    result.flags.insert(short);
                }
                // Unknown char: ignore.
            }

            if consumed_next {
                continue;
            }

            // No option found, only flags (or unknown chars) — already
            // processed.
            continue;
        }

        // 8. Single-dash unknown (-x) — skip.
        if arg.starts_with('-') {
            continue;
        }

        // 9. Positional.
        result.positionals.push((arg.clone(), i));
    }

    // Trailing pending option (no value provided) is dropped — an
    // option without value is not useful.

    result
}

#[cfg(test)]
mod tests {
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

    use super::*;
    use crate::config::{CommandFlag, RuleConfig};
    use crate::types::Action;
    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn config_with(flags: &[&str], options: &[&str]) -> RuleConfig {
        RuleConfig {
            flags: flags
                .iter()
                .map(|f| CommandFlag {
                    flag: f.to_string(),
                    action: Action::Ask,
                    reason: None,
                })
                .collect(),
            options: options
                .iter()
                .map(|o| (o.to_string(), vec!["read".to_string()]))
                .collect(),
            ..RuleConfig::default()
        }
    }

    fn no_indices(_len: usize) -> HashSet<usize> {
        HashSet::new()
    }

    #[test]
    fn classifies_flags_options_and_positionals() {
        let config = config_with(&["-f"], &["-o"]);
        let parsed = parse_args(
            &args(&["-f", "-o", "value", "file"]),
            Some(&config),
            &no_indices(4),
        );
        assert_eq!(parsed.flags, BTreeSet::from(["-f".to_string()]));
        // The consumed value's own index is recorded (index 2).
        assert_eq!(
            parsed.options,
            vec![(
                "-o".to_string(),
                OptionValue {
                    value: "value".into(),
                    original_index: 2
                }
            )]
        );
        assert_eq!(parsed.positionals, vec![("file".to_string(), 3)]);
    }

    #[test]
    fn equals_form_and_unknown_long_options() {
        let config = config_with(&[], &["--target"]);
        let parsed = parse_args(
            &args(&["--target=/etc", "--unknown"]),
            Some(&config),
            &no_indices(2),
        );
        assert_eq!(
            parsed.options,
            vec![(
                "--target".to_string(),
                OptionValue {
                    value: "/etc".into(),
                    original_index: 0
                }
            )]
        );
        assert!(parsed.positionals.is_empty(), "unknown long option skipped");
    }

    #[test]
    fn combined_shorts_decompose_and_options_consume_next() {
        let config = config_with(&["-r", "-x"], &["-f"]);
        let parsed = parse_args(&args(&["-rx"]), Some(&config), &no_indices(1));
        assert_eq!(
            parsed.flags,
            BTreeSet::from(["-r".to_string(), "-x".to_string()])
        );

        // Option last in the combined token consumes the next argument.
        let parsed = parse_args(&args(&["-rf", "val"]), Some(&config), &no_indices(2));
        assert_eq!(parsed.flags, BTreeSet::from(["-r".to_string()]));
        assert_eq!(
            parsed.options,
            vec![(
                "-f".to_string(),
                OptionValue {
                    value: "val".into(),
                    original_index: 1
                }
            )]
        );
    }

    #[test]
    fn end_of_options_marker_makes_everything_positional() {
        let config = config_with(&["-f"], &[]);
        let parsed = parse_args(&args(&["--", "-f", "file"]), Some(&config), &no_indices(3));
        assert!(parsed.flags.is_empty());
        assert_eq!(
            parsed.positionals,
            vec![("-f".to_string(), 1), ("file".to_string(), 2)]
        );
    }
}
