//! The session tree: head-pointer moves, the append invariant, walks.

use super::*;

fn node(id: &str, parent: Option<&str>) -> SessionEntry {
    SessionEntry::with_id(
        id.to_string(),
        parent.map(str::to_string),
        "t".to_string(),
        crate::entry::EntryKind::UserMessage {
            message: rig_core::completion::Message::user("x"),
        },
    )
}

#[test]
fn appends_attach_at_the_head_and_advance_it() {
    let mut tree = SessionTree::empty();
    assert_eq!(tree.head(), None);
    tree.append(node("a", None));
    tree.append(node("b", Some("a")));
    assert_eq!(tree.head(), Some("b"));
    assert_eq!(
        tree.path_to_head()
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
}

#[test]
fn append_with_a_stale_parent_panics() {
    let mut tree = SessionTree::empty();
    tree.append(node("a", None));
    tree.append(node("b", Some("a")));
    // `a` is no longer the head: a node parenting it is a wiring bug.
    let outcome = std::panic::catch_unwind(|| {
        let mut tree = tree.clone();
        tree.append(node("c", Some("a")));
    });
    assert!(outcome.is_err(), "a node parenting a non-head panics");
}

#[test]
fn move_head_switches_branches_and_keeps_both() {
    let mut tree = SessionTree::empty();
    tree.append(node("a", None));
    tree.append(node("b1", Some("a")));
    tree.move_head(Some("a")).expect("rewind to a");
    tree.append(node("b2", Some("a")));
    assert_eq!(tree.head(), Some("b2"));
    assert_eq!(
        tree.path_to_head()
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b2"],
        "the active branch is the new one"
    );
    assert!(tree.contains("b1"), "the abandoned branch stays reachable");
    tree.move_head(Some("b1")).expect("switch back");
    assert_eq!(
        tree.path_to_head()
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b1"]
    );
}

#[test]
fn move_head_rejects_unknown_targets() {
    let mut tree = SessionTree::empty();
    let fault = tree.move_head(Some("ghost")).expect_err("unknown target");
    assert!(fault.0.contains("ghost"));
}

#[test]
fn move_head_to_none_is_the_root() {
    let mut tree = SessionTree::empty();
    tree.append(node("a", None));
    tree.move_head(None).expect("root move");
    assert_eq!(tree.head(), None);
    assert!(tree.path_to_head().is_empty());
}

#[test]
fn load_append_enforces_the_head_invariant() {
    let mut tree = SessionTree::empty();
    tree.load_append(node("a", None)).expect("first node roots");
    let fault = tree
        .load_append(node("c", None))
        .expect_err("a second root violates the append invariant");
    assert!(fault.0.contains("parents"));
    tree.load_append(node("b", Some("a")))
        .expect("child of head");
    let fault = tree
        .load_append(node("d", Some("a")))
        .expect_err("a stale parent is not the head");
    assert!(fault.0.contains("head"));
    assert!(
        tree.load_append(node("b", Some("a"))).is_err(),
        "duplicate id"
    );
}

#[test]
fn path_to_a_broken_link_is_a_fault() {
    let mut tree = SessionTree::empty();
    tree.append(node("a", None));
    // Corrupt the structure directly — only the load door could build
    // this, and it validates; the walk still defends.
    tree.nodes.insert(
        "ghost-child".to_string(),
        node("ghost-child", Some("missing")),
    );
    let fault = tree
        .path_to(Some("ghost-child"))
        .expect_err("missing parent");
    assert!(fault.0.contains("missing node"));
}
