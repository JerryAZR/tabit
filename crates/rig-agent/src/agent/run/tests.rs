//! Contract tests for the ENGINE.md state machine: entry (one joined
//! history, as-is), the at-least-one-turn invariant, turn classification,
//! in-band admission, the single drain point, and the decision.

use super::*;
use crate::completion::PromptError;
use rig_core::message::AssistantContent;

fn tools(names: &[&str]) -> std::collections::BTreeSet<String> {
    names.iter().map(|name| name.to_string()).collect()
}

fn text_turn(text: &str) -> ModelTurn {
    ModelTurn::new(
        Some("msg_1".to_string()),
        OneOrMany::one(AssistantContent::text(text)),
        Usage::new(),
        tools(&[]),
        tools(&[]),
    )
}

fn tool_turn(name: &str, executable: &[&str]) -> ModelTurn {
    let call = AssistantContent::ToolCall(rig_core::message::ToolCall::new(
        "call_1".to_string(),
        rig_core::message::ToolFunction::new(name.to_string(), serde_json::json!({})),
    ));
    ModelTurn::new(
        Some("msg_1".to_string()),
        OneOrMany::one(call),
        Usage::new(),
        tools(executable),
        tools(executable),
    )
}

fn user(text: &str) -> Message {
    Message::user(text)
}

fn drain(run: &mut AgentRun) -> Result<(), PromptError> {
    run.steered(Vec::new())
}

/// Entry: the machine is entered with one joined history and sends it
/// as-is — no prompt/context split anywhere.
#[test]
fn call_model_carries_the_whole_history() {
    let mut run = AgentRun::new(vec![user("first"), user("second"), user("third")]);
    let AgentRunStep::CallModel { history, turn } = run.next_step().expect("step") else {
        panic!("entry issues a model call");
    };
    assert_eq!(turn, 1);
    let texts: Vec<String> = history
        .iter()
        .filter_map(|message| message.user_text())
        .collect();
    assert_eq!(texts, ["first", "second", "third"]);
}

/// An empty entry history violates the entry contract and fails loud.
#[test]
#[should_panic(expected = "history must not be empty")]
fn empty_entry_history_panics() {
    let _ = AgentRun::new(Vec::new());
}

/// Classification: a tool-free turn commits as the Done candidate and the
/// decision finishes the run when the queue is silent.
#[test]
fn final_turn_commits_and_done_on_silent_queue() {
    let mut run = AgentRun::new(vec![user("go")]);
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    run.turn_committed(text_turn("hello")).expect("commit");
    assert!(matches!(run.next_step(), Ok(AgentRunStep::DrainSteers)));
    drain(&mut run).expect("decide");
    let AgentRunStep::Done(response) = run.next_step().expect("step") else {
        panic!("silent queue finalizes");
    };
    assert_eq!(response.output, "hello");
    // The run's own messages include the committed assistant turn.
    assert_eq!(run.messages().len(), 2, "prompt + assistant");
}

/// Classification: a tool turn is admitted and issued to the driver;
/// results feed back and the loop continues.
#[test]
fn tool_turn_admits_executes_and_loops() {
    let mut run = AgentRun::new(vec![user("go")]).max_turns(2);
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    run.turn_committed(tool_turn("add", &["add", "sub"]))
        .expect("commit");
    let AgentRunStep::CallTools { calls } = run.next_step().expect("step") else {
        panic!("tools are issued");
    };
    assert_eq!(calls.len(), 1);
    assert!(calls[0].preresolved_result.is_none(), "known tool admitted");

    let result = crate::agent::prompt_request::tool_result_message(
        "call_1".to_string(),
        None,
        "3".to_string(),
    );
    run.tool_results(vec![result]).expect("results");
    assert!(matches!(run.next_step(), Ok(AgentRunStep::DrainSteers)));
    drain(&mut run).expect("decide");
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    // The history carries the tool roundtrip.
    assert_eq!(run.messages().len(), 3, "prompt + assistant + results");
}

