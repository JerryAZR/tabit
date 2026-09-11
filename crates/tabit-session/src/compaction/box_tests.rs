//! The compaction box's own tests: the trigger formulas, cut
//! selection's maximization, the request-failure classification, and
//! whole-door runs over a scripted mock — the entry lands, the walked
//! context becomes [summary] + tail, the bracket events fire, and a
//! violating summarizer fails the pass with nothing persisted.
//!
//! Fixtures are **measured** dialogues (2026-09 ruling: nothing is
//! estimated — an unmeasured context skips): each round commits a
//! reported usage growing the context by exactly `delta` tokens, so a
//! boundary's tail is its suffix of deltas and the head total is the
//! last turn's reported total.

use super::*;
use crate::entry::EntryKind;
use rig_agent::AgentBuilder;
use rig_agent::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_core::completion::{CompletionError, Message};
use std::sync::Arc;
use tabit_protocol::SessionEvent;

fn config_with_window(window: u64) -> Arc<TabitConfig> {
    Arc::new(
        TabitConfig::from_toml_str(
            &format!(
                r#"
[providers.p]
base_url = "http://127.0.0.1:9999/v1"
api = "openai-completions"

[[providers.p.models]]
id = "m"
context_window = {window}
"#
            ),
            std::path::Path::new("test.toml"),
        )
        .expect("valid config"),
    )
}

fn selection() -> ModelSelection {
    ModelSelection {
        provider: "p".to_string(),
        model: "m".to_string(),
        thinking_level: None,
    }
}

/// A measured dialogue: `rounds` user/assistant pairs where turn
/// r's reported total is `(r+1)·delta` — the first delta folds the
/// system prompt and opening prompt in (measured), every later one
/// grows the context by exactly `delta`. The head measurement is
/// `rounds·delta`; a boundary after round k keeps a tail of
/// `(rounds−1−k)·delta` delta tokens.
fn cell_with_measured_dialogue(rounds: usize, delta: u64) -> ConversationCell {
    let cell: ConversationCell = Arc::new(std::sync::RwLock::new(
        tabit_log::ContextManager::seeded(Vec::new()),
    ));
    for round in 0..rounds {
        crate::lock::write(&cell).fold(Message::user(format!("question {round}")));
        crate::lock::write(&cell).fold_turn_with_id(
            Message::assistant(format!("answer {round}")),
            format!("turn-{round}"),
            Usage {
                input_tokens: (round as u64 + 1) * delta - 10,
                output_tokens: 10,
                total_tokens: (round as u64 + 1) * delta,
                ..Usage::default()
            },
        );
    }
    cell
}

fn branch_of(cell: &ConversationCell) -> Vec<String> {
    read(cell)
        .active_branch()
        .iter()
        .map(|entry| entry.id.clone())
        .collect()
}

/// The compaction count on the raw branch (leaf-appended, never
/// deleted).
fn compactions_of(cell: &ConversationCell) -> usize {
    read(cell)
        .active_branch()
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
        .count()
}

#[test]
fn fires_matches_the_ruled_formulas() {
    // Window 200k: A = >150k (mailbox empty); B = >167,232. The
    // reserve is absolute, so B only binds meaningfully above it.
    let window = 200_000u64;
    assert!(!fires(Door::Idle, 140_000, window, true));
    assert!(fires(Door::Idle, 151_000, window, true));
    // A requires the empty mailbox; B does not.
    assert!(!fires(Door::Idle, 151_000, window, false));
    assert!(fires(Door::Idle, 168_000, window, false));
    assert!(!fires(Door::PreRequest, 151_000, window, false));
    assert!(fires(Door::PreRequest, 168_000, window, false));
    // On small windows the absolute reserve dominates: at a 1k window
    // B is always over — the idle bound can never exceed the seam
    // bound (the disjunction ruling).
    assert!(fires(Door::Idle, 700, 1_000, true));
    assert!(fires(Door::PreRequest, 700, 1_000, true));
    // Forced doors.
    assert!(fires(Door::Manual, 0, 1_000_000, false));
    assert!(fires(Door::Overflow, 0, 1_000_000, false));
}

