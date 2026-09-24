//! The routing layer's real-pipe net: a stub node — the wire-level
//! test double, a process running the node runtime with a trivial
//! functional layer — spawned over real stdio, the same laws the
//! in-process net (`src/node_tests.rs`) pins, now over pipes. The
//! test binary re-execs itself in the child role: `TABIT_STUB_NODE`
//! selects the stub test, every other test returns immediately in
//! the child.

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

use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tabit_protocol::{EventFrame, SessionCommand, SessionEvent, StreamId};
use tabit_wire::client::ChildSpec;
use tabit_wire::node::{Channel, Inbound, Node, parse_shared};

fn stub_mode() -> bool {
    std::env::var("TABIT_STUB_NODE").is_ok()
}

/// Poll until the check holds, bounded — pipe answers cross
/// processes; the test must not hang on a silent one.
fn wait_for(check: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// The stub node's role: run the node runtime over stdio with a
/// trivial functional layer — announce the session at startup (the
/// parent learns), answer every arriving ask (the promise
/// round-trip's far end), and echo every session-addressed message
/// (the learning walk's proof). EOF on stdin is the end.
#[test]
fn stub_node_role() {
    if !stub_mode() {
        return; // the parent side's run; only children take the role
    }
    let node = Arc::new(Node::new("stub"));

    // Everything the stub emits relays upstream, one pipe out.
    let to_parent = Channel::line("parent", {
        let stdout = std::io::stdout();
        move |line: &str| {
            let mut handle = stdout.lock();
            let _ = writeln!(handle, "{line}");
            let _ = handle.flush();
        }
    });
    node.subscribe_channel_all(&to_parent);

    // The layer: its mailbox is the learned route for the stub's own
    // session (the startup emission teaches it), and a received
    // message echoes as an event.
    let self_channel: Arc<OnceLock<Channel>> = Arc::new(OnceLock::new());
    let holder = self_channel.clone();
    let echo_node = node.clone();
    let layer = Channel::local(
        "stub-layer",
        move |_frame: &EventFrame| {},
        move |command: &SessionCommand| {
            let SessionCommand::Message { session, text } = command else {
                return;
            };
            if let Some(layer) = holder.get() {
                echo_node.emit(
                    layer,
                    EventFrame {
                        stream: Some(StreamId::new(session.clone())),
                        origin: None,
                        ttl: None,
                        event: SessionEvent::error_session(format!("stub got: {text}")),
                    },
                );
            }
        },
    );
    let _ = self_channel.set(layer.clone());

    // The auto-answer: every arriving ask is answered.
    let answer_node = node.clone();
    let answer_upstream = to_parent.clone();
    node.subscribe("interaction_request", "stub", move |frame: &EventFrame| {
        let SessionEvent::InteractionRequest { id, .. } = &frame.event else {
            return;
        };
        answer_node.intake(
            &answer_upstream,
            Inbound::Command(SessionCommand::InteractionResponse {
                session: None,
                id: id.clone(),
                payload: json!({"text": "from the stub"}),
            }),
        );
    });

    // Startup announcement: the parent learns the stub's session.
    node.emit(
        &layer,
        EventFrame {
            stream: Some(StreamId::new("stub-sess")),
            origin: None,
            ttl: None,
            event: SessionEvent::error_session("stub session opened".to_string()),
        },
    );

    // The pump: stdin lines in, through the intake, until EOF.
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if let Some(inbound) = parse_shared(&line) {
            node.intake(&to_parent, inbound);
        }
    }
}

/// The parent side of the real-pipe net: a node, the spawned stub,
/// and the recorder of everything the parent hears. Dropping it
/// reaps the stub.
struct PipeNet {
    parent: Arc<Node>,
    saw: Arc<Mutex<Vec<String>>>,
    child: std::process::Child,
}

impl Drop for PipeNet {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_stub() -> PipeNet {
    let parent = Arc::new(Node::new("parent"));
    let saw: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // The recorder: everything the parent hears, as its wire line.
    let sink = saw.clone();
    parent.subscribe_all("recorder", move |frame: &EventFrame| {
        sink.lock()
            .expect("test lock")
            .push(tabit_protocol::to_wire_line(frame));
    });

