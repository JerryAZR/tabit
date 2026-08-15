use super::*;
use crate::agent::hook::InvalidToolCallAction;
use crate::agent::run::{AgentRun, AgentRunStep};
use crate::completion::PromptError;
use crate::test_utils::mock_final;
use rig_core::message::{Text, ToolResultContent, UserContent};
use serde_json::json;

fn tool_names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

fn assembler() -> StreamedTurnAssembler {
    StreamedTurnAssembler::new(tool_names(&["add"]), tool_names(&["add"]))
}

fn text_item(text: &str) -> StreamedAssistantContent {
    StreamedAssistantContent::Text(Text::new(text.to_string()))
}

fn tool_call(id: &str, name: &str) -> ToolCall {
    ToolCall::new(
        id.to_string(),
        ToolFunction::new(name.to_string(), json!({"x": 1})),
    )
}

fn tool_call_item(id: &str, name: &str) -> StreamedAssistantContent {
    StreamedAssistantContent::ToolCall {
        tool_call: tool_call(id, name),
        internal_call_id: format!("internal_{id}"),
    }
}

fn final_item() -> StreamedAssistantContent {
    StreamedAssistantContent::Final(mock_final(Usage::new()))
}

fn name_delta(id: &str, name: &str) -> StreamedAssistantContent {
    StreamedAssistantContent::ToolCallDelta {
        id: id.to_string(),
        internal_call_id: format!("internal_{id}"),
        content: ToolCallDeltaContent::Name(name.to_string()),
    }
}

fn args_delta(id: &str, arguments: &str) -> StreamedAssistantContent {
    StreamedAssistantContent::ToolCallDelta {
        id: id.to_string(),
        internal_call_id: format!("internal_{id}"),
        content: ToolCallDeltaContent::Delta(arguments.to_string()),
    }
}

fn expect_invalid(events: Vec<StreamedTurnEvent>) -> StreamedInvalidToolCall {
    match events.into_iter().next() {
        Some(StreamedTurnEvent::InvalidToolCall(invalid)) => *invalid,
        other => panic!("expected InvalidToolCall, got {other:?}"),
    }
}

#[test]
fn text_accumulates_and_emits() {
    let mut asm = assembler();
    let events = asm
        .ingest(&text_item("hel"))
        .expect("ingest should succeed");
    assert!(matches!(
        events.as_slice(),
        [StreamedTurnEvent::EmitIngested]
    ));
    asm.ingest(&text_item("lo")).expect("ingest should succeed");
    assert_eq!(asm.aggregated_text(), "hello");
}

/// An empty text block carrying provider-specific fields is still
/// representable assistant content and must not be dropped by the
/// provider-aggregate filter.
#[test]
fn assistant_text_items_keep_metadata_only_text_blocks() {
    let choice = OneOrMany::many(vec![
        AssistantContent::Text(Text {
            text: String::new(),
            additional_params: Some(json!({"provider_field": "kept"})),
        }),
        AssistantContent::Text(Text {
            text: String::new(),
            additional_params: None,
        }),
    ])
    .expect("two text items");

    let items = assistant_text_items_from_choice(&choice);
    assert_eq!(
        items.len(),
        1,
        "empty text without fields is dropped, metadata-only text is kept"
    );
    assert!(matches!(&items[0], AssistantContent::Text(text) if text.additional_params.is_some()));
}

/// A partial turn with no reasoning, text, or tool calls has nothing
/// representable to roll back; `assistant_message` must say so with `None`.
#[test]
fn partial_turn_without_content_has_no_assistant_message() {
    let partial = PartialStreamedTurn {
        message_id: Some("msg_1".to_string()),
        text: None,
        reasoning: Vec::new(),
        pending_tool_calls: Vec::new(),
    };
    assert!(partial.assistant_message(None).is_none());
}

/// Once an invalid tool call is pending, further ingests must error
/// instead of silently accumulating behind the unresolved call.
#[test]
fn ingest_while_an_invalid_call_awaits_resolution_is_an_error() {
    let mut asm = assembler();
    expect_invalid(
        asm.ingest(&tool_call_item("tc_1", "default_api"))
            .expect("first ingest surfaces the invalid call"),
    );

    let error = asm
        .ingest(&text_item("late"))
        .expect_err("ingest during pending resolution must be rejected");
    assert!(
        error.to_string().contains("awaits resolution"),
        "unexpected error: {error}"
    );
}

