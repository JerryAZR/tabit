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
//! `tabit-core` — the tabit backend: headless, no UI, no frontend
//! references (frontends spawn this binary, never the other way).
//!
//! Two modes: print mode (`-p`) — one prompt in, one outer loop out,
//! events print as they happen — and JSON mode (`--json`) — the
//! session protocol as LF-JSONL over stdio, for scripts and frontends.
//! The session persists project-locally, and the printed session path
//! resumes the conversation later.
//!
//! ```text
//! tabit-core -p "list the rust files in this project"     # new session
//! tabit-core --continue -p "now count lines in each"      # resume the newest
//! tabit-core --session <path> -p "what did we conclude?"  # resume a specific one
//! tabit-core --continue --rewind 1                        # rewind, then exit
//! tabit-core --json                                       # protocol on stdio
//! tabit-core --list                                       # show this project's sessions
//! ```

mod extensions;
// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and the ask-answer reads live on that ergonomics
// (the same allowance the extension SDK's bins carry).
#[allow(clippy::indexing_slicing)]
mod gate;
mod json;

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use tabit_config::{AuthConfig, TabitConfig};
use tabit_protocol::SessionCommand;
use tabit_session::SessionEvent;
use tabit_session::{
    ModelRegistry, ModelSelection, Session, SessionBuilder, SessionHost, SessionHostWiring,
    SessionStore, build_system_prompt, build_system_prompt_with_base,
};
use tabit_tools::dynamic_contextual;

#[derive(Debug, Clone)]
struct Args {
    print_prompt: Option<String>,
    session: Option<PathBuf>,
    continue_newest: bool,
    list: bool,
    model: Option<String>,
    max_turns: Option<usize>,
    rewind: Option<usize>,
    json: bool,
    /// Child-role flags (the subagent bridge spawns `--json` with
    /// these): the parent to announce, the tool allow-list, and the
    /// in-memory boot session. `parent_call` pairs the announce with
    /// the spawning tool call's correlation id.
    parent: Option<String>,
    parent_call: Option<String>,
    tools: Option<String>,
    /// The deny twin of `--tools`: names removed from this process's
    /// full toolset — core and extension proxies alike. The spawner's
    /// per-invocation blacklist: a read-write agent denies its own
    /// delegate tool so children cannot recurse through it.
    without: Option<String>,
    ephemeral: bool,
    /// System prompt override — replaces the default preamble
    /// (identity + standing body); the environment block, AGENTS.md
    /// files, and skills catalog append as usual. The subagent
    /// bridge's `--preamble` crossing; also valid with `-p`.
    preamble: Option<String>,
    /// The installed-extension root (JSON mode; default
    /// `~/.tabit/extensions`).
    extensions: Option<PathBuf>,
    /// `tabit install <source>` (task 6): the npm:/git:/path: source.
    install: Option<String>,
    /// `tabit extensions list`.
    extensions_list: bool,
    /// `tabit-core extensions uninstall <name>`.
    extensions_uninstall: Option<String>,
}

const USAGE: &str = "\
usage: tabit-core -p <PROMPT>            print mode: one prompt, one run
       tabit-core --continue -p <PROMPT> resume this project's newest session
       tabit-core --session <path> -p <PROMPT>
                                         resume a specific session file
       tabit-core --continue --rewind <n>
                                         rewind n user messages, then exit;
                                         add -p <PROMPT> to branch with it
       tabit-core --json [session flags]
                                         JSON protocol on stdio (scriptable)
                                         child role adds: --parent <id> (the
                                         spawning session), --parent-call <id>
                                         (its tool call), --tools <a,b,..>
                                         (an allow-list), --without <a,b,..>
                                         (a deny list — removed from the
                                         child's core AND extension
                                         tools), --ephemeral (no
                                         file) — the subagent bridge's flags;
                                         --preamble <text> replaces the
                                         default preamble (identity/body);
                                         context appends as usual; also
                                         valid with -p; --extensions
                                         <dir> selects the extension
                                         root (default
                                         ~/.tabit/extensions)
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
Enter). JSON mode: LF-JSONL frames — initialize, then message/abort
commands in; stamped events out (see the tabit-session protocol module).

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
enum Mode {
    List,
    Print,
    Json,
    Install,
    Extensions,
}

/// `None` = nothing on the line selects a mode.
fn mode_of(args: &Args) -> Option<Mode> {
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

fn parse_args() -> Result<Args, String> {
    parse_args_from(std::env::args().skip(1))
}

/// Manual parsing over an injectable iterator (no clap: six flags do not
/// justify the dependency); `parse_args_from` is the testable core.
fn parse_args_from<I>(args: I) -> Result<Args, String>
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

/// Resolve a `--model` value against the config: `provider/model` when
/// the text before the first `/` names a configured provider, otherwise
/// a bare model id that must be unambiguous (see
/// `TabitConfig::resolve_model_ref`).
fn parse_model(raw: &str, config: &TabitConfig) -> Result<ModelSelection, String> {
    let (provider, model) = config
        .resolve_model_ref(raw)
        .map_err(|message| format!("--model: {message}"))?;
    Ok(ModelSelection::new(provider, model))
}

fn list_sessions(store: &SessionStore) -> Result<(), String> {
    let summaries = store.list().map_err(|e| e.to_string())?;
    if summaries.is_empty() {
        println!("no sessions in {}", store.dir().display());
        return Ok(());
    }
    for summary in summaries {
        println!(
            "{}  {:>4} entries  {:<10}  {}",
            summary.created_at,
            summary.entry_count,
            summary.id.get(..8).map(str::to_string).unwrap_or_default(),
            summary.path.display()
        );
    }
    Ok(())
}

fn print_event(event: &SessionEvent) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match event {
        SessionEvent::UserMessage { .. } => {}
        // The submit-time ack for messages that wait; print mode cannot
        // submit mid-run (Esc aborts), so this never fires in practice.
        SessionEvent::MessageQueued { .. } => {}
        SessionEvent::SkillsAvailable { .. } => {}
        // Backend-level catalogs never ride a run's print stream.
        SessionEvent::ExtensionsAvailable { .. } => {}
        SessionEvent::MessagesDiscarded { messages } => {
            let _ = writeln!(out, "[{} queued message(s) discarded]", messages.len());
        }
        // Cards render on stderr in the event loop; stdout stays the
        // answer channel.
        SessionEvent::InteractionRequest { .. } => {}
        SessionEvent::RunAborted { .. } => {
            let _ = writeln!(
                out,
                "
[aborted]"
            );
        }
        SessionEvent::TextDelta { text, .. } => {
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
        }
        SessionEvent::ReasoningDelta { reasoning, .. } => {
            // Reasoning goes to stderr so stdout stays the answer channel.
            let _ = std::io::stderr().write_all(reasoning.as_bytes());
        }
        SessionEvent::ToolCall {
            name, arguments, ..
        } => {
            let _ = writeln!(out, "\n→ {name} {}", arguments.as_deref().unwrap_or(""));
            let _ = out.flush();
        }
        SessionEvent::ToolResult { name, .. } => {
            let _ = writeln!(out, "← {name} done");
        }
        SessionEvent::TurnRetried { .. } => {
            let _ = writeln!(out, "[turn rejected by a hook; retrying]");
        }
        SessionEvent::CompletionCall { .. } => {}
        // Turn brackets are attribution machinery (the GUI's grouping);
        // the terminal view shows content as it streams.
        SessionEvent::TurnStarted { .. } | SessionEvent::TurnCommitted { .. } => {}
        // Informational (ENGINE.md behavior delta 9): the run continues;
        // the note is the user's cue that a steer can ask for more.
        SessionEvent::TurnTruncated { .. } => {
            let _ = writeln!(out, "[model output was truncated (output token limit)]");
        }
        SessionEvent::RunFinished { durable: false, .. } => {
            let _ = writeln!(out, "[output pending on disk — persist degraded]");
        }
        SessionEvent::RunFinished { .. } => {
            let _ = writeln!(out);
        }
        // Not a printable stream event: run() turns it into the process
        // error (stderr, exit 1) once the stream has ended.
        SessionEvent::RunFailed { .. } => {}
        // Replay brackets, checkouts, and model changes never reach
        // print mode (it never requests the pass and has no checkout
        // surface); the arms exist for exhaustiveness.
        SessionEvent::ReplayStarted { .. }
        | SessionEvent::ReplayDone
        | SessionEvent::CheckedOut { .. }
        | SessionEvent::ModelChanged { .. } => {}
        // The compaction bracket (v7): stdout stays the answer channel,
        // so the boundaries note on stderr and the summary stays quiet
        // in print mode.
        SessionEvent::CompactionBegin => {
            let _ = writeln!(std::io::stderr(), "[compacting the conversation…]");
        }
        SessionEvent::CompactionDelta { .. }
        | SessionEvent::CompactionStep { .. }
        | SessionEvent::CompactionRetried
        | SessionEvent::CompactionEnd { .. } => {}
        SessionEvent::CompactionFailed { message, .. } => {
            let _ = writeln!(std::io::stderr(), "warning: compaction failed: {message}");
        }
        // The host's session catalog and creations are frontend
        // concerns; print mode is a single-session consumer.
        SessionEvent::SessionsAvailable { .. } | SessionEvent::SessionOpened { .. } => {}
        // Non-terminal error conditions (startup degradations,
        // persistence): stderr is the human surface in print mode —
        // stdout stays the answer channel.
        SessionEvent::Error { message, .. } => {
            let _ = writeln!(std::io::stderr(), "warning: {message}");
        }
        SessionEvent::NativeItem { .. } => {}
    }
}

