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
//!
//! Layout: `cli` parses the command line, `assemble` is the binary's
//! assembly policy (session builds, tool sets, the extension boot),
//! `print` is print mode — the smallest frontend — and this file is
//! the entry: mode dispatch and the failure reporters.

mod assemble;
mod cli;
mod extensions;
// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and the ask-answer reads live on that ergonomics
// (the same allowance the extension SDK's bins carry).
#[allow(clippy::indexing_slicing)]
mod gate;
mod print;

use std::sync::Arc;
use tabit_config::{AuthConfig, TabitConfig};
use tabit_session::{ModelRegistry, SessionHost, SessionHostWiring, SessionStore};

use crate::assemble::{
    ContinueMiss, assemble, boot_extensions, core_sets, extension_root, extension_skills_catalog,
    host_data, host_node, install_root, merge_fragment_into, partition, seed_extension_skills,
};
use crate::cli::{Mode, mode_of, parse_args};
use crate::print::{list_sessions, print_banner, print_mode};

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
/// the failure carries the first-run guide (a fresh install has no
/// providers.toml — the most common first run; the message must teach,
/// not scare).
fn json_setup_failure(detail: &str) -> Result<i32, String> {
    json_reject(setup_guide(detail))
}

/// JSON-mode startup failure that is *not* a config problem (session
/// unreadable, model unbuildable, cwd gone): fail with the plain
/// reason — the setup guide would be advice for a problem the user
/// does not have.
fn json_startup_failure(detail: &str) -> Result<i32, String> {
    json_reject(format!("could not start the session: {detail}"))
}

/// The report model's startup failure (owner ruling 2026-09-25): the
/// child has reported (the spawner knows the version and that this
/// process is alive), then the reason crosses as an unstamped `error`
/// event — the same grammar every other backend-level failure uses —
/// and the process exits nonzero. The reason also echoes on stderr
/// (the human surface).
fn json_reject(reason: String) -> Result<i32, String> {
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
            seed_extension_skills(extension_skills_catalog(&launchable.packages));
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
            // The lanes mount on the process's node — the same net
            // the session host and the subprocess bridge ride: every
            // extension's grammar lines enter through the node's
            // intake from its lane, and its watch list subscribes the
            // lane's channel. No bridges, no drains — the grammar's
            // other end (the session host) reads the same tables.
            // The boot is structure, then data. Structure first: the
            // frontend stream (every frame any participant emits from
            // its first line crosses it), then the session host's
            // command surface — the by-type lifecycle handlers, live
            // before any child can speak. Participants are peers, not
            // subordinates (owner ruling 2026-09): any node may send
            // anything a frontend can from its handshake onward (a
            // co-frontend's `new_session`, a child's steer), so the
            // net must be prepared before the first child boots —
            // lifecycle arrivals before the data exists park, and
            // serve behind the boot's announcements. Data next: the
            // extensions gather; the session builds last.
            let frontend = tabit_session::mount_frontend(&host_node());
            // The mount spawns (the death watchers, the wind-down), so
            // it runs on the runtime — the whole boot sits inside it.
            let store = SessionStore::project_default();
            let structure = runtime.block_on(async {
                SessionHost::mount(
                    SessionHostWiring {
                        node: host_node(),
                        store: store.clone(),
                        boot_parent: args.parent.clone(),
                        boot_parent_call: args.parent_call.clone(),
                    },
                    frontend,
                )
            });
            let launch_context = tabit_ext::LaunchContext {
                node: host_node(),
                // The host IS the binary: owned-session spawners get
                // the running executable, never a resolution search.
                core_path: std::env::current_exe()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                cwd: std::env::current_dir()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            };
            let mounted = {
                let supervisor = runtime.block_on(async {
                    let supervisor = boot_extensions(launchable.found, launch_context);
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
                &store,
                ContinueMiss::StartFresh,
                Some(mounted.clone()),
            ) {
                Ok(assembled) => assembled,
                Err(detail) => return json_startup_failure(&detail),
            };
            print_banner(&session);
            let data = host_data(&args, &registry, &store, &mounted);
            Ok(runtime.block_on(async {
                let handle = structure.attach(session, startup_notes, data);
                let code = tabit_session::edge::serve(
                    handle,
                    std::io::BufReader::new(std::io::stdin()),
                    std::io::stdout(),
                )
                .await;
                // The edge's contract is the process boundary: exit
                // here, never through the runtime drop (the reader
                // thread parks in an uninterruptible read on the
                // stream-end path, and a runtime drop would wait on it
                // forever).
                std::process::exit(code);
            }))
        }
        Mode::Print => {
            let config = Arc::new(config.map_err(|e| setup_guide(&e))?);
            let auth = Arc::new(auth.map_err(|e| e.to_string())?);
            print_mode(&args, &ModelRegistry::new(config, auth))
        }
    }
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
    // The backend's diagnostics: one subscriber, stderr, WARN and up
    // — the same door the lock TTL tripwire writes to. Print mode's
    // stdout is the answer channel; everything diagnostic is stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
        .init();
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