/// Resolving when nothing is pending is a no-op, not an error.
#[test]
fn resolve_pending_invalid_without_a_pending_call_is_a_noop() {
    let mut asm = assembler();
    let resolution = StreamedResolution::Repaired {
        tool_name: "add".to_string(),
    };
    assert!(asm.resolve_pending_invalid(&resolution).is_empty());
}

#[test]
fn unknown_item_emits_to_consumer_without_touching_accumulation() {
    let mut asm = assembler();
    asm.ingest(&text_item("answer"))
        .expect("ingest text should succeed");

    let events = asm
        .ingest(&StreamedAssistantContent::Unknown(
            json!({ "type": "web_search_call", "id": "ws_1" }),
        ))
        .expect("ingest unknown should succeed");

    // The unmodeled item is forwarded to the consumer ...
    assert!(matches!(
        events.as_slice(),
        [StreamedTurnEvent::EmitIngested]
    ));
    // ... but perturbs no accumulation state used to build the assistant message.
    assert_eq!(asm.aggregated_text(), "answer");
}

#[test]
fn argument_deltas_buffer_until_name_validates() {
    let mut asm = assembler();

    let events = asm
        .ingest(&args_delta("tc_1", "{\"x\""))
        .expect("ingest should succeed");
    assert!(events.is_empty(), "arguments must buffer before the name");

    let events = asm
        .ingest(&name_delta("tc_1", "add"))
        .expect("ingest should succeed");
    let contents: Vec<_> = events
        .iter()
        .map(|event| match event {
            StreamedTurnEvent::EmitToolCallDelta { content, .. } => content.clone(),
            other => panic!("expected EmitToolCallDelta, got {other:?}"),
        })
        .collect();
    assert_eq!(
        contents,
        vec![
            ToolCallDeltaContent::Name("add".to_string()),
            ToolCallDeltaContent::Delta("{\"x\"".to_string()),
        ]
    );

    // Subsequent argument deltas now pass straight through.
    let events = asm
        .ingest(&args_delta("tc_1", ":1}"))
        .expect("ingest should succeed");
    assert_eq!(events.len(), 1);
}

#[test]
fn buffered_arguments_without_validated_name_error_at_final() {
    let mut asm = assembler();
    asm.ingest(&args_delta("tc_1", "{\"x\":1}"))
        .expect("ingest should succeed");

    assert!(asm.pending_delta_error().is_some());
    assert!(asm.ingest(&final_item()).is_err());
}

#[test]
fn finish_orders_reasoning_text_then_tool_calls() {
    let mut asm = assembler();
    asm.ingest(&StreamedAssistantContent::ReasoningDelta {
        id: "rs_1".to_string(),
        reasoning: "think".to_string(),
    })
    .expect("ingest should succeed");
    asm.ingest(&tool_call_item("tc_1", "add"))
        .expect("ingest should succeed");

    // Provider aggregation order differs deliberately.
    let final_choice = OneOrMany::many(vec![
        AssistantContent::text("answer"),
        AssistantContent::ToolCall(tool_call("tc_1", "add")),
    ])
    .expect("two items");

    let turn = asm.finish(Some("msg_1".to_string()), &final_choice);
    let kinds: Vec<&'static str> = turn
        .choice
        .iter()
        .map(|item| match item {
            AssistantContent::Reasoning(_) => "reasoning",
            AssistantContent::Text(_) => "text",
            AssistantContent::ToolCall(_) => "tool_call",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, vec!["reasoning", "text", "tool_call"]);
}

#[test]
fn finish_passes_raw_choice_through_for_plain_text_turns() {
    let mut asm = assembler();
    asm.ingest(&text_item("hi")).expect("ingest should succeed");

    let final_choice = OneOrMany::one(AssistantContent::text("hi"));
    let turn = asm.finish(None, &final_choice);
    assert_eq!(
        serde_json::to_value(&turn.choice).expect("serialize"),
        serde_json::to_value(&final_choice).expect("serialize"),
    );
}

#[test]
fn streamed_run_completes_a_tool_roundtrip() {
    let mut run = AgentRun::new("add things").max_turns(2);

    // Turn 1: the model streams one tool call.
    let AgentRunStep::CallModel { .. } = run.next_step().expect("next_step") else {
        panic!("expected CallModel");
    };
    let mut asm = assembler();
    assert!(
        asm.ingest(&tool_call_item("tc_1", "add"))
            .expect("ingest should succeed")
            .is_empty()
    );
    let usage = Usage {
        input_tokens: 5,
        output_tokens: 7,
        total_tokens: 12,
        ..Usage::new()
    };
    run.record_streamed_completion_call(usage)
        .expect("record should succeed");
    let final_choice = OneOrMany::one(AssistantContent::ToolCall(tool_call("tc_1", "add")));
    run.streamed_turn(asm.finish(Some("msg_1".to_string()), &final_choice))
        .expect("streamed_turn should succeed");

    let AgentRunStep::CallTools { calls } = run.next_step().expect("next_step") else {
        panic!("expected CallTools");
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].internal_call_id.as_deref(), Some("internal_tc_1"));
    run.tool_results(vec![UserContent::tool_result(
        "tc_1".to_string(),
        OneOrMany::one(ToolResultContent::text("2")),
    )])
    .expect("tool_results should succeed");

    // Turn 2: plain text finishes the run.
    let AgentRunStep::CallModel { .. } = run.next_step().expect("next_step") else {
        panic!("expected CallModel");
    };
    let asm = assembler();
    run.record_streamed_completion_call(Usage::new())
        .expect("record should succeed");
    let final_choice = OneOrMany::one(AssistantContent::text("done"));
    run.streamed_turn(asm.finish(None, &final_choice))
        .expect("streamed_turn should succeed");

    let AgentRunStep::Done(response) = run.next_step().expect("next_step") else {
        panic!("expected Done");
    };
    assert_eq!(response.output, "done");
    assert_eq!(response.usage, usage);
    assert_eq!(response.completion_calls.len(), 2);
    assert_eq!(response.completion_calls[0].usage, usage);
    assert_eq!(response.completion_calls[1].usage, Usage::new());
    // prompt, assistant tool call, tool result, final assistant text
    assert_eq!(
        response
            .messages
            .expect("messages should be recorded")
            .len(),
        4
    );
}

