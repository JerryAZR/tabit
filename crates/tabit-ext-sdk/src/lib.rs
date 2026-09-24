//! The tabit extension SDK: the guest's functional layer over its
//! node. Authors register tools, consultations, and watched event
//! kinds; the SDK owns the pipe (handshake, the frozen dialect's
//! lanes, the unconditional drain) and derives the handshake's
//! declarations from the registration. The routing — whose frames
//! cross, how answers walk home, what a death sweeps — is the
//! guest's node under the SDK (the node architecture, 2026-09):
//! authors never meet the router or the channel concepts.
//!
//! The registries are disjoint by vocabulary (the category error is
//! structural — the two handler shapes cannot be confused):
//!
//! - **tools** — the model calls them; the body returns its result.
//! - **consultations** (`consult`) — the engine's hook points, one
//!   declaration per point ([`tabit_protocol::points`]); the handler
//!   returns the point's own answer type (a gate returns a verdict,
//!   an observer returns `()`), and the host waits for it.
//! - **watches** (`watch`) — event kinds (the frontend grammar's
//!   wire tags, [`tabit_protocol::tags`]); the handler observes,
//!   returns nothing.
//!
//! The context ([`Ctx`]) exposes the four directions: `command`
//! (any session command, frontend-grade addressing), `emit` (any
//! session event — it crosses the pipe by the additional-receiver
//! path and surfaces origin-stamped), `ask` (emit an interaction
//! request, await the routed answer), `complete` (one bare model
//! completion, call-correlated), and `cancelled` (the cooperative
//! abort poll). Handlers are plain functions — they may block, ask,
//! emit, command — and never the loop: every invocation runs on its
//! own worker thread, so tools and hooks execute concurrently and a
//! blocked body cannot stall the pipe.
//!
//! The wire laws this side of the pipe: a tool call, a hook, a
//! service request are asks (the taxonomy ruling) — each arriving
//! call is held on the node's ask table against the host's channel
//! and answered through it; an extension's own ask crosses by
//! [`Node::ask_on`]'s additional receivers (the override path — the
//! extension's stdio subscribes to nothing by default); the settle
//! announce rides the same fan, so the host's transit card closes.
//! The SDK shares the host's wire types (the 2026-09 sharing
//! ruling: one wire, one set of shapes — the docs stay the contract
//! for other languages, the conformance tests keep crate and docs
//! honest).

// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point of this crate: the
// author-facing API stays `args["field"]`.
#![cfg_attr(test, allow(clippy::expect_used, clippy::panic, clippy::unwrap_used))]
#![allow(clippy::indexing_slicing, clippy::type_complexity)]

use std::io::{BufRead, Write};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{Value, json};
use tabit_ext::protocol::{
    ExtFrame, HookResult, HostFrame, KIND_HOOK_RESULT, KIND_SERVICE_RESPONSE, KIND_TOOL_RESULT,
    ServiceVerb, ToolWireResult,
};
use tabit_protocol::points::HookPoint;
use tabit_protocol::{EventFrame, SessionCommand, SessionEvent, tags};
use tabit_wire::asks::Outcome;
use tabit_wire::node::{Channel, Node, parse_shared};

/// The extension protocol this SDK speaks — must match the host's
/// exactly (the pipe is a frozen contract, not a negotiated one).
const PROTOCOL_VERSION: u32 = 4;

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

/// A consultation handler after type erasure: the event payload plus
/// the context, returning the point's answer serialized (the host
/// waits for it).
type ErasedConsult = Box<dyn Fn(&Ctx, Value) -> Result<Value, String> + Send + Sync>;

/// One declared consultation. The point and its answer type pair at
/// registration — [`consult::<P>`] is the only constructor, so a
/// `tool_result` consult cannot return a verdict.
pub struct ConsultDef {
    pub point: &'static str,
    body: ErasedConsult,
}

