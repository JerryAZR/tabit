//! The parser substitution proof: brush-parser (the unbash
//! replacement) parses the constructs the walker threads — plain
//! sequences with cd tracking, pipelines, control flow, process
//! substitution, redirects. This is the adapter's reference for the
//! AST shapes (verified against brush-parser 0.4):
//!
//! ```text
//! Program.complete_commands  : Vec<CompleteCommand>
//! CompleteCommand = CompoundList(Vec<CompoundListItem>)
//! CompoundListItem(AndOrList, SeparatorOperator)
//! AndOrList { first: Pipeline, additional: Vec<AndOr::And/Or(Pipeline)> }
//! Pipeline { bang, seq: Vec<Command> }
//! Command::Simple(SimpleCommand { prefix, word_or_name, suffix })
//! CommandPrefixOrSuffixItem::{IoRedirect, Word, AssignmentWord,
//!                              ProcessSubstitution(kind, SubshellCommand)}
//! Word { value: String }   // RAW TEXT — see the dynamic-detection note
//! ```
//!
//! **Dynamic-detection note (the one adapter deviation):** unbash
//! exposes typed word parts (`CommandExpansionPart`, …) so pi-sanity
//! marks dynamic argument indices structurally. brush-parser keeps
//! words as raw text, so the adapter detects dynamic content
//! lexically — a word containing parameter/command expansion shapes
//! (`$`, `${`, `$(`, backticks) is dynamic. Process substitutions
//! ARE structural (`CommandPrefixOrSuffixItem::ProcessSubstitution`).

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

use brush_parser::ast::{AndOr, Command, CommandPrefixOrSuffixItem, CompoundList, Pipeline};

/// One script through the tokenizer + parser, as the walker will
/// invoke it.
fn parse(script: &str) -> Result<brush_parser::ast::Program, brush_parser::ParseError> {
    let tokens =
        brush_parser::tokenize_str(script).map_err(|err| brush_parser::ParseError::Tokenizing {
            inner: err,
            position: None,
        })?;
    brush_parser::parse_tokens(&tokens, &brush_parser::ParserOptions::default())
}

/// Every simple command in the program, in source order (a one-level
/// walk — the real walker threads cd state and descends compounds;
/// this exists to pin the AST shapes).
fn simple_commands(program: &brush_parser::ast::Program) -> Vec<&brush_parser::ast::SimpleCommand> {
    let mut found = Vec::new();
    let mut pipelines: Vec<&Pipeline> = Vec::new();
    for list in &program.complete_commands {
        let CompoundList(items) = list;
        for item in items {
            pipelines.push(&item.0.first);
            for and_or in &item.0.additional {
                pipelines.push(match and_or {
                    AndOr::And(pipeline) | AndOr::Or(pipeline) => pipeline,
                });
            }
        }
    }
    for pipeline in pipelines {
        for command in &pipeline.seq {
            if let Command::Simple(simple) = command {
                found.push(simple);
            }
        }
    }
    found
}

#[test]
fn parses_the_gate_relevant_constructs() {
    let scripts = [
        "cd /tmp && rm -f file",
        "cd sub; cat a.md | grep foo > out.txt",
        "if [ -d build ]; then cd build && cargo test; fi",
        "for f in *.txt; do rm \"$f\"; done",
        "diff <(ls a) <(ls b)",
        "x=1 env VAR=2 command --flag value",
        "case $x in a) rm a;; *) echo no;; esac",
        "f() { cd / && rm -rf x; }; f",
    ];
    for script in scripts {
        let program = parse(script).unwrap_or_else(|err| panic!("`{script}`: {err}"));
        assert!(!program.complete_commands.is_empty(), "`{script}`");
    }
}

#[test]
fn a_failed_parse_is_a_typed_error_not_a_panic() {
    // The walker's contract: garbage yields ParseError and the
    // policy's fallback path decides — the parser never crashes the
    // gate.
    assert!(parse("if then fi ((( ").is_err());
}

#[test]
fn the_ast_separates_name_words_and_redirects() {
    // The walker needs: a command's name, its argument words, and
    // redirects as distinct shapes (unbash's Command/Word/Redirect
    // trio). Env assignments ride prefix/suffix as AssignmentWord.
    let program = parse("cd /tmp && rm -f file > log.txt").expect("valid");
    let commands = simple_commands(&program);
    let [cd, rm] = commands.as_slice() else {
        panic!("expected two simple commands, got {commands:?}");
    };
    assert_eq!(cd.word_or_name.as_ref().expect("name").value, "cd");
    assert_eq!(rm.word_or_name.as_ref().expect("name").value, "rm");
    let words: Vec<&str> = rm
        .suffix
        .as_ref()
        .expect("a suffix")
        .0
        .iter()
        .filter_map(|item| match item {
            CommandPrefixOrSuffixItem::Word(word) => Some(word.value.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(words, ["-f", "file"]);
    assert!(
        rm.suffix
            .as_ref()
            .expect("a suffix")
            .0
            .iter()
            .any(|item| matches!(item, CommandPrefixOrSuffixItem::IoRedirect(_))),
        "the redirect is a structural item"
    );
}

#[test]
fn process_substitution_is_structural() {
    // unbash models `<(cmd)` as a word part; brush-parser surfaces it
    // as a suffix item — the walker must count it for positional
    // indexing and treat its inner command as dynamic content.
    let program = parse("diff <(ls a) <(ls b)").expect("valid");
    let commands = simple_commands(&program);
    let diff = commands.first().expect("diff");
    assert!(
        diff.suffix
            .as_ref()
            .expect("a suffix")
            .0
            .iter()
            .any(|item| matches!(item, CommandPrefixOrSuffixItem::ProcessSubstitution(_, _)))
    );
}
