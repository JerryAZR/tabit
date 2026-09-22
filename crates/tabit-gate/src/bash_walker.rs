//! Bash AST walker — traverses the brush-parser parse tree, adapted
//! from pi-sanity's `bash-walker.ts` over the unbash tree. Extracts
//! commands and redirects for checking.
//!
//! cd tracking: the walker threads a working-directory state through
//! the AST so relative paths can be resolved where they will actually
//! land.
//! - Sequences (`&&`, `||`, `;`) and control-flow branches
//!   (if/while/for/case) are followed sequentially: a cd affects
//!   everything after it, even if a branch might not run (conservative
//!   over-approximation).
//! - Subshells, command/process substitutions, pipeline segments,
//!   background statements, function definitions, and coproc bodies
//!   are isolated: their cd never leaks out (real bash semantics).
//! - Untrackable cd (`cd -`, dynamic target like `$VAR`/`$(...)`) sets
//!   the state to `None`; commands then fall back to the base context
//!   cwd. A later absolute cd re-anchors tracking.
//!
//! The adapter (the one sanctioned deviation): unbash exposes typed
//! word parts, brush-parser keeps words as raw text. Here each word's
//! raw text is decomposed with [`word_parts`] (over
//! `brush_parser::word::parse`), whose typed pieces recover everything
//! the walker needs — quoted/literal values, command and backquoted
//! substitutions (with the inner script as a string), parameter and
//! arithmetic expansions — so word values, dynamic detection, and
//! substitution extraction keep the TS semantics. Dynamic detection
//! comes from the typed pieces: command/backquoted substitutions,
//! parameter expansions, and arithmetic expansions are dynamic; tilde
//! expansions, single-quoted, ANSI-C quoted, and plain text are not —
//! and a top-level double-quoted wrapper is not (matching TS's
//! top-level-only part check) while its substitution children are
//! still extracted. Brace expansions and extglobs, which brush also
//! keeps as literal text, are recovered lexically over the unquoted
//! text pieces so they stay dynamic as in TS. Two more adapter shims:
//! a `select` loop, which brush 0.4 cannot parse, is retried as its
//! structurally identical `for` form (same wordlist/body walk, same
//! cd flow as TS's `Select` case), and every command rides in a
//! one-segment pipeline (unbash only built pipelines for actual `|`
//! chains, so single commands walk unisolated and cd flows).

use std::collections::HashSet;

use brush_parser::ast::{
    AndOr, Assignment, AssignmentValue, Command, CommandPrefixOrSuffixItem, CompoundCommand,
    CompoundListItem, ExtendedTestExpr, IoFileRedirectKind, IoRedirect, ProcessSubstitutionKind,
    SeparatorOperator,
};
use brush_parser::word::ParameterExpr;

use crate::path_permission::default_context;
use crate::path_utils::{
    PathContext, PreprocessOptions, RUNTIME_DEFAULTS, is_drive_absolute, preprocess_path,
};

/// The result of walking one script (TS `WalkResult`; brush reports
/// one flat typed error where unbash carried a list, so `errors`
/// holds messages).
#[derive(Clone, Debug, Default)]
pub struct WalkResult {
    pub commands: Vec<FoundCommand>,
    pub errors: Vec<String>,
}

/// One command extracted from the script (TS `FoundCommand`).
#[derive(Clone, Debug, Default)]
pub struct FoundCommand {
    pub name: Option<String>,
    /// All arguments in original order (static + dynamic).
    pub args: Vec<String>,
    /// Indices of arguments that contain dynamic content
    /// (substitutions, expansions, etc.). These are skipped during
    /// path checking but still count for positional indexing.
    pub dynamic_indices: HashSet<usize>,
    pub redirects: Vec<FoundRedirect>,
    /// Working directory in effect when this command runs: tracked
    /// through cd, falls back to the walk context cwd when tracking
    /// is unavailable.
    pub cwd: String,
}

/// One redirect extracted from a command (TS `FoundRedirect`).
#[derive(Clone, Debug, Default)]
pub struct FoundRedirect {
    pub operator: String,
    pub target: String,
    pub is_input: bool,
    pub is_output: bool,
}

/// Walk a script under the default path context (TS `walkBash`'s
/// default: the process cwd, home, and tmpdir).
pub fn walk(script: &str) -> WalkResult {
    walk_in_context(script, &default_context())
}

/// Walk a script with an explicit base context (TS `walkBash(command,
/// context)`).
pub(crate) fn walk_in_context(script: &str, context: &PathContext) -> WalkResult {
    let program = match parse_program(script) {
        Ok(program) => program,
        Err(error) => {
            return WalkResult {
                commands: vec![],
                errors: vec![error.to_string()],
            };
        }
    };

    let mut walker = Walker {
        base: context,
        cwd_state: Some(context.cwd.clone()),
        commands: vec![],
        errors: vec![],
    };
    walker.walk_program(&program);
    WalkResult {
        commands: walker.commands,
        errors: walker.errors,
    }
}