/// Admission: a tool name the model was not offered is a model-side
/// mistake — the call returns an in-band synthetic result naming the
/// problem; the run does not stop and does not pause.
#[test]
fn unknown_tool_name_gets_an_inband_synthetic_result() {
    let mut run = AgentRun::new(vec![user("go")]);
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    run.turn_committed(tool_turn("nonexistent", &["add", "sub"]))
        .expect("commit");
    let AgentRunStep::CallTools { calls } = run.next_step().expect("step") else {
        panic!("tools are issued");
    };
    let preresolved = calls[0].preresolved_result.as_ref().expect("synthetic");
    let UserContent::ToolResult(tool_result) = preresolved else {
        panic!("the synthetic result is a tool result");
    };
    let rendered = serde_json::to_string(&tool_result.content).expect("render");
    assert!(
        rendered.contains("unknown or disallowed tool `nonexistent`"),
        "the model is told: {rendered}"
    );
    assert!(
        rendered.contains("add, sub"),
        "the model sees what exists: {rendered}"
    );
    // The assistant turn (with the rejected call) committed: the model's
    // own output stays in history, answered by the synthetic result.
    let Message::Assistant { content, .. } = run.messages().last().expect("committed") else {
        panic!("assistant turn committed");
    };
    assert!(matches!(
        content.first(),
        AssistantContent::ToolCall(call) if call.function.name == "nonexistent"
    ));
}

/// The defect path: `broken` discards the turn (never entering history),
/// returns the turn slot, and the decision retries within the cap.
#[test]
fn broken_turn_discards_retries_and_exhausts() {
    let mut run = AgentRun::new(vec![user("go")]).max_turns(1);
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    run.broken("`lookup`: truncated".to_string()).expect("feed");
    assert!(matches!(run.next_step(), Ok(AgentRunStep::DrainSteers)));
    drain(&mut run).expect("retry within cap");
    // The slot was returned: the retry re-issues turn 1 under a budget of 1.
    let AgentRunStep::CallModel { turn, history } = run.next_step().expect("step") else {
        panic!("retry issues a model call");
    };
    assert_eq!(turn, 1, "discarded attempts do not consume the budget");
    assert_eq!(history.len(), 1, "the defective turn never entered history");

    run.broken("`lookup`: truncated again".to_string())
        .expect("feed");
    assert!(matches!(run.next_step(), Ok(AgentRunStep::DrainSteers)));
    let err = drain(&mut run).expect_err("second consecutive defect exhausts");
    assert!(
        err.to_string()
            .contains("repeatedly emitted tool calls with malformed arguments"),
        "got: {err}"
    );
}

/// A drained steer resets every retry streak — a present, steering user is
/// their own circuit breaker.
#[test]
fn a_drained_steer_resets_the_defect_streak() {
    let mut run = AgentRun::new(vec![user("go")]);
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    run.broken("d1".to_string()).expect("feed");
    assert!(matches!(run.next_step(), Ok(AgentRunStep::DrainSteers)));
    run.steered(vec![user("wait, use python")])
        .expect("reset + retry");
    let AgentRunStep::CallModel { history, .. } = run.next_step().expect("step") else {
        panic!("steered retry issues a model call");
    };
    let texts: Vec<String> = history
        .iter()
        .filter_map(|message| message.user_text())
        .collect();
    assert_eq!(texts, ["go", "wait, use python"]);
}

/// A steer drained after a final turn re-opens the run: the final turn
/// stays committed, the queued message continues the conversation.
#[test]
fn a_steer_after_the_final_turn_reopens_the_run() {
    let mut run = AgentRun::new(vec![user("go")]).max_turns(2);
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    run.turn_committed(text_turn("first answer"))
        .expect("commit");
    assert!(matches!(run.next_step(), Ok(AgentRunStep::DrainSteers)));
    run.steered(vec![user("and then?")]).expect("reopen");
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    assert_eq!(
        run.messages().len(),
        3,
        "prompt + committed final turn + steer"
    );
}

/// The budget gates the loop at the decision, never the first turn (the
/// at-least-one-turn invariant).
#[test]
fn budget_fails_the_loop_not_the_first_turn() {
    let mut run = AgentRun::new(vec![user("go")]).max_turns(1);
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    run.turn_committed(tool_turn("add", &["add"]))
        .expect("commit");
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallTools { .. })
    ));
    let result = crate::agent::prompt_request::tool_result_message(
        "call_1".to_string(),
        None,
        "3".to_string(),
    );
    run.tool_results(vec![result]).expect("results");
    assert!(matches!(run.next_step(), Ok(AgentRunStep::DrainSteers)));
    let err = drain(&mut run).expect_err("no budget for another turn");
    assert!(
        matches!(err, PromptError::MaxTurnsError { .. }),
        "got: {err}"
    );
}

