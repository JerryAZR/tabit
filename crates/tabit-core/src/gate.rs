//! The built-in permission gate's integration: pi-sanity's ported
//! policy (crates/tabit-gate) as one in-process `AgentHook` member,
//! assembled into the binary's hook stack (owner ruling 2026-09 — a
//! default safety feature must not fail open on a dead extension
//! process). The card flow follows pi-sanity's integration layer:
//! `allow` runs, `deny` skips with the reason, `ask` opens one
//! `native:select_one` card (Allow / Block, free text carries a
//! block reason) — an answer of Block, a dismissal, or no UI
//! available (print mode, a headless host) all skip, never silently
//! run. There is deliberately no "Always allow" in v1 — pi-sanity
//! has none, and the default rule set is shaped to ask rarely.

use rig_agent::agent::hook::{AgentHook, HookContext, ToolCall, ToolCallAction};
use rig_agent::tool::interaction::InteractionOutcome;
use serde_json::json;

/// The gate: the default rule set plus the ask. Shared across
/// sessions like every hook member (stateless per call — the policy
/// is pure).
#[derive(Clone)]
pub struct PermissionGate {
    config: std::sync::Arc<tabit_gate::config::SanityConfig>,
}

impl PermissionGate {
    /// The default-policy gate (settings carry only the opt-out, so
    /// the rules themselves are the shipped defaults, one instance
    /// per process).
    pub fn mount() -> Self {
        Self {
            config: std::sync::Arc::new(tabit_gate::config::default_config()),
        }
    }

    /// The hook stack with the gate mounted — priority 0, ahead of
    /// whatever the extension mount carries (the cheap in-process
    /// checks run first; extensions see what survived).
    pub fn stack() -> rig_agent::agent::HookStack {
        let mut stack = rig_agent::agent::HookStack::new();
        stack.push(Self::mount());
        stack
    }
}

impl PermissionGate {
    /// The execution action for arguments the gate may have
    /// rewritten: the rewritten map becomes the effective arguments,
    /// so the tool and every later hook see the same paths the gate
    /// checked.
    fn execute(
        args: serde_json::Map<String, serde_json::Value>,
        rewritten: bool,
    ) -> ToolCallAction {
        if rewritten {
            ToolCallAction::rewrite(serde_json::Value::Object(args))
        } else {
            ToolCallAction::run()
        }
    }
}

impl AgentHook for PermissionGate {
    async fn on_tool_call(&self, ctx: &HookContext, call: ToolCall<'_>) -> ToolCallAction {
        // The engine hands the effective arguments as a raw JSON
        // string (post-rewrite by earlier hooks). Unparseable
        // arguments are nothing the gate can check — the
        // strict-arguments doctrine errors the call before execution
        // anyway.
        let Ok(serde_json::Value::Object(mut args)) = serde_json::from_str(call.args) else {
            return ToolCallAction::run();
        };
        // "/tmp always means temp" (pi-sanity's tmp-rewrite): a
        // POSIX-style /tmp path in a file-tool param is rewritten to
        // the real temp dir BEFORE checking, so the check and the
        // execution agree on where the file lands — the shell
        // translates /tmp itself and is never rewritten (the rewrite
        // skips bash-check params).
        let tmpdir = std::env::temp_dir().to_string_lossy().into_owned();
        let rewritten = tabit_gate::tmp_rewrite::rewrite_tool_path_param(
            call.tool_name,
            &mut args,
            &tmpdir,
            &self.config,
            tabit_gate::path_utils::Platform::native(),
        );
        let Some(result) = tabit_gate::check_tool_call(call.tool_name, &args, &self.config) else {
            // No rule names this tool: pass through silently.
            return Self::execute(args, rewritten);
        };
        match result.action {
            tabit_gate::types::Action::Allow => Self::execute(args, rewritten),
            tabit_gate::types::Action::Deny => skip(result.reason, call),
            tabit_gate::types::Action::Ask => {
                let Some(interaction) = ctx.interaction() else {
                    return skip(
                        Some(format!(
                            "{} (no UI available — the gate treats an unanswerable ask as a block)",
                            result
                                .reason
                                .as_deref()
                                .unwrap_or("this operation requires confirmation")
                        )),
                        call,
                    );
                };
                let reason = result
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("`{}` requires confirmation", call.tool_name));
                let payload = json!({
                    "title": reason,
                    "body": tabit_gate::build_tool_details(call.tool_name, &args, &self.config),
                    "options": [
                        {"label": "Allow"},
                        {"label": "Block", "description": "the call does not run"},
                    ],
                    "free_text": true,
                });
                match interaction.request("native:select_one", payload).await {
                    InteractionOutcome::Answered(answer) => {
                        let allowed = answer["selected"][0]
                            .as_str()
                            .is_some_and(|label| label == "Allow");
                        if allowed {
                            Self::execute(args, rewritten)
                        } else {
                            let custom = answer["text"]
                                .as_str()
                                .map(str::trim)
                                .filter(|t| !t.is_empty());
                            skip(
                                Some(match custom {
                                    Some(text) => format!("{reason}: {text}"),
                                    None => format!("{reason} (blocked by user)"),
                                }),
                                call,
                            )
                        }
                    }
                    // A dismissal is a block, never a silent run —
                    // the run ending under the card drops the ask and
                    // the call must not sneak through.
                    InteractionOutcome::Dismissed => {
                        skip(Some(format!("{reason} (blocked by user)")), call)
                    }
                }
            }
        }
    }
}

