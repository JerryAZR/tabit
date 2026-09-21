//! Argument parsing behavior observed through the bash checker
//! (ported from pi-sanity `tests/unit/bash/arg-parsing.test.ts`):
//! flag detection, option value extraction, positional counting,
//! dynamic-arg skipping, and merged short-option strings. The config
//! shape of each case makes exactly one path trigger a non-allow
//! result, so a wrong routing decision flips the verdict.

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

use std::collections::HashSet;

use common::{
    action_rule, allow_rule, assert_action, config_with_command_rule, flags, option_perms,
    positionals, rule_config,
};
use tabit_gate::bash_walker;
use tabit_gate::checker_bash::check_bash;
use tabit_gate::config::SanityConfig;
use tabit_gate::types::{Action, CheckResult};

/// A config where ONLY `pattern` triggers `action` in the given
/// permission domain (read = false, write = true).
fn specific_path_config(
    cmd: &str,
    rule: tabit_gate::config::RuleConfig,
    write: bool,
    pattern: &str,
    action: Action,
) -> SanityConfig {
    let mut config = config_with_command_rule(cmd, Action::Allow, rule);
    let section = if write {
        &mut config.permissions.write
    } else {
        &mut config.permissions.read
    };
    section.default = Action::Allow;
    section.overrides.push(action_rule(&[pattern], action));
    config
}

/// A config with deny defaults and allow overrides for static paths:
/// if a dynamic arg is incorrectly checked, the deny default catches it.
fn deny_default_config(
    cmd: &str,
    rule: tabit_gate::config::RuleConfig,
    read_allows: &[&str],
    write_allows: &[&str],
) -> SanityConfig {
    let mut config = config_with_command_rule(cmd, Action::Allow, rule);
    config.permissions.read.default = Action::Deny;
    config.permissions.write.default = Action::Deny;
    for pattern in read_allows {
        config
            .permissions
            .read
            .overrides
            .push(allow_rule(&[pattern]));
    }
    for pattern in write_allows {
        config
            .permissions
            .write
            .overrides
            .push(allow_rule(&[pattern]));
    }
    config
}

fn check(command: &str, config: &SanityConfig) -> CheckResult {
    check_bash(command, config)
}

// --- flag detection -------------------------------------------------------

#[test]
fn detects_standalone_short_flag() {
    let mut rule = rule_config();
    rule.flags = flags(&[("-f", Action::Deny)]);
    let config = config_with_command_rule("cmd", Action::Allow, rule);
    assert_action(
        &check("cmd -f", &config),
        Action::Deny,
        "exact -f match must set the flag",
    );
}

#[test]
fn detects_standalone_long_flag() {
    let mut rule = rule_config();
    rule.flags = flags(&[("--force", Action::Deny)]);
    let config = config_with_command_rule("cmd", Action::Allow, rule);
    assert_action(
        &check("cmd --force", &config),
        Action::Deny,
        "exact --force match",
    );
}

#[test]
fn detects_short_flag_inside_combined_short_flags() {
    let mut rule = rule_config();
    rule.flags = flags(&[("-f", Action::Deny)]);
    let config = config_with_command_rule("cmd", Action::Allow, rule);
    assert_action(
        &check("cmd -rf", &config),
        Action::Deny,
        "-f must be found inside -rf",
    );
}

#[test]
fn detects_multiple_short_flags_inside_combined_string() {
    let mut rule = rule_config();
    rule.flags = flags(&[("-r", Action::Ask), ("-f", Action::Deny)]);
    let config = config_with_command_rule("cmd", Action::Allow, rule);
    assert_action(
        &check("cmd -rf", &config),
        Action::Deny,
        "both -r and -f detected, deny wins",
    );
}

#[test]
fn does_not_match_long_flag_substring() {
    let mut rule = rule_config();
    rule.flags = flags(&[("--force", Action::Deny)]);
    let config = config_with_command_rule("cmd", Action::Allow, rule);
    assert_action(
        &check("cmd --forced", &config),
        Action::Allow,
        "--forced must not match --force",
    );
}

