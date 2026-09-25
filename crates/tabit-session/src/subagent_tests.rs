//! The example tool's policy units. The substrate itself — spawn,
//! forwarding, routing, steering, the abort leash — is proven
//! end-to-end over the real binary by `crates/tabit/tests/
//! subprocess_children.rs` (the in-process suite died with the
//! in-process substrate, owner ruling 2026-09).

fn named_tool(name: &'static str) -> rig_agent::tool::DynamicTool {
    rig_agent::tool::DynamicTool::new(
        name,
        "a test tool",
        serde_json::json!({"type": "object"}),
        move |_ctx, _args| {
            let output = name;
            Box::pin(async move { Ok(rig_agent::tool::ToolOutput::text(output)) })
        },
    )
}

#[test]
fn filter_tools_keeps_order_and_fails_loudly_on_unknown_names() {
    let tools = vec![named_tool("read"), named_tool("bash")];
    let allow =
        super::filter_tools(&tools, &["bash".to_string(), "read".to_string()]).expect("filters");
    assert_eq!(
        allow.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
        vec!["bash", "read"],
        "the allow-list's order is the child's toolset order"
    );

    let error = match super::filter_tools(&tools, &["read".to_string(), "typo".to_string()]) {
        Err(error) => error,
        Ok(tools) => panic!("an unknown name must be loud, got {}", tools.len()),
    };
    let message = error.to_string();
    assert!(message.contains("typo"), "names the miss: {message}");
    assert!(message.contains("bash"), "lists what exists: {message}");
}

#[tokio::test]
async fn a_pre_cancelled_token_refuses_before_spawning() {
    // Bash's rule (tabit-tools' run_shell): "it never ran" is
    // structural — the check sits ahead of the SpawnContext fetch, so
    // a refused call spawns nothing (and needs no mounted capability).
    let mut context = rig_agent::tool::ToolContext::new();
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    context.insert(token);
    let error = super::subagent(&mut context, "do a thing".to_string(), None)
        .await
        .expect_err("a pre-cancelled run refuses");
    let message = error.to_string();
    assert!(message.contains("did not run"), "{message}");
}

#[tokio::test]
async fn the_tool_refuses_when_the_capability_is_not_mounted() {
    // A session whose assembly skipped subagents still has the tool
    // reachable only through explicit registration — the error names
    // the missing mount.
    let mut context = rig_agent::tool::ToolContext::new();
    let error = super::subagent(&mut context, "do a thing".to_string(), None)
        .await
        .expect_err("no capability mounted");
    let message = error.to_string();
    assert!(message.contains("did not mount"), "{message}");
}

fn summary(outcome: crate::session::RunOutcome, output: &str) -> crate::session::RunSummary {
    crate::session::RunSummary {
        outcome,
        output: output.to_string(),
        events: Vec::new(),
    }
}

#[test]
fn an_aborted_child_maps_to_the_interrupted_report() {
    let error = super::summary_result(summary(crate::session::RunOutcome::Aborted, ""), "child-1")
        .expect_err("aborted is an error");
    let message = error.to_string();
    assert!(
        message.contains("interrupted before completing"),
        "{message}"
    );
    assert!(message.contains("partial"), "{message}");
}

#[test]
fn a_failed_child_carries_its_own_failure_reason() {
    let mut run = summary(crate::session::RunOutcome::Failed, "");
    // The reason search walks backwards, so a non-failure event after
    // the failure is skipped on the way to it.
    run.events.push(tabit_protocol::SessionEvent::RunFailed {
        message: "provider unreachable".to_string(),
        kind: "provider".to_string(),
        started_at_ms: 1_000,
        completed_at_ms: 2_000,
    });
    run.events.push(tabit_protocol::SessionEvent::TurnStarted {
        id: "t1".to_string(),
        started_at_ms: 1_000,
    });
    let error = super::summary_result(run, "child-1").expect_err("failed is an error");
    let message = error.to_string();
    assert!(message.contains("provider unreachable"), "{message}");
}

#[test]
fn an_unknown_tool_in_the_allow_list_is_refused() {
    // The allow-list validates parent-side against the mounted
    // toolset — a name nothing offers is refused before any spawn.
    // (`DynamicTool` carries closures and no Debug — a match, not
    // `expect_err`.)
    let error = match super::filter_tools(&[], &["read".to_string()]) {
        Err(error) => error,
        Ok(offered) => panic!(
            "an empty toolset cannot offer `read` (returned {} tools)",
            offered.len()
        ),
    };
    assert!(error.to_string().contains("read"), "{error}");
}

#[test]
fn a_failed_child_without_a_recorded_reason_says_unknown() {
    // A crash-shaped failure leaves no RunFailed event; the report
    // names the absence instead of inventing a cause.
    let error = super::summary_result(summary(crate::session::RunOutcome::Failed, ""), "child-1")
        .expect_err("failed is an error");
    let message = error.to_string();
    assert!(message.contains("unknown failure"), "{message}");
}

