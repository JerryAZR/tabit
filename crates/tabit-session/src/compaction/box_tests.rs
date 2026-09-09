//! The compaction box's own tests: the trigger formulas, cut
//! selection's maximization, the request-failure classification, and
//! whole-door runs over a scripted mock — the entry lands, the walked
//! context becomes [summary] + tail, the bracket events fire, and a
//! violating summarizer fails the pass with nothing persisted.

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

/// A cell over a scripted conversation: alternating user prompts and
/// tool-free answers (every answer is a valid cut boundary). Each
/// message is ~`filler` bytes, so `rounds` and `filler` size the
/// dialogue past the retained-tail floor when the tests need a
/// feasible cut.
fn cell_with_dialogue(rounds: usize, chars_per_message: usize) -> ConversationCell {
    let mut seeded: Vec<Message> = Vec::new();
    for round in 0..rounds {
        let filler = "x".repeat(chars_per_message);
        seeded.push(Message::user(format!("{filler} question {round}")));
        seeded.push(Message::assistant(format!("{filler} answer {round}")));
    }
    Arc::new(std::sync::RwLock::new(tabit_log::ContextManager::seeded(
        seeded,
    )))
}

/// ~3k estimated tokens per message — four rounds (8 messages, ~24k
/// tokens) leave room to cut at the second boundary while keeping the
/// 16,384-token tail floor.
const BIG_FLOOR_ROUNDS: usize = 4;
const BIG_MESSAGE_CHARS: usize = 12_000;

fn branch_of(cell: &ConversationCell) -> Vec<String> {
    read(cell)
        .active_branch()
        .iter()
        .map(|entry| entry.id.clone())
        .collect()
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
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
    let branch = read(&cell).active_branch();
    // Window big enough that the prefix cap never binds; the maximization
    // cuts at the LATEST boundary keeping the 16,384-token tail floor —
    // here the second pair boundary (6 messages ≈ 18k tokens of tail).
    let cut = select_cut(&branch, 10_000_000, 0).expect("feasible");
    assert_eq!(
        cut.boundary, 2,
        "the maximization cuts as late as the tail floor allows"
    );

    // An empty branch has nothing to compact.
    assert!(select_cut(&[], 10_000_000, 0).is_none());

    // A small tail budget is unreachable through the dial constant, so
    // exercise the maximization through the prefix cap instead: a tiny
    // window makes every late boundary infeasible (prefix over the
    // cap) while the session start stays feasible only if the whole
    // history fits under the cap AND above the tail floor — here it
    // does not, so no cut exists and the door skips.
    assert!(select_cut(&branch, 1, 0).is_none());
}

#[test]
fn select_cut_bounds_the_prefix_under_the_cap() {
    // A large dialogue with a middling window: the latest boundary
    // whose prefix fits the 75% cap wins.
    let cell = cell_with_dialogue(50, 2_000);
    let branch = read(&cell).active_branch();
    let window = 30_000u64; // cap 22,500 tokens ≈ 90,000 chars
    let cut = select_cut(&branch, window, 0).expect("feasible");
    let prefix: u64 = branch[..cut.boundary].iter().map(estimate_entry).sum();
    assert!(prefix < (dials::SENT_PREFIX_FRACTION * window as f64) as u64);
    // It is the LATEST such boundary: the next boundary's prefix
    // breaks the cap or the tail floor.
    let next = cut.boundary + 2; // boundaries sit two entries apart
    if next < branch.len() {
        let next_prefix: u64 = branch[..next].iter().map(estimate_entry).sum();
        assert!(
            next_prefix >= (dials::SENT_PREFIX_FRACTION * window as f64) as u64
                || next >= branch.len(),
            "a later boundary was feasible — the cut is not maximal"
        );
    }
    // The cut lands on a model output: the entry before it is a
    // tool-free assistant (or the session start).
    assert!(valid_boundary(&branch, cut.boundary));
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
    let branch = read(&cell).active_branch();
    // Position 0 is always valid (the session-start floor).
    assert!(valid_boundary(&branch, 0));
    // After the tool-carrying assistant (mid-roundtrip) and after its
    // results: invalid — the model was mid-task and steers may have
    // queued there.
    assert!(!valid_boundary(&branch, 2));
    assert!(!valid_boundary(&branch, 3));
    // After the final, tool-free answer: valid.
    assert!(valid_boundary(&branch, 5));
}