#[test]
fn select_cut_picks_the_latest_feasible_boundary() {
    // Four rounds × 9,000 tokens: the head total is 36,000, and the
    // latest boundary keeping the 16,384-token tail floor retains two
    // rounds (18,000) — boundary 4, right before round 2's prompt.
    let cell = cell_with_measured_dialogue(4, 9_000);
    let history = read(&cell).history();
    let sums = delta_suffix_sums(&history);
    let boundary = select_cut(&history, &sums, 36_000, 10_000_000).expect("feasible");
    assert_eq!(
        boundary, 4,
        "the maximization cuts as late as the tail floor allows"
    );
    assert_eq!(sums[4], 18_000, "the retained tail is two rounds of deltas");

    // An empty view has nothing to compact.
    assert!(select_cut(&[], &[], 0, 10_000_000).is_none());

    // A tiny window makes every boundary's prefix break the 75% cap —
    // no cut exists and the door declines.
    assert!(select_cut(&history, &sums, 36_000, 1).is_none());
}

#[test]
fn select_cut_bounds_the_prefix_under_the_cap() {
    // Fifty rounds × 1,000 tokens (head 50,000) on a 30,000 window:
    // the cap is 22,500, so the latest feasible boundary keeps at
    // least 27,500 tokens of tail — 28 rounds, cutting before round
    // 22 (boundary 44).
    let cell = cell_with_measured_dialogue(50, 1_000);
    let history = read(&cell).history();
    let sums = delta_suffix_sums(&history);
    let window = 30_000u64;
    let boundary = select_cut(&history, &sums, 50_000, window).expect("feasible");
    let cap = (dials::SENT_PREFIX_FRACTION * window as f64) as u64;
    assert_eq!(
        boundary, 44,
        "the latest boundary whose prefix fits the cap"
    );
    assert!(50_000 - sums[44] < cap);
    assert!(sums[44] >= dials::KEEP_TAIL_TOKENS);
    // It is the LATEST such boundary: the next one's prefix breaks
    // the cap.
    assert!(50_000 - sums[46] >= cap, "a later boundary was feasible");
    // The cut lands on a model output: the entry before it is a
    // tool-free assistant (or the leading compaction).
    assert!(valid_boundary(&history, boundary));
}

#[test]
fn valid_boundary_rejects_tool_carrying_outputs_and_mid_turn_positions() {
    let cell: ConversationCell = Arc::new(std::sync::RwLock::new(
        tabit_log::ContextManager::seeded(vec![
            Message::user("q"),
            Message::Assistant {
                id: None,
                content: rig_core::OneOrMany::one(rig_core::message::AssistantContent::ToolCall(
                    rig_core::message::ToolCall::new(
                        "call-1".to_string(),
                        rig_core::message::ToolFunction::new(
                            "echo".to_string(),
                            serde_json::json!({}),
                        ),
                    ),
                )),
            },
            Message::User {
                content: rig_core::OneOrMany::one(rig_core::message::UserContent::ToolResult(
                    rig_core::message::ToolResult {
                        id: "call-1".to_string(),
                        call_id: None,
                        content: rig_core::OneOrMany::one(
                            rig_core::message::ToolResultContent::text("ok"),
                        ),
                        status: None,
                    },
                )),
            },
            Message::user("steer"),
            Message::assistant("final"),
        ]),
    ));
    let history = read(&cell).history();
    // Position 0 is always valid (the session-start floor).
    assert!(valid_boundary(&history, 0));
    // After the tool-carrying assistant (mid-roundtrip) and after its
    // results: invalid — the model was mid-task and steers may have
    // queued there.
    assert!(!valid_boundary(&history, 2));
    assert!(!valid_boundary(&history, 3));
    // After the final, tool-free answer: valid.
    assert!(valid_boundary(&history, 5));
}

#[test]
fn an_overflow_rejection_teaches_the_window_and_shortens() {
    let state = Compaction::new();
    let cell = cell_with_measured_dialogue(4, 9_000);
    let history = read(&cell).history();
    let error =
        CompletionError::HttpError(rig_core::http_client::Error::InvalidStatusCodeWithMessage(
            http::StatusCode::BAD_REQUEST,
            "prompt is too long: 19565 tokens > 16384 tokens maximum".to_string(),
        ));
    match rejected(error, &state, 4, &history) {
        Rejection::Shorten(shortened) => {
            assert!(shortened < 4);
            assert_eq!(state.taught_window(), Some(16384));
        }
        other => panic!("expected a shorten step, got {other:?}"),
    }
}

