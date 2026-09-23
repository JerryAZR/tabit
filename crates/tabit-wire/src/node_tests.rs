//! The routing layer's net test — the ruling's acceptance test
//! (2026-09): stub functional layers wired into a multi-node net,
//! every routing law pinned at the layer, independent of any real
//! functional layer. The in-process net holds the law; the real-pipe
//! net (`tests/net.rs`) holds the same laws over stdio.

use std::sync::{Arc, Mutex, OnceLock};

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
        saw.has("settled:core-a1@-"),
        "the settle announced: {:?}",
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
        saw.has("error@sess-child"),
        "the swept route now misses uniformly: {:?}",
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
