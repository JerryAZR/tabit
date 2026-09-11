//! Supervisor tests: the real lifecycle over real pipes (the
//! `ext-double` binary is the behavior double — every path here is an
//! honest multi-process roundtrip, offline).

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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use rig_agent::tool::interaction::{InteractionOutcome, UserInteraction};
use tabit_ext::supervisor::{self, ExtensionEvent, HANDSHAKE_TIMEOUT, Status};

/// Generous bound for real-process roundtrips (spawn + handshake on a
/// loaded CI box stays well under; the bound catches hangs, not
/// slowness).
const BOUND: Duration = Duration::from_secs(15);

fn test_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::OnceLock<std::sync::Mutex<u32>> = std::sync::OnceLock::new();
    let n = {
        let counter = COUNTER.get_or_init(|| std::sync::Mutex::new(0));
        let mut n = counter.lock().expect("counter lock");
        *n += 1;
        *n
    };
    let dir = std::env::temp_dir().join(format!("tabit-ext-supervisor-tests/{tag}-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn double() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ext-double"))
}

/// Install the double under `root` with the given behavior.
fn install(root: &Path, name: &str, behavior: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("package dir");
    let manifest = serde_json::json!({
        "name": name,
        "version": "0.1.0",
        "description": "the behavior double",
        "entry": [double().display().to_string(), behavior],
    });
    std::fs::write(
        dir.join("tabit.json"),
        serde_json::to_string(&manifest).expect("manifest json"),
    )
    .expect("manifest");
}

/// The next event, bounded.
async fn next_event(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<ExtensionEvent>,
) -> ExtensionEvent {
    tokio::time::timeout(BOUND, events.recv())
        .await
        .expect("an event within the bound")
        .expect("the event channel stays open while the supervisor lives")
}

/// Events until (and including) one for `name` whose status matches.
async fn await_status(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<ExtensionEvent>,
    name: &str,
    matches: impl Fn(&Status) -> bool,
) -> ExtensionEvent {
    loop {
        let event = next_event(events).await;
        if event.name == name && matches(&event.status) {
            return event;
        }
    }
}

fn dead_reason(status: &Status) -> &str {
    match status {
        Status::Dead { reason } => reason,
        other => panic!("expected Dead, got {other:?}"),
    }
}