/// One script through the tokenizer + parser, as the parser smoke
/// test pins it. When the parse fails and the script contains a
/// `select` word — a bash keyword brush 0.4 has no grammar rule for —
/// the walk retries with `select` read as `for`: the loop shapes are
/// structurally identical (variable, wordlist, do-group body), which
/// is exactly the walk TS's `Select` case performs.
fn parse_program(script: &str) -> Result<brush_parser::ast::Program, brush_parser::ParseError> {
    let tokens =
        brush_parser::tokenize_str(script).map_err(|err| brush_parser::ParseError::Tokenizing {
            inner: err,
            position: None,
        })?;
    match brush_parser::parse_tokens(&tokens, &brush_parser::ParserOptions::default()) {
        Ok(program) => Ok(program),
        Err(first_error) => {
            let has_select = tokens
                .iter()
                .any(|t| matches!(t, brush_parser::Token::Word(word, _) if word == "select"));
            if !has_select {
                return Err(first_error);
            }
            let retried: Vec<brush_parser::Token> = tokens
                .into_iter()
                .map(|t| match t {
                    brush_parser::Token::Word(word, loc) if word == "select" => {
                        brush_parser::Token::Word("for".to_string(), loc)
                    }
                    other => other,
                })
                .collect();
            brush_parser::parse_tokens(&retried, &brush_parser::ParserOptions::default())
        }
    }
}

/// cd flags per bash builtin: -L, -P, -e (and combinations).
fn is_cd_flag(arg: &str) -> bool {
    arg.len() >= 2 && arg.starts_with('-') && arg[1..].chars().all(|c| matches!(c, 'L' | 'P' | 'e'))
}

/// A syntactically absolute target (POSIX root, `~`, or Windows
/// drive).
fn is_absolute_target(t: &str) -> bool {
    t.starts_with('/') || t.starts_with('~') || is_drive_absolute(t)
}

struct Walker<'a> {
    base: &'a PathContext,
    /// Tracked working directory; `None` = untrackable (fallback to
    /// base cwd).
    cwd_state: Option<String>,
    commands: Vec<FoundCommand>,
    errors: Vec<String>,
}

