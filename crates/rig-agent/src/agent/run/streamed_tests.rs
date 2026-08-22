//! Contract tests for the streamed-turn assembler: accumulation, delta
//! buffering, canonical replay order — and that unknown tool names flow
//! through (admission is the machine's, not the assembler's).

use super::*;
use crate::completion::Usage;
use rig_core::message::{AssistantContent, Reasoning};
use rig_core::streaming::{StreamedAssistantContent, ToolCallDeltaContent};

fn assembler() -> StreamedTurnAssembler {
    StreamedTurnAssembler::new(
        ["add", "sub"].iter().map(|name| name.to_string()).collect(),
        ["add", "sub"].iter().map(|name| name.to_string()).collect(),
    )
}

fn delta_events(
    assembler: &mut StreamedTurnAssembler,
    items: &[StreamedAssistantContent],
) -> Vec<StreamedTurnEvent> {
    let mut events = Vec::new();
    for item in items {
        events.extend(assembler.ingest(item).expect("ingest"));
    }
    events
}

/// Text accumulates and is emitted as it arrives.
#[test]
fn text_deltas_accumulate_and_emit() {
    let mut assembler = assembler();
    let events = delta_events(
        &mut assembler,
        &[
            StreamedAssistantContent::text("He"),
            StreamedAssistantContent::text("llo"),
        ],
    );
    assert_eq!(events.len(), 2);
    assert_eq!(assembler.aggregated_text(), "Hello");
}

/// A tool name the model was not offered flows through the assembler —
/// admission is the machine's concern.
#[test]
fn unknown_tool_names_flow_through() {
    let mut assembler = assembler();
    let call = StreamedAssistantContent::ToolCall {
        tool_call: rig_core::message::ToolCall::new(
            "call_1".to_string(),
            rig_core::message::ToolFunction::new(
                "nonexistent".to_string(),
                serde_json::json!({"x": 1}),
            ),
        ),
        internal_call_id: "internal_1".to_string(),
    };
    let events = assembler.ingest(&call).expect("no invalid surfacing");
    assert!(events.is_empty(), "no events, no pause, no failure");
}

/// Argument deltas arriving before the name buffer until the name lands,
/// then replay in order.
#[test]
fn argument_deltas_buffer_until_the_name_arrives() {
    let mut assembler = assembler();
    let events = delta_events(
        &mut assembler,
        &[
            StreamedAssistantContent::ToolCallDelta {
                id: "call_1".to_string(),
                internal_call_id: "internal_1".to_string(),
                content: ToolCallDeltaContent::Delta("{\"x\":".to_string()),
            },
            StreamedAssistantContent::ToolCallDelta {
                id: "call_1".to_string(),
                internal_call_id: "internal_1".to_string(),
                content: ToolCallDeltaContent::Name("add".to_string()),
            },
            StreamedAssistantContent::ToolCallDelta {
                id: "call_1".to_string(),
                internal_call_id: "internal_1".to_string(),
                content: ToolCallDeltaContent::Delta("1}".to_string()),
            },
        ],
    );
    // Name first, then the buffered argument delta replays; the final
    // argument delta (after validation) emits directly.
    assert_eq!(events.len(), 3);
    assert!(matches!(
        &events[0],
        StreamedTurnEvent::EmitToolCallDelta { content, .. }
            if matches!(content, ToolCallDeltaContent::Name(name) if name == "add")
    ));
    assert!(matches!(
        &events[1],
        StreamedTurnEvent::EmitToolCallDelta { content, .. }
            if matches!(content, ToolCallDeltaContent::Delta(text) if text == "{\"x\":")
    ));
    assert!(matches!(
        &events[2],
        StreamedTurnEvent::EmitToolCallDelta { content, .. }
            if matches!(content, ToolCallDeltaContent::Delta(text) if text == "1}")
    ));
}

/// The completed turn assembles in canonical replay order (reasoning →
/// text → tool calls) with correlation ids carried.
#[test]
fn finish_assembles_canonical_order_with_correlation() {
    let mut assembler = assembler();
    delta_events(
        &mut assembler,
        &[
            StreamedAssistantContent::text("let me check"),
            StreamedAssistantContent::ToolCall {
                tool_call: rig_core::message::ToolCall::new(
                    "call_1".to_string(),
                    rig_core::message::ToolFunction::new(
                        "add".to_string(),
                        serde_json::json!({"x": 1, "y": 2}),
                    ),
                ),
                internal_call_id: "internal_1".to_string(),
            },
        ],
    );
    let final_choice = OneOrMany::one(AssistantContent::text("let me check"));
    let turn = assembler.finish(Some("msg_1".to_string()), &final_choice);
    assert_eq!(turn.message_id.as_deref(), Some("msg_1"));
    assert_eq!(turn.internal_call_ids.len(), 1);
    let kinds: Vec<&str> = turn
        .choice
        .iter()
        .map(|content| match content {
            AssistantContent::Text(_) => "text",
            AssistantContent::ToolCall(_) => "tool",
            AssistantContent::Reasoning(_) => "reasoning",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["text", "tool"], "text precedes tool calls");
}

/// Reasoning blocks with matching provider ids merge; distinct ids stay
/// separate.
#[test]
fn reasoning_blocks_merge_by_id() {
    let mut reasoning = Reasoning::new("first");
    reasoning.id = Some("rs_1".to_string());
    let mut merged = Vec::new();
    merge_reasoning_blocks(&mut merged, &reasoning);
    let mut same_id = Reasoning::new(" more");
    same_id.id = Some("rs_1".to_string());
    merge_reasoning_blocks(&mut merged, &same_id);
    assert_eq!(merged.len(), 1, "matching ids extend one block");
    assert_eq!(merged[0].content.len(), 2);

    let mut other = Reasoning::new("other");
    other.id = Some("rs_2".to_string());
    merge_reasoning_blocks(&mut merged, &other);
    assert_eq!(merged.len(), 2, "distinct ids stay separate");
}

/// The final event carries usage and the emit-final signal.
#[test]
fn final_event_carries_usage() {
    let mut assembler = assembler();
    let events = delta_events(&mut assembler, &[StreamedAssistantContent::text("answer")]);
    let _ = events;
    let mut usage = Usage::new();
    usage.total_tokens = 9;
    let events = assembler
        .ingest(&StreamedAssistantContent::Final(
            rig_core::streaming::StreamFinal::new("mock", usage),
        ))
        .expect("ingest");
    assert!(matches!(
        events.as_slice(),
        [StreamedTurnEvent::Completed {
            emit_final: true,
            ..
        }]
    ));
}

/// The terminal's finish reason rides the Completed event verbatim — the
/// turn-level fact a consumer warns on (a truncation-class reason is
/// informational, not a failure).
#[test]
fn final_event_carries_the_finish_reason() {
    let mut assembler = assembler();
    let mut usage = Usage::new();
    usage.total_tokens = 9;
    let events = assembler
        .ingest(&StreamedAssistantContent::Final(
            rig_core::streaming::StreamFinal::new("mock", usage)
                .with_finish_reason(rig_core::completion::FinishReason::Length),
        ))
        .expect("ingest");
    assert!(matches!(
        events.as_slice(),
        [StreamedTurnEvent::Completed {
            finish_reason: Some(rig_core::completion::FinishReason::Length),
            ..
        }]
    ));
}
