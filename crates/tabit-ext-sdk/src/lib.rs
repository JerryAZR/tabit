#![cfg_attr(
    test,
    allow(
        clippy::err_expect,
        clippy::expect_used,
        clippy::panic,
        clippy::panic_in_result_fn,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]
// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point of this crate: the
// author-facing API stays `args["field"]`.
#![allow(clippy::indexing_slicing)]

//! The tabit extension SDK — the guest side of the frozen JSONL pipe
//! (ROADMAP item 9, task 2). It owns everything mechanical so an
//! author writes tool bodies and hook declarations: answer the
//! initialize, ack with the declared capabilities, dispatch tool
//! calls to bodies, serialize results, lift asks, and exit loud on a
//! malformed pipe.
//!
//! Deliberately boring: synchronous bodies on worker threads (the
//! main loop must keep reading while a body runs or an ask could
//! never be answered), one call dispatched per thread, no lifecycle
//! machinery. The frames are **hand-rolled here and share no code
//! with the host** — the SDK is the protocol doc's reference
//! consumer; if it can be written from the doc, any language
//! qualifies.
//!
//! ```no_run
//! tabit_ext_sdk::serve(tabit_ext_sdk::Extension::new(vec![
//!     tabit_ext_sdk::tool(
//!         "echo",
//!         "Echo the text back.",
//!         tabit_ext_sdk::schema_for!(["text"]),
//!         |args, _| {
//!             Ok(tabit_ext_sdk::Output::from(
//!                 args["text"].as_str().unwrap_or_default(),
//!             ))
//!         },
//!     ),
//! ]));
//! ```

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// The extension protocol this SDK speaks — must match the host's
/// exactly (the pipe is a frozen contract, not a negotiated one).
const PROTOCOL_VERSION: u64 = 1;

/// One extension's whole declaration.
pub struct Extension {
    /// The tools this process serves.
    pub tools: Vec<ToolDef>,
    /// The hook points it subscribes to (today: `tool_call`,
    /// `tool_result`).
    pub hooks: Vec<HookDef>,
}

impl Extension {
    pub fn new(tools: Vec<ToolDef>) -> Self {
        Self {
            tools,
            hooks: Vec::new(),
        }
    }

    /// Subscribe to hook points ([`hook`]).
    pub fn with_hooks(mut self, hooks: Vec<HookDef>) -> Self {
        self.hooks = hooks;
        self
    }
}

/// The result a tool body returns: the model-facing report plus the
/// optional details JSON (the engine's two-part result shape).
#[derive(Debug, Clone)]
pub struct Output {
    pub report: String,
    pub details: Option<Value>,
}

impl From<&str> for Output {
    fn from(report: &str) -> Self {
        Self {
            report: report.to_string(),
            details: None,
        }
    }
}

impl From<String> for Output {
    fn from(report: String) -> Self {
        Self {
            report,
            details: None,
        }
    }
}

impl Output {
    /// A result with structured details riding alongside the report.
    pub fn with_details(report: impl Into<String>, details: Value) -> Self {
        Self {
            report: report.into(),
            details: Some(details),
        }
    }
}

/// A tool body: arguments plus the ask handle (for the rare body
/// that needs the human mid-call).
pub type Body = Box<dyn Fn(Value, &Ask) -> Result<Output, String> + Send + Sync>;

/// One declared tool.
pub struct ToolDef {
    name: String,
    description: String,
    schema: Value,
    body: Body,
}

/// Declare a tool.
pub fn tool<F>(name: &str, description: &str, schema: Value, body: F) -> ToolDef
where
    F: Fn(Value, &Ask) -> Result<Output, String> + Send + Sync + 'static,
{
    ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        schema,
        body: Box::new(body),
    }
}

/// What a hook handler decided — the wire's `HookDecision`, in the
/// SDK's hand. v1: run the call, skip it with the in-band message,
/// or keep a result's presentation.
#[derive(Debug, Clone)]
pub enum Decision {
    Run,
    Skip { message: String },
    Keep,
}

impl Decision {
    pub fn run() -> Self {
        Decision::Run
    }
    pub fn skip(message: impl Into<String>) -> Self {
        Decision::Skip {
            message: message.into(),
        }
    }
    pub fn keep() -> Self {
        Decision::Keep
    }
}