/// The human startup banner (stderr — stdout is the answer channel in
/// print mode and the protocol channel in JSON mode).
fn print_banner(session: &Session) {
    let stats = session.stats();
    if stats.total_usage.total_tokens > 0 {
        eprintln!(
            "resuming {} ({} prior turns of context)",
            session
                .id()
                .get(..8)
                .map(str::to_string)
                .unwrap_or_default(),
            session.context().len()
        );
    } else {
        let where_ = session
            .path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "in memory".to_string());
        eprintln!("session {where_} started");
    }
}

/// The process-wide child registry — one table per process by design
/// (the host routes through it, subagent spawns register into it), so
/// a `OnceLock` is the honest shape rather than threading an `Arc`
/// through every assembly site.
fn child_router() -> std::sync::Arc<tabit_session::ChildRouter> {
    static ROUTER: std::sync::OnceLock<std::sync::Arc<tabit_session::ChildRouter>> =
        std::sync::OnceLock::new();
    ROUTER
        .get_or_init(tabit_session::ChildRouter::shared)
        .clone()
}

static SKILLS_CATALOG: std::sync::OnceLock<std::sync::Arc<tabit_session::skills::Skills>> =
    std::sync::OnceLock::new();

/// The process-wide skills catalog — one catalog per process, the
/// consistency guarantee between the prompt's listing, the `skill`
/// tool's lookup, and the wire snapshot (two discoveries could race a
/// directory edit and disagree; one cannot). Built against the
/// process cwd (the backend never chdirs — the same fact the session
/// store roots at). The JSON boot seeds it with the extension
/// walker's contribution folded under the ladder BEFORE any assembly
/// reads it; unseeded, the plain ladder discovery is the catalog
/// (print mode, extension-less hosts).
fn skills_catalog() -> std::sync::Arc<tabit_session::skills::Skills> {
    SKILLS_CATALOG
        .get_or_init(|| {
            // A failed `current_dir` assembles nothing anyway (the loud
            // gate lives in `assemble_session`); here it degrades to a
            // discovery that finds nothing.
            let cwd = std::env::current_dir().unwrap_or_default();
            std::sync::Arc::new(tabit_session::skills::discover(&cwd))
        })
        .clone()
}

/// Seed the process's catalog (the JSON boot): the ladder discovery
/// with the extension entries folded under it. Seeding after the
/// catalog's first reader is an ordering bug, not a condition to
/// absorb — the boot runs before any assembly by construction.
#[allow(clippy::panic)] // the sanctioned crash below (AGENTS.md doctrine)
fn seed_skills_catalog(extension_skills: tabit_session::skills::Skills) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let seeded = tabit_session::skills::discover(&cwd).with_extension_defaults(extension_skills);
    if SKILLS_CATALOG.set(std::sync::Arc::new(seeded)).is_err() {
        panic!(
            "internal invariant violated: the skills catalog was read before the boot seeded it"
        );
    }
}

/// The tabit-core executable subprocess children spawn: this very
/// binary (the pi self-spawn pattern). `current_exe`, no exceptions —
/// an inherited `TABIT_CORE_BIN` (a frontend's dev override for
/// finding the backend) must not diverge children from the running
/// image.
fn tabit_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot resolve the tabit-core executable: {e}"))
}

/// The invocation's tool filter (`--tools` allow, `--without` deny),
/// split and validated against the process's full candidate set —
/// core and extension proxies alike. A name nothing offers is a loud
/// startup error listing what exists: a typo'd filter that quietly
/// kept or dropped the wrong tool would look like a broken agent.
/// Allow first, then deny — the surviving set is
/// allowed-and-not-denied.
fn tool_filter(
    args: &Args,
    candidate: &[rig_agent::tool::DynamicTool],
) -> Result<(Option<Vec<String>>, Vec<String>), String> {
    let offered: Vec<&str> = candidate.iter().map(|tool| tool.name()).collect();
    let split = |spec: &Option<String>, flag: &str| -> Result<Option<Vec<String>>, String> {
        let Some(raw) = spec.as_deref() else {
            return Ok(None);
        };
        let mut names = Vec::new();
        for name in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !offered.contains(&name) {
                return Err(format!(
                    "{flag}: unknown tool `{name}` — this process offers: {}",
                    offered.join(", ")
                ));
            }
            names.push(name.to_string());
        }
        Ok(Some(names))
    };
    Ok((
        split(&args.tools, "--tools")?,
        split(&args.without, "--without")?.unwrap_or_default(),
    ))
}

/// Keep the tools the invocation's filter admits: allowed (when an
/// allow-list crossed) and not denied. Pure name matching — both
/// specs were validated against the full candidate set already, so
/// subsets (the child core set behind `SubagentParts::tools`) filter
/// without re-validation; a proxy-only name simply matches nothing
/// here, which is correct (proxy shaping is the child's business,
/// via the crossing flags).
fn retain_filtered(
    tools: Vec<rig_agent::tool::DynamicTool>,
    allow: &Option<Vec<String>>,
    deny: &[String],
) -> Vec<rig_agent::tool::DynamicTool> {
    tools
        .into_iter()
        .filter(|tool| {
            let name = tool.name();
            allow
                .as_ref()
                .is_none_or(|names| names.iter().any(|n| n == name))
                && !deny.iter().any(|n| n == name)
        })
        .collect()
}