impl Walker<'_> {
    /// Run a sub-walk with an isolated copy of the cwd state; the
    /// entry state still applies WITHIN (a cd inside affects later
    /// commands inside) but changes never leak out (subshells,
    /// substitutions, pipelines, background, ...).
    fn isolated(&mut self, run: impl FnOnce(&mut Self)) {
        let saved = self.cwd_state.clone();
        run(self);
        self.cwd_state = saved;
    }

    fn effective_cwd(&self) -> String {
        self.cwd_state
            .clone()
            .unwrap_or_else(|| self.base.cwd.clone())
    }

    /// Parse a script and walk its commands, recording errors from
    /// inner substitutions (whose text is only parsed here).
    fn parse_and_walk_inner(&mut self, script: &str) {
        let program = match parse_program(script) {
            Ok(program) => program,
            Err(error) => {
                self.errors.push(error.to_string());
                return;
            }
        };
        self.walk_program(&program);
    }

    fn walk_program(&mut self, program: &brush_parser::ast::Program) {
        for complete_command in &program.complete_commands {
            for item in &complete_command.0 {
                // CompoundListItem(and_or_list, separator): the
                // separator follows its item, so Async marks that
                // item's list as a background job — async jobs run in
                // a subshell: cd cannot leak out (TS
                // `Statement.background`).
                let CompoundListItem(and_or_list, separator) = item;
                if matches!(separator, SeparatorOperator::Async) {
                    self.isolated(|w| w.walk_and_or_list(and_or_list));
                } else {
                    self.walk_and_or_list(and_or_list);
                }
            }
        }
    }

    fn walk_and_or_list(&mut self, list: &brush_parser::ast::AndOrList) {
        // && and || are followed sequentially (by-design
        // simplification): a cd anywhere in the chain affects
        // everything after it.
        self.walk_pipeline(&list.first);
        for additional in &list.additional {
            match additional {
                AndOr::And(pipeline) | AndOr::Or(pipeline) => self.walk_pipeline(pipeline),
            }
        }
    }

    fn walk_pipeline(&mut self, pipeline: &brush_parser::ast::Pipeline) {
        if pipeline.seq.len() == 1 {
            // A single command is not a pipeline: it runs in the
            // current shell (unbash only produces Pipeline nodes for
            // actual `|` chains; brush wraps every command).
            self.walk_command(&pipeline.seq[0]);
        } else {
            // Each pipeline segment runs in its own subshell in bash.
            for command in &pipeline.seq {
                self.isolated(|w| w.walk_command(command));
            }
        }
    }

    fn walk_command(&mut self, command: &Command) {
        match command {
            Command::Simple(simple) => self.walk_simple(simple),
            Command::Compound(compound, redirects) => {
                self.walk_compound(compound);
                if let Some(redirects) = redirects {
                    self.walk_redirect_list(redirects);
                }
            }
            Command::Function(function) => {
                // Walk the function name.
                self.walk_word(&function.fname.value);
                // Walk the body — a definition does not execute, so
                // its cd cannot leak into following commands.
                // Isolated.
                let body = &function.body;
                self.isolated(|w| w.walk_compound(&body.0));
                if let Some(redirects) = &body.1 {
                    self.walk_redirect_list(redirects);
                }
            }
            Command::ExtendedTest(test, redirects) => {
                self.walk_extended_test(&test.expr);
                if let Some(redirects) = redirects {
                    self.walk_redirect_list(redirects);
                }
            }
        }
    }

    fn walk_compound(&mut self, compound: &CompoundCommand) {
        match compound {
            // { ...; } runs in the current shell: state flows through.
            CompoundCommand::BraceGroup(group) => self.walk_compound_list(&group.list),
            // (...) runs in a subshell: cd inside never leaks out.
            CompoundCommand::Subshell(subshell) => {
                self.isolated(|w| w.walk_compound_list(&subshell.list));
            }
            CompoundCommand::ForClause(for_clause) => {
                // The loop variable is a plain name in brush (nothing
                // to walk). Walk the wordlist (often contains command
                // substitutions), then the body sequentially.
                if let Some(values) = &for_clause.values {
                    for word in values {
                        self.walk_word(&word.value);
                    }
                }
                self.walk_compound_list(&for_clause.body.list);
            }
            CompoundCommand::CaseClause(case_clause) => {
                // Walk the case word (may contain command
                // substitutions), then each item's patterns and body.
                self.walk_word(&case_clause.value.value);
                for case_item in &case_clause.cases {
                    for pattern in &case_item.patterns {
                        self.walk_word(&pattern.value);
                    }
                    if let Some(list) = &case_item.cmd {
                        self.walk_compound_list(list);
                    }
                }
            }
            CompoundCommand::IfClause(if_clause) => {
                self.walk_compound_list(&if_clause.condition);
                self.walk_compound_list(&if_clause.then);
                if let Some(elses) = &if_clause.elses {
                    for else_clause in elses {
                        if let Some(condition) = &else_clause.condition {
                            self.walk_compound_list(condition);
                        }
                        self.walk_compound_list(&else_clause.body);
                    }
                }
            }
            CompoundCommand::WhileClause(clause) | CompoundCommand::UntilClause(clause) => {
                self.walk_compound_list(&clause.0);
                self.walk_compound_list(&clause.1.list);
            }
            CompoundCommand::Coprocess(coprocess) => {
                // Walk the optional name; the body runs async in a
                // subshell: isolated.
                if let Some(name) = &coprocess.name {
                    self.walk_word(&name.value);
                }
                self.isolated(|w| w.walk_command(&coprocess.body));
            }
            CompoundCommand::ArithmeticForClause(clause) => {
                // The arithmetic expressions are raw text in brush —
                // and, per LIMITATIONS.md, a blind spot in TS too
                // (unbash's arithmetic walk extracted nothing). Walk
                // the body.
                self.walk_compound_list(&clause.body.list);
            }
            // `(( expr ))`: raw arithmetic text, nothing extractable
            // (documented arithmetic blind spot).
            CompoundCommand::Arithmetic(_) => {}
        }
    }

    fn walk_compound_list(&mut self, list: &brush_parser::ast::CompoundList) {
        for item in &list.0 {
            self.walk_and_or_list(&item.0);
        }
    }

    fn walk_extended_test(&mut self, expr: &ExtendedTestExpr) {
        match expr {
            ExtendedTestExpr::UnaryTest(_, word) => self.walk_word(&word.value),
            ExtendedTestExpr::BinaryTest(_, left, right) => {
                self.walk_word(&left.value);
                self.walk_word(&right.value);
            }
            ExtendedTestExpr::And(left, right) | ExtendedTestExpr::Or(left, right) => {
                self.walk_extended_test(left);
                self.walk_extended_test(right);
            }
            ExtendedTestExpr::Not(operand) => self.walk_extended_test(operand),
            ExtendedTestExpr::Parenthesized(operand) => self.walk_extended_test(operand),
        }
    }

    fn walk_redirect_list(&mut self, redirects: &brush_parser::ast::RedirectList) {
        for redirect in &redirects.0 {
            self.walk_redirect(redirect);
        }
    }

    /// Walk one redirect's target for command substitutions (TS
    /// walks `redirect.target` and `redirect.body`).
    fn walk_redirect(&mut self, redirect: &IoRedirect) {
        match redirect {
            IoRedirect::File(_, _, target) => match target {
                brush_parser::ast::IoFileRedirectTarget::Filename(word) => {
                    self.walk_word(&word.value);
                }
                brush_parser::ast::IoFileRedirectTarget::Duplicate(word) => {
                    self.walk_word(&word.value);
                }
                brush_parser::ast::IoFileRedirectTarget::ProcessSubstitution(_, subshell) => {
                    self.isolated(|w| w.walk_compound_list(&subshell.list));
                }
                brush_parser::ast::IoFileRedirectTarget::Fd(_) => {}
            },
            IoRedirect::HereDocument(_, doc) => {
                // Target (delimiter) + body.
                self.walk_word(&doc.here_end.value);
                self.walk_word(&doc.doc.value);
            }
            IoRedirect::HereString(_, word) => self.walk_word(&word.value),
            IoRedirect::OutputAndError(word, _) => self.walk_word(&word.value),
        }
    }

    fn walk_simple(&mut self, simple: &brush_parser::ast::SimpleCommand) {
        // Classify prefix/suffix items: redirects in source order,
        // argument words (assignments ride along as their raw word
        // text, exactly as unbash's suffix words), and structural
        // process substitutions.
        let mut redirects: Vec<&IoRedirect> = vec![];
        let mut suffix_words: Vec<&str> = vec![];
        let mut assignments: Vec<&Assignment> = vec![];
        let mut process_substitutions: Vec<(
            &ProcessSubstitutionKind,
            &brush_parser::ast::SubshellCommand,
        )> = vec![];
        if let Some(prefix) = &simple.prefix {
            for item in &prefix.0 {
                if let CommandPrefixOrSuffixItem::IoRedirect(redirect) = item {
                    redirects.push(redirect);
                }
            }
        }
        if let Some(suffix) = &simple.suffix {
            for item in &suffix.0 {
                match item {
                    CommandPrefixOrSuffixItem::IoRedirect(redirect) => redirects.push(redirect),
                    CommandPrefixOrSuffixItem::Word(word) => suffix_words.push(&word.value),
                    // unbash keeps suffix assignments (`env VAR=2 …`)
                    // as plain suffix words; the adapter treats the
                    // assignment's raw word text the same way.
                    CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
                        suffix_words.push(&word.value);
                    }
                    CommandPrefixOrSuffixItem::ProcessSubstitution(kind, subshell) => {
                        // Process substitutions ARE structural: they
                        // count as an argument slot (positionals keep
                        // their indices) and are dynamic content.
                        process_substitutions.push((kind, subshell));
                    }
                }
            }
        }
        // Walk prefix assignments for command substitutions in values
        // (e.g. VAR=$(cmd)).
        if let Some(prefix) = &simple.prefix {
            for item in &prefix.0 {
                if let CommandPrefixOrSuffixItem::AssignmentWord(assignment, _) = item {
                    assignments.push(assignment);
                }
            }
        }

        // Check if the command name is entirely a dynamic substitution
        // (e.g. `$(echo rm)` or `` `which rm` ``) AND the command has
        // no other parts (suffix, prefix, redirects). In that case we
        // skip pushing the outer command since we can't determine its
        // name. (A process substitution cannot stand in name position
        // in brush's grammar.)
        let entirely_dynamic_name = simple
            .word_or_name
            .as_ref()
            .map(|name| {
                let decomposed = word_parts::decompose(&name.value);
                decomposed.pieces.len() == 1
                    && matches!(
                        &decomposed.pieces[0],
                        word_parts::Piece::CommandSubstitution { .. }
                            | word_parts::Piece::Backquoted { .. }
                    )
            })
            .unwrap_or(false);
        let has_other_parts = !suffix_words.is_empty()
            || simple.prefix.as_ref().is_some_and(|p| !p.0.is_empty())
            || !redirects.is_empty();

        // IMPORTANT: Walk inner commands BEFORE pushing the outer
        // command. This ensures nested commands appear first in the
        // results.

        // Walk command name to extract any command substitutions.
        if let Some(name) = &simple.word_or_name {
            self.walk_word(&name.value);
        }
        // Walk assignments (prefix and suffix) for command
        // substitutions in their values.
        for assignment in &assignments {
            self.walk_assignment(assignment);
        }
        // Walk suffix arguments for command substitutions (e.g.
        // `cat $(rm /secret)`).
        for word in &suffix_words {
            self.walk_word(word);
        }
        // Walk process substitution bodies (isolated, like every
        // substitution).
        for (_, subshell) in &process_substitutions {
            self.isolated(|w| w.walk_compound_list(&subshell.list));
        }
        // Walk redirect targets for command substitutions (e.g.
        // `> $(echo file)`).
        for redirect in redirects.iter().copied() {
            self.walk_redirect(redirect);
        }

        // Push the command AFTER extracting nested commands. Skip only
        // if the command is ENTIRELY a dynamic substitution with no
        // other parts — e.g. `$(echo $(rm /))` skips, but
        // `$(echo rm) file` is kept. Nameless commands are kept when
        // they carry redirects (e.g. a bare `> file`): the redirect
        // target must be path-checked.
        let name_value = simple
            .word_or_name
            .as_ref()
            .map(|name| word_parts::word_value(&name.value))
            .filter(|value| !value.is_empty());
        if (name_value.is_some() || !redirects.is_empty())
            && !(entirely_dynamic_name && !has_other_parts)
        {
            let mut args: Vec<String> = vec![];
            let mut dynamic_indices = HashSet::new();
            for (i, word) in suffix_words.iter().enumerate() {
                args.push(word_parts::word_value(word));
                if word_parts::is_dynamic_word(word) {
                    dynamic_indices.insert(i);
                }
            }
            // Process substitutions occupy argument slots as dynamic
            // content (their value text is never path-checked).
            for (kind, subshell) in &process_substitutions {
                args.push(format!("{kind}({})", subshell.list));
                dynamic_indices.insert(args.len() - 1);
            }
            let found = FoundCommand {
                name: name_value,
                args,
                dynamic_indices,
                redirects: redirects.iter().map(|r| extract_redirect(r)).collect(),
                cwd: self.effective_cwd(),
            };
            // Update the tracked cwd for everything after this cd
            // (pushed after applying: the state only affects later
            // commands, so the order is observationally the same).
            let is_cd = found.name.as_deref() == Some("cd");
            if is_cd {
                self.apply_cd(&found);
            }
            self.commands.push(found);
        }
    }

    /// Update the tracked cwd for a `cd` command that is about to run.
    fn apply_cd(&mut self, cmd: &FoundCommand) {
        // Find the target: first non-flag argument ("--" ends options).
        let mut target: Option<&String> = None;
        let mut target_idx: Option<usize> = None;
        let mut opts_ended = false;
        for (i, a) in cmd.args.iter().enumerate() {
            if !opts_ended {
                if a == "--" {
                    opts_ended = true;
                    continue;
                }
                if is_cd_flag(a) {
                    continue;
                }
            }
            target = Some(a);
            target_idx = Some(i);
            break;
        }

        let Some(target) = target else {
            self.cwd_state = Some(self.base.home.clone()); // bare `cd` goes home
            return;
        };
        if target == "-" {
            self.cwd_state = None; // OLDPWD is unknowable statically
            return;
        }
        if target_idx.is_some_and(|idx| cmd.dynamic_indices.contains(&idx)) {
            self.cwd_state = None; // dynamic target ($VAR, $(...))
            return;
        }
        if self.cwd_state.is_none() && !is_absolute_target(target) {
            return; // relative target while untrackable: stay untrackable
        }
        let effective = self.effective_cwd();
        // Tracked cwd is kept in NATIVE form (canonicalize: false): it
        // feeds path resolution in the checker, not glob matching. The
        // git-bash /c/ input conversion still applies (win32) so both
        // cd spellings work.
        let ctx = PathContext {
            cwd: effective,
            ..self.base.clone()
        };
        self.cwd_state = Some(preprocess_path(
            target,
            &ctx,
            PreprocessOptions {
                canonicalize: false,
                expand_env_vars: false,
                ..RUNTIME_DEFAULTS
            },
        ));
    }

    /// Walk a Word to extract command substitutions from its pieces.
    fn walk_word(&mut self, word_text: &str) {
        let decomposed = word_parts::decompose(word_text);
        for piece in &decomposed.pieces {
            self.walk_piece(piece);
        }
    }

    fn walk_piece(&mut self, piece: &word_parts::Piece) {
        match piece {
            word_parts::Piece::CommandSubstitution { script, .. }
            | word_parts::Piece::Backquoted { script, .. } => {
                // Command/process substitutions run in a subshell
                // context: isolated.
                self.isolated(|w| w.parse_and_walk_inner(script));
            }
            word_parts::Piece::DoubleQuoted(children)
            | word_parts::Piece::GettextDoubleQuoted(children) => {
                // Recursively walk parts inside double quotes.
                for child in children {
                    self.walk_piece(child);
                }
            }
            word_parts::Piece::ParameterExpansion(expr, _) => {
                // ${VAR:-$(cmd)} — operand/default/replace/slice
                // strings may contain command substitutions.
                self.walk_parameter_expr(expr);
            }
            word_parts::Piece::ArithmeticExpansion { .. } => {
                // Raw arithmetic text; substitutions inside arithmetic
                // are a documented blind spot (LIMITATIONS.md).
            }
            word_parts::Piece::Text(_)
            | word_parts::Piece::SingleQuoted(_)
            | word_parts::Piece::AnsiCQuoted(_)
            | word_parts::Piece::EscapeSequence(_)
            | word_parts::Piece::TildeExpansion(_) => {
                // No nested commands.
            }
        }
    }

    /// Walk a parameter expansion's string fields for command
    /// substitutions (TS `walkParameterExpansion`: operand, slice
    /// offset/length, replace pattern/replacement).
    fn walk_parameter_expr(&mut self, expr: &ParameterExpr) {
        let texts: Vec<String> = match expr {
            ParameterExpr::UseDefaultValues { default_value, .. }
            | ParameterExpr::AssignDefaultValues { default_value, .. } => {
                default_value.clone().into_iter().collect()
            }
            ParameterExpr::IndicateErrorIfNullOrUnset { error_message, .. } => {
                error_message.clone().into_iter().collect()
            }
            ParameterExpr::UseAlternativeValue {
                alternative_value, ..
            } => alternative_value.clone().into_iter().collect(),
            ParameterExpr::Substring { offset, length, .. } => {
                let mut texts = vec![offset.value.clone()];
                if let Some(length) = length {
                    texts.push(length.value.clone());
                }
                texts
            }
            ParameterExpr::ReplaceSubstring {
                pattern,
                replacement,
                ..
            } => {
                let mut texts = vec![pattern.clone()];
                texts.extend(replacement.clone());
                texts
            }
            _ => vec![],
        };
        for text in texts {
            self.walk_text_for_substitutions(&text);
        }
    }

    /// Walk free text (a parameter-expansion default value, slice
    /// bound, ...) for command substitutions — selectively: the text
    /// is already inside an expansion, so only substitution pieces are
    /// descended into (never the enclosing parameter expansion again).
    fn walk_text_for_substitutions(&mut self, text: &str) {
        let decomposed = word_parts::decompose(text);
        for piece in &decomposed.pieces {
            match piece {
                word_parts::Piece::CommandSubstitution { script, .. }
                | word_parts::Piece::Backquoted { script, .. } => {
                    self.isolated(|w| w.parse_and_walk_inner(script));
                }
                word_parts::Piece::DoubleQuoted(children)
                | word_parts::Piece::GettextDoubleQuoted(children) => {
                    for child in children {
                        self.walk_text_for_substitutions_piece(child);
                    }
                }
                _ => {}
            }
        }
    }

    fn walk_text_for_substitutions_piece(&mut self, piece: &word_parts::Piece) {
        match piece {
            word_parts::Piece::CommandSubstitution { script, .. }
            | word_parts::Piece::Backquoted { script, .. } => {
                self.isolated(|w| w.parse_and_walk_inner(script));
            }
            word_parts::Piece::DoubleQuoted(children)
            | word_parts::Piece::GettextDoubleQuoted(children) => {
                for child in children {
                    self.walk_text_for_substitutions_piece(child);
                }
            }
            _ => {}
        }
    }

    /// Walk an Assignment to extract commands from values (TS
    /// `walkAssignment`): scalar `VAR=$(cmd)` and array
    /// `ARR=($(cmd1) $(cmd2))`.
    fn walk_assignment(&mut self, assignment: &Assignment) {
        match &assignment.value {
            AssignmentValue::Scalar(word) => self.walk_word(&word.value),
            AssignmentValue::Array(elements) => {
                for (_, word) in elements {
                    self.walk_word(&word.value);
                }
            }
        }
    }
}

