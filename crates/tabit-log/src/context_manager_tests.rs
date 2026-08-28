//! The ContextManager's checks: every verify arm of `fold`/`fold_all`,
//! the derived view, the atomic commit, and the checkout rule.

use super::*;
use crate::entry::{EntryKind, FileRecord, SessionEntry, SideKind, SideRecord};
use crate::error::LogError;
use crate::tree::SessionTree;
use crate::writer::WriteBuffer;
use rig_core::OneOrMany;
use rig_core::completion::{Message, Usage};
use rig_core::message::{
    AssistantContent, ToolCall, ToolFunction, ToolResult, ToolResultContent, UserContent,
};

/// A test buffer: an observation tap shared with the `SharedBuffer` the
/// manager holds. It records **batches** — one entry per `queue` call —
/// so tests assert not just what was queued but that a commit arrived
/// as one all-or-nothing unit.
#[derive(Clone, Default)]
struct BufferTap(std::sync::Arc<std::sync::Mutex<Vec<Vec<FileRecord>>>>);

impl BufferTap {
    fn shared(&self) -> SharedBuffer {
        std::sync::Arc::new(std::sync::Mutex::new(self.clone()))
    }

    fn batches(&self) -> Vec<Vec<FileRecord>> {
        self.0.lock().expect("tap lock").clone()
    }

    fn records(&self) -> Vec<FileRecord> {
        self.batches().into_iter().flatten().collect()
    }
}

impl WriteBuffer for BufferTap {
    fn prequeue(&mut self, record: &FileRecord) {
        self.0.lock().expect("tap lock").push(record.clone());
    }

    fn pending(&self) -> usize {
        self.0.lock().expect("tap lock").len()
    }

    fn take_degraded_transition(&mut self) -> Option<bool> {
        None
    }

    fn enqueue(&mut self, records: &[FileRecord]) -> Result<(), LogError> {
        self.0.lock().expect("tap lock").push(records.to_vec());
        Ok(())
    }
}

fn manager() -> (ContextManager, BufferTap) {
    let tap = BufferTap::default();
    (ContextManager::empty(tap.shared()), tap)
}

fn user(text: &str) -> Message {
    Message::user(text)
}

fn assistant_text(text: &str) -> Message {
    Message::Assistant {
        id: None,
        content: OneOrMany::one(AssistantContent::text(text)),
    }
}

fn call(id: &str) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall::new(
        id.to_string(),
        ToolFunction::new("echo".to_string(), serde_json::json!({})),
    ))
}

fn result(id: &str, text: &str) -> UserContent {
    UserContent::ToolResult(ToolResult {
        id: id.to_string(),
        call_id: None,
        content: OneOrMany::one(ToolResultContent::text(text)),
        status: None,
    })
}

fn results_message(parts: Vec<UserContent>) -> Message {
    Message::User {
        content: OneOrMany::many(parts).expect("non-empty"),
    }
}

fn node(record: &FileRecord) -> &SessionEntry {
    let FileRecord::Node(entry) = record else {
        panic!("expected a node record, got `{record:?}`");
    };
    entry
}

/// A closed conversation tree with known ids: u1 → (a1 + r1 + r2) → u2.
fn sample_tree() -> SessionTree {
    let mut tree = SessionTree::empty();
    let assistant = Message::Assistant {
        id: None,
        content: OneOrMany::many(vec![call("c1"), call("c2")]).expect("non-empty"),
    };
    let mut entries = vec![
        SessionEntry::with_id(
            "u1".to_string(),
            None,
            "t".to_string(),
            EntryKind::UserMessage {
                message: user("go"),
            },
        ),
        SessionEntry::with_id(
            "a1".to_string(),
            Some("u1".to_string()),
            "t".to_string(),
            EntryKind::AssistantMessage {
                message: assistant,
                usage: Usage::new(),
            },
        ),
        SessionEntry::with_id(
            "r1".to_string(),
            Some("a1".to_string()),
            "t".to_string(),
            EntryKind::ToolResult {
                result: ToolResult {
                    id: "c1".to_string(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::text("one")),
                    status: None,
                },
            },
        ),
        SessionEntry::with_id(
            "r2".to_string(),
            Some("r1".to_string()),
            "t".to_string(),
            EntryKind::ToolResult {
                result: ToolResult {
                    id: "c2".to_string(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::text("two")),
                    status: None,
                },
            },
        ),
        SessionEntry::with_id(
            "u2".to_string(),
            Some("r2".to_string()),
            "t".to_string(),
            EntryKind::UserMessage {
                message: user("again"),
            },
        ),
    ];
    for entry in entries.drain(..) {
        tree.load_append(entry).expect("sample tree appends");
    }
    tree
}

