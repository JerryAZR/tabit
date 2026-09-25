//! Owned children: an extension's sessions-to-itself, over the
//! shared client (`tabit-wire`) — the same substrate core's subagent
//! bridge drives, the same settle fold, the same reaper. The child's
//! **lane** is a channel on the guest's node: its frames arrive
//! through the node's intake (each card registering a transit entry
//! whose delivery carries the answer home down the child's stdin),
//! its observation registrations live in the node's tables, and its
//! death sweeps them all — subscriptions, learned routes, and open
//! asks, every stranded card settling announced.
//!
//! The registry laws (the 2026-09 rulings, made structural):
//!
//! - **Per-kind observation is subscription** on the guest's node
//!   (owner: the child's id — the death-sweep key). Observation
//!   composes: the child's `on` handlers, the extension's watches,
//!   and other children's handlers all hear a kind's fan.
//! - **Asks follow the co-frontend law, and the card surface is
//!   arrival-lane truth.** The node's fan is arrival-lane-blind, so
//!   the per-child ask policy lives in the settle fold's tap — the
//!   one place that IS the child's pipe: every card crossing it
//!   (the child's own, a grandchild's relayed through it) reaches
//!   the surface. The shipped forward-and-relay default crosses
//!   cards and their settles verbatim to the host — from the
//!   arrival lane, stamps intact, addressability preserved; the
//!   first author registration replaces it (a custom beside the
//!   default would double-surface the card), and author
//!   registrations then stack — any answerer may answer, the
//!   child's hub takes the first arrival, a late answer is a
//!   tolerated no-op. There is no single-owner rule anywhere:
//!   answers are races, arbitrated where the question lives — the
//!   transit entry.
//! - **No lift**: an id-swap re-ask has no reason to exist — the
//!   child's id-space never mints onto the host stream, and the one
//!   card a user sees is the child's own.
//!
//! The child's terminal vocabulary: its run completes or fails on
//! its own, or its owner kills it (this type's kill, the creating
//! call's cancellation via the settle leash, drop). Nobody else
//! commands it.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tabit_protocol::{EventFrame, SessionEvent, tags};
use tabit_wire::client::{ChildSpec, Settlement};
use tabit_wire::node::KIND_INTERACTION;

use crate::{Ctx, Shared, sdk_lock};

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

    /// Forward the child's non-card events verbatim to the host's
    /// pipe — the arrival lane's own emission (the child's stamps
    /// intact, its routes untouched). Cards and settles cross by the
    /// ask policy's own path, never twice.
    pub fn forwarding(mut self, forwarding: bool) -> Self {
        self.forwarding = forwarding;
        self
    }
}