/// Map one brush redirect to the walker's redirect shape (TS
/// `extractRedirects` operator classification; `<>` and `<&` are
/// neither input nor output there).
fn extract_redirect(redirect: &IoRedirect) -> FoundRedirect {
    let (operator, target): (String, String) = match redirect {
        IoRedirect::File(_, kind, target) => {
            let operator = match kind {
                IoFileRedirectKind::Read => "<",
                IoFileRedirectKind::Write => ">",
                IoFileRedirectKind::Append => ">>",
                IoFileRedirectKind::ReadAndWrite => "<>",
                IoFileRedirectKind::Clobber => ">|",
                IoFileRedirectKind::DuplicateInput => "<&",
                IoFileRedirectKind::DuplicateOutput => ">&",
            };
            let target = match target {
                brush_parser::ast::IoFileRedirectTarget::Filename(word) => {
                    word_parts::word_value(&word.value)
                }
                brush_parser::ast::IoFileRedirectTarget::Fd(fd) => fd.to_string(),
                brush_parser::ast::IoFileRedirectTarget::ProcessSubstitution(kind, subshell) => {
                    format!("{kind}({})", subshell.list)
                }
                brush_parser::ast::IoFileRedirectTarget::Duplicate(word) => {
                    word_parts::word_value(&word.value)
                }
            };
            (operator.to_string(), target)
        }
        IoRedirect::HereDocument(_, doc) => (
            "<<".to_string(),
            word_parts::word_value(&doc.here_end.value),
        ),
        IoRedirect::HereString(_, word) => ("<<<".to_string(), word_parts::word_value(&word.value)),
        IoRedirect::OutputAndError(word, append) => (
            if *append { "&>>" } else { "&>" }.to_string(),
            word_parts::word_value(&word.value),
        ),
    };
    FoundRedirect {
        is_input: matches!(operator.as_str(), "<" | "<<" | "<<<"),
        is_output: matches!(operator.as_str(), ">" | ">>" | ">&" | "&>" | "&>>" | ">|"),
        operator,
        target,
    }
}

