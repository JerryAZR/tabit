//! The frozen wire's stdio serving as a library entry — the JSON
//! mode the `tabit-core` binary runs, and the child role an
//! embedder's own binary dispatches to. With this, the subagent
//! self-spawn pattern (`SubagentParts::exe = current_exe`, the "no
//! exceptions" ruling) is correct for embedders too: point your
//! `main` at [`serve_json_stdio`] for the child-role argv, and your
//! process speaks the same wire `tabit-core` does — a spawner
//! cannot tell them apart.

use std::sync::Arc;

use tabit_config::{AuthConfig, TabitConfig};
use tabit_session::{SessionHost, SessionHostWiring, SessionStore};

use crate::assemble::{ContinueMiss, assemble, host_data, host_node, mount_world, world_registry};
use crate::options::AppOptions;

/// The first-run setup guide, for the config errors that remain
/// fatal (a broken file, an explicit `$TABIT_CONFIG` pointer that
/// misses) — the message must teach, not scare. (Pub for the
/// binary's print arm, which shares the guidance on stderr.) A
/// MISSING default providers.toml is no longer an error at all (the
/// owner's first-run ruling, reversed 2026-10 on the pi precedent):
/// zero config boots on the empty config — the session announces
/// `model: null`, `models_available` emits empty, the registry's
/// teaching note rides the startup notes, and a selection-less run
/// fails at open with `run_failed { kind: "model" }`. The teaching
/// moved from process-death to those three carriers.
pub fn setup_guide(detail: &str) -> String {
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

/// JSON-mode setup failure — a config/auth file exists but is
/// unreadable or unparseable — so the failure carries the first-run
/// guide (the message must teach, not scare). A MISSING default file
/// never reaches here: `load_default` answers the empty config (the
/// reversed first-run ruling — see [`setup_guide`]).
fn json_setup_failure(detail: &str) -> ! {
    json_reject(setup_guide(detail))
}

/// JSON-mode startup failure that is *not* a config problem (session
/// unreadable, an explicit `--model` naming a ref config does not
/// know, cwd gone): fail with the plain reason — the setup guide
/// would be advice for a problem the user does not have. ("Model
/// unbuildable" is no longer in this class: the lazy agent cache
/// moves construction failure to the run-open `run_failed`, and the
/// no-usable-model case boots selection-less.)
fn json_startup_failure(detail: &str) -> ! {
    json_reject(format!("could not start the session: {detail}"))
}

/// The report model's startup failure (owner ruling 2026-09-25): the
/// child has reported (the spawner knows the version and that this
/// process is alive), then the reason crosses as an unstamped `error`
/// event — the same grammar every other backend-level failure uses —
/// and the process exits nonzero. The reason also echoes on stderr
/// (the human surface).
fn json_reject(reason: String) -> ! {
    let report = tabit_protocol::ServerControlFrame::Report {
        protocol_version: tabit_protocol::PROTOCOL_VERSION,
    };
    println!("{}", tabit_protocol::to_wire_line(&report));
    let event = tabit_protocol::EventFrame {
        stream: None,
        origin: None,
        ttl: None,
        event: tabit_session::SessionEvent::error_session(reason.clone()),
    };
    println!("{}", tabit_protocol::to_wire_line(&event));
    eprintln!("{reason}");
    std::process::exit(1)
}

/// Serve the frozen wire on stdio: the composition's JSON mode as one
/// call — the extension world, the session host, the boot session,
/// and the edge loop over the real stdin/stdout. This is the
/// `tabit-core` binary's `--json` arm, and an embedder's child-role
/// entry. `config`/`auth` arrive as `load_default`'s outcomes; the
/// first-run shape is theirs (a missing default file loads as the
/// empty config — the reversed ruling), and what remains fatal here
/// is the genuinely broken file.
///
/// **This function does not return** — every path ends in
/// `std::process::exit`: rejection paths exit 1 after the wire
/// frames, and the served path exits with the edge's code (the
/// edge's contract is the process boundary: its reader thread parks
/// in an uninterruptible read on the stream-end path, and a runtime
/// drop would wait on it forever). Call it from `main` and let it
/// own the exit.
pub fn serve_json_stdio(
    options: &AppOptions,
    config: Result<TabitConfig, String>,
    auth: Result<AuthConfig, String>,
) -> ! {
    // A broken config file is a graceful wire rejection carrying the
    // setup guide; a MISSING default file never reaches here (the
    // reversed first-run ruling: zero config boots —
    // `load_default` answers the empty config, the registry degrades
    // to a selection-less session, and the teaching rides the
    // announced catalog, the null selection, and the run-open
    // failure).
    let (config, auth) = match (config, auth) {
        (Ok(config), Ok(auth)) => (config, auth),
        (Err(detail), _) | (_, Err(detail)) => json_setup_failure(&detail),
    };
    // The extension world's data half (settings, scan, fragment
    // merge, skills, registry); a settings failure is a plain
    // startup failure.
    let (registry, launchable) = match world_registry(options, config, Arc::new(auth)) {
        Ok(world) => world,
        Err(detail) => json_startup_failure(&detail),
    };
    let registry = Arc::new(registry);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => json_startup_failure(&error.to_string()),
    };
    // The extension host boots and every handshake resolves BEFORE
    // the session assembly: tools exist at session build (the
    // byte-stability law), and the cost is the slowest single
    // extension — the handshakes run concurrently inside their
    // supervision tasks, so a broken package costs one boot, loudly,
    // and never delays a healthy sibling. The lanes mount on the
    // process's node — the same net the session host and the
    // subprocess bridge ride. The boot is structure, then data:
    // structure first (the frontend stream, then the session host's
    // command surface — live before any child can speak), data next
    // (the extensions gather; the session builds last).
    let frontend = tabit_session::mount_frontend(&host_node());
    let store = SessionStore::project_default();
    let structure = runtime.block_on(async {
        SessionHost::mount(
            SessionHostWiring {
                node: host_node(),
                store: store.clone(),
                boot_parent: options.parent.clone(),
                boot_parent_call: options.parent_call.clone(),
            },
            frontend,
        )
    });
    let mounted = mount_world(launchable, &runtime);
    // Assemble failures (a session unreadable, an explicit `--model`
    // naming a ref config does not know) reject the handshake with
    // the plain reason — not the config setup guide, which would be
    // advice for a problem the user does not have. A `--continue`
    // that finds no sessions is absorbed into a fresh start (the
    // pinned startup contract; `session_opened`'s `resumed: false`
    // says so).
    let (session, startup_notes) = match assemble(
        options,
        &registry,
        &store,
        ContinueMiss::StartFresh,
        Some(mounted.clone()),
    ) {
        Ok(assembled) => assembled,
        Err(detail) => json_startup_failure(&detail),
    };
    startup_banner(&session);
    let data = host_data(options, &registry, &store, &mounted);
    runtime.block_on(async {
        let handle = structure.attach(session, startup_notes, data);
        let code = tabit_session::edge::serve(
            handle,
            std::io::BufReader::new(std::io::stdin()),
            std::io::stdout(),
        )
        .await;
        // The edge's contract is the process boundary: exit here,
        // never through the runtime drop (the reader thread parks in
        // an uninterruptible read on the stream-end path, and a
        // runtime drop would wait on it forever).
        std::process::exit(code);
    })
}

/// The human startup banner, on stderr (stdout is the wire in JSON
/// mode and the answer channel in print mode). Shared by both I/O
/// arms; the binary's print mode imports it.
pub fn startup_banner(session: &tabit_session::Session) {
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