#[test]
fn a_non_overflow_failure_fails_the_pass() {
    let state = Compaction::new();
    let cell = cell_with_measured_dialogue(4, 9_000);
    let history = read(&cell).history();
    let error = CompletionError::ProviderError("model overloaded".to_string());
    assert!(matches!(
        rejected(error, &state, 4, &history),
        Rejection::Fail(message) if message.contains("overloaded")
    ));
}

#[test]
fn an_overflow_rejection_at_the_empty_prefix_fails_the_pass() {
    let state = Compaction::new();
    let cell = cell_with_measured_dialogue(4, 9_000);
    let history = read(&cell).history();
    let error =
        CompletionError::HttpError(rig_core::http_client::Error::InvalidStatusCodeWithMessage(
            http::StatusCode::BAD_REQUEST,
            "prompt is too long: 19565 tokens > 16384 tokens maximum".to_string(),
        ));
    assert!(matches!(
        rejected(error, &state, 0, &history),
        Rejection::Fail(message) if message.contains("empty prefix")
    ));
}

#[test]
fn the_head_measurement_is_the_newest_reported_total() {
    // A measured turn lands after the seeded (never-measured)
    // dialogue: the server's total wins — the unmeasured past does
    // not inflate it.
    let cell = cell_with_measured_dialogue(0, 9_000);
    crate::lock::write(&cell).fold(Message::user("the question"));
    crate::lock::write(&cell).fold_turn_with_id(
        Message::assistant("the measured answer"),
        "measured".to_string(),
        Usage {
            input_tokens: 9_000,
            output_tokens: 1_000,
            total_tokens: 10_000,
            ..Usage::default()
        },
    );
    assert_eq!(read(&cell).measured_total(), Some(10_000));
}

#[test]
fn an_unreported_turn_inherits_the_previous_valid_total() {
    // [user, assistant(total 10k), user, assistant(no report)] — the
    // read is the newest real measurement; the uncounted turn adds
    // nothing and is never estimated.
    let cell = cell_with_measured_dialogue(0, 9_000);
    crate::lock::write(&cell).fold(Message::user("q"));
    crate::lock::write(&cell).fold_turn_with_id(
        Message::assistant("a"),
        "a".to_string(),
        Usage {
            input_tokens: 9_000,
            output_tokens: 1_000,
            total_tokens: 10_000,
            ..Usage::default()
        },
    );
    crate::lock::write(&cell).fold(Message::user("q2"));
    crate::lock::write(&cell).fold_turn_with_id(
        Message::assistant("a2"),
        "a2".to_string(),
        Usage::new(),
    );
    assert_eq!(read(&cell).measured_total(), Some(10_000));
}

#[test]
fn the_window_read_in_the_post_compaction_gap_is_the_regime_base() {
    // The head is the compaction node: the context read is its
    // persisted `tokens_after` exactly — no walk, no estimate.
    let cell = cell_with_measured_dialogue(4, 9_000);
    let cut_child = branch_of(&cell)[6].clone();
    crate::lock::write(&cell).commit_compaction(
        "compaction-id".to_string(),
        "the summary".to_string(),
        cut_child,
        36_000,
        18_020,
        Usage::default(),
    );
    assert_eq!(read(&cell).measured_total(), Some(18_020));
    // The view leads with the compaction, and a boundary right after
    // it is valid — multi-pass cuts exactly there.
    let history = read(&cell).history();
    assert!(matches!(
        history.first().map(|entry| &entry.kind),
        Some(EntryKind::Compaction { .. })
    ));
    assert!(valid_boundary(&history, 1), "a compaction ends a prefix");
}

#[test]
fn a_stale_tail_total_is_unreachable_the_regime_base_wins() {
    // The old bug's shape, now structurally excluded: a measured turn
    // retained in the tail reported 100k against the replaced prefix.
    // The raw walk meets the compaction node before that entry, so
    // the read is the regime's base — the stale 100k can never
    // surface.
    let cell = cell_with_measured_dialogue(0, 9_000);
    crate::lock::write(&cell).fold(Message::user("q"));
    crate::lock::write(&cell).fold_turn_with_id(
        Message::assistant("the stale, measured answer"),
        "stale".to_string(),
        Usage {
            input_tokens: 90_000,
            output_tokens: 10_000,
            total_tokens: 100_000,
            ..Usage::default()
        },
    );
    // The compaction retains the stale entry in its tail.
    let cut_child = branch_of(&cell)[0].clone();
    crate::lock::write(&cell).commit_compaction(
        "compaction-id".to_string(),
        "the summary".to_string(),
        cut_child,
        100_000,
        3_500,
        Usage::default(),
    );
    assert_eq!(
        read(&cell).measured_total(),
        Some(3_500),
        "the stale tail total is behind the compaction on the walk"
    );
    // A younger measurement — a request on the compacted context —
    // is the newest valid number again.
    crate::lock::write(&cell).fold(Message::user("next"));
    crate::lock::write(&cell).fold_turn_with_id(
        Message::assistant("measured on the compacted context"),
        "fresh".to_string(),
        Usage {
            input_tokens: 3_000,
            output_tokens: 500,
            total_tokens: 3_500,
            ..Usage::default()
        },
    );
    assert_eq!(read(&cell).measured_total(), Some(3_500));
}

