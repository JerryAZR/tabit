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
    ClientFrame, PROTOCOL_VERSION, ServerControlFrame, ServerFrame, SessionCommand, SessionEvent,
    to_wire_line,
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
}

fn spawn_backend(stage: &Stage, extra_env: &[(&str, String)]) -> Backend {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tabit"));
    command
        .arg("--json")
        .arg("--ephemeral")
        .arg("--extensions")
        .arg(&stage.extensions)
        .current_dir(&stage.work)
        .env("TABIT_CONFIG", &stage.config)
        .env("TABIT_AUTH", &stage.auth)
        .env("TABIT_SETTINGS", &stage.settings)
        // A redirected home keeps the whole boot hermetic: the
        // extension skills mounts, the prompt's home-level AGENTS.md,
        // and the default roots all key on the home directory, and a
        // developer's real machine must never leak into (or receive
        // writes from) a test.
        .env("USERPROFILE", &stage.home)
        .env("HOME", &stage.home)
        // Localhost never proxies: a developer machine's system proxy
        // (which reqwest reads by default) would otherwise sit between
        // the engine and the local provider mocks — a hop the tests
        // never chose, and one that misreports a dead local port as
        // the proxy's own 502.
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tabit");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    // Drain the backend's stderr into the test output: banners,
    // extension reports, and any crash report must be visible when a
    // scenario hangs or dies (an undrained pipe also blocks the
    // backend once it fills).
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
        serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("unparseable frame {line}: {error}"))
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

/// Write the settings allowlist naming exactly `names`.
fn write_settings(path: &Path, names: &[&str]) {
    let list = names
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(path, format!("[extensions]\nenabled = [{list}]\n")).expect("settings");
}