/// Declare a consultation on one hook point: the body returns the
/// point's own answer type (`P::Answer` — a gate a
/// [`tabit_protocol::points::CallVerdict`], an observer `()`), and
/// the point's name comes with the type. There is no point-name
/// argument to get wrong.
pub fn consult<P, F>(body: F) -> ConsultDef
where
    P: HookPoint,
    F: Fn(&Ctx, Value) -> Result<P::Answer, String> + Send + Sync + 'static,
{
    ConsultDef {
        point: P::NAME,
        body: Box::new(move |ctx, payload| {
            let answer = (body)(ctx, payload)?;
            serde_json::to_value(answer)
                .map_err(|error| format!("the answer does not serialize: {error}"))
        }),
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
    /// `session_opened`, the boot announcement). The line crosses
    /// the pipe and routes at the host, where the sessions live;
    /// effects arrive as the events you watch. Fire-and-forget.
    pub fn command(&self, command: SessionCommand) {
        self.shared.stdio.send_command(&command);
    }

    /// Emit any session event into the shared grammar — it crosses
    /// the pipe by the additional-receiver path (this node's stdio
    /// subscribes to nothing; naming it per emission is the override
    /// path) and surfaces to the frontend and subscribers,
    /// origin-stamped with this extension's id at the host's intake
    /// (attribution, not permission). Local watchers of the kind
    /// hear it too — the layer's own loopback.
    pub fn emit(&self, event: SessionEvent) {
        self.shared.node.emit_to(
            &self.shared.layer,
            std::slice::from_ref(&self.shared.stdio),
            EventFrame {
                stream: None,
                origin: None,
                ttl: None,
                event,
            },
        );
    }

    /// Ask the user: emit an `interaction_request` into the shared
    /// grammar and await the routed `interaction_response` by id.
    /// `ui_type` + opaque payload mirror the templates core tools
    /// use (`native:*` qualifies). Blocks the handler's thread until
    /// answered; the cancellation of this invocation's run resolves
    /// `None` (the card stays open — the run's completion sweeps it
    /// with its settle announced, so the card closes everywhere),
    /// and the pipe's death resolves `None` too — fail closed.
    pub fn ask(&self, ui_type: &str, payload: Value) -> Option<Value> {
        let owner = self
            .correlation
            .clone()
            .unwrap_or_else(|| "watch".to_string());
        let promise = self.shared.node.ask_on(
            &owner,
            std::slice::from_ref(&self.shared.stdio),
            None,
            ui_type,
            payload,
        );
        // The promise resolves on the node (a tokio oneshot); the
        // wait polls cancellation, so it lives on this handler's own
        // thread — the runtime task is the bridge between them.
        let (answered, wait) = std::sync::mpsc::channel::<Value>();
        children::runtime().spawn(async move {
            if let Ok(answer) = promise.await {
                let _ = answered.send(answer);
            }
        });
        loop {
            match wait.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(answer) => return Some(answer),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if self.cancelled() {
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
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<ServiceReply>();
        let owner = call_id.clone();
        self.shared
            .node
            .hold(&owner, &id, KIND_SERVICE_RESPONSE, move |outcome| {
                if let Outcome::Answered(boxed) = outcome {
                    let _ = reply_tx.send(tabit_wire::asks::unanswer::<ServiceReply>(boxed));
                }
            });
        let frame = ExtFrame::ServiceRequest {
            request_id: id.clone(),
            call_id: call_id.clone(),
            verb,
        };
        if !self.shared.write_frame(&frame) {
            self.shared.node.discard(&id);
            return Err("the host closed the pipe".to_string());
        }
        let reply = reply_rx
            .recv()
            .map_err(|_| "the host closed the pipe".to_string())?;
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

/// The id mint (2026-09 ruling): a UUIDv7 — collision-freedom by
/// construction, never naming conventions (ids cross the pipe and
/// register on the host's one table; a name-grammar collision would
/// be a mint-law violation that kills the wrong lane). The family
/// parameter stays for diagnostics.
fn next_request_id(family_root: &str, family: &str) -> String {
    let _ = (family_root, family);
    uuid::Uuid::now_v7().to_string()
}

/// Everything the pipe and the worker threads share: the guest's
/// node with its two faces, the pipe's one writer, and the
/// functional layer's own state.
struct Shared {
    /// The guest's routing layer — every table and law this side of
    /// the pipe. Held asks answer through it; watches and the
    /// children's registrations ride it; emissions leave by its fan.
    node: Arc<Node>,
    /// The pipe's shared-grammar face — the host's channel. Arriving
    /// frames enter through it (the loop's intake); the extension's
    /// own speech names it as an additional receiver.
    stdio: Channel,
    /// The functional layer's own face — the emission anchor (the
    /// unstamped emissions' `from`; the re-stamped-forward
    /// interception surface, when that preset exists).
    layer: Channel,
    /// The pipe's one writer: every outbound line — the dialect's
    /// frames (the ack, results, envelope requests) and the shared
    /// grammar's alike — queues here, and the writer thread owns
    /// stdout exclusively. No caller ever blocks on the pipe (a
    /// worker thread, the children's runtime, the loop itself); a
    /// dead pipe is the writer's exit, the process's end.
    pipe: std::sync::mpsc::Sender<String>,
    /// Correlation ids (calls, consultations) the host cancelled —
    /// long-running handlers poll [`Ctx::cancelled`] and stop: kill
    /// the sandbox, drop the wedge, stop billing.
    cancelled: Mutex<std::collections::HashSet<String>>,
    /// The host's own executable (the initialize's `core_path`) —
    /// owned children spawn it.
    core_path: Mutex<Option<String>>,
}

impl Shared {
    /// One dialect frame out (the frozen pipe's own lanes — not the
    /// shared grammar, nothing routes it): serialize, queue. A dead
    /// pipe is the end, not a failure.
    fn write_frame<T: Serialize>(&self, frame: &T) -> bool {
        let Ok(text) = serde_json::to_string(frame) else {
            return false;
        };
        write_line(&self.pipe, &text);
        true
    }
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
    let node = Arc::new(Node::new(&format!("ext-{}", std::process::id())));
    // The mint-law policy for a guest: the only sender that can
    // re-register a live id on this node's tables is the host, and a
    // host that does has broken the pipe's contract — the honest
    // death, not a thread-panic that wedges the call it came on.
    node.on_mint_violation(|_owner, id| {
        die(&format!(
            "the host re-registered the live ask id `{id}` — the mint law, the pipe's contract"
        ));
    });
    // The pipe's one writer: a dedicated thread owning stdout, fed by
    // an unbounded queue. Every outbound line crosses through it, so
    // no caller (a worker thread, the children's runtime, the loop)
    // ever blocks on the pipe; a dead pipe is the writer's exit.
    let (pipe_tx, pipe_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        for line in pipe_rx {
            if writeln!(handle, "{line}")
                .and_then(|()| handle.flush())
                .is_err()
            {
                std::process::exit(0); // the pipe is gone: the end, not a failure
            }
        }
    });
    let pipe_writer = pipe_tx.clone();
    let stdio = Channel::line("host", move |line: &str| write_line(&pipe_writer, line));
    let layer = Channel::local("sdk", |_| {}, |_| {});
    // The card law's close vocabulary: the stdio carries exactly one
    // default subscription — the settle kind. Every settle announce
    // (the extension's own asks, a swept question, a child's or a
    // grandchild's) crosses the pipe by it, so no channel holds a
    // card that can never be answered; a settle arriving from the
    // host never bounces back (the ingress law).
    node.subscribe_channel(tags::INTERACTION_SETTLED, &stdio);
    let shared = Arc::new(Shared {
        node: node.clone(),
        stdio: stdio.clone(),
        layer,
        pipe: pipe_tx,
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
                event: consult.point.to_string(),
            })
            .collect(),
        watch: watches.iter().map(|w| w.kind.clone()).collect(),
    };
    shared.write_frame(&ack);

    // The watch surface: one subscription per watched kind on the
    // guest's node (the host mirrors the ack's kinds across the
    // pipe; the arrivals fan here). Each handler runs on its own
    // thread — observation never blocks the loop. A card-kind watch
    // carries its settle co-subscription (the node's card law).
    for watch in watches.iter() {
        let kind = watch.kind.clone();
        let body = watch.arc_body().clone();
        let watch_shared = shared.clone();
        node.subscribe(&kind, "watch", move |frame: &EventFrame| {
            let shared = watch_shared.clone();
            let body = body.clone();
            let event = frame.event.clone();
            std::thread::spawn(move || {
                let ctx = Ctx::watch_context(shared);
                catch_unwind_silently(move || body(&ctx, event));
            });
        });
    }

    // The loop: the frozen dialect's lanes first (the pipe's own
    // frames), then the shared grammar through the node's intake —
    // one door, every law. Everything the host can send arrives here.
    loop {
        let line = read_line();
        dispatch_line(&shared, &line, &tools, &consults);
    }
}

/// One inbound line: the dialect's parse (the host's own frames),
/// then the shared grammar's parse into the node's intake.
fn dispatch_line(
    shared: &Arc<Shared>,
    line: &str,
    tools: &Arc<Vec<ToolDef>>,
    consults: &Arc<Vec<ConsultDef>>,
) {
    if let Ok(host) = serde_json::from_str::<HostFrame>(line) {
        match host {
            HostFrame::ToolCall {
                call_id,
                name,
                args,
            } => {
                let (shared, tools) = (shared.clone(), tools.clone());
                std::thread::spawn(move || run_call(&shared, call_id, name, args, &tools));
            }
            HostFrame::Hook {
                hook_id,
                event,
                payload,
            } => {
                let (shared, consults) = (shared.clone(), consults.clone());
                std::thread::spawn(move || {
                    run_consult(&shared, hook_id, &event, &consults, payload)
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
                // The response claims its held round-trip by id: a
                // miss is a late or unknown id — tolerated; a
                // wrong-kind answer is a contract break — the same
                // death the host gives our mirror-image breaks.
                match shared.node.answer(
                    &request_id,
                    KIND_SERVICE_RESPONSE,
                    Box::new(ServiceReply { result, error }),
                ) {
                    tabit_wire::node::AnswerOutcome::WrongKind(kind) => die(&format!(
                        "a service response answered a `{kind}` question (`{request_id}`) — a contract break"
                    )),
                    tabit_wire::node::AnswerOutcome::Delivered
                    | tabit_wire::node::AnswerOutcome::Missed => {}
                }
            }
            HostFrame::Initialize { .. } => {} // a re-send: tolerated, ignored
        }
        return;
    }
    if let Some(inbound) = parse_shared(line) {
        // The shared grammar: watched events, routed answers,
        // session-addressed commands — the node's intake owns every
        // law (the answer claims the ask table; an event fans;
        // a command routes or dispatches by type).
        shared.node.intake(&shared.stdio, inbound);
        return;
    }
    // The double tolerates garbage; the SDK exits loud — a malformed
    // pipe is a broken host or a broken contract.
    die(&format!("unparseable line from the host: {line}"));
}

/// One dispatched tool call — an arriving ask (the taxonomy law):
/// held on the node's table against the host's channel, the body's
/// outcome the answer, the delivery the result line. Panics become
/// error results — the pipe never hangs on a broken body. When the
/// call completes, its own unanswered asks die with it (each settle
/// announced — a card the body left behind closes everywhere).
fn run_call(shared: &Arc<Shared>, call_id: String, name: String, args: Value, tools: &[ToolDef]) {
    let owner = call_id.clone();
    let writer = shared.clone();
    let held = shared.node.try_hold(
        shared.stdio.owner(),
        &call_id,
        KIND_TOOL_RESULT,
        move |outcome| {
            if let Outcome::Answered(boxed) = outcome {
                let result = tabit_wire::asks::unanswer::<ToolWireResult>(boxed);
                let _ = writer.write_frame(&ExtFrame::ToolResult(result));
            }
        },
    );
    if !held {
        // The violation policy fired (a live id re-registered); the
        // frame dies with it.
        return;
    }
    let ctx = Ctx {
        correlation: Some(call_id.clone()),
        shared: shared.clone(),
    };
    let result = match std::panic::catch_unwind(AssertUnwindSafe(move || {
        let Some(tool) = tools.iter().find(|tool| tool.name == name) else {
            return ToolWireResult {
                call_id,
                error: Some(format!("this extension serves no tool `{name}`")),
                report: String::new(),
                details: None,
            };
        };
        match (tool.body)(args, &ctx) {
            Ok(output) => ToolWireResult {
                call_id,
                error: None,
                report: output.report,
                details: output.details,
            },
            Err(error) => ToolWireResult {
                call_id,
                error: Some(error),
                report: String::new(),
                details: None,
            },
        }
    })) {
        Ok(result) => result,
        Err(panic) => ToolWireResult {
            call_id: owner.clone(),
            error: Some(format!("the tool body panicked: {}", panic_note(panic))),
            report: String::new(),
            details: None,
        },
    };
    let _ = shared
        .node
        .answer(&owner, KIND_TOOL_RESULT, Box::new(result));
    shared.node.retract_asks(&owner, "the call completed");
}

/// One dispatched consultation — an arriving ask like a call. **A
/// failing handler is treated as absence** (ruled 2026-09: dead or
/// broken resolve identically — the point's declared neutral; a
/// failed *tool call* is the model-visible failure). The handler's
/// own error paths can choose otherwise; the SDK's failure handling
/// cannot.
fn run_consult(
    shared: &Arc<Shared>,
    hook_id: String,
    event: &str,
    consults: &[ConsultDef],
    payload: Value,
) {
    let owner = hook_id.clone();
    let writer = shared.clone();
    let held = shared.node.try_hold(
        shared.stdio.owner(),
        &hook_id,
        KIND_HOOK_RESULT,
        move |outcome| {
            if let Outcome::Answered(boxed) = outcome {
                let result = tabit_wire::asks::unanswer::<HookResult>(boxed);
                let _ = writer.write_frame(&ExtFrame::HookResult(result));
            }
        },
    );
    if !held {
        return;
    }
    let ctx = Ctx {
        correlation: Some(hook_id.clone()),
        shared: shared.clone(),
    };
    let answer = consult_answer(event, consults, &ctx, payload);
    let _ = shared.node.answer(
        &owner,
        KIND_HOOK_RESULT,
        Box::new(HookResult { hook_id, answer }),
    );
    shared
        .node
        .retract_asks(&owner, "the consultation completed");
}

/// The consultation body's answer: the point's own type serialized,
/// or its declared neutral for a failing or absent handler.
fn consult_answer(event: &str, consults: &[ConsultDef], ctx: &Ctx, payload: Value) -> Value {
    let Some(consult) = consults.iter().find(|consult| consult.point == event) else {
        // No subscription: a newer host's event point this SDK
        // predates — absence, the declared neutral for the name.
        return tabit_protocol::points::neutral_wire(event);
    };
    std::panic::catch_unwind(AssertUnwindSafe(|| (consult.body)(ctx, payload)))
        .unwrap_or_else(|_| Err("the consultation handler panicked".to_string()))
        .unwrap_or_else(|error| {
            let _ = writeln!(
                std::io::stderr(),
                "tabit extension: the handler failed: {error}"
            );
            tabit_protocol::points::neutral_wire(event)
        })
}

/// A watch body's panic is reported, never fatal — observation must
/// not take the process down.
pub(crate) fn catch_unwind_silently(body: impl FnOnce()) {
    if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(body)) {
        let _ = writeln!(
            std::io::stderr(),
            "tabit extension: a watch handler panicked: {}",
            panic_note(panic)
        );
    }
}

/// One handler invocation on its own thread with a fresh watch
/// context — the subscription callbacks' dispatch (a blocked handler
/// must never stall the fan that called it).
pub(crate) fn spawn_handler(shared: Arc<Shared>, body: impl FnOnce(Ctx) + Send + 'static) {
    std::thread::spawn(move || {
        let ctx = Ctx::watch_context(shared);
        catch_unwind_silently(move || body(ctx));
    });
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

/// One line out the pipe — every outbound line (the dialect's frames
/// and the shared grammar alike) queues through the one writer, so
/// ordering is the queue's and no caller blocks on the pipe.
fn write_line(pipe: &std::sync::mpsc::Sender<String>, line: &str) {
    // A send fails only against a dead writer — the writer exits the
    // process on a dead pipe, so the end is already in motion.
    let _ = pipe.send(line.to_string());
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
pub(crate) mod tests {
    use super::*;

    pub(crate) fn shared() -> Arc<Shared> {
        let node = Arc::new(Node::new("test"));
        // A pipe to nowhere: sends drop silently (nothing in these
        // tests writes, and the writer thread is serve's business).
        let (pipe_tx, pipe_rx) = std::sync::mpsc::channel::<String>();
        drop(pipe_rx);
        let pipe_writer = pipe_tx.clone();
        let stdio = Channel::line("host", move |line: &str| write_line(&pipe_writer, line));
        let layer = Channel::local("sdk", |_| {}, |_| {});
        Arc::new(Shared {
            node,
            stdio,
            layer,
            pipe: pipe_tx,
            cancelled: Mutex::new(std::collections::HashSet::new()),
            core_path: Mutex::new(None),
        })
    }

    #[test]
    fn a_failing_consultation_is_absence_run_for_call_points() {
        let consults = [consult::<tabit_protocol::points::ToolCall, _>(
            |_ctx, _payload| Err::<tabit_protocol::points::CallVerdict, _>("boom".to_string()),
        )];
        let answer = consult_answer(
            "tool_call",
            &consults,
            &Ctx::watch_context(shared()),
            json!({}),
        );
        assert_eq!(
            answer,
            json!({"verdict": "run"}),
            "the neutral answer for a call point"
        );
    }

    #[test]
    fn a_failing_consultation_is_absence_unit_for_result_points() {
        let consults = [consult::<tabit_protocol::points::ToolResult, _>(
            |_ctx, _payload| Err::<(), _>("boom".to_string()),
        )];
        let answer = consult_answer(
            "tool_result",
            &consults,
            &Ctx::watch_context(shared()),
            json!({}),
        );
        assert!(answer.is_null(), "the neutral answer for a result point");
    }

    #[test]
    fn an_unsubscribed_point_is_absence() {
        let answer = consult_answer("tool_call", &[], &Ctx::watch_context(shared()), json!({}));
        assert_eq!(
            answer,
            json!({"verdict": "run"}),
            "absence is the neutral answer"
        );
    }

    #[test]
    fn the_answer_serializes_from_the_shared_type() {
        let consults = [consult::<tabit_protocol::points::ToolCall, _>(
            |_ctx, _payload| {
                Ok(tabit_protocol::points::CallVerdict::Skip {
                    message: "not tonight".to_string(),
                })
            },
        )];
        let answer = consult_answer(
            "tool_call",
            &consults,
            &Ctx::watch_context(shared()),
            json!({}),
        );
        assert_eq!(
            answer,
            json!({"verdict": "skip", "message": "not tonight"}),
            "the verdict rode the wire as its own type"
        );
    }
}
