//! The example tool's policy units. The substrate itself — spawn,
//! forwarding, routing, steering, the abort leash — is proven
//! end-to-end over the real binary by `crates/tabit/tests/
//! subprocess_children.rs` (the in-process suite died with the
//! in-process substrate, owner ruling 2026-09).

use tabit_protocol::ModelSelection;

fn parent_selection() -> ModelSelection {
    ModelSelection::new("prov", "model-a")
}

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
fn parse_selection_reads_qualified_and_bare_refs() {
    let parent = parent_selection();
    let qualified = super::parse_selection("other/cheap", &parent).expect("qualified");
    assert_eq!(qualified.provider, "other");
    assert_eq!(qualified.model, "cheap");

    let bare = super::parse_selection("model-b", &parent).expect("bare");
    assert_eq!(
        bare.provider, "prov",
        "a bare id rides the parent's provider"
    );
    assert_eq!(bare.model, "model-b");

    // The thinking level is inherited, not parsed here.
    assert_eq!(bare.thinking_level, parent.thinking_level);

    for bad in ["", "/x", "x/"] {
        assert!(
            super::parse_selection(bad, &parent).is_err(),
            "`{bad}` must not parse"
        );
    }
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