fn assemble_session(
    args: &Args,
    registry: ModelRegistry,
    selection: ModelSelection,
    resume_target: Option<PathBuf>,
    store: SessionStore,
    extensions: Option<&std::sync::Arc<extensions::Mounted>>,
) -> Result<Session, String> {
    let cwd = std::env::current_dir()
        .map_err(|e| format!("cannot determine the working directory: {e}"))?;
    // Built once per process: the prompt must stay byte-stable for the
    // provider's prompt cache (see the prompt module docs). The skills
    // catalog is the same once-per-process fact — one discovery feeds
    // the prompt's listing, the tool's lookup, and the wire snapshot.
    let skills = skills_catalog();
    // `--preamble` replaces the default preamble — the identity and
    // standing body — while the environment block, AGENTS.md files,
    // and skills catalog append as usual (ruled 2026-09: the spawner
    // owns the child's voice; tabit still owns the truthful context).
    let preamble = match &args.preamble {
        Some(text) if text.trim().is_empty() => {
            return Err("the --preamble override is empty".to_string());
        }
        Some(text) => {
            build_system_prompt_with_base(text, &cwd, &skills).map_err(|e| e.to_string())?
        }
        None => build_system_prompt(&cwd, &skills).map_err(|e| e.to_string())?,
    };

    // Subagent support (ROADMAP item 5): the process-wide parts, whose
    // toolset is the child toolset — the parent's minus the subagent
    // tool itself, so children cannot spawn children (recursion depth
    // is enforced by omission). A child-role process (`--parent`)
    // mounts that toolset only: it does not spawn.
    let (children, parent_core) = core_sets(args)?;
    // The process's candidate toolset: its core set plus the extension
    // mount — replaced core tools unmount, the proxies join (one
    // name, one tool, resolved at this assembly). Children resolve
    // against their own core set (the child set): they boot their own
    // hosts.
    let candidate: Vec<rig_agent::tool::DynamicTool> = match extensions {
        Some(mounted) => {
            let replaced = mounted.replaced_core();
            parent_core
                .into_iter()
                .filter(|tool| !replaced.iter().any(|name| name == tool.name()))
                .chain(mounted.tools().iter().cloned())
                .collect()
        }
        None => parent_core,
    };
    // The invocation's filter applies once, here, over the full
    // candidate — `--tools`/`--without` shape extension proxies the
    // same as core tools (a whitelisted read-only agent gets no
    // extension write tools; a denied delegate tool cannot recurse).
    let (allow, deny) = tool_filter(args, &candidate)?;
    let mounted = retain_filtered(candidate, &allow, &deny);
    let subagents = std::sync::Arc::new(tabit_session::subagent::SubagentParts {
        tools: retain_filtered(children, &allow, &deny),
        max_turns: args.max_turns.unwrap_or(tabit_session::DEFAULT_MAX_TURNS),
        router: child_router(),
        exe: tabit_exe()?,
        // Children boot their own hosts against the parent's root
        // (the same packages, the same rules).
        extensions: extension_root(args).unwrap_or_default(),
    });

    // The hook surface: the built-in permission gate (pi-sanity's
    // policy, in-process — a default must not fail open on a dead
    // extension; settings.toml's [gate] enabled = false opts out)
    // ahead of whatever the extension mount carries — forwarded
    // policy hooks of installed packages — composed through the
    // builder's one seam. Children mount their own (they boot their
    // own hosts, the 2026-09 ruling).
    let gate_enabled = tabit_config::SettingsConfig::load_default()
        .map_err(|e| e.to_string())?
        .gate
        .enabled;
    let hooks = {
        let mut stack = if gate_enabled {
            gate::PermissionGate::stack()
        } else {
            rig_agent::agent::HookStack::new()
        };
        if let Some(mounted) = extensions {
            stack = stack.merge(mounted.hooks());
        }
        stack
    };
    let mut builder = SessionBuilder::new(
        store,
        registry.config().clone(),
        registry.auth().clone(),
        selection,
    )
    .map_err(|e| e.to_string())?
    .preamble(preamble)
    .model_factory(registry.factory())
    .hooks(hooks)
    .subagents(subagents)
    .skills(skills);
    for tool in mounted {
        builder = builder.dynamic_tool(tool);
    }
    if let Some(max_turns) = args.max_turns {
        builder = builder.max_turns(max_turns);
    }

    if let Some(path) = &resume_target {
        let (session, _report) = builder.resume(path).map_err(|e| e.to_string())?;
        Ok(session)
    } else {
        let cwd = cwd.display().to_string();
        if args.ephemeral {
            // The child role's in-memory boot: nothing on disk, the
            // process's lifetime is the session's.
            builder.ephemeral(&cwd).map_err(|e| e.to_string())
        } else {
            builder.create(&cwd).map_err(|e| e.to_string())
        }
    }
}

/// The toolset a subagent child runs: every coding tool (contextual —
/// they read the session cwd and the run token from the per-run
/// ToolContext) plus the skill tool, except the subagent tool.
fn child_tools() -> Vec<rig_agent::tool::DynamicTool> {
    vec![
        dynamic_contextual(tabit_tools::Read),
        dynamic_contextual(tabit_tools::Write),
        dynamic_contextual(tabit_tools::Edit),
        tabit_tools::shell_tool(),
        tabit_session::skills::skill_tool(),
    ]
}

/// The two core toolsets every assembly derives from: the child set
/// (every coding tool) and the parent set (the child set plus the
/// subagent tool). Pure derivation — the invocation's tool filter
/// applies later, once, over the full candidate set (core plus
/// extension proxies; see [`tool_filter`]). The extension mount's
/// conflict baseline is the parent set — exactly what the session
/// would mount without extensions.
fn core_sets(
    args: &Args,
) -> Result<
    (
        Vec<rig_agent::tool::DynamicTool>,
        Vec<rig_agent::tool::DynamicTool>,
    ),
    String,
> {
    let children = child_tools();
    let mut parent = children.clone();
    if args.parent.is_none() {
        parent.push(tabit_session::subagent::subagent_tool());
    }
    Ok((children, parent))
}

/// The first-run setup guide: a fresh install has no config, which is
/// normal — the failure message must teach, not scare.
fn setup_guide(detail: &str) -> String {
    let example = r#"create ~/.tabit/providers.toml (or point $TABIT_CONFIG at a file):

    default_model = "lmstudio/your-model-id"   # optional; the first model is the fallback

    [providers.lmstudio]
    base_url = "http://127.0.0.1:1234/v1"
    api = "openai-completions"
    keyless = true

    [[providers.lmstudio.models]]
    id = "your-model-id"

API keys (only if the endpoint needs one) go in ~/.tabit/auth.toml:

    [lmstudio]
    api_key = "..." "#;
    format!("first-run setup needed: {detail}\n\n{example}\n")
}

/// JSON-mode setup failure — the config/auth file is the problem — so
/// the rejection carries the first-run guide (a fresh install has no
/// providers.toml — the most common first run; the message must teach,
/// not scare).
fn json_setup_failure(detail: &str) -> Result<i32, String> {
    json_reject(setup_guide(detail))
}

/// JSON-mode startup failure that is *not* a config problem (session
/// unreadable, model unbuildable, cwd gone): reject with the plain
/// reason — the setup guide would be advice for a problem the user
/// does not have.
fn json_startup_failure(detail: &str) -> Result<i32, String> {
    json_reject(format!("could not start the session: {detail}"))
}

/// One `initialize_rejected` frame to stdout (a startup screen, not a
/// crash), the same text on stderr, exit 1.
fn json_reject(reason: String) -> Result<i32, String> {
    let frame = tabit_protocol::ServerControlFrame::InitializeRejected {
        reason: reason.clone(),
    };
    println!("{}", tabit_protocol::to_wire_line(&frame));
    eprintln!("{reason}");
    Ok(1)
}

