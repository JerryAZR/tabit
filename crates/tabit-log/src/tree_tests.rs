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

fn compaction_node(id: &str, parent: Option<&str>, cut_child: &str) -> SessionEntry {
    SessionEntry::with_id(
        id.to_string(),
        parent.map(str::to_string),
        "t".to_string(),
        crate::entry::EntryKind::Compaction {
            summary: "summarized".to_string(),
            cut_child: cut_child.to_string(),
            tokens_before: 0,
            tokens_after: 0,
            usage: rig_core::completion::Usage::default(),
        },
    )
}

/// The owner's pinning snapshots (2026-09 ruling): the raw branch
/// keeps compactions at their leaf positions, the history view
/// leads with the newest one and stops at its cut child. Branch
/// `[A, B, C, D, COMPACT1, COMPACT2]` with COMPACT1 cutting at B
/// and COMPACT2 at C.
#[test]
fn history_view_matches_the_ruled_snapshots() {
    let mut tree = SessionTree::empty();
    tree.append(node("a", None));
    tree.append(node("b", Some("a")));
    tree.append(node("c", Some("b")));
    tree.append(node("d", Some("c")));
    tree.append(compaction_node("x1", Some("d"), "b"));
    tree.append(compaction_node("x2", Some("x1"), "c"));
    let ids = |tree: &SessionTree| {
        tree.history_to_head()
            .iter()
            .map(|e| e.id.clone())
            .collect::<Vec<_>>()
    };
    // Before COMPACT1: the plain chain.
    tree.move_head(Some("d")).expect("d exists");
    assert_eq!(ids(&tree), ["a", "b", "c", "d"]);
    // Before COMPACT2: pass 1's view.
    tree.move_head(Some("x1")).expect("x1 exists");
    assert_eq!(ids(&tree), ["x1", "b", "c", "d"]);
    // Finally: pass 2's view — the older compaction is dead (its
    // summary was itself summarized) and never enters.
    tree.move_head(Some("x2")).expect("x2 exists");
    assert_eq!(ids(&tree), ["x2", "c", "d"]);
    // The raw branch (checkout/audit surface) keeps every node at its
    // true position — the view is derived, the tree stays honest.
    assert_eq!(
        tree.path_to_head()
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "c", "d", "x1", "x2"]
    );
}

#[test]
fn history_view_appends_new_entries_after_the_leading_compaction() {
    let mut tree = SessionTree::empty();
    tree.append(node("a", None));
    tree.append(node("b", Some("a")));
    tree.append(node("c", Some("b")));
    tree.append(compaction_node("x", Some("c"), "b"));
    // Post-compaction turns chain through the compaction node —
    // exactly the walked order.
    tree.append(node("e", Some("x")));
    assert_eq!(
        tree.history_to_head()
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        ["x", "b", "c", "e"]
    );
    // A branch from a pre-compaction node never sees the compaction.
    assert_eq!(
        tree.path_to(Some("c"))
            .expect("c exists")
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
}

/// The spaced variant: entries between the two compactions, the
/// second cut landing in the retained tail. The walk stops at the
/// cut child without entering the replaced prefix — `a`, `b`, `c`
/// and the dead COMPACT1 are never reached.
#[test]
fn history_view_with_a_spaced_second_compaction() {
    let mut tree = SessionTree::empty();
    tree.append(node("a", None));
    tree.append(node("b", Some("a")));
    tree.append(node("c", Some("b")));
    tree.append(compaction_node("x1", Some("c"), "b"));
    tree.append(node("e", Some("x1")));
    // COMPACT2 cuts inside COMPACT1's tail, before `c`.
    tree.append(compaction_node("x2", Some("e"), "c"));
    assert_eq!(
        tree.history_to_head()
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        ["x2", "c", "e"]
    );
}

#[test]
fn load_append_rejects_a_duplicate_id() {
    let mut tree = SessionTree::empty();
    tree.append(node("a", None));
    // A file replaying `a` again under a correct parent still names a
    // corrupt log: ids are unique by construction.
    let err = tree
        .load_append(node("a", Some("a")))
        .expect_err("duplicate id");
    assert!(err.0.contains("duplicate entry id `a`"), "{}", err.0);
}
