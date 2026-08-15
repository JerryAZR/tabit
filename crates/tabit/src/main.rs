//! `tabit` — a minimal coding agent, print mode.
//!
//! One prompt in, one outer loop out: events print as they happen, the
//! session is persisted project-locally, and the printed session path
//! resumes the conversation later.
//!
//! ```text
//! tabit "list the rust files in this project"     # new session
//! tabit --continue "now count lines in each"      # resume the newest
//! tabit --session <path> "what did we conclude?"  # resume a specific one
//! tabit --list                                    # show this project's sessions
//! ```

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use tabit_config::{AuthConfig, TabitConfig};
use tabit_session::SessionEvent;
use tabit_session::{ModelSelection, Session, SessionBuilder, SessionStore};
use tabit_tools::dynamic;

/// The minimal system preamble until the prompt builder lands
/// (ROADMAP item 3).
const PREAMBLE: &str = "\
You are tabit, a coding agent running in the user's terminal.
Use the read, ls, and bash tools to inspect and change the workspace \
before answering. Prefer reading files over guessing. Keep answers short \
and factual; report commands you ran and files you changed.";

struct Args {
    prompt: Option<String>,
    session: Option<PathBuf>,
    continue_newest: bool,
    list: bool,
    model: Option<String>,
    max_turns: Option<usize>,
}

const USAGE: &str = "\
usage: tabit [PROMPT]
       tabit --continue PROMPT          resume this project's newest session
       tabit --session <path> PROMPT    resume a specific session file
       tabit --list                     list this project's sessions
       tabit --model <provider/model>   select the model for this run
                                       (default: default_model in providers.toml)

config: providers.toml / auth.toml under ~/.tabit (override with
        TABIT_CONFIG / TABIT_AUTH); sessions live in <project>/.tabit/sessions";

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        prompt: None,
        session: None,
        continue_newest: false,
        list: false,
        model: None,
        max_turns: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--continue" | "-c" => args.continue_newest = true,
            "--list" => args.list = true,
            "--session" => {
                let value = it.next().ok_or("--session needs a path (see --help)")?;
                args.session = Some(PathBuf::from(value));
            }
            "--model" | "-m" => {
                let value = it
                    .next()
                    .ok_or("--model needs provider/model (see --help)")?;
                args.model = Some(value);
            }
            "--max-turns" => {
                let value = it.next().ok_or("--max-turns needs a number (see --help)")?;
                args.max_turns = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--max-turns: `{value}` is not a number"))?,
                );
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}`\n{USAGE}"));
            }
            prompt => {
                if args.prompt.is_some() {
                    return Err(format!("two prompts given; expected one\n{USAGE}"));
                }
                args.prompt = Some(prompt.to_string());
            }
        }
    }
    Ok(args)
}

fn parse_model(raw: &str) -> Result<ModelSelection, String> {
    let (provider, model) = raw
        .split_once('/')
        .ok_or_else(|| format!("--model expects provider/model, got `{raw}` (no `/`)"))?;
    if provider.is_empty() || model.is_empty() {
        return Err(format!("--model expects provider/model, got `{raw}`"));
    }
    Ok(ModelSelection::new(provider, model))
}

fn resolve_selection(args: &Args, config: &TabitConfig) -> Result<ModelSelection, String> {
    if let Some(raw) = &args.model {
        return parse_model(raw);
    }
    config
        .default_model
        .as_ref()
        .map(|d| ModelSelection {
            provider: d.provider.clone(),
            model: d.model.clone(),
            thinking_level: d.thinking_level.clone(),
        })
        .ok_or_else(|| {
            "no model selected: pass --model provider/model or set \
             default_model in providers.toml"
                .to_string()
        })
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
        SessionEvent::NativeItem { .. } => {}
    }
}

fn assemble_session(
    args: &Args,
    config: Arc<TabitConfig>,
    auth: Arc<AuthConfig>,
    selection: ModelSelection,
) -> Result<Session, String> {
    let store = SessionStore::project_default();
    let mut builder = SessionBuilder::new(store.clone(), config, auth, selection)
        .map_err(|e| e.to_string())?
        .preamble(PREAMBLE)
        .dynamic_tool(dynamic(tabit_tools::Read))
        .dynamic_tool(dynamic(tabit_tools::Ls))
        .dynamic_tool(dynamic(tabit_tools::Bash));
    if let Some(max_turns) = args.max_turns {
        builder = builder.max_turns(max_turns);
    }

    if let Some(path) = &args.session {
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
    } else if args.continue_newest {
        let summaries = store.list().map_err(|e| e.to_string())?;
        let newest = summaries
            .first()
            .ok_or_else(|| format!("no sessions yet in {}", store.dir().display()))?;
        let (session, report) = builder.resume(&newest.path).map_err(|e| e.to_string())?;
        if report.repaired_tool_calls > 0 {
            eprintln!(
                "repaired {} interrupted tool call(s) with synthetic \
                 results before continuing",
                report.repaired_tool_calls
            );
        }
        Ok(session)
    } else {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        builder.create(&cwd).map_err(|e| e.to_string())
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let config = Arc::new(TabitConfig::load_default().map_err(|e| e.to_string())?);
    let auth = Arc::new(AuthConfig::load_default().map_err(|e| e.to_string())?);

    if args.list {
        let store = SessionStore::project_default();
        return list_sessions(&store);
    }

    let prompt = args
        .prompt
        .clone()
        .ok_or_else(|| format!("no prompt given\n{USAGE}"))?;
    let selection = resolve_selection(&args, &config)?;
    let mut session = assemble_session(&args, config, auth, selection)?;

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

    let summary = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(session.prompt_with(prompt, &mut |event| print_event(&event)))
        .map_err(|e| e.to_string())?;

    let stats = session.stats().ok();
    eprintln!(
        "--- session {} | tokens {} in / {} out{}",
        session.path().display(),
        summary.usage.input_tokens,
        summary.usage.output_tokens,
        stats
            .map(|s| format!(" (session total {:.4} USD)", s.total_cost))
            .unwrap_or_default()
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("tabit: {error}");
        std::process::exit(1);
    }
}
