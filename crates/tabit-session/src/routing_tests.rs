//! The child router's table mechanics — registration, learning,
//! forwarding, and abort's tree walk. The full command surface is the
//! structural point (a child is a session host; forwarding is all the
//! router does), proven end-to-end by the subprocess e2e suite in
//! `crates/tabit/tests/`.

use crate::routing::ChildRouter;
use tabit_protocol::SessionCommand;

/// A child pipe's line inbox.
fn inbox() -> (
    tokio::sync::mpsc::UnboundedSender<String>,
    tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    tokio::sync::mpsc::unbounded_channel()
}

#[test]
fn an_unknown_address_is_not_routed() {
    let router = ChildRouter::default();
    let routed = router.deliver(
        "nobody",
        SessionCommand::Abort {
            session: "nobody".to_string(),
        },
    );
    assert!(!routed, "no table knows the address");
}

#[test]
fn a_registered_child_receives_the_command_as_a_wire_line() {
    let router = ChildRouter::default();
    let (tx, mut rx) = inbox();
    router.register("child", "parent", tx);

    assert!(router.deliver(
        "child",
        SessionCommand::Message {
            session: "child".to_string(),
            text: "steer".to_string(),
        },
    ));
    let line = rx.try_recv().expect("the line crossed");
    assert!(
        line.contains("\"type\":\"message\"") && line.contains("steer"),
        "the command serialized as its wire shape: {line}"
    );
}

#[test]
fn a_learned_descendant_routes_to_the_owning_child() {
    // The learning model: a grandchild's frame taught the bridge which
    // child subtree owns the id; the command walks there, address
    // intact (the CHILD's router resolves the next hop).
    let router = ChildRouter::default();
    let (tx, mut rx) = inbox();
    router.register("child", "parent", tx);
    router.learn("grandchild", "child");

    assert!(router.deliver(
        "grandchild",
        SessionCommand::InteractionResponse {
            session: "grandchild".to_string(),
            id: "req-1".to_string(),
            payload: serde_json::json!({}),
        },
    ));
    let line = rx.try_recv().expect("the line crossed");
    assert!(
        line.contains("grandchild") && line.contains("req-1"),
        "the address survived hop one: {line}"
    );
}

#[test]
fn unregister_purges_the_child_and_everything_learned_through_it() {
    let router = ChildRouter::default();
    let (tx, _rx) = inbox();
    router.register("child", "parent", tx);
    router.learn("grandchild", "child");

    router.unregister("child");
    for address in ["child", "grandchild"] {
        assert!(
            !router.deliver(
                address,
                SessionCommand::Abort {
                    session: address.to_string(),
                },
            ),
            "`{address}` stopped routing with its child"
        );
    }
}

#[test]
fn abort_broadcasts_to_every_child_of_the_parent_as_a_routed_command() {
    let router = ChildRouter::default();
    let (first, mut rx_first) = inbox();
    let (second, mut rx_second) = inbox();
    router.register("first", "parent", first);
    router.register("second", "parent", second);
    let (unrelated, _rx_other) = inbox();
    router.register("unrelated", "someone-else", unrelated);

    router.broadcast_abort("parent");
    for (name, rx) in [("first", &mut rx_first), ("second", &mut rx_second)] {
        let line = rx
            .try_recv()
            .unwrap_or_else(|_| panic!("`{name}` received the routed abort"));
        assert!(
            line.contains("\"type\":\"abort\"") && line.contains(name),
            "the abort addressed `{name}` by id: {line}"
        );
    }
    assert!(
        rx_first.try_recv().is_err() && rx_second.try_recv().is_err(),
        "one abort per child"
    );
}
