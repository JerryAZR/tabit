//! The store: directory management, listing, and the file-naming
//! contract. Parsing semantics live in `parser_tests`; write mechanics
//! in `writer_tests`.

use super::*;
use crate::entry::{EntryKind, SessionEntry};

fn temp_store(tag: &str) -> SessionStore {
    let dir = std::env::temp_dir()
        .join("tabit-session-tests")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    SessionStore::new(&dir)
}

fn user_node() -> crate::entry::FileRecord {
    crate::entry::FileRecord::Node(SessionEntry::new(
        None,
        "t".to_string(),
        EntryKind::UserMessage {
            message: rig_core::completion::Message::user("x"),
        },
    ))
}

fn commit_one(writer: &mut SessionWriter) {
    assert!(
        tabit_log::WriteBuffer::enqueue(writer, &[user_node()]).is_ok(),
        "the fixture write drains"
    );
}

#[test]
fn create_defers_to_disk_until_the_first_commit() {
    let store = temp_store("orphan-gate");
    let mut writer = store.create("C:/work");
    assert!(
        !writer.path().exists(),
        "a session that never commits leaves no file"
    );
    commit_one(&mut writer);
    assert!(writer.path().exists());
    let parsed = store.open_path(writer.path()).expect("open");
    assert_eq!(parsed.header.cwd, "C:/work");
    assert!(
        parsed.tree.head().is_some(),
        "the committed node is the head"
    );
    std::fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn open_by_session_id_finds_the_file() {
    let store = temp_store("by-id");
    let mut writer = store.create("C:/work");
    commit_one(&mut writer);
    let id = writer.session_id().to_string();
    let parsed = store.open(&id).expect("open by id");
    assert_eq!(parsed.header.id, id);
    assert!(store.open("no-such-id").is_err());
    std::fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn a_torn_tail_fails_the_open_loudly() {
    // The repair pass is deleted: corruption is named, never patched.
    let store = temp_store("torn");
    let mut writer = store.create("C:/work");
    commit_one(&mut writer);
    let path = writer.path().to_path_buf();
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open for tear");
    write!(file, r#"{{"id":"torn","timestamp":"t","kind":"lab"#).expect("tear");
    match store.open_path(&path) {
        Err(SessionError::Parse { line, .. }) => assert_eq!(line, 3),
        other => panic!("a torn tail fails loud, got {other:?}"),
    }
    std::fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn list_orders_newest_first_and_counts_records() {
    let store = temp_store("list");
    let mut older = store.create("C:/a");
    commit_one(&mut older);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let mut newer = store.create("C:/b");
    commit_one(&mut newer);

    let summaries = store.list().expect("list");
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].id, newer.session_id());
    assert_eq!(summaries[0].entry_count, 1);
    assert_eq!(summaries[1].id, older.session_id());
    std::fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn project_default_roots_at_the_start_dir_without_discovery() {
    let base = std::env::temp_dir().join("tabit-session-tests/root-discovery");
    let _ = std::fs::remove_dir_all(&base);
    let project = base.join("project");
    let nested = project.join("a").join("b");
    std::fs::create_dir_all(&nested).expect("dirs");
    std::fs::create_dir(project.join(".git")).expect("git dir");

    let store = SessionStore::project_default_from(&nested);
    assert_eq!(
        store.dir(),
        nested.join(".tabit").join("sessions"),
        "cwd is the root — a `.git` ancestor is not consulted (no walking)"
    );
    std::fs::remove_dir_all(&base).ok();
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
fn listing_rejects_files_with_bad_headers() {
    let store = temp_store("bad-header");
    std::fs::create_dir_all(store.dir()).expect("dir");
    std::fs::write(store.dir().join("empty.jsonl"), "").expect("empty");
    match store.list() {
        Err(SessionError::Corrupt { message, .. }) => {
            assert!(message.contains("empty"), "{message}")
        }
        other => panic!("expected corrupt, got {other:?}"),
    }
    std::fs::remove_dir_all(store.dir()).ok();

    let store = temp_store("bad-header-garbage");
    std::fs::create_dir_all(store.dir()).expect("dir");
    std::fs::write(store.dir().join("garbage.jsonl"), "not a header\n").expect("garbage");
    match store.list() {
        Err(SessionError::Parse { line, .. }) => assert_eq!(line, 1),
        other => panic!("expected parse error, got {other:?}"),
    }
    std::fs::remove_dir_all(store.dir()).ok();
}
