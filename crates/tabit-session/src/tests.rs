//! Session facade tests: scripted mock models drive the full outer loop —
//! persistence, events, resume, repair — with no network and no frontend.

use crate::SessionError;
use crate::entry::EntryKind;
use crate::session::{RunSummary, SessionBuilder};
use crate::store::SessionStore;
use rig_agent::agent::ModelHandle;
use rig_agent::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_agent::tool::DynamicTool;
use rig_core::OneOrMany;
use rig_core::completion::{Message, Usage};
use rig_core::message::{AssistantContent, Text, UserContent};
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tabit_config::TabitConfig;
use tabit_protocol::{ModelSelection, SessionEvent};

pub(crate) fn temp_store(tag: &str) -> SessionStore {
    let dir = std::env::temp_dir()
        .join("tabit-session-tests")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    SessionStore::new(&dir)
}

pub(crate) fn test_config() -> Arc<tabit_config::TabitConfig> {
    Arc::new(
        tabit_config::TabitConfig::from_toml_str(
            r#"
[providers.p]
base_url = "http://127.0.0.1:9999/v1"
api = "openai-completions"

[[providers.p.models]]
id = "m"
cost = { input = 2.0, output = 4.0, cache_read = 0.2, cache_write = 2.0 }

[[providers.p.models.thinking_levels]]
name = "off"

[[providers.p.models.thinking_levels]]
name = "high"

[providers.q]
base_url = "http://127.0.0.1:9998/v1"
api = "openai-completions"

[[providers.q.models]]
id = "m2"
cost = { input = 1.0, output = 1.0, cache_read = 0.1, cache_write = 1.0 }
"#,
            Path::new("providers.toml"),
        )
        .expect("test config"),
    )
}

pub(crate) fn test_auth() -> Arc<tabit_config::AuthConfig> {
    Arc::new(
        tabit_config::AuthConfig::from_toml_str(
            r#"
[providers.p]
api_key = "dummy"

[providers.q]
api_key = "dummy"
"#,
            Path::new("auth.toml"),
        )
        .expect("test auth"),
    )
}

/// Stream-scripted turn: text chunks then the terminal record.
pub(crate) fn text_turn(text: &str) -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::text(text),
        MockStreamEvent::final_response(Usage {
            input_tokens: 100,
            output_tokens: 10,
            total_tokens: 110,
            ..Usage::default()
        }),
    ]
}

/// Stream-scripted turn: a complete tool call, then the terminal record.
pub(crate) fn tool_turn(call_id: &str, tool: &str) -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::tool_call(call_id, tool, json!({"value": "x"})),
        MockStreamEvent::final_response(Usage {
            input_tokens: 120,
            output_tokens: 5,
            total_tokens: 125,
            ..Usage::default()
        }),
    ]
}

pub(crate) fn echo_tool() -> DynamicTool {
    DynamicTool::new(
        "echo",
        "Echoes its input",
        json!({"type":"object","properties":{"value":{"type":"string"}}}),
        |_ctx, args| {
            Box::pin(async move {
                Ok(rig_agent::tool::ToolOutput::text(
                    args.get("value").and_then(|v| v.as_str()).unwrap_or(""),
                ))
            })
        },
    )
}

/// Counting factory: hands out scripted models and records which selections
/// were requested.
pub(crate) struct Factory {
    turns: Vec<Vec<MockStreamEvent>>,
    requested: Mutex<Vec<(String, String)>>,
    models: Mutex<Vec<MockCompletionModel>>,
}

