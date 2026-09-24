//! The routing layer's net test — the ruling's acceptance test
//! (2026-09): stub functional layers wired into a multi-node net,
//! every routing law pinned at the layer, independent of any real
//! functional layer. The in-process net holds the law; the real-pipe
//! net (`tests/net.rs`) holds the same laws over stdio.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use tabit_protocol::{EventFrame, SessionCommand, SessionEvent, StreamId, tags, to_wire_line};

use crate::node::{Channel, Inbound, Node, parse_shared};

/// What one stub functional layer saw: the events its all-kinds
/// subscription heard, and the commands its learned mailbox received.
#[derive(Default, Clone)]
struct Saw {
    events: Arc<Mutex<Vec<String>>>,
    commands: Arc<Mutex<Vec<String>>>,
}

impl Saw {
    fn events(&self) -> Vec<String> {
        self.events.lock().expect("test lock").clone()
    }
    fn commands(&self) -> Vec<String> {
        self.commands.lock().expect("test lock").clone()
    }
    fn has(&self, what: &str) -> bool {
        self.events().iter().any(|seen| seen.contains(what))
    }
}

/// The stub functional layer: a local channel that subscribes to
/// every event kind and records, with a mailbox the learning table
/// routes into. Returns the channel and the recording.
fn stub_layer(node: &Node, name: &str) -> (Channel, Saw) {
    let saw = Saw::default();
    let events = saw.events.clone();
    let commands = saw.commands.clone();
    let channel = Channel::local(
        name,
        move |frame| {
            let note = match &frame.event {
                SessionEvent::InteractionSettled { id } => {
                    format!("settled:{id}@{}", stamp(&frame.stream))
                }
                SessionEvent::InteractionRequest { id, .. } => {
                    format!("interaction_request:{id}@{}", stamp(&frame.stream))
                }
                SessionEvent::Error { kind, .. } => {
                    format!("error:{kind}@{}", stamp(&frame.stream))
                }
                event => format!("{}@{}", event.tag(), stamp(&frame.stream)),
            };
            events.lock().expect("test lock").push(note);
        },
        move |command| {
            commands
                .lock()
                .expect("test lock")
                .push(command.tag().to_string())
        },
    );
    node.subscribe_channel_all(&channel);
    (channel, saw)
}

/// The id of the first surfaced request — the honest client shape:
/// ask ids are UUIDs, learnable only from the request frames.
fn request_id(saw: &Saw) -> String {
    saw.events()
        .iter()
        .find_map(|note| {
            note.strip_prefix("interaction_request:")
                .and_then(|rest| rest.split('@').next())
        })
        .expect("a request surfaced")
        .to_string()
}

fn stamp(stream: &Option<StreamId>) -> String {
    stream
        .as_ref()
        .map(StreamId::as_str)
        .unwrap_or("-")
        .to_string()
}

/// Wire two nodes: the child relays everything upstream; anything
/// either side writes crosses as a shared-grammar line through the
/// other's intake. Returns the child's channel at the parent (the
/// parent's route to the child).
fn wire(parent: &Arc<Node>, child: &Arc<Node>, name: &str) -> Channel {
    let upstream: Arc<OnceLock<Channel>> = Arc::new(OnceLock::new());

    let holder = upstream.clone();
    let child_node = child.clone();
    let child_at_parent = Channel::line(name, move |line| {
        if let (Some(parent_at_child), Some(inbound)) = (holder.get(), parse_shared(line)) {
            child_node.intake(parent_at_child, inbound);
        }
    });

    let parent_node = parent.clone();
    let child_side = child_at_parent.clone();
    let parent_at_child = Channel::line("parent", move |line| {
        if let Some(inbound) = parse_shared(line) {
            parent_node.intake(&child_side, inbound);
        }
    });
    let _ = upstream.set(parent_at_child.clone());

    // The child's policy: relay everything upstream.
    child.subscribe_channel_all(&parent_at_child);
    child_at_parent
}

/// Any stamped event teaches; the tests use a one-field one.
fn stamped(session: &str) -> EventFrame {
    EventFrame {
        stream: Some(StreamId::new(session)),
        origin: None,
        ttl: None,
        event: SessionEvent::error_session(format!("the {session} stream's first emission")),
    }
}

/// Law 1: events fan by kind, subscribers compose, and unstamped
/// frames carry no session (nothing to learn).
#[test]
fn events_fan_by_type_and_compose() {
    let node = Arc::new(Node::new("core"));
    let (layer, saw) = stub_layer(&node, "layer");

    let seen: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let kind_sink = seen.clone();
    node.subscribe(tags::ERROR, "watcher", move |_| {
        kind_sink.lock().expect("test lock").push("kind")
    });
    let relay_sink = seen.clone();
    node.subscribe_all("relay", move |_| {
        relay_sink.lock().expect("test lock").push("wildcard")
    });

    node.emit(&layer, stamped("s-1"));
    node.emit(
        &layer,
        EventFrame {
            stream: Some(StreamId::new("s-1")),
            origin: None,
            ttl: None,
            event: SessionEvent::RunFinished {
                output: String::new(),
                started_at_ms: 0,
                completed_at_ms: 0,
                durable: false,
            },
        },
    );
    assert_eq!(
        seen.lock().expect("test lock").clone(),
        vec!["kind", "wildcard", "wildcard"],
        "the matching kind: kind + wildcard compose; the other kind: wildcard only"
    );
    assert_eq!(saw.events().len(), 2, "the all-kinds stub heard both");
}

