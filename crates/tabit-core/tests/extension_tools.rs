//! Extension tools end to end (ROADMAP item 9, task 2): the REAL
//! `tabit --json` backend, a REAL installed extension package (the
//! `ext-double` behavior double), and a scripted model whose turn
//! calls the extension's tool — the proxy roundtrip proven across
//! three processes (backend, extension, mock provider), offline.

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

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use httpmock::MockServer;
use serde_json::json;
use tabit_protocol::{
    ClientFrame, EventFrame, PROTOCOL_VERSION, ServerControlFrame, ServerFrame, SessionCommand,
    SessionEvent, to_wire_line,
};

/// The line-read bound: real processes, real pipes — generous for a
/// loaded box, hard enough to catch hangs.
const BOUND: Duration = Duration::from_secs(30);

fn test_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::OnceLock<std::sync::Mutex<u32>> = std::sync::OnceLock::new();
    let n = {
        let counter = COUNTER.get_or_init(|| std::sync::Mutex::new(0));
        let mut n = counter.lock().expect("counter lock");
        *n += 1;
        *n
    };
    let dir = std::env::temp_dir().join(format!("tabit-ext-tools-tests/{tag}-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Stage one package: the behavior double under the given name.
/// Locate a workspace binary: `CARGO_BIN_EXE_*` is same-package
/// only, but every test exe runs from `<target>/<profile>/deps` —
/// one level up is where cargo puts the workspace's binaries. The
/// file is whatever the last build left there: a filtered
/// `cargo test -p tabit` does NOT rebuild other crates' bins, so
/// after editing an extension example, build it (or run the full
/// gate) before driving it from here.
fn workspace_bin(name: &str) -> PathBuf {
    std::env::current_exe()
        .expect("current exe")
        .parent()
        .and_then(|deps| deps.parent())
        .map(|dir| dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| panic!("workspace binary `{name}` not built — run the workspace gate"))
}

fn install_double(root: &Path, name: &str, behavior: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("package dir");
    let manifest = json!({
        "name": name,
        "version": "0.1.0",
        "description": "the behavior double",
        "entry": [workspace_bin("ext-double").display().to_string(), behavior],
    });
    std::fs::write(
        dir.join("tabit.json"),
        serde_json::to_string(&manifest).expect("manifest"),
    )
    .expect("manifest");
}

/// A chat-completions SSE answer: one text delta and the closer.
fn sse_text(text: &str) -> String {
    let first = json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk",
        "created": 0, "model": "m",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}],
    });
    let last = json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk",
        "created": 0, "model": "m",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
    });
    format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n")
}

/// A chat-completions SSE answer whose turn calls one tool.
fn sse_tool_call(id: &str, name: &str, arguments: &str) -> String {
    let first = json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk",
        "created": 0, "model": "m",
        "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
            "index": 0, "id": id, "type": "function",
            "function": {"name": name, "arguments": arguments},
        }]}, "finish_reason": null}],
    });
    let last = json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk",
        "created": 0, "model": "m",
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
    });
    format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n")
}

/// The real backend over real pipes: lines in, lines out, stderr
/// drained for the crash report.
struct Backend {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: Receiver<String>,
    /// Every frame ever read, in order — a later scan (the grammar
    /// e2e's `until`) can find a frame an earlier helper consumed.
    seen: Vec<ServerFrame>,
}

fn spawn_backend(stage: &Stage, extra_env: &[(&str, String)]) -> Backend {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tabit-core"));
    command
        .arg("--json")
        .arg("--ephemeral")
        .arg("--extensions")
        .arg(&stage.extensions)
        .current_dir(&stage.work)
        .env("TABIT_CONFIG", &stage.config)
        .env("TABIT_AUTH", &stage.auth)
        .env("TABIT_SETTINGS", &stage.settings)
        // A redirected home keeps the whole boot hermetic: the default
        // extension root, home-level skills and AGENTS.md discovery
        // all key on the home directory, and a developer's real
        // machine must never leak into (or receive writes from) a
        // test.
        .env("USERPROFILE", &stage.home)
        .env("HOME", &stage.home);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    finish_spawn(command)
}

/// The common spawn tail: piped stdio, stderr drained into the test
/// output (banners, extension reports, and any crash report must be
/// visible when a scenario hangs or dies — an undrained pipe also
/// blocks the backend once it fills), stdout as a line channel.
fn finish_spawn(mut command: Command) -> Backend {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tabit");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::BufRead as _;
            for line in BufReader::new(stderr).lines().by_ref().flatten() {
                eprintln!("[backend] {line}");
            }
        });
    }
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if !line.trim().is_empty() && tx.send(line).is_err() {
                break;
            }
        }
    });
    Backend {
        child,
        stdin,
        lines: rx,
        seen: Vec::new(),
    }
}