fn stage(tag: &str, behaviors: &[(&str, &str)]) -> Stage {
    let dir = test_dir(tag);
    let server = MockServer::start();
    let config = dir.join("providers.toml");
    std::fs::write(
        &config,
        format!(
            "[providers.p]\nbase_url = \"http://127.0.0.1:{}/v1\"\napi = \"openai-completions\"\n\n[[providers.p.models]]\nid = \"m\"\n",
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
    let settings = dir.join("settings.toml");
    write_settings(
        &settings,
        &behaviors.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
    );
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
                SessionEvent::RunFailed { message } => {
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
                SessionEvent::RunFailed { message } => panic!("the run failed: {message}"),
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

#[test]
fn the_gate_extension_gates_a_model_bash_call_over_the_wire() {
    let stage = stage("gate-e2e", &[]);
    // The gate package: the SDK-built permission policy, installed
    // like any extension — and enabled like any extension.
    write_settings(&stage.settings, &["gate"]);
    {
        let dir = stage.extensions.join("gate");
        std::fs::create_dir_all(&dir).expect("gate dir");
        let manifest = serde_json::json!({
            "name": "gate",
            "version": "0.1.0",
            "description": "the permission gate, moved out of core",
            "entry": [workspace_bin("gate-ext").display().to_string()],
        });
        std::fs::write(
            dir.join("tabit.json"),
            serde_json::to_string(&manifest).expect("manifest"),
        )
        .expect("manifest");
    }
    // Turn 1: the model calls bash; the gate opens a card; turn 2:
    // wrap up once the denial is in history.
    scripted_turns(
        &stage,
        &[
            (
                "gating-check-7f4b".to_string(),
                sse_tool_call("call-1", "bash", r#"{"command":"echo gated"}"#),
            ),
            ("not today".to_string(), sse_text("understood")),
        ],
    );

    let mut backend = spawn_backend(&stage, &[]);
    let (session, catalog, _skills) = handshake(&mut backend);
    // The catalog carries the gate's subscription.
    let gate = catalog
        .extensions
        .iter()
        .find(|extension| extension.name == "gate")
        .expect("the gate is in the catalog");
    assert_eq!(gate.status, "alive");
    assert_eq!(gate.hooks, vec!["tool_call".to_string()]);

    backend.send(&to_wire_line(&SessionCommand::Message {
        session: session.clone(),
        text: "gating-check-7f4b".to_string(),
    }));
    loop {
        match backend.next_frame() {
            ServerFrame::Event(frame) => match frame.event {
                SessionEvent::InteractionRequest { id, payload, .. } => {
                    // The gate's card, over the ordinary frontend wire.
                    assert_eq!(payload["title"], "Allow `bash` to run?");
                    backend.send(&to_wire_line(&SessionCommand::InteractionResponse {
                        session: session.clone(),
                        id,
                        payload: serde_json::json!({
                            "selected": ["Deny"], "text": "not today",
                        }),
                    }));
                }
                SessionEvent::ToolResult { name, content, .. } => {
                    assert_eq!(name, "bash");
                    assert!(content.contains("denied"), "{content}");
                    assert!(content.contains("not today"), "{content}");
                    assert!(content.contains("did not run"), "{content}");
                }
                SessionEvent::RunFinished { output, .. } => {
                    assert_eq!(output, "understood");
                    return;
                }
                SessionEvent::RunFailed { message } => panic!("the run failed: {message}"),
                _ => {}
            },
            ServerFrame::Control(control) => panic!("unexpected control frame: {control:?}"),
        }
    }
}

// ── task 4: enablement, skills mounts, providers fragments ─────────

/// An unlisted package is the user's setting, not a failure: it boots
/// nowhere — no catalog announcement, no launch — while the backend
/// itself runs a normal turn.
#[test]
fn a_package_outside_the_allowlist_mounts_nowhere() {
    let stage = stage("disabled", &[("echoer", "tools-echo")]);
    write_settings(&stage.settings, &[]);
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
                SessionEvent::RunFailed { message } => panic!("the run failed: {message}"),
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

/// The skill-shipping package: its `skills/` directory mounts into the
/// (redirected) home's skills dir, the ordinary discovery announces
/// it, and the extension's catalog entry carries the provenance.
#[test]
fn a_skill_shipping_package_mounts_its_skills_into_discovery() {
    let stage = stage("skills", &[]);
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
            "description": "the skills-only example package",
            "entry": [workspace_bin("skillship-ext").display().to_string()],
        }))
        .expect("manifest"),
    )
    .expect("manifest");
    write_settings(&stage.settings, &["skillship"]);
    scripted_turns(&stage, &[("never-called".to_string(), sse_text("done"))]);

    let mut backend = spawn_backend(&stage, &[]);
    let (_session, catalog, skills) = handshake(&mut backend);
    let extension = catalog
        .extensions
        .iter()
        .find(|extension| extension.name == "skillship")
        .expect("the package is enabled and announced");
    assert_eq!(extension.status, "alive");
    assert_eq!(extension.skills, vec!["skillship-demo".to_string()]);

    // The ordinary discovery ladder found the linked mount. Discovery
    // canonicalizes, so the announced location is the package's real
    // path (honest provenance) — the mount itself is the slot in the
    // redirected home, asserted directly: it resolves to the
    // package's body.
    let skills = skills.expect("skills_available arrived");
    let skill = skills
        .iter()
        .find(|skill| skill.name == "skillship-demo")
        .expect("the shipped skill is discovered");
    assert!(
        skill.location.contains("skillship-demo"),
        "the canonical package path: {}",
        skill.location
    );
    let slot = stage.home.join(".tabit").join("skills").join("skillship");
    let body = std::fs::read_to_string(slot.join("skillship-demo").join("SKILL.md"))
        .expect("the home slot resolves into the package");
    assert!(body.contains("The shipped body"), "{body}");
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
            "[providers.lmstudio-relay]\nbase_url = \"http://127.0.0.1:{relay_port}/v1\"\napi = \"openai-completions\"\n\n[[providers.lmstudio-relay.models]]\nid = \"local-model\"\n"
        ),
    )
    .expect("fragment");
    write_settings(&stage.settings, &["lmstudio"]);

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
    assert_eq!(extension.providers, vec!["lmstudio-relay".to_string()]);

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
                SessionEvent::RunFailed { message } => {
                    assert!(failure_beat_sent, "beat 1 must succeed first");
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
            "[providers.lmstudio-relay]\nbase_url = \"http://127.0.0.1:{relay_port}/v1\"\napi = \"openai-completions\"\n\n[[providers.lmstudio-relay.models]]\nid = \"local-model\"\n"
        ),
    )
    .expect("fragment");
    write_settings(&stage.settings, &["lmstudio"]);

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
    let (session, catalog, _skills) = handshake(&mut backend);
    assert_eq!(
        catalog.extensions[0].providers,
        vec!["lmstudio-relay".to_string()]
    );
    backend.send(&to_wire_line(&SessionCommand::Message {
        session,
        text: "any prompt".to_string(),
    }));
    loop {
        match backend.next_frame() {
            ServerFrame::Event(frame) => {
                if let SessionEvent::RunFailed { message } = frame.event {
                    assert!(message.contains("unreachable"), "{message}");
                    return;
                }
            }
            ServerFrame::Control(control) => panic!("unexpected control frame: {control:?}"),
        }
    }
}