fn run() -> Result<i32, String> {
    let args = parse_args()?;
    let config = TabitConfig::load_default().map_err(|e| e.to_string());
    let auth = AuthConfig::load_default().map_err(|e| e.to_string());

    // Sanctioned crash (AGENTS.md doctrine): parse rejects modeless
    // invocations, so the match always has a mode here.
    #[allow(clippy::expect_used)]
    match mode_of(&args).expect("parse validated a mode") {
        Mode::List => {
            let store = SessionStore::project_default();
            list_sessions(&store)?;
            Ok(0)
        }
        Mode::Install => {
            // The install command owns no config or sessions — the
            // root and the registry are its whole world.
            let source_text = args.install.clone().unwrap_or_default();
            let source = tabit_ext_install::Source::parse(&source_text)?;
            let root = install_root()?;
            let registry = std::env::var("TABIT_NPM_REGISTRY")
                .unwrap_or_else(|_| tabit_ext_install::DEFAULT_REGISTRY.to_string());
            let installed = tabit_ext_install::Installer::new(root, registry).install(&source)?;
            println!(
                "installed: {} (pickup at the next backend start)",
                installed.packages.join(", ")
            );
            Ok(0)
        }
        Mode::Extensions => {
            let root = install_root()?;
            let installer =
                tabit_ext_install::Installer::new(&root, tabit_ext_install::DEFAULT_REGISTRY);
            if args.extensions_list {
                let disabled = tabit_config::SettingsConfig::load_default()
                    .map(|settings| settings.disabled_extensions())
                    .unwrap_or_default();
                let listed = installer.list();
                if listed.is_empty() {
                    println!("no extensions installed under {}", root.display());
                    return Ok(0);
                }
                for (listed, refused) in listed {
                    match refused {
                        Some(reason) => {
                            println!("{:<24} BROKEN   {reason}", listed.name)
                        }
                        None => {
                            let marks = if listed.is_static {
                                "static"
                            } else if disabled.contains(&listed.name) {
                                "disabled"
                            } else {
                                "mounted"
                            };
                            let requires = if listed.requires.is_empty() {
                                String::new()
                            } else {
                                format!(" (requires {})", listed.requires.join(", "))
                            };
                            println!(
                                "{:<24} {:<9} v{} {marks}{requires}",
                                listed.name, marks, listed.version
                            );
                        }
                    }
                }
                return Ok(0);
            }
            let name = args.extensions_uninstall.clone().unwrap_or_default();
            installer.uninstall(&name)?;
            println!("uninstalled {name}");
            Ok(0)
        }
        Mode::Json => {
            // A fresh install has no providers.toml — perfectly
            // normal, and the most common first run. Fail gracefully:
            // reject the handshake with a setup guide instead of
            // dying stderr-only (the owner's first-run ruling).
            let (config, auth) = match (config, auth) {
                (Ok(config), Ok(auth)) => (Arc::new(config), Arc::new(auth)),
                (Err(detail), _) | (_, Err(detail)) => return json_setup_failure(&detail),
            };
            // Settings (the extension disable list's layers): absence
            // is normal — a bare machine disables nothing, packages
            // mount by default — while a broken file is a loud
            // startup failure.
            let settings = match tabit_config::SettingsConfig::load_default() {
                Ok(settings) => settings,
                Err(detail) => return json_startup_failure(&detail.to_string()),
            };
            let disabled = settings.disabled_extensions();
            // One scan feeds every consumer — launch, the providers
            // fragment merge, the skills tables — so they cannot
            // disagree (the same one-scan law as the catalog).
            let found = extension_root(&args)
                .as_deref()
                .map(tabit_ext::manifest::scan)
                .unwrap_or_default();
            let launchable = partition(found, &disabled);
            // Providers fragments merge under the user config (the
            // user's own ids win silently; only a fragment colliding
            // with an earlier fragment warns).
            let mut merged = (*config).clone();
            let user_ids: std::collections::HashSet<String> =
                merged.providers.keys().cloned().collect();
            let mut warnings = Vec::new();
            for (name, dir) in &launchable.packages {
                merge_fragment_into(&mut merged, name, dir, &user_ids, &mut warnings);
            }
            for warning in &warnings {
                eprintln!("warning: {warning}");
            }
            // The skills tables: the extension walker produces what
            // the packages ship, with their original paths, and the
            // process's one catalog folds them under the ladder —
            // seeded before any assembly reads it (the prompt build
            // is the first reader). No filesystem writes: the
            // entries' locations ARE the packages' paths.
            seed_skills_catalog(extension_skills_catalog(&launchable.packages));
            let registry = ModelRegistry::new(Arc::new(merged), auth);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            // The extension host boots and every handshake resolves
            // BEFORE the session assembly: tools exist at session
            // build (the byte-stability law), and the cost is the
            // slowest single extension — the handshakes run
            // concurrently inside their supervision tasks, so a
            // broken package costs one boot, loudly, and never
            // delays a healthy sibling.
            let mounted = {
                let supervisor = runtime.block_on(async {
                    let supervisor = boot_extensions(launchable.found);
                    supervisor.await_resolved().await;
                    supervisor
                });
                // The conflict baseline is the parent core set —
                // exactly what a session would mount without
                // extensions.
                let core = core_sets(&args).map(|(_, parent)| parent)?;
                Arc::new(extensions::Mounted::mount(supervisor, &core))
            };
            // Assemble failures (session unreadable, model unbuildable)
            // reject the handshake with the plain reason — not the
            // config setup guide, which would be advice for a problem
            // the user does not have. A `--continue` that finds no
            // sessions is absorbed into a fresh start (the pinned
            // startup contract; the ack's `resumed: false` says so).
            let (session, startup_notes) = match assemble(
                &args,
                &registry,
                &SessionStore::project_default(),
                ContinueMiss::StartFresh,
                Some(mounted.clone()),
            ) {
                Ok(assembled) => assembled,
                Err(detail) => return json_startup_failure(&detail),
            };
            print_banner(&session);
            let wiring = host_wiring(&args, &registry, SessionStore::project_default(), &mounted);
            Ok(runtime.block_on(async {
                let handle = SessionHost::spawn(session, startup_notes, wiring);
                json::serve(
                    handle,
                    std::io::BufReader::new(std::io::stdin()),
                    std::io::stdout(),
                )
                .await
            }))
        }
        Mode::Print => {
            let config = Arc::new(config.map_err(|e| setup_guide(&e))?);
            let auth = Arc::new(auth.map_err(|e| e.to_string())?);
            print_mode(&args, &ModelRegistry::new(config, auth))
        }
    }
}

/// Print mode: assemble (rewinding first when asked), banner, one
/// message through the session actor, events printed as they arrive,
/// then the closing footer.
/// What one print-mode session left behind, for the footer and exit code.
struct PrintOutcome {
    failed: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    session_path: String,
    stats: Option<tabit_session::SessionStats>,
}