/// Hook stops set the terminating flag and exit at the decision — through
/// the drain, so queued steers land in history first.
#[test]
fn terminate_drains_then_fails() {
    let mut run = AgentRun::new(vec![user("go")]);
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    run.terminate("policy stop").expect("feed");
    assert!(matches!(run.next_step(), Ok(AgentRunStep::DrainSteers)));
    run.steered(vec![user("one last thing")])
        .expect_err("terminating exits at the decision");
    assert_eq!(
        run.messages().len(),
        2,
        "the steer landed in history before the exit"
    );
}

/// A terminal provider error fails the run with the original error, after
/// the drain; a retryable one retries, bounded.
#[test]
fn provider_errors_drain_then_retry_or_fail() {
    // Terminal: drains, fails with the original error.
    let mut run = AgentRun::new(vec![user("go")]);
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    let original = PromptError::prompt_cancelled(Vec::new(), "auth rejected");
    run.provider_error(ProviderErrorClass::Terminal, original)
        .expect("feed");
    assert!(matches!(run.next_step(), Ok(AgentRunStep::DrainSteers)));
    let err = drain(&mut run).expect_err("terminal fails");
    assert!(err.to_string().contains("auth rejected"), "got: {err}");

    // Retryable: retries once, then fails naming the persistence.
    let mut run = AgentRun::new(vec![user("go")]).max_turns(5);
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    let original = PromptError::prompt_cancelled(Vec::new(), "rate limit hit");
    run.provider_error(ProviderErrorClass::Retryable, original)
        .expect("feed");
    assert!(matches!(run.next_step(), Ok(AgentRunStep::DrainSteers)));
    drain(&mut run).expect("retryable retries");
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));

    let original = PromptError::prompt_cancelled(Vec::new(), "rate limit hit again");
    run.provider_error(ProviderErrorClass::Retryable, original)
        .expect("feed");
    assert!(matches!(run.next_step(), Ok(AgentRunStep::DrainSteers)));
    let err = drain(&mut run).expect_err("second consecutive retryable fails");
    assert!(
        err.to_string().contains("rate limit hit again"),
        "got: {err}"
    );
}

/// Final-turn rejection (the model-turn-finished Retry hook): Repeat drops
/// the rejected response; Feedback records it with corrective text.
#[test]
fn reject_final_turn_repeat_and_feedback() {
    let mut run = AgentRun::new(vec![user("go")]).max_turns(3);
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    run.turn_committed(text_turn("weak")).expect("commit");
    run.reject_final_turn(crate::agent::hook::RetryRequest::Repeat)
        .expect("reject");
    assert!(matches!(run.next_step(), Ok(AgentRunStep::DrainSteers)));
    drain(&mut run).expect("rejection loops");
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    assert_eq!(run.messages().len(), 1, "the rejected turn was dropped");

    run.turn_committed(text_turn("still weak")).expect("commit");
    run.reject_final_turn(crate::agent::hook::RetryRequest::Feedback(
        "be more specific".to_string(),
    ))
    .expect("reject");
    assert!(matches!(run.next_step(), Ok(AgentRunStep::DrainSteers)));
    drain(&mut run).expect("rejection loops");
    let texts: Vec<String> = run
        .messages()
        .iter()
        .filter_map(|message| message.user_text())
        .collect();
    assert_eq!(texts, ["go", "be more specific"]);
    assert_eq!(run.messages().len(), 3, "prompt + kept turn + feedback");
}

/// Steering is structural: the feed exists only at the drain.
#[test]
fn steering_outside_the_drain_is_a_protocol_violation() {
    let mut run = AgentRun::new(vec![user("go")]);
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    let err = run
        .steered(vec![user("too early")])
        .expect_err("only the drain accepts steers");
    assert!(
        err.to_string().contains("outside the drain point"),
        "got: {err}"
    );
}

/// Usage accounting: a committed turn records its completion call; a
/// discarded attempt keeps whatever was recorded mid-stream (the tokens
/// were spent).
#[test]
fn usage_records_on_commit() {
    let mut run = AgentRun::new(vec![user("go")]);
    assert!(matches!(
        run.next_step(),
        Ok(AgentRunStep::CallModel { .. })
    ));
    let mut usage = Usage::new();
    usage.total_tokens = 7;
    run.turn_committed(ModelTurn::new(
        None,
        OneOrMany::one(AssistantContent::text("done")),
        usage,
        tools(&[]),
        tools(&[]),
    ))
    .expect("commit");
    assert_eq!(run.usage().total_tokens, 7);
    assert_eq!(run.completion_calls().len(), 1);
}
