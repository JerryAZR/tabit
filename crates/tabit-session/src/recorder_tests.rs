//! Recorder-level tests: the resident state contract — the tree, the
//! head pointer, the incremental context fold, checkout as a pointer
//! move, the one-pass load, and the barrier's validate-then-commit.

use super::*;
use crate::entry::{EntryKind, SessionEntry, SideKind};
use crate::store::{LoadedSession, SessionStore, SessionWriter};
use rig_core::OneOrMany;
use rig_core::completion::Message;
use rig_core::message::{Text, UserContent};
use std::fs;
use std::path::Path;

/// A minimal head-tracking builder (the same fixture shape as the
/// store tests'): nodes chain off the running head, checkouts move it.
#[derive(Default)]
struct Nodes {
    head: Option<String>,
}

impl Nodes {
    fn node(&mut self, kind: EntryKind) -> FileRecord {
        let entry = SessionEntry::new(self.head.clone(), "t".to_string(), kind);
        self.head = Some(entry.id.clone());
        FileRecord::Node(entry)
    }

    fn side(kind: SideKind) -> FileRecord {
        FileRecord::Side(SideRecord {
            timestamp: "t".to_string(),
            kind,
        })
    }

    fn checkout(&mut self, to: Option<&str>) -> FileRecord {
        self.head = to.map(str::to_string);
        Self::side(SideKind::Checkout {
            to: to.map(str::to_string),
        })
    }
}

fn temp_store(tag: &str) -> SessionStore {
    let dir = std::env::temp_dir()
        .join("tabit-recorder-tests")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    SessionStore::new(&dir)
}

fn user_message(text: &str) -> Message {
    Message::User {
        content: OneOrMany::one(UserContent::Text(Text::new(text))),
    }
}

fn user_node(text: &str) -> EntryKind {
    EntryKind::UserMessage {
        message: user_message(text),
    }
}

/// The writer's flush verdict: `None` is success.
fn assert_flushed(outcome: Option<SessionError>) {
    assert!(outcome.is_none(), "the flush failed: {outcome:?}");
}