fn print_mode(args: &Args, registry: &ModelRegistry) -> Result<i32, String> {
    if args.rewind.is_some() && args.session.is_none() && !args.continue_newest {
        return Err(
            "--rewind rewinds a session: pass --continue or --session <path> (see --help)"
                .to_string(),
        );
    }
    // Print mode stays core-only: the extension host is backend
    // machinery the JSON-mode process owns (one host per backend);
    // a print-mode consumer is a later ruling with a real user.
    let (mut session, startup_notes) = assemble(
        args,
        registry,
        &SessionStore::project_default(),
        ContinueMiss::Fail,
        None,
    )?;
    if let Some(turns) = args.rewind {
        let rewind = session.rewind(turns).map_err(|e| e.to_string())?;
        println!(
            "[rewound: dropped {} user message(s) — the next prompt branches from before them]",
            rewind.dropped
        );
    }
    // A promptless rewind is complete: the marker alone carries it.
    let Some(prompt) = args.print_prompt.clone() else {
        return Ok(0);
    };

    print_banner(&session);

    // One stdin reader owns both duties (line-buffered stdin in print
    // mode: press Esc then Enter to abort; any other line answers the
    // open interaction card — its number for buttons, free text
    // otherwise). Real key handling arrives with the GUI.
    let armed: ArmedSlot = std::sync::Arc::default();

    // The message goes through the session host — the same path JSON
    // mode drives — and the stream is read to its end: the host returns
    // the session before closing, so closing stats cover this run.
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(async {
            let empty_mount = std::sync::Arc::new(extensions::Mounted::none());
            let wiring =
                host_wiring(args, registry, SessionStore::project_default(), &empty_mount);
            let mut handle = SessionHost::spawn(session, startup_notes, wiring);
            let boot = handle.info().session_id.clone();
            {
                let link = handle.command_link();
                let armed = armed.clone();
                let boot = boot.clone();
                std::thread::spawn(move || {
                    use std::io::BufRead as _;
                    for line in std::io::stdin().lock().lines().by_ref().flatten() {
                        if line.starts_with('\x1b') {
                            link.send(SessionCommand::Abort {
                                session: boot.clone(),
                            });
                            return;
                        }
                        // Answers apply to the oldest open card (FIFO —
                        // FRONTEND.md §8 allows several open at once, and
                        // concurrent permission gates make that ordinary).
                        let card = { lock_armed(&armed).pop_front() };
                        if let Some((id, options)) = card {
                            link.send(parse_answer(&boot, &id, &options, &line));
                            let waiting = lock_armed(&armed).len();
                            if waiting > 0 {
                                eprintln!("--- {waiting} more open question(s), keep answering");
                            }
                        }
                    }
                });
            }
            let mut outcome = PrintOutcome {
                failed: None,
                input_tokens: 0,
                output_tokens: 0,
                session_path: handle.info().session_path.clone(),
                stats: None,
            };
            handle.message(&boot, prompt);
            handle.close_commands();
            while let Some(frame) = handle.next_event().await {
                match &frame.event {
                    SessionEvent::CompletionCall { usage, .. } => {
                        outcome.input_tokens += usage.input_tokens;
                        outcome.output_tokens += usage.output_tokens;
                    }
                    SessionEvent::RunFailed { message, .. } => {
                        outcome.failed = Some(message.clone());
                        // A terminal closes every card (FRONTEND.md §8).
                        lock_armed(&armed).clear();
                    }
                    SessionEvent::InteractionRequest {
                        id,
                        ui_type,
                        payload,
                        ..
                    } => {
                        // A template consumer like any frontend: render
                        // the natives, report the rest (never answer a
                        // widget this surface cannot construct). Print
                        // mode's select_any rendering is single-select
                        // (number picks one); multi-select needs a
                        // compositing frontend.
                        use tabit_protocol::templates;
                        match ui_type.as_str() {
                            templates::ui::SELECT_ONE | templates::ui::SELECT_ANY => {
                                let (title, body, options) = if ui_type == templates::ui::SELECT_ONE
                                {
                                    let Ok(card) = serde_json::from_value::<
                                        templates::SelectOneCard,
                                    >(payload.clone()) else {
                                        eprintln!(
                                            "(a select card arrived in a shape this surface cannot read)"
                                        );
                                        continue;
                                    };
                                    (card.title, card.body, card.options)
                                } else {
                                    let Ok(card) = serde_json::from_value::<
                                        templates::SelectAnyCard,
                                    >(payload.clone()) else {
                                        eprintln!(
                                            "(a select card arrived in a shape this surface cannot read)"
                                        );
                                        continue;
                                    };
                                    (card.title, card.body, card.options)
                                };
                                eprintln!(
                                    "
--- {title}
{body}"
                                );
                                if options.is_empty() {
                                    eprintln!("(type your answer, then Enter)");
                                } else {
                                    let legend = options
                                        .iter()
                                        .enumerate()
                                        .map(|(n, o)| format!("{}) {}", n + 1, o.label))
                                        .collect::<Vec<_>>()
                                        .join("  ");
                                    eprintln!("{legend}   — number, then Enter");
                                }
                                let mut queue = lock_armed(&armed);
                                queue.push_back((
                                    id.clone(),
                                    options.into_iter().map(|o| o.label).collect(),
                                ));
                                if queue.len() > 1 {
                                    eprintln!(
                                        "({} open questions — answers apply in order)",
                                        queue.len()
                                    );
                                }
                            }
                            other => eprintln!(
                                "(unsupported interaction widget `{other}` — not answered)"
                            ),
                        }
                    }
                    SessionEvent::RunFinished { .. } | SessionEvent::RunAborted { .. } => {
                        // A terminal closes every card (FRONTEND.md §8).
                        lock_armed(&armed).clear();
                    }
                    _ => {}
                }
                print_event(&frame.event);
            }
            outcome.stats = handle.closing_stats();
            outcome
        });

    eprintln!(
        "--- session {} | tokens {} in / {} out{}",
        outcome.session_path,
        outcome.input_tokens,
        outcome.output_tokens,
        outcome
            .stats
            .map(|s| format!(" (session total {:.4} USD)", s.total_cost))
            .unwrap_or_default()
    );
    match outcome.failed {
        Some(message) => Err(format!("run failed: {message}")),
        None => Ok(0),
    }
}

/// What happens when `--continue` finds nothing to resume. Print mode
/// fails loudly (a terminal user asked explicitly); JSON mode starts
/// fresh — the pinned startup contract: the chat UI is unconditional,
/// and an empty store (a brand-new project) is not an error. The
/// handshake's `resumed: false` tells the frontend what happened.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ContinueMiss {
    Fail,
    StartFresh,
}

/// One open interaction card: its request id, widget type, and button
/// labels, waiting for one stdin line. Several may be open at once
/// (concurrent gates); answers apply FIFO.
/// One open card awaiting its answer: the request id and
/// the option labels (numbered answers resolve against them).
type ArmedCard = (String, Vec<String>);
type ArmedSlot = std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<ArmedCard>>>;

/// Lock the armed-card queue (poisoning recovers — the queue is only a
/// hint for the stdin reader).
fn lock_armed(
    armed: &ArmedSlot,
) -> std::sync::MutexGuard<'_, std::collections::VecDeque<ArmedCard>> {
    armed.lock().unwrap_or_else(|error| error.into_inner())
}

/// Turn one stdin line into the card's answer. Numbered buttons parse as
/// `2` or `2 reason text`; a free-text card takes the whole line. An
/// empty or unrecognizable line answers with nothing — the backend's
/// fail-closed default (deny / dismissed), so a card can never hang.
#[cfg(test)]
mod interaction_answer_tests {
    use super::*;

    fn options() -> Vec<String> {
        vec![
            "Allow".to_string(),
            "Always allow".to_string(),
            "Deny".to_string(),
        ]
    }

