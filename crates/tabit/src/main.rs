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
//! `tabit` — a minimal coding agent.
//!
//! Two modes today: print mode (`-p`) — one prompt in, one outer loop
//! out, events print as they happen — and JSON mode (`--json`) — the
//! session protocol as LF-JSONL over stdio, for scripts and future
//! frontends. The session persists project-locally, and the printed
//! session path resumes the conversation later. Interactive TUI mode —
//! the eventual default, like every other agent — is not implemented
//! yet.
//!
//! ```text
//! tabit -p "list the rust files in this project"     # new session
//! tabit --continue -p "now count lines in each"      # resume the newest
//! tabit --session <path> -p "what did we conclude?"  # resume a specific one
//! tabit --continue --rewind 1                        # rewind, then exit
//! tabit --json                                       # protocol on stdio
//! tabit --list                                       # show this project's sessions
//! ```

mod extensions;
mod json;

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use tabit_config::{AuthConfig, TabitConfig};
use tabit_protocol::SessionCommand;
use tabit_session::SessionEvent;
use tabit_session::{
    ModelRegistry, ModelSelection, Session, SessionBuilder, SessionHost, SessionHostWiring,
    SessionStore, build_system_prompt,
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
    ephemeral: bool,
    /// The installed-extension root (JSON mode; default
    /// `~/.tabit/extensions`).
    extensions: Option<PathBuf>,
    /// Positional project path — selects GUI mode (`tabit <path>`).
    path: Option<PathBuf>,
}

const USAGE: &str = "\
usage: tabit -p <PROMPT>                  print mode: one prompt, one run
       tabit --continue -p <PROMPT>       resume this project's newest session
       tabit --session <path> -p <PROMPT> resume a specific session file
       tabit --continue --rewind <n>      rewind n user messages, then exit;
                                         add -p <PROMPT> to branch with it
       tabit --json [session flags]       JSON protocol on stdio (scriptable)
                                         child role adds: --parent <id> (the
                                         spawning session), --parent-call <id>
                                         (its tool call), --tools <a,b,..>
                                         (an allow-list), --ephemeral (no
                                         file) — the subagent bridge's flags;
                                         --extensions <dir> selects the
                                         extension root (default
                                         ~/.tabit/extensions)
       tabit --list                      list this project's sessions

bare `tabit` or `tabit <path>` launches the GUI detached (vscode-style:
the terminal is free immediately and the GUI survives its close). The
GUI spawns one `tabit --json` backend and drives it over the protocol
(many sessions live in one backend).

print mode: Esc aborts the running turn (line-buffered stdin: Esc then
Enter). JSON mode: LF-JSONL frames — initialize, then message/abort
commands in; stamped events out (see the tabit-session protocol module).

       tabit --model <model-id|provider/model> select the model for this run
                                       (default: the resumed session's model,
                                       then default_model in providers.toml,
                                       then the first configured model)

config: providers.toml / auth.toml under ~/.tabit (override with
        TABIT_CONFIG / TABIT_AUTH); sessions live in <project>/.tabit/sessions";

/// What a parsed command line asks for. `-p` and `--rewind` both select
/// print mode, `--json` selects JSON mode; interactive mode is the
/// default once the TUI exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Print,
    Json,
    Gui,
}

fn mode_of(args: &Args) -> Mode {
    if args.list {
        Mode::List
    } else if args.json {
        Mode::Json
    } else if args.print_prompt.is_some() || args.rewind.is_some() {
        Mode::Print
    } else {
        Mode::Gui
    }
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::List => "list",
            Mode::Print => "print",
            Mode::Json => "JSON",
            Mode::Gui => "GUI",
        }
    }
}