impl Backend {
    fn send(&mut self, line: &str) {
        writeln!(self.stdin, "{line}")
            .and_then(|()| self.stdin.flush())
            .expect("write to the backend");
    }

    /// The next parsed frame within the bound.
    fn next_frame(&mut self) -> ServerFrame {
        let line = match self.lines.recv_timeout(BOUND) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => panic!("no frame within the bound"),
            Err(RecvTimeoutError::Disconnected) => panic!("the backend closed its stdout"),
        };
        let frame: ServerFrame = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("unparseable frame {line}: {error}"));
        self.seen.push(frame.clone());
        frame
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Stage config/auth/settings/extension root/work dir for one
/// scenario. The staged packages land in the settings allowlist (the
/// enablement gate: a discovered package mounts only when named).
struct Stage {
    #[allow(dead_code)] // kept: the owner of the mock's lifetime
    server: MockServer,
    work: PathBuf,
    config: PathBuf,
    auth: PathBuf,
    settings: PathBuf,
    /// The redirected home every spawned backend boots under.
    home: PathBuf,
    extensions: PathBuf,
}

/// Write the settings disable list naming exactly `names` — the one
/// explicit act (everything else mounts by default).
fn write_disabled(path: &Path, names: &[&str]) {
    let list = names
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(path, format!("[extensions]\ndisabled = [{list}]\n")).expect("settings");
}

fn stage(tag: &str, behaviors: &[(&str, &str)]) -> Stage {
    let dir = test_dir(tag);
    let server = MockServer::start();
    let config = dir.join("providers.toml");
    std::fs::write(
        &config,
        format!(
            "[providers.p]\nbase_url = \"http://127.0.0.1:{}/v1\"\napi = \"openai-completions\"\nkeyless = true\n\n[[providers.p.models]]\nid = \"m\"\n",
            server.port()
        ),
    )
    .expect("config");
    let auth = dir.join("auth.toml");
    std::fs::write(&auth, "providers = {}\n").expect("auth");
    let extensions = dir.join("extensions");
    std::fs::create_dir_all(&extensions).expect("extensions root");
    for (name, behavior) in behaviors {
        install_double(&extensions, name, behavior);
    }
    // No settings file: the default world — packages mount by
    // default; tests that disable write the list themselves.
    let settings = dir.join("settings.toml");
    let home = dir.join("home");
    std::fs::create_dir_all(&home).expect("redirected home");
    let work = dir.join("work");
    std::fs::create_dir_all(&work).expect("work");
    Stage {
        server,
        work,
        config,
        auth,
        settings,
        home,
        extensions,
    }
}

/// Stage a scripted multi-turn model: each turn's mock matches its
/// own marker AND excludes every later turn's — the user text (and
/// every earlier result) rides in request history forever, so only
/// mutual exclusion makes "turn N" expressible. The trap this
/// encodes cost two debugging rounds before it became a helper:
/// without the excludes, turn 1's mock matches every later request
/// and the model loops to its turn limit with no error anywhere.
fn scripted_turns(stage: &Stage, turns: &[(String, String)]) {
    for (index, (needle, body)) in turns.iter().enumerate() {
        let later: Vec<String> = turns[index + 1..]
            .iter()
            .map(|(needle, _)| needle.clone())
            .collect();
        stage.server.mock(move |when, then| {
            let mut when = when
                .method(httpmock::Method::POST)
                .path("/v1/chat/completions")
                .body_includes(needle.clone());
            for later in &later {
                when = when.body_excludes(later.clone());
            }
            then.status(200)
                .header("Content-Type", "text/event-stream")
                .body(body.clone());
        });
    }
}

/// Drive the handshake and hand back the session id, the extensions
/// announcement, and the skills announcement (when one arrives).
fn handshake(
    backend: &mut Backend,
) -> (
    String,
    tabit_protocol::ExtensionsCatalog,
    Option<Vec<tabit_protocol::AvailableSkill>>,
) {
    backend.send(&to_wire_line(&ClientFrame::Initialize {
        protocol_version: PROTOCOL_VERSION,
        replay: false,
    }));
    let mut session_id = None;
    let mut catalog = None;
    let mut skills = None;
    loop {
        match backend.next_frame() {
            ServerFrame::Control(ServerControlFrame::InitializeAck { session_id: id, .. }) => {
                session_id = Some(id);
            }
            ServerFrame::Event(frame) => match frame.event {
                SessionEvent::ExtensionsAvailable {
                    extensions,
                    conflicts,
                } => {
                    catalog = Some(tabit_protocol::ExtensionsCatalog {
                        extensions,
                        conflicts,
                    });
                }
                SessionEvent::SkillsAvailable { skills: found } => skills = Some(found),
                SessionEvent::RunFailed { message, .. } => {
                    panic!("the run failed: {message}");
                }
                _ => {}
            },
            ServerFrame::Control(other) => panic!("unexpected control frame: {other:?}"),
        }
        if session_id.is_some() && catalog.is_some() {
            return (session_id.take().unwrap(), catalog.take().unwrap(), skills);
        }
    }
}