#[test]
fn matches_declared_multi_char_single_dash_flag_exactly() {
    let mut rule = rule_config();
    rule.flags = flags(&[("-Wall", Action::Ask)]);
    let config = config_with_command_rule("gcc", Action::Allow, rule);
    assert_action(
        &check("gcc -Wall", &config),
        Action::Ask,
        "-Wall is a flag in its own right",
    );
}

#[test]
fn does_not_decompose_declared_multi_char_single_dash_flag() {
    let mut rule = rule_config();
    rule.flags = flags(&[("-Wall", Action::Ask), ("-W", Action::Deny)]);
    let config = config_with_command_rule("gcc", Action::Allow, rule);
    // -Wall is declared, so it is atomic: -W must NOT match inside it.
    assert_action(
        &check("gcc -Wall", &config),
        Action::Ask,
        "declared -Wall must not decompose to -W",
    );
}

#[test]
fn decomposes_undeclared_multi_char_single_dash_flag_for_single_char_match() {
    let mut rule = rule_config();
    rule.flags = flags(&[("-W", Action::Deny)]);
    let config = config_with_command_rule("gcc", Action::Allow, rule);
    // -Wall is not declared, so it decomposes and -W matches inside it.
    assert_action(
        &check("gcc -Wall", &config),
        Action::Deny,
        "undeclared -Wall decomposes for -W",
    );
}

#[test]
fn does_not_match_short_flag_inside_non_flag_arg() {
    let mut rule = rule_config();
    rule.flags = flags(&[("-f", Action::Deny)]);
    let config = config_with_command_rule("cmd", Action::Allow, rule);
    assert_action(
        &check("cmd file.txt", &config),
        Action::Allow,
        "-f must not match inside file.txt",
    );
}

// --- option value extraction ------------------------------------------------

#[test]
fn extracts_option_value_with_space_separator() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&[], &[]));
    rule.options.insert("-o".into(), option_perms(&["write"]));
    let config = specific_path_config("cmd", rule, true, "/specific", Action::Deny);
    assert_action(
        &check("cmd -o /specific", &config),
        Action::Deny,
        "-o's value is write-checked",
    );
}

#[test]
fn extracts_option_value_with_equals_separator() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&[], &[]));
    rule.options.insert("-o".into(), option_perms(&["write"]));
    let config = specific_path_config("cmd", rule, true, "/specific", Action::Deny);
    assert_action(
        &check("cmd -o=/specific", &config),
        Action::Deny,
        "equals form splits into value",
    );
}

#[test]
fn consumes_option_value_without_counting_it_as_positional() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&[], &[("0", &["read"])]));
    rule.options.insert("-o".into(), option_perms(&["write"]));
    let config = specific_path_config("cmd", rule, false, "/pos0-read", Action::Deny);
    // Working: -o consumes /opt-value, so /pos0-read is positional 0 (read).
    // Buggy: /opt-value becomes positional 0 and /pos0-read shifts to 1.
    assert_action(
        &check("cmd -o /opt-value /pos0-read", &config),
        Action::Deny,
        "consumed option value must not shift positional indices",
    );
}

// --- positional counting ------------------------------------------------------

#[test]
fn counts_positionals_correctly_with_flags_mixed_in() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&[], &[("0", &["read"])]));
    let config = specific_path_config("cp", rule, false, "/src", Action::Deny);
    // Working: -r is skipped, /src is positional 0. Buggy: /src shifts to 1.
    assert_action(
        &check("cp -r /src /dest", &config),
        Action::Deny,
        "-r must not become a positional",
    );
}

#[test]
fn applies_negative_index_override_to_last_positional() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&[], &[("-1", &["read"])]));
    let config = specific_path_config("mv", rule, false, "/last", Action::Deny);
    assert_action(
        &check("mv /first /middle /last", &config),
        Action::Deny,
        "-1 override must hit the last positional",
    );
}

#[test]
fn applies_positive_index_override_to_specific_position() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&[], &[("1", &["read"])]));
    let config = specific_path_config("cmd", rule, false, "/pos1", Action::Deny);
    assert_action(
        &check("cmd /pos0 /pos1 /pos2", &config),
        Action::Deny,
        "override \"1\" must hit the second positional",
    );
}

