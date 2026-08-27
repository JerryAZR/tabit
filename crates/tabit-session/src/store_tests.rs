use super::*;
use crate::entry::{EntryKind, FileRecord, SessionEntry, SideKind, SideRecord};
use rig_core::OneOrMany;
use rig_core::completion::Message;
use rig_core::message::{Text, UserContent};
use std::fs;

fn temp_store(tag: &str) -> SessionStore {
    let dir = std::env::temp_dir()
        .join("tabit-session-tests")
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

/// The writer's flush verdict: `None` is success (write-behind: a
/// `Some` is the buffered-and-degraded outcome, an error to name).
fn assert_flushed(outcome: Option<SessionError>) {
    assert!(outcome.is_none(), "the flush failed: {outcome:?}");
}

/// A minimal head-tracking builder for store-level fixtures: constructs
/// records the way the recorder does (each node a child of the running
/// head; checkouts move it). The store itself is structure-blind —
/// these tests pin its parsing and write mechanics, the recorder tests
/// pin the tree.
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

#[test]
fn create_writes_header_and_appends_chain_parents() {
    let store = temp_store("create");
    let mut writer = store.create("C:/work");
    let mut nodes = Nodes::default();
    let first = nodes.node(user_node("one"));
    let second = nodes.node(user_node("two"));
    assert_flushed(writer.append_record(&first));
    assert_flushed(writer.append_record(&second));

    let loaded = store.open_path(writer.path()).expect("open");
    let FileRecord::Node(first_entry) = &first else {
        panic!("fixture builds nodes");
    };
    let FileRecord::Node(second_entry) = &second else {
        panic!("fixture builds nodes");
    };
    assert_eq!(first_entry.parent_id, None);
    assert_eq!(
        second_entry.parent_id,
        Some(first_entry.id.clone()),
        "the fixture chains the way the recorder does"
    );
    assert_eq!(loaded.records.len(), 2);
    assert_eq!(loaded.header.cwd, "C:/work");
    assert!(loaded.repairs.is_empty());
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn the_first_record_materializes_the_file_with_the_header() {
    // The opening model_change is written by the session through the
    // register (the recorder and session suites pin that); what the
    // store pins is that the first record through a deferred writer
    // materializes header-then-records in order.
    let store = temp_store("opening");
    let mut writer = store.create("C:/work");
    assert_flushed(writer.append_record(&Nodes::side(SideKind::ModelChange {
        provider: "p".to_string(),
        model: "m".to_string(),
        thinking_level: None,
    })));
    let node = Nodes::default().node(user_node("one"));
    assert_flushed(writer.append_record(&node));

    let raw = fs::read_to_string(writer.path()).expect("read");
    let mut lines = raw.lines();
    let _header = lines.next();
    let opening = lines.next().expect("the opening side record");
    assert!(opening.contains("\"kind\":\"model_change\""), "{opening}");
    assert!(
        !opening.contains("\"id\""),
        "side records carry no id: {opening}"
    );
    let node_line = lines.next().expect("the node follows");
    assert!(
        node_line.contains("\"kind\":\"user_message\""),
        "{node_line}"
    );
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn open_by_session_id_finds_the_file() {
    let store = temp_store("by-id");
    let mut writer = store.create("C:/work");
    assert_flushed(writer.append_record(&Nodes::default().node(user_node("x"))));
    let id = writer.session_id().to_string();
    let loaded = store.open(&id).expect("open by id");
    assert_eq!(loaded.header.id, id);
    assert!(store.open("no-such-id").is_err());
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn list_orders_newest_first_and_counts_records() {
    let store = temp_store("list");
    let mut older = store.create("C:/a");
    let err = older.append_record(&Nodes::default().node(user_node("x")));
    assert!(err.is_none(), "list older append: {err:?}");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let mut newer = store.create("C:/b");
    let err = newer.append_record(&Nodes::default().node(user_node("y")));
    assert!(err.is_none(), "list newer append: {err:?}");

    let summaries = store.list().expect("list");
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].id, newer.session_id());
    assert_eq!(summaries[0].entry_count, 1);
    // Deferred creation means header-only sessions do not exist.
    assert_eq!(summaries[1].entry_count, 1);
    assert_eq!(summaries[1].id, older.session_id());
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn torn_tail_is_repaired_with_report() {
    let store = temp_store("torn");
    let mut writer = store.create("C:/work");
    let err = writer.append_record(&Nodes::default().node(user_node("kept")));
    assert!(err.is_none(), "torn first append: {err:?}");
    let path = writer.path().to_path_buf();

    // Simulate a crash mid-append: partial JSON without a trailing newline.
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open for tear");
    write!(file, r#"{{"id":"torn","timestamp":"t","kind":"lab"#).expect("tear");

    let loaded = store.open_path(&path).expect("open with repair");
    assert_eq!(loaded.records.len(), 1, "the complete record survives");
    assert_eq!(loaded.repairs.len(), 1);
    assert!(matches!(
        &loaded.repairs[0],
        Repair::TornTail { dropped } if dropped.contains("torn")
    ));

    // The file was truncated back to the valid prefix; a re-open is clean.
    let reopened = store.open_path(&path).expect("reopen");
    assert!(reopened.repairs.is_empty());
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn malformed_middle_line_fails_loudly_with_line_number() {
    let store = temp_store("middle");
    let mut writer = store.create("C:/work");
    assert_flushed(writer.append_record(&Nodes::default().node(user_node("one"))));
    let path = writer.path().to_path_buf();
    let mut lines = fs::read_to_string(&path).expect("read");
    lines.push_str("this is not json at all\n");
    lines.push_str(
        &serde_json::to_string(&Nodes::default().node(user_node("three"))).expect("serialize node"),
    );
    lines.push('\n');
    fs::write(&path, lines).expect("write");

    match store.open_path(&path) {
        Err(SessionError::Parse { line, .. }) => assert_eq!(line, 3),
        other => panic!("expected a loud parse error naming line 3, got {other:?}"),
    }
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn duplicate_entry_id_is_corruption() {
    let store = temp_store("dup");
    let mut writer = store.create("C:/work");
    assert_flushed(writer.append_record(&Nodes::default().node(user_node("seed"))));
    let path = writer.path().to_path_buf();
    let header = fs::read_to_string(&path).expect("read header");
    let first = serde_json::to_string(&SessionEntry::new(None, "t".to_string(), user_node("a")))
        .expect("entry");
    // Same id, different payload — the duplication is the corruption.
    let clone = serde_json::to_string(&SessionEntry {
        id: entry_id_of(&first),
        parent_id: None,
        timestamp: "t".to_string(),
        kind: user_node("b"),
    })
    .expect("entry");
    fs::write(&path, format!("{header}{first}\n{clone}\n")).expect("write");
    match store.open_path(&path) {
        Err(SessionError::Corrupt { message, .. }) => {
            assert!(message.contains("duplicate entry id"), "{message}")
        }
        other => panic!("expected corruption error, got {other:?}"),
    }
    fs::remove_dir_all(store.dir()).ok();
}

fn entry_id_of(line: &str) -> String {
    serde_json::from_str::<SessionEntry>(line)
        .expect("parse for id")
        .id
}

#[test]
fn unknown_parent_is_corruption() {
    let store = temp_store("orphan");
    let mut writer = store.create("C:/work");
    assert_flushed(writer.append_record(&Nodes::default().node(user_node("seed"))));
    let path = writer.path().to_path_buf();
    let header = fs::read_to_string(&path).expect("read header");
    let orphan = serde_json::to_string(&SessionEntry {
        id: "fresh".to_string(),
        parent_id: Some("ghost".to_string()),
        timestamp: "t".to_string(),
        kind: user_node("x"),
    })
    .expect("entry");
    fs::write(&path, format!("{header}{orphan}\n")).expect("write");
    match store.open_path(&path) {
        Err(SessionError::Corrupt { message, .. }) => {
            assert!(message.contains("unknown parent"), "{message}")
        }
        other => panic!("expected corruption error, got {other:?}"),
    }
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn future_format_version_is_rejected_loudly() {
    let store = temp_store("version");
    let mut writer = store.create("C:/work");
    let err = writer.append_record(&Nodes::default().node(user_node("seed")));
    assert!(err.is_none(), "version append: {err:?}");
    let path = writer.path().to_path_buf();
    let raw = fs::read_to_string(&path).expect("read");
    let mut header: serde_json::Value =
        serde_json::from_str(raw.lines().next().unwrap_or("")).expect("header json");
    header["version"] = serde_json::Value::Number(99u32.into());
    let rest = raw.split_once('\n').map(|(_, r)| r).unwrap_or("");
    fs::write(&path, format!("{}\n{}", header, rest)).expect("write");
    match store.open_path(&path) {
        Err(SessionError::Corrupt { message, .. }) => {
            assert!(message.contains("version 99"), "{message}")
        }
        other => panic!("expected version error, got {other:?}"),
    }
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn project_default_roots_at_the_start_dir_without_discovery() {
    let base = std::env::temp_dir().join("tabit-session-tests/root-discovery");
    let _ = fs::remove_dir_all(&base);
    let project = base.join("project");
    let nested = project.join("a").join("b");
    fs::create_dir_all(&nested).expect("dirs");
    fs::create_dir(project.join(".git")).expect("git dir");

    let store = SessionStore::project_default_from(&nested);
    assert_eq!(
        store.dir(),
        nested.join(".tabit").join("sessions"),
        "cwd is the root — a `.git` ancestor is not consulted (no walking)"
    );
    fs::remove_dir_all(&base).ok();
}

#[test]
fn fs_failures_buffer_and_surface_loudly() {
    let store = temp_store("io");
    // create under a path occupied by a file -> materialization fails.
    let blocker = store.dir().with_extension("blocker");
    fs::write(&blocker, "i am a file").expect("blocker");
    let nested = SessionStore::new(blocker.join("sessions"));
    // Creation is deferred: the io failure surfaces on the first
    // append's drain — and the record stays buffered (memory-first),
    // waiting for the retry.
    let mut writer = nested.create("C:/w");
    let record = Nodes::default().node(user_node("x"));
    match writer.append_record(&record) {
        Some(SessionError::Io { .. }) => {}
        other => panic!("expected Io, got {other:?}"),
    }
    assert_eq!(writer.pending(), 1, "the record waits in the outbox");

    // open of a missing file.
    match store.open_path(&store.dir().join("missing.jsonl")) {
        Err(SessionError::Io { .. }) => {}
        other => panic!("expected Io, got {other:?}"),
    }

    // list when the store path cannot exist (its parent is a plain
    // file): Linux reports ENOTDIR, a loud Io error; Windows reports
    // NotFound — indistinguishable from "directory not created yet",
    // which must read as empty — so the loud failure stays on the write
    // side (asserted above), where materialization cannot create the
    // directory.
    let listed = nested.list();
    if cfg!(windows) {
        assert_eq!(listed.expect("missing dir reads as empty").len(), 0);
    } else {
        assert!(
            matches!(listed, Err(SessionError::Io { .. })),
            "ENOTDIR is a loud failure"
        );
    }

    fs::remove_file(&blocker).ok();
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn listing_rejects_files_with_bad_headers() {
    let store = temp_store("bad-header");
    fs::create_dir_all(store.dir()).expect("dir");
    // A file with no header line at all.
    fs::write(store.dir().join("empty.jsonl"), "").expect("empty");
    match store.list() {
        Err(SessionError::Corrupt { message, .. }) => {
            assert!(message.contains("empty"), "{message}")
        }
        other => panic!("expected corrupt, got {other:?}"),
    }
    fs::remove_dir_all(store.dir()).ok();

    // A file whose first line is not a header (its own store so listing
    // hits it deterministically).
    let store = temp_store("bad-header-garbage");
    fs::create_dir_all(store.dir()).expect("dir");
    fs::write(store.dir().join("garbage.jsonl"), "not a header\n").expect("garbage");
    match store.list() {
        Err(SessionError::Parse { line, .. }) => assert_eq!(line, 1),
        other => panic!("expected parse error, got {other:?}"),
    }
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn project_default_resolves_under_the_current_directory() {
    let store = SessionStore::project_default();
    let dir = store.dir().to_string_lossy().to_string();
    assert!(
        dir.replace('\\', "/").ends_with("/.tabit/sessions"),
        "default store sits at <root>/.tabit/sessions: {dir}"
    );
}

#[test]
fn the_outbox_drains_fully_and_the_file_is_the_clean_prefix() {
    let store = temp_store("outbox");
    let mut writer = store.create("C:/work");
    let mut nodes = Nodes::default();
    let first = nodes.node(user_node("one"));
    let second = nodes.node(user_node("two"));
    assert_flushed(writer.append_record(&first));
    assert_flushed(writer.append_record(&second));
    let checkout = nodes.checkout(match &first {
        FileRecord::Node(entry) => Some(entry.id.as_str()),
        _ => None,
    });
    assert_flushed(writer.append_record(&checkout));

    // Every commit attempts the drain, so a healthy disk leaves
    // nothing buffered — and the durable offset is exactly the file's
    // length: the clean-prefix accounting the torn-write rollback
    // depends on (rollback truncates to this offset, so its honesty is
    // the file never holding more than the prefix).
    assert!(writer.outbox.is_empty(), "the outbox drained");
    let len = fs::metadata(writer.path()).expect("file").len();
    assert_eq!(
        writer.durable_offset, len,
        "the durable offset is the file's length"
    );
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn a_dead_flush_leaves_records_buffered_for_retry() {
    // The store's directory is a regular file: materialization cannot
    // succeed, so the drain refuses — the invariant stand-in for any
    // dead flush, staged portably. The commit is still memory-first:
    // the record stays buffered for the next attempt.
    let base = temp_store("dead-flush");
    fs::create_dir_all(base.dir()).expect("base dir");
    let blocked = base.dir().join("blocker");
    fs::write(&blocked, b"not a directory").expect("blocker");
    let store = SessionStore::new(&blocked);
    let mut writer = store.create("C:/work");
    let record = Nodes::default().node(user_node("one"));
    let error = writer.append_record(&record);
    assert!(error.is_some(), "the flush reported failure");
    assert_eq!(writer.pending(), 1, "the record stayed buffered");
    fs::remove_dir_all(base.dir()).ok();
}

#[test]
fn a_barrier_failure_un_buffers_the_whole_batch() {
    // Same staging: the drain cannot materialize the file, so the
    // barrier refuses — and the batch is popped back out whole.
    let base = temp_store("barrier-rollback");
    fs::create_dir_all(base.dir()).expect("base dir");
    let blocked = base.dir().join("blocker");
    fs::write(&blocked, b"not a directory").expect("blocker");
    let store = SessionStore::new(&blocked);
    let mut writer = store.create("C:/work");
    let mut nodes = Nodes::default();
    let batch = vec![
        nodes.node(user_node("b1")),
        nodes.node(user_node("b2")),
        nodes.node(user_node("b3")),
    ];
    assert!(writer.commit_records(&batch).is_err());
    assert_eq!(writer.pending(), 0, "the batch left nothing behind");
    fs::remove_dir_all(base.dir()).ok();
}

#[test]
fn last_model_reads_the_files_last_model_change() {
    let store = temp_store("model-hint");
    let mut writer = store.create("C:/work");
    let mut nodes = Nodes::default();
    assert_flushed(writer.append_record(&Nodes::side(SideKind::ModelChange {
        provider: "p".to_string(),
        model: "m".to_string(),
        thinking_level: None,
    })));
    let turn_one = nodes.node(user_node("one"));
    assert_flushed(writer.append_record(&turn_one));
    let turn_two = nodes.node(user_node("two"));
    assert_flushed(writer.append_record(&turn_two));
    // A switch recorded between the two turns.
    assert_flushed(writer.append_record(&Nodes::side(SideKind::ModelChange {
        provider: "q".to_string(),
        model: "m2".to_string(),
        thinking_level: None,
    })));
    let after = nodes.node(user_node("after switch"));
    assert_flushed(writer.append_record(&after));

    assert_eq!(
        store
            .last_model(writer.path())
            .expect("hint")
            .map(|s| s.model),
        Some("m2".to_string())
    );

    // Branch from before the switch: the hint does NOT roll back — the
    // register is the user's latest model choice in time, whichever
    // branch the conversation is on (owner ruling 2026-08).
    let FileRecord::Node(turn_two_entry) = &turn_two else {
        panic!("fixture builds nodes");
    };
    let checkout = nodes.checkout(Some(turn_two_entry.id.as_str()));
    assert_flushed(writer.append_record(&checkout));
    assert_eq!(
        store
            .last_model(writer.path())
            .expect("hint")
            .map(|s| s.model),
        Some("m2".to_string()),
        "the checkout left the session preference register alone"
    );
    fs::remove_dir_all(store.dir()).ok();
}