#[test]
fn a_model_call_runs_an_extension_tool_and_the_result_feeds_back() {
    let stage = stage("proxy-run", &[("echoer", "tools-echo")]);
    // Turn 1: the model calls the extension's tool; turn 2: wrap up.
    // The second request's body must contain the tool result — the
    // proxy roundtrip's output re-entering the model context.
    scripted_turns(
        &stage,
        &[
            (
                "call the echo tool 7f3a".to_string(),
                sse_tool_call("call-1", "echo", r#"{"text":"from the model"}"#),
            ),
            (
                "EXT-ECHOED:from the model".to_string(),
                sse_text("all done"),
            ),
        ],
    );

    let mut backend = spawn_backend(&stage, &[]);
    let (session, catalog, _skills) = handshake(&mut backend);
    assert_eq!(catalog.extensions.len(), 1);
    let extension = &catalog.extensions[0];
    assert_eq!(extension.name, "echoer");
    assert_eq!(extension.status, "alive");
    assert_eq!(extension.tools.len(), 1);
    assert_eq!(extension.tools[0].name, "echo");
    assert!(catalog.conflicts.is_empty());

    backend.send(&to_wire_line(&SessionCommand::Message {
        session: session.clone(),
        text: "call the echo tool 7f3a".to_string(),
    }));
    loop {
        match backend.next_frame() {
            ServerFrame::Event(frame) => match frame.event {
                SessionEvent::ToolResult {
                    content, status, ..
                } => {
                    let success = matches!(status, tabit_protocol::ToolResultStatus::Success);
                    assert!(success, "the extension tool call succeeds: {content}");
                    assert!(content.contains("EXT-ECHOED:from the model"), "{content}");
                }
                SessionEvent::RunFinished { output, .. } => {
                    assert_eq!(output, "all done");
                    return;
                }
                SessionEvent::RunFailed { message, .. } => panic!("the run failed: {message}"),
                _ => {}
            },
            ServerFrame::Control(control) => panic!("unexpected control frame: {control:?}"),
        }
    }
}

#[test]
fn a_core_name_conflict_is_reported_on_the_channel() {
    let stage = stage("shadow", &[("shadow", "tools-shadow")]);
    let mut backend = spawn_backend(&stage, &[]);
    let (_session, catalog, _skills) = handshake(&mut backend);
    assert_eq!(catalog.conflicts.len(), 1);
    let conflict = &catalog.conflicts[0];
    assert!(matches!(
        conflict.kind,
        tabit_protocol::ExtensionConflictKind::ReplacesCore
    ));
    assert_eq!(conflict.extension, "shadow");
    assert_eq!(conflict.tool, "read");
}

// ── task 4: enablement, skills mounts, providers fragments ─────────

/// A disabled package is the user's setting, not a failure: it boots
/// nowhere — no catalog announcement, no launch — while the backend
/// itself runs a normal turn.
#[test]
fn a_package_on_the_disable_list_mounts_nowhere() {
    let stage = stage("disabled", &[("echoer", "tools-echo")]);
    write_disabled(&stage.settings, &["echoer"]);
    scripted_turns(
        &stage,
        &[("solo-run-5c11".to_string(), sse_text("ran alone"))],
    );

    let mut backend = spawn_backend(&stage, &[]);
    backend.send(&to_wire_line(&ClientFrame::Initialize {
        protocol_version: PROTOCOL_VERSION,
        replay: false,
    }));
    let mut session = None;
    loop {
        match backend.next_frame() {
            ServerFrame::Control(ServerControlFrame::InitializeAck { session_id: id, .. }) => {
                session = Some(id);
            }
            ServerFrame::Event(frame) => match frame.event {
                // The one assertion: the disabled package announces
                // nothing, anywhere between boot and run end.
                SessionEvent::ExtensionsAvailable { .. } => {
                    panic!("a disabled package must not announce")
                }
                SessionEvent::RunFinished { output, .. } => {
                    assert_eq!(output, "ran alone");
                    return;
                }
                SessionEvent::RunFailed { message, .. } => panic!("the run failed: {message}"),
                _ => {}
            },
            ServerFrame::Control(control) => panic!("unexpected control frame: {control:?}"),
        }
        if let Some(session) = session.take() {
            backend.send(&to_wire_line(&SessionCommand::Message {
                session,
                text: "solo-run-5c11".to_string(),
            }));
        }
    }
}

/// The static skill-shipping package (task 6's shape): NO entry, no
/// process, no announcement — its `skills/` directory folds into the
/// in-memory tables, the ordinary discovery announces the skill at
/// its original package path, and the catalog carries nothing for
/// the package itself.
#[test]
fn a_static_skills_package_mounts_without_a_process_or_announcement() {
    let stage = stage("skills", &[("echoer", "tools-echo")]);
    let package = stage.extensions.join("skillship");
    std::fs::create_dir_all(package.join("skills/skillship-demo")).expect("skill dir");
    std::fs::write(
        package.join("skills/skillship-demo/SKILL.md"),
        "---\nname: skillship-demo\ndescription: proves extension-shipped skills mount\n---\n# The shipped body\n",
    )
    .expect("SKILL.md");
    std::fs::write(
        package.join("tabit.json"),
        serde_json::to_string(&json!({
            "name": "skillship",
            "version": "0.1.0",
            "description": "the static skills-only package",
        }))
        .expect("manifest"),
    )
    .expect("manifest");
    scripted_turns(&stage, &[("never-called".to_string(), sse_text("done"))]);

    let mut backend = spawn_backend(&stage, &[]);
    let (_session, catalog, skills) = handshake(&mut backend);
    // The process package announces; the static one does not — the
    // catalog carries exactly the echoer.
    assert_eq!(catalog.extensions.len(), 1, "{:?}", catalog.extensions);
    assert_eq!(catalog.extensions[0].name, "echoer");

    // The extension walker's entries ride the ordinary skills
    // announcement with their ORIGINAL package paths — in-memory
    // tables, no filesystem mount, no process; the location is the
    // provenance, and it reads like any discovered skill.
    let skills = skills.expect("skills_available arrived");
    let skill = skills
        .iter()
        .find(|skill| skill.name == "skillship-demo")
        .expect("the shipped skill is discovered");
    assert!(
        skill.location.contains("extensions") && skill.location.contains("skillship"),
        "the package's real path: {}",
        skill.location
    );
    let body = std::fs::read_to_string(&skill.location).expect("the entry's path reads");
    assert!(body.contains("The shipped body"), "{body}");
}

/// The task-6 load rule: a package whose `requires` names something
/// not in the mounted set refuses at the scan and reports dead with
/// the reason — presence, not liveness.
#[test]
fn an_unmet_requirement_refuses_at_boot_with_its_reason() {
    let stage = stage("unmet", &[("echoer", "tools-echo")]);
    let package = stage.extensions.join("needy");
    std::fs::create_dir_all(&package).expect("dir");
    std::fs::write(
        package.join("tabit.json"),
        serde_json::to_string(&json!({
            "name": "needy",
            "version": "0.1.0",
            "entry": [workspace_bin("ext-double").display().to_string(), "hello"],
            "requires": ["ghost"],
        }))
        .expect("manifest"),
    )
    .expect("manifest");
    scripted_turns(&stage, &[("never".to_string(), sse_text("done"))]);

    let mut backend = spawn_backend(&stage, &[]);
    let (_session, catalog, _skills) = handshake(&mut backend);
    let needy = catalog
        .extensions
        .iter()
        .find(|extension| extension.name == "needy")
        .expect("the refusal announces");
    assert_eq!(needy.status, "dead");
    let reason = needy.reason.as_deref().expect("the reason carries");
    assert!(reason.contains("requires extension `ghost`"), "{reason}");
    // The healthy sibling is untouched.
    let echoer = catalog
        .extensions
        .iter()
        .find(|extension| extension.name == "echoer")
        .expect("the sibling stands");
    assert_eq!(echoer.status, "alive");
}

/// The task-6 journey: the REAL `tabit install path:<dir>` places a
/// package (into the redirected home's default root), then the REAL
/// backend boots over that root and serves it.
#[test]
fn install_path_then_boot_serves_the_package() {
    let dir = test_dir("install-journey");
    // The source package: the double, in its own directory.
    let source = dir.join("source/echoer");
    std::fs::create_dir_all(&source).expect("dir");
    std::fs::write(
        source.join("tabit.json"),
        serde_json::to_string(&json!({
            "name": "echoer",
            "version": "0.1.0",
            "entry": [workspace_bin("ext-double").display().to_string(), "tools-echo"],
        }))
        .expect("manifest"),
    )
    .expect("manifest");

    // The install: redirected home, so the default root is ours.
    let home = dir.join("home");
    std::fs::create_dir_all(&home).expect("home");
    let install = std::process::Command::new(env!("CARGO_BIN_EXE_tabit-core"))
        .arg("install")
        .arg(format!("path:{}", source.display()))
        .env("USERPROFILE", &home)
        .env("HOME", &home)
        .output()
        .expect("run tabit install");
    assert!(
        install.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&install.stdout),
        String::from_utf8_lossy(&install.stderr)
    );
    let root = home.join(".tabit").join("extensions");
    assert!(root.join("echoer/tabit.json").is_file(), "placed");

    // The boot over the installed root.
    let server = MockServer::start();
    let work = dir.join("work");
    std::fs::create_dir_all(&work).expect("work");
    let config = dir.join("providers.toml");
    std::fs::write(
        &config,
        format!(
            "[providers.p]\nbase_url = \"http://127.0.0.1:{}/v1\"\napi = \"openai-completions\"\nkeyless = true\n\n[[providers.p.models]]\nid = \"m\"\n",
            server.port()
        ),
    )
    .expect("config");
    server.mock(move |when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/chat/completions");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_text("done"));
    });
    let mut backend = spawn_raw(&work, &root, &config, &home);
    let (_session, catalog, _skills) = handshake(&mut backend);
    let echoer = catalog
        .extensions
        .iter()
        .find(|extension| extension.name == "echoer")
        .expect("the installed package mounts");
    assert_eq!(echoer.status, "alive");
}