#[test]
fn an_overflow_rejection_teaches_the_window_and_shortens() {
    let state = Compaction::new();
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
    let branch = read(&cell).active_branch();
    let error =
        CompletionError::HttpError(rig_core::http_client::Error::InvalidStatusCodeWithMessage(
            http::StatusCode::BAD_REQUEST,
            "prompt is too long: 19565 tokens > 16384 tokens maximum".to_string(),
        ));
    match rejected(error, &state, 4, &branch) {
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
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
    let branch = read(&cell).active_branch();
    let error = CompletionError::ProviderError("model overloaded".to_string());
    assert!(matches!(
        rejected(error, &state, 4, &branch),
        Rejection::Fail(message) if message.contains("overloaded")
    ));
}

#[test]
fn the_context_measures_from_the_newest_reported_total() {
    // A measured turn lands after the seeded (never-measured)
    // dialogue: the server's total wins — the unmeasured past does
    // not inflate it.
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
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
    let branch = read(&cell).active_branch();
    assert_eq!(context_tokens(&branch, 0), 10_000);
}

#[test]
fn an_unreported_turn_walks_past_and_its_tail_is_estimated() {
    // [user, assistant(total 10k), user, assistant(no report)] — the
    // newest real measurement plus estimated tokens for everything
    // appended after it.
    let cell = Arc::new(std::sync::RwLock::new(tabit_log::ContextManager::seeded(
        vec![Message::user("q")],
    )));
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
    crate::lock::write(&cell).fold(Message::assistant("a2"));
    let branch = read(&cell).active_branch();
    #[allow(clippy::indexing_slicing)] // the dialogue shape is fixed by construction
    let expected = 10_000 + estimate_entry(&branch[2]) + estimate_entry(&branch[3]);
    assert_eq!(context_tokens(&branch, 0), expected);
}

#[test]
fn the_walk_stops_at_a_compaction_entry() {
    // Everything before the insertion is gone from the context; its
    // measurements died with it. The measurement restarts at the
    // summary's estimate plus the retained tail.
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
    let cut_child = branch_of(&cell)[6].clone();
    crate::lock::write(&cell).commit_compaction(
        "compaction-id".to_string(),
        "the summary".to_string(),
        cut_child,
        24_000,
        Usage::default(),
    );
    let branch = read(&cell).active_branch();
    // [u0, a0, u1, a1, u2, a2, COMP, u3, a3] — the insertion sits at
    // the cut, index 6; most of the dialogue is pre-cut.
    #[allow(clippy::indexing_slicing)] // the insertion's position is the constructed shape
    let summary_est = estimate_entry(&branch[6]);
    #[allow(clippy::indexing_slicing)] // the retained tail follows the insertion
    let tail_est: u64 = branch[7..].iter().map(estimate_entry).sum();
    assert_eq!(context_tokens(&branch, 0), summary_est + tail_est);
    // The pre-cut entries are excluded: the whole-branch estimate is
    // several times larger.
    let whole: u64 = branch.iter().map(estimate_entry).sum();
    assert!(
        summary_est + tail_est < whole / 2,
        "the walk stopped at the insertion"
    );
    // The insertion is itself a valid cut boundary (multi-pass cuts
    // right after the previous summary).
    assert!(valid_boundary(&branch, 7), "a compaction ends a prefix");
}

#[test]
fn a_compaction_taints_older_measurements_in_the_tail() {
    // A request that ran before the insertion measured the old
    // prefix; its total is an overcount now. Validity compares entry
    // ids (UUIDv7 time order), so the test mints explicitly ordered
    // v7-shaped ids: stale < compaction < fresh.
    let older_than = |id: &str| format!("00000000-{id}-7000-8000-000000000000");
    let stale_id = older_than("000000000001");
    let compaction_id = older_than("000000000002");
    let fresh_id = older_than("000000000003");

    let cell = Arc::new(std::sync::RwLock::new(tabit_log::ContextManager::seeded(
        vec![Message::user("q")],
    )));
    crate::lock::write(&cell).fold_turn_with_id(
        Message::assistant("the stale, measured answer"),
        stale_id.clone(),
        Usage {
            input_tokens: 90_000,
            output_tokens: 10_000,
            total_tokens: 100_000,
            ..Usage::default()
        },
    );
    // The insertion lands after (its id is strictly later), retaining
    // the stale entry in its tail.
    let cut_child = branch_of(&cell)[1].clone();
    crate::lock::write(&cell).commit_compaction(
        compaction_id,
        "the summary".to_string(),
        cut_child,
        100_000,
        Usage::default(),
    );
    let branch = read(&cell).active_branch();
    // [u, COMP, a(stale, measured 100k)] — the stale total must not
    // stand: the walk stops at the insertion, estimating the summary
    // and the retained (stale) tail.
    #[allow(clippy::indexing_slicing)] // the constructed shape
    let expected = estimate_entry(&branch[1]) + estimate_entry(&branch[2]);
    assert_eq!(context_tokens(&branch, 0), expected);

    // A younger measurement — a request on the compacted context —
    // is the newest valid number again.
    crate::lock::write(&cell).fold_turn_with_id(
        Message::assistant("measured on the compacted context"),
        fresh_id,
        Usage {
            input_tokens: 3_000,
            output_tokens: 500,
            total_tokens: 3_500,
            ..Usage::default()
        },
    );
    let branch = read(&cell).active_branch();
    assert_eq!(context_tokens(&branch, 0), 3_500);
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
        0,
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
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
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
    // The walked context is [summary] + retained tail; the head never
    // moved.
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
        (BIG_FLOOR_ROUNDS * 2) + 1,
        "the tree keeps every node — the walked path gains the insertion, nothing is deleted"
    );
}