/// The flags each mode accepts. A flag that cannot act in the selected
/// mode is a user mistake, rejected loudly at parse time — never a
/// silent no-op. One allow-list per mode replaces the per-pair conflict
/// checks (which kept missing combinations: `--model` with a path,
/// `--session` alone, `--list --continue`, …).
fn validate_mode(args: &Args) -> Result<Mode, String> {
    let mode = mode_of(args);
    let present = [
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
        args.ephemeral.then_some("--ephemeral"),
        args.extensions.is_some().then_some("--extensions"),
        args.path.is_some().then_some("<path>"),
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
            "--ephemeral",
            "--extensions",
        ],
        Mode::Print => &[
            "-p/--print",
            "--rewind",
            "--session",
            "--continue",
            "--model",
            "--max-turns",
        ],
        Mode::Gui => &["<path>"],
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
        ephemeral: false,
        extensions: None,
        path: None,
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
            "--ephemeral" => parsed.ephemeral = true,
            "--extensions" => {
                let value = it
                    .next()
                    .ok_or("--extensions needs a directory (see --help)")?;
                parsed.extensions = Some(PathBuf::from(value));
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}`\n{USAGE}"));
            }
            positional => {
                if parsed.path.is_some() {
                    return Err(format!(
                        "unexpected second argument `{positional}` — GUI mode takes one path\n{USAGE}"
                    ));
                }
                parsed.path = Some(PathBuf::from(positional));
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
        SessionEvent::CompactionStarted { .. } => {
            let _ = writeln!(std::io::stderr(), "[compacting the conversation…]");
        }
        SessionEvent::CompactionDelta { .. } | SessionEvent::CompactionFinished { .. } => {}
        SessionEvent::CompactionFailed { message, .. } => {
            let _ = writeln!(std::io::stderr(), "warning: compaction failed: {message}");
        }
        // The host's session catalog and creations are frontend
        // concerns; print mode is a single-session consumer.
        SessionEvent::SessionsAvailable { .. }
        | SessionEvent::SessionCreated { .. }
        | SessionEvent::SessionOpened { .. } => {}
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

/// The process-wide skills catalog — one discovery per process, the
/// consistency guarantee between the prompt's listing and the `skill`
/// tool's lookup (two discoveries could race a directory edit and
/// disagree; one cannot). Built against the process cwd (the backend
/// never chdirs — the same fact the session store roots at).
fn skills_catalog() -> std::sync::Arc<tabit_session::skills::Skills> {
    static SKILLS: std::sync::OnceLock<std::sync::Arc<tabit_session::skills::Skills>> =
        std::sync::OnceLock::new();
    SKILLS
        .get_or_init(|| {
            // A failed `current_dir` assembles nothing anyway (the loud
            // gate lives in `assemble_session`); here it degrades to a
            // discovery that finds nothing.
            let cwd = std::env::current_dir().unwrap_or_default();
            std::sync::Arc::new(tabit_session::skills::discover(&cwd))
        })
        .clone()
}

/// The tabit executable subprocess children spawn: this very binary
/// (the pi self-spawn pattern). `current_exe`, no exceptions — an
/// inherited `TABIT_BIN` (the frontend's dev override for finding the
/// backend) must not diverge children from the running image.
fn tabit_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot resolve the tabit executable: {e}"))
}

/// Restrict the child toolset to an allow-list (the `--tools` flag). A
/// typo'd name is a loud startup error listing what exists — a
/// silently-emptied toolset would look like a broken child.
fn filter_child_tools(
    tools: Vec<rig_agent::tool::DynamicTool>,
    spec: &str,
) -> Result<Vec<rig_agent::tool::DynamicTool>, String> {
    // Spec splitting is the CLI's; the filter itself is the framework
    // home's one implementation (subagent::filter_tools — the same
    // filter the `subagent` tool's allow-list arg rides).
    let wanted: Vec<String> = spec
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    tabit_session::subagent::filter_tools(&tools, &wanted)
        .map_err(|error| format!("--tools: {error}"))
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
    let preamble = build_system_prompt(&cwd, &skills).map_err(|e| e.to_string())?;

    // Subagent support (ROADMAP item 5): the process-wide parts, whose
    // toolset is the child toolset — the parent's minus the subagent
    // tool itself, so children cannot spawn children (recursion depth
    // is enforced by omission). A child-role process (`--parent`)
    // mounts that toolset only: it does not spawn.
    let (children, parent_core) = core_sets(args)?;
    let subagents = std::sync::Arc::new(tabit_session::subagent::SubagentParts {
        tools: children,
        max_turns: args.max_turns.unwrap_or(tabit_session::DEFAULT_MAX_TURNS),
        router: child_router(),
        exe: tabit_exe()?,
        // Children boot their own hosts against the parent's root
        // (the same packages, the same rules).
        extensions: extension_root(args).unwrap_or_default(),
    });

    let mut builder = SessionBuilder::new(
        store,
        registry.config().clone(),
        registry.auth().clone(),
        selection,
    )
    .map_err(|e| e.to_string())?
    .preamble(preamble)
    .model_factory(registry.factory())
    // The hook surface: whatever the extension mount carries — the
    // forwarded policy hooks of installed packages (the permission
    // gate is the `gate` extension now; EXTENSIONS.md) plus any core
    // stack, composed through the builder's one seam — children mount
    // their own (they boot their own hosts, the 2026-09 ruling).
    .hooks(match extensions {
        Some(mounted) => mounted.hooks(),
        None => rig_agent::agent::HookStack::new(),
    })
    .subagents(subagents)
    .skills(skills);
    // The process's toolset: its core set, plus the extension mount —
    // replaced core tools unmount, the proxies join (one name, one
    // tool, resolved at this assembly). Children resolve against
    // their own core set (the child set): they boot their own hosts.
    let mounted: Vec<_> = match extensions {
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
        dynamic_contextual(tabit_tools::AskUser),
        tabit_session::skills::skill_tool(),
    ]
}

/// The two core toolsets every assembly derives from: the child set
/// (every coding tool, the `--tools` allow-list applied) and the
/// parent set (the child set plus the subagent tool). The extension
/// mount's conflict baseline is the parent set — exactly what the
/// session would mount without extensions.
fn core_sets(
    args: &Args,
) -> Result<
    (
        Vec<rig_agent::tool::DynamicTool>,
        Vec<rig_agent::tool::DynamicTool>,
    ),
    String,
> {
    let mut children = child_tools();
    if let Some(spec) = &args.tools {
        children = filter_child_tools(children, spec)?;
    }
    let mut parent = children.clone();
    if args.parent.is_none() {
        parent.push(tabit_session::subagent::subagent_tool());
    }
    Ok((children, parent))
}

/// The `tabit-gui` executable: explicit override, else the sibling of
/// this binary (cargo installs workspace binaries side by side).
fn gui_bin() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("TABIT_GUI_BIN") {
        return Some(PathBuf::from(path));
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.parent()
                .map(|dir| dir.join(format!("tabit-gui{}", std::env::consts::EXE_SUFFIX)))
        })
        .filter(|path| path.is_file())
}

/// Launch the GUI detached (vscode-style: it survives this terminal)
/// and return immediately. Stderr goes to `<project>/.tabit/gui.log`
/// so GUI crashes are diagnosable after the fact.
fn launch_gui(path: Option<&std::path::Path>) -> Result<i32, String> {
    use std::process::{Command, Stdio};
    let bin = gui_bin().ok_or_else(|| {
        "the tabit-gui executable was not found next to tabit; set TABIT_GUI_BIN or install both binaries together"
            .to_string()
    })?;
    let cwd = match path {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    let log_dir = cwd.join(".tabit");
    std::fs::create_dir_all(&log_dir).map_err(|e| e.to_string())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("gui.log"))
        .map_err(|e| e.to_string())?;

    // The GUI never guesses where the backend is: hand it the exact
    // executable that launched it ("can't find tabit" is not a valid
    // failure mode in the supported flow).
    let tabit_exe =
        std::env::current_exe().map_err(|e| format!("cannot resolve the tabit executable: {e}"))?;
    let mut command = Command::new(bin);
    command
        .arg("--tabit")
        .arg(&tabit_exe)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    detach(&mut command);
    command
        .spawn()
        .map_err(|e| format!("could not start the GUI: {e}"))?;
    println!("opening tabit in {} …", cwd.display());
    Ok(0)
}

/// Put the child in its own process group so closing this terminal
/// (SIGHUP to the foreground group on Unix, the console job on
/// Windows) cannot reach it — the survive-the-terminal trick.
#[cfg(windows)]
fn detach(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

#[cfg(unix)]
fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // 0 = a fresh process group.
    command.process_group(0);
}

/// The first-run setup guide: a fresh install has no config, which is
/// normal — the failure message must teach, not scare.
fn setup_guide(detail: &str) -> String {
    let example = r#"create ~/.tabit/providers.toml (or point $TABIT_CONFIG at a file):

    default_model = "lmstudio/your-model-id"   # optional; the first model is the fallback

    [providers.lmstudio]
    base_url = "http://127.0.0.1:1234/v1"
    api = "openai-completions"

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
    if mode_of(&args) == Mode::Gui {
        // The GUI loads its own config in its own process; the
        // launcher needs nothing but the binary.
        return launch_gui(args.path.as_deref());
    }
    let config = TabitConfig::load_default().map_err(|e| e.to_string());
    let auth = AuthConfig::load_default().map_err(|e| e.to_string());

    match mode_of(&args) {
        // Unreachable: GUI returns before the config load above. Loud
        // rather than silent.
        #[allow(clippy::unreachable)]
        Mode::Gui => unreachable!("GUI mode handled before config load"),
        Mode::List => {
            let store = SessionStore::project_default();
            list_sessions(&store)?;
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
            let registry = ModelRegistry::new(config, auth);
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
                    let supervisor = boot_extensions(&args);
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
                    SessionEvent::CompletionCall {
                        input_tokens,
                        output_tokens,
                        ..
                    } => {
                        outcome.input_tokens += input_tokens;
                        outcome.output_tokens += output_tokens;
                    }
                    SessionEvent::RunFailed { message } => {
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

/// The extension host boot (ROADMAP item 9, task 1): launch every
/// installed extension, handshake it, supervise for the backend's
/// life — reports land on stderr (stdout is protocol). Children
/// Every tabit process boots its own extension host — the
/// frontend-attached backend AND every subagent child (ruled
/// 2026-09: children pick up extensions; the leaf law outlaws
/// loading into a parent's process, not a child hosting its own
/// set). Must run on the serving runtime (it spawns).
fn extension_root(args: &Args) -> Option<PathBuf> {
    args.extensions
        .clone()
        .or_else(|| tabit_config::home_dir().map(|home| home.join(".tabit").join("extensions")))
}
fn boot_extensions(args: &Args) -> std::sync::Arc<tabit_ext::supervisor::Supervisor> {
    // An extension-less boot (no root) holds the empty supervisor so
    // assembly has a single shape. Reports land on stderr (stdout is
    // protocol).
    let Some(root) = extension_root(args) else {
        return std::sync::Arc::new(tabit_ext::supervisor::Supervisor::empty());
    };
    let (supervisor, mut events) =
        tabit_ext::supervisor::launch(&root, tabit_ext::supervisor::HANDSHAKE_TIMEOUT);
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

    #[test]
    fn a_set_tabit_bin_never_overrides_self_reference() {
        // Pins the ruling: backend self-reference is current_exe, no
        // exceptions — a frontend's TABIT_BIN (stale or not) must not
        // diverge subagent children from the running image.
        // SAFETY: process-global state; no other test in this binary
        // reads the variable, and it is removed before the assertion.
        unsafe {
            std::env::set_var("TABIT_BIN", "a-stale-override");
        }
        let resolved = tabit_exe().expect("current_exe resolves in a test binary");
        unsafe {
            std::env::remove_var("TABIT_BIN");
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
        let gui = args(&["hello"]).expect("a positional path parses");
        assert_eq!(gui.path.as_deref(), Some(std::path::Path::new("hello")));
        let second = args(&["hello", "world"]).expect_err("two positionals");
        assert!(second.contains("second argument"), "{second}");
    }

    #[test]
    fn print_mode_is_selected_by_prompt_or_rewind() {
        assert_eq!(mode_of(&args(&[]).expect("bare")), Mode::Gui);
        assert_eq!(
            mode_of(&args(&["."]).expect("path")),
            Mode::Gui,
            "a positional path selects GUI mode"
        );
        assert!(args(&["a", "b"]).is_err(), "two paths are rejected");
        assert_eq!(mode_of(&args(&["-p", "hi"]).expect("print")), Mode::Print);
        assert_eq!(
            mode_of(&args(&["--rewind", "1"]).expect("rewind")),
            Mode::Print
        );
        assert_eq!(mode_of(&args(&["--json"]).expect("json")), Mode::Json);
        assert_eq!(mode_of(&args(&["--list"]).expect("list")), Mode::List);
    }

    #[test]
    fn flags_outside_the_selected_mode_are_loud_parse_errors() {
        // One allow-list per mode, so every combination class is covered,
        // including ones the old per-pair checks missed.
        let cases: &[&[&str]] = &[
            &[".", "-p", "hi"],       // path × print
            &[".", "--json"],         // path × json
            &[".", "--list"],         // path × list
            &[".", "--model", "p/m"], // path × model (missed before)
            &[".", "--continue"],     // path × continue (missed before)
            &[".", "--session", "s"], // path × session (missed before)
            &["--json", "-p", "hi"],  // json × print
            &["--json", "--rewind", "1"],
            &["--list", "-p", "hi"], // list is exclusive (was a silent win)
            &["--list", "--continue"],
            &["--session", "s"], // session alone is modeless
        ];
        for case in cases {
            let error = args(case).expect_err("foreign flags must not parse");
            assert!(error.contains("do not combine"), "case {case:?}: {error}");
        }

        // The shared flags still combine within print and json modes.
        args(&["--continue", "--session", "s", "--model", "p/m", "-p", "hi"])
            .expect("print accepts the shared flags");
        args(&["--continue", "--json", "--max-turns", "5"]).expect("json accepts the shared flags");
        // GUI mode takes only the optional path.
        args(&[]).expect("bare tabit is GUI mode");
        args(&["."]).expect("a path alone is GUI mode");
    }

    #[test]
    fn json_mode_parses_and_print_conflicts_at_parse_time() {
        let parsed = args(&["--continue", "--json"]).expect("valid");
        assert!(parsed.json && parsed.continue_newest);
        assert_eq!(mode_of(&parsed), Mode::Json);

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
        assert!(error.to_string().contains("no models"), "{error}");
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
