use super::*;
use crate::tool::{ToolErrorKind, ToolExecutionError};

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use serde_json::{Value, json};

fn ctx() -> HookContext {
    HookContext::new(false, Some("test-agent".to_string()), Default::default())
}

#[derive(Clone)]
struct CallRewriter {
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    replacement: serde_json::Value,
}

impl AgentHook for CallRewriter {
    async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        self.seen.lock().unwrap().push(event.args.to_string());
        ToolCallAction::rewrite(self.replacement.clone())
    }
}

#[tokio::test]
async fn tool_call_rewrites_chain_in_registration_order() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut stack = HookStack::with(CallRewriter {
        seen: seen.clone(),
        replacement: serde_json::json!({"step": 1}),
    });
    stack.push(CallRewriter {
        seen: seen.clone(),
        replacement: serde_json::json!({"step": 2}),
    });

    let action = stack
        .on_tool_call(
            &HookContext::new(false, None, Default::default()),
            ToolCall {
                tool_name: "tool",
                tool_call_id: Some("provider-id"),
                internal_call_id: "internal-id",
                args: r#"{"step":0}"#,
            },
        )
        .await;

    assert_eq!(
        *seen.lock().unwrap(),
        vec![r#"{"step":0}"#.to_string(), r#"{"step":1}"#.to_string()]
    );
    assert_eq!(
        action,
        ToolCallAction::rewrite(serde_json::json!({"step": 2}))
    );
}

#[derive(Clone)]
struct ResultRewriter {
    seen: Arc<std::sync::Mutex<Vec<(String, ToolErrorKind, String)>>>,
    replacement: String,
}

impl AgentHook for ResultRewriter {
    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        self.seen.lock().unwrap().push((
            event.presentation.render(),
            event.raw_result.error().unwrap().kind(),
            event.tool_context.result::<String>().unwrap().clone(),
        ));
        ToolResultAction::rewrite(self.replacement.clone())
    }
}

#[tokio::test]
async fn result_rewrites_chain_without_mutating_raw_result_or_context() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut stack = HookStack::with(ResultRewriter {
        seen: seen.clone(),
        replacement: "redacted".into(),
    });
    stack.push(ResultRewriter {
        seen: seen.clone(),
        replacement: "truncated".into(),
    });
    let raw = ToolResult::failed(ToolExecutionError::timeout("raw failure"));
    let mut context = ToolContext::new();
    context.insert_result("request-metadata".to_string());

    let action = stack
        .on_tool_result(
            &HookContext::new(false, None, Default::default()),
            ToolResultEvent {
                tool_name: "tool",
                tool_call_id: None,
                internal_call_id: "internal-id",
                args: "{}",
                presentation: raw.output(),
                raw_result: &raw,
                tool_context: &context,
            },
        )
        .await;

    assert_eq!(action, ToolResultAction::rewrite("truncated"));
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            (
                "raw failure".into(),
                ToolErrorKind::Timeout,
                "request-metadata".into()
            ),
            (
                "redacted".into(),
                ToolErrorKind::Timeout,
                "request-metadata".into()
            ),
        ]
    );
    assert_eq!(raw.output().as_text(), Some("raw failure"));
    assert_eq!(
        context.result::<String>().map(String::as_str),
        Some("request-metadata")
    );
}

struct StopThenCount {
    stop: bool,
    calls: Arc<AtomicUsize>,
}

impl AgentHook for StopThenCount {
    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        _event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.stop {
            ToolResultAction::stop("terminal")
        } else {
            ToolResultAction::keep()
        }
    }
}

#[tokio::test]
async fn terminal_result_action_short_circuits_later_hooks() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut stack = HookStack::with(StopThenCount {
        stop: true,
        calls: calls.clone(),
    });
    stack.push(StopThenCount {
        stop: false,
        calls: calls.clone(),
    });
    let raw = ToolResult::success(ToolOutput::text("ok"));
    let context = ToolContext::new();
    let action = stack
        .on_tool_result(
            &HookContext::new(false, None, Default::default()),
            ToolResultEvent {
                tool_name: "tool",
                tool_call_id: None,
                internal_call_id: "internal-id",
                args: "{}",
                presentation: raw.output(),
                raw_result: &raw,
                tool_context: &context,
            },
        )
        .await;

    assert_eq!(action, ToolResultAction::stop("terminal"));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

