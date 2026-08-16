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

mod json;

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use tabit_config::{AuthConfig, TabitConfig};
use tabit_session::SessionEvent;
use tabit_session::{
    ModelRegistry, ModelSelection, Session, SessionBuilder, SessionHandle, SessionStore,
    build_system_prompt,
};
use tabit_tools::{dynamic, dynamic_contextual};

#[derive(Debug)]
struct Args {
    print_prompt: Option<String>,
    session: Option<PathBuf>,
    continue_newest: bool,
    list: bool,
    model: Option<String>,
    max_turns: Option<usize>,
    rewind: Option<usize>,
    json: bool,
}

const USAGE: &str = "\
usage: tabit -p <PROMPT>                  print mode: one prompt, one run
       tabit --continue -p <PROMPT>       resume this project's newest session
       tabit --session <path> -p <PROMPT> resume a specific session file
       tabit --continue --rewind <n>      rewind n user messages, then exit;
                                         add -p <PROMPT> to branch with it
       tabit --json [session flags]       JSON protocol on stdio (scriptable)
       tabit --list                      list this project's sessions

bare `tabit` starts interactive mode — not implemented yet; pass
-p <PROMPT> for print mode or --json for the stdio protocol until the
TUI lands.

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
    Interactive,
}

fn mode_of(args: &Args) -> Mode {
    if args.list {
        Mode::List
    } else if args.json {
        Mode::Json
    } else if args.print_prompt.is_some() || args.rewind.is_some() {
        Mode::Print
    } else {
        Mode::Interactive
    }
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
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}`\n{USAGE}"));
            }
            positional => {
                return Err(format!(
                    "unexpected argument `{positional}` — pass the prompt with -p\n{USAGE}"
                ));
            }
        }
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
        SessionEvent::RunAborted { .. } => {
            let _ = writeln!(
                out,
                "
[aborted]"
            );
        }
        SessionEvent::TextDelta { text } => {
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
        SessionEvent::TurnRetried { turn } => {
            let _ = writeln!(out, "[turn {turn} rejected by a hook; retrying]");
        }
        SessionEvent::CompletionCall { .. } => {}
        SessionEvent::RunFinished { .. } => {
            let _ = writeln!(out);
        }
        // Not a printable stream event: run() turns it into the process
        // error (stderr, exit 1) once the stream has ended.
        SessionEvent::RunFailed { .. } => {}
        SessionEvent::NativeItem { .. } => {}
    }
}

/// The human startup banner (stderr — stdout is the answer channel in
/// print mode and the protocol channel in JSON mode).
fn print_banner(session: &Session) {
    let stats = session.stats().ok();
    if stats
        .as_ref()
        .is_some_and(|s| s.total_usage.total_tokens > 0)
    {
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
        eprintln!("session {} started", session.path().display());
    }
}

fn assemble_session(
    args: &Args,
    registry: ModelRegistry,
    selection: ModelSelection,
    resume_target: Option<PathBuf>,
) -> Result<Session, String> {
    let store = SessionStore::project_default();
    let cwd = std::env::current_dir()
        .map_err(|e| format!("cannot determine the working directory: {e}"))?;
    // Built once per process: the prompt must stay byte-stable for the
    // provider's prompt cache (see the prompt module docs).
    let preamble = build_system_prompt(&cwd).map_err(|e| e.to_string())?;
    let mut builder = SessionBuilder::new(
        store.clone(),
        registry.config().clone(),
        registry.auth().clone(),
        selection,
    )
    .map_err(|e| e.to_string())?
    .preamble(preamble)
    .dynamic_tool(dynamic(tabit_tools::Read))
    .dynamic_tool(dynamic(tabit_tools::Ls))
    .dynamic_tool(dynamic_contextual(tabit_tools::Bash))
    .model_factory({
        let factory = registry.factory();
        move |provider, model| factory(provider, model)
    });
    if let Some(max_turns) = args.max_turns {
        builder = builder.max_turns(max_turns);
    }

    if let Some(path) = &resume_target {
        let (session, report) = builder.resume(path).map_err(|e| e.to_string())?;
        for repair in &report.file_repairs {
            eprintln!(
                "repaired session file {path:?}: {repair:?} \
                 (dropped the torn trailing record)"
            );
        }
        if report.repaired_tool_calls > 0 {
            eprintln!(
                "repaired {} interrupted tool call(s) with synthetic \
                 results before continuing",
                report.repaired_tool_calls
            );
        }
        Ok(session)
    } else {
        let cwd = cwd.display().to_string();
        builder.create(&cwd).map_err(|e| e.to_string())
    }
}

fn run() -> Result<i32, String> {
    let args = parse_args()?;
    let config = Arc::new(TabitConfig::load_default().map_err(|e| e.to_string())?);
    let auth = Arc::new(AuthConfig::load_default().map_err(|e| e.to_string())?);

    match mode_of(&args) {
        Mode::List => {
            let store = SessionStore::project_default();
            list_sessions(&store)?;
            Ok(0)
        }
        Mode::Interactive => Err(format!(
            "interactive mode is not implemented yet; pass -p <PROMPT> for print mode \
             or --json for the stdio protocol\n{USAGE}"
        )),
        Mode::Json if args.print_prompt.is_some() || args.rewind.is_some() => {
            Err("--json selects JSON mode; -p/--rewind select print mode — pick one".to_string())
        }
        Mode::Json => {
            let session = assemble(&args, &config, &auth)?;
            print_banner(&session);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            Ok(runtime.block_on(async {
                let handle = SessionHandle::spawn(session);
                json::serve(
                    handle,
                    std::io::BufReader::new(std::io::stdin()),
                    std::io::stdout(),
                )
                .await
            }))
        }
        Mode::Print => print_mode(&args, &config, &auth),
    }
}

/// Print mode: assemble (rewinding first when asked), banner, one
/// message through the session actor, events printed as they arrive,
/// then the closing footer.
fn print_mode(
    args: &Args,
    config: &Arc<TabitConfig>,
    auth: &Arc<AuthConfig>,
) -> Result<i32, String> {
    if args.rewind.is_some() && args.session.is_none() && !args.continue_newest {
        return Err(
            "--rewind rewinds a session: pass --continue or --session <path> (see --help)"
                .to_string(),
        );
    }
    let mut session = assemble(args, config, auth)?;
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

    // Esc aborts the running turn (stdin is line-buffered in print mode:
    // press Esc, then Enter; real key handling arrives with the TUI). The
    // watcher thread is the single Esc consumer.
    {
        let abort = session.abort_handle();
        std::thread::spawn(move || {
            use std::io::Read as _;
            for byte in std::io::stdin().lock().bytes() {
                if matches!(byte, Ok(0x1b)) {
                    abort.abort();
                    return;
                }
            }
        });
    }

    // The message goes through the session actor — the same path JSON
    // mode drives — and the stream is read to its end: the actor returns
    // the session before closing, so closing stats cover this run.
    let (failed, run_usage, session_path, stats) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(async {
            let mut handle = SessionHandle::spawn(session);
            let session_path = handle.info().session_path.clone();
            handle.message(prompt);
            handle.close_commands();
            let mut failed = None;
            let (mut input, mut output) = (0u64, 0u64);
            while let Some(frame) = handle.next_event().await {
                match &frame.event {
                    SessionEvent::CompletionCall {
                        input_tokens,
                        output_tokens,
                    } => {
                        input += input_tokens;
                        output += output_tokens;
                    }
                    SessionEvent::RunFailed { message } => failed = Some(message.clone()),
                    _ => {}
                }
                print_event(&frame.event);
            }
            (
                failed,
                (input, output),
                session_path,
                handle.closing_stats(),
            )
        });

    eprintln!(
        "--- session {} | tokens {} in / {} out{}",
        session_path,
        run_usage.0,
        run_usage.1,
        stats
            .map(|s| format!(" (session total {:.4} USD)", s.total_cost))
            .unwrap_or_default()
    );
    match failed {
        Some(message) => Err(format!("run failed: {message}")),
        None => Ok(0),
    }
}

/// Resolve config/auth into a session per the args (model selection,
/// resume target, tools, preamble).
fn assemble(
    args: &Args,
    config: &Arc<TabitConfig>,
    auth: &Arc<AuthConfig>,
) -> Result<Session, String> {
    // Default-model resolution (registry): an explicit --model wins,
    // then the resumed session's last model, then default_model in
    // providers.toml, then the first configured model.
    let registry = ModelRegistry::new(config.clone(), auth.clone());
    let store = SessionStore::project_default();
    let resume_target = match (&args.session, args.continue_newest) {
        (Some(path), _) => Some(path.clone()),
        (None, true) => {
            let newest = store
                .list()
                .map_err(|e| e.to_string())?
                .into_iter()
                .next()
                .ok_or_else(|| format!("no sessions yet in {}", store.dir().display()))?;
            Some(newest.path)
        }
        (None, false) => None,
    };
    let resumed = match &resume_target {
        Some(path) => store.last_model(path).map_err(|e| e.to_string())?,
        None => None,
    };
    let explicit = args
        .model
        .as_deref()
        .map(|raw| parse_model(raw, registry.config()))
        .transpose()?;
    let selection = registry
        .default_selection(explicit, resumed)
        .map_err(|e| e.to_string())?;
    assemble_session(args, registry, selection, resume_target)
}

fn main() {
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
        let positional = args(&["hello"]).expect_err("positional prompt");
        assert!(positional.contains("-p"), "{positional}");
    }

    #[test]
    fn print_mode_is_selected_by_prompt_or_rewind() {
        assert_eq!(mode_of(&args(&[]).expect("bare")), Mode::Interactive);
        assert_eq!(mode_of(&args(&["-p", "hi"]).expect("print")), Mode::Print);
        assert_eq!(
            mode_of(&args(&["--rewind", "1"]).expect("rewind")),
            Mode::Print
        );
        assert_eq!(mode_of(&args(&["--json"]).expect("json")), Mode::Json);
        assert_eq!(mode_of(&args(&["--list"]).expect("list")), Mode::List);
        // --list short-circuits everything else.
        assert_eq!(
            mode_of(&args(&["--list", "-p", "hi"]).expect("list wins")),
            Mode::List
        );
    }

    #[test]
    fn json_mode_parses_and_conflicts_loudly_with_print_flags() {
        let parsed = args(&["--continue", "--json"]).expect("valid");
        assert!(parsed.json && parsed.continue_newest);
        assert_eq!(mode_of(&parsed), Mode::Json);

        let conflict = args(&["--json", "-p", "hi"]).expect("parses; mode resolution errors");
        assert!(conflict.json && conflict.print_prompt.is_some());
        // The conflict is a run-time error, not a parse error; the check
        // lives next to mode dispatch in `run`.
        assert_eq!(
            conflict.print_prompt.as_deref(),
            Some("hi"),
            "parsing stays permissive; dispatch rejects the combination"
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