/// Laws 1 and 2: a child's stamped emission teaches the parent, and
/// session-addressed commands route by the learning table — on the
/// emitting node too (its own layer's mailbox is the learned route).
#[test]
fn stamped_emissions_teach_and_commands_route_by_learning() {
    let parent = Arc::new(Node::new("parent"));
    let child = Arc::new(Node::new("child"));
    let child_at_parent = wire(&parent, &child, "child");

    let (child_layer, child_saw) = stub_layer(&child, "child-layer");
    child.emit(&child_layer, stamped("sess-child"));

    let (parent_layer, parent_saw) = stub_layer(&parent, "parent-layer");
    parent.emit(&parent_layer, stamped("sess-local"));

    parent.intake(
        &child_at_parent,
        Inbound::Command(SessionCommand::Message {
            session: "sess-child".to_string(),
            text: "for the child".to_string(),
        }),
    );
    parent.intake(
        &parent_layer,
        Inbound::Command(SessionCommand::Abort {
            session: "sess-local".to_string(),
        }),
    );

    assert_eq!(
        child_saw.commands(),
        vec!["message"],
        "the child's layer mailbox received the routed command"
    );
    assert_eq!(
        parent_saw.commands(),
        vec!["abort"],
        "the parent's own session routed to its own layer"
    );
}

/// Law 2's walk: commands addressed to a grandchild cross hop by hop
/// — each node routes by what it learned, no node knows the tree.
#[test]
fn commands_walk_hop_by_hop() {
    let root = Arc::new(Node::new("root"));
    let mid = Arc::new(Node::new("mid"));
    let leaf = Arc::new(Node::new("leaf"));

    let mid_at_root = wire(&root, &mid, "mid");
    let _leaf_at_mid = wire(&mid, &leaf, "leaf");

    let (leaf_layer, leaf_saw) = stub_layer(&leaf, "leaf-layer");
    leaf.emit(&leaf_layer, stamped("sess-leaf"));

    root.intake(
        &mid_at_root,
        Inbound::Command(SessionCommand::Message {
            session: "sess-leaf".to_string(),
            text: "grandchild".to_string(),
        }),
    );

    assert_eq!(
        leaf_saw.commands(),
        vec!["message"],
        "the leaf's layer received the walked command"
    );
}

/// Laws 4, 5, and 6: a local asker holds a promise; the response
/// claims the table entry wherever it arrives; the settle is
/// announced; the late answer drops.
#[test]
fn a_local_ask_awaits_its_promise_and_the_late_answer_drops() {
    let node = Arc::new(Node::new("core"));
    let (layer, saw) = stub_layer(&node, "layer");

    let awaiter = node.ask(
        "layer",
        &StreamId::new("s-1"),
        "native:select_one",
        json!({"body": "allow this call?"}),
    );
    assert_eq!(
        saw.events(),
        vec![format!("interaction_request:{}@s-1", request_id(&saw))],
        "the request surfaced, stamped with the asking stream"
    );

    // The answer arrives from anywhere — here, the same layer. The id
    // is learnable only from the request frame (a UUID mint).
    let id = request_id(&saw);
    node.intake(
        &layer,
        Inbound::Command(SessionCommand::InteractionResponse {
            session: None,
            id: id.clone(),
            payload: json!({"selected": ["Allow"]}),
        }),
    );
    let answer: Value = awaiter.blocking_recv().expect("the promise resolved");
    assert_eq!(answer, json!({"selected": ["Allow"]}));
    assert!(
        saw.has(&format!("settled:{id}@s-1")),
        "the settle announced, stamped with the asking stream: {:?}",
        saw.events()
    );

    // The race's loser: a second answer for the gone id drops —
    // no second resolution, no second settle.
    let before = saw.events().len();
    node.intake(
        &layer,
        Inbound::Command(SessionCommand::InteractionResponse {
            session: None,
            id: request_id(&saw),
            payload: json!({"selected": ["Deny"]}),
        }),
    );
    assert_eq!(
        saw.events().len(),
        before,
        "the late answer changed nothing"
    );
}

/// Laws 4 and 5 across the net: an ask arriving on a channel
/// registers against it — the answer, given anywhere, routes home
/// down the same pipe and resolves the asker's promise at the far
/// node.
#[test]
fn an_arriving_ask_routes_its_answer_home() {
    let parent = Arc::new(Node::new("parent"));
    let child = Arc::new(Node::new("child"));
    let child_at_parent = wire(&parent, &child, "child");

    let (_parent_layer, parent_saw) = stub_layer(&parent, "parent-layer");
    let (child_layer, _child_saw) = stub_layer(&child, "child-layer");

    let awaiter = child.ask(
        "child-layer",
        &StreamId::new("sess-child"),
        "native:select_any",
        json!({"body": "from the child"}),
    );
    assert!(
        parent_saw
            .events()
            .iter()
            .any(|seen| seen.starts_with("interaction_request:") && seen.ends_with("@sess-child")),
        "the ask surfaced at the parent: {:?}",
        parent_saw.events()
    );

    // The parent answers; the id routes the answer down the child's
    // pipe and into the child's ask table (the promise).
    parent.intake(
        &child_at_parent,
        Inbound::Command(SessionCommand::InteractionResponse {
            session: None,
            id: request_id(&parent_saw),
            payload: json!({"text": "yes"}),
        }),
    );
    let answer: Value = awaiter
        .blocking_recv()
        .expect("the promise crossed the net");
    assert_eq!(answer, json!({"text": "yes"}));
    assert!(
        parent_saw.has(&format!("settled:{}", request_id(&parent_saw))),
        "the parent announced the settle it resolved: {:?}",
        parent_saw.events()
    );
    let _ = child_layer;
}