/// One frame handler after erasure: a cloned context plus the frame
/// (not the bare event — forwarding and ask-owning both need the
/// stream stamp), returning the handler's future.
type FrameHandler =
    Arc<dyn Fn(Ctx, EventFrame) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The per-child ask policy, consulted at the arrival lane (the
/// settle fold's tap): whether the shipped forward-and-relay default
/// still stands, and the author answerers it yielded to. One lock
/// for both facts — a registration and a consultation never observe
/// a half-replaced default (a card in that window would surface
/// nowhere).
struct AskPolicy {
    inner: Mutex<AskState>,
}

struct AskState {
    /// Whether the shipped default still stands (the first author
    /// registration retires it — a custom beside the default would
    /// double-surface the card).
    default: bool,
    /// The author answerers, stacking (any may answer; the child's
    /// hub arbitrates; late answers are tolerated no-ops).
    answerers: Vec<FrameHandler>,
}

impl AskPolicy {
    fn shipped() -> Self {
        Self {
            inner: Mutex::new(AskState {
                default: true,
                answerers: Vec::new(),
            }),
        }
    }

    /// One author answerer: retires the default (idempotent — only
    /// the first registration finds it standing), then stacks.
    fn register_answerer(&self, body: FrameHandler) {
        let mut state = sdk_lock(&self.inner);
        state.default = false;
        state.answerers.push(body);
    }

    /// What the arrival lane does with one frame's kind, and the
    /// answerers to reach if that is the action.
    fn lane_action(&self, forwarding: bool, tag: &str) -> (LaneAction, Vec<FrameHandler>) {
        let state = sdk_lock(&self.inner);
        let action = lane_action(state.default, !state.answerers.is_empty(), forwarding, tag);
        (action, state.answerers.clone())
    }
}

/// What the arrival lane does with one frame — the crossing rules in
/// one place. The card kind belongs to the ask surface (the shipped
/// default's verbatim crossing, or the author answerers that
/// replaced it). The settle kind reaches the answerers (the pair —
/// whoever surfaces a card hears it close) but never crosses here:
/// settles cross by the stdio's one default subscription whatever
/// the ask policy — the card law's close vocabulary always crosses.
/// Everything else crosses only under the forwarding option.
enum LaneAction {
    /// The frame crosses to the host verbatim (the arrival lane's
    /// own write — no fan, no teaching; the intake already did
    /// both).
    Forward,
    /// The frame reaches the author answerers (their threads).
    Answerers,
    /// The frame stays local (the intake's fan is all it gets).
    None,
}

fn lane_action(
    default_stands: bool,
    has_answerers: bool,
    forwarding: bool,
    tag: &str,
) -> LaneAction {
    if tag == tags::INTERACTION_REQUEST {
        if default_stands {
            LaneAction::Forward
        } else if has_answerers {
            LaneAction::Answerers
        } else {
            LaneAction::None
        }
    } else if tag == tags::INTERACTION_SETTLED {
        if has_answerers {
            LaneAction::Answerers
        } else {
            LaneAction::None
        }
    } else if forwarding {
        LaneAction::Forward
    } else {
        LaneAction::None
    }
}

/// One owned child: spawn, per-kind observation, one task at a time
/// through the shared settle fold, kill at the owner's hand.
#[derive(Clone)]
pub struct Child {
    id: Arc<String>,
    shared: Arc<Shared>,
    commands: tokio::sync::mpsc::UnboundedSender<ChildCmd>,
    asks: Arc<AskPolicy>,
}

enum ChildCmd {
    Run {
        task: String,
        reply: tokio::sync::oneshot::Sender<Result<Settlement, String>>,
    },
    Kill,
}

impl Child {
    /// Spawn one owned child. The binary is the host's own (the
    /// initialize's `core_path` — the host IS the binary); the spawn
    /// resolves the handshake before this call returns.
    pub async fn create(ctx: &Ctx, options: ChildOptions) -> Result<Child, String> {
        let shared = ctx.shared_clone();
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
        // The lane mount is the client's (`on_node`): armed inside the
        // frame pump at the handshake, every stamped arrival intaking
        // through the lane, the exit sweeping it (routes, transit
        // asks, settles announced — the stdio's settle subscription
        // carries the announce across).
        spec = spec.on_node(shared.node.clone());
        // The card surface rides the pump-order policy seam, AFTER
        // the mount's intake (an answerer may answer the moment the
        // transit entry exists) and always-live — not only during
        // runs: a card crossing the child's pipe surfaces the moment
        // it arrives.
        let asks = Arc::new(AskPolicy::shipped());
        let policy_asks = asks.clone();
        let policy_shared = shared.clone();
        let forwarding = options.forwarding;
        spec = spec.on_stamped_frame(Arc::new(move |_child: &str, frame: &EventFrame| {
            let (action, answerers) = policy_asks.lane_action(forwarding, frame.event.tag());
            match action {
                // The verbatim crossing: the write alone — the
                // mount's intake already taught the route and fanned
                // the local subscribers, so this is never a second
                // delivery.
                LaneAction::Forward => policy_shared.stdio.send_event(frame),
                LaneAction::Answerers => {
                    for answerer in answerers {
                        let frame = frame.clone();
                        crate::spawn_observation(policy_shared.clone(), move |ctx| {
                            answerer(ctx, frame)
                        });
                    }
                }
                LaneAction::None => {}
            }
        }));
        // The driver task owns the handle on the SDK's runtime: one
        // loop — commands in, the shared settle fold per run, the
        // settlement reported back.
        let (spawn_tx, spawn_rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<ChildCmd>();
        tokio::spawn(async move {
            let mut handle = match spec.spawn().await {
                Ok(handle) => handle,
                Err(error) => {
                    let _ = spawn_tx.send(Err(error));
                    return;
                }
            };
            let _ = spawn_tx.send(Ok(handle.id().to_string()));
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    ChildCmd::Run { task, reply } => {
                        handle.prompt(task);
                        let settled = handle.settle(None).await;
                        let _ = reply.send(Ok(settled));
                    }
                    ChildCmd::Kill => handle.close(),
                }
            }
            // The commands sender drops with the Child clones; the
            // handle drops here — the reaper bounds the exit, and the
            // mount's sweep clears the node.
        });
        let id = spawn_rx
            .await
            .map_err(|_| "the child driver died at the spawn".to_string())??;

        Ok(Child {
            id: Arc::new(id),
            shared,
            commands: cmd_tx,
            asks,
        })
    }

    /// The child session's id — its stream stamp.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Observe one event kind (typed, task-dispatched like every
    /// invocation). Observation composes: the child's handlers, the
    /// extension's watches of the same kind, and other children's
    /// all hear the fan — the node's one subscription surface, owned
    /// by the child's id (the death sweep's key: the mount's exit
    /// sweep retracts it with everything else the child holds).
    pub fn on<F, Fut>(&self, kind: &str, body: F) -> Result<(), String>
    where
        F: Fn(Ctx, SessionEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        refuse_ask_kind(kind)?;
        let shared = self.shared.clone();
        let body: crate::ErasedWatch = Arc::new(move |ctx, event| Box::pin(body(ctx, event)));
        let owner = (*self.id).clone();
        self.shared.node.subscribe(kind, &owner, move |frame| {
            let event = frame.event.clone();
            let (shared, body) = (shared.clone(), body.clone());
            crate::spawn_observation(shared, move |ctx| body(ctx, event));
        });
        Ok(())
    }

    /// Register an ask-answerer for the child — the arrival lane's
    /// ask surface: every card crossing this child's pipe reaches
    /// the answerers (the child's own, a grandchild's relayed
    /// through it). The first registration retires the shipped
    /// forward-and-relay default (a custom beside it would
    /// double-surface the card); registrations then stack — any
    /// answerer may answer, the child's hub takes the first arrival,
    /// and a late answer is a tolerated no-op (the co-frontend law).
    /// Answer with [`Child::answer`]; a handler that surfaces the
    /// question to users itself (its own card) relays the answer it
    /// receives. Answerers hear the card and its settle (the pair —
    /// whoever surfaces a card hears it close).
    pub fn on_ask<F, Fut>(&self, body: F) -> Result<(), String>
    where
        F: Fn(Ctx, EventFrame) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.asks
            .register_answerer(Arc::new(move |ctx, frame| Box::pin(body(ctx, frame))));
        Ok(())
    }

    /// Answer one of the child's questions by id — the ask table's
    /// claim: the transit entry's delivery writes the response line
    /// down the child's stdin. Races are the co-frontend law: the
    /// child takes the first answer to land; one that arrives after
    /// another (or after the question died) is a tolerated no-op.
    pub fn answer(&self, id: &str, payload: serde_json::Value) {
        let _ = self
            .shared
            .node
            .answer(id, KIND_INTERACTION, Box::new(payload));
    }

    /// Run one task to the child's terminal (the shared settle fold).
    /// The child's asks surface per the policy while the run is in
    /// flight.
    pub async fn run(&self, task: String) -> Result<Settlement, String> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Settlement, String>>();
        self.commands
            .send(ChildCmd::Run { task, reply: tx })
            .map_err(|_| "the child's driver is gone".to_string())?;
        rx.await
            .map_err(|_| "the child's driver is gone".to_string())?
    }

    /// Kill the child now (idempotent): stdin closes, the reaper's
    /// grace bounds the exit with the tree kill, and the exit tap
    /// sweeps the child's every registration.
    pub fn kill(&self) {
        let _ = self.commands.send(ChildCmd::Kill);
    }
}

