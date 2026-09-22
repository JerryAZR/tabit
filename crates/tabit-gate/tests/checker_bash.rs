//! The bash checker over the shipped default config (ported from
//! pi-sanity `tests/integration/checker/bash.test.ts`): safe commands,
//! dangerous commands, package managers, file operations, parse errors,
//! sed in-place editing, git clean, redirections, and dangerous
//! commands hidden in nested contexts.

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

use common::assert_action;
use tabit_gate::checker_bash::check_bash;
use tabit_gate::config;
use tabit_gate::types::Action;

fn deny_or_ask(result: &tabit_gate::types::CheckResult, context: &str) {
    assert!(
        result.action == Action::Deny || result.action == Action::Ask,
        "{context}: got {:?}",
        result.action
    );
}

// --- safe commands -------------------------------------------------------

#[test]
fn allows_ls_la() {
    let result = check_bash("ls -la", &config::default_config());
    assert_action(&result, Action::Allow, "ls -la is safe");
}

#[test]
fn allows_cat_file() {
    let result = check_bash("cat file.txt", &config::default_config());
    assert_action(&result, Action::Allow, "cat of a CWD file is safe");
}

#[test]
fn allows_grep() {
    let result = check_bash("grep pattern file", &config::default_config());
    assert_action(&result, Action::Allow, "grep is safe");
}

// --- dangerous commands --------------------------------------------------

#[test]
fn denies_dd() {
    let result = check_bash("dd if=/dev/zero of=/tmp/test", &config::default_config());
    assert_action(&result, Action::Deny, "dd is denied by default");
}

// --- package managers ----------------------------------------------------

#[test]
fn allows_npm_install_local() {
    let result = check_bash("npm install package", &config::default_config());
    assert_action(&result, Action::Allow, "local npm install is allowed");
}

#[test]
fn denies_npm_install_global_flag() {
    let result = check_bash("npm install -g package", &config::default_config());
    assert_action(&result, Action::Deny, "global npm install is denied");
}

#[test]
fn denies_npm_long_global_flag() {
    let result = check_bash("npm --global install package", &config::default_config());
    assert_action(&result, Action::Deny, "--global npm install is denied");
}

#[test]
fn denies_yarn_global_add() {
    let result = check_bash("yarn global add package", &config::default_config());
    assert_action(&result, Action::Deny, "yarn global installs are denied");
}

// --- file operations -----------------------------------------------------

#[test]
fn allows_cp_within_cwd() {
    let result = check_bash("cp file.txt backup/", &config::default_config());
    assert_action(&result, Action::Allow, "cp inside CWD is allowed");
}

#[test]
fn denies_cp_into_etc() {
    let result = check_bash("cp file.txt /etc/", &config::default_config());
    assert_action(&result, Action::Deny, "cp into /etc is denied");
}

#[test]
fn denies_cp_with_target_directory_option() {
    let result = check_bash(
        "cp --target-directory=/etc/ file.txt",
        &config::default_config(),
    );
    assert_action(
        &result,
        Action::Deny,
        "--target-directory=/etc/ is checked as a write",
    );
}

#[test]
fn allows_cp_with_dynamic_target_directory() {
    let result = check_bash(
        "cp --target-directory=$(echo /etc/) file.txt",
        &config::default_config(),
    );
    // Dynamic option value is skipped; file.txt is checked as write
    // (last positional) and file.txt in CWD is allowed.
    assert_action(
        &result,
        Action::Allow,
        "dynamic --target-directory is skipped",
    );
}

#[test]
fn allows_mv_within_cwd() {
    let result = check_bash("mv file.txt archive/", &config::default_config());
    assert_action(&result, Action::Allow, "mv inside CWD is allowed");
}

#[test]
fn allows_rm_within_cwd() {
    let result = check_bash("rm file.txt", &config::default_config());
    assert_action(&result, Action::Allow, "rm inside CWD is allowed");
}

#[test]
fn denies_rm_in_etc() {
    let result = check_bash("rm /etc/config", &config::default_config());
    assert_action(&result, Action::Deny, "rm in /etc is denied");
}

