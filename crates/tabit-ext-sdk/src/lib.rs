//! The tabit extension SDK: the guest library for the frozen pipe.
//! Authors register tools, consultations, and watched event kinds;
//! the SDK owns the loop (handshake, dispatch, the unconditional
//! drain), derives the handshake's declarations from the
//! registration, and hands every handler one context object
//! carrying the utilities. Handlers are plain functions — they may
//! block, ask, emit, command — and never the loop: every invocation
//! runs on its own worker thread, so tools and hooks execute
//! concurrently and a blocked body cannot stall the pipe.
//!
//! The registries are disjoint by vocabulary (the category error is
//! structural — the two handler shapes cannot be confused):
//!
//! - **tools** — the model calls them; the body returns its result.
//! - **consultations** (`consult`) — the engine's hook points; the
//!   handler returns a decision (run/skip, keep), and the host
//!   waits for it.
//! - **watches** (`watch`) — event kinds (the frontend grammar's
//!   wire tags, [`tabit_protocol::tags`]); the handler observes,
//!   returns nothing.
//!
//! The context ([`Ctx`]) exposes the four directions: `command`
//! (any session command, frontend-grade addressing), `emit` (any
//! session event — surfaced origin-stamped), `ask` (emit an
//! interaction request, await the routed answer), `complete` (one
//! bare model completion, call-correlated), and `cancelled` (the
//! cooperative abort poll). The SDK shares the host's wire types
//! (the 2026-09 sharing ruling: one wire, one set of shapes — the
//! docs stay the contract for other languages, the conformance
//! tests keep crate and docs honest).

// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point: the
// author-facing API stays `args["field"]`.
#![cfg_attr(test, allow(clippy::expect_used, clippy::panic, clippy::unwrap_used))]
#![allow(clippy::indexing_slicing, clippy::type_complexity)]

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{Value, json};
use tabit_ext::protocol::{
    ExtFrame, HOOK_POINTS, HookDecision, HookResult, HostFrame, ServiceVerb, ToolWireResult,
};
use tabit_protocol::{EventFrame, SessionCommand, SessionEvent};

/// The extension protocol this SDK speaks — must match the host's
/// exactly (the pipe is a frozen contract, not a negotiated one).
const PROTOCOL_VERSION: u32 = 3;

pub mod children;

pub use children::{Child, ChildOptions};

/// One extension's whole declaration, built by registering tools,
/// consultations, and watches; `serve` derives the handshake from
/// it. Nothing is declared twice — the registration IS the ack.
pub struct Extension {
    tools: Vec<ToolDef>,
    consults: Vec<ConsultDef>,
    watches: Vec<WatchDef>,
}

impl Default for Extension {
    fn default() -> Self {
        Self::new()
    }
}

impl Extension {
    /// An empty registration.
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            consults: Vec::new(),
            watches: Vec::new(),
        }
    }

    /// Serve one tool (see [`tool`]).
    pub fn tool(mut self, tool: ToolDef) -> Self {
        self.tools.push(tool);
        self
    }

    /// Serve tools in bulk.
    pub fn tools(mut self, tools: Vec<ToolDef>) -> Self {
        self.tools.extend(tools);
        self
    }

    /// Register one consultation (see [`consult`]).
    pub fn consult(mut self, consult: ConsultDef) -> Self {
        self.consults.push(consult);
        self
    }

    /// Register one watch (see [`watch`]).
    pub fn watch(mut self, watch: WatchDef) -> Self {
        self.watches.push(watch);
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

/// A tool body: the arguments plus the context (blocking calls, ask
/// the user, emit, command — the loop keeps reading regardless).
pub type Body = Box<dyn Fn(Value, &Ctx) -> Result<Output, String> + Send + Sync>;

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
    F: Fn(Value, &Ctx) -> Result<Output, String> + Send + Sync + 'static,
{
    ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        schema,
        body: Box::new(body),
    }
}

/// What a consultation decided — the wire's `HookDecision`, in the
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