/// The raw backend spawn the journey test needs: an arbitrary root
/// and config, not the stage helper's own.
fn spawn_raw(work: &Path, extensions_root: &Path, config: &Path, home: &Path) -> Backend {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tabit-core"));
    command
        .arg("--json")
        .arg("--ephemeral")
        .arg("--extensions")
        .arg(extensions_root)
        .current_dir(work)
        .env("TABIT_CONFIG", config)
        .env("TABIT_AUTH", config.with_file_name("auth.toml"))
        .env("USERPROFILE", home)
        .env("HOME", home);
    finish_spawn(command)
}

/// The provider-relay package: its `providers.toml` fragment merges
/// into an empty user config (the fragment is the only provider), the
/// model call rides the relay, and the relay translates to LM Studio's
/// native REST API — four processes: backend, relay, native mock.

#[test]
fn a_providers_fragment_relays_a_model_call_over_the_native_api() {
    let stage = stage("relay", &[]);
    // An empty user config: everything the backend knows about
    // providers comes from the fragment.
    std::fs::write(&stage.config, "").expect("empty user config");

    // A free port for the relay to listen on.
    let relay_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        port
    };
    let package = stage.extensions.join("lmstudio");
    std::fs::create_dir_all(&package).expect("package dir");
    std::fs::write(
        package.join("tabit.json"),
        serde_json::to_string(&json!({
            "name": "lmstudio",
            "version": "0.1.0",
            "description": "LM Studio behind its native REST API",
            "entry": [workspace_bin("lmstudio-ext").display().to_string()],
        }))
        .expect("manifest"),
    )
    .expect("manifest");
    std::fs::write(
        package.join("providers.toml"),
        format!(
            "[providers.lmstudio-relay]\nbase_url = \"http://127.0.0.1:{relay_port}/v1\"\napi = \"openai-completions\"\nkeyless = true\n\n[[providers.lmstudio-relay.models]]\nid = \"local-model\"\n"
        ),
    )
    .expect("fragment");

    // LM Studio's native answer (the mock plays the native API — the
    // whole point is that tabit never speaks it directly).
    let native_answer = json!({
        "choices": [{
            "message": {"role": "assistant", "content": "relayed answer"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 4, "completion_tokens": 3, "total_tokens": 7},
    });
    let upstream = format!("http://127.0.0.1:{}", stage.server.port());
    // The first-turn mock matches its own marker AND excludes the
    // second turn's — the user text rides in history forever, so only
    // the exclusion keeps "turn 2" expressible (the scripted-turns
    // lesson, on the native path). No second mock exists: turn 2 is
    // the unmatched-native call (the mock's 404), the failure beat.
    stage.server.mock(move |when, then| {
        when.method(httpmock::Method::POST)
            .path("/api/v0/chat/completions")
            .body_includes("relay-check-6d77")
            .body_excludes("failure-beat-2c91");
        then.status(200).json_body(native_answer);
    });

    let mut backend = spawn_backend(
        &stage,
        &[
            ("TABIT_LMSTUDIO_RELAY_PORT", relay_port.to_string()),
            ("TABIT_LMSTUDIO_URL", upstream),
        ],
    );
    let (session, catalog, _skills) = handshake(&mut backend);
    let extension = catalog
        .extensions
        .iter()
        .find(|extension| extension.name == "lmstudio")
        .expect("the relay package is announced");
    assert_eq!(extension.status, "alive");

    let session_id = session.clone();
    backend.send(&to_wire_line(&SessionCommand::Message {
        session,
        text: "relay-check-6d77".to_string(),
    }));
    let mut failure_beat_sent = false;
    loop {
        match backend.next_frame() {
            ServerFrame::Event(frame) => match frame.event {
                SessionEvent::RunFinished { output, .. } => {
                    assert_eq!(output, "relayed answer");
                    // Beat 2, sent once: the unmatched native call — the
                    // mock answers 404, the relay reports the non-success
                    // upstream, and the failure rides the provider path
                    // as an ordinary run failure.
                    assert!(!failure_beat_sent, "one success, then the failure beat");
                    failure_beat_sent = true;
                    backend.send(&to_wire_line(&SessionCommand::Message {
                        session: session_id.clone(),
                        text: "failure-beat-2c91".to_string(),
                    }));
                }
                SessionEvent::RunFailed { message, .. } => {
                    assert!(failure_beat_sent, "beat 1 must succeed first: {message}");
                    assert!(message.contains("LM Studio answered"), "{message}");
                    return;
                }
                _ => {}
            },
            ServerFrame::Control(control) => panic!("unexpected control frame: {control:?}"),
        }
    }
}

/// The relay's external failures stay graceful and model-visible: an
/// unreachable upstream answers 502, which rides the provider path as
/// an ordinary run failure — never a hang, never a crash.
#[test]
fn an_unreachable_upstream_fails_the_run_through_the_relay() {
    let stage = stage("relay-down", &[]);
    std::fs::write(&stage.config, "").expect("empty user config");
    let relay_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        port
    };
    let package = stage.extensions.join("lmstudio");
    std::fs::create_dir_all(&package).expect("package dir");
    std::fs::write(
        package.join("tabit.json"),
        serde_json::to_string(&json!({
            "name": "lmstudio",
            "version": "0.1.0",
            "entry": [workspace_bin("lmstudio-ext").display().to_string()],
        }))
        .expect("manifest"),
    )
    .expect("manifest");
    std::fs::write(
        package.join("providers.toml"),
        format!(
            "[providers.lmstudio-relay]\nbase_url = \"http://127.0.0.1:{relay_port}/v1\"\napi = \"openai-completions\"\nkeyless = true\n\n[[providers.lmstudio-relay.models]]\nid = \"local-model\"\n"
        ),
    )
    .expect("fragment");

    // A port nothing listens on: the relay is up (its fragment
    // merged, the package alive) but LM Studio is not.
    let dead_upstream = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    };
    let mut backend = spawn_backend(
        &stage,
        &[
            ("TABIT_LMSTUDIO_RELAY_PORT", relay_port.to_string()),
            ("TABIT_LMSTUDIO_URL", dead_upstream),
        ],
    );
    let (session, _catalog, _skills) = handshake(&mut backend);
    backend.send(&to_wire_line(&SessionCommand::Message {
        session,
        text: "any prompt".to_string(),
    }));
    loop {
        match backend.next_frame() {
            ServerFrame::Event(frame) => {
                if let SessionEvent::RunFailed { message, .. } = frame.event {
                    // The failure is model-visible and names the
                    // upstream — whichever arm fired (unreachable, or
                    // answered-with-an-error); the wording depends on
                    // the machine's network stack.
                    assert!(message.contains("LM Studio"), "{message}");
                    return;
                }
            }
            ServerFrame::Control(control) => panic!("unexpected control frame: {control:?}"),
        }
    }
}