async fn run_manual(
    cell: &ConversationCell,
    agent: &rig_agent::agent::Agent,
    config: &Arc<TabitConfig>,
) -> (Outcome, Vec<SessionEvent>) {
    let state = Compaction::new();
    let token = CancellationToken::new();
    let mut events = Vec::new();
    let outcome = run(
        Door::Manual,
        cell,
        &state,
        agent,
        &token,
        config,
        &selection(),
        true,
        &mut |event| events.push(event),
    )
    .await;
    (outcome, events)
}

fn summary_stream_turns() -> Vec<Vec<MockStreamEvent>> {
    vec![vec![
        MockStreamEvent::text("## Goal\n- keep working"),
        MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(
            rig_core::completion::Usage {
                input_tokens: 100,
                output_tokens: 20,
                ..Default::default()
            },
        )),
    ]]
}

#[tokio::test]
async fn a_manual_pass_commits_the_entry_and_truncates_the_context() {
    let cell = cell_with_measured_dialogue(4, 9_000);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns(
        summary_stream_turns(),
    ))
    .build();
    let config = config_with_window(10_000_000);
    let (outcome, events) = run_manual(&cell, &agent, &config).await;
    assert!(
        matches!(&outcome, Outcome::Compacted { passes: 1, .. }),
        "{outcome:?}"
    );
    // The bracket: started, the delta, finished.
    assert!(matches!(
        events.first(),
        Some(SessionEvent::CompactionStarted { .. })
    ));
    assert!(matches!(
        events.last(),
        Some(SessionEvent::CompactionFinished { .. })
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::CompactionDelta { text, .. } if text.contains("keep working")
    )));
    // The walked context is [summary] + retained tail; the tree keeps
    // every node and gains the compaction leaf.
    let messages = read(&cell).messages();
    let Message::User { content } = &messages[0] else {
        panic!("the summary leads");
    };
    assert!(matches!(
        content.first(),
        rig_core::message::UserContent::Text(text) if text.text.contains("keep working")
    ));
    assert_eq!(
        branch_of(&cell).len(),
        (4 * 2) + 1,
        "the tree keeps every node — the branch gains the compaction, nothing is deleted"
    );
}

#[tokio::test]
async fn a_committed_pass_satisfies_the_telescoping_identity() {
    // The design's closing identity over a **real** pass — the base
    // the box computed itself: the committed node's `tokens_after`
    // equals the retained tail's suffix-delta sum plus the summary's
    // own measured output, and the head measurement equals the view's
    // delta sum plus that same output. Cut at boundary 4 retains two
    // rounds (18,000 of deltas); the scripted summary outputs 20.
    let cell = cell_with_measured_dialogue(4, 9_000);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns(
        summary_stream_turns(),
    ))
    .build();
    let config = config_with_window(10_000_000);
    let (outcome, _events) = run_manual(&cell, &agent, &config).await;
    assert!(
        matches!(
            &outcome,
            Outcome::Compacted {
                passes: 1,
                tokens_after: 18_020
            }
        ),
        "{outcome:?}"
    );
    let view = read(&cell).history();
    let Some(EntryKind::Compaction {
        tokens_after,
        usage,
        ..
    }) = view.first().map(|entry| &entry.kind)
    else {
        panic!("the compaction leads the view");
    };
    assert_eq!(*tokens_after, 18_020);
    assert_eq!(usage.output_tokens, 20);
    let delta_sum: u64 = view
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::AssistantMessage { delta_tokens, .. } => *delta_tokens,
            _ => None,
        })
        .sum();
    assert_eq!(delta_sum, 18_000, "the two retained rounds");
    assert_eq!(
        read(&cell).measured_total(),
        Some(delta_sum + usage.output_tokens)
    );
}

