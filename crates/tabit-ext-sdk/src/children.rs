//! Owned children: an extension's sessions-to-itself, over the
//! shared client (`tabit-wire`) — the same substrate core's subagent
//! bridge drives, the same settle fold, the same reaper. The
//! extension is the child's frontend: its frames arrive here typed,
//! its asks surface through the owner's pipe, and silence is the
//! default — nothing crosses to the host's channel unless the
//! forward callback carries it (the boolean) or the ask slot's
//! default does.
//!
//! The registry laws (the 2026-09 rulings, made structural):
//!
//! - **Per-kind lookup, one map probe per frame**, plus a **generic
//!   slot**. Observation kinds compose: specifics and the generic
//!   both run (forwarding keeps forwarding the kinds you also
//!   watch).
//! - **Asks follow the co-frontend law.** The shipped
//!   forward-and-relay default surfaces the child's card verbatim
//!   (stream preserved, the origin naming the conduit once the host
//!   re-stamps it) and the answer routes home by id through the same
//!   registry the extension's own asks answer through; the first
//!   author registration replaces the default (a custom beside it
//!   would double-surface the card), and author registrations then
//!   stack — any answerer may answer, the child's hub takes the
//!   first arrival, a late answer is a tolerated no-op. There is no
//!   single-owner rule anywhere: answers are races, arbitrated where
//!   the question lives.
//! - **No lift**: an id-swap re-ask has no reason to exist — the
//!   child's id-space never mints onto the host stream, and the one
//!   card a user sees is the child's own.
//!
//! The child's terminal vocabulary: its run completes or fails on
//! its own, or its owner kills it (this type's kill, the creating
//! call's cancellation via the settle leash, drop). Nobody else
//! commands it.

use std::sync::{Arc, Mutex};

use tabit_protocol::{EventFrame, SessionEvent, tags};
use tabit_wire::client::{ChildSpec, Settlement};

use crate::{Ctx, Shared, emit};

/// Shape one owned child before the spawn. Everything omitted
/// inherits the default: ephemeral, the extension's cwd.
pub struct ChildOptions {
    cwd: std::path::PathBuf,
    model: Option<String>,
    session: Option<std::path::PathBuf>,
    max_turns: Option<usize>,
    forwarding: bool,
}

impl ChildOptions {
    /// An ephemeral child in `cwd` (the process cwd — the OS
    /// enforces the scope every tool inside resolves against).
    pub fn new(cwd: std::path::PathBuf) -> Self {
        Self {
            cwd,
            model: None,
            session: None,
            max_turns: None,
            forwarding: false,
        }
    }

    /// The child's model (`provider/model`; absent means the child
    /// resolves its own default).
    pub fn model(mut self, reference: &str) -> Self {
        self.model = Some(reference.to_string());
        self
    }

    /// Resume the stored session instead of starting fresh.
    pub fn session(mut self, path: std::path::PathBuf) -> Self {
        self.session = Some(path);
        self
    }

    /// The per-child model-call budget.
    pub fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    /// Forward the child's events verbatim to the host's channel —
    /// the boolean sugar over installing the shipped forward
    /// callback in the generic slot (stream preserved, origin naming
    /// this extension as the conduit).
    pub fn forwarding(mut self, forwarding: bool) -> Self {
        self.forwarding = forwarding;
        self
    }
}

/// One frame handler: the frame, not the bare event — forwarding and
/// ask-owning both need the stream stamp.
type FrameHandler = Arc<dyn Fn(&Ctx, &EventFrame) + Send + Sync>;

/// The per-child frame registry: kind-keyed specifics, one generic
/// slot, and the single-owner ask slot.
struct Registry {
    /// The observation surface: the shared FrameRouter (one mechanism
    /// with the watch surface — the 2026-09 unification).
    router: crate::FrameRouter,
    /// The ask-answerers: the shipped forward-and-relay default
    /// yields to the first author registration (a custom beside the
    /// default would double-surface the card); author registrations
    /// then stack — any may answer, the child's hub takes the first,
    /// late answers are tolerated no-ops (the co-frontend law).
    ask_answerers: Mutex<Vec<FrameHandler>>,
    /// Whether the shipped default still stands (replaced by the
    /// first author registration).
    ask_default: Mutex<bool>,
    /// The driver's command lane — answers ride it home to the child.
    commands: Mutex<Option<std::sync::mpsc::Sender<ChildCmd>>>,
}