#[tokio::test]
async fn a_violating_summarizer_is_discarded_and_the_request_retried() {
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
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
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
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
    assert!(
        !read(&cell)
            .active_branch()
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
    );
}

#[tokio::test]
async fn an_empty_summary_fails_the_pass() {
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
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
async fn an_unknown_window_skips_with_nothing_run() {
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
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
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
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
        0,
        true,
        &mut |event| events.push(event),
    ));
    assert_eq!(outcome, Outcome::Cancelled { passes: 0 });
    assert!(
        !read(&cell)
            .active_branch()
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
    );
}

#[tokio::test]
async fn a_broken_tool_call_is_the_same_violation_discarded_and_retried() {
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
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
    let cell = cell_with_dialogue(1, 100);
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
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns(
        summary_stream_turns(),
    ))
    .build();
    let config = config_with_window(40_000);
    let (outcome, events) = run_manual(&cell, &agent, &config).await;
    assert_eq!(outcome, Outcome::Skipped, "{outcome:?}");
    assert!(events.is_empty(), "no bracket opened");
    assert_eq!(
        branch_of(&cell).len(),
        BIG_FLOOR_ROUNDS * 2,
        "nothing persisted"
    );
}

#[tokio::test]
async fn a_history_far_over_the_window_compacts_in_strictly_shrinking_passes() {
    // The designed multi-pass: a 100k history on a 70k window. The
    // prefix cap (75%) limits one pass to ~52k, so pass 1 keeps a
    // ~50k tail — still over B's bound (70k − 32.8k = 37.2k) — and
    // pass 2 cuts to the floor-pinned tail and exits. Strict shrink
    // each pass; the pass cap never comes into play.
    let cell = cell_with_dialogue(10, 20_000);
    let summary = || {
        vec![
            MockStreamEvent::text("## Goal\n- pass"),
            MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(Usage::default())),
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
        matches!(&outcome, Outcome::Compacted { passes: 2, .. }),
        "{outcome:?}"
    );
    let started = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::CompactionStarted { .. }))
        .count();
    assert_eq!(started, 2);
}