#[test]
fn streamed_invalid_tool_call_retry_rolls_back_with_partial_turn() {
    let mut run = AgentRun::new("use the tool")
        .max_turns(2)
        .max_invalid_tool_call_retries(1);
    run.next_step().expect("next_step");

    let mut asm = assembler();
    asm.ingest(&text_item("thinking ")).expect("ingest");
    let invalid = expect_invalid(
        asm.ingest(&tool_call_item("tc_1", "default_api"))
            .expect("ingest should succeed"),
    );
    let partial = asm.partial_turn(Some("msg_1".to_string()));
    assert_eq!(partial.text.as_deref(), Some("thinking "));

    let context = run.streamed_invalid_tool_call_context(&partial, &invalid);
    assert!(context.is_streaming);
    assert_eq!(context.tool_name, "default_api");
    assert_eq!(context.internal_call_id.as_deref(), Some("internal_tc_1"));

    let resolution = run
        .resolve_streamed_invalid_tool_call(
            &partial,
            &invalid,
            InvalidToolCallAction::retry("use add instead"),
        )
        .expect("retry should be accepted");
    assert!(matches!(
        resolution,
        StreamedResolution::TurnAbandoned {
            skipped_tool_result: None
        }
    ));
    asm.resolve_pending_invalid(&resolution);

    // Usage from the drained stream is recorded after the rollback.
    run.record_streamed_completion_call(Usage::new())
        .expect("record after rollback should succeed");

    // The rollback appended the partial assistant turn and feedback.
    assert_eq!(run.messages().len(), 3);
    let AgentRunStep::CallModel { turn, .. } = run.next_step().expect("next_step") else {
        panic!("expected CallModel retry");
    };
    assert_eq!(turn, 2);
}

#[test]
fn streamed_invalid_tool_call_stop_leaves_run_terminal() {
    let mut run = AgentRun::new("use the tool");
    run.next_step().expect("next_step");

    let mut asm = assembler();
    let invalid = expect_invalid(
        asm.ingest(&tool_call_item("tc_1", "default_api"))
            .expect("ingest should succeed"),
    );
    let partial = asm.partial_turn(Some("msg_1".to_string()));

    let err = run
        .resolve_streamed_invalid_tool_call(
            &partial,
            &invalid,
            InvalidToolCallAction::stop("operator stop"),
        )
        .expect_err("stop should cancel the run");
    assert!(matches!(
        err,
        PromptError::PromptCancelled { reason, .. } if reason == "operator stop"
    ));

    let err = run
        .next_step()
        .expect_err("a stopped streamed run must remain terminal");
    assert!(matches!(
        err,
        PromptError::PromptCancelled { reason, .. }
            if reason.contains("next_step called after the run already failed")
    ));
}