/// One owned child: spawn, per-kind observation, one task at a time
/// through the shared settle fold, kill at the owner's hand.
#[derive(Clone)]
pub struct Child {
    id: Arc<String>,
    commands: std::sync::mpsc::Sender<ChildCmd>,
    registry: Arc<Registry>,
}

enum ChildCmd {
    Run {
        task: String,
        reply: std::sync::mpsc::Sender<Result<Settlement, String>>,
    },
    /// A relayed answer coming home: the interaction response, down
    /// the child's pipe by id.
    Answer {
        id: String,
        payload: serde_json::Value,
    },
    Kill,
}

impl Child {
    /// Spawn one owned child. The binary is the host's own (the
    /// initialize's `core_path` — the host IS the binary); the spawn
    /// resolves the handshake before this call returns.
    pub fn create(ctx: &Ctx, options: ChildOptions) -> Result<Child, String> {
        let core_path = ctx.core_path()?;
        let mut spec = ChildSpec::new(std::path::PathBuf::from(core_path), options.cwd);
        if let Some(reference) = &options.model {
            let (provider, model) = reference
                .split_once('/')
                .ok_or("the model reference must be `provider/model`")?;
            spec = spec.model(tabit_protocol::ModelSelection::new(provider, model));
        }
        if let Some(path) = &options.session {
            spec = spec.session(path.clone());
        }
        if let Some(max_turns) = options.max_turns {
            spec = spec.max_turns(max_turns);
        }

        let registry = Arc::new(Registry {
            router: crate::FrameRouter::default(),
            ask_answerers: Mutex::new(vec![Arc::new(forward_frame)]),
            ask_default: Mutex::new(true),
            commands: Mutex::new(None),
        });
        // The generic slot's default: forward-everything when the
        // boolean says so. The ask slot's default (forward-and-relay —
        // an unanswered ask hangs a child, so asks surface by default)
        // is installed above and yields to the first author
        // registration.
        if options.forwarding {
            registry.router.set_generic(forward_frame);
        }

        // The driver task owns the handle on the SDK's runtime: one
        // loop — commands in, the shared settle fold under the tap
        // that feeds the dispatcher, the settlement reported per run.
        let (spawn_tx, spawn_rx) = std::sync::mpsc::channel::<Result<String, String>>();
        let (frames_tx, frames_rx) = std::sync::mpsc::channel::<EventFrame>();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<ChildCmd>();
        let runtime = runtime();
        runtime.spawn(async move {
            let mut handle = match spec.spawn().await {
                Ok(handle) => handle,
                Err(error) => {
                    let _ = spawn_tx.send(Err(error));
                    return;
                }
            };
            let _ = spawn_tx.send(Ok(handle.id().to_string()));
            while let Ok(command) = cmd_rx.recv() {
                match command {
                    ChildCmd::Run { task, reply } => {
                        handle.prompt(task);
                        let settled = handle
                            .settle_with_tap(None, |frame| {
                                let _ = frames_tx.send(frame.clone());
                            })
                            .await;
                        let _ = reply.send(Ok(settled));
                    }
                    ChildCmd::Answer { id, payload } => {
                        handle.send_line(tabit_protocol::to_wire_line(
                            &tabit_protocol::SessionCommand::InteractionResponse {
                                session: Some(handle.id().to_string()),
                                id,
                                payload,
                            },
                        ))
                    }
                    ChildCmd::Kill => handle.close(),
                }
            }
            // The commands sender drops with the Child clones; the
            // handle drops here — the reaper bounds the exit.
        });
        let id = spawn_rx
            .recv()
            .map_err(|_| "the child driver died at the spawn".to_string())??;

        // The dispatcher: every frame through the registry, every
        // handler on its own thread — observation never blocks the
        // fold's tap.
        let dispatch_ctx = Ctx::watch_context(ctx.shared_clone());
        let dispatch_registry = registry.clone();
        std::thread::spawn(move || {
            for frame in frames_rx {
                dispatch_frame(&dispatch_ctx, &dispatch_registry, frame);
            }
        });

        *crate::sdk_lock(&registry.commands) = Some(cmd_tx.clone());
        Ok(Child {
            id: Arc::new(id),
            commands: cmd_tx,
            registry,
        })
    }