#[test]
fn fold_user_commits_record_and_tree_together() {
    let (mut manager, tap) = manager();
    manager.fold(user("hello"));
    assert_eq!(manager.messages(), vec![user("hello")]);
    let records = tap.records();
    assert_eq!(records.len(), 1);
    assert!(matches!(
        node(&records[0]).kind,
        EntryKind::UserMessage { .. }
    ));
}

#[test]
fn fold_records_the_deferred_usage_fact() {
    let (mut manager, tap) = manager();
    manager.fold(assistant_text("done"));
    let records = tap.records();
    let EntryKind::AssistantMessage { usage, .. } = &node(&records[0]).kind else {
        panic!("expected an assistant node");
    };
    // The deferral made visible: zeros until the usage discussion lands.
    assert_eq!(*usage, Usage::new());
}

#[test]
#[should_panic(expected = "commits only through fold_all")]
fn fold_refuses_a_tool_calling_assistant() {
    let (mut manager, _tap) = manager();
    manager.fold(Message::Assistant {
        id: None,
        content: OneOrMany::one(call("c1")),
    });
}

#[test]
#[should_panic(expected = "only user and assistant messages fold")]
fn fold_refuses_other_messages() {
    let (mut manager, _tap) = manager();
    manager.fold(Message::System {
        content: "no".to_string(),
    });
}

#[test]
fn fold_all_commits_the_roundtrip_as_one_blob() {
    let (mut manager, tap) = manager();
    manager.fold(user("go"));
    let assistant = Message::Assistant {
        id: None,
        content: OneOrMany::many(vec![call("c1"), call("c2")]).expect("non-empty"),
    };
    let batch = vec![
        assistant,
        results_message(vec![result("c1", "one"), result("c2", "two")]),
    ];
    manager.fold_all(batch);
    // The view: the assistant, then the batch merged into one user
    // message of results — the same shape the engine folds at
    // settlement (fold_branch's batch merge).
    let messages = manager.messages();
    assert_eq!(messages.len(), 3);
    assert!(matches!(messages[0], Message::User { .. }));
    assert!(matches!(messages[1], Message::Assistant { .. }));
    assert!(matches!(messages[2], Message::User { .. }));
    // The blob: assistant node plus one node per result, one commit.
    let records = tap.records();
    assert_eq!(records.len(), 4); // u1's record + 3 of the roundtrip
    assert!(matches!(
        node(&records[1]).kind,
        EntryKind::AssistantMessage { .. }
    ));
    assert!(matches!(
        node(&records[2]).kind,
        EntryKind::ToolResult { .. }
    ));
    assert!(matches!(
        node(&records[3]).kind,
        EntryKind::ToolResult { .. }
    ));
}

#[test]
#[should_panic(expected = "must lead with an assistant turn")]
fn fold_all_refuses_a_non_assistant_head() {
    let (mut manager, _tap) = manager();
    manager.fold_all(vec![user("go")]);
}

#[test]
#[should_panic(expected = "carries no tool calls")]
fn fold_all_refuses_a_call_free_head() {
    let (mut manager, _tap) = manager();
    manager.fold_all(vec![assistant_text("plain")]);
}

#[test]
#[should_panic(expected = "1 call(s) unanswered")]
fn fold_all_refuses_an_incomplete_batch() {
    let (mut manager, _tap) = manager();
    manager.fold_all(vec![
        Message::Assistant {
            id: None,
            content: OneOrMany::many(vec![call("c1"), call("c2")]).expect("non-empty"),
        },
        results_message(vec![result("c1", "one")]),
    ]);
}

#[test]
#[should_panic(expected = "answers no open call")]
fn fold_all_refuses_an_orphan_result() {
    let (mut manager, _tap) = manager();
    manager.fold_all(vec![
        Message::Assistant {
            id: None,
            content: OneOrMany::one(call("c1")),
        },
        results_message(vec![result("c9", "stray")]),
    ]);
}