#[tokio::test]
async fn a_healthy_extension_handshakes_alive() {
    let root = test_dir("alive");
    install(&root, "hello", "hello");
    let (supervisor, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    let event = await_status(&mut events, "hello", |s| matches!(s, Status::Alive)).await;
    assert_eq!(event.name, "hello");
    let reports = supervisor.reports();
    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    assert!(matches!(report.status, Status::Alive));
    assert_eq!(report.version, "0.1.0");
    assert_eq!(report.description.as_deref(), Some("the behavior double"));
    assert!(report.tools.is_empty());
    assert!(report.hooks.is_empty());
    supervisor.shutdown().await;
}

#[tokio::test]
async fn an_exit_before_the_ack_is_dead() {
    let root = test_dir("pre-ack");
    install(&root, "early", "die-pre-ack");
    let (supervisor, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    let event = await_status(&mut events, "early", |s| matches!(s, Status::Dead { .. })).await;
    assert!(dead_reason(&event.status).contains("before the handshake"));
    supervisor.shutdown().await;
}

#[tokio::test]
async fn an_exit_after_the_ack_marks_dead() {
    let root = test_dir("post-ack");
    install(&root, "ghost", "die-post-ack");
    let (supervisor, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    await_status(&mut events, "ghost", |s| matches!(s, Status::Alive)).await;
    let event = await_status(&mut events, "ghost", |s| matches!(s, Status::Dead { .. })).await;
    let reason = dead_reason(&event.status);
    assert!(reason.contains("the extension process exited"), "{reason}");
    assert!(reason.contains("exit code 0"), "{reason}");
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_silent_handshake_times_out() {
    let root = test_dir("mute");
    install(&root, "mute", "mute");
    let (supervisor, mut events) = supervisor::launch(&root, Duration::from_millis(300));
    let event = await_status(&mut events, "mute", |s| matches!(s, Status::Dead { .. })).await;
    assert!(dead_reason(&event.status).contains("no handshake"));
    supervisor.shutdown().await;
}

#[tokio::test]
async fn garbage_in_the_ack_is_refused() {
    let root = test_dir("bad-ack");
    install(&root, "bad", "bad-ack");
    let (supervisor, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    let event = await_status(&mut events, "bad", |s| matches!(s, Status::Dead { .. })).await;
    assert!(dead_reason(&event.status).contains("unparseable"));
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_version_mismatch_is_refused() {
    let root = test_dir("version");
    install(&root, "future", "wrong-version");
    let (supervisor, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    let event = await_status(&mut events, "future", |s| matches!(s, Status::Dead { .. })).await;
    assert!(
        dead_reason(&event.status).contains("speaks extension protocol version 99"),
        "{}",
        dead_reason(&event.status)
    );
    supervisor.shutdown().await;
}

#[tokio::test]
async fn garbage_after_the_ack_kills() {
    let root = test_dir("late");
    install(&root, "late", "late-garbage");
    let (supervisor, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    await_status(&mut events, "late", |s| matches!(s, Status::Alive)).await;
    let event = await_status(&mut events, "late", |s| matches!(s, Status::Dead { .. })).await;
    assert!(dead_reason(&event.status).contains("unparseable"));
    supervisor.shutdown().await;
}

#[tokio::test]
async fn scan_refusals_report_without_spawning() {
    let root = test_dir("refused");
    // A name mismatch, an empty entry, and a plain dir that is not an
    // extension at all.
    let mismatched = root.join("mismatched");
    std::fs::create_dir_all(&mismatched).expect("dir");
    std::fs::write(
        mismatched.join("tabit.json"),
        r#"{"name":"other","version":"1","entry":["x"]}"#,
    )
    .expect("manifest");
    let noentry = root.join("noentry");
    std::fs::create_dir_all(&noentry).expect("dir");
    std::fs::write(
        noentry.join("tabit.json"),
        r#"{"name":"noentry","version":"1","entry":[]}"#,
    )
    .expect("manifest");
    std::fs::create_dir_all(root.join("plain")).expect("dir");

    let (supervisor, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    let mut reported = Vec::new();
    for _ in 0..2 {
        let event = next_event(&mut events).await;
        reported.push((event.name, matches!(event.status, Status::Dead { .. })));
    }
    reported.sort();
    assert_eq!(
        reported,
        vec![
            ("mismatched".to_string(), true),
            ("noentry".to_string(), true)
        ]
    );
    // The plain dir never reported: it is not an extension.
    let reports = supervisor.reports();
    assert_eq!(reports.len(), 2);
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_missing_root_is_an_empty_install() {
    let root = test_dir("absent").join("never-created");
    let (supervisor, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    assert!(supervisor.reports().is_empty());
    assert!(events.try_recv().is_err());
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_mute_sibling_does_not_delay_the_healthy() {
    let root = test_dir("sibling");
    install(&root, "aaa-hello", "hello");
    install(&root, "zzz-mute", "mute");
    // The mute sibling's timeout is the whole window: if handshakes
    // serialized, hello would only resolve after it burned. The
    // margin is deliberately loose — a machine saturated by the
    // sibling tests may starve hello's task, and that is load, not
    // serialization; the fail-fast on Dead below keeps a real
    // timeout diagnosable instead of burning the bound.
    let timeout = Duration::from_secs(5);
    let (supervisor, mut events) = supervisor::launch(&root, timeout);
    let start = std::time::Instant::now();
    loop {
        let event = next_event(&mut events).await;
        if event.name != "aaa-hello" {
            continue;
        }
        match event.status {
            Status::Alive => break,
            Status::Dead { reason } => panic!("the healthy sibling died: {reason}"),
            Status::Starting => {}
        }
    }
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "the healthy extension must not wait for its mute sibling"
    );
    await_status(&mut events, "zzz-mute", |s| {
        matches!(s, Status::Dead { .. })
    })
    .await;
    supervisor.shutdown().await;
}

#[tokio::test]
async fn shutdown_reclaims_the_extension_tree() {
    let root = test_dir("reclaim");
    install(&root, "hello", "hello");
    // The double touches the marker on a clean EOF exit — the proof
    // the host closed the pipe (and the process observed it).
    let marker = root.join("marker");
    // Only this test uses EXT_DOUBLE_MARKER, so the process-wide env
    // cannot race another test.
    #[allow(unsafe_code, clippy::missing_safety_doc)]
    unsafe {
        std::env::set_var("EXT_DOUBLE_MARKER", &marker);
    }
    let (supervisor, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    await_status(&mut events, "hello", |s| matches!(s, Status::Alive)).await;
    supervisor.shutdown().await;
    assert!(
        marker.is_file(),
        "the extension must have exited on the pipe close"
    );
}

/// A scripted interaction capability: records what crossed, answers
/// (or dismisses) on cue.
struct FakeInteraction {
    answer: Option<serde_json::Value>,
    seen: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
}

impl UserInteraction for FakeInteraction {
    fn request(
        &self,
        ui_type: &str,
        payload: serde_json::Value,
    ) -> BoxFuture<'static, InteractionOutcome> {
        let ui_type = ui_type.to_string();
        let answer = self.answer.clone();
        let seen = self.seen.clone();
        Box::pin(async move {
            seen.lock().expect("seen lock").push((ui_type, payload));
            match answer {
                Some(payload) => InteractionOutcome::Answered(payload),
                None => InteractionOutcome::Dismissed,
            }
        })
    }
}

#[tokio::test]
async fn a_tool_call_round_trips_over_the_pipe() {
    let root = test_dir("call");
    install(&root, "echoer", "tools-echo");
    let (supervisor, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    await_status(&mut events, "echoer", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("echoer").expect("installed");
    let result = handle
        .call("echo", serde_json::json!({"text": "hi"}), None)
        .await
        .expect("the call resolves");
    assert_eq!(result.error, None);
    assert_eq!(result.report, "EXT-ECHOED:hi");
    assert_eq!(result.details, Some(serde_json::json!({"echoed": true})));
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_failing_tool_carries_its_error() {
    let root = test_dir("fail");
    install(&root, "boomer", "tools-fail");
    let (supervisor, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    await_status(&mut events, "boomer", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("boomer").expect("installed");
    let result = handle
        .call("boom", serde_json::json!({"text": "x"}), None)
        .await
        .expect("the call resolves");
    assert_eq!(
        result.error.as_deref(),
        Some("the boom tool refuses"),
        "{result:?}"
    );
    supervisor.shutdown().await;
}

#[tokio::test]
async fn an_ask_lifts_through_the_interaction_capability() {
    let root = test_dir("ask");
    install(&root, "asker", "tools-ask");
    let (supervisor, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    await_status(&mut events, "asker", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("asker").expect("installed");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let interaction = FakeInteraction {
        answer: Some(serde_json::json!({"text": "yes"})),
        seen: seen.clone(),
    };
    let result = handle
        .call(
            "ask",
            serde_json::json!({"text": "should we?"}),
            Some(Arc::new(interaction)),
        )
        .await
        .expect("the call resolves");
    assert_eq!(result.error, None);
    assert_eq!(result.report, "answered: yes");
    {
        let seen = seen.lock().expect("seen lock");
        let (ui_type, payload) = &seen[0];
        assert_eq!(ui_type, "native:select_any");
        assert_eq!(payload["body"], "the ask tool was called with should we?");
    }
    supervisor.shutdown().await;
}

#[tokio::test]
async fn an_ask_without_a_capability_fails_closed() {
    let root = test_dir("no-ask");
    install(&root, "asker", "tools-ask");
    let (supervisor, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    await_status(&mut events, "asker", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("asker").expect("installed");
    let result = handle
        .call("ask", serde_json::json!({"text": "anyone?"}), None)
        .await
        .expect("the call resolves");
    assert_eq!(result.error, None);
    assert_eq!(result.report, "dismissed");
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_call_after_death_fails_fast() {
    let root = test_dir("late-call");
    install(&root, "echoer", "tools-echo");
    let (supervisor, mut events) = supervisor::launch(&root, HANDSHAKE_TIMEOUT);
    await_status(&mut events, "echoer", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("echoer").expect("installed");
    // One healthy call, then the host closes (the supervisor drops:
    // stdin EOF, the double exits, the reader drains the lane).
    let result = handle
        .call("echo", serde_json::json!({"text": "first"}), None)
        .await
        .expect("the first call resolves");
    assert_eq!(result.report, "EXT-ECHOED:first");
    drop(supervisor);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match handle
            .call("echo", serde_json::json!({"text": "again"}), None)
            .await
        {
            Ok(result) => assert_eq!(result.error, None, "still serving before the close lands"),
            Err(reason) => {
                assert!(reason.contains("not running"), "{reason}");
                return;
            }
        }
        assert!(std::time::Instant::now() < deadline, "the lane never died");
    }
}

#[tokio::test]
async fn await_resolved_joins_every_handshake() {
    let root = test_dir("resolved");
    install(&root, "aaa-hello", "hello");
    install(&root, "zzz-mute", "mute");
    let (supervisor, mut events) = supervisor::launch(&root, Duration::from_millis(300));
    supervisor.await_resolved().await;
    // Both verdicts stand — the join did not return on the first.
    let reports = supervisor.reports();
    assert_eq!(reports.len(), 2);
    for report in &reports {
        assert!(
            !matches!(report.status, Status::Starting),
            "join returned with a Starting child: {:?}",
            report.status
        );
    }
    await_status(&mut events, "aaa-hello", |s| matches!(s, Status::Alive)).await;
    supervisor.shutdown().await;
}