#[tokio::test]
async fn a_violating_summarizer_is_discarded_and_the_request_retried() {
    let cell = cell_with_measured_dialogue(4, 9_000);
    // First attempt: the summarizer reaches for a tool. Second: a
    // clean summary. The discard-and-retry (owner ruling) recovers.
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::text("let me look"),
            MockStreamEvent::tool_call("call-1", "read", serde_json::json!({"path": "x"})),
            MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(
                rig_core::completion::Usage::default(),
            )),
        ],
        vec![
            MockStreamEvent::text(
                "## Goal
- recovered",
            ),
            MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(
                rig_core::completion::Usage::default(),
            )),
        ],
    ]))
    .build();
    let config = config_with_window(10_000_000);
    let (outcome, events) = run_manual(&cell, &agent, &config).await;
    assert!(
        matches!(&outcome, Outcome::Compacted { passes: 1, .. }),
        "{outcome:?}"
    );
    // The discarded attempt closed its bracket as failed; the retry
    // opened a fresh one and committed.
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::CompactionFailed { message, .. } if message.contains("discarded and the request retried")
    )));
    assert!(events.iter().any(|event| matches!(
        &event,
        SessionEvent::CompactionDelta { text, .. } if text.contains("recovered")
    )));
    assert!(matches!(
        events.last(),
        Some(SessionEvent::CompactionFinished { .. })
    ));
}

#[tokio::test]
async fn a_persistently_violating_summarizer_fails_the_pass_and_persists_nothing() {
    let cell = cell_with_measured_dialogue(4, 9_000);
    // Every attempt (the initial + the one bounded retry) violates.
    let violating_turn = vec![
        MockStreamEvent::text("let me look"),
        MockStreamEvent::tool_call("call-1", "read", serde_json::json!({"path": "x"})),
        MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(
            rig_core::completion::Usage::default(),
        )),
    ];
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([
        violating_turn.clone(),
        violating_turn,
    ]))
    .build();
    let config = config_with_window(10_000_000);
    let (outcome, events) = run_manual(&cell, &agent, &config).await;
    assert!(matches!(
        &outcome,
        Outcome::Failed { message, passes: 0 } if message.contains("every attempt")
    ));
    // Both attempts announced their discard.
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SessionEvent::CompactionFailed { .. }))
            .count(),
        2
    );
    // Nothing committed: no compaction node exists.
    assert_eq!(compactions_of(&cell), 0);
}

#[tokio::test]
async fn an_empty_summary_fails_the_pass() {
    let cell = cell_with_measured_dialogue(4, 9_000);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([vec![
        MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(
            rig_core::completion::Usage::default(),
        )),
    ]]))
    .build();
    let config = config_with_window(10_000_000);
    let (outcome, _events) = run_manual(&cell, &agent, &config).await;
    assert!(matches!(
        &outcome,
        Outcome::Failed { message, .. } if message.contains("empty summary")
    ));
}

#[tokio::test]
async fn an_unmeasured_context_skips_every_door() {
    // Nothing on the branch ever reported usage: the context size is
    // absence, never an estimate — even the forced doors decline.
    let cell: ConversationCell = Arc::new(std::sync::RwLock::new(
        tabit_log::ContextManager::seeded(vec![
            Message::user("q"),
            Message::assistant("an unmeasured answer"),
        ]),
    ));
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns(
        summary_stream_turns(),
    ))
    .build();
    let config = config_with_window(10_000_000);
    let (outcome, events) = run_manual(&cell, &agent, &config).await;
    assert_eq!(outcome, Outcome::Skipped);
    assert!(events.is_empty(), "no bracket opened");
    assert_eq!(compactions_of(&cell), 0, "nothing persisted");
}

#[tokio::test]
async fn an_unknown_window_skips_with_nothing_run() {
    let cell = cell_with_measured_dialogue(4, 9_000);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns(
        summary_stream_turns(),
    ))
    .build();
    // No context_window in the config: every door skips.
    let config = Arc::new(
        TabitConfig::from_toml_str(
            r#"
[providers.p]
base_url = "http://127.0.0.1:9999/v1"
api = "openai-completions"

[[providers.p.models]]
id = "m"
"#,
            std::path::Path::new("test.toml"),
        )
        .expect("valid config"),
    );
    let (outcome, events) = run_manual(&cell, &agent, &config).await;
    assert_eq!(outcome, Outcome::Skipped);
    assert!(events.is_empty());
}