    /// The child session's id — its stream stamp.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Observe one event kind (typed, thread-dispatched like every
    /// invocation). Observation composes: many subscribers per kind,
    /// plus the generic slot (forwarding) when installed.
    pub fn on<F>(&self, kind: &str, body: F) -> Result<(), String>
    where
        F: Fn(&Ctx, &SessionEvent) + Send + Sync + 'static,
    {
        self.registry.on(kind, body)
    }

    /// Register an ask-answerer for the child. The first
    /// registration replaces the shipped forward-and-relay default
    /// (a custom beside it would double-surface the card);
    /// registrations then stack — any answerer may answer, the
    /// child's hub takes the first arrival, and a late answer is a
    /// tolerated no-op (the co-frontend law, applied one hop down).
    /// Answer with [`Child::answer`]; a handler that surfaces the
    /// question to users itself (its own card) relays the answer it
    /// receives.
    pub fn on_ask<F>(&self, body: F) -> Result<(), String>
    where
        F: Fn(&Ctx, &EventFrame) + Send + Sync + 'static,
    {
        self.registry.on_ask(body)
    }

    /// Answer one of the child's questions by id. Races are the
    /// co-frontend law: the child takes the first answer to land;
    /// one that arrives after another (or after the question died)
    /// is a tolerated no-op.
    pub fn answer(&self, id: &str, payload: serde_json::Value) {
        if let Some(commands) = crate::sdk_lock(&self.registry.commands).clone() {
            let _ = commands.send(ChildCmd::Answer {
                id: id.to_string(),
                payload,
            });
        }
    }

    /// Run one task to the child's terminal (blocking — the shared
    /// settle fold). The child's asks surface per the registry while
    /// the run is in flight.
    pub fn run(&self, task: String) -> Result<Settlement, String> {
        let (tx, rx) = std::sync::mpsc::channel::<Result<Settlement, String>>();
        self.commands
            .send(ChildCmd::Run { task, reply: tx })
            .map_err(|_| "the child's driver is gone".to_string())?;
        rx.recv()
            .map_err(|_| "the child's driver is gone".to_string())?
    }

    /// Kill the child now (idempotent): stdin closes, the reaper's
    /// grace bounds the exit with the tree kill.
    pub fn kill(&self) {
        let _ = self.commands.send(ChildCmd::Kill);
    }
}

impl Registry {
    /// The observation registration (many per kind; the ask kind is
    /// refused here — it owes an answer).
    fn on<F>(&self, kind: &str, body: F) -> Result<(), String>
    where
        F: Fn(&Ctx, &SessionEvent) + Send + Sync + 'static,
    {
        if kind == tags::INTERACTION_REQUEST {
            return Err(
                "interaction_request owes an answer — register with `on_ask` (its single owner slot)"
                    .to_string(),
            );
        }
        self.router
            .register(kind, move |ctx, frame| body(ctx, &frame.event));
        Ok(())
    }

    /// The ask law: the default yields to the first author
    /// registration, then registrations stack (any may answer, the
    /// child arbitrates, late answers are no-ops).
    fn on_ask<F>(&self, body: F) -> Result<(), String>
    where
        F: Fn(&Ctx, &EventFrame) + Send + Sync + 'static,
    {
        let mut answerers = crate::sdk_lock(&self.ask_answerers);
        if *crate::sdk_lock(&self.ask_default) {
            answerers.clear();
            *crate::sdk_lock(&self.ask_default) = false;
        }
        answerers.push(Arc::new(body));
        Ok(())
    }
}

