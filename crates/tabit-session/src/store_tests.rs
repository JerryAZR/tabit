use super::*;
use crate::entry::EntryKind;
use rig_core::OneOrMany;
use rig_core::completion::Message;
use rig_core::message::{Text, UserContent};
use std::fs;

/// One entry of the least-consequential kind: enough to materialize a
/// deferred session file without coloring what the test inspects.
fn seed_entry() -> EntryKind {
    EntryKind::Label {
        name: "seed".to_string(),
    }
}

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

#[test]
fn create_writes_header_and_appends_chain_parents() {
    let store = temp_store("create");
    let mut writer = store.create("C:/work");
    let first = writer
        .append(EntryKind::UserMessage {
            message: user_message("one"),
        })
        .expect("append");
    assert_eq!(first.parent_id, None);
    let second = writer
        .append(EntryKind::UserMessage {
            message: user_message("two"),
        })
        .expect("append");
    assert_eq!(second.parent_id, Some(first.id.clone()));
    assert_eq!(writer.leaf(), Some(second.id.as_str()));

    let loaded = store.open_path(writer.path()).expect("open");
    assert_eq!(loaded.entries.len(), 2);
    assert_eq!(loaded.header.cwd, "C:/work");
    assert!(loaded.repairs.is_empty());
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn open_by_session_id_finds_the_file() {
    let store = temp_store("by-id");
    let mut writer = store.create("C:/work");
    writer.append(seed_entry()).expect("materialize");
    let id = writer.session_id().to_string();
    let loaded = store.open(&id).expect("open by id");
    assert_eq!(loaded.header.id, id);
    assert!(store.open("no-such-id").is_err());
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn list_orders_newest_first_and_counts_entries() {
    let store = temp_store("list");
    let mut older = store.create("C:/a");
    older.append(seed_entry()).expect("materialize");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let mut newer = store.create("C:/b");
    newer
        .append(EntryKind::UserMessage {
            message: user_message("x"),
        })
        .expect("append");

    let summaries = store.list().expect("list");
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].id, newer.session_id());
    assert_eq!(summaries[0].entry_count, 1);
    // Deferred creation means header-only sessions do not exist; the
    // older file carries its materializing entry.
    assert_eq!(summaries[1].entry_count, 1);
    assert_eq!(summaries[1].id, older.session_id());
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn torn_tail_is_repaired_with_report() {
    let store = temp_store("torn");
    let mut writer = store.create("C:/work");
    writer
        .append(EntryKind::UserMessage {
            message: user_message("kept"),
        })
        .expect("append");
    let path = writer.path().to_path_buf();

    // Simulate a crash mid-append: partial JSON without a trailing newline.
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open for tear");
    write!(file, r#"{{"id":"torn","timestamp":"t","kind":"lab"#).expect("tear");

    let loaded = store.open_path(&path).expect("open with repair");
    assert_eq!(loaded.entries.len(), 1, "the complete record survives");
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
    writer
        .append(EntryKind::UserMessage {
            message: user_message("one"),
        })
        .expect("append");
    let path = writer.path().to_path_buf();
    let mut lines = fs::read_to_string(&path).expect("read");
    lines.push_str("this is not json at all\n");
    lines.push_str(
        &serde_json::to_string(&EntryKind::UserMessage {
            message: user_message("three"),
        })
        .map(|kind| format!("{{\"id\":\"z\",\"parent_id\":null,\"timestamp\":\"t\",\"{kind}\"}}\n"))
        .expect("serialize"),
    );
    // Replace the middle line's wrapper to keep it malformed but positioned
    // mid-file (the trailing record stays valid).
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
    writer.append(seed_entry()).expect("materialize");
    let path = writer.path().to_path_buf();
    let header = fs::read_to_string(&path).expect("read header");
    let entry = serde_json::to_string(&SessionEntry::new(
        None,
        "t".to_string(),
        EntryKind::Label {
            name: "a".to_string(),
        },
    ))
    .expect("entry");
    let same = serde_json::to_string(&SessionEntry {
        id: entry_id_of(&entry),
        parent_id: None,
        timestamp: "t".to_string(),
        kind: EntryKind::Label {
            name: "b".to_string(),
        },
    })
    .expect("entry");
    fs::write(&path, format!("{header}{entry}\n{same}\n")).expect("write");
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
    writer.append(seed_entry()).expect("materialize");
    let path = writer.path().to_path_buf();
    let header = fs::read_to_string(&path).expect("read header");
    let orphan = serde_json::to_string(&SessionEntry {
        id: "fresh".to_string(),
        parent_id: Some("ghost".to_string()),
        timestamp: "t".to_string(),
        kind: EntryKind::Label {
            name: "x".to_string(),
        },
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
    writer.append(seed_entry()).expect("materialize");
    let path = writer.path().to_path_buf();
    let raw = fs::read_to_string(&path).expect("read");
    let mut header: serde_json::Value =
        serde_json::from_str(raw.lines().next().unwrap_or("")).expect("header json");
    header["version"] = json_from_u32(99);
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

fn json_from_u32(v: u32) -> serde_json::Value {
    serde_json::Value::Number(v.into())
}

#[test]
fn project_root_discovery_prefers_git_ancestor() {
    let base = std::env::temp_dir().join("tabit-session-tests/root-discovery");
    let _ = fs::remove_dir_all(&base);
    let project = base.join("project");
    let nested = project.join("a").join("b");
    fs::create_dir_all(&nested).expect("dirs");
    fs::create_dir(project.join(".git")).expect("git dir");

    let store = SessionStore::project_default_from(&nested);
    assert_eq!(
        store.dir(),
        project.join(".tabit").join("sessions"),
        "discovery walks up to the git root"
    );

    let no_git = SessionStore::project_default_from(&nested);
    fs::remove_dir_all(project.join(".git")).expect("remove git");
    let fallback = SessionStore::project_default_from(&nested);
    assert_eq!(
        fallback.dir(),
        nested.join(".tabit").join("sessions"),
        "without .git the start dir is the project root; store {}/{} vs {}/{}",
        no_git.dir().display(),
        fallback.dir().display(),
        nested.display(),
        nested.display()
    );
    fs::remove_dir_all(&base).ok();
}

#[test]
fn fs_failures_are_loud_io_errors() {
    let store = temp_store("io");
    // create under a path occupied by a file -> create_dir_all fails.
    let blocker = store.dir().with_extension("blocker");
    fs::write(&blocker, "i am a file").expect("blocker");
    let nested = SessionStore::new(blocker.join("sessions"));
    // Creation is deferred: the io failure surfaces on the first append,
    // when the file would materialize under the blocked path.
    let mut writer = nested.create("C:/w");
    match writer.append(EntryKind::Aborted) {
        Err(SessionError::Io { .. }) => {}
        other => panic!("expected Io, got {other:?}"),
    }

    // open of a missing file.
    match store.open_path(&store.dir().join("missing.jsonl")) {
        Err(SessionError::Io { .. }) => {}
        other => panic!("expected Io, got {other:?}"),
    }

    // list when the store path cannot exist (its parent is a plain
    // file): indistinguishable from "directory not created yet" at read
    // time, so it reads as empty — the loud failure belongs to the write
    // side, where materialization cannot create the directory (asserted
    // above).
    assert_eq!(nested.list().expect("missing dir reads as empty").len(), 0);

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
fn rewind_moves_the_leaf_and_branches_the_next_append() {
    let store = temp_store("rewind-writer");
    let mut writer = store.create("C:/work");
    let first = writer
        .append(EntryKind::UserMessage {
            message: user_message("one"),
        })
        .expect("append");
    let second = writer
        .append(EntryKind::UserMessage {
            message: user_message("two"),
        })
        .expect("append");
    writer.rewind_to(Some(&first.id)).expect("rewind");
    assert_eq!(writer.leaf(), Some(first.id.as_str()));

    let third = writer
        .append(EntryKind::UserMessage {
            message: user_message("branched"),
        })
        .expect("append");
    assert_eq!(
        third.parent_id,
        Some(first.id.clone()),
        "branches from the target"
    );

    // The marker recorded the abandoned tip honestly, and load follows
    // the branch: the chain skips `second`, the file keeps everything.
    let loaded = store.open_path(writer.path()).expect("open");
    assert_eq!(
        loaded.entries.len(),
        4,
        "nothing was deleted, one marker added"
    );
    assert!(matches!(
        &loaded.entries[2].kind,
        EntryKind::Rewound { to: Some(target) } if target == &first.id
    ));
    assert_eq!(
        loaded.entries[2].parent_id,
        Some(second.id.clone()),
        "the marker parents to the abandoned tip"
    );
    let chain_texts: Vec<&str> = loaded
        .chain
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::UserMessage {
                message: Message::User { content },
            } => content.iter().find_map(|part| match part {
                UserContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    assert_eq!(chain_texts, vec!["one", "branched"]);
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn trailing_marker_directs_the_reopened_writer() {
    // A rewind as the final write must survive reopen: nothing was
    // appended to carry the new parent implicitly.
    let store = temp_store("rewind-durable");
    let mut writer = store.create("C:/work");
    let first = writer
        .append(EntryKind::UserMessage {
            message: user_message("one"),
        })
        .expect("append");
    writer
        .append(EntryKind::UserMessage {
            message: user_message("dropped"),
        })
        .expect("append");
    writer.rewind_to(Some(&first.id)).expect("rewind");
    let path = writer.path().to_path_buf();
    drop(writer);

    let loaded = store.open_path(&path).expect("open");
    assert_eq!(loaded.chain.len(), 1, "the abandoned tail is off-chain");

    let mut reopened = SessionWriter::open_existing(&path).expect("reopen");
    let next = reopened
        .append(EntryKind::UserMessage {
            message: user_message("after reopen"),
        })
        .expect("append");
    assert_eq!(next.parent_id, Some(first.id));
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn rewind_to_none_branches_from_the_root() {
    let store = temp_store("rewind-root");
    let mut writer = store.create("C:/work");
    writer
        .append(EntryKind::UserMessage {
            message: user_message("only"),
        })
        .expect("append");
    writer.rewind_to(None).expect("rewind to root");
    assert_eq!(writer.leaf(), None);

    let loaded = store.open_path(writer.path()).expect("open");
    assert!(loaded.chain.is_empty(), "the root chain is empty");
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn marker_targeting_an_unknown_entry_is_corrupt() {
    let store = temp_store("rewind-corrupt");
    let mut writer = store.create("C:/work");
    writer.append(seed_entry()).expect("materialize");
    let path = writer.path().to_path_buf();
    let header = fs::read_to_string(&path).expect("read header");
    let marker = serde_json::to_string(&SessionEntry {
        id: "rw".to_string(),
        parent_id: None,
        timestamp: "t".to_string(),
        kind: EntryKind::Rewound {
            to: Some("ghost".to_string()),
        },
    })
    .expect("marker");
    fs::write(&path, format!("{header}{marker}\n")).expect("write");
    match store.open_path(&path) {
        Err(SessionError::Corrupt { message, .. }) => {
            assert!(message.contains("unknown or later"), "{message}")
        }
        other => panic!("expected corruption error, got {other:?}"),
    }
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn last_model_follows_the_active_chain_not_the_file() {
    let store = temp_store("rewind-model-hint");
    let mut writer = store.create("C:/work");
    writer
        .append(EntryKind::ModelChange {
            provider: "p".to_string(),
            model: "m".to_string(),
            thinking_level: None,
        })
        .expect("append");
    let turn_one = writer
        .append(EntryKind::UserMessage {
            message: user_message("one"),
        })
        .expect("append");
    let turn_two = writer
        .append(EntryKind::UserMessage {
            message: user_message("two"),
        })
        .expect("append");
    // A switch recorded between the two turns.
    writer
        .append(EntryKind::ModelChange {
            provider: "q".to_string(),
            model: "m2".to_string(),
            thinking_level: None,
        })
        .expect("append");
    writer
        .append(EntryKind::UserMessage {
            message: user_message("after switch"),
        })
        .expect("append");

    // Before rewinding: the file's last model_change is q/m2.
    assert_eq!(
        store
            .last_model(writer.path())
            .expect("hint")
            .map(|s| s.model),
        Some("m2".to_string())
    );

    // Branch from before the switch: the hint rolls back with the chain.
    writer.rewind_to(Some(&turn_two.id)).expect("rewind");
    assert_eq!(
        store
            .last_model(writer.path())
            .expect("hint")
            .map(|s| s.model),
        Some("m".to_string())
    );
    // Turn one's entry is still reachable as an earlier chain node.
    let loaded = store.open_path(writer.path()).expect("open");
    assert!(loaded.chain.iter().any(|entry| entry.id == turn_one.id));
    fs::remove_dir_all(store.dir()).ok();
}
