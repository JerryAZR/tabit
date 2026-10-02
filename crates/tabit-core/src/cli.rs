//! The command line: parse, validate, and the model-string
//! resolution. Hand-rolled (no clap — six flags do not justify the
//! dependency, the standing ruling); a flag that cannot act in the
//! selected mode is a parse-time error, never a silent no-op.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) struct Args {
    pub(crate) print_prompt: Option<String>,
    pub(crate) session: Option<PathBuf>,
    pub(crate) continue_newest: bool,
    pub(crate) list: bool,
    pub(crate) model: Option<String>,
    pub(crate) max_turns: Option<usize>,
    pub(crate) rewind: Option<usize>,
    pub(crate) json: bool,
    /// Child-role flags (the subagent bridge spawns `--json` with
    /// these): the parent to announce, the tool allow-list, and the
    /// in-memory boot session. `parent_call` pairs the announce with
    /// the spawning tool call's correlation id.
    pub(crate) parent: Option<String>,
    pub(crate) parent_call: Option<String>,
    pub(crate) tools: Option<String>,
    /// The deny twin of `--tools`: names removed from this process's
    /// full toolset — core and extension proxies alike. The spawner's
    /// per-invocation blacklist: a read-write agent denies its own
    /// delegate tool so children cannot recurse through it.
    pub(crate) without: Option<String>,
    pub(crate) ephemeral: bool,
    /// System prompt override — replaces the default preamble
    /// (identity + standing body); the environment block, AGENTS.md
    /// files, and skills catalog append as usual. The subagent
    /// bridge's `--preamble` crossing; also valid with `-p`.
    pub(crate) preamble: Option<String>,
    /// The installed-extension root (JSON mode; default
    /// `~/.tabit/extensions`).
    pub(crate) extensions: Option<PathBuf>,
    /// `tabit install <source>` (task 6): the npm:/git:/path: source.
    pub(crate) install: Option<String>,
    /// `tabit extensions list`.
    pub(crate) extensions_list: bool,
    /// `tabit-core extensions uninstall <name>`.
    pub(crate) extensions_uninstall: Option<String>,
}

const USAGE: &str = "\
usage: tabit-core -p <PROMPT>            print mode: one prompt, one run
       tabit-core --continue -p <PROMPT> resume this project's newest session
       tabit-core --session <path> -p <PROMPT>
                                         resume a specific session file
       tabit-core --continue --rewind <n>
                                         rewind n user messages, then exit;
                                         add -p <PROMPT> to branch with it
       tabit-core -p <PROMPT> --tools <a,b,..> | --without <a,b,..>
                                         filter this run's toolset:
                                         include-if-it-exists (an unknown
                                         name matches nothing; an allow
                                         matching nothing is a tool-less
                                         chatbot); --without also forwards
                                         to subagent children
       tabit-core --json [session flags]
                                         JSON protocol on stdio (scriptable)
                                         child role adds: --parent <id> (the
                                         spawning session), --parent-call <id>
                                         (its tool call), --tools <a,b,..>
                                         (an allow-list), --without <a,b,..>
                                         (a deny list — removed from the
                                         child's core AND extension
                                         tools), --ephemeral (no
                                         file) — the subagent bridge's
                                         flags, all also valid with -p;
                                         --preamble <text> replaces the
                                         default preamble (identity/body);
                                         context appends as usual; also
                                         valid with -p; --extensions
                                         <dir> selects the extension
                                         root (default
                                         ~/.tabit/extensions; also
                                         valid with -p)
       tabit-core install <npm:pkg|git:repo|path:dir>
                                        install an extension package (npm as
                                        plain registry HTTP, no npm CLI; scoped
                                        names nest; missing requirements pull;
                                        $TABIT_NPM_REGISTRY overrides the
                                        registry; update = install again)
       tabit-core extensions list        list installed extension packages
       tabit-core extensions uninstall <name>
                                        remove one (refuses while other
                                        installed packages still require it)
       tabit-core --list                 list this project's sessions

a modeless invocation (bare `tabit-core`) is a usage error — this
binary is the headless backend; the tabit frontend is a separate
binary that spawns `tabit-core --json`.

print mode: Esc aborts the running turn (line-buffered stdin: Esc then
Enter). JSON mode: LF-JSONL frames — the backend's report and
stamped events out, commands in from the very first line (see the
tabit-session protocol module).

       tabit-core --model <model-id|provider/model>
                                       select the model for this run
                                       (default: the resumed session's model,
                                       then default_model in providers.toml,
                                       then the first configured model)

