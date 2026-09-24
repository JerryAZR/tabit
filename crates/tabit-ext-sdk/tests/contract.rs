//! The contract tests: the SDK's example extensions driven by the
//! real host (`tabit-ext`) — both sides of the frozen pipe in one
//! test, offline. If the host and the SDK ever drift, these break
//! before anything ships.

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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use rig_agent::tool::services::HostServices;
use tabit_ext::supervisor::{self, HANDSHAKE_TIMEOUT, Status};

// Generous: the bound exists to catch hangs, not to race a loaded
// runner's process spawns — it sits above the protocol's own 30s
// handshake window on purpose (the load-timing family's second
// occurrence bought this comment).
const BOUND: Duration = Duration::from_secs(45);

/// A never-fired run token for call sites that test the steady
/// state (cancellation has its own tests).
/// The recorded grammar: what crossed, in arrival order.
#[derive(Default, Clone)]
struct Recorded {
    node: std::sync::OnceLock<std::sync::Arc<tabit_wire::node::Node>>,
    events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl Recorded {
    fn host(&self) -> tabit_ext::LaunchContext {
        let events = self.events.clone();
        let node = std::sync::Arc::new(tabit_wire::node::Node::new("test"));
        node.subscribe_all("recorder", move |frame: &tabit_protocol::EventFrame| {
            let origin = frame.origin.clone().unwrap_or_else(|| "-".to_string());
            events.lock().unwrap().push(format!(
                "{origin}|{}",
                serde_json::to_string(&frame.event).unwrap()
            ));
        });
        self.node.get_or_init(|| node.clone());
        tabit_ext::LaunchContext {
            node,
            core_path: "tabit-core".to_string(),
            cwd: ".".to_string(),
        }
    }

    /// The recorded net (set by `host`).
    fn net(&self) -> std::sync::Arc<tabit_wire::node::Node> {
        self.node.get().expect("host() ran first").clone()
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    /// The newest interaction-request id from one extension.
    fn newest_ask(&self, extension: &str) -> Option<String> {
        let needle = "\"id\":\"".to_string();
        self.events()
            .iter()
            .rev()
            .find(|e| e.starts_with(extension) && e.contains("interaction_request"))
            .and_then(|e| e.split(&needle).nth(1))
            .and_then(|rest| rest.split('"').next().map(str::to_string))
    }
}

/// Poll until true, bounded.
async fn wait_for(mut check: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + BOUND;
    while std::time::Instant::now() < deadline {
        if check() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

/// The launch context the contract tests serve: grammar dropped, and
/// the host facts a real boot would carry.
fn host_ctx() -> tabit_ext::LaunchContext {
    tabit_ext::LaunchContext {
        node: std::sync::Arc::new(tabit_wire::node::Node::new("test")),
        core_path: "tabit-core".to_string(),
        cwd: ".".to_string(),
    }
}

fn run_token() -> tokio_util::sync::CancellationToken {
    tokio_util::sync::CancellationToken::new()
}

fn test_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::OnceLock<std::sync::Mutex<u32>> = std::sync::OnceLock::new();
    let n = {
        let counter = COUNTER.get_or_init(|| std::sync::Mutex::new(0));
        let mut n = counter.lock().expect("counter lock");
        *n += 1;
        *n
    };
    let dir = std::env::temp_dir().join(format!("tabit-ext-sdk-tests/{tag}-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Install one example binary as an extension package.
fn install(root: &Path, name: &str, bin: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("package dir");
    let exe = PathBuf::from(bin);
    let manifest = serde_json::json!({
        "name": name,
        "version": "0.1.0",
        "description": "an example extension",
        "entry": [exe.display().to_string()],
    });
    std::fs::write(
        dir.join("tabit.json"),
        serde_json::to_string(&manifest).expect("manifest"),
    )
    .expect("manifest");
}

async fn await_alive(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<supervisor::ExtensionEvent>,
    name: &str,
) {
    tokio::time::timeout(BOUND, async {
        loop {
            let event = events
                .recv()
                .await
                .expect("the channel stays open while the supervisor lives");
            if event.name == name {
                match event.status {
                    Status::Alive => return,
                    Status::Dead { reason } => panic!("{name} died at the handshake: {reason}"),
                    Status::Starting => {}
                }
            }
        }
    })
    .await
    .expect("the handshake resolves within the bound");
}

struct FakeServices {
    /// model_prompt callers, recorded (no asserts in futures).
    prompted: Arc<std::sync::Mutex<Vec<String>>>,
}

// No asserts inside the capability futures: a panic there kills the
// routing task and wedges the asking extension. Record; the tests
// assert on the recording.
impl HostServices for FakeServices {
    fn model_prompt(
        &self,
        caller: &str,
        _request: rig_agent::tool::services::ModelPromptRequest,
    ) -> BoxFuture<'static, Result<rig_agent::tool::services::ModelPromptOk, String>> {
        self.prompted
            .lock()
            .expect("prompted lock")
            .push(caller.to_string());
        Box::pin(async move {
            Ok(rig_agent::tool::services::ModelPromptOk {
                text: "the contract's canned title".to_string(),
                usage: rig_agent::tool::services::ServiceUsage {
                    input_tokens: 5,
                    output_tokens: 4,
                    total_tokens: 9,
                },
            })
        })
    }
}

#[tokio::test]
async fn the_echo_example_declares_and_serves() {
    let root = test_dir("echo");
    install(&root, "echo", env!("CARGO_BIN_EXE_echo-ext"));
    let (host, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, host_ctx());
    await_alive(&mut events, "echo").await;

    let reports = host.reports();
    let report = &reports[0];
    let names: Vec<&str> = report.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["echo", "ask"], "the ack declared both tools");

    let handle = host.extension("echo").expect("installed");
    let result = handle
        .call("echo", serde_json::json!({"text": "hi"}), None, run_token())
        .await
        .expect("the call resolves");
    assert_eq!(result.error, None);
    assert_eq!(result.report, "echo: hi");
    assert_eq!(result.details, Some(serde_json::json!({"length": 2})));
    host.shutdown().await;
}

#[tokio::test]
async fn the_ask_example_routes_its_answer() {
    let root = test_dir("ask-answered");
    install(&root, "echo", env!("CARGO_BIN_EXE_echo-ext"));
    let recorded = Recorded::default();
    let (host, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, recorded.host());
    await_alive(&mut events, "echo").await;
    let handle = host.extension("echo").expect("installed");

    // The grammar flow: the SDK's ask emits an interaction request;
    // the answer crosses back by id through the backend registry.
    let call = tokio::spawn(async move {
        handle
            .call(
                "ask",
                serde_json::json!({"question": "is this thing on?"}),
                None,
                run_token(),
            )
            .await
            .expect("the call resolves")
    });
    assert!(
        wait_for(|| recorded.newest_ask("echo").is_some()).await,
        "the ask surfaced: {:?}",
        recorded.events()
    );
    let id = recorded.newest_ask("echo").expect("the id");
    recorded.net().intake(
        &tabit_wire::node::Channel::local("test", |_| {}, |_| {}),
        tabit_wire::node::Inbound::Command(tabit_protocol::SessionCommand::InteractionResponse {
            session: None,
            id: id.clone(),
            payload: serde_json::json!({"text": "yes"}),
        }),
    );
    let result = call.await.expect("joined");
    assert_eq!(result.error, None);
    assert_eq!(result.report, "the user answered: yes");
    // The watch lane: echo watches `interaction_settled`. The
    // harness has no pump (the json edge's forwarder owns the
    // mirror), so this test plays the pump's one broadcast when the
    // settlement appears — then the watch's own emission must come
    // back on the recording: typed delivery, thread-dispatched, no
    // cancellation owed.
    let mut frame = None;
    let mirrored = wait_for(|| {
        frame = recorded
            .events()
            .iter()
            .filter_map(|e| e.strip_prefix("echo|"))
            .filter_map(|rest| serde_json::from_str::<tabit_protocol::EventFrame>(rest).ok())
            .find(|f| {
                matches!(
                    f.event,
                    tabit_protocol::SessionEvent::InteractionSettled { .. }
                )
            });
        frame.is_some()
    })
    .await;
    if !mirrored {
        panic!("no settlement to mirror: {:?}", recorded.events());
    }
    recorded.net().emit(
        &tabit_wire::node::Channel::local("test", |_| {}, |_| {}),
        frame.expect("the wait proved it"),
    );
    assert!(
        wait_for(|| {
            recorded.events().iter().any(|e| {
                e.starts_with("echo|")
                    && e.contains("error")
                    && e.contains(&format!("saw card `{id}` settle"))
            })
        })
        .await,
        "the watch observed the settlement and emitted: {:?}",
        recorded.events()
    );
    host.shutdown().await;
}

#[tokio::test]
async fn the_ask_example_abandoned_by_cancellation_fails_the_call() {
    let root = test_dir("ask-cancelled");
    install(&root, "echo", env!("CARGO_BIN_EXE_echo-ext"));
    let recorded = Recorded::default();
    let (host, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, recorded.host());
    await_alive(&mut events, "echo").await;
    let handle = host.extension("echo").expect("installed");

    // The grammar ask has no in-band dismissal; abandonment is the
    // run's cancellation, and the call fails cancelled at the leash.
    let token = tokio_util::sync::CancellationToken::new();
    let call_token = token.clone();
    let call = tokio::spawn(async move {
        handle
            .call(
                "ask",
                serde_json::json!({"question": "is this thing on?"}),
                None,
                call_token,
            )
            .await
    });
    assert!(
        wait_for(|| recorded.newest_ask("echo").is_some()).await,
        "the ask surfaced: {:?}",
        recorded.events()
    );
    token.cancel();
    let outcome = call.await.expect("joined");
    assert!(
        outcome
            .as_ref()
            .is_err_and(|error| error.contains("cancelled")),
        "the call fails cancelled: {outcome:?}"
    );
    host.shutdown().await;
}

#[tokio::test]
async fn a_failing_body_is_an_error_not_a_hang() {
    let root = test_dir("clash");
    // Both clash examples install together; the pair's conflict is
    // the host-assembly's business (the tabit binary), here we only
    // need one binary — but the pair also proves two SDK processes
    // can handshake side by side.
    install(&root, "clash-a", env!("CARGO_BIN_EXE_clash-a-ext"));
    install(&root, "clash-b", env!("CARGO_BIN_EXE_clash-b-ext"));
    let (host, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, host_ctx());
    await_alive(&mut events, "clash-a").await;
    await_alive(&mut events, "clash-b").await;

    // Both declared the shared name — the reports carry both; the
    // one-name-one-tool resolution is the assembler's ruling.
    let reports = host.reports();
    assert_eq!(reports.len(), 2);

    let handle = host.extension("clash-b").expect("installed");
    let result = handle
        .call(
            "clashy",
            serde_json::json!({"text": "x"}),
            None,
            run_token(),
        )
        .await
        .expect("the call resolves");
    assert_eq!(result.report, "clash-b served: x");
    host.shutdown().await;
}

/// Task 5's demo, contract-proven: the autotitle extension answers a
/// `tool_result` hook with one `model_prompt` over the envelope — the
/// host side sees the request attributed to the extension's own name,
/// and the extension reports the canned completion.
#[tokio::test]
async fn the_autotitle_example_prompts_the_model_over_the_envelope() {
    let root = test_dir("autotitle");
    install(&root, "autotitle", env!("CARGO_BIN_EXE_autotitle-ext"));
    let (host, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, host_ctx());
    await_alive(&mut events, "autotitle").await;
    let handle = host.extension("autotitle").expect("installed");

    let prompted = Arc::new(std::sync::Mutex::new(Vec::new()));
    let services: Arc<dyn HostServices> = Arc::new(FakeServices {
        prompted: prompted.clone(),
    });
    let result = serde_json::json!({"result": "42 lines changed"});
    // The observer point's answer is the unit — owed as completion,
    // not as a decision, so there is nothing to bind.
    handle
        .hook::<tabit_protocol::points::ToolResult>(result, Some(services), run_token())
        .await
        .expect("resolves");
    {
        let prompted = prompted.lock().expect("prompted lock");
        assert_eq!(prompted.len(), 1, "one prompt, once per session");
        assert_eq!(
            prompted[0], "autotitle",
            "attributed to the extension's name"
        );
    }
    host.shutdown().await;
}

/// The owned-children demo, end to end against a real `tabit-core`
/// child: the tool spawns the child (the handshake's `core_path` is
/// the backend's own binary), the child opens a session and fails its
/// task (no model is configured — offline by design), and the run
/// settles. The observation demo rides along: the child's
/// `session_opened` reaches the tool's `on` handler (per-child
/// observation composes with the default silence — no forwarding), and
/// its emission crosses the pipe origin-stamped.
#[tokio::test]
async fn the_child_example_spawns_observes_and_settles() {
    // The child spawner resolves `core_path` as an executable: the
    // workspace build's own backend, a sibling of this test's bins.
    let core = std::path::Path::new(env!("CARGO_BIN_EXE_child-ext"))
        .parent()
        .map(|dir| dir.join("tabit-core.exe"))
        .filter(|path| path.is_file());
    let Some(core) = core else {
        eprintln!(
            "child-ext e2e: no tabit-core.exe beside the test bins — \
             run the workspace suite (scripts/test.sh) to cover it"
        );
        return;
    };
    // Offline by design: the child's model is an EXPLICIT reference
    // to a provider whose endpoint is a dead loopback port, defined
    // by the env config layer the whole spawn chain inherits — the
    // run fails on a refused loopback connection, never touching the
    // user's real providers or any live network (rule 5). The env
    // claim is ours alone: no other test in this binary reads
    // config, so the unsafety's precondition holds by construction.
    let isolated_config = test_dir("child-ext-config").join("providers.toml");
    std::fs::write(
        &isolated_config,
        "[providers.offline]\nbase_url = \"http://127.0.0.1:9/v1\"\napi = \"openai-completions\"\n\
         keyless = true\n\n[[providers.offline.models]]\nid = \"dead\"\n",
    )
    .expect("the offline provider fragment");
    unsafe { std::env::set_var("TABIT_CONFIG", &isolated_config) };

    let root = test_dir("child-ext");
    install(&root, "child-ext", env!("CARGO_BIN_EXE_child-ext"));
    let recorded = Recorded::default();
    let mut ctx = recorded.host();
    ctx.core_path = core.display().to_string();
    let (host, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, ctx);
    await_alive(&mut events, "child-ext").await;
    let handle = host.extension("child-ext").expect("installed");

    let result = handle
        .call(
            "delegate",
            serde_json::json!({"task": "say hello", "model": "offline/dead"}),
            None,
            run_token(),
        )
        .await
        .expect("the call resolves");
    unsafe { std::env::remove_var("TABIT_CONFIG") };
    assert_eq!(result.error, None, "the delegation itself worked");
    assert!(
        result.report.starts_with("The child failed"),
        "the child ran and failed without a model: {}",
        result.report
    );
    assert!(
        wait_for(|| {
            recorded
                .events()
                .iter()
                .any(|e| e.starts_with("child-ext|") && e.contains("saw the child open"))
        })
        .await,
        "the observation heard the child and its emission crossed: {:?}",
        recorded.events()
    );
    host.shutdown().await;
}