/// A hook handler: the event payload plus the ask handle (a policy
/// may need the human mid-hook).
pub type HookBody = Arc<dyn Fn(Value, &Ask) -> Result<Decision, String> + Send + Sync>;

/// One declared hook subscription.
pub struct HookDef {
    pub event: String,
    body: HookBody,
}

/// Subscribe to a hook point: `hook("tool_call", |event, ask| ...)`.
/// The payload carries the event facts (`session`, `tool`, `args` —
/// and for `tool_result`, the presentation and outcome).
pub fn hook<F>(event: &str, body: F) -> HookDef
where
    F: Fn(Value, &Ask) -> Result<Decision, String> + Send + Sync + 'static,
{
    HookDef {
        event: event.to_string(),
        body: Arc::new(body),
    }
}

/// A minimal JSON Schema for an object with the given string
/// properties (the common shape; anything richer is a hand-written
/// `serde_json::json!` schema).
#[macro_export]
macro_rules! schema_for {
    ([$($field:literal),* $(,)?]) => {{
        let mut properties = ::serde_json::Map::new();
        $( properties.insert($field.to_string(), ::serde_json::json!({"type": "string"})); )*
        ::serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": [$($field),*],
        })
    }};
}

/// The ask handle a body may use: the host-service envelope, bound
/// to the in-flight call (verb zero is the ask; `model_prompt` is
/// verb one). Blocking calls on the body's thread, answered by
/// request id — the same pipe, one correlation namespace.
pub struct Ask {
    call_id: String,
    shared: Arc<Shared>,
}

/// One envelope reply as the SDK sees it: the verb's result value
/// (absent for a dismissal) or its error message.
struct ServiceReply {
    result: Option<Value>,
    error: Option<String>,
}

impl Ask {
    /// Ask the user (envelope verb zero): `ui_type` + opaque payload,
    /// mirroring the engine's interaction capability verbatim (core
    /// tools' `native:*` templates qualify). Blocks the body's thread
    /// until answered; a dismissal (nobody will ever answer) resolves
    /// `None` — fail closed.
    pub fn ask(&self, ui_type: &str, payload: Value) -> Option<Value> {
        let id = self.next_request_id("ask");
        let reply = self
            .request(
                &id,
                json!({"verb": "ask", "ui_type": ui_type, "payload": payload}),
            )
            .ok()?;
        reply.result
    }

    /// One model completion (envelope verb one): complete-only,
    /// `max_tokens` capped by the host; `model` is an optional
    /// `provider/model` (or bare-id) reference — absent means the
    /// session's current model. Usage bills to the session under
    /// this extension's name and rides the result.
    pub fn model_prompt(
        &self,
        prompt: &str,
        model: Option<&str>,
        max_tokens: Option<u64>,
    ) -> Result<ModelPrompt, String> {
        let id = self.next_request_id("svc");
        let mut verb = json!({"verb": "model_prompt", "prompt": prompt});
        if let Some(model) = model {
            verb["model"] = json!(model);
        }
        if let Some(max_tokens) = max_tokens {
            verb["max_tokens"] = json!(max_tokens);
        }
        let reply = self.request(&id, verb)?;
        match reply.error {
            Some(message) => Err(message),
            None => {
                let result = reply.result.unwrap_or_else(|| json!({}));
                Ok(ModelPrompt {
                    text: result["text"].as_str().unwrap_or_default().to_string(),
                    input_tokens: result["usage"]["input_tokens"].as_u64().unwrap_or_default(),
                    output_tokens: result["usage"]["output_tokens"]
                        .as_u64()
                        .unwrap_or_default(),
                    total_tokens: result["usage"]["total_tokens"].as_u64().unwrap_or_default(),
                })
            }
        }
    }