/// Word decomposition over brush's raw word text — the adapter's
/// replacement for unbash's typed `WordPart`s. Also provides the
/// quoting-aware word value (TS `wordValue`/`partValue`).
pub(crate) mod word_parts {
    use brush_parser::ParserOptions;
    use brush_parser::word::{ParameterExpr, WordPiece};

    /// One decomposed piece of a word, quoting resolved:
    /// unquoted/single-quoted/ANSI-C/escape text carries its clean
    /// value; substitutions carry the inner script; expansions carry
    /// their raw text.
    #[derive(Clone, Debug)]
    pub(crate) enum Piece {
        Text(String),
        SingleQuoted(String),
        AnsiCQuoted(String),
        /// Raw escape text, backslash included (`"\ "`).
        EscapeSequence(String),
        DoubleQuoted(Vec<Piece>),
        GettextDoubleQuoted(Vec<Piece>),
        /// Raw tilde expression (`~`, `~user`).
        TildeExpansion(String),
        /// The typed parameter expression plus its raw text.
        ParameterExpansion(ParameterExpr, String),
        CommandSubstitution {
            script: String,
            raw: String,
        },
        Backquoted {
            script: String,
            raw: String,
        },
        ArithmeticExpansion {
            raw: String,
        },
    }

    /// A word decomposed into its pieces.
    #[derive(Clone, Debug)]
    pub(crate) struct Decomposed {
        pub pieces: Vec<Piece>,
    }

