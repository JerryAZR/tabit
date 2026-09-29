//! The composition root's assembly policy — everything ruled to
//! live HERE, never in tabit-session (the front/back split: sessions
//! are mechanism, the composition decides what mounts). Session
//! builds (the preamble, the tool sets, the invocation's tool
//! filter, the hook stack), the process's node, and the extension
//! side of a boot (the scan's shaping, the providers-fragment merge,
//! the skills contribution, the supervisor launch). The `tabit-core`
//! binary is one consumer; an embedder is any other.

use std::path::PathBuf;
use std::sync::Arc;

use tabit_config::TabitConfig;
use tabit_session::{
    ModelRegistry, ModelSelection, Session, SessionBuilder, SessionStore, build_system_prompt,
    build_system_prompt_with_base,
};
use tabit_tools::dynamic_contextual;

use crate::extensions;
use crate::gate;
use crate::options::AppOptions;
use crate::options::parse_model;

/// The process's node — the one routing layer the session host and
/// its subprocess children mount on (one net per process, by design:
/// the host's routes and the children's lanes live in one learning
/// table), so a `OnceLock` is the honest shape rather than threading
/// an `Arc` through every assembly site. The name is the ask-id mint
/// — unique per process, never colliding with a child's (a child
/// names its node by its boot session's uuid).
pub fn host_node() -> std::sync::Arc<tabit_session::Node> {
    static NODE: std::sync::OnceLock<std::sync::Arc<tabit_session::Node>> =
        std::sync::OnceLock::new();
    NODE.get_or_init(|| {
        std::sync::Arc::new(tabit_session::Node::new(&format!(
            "core-{}",
            std::process::id()
        )))
    })
    .clone()
}

static EXTENSION_SKILLS: std::sync::OnceLock<tabit_session::skills::Skills> =
    std::sync::OnceLock::new();

/// The extension host's skills contribution — the one process-level
/// piece of the catalog (one host per backend; children boot their
/// own hosts against the parent's root, the 2026-09 ruling). The
/// LADDER half is per-session now (the session-level catalog ruling,
/// 2026-09: each session build discovers over its own cwd, so a
/// subagent in another directory announces and runs ITS skills);
/// within a session the consistency triple — the prompt's listing,
/// the `skill` tool's lookup, the wire snapshot — still reads one
/// catalog object, built once at the session's build.
fn extension_skills_part() -> tabit_session::skills::Skills {
    EXTENSION_SKILLS.get().cloned().unwrap_or_default()
}

/// Seed the extension contribution (the JSON boot). Seeding after a
/// reader is an ordering bug, not a condition to absorb — the boot
/// runs before any assembly by construction.
#[allow(clippy::panic)] // the sanctioned crash below (AGENTS.md doctrine)
pub(crate) fn seed_extension_skills(extension_skills: tabit_session::skills::Skills) {
    if EXTENSION_SKILLS.set(extension_skills).is_err() {
        panic!(
            "internal invariant violated: a session assembled before the boot seeded the extension skills"
        );
    }
}

/// The tabit-core executable subprocess children spawn: this very
/// binary (the pi self-spawn pattern). `current_exe`, no exceptions —
/// an inherited `TABIT_CORE_BIN` (a frontend's dev override for
/// finding the backend) must not diverge children from the running
/// image.
fn tabit_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot resolve the tabit-core executable: {e}"))
}

/// The invocation's tool filter (`--tools` allow, `--without` deny),
/// split and validated against the process's full candidate set —
/// core and extension proxies alike. A name nothing offers is a loud
/// startup error listing what exists: a typo'd filter that quietly
/// kept or dropped the wrong tool would look like a broken agent.
/// Allow first, then deny — the surviving set is
/// allowed-and-not-denied.
/// The invocation's tool filter: the `--tools` allow-list and the
/// `--without` deny-list, comma-split. No validation against the
/// offered set — include/exclude-if-it-exists (owner ruling
/// 2026-09-27): a name the process does not offer simply matches
/// nothing, and an allow that matches nothing is a tool-less
/// session, a legal shape (a chatbot). Forwarded child lists rely
/// on this — a child may be sent names it does not offer.
fn tool_filter(args: &AppOptions) -> (Option<Vec<String>>, Vec<String>) {
    let split = |spec: &Option<String>| -> Option<Vec<String>> {
        let raw = spec.as_deref()?;
        Some(
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        )
    };
    (split(&args.tools), split(&args.without).unwrap_or_default())
}