    /// One envelope roundtrip: emit the request (the call correlation
    /// plus the verb payload under the `service_request` type), await
    /// the reply by request id. `Err` is the pipe's end — the host is
    /// gone and no answer can ever come.
    fn request(&self, id: &str, mut verb: Value) -> Result<ServiceReply, String> {
        let (tx, rx) = std::sync::mpsc::channel::<ServiceReply>();
        tabit_ext_sdk_lock(&self.shared.asks).insert(id.to_string(), tx);
        // The frame is the envelope plus the verb's own fields (the
        // verb object carries exactly what it needs — nothing null).
        let mut frame = json!({
            "type": "service_request",
            "request_id": id,
            "call_id": self.call_id,
        });
        if let (Some(fields), Some(verb_fields)) = (frame.as_object_mut(), verb.as_object_mut()) {
            fields.append(verb_fields);
        }
        let sent = emit(&self.shared, frame);
        if !sent {
            tabit_ext_sdk_lock(&self.shared.asks).remove(id);
            return Err("the host closed the pipe".to_string());
        }
        let reply = rx
            .recv()
            .map_err(|_| "the host closed the pipe".to_string());
        tabit_ext_sdk_lock(&self.shared.asks).remove(id);
        reply
    }

    fn next_request_id(&self, family: &str) -> String {
        format!(
            "{}-{}-{}",
            self.call_id,
            family,
            SHARED_COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }
}

/// A completed `model_prompt` as the SDK hands it to the author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPrompt {
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

impl Ask {
    /// Whether the host cancelled this call's run (the run aborted
    /// under it). THE contract for long-running bodies: poll between
    /// units of work — kill the process, close the stream, stop —
    /// and return whatever partial result is honest. A body that
    /// never checks simply finishes into the void.
    pub fn is_cancelled(&self) -> bool {
        tabit_ext_sdk_lock(&self.shared.cancelled).contains(&self.call_id)
    }
}

static SHARED_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Everything the loop and the worker threads share.
struct Shared {
    stdout: std::sync::Mutex<()>,
    asks: Mutex<HashMap<String, std::sync::mpsc::Sender<ServiceReply>>>,
    /// Call ids the host cancelled (the run aborted under them) —
    /// long-running bodies poll [`Ask::is_cancelled`] and stop: kill
    /// the sandbox, drop the wedge, stop billing.
    cancelled: Mutex<std::collections::HashSet<String>>,
}

/// The dispatcher: answer the initialize, ack, then serve the pipe
/// until EOF. Never returns on success (the pipe's end is the end);
/// a malformed handshake or an unencodable frame exits loud.
pub fn serve(extension: Extension) -> ! {
    let Extension { tools, hooks } = extension;
    // Bodies are not clonable (they are the author's closures) — the
    // worker threads share them behind an Arc.
    let (tools, hooks) = (Arc::new(tools), Arc::new(hooks));
    let shared = Arc::new(Shared {
        stdout: std::sync::Mutex::new(()),
        asks: Mutex::new(HashMap::new()),
        cancelled: Mutex::new(std::collections::HashSet::new()),
    });

    // The handshake: the initialize must be the first line, and its
    // version must be ours exactly.
    let first = read_line();
    let initialize = serde_json::from_str::<Value>(&first)
        .map_err(|error| format!("the first line is not the initialize: {error}"))
        .unwrap_or_else(|reason| die(&reason));
    if initialize["type"] != "initialize" {
        die("the first line is not the initialize");
    }
    if initialize["protocol_version"].as_u64() != Some(PROTOCOL_VERSION) {
        die(&format!(
            "this host speaks protocol version {}, this extension speaks {PROTOCOL_VERSION}",
            initialize["protocol_version"]
        ));
    }
    emit(
        &shared,
        json!({
            "type": "ack",
            "protocol_version": PROTOCOL_VERSION,
            "tools": tools
                .iter()
                .map(|tool| json!({
                    "name": tool.name,
                    "description": tool.description,
                    "schema": tool.schema,
                }))
                .collect::<Vec<_>>(),
            "hooks": hooks
                .iter()
                .map(|hook| json!({"event": hook.event}))
                .collect::<Vec<_>>(),
        }),
    );

    // The dispatch loop: tool calls run on worker threads (a body may
    // block or ask — the loop must keep reading to deliver the
    // answer), everything else routes or is ignored.
    loop {
        let line = read_line();
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            // The double tolerates garbage; the SDK exits loud — a
            // malformed pipe is a broken host or a broken contract.
            die(&format!("unparseable line from the host: {line}"));
        };
        match frame["type"].as_str() {
            Some("tool_call") => {
                let call_id = frame["call_id"].as_str().unwrap_or_default().to_string();
                let name = frame["name"].as_str().unwrap_or_default().to_string();
                let args = frame["args"].clone();
                let (shared, tools) = (shared.clone(), tools.clone());
                std::thread::spawn(move || run_body(&shared, call_id, name, args, &tools));
            }
            Some("hook") => {
                let hook_id = frame["hook_id"].as_str().unwrap_or_default().to_string();
                let event = frame["event"].as_str().unwrap_or_default().to_string();
                let payload = frame["payload"].clone();
                let (shared, hooks) = (shared.clone(), hooks.clone());
                std::thread::spawn(move || {
                    let handler = hooks
                        .iter()
                        .find(|h| h.event == event)
                        .map(|h| h.body.clone());
                    let frame = run_hook(&shared, hook_id, &event, handler, payload);
                    let _ = emit(&shared, frame);
                });
            }
            Some("cancel") => {
                let id = frame["call_id"].as_str().unwrap_or_default().to_string();
                if !id.is_empty() {
                    tabit_ext_sdk_lock(&shared.cancelled).insert(id);
                }
            }
            Some("service_response") => {
                let id = frame["request_id"].as_str().unwrap_or_default().to_string();
                let reply = ServiceReply {
                    result: if frame["result"].is_null() {
                        None
                    } else {
                        Some(frame["result"].clone())
                    },
                    error: frame["error"].as_str().map(str::to_string),
                };
                if let Some(sender) = tabit_ext_sdk_lock(&shared.asks).get(&id) {
                    let _ = sender.send(reply);
                }
            }
            _ => {} // a newer host's frame: tolerated, ignored
        }
    }
}