/// A consultation handler: the event payload plus the context (a
/// policy may need the human mid-hook). The host waits for the
/// decision.
pub type ConsultBody = Box<dyn Fn(&Ctx, Value) -> Result<Decision, String> + Send + Sync>;

/// One declared consultation. The point must be one of the host's
/// hook points (`HOOK_POINTS`) — anything else refuses loudly at
/// registration, not silently mid-run.
pub struct ConsultDef {
    pub point: String,
    body: ConsultBody,
}

/// Declare a consultation on one hook point.
pub fn consult<F>(point: &str, body: F) -> ConsultDef
where
    F: Fn(&Ctx, Value) -> Result<Decision, String> + Send + Sync + 'static,
{
    if !HOOK_POINTS.contains(&point) {
        die(&format!(
            "unknown hook point `{point}` (known: {})",
            HOOK_POINTS.join(", ")
        ));
    }
    ConsultDef {
        point: point.to_string(),
        body: Box::new(body),
    }
}

/// A watch handler: the typed event plus the context. Observation
/// only — nothing is owed back, nothing is cancelled (no pending
/// entry exists for a watch).
pub type WatchBody = Box<dyn Fn(&Ctx, SessionEvent) + Send + Sync>;

/// One declared watch. The kind must name a real event kind (a
/// wire tag, [`tabit_protocol::tags`]) — a typo'd kind watches
/// nothing, so it refuses loudly at registration instead.
pub struct WatchDef {
    pub kind: String,
    body: Arc<WatchBody>,
}

/// Declare a watch on one event kind.
pub fn watch<F>(kind: &str, body: F) -> WatchDef
where
    F: Fn(&Ctx, SessionEvent) + Send + Sync + 'static,
{
    if !SessionEvent::is_known_tag(kind) {
        die(&format!(
            "unknown event kind `{kind}` (the spellings live in tabit_protocol::tags)"
        ));
    }
    WatchDef {
        kind: kind.to_string(),
        body: Arc::new(Box::new(body)),
    }
}