/// The death sweep: subscriptions, learned routes, and open asks go
/// by owner; the orphaned ask settles announced; the swept route
/// now misses with the uniform error.
#[test]
fn a_death_sweeps_routes_subscriptions_and_asks() {
    let node = Arc::new(Node::new("core"));
    let child = Arc::new(Node::new("child"));
    let child_at_parent = wire(&node, &child, "child");

    let (layer, saw) = stub_layer(&node, "layer");
    let (child_layer, _child_saw) = stub_layer(&child, "child-layer");
    child.emit(&child_layer, stamped("sess-child"));
    let _open = child.ask(
        "child-layer",
        &StreamId::new("sess-child"),
        "native:select_any",
        json!({}),
    );

    node.retract("child", "the child process exited");
    assert!(
        saw.has(&format!("settled:{}", request_id(&saw))),
        "the orphaned ask settled, announced: {:?}",
        saw.events()
    );

    node.intake(
        &child_at_parent,
        Inbound::Command(SessionCommand::Message {
            session: "sess-child".to_string(),
            text: "to a dead child".to_string(),
        }),
    );
    assert!(
        saw.has("error:session@-"),
        "the swept route misses uniformly, unstamped and session-kind (no session owns it): {:?}",
        saw.events()
    );
    let _ = layer;
}

/// Law 3: non-session commands dispatch by type to the functional
/// layer's handlers; the unhandled drop.
#[test]
fn non_session_commands_dispatch_by_type() {
    let node = Arc::new(Node::new("core"));
    let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = handled.clone();
    node.handle("new_session", "core", move |command: &SessionCommand| {
        sink.lock()
            .expect("test lock")
            .push(command.tag().to_string());
    });
    let edge = Channel::local("edge", |_| {}, |_| {});

    node.intake(&edge, Inbound::Command(SessionCommand::NewSession));
    node.intake(
        &edge,
        Inbound::Command(SessionCommand::OpenSession {
            id: "s".to_string(),
        }),
    );
    assert_eq!(
        handled.lock().expect("test lock").clone(),
        vec!["new_session"],
        "only the handled type ran; the unhandled one dropped"
    );
}

/// The one-intake mount: `handle_all` sees every command type — the
/// functional layer that prefers its own dispatch over per-type
/// handlers.
#[test]
fn a_one_intake_layer_handles_every_command_type() {
    let node: Arc<Node> = Arc::new(Node::new("core"));
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    node.handle_all("layer", move |command: &SessionCommand| {
        sink.lock()
            .expect("test lock")
            .push(command.tag().to_string());
    });
    let edge = Channel::local("edge", |_| {}, |_| {});

    node.intake(&edge, Inbound::Command(SessionCommand::NewSession));
    node.intake(
        &edge,
        Inbound::Command(SessionCommand::OpenSession {
            id: "s".to_string(),
        }),
    );
    assert_eq!(
        seen.lock().expect("test lock").clone(),
        vec!["new_session", "open_session"],
        "the catch-all heard both non-session commands"
    );
}

/// Attribution, not permission: an unstamped emission names its
/// speaker as it crosses into a node; a stamped frame crosses
/// verbatim (someone else's traffic).
#[test]
fn unstamped_arrivals_are_attributed_and_stamped_cross_verbatim() {
    let parent = Arc::new(Node::new("parent"));
    let child = Arc::new(Node::new("child"));
    let _child_at_parent = wire(&parent, &child, "child");

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    parent.subscribe_all("recorder", move |frame| {
        let origin = frame.origin.clone().unwrap_or_else(|| "-".to_string());
        sink.lock()
            .expect("test lock")
            .push(format!("{}:{}", frame.event.tag(), origin));
    });

    let (child_layer, _child_saw) = stub_layer(&child, "child-layer");
    // An unstamped emission: attributed to the child when it crosses.
    child.emit(
        &child_layer,
        EventFrame {
            stream: None,
            origin: None,
            ttl: None,
            event: SessionEvent::error_session("an emission".to_string()),
        },
    );
    // Stamped traffic: someone else's — verbatim, no attribution.
    child.emit(&child_layer, stamped("sess-grand"));

    let seen = seen.lock().expect("test lock").clone();
    assert_eq!(
        seen,
        vec!["error:child".to_string(), "error:-".to_string()],
        "the emission names its speaker; the relay stays verbatim"
    );
}

/// The correlation-kind law: an ask held for one response tag,
/// answered by another, is consumed loudly — never delivered to a
/// closure expecting the wrong shape (an external contract break
/// stays external; the host does not crash on it).
#[test]
fn a_wrong_kind_answer_breaks_loudly_not_fatal() {
    let node = Arc::new(Node::new("core"));
    let (layer, saw) = stub_layer(&node, "layer");

    let delivered = Arc::new(Mutex::new(false));
    let sink = delivered.clone();
    node.hold("layer", "call-1", "tool_result", move |outcome| {
        if let crate::asks::Outcome::Answered(_) = outcome {
            *sink.lock().expect("test lock") = true;
        }
    });

    // A `tool_result` question answered by an interaction response:
    // consumed, an error names the break, the delivery never runs.
    node.intake(
        &layer,
        Inbound::Command(SessionCommand::InteractionResponse {
            session: None,
            id: "call-1".to_string(),
            payload: json!({"text": "wrong shape"}),
        }),
    );
    assert!(
        !*delivered.lock().expect("test lock"),
        "the wrong-kind answer was never delivered"
    );
    assert!(
        saw.has("error:session@-"),
        "the break is loud, session-kind, unstamped: {:?}",
        saw.events()
    );

    // And the entry is gone: the right answer now finds nothing.
    let before = saw.events().len();
    node.intake(
        &layer,
        Inbound::Command(SessionCommand::InteractionResponse {
            session: None,
            id: "call-1".to_string(),
            payload: json!({}),
        }),
    );
    assert_eq!(saw.events().len(), before, "the consumed entry is gone");
}