#[test]
fn handles_positionals_with_overrides_but_no_default_perm() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&[], &[("1", &["read"])]));
    let config = specific_path_config("cmd", rule, false, "/pos1", Action::Deny);
    // Missing default_perm is treated as []: only overridden slots are checked.
    assert_action(
        &check("cmd /pos0 /pos1 /pos2", &config),
        Action::Deny,
        "overridden positional still checked",
    );
    let other = check("cmd /pos0 /other", &config);
    assert_action(
        &other,
        Action::Allow,
        "no override-matching positional means allow",
    );
}

#[test]
fn skips_declared_flags_from_positional_counting() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&[], &[("0", &["read"])]));
    rule.flags = flags(&[("--force", Action::Allow)]);
    let config = specific_path_config("cmd", rule, false, "/pos0", Action::Deny);
    assert_action(
        &check("cmd --force /pos0", &config),
        Action::Deny,
        "--force must be skipped so /pos0 is positional 0",
    );
}

// --- dynamic args: detection in the walker -----------------------------------

#[test]
fn walker_marks_command_substitution_as_dynamic() {
    let result = bash_walker::walk("cat $(echo secret.txt)");
    let cat = result
        .commands
        .iter()
        .find(|c| c.name.as_deref() == Some("cat"))
        .expect("cat");
    assert_eq!(
        cat.args,
        ["$(echo secret.txt)"],
        "raw text is preserved in args"
    );
    assert!(
        cat.dynamic_indices.contains(&0),
        "substitution makes the arg dynamic"
    );
}

#[test]
fn walker_marks_parameter_expansion_as_dynamic() {
    let result = bash_walker::walk("cat $HOME/file.txt");
    let cat = result
        .commands
        .iter()
        .find(|c| c.name.as_deref() == Some("cat"))
        .expect("cat");
    assert_eq!(cat.args, ["$HOME/file.txt"]);
    assert!(
        cat.dynamic_indices.contains(&0),
        "parameter expansion is dynamic"
    );
}

#[test]
fn walker_marks_brace_expansion_as_dynamic() {
    let result = bash_walker::walk("cat file{1,2}.txt");
    let cat = result
        .commands
        .iter()
        .find(|c| c.name.as_deref() == Some("cat"))
        .expect("cat");
    assert_eq!(cat.args, ["file{1,2}.txt"]);
    assert!(
        cat.dynamic_indices.contains(&0),
        "brace expansion is dynamic"
    );
}

#[test]
fn walker_does_not_mark_literal_args_as_dynamic() {
    let result = bash_walker::walk("cat file.txt");
    let cat = result
        .commands
        .iter()
        .find(|c| c.name.as_deref() == Some("cat"))
        .expect("cat");
    assert_eq!(cat.args, ["file.txt"]);
    assert_eq!(
        cat.dynamic_indices,
        HashSet::new(),
        "literal args are static"
    );
}

#[test]
fn walker_tracks_dynamic_indices_in_multi_arg_commands() {
    let result = bash_walker::walk("cp $(echo src) dest");
    let cp = result
        .commands
        .iter()
        .find(|c| c.name.as_deref() == Some("cp"))
        .expect("cp");
    assert_eq!(cp.args, ["$(echo src)", "dest"]);
    assert!(cp.dynamic_indices.contains(&0));
    assert!(!cp.dynamic_indices.contains(&1));
}

// --- dynamic args excluded from path checks -----------------------------------

#[test]
fn does_not_check_dynamic_arg_as_a_path() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&["read"], &[]));
    let config = deny_default_config("cmd", rule, &["/static"], &[]);
    // Working: $(echo /anything) is skipped, /static is allowed.
    // Buggy: the dynamic arg is checked and hits the deny default.
    assert_action(
        &check("cmd $(echo /anything) /static", &config),
        Action::Allow,
        "dynamic args must be skipped from path checks",
    );
}

// --- positional indices preserved with dynamic args ----------------------------

#[test]
fn counts_dynamic_arg_in_index_calculation() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&[], &[("-1", &["read"])]));
    let config = specific_path_config("cp", rule, false, "/last", Action::Deny);
    // $(echo src) occupies slot 0, so /last is positional 1 = last.
    assert_action(
        &check("cp $(echo src) /last", &config),
        Action::Deny,
        "dynamic args still occupy positional slots",
    );
}

