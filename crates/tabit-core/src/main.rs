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

mod cli;
mod print;

use std::sync::Arc;
use tabit_config::{AuthConfig, TabitConfig};
use tabit_session::SessionStore;

use crate::cli::{Mode, mode_of, parse_args};
use crate::print::{list_sessions, print_mode};
use tabit_app::{install_root, mount_world, serve_json_stdio, setup_guide, world_registry};

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
        Mode::Json => serve_json_stdio(&args.options(), config, auth),
        Mode::Print => {
            let config = config.map_err(|e| setup_guide(&e))?;
            let auth = auth.map_err(|e| e.to_string())?;
            // The same extension world JSON mode rides (owner ruling
            // 2026-09-27): an installed package exists in every mode.
            // The runtime is the boot's and the run's one serving
            // runtime — the supervisor's watchers outlive the boot.
            let (registry, launchable) = world_registry(&args.options(), config, Arc::new(auth))?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            let mounted = mount_world(launchable, &runtime);
            print_mode(&args, &registry, Some(&mounted), &runtime)
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
