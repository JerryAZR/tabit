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
use tabit_session::{
    ModelRegistry, ModelSelection, Session, SessionBuilder, SessionStore, build_system_prompt,
};
use tabit_tools::{dynamic, dynamic_contextual};

#[derive(Debug)]
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

Esc aborts the running turn (line-buffered stdin: Esc then Enter).
       tabit --model <model-id|provider/model> select the model for this run
                                       (default: the resumed session's model,
                                       then default_model in providers.toml,
                                       then the first configured model)

config: providers.toml / auth.toml under ~/.tabit (override with
        TABIT_CONFIG / TABIT_AUTH); sessions live in <project>/.tabit/sessions";

fn parse_args() -> Result<Args, String> {
    parse_args_from(std::env::args().skip(1))
}

/// Manual parsing over an injectable iterator (no clap: four flags do not
/// justify the dependency); `parse_args_from` is the testable core.
fn parse_args_from<I>(args: I) -> Result<Args, String>
where
    I: Iterator<Item = String>,
{
    let mut parsed = Args {
        prompt: None,
        session: None,
        continue_newest: false,
        list: false,
        model: None,
        max_turns: None,
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
            "--session" => {
                let value = it.next().ok_or("--session needs a path (see --help)")?;
                parsed.session = Some(PathBuf::from(value));
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
            prompt => {
                if parsed.prompt.is_some() {
                    return Err(format!("two prompts given; expected one\n{USAGE}"));
                }
                parsed.prompt = Some(prompt.to_string());
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
        SessionEvent::NativeItem { .. } => {}
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

    // Default-model resolution (registry): an explicit --model wins,
    // then the resumed session's last model, then default_model in
    // providers.toml, then the first configured model.
    let registry = ModelRegistry::new(config, auth);
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
    let mut session = assemble_session(&args, registry, selection, resume_target)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse_args_from(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_prompt_and_flags() {
        let parsed = args(&["--continue", "--model", "p/m", "hello world"]).expect("valid");
        assert_eq!(parsed.prompt.as_deref(), Some("hello world"));
        assert!(parsed.continue_newest);
        assert_eq!(parsed.model.as_deref(), Some("p/m"));

        let parsed = args(&["--session", "s.jsonl", "--max-turns", "5", "go"]).expect("valid");
        assert_eq!(
            parsed.session.as_deref(),
            Some(std::path::Path::new("s.jsonl"))
        );
        assert_eq!(parsed.max_turns, Some(5));

        let parsed = args(&["--list"]).expect("valid");
        assert!(parsed.list);
    }

    #[test]
    fn rejects_missing_values_unknown_flags_and_double_prompts() {
        assert!(args(&["--session"]).is_err());
        assert!(args(&["--model"]).is_err());
        assert!(args(&["--max-turns", "x"]).is_err());
        let unknown = args(&["--bogus"]).expect_err("unknown flag");
        assert!(unknown.contains("--bogus"), "{unknown}");
        let two = args(&["one", "two"]).expect_err("two prompts");
        assert!(two.contains("two prompts"), "{two}");
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
