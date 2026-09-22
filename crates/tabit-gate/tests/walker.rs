//! The bash AST walker (ported from pi-sanity
//! `tests/unit/bash/walker.test.ts`): command extraction, pipelines,
//! redirects, subshells, command/process substitutions, advanced bash
//! constructs, security-critical nested contexts, and bash quoting
//! semantics.

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

use tabit_gate::bash_walker;

/// The command names extracted from a walk, in source order.
fn names(result: &bash_walker::WalkResult) -> Vec<String> {
    result
        .commands
        .iter()
        .map(|cmd| cmd.name.clone().unwrap_or_default())
        .collect()
}

// --- simple commands ------------------------------------------------------

#[test]
fn extracts_simple_command() {
    let result = bash_walker::walk("rm file.txt");
    assert_eq!(result.commands.len(), 1, "one command");
    let cmd = &result.commands[0];
    assert_eq!(cmd.name.as_deref(), Some("rm"));
    assert_eq!(cmd.args, ["file.txt"]);
}

#[test]
fn extracts_command_with_flags() {
    let result = bash_walker::walk("rm -rf dir/");
    let cmd = &result.commands[0];
    assert_eq!(cmd.name.as_deref(), Some("rm"));
    assert_eq!(cmd.args, ["-rf", "dir/"]);
}

#[test]
fn extracts_command_with_multiple_args() {
    let result = bash_walker::walk("cp file1 file2 file3 dest/");
    assert_eq!(
        result.commands[0].args,
        ["file1", "file2", "file3", "dest/"]
    );
}

#[test]
fn handles_empty_command() {
    let result = bash_walker::walk("");
    assert!(result.commands.is_empty(), "no commands in an empty script");
}

// --- pipelines ------------------------------------------------------------

#[test]
fn extracts_pipeline_commands() {
    let result = bash_walker::walk("cat file.txt | grep pattern");
    assert_eq!(result.commands.len(), 2, "both pipeline segments");
    assert_eq!(names(&result), ["cat", "grep"]);
}

// --- redirects ------------------------------------------------------------

#[test]
fn extracts_output_redirect() {
    let result = bash_walker::walk("echo hello > output.txt");
    let cmd = &result.commands[0];
    assert_eq!(cmd.name.as_deref(), Some("echo"));
    assert_eq!(cmd.redirects.len(), 1, "one redirect");
    let redirect = &cmd.redirects[0];
    assert_eq!(redirect.operator, ">");
    assert_eq!(redirect.target, "output.txt");
    assert!(redirect.is_output, "> is an output redirect");
    assert!(!redirect.is_input, "> is not an input redirect");
}

#[test]
fn extracts_input_redirect() {
    let result = bash_walker::walk("cat < input.txt");
    let redirect = &result.commands[0].redirects[0];
    assert_eq!(redirect.operator, "<");
    assert!(redirect.is_input, "< is an input redirect");
    assert!(!redirect.is_output, "< is not an output redirect");
}

#[test]
fn handles_standalone_redirect_without_command() {
    // A nameless statement carrying only a redirect must not crash the
    // walk — the checker path-checks the redirect target, so the walk
    // must surface it as a (nameless) command.
    let result = bash_walker::walk("> output.txt");
    assert!(!result.commands.is_empty(), "the redirect is walked");
}

// --- subshells ------------------------------------------------------------

#[test]
fn extracts_subshell_commands() {
    let result = bash_walker::walk("(cd /tmp && rm file)");
    assert_eq!(result.commands.len(), 2, "both commands in the subshell");
    assert_eq!(names(&result), ["cd", "rm"]);
}

// --- command substitution -------------------------------------------------

#[test]
fn extracts_inner_command_from_substitution_in_args() {
    let result = bash_walker::walk("cat $(rm /secret)");
    assert_eq!(
        result.commands.len(),
        2,
        "inner command first, outer second"
    );
    assert_eq!(names(&result), ["rm", "cat"]);
    assert_eq!(result.commands[0].args, ["/secret"]);
}

#[test]
fn extracts_inner_command_from_backtick_substitution() {
    let result = bash_walker::walk("`which rm` file");
    assert_eq!(result.commands.len(), 2);
    assert_eq!(result.commands[0].name.as_deref(), Some("which"));
    assert_eq!(result.commands[0].args, ["rm"]);
}