/// Keep the tools the invocation's filter admits: allowed (when an
/// allow-list crossed) and not denied. Pure name matching — both
/// specs were validated against the full candidate set already, so
/// subsets (the child core set behind `SubagentParts::tools`) filter
/// without re-validation; a proxy-only name simply matches nothing
/// here, which is correct (proxy shaping is the child's business,
/// via the crossing flags).
fn retain_filtered(
    tools: Vec<tabit_engine::tool::DynamicTool>,
    allow: &Option<Vec<String>>,
    deny: &[String],
) -> Vec<tabit_engine::tool::DynamicTool> {
    tools
        .into_iter()
        .filter(|tool| {
            let name = tool.name();
            allow
                .as_ref()
                .is_none_or(|names| names.iter().any(|n| n == name))
                && !deny.iter().any(|n| n == name)
        })
        .collect()
}

fn assemble_session(
    args: &AppOptions,
    registry: ModelRegistry,
    selection: ModelSelection,
    resume_target: Option<PathBuf>,
    store: SessionStore,
    extensions: Option<&std::sync::Arc<extensions::Mounted>>,
) -> Result<Session, String> {
    let cwd = std::env::current_dir()
        .map_err(|e| format!("cannot determine the working directory: {e}"))?;
    // The session's skills catalog — ONE DISCOVERY PER SESSION
    // (the session-level catalog ruling): the ladder over this
    // session's cwd with the process-level extension contribution
    // folded in. The prompt stays byte-stable per session for the
    // provider's prompt cache, and the consistency triple — the
    // prompt's listing, the `skill` tool's lookup, the wire snapshot
    // — reads this one object.
    let skills = std::sync::Arc::new(
        tabit_session::skills::discover(&cwd).with_extension_defaults(extension_skills_part()),
    );
    // `--preamble` replaces the default preamble — the identity and
    // standing body — while the environment block, AGENTS.md files,
    // and skills catalog append as usual (ruled 2026-09: the spawner
    // owns the child's voice; tabit still owns the truthful context).
    let preamble = match &args.preamble {
        Some(text) if text.trim().is_empty() => {
            return Err("the --preamble override is empty".to_string());
        }
        Some(text) => {
            build_system_prompt_with_base(text, &cwd, &skills).map_err(|e| e.to_string())?
        }
        None => build_system_prompt(&cwd, &skills).map_err(|e| e.to_string())?,
    };

    // Subagent support (ROADMAP item 5): the process-wide parts. The
    // child's toolset is the CHILD's assembly over its own candidate
    // set — the parent forwards policy, never tools: the assembly's
    // effective lists cross as `--tools`/`--without`, the blacklist
    // extended with `subagent`/`followup` (the recursion guard is
    // the spawner's blacklist policy, never a baked-in role check).
    let parent_core = core_tools();
    // The process's candidate toolset: its core set plus the extension
    // mount — replaced core tools unmount, the proxies join (one
    // name, one tool, resolved at this assembly). Children resolve
    // against their own core set (the child set): they boot their own
    // hosts.
    let mut manifest_disables: Vec<String> = Vec::new();
    let candidate: Vec<tabit_engine::tool::DynamicTool> = match extensions {
        Some(mounted) => {
            manifest_disables = mounted.manifest_disables().to_vec();
            let replaced = mounted.replaced_core();
            parent_core
                .into_iter()
                .filter(|tool| !replaced.iter().any(|name| name == tool.name()))
                .chain(mounted.tools().iter().cloned())
                .collect()
        }
        None => parent_core,
    };
    // The invocation's filter applies once, here, over the full
    // candidate — `--tools`/`--without` shape extension proxies the
    // same as core tools (a whitelisted read-only agent gets no
    // extension write tools; a denied delegate tool cannot recurse).
    let (allow, mut deny) = tool_filter(args);
    // The scanned manifests' role-shaping declarations join the deny
    // list — `--without`'s own storage, the same filter at the same
    // point, no separate mechanism.
    deny.extend(manifest_disables);
    let mounted = retain_filtered(candidate, &allow, &deny);
    // The same lists forward to children (the deny already carries
    // the manifests' role-shaping disables — children re-derive the
    // same ones from the same root; agreement, not a second source).
    let subagents = std::sync::Arc::new(tabit_session::subagent::SubagentParts {
        tool_allow: allow.clone(),
        tool_deny: deny.clone(),
        max_turns: args.max_turns.unwrap_or(tabit_session::DEFAULT_MAX_TURNS),
        node: host_node(),
        exe: tabit_exe()?,
        // Children boot their own hosts against the parent's root
        // (the same packages, the same rules).
        extensions: extension_root(args).unwrap_or_default(),
    });

    // The hook surface: the built-in permission gate (pi-sanity's
    // policy, in-process — a default must not fail open on a dead
    // extension; settings.toml's [gate] enabled = false opts out)
    // ahead of whatever the extension mount carries — forwarded
    // policy hooks of installed packages — composed through the
    // builder's one seam. Children mount their own (they boot their
    // own hosts, the 2026-09 ruling).
    let gate_enabled = tabit_config::SettingsConfig::load_default()
        .map_err(|e| e.to_string())?
        .gate
        .enabled;
    let hooks = {
        let mut stack = if gate_enabled {
            gate::PermissionGate::stack()
        } else {
            tabit_engine::agent::HookStack::new()
        };
        if let Some(mounted) = extensions {
            stack = stack.merge(mounted.hooks());
        }
        stack
    };
    let mut builder = SessionBuilder::new(
        store,
        registry.config().clone(),
        registry.auth().clone(),
        selection,
    )
    .map_err(|e| e.to_string())?
    .preamble(preamble)
    .model_factory(registry.factory())
    .hooks(hooks)
    .subagents(subagents)
    .skills(skills);
    for tool in mounted {
        builder = builder.dynamic_tool(tool);
    }
    if let Some(max_turns) = args.max_turns {
        builder = builder.max_turns(max_turns);
    }

    let cwd = cwd.display().to_string();
    if let Some(path) = &resume_target {
        let (session, _report) = builder.resume(path, &cwd).map_err(|e| e.to_string())?;
        Ok(session)
    } else {
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
/// The coding tools every assembly starts from.
fn coding_tools() -> Vec<tabit_engine::tool::DynamicTool> {
    vec![
        dynamic_contextual(tabit_tools::Read),
        dynamic_contextual(tabit_tools::Write),
        dynamic_contextual(tabit_tools::Edit),
        tabit_tools::shell_tool(),
        tabit_session::skills::skill_tool(),
    ]
}

/// The core toolset every assembly starts from: the coding tools
/// plus the delegation pair. Role-independent (owner ruling
/// 2026-09-27): recursion is the spawner's blacklist policy — the
/// built-in subagent tool denies `subagent`/`followup` in its
/// children — never a baked-in parent check. Pure derivation; the
/// invocation's tool filter applies later, once, over the full
/// candidate set (core plus extension proxies). The extension
/// mount's conflict baseline is this set — exactly what the session
/// would mount without extensions.
pub fn core_tools() -> Vec<tabit_engine::tool::DynamicTool> {
    let mut tools = coding_tools();
    tools.push(tabit_session::subagent::subagent_tool());
    tools.push(tabit_session::subagent::followup_tool());
    tools
}

/// The host's session builders — the boot's DATA half (what only
/// exists once the extension handshakes resolved): how
/// `new_session`/`open_session` build sessions, the same assembly as
/// the boot (config, tools, preamble), behind closures so
/// tabit-session stays free of front-facing wiring. The process's
/// `--model`/`--max-turns` apply to sessions created later;
/// `open_session` resolves by stored id and resumes that file.
/// One registry for the whole process (the ruling: providers are user
/// config, not per-session) — every session the host builds shares
/// the provider client caches.
pub fn host_data(
    args: &AppOptions,
    registry: &ModelRegistry,
    store: &SessionStore,
    extensions: &std::sync::Arc<extensions::Mounted>,
) -> tabit_session::SessionHostData {
    let fresh_args = AppOptions {
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
    tabit_session::SessionHostData {
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
            let args = AppOptions {
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

/// The install target: the default extensions root only (task 6's
/// ruling — `--extensions` is a backend test/dev override, not an
/// install destination).
pub fn install_root() -> Result<PathBuf, String> {
    tabit_config::home_dir()
        .map(|home| home.join(".tabit").join("extensions"))
        .ok_or_else(|| "cannot resolve the home directory for the extensions root".to_string())
}

pub fn extension_root(args: &AppOptions) -> Option<PathBuf> {
    args.extensions
        .clone()
        .or_else(|| tabit_config::home_dir().map(|home| home.join(".tabit").join("extensions")))
}

/// The process's extension world, data half: settings (the disable
/// list), the one scan, the partition, the providers-fragment merge
/// under the user config, the skills tables, and the registry over
/// the merged providers. Shared by both session modes (owner ruling
/// 2026-09-27: the extension host is process machinery — print mode
/// rides the same world, so an installed package surprises nobody by
/// existing in one mode and not the other). The boot half is
/// [`mount_world`], kept separate so the JSON edge can mount its
/// wire structure between them.
pub fn world_registry(
    args: &AppOptions,
    config: tabit_config::TabitConfig,
    auth: std::sync::Arc<tabit_config::AuthConfig>,
) -> Result<(ModelRegistry, Launchable), String> {
    // Settings (the extension disable list's layers): absence is
    // normal — a bare machine disables nothing, packages mount by
    // default — while a broken file is a loud startup failure.
    let settings = tabit_config::SettingsConfig::load_default().map_err(|e| e.to_string())?;
    let disabled = settings.disabled_extensions();
    // One scan feeds every consumer — launch, the providers fragment
    // merge, the skills tables — so they cannot disagree (the same
    // one-scan law as the catalog).
    let found = extension_root(args)
        .as_deref()
        .map(tabit_ext::manifest::scan)
        .unwrap_or_default();
    let launchable = partition(found, &disabled);
    // Providers fragments merge under the user config (the user's
    // own ids win silently; only a fragment colliding with an
    // earlier fragment warns).
    let mut merged = config;
    let user_ids: std::collections::HashSet<String> = merged.providers.keys().cloned().collect();
    let mut warnings = Vec::new();
    for (name, dir) in &launchable.packages {
        merge_fragment_into(&mut merged, name, dir, &user_ids, &mut warnings);
    }
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    // The skills tables: the extension walker produces what the
    // packages ship, with their original paths, and the process's
    // one catalog folds them under the ladder — seeded before any
    // assembly reads it (the prompt build is the first reader). No
    // filesystem writes: the entries' locations ARE the packages'
    // paths.
    seed_extension_skills(extension_skills_catalog(&launchable.packages));
    Ok((
        ModelRegistry::new(std::sync::Arc::new(merged), auth),
        launchable,
    ))
}

/// The extension world, boot half: launch + handshakes + the tool
/// mount over the core baseline. `runtime` is the SERVING runtime —
/// the boot spawns watchers that must outlive it.
pub fn mount_world(
    launchable: Launchable,
    runtime: &tokio::runtime::Runtime,
) -> std::sync::Arc<extensions::Mounted> {
    let launch_context = tabit_ext::LaunchContext {
        node: host_node(),
        // The host IS the binary: owned-session spawners get the
        // running executable, never a resolution search.
        core_path: std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        cwd: std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    };
    let supervisor = runtime.block_on(async {
        let supervisor = boot_extensions(launchable.found, launch_context);
        supervisor.await_resolved().await;
        supervisor
    });
    // The conflict baseline is the core toolset — exactly what a
    // session would mount without extensions.
    let core = core_tools();
    std::sync::Arc::new(extensions::Mounted::mount(supervisor, &core))
}

/// The extension host boot: launch the scanned packages, handshake
/// each, supervise for the backend's life — reports land on stderr
/// (stdout is protocol). Every tabit process boots its own extension
/// host — the frontend-attached backend AND every subagent child
/// (ruled 2026-09: children pick up extensions; the leaf law outlaws
/// loading into a parent's process, not a child hosting its own
/// set). Must run on the serving runtime (it spawns).
pub(crate) fn boot_extensions(
    found: Vec<tabit_ext::manifest::Discovered>,
    host: tabit_ext::LaunchContext,
) -> std::sync::Arc<tabit_ext::supervisor::Supervisor> {
    // Reports land on stderr (stdout is protocol).
    let (supervisor, mut events) =
        tabit_ext::supervisor::launch(found, tabit_ext::supervisor::BOOT_TIMEOUT, host);
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

/// The scan, shaped for boot (item 9, tasks 4+6): what the host
/// LAUNCHES — every refusal plus the not-disabled PROCESS packages —
/// and the MOUNTED packages themselves (name + dir), the list the
/// providers fragment merge, the skills tables, and the requirement
/// check apply to. Packages mount by default (install was the
/// consent; disabling is the explicit act), and a disabled package
/// is absent everywhere by design — including as a requirement
/// (unmet). **Static packages** (no entry) mount but never launch
/// and never announce: their contributions are exactly the
/// scan-driven ones. Unmet `requires` become refusals (presence, not
/// liveness — the mounted set is the truth, standing never is).
pub struct Launchable {
    pub found: Vec<tabit_ext::manifest::Discovered>,
    pub packages: Vec<(String, PathBuf)>,
}

pub(crate) fn partition(
    found: Vec<tabit_ext::manifest::Discovered>,
    disabled: &std::collections::HashSet<String>,
) -> Launchable {
    let mut mounted = Vec::new();
    let mut refusals = Vec::new();
    for found in found {
        match found {
            tabit_ext::manifest::Discovered::Package { dir, manifest } => {
                if !disabled.contains(&manifest.name) {
                    mounted.push(tabit_ext::manifest::Discovered::Package { dir, manifest });
                }
                // A disabled package is absent everywhere — not a
                // refusal (the user's setting reports nowhere), just
                // gone.
            }
            // Refusals always launch (as dead reports): a broken
            // package is loud, whatever the settings say.
            refused => refusals.push(refused),
        }
    }
    // The requirement check runs over the mounted set: disabled or
    // missing requirements are unmet, dead ones are not.
    let mounted_names: std::collections::HashSet<String> = mounted
        .iter()
        .filter_map(|found| found.manifest().map(|m| m.name.clone()))
        .collect();
    let mounted = tabit_ext::manifest::enforce_requires(mounted, &mounted_names);
    // Split by process-ness: static packages contribute scan facts
    // only; process packages (and every refusal) reach the launch.
    let mut launchable = refusals;
    let mut packages = Vec::new();
    for found in mounted {
        match &found {
            tabit_ext::manifest::Discovered::Package { dir, manifest } => {
                packages.push((manifest.name.clone(), dir.clone()));
                if !manifest.is_static() {
                    launchable.push(found);
                }
            }
            tabit_ext::manifest::Discovered::Refused { .. } => {
                launchable.push(found);
            }
        }
    }
    launchable.sort_by(|a, b| a.dir().cmp(b.dir()));
    Launchable {
        found: launchable,
        packages,
    }
}

/// Merge one mounted package's `providers.toml` fragment into `config`
/// (EXTENSIONS.md: the user's own provider ids win silently; only a
/// fragment colliding with an earlier fragment warns). A broken
/// fragment refuses the *fragment* — warned, skipped — never the
/// package, whose tools and hooks are unaffected.
pub(crate) fn merge_fragment_into(
    config: &mut TabitConfig,
    name: &str,
    dir: &std::path::Path,
    user_ids: &std::collections::HashSet<String>,
    warnings: &mut Vec<String>,
) {
    let path = dir.join("providers.toml");
    if !path.is_file() {
        return;
    }
    match TabitConfig::load(&path) {
        Ok(fragment) => {
            config.merge_fragment(fragment, &format!("extension `{name}`"), user_ids, warnings);
        }
        Err(detail) => {
            warnings.push(format!(
                "extension `{name}`: providers fragment refused: {detail}"
            ));
        }
    }
}

/// The extension walker's skills contribution (item 9, task 4): every
/// mounted package's `skills/` tree, entries at their original paths,
/// first-package-wins on a name collision (scan order — the same
/// determinism law as tool registration). In-memory tables only; the
/// catalog, the `skill` tool, and the wire snapshot read them.
pub(crate) fn extension_skills_catalog(
    packages: &[(String, PathBuf)],
) -> tabit_session::skills::Skills {
    let mut extension_skills = tabit_session::skills::Skills::default();
    for (_name, dir) in packages {
        for entry in tabit_session::skills::entries_in(&dir.join("skills")) {
            extension_skills.register(entry);
        }
    }
    extension_skills
}

/// What happens when `--continue` finds nothing to resume. Print mode
/// fails loudly (a terminal user asked explicitly); JSON mode starts
/// fresh — the pinned startup contract: the chat UI is unconditional,
/// and an empty store (a brand-new project) is not an error. The
/// handshake's `resumed: false` tells the frontend what happened.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ContinueMiss {
    Fail,
    StartFresh,
}

/// Resolve config/auth into a session per the args (model selection,
/// resume target, tools, preamble). `store` is injected so tests drive
/// a temp store instead of the repo's. The registry is the caller's
/// process-shared one (owner ruling: providers are user config, not
/// per-session — one client cache per provider per process).
pub fn assemble(
    args: &AppOptions,
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

#[cfg(test)]
mod tests {
    use super::*;
    use tabit_config::AuthConfig;
    use tabit_session::{SessionHost, SessionHostWiring};

    /// The shared two-model test config.
    fn test_config() -> tabit_config::TabitConfig {
        tabit_config::TabitConfig::from_toml_str(
            r#"
default_model = { provider = "lmstudio", model = "m" }

[providers.lmstudio]
base_url = "http://127.0.0.1:1234/v1"
api = "openai-completions"
keyless = true

[[providers.lmstudio.models]]
id = "m"

[[providers.lmstudio.models]]
id = "m2"
"#,
            std::path::Path::new("providers.toml"),
        )
        .expect("test config")
    }

    fn named_tool(name: &'static str) -> tabit_engine::tool::DynamicTool {
        tabit_engine::tool::DynamicTool::new(
            name,
            "a test tool",
            serde_json::json!({"type": "object"}),
            move |_ctx, _args| {
                let output = name;
                Box::pin(async move { Ok(tabit_engine::tool::ToolOutput::text(output)) })
            },
        )
    }

    #[test]
    fn the_tool_filter_admits_allowed_and_not_denied() {
        let candidate = vec![named_tool("read"), named_tool("bash"), named_tool("echo")];
        let args = AppOptions {
            tools: Some("read,echo".to_string()),
            without: Some("echo".to_string()),
            ..AppOptions::default()
        };
        let (allow, deny) = tool_filter(&args);
        let kept = retain_filtered(candidate, &allow, &deny);
        assert_eq!(
            kept.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            vec!["read"],
            "allow first, then deny: the intersection survives"
        );
    }

    #[test]
    fn unknown_flag_names_match_nothing() {
        // Include/exclude-if-it-exists (owner ruling 2026-09-27): no
        // validation against the offered set — forwarded child lists
        // legitimately carry names the child does not offer, and an
        // allow that matches nothing is a tool-less session, a legal
        // shape (a chatbot).
        let candidate = || vec![named_tool("read"), named_tool("bash")];
        let args = AppOptions {
            tools: Some("read,typo".to_string()),
            ..AppOptions::default()
        };
        let (allow, deny) = tool_filter(&args);
        let kept = retain_filtered(candidate(), &allow, &deny);
        assert_eq!(
            kept.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            vec!["read"],
            "the unknown allow name matched nothing, without error"
        );

        let args = AppOptions {
            without: Some("typo,bash".to_string()),
            ..AppOptions::default()
        };
        let (allow, deny) = tool_filter(&args);
        let kept = retain_filtered(candidate(), &allow, &deny);
        assert_eq!(
            kept.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            vec!["read"],
            "the unknown deny name matched nothing, without error"
        );

        // The chatbot: an allow matching nothing is legal.
        let args = AppOptions {
            tools: Some("nothing-real".to_string()),
            ..AppOptions::default()
        };
        let (allow, deny) = tool_filter(&args);
        assert!(
            retain_filtered(candidate(), &allow, &deny).is_empty(),
            "a tool-less session is a legal shape"
        );
    }

    #[test]
    fn a_set_tabit_core_bin_never_overrides_self_reference() {
        // Pins the ruling: backend self-reference is current_exe, no
        // exceptions — a frontend's TABIT_CORE_BIN (stale or not) must
        // not diverge subagent children from the running image.
        // SAFETY: process-global state; no other test in this binary
        // reads the variable, and it is removed before the assertion.
        unsafe {
            std::env::set_var("TABIT_CORE_BIN", "a-stale-override");
        }
        let resolved = tabit_exe().expect("current_exe resolves in a test binary");
        unsafe {
            std::env::remove_var("TABIT_CORE_BIN");
        }
        assert_eq!(
            resolved,
            std::env::current_exe().expect("current_exe resolves in a test binary")
        );
    }

    #[test]
    fn enablement_partitions_the_scan_and_refusals_always_launch() {
        use std::collections::HashSet;
        use tabit_ext::manifest::{Discovered, Manifest};
        fn package(name: &str) -> Discovered {
            Discovered::Package {
                dir: PathBuf::from(format!("C:/ext/{name}")),
                manifest: Manifest {
                    name: name.to_string(),
                    version: "0.1.0".to_string(),
                    entry: Some(vec!["bin".to_string()]),
                    description: None,
                    requires: Vec::new(),
                    disables: Vec::new(),
                },
            }
        }
        fn static_package(name: &str, requires: &[&str]) -> Discovered {
            Discovered::Package {
                dir: PathBuf::from(format!("C:/ext/{name}")),
                manifest: Manifest {
                    name: name.to_string(),
                    version: "0.1.0".to_string(),
                    entry: None,
                    description: None,
                    requires: requires.iter().map(|r| r.to_string()).collect(),
                    disables: Vec::new(),
                },
            }
        }
        fn refused(dir: &str) -> Discovered {
            Discovered::Refused {
                dir: PathBuf::from(format!("C:/ext/{dir}")),
                reason: "invalid manifest".to_string(),
            }
        }
        let found = vec![
            package("alpha"),
            package("beta"),
            Discovered::Refused {
                dir: PathBuf::from("C:/ext/broken"),
                reason: "invalid manifest".to_string(),
            },
        ];
        let mut disabled = HashSet::new();
        disabled.insert("alpha".to_string());
        let launchable = partition(found, &disabled);
        // The disabled alpha is absent everywhere; beta mounts by
        // default (install was the consent) and reaches the
        // fragment/skills consumers; the refusal still reports (a
        // broken package is loud whatever the settings say).
        assert_eq!(launchable.packages.len(), 1);
        assert_eq!(launchable.packages[0].0, "beta");
        assert_eq!(launchable.found.len(), 2);
        assert!(launchable.found.iter().any(
            |found| matches!(found, Discovered::Package { manifest, .. } if manifest.name == "beta")
        ));
        assert!(
            launchable
                .found
                .iter()
                .any(|found| matches!(found, Discovered::Refused { .. }))
        );
        // Nothing disabled: everything launches (install was the
        // consent); the refusal still reports.
        let none = partition(
            vec![
                package("alpha"),
                Discovered::Refused {
                    dir: PathBuf::from("C:/ext/broken"),
                    reason: "invalid manifest".to_string(),
                },
            ],
            &HashSet::new(),
        );
        assert_eq!(none.packages.len(), 1, "mounted by default");
        assert_eq!(none.found.len(), 2);

        // Everything disabled: nothing launches but the refusal.
        let mut all = HashSet::new();
        all.insert("alpha".to_string());
        let none = partition(vec![package("alpha"), refused("broken")], &all);
        assert!(none.packages.is_empty());
        assert_eq!(none.found.len(), 1);

        // Static packages (task 6): mounted for scan facts and
        // requirement presence, never launched, never announced.
        let launchable = partition(
            vec![package("alpha"), static_package("bundle", &["alpha"])],
            &HashSet::new(),
        );
        assert_eq!(launchable.packages.len(), 2, "the static package mounts");
        assert_eq!(
            launchable.found.len(),
            1,
            "only the process package launches"
        );
        assert_eq!(
            launchable.found[0].manifest().map(|m| m.name.clone()),
            Some("alpha".to_string())
        );

        // An unmet requirement refuses — and the refusal announces
        // (a static dependent with an unmet requirement reports dead).
        let launchable = partition(vec![static_package("needy", &["ghost"])], &HashSet::new());
        assert!(
            launchable.packages.is_empty(),
            "the refused package does not mount"
        );
        assert_eq!(
            launchable.found.len(),
            1,
            "the refusal launches as a report"
        );
        match &launchable.found[0] {
            Discovered::Refused { reason, .. } => {
                assert!(reason.contains("requires extension `ghost`"), "{reason}");
            }
            _ => panic!("unmet requirements refuse"),
        }

        // A disabled requirement is unmet ("disabled is absent
        // everywhere").
        let mut disabled = HashSet::new();
        disabled.insert("there".to_string());
        let launchable = partition(
            vec![package("there"), static_package("needy", &["there"])],
            &disabled,
        );
        assert!(launchable.packages.is_empty());
    }

    #[test]
    fn a_broken_fragment_refuses_the_fragment_not_the_package() {
        let dir = std::env::temp_dir().join(format!("tabit-frag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("package dir");
        std::fs::write(dir.join("providers.toml"), "not toml").expect("broken fragment");
        let mut config = TabitConfig::default();
        let user_ids = std::collections::HashSet::new();
        let mut warnings = Vec::new();
        merge_fragment_into(&mut config, "broken", &dir, &user_ids, &mut warnings);
        assert!(config.providers.is_empty());
        assert_eq!(warnings.len(), 1);
        let warning = &warnings[0];
        assert!(warning.contains("providers fragment refused"), "{warning}");

        // A healthy fragment lands under the same call.
        std::fs::write(
            dir.join("providers.toml"),
            "[providers.relay]\nbase_url = \"http://127.0.0.1:8391/v1\"\napi = \"openai-completions\"\n",
        )
        .expect("fragment");
        let mut warnings = Vec::new();
        merge_fragment_into(&mut config, "broken", &dir, &user_ids, &mut warnings);
        assert!(config.provider("relay").is_some(), "the fragment landed");
        assert!(warnings.is_empty(), "{warnings:?}");
        let _ = std::fs::remove_dir_all(&dir);
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
keyless = true

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
        assert!(
            error.to_string().contains("usable model provider"),
            "{error}"
        );
    }

    #[test]
    fn continue_miss_is_loud_in_print_and_absorbed_in_json() {
        let dir = std::env::temp_dir().join(format!("tabit-assemble-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::new(&dir);
        let config = Arc::new(test_config());
        let auth = Arc::new(AuthConfig::default());
        let registry = ModelRegistry::new(config.clone(), auth.clone());
        let cont_print = AppOptions {
            continue_newest: true,
            ..AppOptions::default()
        };

        let error = match assemble(&cont_print, &registry, &store, ContinueMiss::Fail, None) {
            Err(error) => error,
            Ok(_) => panic!("print mode fails loudly on an empty store"),
        };
        assert!(error.contains("no sessions yet"), "{error}");

        let cont_json = AppOptions {
            continue_newest: true,
            ..AppOptions::default()
        };
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

    #[test]
    fn core_tools_mount_the_delegation_pair_in_every_role() {
        // Role-independent (owner ruling 2026-09-27): recursion is
        // the spawner's blacklist policy — the built-in subagent
        // tool denies `subagent`/`followup` in its children — never
        // a baked-in parent check.
        let names: Vec<String> = core_tools().iter().map(|t| t.name().to_string()).collect();
        assert!(
            names.contains(&"subagent".to_string()) && names.contains(&"followup".to_string()),
            "every process's candidate carries the delegation pair: {names:?}"
        );
    }

    #[test]
    fn host_data_creates_and_opens_through_the_assembly() {
        let dir = std::env::temp_dir().join(format!("tabit-hostdata-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::new(&dir);
        let registry = ModelRegistry::new(Arc::new(test_config()), Arc::new(AuthConfig::default()));
        let mounted = Arc::new(extensions::Mounted::none());
        let data = host_data(&AppOptions::default(), &registry, &store, &mounted);

        let (created, notes) = (data.create)().expect("the create closure builds");
        assert!(!created.resumed(), "a create is always fresh");
        assert!(notes.is_empty(), "a clean config degrades nothing");

        // A never-used session leaves no file (deferred creation — the
        // catalog's law), so the open closure cannot see it yet.
        // Materialize the file the real way: one run over the
        // dead-port provider — the user message commits at acceptance,
        // the run fails, the file exists.
        let id = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let wiring = SessionHostWiring {
                    node: host_node(),
                    store: store.clone(),
                    boot_parent: None,
                    boot_parent_call: None,
                };
                let mut handle = SessionHost::spawn(created, Vec::new(), wiring, data.clone());
                let id = handle.info().session_id.clone();
                handle.message(&id, "seed the file");
                handle.close_commands();
                while let Some(frame) = handle.next_event().await {
                    if matches!(frame.event, tabit_session::SessionEvent::RunFailed { .. }) {
                        break;
                    }
                }
                id
            });

        // The open closure resolves by the stored id and resumes the
        // same session.
        let (reopened, _) = (data.open)(&id).expect("the open closure resumes");
        assert!(reopened.resumed(), "an open resumes the stored file");
        assert_eq!(reopened.id(), id, "resume keeps the id");

        // An unknown id is the loud, named error.
        let error = match (data.open)("no-such-id") {
            Err(error) => error,
            Ok(_) => panic!("an unknown id is a loud error"),
        };
        assert!(
            error.contains("no stored session with id `no-such-id`"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ephemeral_assembly_boots_in_memory() {
        let dir = std::env::temp_dir().join(format!("tabit-ephemeral-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SessionStore::new(&dir);
        let registry = ModelRegistry::new(Arc::new(test_config()), Arc::new(AuthConfig::default()));
        let child_role = AppOptions {
            ephemeral: true,
            ..AppOptions::default()
        };
        let (session, _) = assemble(
            &child_role,
            &registry,
            &store,
            ContinueMiss::StartFresh,
            None,
        )
        .expect("the ephemeral boot assembles");
        assert!(
            session.path().is_none(),
            "the in-memory boot leaves no file"
        );
        assert!(!session.resumed());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