/// Law 4's collision rule (2026-09 ruling): an ask's echo
/// re-arriving finds the entry live and PANICS — a mint-law
/// violation, surfaced, never masked (the TTL law kills accidental
/// echo loops before they ever reach this).
#[test]
#[should_panic(expected = "the mint law was violated")]
fn an_ask_echo_re_registered_panics() {
    let node: Arc<Node> = Arc::new(Node::new("core"));
    let first_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let second_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let sink = first_lines.clone();
    let first = Channel::line("ext-a", move |line: &str| {
        sink.lock().expect("test lock").push(line.to_string());
    });
    let sink = second_lines.clone();
    let second = Channel::line("ext-b", move |line: &str| {
        sink.lock().expect("test lock").push(line.to_string());
    });

    let ask = EventFrame {
        stream: None,
        origin: None,
        ttl: None,
        event: SessionEvent::InteractionRequest {
            id: "ext-a-ask-1".to_string(),
            ui_type: "native:select_any".to_string(),
            payload: json!({}),
        },
    };
    // The ask arrives; its echo re-arrives on the other channel —
    // the live id re-registers, the sanctioned crash.
    node.intake(&first, Inbound::Event(ask.clone()));
    node.intake(&second, Inbound::Event(ask));
}

/// Law 1's ingress half: a mirrored frame arriving back on the pipe
/// it came from does not fan out through that pipe again — the
/// mirror-and-relay wiring cannot echo-loop.
#[test]
fn the_ingress_law_breaks_mirror_relay_loops() {
    let parent = Arc::new(Node::new("parent"));
    let child = Arc::new(Node::new("child"));
    let child_at_parent = wire(&parent, &child, "child");

    // The pathological wiring the net test found: the parent mirrors
    // the ask kind down, the child relays everything up.
    parent.subscribe_channel("interaction_request", &child_at_parent);

    let (_parent_layer, parent_saw) = stub_layer(&parent, "parent-layer");
    let (_child_layer, _child_saw) = stub_layer(&child, "child-layer");
    drop(child.ask(
        "child-layer",
        &StreamId::new("sess-child"),
        "native:select_any",
        json!({"body": "asked"}),
    ));

    // Without the ingress law the request ping-ponged unboundedly;
    // with it, the parent sees the ask exactly once.
    std::thread::sleep(Duration::from_millis(50));
    let requests = parent_saw
        .events()
        .iter()
        .filter(|seen| seen.starts_with("interaction_request:"))
        .count();
    assert_eq!(
        requests,
        1,
        "the mirrored ask came back once, not in a loop: {:?}",
        parent_saw.events()
    );
}

/// The two deaths: a run dying retracts its questions but not the
/// participant's routes; a participant dying takes everything. The
/// live asker reads the run-death sweep as dismissal (law 6: the
/// promise resolves Err when its closure drops unresolved).
#[test]
fn a_run_death_retracts_asks_but_keeps_routes() {
    let node = Arc::new(Node::new("core"));
    let (layer, saw) = stub_layer(&node, "layer");
    node.emit(&layer, stamped("sess-1"));

    let awaiter = node.ask(
        "run-1",
        &StreamId::new("sess-1"),
        "native:select_any",
        json!({}),
    );

    node.retract_asks("run-1", "the run ended");
    assert!(
        awaiter.blocking_recv().is_err(),
        "the live asker reads dismissal — the sweep resolved the promise by dropping it"
    );
    assert!(
        saw.has(&format!("settled:{}", request_id(&saw))),
        "the run's question settled, announced by the sweep: {:?}",
        saw.events()
    );

    // The route outlives the run: a session command still walks.
    node.intake(
        &Channel::local("frontend", |_| {}, |_| {}),
        Inbound::Command(SessionCommand::Abort {
            session: "sess-1".to_string(),
        }),
    );
    assert_eq!(
        saw.commands(),
        vec!["abort"],
        "the layer's route survived the run's death"
    );
}

/// Law 5's routing half: the origin's settle, passing through a node
/// that still holds a routed entry for the ask, clears it — a late
/// answer at that node finds nothing and drops.
#[test]
fn a_routing_settle_clears_entries_on_arrival() {
    let parent = Arc::new(Node::new("parent"));
    let child = Arc::new(Node::new("child"));
    let _child_at_parent = wire(&parent, &child, "child");

    let (parent_layer, parent_saw) = stub_layer(&parent, "parent-layer");
    let (_child_layer, _child_saw) = stub_layer(&child, "child-layer");

    // The child asks; the parent's routed entry exists; the origin's
    // run dies — the sweep's settle routes up through the parent,
    // clearing its routed entry.
    drop(child.ask(
        "child-layer",
        &StreamId::new("sess-child"),
        "native:select_any",
        json!({"body": "cleared en route"}),
    ));
    child.retract_asks("child-layer", "the run ended");
    std::thread::sleep(Duration::from_millis(20));

    // A late answer at the parent: a miss (the entry cleared), not a
    // mismatch — no error fired, nothing delivered.
    let before = parent_saw.events().len();
    parent.intake(
        &parent_layer,
        Inbound::Command(SessionCommand::InteractionResponse {
            session: None,
            id: request_id(&parent_saw),
            payload: json!({"text": "too late"}),
        }),
    );
    assert_eq!(
        parent_saw.events().len(),
        before,
        "the cleared entry made the late answer a silent drop"
    );
}

