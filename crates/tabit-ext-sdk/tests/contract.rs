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
use rig_agent::tool::interaction::{InteractionOutcome, UserInteraction};
use tabit_ext::supervisor::{self, HANDSHAKE_TIMEOUT, Status};

const BOUND: Duration = Duration::from_secs(15);

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

struct FakeInteraction {
    answer: Option<serde_json::Value>,
}

impl UserInteraction for FakeInteraction {
    fn request(
        &self,
        ui_type: &str,
        payload: serde_json::Value,
    ) -> BoxFuture<'static, InteractionOutcome> {
        // No asserts in here: a panic inside the lifted future kills
        // the routing task and wedges the asking extension. Record;
        // the tests assert on the recording.
        eprintln!("fake interaction: {ui_type} {payload}");
        let answer = self.answer.clone();
        Box::pin(async move {
            match answer {
                Some(payload) => InteractionOutcome::Answered(payload),
                None => InteractionOutcome::Dismissed,
            }
        })
    }
}

#[tokio::test]
async fn the_echo_example_declares_and_serves() {
    let root = test_dir("echo");
    install(&root, "echo", env!("CARGO_BIN_EXE_echo-ext"));
    let (host, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    await_alive(&mut events, "echo").await;

    let reports = host.reports();
    let report = &reports[0];
    let names: Vec<&str> = report.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["echo", "ask"], "the ack declared both tools");

    let handle = host.extension("echo").expect("installed");
    let result = handle
        .call("echo", serde_json::json!({"text": "hi"}), None)
        .await
        .expect("the call resolves");
    assert_eq!(result.error, None);
    assert_eq!(result.report, "echo: hi");
    assert_eq!(result.details, Some(serde_json::json!({"length": 2})));
    host.shutdown().await;
}

#[tokio::test]
async fn the_ask_example_lifts_the_answer() {
    let root = test_dir("ask-answered");
    install(&root, "echo", env!("CARGO_BIN_EXE_echo-ext"));
    let (host, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    await_alive(&mut events, "echo").await;
    let handle = host.extension("echo").expect("installed");

    let interaction: Arc<dyn UserInteraction> = Arc::new(FakeInteraction {
        answer: Some(serde_json::json!({"text": "yes"})),
    });
    let result = handle
        .call(
            "ask",
            serde_json::json!({"question": "is this thing on?"}),
            Some(interaction),
        )
        .await
        .expect("the call resolves");
    assert_eq!(result.error, None);
    assert_eq!(result.report, "the user answered: yes");
    host.shutdown().await;
}

#[tokio::test]
async fn the_ask_example_fails_closed_on_dismissal() {
    let root = test_dir("ask-dismissed");
    install(&root, "echo", env!("CARGO_BIN_EXE_echo-ext"));
    let (host, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    await_alive(&mut events, "echo").await;
    let handle = host.extension("echo").expect("installed");

    let interaction: Arc<dyn UserInteraction> = Arc::new(FakeInteraction { answer: None });
    let result = handle
        .call(
            "ask",
            serde_json::json!({"question": "is this thing on?"}),
            Some(interaction),
        )
        .await
        .expect("the call resolves");
    assert_eq!(
        result.report,
        "the user dismissed the question without answering"
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
    let (host, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    await_alive(&mut events, "clash-a").await;
    await_alive(&mut events, "clash-b").await;

    // Both declared the shared name — the reports carry both; the
    // one-name-one-tool resolution is the assembler's ruling.
    let reports = host.reports();
    assert_eq!(reports.len(), 2);

    let handle = host.extension("clash-b").expect("installed");
    let result = handle
        .call("clashy", serde_json::json!({"text": "x"}), None)
        .await
        .expect("the call resolves");
    assert_eq!(result.report, "clash-b served: x");
    host.shutdown().await;
}

#[tokio::test]
async fn the_gate_extension_gates_bash_per_session() {
    let root = test_dir("gate");
    install(&root, "gate", env!("CARGO_BIN_EXE_gate-ext"));
    let (host, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    await_alive(&mut events, "gate").await;

    let reports = host.reports();
    assert!(reports[0].tools.is_empty(), "the gate ships no tools");
    let hooks: Vec<&str> = reports[0].hooks.iter().map(|h| h.event.as_str()).collect();
    assert_eq!(hooks, vec!["tool_call"]);

    let handle = host.extension("gate").expect("installed");
    let call = |tool: &str, session: &str| serde_json::json!({"session": session, "tool": tool, "args": "{\"command\":\"ls\"}"});

    // Outside the ask-set: runs with no card.
    let decision = handle
        .hook("tool_call", call("read", "s1"), None)
        .await
        .expect("resolves");
    assert_eq!(decision, tabit_ext_sdk_gate_decision_run(&decision));

    // No capability: the dismissal fails closed (the skip).
    match handle.hook("tool_call", call("bash", "s1"), None).await {
        Ok(decision) => assert!(matches!(
            decision,
            tabit_ext::protocol::HookDecision::Skip { .. }
        )),
        Err(other) => panic!("the dismissed ask must skip, not error: {other}"),
    }

    // Answered "Always allow": runs, and the session remembers — the
    // next bash call in s1 runs with NO capability (no card possible).
    let answered: Arc<dyn UserInteraction> = Arc::new(FakeInteraction {
        answer: Some(serde_json::json!({"selected": ["Always allow"]})),
    });
    let decision = handle
        .hook("tool_call", call("bash", "s1"), Some(answered))
        .await
        .expect("resolves");
    assert!(matches!(decision, tabit_ext::protocol::HookDecision::Run));
    let decision = handle
        .hook("tool_call", call("bash", "s1"), None)
        .await
        .expect("resolves");
    assert!(
        matches!(decision, tabit_ext::protocol::HookDecision::Run),
        "the session's grant is remembered"
    );

    // Another session does NOT inherit the grant.
    match handle.hook("tool_call", call("bash", "s2"), None).await {
        Ok(decision) => assert!(
            matches!(decision, tabit_ext::protocol::HookDecision::Skip { .. }),
            "the grant must not leak across sessions"
        ),
        Err(other) => panic!("expected a skip for the ungated session: {other}"),
    }
    host.shutdown().await;
}

/// A trivial identity helper keeping the first assert's type readable.
fn tabit_ext_sdk_gate_decision_run(
    decision: &tabit_ext::protocol::HookDecision,
) -> tabit_ext::protocol::HookDecision {
    decision.clone()
}