struct ToolRecorder {
    label: u32,
    log: Arc<Mutex<Vec<u32>>>,
    stop: bool,
}
impl AgentHook for ToolRecorder {
    async fn on_tool_call(&self, _ctx: &HookContext, _event: ToolCall<'_>) -> ToolCallAction {
        self.log.lock().expect("log").push(self.label);
        if self.stop {
            ToolCallAction::skip("stop")
        } else {
            ToolCallAction::run()
        }
    }
}

fn tool_call_event() -> ToolCall<'static> {
    ToolCall {
        tool_name: "add",
        tool_call_id: Some("tc1"),
        internal_call_id: "ic1",
        args: "{}",
    }
}

#[tokio::test]
async fn runs_hooks_in_registration_order_and_consults_all_on_continue() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut stack = HookStack::with(ToolRecorder {
        label: 1,
        log: log.clone(),
        stop: false,
    });
    stack.push(ToolRecorder {
        label: 2,
        log: log.clone(),
        stop: false,
    });
    assert_eq!(
        stack.on_tool_call(&ctx(), tool_call_event()).await,
        ToolCallAction::run()
    );
    assert_eq!(*log.lock().unwrap(), vec![1, 2]);
}

#[tokio::test]
async fn first_skip_short_circuits_on_chained_tool_call() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut stack = HookStack::with(ToolRecorder {
        label: 1,
        log: log.clone(),
        stop: true,
    });
    stack.push(ToolRecorder {
        label: 2,
        log: log.clone(),
        stop: false,
    });
    assert!(matches!(
        stack.on_tool_call(&ctx(), tool_call_event()).await,
        ToolCallAction::Skip(_)
    ));
    assert_eq!(*log.lock().unwrap(), vec![1]);
}

#[test]
fn hook_context_reports_identity_and_turn() {
    let context = HookContext::new(true, Some("agent".into()), Default::default());
    assert!(context.is_streaming());
    assert_eq!(context.agent_name(), Some("agent"));
    context.set_turn(3);
    assert_eq!(context.turn(), 3);
    assert!(!context.run_id().as_str().is_empty());
}

struct RewriteHook(Value);
impl AgentHook for RewriteHook {
    async fn on_tool_call(&self, _: &HookContext, _: ToolCall<'_>) -> ToolCallAction {
        ToolCallAction::rewrite(self.0.clone())
    }
}
struct SkipHook;
impl AgentHook for SkipHook {
    async fn on_tool_call(&self, _: &HookContext, _: ToolCall<'_>) -> ToolCallAction {
        ToolCallAction::skip("denied")
    }
}
#[derive(Clone, Default)]
struct ArgsSpy(Arc<Mutex<Vec<String>>>);
impl AgentHook for ArgsSpy {
    async fn on_tool_call(&self, _: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        self.0.lock().unwrap().push(event.args.into());
        ToolCallAction::run()
    }
}

struct OnToolCallOnly(Arc<AtomicUsize>);
impl AgentHook for OnToolCallOnly {
    async fn on_tool_call(&self, _: &HookContext, _: ToolCall<'_>) -> ToolCallAction {
        self.0.fetch_add(1, Ordering::Relaxed);
        ToolCallAction::skip("called")
    }
}

struct YieldingRewriteFromCallId;
impl AgentHook for YieldingRewriteFromCallId {
    async fn on_tool_call(&self, _: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        tokio::task::yield_now().await;
        ToolCallAction::rewrite(json!({"call_id": event.internal_call_id}))
    }
}

struct YieldingSkip;
impl AgentHook for YieldingSkip {
    async fn on_tool_call(&self, _: &HookContext, _: ToolCall<'_>) -> ToolCallAction {
        tokio::task::yield_now().await;
        ToolCallAction::skip("denied")
    }
}

async fn resolve(stack: &HookStack) -> (ToolCallAction, Option<Value>) {
    stack.resolve_tool_call(&ctx(), tool_call_event()).await
}

