//! The tabit extension SDK: the guest's functional layer over its
//! node. Authors register tools, consultations, and watched event
//! kinds; the SDK owns the pipe (handshake, the frozen dialect's
//! lanes, the unconditional drain) and derives the handshake's
//! declarations from the registration. The routing — whose frames
//! cross, how answers walk home, what a death sweeps — is the
//! guest's node under the SDK (the node architecture, 2026-09):
//! authors never meet the router or the channel concepts.
//!
//! The SDK is async (owner ruling 2026-09): bodies are futures, an
//! ask awaits its promise natively, cancellation is a wake not a
//! poll — honest about how the core works. Every invocation runs on
//! its own task, so tools and hooks execute concurrently and a
//! blocked body cannot stall the pipe.
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
//! session event — it crosses the pipe by the stdio's local
//! subscription and surfaces origin-stamped at the host's intake),
//! `ask` (emit an interaction request, await the routed answer),
//! `complete` (one bare model completion, call-correlated), and
//! `cancelled` (the cooperative abort poll).
//!
//! The wire laws this side of the pipe: a tool call, a hook, a
//! service request are asks (the taxonomy ruling) — each arriving
//! call is held on the node's ask table against the host's channel
//! and answered through it; an extension's own ask and its emissions
//! cross the stdio by that pipe's crossing policy — the locality
//! ruling (2026-09-25): the stdio subscribes every kind from the
//! LOCAL door plus the settle kind from either door, so own speech
//! crosses, the close vocabulary crosses from anywhere, and nothing
//! else does. The SDK shares the host's wire types (the 2026-09
//! sharing ruling: one wire, one set of shapes — the docs stay the
//! contract for other languages, the conformance tests keep crate
//! and docs honest).

// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and that ergonomics is the point of this crate: the
// author-facing API stays `args["field"]`.
#![cfg_attr(test, allow(clippy::expect_used, clippy::panic, clippy::unwrap_used))]
#![allow(clippy::indexing_slicing, clippy::type_complexity)]

use std::future::Future;
use std::io::Write as _;
use std::pin::Pin;
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
use tabit_wire::client::ChildSpec;
use tabit_wire::node::{Channel, KIND_INTERACTION, Locality, Node, parse_shared};
use tokio::io::AsyncBufReadExt;

/// The extension protocol this SDK speaks — must match the host's
/// exactly (the pipe is a frozen contract, not a negotiated one).
const PROTOCOL_VERSION: u32 = 5;

pub mod children;

pub use children::Child;

/// The SDK's one runtime (multi-thread): the pipe loop, every
/// invocation task, the ask promise bridges, and the owned
/// children's settle folds all ride it.
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        #[allow(clippy::expect_used)]
        // sanctioned crash: a runtime that fails to build cannot be served around
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("the SDK runtime builds")
    })
}

/// One extension's whole declaration, built by registering tools,
/// consultations, and watches; `serve` derives the handshake from
/// it. Nothing is declared twice — the registration IS the ack.
pub struct Extension {
    tools: Vec<ToolDef>,
    consults: Vec<ConsultDef>,
    watches: Vec<WatchDef>,
    asks: Vec<ErasedWatch>,
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
            asks: Vec::new(),
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