/// The observation path's vocabulary guard: the card kind owes an
/// answer, and answers belong to the ask policy (`on_ask`).
fn refuse_ask_kind(kind: &str) -> Result<(), String> {
    if kind == tags::INTERACTION_REQUEST {
        return Err(
            "interaction_request owes an answer — register with `on_ask` (its answerer list)"
                .to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tabit_wire::node::{Channel, Inbound};

    /// The ask law: the shipped default owns the card kind's
    /// crossing until the first author answerer retires it;
    /// registrations then stack — and no window exists where a card
    /// surfaces nowhere.
    #[test]
    fn the_ask_law_defaults_yield_then_authors_stack() {
        let asks = AskPolicy::shipped();
        // The default stands: cards cross verbatim; settles cross by
        // the stdio's settle subscription (never here); non-cards
        // cross only under the forwarding option.
        let (action, _) = asks.lane_action(false, tags::INTERACTION_REQUEST);
        assert!(matches!(action, LaneAction::Forward));
        let (action, _) = asks.lane_action(false, tags::INTERACTION_SETTLED);
        assert!(matches!(action, LaneAction::None));
        let (action, _) = asks.lane_action(false, tags::RUN_FINISHED);
        assert!(matches!(action, LaneAction::None));
        let (action, _) = asks.lane_action(true, tags::RUN_FINISHED);
        assert!(matches!(action, LaneAction::Forward));

        asks.register_answerer(Arc::new(|_ctx: Ctx, _frame: EventFrame| Box::pin(async {})));
        asks.register_answerer(Arc::new(|_ctx: Ctx, _frame: EventFrame| Box::pin(async {})));
        {
            let state = sdk_lock(&asks.inner);
            assert_eq!(state.answerers.len(), 2, "author registrations stack");
            assert!(!state.default, "the default yielded");
        }
        // The card kind now reaches the answerers, never both; the
        // settle kind reaches them too (the pair) without crossing.
        let (action, answerers) = asks.lane_action(true, tags::INTERACTION_REQUEST);
        assert!(matches!(action, LaneAction::Answerers));
        assert_eq!(answerers.len(), 2);
        let (action, _) = asks.lane_action(false, tags::INTERACTION_SETTLED);
        assert!(matches!(action, LaneAction::Answerers));
    }

    /// The ingress skip matches channel identity, never owner
    /// strings: the child's observation handler — a plain callback
    /// owned by the lane's own id, swept with the child's everything
    /// in one act — still hears frames arriving on the lane.
    #[test]
    fn arrivals_on_the_lane_reach_the_same_owner_callback() {
        let shared = crate::tests::shared();
        let node = &shared.node;
        let lane = Channel::local("child-1", |_| {}, |_| {});
        let heard = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let sink = heard.clone();
        node.subscribe(tags::RUN_FINISHED, lane.owner(), move |_| {
            *sink.lock().expect("test lock") += 1
        });
        node.intake(
            &lane,
            Inbound::Event(EventFrame {
                stream: None,
                origin: None,
                ttl: None,
                event: SessionEvent::RunFinished {
                    output: String::new(),
                    started_at_ms: 0,
                    completed_at_ms: 0,
                    durable: false,
                },
            }),
        );
        assert_eq!(
            *heard.lock().expect("test lock"),
            1,
            "the observation heard its own child's frame"
        );
    }

    /// The observation kind that owes an answer is refused on the
    /// observation path — it belongs to `on_ask`.
    #[test]
    fn the_ask_kind_is_refused_on_the_observation_path() {
        assert!(refuse_ask_kind(tags::INTERACTION_REQUEST).is_err());
        assert!(refuse_ask_kind(tags::RUN_FINISHED).is_ok());
    }
}