#[test]
fn reports_the_deny_reason_once_for_multiple_blocked_paths() {
    let result = check_bash(
        "rm /etc/hosts /etc/passwd /etc/group",
        &config::default_config(),
    );
    assert_action(
        &result,
        Action::Deny,
        "all three paths are outside allowed regions",
    );
    assert_eq!(
        result.reason.as_deref(),
        Some("Writing outside allowed locations requires explicit permission"),
        "identical reasons deduplicate to one"
    );
}

// --- parse errors --------------------------------------------------------

#[test]
fn denies_commands_with_parse_errors() {
    let result = check_bash("echo \"unclosed", &config::default_config());
    assert_action(&result, Action::Deny, "a parse error denies the command");
}

// --- sed in-place editing ------------------------------------------------

#[test]
fn allows_sed_in_place_within_cwd() {
    let result = check_bash("sed -i 's/foo/bar/' file.txt", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "sed -i in CWD passes the write check",
    );
}

#[test]
fn denies_sed_in_place_on_system_path() {
    let result = check_bash("sed -i 's/foo/bar/' /etc/passwd", &config::default_config());
    assert_action(
        &result,
        Action::Deny,
        "sed -i on /etc/passwd fails the write check",
    );
}

#[test]
fn allows_sed_without_in_place() {
    let result = check_bash("sed 's/foo/bar/' file.txt", &config::default_config());
    assert_action(&result, Action::Allow, "plain sed is read-only");
}

// --- git clean -----------------------------------------------------------

#[test]
fn asks_for_git_clean_force_flag() {
    let result = check_bash("git clean -f", &config::default_config());
    assert_action(&result, Action::Ask, "git clean -f removes untracked files");
}

#[test]
fn asks_for_git_clean_long_force_flag() {
    let result = check_bash("git clean --force", &config::default_config());
    assert_action(
        &result,
        Action::Ask,
        "git clean --force removes untracked files",
    );
}

#[test]
fn allows_git_clean_without_flags() {
    let result = check_bash("git clean", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "bare git clean is a harmless preview",
    );
}

#[test]
fn allows_git_clean_dry_run() {
    let result = check_bash("git clean -n", &config::default_config());
    assert_action(&result, Action::Allow, "git clean -n is a dry run");
}

// --- redirections --------------------------------------------------------

#[test]
fn allows_redirect_to_dev_null() {
    let result = check_bash("echo test >/dev/null", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "/dev/null is an allowed write target",
    );
}

#[test]
fn allows_redirect_into_tmp() {
    let result = check_bash("echo test > /tmp/output.log", &config::default_config());
    assert_action(&result, Action::Allow, "/tmp writes are allowed");
}

#[test]
fn denies_redirect_escaping_tmp_via_traversal() {
    let result = check_bash("echo test > /tmp/../etc/x", &config::default_config());
    assert_action(&result, Action::Deny, "traversal out of /tmp lands in /etc");
}

#[test]
fn checks_bare_statement_redirects_with_the_tracked_cwd() {
    let result = check_bash("cd /etc; > stamp", &config::default_config());
    assert_action(
        &result,
        Action::Deny,
        "the bare redirect lands in /etc after the cd",
    );
}

#[test]
fn allows_bare_statement_redirects_in_cwd() {
    let result = check_bash("> out.txt", &config::default_config());
    assert_action(&result, Action::Allow, "a bare redirect in CWD is allowed");
}

#[test]
fn extracts_commands_from_parameter_expansion_defaults() {
    let result = check_bash(
        "echo ${A:-$(rm /etc/passwd)} > log.txt",
        &config::default_config(),
    );
    assert_action(&result, Action::Deny, "the inner rm must be found");
    assert!(
        result
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("outside allowed"),
        "deny must come from the path check, not a parse error: {result:?}"
    );
}

#[test]
fn extracts_commands_from_slice_offsets() {
    let result = check_bash(
        "echo ${VAR:$(rm /etc/passwd):2} > log.txt",
        &config::default_config(),
    );
    assert_action(
        &result,
        Action::Deny,
        "the inner rm in the slice offset must be found",
    );
    assert!(
        result
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("outside allowed"),
        "deny must come from the path check, not a parse error: {result:?}"
    );
}

#[test]
fn allows_git_paths_under_tmp() {
    // git protection is CWD-scoped by design; temp is trusted space.
    let result = check_bash(
        "echo test > /tmp/repo/.git/config",
        &config::default_config(),
    );
    assert_action(
        &result,
        Action::Allow,
        ".git under /tmp is not git-protected",
    );
}