#[test]
fn a_cancelled_token_kills_the_stream_before_anything_persists() {
    // The token fires before the first poll: the select's biased arm
    // returns Cancelled without ever yielding from the mock.
    let cell = cell_with_measured_dialogue(4, 9_000);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns(
        summary_stream_turns(),
    ))
    .build();
    let config = config_with_window(10_000_000);
    let state = Compaction::new();
    let token = CancellationToken::new();
    token.cancel();
    let mut events = Vec::new();
    let outcome = futures::executor::block_on(run(
        Door::Manual,
        &cell,
        &state,
        &agent,
        &token,
        &config,
        &selection(),
        true,
        &mut |event| events.push(event),
    ));
    assert_eq!(outcome, Outcome::Cancelled { passes: 0 });
    assert_eq!(compactions_of(&cell), 0);
}

#[tokio::test]
async fn a_broken_tool_call_is_the_same_violation_discarded_and_retried() {
    let cell = cell_with_measured_dialogue(4, 9_000);
    // First attempt: a tool call with unparseable arguments — the
    // model-side defect, classified by the common path. The pass
    // treats it exactly like any attempted tool call: discard and
    // resend. Second attempt: a clean summary.
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([
        vec![MockStreamEvent::Error(
            rig_agent::test_utils::MockError::malformed_tool_call(
                "read",
                "arguments are not valid JSON",
            ),
        )],
        vec![
            MockStreamEvent::text("## Goal\n- recovered"),
            MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(
                rig_core::completion::Usage::default(),
            )),
        ],
    ]))
    .build();
    let config = config_with_window(10_000_000);
    let (outcome, events) = run_manual(&cell, &agent, &config).await;
    assert!(
        matches!(&outcome, Outcome::Compacted { passes: 1, .. }),
        "{outcome:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::CompactionFailed { message, .. }
            if message.contains("discarded and the request retried")
    )));
}

#[tokio::test]
async fn a_manual_request_below_the_tail_floor_declines_benignly() {
    // A short history has no feasible cut: nothing worth folding is
    // not a failure — compaction did not happen, and the context is
    // good to continue with as-is. The manual command reports this
    // as a friendly note, not a failed bracket.
    let cell = cell_with_measured_dialogue(1, 100);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns(
        summary_stream_turns(),
    ))
    .build();
    let config = config_with_window(10_000_000);
    let (outcome, events) = run_manual(&cell, &agent, &config).await;
    assert_eq!(outcome, Outcome::NothingToCompact, "{outcome:?}");
    assert!(events.is_empty());
}

#[tokio::test]
async fn a_below_envelope_window_skips_loudly_without_running_a_pass() {
    // A 40k window contradicts the dials (B demands a context below
    // the kept-tail floor), so the door declines up front — even
    // forced — rather than burning passes that provably cannot
    // satisfy the post-check. The declared envelope is 64k, rounded
    // up from the 57,344 contradiction line to leave room for real
    // work.
    let cell = cell_with_measured_dialogue(4, 9_000);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns(
        summary_stream_turns(),
    ))
    .build();
    let config = config_with_window(40_000);
    let (outcome, events) = run_manual(&cell, &agent, &config).await;
    assert_eq!(outcome, Outcome::Skipped, "{outcome:?}");
    assert!(events.is_empty(), "no bracket opened");
    assert_eq!(branch_of(&cell).len(), 4 * 2, "nothing persisted");
}

#[tokio::test]
async fn a_history_far_over_the_window_compacts_in_strictly_shrinking_passes() {
    // The designed multi-pass: a 100k measured history on a 70k
    // window. Pass 1's latest boundary keeps a 50k tail (the 75% cap
    // forbids more prefix) — still over B's bound (70k − 32.8k) —
    // and pass 2 cuts to a 20k tail and exits. Strict shrink each
    // pass; the pass cap never comes into play.
    let cell = cell_with_measured_dialogue(10, 10_000);
    let summary = || {
        vec![
            MockStreamEvent::text("## Goal\n- pass"),
            MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(
                rig_core::completion::Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    ..Default::default()
                },
            )),
        ]
    };
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([
        summary(),
        summary(),
    ]))
    .build();
    let config = config_with_window(70_000);
    let (outcome, events) = run_manual(&cell, &agent, &config).await;
    assert!(
        matches!(
            &outcome,
            Outcome::Compacted {
                passes: 2,
                tokens_after: 20_020
            }
        ),
        "{outcome:?}"
    );
    let started = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::CompactionStarted { .. }))
        .count();
    assert_eq!(started, 2);
}