    fn answer(
        session: &str,
        id: &str,
        options: &[String],
        line: &str,
    ) -> (Vec<String>, Option<String>) {
        match parse_answer(session, id, options, line) {
            SessionCommand::InteractionResponse { payload, .. } => {
                let parsed: tabit_protocol::templates::SelectAnswer =
                    serde_json::from_value(payload).expect("the template payload parses");
                (parsed.selected, parsed.text)
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn numbered_buttons_select_by_index_with_optional_reason() {
        assert_eq!(
            answer("s1", "i1", &options(), "1"),
            (vec!["Allow".to_string()], None)
        );
        assert_eq!(
            answer("s1", "i2", &options(), "3 never delete build dirs"),
            (
                vec!["Deny".to_string()],
                Some("never delete build dirs".to_string())
            )
        );
    }

    #[test]
    fn free_text_cards_take_the_whole_line() {
        assert_eq!(
            answer("s1", "i3", &[], "use python"),
            (Vec::new(), Some("use python".to_string()))
        );
    }

    #[test]
    fn empty_or_unknown_answers_fail_closed_with_nothing() {
        assert_eq!(answer("s1", "i4", &options(), ""), (Vec::new(), None));
        assert_eq!(answer("s1", "i5", &options(), "   "), (Vec::new(), None));
        // Out-of-range numbers carry no option: the backend's default
        // (deny for permission) applies rather than a wrong button.
        assert_eq!(answer("s1", "i6", &options(), "9"), (Vec::new(), None));
    }
}

#[allow(clippy::expect_used)] // sanctioned crash: pure-data serialization (AGENTS.md doctrine)
fn parse_answer(session: &str, id: &str, options: &[String], line: &str) -> SessionCommand {
    use tabit_protocol::templates;
    let line = line.trim();
    // Numbered options parse as `2` or `2 reason`; a free-text card (no
    // options) takes the whole line; anything else answers with nothing
    // (the backend's fail-closed default, so a card can never hang).
    // Both select templates share the one SelectAnswer shape.
    let answer = if line.is_empty() || options.is_empty() {
        templates::SelectAnswer {
            selected: Vec::new(),
            text: (!line.is_empty() && options.is_empty()).then(|| line.to_string()),
        }
    } else {
        let (number, reason) = match line.split_once(char::is_whitespace) {
            Some((number, reason)) => (number, reason.trim()),
            None => (line, ""),
        };
        let option = number
            .parse::<usize>()
            .ok()
            .and_then(|n| options.get(n.checked_sub(1)?))
            .map(String::as_str);
        templates::SelectAnswer {
            selected: option.map(|o| vec![o.to_string()]).unwrap_or_default(),
            text: (!reason.is_empty()).then(|| reason.to_string()),
        }
    };
    let payload = serde_json::to_value(answer).expect("template payloads always serialize");
    SessionCommand::InteractionResponse {
        session: session.to_string(),
        id: id.to_string(),
        payload,
    }
}

/// The host's session wiring: how `new_session`/`open_session` build
/// sessions — the same assembly as the boot (config, tools, preamble),
/// behind closures so tabit-session stays free of front-facing wiring.
/// The process's `--model`/`--max-turns` apply to sessions created
/// later; `open_session` resolves by stored id and resumes that file.
/// One registry for the whole process (the ruling: providers are user
/// config, not per-session) — every session the host builds shares
/// the provider client caches.
fn host_wiring(
    args: &Args,
    registry: &ModelRegistry,
    store: SessionStore,
    extensions: &std::sync::Arc<extensions::Mounted>,
) -> SessionHostWiring {
    let fresh_args = Args {
        session: None,
        continue_newest: false,
        // A new session is a user session of this process: no parent
        // to announce, a file behind it.
        parent: None,
        ephemeral: false,
        ..args.clone()
    };
    let fresh_registry = registry.clone();
    let fresh_store = store.clone();
    let fresh_extensions = extensions.clone();
    let open_args = args.clone();
    let open_registry = registry.clone();
    let open_store = store.clone();
    let open_extensions = extensions.clone();
    SessionHostWiring {
        store,
        children: child_router(),
        boot_parent: args.parent.clone(),
        boot_parent_call: args.parent_call.clone(),
        skills: skills_catalog().available(),
        extensions: extensions.catalog.clone(),
        create: Arc::new(move || {
            assemble(
                &fresh_args,
                &fresh_registry,
                &fresh_store,
                ContinueMiss::StartFresh,
                Some(fresh_extensions.clone()),
            )
        }),
        open: Arc::new(move |session_id: &str| {
            let path = open_store
                .list()
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|summary| summary.id == session_id)
                .ok_or_else(|| format!("no stored session with id `{session_id}`"))?
                .path;
            let args = Args {
                session: Some(path),
                ..open_args.clone()
            };
            assemble(
                &args,
                &open_registry,
                &open_store,
                ContinueMiss::Fail,
                Some(open_extensions.clone()),
            )
        }),
    }
}

/// The extension host boot: launch the scanned packages, handshake
/// each, supervise for the backend's life — reports land on stderr
/// (stdout is protocol). Every tabit process boots its own extension
/// host — the frontend-attached backend AND every subagent child
/// (ruled 2026-09: children pick up extensions; the leaf law outlaws
/// loading into a parent's process, not a child hosting its own
/// set). Must run on the serving runtime (it spawns).
/// The install target: the default extensions root only (task 6's
/// ruling — `--extensions` is a backend test/dev override, not an
/// install destination).
fn install_root() -> Result<PathBuf, String> {
    tabit_config::home_dir()
        .map(|home| home.join(".tabit").join("extensions"))
        .ok_or_else(|| "cannot resolve the home directory for the extensions root".to_string())
}

fn extension_root(args: &Args) -> Option<PathBuf> {
    args.extensions
        .clone()
        .or_else(|| tabit_config::home_dir().map(|home| home.join(".tabit").join("extensions")))
}
fn boot_extensions(
    found: Vec<tabit_ext::manifest::Discovered>,
) -> std::sync::Arc<tabit_ext::supervisor::Supervisor> {
    // Reports land on stderr (stdout is protocol).
    let (supervisor, mut events) =
        tabit_ext::supervisor::launch(found, tabit_ext::supervisor::HANDSHAKE_TIMEOUT);
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match &event.status {
                tabit_ext::supervisor::Status::Alive => {
                    eprintln!("extension {}: loaded", event.name)
                }
                tabit_ext::supervisor::Status::Dead { reason } => {
                    eprintln!("extension {}: not running — {reason}", event.name)
                }
                tabit_ext::supervisor::Status::Starting => {}
            }
        }
    });
    std::sync::Arc::new(supervisor)
}

/// The scan, shaped for boot (item 9, tasks 4+6): what the host
/// LAUNCHES — every refusal plus the not-disabled PROCESS packages —
/// and the MOUNTED packages themselves (name + dir), the list the
/// providers fragment merge, the skills tables, and the requirement
/// check apply to. Packages mount by default (install was the
/// consent; disabling is the explicit act), and a disabled package
/// is absent everywhere by design — including as a requirement
/// (unmet). **Static packages** (no entry) mount but never launch
/// and never announce: their contributions are exactly the
/// scan-driven ones. Unmet `requires` become refusals (presence, not
/// liveness — the mounted set is the truth, standing never is).
struct Launchable {
    found: Vec<tabit_ext::manifest::Discovered>,
    packages: Vec<(String, PathBuf)>,
}

fn partition(
    found: Vec<tabit_ext::manifest::Discovered>,
    disabled: &std::collections::HashSet<String>,
) -> Launchable {
    let mut mounted = Vec::new();
    let mut refusals = Vec::new();
    for found in found {
        match found {
            tabit_ext::manifest::Discovered::Package { dir, manifest } => {
                if !disabled.contains(&manifest.name) {
                    mounted.push(tabit_ext::manifest::Discovered::Package { dir, manifest });
                }
                // A disabled package is absent everywhere — not a
                // refusal (the user's setting reports nowhere), just
                // gone.
            }
            // Refusals always launch (as dead reports): a broken
            // package is loud, whatever the settings say.
            refused => refusals.push(refused),
        }
    }
    // The requirement check runs over the mounted set: disabled or
    // missing requirements are unmet, dead ones are not.
    let mounted_names: std::collections::HashSet<String> = mounted
        .iter()
        .filter_map(|found| found.manifest().map(|m| m.name.clone()))
        .collect();
    let mounted = tabit_ext::manifest::enforce_requires(mounted, &mounted_names);
    // Split by process-ness: static packages contribute scan facts
    // only; process packages (and every refusal) reach the launch.
    let mut launchable = refusals;
    let mut packages = Vec::new();
    for found in mounted {
        match &found {
            tabit_ext::manifest::Discovered::Package { dir, manifest } => {
                packages.push((manifest.name.clone(), dir.clone()));
                if !manifest.is_static() {
                    launchable.push(found);
                }
            }
            tabit_ext::manifest::Discovered::Refused { .. } => {
                launchable.push(found);
            }
        }
    }
    launchable.sort_by(|a, b| a.dir().cmp(b.dir()));
    Launchable {
        found: launchable,
        packages,
    }
}