/// Task 5 over the real wire: the model calls the extension's
/// `summarize` tool, the extension's `model_prompt` (envelope verb
/// one, hand-rolled by the double) makes a SECOND provider call
/// through the session's own model, and the completion's text feeds
/// the next turn — four processes: backend, extension, provider mock
/// (twice: the run's turns and the bare prompt).
#[test]
fn a_model_prompt_round_trips_through_the_session() {
    let stage = stage("model-prompt", &[("modeler", "tools-model")]);
    scripted_turns(
        &stage,
        &[
            (
                "model-prompt-check-5e21".to_string(),
                sse_tool_call(
                    "call-1",
                    "summarize",
                    r#"{"text":"a long session summary"}"#,
                ),
            ),
            (
                "EXT-MODELED:the five word answer".to_string(),
                sse_text("all done"),
            ),
        ],
    );
    // The bare prompt's own request: no history rides (the completion
    // is standalone), so its marker is naturally exclusive.
    stage.server.mock(move |when, then| {
        when.method(httpmock::Method::POST)
            .path("/v1/chat/completions")
            .body_includes("summarize this in five words");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_text("the five word answer"));
    });

    let mut backend = spawn_backend(&stage, &[]);
    let (session, catalog, _skills) = handshake(&mut backend);
    let extension = catalog
        .extensions
        .iter()
        .find(|extension| extension.name == "modeler")
        .expect("the package mounts by default and announces");
    assert_eq!(extension.status, "alive");
    assert_eq!(extension.tools.len(), 1);

    backend.send(&to_wire_line(&SessionCommand::Message {
        session,
        text: "model-prompt-check-5e21".to_string(),
    }));
    loop {
        match backend.next_frame() {
            ServerFrame::Event(frame) => match frame.event {
                SessionEvent::ToolResult {
                    name,
                    content,
                    details,
                    status,
                    ..
                } => {
                    assert_eq!(name, "summarize");
                    assert!(matches!(status, tabit_protocol::ToolResultStatus::Success));
                    assert!(
                        content.contains("EXT-MODELED:the five word answer"),
                        "{content}"
                    );
                    // The verb's usage rides the details — the
                    // attribution the extension sees.
                    let usage = details.expect("details carry the usage");
                    assert_eq!(usage["usage"]["total_tokens"], 5, "{usage}");
                }
                SessionEvent::RunFinished { output, .. } => {
                    assert_eq!(output, "all done");
                    return;
                }
                SessionEvent::RunFailed { message, .. } => panic!("the run failed: {message}"),
                _ => {}
            },
            ServerFrame::Control(control) => panic!("unexpected control frame: {control:?}"),
        }
    }
}