#[test]
fn streamed_invalid_tool_call_retry_cannot_emit_call_past_total_budget() {
    let mut run = AgentRun::new("use the tool")
        .max_turns(1)
        .max_invalid_tool_call_retries(1);
    run.next_step().expect("initial model call");

    let mut asm = assembler();
    let invalid = expect_invalid(
        asm.ingest(&tool_call_item("tc_1", "default_api"))
            .expect("ingest should succeed"),
    );
    let partial = asm.partial_turn(Some("msg_1".to_string()));
    let resolution = run
        .resolve_streamed_invalid_tool_call(
            &partial,
            &invalid,
            InvalidToolCallAction::retry("use add instead"),
        )
        .expect("retry resolution should be accepted");
    assert!(matches!(
        resolution,
        StreamedResolution::TurnAbandoned {
            skipped_tool_result: None
        }
    ));
    run.record_streamed_completion_call(Usage::new())
        .expect("completion call should be recorded");
    assert_eq!(run.completion_calls().len(), 1);

    let err = run
        .next_step()
        .expect_err("retry must not emit a second model call");
    assert!(matches!(
        err,
        PromptError::MaxTurnsError { max_turns: 1, .. }
    ));
    assert_eq!(run.turn(), 1);
}

#[test]
fn streamed_invalid_tool_call_skip_returns_synthetic_result() {
    let mut run = AgentRun::new("use the tool").max_turns(2);
    run.next_step().expect("next_step");

    let mut asm = assembler();
    let invalid = expect_invalid(
        asm.ingest(&tool_call_item("tc_1", "default_api"))
            .expect("ingest should succeed"),
    );
    let partial = asm.partial_turn(None);

    let resolution = run
        .resolve_streamed_invalid_tool_call(
            &partial,
            &invalid,
            InvalidToolCallAction::skip("not available"),
        )
        .expect("skip should be accepted");
    let StreamedResolution::TurnAbandoned {
        skipped_tool_result: Some(tool_result),
    } = &resolution
    else {
        panic!("expected skipped tool result");
    };
    assert_eq!(tool_result.id, "tc_1");
}

#[test]
fn streamed_invalid_name_delta_repair_replays_buffered_arguments() {
    let mut run = AgentRun::new("use the tool").max_turns(2);
    run.next_step().expect("next_step");

    let mut asm = assembler();
    asm.ingest(&args_delta("tc_1", "{\"x\":1}"))
        .expect("ingest should succeed");
    let invalid = expect_invalid(
        asm.ingest(&name_delta("tc_1", "default_api"))
            .expect("ingest should succeed"),
    );
    assert_eq!(invalid.args.as_deref(), Some("{\"x\":1}"));

    let partial = asm.partial_turn(None);
    let resolution = run
        .resolve_streamed_invalid_tool_call(
            &partial,
            &invalid,
            InvalidToolCallAction::repair("add"),
        )
        .expect("repair should be accepted");
    assert!(matches!(
        resolution,
        StreamedResolution::Repaired { ref tool_name } if tool_name == "add"
    ));

    let events = asm.resolve_pending_invalid(&resolution);
    let contents: Vec<_> = events
        .iter()
        .map(|event| match event {
            StreamedTurnEvent::EmitToolCallDelta { content, .. } => content.clone(),
            other => panic!("expected EmitToolCallDelta, got {other:?}"),
        })
        .collect();
    assert_eq!(
        contents,
        vec![
            ToolCallDeltaContent::Name("add".to_string()),
            ToolCallDeltaContent::Delta("{\"x\":1}".to_string()),
        ]
    );
}

#[test]
fn streamed_turn_rejects_unknown_tool_calls_fail_fast() {
    let mut run = AgentRun::new("use the tool");
    run.next_step().expect("next_step");

    let turn = StreamedTurn {
        message_id: None,
        choice: OneOrMany::one(AssistantContent::ToolCall(tool_call("tc_1", "unknown"))),
        executable_tool_names: tool_names(&["add"]),
        allowed_tool_names: tool_names(&["add"]),
        internal_call_ids: Vec::new(),
    };
    let err = run
        .streamed_turn(turn)
        .expect_err("unknown tool should fail fast");
    assert!(matches!(
        err,
        PromptError::UnknownToolCall { tool_name, .. } if tool_name == "unknown"
    ));
}

