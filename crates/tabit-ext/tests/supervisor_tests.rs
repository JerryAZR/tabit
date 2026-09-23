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
use rig_agent::tool::services::{HostServices, ModelPromptOk, ModelPromptRequest, ServiceUsage};
use tabit_ext::supervisor::{self, ExtensionEvent, HANDSHAKE_TIMEOUT, Status};

/// Generous bound for real-process roundtrips (spawn + handshake on a
/// loaded CI box stays well under; the bound catches hangs, not
/// slowness).
const BOUND: Duration = Duration::from_secs(15);

/// A never-fired run token for call sites that test the steady
/// state (cancellation has its own tests).
/// The launch context tests serve: grammar that records what crossed
/// (commands and events both), and placeholder host facts.
fn test_host() -> tabit_ext::LaunchContext {
    tabit_ext::LaunchContext {
        routes: tabit_ext::GrammarRoutes::noop(),
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
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
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
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    let event = await_status(&mut events, "early", |s| matches!(s, Status::Dead { .. })).await;
    assert!(dead_reason(&event.status).contains("before the handshake"));
    supervisor.shutdown().await;
}

#[tokio::test]
async fn an_exit_after_the_ack_marks_dead() {
    let root = test_dir("post-ack");
    install(&root, "ghost", "die-post-ack");
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
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
    let (supervisor, mut events) =
        supervisor::launch_root(&root, Duration::from_millis(300), test_host());
    let event = await_status(&mut events, "mute", |s| matches!(s, Status::Dead { .. })).await;
    assert!(dead_reason(&event.status).contains("no handshake"));
    supervisor.shutdown().await;
}

#[tokio::test]
async fn garbage_in_the_ack_is_refused() {
    let root = test_dir("bad-ack");
    install(&root, "bad", "bad-ack");
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    let event = await_status(&mut events, "bad", |s| matches!(s, Status::Dead { .. })).await;
    assert!(dead_reason(&event.status).contains("unparseable"));
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_version_mismatch_is_refused() {
    let root = test_dir("version");
    install(&root, "future", "wrong-version");
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
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
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    await_status(&mut events, "late", |s| matches!(s, Status::Alive)).await;
    let event = await_status(&mut events, "late", |s| matches!(s, Status::Dead { .. })).await;
    assert!(dead_reason(&event.status).contains("unparseable"));
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_well_formed_unknown_frame_type_is_the_same_death() {
    // The compatibility ruling (2026-09): tolerance is one-directional
    // — a newer host keeps an older extension working, but an
    // extension speaking vocabulary its host lacks is refused, not run
    // half-working. A valid-JSON line of an unknown type is exactly
    // that extension.
    let root = test_dir("late-unknown");
    install(&root, "future", "late-unknown");
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    await_status(&mut events, "future", |s| matches!(s, Status::Alive)).await;
    let event = await_status(&mut events, "future", |s| matches!(s, Status::Dead { .. })).await;
    let reason = dead_reason(&event.status);
    assert!(reason.contains("unknown-type"), "{reason}");
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

    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
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
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    assert!(supervisor.reports().is_empty());
    assert!(events.try_recv().is_err());
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_mute_sibling_does_not_delay_the_healthy() {
    let root = test_dir("sibling");
    // The healthy sibling declares a tool so the survival assert can
    // also drive a call through its lane.
    install(&root, "aaa-echo", "tools-echo");
    install(&root, "zzz-mute", "mute");
    // The mute sibling's timeout is the whole window: if handshakes
    // serialized, hello would only resolve after it burned. The
    // margin is deliberately loose — a machine saturated by the
    // sibling tests may starve hello's task, and that is load, not
    // serialization; the fail-fast on Dead below keeps a real
    // timeout diagnosable instead of burning the bound.
    let timeout = Duration::from_secs(5);
    let (supervisor, mut events) = supervisor::launch_root(&root, timeout, test_host());
    let start = std::time::Instant::now();
    loop {
        let event = next_event(&mut events).await;
        if event.name != "aaa-echo" {
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
    // The isolation invariant, past the failure: one broken package
    // costs one boot, loudly, and NOTHING more — the healthy sibling
    // is still Alive and still serves. (The fleet-kill bug this pins:
    // the broken child's pre-ack failure once cancelled the
    // supervisor-wide closing token, silently closing every healthy
    // sibling's pipe with their status stuck at Alive.)
    let reports = supervisor.reports();
    let healthy = reports
        .iter()
        .find(|r| r.name == "aaa-echo")
        .expect("listed");
    assert!(
        matches!(healthy.status, Status::Alive),
        "the healthy sibling survives the broken one's failure: {:?}",
        healthy.status
    );
    let handle = supervisor.extension("aaa-echo").expect("the lane lives");
    let result = handle
        .call(
            "echo",
            serde_json::json!({"text": "still here"}),
            None,
            run_token(),
        )
        .await
        .expect("the healthy sibling still serves");
    assert!(result.error.is_none(), "{result:?}");
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
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    await_status(&mut events, "hello", |s| matches!(s, Status::Alive)).await;
    supervisor.shutdown().await;
    assert!(
        marker.is_file(),
        "the extension must have exited on the pipe close"
    );
}

/// A scripted host-service capability: records what crossed, answers
/// asks (or dismisses) and model prompts on cue.
struct FakeServices {
    prompt: Option<Result<String, String>>,
    seen_prompt: Arc<Mutex<Vec<(String, String)>>>,
}

impl HostServices for FakeServices {
    fn model_prompt(
        &self,
        caller: &str,
        request: ModelPromptRequest,
    ) -> BoxFuture<'static, Result<ModelPromptOk, String>> {
        let caller = caller.to_string();
        let answer = self.prompt.clone();
        let seen = self.seen_prompt.clone();
        Box::pin(async move {
            seen.lock()
                .expect("seen lock")
                .push((caller, request.prompt));
            match answer {
                Some(Ok(text)) => Ok(ModelPromptOk {
                    text,
                    usage: ServiceUsage {
                        input_tokens: 11,
                        output_tokens: 7,
                        total_tokens: 18,
                    },
                }),
                Some(Err(message)) => Err(message),
                None => Err("no scripted prompt".to_string()),
            }
        })
    }
}

#[tokio::test]
async fn a_tool_call_round_trips_over_the_pipe() {
    let root = test_dir("call");
    install(&root, "echoer", "tools-echo");
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    await_status(&mut events, "echoer", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("echoer").expect("installed");
    let result = handle
        .call("echo", serde_json::json!({"text": "hi"}), None, run_token())
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
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    await_status(&mut events, "boomer", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("boomer").expect("installed");
    let result = handle
        .call("boom", serde_json::json!({"text": "x"}), None, run_token())
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
async fn an_ask_routes_through_the_backend_registry() {
    let root = test_dir("ask");
    install(&root, "asker", "tools-ask");
    let recorded = Recorded::default();
    let (supervisor, mut events) =
        supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, recorded.host());
    await_status(&mut events, "asker", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("asker").expect("installed");

    // The call parks on its ask; the answer arrives by id through the
    // backend registry (the grammar flow — the envelope verb is gone).
    let call = tokio::spawn(async move {
        handle
            .call(
                "ask",
                serde_json::json!({"text": "should we?"}),
                None,
                run_token(),
            )
            .await
            .expect("the call resolves")
    });
    let asked = wait_for(|| {
        recorded
            .events()
            .iter()
            .any(|e| e.starts_with("asker|") && e.contains("interaction_request"))
    })
    .await;
    assert!(asked, "the ask emission routed: {:?}", recorded.events());
    let id = recorded
        .events()
        .iter()
        .find(|e| e.contains("interaction_request"))
        .and_then(|e| e.split("\"id\":\"").nth(1))
        .and_then(|rest| rest.split('"').next().map(str::to_string))
        .expect("the ask id");
    assert!(
        supervisor
            .asks()
            .respond(&id, serde_json::json!({"text": "yes"}))
    );
    let result = call.await.expect("joined");
    assert_eq!(result.error, None);
    assert_eq!(result.report, "answered: yes");
    // Settlement is announced for every channel holding the card.
    assert!(
        wait_for(|| {
            recorded
                .events()
                .iter()
                .any(|e| e.contains("interaction_settled") && e.contains(&id))
        })
        .await
    );
    supervisor.shutdown().await;
}

#[tokio::test]
async fn an_ask_abandoned_by_cancellation_reports_dismissed() {
    let root = test_dir("no-ask");
    install(&root, "asker", "tools-ask");
    let recorded = Recorded::default();
    let (supervisor, mut events) =
        supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, recorded.host());
    await_status(&mut events, "asker", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("asker").expect("installed");

    // The grammar ask has no in-band dismissal: abandonment is the
    // run's cancellation (the guest reads the cancel frame as its
    // ask resolving dismissed).
    let token = tokio_util::sync::CancellationToken::new();
    let call_token = token.clone();
    let call = tokio::spawn(async move {
        handle
            .call(
                "ask",
                serde_json::json!({"text": "anyone?"}),
                None,
                call_token,
            )
            .await
    });
    assert!(
        wait_for(|| {
            recorded
                .events()
                .iter()
                .any(|e| e.starts_with("asker|") && e.contains("interaction_request"))
        })
        .await,
        "the ask surfaced first: {:?}",
        recorded.events()
    );
    token.cancel();
    // Token-and-detach: the call fails cancelled (the model-visible
    // failure); the guest's own dismissal handling is its cleanup,
    // racing a pending entry that is already gone.
    let outcome = call.await.expect("joined");
    assert!(
        outcome
            .as_ref()
            .is_err_and(|error| error.contains("cancelled")),
        "the call fails cancelled: {outcome:?}"
    );
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_model_prompt_dispatches_through_the_envelope() {
    let root = test_dir("model");
    install(&root, "modeler", "tools-model");
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    await_status(&mut events, "modeler", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("modeler").expect("installed");
    let seen_prompt = Arc::new(Mutex::new(Vec::new()));
    let services = FakeServices {
        prompt: Some(Ok("five words exactly right".to_string())),
        seen_prompt: seen_prompt.clone(),
    };
    let result = handle
        .call(
            "summarize",
            serde_json::json!({"text": "the long tail of a session"}),
            Some(Arc::new(services)),
            run_token(),
        )
        .await
        .expect("the call resolves");
    assert_eq!(result.error, None);
    assert_eq!(result.report, "EXT-MODELED:five words exactly right");
    assert_eq!(
        result.details,
        Some(
            serde_json::json!({"usage": {"input_tokens": 11, "output_tokens": 7, "total_tokens": 18}})
        )
    );
    {
        // The caller tag is the supervisor's lane: the extension's
        // own name — the attribution the verb bills by.
        let seen = seen_prompt.lock().expect("seen lock");
        assert_eq!(seen[0].0, "modeler");
        assert!(seen[0].1.contains("the long tail of a session"), "{seen:?}");
    }
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_model_prompt_without_services_fails_with_the_verb_error() {
    let root = test_dir("model-bare");
    install(&root, "modeler", "tools-model");
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    await_status(&mut events, "modeler", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("modeler").expect("installed");
    let result = handle
        .call(
            "summarize",
            serde_json::json!({"text": "anything"}),
            None,
            run_token(),
        )
        .await
        .expect("the call resolves");
    let error = result.error.expect("the verb errors, not the call");
    assert!(error.contains("no session context"), "{error}");
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_call_after_death_fails_fast() {
    let root = test_dir("late-call");
    install(&root, "echoer", "tools-echo");
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    await_status(&mut events, "echoer", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("echoer").expect("installed");
    // One healthy call, then the host closes (the supervisor drops:
    // stdin EOF, the double exits, the reader drains the lane).
    let result = handle
        .call(
            "echo",
            serde_json::json!({"text": "first"}),
            None,
            run_token(),
        )
        .await
        .expect("the first call resolves");
    assert_eq!(result.report, "EXT-ECHOED:first");
    drop(supervisor);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match handle
            .call(
                "echo",
                serde_json::json!({"text": "again"}),
                None,
                run_token(),
            )
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
    let (supervisor, mut events) =
        supervisor::launch_root(&root, Duration::from_millis(300), test_host());
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

#[tokio::test]
async fn a_hook_round_trips_its_decision() {
    let root = test_dir("hook-allow");
    install(&root, "allower", "hooks-allow");
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    await_status(&mut events, "allower", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("allower").expect("installed");
    let decision = handle
        .hook(
            "tool_call",
            serde_json::json!({"session": "s1", "tool": "bash", "args": "{\"command\":\"ls\"}"}),
            None,
            run_token(),
        )
        .await
        .expect("the hook resolves");
    assert_eq!(decision, tabit_ext::protocol::HookDecision::Run);
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_hook_skip_carries_its_message() {
    let root = test_dir("hook-skip");
    install(&root, "denier", "hooks-skip");
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    await_status(&mut events, "denier", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("denier").expect("installed");
    let decision = handle
        .hook(
            "tool_call",
            serde_json::json!({"tool": "bash"}),
            None,
            run_token(),
        )
        .await
        .expect("the hook resolves");
    assert_eq!(
        decision,
        tabit_ext::protocol::HookDecision::Skip {
            message: "the double denies".to_string()
        }
    );
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_hook_ask_decides_through_the_backend_registry() {
    let root = test_dir("hook-ask");
    install(&root, "asker", "hooks-ask");
    let recorded = Recorded::default();
    let (supervisor, mut events) =
        supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, recorded.host());
    await_status(&mut events, "asker", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("asker").expect("installed");

    // Allowed: the answer routes by id, the hook runs the call.
    let hook = tokio::spawn(async move {
        handle
            .hook(
                "tool_call",
                serde_json::json!({"tool": "bash"}),
                None,
                run_token(),
            )
            .await
            .expect("the hook resolves")
    });
    assert!(
        wait_for(|| {
            recorded
                .events()
                .iter()
                .any(|e| e.starts_with("asker|") && e.contains("interaction_request"))
        })
        .await,
        "the hook's ask surfaced: {:?}",
        recorded.events()
    );
    let id = recorded
        .events()
        .iter()
        .find(|e| e.contains("interaction_request"))
        .and_then(|e| e.split("\"id\":\"").nth(1))
        .and_then(|rest| rest.split('"').next().map(str::to_string))
        .expect("the ask id");
    assert!(
        supervisor
            .asks()
            .respond(&id, serde_json::json!({"selected": ["Allow"]}))
    );
    let decision = hook.await.expect("joined");
    assert_eq!(decision, tabit_ext::protocol::HookDecision::Run);

    // Denied: the same flow, a Block answer skips with the reason.
    let handle = supervisor.extension("asker").expect("installed");
    let hook = tokio::spawn(async move {
        handle
            .hook(
                "tool_call",
                serde_json::json!({"tool": "bash"}),
                None,
                run_token(),
            )
            .await
            .expect("the hook resolves")
    });
    let baseline = recorded.events().len();
    assert!(
        wait_for(|| recorded.events().len() > baseline).await,
        "the second ask surfaced"
    );
    let id = recorded.events()[baseline..]
        .iter()
        .find(|e| e.contains("interaction_request"))
        .and_then(|e| e.split("\"id\":\"").nth(1))
        .and_then(|rest| rest.split('"').next().map(str::to_string))
        .expect("the second ask id");
    assert!(
        supervisor
            .asks()
            .respond(&id, serde_json::json!({"selected": ["Deny"]}))
    );
    let decision = hook.await.expect("joined");
    assert!(matches!(
        decision,
        tabit_ext::protocol::HookDecision::Skip { .. }
    ));
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_death_answers_pending_policy_with_the_fail_open_fallback() {
    let root = test_dir("hook-hang");
    install(&root, "wedge", "hooks-hang");
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    await_status(&mut events, "wedge", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("wedge").expect("installed");
    let pending = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .hook(
                    "tool_call",
                    serde_json::json!({"tool": "bash"}),
                    None,
                    run_token(),
                )
                .await
        })
    };
    // Give the forward a moment to cross, then close the host: the
    // drain must answer the pending hook with the fail-open fallback
    // (crash isolation — policy fails open, executions fail loudly).
    tokio::time::sleep(Duration::from_millis(200)).await;
    supervisor.shutdown().await;
    let decision = tokio::time::timeout(BOUND, pending)
        .await
        .expect("the drain answers within the bound")
        .expect("the task lives")
        .expect("the hook resolves");
    assert_eq!(decision, tabit_ext::protocol::HookDecision::Run);
}

/// The cancellation contract (the sandboxed-bash consumer's gap):
/// firing the run token sends `cancel` down the pipe, fails the
/// call, removes the pending entry — and the guest, which was
/// parked waiting for exactly that frame, answers into the void
/// while the LANE survives (a second call succeeds).
#[tokio::test]
async fn cancelling_the_run_token_cancels_the_call_across_the_pipe() {
    let root = test_dir("cancel");
    install(&root, "hanger", "tools-cancel");
    let (supervisor, mut events) = supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, test_host());
    await_status(&mut events, "hanger", |s| matches!(s, Status::Alive)).await;
    let handle = supervisor.extension("hanger").expect("installed");

    let token = tokio_util::sync::CancellationToken::new();
    let call = {
        let handle = handle.clone();
        let token = token.clone();
        tokio::spawn(async move {
            handle
                .call("hang", serde_json::json!({"text": "forever"}), None, token)
                .await
        })
    };
    // Give the call a moment to cross and park, then abort the run.
    tokio::time::sleep(Duration::from_millis(300)).await;
    token.cancel();
    let outcome = call.await.expect("the task joins").expect_err("cancelled");
    assert!(outcome.contains("cancelled"), "{outcome}");

    // The lane survived: a follow-up call serves normally.
    let result = handle
        .call(
            "echo",
            serde_json::json!({"text": "still here"}),
            None,
            run_token(),
        )
        .await
        .expect("the lane serves");
    assert_eq!(result.report, "EXT-ECHOED:still here");
    supervisor.shutdown().await;
}

/// What the recording routes captured, in arrival order.
#[derive(Default, Clone)]
struct Recorded {
    commands: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl Recorded {
    fn host(&self) -> tabit_ext::LaunchContext {
        let commands = self.commands.clone();
        let events = self.events.clone();
        tabit_ext::LaunchContext {
            routes: tabit_ext::GrammarRoutes::new(
                std::sync::Arc::new(move |command| {
                    commands
                        .lock()
                        .unwrap()
                        .push(serde_json::to_string(&command).unwrap());
                }),
                std::sync::Arc::new(move |origin, event| {
                    events.lock().unwrap().push(format!(
                        "{origin}|{}",
                        serde_json::to_string(&event).unwrap()
                    ));
                }),
                std::sync::Arc::new(|_, _| {}),
            ),
            core_path: "tabit-core".to_string(),
            cwd: ".".to_string(),
        }
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

/// The routing generalization over the real pipe: the extension's
/// command and emission route through the host glue; the ask
/// registers, the routed answer crosses back down the pipe (mirrored
/// out by the double as an event), settlement is announced, and the
/// broadcast mirror honors the watch list.
#[tokio::test]
async fn the_shared_grammar_flows_both_directions_over_the_pipe() {
    let root = test_dir("grammar");
    install(&root, "grammar-ext", "grammar");
    let recorded = Recorded::default();
    let (supervisor, mut events) =
        supervisor::launch_root(&root, HANDSHAKE_TIMEOUT, recorded.host());
    await_status(&mut events, "grammar-ext", |s| matches!(s, Status::Alive)).await;

    // The double's opening emissions crossed: one command, one ask.
    // Both race the Alive transition (the ack lands before the
    // double's next writes are processed), so poll for them — never
    // assert on an instantaneous snapshot.
    let saw_command = wait_for(|| {
        recorded
            .commands()
            .iter()
            .any(|c| c.contains("steered by the extension"))
    })
    .await;
    assert!(saw_command, "the command routed: {:?}", recorded.commands());
    assert_eq!(recorded.commands().len(), 1, "exactly one command routed");
    let saw_ask = wait_for(|| {
        recorded.events().iter().any(|e| {
            e.starts_with("grammar-ext|") && e.contains("interaction_request") && e.contains("g-1")
        })
    })
    .await;
    assert!(
        saw_ask,
        "the ask emission routed, origin-stamped: {:?}",
        recorded.events()
    );

    // Broadcast honors the watch list: a watched kind mirrors (the
    // double echoes it back out), an unwatched kind does not.
    let watched = tabit_protocol::EventFrame {
        stream: None,
        origin: None,
        event: tabit_protocol::SessionEvent::SessionOpened {
            id: "0198".to_string(),
            path: String::new(),
            cwd: String::new(),
            model: tabit_protocol::ModelSelection::new("p", "m"),
            resumed: false,
            parent: None,
            parent_call: None,
        },
    };
    supervisor.broadcast(&watched);
    let unwatched = tabit_protocol::EventFrame {
        stream: None,
        origin: None,
        event: tabit_protocol::SessionEvent::CompactionBegin,
    };
    supervisor.broadcast(&unwatched);
    let echoed = wait_for(|| {
        recorded
            .events()
            .iter()
            .any(|e| e.contains("session_opened") && e.contains("0198"))
    })
    .await;
    assert!(
        echoed,
        "the watched kind mirrored back: {:?}",
        recorded.events()
    );
    assert!(
        !wait_for_short(|| recorded
            .events()
            .iter()
            .any(|e| e.contains("compaction_begin")))
        .await,
        "the unwatched kind never mirrors"
    );

    // The answer routes back by id, and settlement is announced.
    assert!(supervisor.asks().respond(
        "g-1",
        serde_json::json!({"selected": [], "text": "go ahead"})
    ));
    let settled = wait_for(|| {
        recorded
            .events()
            .iter()
            .any(|e| e.contains("interaction_settled") && e.contains("g-1"))
    })
    .await;
    assert!(settled, "settlement announced: {:?}", recorded.events());
    let answered = wait_for(|| {
        recorded
            .events()
            .iter()
            .any(|e| e.contains("interaction_response") && e.contains("go ahead"))
    })
    .await;
    assert!(
        answered,
        "the routed answer crossed the pipe (mirrored by the double): {:?}",
        recorded.events()
    );

    // Death settles the asks: none are open, but the sweep is the
    // contract — clear_extension on a live registry is quiet.
    supervisor.asks().clear_extension("grammar-ext");
    supervisor.shutdown().await;
}

/// Poll until true, bounded by the test bound.
async fn wait_for(check: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + BOUND;
    while std::time::Instant::now() < deadline {
        if check() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

/// The negative poll: nothing should appear within a short window.
async fn wait_for_short(check: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
    while std::time::Instant::now() < deadline {
        if check() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}