#[tokio::test]
async fn a_huge_late_growth_the_cut_cannot_shed_stops_the_loop_loud() {
    // The guard's designed case: a 60k late paste riding the last
    // turn's delta. Passes peel the small turns off the tail one at a
    // time, but the paste rides every feasible tail (cutting after it
    // leaves nothing measurable behind); once the tail is only the
    // paste's turn, the next pass cannot shrink and the guard fails
    // loud with what landed standing.
    let cell: ConversationCell = Arc::new(std::sync::RwLock::new(
        tabit_log::ContextManager::seeded(Vec::new()),
    ));
    let paste = "x".repeat(1_000);
    crate::lock::write(&cell).fold(Message::user("first question"));
    crate::lock::write(&cell).fold_turn_with_id(
        Message::assistant("first answer"),
        "t0".to_string(),
        Usage {
            input_tokens: 990,
            output_tokens: 10,
            total_tokens: 1_000,
            ..Usage::default()
        },
    );
    crate::lock::write(&cell).fold(Message::user("second question"));
    crate::lock::write(&cell).fold_turn_with_id(
        Message::assistant("second answer"),
        "t1".to_string(),
        Usage {
            input_tokens: 1_990,
            output_tokens: 10,
            total_tokens: 2_000,
            ..Usage::default()
        },
    );
    crate::lock::write(&cell).fold(Message::user(paste));
    crate::lock::write(&cell).fold_turn_with_id(
        Message::assistant("done with the paste"),
        "t2".to_string(),
        Usage {
            input_tokens: 2_000,
            output_tokens: 60_000,
            total_tokens: 62_000,
            ..Usage::default()
        },
    );
    let summary = || {
        vec![
            MockStreamEvent::text("## Goal\n- pass"),
            MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(
                rig_core::completion::Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    ..Default::default()
                },
            )),
        ]
    };
    // Pass 1 retains the paste's turn alone (60k tail — the 75% cap
    // forbids folding it into the prefix), pass 2 re-cuts to the same
    // tail and the guard's `>=` fires.
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([
        summary(),
        summary(),
    ]))
    .build();
    let config = config_with_window(70_000);
    let (outcome, events) = run_manual(&cell, &agent, &config).await;
    assert!(
        matches!(&outcome, Outcome::Oversized { reason, passes: 2, .. }
            if reason.contains("cannot shrink")),
        "{outcome:?}"
    );
    let started = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::CompactionStarted { .. }))
        .count();
    assert_eq!(started, 2, "every pass ran and committed");
    assert_eq!(compactions_of(&cell), 2, "what landed stands");
}

fn length_capped_turn() -> Vec<MockStreamEvent> {
    let mut final_record = rig_core::test_utils::mock_final(Usage::default());
    final_record.finish_reason = Some(rig_core::completion::FinishReason::Length);
    vec![
        MockStreamEvent::text("cut short"),
        MockStreamEvent::FinalResponse(final_record),
    ]
}