/// The TTL tripwire: a frame forwarded in a cycle across two nodes
/// dies at the hop budget — normally it never fires. The wiring is
/// the load-bearing part: each node's FORWARDING channel (its
/// subscriber) is a different object than the channel its intake
/// sees as the arrival (a distinct owner), so the ingress law
/// cannot break the cycle — only the budget can.
#[test]
fn the_ttl_tripwire_kills_cross_node_loops() {
    let a = Arc::new(Node::new("a"));
    let b = Arc::new(Node::new("b"));
    let laps: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));

    // The arrival channels: what each node's intake names as the
    // source (never a subscriber — the ingress skip never fires).
    let a_arrival = Channel::line("a-arr", |_| {});
    let b_arrival = Channel::line("b-arr", |_| {});

    // The forwarding channels: each node's everything-subscriber,
    // writing into the other node's intake over the arrival channel.
    let b_node = b.clone();
    let b_side = b_arrival.clone();
    let a_fwd = Channel::line("a-fwd", move |line: &str| {
        if let Some(inbound) = parse_shared(line) {
            b_node.intake(&b_side, inbound);
        }
    });
    let a_node = a.clone();
    let a_side = a_arrival.clone();
    let b_fwd = Channel::line("b-fwd", move |line: &str| {
        if let Some(inbound) = parse_shared(line) {
            a_node.intake(&a_side, inbound);
        }
    });

    let counter = laps.clone();
    a.subscribe_all("recorder", move |_| {
        *counter.lock().expect("test lock") += 1;
    });
    a.subscribe_channel_all(&a_fwd);
    b.subscribe_channel_all(&b_fwd);

    // A local emission enters the cycle; the budget bounds it.
    let (a_layer, _a_saw) = stub_layer(&a, "a-layer");
    a.emit(
        &a_layer,
        EventFrame {
            stream: None,
            origin: None,
            ttl: None,
            event: SessionEvent::error_session("into the loop".to_string()),
        },
    );

    let bounded = *laps.lock().expect("test lock");
    // One budget of 32 crossings means ~16 arrivals back at `a`
    // beside the emission — a single-lap number would mean the
    // ingress law (not the budget) broke the cycle.
    assert!(
        (5..=20).contains(&bounded),
        "the cycle genuinely ran and the budget bounded it: {bounded} laps"
    );
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(
        *laps.lock().expect("test lock"),
        bounded,
        "the cycle terminated — no residual growth"
    );
}

/// A wildcard subscriber may emit from inside dispatch (the most
/// natural functional-layer act: hear an event, derive one) — the
/// router's callback runs outside its locks.
#[test]
fn a_subscriber_may_emit_from_inside_dispatch() {
    let node = Arc::new(Node::new("core"));
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let sink = seen.clone();
    node.subscribe(tags::ERROR, "watcher", move |frame| {
        sink.lock()
            .expect("test lock")
            .push(frame.event.tag().to_string());
    });

    let (layer, _saw) = stub_layer(&node, "layer");
    {
        let emitter = node.clone();
        let layer = layer.clone();
        node.subscribe_all("deriver", move |frame| {
            // Hear an error, derive a run_finished — an emit while
            // dispatch holds the wildcard iteration.
            if matches!(frame.event, SessionEvent::Error { .. }) {
                let layer = layer.clone();
                emitter.emit(
                    &layer,
                    EventFrame {
                        stream: None,
                        origin: None,
                        ttl: None,
                        event: SessionEvent::RunFinished {
                            output: String::new(),
                            started_at_ms: 0,
                            completed_at_ms: 0,
                            durable: false,
                        },
                    },
                );
            }
        });
    }

    node.emit(
        &layer,
        EventFrame {
            stream: None,
            origin: None,
            ttl: None,
            event: SessionEvent::error_session("the trigger".to_string()),
        },
    );

    let seen = seen.lock().expect("test lock").clone();
    assert_eq!(
        seen,
        vec!["error".to_string()],
        "the derived frame fanned without deadlocking"
    );
    let _ = &layer;
}

/// The sweep speaks card vocabulary only: held round-trips (tool
/// calls, service requests) orphan silently — no settle announces
/// for a non-card.
#[test]
fn a_death_sweep_settles_cards_not_held_round_trips() {
    let node = Arc::new(Node::new("core"));
    let (layer, saw) = stub_layer(&node, "layer");

    let orphaned = Arc::new(Mutex::new(false));
    let sink = orphaned.clone();
    node.hold("lane", "call-9", "tool_result", move |outcome| {
        if let crate::asks::Outcome::Orphaned(_) = outcome {
            *sink.lock().expect("test lock") = true;
        }
    });
    // A card beside it, for contrast.
    drop(node.ask(
        "lane",
        &StreamId::new("s-1"),
        "native:select_any",
        json!({}),
    ));

    node.retract("lane", "the lane died");
    assert!(
        *orphaned.lock().expect("test lock"),
        "the held call orphaned (the site's fail-open ran)"
    );
    assert_eq!(
        saw.events()
            .iter()
            .filter(|seen| seen.starts_with("settled:"))
            .count(),
        1,
        "exactly the card settled — the held call spoke nothing: {:?}",
        saw.events()
    );
    assert!(
        saw.events()
            .iter()
            .any(|s| s.starts_with(&format!("settled:{}", request_id(&saw)))),
        "the card's settle announced: {:?}",
        saw.events()
    );
    let _ = layer;
}