config: providers.toml / auth.toml / settings.toml under ~/.tabit
        (override with TABIT_CONFIG / TABIT_AUTH / TABIT_SETTINGS);
        sessions live in <project>/.tabit/sessions. Extensions install
        under ~/.tabit/extensions and load unless named in
        settings.toml's [extensions] disabled list. The built-in
        permission gate mounts by default; [gate] enabled = false in
        settings.toml opts out";

/// What a parsed command line asks for. `-p` and `--rewind` both select
/// print mode, `--json` selects JSON mode; no mode selected is a usage
/// error (there is no default — the interactive frontend is a separate
/// binary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    List,
    Print,
    Json,
    Install,
    Extensions,
}

/// `None` = nothing on the line selects a mode.
pub(crate) fn mode_of(args: &Args) -> Option<Mode> {
    if args.install.is_some() {
        Some(Mode::Install)
    } else if args.extensions_list || args.extensions_uninstall.is_some() {
        Some(Mode::Extensions)
    } else if args.list {
        Some(Mode::List)
    } else if args.json {
        Some(Mode::Json)
    } else if args.print_prompt.is_some() || args.rewind.is_some() {
        Some(Mode::Print)
    } else {
        None
    }
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::List => "list",
            Mode::Print => "print",
            Mode::Json => "JSON",
            Mode::Install => "install",
            Mode::Extensions => "extensions",
        }
    }
}

