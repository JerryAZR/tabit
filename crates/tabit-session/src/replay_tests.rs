//! Projection tests: synthesized chains → the finalized live events.
//! Ids are the entries' own (the live/replay id continuity is pinned
//! end-to-end in tests.rs); these pin the projection's shape.

use super::*;
use crate::entry::{EntryKind, SessionEntry};
use rig_core::OneOrMany;
use rig_core::completion::{Message, Usage};
use rig_core::message::{AssistantContent, ToolCall, ToolFunction, ToolResultContent};
use tabit_protocol::SessionEvent;

fn entry(id: &str, kind: EntryKind) -> SessionEntry {
    SessionEntry::with_id(
        id.to_string(),
        None,
        "2026-08-22T00:00:00Z".to_string(),
        kind,
    )
}

fn assistant(content: Vec<AssistantContent>, usage: Usage) -> EntryKind {
    EntryKind::AssistantMessage {
        message: Message::Assistant {
            id: None,
            content: OneOrMany::many(content).expect("non-empty assistant content"),
        },
        usage,
    }
}

fn tool_call(id: &str, call_id: Option<&str>, name: &str) -> AssistantContent {
    let mut call = ToolCall::new(
        id.to_string(),
        ToolFunction {
            name: name.to_string(),
            arguments: serde_json::json!({"path": "x"}),
        },
    );
    if let Some(call_id) = call_id {
        call = call.with_call_id(call_id.to_string());
    }
    AssistantContent::ToolCall(call)
}

#[test]
fn a_chain_projects_to_bracketed_whole_text_events() {
    let chain = vec![
        entry(
            "u1",
            EntryKind::UserMessage {
                message: Message::user("list the files"),
            },
        ),
        entry(
            "t1",
            assistant(
                vec![
                    AssistantContent::reasoning("thinking about it"),
                    AssistantContent::text("let me look"),
                    tool_call("call-1", Some("wire-1"), "ls"),
                ],
                Usage {
                    input_tokens: 10,
                    output_tokens: 4,
                    ..Usage::new()
                },
            ),
        ),
        entry(
            "r1",
            EntryKind::ToolResult {
                result: rig_core::message::ToolResult {
                    id: "call-1".to_string(),
                    call_id: Some("wire-1".to_string()),
                    content: OneOrMany::one(ToolResultContent::text("3 files")),
                    status: Some(rig_core::completion::ToolResultStatus::Success),
                },
            },
        ),
        entry(
            "t2",
            assistant(
                vec![AssistantContent::text("all done")],
                Usage {
                    input_tokens: 20,
                    output_tokens: 2,
                    ..Usage::new()
                },
            ),
        ),
    ];

    let events = project_events(&chain);

    // One sequence, exact: the user message with its entry id, then each
    // turn bracketed by its entry id around whole-text deltas, its call,
    // its usage — the tool result stamped with its turn — and the final
    // turn. The chain's leading `model_change` produces nothing: state
    // is announced live at visibility, never reconstructed from history
    // (the register ruling) — its absence here is the pin.
    let labels: Vec<String> = events
        .iter()
        .map(|event| match event {
            SessionEvent::UserMessage { entry_id, .. } => format!("user:{entry_id}"),
            SessionEvent::TurnStarted { id } => format!("start:{id}"),
            SessionEvent::ReasoningDelta { id, .. } => format!("think:{id}"),
            SessionEvent::TextDelta { text, .. } => format!("text:{text}"),
            SessionEvent::ToolCall { name, .. } => format!("call:{name}"),
            SessionEvent::CompletionCall { input_tokens, .. } => format!("usage:{input_tokens}"),
            SessionEvent::TurnCommitted { id } => format!("commit:{id}"),
            SessionEvent::ToolResult { name, .. } => format!("result:{name}"),
            other => format!("other:{other:?}"),
        })
        .collect();
    assert_eq!(
        labels,
        vec![
            "user:u1",
            "start:t1",
            // Reasoning block id synthesized from the turn (the block
            // carried none; live deltas arrived without one too).
            "think:t1-reasoning-2",
            "text:let me look",
            "call:ls",
            "usage:10",
            "commit:t1",
            "result:ls",
            "start:t2",
            "text:all done",
            "usage:20",
            "commit:t2",
        ]
    );

    // The shapes behind the labels: whole texts, stamps, structure.
    assert!(matches!(
        &events[2],
        SessionEvent::ReasoningDelta { turn_id, reasoning, .. }
            if turn_id == "t1" && reasoning == "thinking about it"
    ));
    assert!(matches!(
        &events[7],
        SessionEvent::ToolResult {
            turn_id, entry_id, name, internal_call_id, content, ..
        } if turn_id == "t1"
            && entry_id == "r1"
            && name == "ls"
            && internal_call_id == "wire-1"
            && content == "3 files"
    ));
}

