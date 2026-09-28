//! EMBEDDING.md's tiers 2+3 together: the composition's presets
//! (`world_registry`/`mount_world`/`assemble`/`host_data`) driving
//! one prompt through a streaming `SessionHost` — the terminal's
//! response text printed once, a `select_one` card
//! rendered on stderr and answered from one stdin line, abort on an
//! Esc-prefixed line. This is the binary's print mode, reduced to
//! its teachable core.

// Example code rides the test-lint relaxation: unwrap and index are
// the shape a reader skims, and the crash family is warn-only per
// the workspace doctrine anyway.
#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;

use tabit_app::{
    AppOptions, ContinueMiss, assemble, host_data, host_node, mount_world, world_registry,
};
use tabit_config::{AuthConfig, TabitConfig};
use tabit_protocol::SessionCommand;
use tabit_session::{Session, SessionEvent, SessionHost, SessionHostWiring, SessionStore};

const PROVIDERS: &str = r#"
default_model = "p/m"

[providers.p]
base_url = "http://127.0.0.1:1234/v1"
api = "openai-completions"
keyless = true

[[providers.p.models]]
id = "m"
"#;

/// One armed card: the interaction id and the label to answer with
/// (the first option — an example always allows; a real frontend
/// reads the number).
type ArmedCard = (String, String);
static ARMED: std::sync::Mutex<std::collections::VecDeque<ArmedCard>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Tier 3: the composition — the extension world's two halves and
    // the assembled session (toolset, gate, skills), the same boot
    // tabit-core runs.
    let options = AppOptions::default();
    let config = TabitConfig::from_toml_str(PROVIDERS, Path::new("providers.toml"))?;
    let auth = AuthConfig::default();
    let (registry, launchable) = world_registry(&options, config, Arc::new(auth))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mounted = mount_world(launchable, &runtime);
    let store = SessionStore::project_default();
    let (session, notes): (Session, Vec<String>) = assemble(
        &options,
        &registry,
        &store,
        ContinueMiss::Fail,
        Some(mounted.clone()),
    )?;

    // Tier 2: the host — events as they happen, commands back.
    let wiring = SessionHostWiring {
        node: host_node(),
        store: store.clone(),
        boot_parent: None,
        boot_parent_call: None,
    };
    let data = host_data(&options, &registry, &store, &mounted);
    let boot = session.id().to_string();
    let mut handle = SessionHost::spawn(session, notes, wiring, data);

    // The input side: an Esc-prefixed line aborts; any other line
    // answers the oldest armed card. Both ride the command link.
    let link = handle.command_link();
    let abort_session = boot.clone();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::stdin().lock().lines().by_ref().flatten() {
            if line.starts_with('\x1b') {
                link.send(SessionCommand::Abort {
                    session: abort_session,
                });
                return;
            }
            if let Some((id, label)) = ARMED.lock().unwrap().pop_front() {
                link.send(SessionCommand::InteractionResponse {
                    session: Some(abort_session.clone()),
                    id,
                    payload: serde_json::json!({ "selected": [label] }),
                });
            }
        }
    });

    handle.message(&boot, "hello — use the bash tool");
    handle.close_commands();
    // The output side: buffer the response, print it once at the
    // terminal (stdout is the answer channel; everything else is
    // stderr).
    while let Some(frame) = handle.next_event().await {
        match &frame.event {
            SessionEvent::ToolCall { name, .. } => eprintln!("→ {name}"),
            SessionEvent::InteractionRequest {
                id,
                ui_type,
                payload,
                ..
            } => card_on_stderr(id, ui_type, payload),
            SessionEvent::RunFinished { output, .. } | SessionEvent::RunAborted { output, .. }
                if !output.is_empty() =>
            {
                println!("{output}");
            }
            _ => {}
        }
    }
    Ok(())
}

/// Render a `select_one` card on stderr and arm one answer (the
/// first option's label — the built-in gate's "Allow").
fn card_on_stderr(id: &str, ui_type: &str, payload: &serde_json::Value) {
    use tabit_protocol::templates;
    if ui_type != templates::ui::SELECT_ONE {
        eprintln!("(unsupported interaction widget `{ui_type}` — not answered)");
        return;
    }
    let card = match serde_json::from_value::<templates::SelectOneCard>(payload.clone()) {
        Ok(card) => card,
        Err(_) => {
            eprintln!("(a card arrived in a shape this surface cannot read)");
            return;
        }
    };
    let legend = card
        .options
        .iter()
        .enumerate()
        .map(|(n, o)| format!("{}) {}", n + 1, o.label))
        .collect::<Vec<_>>()
        .join("  ");
    eprintln!(
        "\n--- {}\n{}\n{legend}   — number, then Enter",
        card.title, card.body
    );
    ARMED
        .lock()
        .unwrap()
        .push_back((id.to_string(), card.options[0].label.clone()));
}