// ── the built-in permission gate (pi-sanity's policy, in-process) ──

/// The empty-stage handshake: `extensions_available` never announces
/// with nothing installed, so `handshake` would wait out its bound —
/// the ack alone carries the boot session id.
fn handshake_bare(backend: &mut Backend) -> String {
    backend.send(&to_wire_line(&ClientFrame::Initialize {
        protocol_version: PROTOCOL_VERSION,
        replay: false,
    }));
    loop {
        match backend.next_frame() {
            ServerFrame::Control(ServerControlFrame::InitializeAck { session_id, .. }) => {
                return session_id;
            }
            ServerFrame::Control(other) => panic!("unexpected control frame: {other:?}"),
            ServerFrame::Event(_) => {}
        }
    }
}

/// The gate mounts by default: a command the shipped rules ASK on
/// (`git push --force` — the force-push flag rule) opens one card on
/// the ordinary frontend wire; a Block answer (with free text) skips
/// the call in-band and the model is told — the run continues and
/// wraps up on the scripted second turn.
#[test]
fn the_builtin_gate_asks_on_a_risky_bash_and_a_block_skips() {
    let stage = stage("gate-ask", &[]);
    scripted_turns(
        &stage,
        &[
            (
                "gate-check-9a31".to_string(),
                sse_tool_call("call-1", "bash", r#"{"command":"git push --force"}"#),
            ),
            ("not today".to_string(), sse_text("understood")),
        ],
    );

    let mut backend = spawn_backend(&stage, &[]);
    let session = handshake_bare(&mut backend);
    backend.send(&to_wire_line(&SessionCommand::Message {
        session: session.clone(),
        text: "gate-check-9a31".to_string(),
    }));

    loop {
        match backend.next_frame() {
            ServerFrame::Event(frame) => match frame.event {
                SessionEvent::InteractionRequest { id, payload, .. } => {
                    // The gate's card: the force-push reason and the
                    // command details, over the ordinary ask lane.
                    assert_eq!(payload["title"], "Force push rewrites history");
                    assert!(
                        payload["body"].to_string().contains("git push"),
                        "the card shows the checked command: {payload}"
                    );
                    backend.send(&to_wire_line(&SessionCommand::InteractionResponse {
                        session: Some(session.clone()),
                        id,
                        payload: json!({
                            "selected": ["Block"], "text": "not today",
                        }),
                    }));
                }
                SessionEvent::ToolResult { name, content, .. } => {
                    assert_eq!(name, "bash");
                    assert!(content.contains("permission gate"), "{content}");
                    assert!(content.contains("not today"), "{content}");
                    assert!(content.contains("did not run"), "{content}");
                }
                SessionEvent::RunFinished { output, .. } => {
                    assert_eq!(output, "understood");
                    return;
                }
                SessionEvent::RunFailed { message, .. } => panic!("the run failed: {message}"),
                _ => {}
            },
            ServerFrame::Control(control) => panic!("unexpected control frame: {control:?}"),
        }
    }
}

/// The opt-out: `[gate] enabled = false` in settings.toml unmounts
/// the gate — the same risky command runs straight through to the
/// real shell (here: git failing in a non-repo — the point is the
/// absence of a card and of a skip, not git's exit).
#[test]
fn a_disabled_gate_mounts_nowhere() {
    let stage = stage("gate-off", &[]);
    std::fs::write(&stage.settings, "[gate]\nenabled = false\n").expect("settings");
    // Turn 2's needle is a fragment of the REAL bash output (git's
    // non-repo error) — the ungated run's tool result is what tells
    // turn 2 apart from turn 1, whose mock excludes it.
    scripted_turns(
        &stage,
        &[
            (
                "gate-off-check-4c22".to_string(),
                sse_tool_call("call-1", "bash", r#"{"command":"git push --force"}"#),
            ),
            ("not a git repository".to_string(), sse_text("done")),
        ],
    );

    let mut backend = spawn_backend(&stage, &[]);
    let session = handshake_bare(&mut backend);
    backend.send(&to_wire_line(&SessionCommand::Message {
        session: session.clone(),
        text: "gate-off-check-4c22".to_string(),
    }));

    loop {
        match backend.next_frame() {
            ServerFrame::Event(frame) => match frame.event {
                SessionEvent::InteractionRequest { .. } => {
                    panic!("a disabled gate opens no card")
                }
                SessionEvent::ToolResult { name, content, .. } => {
                    assert_eq!(name, "bash");
                    assert!(
                        content.contains("not a git repository"),
                        "the real git ran and failed in the non-repo: {content}"
                    );
                    assert!(
                        !content.contains("permission gate"),
                        "nothing skipped the call: {content}"
                    );
                }
                SessionEvent::RunFinished { output, .. } => {
                    assert_eq!(output, "done");
                    return;
                }
                SessionEvent::RunFailed { message, .. } => panic!("the run failed: {message}"),
                _ => {}
            },
            ServerFrame::Control(control) => panic!("unexpected control frame: {control:?}"),
        }
    }
}

/// The routing generalization over the real json edge: the grammar
/// double's emissions surface origin-stamped at the frontend, the
/// watched boot announcement mirrors onto its pipe (it echoes the
/// line back out), and the frontend's answer routes to the extension
/// by id — with the settlement announced and the routed response
/// crossing back down the pipe (mirrored out again by the double).
#[test]
fn the_shared_grammar_crosses_the_json_edge_end_to_end() {
    let stage = stage("grammar-e2e", &[("grammar-ext", "grammar")]);
    let mut backend = spawn_backend(&stage, &[]);
    let (session, _catalog, _skills) = handshake(&mut backend);

    // Frames until a predicate holds, bounded — the emissions race
    // the handshake, so scan rather than assume order.
    fn until<F: Fn(&EventFrame) -> bool>(backend: &mut Backend, want: &str, pred: F) -> EventFrame {
        // Already-read frames first: the double's reactive speech can
        // land inside the handshake's own scan, and must not be lost
        // to the helper that happened to read it.
        if let Some(frame) = backend.seen.iter().rev().find_map(|frame| match frame {
            ServerFrame::Event(frame) if pred(frame) => Some(frame.clone()),
            _ => None,
        }) {
            return frame;
        }
        let deadline = std::time::Instant::now() + BOUND;
        while std::time::Instant::now() < deadline {
            let frame = match backend.next_frame() {
                ServerFrame::Event(frame) => frame,
                other => panic!("unexpected frame while waiting for {want}: {other:?}"),
            };
            if pred(&frame) {
                return frame;
            }
        }
        panic!("no {want} within the bound");
    }

    // The ask surfaced: backend-level (no stream), origin-stamped.
    let ask = until(
        &mut backend,
        "origin-stamped interaction_request",
        |frame| {
            matches!(
                &frame.event,
                SessionEvent::InteractionRequest { id, .. } if id == "g-1"
            ) && frame.stream.is_none()
                && frame.origin.as_deref() == Some("grammar-ext")
        },
    );
    assert!(ask.origin.as_deref() == Some("grammar-ext"));

    // The watch mirror round-tripped: the double watched
    // `session_opened`, the forwarder mirrored the boot's
    // announcement, and the double echoed the line back out as an
    // origin-stamped error event.
    until(
        &mut backend,
        "the mirrored session_opened echoed back",
        |frame| {
            matches!(&frame.event, SessionEvent::Error { message, .. } if message
                .contains("session_opened"))
                && frame.origin.as_deref() == Some("grammar-ext")
        },
    );

    // The answer routes by id — the session it names is irrelevant
    // (id-first dispatch), the settlement is announced, and the
    // routed response crosses back down the pipe (the double mirrors
    // it out).
    backend.send(&to_wire_line(&SessionCommand::InteractionResponse {
        session: Some(session),
        id: "g-1".to_string(),
        payload: json!({"selected": [], "text": "go ahead from the frontend"}),
    }));
    until(
        &mut backend,
        "the settlement announced",
        |frame| matches!(&frame.event, SessionEvent::InteractionSettled { id } if id == "g-1"),
    );
    until(
        &mut backend,
        "the routed answer echoed back from the extension",
        |frame| {
            matches!(&frame.event, SessionEvent::Error { message, .. } if message
                .contains("interaction_response")
                && message.contains("go ahead from the frontend"))
                && frame.origin.as_deref() == Some("grammar-ext")
        },
    );
}