#[test]
fn recursively_extracts_nested_command_substitutions() {
    let result = bash_walker::walk("$(echo $(rm /))");
    assert_eq!(result.commands.len(), 2, "deepest first");
    assert_eq!(names(&result), ["rm", "echo"]);
    assert_eq!(result.commands[0].args, ["/"]);
}

#[test]
fn extracts_command_substitution_from_redirect_target() {
    let result = bash_walker::walk("cat > $(echo /etc/file)");
    assert_eq!(result.commands.len(), 2);
    assert_eq!(result.commands[0].name.as_deref(), Some("echo"));
    assert_eq!(result.commands[0].args, ["/etc/file"]);
}

#[test]
fn handles_multiple_command_substitutions() {
    let result = bash_walker::walk("cat $(echo file1) $(echo file2)");
    assert_eq!(
        result.commands.len(),
        3,
        "both substitutions then the outer cat"
    );
    assert_eq!(names(&result), ["echo", "echo", "cat"]);
}

// --- process substitution -------------------------------------------------

#[test]
fn extracts_commands_from_input_process_substitution() {
    let result = bash_walker::walk("cat <(echo content)");
    assert_eq!(result.commands.len(), 2);
    assert_eq!(result.commands[0].name.as_deref(), Some("echo"));
    assert_eq!(result.commands[0].args, ["content"]);
    assert_eq!(result.commands[1].name.as_deref(), Some("cat"));
}

#[test]
fn extracts_commands_from_output_process_substitution() {
    let result = bash_walker::walk("echo data > >(tee log.txt)");
    assert_eq!(result.commands.len(), 2);
    assert_eq!(result.commands[0].name.as_deref(), Some("tee"));
    assert_eq!(result.commands[0].args, ["log.txt"]);
    assert_eq!(result.commands[1].name.as_deref(), Some("echo"));
}

// --- advanced bash constructs ----------------------------------------------

#[test]
fn extracts_commands_from_else_branch_with_braces() {
    let result = bash_walker::walk("if true; then :; else { rm file; }; fi");
    assert!(
        names(&result).iter().any(|n| n == "rm"),
        "should extract 'rm' from else {{ ... }}"
    );
}

#[test]
fn extracts_commands_from_standalone_braced_groups() {
    let result = bash_walker::walk("{ echo start; rm file; echo end; }");
    assert_eq!(result.commands.len(), 3, "all three commands in the group");
    assert!(names(&result).iter().any(|n| n == "rm"));
}

#[test]
fn extracts_commands_from_case_body() {
    let result = bash_walker::walk("case $x in a) rm file1 ;; b) rm file2 ;; esac");
    assert!(
        names(&result).iter().any(|n| n == "rm"),
        "should extract 'rm' from case body"
    );
}

#[test]
fn extracts_commands_from_case_pattern_expressions() {
    let result = bash_walker::walk("case $(echo a) in a) echo yes ;; esac");
    assert!(
        names(&result).iter().any(|n| n == "echo"),
        "should extract 'echo' from case pattern"
    );
}

#[test]
fn extracts_commands_from_for_loop_wordlist() {
    let result = bash_walker::walk("for x in $(ls); do echo $x; done");
    assert!(
        names(&result).iter().any(|n| n == "ls"),
        "should extract 'ls' from for wordlist"
    );
}

#[test]
fn extracts_commands_from_variable_assignments() {
    let result = bash_walker::walk("VAR=$(echo value)");
    assert!(
        names(&result).iter().any(|n| n == "echo"),
        "should extract 'echo' from VAR=$(...)"
    );
}

#[test]
fn extracts_commands_from_array_assignments() {
    let result = bash_walker::walk("ARR=($(echo a) $(echo b))");
    assert!(
        result.commands.len() >= 2,
        "should extract both 'echo' commands"
    );
}

#[test]
fn extracts_commands_from_test_expressions() {
    let result = bash_walker::walk("[[ -f $(echo file.txt) ]]");
    assert!(
        names(&result).iter().any(|n| n == "echo"),
        "should extract 'echo' from [[ ]]"
    );
}

#[test]
fn extracts_commands_from_binary_test_expressions() {
    let result = bash_walker::walk("[[ $(echo a) == $(echo b) ]]");
    assert!(
        result.commands.len() >= 2,
        "should extract both sides of comparison"
    );
}