/// Merge one mounted package's `providers.toml` fragment into `config`
/// (EXTENSIONS.md: the user's own provider ids win silently; only a
/// fragment colliding with an earlier fragment warns). A broken
/// fragment refuses the *fragment* — warned, skipped — never the
/// package, whose tools and hooks are unaffected.
fn merge_fragment_into(
    config: &mut TabitConfig,
    name: &str,
    dir: &std::path::Path,
    user_ids: &std::collections::HashSet<String>,
    warnings: &mut Vec<String>,
) {
    let path = dir.join("providers.toml");
    if !path.is_file() {
        return;
    }
    match TabitConfig::load(&path) {
        Ok(fragment) => {
            config.merge_fragment(fragment, &format!("extension `{name}`"), user_ids, warnings);
        }
        Err(detail) => {
            warnings.push(format!(
                "extension `{name}`: providers fragment refused: {detail}"
            ));
        }
    }
}

/// The extension walker's skills contribution (item 9, task 4): every
/// mounted package's `skills/` tree, entries at their original paths,
/// first-package-wins on a name collision (scan order — the same
/// determinism law as tool registration). In-memory tables only; the
/// catalog, the `skill` tool, and the wire snapshot read them.
fn extension_skills_catalog(packages: &[(String, PathBuf)]) -> tabit_session::skills::Skills {
    let mut extension_skills = tabit_session::skills::Skills::default();
    for (_name, dir) in packages {
        for entry in tabit_session::skills::entries_in(&dir.join("skills")) {
            extension_skills.register(entry);
        }
    }
    extension_skills
}

/// Resolve config/auth into a session per the args (model selection,
/// resume target, tools, preamble). `store` is injected so tests drive
/// a temp store instead of the repo's. The registry is the caller's
/// process-shared one (owner ruling: providers are user config, not
/// per-session — one client cache per provider per process).
fn assemble(
    args: &Args,
    registry: &ModelRegistry,
    store: &SessionStore,
    miss: ContinueMiss,
    extensions: Option<std::sync::Arc<extensions::Mounted>>,
) -> Result<(Session, Vec<String>), String> {
    // Default-model resolution (registry): an explicit --model wins,
    // then the resumed session's last model, then default_model in
    // providers.toml, then the first configured model.
    let resume_target = match (&args.session, args.continue_newest) {
        (Some(path), _) => Some(path.clone()),
        (None, true) => {
            let newest = store.list().map_err(|e| e.to_string())?.into_iter().next();
            match (newest, miss) {
                (Some(newest), _) => Some(newest.path),
                (None, ContinueMiss::Fail) => {
                    return Err(format!("no sessions yet in {}", store.dir().display()));
                }
                (None, ContinueMiss::StartFresh) => None,
            }
        }
        (None, false) => None,
    };
    let resumed = match &resume_target {
        Some(path) => store.open_path(path).map_err(|e| e.to_string())?.register,
        None => None,
    };
    let explicit = args
        .model
        .as_deref()
        .map(|raw| parse_model(raw, registry.config()))
        .transpose()?;
    let (selection, startup_notes) = registry
        .default_selection(explicit, resumed)
        .map_err(|e| e.to_string())?;
    // Startup degradations are data (ruled: external errors ride the
    // channel): the worker emits them as `error { kind: model }` frames —
    // the first frames after the handshake ack.
    let session = assemble_session(
        args,
        registry.clone(),
        selection,
        resume_target,
        store.clone(),
        extensions.as_ref(),
    )?;
    Ok((session, startup_notes))
}

/// Internal errors crash the process (owner ruling): a panic anywhere —
/// the session actor's tokio task, a transport thread, anywhere — must
/// end the process, never linger as a zombie holding a live stdin. The
/// hook chains the default report (message, location, backtrace per
/// RUST_BACKTRACE) and exits 101: nonzero so the frontend's crash path
/// fires, and distinct from 1 (handshake rejection) so the two are
/// never confused. The stderr report is what the user sends back.
fn install_crash_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        eprintln!(
            "tabit: internal error — exiting with code 101; \
             please report this together with the output above"
        );
        std::process::exit(101);
    }));
}

/// Test-only crash injection (tests/crash.rs): exercises the hook
/// end-to-end through the real binary. Sanctioned crash — that is the
/// branch's whole point.
#[allow(clippy::panic)]
fn crash_injection() {
    panic!("injected internal error");
}