#[tokio::test]
async fn a_length_capped_summary_shortens_and_retries() {
    // Length-cap is rejection-shaped (ruled): the first capped
    // summary shortens the request one boundary; the retry commits.
    let cell = cell_with_measured_dialogue(4, 9_000);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([
        length_capped_turn(),
        vec![
            MockStreamEvent::text("## Goal\n- fits now"),
            MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(
                rig_core::completion::Usage::default(),
            )),
        ],
    ]))
    .build();
    let config = config_with_window(10_000_000);
    let (outcome, _) = run_manual(&cell, &agent, &config).await;
    assert!(
        matches!(&outcome, Outcome::Compacted { passes: 1, .. }),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn a_length_cap_at_the_shortest_prefix_fails_the_pass() {
    // Capped all the way down to the empty prefix: there is nothing
    // shorter to send — the pass fails naming the output cap. (The
    // selection starts at boundary 4; each cap steps one boundary
    // earlier: 4 → 2 → 0 → fail.)
    let cell = cell_with_measured_dialogue(4, 9_000);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([
        length_capped_turn(),
        length_capped_turn(),
        length_capped_turn(),
    ]))
    .build();
    let config = config_with_window(10_000_000);
    let (outcome, _) = run_manual(&cell, &agent, &config).await;
    assert!(
        matches!(&outcome, Outcome::Failed { message, passes: 0 } if message.contains("output cap")),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn an_in_stream_overflow_rejection_shortens_and_retries() {
    // The request itself is rejected mid-stream with the wall's
    // message: the window is learned, the request shortens one
    // boundary, and the retry commits.
    let cell = cell_with_measured_dialogue(4, 9_000);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([
        vec![MockStreamEvent::Error(
            rig_agent::test_utils::MockError::http(
                400,
                "prompt is too long: 19565 tokens > 16384 tokens maximum",
            ),
        )],
        vec![
            MockStreamEvent::text("## Goal\n- after the wall"),
            MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(
                rig_core::completion::Usage::default(),
            )),
        ],
    ]))
    .build();
    let config = config_with_window(10_000_000);
    let state = Compaction::new();
    let token = CancellationToken::new();
    let mut events = Vec::new();
    let outcome = run(
        Door::Manual,
        &cell,
        &state,
        &agent,
        &token,
        &config,
        &selection(),
        true,
        &mut |event| events.push(event),
    )
    .await;
    assert!(
        matches!(&outcome, Outcome::Compacted { passes: 1, .. }),
        "{outcome:?}"
    );
    assert_eq!(
        state.taught_window(),
        Some(16384),
        "the wall taught the window"
    );
}

#[tokio::test]
async fn the_pass_cap_bounds_pathological_regimes() {
    // 410 rounds of 2,000 tokens (820k) on the minimum supported
    // window (65,536): every pass folds the prefix cap — 24 rounds
    // (48,000) against the 49,152 fraction — and still sits over the
    // urgent bound (32,768), so a regime that never fits would loop
    // forever; the cap stops it at 16 passes with what landed
    // standing (26 kept rounds + the last summary = 52,020 — only
    // the final pass's output rides the base, earlier summaries fold
    // into the prefixes that replaced them).
    let cell = cell_with_measured_dialogue(410, 2_000);
    let mut turns = summary_stream_turns();
    while turns.len() < 16 {
        let turn = turns[0].clone();
        turns.push(turn);
    }
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns(turns)).build();
    let config = config_with_window(65_536);
    let (outcome, _events) = run_manual(&cell, &agent, &config).await;
    assert!(
        matches!(
            &outcome,
            Outcome::Oversized {
                passes: 16,
                tokens_after: 52_020,
                ..
            }
        ),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn an_unrepairable_in_stream_failure_fails_the_invocation() {
    // Not the wall: a plain provider failure mid-summarization has no
    // shrink answer — the pass fails without retry, nothing commits,
    // and the outcome carries the failure.
    let cell = cell_with_measured_dialogue(4, 9_000);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([vec![
        MockStreamEvent::Error(rig_agent::test_utils::MockError::http(
            500,
            "upstream exploded",
        )),
    ]]))
    .build();
    let config = config_with_window(10_000_000);
    let (outcome, _events) = run_manual(&cell, &agent, &config).await;
    assert!(
        matches!(
            &outcome,
            Outcome::Failed { message, .. } if message.contains("upstream exploded")
        ),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn the_pre_request_leaf_compacts_when_condition_b_holds() {
    // The engine awaits this leaf blindly; directly: a measured
    // context past the urgent bound runs the box through the leaf's
    // own door and emission path.
    let cell = cell_with_measured_dialogue(3, 25_000);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns(
        summary_stream_turns(),
    ))
    .build();
    let door = PreRequestDoor {
        cell: cell.clone(),
        state: Arc::new(Compaction::new()),
        agent: Arc::new(agent),
        // Window 80k with a 75k measured context: condition B holds.
        config: config_with_window(80_000),
        selection: selection(),
        token: CancellationToken::new(),
        notice: None,
    };
    rig_agent::agent::PreRequestSource::at_door(&door).await;
    // The box ran through the leaf: the branch holds a compaction and
    // the walked context begins with the summary.
    let branch = read(&cell).active_branch();
    assert!(
        branch
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
    );
    let messages = read(&cell).messages();
    assert!(messages.len() < branch.len(), "the walk truncated");
}