fn context_texts(recorder: &SessionRecorder) -> Vec<String> {
    recorder
        .context()
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => content.iter().find_map(|part| match part {
                UserContent::Text(text) => Some(text.text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect()
}

fn branch_texts(recorder: &SessionRecorder) -> Vec<String> {
    recorder
        .active_branch()
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::UserMessage {
                message: Message::User { content },
            } => content.iter().find_map(|part| match part {
                UserContent::Text(text) => Some(text.text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect()
}

#[test]
fn records_grow_the_tree_head_and_context_together() {
    let store = temp_store("grow");
    let writer = store.create("C:/w");
    let recorder = SessionRecorder::new(writer);

    let first = recorder.record(user_node("one"));
    let second = recorder.record(user_node("two"));

    let branch = recorder.active_branch();
    assert_eq!(
        branch
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        vec![first.as_str(), second.as_str()],
        "the branch follows commit order"
    );
    let parent_of = |id: &str| {
        branch
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.parent_id.clone())
    };
    assert_eq!(parent_of(&first), None);
    assert_eq!(parent_of(&second), Some(first.clone()));
    assert_eq!(context_texts(&recorder), vec!["one", "two"]);
    assert!(recorder.id_probe().contains(&second));
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn checkout_moves_the_head_and_branches_the_next_append() {
    let store = temp_store("checkout");
    let writer = store.create("C:/w");
    let recorder = SessionRecorder::new(writer);
    let first = recorder.record(user_node("one"));
    recorder.record(user_node("two"));

    recorder
        .checkout(Some(&first), Path::new(""))
        .expect("checkout");

    assert_eq!(branch_texts(&recorder), vec!["one"], "the pointer moved");
    let third = recorder.record(user_node("branched"));
    let branch = recorder.active_branch();
    let branched = branch
        .iter()
        .find(|entry| entry.id == third)
        .expect("the new node");
    assert_eq!(branched.parent_id, Some(first), "branches from the target");
    assert_eq!(branch_texts(&recorder), vec!["one", "branched"]);
    assert_eq!(
        context_texts(&recorder),
        vec!["one", "branched"],
        "the context re-projected from the new branch"
    );
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn a_trailing_checkout_survives_the_reload() {
    let store = temp_store("checkout-durable");
    let writer = store.create("C:/w");
    let path = writer.path().to_path_buf();
    let recorder = SessionRecorder::new(writer);
    let first = recorder.record(user_node("one"));
    recorder.record(user_node("dropped"));
    recorder
        .checkout(Some(&first), Path::new(""))
        .expect("checkout");
    drop(recorder);

    // The next process folds the same resident state from the file.
    let reopened = SessionWriter::open_existing(&path).expect("reopen");
    let recorder = SessionRecorder::new(reopened);
    let loaded = store.open_path(&path).expect("open");
    let outcome = recorder.load(loaded).expect("load");
    assert_eq!(
        branch_texts(&recorder),
        vec!["one"],
        "nothing was appended after the checkout, and the head still moved"
    );
    assert_eq!(outcome.repaired_tool_calls, 0);
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn load_folds_the_tree_the_head_and_the_register() {
    let store = temp_store("load");
    let mut writer = store.create("C:/w");
    let mut nodes = Nodes::default();
    let error = writer.append_record(&Nodes::side(SideKind::ModelChange {
        provider: "p".to_string(),
        model: "m".to_string(),
        thinking_level: None,
    }));
    assert!(error.is_none(), "write model change: {error:?}");
    let one = nodes.node(user_node("one"));
    assert_flushed(writer.append_record(&one));
    let two = nodes.node(user_node("two"));
    assert_flushed(writer.append_record(&two));
    let FileRecord::Node(two_entry) = &two else {
        panic!("fixture builds nodes");
    };
    let checkout = nodes.checkout(Some(two_entry.id.as_str()));
    assert_flushed(writer.append_record(&checkout));
    let three = nodes.node(user_node("three"));
    assert_flushed(writer.append_record(&three));
    let path = writer.path().to_path_buf();
    drop(writer);

    let reopened = SessionWriter::open_existing(&path).expect("reopen");
    let recorder = SessionRecorder::new(reopened);
    let loaded = store.open_path(&path).expect("open");
    let outcome = recorder.load(loaded).expect("load");

    assert_eq!(
        branch_texts(&recorder),
        vec!["one", "two", "three"],
        "the checkout branched and the next append extended the branch"
    );
    assert_eq!(context_texts(&recorder), vec!["one", "two", "three"]);
    assert_eq!(outcome.selection.expect("register").model, "m");
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn load_rejects_a_node_that_breaks_the_head_invariant() {
    let store = temp_store("load-invariant");
    let mut writer = store.create("C:/w");
    let mut nodes = Nodes::default();
    let one = nodes.node(user_node("one"));
    assert_flushed(writer.append_record(&one));
    let two = nodes.node(user_node("two"));
    assert_flushed(writer.append_record(&two));
    // A sibling welded in after `two`: its parent exists, but the head
    // at that point was `two` — appends only ever attach to the head,
    // so this shape cannot have been produced honestly.
    let FileRecord::Node(one_entry) = &one else {
        panic!("fixture builds nodes");
    };
    let orphan = SessionEntry::new(
        Some(one_entry.id.clone()),
        "t".to_string(),
        user_node("ghost"),
    );
    let path = writer.path().to_path_buf();
    drop(writer);

    let reopened = SessionWriter::open_existing(&path).expect("reopen");
    let recorder = SessionRecorder::new(reopened);
    let mut loaded = store.open_path(&path).expect("open");
    loaded.records.push(FileRecord::Node(orphan));
    match recorder.load(loaded) {
        Err(SessionError::Corrupt { message, .. }) => {
            assert!(message.contains("the head at that point"), "{message}")
        }
        other => panic!("expected corruption error, got {other:?}"),
    }
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn load_rejects_a_checkout_targeting_the_future() {
    let store = temp_store("load-future");
    let writer = store.create("C:/w");
    let recorder = SessionRecorder::new(writer);
    let loaded = LoadedSession {
        header: crate::entry::SessionHeader {
            version: crate::entry::SESSION_FORMAT_VERSION,
            id: "s".to_string(),
            created_at: "t".to_string(),
            cwd: "C:/w".to_string(),
            parent_session: None,
        },
        records: vec![FileRecord::Side(SideRecord {
            timestamp: "t".to_string(),
            kind: SideKind::Checkout {
                to: Some("not-yet-written".to_string()),
            },
        })],
        path: Path::new("mem").to_path_buf(),
        repairs: Vec::new(),
    };
    match recorder.load(loaded) {
        Err(SessionError::Corrupt { message, .. }) => {
            assert!(message.contains("unknown or later"), "{message}")
        }
        other => panic!("expected corruption error, got {other:?}"),
    }
}

#[test]
fn a_failed_barrier_touches_nothing_resident() {
    // The store's directory is a regular file: materialization cannot
    // succeed, so the barrier's drain refuses — the environmental
    // failure class, staged portably.
    let base = temp_store("barrier-resident");
    fs::create_dir_all(base.dir()).expect("base dir");
    let blocked = base.dir().join("blocker");
    fs::write(&blocked, b"not a directory").expect("blocker");
    let store = SessionStore::new(&blocked);
    let writer = store.create("C:/w");
    let recorder = SessionRecorder::new(writer);

    let standing = recorder.record(user_node("standing"));

    let batch = vec![(None, user_node("b1")), (None, user_node("b2"))];
    let error = recorder
        .commit_barrier(batch)
        .expect_err("the disk refuses the batch");
    assert!(!error.is_empty());

    assert_eq!(
        branch_texts(&recorder),
        vec!["standing"],
        "no tree growth, no head move"
    );
    assert_eq!(context_texts(&recorder), vec!["standing"]);
    let _ = standing;
    fs::remove_dir_all(base.dir()).ok();
}

#[test]
fn a_successful_barrier_grows_the_branch_atomically() {
    let store = temp_store("barrier-ok");
    let writer = store.create("C:/w");
    let recorder = SessionRecorder::new(writer);
    let batch = vec![(None, user_node("first")), (None, user_node("second"))];
    let ids = recorder
        .commit_barrier(batch)
        .expect("the disk accepts the batch");
    assert_eq!(ids.len(), 2);
    assert_eq!(branch_texts(&recorder), vec!["first", "second"]);
    assert_eq!(context_texts(&recorder), vec!["first", "second"]);
    assert_eq!(recorder.records().len(), 2);
    fs::remove_dir_all(store.dir()).ok();
}
