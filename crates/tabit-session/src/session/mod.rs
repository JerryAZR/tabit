//! The session facade: owns the entry log, the model selection, and the
//! outer loop's policy, and consumes the rig-agent item stream as its
//! driver.
//!
//! User messages enter through one door — the run-agnostic mailbox
//! ([`Session::submit`]) — and are drained by [`Session::pump`]: as the
//! next run's initial prompt, or — while a run is in flight — as a steer
//! injected at the next turn boundary. Because the mailbox outlives runs,
//! a message submitted at any instant is never lost; only the clear
//! sites discard queued messages (abort, checkout — each only what was
//! submitted before it). Each pump iteration is one outer loop: the user
//! message commits through the prompt barrier, the rig-agent engine runs
//! the turns (a recorder hook stages each completed turn; the roundtrip
//! commits atomically when the item stream closes it), and the item
//! stream is folded into the serializable event list a frontend
//! consumes. The recorder's resident state — the conversation tree, the
//! head, the incrementally folded context — is the in-session truth; the
//! file is its write-behind mirror and the handoff between processes,
//! parsed once at load and never re-read mid-session. Permissions and
//! extensions later plug into this same seam.
//!
//! Split by concern: [`builder`] (construction — fresh `create` and log
//! `resume`), [`mailbox`] (the message queue, its handles, the engine's
//! steering view), [`run`] (the outer loop — pump, drive, conclude, the
//! item-to-event fold), [`rewind`] (chain checkouts), [`selection`] (the
//! model register and its write path), [`persist`] (write-behind
//! durability: the cleanliness verdict and notices), [`assemble`] (agent
//! construction and the session's own assembly), [`wire`] (protocol-shape
//! translations).

mod assemble;
mod builder;
mod mailbox;
mod persist;
mod rewind;
mod run;
mod selection;
mod wire;

pub(crate) use builder::ModelFactory;
pub use builder::SessionBuilder;
pub use mailbox::{AbortHandle, MailboxHandle};
pub use rewind::RewindSummary;
pub use run::{RunOutcome, RunSummary};
pub(crate) use selection::{ModelProbe, ModelRegister};
pub(crate) use wire::{result_details, result_text, user_text, wire_status};

use crate::context_manager::ContextManager;
use crate::interaction::InteractionHub;
use crate::notice::NoticeSlot;
use crate::stats::{ModelStats, SessionStats, UsageLedger};
use mailbox::Mailbox;
use rig_agent::agent::Agent;
use rig_agent::completion::{Message, Usage};
use rig_agent::tool::DynamicTool;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tabit_config::TabitConfig;
use tabit_protocol::{ModelSelection, SessionEvent};
use tokio_util::sync::CancellationToken;

/// Default model-call budget for one outer loop.
pub const DEFAULT_MAX_TURNS: usize = 32;

/// How many of a turn's tool chains run at once (ENGINE.md's tool
/// phase: chains are independent and bounded). Named and visible —
/// a config surface arrives with the settings story.
pub const TOOL_CONCURRENCY: usize = 4;

/// A persistent, resumable conversation.
pub struct Session {
    config: Arc<TabitConfig>,
    /// The active model selection — a **shared cell, not worker
    /// state**: the endpoint writes it at receive through the
    /// [`ModelRegister`] (record + swap, one operation, any thread),
    /// and every reader derives — run open's agent derivation
    /// ([`Self::ensure_agent`]), announcements. The lazy-agent rule
    /// makes any-writer safe: the reader checks freshness, it does not
    /// trust writers to rebuild.
    selection: Arc<Mutex<ModelSelection>>,
    preamble: Option<String>,
    tools: Vec<DynamicTool>,
    max_turns: usize,
    model_factory: ModelFactory,
    /// The assembly's mounted hook stack (see
    /// [`SessionBuilder::hooks`]); added to every run.
    run_hooks: Option<rig_agent::agent::HookStack>,
    /// The built agent — a derived cache of `selection`, not a second
    /// truth. Run open rebuilds it whenever it no longer matches the
    /// selection (owner ruling 2026-08: check at the single point of
    /// use, so a stale agent cannot serve a request no matter who
    /// wrote the selection or how). `agent_built_for` is the cache key.
    agent: Arc<Agent>,
    agent_built_for: ModelSelection,
    /// The conversation's source of truth (tabit-log): owned here,
    /// forever — the engine never holds it; the handler folds at the
    /// item arms (steer drained → fold, batch settled → fold_all,
    /// final committed → fold), and the receive-time probes read it
    /// through the same cell (the shared handle below).
    conversation: Arc<std::sync::RwLock<ContextManager>>,
    /// The shared write buffer: the session's own handle for side
    /// records (the manager holds its clone; the writer lives behind
    /// it).
    buffer: crate::writer::SharedBuffer,
    /// The receive-time view of the conversation (checkout validation
    /// at receive): reads through a lock so a racing checkout sees the
    /// live tree even mid-run — the probes never mutate.
    shared_conversation: SharedConversation,
    /// The persist-state notice sink (flag 8's degraded/recovered
    /// events), attached by the worker at spawn.
    persist_notices: Arc<NoticeSlot>,
    /// The cumulative usage ledger as of load (the parser's fold over
    /// the file's usage facts — usage facts ride records; the ledger
    /// is derived at open, and **deferred after**: the live manager
    /// records zero usage (the ruling) until the usage discussion
    /// returns, so the ledger's live growth rejoins then. Stats at
    /// close are the as-of-load totals.
    ledger: crate::stats::UsageLedger,
    /// Per-run cancellation token, refreshed by every outer loop; the
    /// abort handle cancels whatever run is current.
    abort: std::sync::Arc<std::sync::Mutex<CancellationToken>>,
    /// The run-agnostic message mailbox: the one door user messages enter
    /// (see [`Session::submit`]); drained by [`Session::pump`] and by the
    /// engine's turn-boundary steering.
    mailbox: Mailbox,
    /// The session file, when the session is file-backed. `None` for
    /// an **ephemeral** session ([`SessionBuilder::ephemeral`]): it
    /// lives in memory only (a `NullBuffer` underneath — everything
    /// folds and grows; nothing persists), so there is nothing to
    /// resume, replay, or list. The subagent scratch child.
    path: Option<PathBuf>,
    /// The working directory this session runs in — recorded in the
    /// header, mounted into every run's tool context as
    /// [`SessionCwd`](rig_agent::tool::SessionCwd) so relative tool
    /// paths and spawned commands resolve against it, not the process
    /// cwd (the subagent ruling: a child may scope elsewhere).
    cwd: PathBuf,
    id: String,
    /// Whether this session continues an existing chain (`resume`) or
    /// started fresh (`create`) — reported in the handshake so a
    /// frontend that asked to resume can note a silent fresh start.
    resumed: bool,
    /// The interaction hub, attached by the session worker when it takes
    /// ownership (the hub needs the worker's event channel, which does
    /// not exist until spawn). `None` for direct [`Session`] consumers:
    /// the permission gate fails closed and ask-the-user tools report
    /// no frontend, in-band.
    interaction: Option<InteractionHub>,
}