#[test]
fn streamed_completion_call_record_requires_a_model_call() {
    // A fresh run has emitted no CallModel: recording must be rejected
    // even though the machine is in its initial PreparingRequest state.
    let mut run = AgentRun::new("hello");
    let err = run
        .record_streamed_completion_call(Usage::new())
        .expect_err("recording before any model call must be rejected");
    assert!(matches!(err, PromptError::PromptCancelled { .. }));

    // The run stays drivable.
    run.next_step().expect("next_step should still succeed");
    run.record_streamed_completion_call(Usage::new())
        .expect("recording during a pending model call succeeds");
}

#[test]
fn duplicate_tool_call_ids_keep_distinct_internal_ids_through_the_run() {
    let mut run = AgentRun::new("do both").max_turns(2);
    run.next_step().expect("next_step");

    let mut asm = assembler();
    asm.ingest(&StreamedAssistantContent::ToolCall {
        tool_call: tool_call("tc_1", "add"),
        internal_call_id: "internal_a".to_string(),
    })
    .expect("ingest should succeed");
    asm.ingest(&StreamedAssistantContent::ToolCall {
        tool_call: tool_call("tc_1", "add"),
        internal_call_id: "internal_b".to_string(),
    })
    .expect("ingest should succeed");
    run.record_streamed_completion_call(Usage::new())
        .expect("record should succeed");

    let final_choice = OneOrMany::many(vec![
        AssistantContent::ToolCall(tool_call("tc_1", "add")),
        AssistantContent::ToolCall(tool_call("tc_1", "add")),
    ])
    .expect("two items");
    run.streamed_turn(asm.finish(None, &final_choice))
        .expect("streamed_turn should succeed");

    // The internal IDs survive in the run state itself: a serde round
    // trip must keep both calls distinguishable.
    let serialized = serde_json::to_string(&run).expect("serialize");
    let mut restored: AgentRun = serde_json::from_str(&serialized).expect("deserialize");
    let AgentRunStep::CallTools { calls } = restored.next_step().expect("next_step") else {
        panic!("expected CallTools");
    };
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].internal_call_id.as_deref(), Some("internal_a"));
    assert_eq!(calls[1].internal_call_id.as_deref(), Some("internal_b"));
}

#[test]
fn streamed_turn_records_the_completion_call_when_the_driver_did_not() {
    let mut run = AgentRun::new("hello");
    run.next_step().expect("next_step");

    let asm = assembler();
    let final_choice = OneOrMany::one(AssistantContent::text("done"));
    run.streamed_turn(asm.finish(None, &final_choice))
        .expect("streamed_turn should succeed");

    // Exactly one CompletionCall per model call, even without an explicit
    // record; usage is simply unreported.
    assert_eq!(run.completion_calls().len(), 1);
    assert_eq!(run.completion_calls()[0].usage, Usage::new());
}

#[test]
fn streamed_completion_call_is_recorded_once_per_turn() {
    let mut run = AgentRun::new("hello");
    run.next_step().expect("next_step");

    run.record_streamed_completion_call(Usage::new())
        .expect("first record succeeds");
    let err = run
        .record_streamed_completion_call(Usage::new())
        .expect_err("second record for the same turn must be rejected");
    assert!(matches!(err, PromptError::PromptCancelled { .. }));
    assert_eq!(run.completion_calls().len(), 1);
}

#[test]
fn streamed_run_serde_round_trips_while_tools_pend() {
    let mut run = AgentRun::new("add things").max_turns(2);
    run.next_step().expect("next_step");

    let mut asm = assembler();
    asm.ingest(&tool_call_item("tc_1", "add"))
        .expect("ingest should succeed");
    run.record_streamed_completion_call(Usage::new())
        .expect("record should succeed");
    let final_choice = OneOrMany::one(AssistantContent::ToolCall(tool_call("tc_1", "add")));
    run.streamed_turn(asm.finish(None, &final_choice))
        .expect("streamed_turn should succeed");
    run.next_step().expect("CallTools step");

    let serialized = serde_json::to_string(&run).expect("serialize mid-run");
    let mut restored: AgentRun = serde_json::from_str(&serialized).expect("deserialize mid-run");
    restored
        .tool_results(vec![UserContent::tool_result(
            "tc_1".to_string(),
            OneOrMany::one(ToolResultContent::text("2")),
        )])
        .expect("tool_results should succeed");
    assert!(matches!(
        restored.next_step().expect("next turn"),
        AgentRunStep::CallModel { turn: 2, .. }
    ));
}