    /// Decompose a word's raw text (parse failures fall back to one
    /// literal text piece — brush words from a valid program parse
    /// cleanly; the fallback keeps garbage checkable as raw text).
    pub(crate) fn decompose(word_text: &str) -> Decomposed {
        match brush_parser::word::parse(word_text, &ParserOptions::default()) {
            Ok(pieces) => Decomposed {
                pieces: pieces
                    .into_iter()
                    .map(|p| convert_piece(p.piece, word_text, p.start_index, p.end_index))
                    .collect(),
            },
            Err(_) => Decomposed {
                pieces: vec![Piece::Text(word_text.to_string())],
            },
        }
    }

    fn convert_piece(piece: WordPiece, source: &str, start: usize, end: usize) -> Piece {
        let raw = source[start..end].to_string();
        match piece {
            WordPiece::Text(text) => Piece::Text(text),
            WordPiece::SingleQuotedText(text) => Piece::SingleQuoted(text),
            WordPiece::AnsiCQuotedText(text) => Piece::AnsiCQuoted(text),
            WordPiece::EscapeSequence(text) => Piece::EscapeSequence(text),
            WordPiece::DoubleQuotedSequence(children) => Piece::DoubleQuoted(
                children
                    .into_iter()
                    .map(|c| convert_piece(c.piece, source, c.start_index, c.end_index))
                    .collect(),
            ),
            WordPiece::GettextDoubleQuotedSequence(children) => Piece::GettextDoubleQuoted(
                children
                    .into_iter()
                    .map(|c| convert_piece(c.piece, source, c.start_index, c.end_index))
                    .collect(),
            ),
            WordPiece::TildeExpansion(_) => Piece::TildeExpansion(raw),
            WordPiece::ParameterExpansion(expr) => Piece::ParameterExpansion(expr, raw),
            WordPiece::CommandSubstitution(script) => Piece::CommandSubstitution { script, raw },
            WordPiece::BackquotedCommandSubstitution(script) => Piece::Backquoted { script, raw },
            WordPiece::ArithmeticExpression(_) => Piece::ArithmeticExpansion { raw },
        }
    }