/// The receive-time view of the conversation (checkout validation at
/// receive): a locked read over the session's manager — the probes
/// never mutate, and the read is the live tree even mid-run.
#[derive(Clone)]
pub(crate) struct SharedConversation {
    conversation: Arc<std::sync::RwLock<ContextManager>>,
}

impl SharedConversation {
    /// Whether `id` names a node in the tree (any branch) — the
    /// receive-time checkout validation.
    pub(crate) fn contains(&self, id: &str) -> bool {
        crate::lock::read(&self.conversation).contains(id)
    }
}

impl Session {
    /// The read-only conversation probe (checkout validation at
    /// receive time — see [`SharedConversation`]).
    pub(crate) fn entry_id_probe(&self) -> SharedConversation {
        self.shared_conversation.clone()
    }

    /// Attach the interaction hub. Called once by the session worker
    /// ([`crate::endpoint::spawn_worker`]) when it takes
    /// ownership — the hub is built over the worker's event channel,
    /// which exists only there.
    pub fn attach_interaction(&mut self, hub: InteractionHub) {
        self.interaction = Some(hub);
    }

    /// The session id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The session file, when file-backed; `None` for an ephemeral
    /// session (nothing to resume or list).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The path as the wire carries it — the empty string for an
    /// ephemeral session (a frontend treats empty as "no file"; the
    /// v5 changelog states it).
    pub(crate) fn wire_path(&self) -> String {
        self.path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    }

    /// The working directory this session runs in.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Whether this session continues an existing chain or started
    /// fresh. A frontend that asked to resume (`--continue`) reports a
    /// silent fresh start from this (the pinned startup contract: an
    /// empty store is not an error).
    pub fn resumed(&self) -> bool {
        self.resumed
    }

    /// The projected model-visible context (what the next outer loop
    /// sees) — the derived view, folded per call.
    pub fn context(&self) -> Vec<Message> {
        crate::lock::read(&self.conversation).messages()
    }

    /// Usage and cost totals — the cumulative ledger as of load
    /// (usage facts are deferred live, the ruling; the totals resume
    /// at the usage discussion) with costs derived from the config's
    /// rates.
    pub fn stats(&self) -> SessionStats {
        let ledger: UsageLedger = self.ledger.clone();
        let mut stats = SessionStats::default();
        for model_usage in ledger.per_model() {
            let mut model_stats = ModelStats {
                provider: model_usage.provider.clone(),
                model: model_usage.model.clone(),
                thinking_level: model_usage.thinking_level.clone(),
                usage: model_usage.usage,
                cost: None,
            };
            if let Some(cost) = self
                .config
                .provider(&model_stats.provider)
                .and_then(|p| p.model(&model_stats.model))
                .and_then(|m| m.cost)
            {
                let dollars = cost_of(&model_stats.usage, &cost);
                stats.total_cost += dollars;
                model_stats.cost = Some(dollars);
            }
            stats.per_model.push(model_stats);
        }
        stats.total_usage = ledger.total_usage();
        stats
    }

    /// The replay pass (PROTOCOL.md v2): the active branch (the
    /// temporary path container, materialized on demand) projected into
    /// finalized live events — the same shapes a live run produces,
    /// ids verbatim from the tree, so a frontend renders history and
    /// live turns with one set of arms. A checkout re-renders over a
    /// different branch through the same door.
    pub fn replay_events(&self) -> Vec<SessionEvent> {
        crate::replay::project_events(&crate::lock::read(&self.conversation).active_branch())
    }
}

fn cost_of(usage: &Usage, cost: &tabit_config::Cost) -> f64 {
    (usage.input_tokens as f64 / 1_000_000.0) * cost.input
        + (usage.output_tokens as f64 / 1_000_000.0) * cost.output
        + (usage.cached_input_tokens as f64 / 1_000_000.0) * cost.cache_read
        + (usage.cache_creation_input_tokens as f64 / 1_000_000.0) * cost.cache_write
}