#[tokio::test]
async fn erased_dispatch_uses_the_public_on_tool_call_method() {
    let calls = Arc::new(AtomicUsize::new(0));
    let stack = HookStack::with(OnToolCallOnly(calls.clone()));

    let (action, salvaged) = resolve(&stack).await;

    assert_eq!(action, ToolCallAction::skip("called"));
    assert_eq!(salvaged, None);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn string_rewrite_is_json_encoded_for_later_hook_in_same_stack() {
    let spy = ArgsSpy::default();
    let replacement = Value::String("sanitized".into());
    let mut stack = HookStack::new();
    stack.push(RewriteHook(replacement.clone()));
    stack.push(spy.clone());

    let (action, salvaged) = resolve(&stack).await;

    assert_eq!(action, ToolCallAction::rewrite(replacement.clone()));
    assert_eq!(salvaged, None);
    assert_eq!(
        spy.0.lock().unwrap().as_slice(),
        [serde_json::to_string(&replacement).unwrap()]
    );
}

#[tokio::test]
async fn string_rewrite_is_json_encoded_for_hook_in_nested_stack() {
    let spy = ArgsSpy::default();
    let replacement = Value::String("sanitized".into());
    let inner = HookStack::with(spy.clone());
    let mut outer = HookStack::new();
    outer.push(RewriteHook(replacement.clone()));
    outer.push(inner);

    let (action, salvaged) = resolve(&outer).await;

    assert_eq!(action, ToolCallAction::rewrite(replacement.clone()));
    assert_eq!(salvaged, None);
    assert_eq!(
        spy.0.lock().unwrap().as_slice(),
        [serde_json::to_string(&replacement).unwrap()]
    );
}

#[tokio::test]
async fn nested_rewrite_then_skip_preserves_rewrite() {
    let mut inner = HookStack::new();
    inner.push(RewriteHook(json!({"x":41})));
    inner.push(SkipHook);
    let mut outer = HookStack::new();
    outer.push(inner);
    let (action, salvaged) = resolve(&outer).await;
    assert!(matches!(action, ToolCallAction::Skip(_)));
    assert_eq!(salvaged, Some(json!({"x":41})));
}

#[tokio::test]
async fn deeply_nested_terminal_action_preserves_the_last_rewrite() {
    let mut inner = HookStack::new();
    inner.push(RewriteHook(json!({"x":3})));
    inner.push(SkipHook);

    let mut middle = HookStack::new();
    middle.push(RewriteHook(json!({"x":2})));
    middle.push(inner);

    let mut outer = HookStack::new();
    outer.push(RewriteHook(json!({"x":1})));
    outer.push(middle);

    let (action, salvaged) = resolve(&outer).await;

    assert_eq!(action, ToolCallAction::skip("denied"));
    assert_eq!(salvaged, Some(json!({"x":3})));
}

#[tokio::test]
async fn concurrent_nested_resolutions_keep_rewrites_isolated_by_call() {
    let mut inner = HookStack::new();
    inner.push(YieldingRewriteFromCallId);
    inner.push(YieldingSkip);
    let outer = HookStack::with(inner);
    let context = ctx();

    let first = outer.resolve_tool_call(
        &context,
        ToolCall {
            internal_call_id: "first",
            ..tool_call_event()
        },
    );
    let second = outer.resolve_tool_call(
        &context,
        ToolCall {
            internal_call_id: "second",
            ..tool_call_event()
        },
    );
    let ((first_action, first_rewrite), (second_action, second_rewrite)) =
        tokio::join!(first, second);

    assert_eq!(first_action, ToolCallAction::skip("denied"));
    assert_eq!(first_rewrite, Some(json!({"call_id": "first"})));
    assert_eq!(second_action, ToolCallAction::skip("denied"));
    assert_eq!(second_rewrite, Some(json!({"call_id": "second"})));
}

#[tokio::test]
async fn outer_rewrite_threads_into_nested_stack() {
    let spy = ArgsSpy::default();
    let mut inner = HookStack::new();
    inner.push(spy.clone());
    inner.push(SkipHook);
    let mut outer = HookStack::new();
    outer.push(RewriteHook(json!({"x":1})));
    outer.push(inner);
    let (action, salvaged) = resolve(&outer).await;
    assert!(matches!(action, ToolCallAction::Skip(_)));
    assert_eq!(salvaged, Some(json!({"x":1})));
    assert_eq!(
        spy.0.lock().unwrap().as_slice(),
        [serde_json::to_string(&json!({"x":1})).unwrap()]
    );
}

#[tokio::test]
async fn nested_proceeding_rewrite_surfaces_as_rewrite_action() {
    let mut proceed = HookStack::new();
    proceed.push(RewriteHook(json!({"x":5})));
    let (action, salvaged) = resolve(&proceed).await;
    assert_eq!(action, ToolCallAction::rewrite(json!({"x":5})));
    assert_eq!(salvaged, None);
}

struct NeverResolvingHook;
impl AgentHook for NeverResolvingHook {
    async fn on_tool_call(&self, _: &HookContext, _: ToolCall<'_>) -> ToolCallAction {
        std::future::pending().await
    }
}

#[test]
fn run_id_displays_as_text() {
    let run_id = RunId::generate();
    assert!(!run_id.as_str().is_empty());
    assert_eq!(run_id.to_string(), run_id.as_str());
}

#[test]
fn rewrite_output_replaces_with_explicit_tool_output() {
    let output = ToolOutput::text("redacted");
    assert_eq!(
        ToolResultAction::rewrite_output(output.clone()),
        ToolResultAction::Rewrite(output)
    );
}

struct NoopHook;
impl AgentHook for NoopHook {}

#[test]
fn hook_stack_len_reports_registration_count() {
    let mut stack = HookStack::new();
    assert_eq!(stack.len(), 0);
    assert!(stack.is_empty());
    stack.push(NoopHook);
    stack.push(NoopHook);
    assert_eq!(stack.len(), 2);
    assert!(!stack.is_empty());
}

#[test]
fn hook_stack_debug_lists_registration_ids() {
    let mut stack = HookStack::new();
    stack.push(NoopHook);
    let debug = format!("{stack:?}");
    assert!(
        debug.contains("trait-0"),
        "the registration id is the stack's identity: {debug}"
    );
}

#[tokio::test]
async fn dropped_tool_call_dispatch_future_releases_its_resolution_frame() {
    let stack = HookStack::with(NeverResolvingHook);
    let context = ctx();

    // Poll the erased dispatch future once (creating its resolution
    // frame), then drop it unfinished: `ToolCallResolutionFrame::drop`
    // must clean the frame up so later resolutions stay balanced.
    let mut dispatch = stack.hooks[0].hook.tool_call(&context, tool_call_event());
    tokio::select! {
        biased;
        _ = &mut dispatch => panic!("the hook must never resolve"),
        _ = tokio::task::yield_now() => {}
    }
    drop(dispatch);

    // A later resolution for the same internal call id works normally.
    let later = HookStack::with(RewriteHook(json!({"x": 1})));
    let (action, salvaged) = later.resolve_tool_call(&context, tool_call_event()).await;
    assert_eq!(action, ToolCallAction::rewrite(json!({"x": 1})));
    assert_eq!(salvaged, None);
}

#[test]
fn action_types_are_event_specific() {
    fn call(_: ToolCallAction) {}
    fn result(_: ToolResultAction) {}
    call(ToolCallAction::run());
    result(ToolResultAction::keep());
    let calls = AtomicUsize::new(0);
    calls.fetch_add(1, Ordering::Relaxed);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn closure_records_order_by_priority_and_deny_is_absorbing() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let note = |label: &'static str| -> crate::agent::ToolCallFn {
        let seen = seen.clone();
        Box::new(move |_, _| {
            let seen = seen.clone();
            Box::pin(async move {
                seen.lock().unwrap().push(label);
                ToolCallAction::run()
            })
        })
    };
    // Registered late (auditor) but sorted first (priority -10); the
    // deny at 0 absorbs; the equal-priority tie keeps registration
    // order (first before second).
    let stack = HookStack::new()
        .hook(("first", 0), on::tool_call(note("first")))
        .hook(("auditor", -10), on::tool_call(note("auditor")))
        .hook(
            ("denier", 0),
            on::tool_call(|_, _| Box::pin(async { ToolCallAction::skip("no") })),
        )
        .hook(("second", 0), on::tool_call(note("second")));
    let action = AgentHook::on_tool_call(
        &stack,
        &HookContext::new(false, None, Default::default()),
        ToolCall {
            tool_name: "t",
            tool_call_id: None,
            internal_call_id: "i",
            args: "{}",
        },
    )
    .await;
    assert!(matches!(action, ToolCallAction::Skip(reason) if reason == "no"));
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["auditor", "first"],
        "priority orders; the deny absorbs before `second`"
    );
}