/// The co-subscription rule: subscribing the request kind also
/// subscribes the settle — the card lifecycle is one interest.
#[test]
fn subscribing_requests_hears_the_settles() {
    let node = Arc::new(Node::new("core"));
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    node.subscribe("interaction_request", "cards", move |frame| {
        sink.lock()
            .expect("test lock")
            .push(frame.event.tag().to_string());
    });

    let (layer, saw) = stub_layer(&node, "layer");
    let awaiter = node.ask(
        "layer",
        &StreamId::new("s-1"),
        "native:select_any",
        json!({}),
    );
    node.intake(
        &layer,
        Inbound::Command(SessionCommand::InteractionResponse {
            session: None,
            id: request_id(&saw),
            payload: json!({"text": "yes"}),
        }),
    );
    drop(awaiter.blocking_recv());

    let seen = seen.lock().expect("test lock").clone();
    assert_eq!(
        seen,
        vec![
            "interaction_request".to_string(),
            "interaction_settled".to_string()
        ],
        "the one interest heard the card open and close"
    );
}

/// The mint law, containment option (2026-09 ruling): a sender that
/// re-registers a live ask id meets the registered policy — a host
/// with killable lanes kills the sender; the frame dies with the
/// violation; the surviving entry is untouched and still answerable.
#[test]
fn a_mint_violation_is_contained_by_policy() {
    let node: Arc<Node> = Arc::new(Node::new("core"));
    let contained: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let sink = contained.clone();
    node.on_mint_violation(move |owner, id| {
        sink.lock()
            .expect("test lock")
            .push(format!("{owner}:{id}"));
    });

    let first_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = first_lines.clone();
    let first = Channel::line("lane-a", move |line: &str| {
        sink.lock().expect("test lock").push(line.to_string());
    });
    let second = Channel::line("lane-b", |_| {});

    let ask = EventFrame {
        stream: None,
        origin: None,
        ttl: None,
        event: SessionEvent::InteractionRequest {
            id: "lane-a-ask-1".to_string(),
            ui_type: "native:select_any".to_string(),
            payload: json!({}),
        },
    };
    node.intake(&first, Inbound::Event(ask.clone()));

    // The violating re-send: contained, not fatal.
    node.intake(&second, Inbound::Event(ask));
    assert_eq!(
        contained.lock().expect("test lock").clone(),
        vec!["lane-b:lane-a-ask-1".to_string()],
        "the policy met the violator"
    );

    // The surviving entry still answers home to the first lane.
    node.intake(
        &Channel::local("frontend", |_| {}, |_| {}),
        Inbound::Command(SessionCommand::InteractionResponse {
            session: None,
            id: "lane-a-ask-1".to_string(),
            payload: json!({"text": "home"}),
        }),
    );
    assert!(
        first_lines
            .lock()
            .expect("test lock")
            .iter()
            .any(|line| line.contains("interaction_response")),
        "the original entry was untouched by the containment: {:?}",
        first_lines.lock().expect("test lock")
    );
}

/// The claim-and-discard law at the origin: a settle arriving for a
/// locally-held promise dismisses the awaiter directly — the entry is
/// claimed and dropped without running the Orphaned arm, so the node
/// announces no settle of its own (the arrival IS the settle), and
/// the dismissed entry is gone for any late answer.
#[test]
fn an_arriving_settle_dismisses_the_origin_without_reannouncing() {
    let node = Arc::new(Node::new("core"));
    let (layer, saw) = stub_layer(&node, "layer");

    let awaiter = node.ask(
        "run-1",
        &StreamId::new("s-1"),
        "native:select_any",
        json!({}),
    );
    let id = request_id(&saw);
    let open = saw.events().len();

    // The settle arrives from a channel OTHER than the watching
    // layer's — the ingress law would skip the layer it came in on.
    node.intake(
        &Channel::local("elsewhere", |_| {}, |_| {}),
        Inbound::Event(EventFrame {
            stream: Some(StreamId::new("s-1")),
            origin: None,
            ttl: None,
            event: SessionEvent::InteractionSettled { id },
        }),
    );
    assert!(
        awaiter.blocking_recv().is_err(),
        "the origin's promise reads dismissal"
    );
    assert_eq!(
        saw.events().len(),
        open + 1,
        "the arriving settle fanned once and the node derived nothing: {:?}",
        saw.events()
    );
    assert!(saw.has(&format!("settled:{}@s-1", request_id(&saw))));

    let settled_at = saw.events().len();
    node.intake(
        &layer,
        Inbound::Command(SessionCommand::InteractionResponse {
            session: None,
            id: request_id(&saw),
            payload: json!({"text": "too late"}),
        }),
    );
    assert_eq!(
        saw.events().len(),
        settled_at,
        "the late answer was the race's tolerated loser"
    );
}

/// The shared parse: an event line, a command line, and noise — the
/// intake vocabulary's whole discrimination.
#[test]
fn parse_shared_discriminates_lines() {
    let event_line = to_wire_line(&EventFrame {
        stream: None,
        origin: None,
        ttl: None,
        event: SessionEvent::error_session("an emission".to_string()),
    });
    assert!(matches!(parse_shared(&event_line), Some(Inbound::Event(_))));

    let command_line = to_wire_line(&SessionCommand::NewSession);
    assert!(matches!(
        parse_shared(&command_line),
        Some(Inbound::Command(_))
    ));

    assert!(parse_shared("not a wire line").is_none());
}

/// A hand-emitted ask arriving on a LOCAL channel has no answer route
/// home — local channels never take answer delivery (their askers
/// hold promises from `Node::ask`). The answer claims the entry and
/// delivers it nowhere: a tolerated dead end, not a corruption.
#[test]
fn a_hand_emitted_ask_on_a_local_channel_has_no_answer_home() {
    let node = Arc::new(Node::new("core"));
    let (layer, saw) = stub_layer(&node, "layer");

    node.intake(
        &layer,
        Inbound::Event(EventFrame {
            stream: Some(StreamId::new("s-1")),
            origin: None,
            ttl: None,
            event: SessionEvent::InteractionRequest {
                id: "hand-1".to_string(),
                ui_type: "native:select_any".to_string(),
                payload: json!({}),
            },
        }),
    );
    let open = saw.events().len();

    node.intake(
        &layer,
        Inbound::Command(SessionCommand::InteractionResponse {
            session: None,
            id: "hand-1".to_string(),
            payload: json!({"text": "an answer with no route"}),
        }),
    );
    assert_eq!(
        saw.events().len(),
        open,
        "the answer was claimed and delivered nowhere: {:?}",
        saw.events()
    );
}