#[test]
fn extracts_commands_from_select_loop() {
    let result = bash_walker::walk("select x in $(echo a); do rm $x; done");
    let found = names(&result);
    assert!(
        found.iter().any(|n| n == "echo"),
        "should extract 'echo' from select wordlist"
    );
    assert!(
        found.iter().any(|n| n == "rm"),
        "should extract 'rm' from select body"
    );
}

#[test]
fn extracts_commands_from_coproc() {
    let result = bash_walker::walk("coproc echo hello");
    assert!(
        names(&result).iter().any(|n| n == "echo"),
        "should extract 'echo' from coproc"
    );
}

#[test]
fn extracts_commands_from_named_coproc() {
    let result = bash_walker::walk("coproc MYPROC { echo start; rm file; }");
    assert!(
        names(&result).iter().any(|n| n == "rm"),
        "should extract 'rm' from coproc body"
    );
}

#[test]
fn extracts_commands_from_c_style_for_loop_body() {
    let result = bash_walker::walk("for ((i=0; i<3; i++)); do rm file$i; done");
    assert!(
        names(&result).iter().any(|n| n == "rm"),
        "should extract 'rm' from for ((...)) body"
    );
}

#[test]
fn extracts_commands_from_parameter_expansion() {
    let result = bash_walker::walk("echo ${VAR:-$(echo default)}");
    let echo_count = result
        .commands
        .iter()
        .filter(|c| c.name.as_deref() == Some("echo"))
        .count();
    assert_eq!(echo_count, 2, "should extract both 'echo' commands");
}

#[test]
fn extracts_commands_inside_double_quotes() {
    let result = bash_walker::walk("echo \"result: $(echo inner)\"");
    let echo_count = result
        .commands
        .iter()
        .filter(|c| c.name.as_deref() == Some("echo"))
        .count();
    assert_eq!(echo_count, 2, "should extract 'echo' inside double quotes");
}

// --- security: dangerous commands in nested contexts -----------------------

#[test]
fn must_extract_rm_in_command_substitution() {
    let result = bash_walker::walk("echo $(rm -rf /)");
    assert!(
        result
            .commands
            .iter()
            .any(|c| c.name.as_deref() == Some("rm")),
        "MUST extract 'rm' from $()"
    );
}

#[test]
fn must_extract_rm_in_else_branch_compound_list() {
    let result = bash_walker::walk("if false; then :; else { rm -rf /; }; fi");
    assert!(
        result
            .commands
            .iter()
            .any(|c| c.name.as_deref() == Some("rm")),
        "MUST extract 'rm' from else {{}}"
    );
}

#[test]
fn must_extract_rm_in_case_statement() {
    let result = bash_walker::walk("case x in *) rm -rf / ;; esac");
    assert!(
        result
            .commands
            .iter()
            .any(|c| c.name.as_deref() == Some("rm")),
        "MUST extract 'rm' from case"
    );
}

#[test]
fn must_extract_rm_in_for_loop_wordlist() {
    let result = bash_walker::walk("for f in $(rm -rf /); do :; done");
    assert!(
        result
            .commands
            .iter()
            .any(|c| c.name.as_deref() == Some("rm")),
        "MUST extract 'rm' from for wordlist"
    );
}

#[test]
fn must_extract_rm_in_assignment() {
    let result = bash_walker::walk("VAR=$(rm -rf /)");
    assert!(
        result
            .commands
            .iter()
            .any(|c| c.name.as_deref() == Some("rm")),
        "MUST extract 'rm' from VAR=$(...)"
    );
}

// --- quoted word handling ---------------------------------------------------

#[test]
fn strips_double_quotes_from_fully_quoted_args() {
    let result = bash_walker::walk("rm \"/a b\"");
    assert_eq!(result.commands[0].args, ["/a b"]);
}

#[test]
fn strips_single_quotes_from_fully_quoted_args() {
    let result = bash_walker::walk("rm 'c d'");
    assert_eq!(result.commands[0].args, ["c d"]);
}

#[test]
fn splices_inner_quotes_with_bash_concatenation_semantics() {
    let result = bash_walker::walk("echo pre\"fix\"post");
    assert_eq!(result.commands[0].args, ["prefixpost"]);
}

#[test]
fn unquotes_redirect_targets() {
    let result = bash_walker::walk("echo hi > \"/etc/out\"");
    assert_eq!(result.commands[0].redirects[0].target, "/etc/out");
}