#[tokio::test]
async fn a_huge_late_entry_the_cut_cannot_move_stops_the_loop_loud() {
    // The guard's designed case: a ~40k user paste so late that every
    // feasible cut keeps it in the retained tail (cutting after it
    // leaves too little tail). Pass 1 commits what it can; pass 2
    // re-cuts to the same boundary, cannot shrink, and the guard
    // fails loud with what landed standing.
    let paste = "x".repeat(160_000);
    let cell = Arc::new(std::sync::RwLock::new(tabit_log::ContextManager::seeded(
        vec![
            Message::user("first question"),
            Message::assistant("first answer"),
            Message::user("second question"),
            Message::assistant("second answer"),
            Message::user(paste),
            Message::assistant("done with the paste"),
        ],
    )));
    let summary = || {
        vec![
            MockStreamEvent::text("## Goal\n- pass"),
            MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(Usage::default())),
        ]
    };
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([
        summary(),
        summary(),
    ]))
    .build();
    let config = config_with_window(70_000);
    let (outcome, events) = run_manual(&cell, &agent, &config).await;
    // Pass 1 commits (it shrinks by the small prefix it folds); pass 2
    // re-cuts right after the insertion, retains the same paste, and
    // the estimates come back equal — the guard's `>=`. Oversized:
    // compaction happened (both passes landed), not good to continue.
    assert!(
        matches!(&outcome, Outcome::Oversized { reason, passes: 2, .. }
            if reason.contains("cannot shrink") && reason.contains("single entry")),
        "{outcome:?}"
    );
    let started = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::CompactionStarted { .. }))
        .count();
    assert_eq!(started, 2, "both passes ran and committed");
    let branch = read(&cell).active_branch();
    assert_eq!(
        branch
            .iter()
            .filter(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
            .count(),
        2,
        "what landed stands — both insertions persisted"
    );
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
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([
        length_capped_turn(),
        vec![
            MockStreamEvent::text("## Goal\n- fits now"),
            MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(Usage::default())),
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
    // Capped at the empty prefix too: there is nothing shorter to
    // send — the pass fails naming the output cap.
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([
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
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns([
        vec![MockStreamEvent::Error(
            rig_agent::test_utils::MockError::http(
                400,
                "prompt is too long: 19565 tokens > 16384 tokens maximum",
            ),
        )],
        vec![
            MockStreamEvent::text("## Goal\n- after the wall"),
            MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(Usage::default())),
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
        0,
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

#[test]
fn an_overflow_rejection_at_the_empty_prefix_fails_the_pass() {
    let state = Compaction::new();
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
    let branch = read(&cell).active_branch();
    let error =
        CompletionError::HttpError(rig_core::http_client::Error::InvalidStatusCodeWithMessage(
            http::StatusCode::BAD_REQUEST,
            "prompt is too long: 19565 tokens > 16384 tokens maximum".to_string(),
        ));
    assert!(matches!(
        rejected(error, &state, 0, &branch),
        Rejection::Fail(message) if message.contains("empty prefix")
    ));
}

#[tokio::test]
async fn the_pre_request_leaf_compacts_when_condition_b_holds() {
    // The engine awaits this leaf blindly; directly: a measured
    // context past the urgent bound runs the box through the leaf's
    // own door and emission path.
    let cell = cell_with_dialogue(BIG_FLOOR_ROUNDS, BIG_MESSAGE_CHARS);
    crate::lock::write(&cell).fold_turn_with_id(
        Message::assistant("the latest, measured answer"),
        "measured".to_string(),
        Usage {
            input_tokens: 99_000,
            output_tokens: 1_000,
            total_tokens: 100_000,
            ..Usage::default()
        },
    );
    let agent = AgentBuilder::new(MockCompletionModel::from_stream_turns(
        summary_stream_turns(),
    ))
    .build();
    let door = PreRequestDoor {
        cell: cell.clone(),
        state: Arc::new(Compaction::new()),
        agent: Arc::new(agent),
        config: config_with_window(80_000),
        selection: selection(),
        preamble_chars: 0,
        token: CancellationToken::new(),
        notice: None,
    };
    rig_agent::agent::PreRequestSource::at_door(&door).await;
    // The box ran through the leaf: the branch holds an insertion and
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