fn main() {
    install_crash_hook();
    if std::env::var_os("TABIT_CRASH_TEST").is_some() {
        crash_injection();
    }
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("tabit: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse_args_from(list.iter().map(|s| s.to_string()))
    }

    fn bare_args() -> Args {
        Args {
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
        }
    }

    fn named_tool(name: &'static str) -> rig_agent::tool::DynamicTool {
        rig_agent::tool::DynamicTool::new(
            name,
            "a test tool",
            serde_json::json!({"type": "object"}),
            move |_ctx, _args| {
                let output = name;
                Box::pin(async move { Ok(rig_agent::tool::ToolOutput::text(output)) })
            },
        )
    }

    #[test]
    fn the_tool_filter_admits_allowed_and_not_denied() {
        let candidate = vec![named_tool("read"), named_tool("bash"), named_tool("echo")];
        let args = Args {
            tools: Some("read,echo".to_string()),
            without: Some("echo".to_string()),
            ..bare_args()
        };
        let (allow, deny) = tool_filter(&args, &candidate).expect("valid filter");
        let kept = retain_filtered(candidate, &allow, &deny);
        assert_eq!(
            kept.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            vec!["read"],
            "allow first, then deny: the intersection survives"
        );

        // A subset of the candidate (the child core set behind
        // SubagentParts::tools) filters without re-validation; a
        // proxy-only allow name matches nothing there.
        let core_subset = vec![named_tool("read"), named_tool("bash")];
        let kept = retain_filtered(core_subset, &allow, &deny);
        assert_eq!(
            kept.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            vec!["read"]
        );
    }

    #[test]
    fn the_tool_filter_rejects_unknown_names_loudly() {
        let candidate = vec![named_tool("read")];
        let mut args = Args {
            tools: Some("bogus".to_string()),
            ..bare_args()
        };
        let error = tool_filter(&args, &candidate).expect_err("unknown allow name");
        assert!(error.contains("bogus") && error.contains("read"), "{error}");

        args.tools = None;
        args.without = Some("bogus".to_string());
        let error = tool_filter(&args, &candidate).expect_err("unknown deny name");
        assert!(
            error.contains("--without") && error.contains("read"),
            "{error}"
        );
    }

    #[test]
    fn a_set_tabit_core_bin_never_overrides_self_reference() {
        // Pins the ruling: backend self-reference is current_exe, no
        // exceptions — a frontend's TABIT_CORE_BIN (stale or not) must
        // not diverge subagent children from the running image.
        // SAFETY: process-global state; no other test in this binary
        // reads the variable, and it is removed before the assertion.
        unsafe {
            std::env::set_var("TABIT_CORE_BIN", "a-stale-override");
        }
        let resolved = tabit_exe().expect("current_exe resolves in a test binary");
        unsafe {
            std::env::remove_var("TABIT_CORE_BIN");
        }
        assert_eq!(
            resolved,
            std::env::current_exe().expect("current_exe resolves in a test binary")
        );
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
    fn the_extensions_flag_is_json_mode_only() {
        let parsed = args(&["--json", "--extensions", "C:/tmp/ext"]).expect("valid");
        assert_eq!(parsed.extensions, Some(PathBuf::from("C:/tmp/ext")));

        let conflict =
            args(&["-p", "hi", "--extensions", "C:/tmp/ext"]).expect_err("print mode rejects it");
        assert!(conflict.contains("do not combine"), "{conflict}");
    }

    #[test]
    fn child_role_flags_parse_only_in_json_mode() {
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

        // Child flags outside JSON mode are foreign flags.
        let foreign = args(&["--parent", "p1", "-p", "hi"]).expect_err("parent × print");
        assert!(foreign.contains("do not combine"), "{foreign}");
    }

    #[test]
    fn enablement_partitions_the_scan_and_refusals_always_launch() {
        use std::collections::HashSet;
        use tabit_ext::manifest::{Discovered, Manifest};
        fn package(name: &str) -> Discovered {
            Discovered::Package {
                dir: PathBuf::from(format!("C:/ext/{name}")),
                manifest: Manifest {
                    name: name.to_string(),
                    version: "0.1.0".to_string(),
                    entry: Some(vec!["bin".to_string()]),
                    description: None,
                    requires: Vec::new(),
                },
            }
        }
        fn static_package(name: &str, requires: &[&str]) -> Discovered {
            Discovered::Package {
                dir: PathBuf::from(format!("C:/ext/{name}")),
                manifest: Manifest {
                    name: name.to_string(),
                    version: "0.1.0".to_string(),
                    entry: None,
                    description: None,
                    requires: requires.iter().map(|r| r.to_string()).collect(),
                },
            }
        }
        fn refused(dir: &str) -> Discovered {
            Discovered::Refused {
                dir: PathBuf::from(format!("C:/ext/{dir}")),
                reason: "invalid manifest".to_string(),
            }
        }
        let found = vec![
            package("alpha"),
            package("beta"),
            Discovered::Refused {
                dir: PathBuf::from("C:/ext/broken"),
                reason: "invalid manifest".to_string(),
            },
        ];
        let mut disabled = HashSet::new();
        disabled.insert("alpha".to_string());
        let launchable = partition(found, &disabled);
        // The disabled alpha is absent everywhere; beta mounts by
        // default (install was the consent) and reaches the
        // fragment/skills consumers; the refusal still reports (a
        // broken package is loud whatever the settings say).
        assert_eq!(launchable.packages.len(), 1);
        assert_eq!(launchable.packages[0].0, "beta");
        assert_eq!(launchable.found.len(), 2);
        assert!(launchable.found.iter().any(
            |found| matches!(found, Discovered::Package { manifest, .. } if manifest.name == "beta")
        ));
        assert!(
            launchable
                .found
                .iter()
                .any(|found| matches!(found, Discovered::Refused { .. }))
        );
        // Nothing disabled: everything launches (install was the
        // consent); the refusal still reports.
        let none = partition(
            vec![
                package("alpha"),
                Discovered::Refused {
                    dir: PathBuf::from("C:/ext/broken"),
                    reason: "invalid manifest".to_string(),
                },
            ],
            &HashSet::new(),
        );
        assert_eq!(none.packages.len(), 1, "mounted by default");
        assert_eq!(none.found.len(), 2);

        // Everything disabled: nothing launches but the refusal.
        let mut all = HashSet::new();
        all.insert("alpha".to_string());
        let none = partition(vec![package("alpha"), refused("broken")], &all);
        assert!(none.packages.is_empty());
        assert_eq!(none.found.len(), 1);

        // Static packages (task 6): mounted for scan facts and
        // requirement presence, never launched, never announced.
        let launchable = partition(
            vec![package("alpha"), static_package("bundle", &["alpha"])],
            &HashSet::new(),
        );
        assert_eq!(launchable.packages.len(), 2, "the static package mounts");
        assert_eq!(
            launchable.found.len(),
            1,
            "only the process package launches"
        );
        assert_eq!(
            launchable.found[0].manifest().map(|m| m.name.clone()),
            Some("alpha".to_string())
        );

        // An unmet requirement refuses — and the refusal announces
        // (a static dependent with an unmet requirement reports dead).
        let launchable = partition(vec![static_package("needy", &["ghost"])], &HashSet::new());
        assert!(
            launchable.packages.is_empty(),
            "the refused package does not mount"
        );
        assert_eq!(
            launchable.found.len(),
            1,
            "the refusal launches as a report"
        );
        match &launchable.found[0] {
            Discovered::Refused { reason, .. } => {
                assert!(reason.contains("requires extension `ghost`"), "{reason}");
            }
            _ => panic!("unmet requirements refuse"),
        }

        // A disabled requirement is unmet ("disabled is absent
        // everywhere").
        let mut disabled = HashSet::new();
        disabled.insert("there".to_string());
        let launchable = partition(
            vec![package("there"), static_package("needy", &["there"])],
            &disabled,
        );
        assert!(launchable.packages.is_empty());
    }

    #[test]
    fn a_broken_fragment_refuses_the_fragment_not_the_package() {
        let dir = std::env::temp_dir().join(format!("tabit-frag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("package dir");
        std::fs::write(dir.join("providers.toml"), "not toml").expect("broken fragment");
        let mut config = TabitConfig::default();
        let user_ids = std::collections::HashSet::new();
        let mut warnings = Vec::new();
        merge_fragment_into(&mut config, "broken", &dir, &user_ids, &mut warnings);
        assert!(config.providers.is_empty());
        assert_eq!(warnings.len(), 1);
        let warning = &warnings[0];
        assert!(warning.contains("providers fragment refused"), "{warning}");

        // A healthy fragment lands under the same call.
        std::fs::write(
            dir.join("providers.toml"),
            "[providers.relay]\nbase_url = \"http://127.0.0.1:8391/v1\"\napi = \"openai-completions\"\n",
        )
        .expect("fragment");
        let mut warnings = Vec::new();
        merge_fragment_into(&mut config, "broken", &dir, &user_ids, &mut warnings);
        assert!(config.provider("relay").is_some(), "the fragment landed");
        assert!(warnings.is_empty(), "{warnings:?}");
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn selection_defaults_follow_the_registry_chain() {
        let registry = ModelRegistry::new(
            std::sync::Arc::new(test_config()),
            std::sync::Arc::new(AuthConfig::default()),
        );
        assert_eq!(
            registry
                .default_selection(None, None)
                .expect("preference from default_model")
                .0
                .provider,
            "lmstudio"
        );

        // No preference: the first configured model is the default.
        let bare = TabitConfig::from_toml_str(
            r#"
[providers.lmstudio]
base_url = "http://127.0.0.1:1234/v1"
api = "openai-completions"
keyless = true

[[providers.lmstudio.models]]
id = "m"
"#,
            std::path::Path::new("providers.toml"),
        )
        .expect("bare config");
        let registry = ModelRegistry::new(
            std::sync::Arc::new(bare),
            std::sync::Arc::new(AuthConfig::default()),
        );
        assert_eq!(
            registry
                .default_selection(None, None)
                .expect("first-seen")
                .0
                .model,
            "m"
        );

        let empty = ModelRegistry::new(
            std::sync::Arc::new(TabitConfig::default()),
            std::sync::Arc::new(AuthConfig::default()),
        );
        let error = empty
            .default_selection(None, None)
            .expect_err("nothing configured");
        assert!(
            error.to_string().contains("usable model provider"),
            "{error}"
        );
    }

    #[test]
    fn continue_miss_is_loud_in_print_and_absorbed_in_json() {
        let dir = std::env::temp_dir().join(format!("tabit-assemble-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::new(&dir);
        let config = Arc::new(test_config());
        let auth = Arc::new(AuthConfig::default());
        let registry = ModelRegistry::new(config.clone(), auth.clone());
        let cont_print = args(&["--continue", "-p", "hi"]).expect("valid print combo");

        let error = match assemble(&cont_print, &registry, &store, ContinueMiss::Fail, None) {
            Err(error) => error,
            Ok(_) => panic!("print mode fails loudly on an empty store"),
        };
        assert!(error.contains("no sessions yet"), "{error}");

        let cont_json = args(&["--continue", "--json"]).expect("valid json combo");
        let (session, notes) = assemble(
            &cont_json,
            &registry,
            &store,
            ContinueMiss::StartFresh,
            None,
        )
        .expect("json mode starts fresh");
        assert!(!session.resumed(), "the fresh start is reported");
        assert!(notes.is_empty(), "a clean config degrades nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_config() -> TabitConfig {
        TabitConfig::from_toml_str(
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
}