#[test]
fn a_completed_child_without_a_final_answer_says_so() {
    let output = super::summary_result(
        summary(crate::session::RunOutcome::Completed, "   "),
        "child-1",
    )
    .expect("completed is a result");
    let text = output.render();
    assert!(
        text.contains("without a final answer"),
        "the report names the empty answer: {text}"
    );
}

#[tokio::test]
async fn a_missing_executable_fails_the_spawn_with_the_exe_named() {
    // The bridge's spawn error carries the executable path — the
    // operator's first question is which binary failed to start.
    let parts = std::sync::Arc::new(super::SubagentParts {
        node: std::sync::Arc::new(tabit_wire::node::Node::new("test")),
        exe: std::path::PathBuf::from("Z:/does-not-exist/tabit-child.exe"),
        tools: Vec::new(),
        max_turns: 4,
        extensions: std::path::PathBuf::new(),
    });
    let ctx = super::SpawnContext::new(
        parts,
        "parent-session".to_string(),
        tabit_protocol::ModelSelection::new("p", "m"),
        std::path::PathBuf::from("."),
    );
    let result = ctx
        .spawn_subprocess()
        .cwd(std::path::PathBuf::from("."))
        .model(tabit_protocol::ModelSelection::new("p", "m"))
        .max_turns(4)
        .ephemeral(true)
        .spawn()
        .await;
    let Err(error) = result else {
        panic!("the executable does not exist — the spawn must fail");
    };
    assert!(error.contains("tabit-child.exe"), "{error}");
}

/// The bridge's mount invariant, over a REAL child: the child's
/// first stamped frames — its `session_opened` lands right after the
/// handshake ack, usually in the same pipe read — must reach the
/// node's fan (the frontend's subscription hears them; the learning
/// table learns the child's stream). The pre-mount bug this pins:
/// the lane was caller-assembled after `spawn` returned, so any
/// frame the pump read before the caller set the lane dropped from
/// the fan silently (the run still worked — the fold has its own
/// mirror — but the frontend never saw the child open).
#[tokio::test]
async fn a_childs_first_frames_reach_the_node_fan() {
    let core = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("debug")
        .join("tabit-core.exe");
    if !core.is_file() {
        eprintln!(
            "bridge e2e: no tabit-core.exe at {} — \
             run the workspace suite (scripts/test.sh) to cover it",
            core.display()
        );
        return;
    }
    // A minimal offline config, isolated to this test: the child
    // parses the provider and never calls it (no message is sent —
    // the child boots, announces, and is killed). The env claim is
    // ours alone in this test run.
    let dir = std::env::temp_dir().join(format!("tabit-bridge-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("cwd")).expect("cwd dir");
    std::fs::create_dir_all(dir.join("ext")).expect("ext dir");
    std::fs::write(
        dir.join("providers.toml"),
        "[providers.offline]\nbase_url = \"http://127.0.0.1:9/v1\"\n\
         api = \"openai-completions\"\nkeyless = true\n\n\
         [[providers.offline.models]]\nid = \"dead\"\n",
    )
    .expect("the offline provider fragment");
    unsafe { std::env::set_var("TABIT_CONFIG", dir.join("providers.toml")) };

    let node = std::sync::Arc::new(tabit_wire::node::Node::new("test"));
    let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    node.subscribe_all(
        tabit_wire::node::Locality::Both,
        move |frame: &tabit_protocol::EventFrame| {
            let note = format!(
                "{}@{}",
                frame.event.tag(),
                frame
                    .stream
                    .as_ref()
                    .map(|s| s.as_str().to_string())
                    .unwrap_or_default()
            );
            sink.lock().expect("test lock").push(note);
        },
    );
    let parts = std::sync::Arc::new(super::SubagentParts {
        node: node.clone(),
        exe: core,
        tools: Vec::new(),
        max_turns: 1,
        extensions: dir.join("ext"),
    });
    let offline = tabit_protocol::ModelSelection::new("offline", "dead");
    let ctx = super::SpawnContext::new(
        parts,
        "parent-session".to_string(),
        offline.clone(),
        dir.join("cwd"),
    );
    let mut child = ctx
        .spawn_subprocess()
        .cwd(dir.join("cwd"))
        .model(offline)
        .ephemeral(true)
        .spawn()
        .await
        .expect("the child spawned and handshook");
    unsafe { std::env::remove_var("TABIT_CONFIG") };
    let id = child.id().to_string();

    // Bounded wait: the child's opening announcement must reach the
    // fan, stamped with the child's stream.
    let opened = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            if seen
                .lock()
                .expect("test lock")
                .iter()
                .any(|note| note.starts_with("session_opened") && note.ends_with(&id))
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    child.close();
    let _ = child.wait_exit().await;
    assert!(
        opened.is_ok(),
        "the child's session_opened never reached the node's fan: {:?}",
        seen.lock().expect("test lock")
    );
}
