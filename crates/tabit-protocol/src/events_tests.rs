use super::*;

const TURN: &str = "0192uuidv7turn";

#[test]
fn events_round_trip_through_json() {
    let events = vec![
        SessionEvent::UserMessage {
            text: "hi".to_string(),
            entry_id: "0192uuidv7user".to_string(),
        },
        SessionEvent::MessageQueued {
            id: "0192uuidv7queued".to_string(),
            text: "queued while running".to_string(),
        },
        SessionEvent::MessagesDiscarded {
            messages: vec![DiscardedMessage {
                id: "0192uuidv7queued".to_string(),
                text: "queued while running".to_string(),
            }],
        },
        SessionEvent::TurnStarted {
            id: TURN.to_string(),
        },
        SessionEvent::TextDelta {
            turn_id: TURN.to_string(),
            text: "hel".to_string(),
        },
        SessionEvent::ReasoningDelta {
            turn_id: TURN.to_string(),
            id: "r0".to_string(),
            reasoning: "thinking...".to_string(),
        },
        SessionEvent::ToolCall {
            turn_id: TURN.to_string(),
            name: "echo".to_string(),
            call_id: "c1".to_string(),
            internal_call_id: "i1".to_string(),
            arguments: Some("{}".to_string()),
        },
        SessionEvent::TurnCommitted {
            id: TURN.to_string(),
        },
        SessionEvent::ToolResult {
            turn_id: TURN.to_string(),
            entry_id: "0192uuidv7entry".to_string(),
            name: "echo".to_string(),
            internal_call_id: "i1".to_string(),
            content: "0".to_string(),
            status: ToolResultStatus::Success,
            details: None,
        },
        // The same event with presentation cargo (the edit tool's
        // shape) — details round-trips like every other field.
        SessionEvent::ToolResult {
            turn_id: TURN.to_string(),
            entry_id: "0192uuidv7entry2".to_string(),
            name: "edit".to_string(),
            internal_call_id: "i2".to_string(),
            content: "Edited f.txt (1 of 1 blocks applied; +1/-1 lines, first change at line 2)"
                .to_string(),
            status: ToolResultStatus::Success,
            details: Some(serde_json::json!({
                "diff": {
                    "first_changed_line": 2,
                    "hunks": [{
                        "old_start": 1, "old_lines": 3,
                        "new_start": 1, "new_lines": 3,
                        "lines": [
                            { "kind": "context", "text": "alpha" },
                            { "kind": "removed", "text": "beta" },
                            { "kind": "added", "text": "BETA" },
                            { "kind": "context", "text": "gamma" }
                        ]
                    }]
                },
                "outcomes": [{ "index": 0, "applied": true }]
            })),
        },
        SessionEvent::TurnRetried {
            turn_id: TURN.to_string(),
        },
        SessionEvent::CompletionCall {
            turn_id: TURN.to_string(),
            input_tokens: 10,
            output_tokens: 4,
        },
        SessionEvent::TurnTruncated {
            turn_id: TURN.to_string(),
        },
        SessionEvent::RunFinished {
            output: "done".to_string(),
            usage: Usage::default(),
            durable: true,
        },
        SessionEvent::RunFailed {
            message: "provider stream ended early".to_string(),
        },
        SessionEvent::RunAborted {
            output: "partial text".to_string(),
        },
        SessionEvent::InteractionRequest {
            id: "0199".to_string(),
            ui_type: "native:confirm".to_string(),
            payload: serde_json::json!({"title": "Run command?"}),
        },
        SessionEvent::error_model("default_model `gone` is not usable"),
        SessionEvent::ReplayStarted { total: 7 },
        SessionEvent::ReplayDone,
        SessionEvent::CheckedOut {
            entry_id: "0197".to_string(),
            base_id: None,
        },
        SessionEvent::error_checkout("no entry `0199` in this session"),
        SessionEvent::SessionsAvailable {
            sessions: vec![
                AvailableSession {
                    id: "0197".to_string(),
                    created_at: "2026-08-22T10:00:00Z".to_string(),
                    entry_count: 14,
                },
                AvailableSession {
                    id: "0196".to_string(),
                    created_at: "2026-08-21T09:00:00Z".to_string(),
                    entry_count: 0,
                },
            ],
        },
        SessionEvent::SessionCreated {
            id: "0198".to_string(),
            path: "C:/w/.tabit/sessions/20260822_0198.jsonl".to_string(),
            model: ModelSelection::new("p", "m"),
        },
        SessionEvent::error_session("no session with id `0195`"),
        SessionEvent::ModelChanged {
            provider: "p".to_string(),
            model: "m".to_string(),
            thinking_level: Some("high".to_string()),
        },
        SessionEvent::error_persist_degraded(3, "records are pending on disk"),
        SessionEvent::SkillsAvailable {
            skills: vec![AvailableSkill {
                name: "lint".to_string(),
                description: "Lint the workspace".to_string(),
                location: "C:/w/.agents/skills/lint/SKILL.md".to_string(),
                level: "user".to_string(),
            }],
        },
        SessionEvent::ExtensionsAvailable {
            extensions: vec![AvailableExtension {
                name: "echo".to_string(),
                version: "0.1.0".to_string(),
                description: Some("the example".to_string()),
                dir: "C:/u/.tabit/extensions/echo".to_string(),
                status: "alive".to_string(),
                reason: None,
                tools: vec![AvailableExtensionTool {
                    name: "echo".to_string(),
                    description: "Echo the text back.".to_string(),
                }],
                hooks: vec!["tool_call".to_string()],
            }],
            conflicts: vec![ExtensionConflict {
                kind: ExtensionConflictKind::RefusedPeer,
                extension: "clash-b".to_string(),
                tool: "clashy".to_string(),
                incumbent: Some("clash-a".to_string()),
            }],
        },
        SessionEvent::NativeItem {
            turn_id: TURN.to_string(),
            item: serde_json::json!({"web_search_call": {}}),
        },
    ];
    for event in &events {
        let json = serde_json::to_string(event).expect("serialize");
        let back: SessionEvent = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, *event);
    }

    // The error carrier: kind is an open string; kind-specific structure
    // (the pending count) rides only when present.
    assert_eq!(
        serde_json::to_string(&SessionEvent::error_model("stale default_model"))
            .expect("serialize"),
        r#"{"type":"error","kind":"model","message":"stale default_model"}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionEvent::error_persist_degraded(3, "pending"))
            .expect("serialize"),
        r#"{"type":"error","kind":"persist_degraded","message":"pending","pending":3}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionEvent::SessionsAvailable {
            sessions: vec![AvailableSession {
                id: "0197".to_string(),
                created_at: "2026-08-22T10:00:00Z".to_string(),
                entry_count: 14,
            }]
        })
        .expect("serialize"),
        r#"{"type":"sessions_available","sessions":[{"id":"0197","created_at":"2026-08-22T10:00:00Z","entry_count":14}]}"#
    );
    // The skills announcement: unstamped backend-level facts, same
    // shape family as the session catalog.
    assert_eq!(
        serde_json::to_string(&SessionEvent::SkillsAvailable {
            skills: vec![AvailableSkill {
                name: "code-review".to_string(),
                description: "Review a changeset".to_string(),
                location: "C:/w/.tabit/skills/code-review/SKILL.md".to_string(),
                level: "workspace".to_string(),
            }]
        })
        .expect("serialize"),
        r#"{"type":"skills_available","skills":[{"name":"code-review","description":"Review a changeset","location":"C:/w/.tabit/skills/code-review/SKILL.md","level":"workspace"}]}"#
    );
    // The extension announcement: the same unstamped backend-level
    // family, carrying provenance and the mandatory conflict reports.
    assert_eq!(
        serde_json::to_string(&SessionEvent::ExtensionsAvailable {
            extensions: vec![AvailableExtension {
                name: "shadow".to_string(),
                version: "0.1.0".to_string(),
                description: None,
                dir: "C:/u/.tabit/extensions/shadow".to_string(),
                status: "dead".to_string(),
                reason: Some("no handshake within 30s".to_string()),
                tools: Vec::new(),
                hooks: Vec::new(),
            }],
            conflicts: vec![ExtensionConflict {
                kind: ExtensionConflictKind::ReplacesCore,
                extension: "shadow".to_string(),
                tool: "read".to_string(),
                incumbent: None,
            }],
        })
        .expect("serialize"),
        r#"{"type":"extensions_available","extensions":[{"name":"shadow","version":"0.1.0","description":null,"dir":"C:/u/.tabit/extensions/shadow","status":"dead","reason":"no handshake within 30s","tools":[],"hooks":[]}],"conflicts":[{"kind":"replaces_core","extension":"shadow","tool":"read","incumbent":null}]}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionEvent::SessionCreated {
            id: "0198".to_string(),
            path: "C:/w/s.jsonl".to_string(),
            model: ModelSelection::new("p", "m"),
        })
        .expect("serialize"),
        r#"{"type":"session_created","id":"0198","path":"C:/w/s.jsonl","model":{"provider":"p","model":"m","thinking_level":null}}"#
    );
    // The announce's wire spelling: a subagent child carries its
    // parentage and the spawning call's correlation id; a user
    // session carries neither (absent fields never hit the wire).
    assert_eq!(
        serde_json::to_string(&SessionEvent::SessionOpened {
            id: "0199".to_string(),
            path: String::new(),
            model: ModelSelection::new("p", "m"),
            resumed: false,
            parent: Some("0192uuidv7parent".to_string()),
            parent_call: Some("i1".to_string()),
        })
        .expect("serialize"),
        r#"{"type":"session_opened","id":"0199","path":"","model":{"provider":"p","model":"m","thinking_level":null},"resumed":false,"parent":"0192uuidv7parent","parent_call":"i1"}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionEvent::SessionOpened {
            id: "0199".to_string(),
            path: "C:/w/s.jsonl".to_string(),
            model: ModelSelection::new("p", "m"),
            resumed: true,
            parent: None,
            parent_call: None,
        })
        .expect("serialize"),
        r#"{"type":"session_opened","id":"0199","path":"C:/w/s.jsonl","model":{"provider":"p","model":"m","thinking_level":null},"resumed":true}"#
    );
    // The wire spelling of the brackets and the truncation warning.
    assert_eq!(
        serde_json::to_string(&SessionEvent::TurnStarted {
            id: TURN.to_string()
        })
        .expect("serialize"),
        r#"{"type":"turn_started","id":"0192uuidv7turn"}"#
    );
    assert_eq!(
        serde_json::to_string(&SessionEvent::TurnTruncated {
            turn_id: TURN.to_string()
        })
        .expect("serialize"),
        r#"{"type":"turn_truncated","turn_id":"0192uuidv7turn"}"#
    );
}