#[test]
fn applies_positive_index_override_accounting_for_dynamic_args() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&[], &[("1", &["read"])]));
    let config = specific_path_config("cmd", rule, false, "/pos1", Action::Deny);
    assert_action(
        &check("cmd $(echo a) /pos1 /pos2", &config),
        Action::Deny,
        "/pos1 is positional 1 even with a dynamic arg in front",
    );
}

#[test]
fn handles_multiple_dynamic_args_mixed_with_static() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&["read"], &[]));
    let config = deny_default_config("cmd", rule, &["/static*"], &[]);
    assert_action(
        &check("cmd $(echo a) /static1 $(echo b) /static2", &config),
        Action::Allow,
        "static args checked and allowed, dynamic args skipped",
    );
}

// --- dynamic args with options and flags ----------------------------------------

#[test]
fn skips_dynamic_args_even_after_option_consumption() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&["read"], &[]));
    rule.options.insert("-o".into(), option_perms(&["write"]));
    let config = deny_default_config("cmd", rule, &["/static"], &[]);
    assert_action(
        &check("cmd -o $(echo /opt) $(echo /pos) /static", &config),
        Action::Allow,
        "the dynamic option value and dynamic positional are both skipped",
    );
}

// --- dynamic option values --------------------------------------------------------

#[test]
fn skips_dynamic_option_value_from_path_check() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&["read"], &[]));
    rule.options.insert("-o".into(), option_perms(&["write"]));
    let config = deny_default_config("gcc", rule, &["/static"], &[]);
    assert_action(
        &check("gcc -o $(echo /opt) /static", &config),
        Action::Allow,
        "dynamic option value must not be write-checked",
    );
}

#[test]
fn checks_static_option_value_normally() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&[], &[]));
    rule.options.insert("-o".into(), option_perms(&["write"]));
    let config = specific_path_config("gcc", rule, true, "/specific", Action::Deny);
    assert_action(
        &check("gcc -o /specific main.c", &config),
        Action::Deny,
        "static option value is write-checked",
    );
}

#[test]
fn handles_equals_form_with_dynamic_value() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&["read"], &[]));
    rule.options.insert("-o".into(), option_perms(&["write"]));
    let config = deny_default_config("cmd", rule, &["/static"], &[]);
    // The entire token -o=$(echo /opt) is dynamic, so it is skipped.
    assert_action(
        &check("cmd -o=$(echo /opt) /static", &config),
        Action::Allow,
        "dynamic equals-form option value is skipped",
    );
}

// --- merged options and flags (tar-like) --------------------------------------------

#[test]
fn detects_option_in_combined_short_string_and_consumes_next_arg() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&[], &[]));
    rule.flags = flags(&[("-x", Action::Allow)]);
    rule.options.insert("-f".into(), option_perms(&["read"]));
    let config = specific_path_config("tar", rule, false, "/specific", Action::Deny);
    assert_action(
        &check("tar -xzf /specific", &config),
        Action::Deny,
        "-f inside -xzf consumes /specific as its read-checked value",
    );
}

#[test]
fn applies_option_check_on_consumed_value_from_combined_string() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&[], &[]));
    rule.flags = flags(&[("-x", Action::Allow)]);
    rule.options.insert("-f".into(), option_perms(&["write"]));
    let config = specific_path_config("tar", rule, true, "/specific", Action::Deny);
    assert_action(
        &check("tar -xf /specific", &config),
        Action::Deny,
        "-f inside -xf consumes /specific as its write-checked value",
    );
}

#[test]
fn handles_declared_multi_char_flag_atomically_in_combined_context() {
    let mut rule = rule_config();
    rule.positionals = Some(positionals(&["read"], &[]));
    rule.flags = flags(&[("-Wall", Action::Ask)]);
    rule.options.insert("-W".into(), option_perms(&["write"]));
    let config = deny_default_config("cmd", rule, &["/file"], &[]);
    // Working: -Wall is an atomic ask flag, /file is positional 0 (read, allowed).
    // Buggy: -Wall decomposes into the -W option and consumes /file.
    assert_action(
        &check("cmd -Wall /file", &config),
        Action::Ask,
        "declared -Wall must stay atomic: ask from the flag, not deny from a bogus option",
    );
}