#[test]
fn multiple_text_items_and_reasoning_blocks_project_to_one_text_and_per_block_deltas() {
    let chain = vec![entry(
        "t1",
        assistant(
            vec![
                AssistantContent::reasoning("first block"),
                AssistantContent::text("part one "),
                AssistantContent::text("part two"),
                AssistantContent::Reasoning(
                    rig_core::message::Reasoning::new_with_signature("second block", None)
                        .with_id("r2".to_string()),
                ),
            ],
            Usage::new(),
        ),
    )];
    let events = project_events(&chain);
    // Canonical content order is reasoning → text → tool calls; the
    // projection walks that order, flushing accumulated text whenever a
    // reasoning block interrupts it: reasoning, text, reasoning.
    let reasoning: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ReasoningDelta { id, reasoning, .. } => {
                Some(if id == "r2" { &reasoning[..] } else { "" })
            }
            _ => None,
        })
        .collect();
    // (first block present with a synthesized id, second with `r2`)
    assert!(events.iter().any(|e| matches!(e,
        SessionEvent::ReasoningDelta { id, reasoning, .. }
            if id == "r2" && reasoning == "second block")));
    assert!(events.iter().any(|e| matches!(e,
        SessionEvent::ReasoningDelta { reasoning, .. } if reasoning == "first block")));
    let _ = reasoning;
    // One text delta carrying both items, whole.
    assert!(events.iter().any(|e| matches!(e,
        SessionEvent::TextDelta { text, .. } if text == "part one part two")));
}

#[test]
fn an_empty_reasoning_block_projects_no_delta() {
    // A recorded reasoning block with no displayable text emits
    // nothing — no empty reasoning row.
    let chain = vec![entry(
        "t1",
        assistant(
            vec![
                AssistantContent::reasoning(""),
                AssistantContent::text("body"),
            ],
            Usage::new(),
        ),
    )];
    let events = project_events(&chain);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ReasoningDelta { .. })),
        "an empty block projects no delta: {events:?}"
    );
    assert!(events.iter().any(|e| matches!(e,
        SessionEvent::TextDelta { text, .. } if text == "body")));
}

#[test]
fn an_empty_branch_projects_to_nothing() {
    // v3: bookkeeping lives in side records that never enter a branch,
    // so there is nothing to skip — an empty branch is simply empty.
    assert!(project_events(&[]).is_empty());
}

#[test]
fn failed_results_keep_their_structured_status() {
    let chain = vec![
        entry(
            "t1",
            assistant(vec![tool_call("call-1", None, "bash")], Usage::new()),
        ),
        entry(
            "r1",
            EntryKind::ToolResult {
                result: rig_core::message::ToolResult {
                    id: "call-1".to_string(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::text(
                        "command exited with status 3:\nboom",
                    )),
                    status: Some(rig_core::completion::ToolResultStatus::Failed {
                        code: Some("3".to_string()),
                    }),
                },
            },
        ),
    ];
    let events = project_events(&chain);
    assert!(events.iter().any(|e| matches!(e,
        SessionEvent::ToolResult { status, content, .. }
            if *status == tabit_protocol::ToolResultStatus::Failed { exit_code: Some(3) }
                && content.contains("status 3")
    )));
}