    /// Register one ask answerer for arriving cards — the card
    /// surface's author arm. Cards reach this extension's node from
    /// every owned child at once (one registration covers them all —
    /// the frame's stamp attributes the child); the first
    /// registration also retires the shipped default, which crosses
    /// cards verbatim to the host (a custom beside the default would
    /// double-surface the card). Answerers stack — any may answer
    /// ([`Ctx::answer`]); the child's hub takes the first arrival,
    /// and a late answer is a tolerated no-op. Answerers hear the
    /// card and its settle (the pair — whoever surfaces a card hears
    /// it close).
    pub fn on_ask<F, Fut>(mut self, body: F) -> Self
    where
        F: Fn(Ctx, EventFrame) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let body: ErasedWatch = Arc::new(move |ctx, frame| Box::pin(body(ctx, frame)));
        self.asks.push(body);
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

/// A tool body after erasure: the arguments plus a cloned context,
/// returning the body's future (blocking awaits, asks, emits,
/// commands — the loop keeps reading regardless).
type ErasedBody = Box<
    dyn Fn(Value, Ctx) -> Pin<Box<dyn Future<Output = Result<Output, String>> + Send>>
        + Send
        + Sync,
>;

/// One declared tool.
pub struct ToolDef {
    name: String,
    description: String,
    schema: Value,
    body: ErasedBody,
}

/// Declare a tool: the body is an async function of the arguments
/// and a cloneable context.
pub fn tool<F, Fut>(name: &str, description: &str, schema: Value, body: F) -> ToolDef
where
    F: Fn(Value, Ctx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Output, String>> + Send + 'static,
{
    ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        schema,
        body: Box::new(move |args, ctx| Box::pin(body(args, ctx))),
    }
}

/// A consultation handler after type erasure: a cloned context plus
/// the event payload, returning the point's answer serialized (the
/// host waits for it).
type ErasedConsult = Box<
    dyn Fn(Ctx, Value) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>> + Send + Sync,
>;

/// One declared consultation. The point and its answer type pair at
/// registration — [`consult::<P>`] is the only constructor, so a
/// `tool_result` consult cannot return a verdict.
pub struct ConsultDef {
    pub point: &'static str,
    body: ErasedConsult,
}

/// Declare a consultation on one hook point: the async body returns
/// the point's own answer type (`P::Answer` — a gate a
/// [`tabit_protocol::points::CallVerdict`], an observer `()`), and
/// the point's name comes with the type. There is no point-name
/// argument to get wrong.
pub fn consult<P, F, Fut>(body: F) -> ConsultDef
where
    P: HookPoint,
    F: Fn(Ctx, Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<P::Answer, String>> + Send + 'static,
{
    ConsultDef {
        point: P::NAME,
        body: Box::new({
            let body = std::sync::Arc::new(body);
            move |ctx, payload| {
                let body = body.clone();
                Box::pin(async move {
                    let answer = body(ctx, payload).await?;
                    serde_json::to_value(answer)
                        .map_err(|error| format!("the answer does not serialize: {error}"))
                })
            }
        }),
    }
}

/// A watch handler after erasure: a cloned context plus the whole
/// frame — the stamp is the attribution (which session, which child),
/// and stripping it at the boundary would lose exactly that
/// (owner ruling 2026-09-25).
type ErasedWatch =
    Arc<dyn Fn(Ctx, EventFrame) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// One declared watch. The kind must name a real event kind (a
/// wire tag, [`tabit_protocol::tags`]) — a typo'd kind watches
/// nothing, so it refuses loudly at registration instead.
pub struct WatchDef {
    pub kind: String,
    body: ErasedWatch,
}

/// Declare a watch on one event kind: an async observer over the
/// whole frame (the stamp is the attribution — which session, which
/// child).
pub fn watch<F, Fut>(kind: &str, body: F) -> WatchDef
where
    F: Fn(Ctx, EventFrame) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    if !SessionEvent::is_known_tag(kind) {
        die(&format!(
            "unknown event kind `{kind}` (the spellings live in tabit_protocol::tags)"
        ));
    }
    WatchDef {
        kind: kind.to_string(),
        body: Arc::new(move |ctx, event| Box::pin(body(ctx, event))),
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
/// Cheap to clone (one `Arc` and an optional correlation); valid for
/// the handler's invocation — the correlation it carries names the
/// call or consultation the host would cancel.
#[derive(Clone)]
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
    /// An awaited [`Ctx::ask`] resolves `None` on the same wake —
    /// polling is for the body's own phases.
    pub fn cancelled(&self) -> bool {
        match &self.correlation {
            Some(id) => sdk_lock(&self.shared.cancelled).contains(id),
            None => false,
        }
    }

    /// Await this invocation's cancellation, if it ever comes — the
    /// ask's resolve-or-cancel race and any body that prefers
    /// `await` over poll.
    async fn cancelled_wake(&self) {
        loop {
            let notified = self.shared.cancel_notify.notified();
            if self.cancelled() {
                return;
            }
            notified.await;
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
    /// the pipe by the stdio's local subscription (the crossing
    /// policy: this node's own speech leaves, arrivals do not) and
    /// surfaces to the frontend and subscribers, origin-stamped with
    /// this extension's id at the host's intake (attribution, not
    /// permission). Local watchers of the kind hear it too — the
    /// layer's own loopback.
    pub fn emit(&self, event: SessionEvent) {
        self.shared.node.emit(
            &self.shared.layer,
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
    /// use (`native:*` qualifies). The await resolves on the answer,
    /// on this invocation's cancellation (`None` — the card stays
    /// open; the call's completion sweeps it with its settle
    /// announced, so the card closes everywhere), and on the pipe's
    /// death (`None` too — fail closed).
    pub async fn ask(&self, ui_type: &str, payload: Value) -> Option<Value> {
        let owner = self
            .correlation
            .clone()
            .unwrap_or_else(|| "watch".to_string());
        let promise = self.shared.node.ask(&owner, None, ui_type, payload);
        tokio::select! {
            answer = promise => answer.ok(),
            // The cancellation of this invocation's run: abandon the
            // await (the host-side card settles whenever the user
            // answers it; the call's completion sweep announces any
            // card left behind).
            _ = self.cancelled_wake() => None,
        }
    }

    /// Begin an owned child over the shared spec — the spawner's
    /// preset: the host's own executable (the host IS the binary) in
    /// the given cwd. Chain the child-role knobs (model, toolset,
    /// budget, persistence) and hand the spec to
    /// [`Child::create`](crate::Child::create).
    pub fn child(&self, cwd: std::path::PathBuf) -> Result<ChildSpec, String> {
        Ok(ChildSpec::new(
            std::path::PathBuf::from(self.core_path()?),
            cwd,
        ))
    }

    /// Answer one arriving card by id — the ask table's claim: the
    /// transit entry's delivery writes the response line down the
    /// asking child's stdin. Races are the co-frontend law: the
    /// child takes the first answer to land; one that arrives after
    /// another (or after the question died) is a tolerated no-op.
    pub fn answer(&self, id: &str, payload: Value) {
        let _ = self
            .shared
            .node
            .answer(id, KIND_INTERACTION, Box::new(payload));
    }

    /// One bare model completion (the envelope's one verb):
    /// complete-only, `max_tokens` capped by the host. `model` is an
    /// optional provider/model or bare-id reference; absent means
    /// the session's current model. Usage bills to the session under
    /// this extension's name and rides the result. Needs a call or
    /// consultation correlation — a watch observes, it does not
    /// spend.
    pub async fn complete(
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
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<ServiceReply>();
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
            .await
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

/// Everything the pipe and the invocation tasks share: the guest's
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
    /// grammar's alike — queues here, and the line pump owns stdout
    /// exclusively. No invocation ever blocks on the pipe; a dead
    /// pipe is the loop's EOF, the process's end.
    pipe: tokio::sync::mpsc::UnboundedSender<String>,
    /// Correlation ids (calls, consultations) the host cancelled —
    /// long-running handlers poll [`Ctx::cancelled`] and stop: kill
    /// the sandbox, drop the wedge, stop billing. The notify wakes
    /// every waiter the cancel arm can find.
    cancelled: Mutex<std::collections::HashSet<String>>,
    cancel_notify: tokio::sync::Notify,
    /// The host's own executable (the initialize's `core_path`) —
    /// owned children spawn it.
    core_path: Mutex<Option<String>>,
}

impl Shared {
    /// One dialect frame out (the frozen pipe's own lanes — not the
    /// shared grammar, nothing routes it): serialize, queue.
    fn write_frame<T: Serialize>(&self, frame: &T) -> bool {
        let Ok(text) = serde_json::to_string(frame) else {
            return false;
        };
        write_line(&self.pipe, &text);
        true
    }
}

/// One line out the pipe — every outbound line (the dialect's frames
/// and the shared grammar alike) queues through the one pump, so
/// ordering is the queue's and no invocation blocks on the pipe.
fn write_line(pipe: &tokio::sync::mpsc::UnboundedSender<String>, line: &str) {
    // A send fails only against a dead pump — the pipe is broken and
    // the loop's EOF is already in motion.
    let _ = pipe.send(line.to_string());
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
        asks,
    } = extension;
    let (tools, consults, watches, asks) = (
        Arc::new(tools),
        Arc::new(consults),
        Arc::new(watches),
        Arc::new(asks),
    );
    runtime().block_on(async move {
        let node = Arc::new(Node::new(&format!("ext-{}", std::process::id())));
        // The mint-law policy for a guest: the only sender that can
        // re-register a live id on this node's tables is the host,
        // and a host that does has broken the pipe's contract — the
        // honest death, not a task-panic that wedges the call it
        // came on.
        node.on_mint_violation(|_owner, id| {
            die(&format!(
                "the host re-registered the live ask id `{id}` — the mint law, the pipe's contract"
            ));
        });
        // The pipe's one writer: the shared line pump over stdout,
        // closed by the senders' drop.
        let (pipe_tx, pipe_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        tabit_wire::process::spawn_line_writer(tokio::io::stdout(), pipe_rx, None);
        let pipe_writer = pipe_tx.clone();
        let stdio = Channel::line("host", move |line: &str| write_line(&pipe_writer, line));
        let layer = Channel::local("sdk", |_| {}, |_| {});
        // The pipe's crossing policy, stated as two subscriptions
        // (the locality ruling, 2026-09-25): every kind from the
        // LOCAL door — this extension's own speech (its emissions,
        // its asks, their settle announces) crosses — plus the
        // settle kind from EITHER door, so no channel anywhere holds
        // a card that can never close. Arrivals cross nothing: a
        // child's or grandchild's card reaches only the card surface,
        // and a settle arriving from the host never bounces back
        // (the ingress law).
        node.subscribe_channel_all(Locality::Local, &stdio);
        node.subscribe_channel(tags::INTERACTION_SETTLED, Locality::Remote, &stdio);
        let shared = Arc::new(Shared {
            node: node.clone(),
            stdio: stdio.clone(),
            layer,
            pipe: pipe_tx,
            cancelled: Mutex::new(std::collections::HashSet::new()),
            cancel_notify: tokio::sync::Notify::new(),
            core_path: Mutex::new(None),
        });

        // The self-report — this extension's FIRST line on the
        // channel (owner ruling 2026-09-25: children report first,
        // spawners decide; the host version-checks and kills an
        // incompatible guest). The host's facts (`core_path`, `cwd`)
        // arrive later as `host_facts`, whenever the host sends them —
        // the core's own executable is the owned-children spawner's
        // path, and it lands in `core_path` then.
        let report = ExtFrame::Report {
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
        shared.write_frame(&report);

        // The watch surface: one subscription per watched kind on the
        // guest's node (the host mirrors the report's kinds across
        // the pipe; the arrivals fan here) — one registration covers
        // every child this extension ever spawns, the frame's stamp
        // carrying the attribution. Each handler runs on its own
        // task — observation never blocks the loop.
        for watch in watches.iter() {
            let kind = watch.kind.clone();
            let body = watch.body.clone();
            let watch_shared = shared.clone();
            node.subscribe(&kind, "watch", Locality::Both, move |frame: &EventFrame| {
                let shared = watch_shared.clone();
                let body = body.clone();
                let frame = frame.clone();
                spawn_observation(shared, move |ctx| body(ctx, frame));
            });
        }

        // The card surface (the shipped lift and the author
        // answerers) — see [`mount_card_surface`].
        mount_card_surface(&node, &shared, asks.clone());

        // The loop: the frozen dialect's lanes first (the pipe's own
        // frames), then the shared grammar through the node's intake —
        // one door, every law. Everything the host can send arrives
        // here.
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                _ => std::process::exit(0), // EOF: the host closed, so are we
            };
            dispatch_line(&shared, &line, &tools, &consults);
        }
    })
}

/// The card surface: ONE node-level registration pair covering every
/// owned child (owner ruling 2026-09-25 — registration is router
/// config, a single subscription spans multiple children; the
/// frame's stamp attributes which), hearing the REMOTE door alone —
/// the locality ruling makes the split structural: this extension's
/// own asks are local speech and never surface here, a child's
/// arriving card is remote and always does. With no author
/// answerers the card crosses verbatim (the shipped lift: the
/// host's frontend surfaces it, its settle crossing back by the
/// stdio's remote settle subscription); the first author answerer
/// retires that default (a custom beside it would double-surface
/// the card), and answerers then stack — any may answer
/// ([`Ctx::answer`]), the child's hub takes the first arrival, a
/// late answer a tolerated no-op. Settles reach the answerers too
/// (the declared pair — whoever surfaces a card hears it close) but
/// never cross here: the stdio's own settle subscription carries
/// them.
fn mount_card_surface(node: &Node, shared: &Arc<Shared>, asks: Arc<Vec<ErasedWatch>>) {
    let surface_asks = asks.clone();
    let surface_shared = shared.clone();
    let card_surface = move |frame: &EventFrame| {
        if surface_asks.is_empty() {
            // The shipped lift: the verbatim crossing — the write
            // alone; the intake already fanned and taught.
            surface_shared.stdio.send_event(frame);
            return;
        }
        for body in surface_asks.iter() {
            let (shared, frame, body) = (surface_shared.clone(), frame.clone(), body.clone());
            spawn_observation(shared, move |ctx| body(ctx, frame));
        }
    };
    let asks_for_settled = asks;
    let settled_shared = shared.clone();
    let settled_surface = move |frame: &EventFrame| {
        for body in asks_for_settled.iter() {
            let (shared, frame, body) = (settled_shared.clone(), frame.clone(), body.clone());
            spawn_observation(shared, move |ctx| body(ctx, frame));
        }
    };
    node.subscribe(
        tags::INTERACTION_REQUEST,
        "card-surface",
        Locality::Remote,
        card_surface,
    );
    node.subscribe(
        tags::INTERACTION_SETTLED,
        "card-surface",
        Locality::Remote,
        settled_surface,
    );
}

/// One invocation on its own task with a fresh watch context — the
/// subscription callbacks' dispatch (a blocked handler must never
/// stall the fan that called it). A panicking body is reported on
/// stderr, never fatal.
pub(crate) fn spawn_observation<Fut>(
    shared: Arc<Shared>,
    body: impl FnOnce(Ctx) -> Fut + Send + 'static,
) where
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let ctx = Ctx::watch_context(shared);
        // The body runs as its own task so a panic is reported, never
        // fatal — observation must not take the process down.
        if let Err(join) = tokio::spawn(body(ctx)).await {
            let _ = writeln!(
                std::io::stderr(),
                "tabit extension: a handler panicked: {}",
                panic_note_from_join(join)
            );
        }
    });
}

/// A task's panic as the report line (a failed handler never takes
/// the process down).
fn panic_note_from_join(error: tokio::task::JoinError) -> String {
    match error.try_into_panic() {
        Ok(panic) => panic_note(panic),
        Err(_) => "the task was cancelled".to_string(),
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
                tokio::spawn(async move { run_call(&shared, call_id, name, args, &tools).await });
            }
            HostFrame::Hook {
                hook_id,
                event,
                payload,
            } => {
                let (shared, consults) = (shared.clone(), consults.clone());
                tokio::spawn(async move {
                    run_consult(&shared, hook_id, &event, &consults, payload).await
                });
            }
            HostFrame::Cancel { call_id } => {
                sdk_lock(&shared.cancelled).insert(call_id);
                shared.cancel_notify.notify_waiters();
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
            HostFrame::HostFacts { core_path, .. } => {
                // The host's facts, after our report cleared its
                // check: the owned-children spawner's path.
                *sdk_lock(&shared.core_path) = Some(core_path);
            }
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
async fn run_call(
    shared: &Arc<Shared>,
    call_id: String,
    name: String,
    args: Value,
    tools: &[ToolDef],
) {
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
    // The body runs as its own task: a panic becomes the error result
    // (the pipe never hangs on a broken body).
    let body = match tools.iter().find(|tool| tool.name == name) {
        None => {
            let _ = shared.node.answer(
                &owner,
                KIND_TOOL_RESULT,
                Box::new(ToolWireResult {
                    call_id,
                    error: Some(format!("this extension serves no tool `{name}`")),
                    report: String::new(),
                    details: None,
                }),
            );
            shared.node.retract_asks(&owner, "the call completed");
            return;
        }
        Some(tool) => tokio::spawn((tool.body)(args, ctx)),
    };
    let result = match body.await {
        Ok(Ok(output)) => ToolWireResult {
            call_id,
            error: None,
            report: output.report,
            details: output.details,
        },
        Ok(Err(error)) => ToolWireResult {
            call_id,
            error: Some(error),
            report: String::new(),
            details: None,
        },
        Err(join) => ToolWireResult {
            call_id,
            error: Some(format!(
                "the tool body panicked: {}",
                panic_note_from_join(join)
            )),
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
async fn run_consult(
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
    let answer = consult_answer(event, consults, ctx, payload).await;
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
async fn consult_answer(event: &str, consults: &[ConsultDef], ctx: Ctx, payload: Value) -> Value {
    let Some(consult) = consults.iter().find(|consult| consult.point == event) else {
        // No subscription: a newer host's event point this SDK
        // predates — absence, the declared neutral for the name.
        return tabit_protocol::points::neutral_wire(event);
    };
    // The body runs as its own task: a failed OR panicking handler
    // resolves as absence (the point's declared neutral).
    match tokio::spawn((consult.body)(ctx, payload)).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(error)) => {
            let _ = writeln!(
                std::io::stderr(),
                "tabit extension: the handler failed: {error}"
            );
            tabit_protocol::points::neutral_wire(event)
        }
        Err(join) => {
            let _ = writeln!(
                std::io::stderr(),
                "tabit extension: the handler panicked: {}",
                panic_note_from_join(join)
            );
            tabit_protocol::points::neutral_wire(event)
        }
    }
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
        // tests writes, and the writer pump is serve's business).
        let (pipe_tx, mut pipe_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        pipe_rx.close();
        let pipe_writer = pipe_tx.clone();
        let stdio = Channel::line("host", move |line: &str| write_line(&pipe_writer, line));
        let layer = Channel::local("sdk", |_| {}, |_| {});
        Arc::new(Shared {
            node,
            stdio,
            layer,
            pipe: pipe_tx,
            cancelled: Mutex::new(std::collections::HashSet::new()),
            cancel_notify: tokio::sync::Notify::new(),
            core_path: Mutex::new(None),
        })
    }

    #[tokio::test]
    async fn a_failing_consultation_is_absence_run_for_call_points() {
        let consults = [consult::<tabit_protocol::points::ToolCall, _, _>(
            |_ctx, _payload| async {
                Err::<tabit_protocol::points::CallVerdict, _>("boom".to_string())
            },
        )];
        let answer = consult_answer(
            "tool_call",
            &consults,
            Ctx::watch_context(shared()),
            json!({}),
        )
        .await;
        assert_eq!(
            answer,
            json!({"verdict": "run"}),
            "the neutral answer for a call point"
        );
    }

    #[tokio::test]
    async fn a_failing_consultation_is_absence_unit_for_result_points() {
        let consults = [consult::<tabit_protocol::points::ToolResult, _, _>(
            |_ctx, _payload| async { Err::<(), _>("boom".to_string()) },
        )];
        let answer = consult_answer(
            "tool_result",
            &consults,
            Ctx::watch_context(shared()),
            json!({}),
        )
        .await;
        assert!(answer.is_null(), "the neutral answer for a result point");
    }

    #[tokio::test]
    async fn an_unsubscribed_point_is_absence() {
        let answer =
            consult_answer("tool_call", &[], Ctx::watch_context(shared()), json!({})).await;
        assert_eq!(
            answer,
            json!({"verdict": "run"}),
            "absence is the neutral answer"
        );
    }

    #[tokio::test]
    async fn the_answer_serializes_from_the_shared_type() {
        let consults = [consult::<tabit_protocol::points::ToolCall, _, _>(
            |_ctx, _payload| async {
                Ok(tabit_protocol::points::CallVerdict::Skip {
                    message: "not tonight".to_string(),
                })
            },
        )];
        let answer = consult_answer(
            "tool_call",
            &consults,
            Ctx::watch_context(shared()),
            json!({}),
        )
        .await;
        assert_eq!(
            answer,
            json!({"verdict": "skip", "message": "not tonight"}),
            "the verdict rode the wire as its own type"
        );
    }

    /// The review round's latent break, pinned: a child's card
    /// arrives exactly as it crosses the wire — stamped with the
    /// child's session, origin carrying the CHILD's own asker (its
    /// node's ask stamped it before the card left) — and the card
    /// surface lifts it anyway. The origin-discriminating surface
    /// once dropped exactly this shape; the locality split makes
    /// the lift structural (remote arrivals surface, local speech
    /// does not).
    #[test]
    fn a_childs_card_surfaces_through_the_card_surface() {
        use tabit_protocol::StreamId;
        use tabit_wire::node::Inbound;

        let node = Arc::new(Node::new("test"));
        let (pipe_tx, mut pipe_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let pipe_writer = pipe_tx.clone();
        let stdio = Channel::line("host", move |line: &str| write_line(&pipe_writer, line));
        let layer = Channel::local("sdk", |_| {}, |_| {});
        node.subscribe_channel_all(Locality::Local, &stdio);
        node.subscribe_channel(tags::INTERACTION_SETTLED, Locality::Remote, &stdio);
        let shared = Arc::new(Shared {
            node: node.clone(),
            stdio: stdio.clone(),
            layer,
            pipe: pipe_tx,
            cancelled: Mutex::new(std::collections::HashSet::new()),
            cancel_notify: tokio::sync::Notify::new(),
            core_path: Mutex::new(None),
        });
        mount_card_surface(&node, &shared, Arc::new(Vec::new()));

        let lane = Channel::line("lane-1", |_| {});
        node.intake(
            &lane,
            Inbound::Event(EventFrame {
                stream: Some(StreamId::new("child-sess")),
                origin: Some("child-asker".to_string()),
                ttl: None,
                event: SessionEvent::InteractionRequest {
                    id: "card-1".to_string(),
                    ui_type: "native:select_one".to_string(),
                    payload: json!({}),
                },
            }),
        );
        let line = pipe_rx
            .blocking_recv()
            .expect("the child's card lifted to the host");
        assert!(
            line.contains("card-1"),
            "the lifted line is the card: {line}"
        );
        assert!(
            line.contains("child-sess"),
            "the stamp crossed verbatim: {line}"
        );

        // This extension's OWN ask (local speech) crosses by the
        // local subscription alone — never double-crossed by the
        // surface.
        let _promise = node.ask("call-1", None, "native:select_one", json!({}));
        let mut requests = 0;
        while let Ok(line) = pipe_rx.try_recv() {
            if line.contains("interaction_request") {
                requests += 1;
            }
        }
        assert_eq!(requests, 1, "the own ask crossed exactly once");
    }
}
