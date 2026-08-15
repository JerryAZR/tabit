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
use tabit_session::{ModelSelection, Session, SessionBuilder, SessionStore, build_system_prompt};
use tabit_tools::dynamic;

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
       tabit --model <provider/model>   select the model for this run
                                       (default: default_model in providers.toml)

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
    let cwd = std::env::current_dir()
        .map_err(|e| format!("cannot determine the working directory: {e}"))?;
    // Built once per process: the prompt must stay byte-stable for the
    // provider's prompt cache (see the prompt module docs).
    let preamble = build_system_prompt(&cwd).map_err(|e| e.to_string())?;
    let mut builder = SessionBuilder::new(store.clone(), config, auth, selection)
        .map_err(|e| e.to_string())?
        .preamble(preamble)
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
    fn model_strings_require_provider_slash_model() {
        assert_eq!(
            parse_model("lmstudio/openai/gpt-oss-20b")
                .expect("three-part ok")
                .model,
            "openai/gpt-oss-20b",
            "the first `/` splits; model ids may contain slashes"
        );
        assert!(parse_model("noprovider").is_err());
        assert!(parse_model("/m").is_err());
        assert!(parse_model("p/").is_err());
    }

    #[test]
    fn selection_resolution_prefers_flag_then_config_default() {
        let mut parsed = args(&["--model", "p/m2", "hi"]).expect("valid");
        assert_eq!(
            resolve_selection(&parsed, &test_config())
                .expect("flag wins")
                .model,
            "m2"
        );

        parsed.model = None;
        assert_eq!(
            resolve_selection(&parsed, &test_config())
                .expect("default")
                .provider,
            "lmstudio"
        );

        let empty = tabit_config::TabitConfig::default();
        let error = resolve_selection(&parsed, &empty).expect_err("no selection");
        assert!(error.contains("default_model"), "{error}");
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
