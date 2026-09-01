//! The write buffer's one interface: enqueue — the no-orphan gate, the
//! clean-prefix accounting, keep-on-failure, the drop-time flush.

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

/// A writer whose file cannot materialize, plus the blocker to remove
/// when a test wants the retry to succeed: the sessions directory's
/// parent is a plain file, the portable stand-in for any dead flush.
fn blocked_writer(tag: &str) -> (SessionWriter, PathBuf) {
    let dir = std::env::temp_dir()
        .join("tabit-writer-tests")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let blocker = dir.join("blocker");
    std::fs::write(&blocker, b"not a directory").expect("blocker");
    let writer = SessionWriter::create(blocker.join("sessions/x.jsonl"), header());
    (writer, blocker)
}

#[test]
fn a_fresh_writer_touches_nothing_until_the_first_enqueue() {
    let path = temp_path("orphan-gate");
    let mut writer = SessionWriter::create(path.clone(), header());
    assert!(
        !path.exists(),
        "creation materializes nothing — the no-orphan gate"
    );
    // The first enqueue's write attempt materializes the file with
    // everything queued, in order.
    writer
        .enqueue(&[user_record("a", None)])
        .expect("a healthy disk accepts the batch");
    let raw = std::fs::read_to_string(&path).expect("read");
    let mut lines = raw.lines();
    assert!(lines.next().unwrap_or_default().contains("\"version\""));
    assert!(
        lines
            .next()
            .unwrap_or_default()
            .contains("\"kind\":\"user_message\"")
    );
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn a_never_committed_writer_leaves_nothing_behind_on_drop() {
    let path = temp_path("orphan-drop");
    let writer = SessionWriter::create(path.clone(), header());
    drop(writer);
    assert!(
        !path.exists(),
        "the drop flush is gated on having ever materialized the file"
    );
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn the_write_leaves_a_clean_prefix_and_advances_the_offset() {
    let path = temp_path("clean-prefix");
    let mut writer = SessionWriter::create(path.clone(), header());
    writer
        .enqueue(&[user_record("a", None)])
        .expect("first batch");
    writer
        .enqueue(&[user_record("b", Some("a"))])
        .expect("second batch");
    assert_eq!(writer.pending(), 0, "a healthy disk drains everything");
    assert_eq!(
        writer.durable_offset,
        std::fs::metadata(&path).expect("file").len(),
        "the durable offset is the file's length"
    );
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn a_failed_enqueue_keeps_its_lines_for_the_retry() {
    let (mut writer, blocker) = blocked_writer("enqueue-keeps");
    assert_eq!(writer.pending(), 1, "the header waits in the outbox");
    assert!(
        writer.enqueue(&[user_record("a", None)]).is_err(),
        "the write attempt reported failure"
    );
    assert_eq!(
        writer.pending(),
        2,
        "header + record stay queued — the Err is a report, not an undo"
    );
    std::fs::remove_dir_all(blocker.parent().unwrap()).ok();
}

#[test]
fn a_later_enqueue_retries_the_queued_lines() {
    let (mut writer, blocker) = blocked_writer("enqueue-retries");
    assert!(writer.enqueue(&[user_record("a", None)]).is_err());
    // Unblock the path: the next enqueue's attempt writes everything
    // queued — the retried batch first, then its own.
    std::fs::remove_file(&blocker).expect("unblock");
    writer
        .enqueue(&[user_record("b", Some("a"))])
        .expect("the retry succeeds");
    assert_eq!(writer.pending(), 0);
    let path = writer.path().to_path_buf();
    let raw = std::fs::read_to_string(&path).expect("read");
    assert_eq!(raw.lines().count(), 3, "header + both records, no gap");
    assert!(
        raw.lines()
            .nth(2)
            .unwrap_or_default()
            .contains("\"id\":\"b\"")
    );
    std::fs::remove_dir_all(blocker.parent().unwrap()).ok();
}

#[test]
fn append_to_resumes_from_the_files_end() {
    let path = temp_path("resume");
    let mut writer = SessionWriter::create(path.clone(), header());
    writer
        .enqueue(&[user_record("a", None)])
        .expect("first batch");
    let len = std::fs::metadata(&path).expect("file").len();

    let mut resumed =
        SessionWriter::append_to(&path, "sid".to_string(), len).expect("append_to opens");
    assert_eq!(resumed.pending(), 0);
    resumed
        .enqueue(&[user_record("b", Some("a"))])
        .expect("appended batch");
    let raw = std::fs::read_to_string(&path).expect("read");
    assert_eq!(raw.lines().count(), 3, "header + two records, no gap");
    assert_eq!(
        resumed.durable_offset,
        std::fs::metadata(&path).unwrap().len()
    );
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn dropping_a_committed_writer_flushes_its_tail() {
    let path = temp_path("drop-flush");
    let mut writer = SessionWriter::create(path.clone(), header());
    writer
        .enqueue(&[user_record("a", None)])
        .expect("materializes the file");
    // Stand in for a mid-session write failure: lines queued, file
    // already materialized (enqueue always attempts, so a real
    // failure needs I/O to die between two attempts).
    let stranded = serde_json::to_string(&user_record("b", Some("a"))).expect("serializes");
    writer.outbox.push_back(stranded);
    drop(writer);
    let raw = std::fs::read_to_string(&path).expect("read");
    assert_eq!(raw.lines().count(), 3, "the drop flush wrote the tail");
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

fn model_change_record() -> FileRecord {
    FileRecord::Side(crate::entry::SideRecord {
        timestamp: "t".to_string(),
        kind: crate::entry::SideKind::ModelChange {
            provider: "p".to_string(),
            model: "m".to_string(),
            thinking_level: None,
        },
    })
}

#[test]
fn an_unborn_session_survives_every_flush_without_a_file() {
    let path = temp_path("unborn-flush");
    let mut writer = SessionWriter::create(path.clone(), header());
    // Birth lines only — the opening model_change (a selection, not a
    // user message: opening a tab and changing model stays off disk).
    writer.prequeue(&model_change_record());
    // The exit-time flush (what the worker calls at wind-down): an
    // outbox of birth lines with no file is a no-op.
    writer
        .flush_on_exit()
        .expect("the exit flush never fails here");
    assert!(
        !path.exists(),
        "no file + no user message: the flush materializes nothing"
    );
    // The drop path agrees.
    drop(writer);
    assert!(!path.exists(), "drop manufactures no orphan either");
}

#[test]
fn the_first_commit_flushes_the_birth_lines_with_it() {
    let path = temp_path("born-flush");
    let mut writer = SessionWriter::create(path.clone(), header());
    writer.prequeue(&model_change_record());
    writer
        .enqueue(&[user_record("a", None)])
        .expect("a healthy disk accepts the batch");
    let raw = std::fs::read_to_string(&path).expect("read");
    assert_eq!(
        raw.lines().count(),
        3,
        "header + opening model_change + the user message: {raw}"
    );
    assert!(raw.contains("model_change"), "the register landed: {raw}");
}

#[test]
fn a_degraded_unborn_session_still_leaves_no_file() {
    // The disk is dead from the start: the flush attempt must not
    // create the file even while the outbox grows.
    let (mut writer, _blocker) = blocked_writer("unborn-degraded");
    writer.prequeue(&model_change_record());
    let path = writer.path().to_path_buf();
    let _ = writer.flush_on_exit();
    assert!(!path.exists(), "degraded ≠ born: still no orphan");
}
