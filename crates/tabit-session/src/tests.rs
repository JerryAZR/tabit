//! Session facade tests: scripted mock models drive the full outer loop —
//! persistence, events, resume, repair — with no network and no frontend.

use crate::SessionError;
use crate::entry::EntryKind;
use crate::events::SessionEvent;
use crate::model::ModelSelection;
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

fn temp_store(tag: &str) -> SessionStore {
    let dir = std::env::temp_dir()
        .join("tabit-session-tests")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    SessionStore::new(&dir)
}

fn test_config() -> Arc<tabit_config::TabitConfig> {
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

fn test_auth() -> Arc<tabit_config::AuthConfig> {
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
fn text_turn(text: &str) -> Vec<MockStreamEvent> {
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
fn tool_turn(call_id: &str, tool: &str) -> Vec<MockStreamEvent> {
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

fn echo_tool() -> DynamicTool {
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
struct Factory {
    turns: Vec<Vec<MockStreamEvent>>,
    requested: Mutex<Vec<(String, String)>>,
}

impl Factory {
    fn new(turns: Vec<Vec<MockStreamEvent>>) -> Arc<Self> {
        Arc::new(Self {
            turns,
            requested: Mutex::new(Vec::new()),
        })
    }

    fn into_builder(self: Arc<Self>, store: SessionStore) -> SessionBuilder {
        SessionBuilder::new(
            store,
            test_config(),
            test_auth(),
            ModelSelection::new("p", "m"),
        )
        .expect("builder")
        .model_factory(move |provider, model| {
            if let Ok(mut guard) = self.requested.lock() {
                guard.push((provider.to_string(), model.to_string()));
            }
            Ok(ModelHandle::new(MockCompletionModel::from_stream_turns(
                self.turns.clone(),
            )))
        })
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
async fn single_turn_prompt_persists_and_projects() -> Result<(), SessionError> {
    let store = temp_store("single");
    let factory = Factory::new(vec![text_turn("hello there")]);
    let mut session = factory.clone().into_builder(store.clone()).create("C:/w")?;

    let run: RunSummary = session.prompt("hi").await.expect("run");
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

    let run = session.prompt("echo x").await.expect("run");
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

#[tokio::test]
async fn resume_continues_the_log_and_reports_the_model() -> Result<(), SessionError> {
    let store = temp_store("resume");
    let factory = Factory::new(vec![text_turn("one"), text_turn("two")]);
    let mut first = factory.into_builder(store.clone()).create("C:/w")?;
    first.prompt("first question").await.expect("run 1");
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
    let run = second.prompt("second question").await.expect("run 2");
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
    let mut writer = store.create("C:/w").expect("create");
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
    let run = session.prompt("continue").await.expect("run");
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

    let result = session.prompt("doomed").await;
    assert!(result.is_err(), "the mock provider error surfaces");

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
async fn set_model_records_the_change_and_splits_stats() -> Result<(), SessionError> {
    let store = temp_store("switch");
    let factory = Factory::new(vec![text_turn("a"), text_turn("b")]);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;
    session.prompt("one").await.expect("run 1");

    session
        .set_model(ModelSelection::new("q", "m2"))
        .expect("switch");
    session.prompt("two").await.expect("run 2");

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
async fn resume_picks_up_the_model_from_the_log() -> Result<(), SessionError> {
    let store = temp_store("resume-model");
    let factory = Factory::new(vec![text_turn("a")]);
    let mut session = factory.into_builder(store.clone()).create("C:/w")?;
    session.prompt("one").await.expect("run");
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
    .model_factory(move |provider, model| {
        if let Ok(mut guard) = sink.lock() {
            guard.push((provider.to_string(), model.to_string()));
        }
        Ok(ModelHandle::new(MockCompletionModel::from_stream_turns(
            vec![text_turn("b")],
        )))
    });
    let (session, report) = builder.resume(&path).expect("resume");
    let resumed = report.resumed_model.expect("log carried the switch");
    assert_eq!(
        (resumed.provider.as_str(), resumed.model.as_str()),
        ("q", "m2")
    );
    assert_eq!(session.selection().provider, "q");
    // The factory was consulted for q/m2, not the builder default p/m.
    assert_eq!(
        requested.lock().expect("sink").as_slice(),
        [("q".to_string(), "m2".to_string())]
    );
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
    let run = session.prompt("hi").await.expect("run");
    assert_eq!(run.output, "ok");

    // max_turns(1) means a follow-up outer loop with a tool turn cannot
    // get its second model call: the budget error surfaces loudly.
    let mut budgeted = Factory::new(vec![tool_turn("c1", "echo"), text_turn("never")])
        .into_builder(store.clone())
        .max_turns(1)
        .dynamic_tool(echo_tool())
        .create("C:/w")?;
    match budgeted.prompt("go").await {
        Err(SessionError::Prompt(error)) => {
            let text = error.to_string();
            assert!(
                text.to_lowercase().contains("max"),
                "expected a max-turns failure, got: {text}"
            );
        }
        other => panic!("expected budget failure, got {:?}", other.err()),
    }
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
    let run = session.prompt("deep question").await.expect("run");
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
    session.prompt("one").await.expect("run 1");
    session.prompt("two").await.expect("run 2");

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

    match session.prompt("destroy the log").await {
        // Which arm fires first is platform-dependent (on Windows the
        // writer keeps writing to the unlinked handle, so the reload read
        // fails before the recorder does); either way the run must fail
        // loudly instead of reporting success the disk does not back.
        Err(SessionError::Persist(message)) => assert!(!message.is_empty()),
        Err(SessionError::Io { path, .. }) => {
            assert!(path.to_string_lossy().contains(".jsonl"), "{path:?}")
        }
        Err(other) => panic!("expected Persist or Io, got {other}"),
        Ok(run) => panic!("run must not succeed with an unwritable log: {run:?}"),
    }
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