impl WatchDef {
    fn arc_body(&self) -> &Arc<WatchBody> {
        &self.body
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

/// The handler context: the four directions plus the abort poll.
/// Cheap to hold (one `Arc`); valid for the handler's invocation —
/// the correlation it carries names the call or consultation the
/// host would cancel.
pub struct Ctx {
    correlation: Option<String>,
    shared: Arc<Shared>,
}

impl Ctx {
    /// A watch-shaped context: no correlation (nothing owed, nothing
    /// cancelled). The dispatcher builds these for observation and
    /// child-event handlers.
    pub(crate) fn watch_context(shared: Arc<Shared>) -> Self {
        Self {
            correlation: None,
            shared,
        }
    }

    pub(crate) fn shared_clone(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// The host's own executable path (the initialize's
    /// `core_path`) — the thing owned children spawn.
    pub(crate) fn core_path(&self) -> Result<String, String> {
        self.shared
            .core_path
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "the host named no core_path at the handshake".to_string())
    }
}

impl Ctx {
    /// Whether the host cancelled this invocation's run (the run
    /// aborted under it). THE contract for long-running bodies: poll
    /// between units of work — kill the process, close the stream,
    /// stop — and return whatever partial result is honest. A body
    /// that never checks simply finishes into the void. Watches have
    /// no cancellation (nothing is owed); their flag never flips.
    pub fn cancelled(&self) -> bool {
        match &self.correlation {
            Some(id) => sdk_lock(&self.shared.cancelled).contains(id),
            None => false,
        }
    }

    /// Issue any session command — the frontend grammar verbatim,
    /// frontend-grade addressing (the ids arrive on watched events:
    /// `session_opened`, the boot announcement). Fire-and-forget:
    /// effects arrive as the events you watch.
    pub fn command(&self, command: SessionCommand) {
        let _ = emit(&self.shared, &command);
    }

    /// Emit any session event into the shared grammar — it surfaces
    /// to the frontend and subscribers, origin-stamped with this
    /// extension's id (attribution, not permission).
    pub fn emit(&self, event: SessionEvent) {
        let _ = emit(&self.shared, &event);
    }

    /// Ask the user: emit an `interaction_request` into the shared
    /// grammar and await the routed `interaction_response` by id.
    /// `ui_type` + opaque payload mirror the templates core tools
    /// use (`native:*` qualifies). Blocks the handler's thread until
    /// answered; the cancellation of this invocation's run resolves
    /// `None` (the card may still be answered behind us — a late
    /// response finds no waiter and drops), and the pipe's death
    /// resolves `None` too — fail closed.
    pub fn ask(&self, ui_type: &str, payload: Value) -> Option<Value> {
        let id = next_request_id(self.correlation.as_deref().unwrap_or("watch"), "ask");
        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        sdk_lock(&self.shared.grammar_asks).insert(id.clone(), tx);
        let sent = emit(
            &self.shared,
            &SessionEvent::InteractionRequest {
                id: id.clone(),
                ui_type: ui_type.to_string(),
                payload,
            },
        );
        if !sent {
            sdk_lock(&self.shared.grammar_asks).remove(&id);
            return None;
        }
        // The wait honors cancellation: the run aborting under this
        // invocation abandons the ask (the host-side card settles
        // whenever the user answers it; the routed response then
        // finds no waiter).
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(answer) => {
                    sdk_lock(&self.shared.grammar_asks).remove(&id);
                    return Some(answer);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if self.cancelled() {
                        sdk_lock(&self.shared.grammar_asks).remove(&id);
                        return None;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// One bare model completion (the envelope's one verb):
    /// complete-only, `max_tokens` capped by the host. `model` is an
    /// optional provider/model or bare-id reference; absent means
    /// the session's current model. Usage bills to the session under
    /// this extension's name and rides the result. Needs a call or
    /// consultation correlation — a watch observes, it does not
    /// spend.
    pub fn complete(
        &self,
        prompt: &str,
        model: Option<&str>,
        max_tokens: Option<u64>,
    ) -> Result<ModelPrompt, String> {
        let Some(call_id) = &self.correlation else {
            return Err(
                "model completions need a call correlation — a watch observes, it does not spend"
                    .to_string(),
            );
        };
        let id = next_request_id(call_id, "svc");
        let mut verb = ServiceVerb::ModelPrompt {
            prompt: prompt.to_string(),
            model: None,
            max_tokens,
        };
        if let (Some(model), ServiceVerb::ModelPrompt { model: slot, .. }) = (model, &mut verb) {
            *slot = Some(model.to_string());
        }
        let reply = request(&self.shared, call_id, &id, verb)?;
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
}

/// One envelope reply as the SDK sees it: the verb's result value
/// (absent for a dismissal) or its error message.
struct ServiceReply {
    result: Option<Value>,
    error: Option<String>,
}

/// A completed model completion as the SDK hands it to the author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPrompt {
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

static SHARED_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_request_id(family_root: &str, family: &str) -> String {
    format!(
        "{family_root}-{family}-{}",
        SHARED_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Everything the loop and the worker threads share.
struct Shared {
    stdout: std::sync::Mutex<()>,
    /// Envelope requests in flight, by request id.
    asks: Mutex<HashMap<String, std::sync::mpsc::Sender<ServiceReply>>>,
    /// Grammar asks in flight: interaction-request ids the extension
    /// minted, awaiting their routed interaction responses.
    grammar_asks: Mutex<HashMap<String, std::sync::mpsc::Sender<Value>>>,
    /// Correlation ids (calls, consultations) the host cancelled —
    /// long-running handlers poll [`Ctx::cancelled`] and stop: kill
    /// the sandbox, drop the wedge, stop billing.
    cancelled: Mutex<std::collections::HashSet<String>>,
    /// The host's own executable (the initialize's `core_path`) —
    /// owned children spawn it.
    core_path: Mutex<Option<String>>,
}

/// The dispatcher: answer the initialize, ack (the registration
/// derived), then serve the pipe until EOF. Never returns on success
/// (the pipe's end is the end); a malformed handshake or an
/// unencodable frame exits loud.
pub fn serve(extension: Extension) -> ! {
    let Extension {
        tools,
        consults,
        watches,
    } = extension;
    let (tools, consults, watches) = (Arc::new(tools), Arc::new(consults), Arc::new(watches));
    let shared = Arc::new(Shared {
        stdout: std::sync::Mutex::new(()),
        asks: Mutex::new(HashMap::new()),
        grammar_asks: Mutex::new(HashMap::new()),
        cancelled: Mutex::new(std::collections::HashSet::new()),
        core_path: Mutex::new(None),
    });

    // The handshake: the initialize must be the first line, and its
    // version must be ours exactly. The host facts ride it; the
    // core's own executable is the owned-children spawner's path.
    let first = read_line();
    let initialize = serde_json::from_str::<Value>(&first)
        .map_err(|error| format!("the first line is not the initialize: {error}"))
        .unwrap_or_else(|reason| die(&reason));
    if initialize["type"] != "initialize" {
        die("the first line is not the initialize");
    }
    if initialize["protocol_version"].as_u64() != Some(PROTOCOL_VERSION as u64) {
        die(&format!(
            "this host speaks protocol version {}, this extension speaks {PROTOCOL_VERSION}",
            initialize["protocol_version"]
        ));
    }
    *sdk_lock(&shared.core_path) = initialize["core_path"].as_str().map(str::to_string);
    let ack = ExtFrame::Ack {
        protocol_version: PROTOCOL_VERSION,
        tools: tools
            .iter()
            .map(|tool| tabit_ext::protocol::ToolDecl {
                name: tool.name.clone(),
                description: tool.description.clone(),
                schema: tool.schema.clone(),
            })
            .collect(),
        hooks: consults
            .iter()
            .map(|consult| tabit_ext::protocol::HookDecl {
                event: consult.point.clone(),
            })
            .collect(),
        watch: watches.iter().map(|w| w.kind.clone()).collect(),
    };
    emit(&shared, &ack);

    // The dispatch loop: every invocation (a tool call, a
    // consultation, a watched event) runs on its own worker thread —
    // handlers may block, concurrently, and the loop keeps reading.
    loop {
        let line = read_line();
        dispatch_line(&shared, &line, &tools, &consults, &watches);
    }
}

/// One inbound line: the parse cascade (host lanes first, then the
/// shared grammar's frames), then the routing.
fn dispatch_line(
    shared: &Arc<Shared>,
    line: &str,
    tools: &Arc<Vec<ToolDef>>,
    consults: &Arc<Vec<ConsultDef>>,
    watches: &Arc<Vec<WatchDef>>,
) {
    if let Ok(host) = serde_json::from_str::<HostFrame>(line) {
        match host {
            HostFrame::ToolCall {
                call_id,
                name,
                args,
            } => {
                let (shared, tools) = (shared.clone(), tools.clone());
                std::thread::spawn(move || run_body(&shared, call_id, name, args, &tools));
            }
            HostFrame::Hook {
                hook_id,
                event,
                payload,
            } => {
                let (shared, consults) = (shared.clone(), consults.clone());
                std::thread::spawn(move || {
                    let frame = run_consult(&shared, hook_id, &event, &consults, payload);
                    let _ = emit(&shared, &frame);
                });
            }
            HostFrame::Cancel { call_id } => {
                sdk_lock(&shared.cancelled).insert(call_id);
            }
            HostFrame::ServiceResponse {
                request_id,
                result,
                error,
            } => {
                let reply = ServiceReply { result, error };
                if let Some(sender) = sdk_lock(&shared.asks).remove(&request_id) {
                    let _ = sender.send(reply);
                }
            }
            HostFrame::Initialize { .. } => {} // a re-send: tolerated, ignored
        }
        return;
    }
    if let Ok(frame) = serde_json::from_str::<EventFrame>(line) {
        // A watched event, typed. Every matching watch fires, each on
        // its own thread.
        let kind = frame.event.tag();
        for watch in watches.iter() {
            if watch.kind == kind {
                let (shared, body) = (shared.clone(), watch.arc_body().clone());
                let event = frame.event.clone();
                std::thread::spawn(move || {
                    let ctx = Ctx {
                        correlation: None,
                        shared,
                    };
                    catch_unwind_silently(move || body(&ctx, event));
                });
            }
        }
        return;
    }
    if let Ok(SessionCommand::InteractionResponse { id, payload, .. }) =
        serde_json::from_str::<SessionCommand>(line)
    {
        // A routed answer to one of our grammar asks: resolve by id;
        // a late response for a gone waiter drops.
        if let Some(sender) = sdk_lock(&shared.grammar_asks).remove(&id) {
            let _ = sender.send(payload);
        }
        return;
    }
    // The double tolerates garbage; the SDK exits loud — a malformed
    // pipe is a broken host or a broken contract.
    die(&format!("unparseable line from the host: {line}"));
}

/// One dispatched tool call: find the body, run it (panics become
/// error results — the pipe never hangs on a broken body), send the
/// result.
fn run_body(shared: &Arc<Shared>, call_id: String, name: String, args: Value, tools: &[ToolDef]) {
    let Some(tool) = tools.iter().find(|tool| tool.name == name) else {
        let _ = emit(
            shared,
            &ExtFrame::ToolResult(ToolWireResult {
                call_id,
                error: Some(format!("this extension serves no tool `{name}`")),
                report: String::new(),
                details: None,
            }),
        );
        return;
    };
    let ctx = Ctx {
        correlation: Some(call_id.clone()),
        shared: shared.clone(),
    };
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| (tool.body)(args, &ctx)));
    let frame = match outcome {
        Ok(Ok(output)) => ExtFrame::ToolResult(ToolWireResult {
            call_id,
            error: None,
            report: output.report,
            details: output.details,
        }),
        Ok(Err(error)) => ExtFrame::ToolResult(ToolWireResult {
            call_id,
            error: Some(error),
            report: String::new(),
            details: None,
        }),
        Err(panic) => ExtFrame::ToolResult(ToolWireResult {
            call_id,
            error: Some(format!("the tool body panicked: {}", panic_note(panic))),
            report: String::new(),
            details: None,
        }),
    };
    let _ = emit(shared, &frame);
}

/// One dispatched consultation. **A failing handler is treated as
/// absence** (ruled 2026-09: dead or broken resolve identically —
/// the neutral decision, run for call points / keep for result
/// points; a failed *tool call* is the model-visible failure). The
/// handler's own error paths can choose otherwise; the SDK's
/// failure handling cannot.
fn run_consult(
    shared: &Arc<Shared>,
    hook_id: String,
    event: &str,
    consults: &[ConsultDef],
    payload: Value,
) -> ExtFrame {
    fn neutral(point: &str) -> HookDecision {
        if point == "tool_result" {
            HookDecision::Keep
        } else {
            HookDecision::Run
        }
    }
    let Some(consult) = consults.iter().find(|consult| consult.point == event) else {
        // No subscription: a newer host's event point this SDK
        // predates — absence.
        return ExtFrame::HookResult(HookResult {
            hook_id,
            decision: neutral(event),
        });
    };
    let ctx = Ctx {
        correlation: Some(hook_id.clone()),
        shared: shared.clone(),
    };
    let decision = std::panic::catch_unwind(AssertUnwindSafe(|| (consult.body)(&ctx, payload)))
        .unwrap_or_else(|_| Err("the consultation handler panicked".to_string()));
    let decision = decision.unwrap_or_else(|error| {
        let _ = writeln!(
            std::io::stderr(),
            "tabit extension: the handler failed: {error}"
        );
        match neutral(event) {
            HookDecision::Keep => Decision::Keep,
            _ => Decision::Run,
        }
    });
    let wire = match decision {
        Decision::Run => HookDecision::Run,
        Decision::Skip { message } => HookDecision::Skip { message },
        Decision::Keep => HookDecision::Keep,
    };
    ExtFrame::HookResult(HookResult {
        hook_id,
        decision: wire,
    })
}

/// A watch body's panic is reported, never fatal — observation must
/// not take the process down.
/// Register a relayed child ask: the routed answer resolves through
/// the same pending map the extension's own asks use; the receiver
/// carries it home to the child.
pub(crate) fn register_relay(
    shared: &Arc<Shared>,
    id: &str,
    sender: std::sync::mpsc::Sender<Value>,
) {
    sdk_lock(&shared.grammar_asks).insert(id.to_string(), sender);
}

pub(crate) fn catch_unwind_silently(body: impl FnOnce()) {
    if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(body)) {
        let _ = writeln!(
            std::io::stderr(),
            "tabit extension: a watch handler panicked: {}",
            panic_note(panic)
        );
    }
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

fn emit<T: Serialize>(shared: &Arc<Shared>, frame: &T) -> bool {
    let _guard = sdk_lock(&shared.stdout);
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let Ok(text) = serde_json::to_string(frame) else {
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

/// One envelope roundtrip: emit the request (the call correlation
/// plus the verb's payload under the `service_request` frame), await
/// the reply by request id. `Err` is the pipe's end — the host is
/// gone and no answer can ever come.
fn request(
    shared: &Arc<Shared>,
    call_id: &str,
    id: &str,
    verb: ServiceVerb,
) -> Result<ServiceReply, String> {
    let (tx, rx) = std::sync::mpsc::channel::<ServiceReply>();
    sdk_lock(&shared.asks).insert(id.to_string(), tx);
    let frame = ExtFrame::ServiceRequest {
        request_id: id.to_string(),
        call_id: call_id.to_string(),
        verb,
    };
    if !emit(shared, &frame) {
        sdk_lock(&shared.asks).remove(id);
        return Err("the host closed the pipe".to_string());
    }
    let reply = rx
        .recv()
        .map_err(|_| "the host closed the pipe".to_string());
    sdk_lock(&shared.asks).remove(id);
    reply
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
/// claim (poison-recovering).
pub(crate) fn sdk_lock<T: ?Sized>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            stdout: std::sync::Mutex::new(()),
            asks: Mutex::new(HashMap::new()),
            grammar_asks: Mutex::new(HashMap::new()),
            cancelled: Mutex::new(std::collections::HashSet::new()),
            core_path: Mutex::new(None),
        })
    }

    #[test]
    fn a_failing_consultation_is_absence_run_for_call_points() {
        let shared = shared();
        let consults = [consult("tool_call", |_ctx, _payload| {
            Err("boom".to_string())
        })];
        let frame = run_consult(
            &shared,
            "h-1".to_string(),
            "tool_call",
            &consults,
            json!({}),
        );
        match frame {
            ExtFrame::HookResult(HookResult {
                decision: HookDecision::Run,
                ..
            }) => {}
            other => panic!("the neutral decision for a call point: {other:?}"),
        }
    }

    #[test]
    fn a_failing_consultation_is_absence_keep_for_result_points() {
        let shared = shared();
        let consults = [consult("tool_result", |_ctx, _payload| {
            Err("boom".to_string())
        })];
        let frame = run_consult(
            &shared,
            "h-2".to_string(),
            "tool_result",
            &consults,
            json!({}),
        );
        match frame {
            ExtFrame::HookResult(HookResult {
                decision: HookDecision::Keep,
                ..
            }) => {}
            other => panic!("the neutral decision for a result point: {other:?}"),
        }
    }

    #[test]
    fn an_unsubscribed_point_is_absence() {
        let shared = shared();
        let frame = run_consult(&shared, "h-3".to_string(), "tool_call", &[], json!({}));
        match frame {
            ExtFrame::HookResult(HookResult {
                decision: HookDecision::Run,
                ..
            }) => {}
            other => panic!("absence is the neutral decision: {other:?}"),
        }
    }
}
