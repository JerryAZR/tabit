//! The routing layer's net test — the ruling's acceptance test
//! (2026-09): stub functional layers wired into a multi-node net,
//! every routing law pinned at the layer, independent of any real
//! functional layer. The in-process net holds the law; the real-pipe
//! net (`tests/net.rs`) holds the same laws over stdio.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use tabit_protocol::{EventFrame, SessionCommand, SessionEvent, StreamId, tags};

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
/// The TTL test's cross-wiring slots (the two sides reference each
/// other; OnceLock breaks the cycle).
static A_SIDE: OnceLock<Channel> = OnceLock::new();
static B_SIDE: OnceLock<Channel> = OnceLock::new();

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
                SessionEvent::Error { .. } => format!("error@{}", stamp(&frame.stream)),
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
        vec!["interaction_request@s-1".to_string()],
        "the request surfaced, stamped with the asking stream"
    );

    // The answer arrives from anywhere — here, the same layer.
    node.intake(
        &layer,
        Inbound::Command(SessionCommand::InteractionResponse {
            session: None,
            id: "core-a1".to_string(),
            payload: json!({"selected": ["Allow"]}),
        }),
    );
    let answer: Value = awaiter.blocking_recv().expect("the promise resolved");
    assert_eq!(answer, json!({"selected": ["Allow"]}));
    assert!(
        saw.has("settled:core-a1@s-1"),
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
            id: "core-a1".to_string(),
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
        parent_saw.has("interaction_request@sess-child"),
        "the ask surfaced at the parent: {:?}",
        parent_saw.events()
    );

    // The parent answers; the id routes the answer down the child's
    // pipe and into the child's ask table (the promise).
    parent.intake(
        &child_at_parent,
        Inbound::Command(SessionCommand::InteractionResponse {
            session: None,
            id: "child-a1".to_string(),
            payload: json!({"text": "yes"}),
        }),
    );
    let answer: Value = awaiter
        .blocking_recv()
        .expect("the promise crossed the net");
    assert_eq!(answer, json!({"text": "yes"}));
    assert!(
        parent_saw.has("settled:child-a1"),
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
        saw.has("settled:child-a1"),
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
        saw.has("error@-"),
        "the swept route misses uniformly, unstamped (no session owns it): {:?}",
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
    assert!(saw.has("error@-"), "the break is loud: {:?}", saw.events());

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
        .filter(|seen| seen.starts_with("interaction_request@"))
        .count();
    assert_eq!(
        requests,
        1,
        "the mirrored ask came back once, not in a loop: {:?}",
        parent_saw.events()
    );
}

/// The two deaths: a run dying retracts its questions but not the
/// participant's routes; a participant dying takes everything.
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
    drop(awaiter); // the asker's run ends: the retraction is its dismissal

    node.retract_asks("run-1", "the run ended");
    assert!(
        saw.has("settled:core-a1"),
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
            id: "child-a1".to_string(),
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
/// dies at the hop budget, loudly — normally it never fires.
#[test]
fn the_ttl_tripwire_kills_cross_node_loops() {
    let a = Arc::new(Node::new("a"));
    let b = Arc::new(Node::new("b"));
    let saw: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    // The misconfigured cycle: each side forwards everything it
    // hears into the other node's intake via the FORWARDING side's
    // own channel — a distinct arrival owner each hop, so the
    // ingress law cannot break it; only the budget can.
    let a_node = a.clone();
    let b_node = b.clone();
    let a_side = Channel::line("a-fwd", move |line: &str| {
        if let (Some(channel), Some(inbound)) = (B_SIDE.get(), parse_shared(line)) {
            b_node.intake(channel, inbound);
        }
    });
    let b_node2 = b.clone();
    let a_node2 = a.clone();
    let b_side = Channel::line("b-fwd", move |line: &str| {
        if let (Some(channel), Some(inbound)) = (A_SIDE.get(), parse_shared(line)) {
            a_node2.intake(channel, inbound);
        }
    });
    let _ = (a_node, b_node2);
    B_SIDE.set(b_side.clone()).ok();
    A_SIDE.set(a_side.clone()).ok();

    let sink = saw.clone();
    a.subscribe_all("recorder", move |frame| {
        sink.lock()
            .expect("test lock")
            .push(frame.event.tag().to_string());
    });
    // Both nodes forward everything they hear across the cycle.
    a.subscribe_channel_all(&a_side);
    b.subscribe_channel_all(&b_side);

    // A local emission enters the cycle.
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

    // The cycle runs, TTL-bound: it ends in the loud tripwire error,
    // and the total frame count is bounded by the budget.
    std::thread::sleep(Duration::from_millis(50));
    let seen = saw.lock().expect("test lock").clone();
    assert!(
        seen.len() <= 70,
        "the hop budget bounded the cycle: {} frames: {seen:?}",
        seen.len()
    );
    assert!(
        seen.last().map(String::as_str) == Some("error"),
        "the loop's last gasp is the loud tripwire: {seen:?}"
    );
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

    let (layer, _saw) = stub_layer(&node, "layer");
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
            id: "core-a1".to_string(),
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