#[test]
#[should_panic(expected = "answers no open call")]
fn fold_all_refuses_a_duplicate_answer() {
    let (mut manager, _tap) = manager();
    manager.fold_all(vec![
        Message::Assistant {
            id: None,
            content: OneOrMany::one(call("c1")),
        },
        results_message(vec![result("c1", "one"), result("c1", "again")]),
    ]);
}

#[test]
#[should_panic(expected = "carry only tool results")]
fn fold_all_refuses_plain_text_in_the_results() {
    let (mut manager, _tap) = manager();
    manager.fold_all(vec![
        Message::Assistant {
            id: None,
            content: OneOrMany::one(call("c1")),
        },
        Message::User {
            content: OneOrMany::one(UserContent::text("not a result")),
        },
    ]);
}

#[test]
#[should_panic(expected = "only result messages")]
fn fold_all_refuses_an_assistant_in_the_tail() {
    let (mut manager, _tap) = manager();
    manager.fold_all(vec![
        Message::Assistant {
            id: None,
            content: OneOrMany::one(call("c1")),
        },
        results_message(vec![result("c1", "one")]),
        assistant_text("extra"),
    ]);
}

#[test]
fn from_tree_reproduces_the_incremental_history() {
    // The same conversation built two ways — folded live, and born from
    // the resulting tree — derives the same view: reload and live are
    // one derivation.
    let (mut live, _tap) = manager();
    live.fold(user("go"));
    live.fold_all(vec![
        Message::Assistant {
            id: None,
            content: OneOrMany::many(vec![call("c1"), call("c2")]).expect("non-empty"),
        },
        results_message(vec![result("c1", "one"), result("c2", "two")]),
    ]);
    live.fold(user("again"));

    let tap = BufferTap::default();
    let reloaded = ContextManager::from_tree(live.tree.clone(), tap.shared());
    assert_eq!(reloaded.messages(), live.messages());
}

#[test]
fn checkout_moves_the_view_to_the_target() {
    let tap = BufferTap::default();
    let mut manager = ContextManager::from_tree(sample_tree(), tap.shared());
    assert_eq!(manager.messages().len(), 4); // u1, a1, merged r1+r2, u2
    manager.checkout(Some("u1")).expect("u1 is a closed target");
    assert_eq!(manager.messages(), vec![user("go")]);
}

#[test]
fn checkout_to_root_empties_the_view() {
    let tap = BufferTap::default();
    let mut manager = ContextManager::from_tree(sample_tree(), tap.shared());
    manager.checkout(None).expect("root is always valid");
    assert!(manager.messages().is_empty());
}

#[test]
fn checkout_unknown_target_is_a_graceful_error() {
    let tap = BufferTap::default();
    let mut manager = ContextManager::from_tree(sample_tree(), tap.shared());
    let Err(error) = manager.checkout(Some("nope")) else {
        panic!("an unknown target must error");
    };
    assert_eq!(
        error.to_string(),
        "checkout target `nope` is not in this session"
    );
}

#[test]
#[should_panic(expected = "refused")]
fn checkout_into_an_open_roundtrip_panics() {
    let tap = BufferTap::default();
    let mut manager = ContextManager::from_tree(sample_tree(), tap.shared());
    // a1 carries two calls; the branch ending at it is mid-roundtrip.
    manager.checkout(Some("a1")).expect("unchecked");
}

#[test]
#[should_panic(expected = "refused")]
fn checkout_into_a_mid_batch_result_panics() {
    let tap = BufferTap::default();
    let mut manager = ContextManager::from_tree(sample_tree(), tap.shared());
    // r1 answers c1 but leaves c2 open — mid-batch.
    manager.checkout(Some("r1")).expect("unchecked");
}

#[test]
fn the_buffer_serves_the_manager_and_the_session() {
    // Non-context records ride the same buffer from the session side:
    // one lock, one outbox, both producers.
    let tap = BufferTap::default();
    let shared = tap.shared();
    let mut manager = ContextManager::empty(tap.shared());
    manager.fold(user("hello"));
    {
        let mut session_side = lock::lock(&shared);
        session_side
            .enqueue(&[FileRecord::Side(SideRecord {
                timestamp: "t".to_string(),
                kind: SideKind::Label {
                    name: "bookmark".to_string(),
                },
            })])
            .expect("the tap accepts side batches");
    }
    let records = tap.records();
    assert_eq!(records.len(), 2);
    assert!(matches!(records[0], FileRecord::Node(_)));
    assert!(matches!(records[1], FileRecord::Side(_)));
}