#[test]
fn allows_stderr_redirect_to_dev_null() {
    let result = check_bash("rm -f test.txt 2>/dev/null", &config::default_config());
    assert_action(
        &result,
        Action::Allow,
        "2>/dev/null is an allowed write target",
    );
}

#[test]
fn denies_redirect_into_etc() {
    let result = check_bash("echo test >/etc/file", &config::default_config());
    assert_action(&result, Action::Deny, "/etc/file is a denied write target");
}

// --- dangerous commands in nested contexts --------------------------------

#[test]
fn denies_rm_inside_command_substitution() {
    let result = check_bash("echo $(rm -rf /)", &config::default_config());
    deny_or_ask(&result, "rm -rf / inside $() must be extracted");
}

#[test]
fn denies_npm_global_inside_command_substitution() {
    let result = check_bash("echo $(npm install -g package)", &config::default_config());
    assert_action(&result, Action::Deny, "npm -g inside $() must be extracted");
}

#[test]
fn denies_or_asks_rm_inside_backticks() {
    let result = check_bash("cat `rm -rf /`", &config::default_config());
    deny_or_ask(&result, "rm -rf / inside backticks must be extracted");
}

#[test]
fn denies_or_asks_rm_in_input_process_substitution() {
    let result = check_bash("cat <(rm -rf /)", &config::default_config());
    deny_or_ask(&result, "rm -rf / inside <() must be extracted");
}

#[test]
fn denies_or_asks_rm_in_output_process_substitution() {
    let result = check_bash("echo data > >(rm -rf /)", &config::default_config());
    deny_or_ask(&result, "rm -rf / inside >() must be extracted");
}

#[test]
fn denies_or_asks_rm_in_case_statement() {
    let result = check_bash("case x in *) rm -rf / ;; esac", &config::default_config());
    deny_or_ask(&result, "rm -rf / in a case body must be extracted");
}

#[test]
fn denies_or_asks_rm_in_for_loop_wordlist() {
    let result = check_bash(
        "for x in $(rm -rf /); do :; done",
        &config::default_config(),
    );
    deny_or_ask(&result, "rm -rf / in a for wordlist must be extracted");
}

#[test]
fn denies_or_asks_rm_in_else_branch_with_braces() {
    let result = check_bash(
        "if false; then :; else { rm -rf /; }; fi",
        &config::default_config(),
    );
    deny_or_ask(&result, "rm -rf / in an else branch must be extracted");
}

#[test]
fn denies_or_asks_rm_in_variable_assignment() {
    let result = check_bash("VAR=$(rm -rf /)", &config::default_config());
    deny_or_ask(&result, "rm -rf / in an assignment must be extracted");
}

#[test]
fn denies_or_asks_rm_in_test_expression() {
    let result = check_bash("[[ -f $(rm -rf /) ]]", &config::default_config());
    deny_or_ask(&result, "rm -rf / in [[ ]] must be extracted");
}

#[test]
fn denies_or_asks_rm_in_select_loop() {
    let result = check_bash(
        "select x in $(rm -rf /); do echo $x; done",
        &config::default_config(),
    );
    deny_or_ask(&result, "rm -rf / in a select loop must be extracted");
}

#[test]
fn denies_or_asks_rm_in_coproc() {
    let result = check_bash("coproc rm -rf /", &config::default_config());
    deny_or_ask(&result, "rm -rf / in coproc must be extracted");
}

#[test]
fn denies_or_asks_rm_in_c_style_for_loop() {
    let result = check_bash(
        "for ((i=0; i<1; i++)); do rm -rf /; done",
        &config::default_config(),
    );
    deny_or_ask(&result, "rm -rf / in for ((...)) must be extracted");
}

#[test]
fn denies_or_asks_rm_in_parameter_expansion() {
    let result = check_bash("echo ${VAR:-$(rm -rf /)}", &config::default_config());
    deny_or_ask(&result, "rm -rf / in ${VAR:-...} must be extracted");
}

#[test]
fn denies_or_asks_rm_inside_double_quotes() {
    let result = check_bash("echo \"running: $(rm -rf /)\"", &config::default_config());
    deny_or_ask(&result, "rm -rf / inside double quotes must be extracted");
}