impl Factory {
    pub(crate) fn new(turns: Vec<Vec<MockStreamEvent>>) -> Arc<Self> {
        Arc::new(Self {
            turns,
            requested: Mutex::new(Vec::new()),
            models: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn into_builder(self: Arc<Self>, store: SessionStore) -> SessionBuilder {
        self.into_builder_with_config(store, test_config(), ModelSelection::new("p", "m"))
    }

    /// The same scripted factory over an explicit config and selection, for
    /// tests that assert how config properties reach the request.
    pub(crate) fn into_builder_with_config(
        self: Arc<Self>,
        store: SessionStore,
        config: Arc<TabitConfig>,
        selection: ModelSelection,
    ) -> SessionBuilder {
        SessionBuilder::new(store, config, test_auth(), selection)
            .expect("builder")
            .model_factory(std::sync::Arc::new(move |provider, model| {
                if let Ok(mut guard) = self.requested.lock() {
                    guard.push((provider.to_string(), model.to_string()));
                }
                // The mock records every request it serves; clones share the
                // recording, so the test can read what the session sent.
                let mock = MockCompletionModel::from_stream_turns(self.turns.clone());
                if let Ok(mut guard) = self.models.lock() {
                    guard.push(mock.clone());
                }
                Ok(ModelHandle::new(mock))
            }))
    }

    /// The requests served by the latest model this factory handed out.
    pub(crate) fn requests(&self) -> Vec<rig_core::completion::CompletionRequest> {
        self.models
            .lock()
            .ok()
            .and_then(|guard| guard.last().cloned())
            .map(|model| model.requests())
            .unwrap_or_default()
    }
}

fn user_messages(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(
                content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

fn assistant_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_fresh_session_materializes_at_the_first_user_message() -> Result<(), SessionError> {
    let store = temp_store("lazy-create");
    let factory = Factory::new(vec![text_turn("hello")]);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;

    // Created, never run: nothing on disk (no header-only orphans), and
    // the session still knows the path it would materialize at.
    let path = session.path().to_path_buf();
    assert!(!path.exists(), "no file before the first message");
    assert!(store.list()?.is_empty(), "nothing to list yet");

    // The first run materializes the file: header, the opening model
    // selection, then the conversation.
    session.prompt("hi").await;
    let loaded = store.open_path(&path)?;
    let kinds: Vec<&str> = loaded
        .entries
        .iter()
        .map(|e| match &e.kind {
            EntryKind::ModelChange { .. } => "model",
            EntryKind::UserMessage { .. } => "user",
            EntryKind::AssistantMessage { .. } => "assistant",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, vec!["model", "user", "assistant"]);
    assert_eq!(loaded.header.id, session.id());
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn single_turn_prompt_persists_and_projects() -> Result<(), SessionError> {
    let store = temp_store("single");
    let factory = Factory::new(vec![text_turn("hello there")]);
    let mut session = factory.clone().into_builder(store.clone()).create("C:/w")?;

    let run: RunSummary = session.prompt("hi").await;
    assert_eq!(run.output, "hello there");
    assert_eq!(
        run.usage.input_tokens, 100,
        "usage aggregates from the terminal record"
    );

    // Log: initial model change, user message, assistant turn with usage.
    let loaded = store.open_path(session.path()).expect("reload");
    assert_eq!(loaded.entries.len(), 3);
    assert!(matches!(
        &loaded.entries[1].kind,
        EntryKind::UserMessage { .. }
    ));
    assert!(matches!(
        &loaded.entries[2].kind,
        EntryKind::AssistantMessage { usage, .. } if usage.input_tokens == 100
    ));

    // Events tell the whole run in order.
    assert!(matches!(&run.events[0], SessionEvent::UserMessage { text } if text == "hi"));
    assert!(matches!(
        run.events.last(),
        Some(SessionEvent::RunFinished { output, .. }) if output == "hello there"
    ));

    // Context is re-derived from the log.
    assert_eq!(user_messages(session.context()), vec!["hi"]);
    assert_eq!(assistant_texts(session.context()), vec!["hello there"]);
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn tool_roundtrip_is_recorded_and_events_name_the_tool() -> Result<(), SessionError> {
    let store = temp_store("tool");
    let factory = Factory::new(vec![tool_turn("call-1", "echo"), text_turn("did it")]);
    let mut session = factory
        .into_builder(store.clone())
        .dynamic_tool(echo_tool())
        .create("C:/w")?;

    let run = session.prompt("echo x").await;
    assert_eq!(run.output, "did it");

    let loaded = store.open_path(session.path()).expect("reload");
    let kinds: Vec<&str> = loaded
        .entries
        .iter()
        .map(|e| match &e.kind {
            EntryKind::UserMessage { .. } => "user",
            EntryKind::AssistantMessage { .. } => "assistant",
            EntryKind::ToolResult { .. } => "tool_result",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["other", "user", "assistant", "tool_result", "assistant"]
    );

    // Negative pin: an ordinary multi-turn run never warns about
    // truncation (the warning fires only on a Length-class finish).
    assert!(
        !run.events
            .iter()
            .any(|e| matches!(e, SessionEvent::TurnTruncated)),
        "a non-truncated run must not emit turn_truncated"
    );

    let tool_call_events: Vec<&str> = run
        .events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolCall { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_call_events, vec!["echo"]);
    let result_events: Vec<&str> = run
        .events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolResult { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(result_events, vec!["echo"]);

    // Projection merges the result into one user message.
    assert_eq!(session.context().len(), 4);
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

/// A turn that ends truncated (`finish_reason: length`) warns the frontend
/// and the run completes normally — informational, not a failure (ENGINE.md
/// behavior delta 9).
#[tokio::test]
async fn truncated_turn_warns_and_the_run_still_completes() -> Result<(), SessionError> {
    let store = temp_store("truncation-warning");
    let factory = Factory::new(vec![vec![
        MockStreamEvent::text("partial answer"),
        MockStreamEvent::FinalResponse(
            rig_agent::test_utils::mock_final(Usage {
                input_tokens: 100,
                output_tokens: 10,
                total_tokens: 110,
                ..Usage::default()
            })
            .with_finish_reason(rig_core::completion::FinishReason::Length),
        ),
    ]]);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;

    let run = session.prompt("go deep").await;

    assert_eq!(run.output, "partial answer");
    assert_eq!(run.outcome, crate::session::RunOutcome::Completed);
    assert_eq!(
        run.events
            .iter()
            .filter(|e| matches!(e, SessionEvent::TurnTruncated))
            .count(),
        1,
        "exactly one truncation warning for one truncated turn"
    );
    assert!(matches!(
        run.events.last(),
        Some(SessionEvent::RunFinished { output, .. }) if output == "partial answer"
    ));
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn resumed_reflects_create_vs_resume() -> Result<(), SessionError> {
    // The handshake reports this so a frontend that asked to resume
    // can note a silent fresh start (the pinned startup contract).
    let store = temp_store("resumed-flag");
    let factory = Factory::new(vec![text_turn("a")]);
    let mut first = factory.into_builder(store.clone()).create("C:/w")?;
    assert!(!first.resumed(), "a created session is fresh");
    first.prompt("hi").await;
    let path = first.path().to_path_buf();
    drop(first);

    let (second, _report) = Factory::new(vec![text_turn("b")])
        .into_builder(store)
        .resume(&path)?;
    assert!(second.resumed(), "a resumed session continues a chain");
    Ok(())
}

#[tokio::test]
async fn resume_continues_the_log_and_reports_the_model() -> Result<(), SessionError> {
    let store = temp_store("resume");
    let factory = Factory::new(vec![text_turn("one"), text_turn("two")]);
    let mut first = factory.into_builder(store.clone()).create("C:/w")?;
    first.prompt("first question").await;
    let path = first.path().to_path_buf();
    drop(first);

    let (second, report) = Factory::new(vec![text_turn("two")])
        .into_builder(store.clone())
        .resume(&path)
        .expect("resume");
    assert_eq!(report.repaired_tool_calls, 0);
    assert_eq!(
        report
            .resumed_model
            .as_ref()
            .map(|m| (m.provider.as_str(), m.model.as_str())),
        Some(("p", "m")),
        "the initial selection is recorded at create and restored on resume"
    );
    assert_eq!(
        user_messages(second.context()),
        vec!["first question"],
        "history replays from the log"
    );

    let mut second = second;
    let run = second.prompt("second question").await;
    assert_eq!(run.output, "two");

    let loaded = store.open_path(&path).expect("reload");
    assert_eq!(
        loaded.entries.len(),
        5,
        "initial model change + two runs of (user, assistant)"
    );
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn dangling_tool_roundtrip_is_repaired_on_resume() -> Result<(), SessionError> {
    let store = temp_store("dangling");
    // Hand-write a log that ends mid tool-use roundtrip: the process died
    // after the assistant called a tool, before any result existed.
    let mut writer = store.create("C:/w");
    writer
        .append(EntryKind::UserMessage {
            message: Message::User {
                content: OneOrMany::one(UserContent::Text(Text::new("go"))),
            },
        })
        .expect("user");
    writer
        .append(EntryKind::AssistantMessage {
            message: Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::ToolCall(
                    rig_core::message::ToolCall::new(
                        "c1".to_string(),
                        rig_core::message::ToolFunction::new("echo".to_string(), json!({})),
                    ),
                )),
            },
            usage: Usage::default(),
        })
        .expect("assistant");
    let path = writer.path().to_path_buf();

    let (session, report) = Factory::new(vec![text_turn("recovered")])
        .into_builder(store.clone())
        .resume(&path)
        .expect("resume");
    assert_eq!(report.repaired_tool_calls, 1);
    assert_eq!(
        session.context().len(),
        3,
        "user, dangling assistant, synthesized results"
    );

    // The repair is durable: the file now carries the synthesized result.
    let loaded = store.open_path(&path).expect("reload");
    assert!(matches!(
        loaded.entries.last().map(|e| &e.kind),
        Some(EntryKind::ToolResult { .. })
    ));

    let mut session = session;
    let run = session.prompt("continue").await;
    assert_eq!(run.output, "recovered");
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn failed_run_still_records_the_user_message() -> Result<(), SessionError> {
    let store = temp_store("failed");
    let turns = vec![vec![MockStreamEvent::error("boom")]];
    let factory = Factory::new(turns);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;

    let run = session.prompt("doomed").await;
    assert_eq!(run.outcome, crate::session::RunOutcome::Failed);
    assert!(
        run.events.iter().any(|e| matches!(
            e,
            SessionEvent::RunFailed { message } if message.contains("boom")
        )),
        "the mock provider error surfaces as an event: {:?}",
        run.events
    );

    let loaded = store.open_path(session.path()).expect("reload");
    assert!(matches!(
        loaded.entries.get(1).map(|e| &e.kind),
        Some(EntryKind::UserMessage { .. })
    ));
    assert_eq!(
        loaded.entries.len(),
        2,
        "initial model change + the user message; no assistant record for a          failed run"
    );
    // The context still contains the user message for the next attempt.
    assert_eq!(user_messages(session.context()), vec!["doomed"]);
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn malformed_tool_call_exhaustion_fails_the_run_and_leaves_the_session_alive()
-> Result<(), SessionError> {
    use rig_agent::test_utils::MockError;

    let store = temp_store("malformed-exhaustion");
    let malformed = || {
        vec![MockStreamEvent::Error(MockError::malformed_tool_call(
            "lookup",
            "arguments arrived truncated",
        ))]
    };
    let factory = Factory::new(vec![malformed(), malformed(), text_turn("recovered")]);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;

    let run = session.prompt("doomed").await;
    assert_eq!(run.outcome, crate::session::RunOutcome::Failed);
    assert!(
        run.events.iter().any(|e| matches!(
            e,
            SessionEvent::RunFailed { message }
                if message.contains("repeatedly emitted tool calls with malformed arguments")
        )),
        "the exhaustion surfaces with its actionable message: {:?}",
        run.events
    );

    // The defective turns never entered history: only the user message and
    // the initial model change are on disk, and the next message runs.
    let loaded = store.open_path(session.path()).expect("reload");
    assert_eq!(
        loaded.entries.len(),
        2,
        "model change + the user message; the discarded turns recorded nothing"
    );
    let run = session.prompt("try again").await;
    assert_eq!(run.output, "recovered");
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn set_model_records_the_change_and_splits_stats() -> Result<(), SessionError> {
    let store = temp_store("switch");
    let factory = Factory::new(vec![text_turn("a"), text_turn("b")]);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;
    session.prompt("one").await;

    session
        .set_model(ModelSelection::new("q", "m2"))
        .expect("switch");
    session.prompt("two").await;

    assert_eq!(session.selection().provider, "q");

    let loaded = store.open_path(session.path()).expect("reload");
    let changes: Vec<_> = loaded
        .entries
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::ModelChange { .. }))
        .collect();
    assert_eq!(changes.len(), 2, "initial selection + the switch");

    let stats = session.stats().expect("stats");
    assert_eq!(stats.per_model.len(), 2, "usage splits per model");
    // p: 100 in / 10 out at $2/$4 per million => 0.00024
    let p_cost = stats.per_model[0].cost.expect("p has rates");
    assert!((p_cost - 0.00024).abs() < 1e-12, "p cost {p_cost}");
    // q: 100 in / 10 out at $1/$1 per million => 0.00011
    let q_cost = stats.per_model[1].cost.expect("q has rates");
    assert!((q_cost - 0.00011).abs() < 1e-12, "q cost {q_cost}");
    assert!((stats.total_cost - 0.00035).abs() < 1e-12);
    assert_eq!(stats.total_usage.input_tokens, 200);

    // The factory saw both selections.
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn resume_uses_the_builder_selection_and_records_the_switch() -> Result<(), SessionError> {
    let store = temp_store("resume-model");
    let factory = Factory::new(vec![text_turn("a")]);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;
    session.prompt("one").await;
    session
        .set_model(ModelSelection::new("q", "m2"))
        .expect("switch");
    let path = session.path().to_path_buf();
    drop(session);

    let requested = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let sink = requested.clone();
    let builder = SessionBuilder::new(
        store.clone(),
        test_config(),
        test_auth(),
        ModelSelection::new("p", "m"),
    )
    .expect("builder")
    .model_factory(std::sync::Arc::new(move |provider, model| {
        if let Ok(mut guard) = sink.lock() {
            guard.push((provider.to_string(), model.to_string()));
        }
        Ok(ModelHandle::new(MockCompletionModel::from_stream_turns(
            vec![text_turn("b")],
        )))
    }));
    let (session, report) = builder.resume(&path).expect("resume");
    // The report still says what the log last used...
    let resumed = report.resumed_model.expect("log carried the switch");
    assert_eq!(
        (resumed.provider.as_str(), resumed.model.as_str()),
        ("q", "m2")
    );
    // ...but the builder's selection (the caller's explicit choice, or
    // the registry-resolved default) is what continues.
    assert_eq!(session.selection().provider, "p");
    assert_eq!(
        requested.lock().expect("sink").as_slice(),
        [("p".to_string(), "m".to_string())]
    );
    // The switch is durable: the log records p/m as the model in effect.
    let loaded = store.open_path(&path).expect("reload");
    let last = crate::projection::last_model_change(&loaded.entries).expect("recorded");
    assert_eq!(last, ("p", "m", None));
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn selection_errors_are_loud_at_builder_time() -> Result<(), SessionError> {
    let store = temp_store("bad-selection");
    let result = SessionBuilder::new(
        store.clone(),
        test_config(),
        test_auth(),
        ModelSelection::new("missing", "model"),
    );
    match result {
        Err(SessionError::Config { message }) => {
            assert!(message.contains("provider `missing`"), "{message}")
        }
        other => panic!("expected config error, got {}", other.is_err()),
    }
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn builder_options_reach_the_request_and_the_budget_enforces() -> Result<(), SessionError> {
    let store = temp_store("builder");
    let mut session = Factory::new(vec![text_turn("ok")])
        .into_builder(store.clone())
        .preamble("you are a test agent")
        .max_turns(1)
        .create("C:/w")?;
    assert!(!session.id().is_empty(), "session exposes its id");

    // The preamble rides on the outgoing request.
    let run = session.prompt("hi").await;
    assert_eq!(run.output, "ok");

    // max_turns(1) means a follow-up outer loop with a tool turn cannot
    // get its second model call: the budget error surfaces loudly.
    let mut budgeted = Factory::new(vec![tool_turn("c1", "echo"), text_turn("never")])
        .into_builder(store.clone())
        .max_turns(1)
        .dynamic_tool(echo_tool())
        .create("C:/w")?;
    let run = budgeted.prompt("go").await;
    assert_eq!(run.outcome, crate::session::RunOutcome::Failed);
    let failure = run
        .events
        .iter()
        .find_map(|e| match e {
            SessionEvent::RunFailed { message } => Some(message.clone()),
            _ => None,
        })
        .expect("a run_failed event");
    assert!(
        failure.to_lowercase().contains("max"),
        "expected a max-turns failure, got: {failure}"
    );
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn reasoning_deltas_surface_as_events() -> Result<(), SessionError> {
    let store = temp_store("reasoning");
    let reasoning_turn = vec![
        MockStreamEvent::reasoning_delta_with_id("r0", "pondering..."),
        MockStreamEvent::text("the answer"),
        MockStreamEvent::final_response(Usage::default()),
    ];
    let mut session = Factory::new(vec![reasoning_turn])
        .into_builder(store.clone())
        .create("C:/w")?;
    let run = session.prompt("deep question").await;
    assert!(
        run.events.iter().any(|e| matches!(
            e,
            SessionEvent::ReasoningDelta { id, reasoning }
                if id == "r0" && reasoning == "pondering..."
        )),
        "reasoning delta must surface: {:?}",
        run.events
    );
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn repeated_runs_under_one_model_share_a_stats_slot() -> Result<(), SessionError> {
    let store = temp_store("stats-slot");
    let factory = Factory::new(vec![text_turn("a"), text_turn("b")]);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;
    session.prompt("one").await;
    session.prompt("two").await;

    let stats = session.stats().expect("stats");
    assert_eq!(stats.per_model.len(), 1, "same model, one slot");
    assert_eq!(stats.per_model[0].usage.input_tokens, 200);
    assert_eq!(
        stats.per_model[0].key(),
        "p/m",
        "display key is provider/model"
    );
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn persistence_failure_fails_the_run_loudly() -> Result<(), SessionError> {
    let store = temp_store("persist-fail");
    // A tool that deletes the session log mid-run: the next record the
    // recorder tries to append must fail, and prompt() must surface it
    // instead of returning a success the disk does not back.
    let session_dir = store.dir().to_path_buf();
    let destroyer = DynamicTool::new(
        "selfdestruct",
        "Deletes the session log",
        serde_json::Value::Object(serde_json::Map::new()),
        move |_ctx, _args| {
            let dir = session_dir.clone();
            Box::pin(async move {
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                            std::fs::remove_file(&path).ok();
                        }
                    }
                }
                Ok(rig_agent::tool::ToolOutput::text("log deleted"))
            })
        },
    );
    let mut session = Factory::new(vec![tool_turn("c1", "selfdestruct"), text_turn("done")])
        .into_builder(store.clone())
        .dynamic_tool(destroyer)
        .create("C:/w")?;

    let run = session.prompt("destroy the log").await;
    // Which failure fires first is platform-dependent (on Windows the
    // writer keeps writing to the unlinked handle, so the reload read
    // fails before the recorder does); either way the run must fail
    // loudly instead of reporting success the disk does not back.
    assert_eq!(
        run.outcome,
        crate::session::RunOutcome::Failed,
        "run must not succeed with an unwritable log: {run:?}"
    );
    assert!(
        run.events
            .iter()
            .any(|e| matches!(e, SessionEvent::RunFailed { .. })),
        "the failure is visible as an event: {:?}",
        run.events
    );
    let _ = std::fs::remove_dir_all(store.dir());
    Ok(())
}

#[tokio::test]
async fn thinking_level_changes_are_validated_and_recorded() -> Result<(), SessionError> {
    let store = temp_store("level");
    let mut session = Factory::new(vec![text_turn("a")])
        .into_builder(store.clone())
        .create("C:/w")?;
    session
        .set_thinking_level(Some("high"))
        .expect("defined level switches");
    assert_eq!(session.selection().thinking_level.as_deref(), Some("high"));
    session.set_thinking_level(None).expect("clearing works");
    assert_eq!(session.selection().thinking_level, None);
    match session.set_thinking_level(Some("maximum")) {
        Err(SessionError::Config { message }) => {
            assert!(message.contains("`maximum`"), "{message}")
        }
        other => panic!("expected config error, got {:?}", other.err()),
    }
    // Every accepted switch left a model_change entry in the log.
    let loaded = store.open_path(session.path())?;
    let changes = loaded
        .entries
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::ModelChange { .. }))
        .count();
    assert_eq!(changes, 3, "initial + two switches");
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn abort_mid_run_records_marker_and_stays_resumable() -> Result<(), SessionError> {
    let store = temp_store("abort");
    let factory = Factory::new(vec![text_turn("never reached")]);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;
    let abort = session.abort_handle();
    let mut armed = false;
    let summary = session
        .prompt_with("hello", &mut |event| {
            // Abort as soon as the run is in flight — the token is
            // per-run, so cancelling must happen after it starts.
            if !armed {
                armed = true;
                abort.abort();
            }
            let _ = event;
        })
        .await;
    assert_eq!(summary.outcome, crate::session::RunOutcome::Aborted);

    // The log carries the prompt, the abort marker, and nothing partial.
    let loaded = store.open_path(session.path())?;
    let kinds: Vec<&str> = loaded
        .entries
        .iter()
        .map(|e| match &e.kind {
            EntryKind::UserMessage { .. } => "user",
            EntryKind::Aborted => "aborted",
            EntryKind::AssistantMessage { .. } => "assistant",
            EntryKind::ModelChange { .. } => "model",
            _ => "other",
        })
        .collect();
    // `create` records the opening model_change; the aborted run adds the
    // prompt and the marker, nothing partial.
    assert_eq!(kinds, vec!["model", "user", "aborted"]);

    // A fresh token lets the next run proceed normally.
    let run = session.prompt("again").await;
    assert_eq!(run.outcome, crate::session::RunOutcome::Completed);
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn steering_during_a_run_is_recorded_one_to_one() -> Result<(), SessionError> {
    let store = temp_store("steer");
    let factory = Factory::new(vec![tool_turn("t1", "echo"), text_turn("done after steer")]);
    let mut session = factory
        .into_builder(store.clone())
        .dynamic_tool(echo_tool())
        .create("C:/w")?;
    let mailbox = session.mailbox_handle();

    let mut seen_call = false;
    let run = session
        .prompt_with("run the tool", &mut |event| {
            // Submit while the run is in flight: the turn-end drain
            // delivers it right after the tool results commit.
            if matches!(event, SessionEvent::ToolCall { .. }) && !seen_call {
                seen_call = true;
                mailbox.submit("also this");
            }
        })
        .await;
    assert_eq!(run.outcome, crate::session::RunOutcome::Completed);

    // One steer: one event, one entry, and the replayed context carries it
    // after the tool results.
    assert_eq!(
        run.events
            .iter()
            .filter(|e| matches!(e, SessionEvent::UserMessage { text } if text == "also this"))
            .count(),
        1
    );
    let loaded = store.open_path(session.path())?;
    let steers: Vec<&crate::SessionEntry> = loaded
        .entries
        .iter()
        .filter(|e| matches!(&e.kind, EntryKind::UserMessage { message } if message.user_text().as_deref() == Some("also this")))
        .collect();
    assert_eq!(steers.len(), 1);
    let texts: Vec<Option<String>> = session.context().iter().map(|m| m.user_text()).collect();
    let tool_results_at = texts
        .iter()
        .position(|t| t.is_none())
        .expect("tool-results message in context");
    let steer_at = texts
        .iter()
        .position(|t| t.as_deref() == Some("also this"))
        .expect("steer in context");
    assert!(steer_at > tool_results_at, "steer follows the results");
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn messages_queued_before_pump_all_join_the_first_run() -> Result<(), SessionError> {
    // Drain-all at idle entry: both messages become the run's opening
    // input — one entry each, before any deltas — so the run is a single
    // turn answering them together. (A message arriving after a run ends
    // starts the next run instead.)
    let store = temp_store("mailbox-serial");
    let factory = Factory::new(vec![text_turn("first answer")]);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;
    session.submit("one");
    session.submit("two");

    let run = session.pump(&mut |_| {}).await;
    let user_texts: Vec<&str> = run
        .events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::UserMessage { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(user_texts, vec!["one", "two"]);
    // The whole batch opens the run: no delta precedes the last message.
    let last_user = run
        .events
        .iter()
        .rposition(|e| matches!(e, SessionEvent::UserMessage { .. }))
        .expect("user messages");
    let first_delta = run
        .events
        .iter()
        .position(|e| matches!(e, SessionEvent::TextDelta { .. }))
        .expect("deltas");
    assert!(last_user < first_delta, "the batch precedes the run");
    assert_eq!(run.output, "first answer");
    assert_eq!(
        run.events
            .iter()
            .filter(|e| matches!(e, SessionEvent::RunFinished { .. }))
            .count(),
        1,
        "one run carries the whole batch"
    );
    // One entry per message: the log keeps 1:1 fidelity.
    let loaded = store.open_path(session.path())?;
    assert_eq!(loaded.entries.len(), 4, "model + two users + assistant");
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn a_message_landing_after_the_last_drain_runs_as_the_next_prompt() -> Result<(), SessionError>
{
    // The no-lost-messages invariant: a message submitted after the
    // engine's final steer drain but before the run fully ends — staged
    // on RunFinished, which is emitted inside the run after the engine
    // stream is done — must surface as a new run, not vanish.
    let store = temp_store("mailbox-tail");
    let factory = Factory::new(vec![text_turn("a"), text_turn("b")]);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;
    let mailbox = session.mailbox_handle();
    session.submit("first");
    let mut finished = 0;
    session
        .pump(&mut |event| {
            if matches!(event, SessionEvent::RunFinished { .. }) {
                finished += 1;
                if finished == 1 {
                    mailbox.submit("arrived too late to steer");
                }
            }
        })
        .await;
    assert_eq!(finished, 2, "the late message ran as the next prompt");
    let loaded = store.open_path(session.path())?;
    let late: Vec<&crate::SessionEntry> = loaded
        .entries
        .iter()
        .filter(|e| {
            matches!(&e.kind, EntryKind::UserMessage { message } if message.user_text().as_deref() == Some("arrived too late to steer"))
        })
        .collect();
    assert_eq!(late.len(), 1, "the late message was recorded once");
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn abort_discards_messages_queued_behind_the_run() -> Result<(), SessionError> {
    let store = temp_store("mailbox-abort");
    let factory = Factory::new(vec![tool_turn("t1", "echo"), text_turn("never")]);
    let mut session = factory
        .into_builder(store.clone())
        .dynamic_tool(echo_tool())
        .create("C:/w")?;
    let mailbox = session.mailbox_handle();
    let abort = session.abort_handle();
    session.submit("run the tool");
    let mut saw_aborted = false;
    let mut queued_user_messages = 0;
    session
        .pump(&mut |event| match event {
            SessionEvent::ToolCall { .. } => {
                // Queued behind the run, then stopped: abort discards the
                // queue (nothing queued was ever acknowledged).
                mailbox.submit("queued behind");
                abort.abort();
            }
            SessionEvent::RunAborted { .. } => saw_aborted = true,
            SessionEvent::UserMessage { text } if text != "run the tool" => {
                queued_user_messages += 1;
            }
            _ => {}
        })
        .await;
    assert!(saw_aborted);
    assert_eq!(queued_user_messages, 0, "queued messages were discarded");
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn pump_continues_with_the_next_message_after_a_failed_run() -> Result<(), SessionError> {
    // max_turns(1) makes the first message's run fail (a tool turn needs
    // a second model call); a message submitted after the failure still
    // runs — one failed prompt does not strand the session.
    let store = temp_store("mailbox-failure");
    let mut session = Factory::new(vec![tool_turn("c1", "echo"), text_turn("recovered")])
        .into_builder(store.clone())
        .max_turns(1)
        .dynamic_tool(echo_tool())
        .create("C:/w")?;
    let mailbox = session.mailbox_handle();
    session.submit("will fail");
    let mut failures = 0;
    let mut outputs = Vec::new();
    session
        .pump(&mut |event| match event {
            SessionEvent::RunFailed { .. } => {
                failures += 1;
                mailbox.submit("still runs");
            }
            SessionEvent::RunFinished { output, .. } => outputs.push(output),
            _ => {}
        })
        .await;
    assert_eq!(failures, 1);
    assert_eq!(outputs, vec!["recovered"]);
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn abort_while_idle_does_nothing() -> Result<(), SessionError> {
    // Each run mints a fresh token before any observable activity, so a
    // stray cancel between runs (or before the first) hits a dead token.
    let store = temp_store("abort-idle");
    let factory = Factory::new(vec![text_turn("fine")]);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;
    session.abort_handle().abort();
    session.abort_handle().abort();
    let run = session.prompt("hello").await;
    assert_eq!(run.outcome, crate::session::RunOutcome::Completed);
    assert_eq!(run.output, "fine");
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn rewind_drops_the_last_turn_and_branches_the_next_prompt() -> Result<(), SessionError> {
    let store = temp_store("rewind");
    let factory = Factory::new(vec![
        text_turn("first answer"),
        text_turn("second answer"),
        text_turn("revised answer"),
    ]);
    let mut session = factory.clone().into_builder(store.clone()).create("C:/w")?;
    session.prompt("question one").await;
    session.prompt("question two").await;

    let rewind = session.rewind(1).expect("rewind");
    assert_eq!(rewind.dropped, 1);
    assert_eq!(
        user_messages(session.context()),
        vec!["question one"],
        "the second turn left the chain"
    );

    // The marker is the last line; the dropped entries stay in the file,
    // off-chain but present.
    let loaded = store.open_path(session.path()).expect("reload");
    assert!(matches!(
        loaded.entries.last().map(|e| &e.kind),
        Some(EntryKind::Rewound { .. })
    ));
    assert_eq!(loaded.entries.len(), 6, "nothing was deleted");
    assert_eq!(loaded.chain.len(), 3, "model change + first turn only");
    let first_answer_id = loaded.entries[2].id.clone();

    // The next prompt branches from the branch point.
    session.prompt("question two, revised").await;
    let loaded = store
        .open_path(session.path())
        .expect("reload after branch");
    let branched = loaded
        .entries
        .iter()
        .rev()
        .find(|entry| matches!(entry.kind, EntryKind::UserMessage { .. }))
        .expect("the new user message");
    assert_eq!(
        branched.parent_id.as_deref(),
        Some(first_answer_id.as_str()),
        "the branch attaches before the dropped turn"
    );
    assert_eq!(
        user_messages(session.context()),
        vec!["question one", "question two, revised"]
    );
    assert_eq!(
        assistant_texts(session.context()),
        vec!["first answer", "revised answer"]
    );
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn rewind_rejects_zero_and_more_than_the_chain_holds() -> Result<(), SessionError> {
    let store = temp_store("rewind-errors");
    let factory = Factory::new(vec![text_turn("answer")]);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;
    session.prompt("only question").await;

    let zero = session.rewind(0).expect_err("zero is not a rewind");
    assert!(zero.to_string().contains("at least 1"), "{zero}");
    let too_far = session.rewind(2).expect_err("only one message to drop");
    assert!(too_far.to_string().contains("holds 1"), "{too_far}");

    // Nothing was written by the failed attempts.
    let loaded = store.open_path(session.path()).expect("reload");
    assert!(matches!(
        loaded.entries.last().map(|e| &e.kind),
        Some(EntryKind::AssistantMessage { .. })
    ));
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn rewind_past_a_model_switch_adopts_the_chains_model() -> Result<(), SessionError> {
    let store = temp_store("rewind-model");
    let factory = Factory::new(vec![text_turn("answer one"), text_turn("answer two")]);
    let mut session = factory.clone().into_builder(store.clone()).create("C:/w")?;
    session.prompt("question one").await;
    session.prompt("question two").await;
    // The switch lands after the last prompt, so rewinding one message
    // drops it with the turn.
    session
        .set_model(ModelSelection::new("q", "m2"))
        .expect("switch");
    assert_eq!(session.selection().model, "m2");

    // The chain's model is p/m again; the session adopts it without
    // recording a duplicate change — the chain already says it.
    session.rewind(1).expect("rewind");
    assert_eq!(session.selection().model, "m");
    let requested = factory.requested.lock().expect("requested").clone();
    assert_eq!(
        requested,
        vec![
            ("p".to_string(), "m".to_string()),
            ("q".to_string(), "m2".to_string()),
            ("p".to_string(), "m".to_string()), // the rewind rebuild
        ]
    );
    let loaded = store.open_path(session.path()).expect("reload");
    let model_changes = loaded
        .chain
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::ModelChange { .. }))
        .count();
    assert_eq!(model_changes, 1, "the switch left the chain with the turn");
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn promptless_rewind_survives_reopen() -> Result<(), SessionError> {
    let store = temp_store("rewind-reopen");
    let factory = Factory::new(vec![text_turn("a"), text_turn("b")]);
    let path = {
        let mut session = factory.into_builder(store.clone()).create("C:/w")?;
        session.prompt("question one").await;
        session.prompt("question two").await;
        session.rewind(1).expect("rewind");
        session.path().to_path_buf()
    };

    let (session, _report) = Factory::new(vec![text_turn("continued")])
        .into_builder(store.clone())
        .resume(&path)
        .expect("resume");
    assert_eq!(
        user_messages(session.context()),
        vec!["question one"],
        "the marker alone carried the rewind"
    );

    let mut session = session;
    session.prompt("question two, again").await;
    let loaded = store.open_path(&path).expect("reload");
    let chain_texts = user_messages_from_entries(&loaded.chain);
    assert_eq!(chain_texts, vec!["question one", "question two, again"]);
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

fn user_messages_from_entries(entries: &[crate::entry::SessionEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::UserMessage {
                message: Message::User { content },
            } => Some(
                content
                    .iter()
                    .filter_map(|part| match part {
                        UserContent::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn rewind_targets_steers_like_prompts() -> Result<(), SessionError> {
    let store = temp_store("rewind-steer");
    // Hand-written log whose last user message is a mid-run steer: a
    // rewind of one message drops the steer — "un-send it".
    let mut writer = store.create("C:/w");
    writer
        .append(EntryKind::UserMessage {
            message: Message::user("question"),
        })
        .expect("user");
    writer
        .append(EntryKind::AssistantMessage {
            message: Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::text("answer")),
            },
            usage: Usage::default(),
        })
        .expect("assistant");
    writer
        .append(EntryKind::UserMessage {
            message: Message::user("actually, do it differently"),
        })
        .expect("steer");
    let path = writer.path().to_path_buf();

    let (mut session, _report) = Factory::new(vec![text_turn("ok")])
        .into_builder(store.clone())
        .resume(&path)
        .expect("resume");
    let rewind = session.rewind(1).expect("rewind");
    assert_eq!(rewind.dropped, 1);
    assert_eq!(
        user_messages(session.context()),
        vec!["question"],
        "the steer was dropped like any user message"
    );
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn rewinding_mid_batch_repairs_only_the_unanswered_call() -> Result<(), SessionError> {
    let store = temp_store("rewind-midbatch");
    // Hand-written log with a complete two-call roundtrip; rewinding to
    // the FIRST result entry branches mid-batch — the second call dangles
    // and must be repaired on the new chain.
    let mut writer = store.create("C:/w");
    writer
        .append(EntryKind::UserMessage {
            message: Message::user("go"),
        })
        .expect("user");
    writer
        .append(EntryKind::AssistantMessage {
            message: Message::Assistant {
                id: None,
                content: OneOrMany::many(vec![
                    AssistantContent::ToolCall(rig_core::message::ToolCall::new(
                        "c1".to_string(),
                        rig_core::message::ToolFunction::new("echo".to_string(), json!({})),
                    )),
                    AssistantContent::ToolCall(rig_core::message::ToolCall::new(
                        "c2".to_string(),
                        rig_core::message::ToolFunction::new("echo".to_string(), json!({})),
                    )),
                ])
                .expect("two calls"),
            },
            usage: Usage::default(),
        })
        .expect("assistant");
    let first_result = writer
        .append(EntryKind::ToolResult {
            result: rig_core::message::ToolResult {
                id: "c1".to_string(),
                call_id: None,
                content: OneOrMany::one(rig_core::message::ToolResultContent::text("one")),
            },
        })
        .expect("first result");
    writer
        .append(EntryKind::ToolResult {
            result: rig_core::message::ToolResult {
                id: "c2".to_string(),
                call_id: None,
                content: OneOrMany::one(rig_core::message::ToolResultContent::text("two")),
            },
        })
        .expect("second result");
    let path = writer.path().to_path_buf();

    let (mut session, _report) = Factory::new(vec![text_turn("ok")])
        .into_builder(store.clone())
        .resume(&path)
        .expect("resume");

    let rewind = session
        .rewind_to_entry(&first_result.id)
        .expect("rewind to the first result");
    assert_eq!(rewind.to_entry, first_result.id);

    // The new chain: user, assistant, one user message carrying the real
    // c1 result plus the synthesized c2 result — balanced again.
    let context = session.context();
    assert_eq!(context.len(), 3);
    let Message::User { content } = &context[2] else {
        panic!("the trailing batch is one user message");
    };
    assert_eq!(content.len(), 2, "the real result and the repair");

    let loaded = store.open_path(&path).expect("reload");
    let repair = loaded
        .entries
        .iter()
        .rev()
        .find(|entry| matches!(&entry.kind, EntryKind::ToolResult { result } if result.id == "c2"))
        .expect("the synthesized repair");
    assert_eq!(
        repair.parent_id.as_deref(),
        Some(first_result.id.as_str()),
        "the repair lands on the new chain"
    );
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn rewinding_to_an_unknown_entry_changes_nothing() -> Result<(), SessionError> {
    let store = temp_store("rewind-unknown");
    let factory = Factory::new(vec![text_turn("answer")]);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;
    session.prompt("question").await;

    let error = session
        .rewind_to_entry("no-such-entry")
        .expect_err("unknown");
    assert!(error.to_string().contains("no entry"), "{error}");
    let loaded = store.open_path(session.path()).expect("reload");
    assert_eq!(loaded.entries.len(), 3, "nothing was written");
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn rewind_to_the_root_records_the_current_model() -> Result<(), SessionError> {
    let store = temp_store("rewind-root");
    // Hand-written log whose first entry is a user message with no
    // parent (create always records a model change first, so only a
    // hand-written log reaches a root branch).
    let mut writer = store.create("C:/w");
    writer
        .append(EntryKind::UserMessage {
            message: Message::user("question"),
        })
        .expect("user");
    writer
        .append(EntryKind::AssistantMessage {
            message: Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::text("answer")),
            },
            usage: Usage::default(),
        })
        .expect("assistant");
    let path = writer.path().to_path_buf();
    let mut session = Factory::new(vec![text_turn("fresh answer"), text_turn("next")])
        .into_builder(store.clone())
        .resume(&path)
        .expect("resume")
        .0;

    // Branch from the root: the chain empties, so the current selection
    // becomes durable at the new tip, exactly like resume on a bare log.
    let rewind = session.rewind(1).expect("rewind");
    assert_eq!(rewind.to_entry, "");
    assert!(session.context().is_empty());
    let loaded = store.open_path(&path).expect("reload");
    assert!(matches!(
        loaded.entries.last().map(|e| &e.kind),
        Some(EntryKind::ModelChange { model, .. }) if model == "m"
    ));
    assert_eq!(loaded.chain.len(), 1);

    // The session is usable from the emptied chain.
    session.prompt("fresh start").await;
    assert_eq!(user_messages(session.context()), vec!["fresh start"]);
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

#[tokio::test]
async fn rewind_fails_loudly_when_the_chains_model_left_the_config() -> Result<(), SessionError> {
    let store = temp_store("rewind-ghost-model");
    // The chain's only model change names a provider the config no longer
    // carries; adopting it must fail before anything is written.
    let mut writer = store.create("C:/w");
    writer
        .append(EntryKind::ModelChange {
            provider: "ghost".to_string(),
            model: "gone".to_string(),
            thinking_level: None,
        })
        .expect("model change");
    writer
        .append(EntryKind::UserMessage {
            message: Message::user("question"),
        })
        .expect("user");
    writer
        .append(EntryKind::AssistantMessage {
            message: Message::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::text("answer")),
            },
            usage: Usage::default(),
        })
        .expect("assistant");
    let path = writer.path().to_path_buf();
    let entries_before = store.open_path(&path).expect("reload").entries.len();

    let (mut session, _report) = Factory::new(vec![text_turn("ok")])
        .into_builder(store.clone())
        .resume(&path)
        .expect("resume");
    let error = session.rewind(1).expect_err("ghost model");
    assert!(error.to_string().contains("ghost"), "{error}");
    let loaded = store.open_path(&path).expect("reload");
    // Resume itself appended its model change; the failed rewind added
    // nothing on top.
    assert_eq!(loaded.entries.len(), entries_before + 1);
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}

/// Configured request parameters are pure forwarding: the model's
/// `max_tokens` and sampling knobs, and the `extra_body` chain (provider →
/// model → active thinking level, later sources win), reach the request the
/// model serves.
#[tokio::test]
async fn configured_request_parameters_reach_the_model() -> Result<(), SessionError> {
    let store = temp_store("params-forwarding");
    let config = Arc::new(
        TabitConfig::from_toml_str(
            r#"
[providers.p]
base_url = "http://127.0.0.1:9999/v1"
api = "openai-completions"
extra_body = { shared = "provider", only_provider = true }

[[providers.p.models]]
id = "m"
max_tokens = 512
sampling_params = { temperature = 0.7, top_p = 0.9, top_k = 40 }
extra_body = { shared = "model", model_only = true }

[[providers.p.models.thinking_levels]]
name = "high"
extra_body = { shared = "level" }
"#,
            Path::new("providers.toml"),
        )
        .expect("config"),
    );

    let factory = Factory::new(vec![text_turn("done")]);
    let selection = ModelSelection {
        provider: "p".to_string(),
        model: "m".to_string(),
        thinking_level: Some("high".to_string()),
    };
    let mut session = factory
        .clone()
        .into_builder_with_config(store.clone(), config, selection)
        .create("C:/w")?;

    let run = session.prompt("hi").await;
    assert_eq!(run.output, "done");

    let requests = factory.requests();
    let request = requests.first().expect("the mock served a request");
    assert_eq!(request.temperature, Some(0.7));
    assert_eq!(request.max_tokens, Some(512));
    let additional = request
        .additional_params
        .as_ref()
        .and_then(|value| value.as_object())
        .expect("additional params on the request");
    // `top_p`/`top_k` have no dedicated field — they ride the flattened map.
    assert_eq!(additional.get("top_p"), Some(&json!(0.9)));
    assert_eq!(additional.get("top_k"), Some(&json!(40)));
    // The overlay: level over model over provider, earlier sources' unique
    // keys retained.
    assert_eq!(additional.get("shared"), Some(&json!("level")));
    assert_eq!(additional.get("model_only"), Some(&json!(true)));
    assert_eq!(additional.get("only_provider"), Some(&json!(true)));
    std::fs::remove_dir_all(store.dir()).ok();
    Ok(())
}