/// The skip: pi-sanity's block shape — the call did not run, the
/// agent is told why, and the turn continues.
fn skip(reason: Option<String>, call: ToolCall<'_>) -> ToolCallAction {
    ToolCallAction::skip(format!(
        "the permission gate blocked `{}` — {}: the call did not run",
        call.tool_name,
        reason.as_deref().unwrap_or("not permitted by policy")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_agent::tool::interaction::UserInteraction;

    /// The scripted user: one canned answer per ask, remembering the
    /// ask it answered (ui_type + payload) for the assertions.
    enum Answer {
        Allow,
        Block {
            text: Option<&'static str>,
        },
        /// A malformed answer payload (no readable selection): the
        /// gate must read it as not-Allow and fail closed.
        Malformed,
        Dismissed,
    }

    struct ScriptedUser {
        answer: Answer,
        ask: std::sync::Mutex<Option<(String, serde_json::Value)>>,
    }

    impl UserInteraction for ScriptedUser {
        fn request(
            &self,
            ui_type: &str,
            payload: serde_json::Value,
        ) -> futures::future::BoxFuture<'static, InteractionOutcome> {
            *self.ask.lock().expect("test lock") = Some((ui_type.to_string(), payload));
            let outcome = match self.answer {
                Answer::Allow => {
                    InteractionOutcome::Answered(serde_json::json!({"selected": ["Allow"]}))
                }
                Answer::Block { text } => InteractionOutcome::Answered(match text {
                    Some(text) => serde_json::json!({"selected": ["Block"], "text": text}),
                    None => serde_json::json!({"selected": ["Block"]}),
                }),
                Answer::Malformed => InteractionOutcome::Answered(serde_json::json!({})),
                Answer::Dismissed => InteractionOutcome::Dismissed,
            };
            Box::pin(async move { outcome })
        }
    }

    /// The gate over the default rule set, plus the real path context
    /// the checkers expand `{{HOME}}`/`{{TMPDIR}}` against.
    fn gate() -> (PermissionGate, tabit_gate::path_permission::PathContext) {
        (
            PermissionGate::mount(),
            tabit_gate::path_permission::default_context(),
        )
    }

    fn ctx(user: Option<&std::sync::Arc<ScriptedUser>>) -> HookContext {
        let mut capabilities = rig_agent::tool::ToolContext::new();
        if let Some(user) = user {
            capabilities.insert(user.clone() as std::sync::Arc<dyn UserInteraction>);
        }
        rig_agent::test_utils::hook_context(capabilities)
    }

    fn call(args: &str) -> ToolCall<'_> {
        ToolCall {
            tool_name: "write",
            tool_call_id: None,
            internal_call_id: "test-call",
            args,
        }
    }

    #[tokio::test]
    async fn an_allowed_write_runs_without_asking() {
        let (gate, _) = gate();
        let user = std::sync::Arc::new(ScriptedUser {
            answer: Answer::Allow,
            ask: std::sync::Mutex::new(None),
        });
        // /dev/null is an explicit allow override — no card, plain run.
        let action = gate
            .on_tool_call(&ctx(Some(&user)), call(r#"{"path": "/dev/null"}"#))
            .await;
        assert!(matches!(action, ToolCallAction::Run), "{action:?}");
        assert!(
            user.ask.lock().expect("test lock").is_none(),
            "an allow never asks"
        );
    }

    #[tokio::test]
    async fn a_write_outside_every_override_is_denied_with_its_reason() {
        let (gate, _) = gate();
        // A path outside home, cwd, and tmp on the checking platform —
        // the drive-absolute form is not drive-absolute to POSIX
        // (it would resolve under the cwd allow), so each platform
        // names its own outside.
        #[cfg(windows)]
        let outside = "C:/Windows/system32/gate-test.txt";
        #[cfg(not(windows))]
        let outside = "/etc/gate-test.txt";
        let args = serde_json::json!({ "path": outside }).to_string();
        let action = gate.on_tool_call(&ctx(None), call(&args)).await;
        let ToolCallAction::Skip(feedback) = action else {
            panic!("the default write policy is deny: {action:?}")
        };
        assert!(
            feedback.contains("the permission gate blocked `write`"),
            "{feedback}"
        );
        assert!(
            feedback.contains("Writing outside allowed locations"),
            "{feedback}"
        );
    }

    #[tokio::test]
    async fn unparseable_and_unruled_calls_pass_through() {
        let (gate, _) = gate();
        let action = gate.on_tool_call(&ctx(None), call("not json")).await;
        assert!(matches!(action, ToolCallAction::Run), "{action:?}");

        let mystery = ToolCall {
            tool_name: "mystery",
            tool_call_id: None,
            internal_call_id: "test-call",
            args: r#"{"any": "thing"}"#,
        };
        let action = gate.on_tool_call(&ctx(None), mystery).await;
        assert!(matches!(action, ToolCallAction::Run), "{action:?}");
    }

    #[tokio::test]
    async fn an_ask_answered_allow_runs_and_the_card_is_a_native_select() {
        let (gate, paths) = gate();
        let user = std::sync::Arc::new(ScriptedUser {
            answer: Answer::Allow,
            ask: std::sync::Mutex::new(None),
        });
        // A home write asks ("Writing outside working directory…").
        let home_file = format!("{}/gate-ask-test.txt", paths.home);
        let args = serde_json::json!({ "path": home_file }).to_string();
        let action = gate.on_tool_call(&ctx(Some(&user)), call(&args)).await;
        assert!(matches!(action, ToolCallAction::Run), "{action:?}");
        let (ui_type, payload) = user
            .ask
            .lock()
            .expect("test lock")
            .clone()
            .expect("an ask path opens exactly one card");
        assert_eq!(ui_type, "native:select_one");
        assert_eq!(
            payload["options"]
                .as_array()
                .expect("options array")
                .iter()
                .filter_map(|o| o["label"].as_str())
                .collect::<Vec<_>>(),
            vec!["Allow", "Block"]
        );
        assert_eq!(payload["free_text"], true);
        assert!(
            payload["title"]
                .as_str()
                .is_some_and(|t| t.contains("requires confirmation")),
            "the title carries the rule's reason: {payload}"
        );
    }

    #[tokio::test]
    async fn an_ask_answered_block_skips_with_the_custom_reason_or_the_plain_one() {
        let (gate, paths) = gate();
        let home_file = format!("{}/gate-ask-test.txt", paths.home);
        let args = serde_json::json!({ "path": home_file }).to_string();

        let with_text = std::sync::Arc::new(ScriptedUser {
            answer: Answer::Block {
                text: Some("not today"),
            },
            ask: std::sync::Mutex::new(None),
        });
        let action = gate.on_tool_call(&ctx(Some(&with_text)), call(&args)).await;
        let ToolCallAction::Skip(feedback) = action else {
            panic!("a block never runs: {action:?}")
        };
        assert!(
            feedback.contains("requires confirmation: not today"),
            "{feedback}"
        );

        let bare = std::sync::Arc::new(ScriptedUser {
            answer: Answer::Block { text: None },
            ask: std::sync::Mutex::new(None),
        });
        let action = gate.on_tool_call(&ctx(Some(&bare)), call(&args)).await;
        let ToolCallAction::Skip(feedback) = action else {
            panic!("a block never runs: {action:?}")
        };
        assert!(
            feedback.contains("requires confirmation (blocked by user)"),
            "{feedback}"
        );
    }

    #[tokio::test]
    async fn a_dismissed_ask_and_an_unanswerable_ask_both_skip() {
        let (gate, paths) = gate();
        let home_file = format!("{}/gate-ask-test.txt", paths.home);
        let args = serde_json::json!({ "path": home_file }).to_string();

        let dismissed = std::sync::Arc::new(ScriptedUser {
            answer: Answer::Dismissed,
            ask: std::sync::Mutex::new(None),
        });
        let action = gate.on_tool_call(&ctx(Some(&dismissed)), call(&args)).await;
        let ToolCallAction::Skip(feedback) = action else {
            panic!("a dismissal is a block, never a silent run: {action:?}")
        };
        assert!(feedback.contains("(blocked by user)"), "{feedback}");

        // No UI capability at all (print mode, a headless host): the
        // ask fails closed with the no-UI reason, still a skip.
        let action = gate.on_tool_call(&ctx(None), call(&args)).await;
        let ToolCallAction::Skip(feedback) = action else {
            panic!("an unanswerable ask must not run: {action:?}")
        };
        assert!(feedback.contains("no UI available"), "{feedback}");

        // A malformed answer (nothing readable in `selected`) is
        // not-Allow — the same fail-closed read as a block.
        let malformed = std::sync::Arc::new(ScriptedUser {
            answer: Answer::Malformed,
            ask: std::sync::Mutex::new(None),
        });
        let action = gate.on_tool_call(&ctx(Some(&malformed)), call(&args)).await;
        assert!(
            matches!(action, ToolCallAction::Skip(_)),
            "an unreadable answer must not run: {action:?}"
        );
    }

    #[tokio::test]
    async fn a_posix_tmp_path_is_rewritten_to_the_real_temp_dir_before_the_check() {
        let (gate, paths) = gate();
        let action = gate
            .on_tool_call(
                &ctx(None),
                call(r#"{"path": "/tmp/gate-rewrite-test.txt"}"#),
            )
            .await;
        if cfg!(windows) {
            // The rewritten path lands in {{TMPDIR}} (an allow override
            // listed after home, so it wins), and the execution must
            // see the rewritten path — a plain Run would write
            // C:\tmp\… junk while the check blessed a temp file.
            let ToolCallAction::Rewrite(args) = action else {
                panic!("the /tmp rewrite must ride the action: {action:?}")
            };
            let rewritten = args["path"].as_str().expect("path arg");
            assert_ne!(rewritten, "/tmp/gate-rewrite-test.txt");
            assert!(
                std::path::Path::new(rewritten).starts_with(std::env::temp_dir()),
                "the rewrite lands in the real temp dir: {rewritten}"
            );
            assert!(rewritten.ends_with("gate-rewrite-test.txt"), "{rewritten}");
        } else {
            // POSIX /tmp IS the real temp dir: the rewrite is the
            // identity there (by design — only win32 rewrites), and
            // the {{TMPDIR}}/** allow matches the path directly.
            assert!(
                matches!(action, ToolCallAction::Run),
                "no rewrite off win32, a plain allow: {action:?}"
            );
        }
        assert_eq!(
            paths.tmpdir,
            std::env::temp_dir().to_string_lossy(),
            "the checker's TMPDIR and the rewrite's tmpdir agree"
        );
    }
}