    /// Dynamic words cannot be statically evaluated: command
    /// substitutions, backticks, parameter expansions, and arithmetic
    /// expansions (TS `isDynamicWord`'s typed-part list; top-level
    /// pieces only — a double-quoted wrapper is not dynamic, as in TS).
    pub(crate) fn is_dynamic_word(word_text: &str) -> bool {
        decompose(word_text).pieces.iter().any(|piece| match piece {
            Piece::CommandSubstitution { .. }
            | Piece::Backquoted { .. }
            | Piece::ParameterExpansion(..)
            | Piece::ArithmeticExpansion { .. } => true,
            // Brace expansions and extglobs are literal text to brush;
            // recover them lexically over unquoted text (TS's
            // BraceExpansion / ExtendedGlob part types).
            Piece::Text(text) => has_brace_expansion(text) || has_extglob(text),
            _ => false,
        })
    }

    /// Compute a word's value with bash quoting semantics: quotes are
    /// syntax, not content. Literal / single-quoted / ANSI-C / escaped
    /// parts contribute their clean value; double-quoted parts recurse
    /// into their children (splicing, e.g. pre"fix"post ->
    /// prefixpost); dynamic parts contribute their raw text — the word
    /// is still flagged dynamic via its pieces, so downstream skipping
    /// is unaffected (TS `wordValue` + `partValue`).
    pub(crate) fn word_value(word_text: &str) -> String {
        let decomposed = decompose(word_text);
        let mut out = String::new();
        for piece in &decomposed.pieces {
            piece_value(piece, &mut out);
        }
        out
    }

    fn piece_value(piece: &Piece, out: &mut String) {
        match piece {
            Piece::Text(text) | Piece::SingleQuoted(text) | Piece::AnsiCQuoted(text) => {
                out.push_str(text);
            }
            Piece::EscapeSequence(raw) => {
                // The piece keeps the backslash; the value is the
                // escaped character itself.
                if let Some(escaped) = raw.strip_prefix('\\') {
                    out.push_str(escaped);
                }
            }
            Piece::DoubleQuoted(children) | Piece::GettextDoubleQuoted(children) => {
                for child in children {
                    piece_value(child, out);
                }
            }
            Piece::TildeExpansion(raw)
            | Piece::ParameterExpansion(_, raw)
            | Piece::CommandSubstitution { raw, .. }
            | Piece::Backquoted { raw, .. }
            | Piece::ArithmeticExpansion { raw } => out.push_str(raw),
        }
    }

