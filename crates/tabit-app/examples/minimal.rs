//! EMBEDDING.md's tier 1, runnable: one ephemeral session, one
//! prompt, the response printed once. The provider comes from a
//! TOML literal (no config file needed — `from_toml_str` is the
//! programmatic door); point `base_url` at any OpenAI-compatible
//! endpoint. `auth` is empty because this provider is keyless.

use std::path::Path;
use std::sync::Arc;

use tabit_config::{AuthConfig, TabitConfig};
use tabit_session::{ModelSelection, SessionBuilder, SessionStore};

// Any OpenAI-compatible chat endpoint; `m` is whatever model id the
// endpoint serves.
const PROVIDERS: &str = r#"
[providers.p]
base_url = "http://127.0.0.1:1234/v1"
api = "openai-completions"
keyless = true

[[providers.p.models]]
id = "m"
"#;

#[tokio::main]
async fn main() -> Result<(), tabit_session::SessionError> {
    let config = Arc::new(TabitConfig::from_toml_str(
        PROVIDERS,
        Path::new("providers.toml"),
    )?);
    let auth = Arc::new(AuthConfig::default());
    let store = SessionStore::project_default();
    let mut session = SessionBuilder::new(store, config, auth, ModelSelection::new("p", "m"))?
        // Ephemeral: in memory only — nothing touches disk. Swap for
        // `.create(cwd)` to leave a resumable session file behind.
        .ephemeral(".")?;
    // The whole outer loop — every turn, every tool roundtrip —
    // awaited in one call; `run.output` is the final answer.
    let run = session.prompt("explain what you can do, briefly").await;
    match run.outcome {
        tabit_session::RunOutcome::Completed => {
            println!("{}", run.output);
            Ok(())
        }
        tabit_session::RunOutcome::Failed => {
            eprintln!(
                "run failed: {}",
                run.events
                    .iter()
                    .rev()
                    .find_map(|e| match e {
                        tabit_session::SessionEvent::RunFailed { message, .. } =>
                            Some(message.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "unknown failure".into())
            );
            std::process::exit(1);
        }
        tabit_session::RunOutcome::Aborted => {
            eprintln!("aborted");
            std::process::exit(130);
        }
    }
}
