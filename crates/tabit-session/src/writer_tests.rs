//! The write queue: materialization, the two write verbs' failure
//! policies, the clean-prefix accounting.

use super::*;
use crate::entry::{EntryKind, SessionEntry};

fn temp_path(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("tabit-writer-tests")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("session.jsonl")
}

fn header() -> SessionHeader {
    SessionHeader {
        version: crate::entry::SESSION_FORMAT_VERSION,
        id: "sid".to_string(),
        created_at: "t".to_string(),
        cwd: "C:/w".to_string(),
        parent_session: None,
    }
}

fn user_record(id: &str, parent: Option<&str>) -> FileRecord {
    FileRecord::Node(SessionEntry::with_id(
        id.to_string(),
        parent.map(str::to_string),
        "t".to_string(),
        EntryKind::UserMessage {
            message: rig_core::completion::Message::user("x"),
        },
    ))
}

#[test]
fn a_fresh_writer_touches_nothing_until_the_first_drain() {
    let path = temp_path("orphan-gate");
    let mut writer = SessionWriter::create(path.clone(), header());
    assert!(
        !path.exists(),
        "creation materializes nothing — the no-orphan gate"
    );
    // Pre-population (the deferred opening model_change) buffers without
    // draining; the file still does not exist.
    assert!(
        writer
            .buffer(&FileRecord::Side(crate::entry::SideRecord {
                timestamp: "t".to_string(),
                kind: crate::entry::SideKind::Aborted,
            }))
            .is_none()
    );
    assert!(!path.exists(), "buffering alone touches nothing");
    assert_eq!(writer.pending(), 2, "the header line plus the record");

    // The first drain materializes the file with everything, in order.
    assert!(writer.write_behind(&[user_record("a", None)]).is_none());
    let raw = std::fs::read_to_string(&path).expect("read");
    let mut lines = raw.lines();
    assert!(lines.next().unwrap_or_default().contains("\"version\""));
    assert!(
        lines
            .next()
            .unwrap_or_default()
            .contains("\"kind\":\"aborted\"")
    );
    assert!(
        lines
            .next()
            .unwrap_or_default()
            .contains("\"kind\":\"user_message\"")
    );
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn the_drain_leaves_a_clean_prefix_and_advances_the_offset() {
    let path = temp_path("clean-prefix");
    let mut writer = SessionWriter::create(path.clone(), header());
    assert!(writer.write_behind(&[user_record("a", None)]).is_none());
    assert!(
        writer
            .write_behind(&[user_record("b", Some("a"))])
            .is_none()
    );
    assert_eq!(writer.pending(), 0, "a healthy disk drains everything");
    let len = std::fs::metadata(&path).expect("file").len();
    assert_eq!(
        writer.durable_offset, len,
        "the durable offset is the file's length"
    );
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

/// A writer whose file cannot materialize: the sessions directory's
/// parent is a plain file. The portable stand-in for any dead flush.
fn blocked_writer(tag: &str) -> SessionWriter {
    let dir = std::env::temp_dir()
        .join("tabit-writer-tests")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let blocker = dir.join("blocker");
    std::fs::write(&blocker, b"not a directory").expect("blocker");
    SessionWriter::create(blocker.join("sessions/x.jsonl"), header())
}

#[test]
fn a_failed_write_behind_keeps_its_lines_for_the_retry() {
    let mut writer = blocked_writer("write-behind-retry");
    assert_eq!(writer.pending(), 1, "the header waits in the outbox");
    let error = writer.write_behind(&[user_record("a", None)]);
    assert!(error.is_some(), "the drain reported failure");
    assert_eq!(
        writer.pending(),
        2,
        "header + record stay queued — memory already accepted them"
    );
}

#[test]
fn a_failed_gated_write_pops_the_batch_back_out() {
    let mut writer = blocked_writer("gated-rollback");
    let batch = vec![user_record("b1", None), user_record("b2", Some("b1"))];
    assert!(writer.write_gated(&batch).is_err());
    assert_eq!(
        writer.pending(),
        1,
        "only the pre-queued header remains — the batch exists nowhere"
    );
}

#[test]
fn append_to_resumes_from_the_files_end() {
    let path = temp_path("resume");
    let mut writer = SessionWriter::create(path.clone(), header());
    assert!(writer.write_behind(&[user_record("a", None)]).is_none());
    let len = std::fs::metadata(&path).expect("file").len();

    let mut resumed =
        SessionWriter::append_to(&path, "sid".to_string(), len).expect("append_to opens");
    assert_eq!(resumed.pending(), 0);
    assert!(
        resumed
            .write_behind(&[user_record("b", Some("a"))])
            .is_none()
    );
    let raw = std::fs::read_to_string(&path).expect("read");
    assert_eq!(raw.lines().count(), 3, "header + two records, no gap");
    assert_eq!(
        resumed.durable_offset,
        std::fs::metadata(&path).unwrap().len()
    );
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}
