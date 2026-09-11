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
    /// The engine hook points it subscribes to (today: `tool_call`,
    /// `tool_result` — hook *events* are not delivered yet; the
    /// declaration mounts now so handshakes do not drift later).
    pub hooks: Vec<String>,
}

impl Extension {
    pub fn new(tools: Vec<ToolDef>) -> Self {
        Self {
            tools,
            hooks: Vec::new(),
        }
    }

    /// Subscribe to hook points (see [`Extension::tools`]'s sibling
    /// field doc).
    pub fn with_hooks(mut self, hooks: Vec<String>) -> Self {
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

/// The ask handle a body may use: one question to the user over the
/// same pipe, answered by id.
pub struct Ask {
    call_id: String,
    shared: Arc<Shared>,
}

impl Ask {
    /// Ask the user: `ui_type` + opaque payload, mirroring the
    /// engine's interaction capability verbatim (core tools'
    /// `native:*` templates qualify). Blocks the body's thread until
    /// answered; a dismissal (nobody will ever answer) resolves
    /// `None` — fail closed.
    pub fn ask(&self, ui_type: &str, payload: Value) -> Option<Value> {
        let id = format!(
            "{}-ask-{}",
            self.call_id,
            SHARED_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = std::sync::mpsc::channel::<Option<Value>>();
        tabit_ext_sdk_lock(&self.shared.asks).insert(id.clone(), tx);
        let sent = emit(
            &self.shared,
            json!({
                "type": "interaction_request",
                "call_id": self.call_id,
                "id": id,
                "ui_type": ui_type,
                "payload": payload,
            }),
        );
        if !sent {
            tabit_ext_sdk_lock(&self.shared.asks).remove(&id);
            return None;
        }
        let answer = rx.recv().ok().flatten();
        tabit_ext_sdk_lock(&self.shared.asks).remove(&id);
        answer
    }
}

static SHARED_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Everything the loop and the worker threads share.
struct Shared {
    stdout: std::sync::Mutex<()>,
    asks: Mutex<HashMap<String, std::sync::mpsc::Sender<Option<Value>>>>,
}

/// The dispatcher: answer the initialize, ack, then serve the pipe
/// until EOF. Never returns on success (the pipe's end is the end);
/// a malformed handshake or an unencodable frame exits loud.
pub fn serve(extension: Extension) -> ! {
    let Extension { tools, hooks } = extension;
    // Tool bodies are not clonable (they are the author's closures) —
    // the worker threads share them behind an Arc.
    let tools = Arc::new(tools);
    let shared = Arc::new(Shared {
        stdout: std::sync::Mutex::new(()),
        asks: Mutex::new(HashMap::new()),
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
            "hooks": hooks,
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
            Some("interaction_response") => {
                let id = frame["id"].as_str().unwrap_or_default().to_string();
                let outcome = match &frame["outcome"] {
                    Value::Null => None,
                    other => Some(other.clone()),
                };
                if let Some(sender) = tabit_ext_sdk_lock(&shared.asks).get(&id) {
                    let _ = sender.send(outcome);
                }
            }
            _ => {} // a newer host's frame: tolerated, ignored
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