    let mut child = Command::new(std::env::current_exe().expect("this binary"))
        .env("TABIT_STUB_NODE", "1")
        .args(["--exact", "stub_node_role", "--nocapture"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the stub spawns");
    let stdin = child.stdin.take().expect("the stub's stdin");
    let stdout = child.stdout.take().expect("the stub's stdout");

    // The parent's route to the stub: shared-grammar lines down its
    // stdin. Downstream deliveries are subscription-driven — the
    // stub "watches" the ask kind — while answers and commands
    // route by the tables (route-all downstream would echo-loop:
    // the stub relays everything back up).
    let stub_at_parent = {
        let stdin = Mutex::new(stdin);
        Channel::line("stub", move |line: &str| {
            let mut handle = stdin.lock().expect("stdin");
            let _ = writeln!(handle, "{line}");
            let _ = handle.flush();
        })
    };
    parent.subscribe_channel("interaction_request", &stub_at_parent);

    // The reader: the stub's stdout lines arrive through the intake.
    let reader_node = parent.clone();
    let reader_channel = stub_at_parent.clone();
    std::thread::spawn(move || {
        let stdin = std::io::BufReader::new(stdout);
        for line in stdin.lines() {
            let Ok(line) = line else { break };
            if let Some(inbound) = parse_shared(&line) {
                reader_node.intake(&reader_channel, inbound);
            }
        }
    });

    PipeNet { parent, saw, child }
}

/// Laws 1, 2, 4, and 5 over real pipes: the stub's startup emission
/// arrives attributed and teaches the parent; a session-addressed
/// command walks to the stub and its echo walks back; an ask
/// round-trips to the stub's auto-answer and the promise resolves.
#[test]
fn the_net_laws_hold_over_real_pipes() {
    if stub_mode() {
        return; // the child's run of this same test
    }
    let net = spawn_stub();

    // Law 1: the startup announcement arrived. It is stamped — so it
    // crosses verbatim (attribution is for unstamped emissions, the
    // in-process net's law).
    assert!(
        wait_for(|| {
            net.saw
                .lock()
                .expect("test lock")
                .iter()
                .any(|seen| seen.contains("stub session opened") && seen.contains("stub-sess"))
        }),
        "the startup emission arrived: {:?}",
        net.saw.lock().expect("test lock")
    );

    // Law 2's walk: a message to the stub's learned session reaches
    // the stub, and its echo event walks back up.
    net.parent.intake(
        &Channel::local("frontend", |_| {}, |_| {}),
        Inbound::Command(SessionCommand::Message {
            session: "stub-sess".to_string(),
            text: "over the pipe".to_string(),
        }),
    );
    assert!(
        wait_for(|| {
            net.saw
                .lock()
                .expect("test lock")
                .iter()
                .any(|seen| seen.contains("stub got: over the pipe"))
        }),
        "the command walked down and the echo walked up: {:?}",
        net.saw.lock().expect("test lock")
    );

    // Laws 4 and 5: the ask crosses the pipe; the stub answers; the
    // promise resolves; the settle announces.
    let awaiter = net.parent.ask(
        "frontend",
        &StreamId::new("stub-sess"),
        "native:select_any",
        json!({"body": "asked over a pipe"}),
    );
    let answered = Arc::new(Mutex::new(None::<Value>));
    let sink = answered.clone();
    std::thread::spawn(move || {
        *sink.lock().expect("test lock") = awaiter.blocking_recv().ok();
    });
    assert!(
        wait_for(|| {
            *answered.lock().expect("test lock") == Some(json!({"text": "from the stub"}))
        }),
        "the ask round-tripped the pipe: {:?} / {:?}",
        answered.lock().expect("test lock"),
        net.saw.lock().expect("test lock")
    );
    assert!(
        wait_for(|| net
            .saw
            .lock()
            .expect("test lock")
            .iter()
            .any(|seen| seen.contains("interaction_settled"))),
        "the settle announced: {:?}",
        net.saw.lock().expect("test lock")
    );

    // Loop liveness: the mirror (ask kind down) against the stub's
    // relay-everything-up must not echo — the ingress law holds it.
    // One ask crosses the pipe exactly once each way, and stays that
    // way (the first review's suite was green with a livelock
    // hiding in exactly this wiring).
    let requests = || {
        net.saw
            .lock()
            .expect("test lock")
            .iter()
            .filter(|seen| seen.contains("interaction_request"))
            .count()
    };
    std::thread::sleep(Duration::from_millis(150));
    let first_count = requests();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        requests(),
        first_count,
        "no echo growth: the net is stable after the settle"
    );
}

/// The built `stub_child` example's path — a sibling of this test
/// binary under the active target dir (robust to `--target-dir`
/// overrides). `None` when the example was not built.
fn stub_child_exe() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) {
        "stub_child.exe"
    } else {
        "stub_child"
    };
    let examples = exe.parent()?.parent()?.join("examples");
    let stub = examples.join(name);
    stub.is_file().then_some(stub)
}

/// The client's mount invariant, deterministically exposed: a child
/// whose first stamped frame shares the pipe read with the handshake
/// ack must still see that frame reach the node's fan. The
/// caller-assembled mount this test once reproduced (a OnceLock lane
/// set after `spawn` returned) dropped the burst — the pump read and
/// tapped it inside the same buffer the ack came in, before the
/// caller's set could run, three runs out of three. The fix is the
/// mount's home: [`ChildSpec::on_node`] arms the lane inside the
/// pump at the handshake's resolution, so nothing can beat it.
#[test]
fn the_childs_burst_frame_reaches_the_mounted_lane() {
    let Some(stub) = stub_child_exe() else {
        eprintln!(
            "client burst test: no stub_child example built - run the workspace suite to cover it"
        );
        return;
    };
    let parent: Arc<Node> = Arc::new(Node::new("parent"));
    let saw: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = saw.clone();
    parent.subscribe_all("recorder", move |frame: &EventFrame| {
        sink.lock().expect("test lock").push(format!(
            "{}@{}",
            frame.event.tag(),
            frame
                .stream
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or_default()
        ));
    });

    let spec = ChildSpec::new(stub, std::env::temp_dir()).on_node(parent.clone());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the test runtime");
    // Everything inside one block_on: the current-thread runtime must
    // stay driven for the pump to poll while the test waits.
    let (reached, report) = runtime.block_on(async move {
        let mut handle = spec.spawn().await.expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(10);
        let reached = loop {
            if saw
                .lock()
                .expect("test lock")
                .iter()
                .any(|seen| seen.contains("error") && seen.ends_with("stub-sess"))
            {
                break true;
            }
            if Instant::now() > deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        handle.close();
        let _ = handle.wait_exit().await;
        (reached, saw.lock().expect("test lock").clone())
    });
    assert!(
        reached,
        "the burst frame reached the node's fan: {report:?}"
    );
}