/// The flags each mode accepts. A flag that cannot act in the selected
/// mode is a user mistake, rejected loudly at parse time — never a
/// silent no-op. One allow-list per mode replaces the per-pair conflict
/// checks (which kept missing combinations: `--model` with a path,
/// `--session` alone, `--list --continue`, …).
fn validate_mode(args: &Args) -> Result<Mode, String> {
    let Some(mode) = mode_of(args) else {
        return Err(format!(
            "nothing to do — pass -p for print mode, --json for the protocol edge, or a subcommand; this binary is the headless backend, the frontend is separate\n{USAGE}"
        ));
    };
    let present = [
        args.install.is_some().then_some("install <source>"),
        args.extensions_list.then_some("extensions list"),
        args.extensions_uninstall
            .is_some()
            .then_some("extensions uninstall <name>"),
        args.print_prompt.is_some().then_some("-p/--print"),
        args.rewind.is_some().then_some("--rewind"),
        args.session.is_some().then_some("--session"),
        args.continue_newest.then_some("--continue"),
        args.model.is_some().then_some("--model"),
        args.max_turns.is_some().then_some("--max-turns"),
        args.json.then_some("--json"),
        args.list.then_some("--list"),
        args.parent.is_some().then_some("--parent"),
        args.parent_call.is_some().then_some("--parent-call"),
        args.tools.is_some().then_some("--tools"),
        args.without.is_some().then_some("--without"),
        args.ephemeral.then_some("--ephemeral"),
        args.preamble.is_some().then_some("--preamble"),
        args.extensions.is_some().then_some("--extensions"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let allowed: &[&str] = match mode {
        Mode::List => &["--list"],
        Mode::Json => &[
            "--json",
            "--session",
            "--continue",
            "--model",
            "--max-turns",
            "--parent",
            "--parent-call",
            "--tools",
            "--without",
            "--ephemeral",
            "--preamble",
            "--extensions",
        ],
        Mode::Print => &[
            "-p/--print",
            "--rewind",
            "--session",
            "--continue",
            "--model",
            "--max-turns",
            "--preamble",
            "--tools",
            "--without",
            "--extensions",
            "--parent",
            "--parent-call",
            "--ephemeral",
        ],
        Mode::Install => &["install <source>"],
        Mode::Extensions => &["extensions list", "extensions uninstall <name>"],
    };
    if present.iter().any(|flag| !allowed.contains(flag)) {
        return Err(format!(
            "those flags do not combine: {} mode accepts only [{}]; pick one mode\n{USAGE}",
            mode.name(),
            allowed.join(", ")
        ));
    }
    Ok(mode)
}

pub(crate) fn parse_args() -> Result<Args, String> {
    parse_args_from(std::env::args().skip(1))
}

/// Manual parsing over an injectable iterator (no clap: six flags do not
/// justify the dependency); `parse_args_from` is the testable core.
pub(crate) fn parse_args_from<I>(args: I) -> Result<Args, String>
where
    I: Iterator<Item = String>,
{
    let mut parsed = Args {
        print_prompt: None,
        session: None,
        continue_newest: false,
        list: false,
        model: None,
        max_turns: None,
        rewind: None,
        json: false,
        parent: None,
        parent_call: None,
        tools: None,
        without: None,
        ephemeral: false,
        preamble: None,
        extensions: None,
        install: None,
        extensions_list: false,
        extensions_uninstall: None,
    };
    let mut it = args;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--continue" | "-c" => parsed.continue_newest = true,
            "--list" => parsed.list = true,
            "--json" => parsed.json = true,
            "--session" => {
                let value = it.next().ok_or("--session needs a path (see --help)")?;
                parsed.session = Some(PathBuf::from(value));
            }
            "-p" | "--print" => {
                let value = it.next().ok_or("-p needs a prompt (see --help)")?;
                parsed.print_prompt = Some(value);
            }
            "--rewind" => {
                let value = it.next().ok_or("--rewind needs a number (see --help)")?;
                parsed.rewind = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--rewind: `{value}` is not a number"))?,
                );
            }
            "--model" | "-m" => {
                let value = it
                    .next()
                    .ok_or("--model needs provider/model (see --help)")?;
                parsed.model = Some(value);
            }
            "--max-turns" => {
                let value = it.next().ok_or("--max-turns needs a number (see --help)")?;
                parsed.max_turns = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--max-turns: `{value}` is not a number"))?,
                );
            }
            "--parent" => {
                let value = it
                    .next()
                    .ok_or("--parent needs a session id (see --help)")?;
                parsed.parent = Some(value);
            }
            "--parent-call" => {
                let value = it
                    .next()
                    .ok_or("--parent-call needs a call id (see --help)")?;
                parsed.parent_call = Some(value);
            }
            "--tools" => {
                let value = it
                    .next()
                    .ok_or("--tools needs a comma-separated list (see --help)")?;
                parsed.tools = Some(value);
            }
            "--without" => {
                let value = it
                    .next()
                    .ok_or("--without needs a comma-separated list (see --help)")?;
                parsed.without = Some(value);
            }
            "--ephemeral" => parsed.ephemeral = true,
            "--preamble" => {
                let value = it
                    .next()
                    .ok_or("--preamble needs the replacement prompt text (see --help)")?;
                parsed.preamble = Some(value);
            }
            "--extensions" => {
                let value = it
                    .next()
                    .ok_or("--extensions needs a directory (see --help)")?;
                parsed.extensions = Some(PathBuf::from(value));
            }
            "install" => {
                if parsed.install.is_some()
                    || parsed.extensions_list
                    || parsed.extensions_uninstall.is_some()
                {
                    return Err(format!("one subcommand per run\n{USAGE}"));
                }
                let source = it.next().ok_or("install needs a source (see --help)")?;
                parsed.install = Some(source);
            }
            "extensions" => {
                if parsed.install.is_some()
                    || parsed.extensions_list
                    || parsed.extensions_uninstall.is_some()
                {
                    return Err(format!("one subcommand per run\n{USAGE}"));
                }
                match it.next().as_deref() {
                    Some("list") => parsed.extensions_list = true,
                    Some("uninstall") => {
                        let name = it
                            .next()
                            .ok_or("extensions uninstall needs a package name (see --help)")?;
                        parsed.extensions_uninstall = Some(name);
                    }
                    other => {
                        return Err(format!(
                            "extensions expects list or uninstall <name>, not {other:?}\n{USAGE}"
                        ));
                    }
                }
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}`\n{USAGE}"));
            }
            positional => {
                return Err(format!(
                    "unexpected argument `{positional}` — this binary takes flags, not paths (the frontend is separate)\n{USAGE}"
                ));
            }
        }
    }
    // The selected mode's flag set must cover everything present.
    validate_mode(&parsed)?;
    // The ephemeral child has nothing to resume: the two persistence
    // entrances and the in-memory one are mutually exclusive.
    if parsed.ephemeral && (parsed.session.is_some() || parsed.continue_newest) {
        return Err(format!(
            "--ephemeral cannot combine with --session or --continue — an in-memory session resumes nothing\n{USAGE}"
        ));
    }
    Ok(parsed)
}

impl Args {
    /// The assembly's input: the CLI's fields converted to the
    /// library's options (`tabit-app` owns the shape; the binary
    /// owns only argv).
    pub(crate) fn options(&self) -> tabit_app::AppOptions {
        tabit_app::AppOptions {
            session: self.session.clone(),
            continue_newest: self.continue_newest,
            model: self.model.clone(),
            max_turns: self.max_turns,
            parent: self.parent.clone(),
            parent_call: self.parent_call.clone(),
            tools: self.tools.clone(),
            without: self.without.clone(),
            ephemeral: self.ephemeral,
            preamble: self.preamble.clone(),
            extensions: self.extensions.clone(),
        }
    }
}

/// The shared two-model test config.
#[cfg(test)]
pub(crate) fn test_config() -> tabit_config::TabitConfig {
    tabit_config::TabitConfig::from_toml_str(
        r#"
default_model = { provider = "lmstudio", model = "m" }

[providers.lmstudio]
base_url = "http://127.0.0.1:1234/v1"
api = "openai-completions"
keyless = true

[[providers.lmstudio.models]]
id = "m"

[[providers.lmstudio.models]]
id = "m2"
"#,
        std::path::Path::new("providers.toml"),
    )
    .expect("test config")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tabit_app::parse_model;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse_args_from(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_prompt_and_flags() {
        let parsed = args(&["--continue", "--model", "p/m", "-p", "hello world"]).expect("valid");
        assert_eq!(parsed.print_prompt.as_deref(), Some("hello world"));
        assert!(parsed.continue_newest);
        assert_eq!(parsed.model.as_deref(), Some("p/m"));

        let parsed =
            args(&["--session", "s.jsonl", "--max-turns", "5", "-p", "go"]).expect("valid");
        assert_eq!(
            parsed.session.as_deref(),
            Some(std::path::Path::new("s.jsonl"))
        );
        assert_eq!(parsed.max_turns, Some(5));

        let parsed = args(&["--continue", "--rewind", "2"]).expect("valid");
        assert_eq!(parsed.rewind, Some(2));

        // The preamble override parses (and is not a child-only flag:
        // print mode accepts it too — validate_mode's allow-lists).
        let parsed = args(&["--json", "--preamble", "You are a research probe"]).expect("valid");
        assert_eq!(parsed.preamble.as_deref(), Some("You are a research probe"));

        let parsed = args(&["--list"]).expect("valid");
        assert!(parsed.list);
    }

    #[test]
    fn rejects_missing_values_unknown_flags_and_positionals() {
        assert!(args(&["--session"]).is_err());
        assert!(args(&["--model"]).is_err());
        assert!(args(&["-p"]).is_err());
        assert!(args(&["--rewind"]).is_err());
        assert!(args(&["--max-turns", "x"]).is_err());
        assert!(args(&["--rewind", "x"]).is_err());
        let unknown = args(&["--bogus"]).expect_err("unknown flag");
        assert!(unknown.contains("--bogus"), "{unknown}");
        // Positionals are not paths anymore — the frontend is separate.
        let positional = args(&["hello"]).expect_err("a positional is a usage error");
        assert!(positional.contains("unexpected argument"), "{positional}");
    }

    #[test]
    fn print_mode_is_selected_by_prompt_or_rewind() {
        assert!(
            args(&[]).is_err(),
            "a modeless invocation is a usage error, not a default mode"
        );
        assert_eq!(
            mode_of(&args(&["-p", "hi"]).expect("print")),
            Some(Mode::Print)
        );
        assert_eq!(
            mode_of(&args(&["--rewind", "1"]).expect("rewind")),
            Some(Mode::Print)
        );
        assert_eq!(mode_of(&args(&["--json"]).expect("json")), Some(Mode::Json));
        assert_eq!(mode_of(&args(&["--list"]).expect("list")), Some(Mode::List));
    }

    #[test]
    fn flags_outside_the_selected_mode_are_loud_parse_errors() {
        // One allow-list per mode, so every combination class is covered,
        // including ones the old per-pair checks missed.
        let cases: &[&[&str]] = &[
            &["--json", "-p", "hi"], // json × print
            &["--json", "--rewind", "1"],
            &["--list", "-p", "hi"], // list is exclusive (was a silent win)
            &["--list", "--continue"],
        ];
        for case in cases {
            let error = args(case).expect_err("foreign flags must not parse");
            assert!(error.contains("do not combine"), "case {case:?}: {error}");
        }

        // Session flags with no mode select nothing — the modeless
        // usage error (there is no default mode anymore).
        let modeless = args(&["--session", "s"]).expect_err("session alone is modeless");
        assert!(modeless.contains("nothing to do"), "{modeless}");

        // The shared flags still combine within print and json modes.
        args(&["--continue", "--session", "s", "--model", "p/m", "-p", "hi"])
            .expect("print accepts the shared flags");
        args(&["--continue", "--json", "--max-turns", "5"]).expect("json accepts the shared flags");
    }

    #[test]
    fn json_mode_parses_and_print_conflicts_at_parse_time() {
        let parsed = args(&["--continue", "--json"]).expect("valid");
        assert!(parsed.json && parsed.continue_newest);
        assert_eq!(mode_of(&parsed), Some(Mode::Json));

        // json × print is a parse error now (validate_mode), not a
        // run-time dispatch check.
        let conflict = args(&["--json", "-p", "hi"]).expect_err("parse rejects the combination");
        assert!(conflict.contains("do not combine"), "{conflict}");
    }

    #[test]
    fn the_extensions_flag_crosses_both_session_modes() {
        // The 2026-09-27 ruling: both modes boot the same extension
        // world, so both take the root flag.
        let parsed = args(&["--json", "--extensions", "C:/tmp/ext"]).expect("valid");
        assert_eq!(parsed.extensions, Some(PathBuf::from("C:/tmp/ext")));
        let parsed = args(&["-p", "hi", "--extensions", "C:/tmp/ext"]).expect("valid");
        assert_eq!(parsed.extensions, Some(PathBuf::from("C:/tmp/ext")));
    }

    #[test]
    fn child_role_flags_cross_both_session_modes() {
        let parsed = args(&[
            "--json",
            "--parent",
            "p1",
            "--parent-call",
            "c7",
            "--tools",
            "read,bash",
            "--ephemeral",
        ])
        .expect("child role parses");
        assert_eq!(parsed.parent.as_deref(), Some("p1"));
        assert_eq!(parsed.parent_call.as_deref(), Some("c7"));
        assert_eq!(parsed.tools.as_deref(), Some("read,bash"));
        assert!(parsed.ephemeral);

        // The pairing flag needs its id like every value flag.
        let bare = args(&["--json", "--parent", "p1", "--parent-call"])
            .expect_err("parent-call without a value");
        assert!(bare.contains("--parent-call needs a call id"), "{bare}");

        // The ephemeral boot resumes nothing — the persistence
        // entrances are mutually exclusive.
        let conflict =
            args(&["--json", "--ephemeral", "--continue"]).expect_err("ephemeral × continue");
        assert!(conflict.contains("--ephemeral"), "{conflict}");

        // The child-role flags cross to print mode too (owner ruling
        // 2026-09-27: a one-shot print child is a natural shape —
        // `-p task --parent X --ephemeral` — and print's host wiring
        // already threads the lineage; the flag table was the only
        // wall).
        let print_child = args(&[
            "--parent",
            "p1",
            "--parent-call",
            "c7",
            "--ephemeral",
            "-p",
            "hi",
        ])
        .expect("a print child parses");
        assert_eq!(print_child.parent.as_deref(), Some("p1"));
        assert_eq!(print_child.parent_call.as_deref(), Some("c7"));
        assert!(print_child.ephemeral);
    }

    #[test]
    fn install_subcommands_parse_exclusively() {
        let parsed = args(&["install", "npm:thing"]).expect("parses");
        assert_eq!(parsed.install.as_deref(), Some("npm:thing"));
        assert_eq!(mode_of(&parsed), Some(Mode::Install));

        let parsed = args(&["extensions", "list"]).expect("parses");
        assert!(parsed.extensions_list);
        assert_eq!(mode_of(&parsed), Some(Mode::Extensions));

        let parsed = args(&["extensions", "uninstall", "thing"]).expect("parses");
        assert_eq!(parsed.extensions_uninstall.as_deref(), Some("thing"));

        // The subcommands accept nothing else (and unknown actions
        // name what was expected).
        let error = args(&["install", "npm:x", "--json"]).expect_err("exclusive");
        assert!(error.contains("do not combine"), "{error}");
        let error = args(&["extensions", "bogus"]).expect_err("unknown action");
        assert!(error.contains("expects list or uninstall"), "{error}");
        assert!(args(&["install"]).is_err(), "install needs a source");
        assert!(
            args(&["extensions", "uninstall"]).is_err(),
            "uninstall needs a name"
        );
    }

    #[test]
    fn model_strings_resolve_against_the_config() {
        let config = test_config();
        assert_eq!(
            parse_model("lmstudio/m2", &config)
                .expect("qualified")
                .model,
            "m2"
        );
        // A bare id works when it is unambiguous.
        assert_eq!(
            parse_model("m2", &config).expect("bare").provider,
            "lmstudio"
        );
        assert!(parse_model("nope", &config).is_err());
        assert!(parse_model("lmstudio/nope", &config).is_err());
    }
}