/// One dispatched hook. **A failing hook is treated as absence**
/// (ruled 2026-09: dead or broken resolve identically — the neutral
/// decision, run for call points / keep for result points; a failed
/// *tool call* is the model-visible failure). The handler's own
/// error paths can choose otherwise; the SDK's failure handling
/// cannot.
fn run_hook(
    _shared: &Arc<Shared>,
    hook_id: String,
    event: &str,
    body: Option<HookBody>,
    payload: Value,
) -> Value {
    let neutral = if event == "tool_result" {
        json!({"type": "hook_result", "hook_id": hook_id, "decision": "keep"})
    } else {
        json!({"type": "hook_result", "hook_id": hook_id, "decision": "run"})
    };
    let Some(body) = body else {
        // No subscription: a newer host's event point this SDK
        // predates — absence.
        return neutral;
    };
    let ask = Ask {
        call_id: hook_id.clone(),
        shared: _shared.clone(),
    };
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| body(payload, &ask)));
    match outcome {
        Ok(Ok(decision)) => match decision {
            Decision::Run => json!({
                "type": "hook_result", "hook_id": hook_id, "decision": "run",
            }),
            Decision::Skip { message } => json!({
                "type": "hook_result", "hook_id": hook_id,
                "decision": "skip", "message": message,
            }),
            Decision::Keep => json!({
                "type": "hook_result", "hook_id": hook_id, "decision": "keep",
            }),
        },
        // Absence, by the ruling — but loud: the failure lands on
        // stderr where the host's report can find it.
        Ok(Err(error)) => {
            let _ = writeln!(
                std::io::stderr(),
                "tabit extension: hook handler failed: {error}"
            );
            neutral
        }
        Err(panic) => {
            let _ = writeln!(
                std::io::stderr(),
                "tabit extension: hook handler panicked: {}",
                panic_note(panic)
            );
            neutral
        }
    }
}

/// One dispatched call: find the body, run it (panics become error
/// results — the pipe never hangs on a broken body), send the result.
fn run_body(shared: &Arc<Shared>, call_id: String, name: String, args: Value, tools: &[ToolDef]) {
    let Some(tool) = tools.iter().find(|tool| tool.name == name) else {
        let _ = emit(
            shared,
            json!({
                "type": "tool_result", "call_id": call_id,
                "error": format!("this extension serves no tool `{name}`"),
                "report": "", "details": null,
            }),
        );
        return;
    };
    let ask = Ask {
        call_id: call_id.clone(),
        shared: shared.clone(),
    };
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| (tool.body)(args, &ask)));
    let frame = match outcome {
        Ok(Ok(output)) => json!({
            "type": "tool_result", "call_id": call_id,
            "error": null, "report": output.report, "details": output.details,
        }),
        Ok(Err(error)) => json!({
            "type": "tool_result", "call_id": call_id,
            "error": error, "report": "", "details": null,
        }),
        Err(panic) => json!({
            "type": "tool_result", "call_id": call_id,
            "error": format!("the tool body panicked: {}", panic_note(panic)),
            "report": "", "details": null,
        }),
    };
    let _ = emit(shared, frame);
}

