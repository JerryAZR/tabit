//! Recorder-level tests: the door — validate → write → grow — the
//! pending-roundtrip slot, checkout as a pointer move, and the
//! reload round-trip through the parser.

use super::*;
use crate::entry::{EntryKind, SideKind};
use crate::store::SessionStore;
use crate::writer::SessionWriter;
use rig_core::OneOrMany;
use rig_core::completion::{Message, Usage};
use rig_core::message::{
    AssistantContent, Text, ToolCall, ToolFunction, ToolResult, ToolResultContent, UserContent,
};
use serde_json::json;
use std::fs;
use std::path::Path;

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

fn assistant_call(turn_id: &str, calls: &[&str]) -> Message {
    let content = if calls.is_empty() {
        OneOrMany::one(AssistantContent::text("done"))
    } else {
        OneOrMany::many(
            calls
                .iter()
                .map(|call| {
                    AssistantContent::ToolCall(ToolCall::new(
                        call.to_string(),
                        ToolFunction::new("echo".to_string(), json!({})),
                    ))
                })
                .collect::<Vec<_>>(),
        )
        .expect("calls")
    };
    Message::Assistant {
        id: Some(turn_id.to_string()),
        content,
    }
}

fn result(call_id: &str) -> ToolResult {
    ToolResult {
        id: call_id.to_string(),
        call_id: None,
        content: OneOrMany::one(ToolResultContent::text("ok")),
        status: None,
    }
}

fn usage(tokens: u64) -> Usage {
    let mut usage = Usage::new();
    usage.total_tokens = tokens;
    usage
}

/// The message a caught panic carries, whatever box it landed in.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_string();
    }
    String::new()
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

/// One staged-and-closed tools roundtrip through the door.
fn roundtrip(recorder: &SessionRecorder, turn_id: &str, calls: &[&str]) {
    recorder.stage_assistant(turn_id, assistant_call(turn_id, calls), usage(10));
    for call in calls {
        recorder.stage_result(turn_id, result(call));
    }
    recorder.close_roundtrip(turn_id, None);
}