    /// A `{a,...}` expansion shape in unquoted text: a `{` with a
    /// top-level comma before its matching `}`.
    fn has_brace_expansion(text: &str) -> bool {
        let bytes = text.as_bytes();
        let Some(open) = text.find('{') else {
            return false;
        };
        let mut depth = 0usize;
        for &b in bytes.iter().skip(open) {
            match b {
                b'{' => depth += 1,
                b',' if depth == 1 => return true,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        // No comma in this group; keep scanning for a
                        // later group.
                        if let Some(next) = text[open + 1..].find('{') {
                            return has_brace_expansion(&text[open + 1 + next..]);
                        }
                        return false;
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// An extglob operator immediately followed by `(` (bash's
    /// `@(`, `!(`, `*(`, `+(`, `?(` patterns).
    fn has_extglob(text: &str) -> bool {
        let bytes = text.as_bytes();
        bytes
            .windows(2)
            .any(|pair| matches!(pair[0], b'@' | b'!' | b'*' | b'+' | b'?') && pair[1] == b'(')
    }
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

    fn ctx(cwd: &str) -> PathContext {
        PathContext {
            cwd: cwd.to_string(),
            home: "/home/user".to_string(),
            repo: None,
            tmpdir: "/tmp".to_string(),
            platform: crate::path_utils::Platform::Other,
        }
    }

    fn walk_in(cwd: &str, script: &str) -> WalkResult {
        walk_in_context(script, &ctx(cwd))
    }

    #[test]
    fn extracts_simple_commands_and_flags() {
        let result = walk_in("/p", "rm -rf dir/");
        assert!(result.errors.is_empty());
        assert_eq!(result.commands.len(), 1);
        assert_eq!(result.commands[0].name.as_deref(), Some("rm"));
        assert_eq!(result.commands[0].args, vec!["-rf", "dir/"]);
        assert!(result.commands[0].dynamic_indices.is_empty());
    }

    #[test]
    fn empty_script_yields_no_commands() {
        let result = walk_in("/p", "");
        assert!(result.commands.is_empty());
        assert!(result.errors.is_empty());
    }

    #[test]
    fn garbage_is_a_typed_error() {
        let result = walk_in("/p", "if then fi ((( ");
        assert!(result.commands.is_empty());
        assert!(!result.errors.is_empty());
    }

    #[test]
    fn inner_commands_are_extracted_before_outer() {
        let result = walk_in("/p", "cat $(rm /secret)");
        let names: Vec<_> = result.commands.iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, vec![Some("rm".into()), Some("cat".into())]);
        // The dynamic arg is flagged on the outer command.
        assert!(result.commands[1].dynamic_indices.contains(&0));
    }

    #[test]
    fn quoted_words_unquote_into_values() {
        let result = walk_in("/p", "rm \"/a b\"");
        assert_eq!(result.commands[0].args, vec!["/a b"]);
        let result = walk_in("/p", "echo hi > \"/etc/out\"");
        assert_eq!(result.commands[0].redirects[0].target, "/etc/out");
        assert!(result.commands[0].redirects[0].is_output);
    }

    #[test]
    fn process_substitution_is_a_dynamic_arg_slot() {
        let result = walk_in("/p", "diff <(ls a) <(ls b)");
        assert_eq!(result.commands.len(), 3);
        let diff = &result.commands[2];
        assert_eq!(diff.name.as_deref(), Some("diff"));
        assert_eq!(diff.args.len(), 2, "procsubs count as argument slots");
        assert_eq!(diff.dynamic_indices, HashSet::from([0, 1]));
    }

    #[test]
    fn cd_tracking_follows_sequences_and_reanchors() {
        let result = walk_in("/project", "cd /etc && rm conf");
        assert_eq!(result.commands[1].cwd, "/etc");
        // Absolute cd re-anchors after an untrackable one.
        let result = walk_in("/project", "cd $DIR; cd /etc; rm conf");
        assert_eq!(result.commands[2].cwd, "/etc");
        // Untrackable cd falls back to the base cwd.
        let result = walk_in("/project", "cd $DIR && rm file.txt");
        assert_eq!(result.commands[1].cwd, "/project");
    }

    #[test]
    fn subshells_and_pipelines_isolate_cd() {
        let result = walk_in("/project", "(cd /etc); echo done > log.txt");
        assert_eq!(result.commands[1].cwd, "/project");
        let result = walk_in("/project", "cd /etc; cat x | grep y");
        // Pipeline segments run isolated: their cwd is the entry cwd.
        assert_eq!(result.commands[2].cwd, "/etc");
    }

    #[test]
    fn bare_redirects_carry_redirects_and_cwd() {
        let result = walk_in("/etc", "> stamp");
        assert_eq!(result.commands.len(), 1);
        assert_eq!(result.commands[0].name, None);
        assert_eq!(result.commands[0].redirects[0].target, "stamp");
        assert_eq!(result.commands[0].cwd, "/etc");
    }

    #[test]
    fn standalone_dynamic_name_without_parts_is_skipped() {
        let result = walk_in("/p", "$(echo $(rm /))");
        let names: Vec<_> = result.commands.iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, vec![Some("rm".into()), Some("echo".into())]);
    }
}