/// One frame through the registry: the ask slot (single owner), the
/// kind's specifics, the generic slot for kinds without specifics —
/// one map probe plus the fallback, the ruled lookup law.
fn dispatch_frame(ctx: &Ctx, registry_arc: &Arc<Registry>, frame: EventFrame) {
    let registry = registry_arc;
    let shared = ctx.shared_clone();
    let kind = frame.event.tag();
    if kind == tags::INTERACTION_REQUEST {
        // The ask arm: the relay registration (the routed answer
        // finds this child's driver through the extension's own
        // pending map — the same registry `Ctx::ask` answers
        // through) stands for the shipped default; every answerer
        // runs, any may answer, the child's hub takes the first
        // arrival, late answers are tolerated no-ops.
        let id = match &frame.event {
            SessionEvent::InteractionRequest { id, .. } => id.clone(),
            _ => return,
        };
        let (tx, rx) = std::sync::mpsc::channel::<serde_json::Value>();
        crate::register_relay(&shared, &id, tx);
        let answerers = crate::sdk_lock(&registry.ask_answerers).clone();
        let default_stands = *crate::sdk_lock(&registry.ask_default);
        for answerer in answerers {
            let frame = frame.clone();
            spawn_handler(shared.clone(), move |ctx| answerer(&ctx, &frame));
        }
        if default_stands {
            let registry_ref = registry_arc.clone();
            spawn_handler(shared, move |_ctx| {
                // Drain the relay until an answer lands, then carry it
                // home to the child. A kill ends the loop with the
                // pipe; a dismissed card starves until then (the
                // documented shape — the author's timeout is author
                // code).
                if let Ok(answer) = rx.recv()
                    && let Some(commands) = crate::sdk_lock(&registry_ref.commands).clone()
                {
                    let _ = commands.send(ChildCmd::Answer {
                        id,
                        payload: answer,
                    });
                }
            });
        }
        return;
    }
    registry.router.dispatch(&shared, &frame);
}

fn spawn_handler(shared: std::sync::Arc<Shared>, body: impl FnOnce(Ctx) + Send + 'static) {
    std::thread::spawn(move || {
        let ctx = Ctx::watch_context(shared);
        crate::catch_unwind_silently(move || body(ctx));
    });
}

/// The shipped forward callback (the generic slot and the ask
/// default): the frame crosses verbatim (stream preserved; the host
/// re-stamps the origin, naming this extension as the conduit). The
/// ask install site differs only in what the dispatcher registers
/// alongside it — the id relay that carries routed answers home.
fn forward_frame(ctx: &Ctx, frame: &EventFrame) {
    let _ = emit(&ctx.shared_clone(), frame);
}

/// The SDK's one runtime for owned children (multi-thread; the
/// settle folds and spawns ride it).
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        #[allow(clippy::expect_used)]
        // sanctioned crash: a runtime that fails to build cannot be served around
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("the owned-children runtime builds")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry {
            router: crate::FrameRouter::default(),
            ask_answerers: Mutex::new(Vec::new()),
            ask_default: Mutex::new(true),
            commands: Mutex::new(None),
        }
    }

    #[test]
    fn observation_kinds_compose_many_subscribers() {
        let registry = registry();
        registry
            .on(tags::RUN_FINISHED, |_ctx, _event| {})
            .expect("the first registers");
        registry
            .on(tags::RUN_FINISHED, |_ctx, _event| {})
            .expect("and so does the second — observation composes");
        // Two registrations of one kind compose — the router's law;
        // both succeeded, which is it.
    }

    #[test]
    fn the_ask_kind_is_refused_on_the_observation_path() {
        let registry = registry();
        assert!(
            registry
                .on(tags::INTERACTION_REQUEST, |_ctx, _event| {})
                .is_err()
        );
    }

    #[test]
    fn the_ask_law_defaults_yield_then_authors_stack() {
        let registry = registry();
        // The default stands; the first author registration replaces
        // it (a custom beside the default would double-surface the
        // card)...
        registry
            .on_ask(|_ctx: &Ctx, _frame: &EventFrame| {})
            .expect("the first replaces the default");
        assert!(
            !*crate::sdk_lock(&registry.ask_default),
            "the default yielded"
        );
        assert_eq!(crate::sdk_lock(&registry.ask_answerers).len(), 1);
        // ...and further registrations stack — any may answer, the
        // child arbitrates, late answers are no-ops (the co-frontend
        // law one hop down).
        registry
            .on_ask(|_ctx: &Ctx, _frame: &EventFrame| {})
            .expect("the second stacks");
        assert_eq!(crate::sdk_lock(&registry.ask_answerers).len(), 2);
    }
}