#[test]
fn commits_grow_the_tree_head_and_context_together() {
    let store = temp_store("grow");
    let writer = store.create("C:/w");
    let recorder = SessionRecorder::new(writer);

    recorder.commit_steer("id-1", user_message("one"));
    recorder.commit_steer("id-2", user_message("two"));

    let branch = recorder.active_branch();
    assert_eq!(
        branch
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        vec!["id-1", "id-2"],
        "the branch follows commit order, under the announced ids"
    );
    assert_eq!(branch[0].parent_id, None);
    assert_eq!(branch[1].parent_id, Some("id-1".to_string()));
    assert_eq!(context_texts(&recorder), vec!["one", "two"]);
    assert!(recorder.id_probe().contains("id-2"));
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn checkout_moves_the_head_and_branches_the_next_commit() {
    let store = temp_store("checkout");
    let writer = store.create("C:/w");
    let recorder = SessionRecorder::new(writer);
    recorder.commit_steer("one", user_message("one"));
    recorder.commit_steer("two", user_message("two"));

    recorder
        .checkout(Some("one"), Path::new(""))
        .expect("checkout");

    assert_eq!(branch_texts(&recorder), vec!["one"], "the pointer moved");
    recorder.commit_steer("three", user_message("branched"));
    let branch = recorder.active_branch();
    assert_eq!(
        branch[1].parent_id,
        Some("one".to_string()),
        "branches from the target"
    );
    assert_eq!(branch_texts(&recorder), vec!["one", "branched"]);
    assert_eq!(
        context_texts(&recorder),
        vec!["one", "branched"],
        "the context re-projected from the new branch"
    );
    assert!(
        recorder.id_probe().contains("two"),
        "the abandoned branch stays"
    );
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn a_roundtrip_commits_atomically_and_folds_one_batch() {
    let store = temp_store("roundtrip");
    let writer = store.create("C:/w");
    let path = writer.path().to_path_buf();
    let recorder = SessionRecorder::new(writer);
    recorder.commit_steer("u1", user_message("go"));
    roundtrip(&recorder, "t1", &["c1", "c2"]);

    let messages = recorder.context();
    assert_eq!(messages.len(), 3, "user, assistant, ONE merged batch");
    assert!(matches!(&messages[2], Message::User { content } if content.len() == 2));
    // The branch holds assistant + both results, chained.
    let branch = recorder.active_branch();
    assert_eq!(branch.len(), 4, "user + assistant + two results");
    assert_eq!(branch[1].id, "t1", "the announced turn id is the entry id");
    assert_eq!(
        branch[3].parent_id,
        Some(branch[2].id.clone()),
        "results chain"
    );
    // The file holds the same four nodes, contiguously.
    let parsed = crate::parser::parse_file(&path).expect("reload");
    assert_eq!(
        parsed.tree.head(),
        Some(branch[3].id.as_str()),
        "the reload's head is the roundtrip's last node"
    );
    assert_eq!(parsed.context.messages().len(), 3);
    assert_eq!(parsed.stats.total_usage().total_tokens, 10);
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn a_final_roundtrip_commits_the_assistant_alone() {
    let store = temp_store("final");
    let writer = store.create("C:/w");
    let recorder = SessionRecorder::new(writer);
    recorder.commit_steer("u1", user_message("go"));
    recorder.stage_assistant("t1", assistant_call("t1", &[]), usage(5));
    recorder.close_roundtrip("t1", None);
    assert_eq!(recorder.context().len(), 2, "user + assistant");
    assert_eq!(recorder.stats().total_usage().total_tokens, 5);
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn a_feedback_close_commits_the_engine_message_as_the_batch() {
    let store = temp_store("feedback");
    let writer = store.create("C:/w");
    let recorder = SessionRecorder::new(writer);
    recorder.commit_steer("u1", user_message("go"));
    recorder.stage_assistant("t1", assistant_call("t1", &["c1"]), usage(7));
    let feedback = Message::User {
        content: OneOrMany::one(UserContent::ToolResult(result("c1"))),
    };
    recorder.close_roundtrip("t1", Some(feedback));

    assert_eq!(recorder.context().len(), 3, "user + assistant + feedback");
    let branch = recorder.active_branch();
    assert_eq!(branch.len(), 3);
    assert!(matches!(branch[2].kind, EntryKind::UserMessage { .. }));
    fs::remove_dir_all(store.dir()).ok();
}

/// The pending slot is single-occupancy: every mismatch is a wiring
/// bug on the engine↔door contract and fails loud.
#[test]
fn roundtrip_slot_mismatches_panics() {
    let store = temp_store("slot-mismatch");
    let writer = store.create("C:/w");
    let recorder = SessionRecorder::new(writer);
    recorder.commit_steer("u1", user_message("go"));

    // Staging a second turn while one is open.
    recorder.stage_assistant("t1", assistant_call("t1", &[]), usage(1));
    let outcome = std::panic::catch_unwind(|| {
        recorder.stage_assistant("t2", assistant_call("t2", &[]), usage(1));
    });
    let fault = panic_message(&outcome.expect_err("double staging panics"));
    assert!(fault.contains("still open"), "{fault}");

    // A result for a turn that never staged.
    let outcome = std::panic::catch_unwind(|| {
        recorder.stage_result("ghost", result("c1"));
    });
    let fault = panic_message(&outcome.expect_err("an unstaged result panics"));
    assert!(fault.contains("without its staged assistant"), "{fault}");

    // Discarding while a different turn is staged.
    let outcome = std::panic::catch_unwind(|| {
        recorder.discard_roundtrip("ghost", usage(1));
    });
    let fault = panic_message(&outcome.expect_err("a mismatched discard panics"));
    assert!(fault.contains("while `t1` was staged"), "{fault}");

    // Closing a turn that never staged (the staged one mismatches, and
    // the failed close consumes it — nothing lingers).
    let outcome = std::panic::catch_unwind(|| {
        recorder.close_roundtrip("ghost", None);
    });
    let fault = panic_message(&outcome.expect_err("a mismatched close panics"));
    assert!(fault.contains("closed while `t1` was staged"), "{fault}");
    assert!(
        recorder.context().len() == 1,
        "the mismatched close wrote nothing"
    );

    // With the slot empty, closing anything is the no-staged-turn crash.
    let outcome = std::panic::catch_unwind(|| {
        recorder.close_roundtrip("ghost", None);
    });
    let fault = panic_message(&outcome.expect_err("an empty-slot close panics"));
    assert!(fault.contains("without a staged turn"), "{fault}");
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn an_unpaired_roundtrip_panics_at_the_door() {
    let store = temp_store("unpaired");
    let writer = store.create("C:/w");
    let recorder = SessionRecorder::new(writer);
    recorder.commit_steer("u1", user_message("go"));
    recorder.stage_assistant("t1", assistant_call("t1", &["c1", "c2"]), usage(1));
    recorder.stage_result("t1", result("c1"));
    let outcome = std::panic::catch_unwind(|| {
        recorder.close_roundtrip("t1", None);
    });
    let fault = outcome.expect_err("c2 unanswered");
    let fault = panic_message(&fault);
    assert!(fault.contains("unanswered"), "{fault}");
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn a_discarded_attempt_records_usage_and_lands_nothing() {
    let store = temp_store("discard");
    let writer = store.create("C:/w");
    let path = writer.path().to_path_buf();
    let recorder = SessionRecorder::new(writer);
    recorder.record_side(SideKind::ModelChange {
        provider: "p".to_string(),
        model: "m".to_string(),
        thinking_level: None,
    });
    recorder.commit_steer("u1", user_message("go"));
    // A vetoed attempt: staged, then discarded with its usage.
    recorder.stage_assistant("t1", assistant_call("t1", &[]), usage(3));
    recorder.discard_roundtrip("t1", usage(9));

    assert_eq!(recorder.context().len(), 1, "only the user message");
    assert_eq!(
        recorder.stats().total_usage().total_tokens,
        9,
        "the discard is billed (flag 22)"
    );
    // The file records exactly the discarded side record after the user node.
    let parsed = crate::parser::parse_file(&path).expect("reload");
    assert_eq!(parsed.stats.total_usage().total_tokens, 9);
    assert_eq!(parsed.tree.head(), Some("u1"), "the tree never grew");
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn drop_open_roundtrip_leaves_no_trace() {
    let store = temp_store("abort");
    let writer = store.create("C:/w");
    let path = writer.path().to_path_buf();
    let recorder = SessionRecorder::new(writer);
    recorder.commit_steer("u1", user_message("go"));
    recorder.stage_assistant("t1", assistant_call("t1", &["c1"]), usage(4));
    recorder.stage_result("t1", result("c1"));
    recorder.drop_open_roundtrip();

    assert_eq!(recorder.context().len(), 1);
    let parsed = crate::parser::parse_file(&path).expect("reload");
    assert_eq!(
        parsed.tree.head(),
        Some("u1"),
        "the aborted roundtrip never landed"
    );
    assert_eq!(
        parsed.stats.total_usage().total_tokens,
        0,
        "unbilled: not a ruled discard"
    );
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn a_trailing_checkout_survives_the_reload() {
    let store = temp_store("checkout-durable");
    let writer = store.create("C:/w");
    let path = writer.path().to_path_buf();
    let recorder = SessionRecorder::new(writer);
    recorder.commit_steer("one", user_message("one"));
    recorder.commit_steer("two", user_message("dropped"));
    recorder
        .checkout(Some("one"), Path::new(""))
        .expect("checkout");
    drop(recorder);

    // The next process adopts the same resident state from the one pass.
    let parsed = store.open_path(&path).expect("open");
    let writer = SessionWriter::append_to(&path, "s".to_string(), parsed.file_len).expect("reopen");
    let recorder = SessionRecorder::new(writer);
    recorder.adopt(parsed);
    assert_eq!(
        branch_texts(&recorder),
        vec!["one"],
        "nothing was appended after the checkout, and the head still moved"
    );
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn a_mid_roundtrip_checkout_target_panics() {
    // The owner's flag-23 ruling: not supported, revisited later.
    let store = temp_store("mid-batch");
    let writer = store.create("C:/w");
    let recorder = SessionRecorder::new(writer);
    recorder.commit_steer("u1", user_message("go"));
    roundtrip(&recorder, "t1", &["c1", "c2"]);
    let branch = recorder.active_branch();
    let mid = branch[2].id.clone(); // the first result of the batch

    let outcome = std::panic::catch_unwind(|| {
        let _ = recorder.checkout(Some(&mid), Path::new(""));
    });
    let fault = panic_message(&outcome.expect_err("a mid-roundtrip target panics"));
    assert!(
        fault.contains("open tool roundtrip"),
        "the panic names the ruled shape: {fault}"
    );
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn checkout_rejects_an_unknown_target_gracefully() {
    let store = temp_store("unknown");
    let writer = store.create("C:/w");
    let recorder = SessionRecorder::new(writer);
    match recorder.checkout(Some("ghost"), Path::new("")) {
        Err(SessionError::Corrupt { message, .. }) => {
            assert!(message.contains("not in this session"), "{message}")
        }
        other => panic!("expected a graceful error, got {other:?}"),
    }
    fs::remove_dir_all(store.dir()).ok();
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

    recorder.commit_steer("standing", user_message("standing"));

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
    fs::remove_dir_all(store.dir()).ok();
}

#[test]
fn the_deferred_register_rides_the_first_barrier() {
    let store = temp_store("deferred");
    let writer = store.create("C:/w");
    let path = writer.path().to_path_buf();
    let recorder = SessionRecorder::new(writer);
    recorder.defer_register(tabit_protocol::ModelSelection {
        provider: "p".to_string(),
        model: "m".to_string(),
        thinking_level: None,
    });
    assert!(!path.exists(), "nothing materialized yet");
    recorder
        .commit_barrier(vec![(None, user_node("go"))])
        .expect("barrier");
    let parsed = crate::parser::parse_file(&path).expect("reload");
    assert_eq!(
        parsed.register.map(|r| r.model),
        Some("m".to_string()),
        "the opening model_change rode the first commit"
    );
    fs::remove_dir_all(store.dir()).ok();
}