/// The override path (2026-09 ruling): a local emission may name
/// additional receivers — delivered directly, beside the subscriber
/// fan, so a node whose stdio subscribes to nothing still speaks
/// across it.
#[test]
fn an_emission_can_name_additional_receivers() {
    let node = Arc::new(Node::new("ext"));
    let (layer, saw) = stub_layer(&node, "layer");

    // The stdio: a line channel subscribed to NOTHING (the opt-in
    // ruling — hearing only).
    let up_the_pipe: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = up_the_pipe.clone();
    let stdio = Channel::line("stdio", move |line: &str| {
        sink.lock().expect("test lock").push(line.to_string());
    });

    node.emit_to(
        &layer,
        &[stdio],
        EventFrame {
            stream: None,
            origin: None,
            ttl: None,
            event: SessionEvent::error_session("the ext speaks".to_string()),
        },
    );

    // It crossed the pipe AND the fan reached the subscriber.
    assert_eq!(up_the_pipe.lock().expect("test lock").len(), 1);
    assert!(
        saw.has("error:session@-"),
        "the fan is untouched by the override: {:?}",
        saw.events()
    );
}

/// The dedup: a channel that would also hear via subscription —
/// because it is one — receives exactly once.
#[test]
fn additional_receivers_dedupe_against_subscriptions() {
    let node = Arc::new(Node::new("core"));
    let (layer, _saw) = stub_layer(&node, "layer");

    let heard: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = heard.clone();
    let both = Channel::local(
        "both",
        move |frame: &EventFrame| {
            sink.lock()
                .expect("test lock")
                .push(frame.event.tag().to_string());
        },
        |_| {},
    );
    {
        let deliver = heard.clone();
        node.subscribe(tags::ERROR, "both", move |frame: &EventFrame| {
            deliver
                .lock()
                .expect("test lock")
                .push(frame.event.tag().to_string());
        });
    }

    node.emit_to(
        &layer,
        &[both],
        EventFrame {
            stream: None,
            origin: None,
            ttl: None,
            event: SessionEvent::error_session("once".to_string()),
        },
    );
    assert_eq!(
        heard.lock().expect("test lock").len(),
        1,
        "a channel that hears via subscription AND is named additional sees the frame once"
    );
}

/// The ruling's whole scenario at an extension-shaped node: the
/// stdio subscribes to nothing, so a child's arrivals do not
/// auto-cross it — while the layer's own speech (and its manual
/// forward of a captured frame) leaves by naming it.
#[test]
fn an_extension_shaped_node_speaks_but_does_not_relay() {
    let node: Arc<Node> = Arc::new(Node::new("ext"));

    let up_the_pipe: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = up_the_pipe.clone();
    let stdio = Channel::line("stdio", move |line: &str| {
        sink.lock().expect("test lock").push(line.to_string());
    });

    // The layer captures message-shaped events (its opt-in watch) and
    // holds the stdio for its own speech.
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    node.subscribe("text_delta", "capture", move |frame: &EventFrame| {
        let note = match &frame.event {
            SessionEvent::TextDelta { text, .. } => text.clone(),
            event => event.tag().to_string(),
        };
        sink.lock().expect("test lock").push(note);
    });

    // A child's frame arrives from its lane: the capture hears it,
    // the stdio (subscribed to nothing) does not carry it up.
    let child_lane = Channel::line("child", |_| {});
    node.intake(
        &child_lane,
        Inbound::Event(EventFrame {
            stream: Some(StreamId::new("child-sess")),
            origin: None,
            ttl: None,
            event: SessionEvent::TextDelta {
                turn_id: "t".to_string(),
                text: "the child streams".to_string(),
            },
        }),
    );
    assert_eq!(
        captured.lock().expect("test lock").clone(),
        vec!["the child streams".to_string()],
        "the opt-in capture heard the child"
    );
    assert!(
        up_the_pipe.lock().expect("test lock").is_empty(),
        "nothing auto-crosses the stdio"
    );

    // The layer's manual forward of the captured frame: re-emitted
    // with the stdio as an additional receiver, FROM the lane the
    // frame arrived on (re-teaching the lane is idempotent; teaching
    // the stdio would hijack the child's route).
    node.emit_to(
        &child_lane,
        &[stdio],
        EventFrame {
            stream: Some(StreamId::new("child-sess")),
            origin: None,
            ttl: None,
            event: SessionEvent::TextDelta {
                turn_id: "t".to_string(),
                text: "the child streams".to_string(),
            },
        },
    );
    assert_eq!(
        up_the_pipe.lock().expect("test lock").len(),
        1,
        "the manual forward crossed"
    );
}