fn panic_note(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = panic.downcast_ref::<String>() {
        text.clone()
    } else if let Some(text) = panic.downcast_ref::<&str>() {
        (*text).to_string()
    } else {
        "<no message>".to_string()
    }
}

fn emit(shared: &Arc<Shared>, frame: Value) -> bool {
    let _guard = tabit_ext_sdk_lock(&shared.stdout);
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let Ok(text) = serde_json::to_string(&frame) else {
        return false;
    };
    if writeln!(handle, "{text}")
        .and_then(|()| handle.flush())
        .is_err()
    {
        std::process::exit(0); // the pipe is gone: the end, not a failure
    }
    true
}

fn read_line() -> String {
    let mut line = String::new();
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    if lock.read_line(&mut line).unwrap_or(0) == 0 {
        std::process::exit(0); // EOF: the host closed, so are we
    }
    line
}

fn die(reason: &str) -> ! {
    let _ = writeln!(std::io::stderr(), "tabit extension: {reason}");
    std::process::exit(1);
}

/// The lock helper — same shape as the workspace's `tabit_log::lock`
/// claim (poison-recovering); local because this crate deliberately
/// depends on nothing above the protocol.
fn tabit_ext_sdk_lock<T: ?Sized>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            stdout: std::sync::Mutex::new(()),
            asks: Mutex::new(HashMap::new()),
            cancelled: Mutex::new(std::collections::HashSet::new()),
        })
    }

    fn decision_of(frame: &Value) -> &str {
        frame["decision"].as_str().unwrap_or_default()
    }

    /// The failure symmetry (ruled 2026-09): a failing hook is
    /// treated as absence — the neutral decision for its point, dead
    /// or broken alike.
    #[test]
    fn a_failing_handler_is_absence_run_for_call_points() {
        let frame = run_hook(
            &shared(),
            "h-1".to_string(),
            "tool_call",
            Some(Arc::new(|_payload, _ask| Err("boom".to_string()))),
            json!({}),
        );
        assert_eq!(decision_of(&frame), "run");
    }

    #[test]
    fn a_failing_handler_is_absence_keep_for_result_points() {
        let frame = run_hook(
            &shared(),
            "h-2".to_string(),
            "tool_result",
            Some(Arc::new(|_payload, _ask| Err("boom".to_string()))),
            json!({}),
        );
        assert_eq!(decision_of(&frame), "keep");
    }

    #[test]
    #[allow(clippy::panic)]
    fn a_panicking_handler_is_absence_too() {
        let frame = run_hook(
            &shared(),
            "h-3".to_string(),
            "tool_call",
            Some(Arc::new(|_payload, _ask| panic!("handler broke"))),
            json!({}),
        );
        assert_eq!(decision_of(&frame), "run");
    }

    #[test]
    fn an_unsubscribed_point_is_absence() {
        let frame = run_hook(&shared(), "h-4".to_string(), "tool_call", None, json!({}));
        assert_eq!(decision_of(&frame), "run");
    }

    #[test]
    fn a_host_cancel_flips_the_bodys_view_of_its_call() {
        // The author surface a sandboxed-bash body polls: false
        // before the host's cancel frame, true after, keyed to the
        // call (a sibling call is untouched).
        let shared = shared();
        let ask = Ask {
            call_id: "echo-2".to_string(),
            shared: shared.clone(),
        };
        let sibling = Ask {
            call_id: "echo-3".to_string(),
            shared: shared.clone(),
        };
        assert!(!ask.is_cancelled());
        tabit_ext_sdk_lock(&shared.cancelled).insert("echo-2".to_string());
        assert!(ask.is_cancelled());
        assert!(!sibling.is_cancelled(), "cancellation is per call");
    }

    #[test]
    fn a_healthy_decision_crosses_untouched() {
        let frame = run_hook(
            &shared(),
            "h-5".to_string(),
            "tool_call",
            Some(Arc::new(|_payload, _ask| Ok(Decision::skip("denied")))),
            json!({}),
        );
        assert_eq!(decision_of(&frame), "skip");
        assert_eq!(frame["message"], "denied");
    }
}