/// The two manual-forwarding shapes (2026-09 ruling): verbatim —
/// from the arrival lane, child's stamp intact, the child stays
/// directly addressable through the chain; re-stamped — from the
/// layer, the forwarder's own id, commands come addressed to the
/// forwarder and route to its layer (the interception surface).
#[test]
fn manual_forwards_are_verbatim_from_the_lane_or_re_stamped_from_the_layer() {
    let node: Arc<Node> = Arc::new(Node::new("ext"));
    let (layer, saw) = stub_layer(&node, "layer");

    // The ext's stdio up to its host: subscribed to nothing, a pure
    // writer (what crosses is what the layer sends it).
    let up: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = up.clone();
    let stdio = Channel::line("stdio", move |line: &str| {
        sink.lock().expect("test lock").push(line.to_string());
    });

    // The child's lane: its writer records routed commands (the
    // "child received this" proof).
    let child_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = child_lines.clone();
    let child_lane = Channel::line("child", move |line: &str| {
        sink.lock().expect("test lock").push(line.to_string());
    });

    let child_frame = EventFrame {
        stream: Some(StreamId::new("child-sess")),
        origin: None,
        ttl: None,
        event: SessionEvent::error_session("the child speaks".to_string()),
    };
    // The child's frame arrives (teaching the lane; nothing crosses —
    // the stdio subscribes to nothing), and the ext forwards it
    // VERBATIM, from the lane, stamp intact.
    node.intake(&child_lane, Inbound::Event(child_frame.clone()));
    assert!(
        up.lock().expect("test lock").is_empty(),
        "the arrival did not auto-cross"
    );
    node.emit_to(&child_lane, std::slice::from_ref(&stdio), child_frame);
    assert_eq!(
        up.lock().expect("test lock").len(),
        1,
        "the verbatim forward crossed"
    );

    // Verbatim keeps the child directly addressable: a command for
    // its stream routes down the child's lane.
    node.intake(
        &stdio,
        Inbound::Command(SessionCommand::Abort {
            session: "child-sess".to_string(),
        }),
    );
    assert!(
        child_lines
            .lock()
            .expect("test lock")
            .iter()
            .any(|line| line.contains("abort")),
        "the child stayed directly addressable through the chain"
    );

    // Re-stamped: the ext speaks as itself, from its layer — upstream
    // learns the ext, and its id routes to the layer (the
    // interception surface), never to the child.
    node.emit_to(
        &layer,
        std::slice::from_ref(&stdio),
        EventFrame {
            stream: Some(StreamId::new("ext")),
            origin: None,
            ttl: None,
            event: SessionEvent::error_session("the child speaks".to_string()),
        },
    );
    node.intake(
        &stdio,
        Inbound::Command(SessionCommand::Abort {
            session: "ext".to_string(),
        }),
    );
    assert_eq!(
        saw.commands(),
        vec!["abort"],
        "the intercepted command reached the layer's mailbox"
    );
    assert_eq!(
        child_lines.lock().expect("test lock").len(),
        1,
        "the re-stamped forward's command never reached the child"
    );
}

/// The contained hold door (the review round's major finding): an id
/// a SENDER minted and crossed a pipe, held under a live id, is the
/// sender's violation — the policy fires, the entry is untouched,
/// and nothing panics.
#[test]
fn a_held_sender_minted_id_is_contained_not_fatal() {
    let node: Arc<Node> = Arc::new(Node::new("core"));
    let contained: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = contained.clone();
    node.on_mint_violation(move |owner, id| {
        sink.lock()
            .expect("test lock")
            .push(format!("{owner}:{id}"));
    });

    let first = node.try_hold("lane-a", "svc-1", "service_response", |_| {});
    assert!(first, "the fresh id held");
    // The same id arrives again — a plain guest retry bug.
    let second = node.try_hold("lane-a", "svc-1", "service_response", |_| {});
    assert!(!second, "the live id refused");
    assert_eq!(
        contained.lock().expect("test lock").clone(),
        vec!["lane-a:svc-1".to_string()],
        "the policy met the violator"
    );
    // The surviving entry is untouched and still answerable.
    assert!(
        matches!(
            node.answer("svc-1", "service_response", Box::new(serde_json::json!({}))),
            crate::node::AnswerOutcome::Delivered
        ),
        "the original entry was untouched by the containment"
    );
}

/// The registration's atomicity: the mint decision and the insert
/// are one act (the review round's race finding — no pre-check to
/// both pass). Two sequential registrations of one id: exactly one
/// wins, the loser is contained, never a panic.
#[test]
fn the_mint_decision_is_atomic_with_the_registration() {
    let asks = crate::asks::PendingAsks::default();
    assert!(asks.register("id".to_string(), "a", "interaction", |_| {}));
    assert!(!asks.register("id".to_string(), "b", "interaction", |_| {}));
    // The winner's entry survived under the original owner.
    assert!(asks.held("id"));
    asks.retract_owner("a", "done");
    assert!(!asks.held("id"));
}

/// One owner holds one kind once: the card co-subscription beside an
/// explicit settle watch (a watch list naming both kinds) delivers
/// each settled frame ONCE, not twice.
#[test]
fn an_owner_holds_a_kind_once() {
    let node = Arc::new(Node::new("core"));
    // The watch list names BOTH card kinds; the lane channel is
    // subscribed to each (the ack loop's verbatim shape).
    let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let writer = lines.clone();
    let lane = Channel::line("watcher", move |line: &str| {
        writer.lock().expect("test lock").push(line.to_string());
    });
    node.subscribe_channel(tags::INTERACTION_REQUEST, &lane);
    node.subscribe_channel(tags::INTERACTION_SETTLED, &lane);

    let (layer, saw) = stub_layer(&node, "layer");
    let awaiter = node.ask(
        "layer",
        &StreamId::new("s-1"),
        "native:select_any",
        json!({}),
    );
    node.intake(
        &layer,
        Inbound::Command(SessionCommand::InteractionResponse {
            session: None,
            id: request_id(&saw),
            payload: json!({"text": "yes"}),
        }),
    );
    drop(awaiter.blocking_recv());

    let settled_count = lines
        .lock()
        .expect("test lock")
        .iter()
        .filter(|line| line.contains("interaction_settled"))
        .count();
    assert_eq!(settled_count, 1, "the settle crossed the lane once");
}
