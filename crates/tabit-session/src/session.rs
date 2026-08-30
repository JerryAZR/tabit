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

use crate::context_manager::ContextManager;
use crate::entry::{FileRecord, SideKind, SideRecord};
use crate::error::SessionError;
use crate::interaction::InteractionHub;
use crate::lock::lock;
use crate::model::validate_selection;
use crate::registry::ModelRegistry;
use crate::stats::{UsageLedger, add_usage};
use crate::store::SessionStore;
use crate::writer::SessionWriter;
use futures::StreamExt;
use rig_agent::agent::{Agent, AgentBuilder, ModelHandle};
use rig_agent::agent::{MultiTurnStreamItem, StreamingError};
use rig_agent::completion::{Message, Usage};
use rig_agent::streaming::{StreamedUserContent, StreamingChat};
use rig_agent::tool::DynamicTool;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tabit_config::{AuthConfig, TabitConfig};
use tabit_protocol::{EventFrame, ModelSelection, SessionEvent, StreamId};
use tokio_util::sync::CancellationToken;

/// Default model-call budget for one outer loop.
pub const DEFAULT_MAX_TURNS: usize = 32;

/// How many of a turn's tool chains run at once (ENGINE.md's tool
/// phase: chains are independent and bounded). Named and visible —
/// a config surface arrives with the settings story.
pub const TOOL_CONCURRENCY: usize = 4;

/// How an outer loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The run produced a final response.
    Completed,
    /// The user aborted the run mid-flight; `output` holds whatever
    /// assistant text had arrived.
    Aborted,
    /// The run failed (provider error, or a persistence failure after a
    /// completed response — `RunFailed` events carry the messages).
    Failed,
}

/// One outer loop's outcome and artifacts.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    /// How the run ended.
    pub outcome: RunOutcome,
    /// The final assistant text.
    pub output: String,
    /// Aggregated usage across the whole run.
    pub usage: Usage,
    /// Everything the run emitted, in order.
    pub events: Vec<SessionEvent>,
}

/// What a rewind did: how many user messages left the active chain, and
/// the entry the chain now ends at (the branch point; empty for a branch
/// from the root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindSummary {
    /// How many trailing user messages the rewind dropped from the chain.
    pub dropped: usize,
    /// The entry the active chain now ends at.
    pub to_entry: String,
}

/// What happened while resuming a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResumeReport {
    /// The model selection the session resumed with (from the last
    /// `model_change` record, if any).
    pub resumed_model: Option<ModelSelection>,
}

/// Per-model token and cost totals for a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelStats {
    /// Provider id in effect.
    pub provider: String,
    /// Model id in effect.
    pub model: String,
    /// Thinking level in effect, when one was set.
    pub thinking_level: Option<String>,
    /// Summed usage.
    pub usage: Usage,
    /// Cost in USD, when the config carries rates for the model.
    pub cost: Option<f64>,
}

impl ModelStats {
    /// The `provider/model` display key.
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// Session-level totals.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionStats {
    /// Usage and cost per model that served this session.
    pub per_model: Vec<ModelStats>,
    /// Totals across all models.
    pub total_usage: Usage,
    /// Total cost in USD (models without rates contribute tokens but no
    /// cost).
    pub total_cost: f64,
}

/// Builds a [`Session`], either fresh or resumed from a log.
pub struct SessionBuilder {
    store: SessionStore,
    config: Arc<TabitConfig>,
    selection: ModelSelection,
    preamble: Option<String>,
    tools: Vec<DynamicTool>,
    max_turns: usize,
    model_factory: ModelFactory,
    run_hooks: Option<rig_agent::agent::HookStack>,
}

/// Builds the model behind a selection: `(provider, model, cache_key)`
/// to a type-erased handle. The cache key is the session's stable id —
/// a provider-neutral prompt-cache routing hint; providers with no
/// such knob ignore it. Overridable for callers that construct models
/// themselves (and for tests).
pub type ModelFactory =
    Arc<dyn Fn(&str, &str, &str) -> Result<ModelHandle, SessionError> + Send + Sync>;

/// Validates a selection against a session's config without touching
/// the session — the `model` command's receive-time check (the
/// checkout probe's sibling; see [`Session::model_probe`]).
pub type ModelProbe = Arc<dyn Fn(&ModelSelection) -> Result<(), String> + Send + Sync>;

/// The persist-notice cell (flag 8): the event channel's weak sender
/// plus the stream id every persist notice is stamped with. Attached by
/// the worker at spawn; `None` before that or after it left.
pub type PersistNotices = Arc<
    Mutex<
        Option<(
            tokio::sync::mpsc::WeakUnboundedSender<tabit_protocol::EventFrame>,
            tabit_protocol::StreamId,
        )>,
    >,
>;

impl SessionBuilder {
    /// Start building a session that will use `selection`. The selection is
    /// validated against the config immediately.
    ///
    /// The default model factory mints a **per-builder registry** (its
    /// own provider client caches) — an ergonomic default for
    /// single-session callers. Hosts serving many sessions pass one
    /// shared factory ([`ModelRegistry::factory`]) instead: providers
    /// are user config, process-wide, and so are their connection
    /// pools (owner ruling, PROTOCOL.md v3).
    pub fn new(
        store: SessionStore,
        config: Arc<TabitConfig>,
        auth: Arc<AuthConfig>,
        selection: ModelSelection,
    ) -> Result<Self, SessionError> {
        validate_selection(&selection, &config)?;
        let default_factory: ModelFactory =
            ModelRegistry::new(config.clone(), auth.clone()).factory();
        Ok(Self {
            store,
            config,
            selection,
            preamble: None,
            tools: Vec::new(),
            max_turns: DEFAULT_MAX_TURNS,
            model_factory: default_factory,
            run_hooks: None,
        })
    }

    /// The system preamble hoisted into every request.
    pub fn preamble(mut self, preamble: impl Into<String>) -> Self {
        self.preamble = Some(preamble.into());
        self
    }

    /// Register a runtime-defined tool available to every outer loop.
    pub fn dynamic_tool(mut self, tool: DynamicTool) -> Self {
        self.tools.push(tool);
        self
    }

    /// Model-call budget per outer loop.
    pub fn max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Mount a hook stack on every run (the assembly's seam for
    /// dev-time/extension policy — the permission gate). The stack is
    /// a value: build it once, closures capture their own
    /// session-scoped state, and it clones into each run.
    pub fn hooks(mut self, stack: rig_agent::agent::HookStack) -> Self {
        self.run_hooks = Some(stack);
        self
    }

    /// Supply models yourself instead of through tabit config. The factory
    /// receives `(provider, model, cache_key)`; it is consulted on session
    /// creation, on resume, and on every model switch. Takes the named
    /// [`ModelFactory`] handle (cheaply clonable, shareable across
    /// builders) so callers like `ModelRegistry::factory` pass through
    /// unwrapped.
    pub fn model_factory(mut self, factory: ModelFactory) -> Self {
        self.model_factory = factory;
        self
    }

    /// Create a fresh session. Nothing touches the disk: the file (with
    /// the opening model selection recorded right after the header)
    /// materializes at the first user message, so a session that never
    /// runs leaves nothing behind — not a header-only orphan.
    pub fn create(self, cwd: &str) -> Result<Session, SessionError> {
        let writer = self.store.create(cwd);
        let selection = self.selection.clone();
        let session = Session::assemble(self, writer, false)?;
        // The opening model_change enqueues at once: write-behind — it
        // lands with the session's first drain, and a session that
        // never runs materializes nothing (the writer's no-orphan
        // gate). A register write before then supersedes it (last
        // model_change wins).
        crate::lock::lock(&session.buffer).prequeue(&register_record(&selection));
        Ok(session)
    }

    /// Resume the session stored at `path`: parse it once (the tree, the
    /// head, the selection register, the context, the cumulative stats),
    /// adopt the result as the resident state, and continue with the
    /// builder's selection. Callers resolve that selection through
    /// [`ModelRegistry::default_selection`] (explicit choice > the log's
    /// last model > configured preference); when it differs from the
    /// file's last recorded model the switch is recorded as a
    /// `model_change` side record. The register is file-scoped (the
    /// owner ruling): the last model_change in append order wins,
    /// whichever branch the conversation is on.
    pub fn resume(self, path: &Path) -> Result<(Session, ResumeReport), SessionError> {
        let parsed = self.store.open_path(path)?;
        let report = ResumeReport {
            resumed_model: parsed.register.clone(),
        };
        validate_selection(&self.selection, &self.config)?;
        let id = parsed.header.id.clone();
        let writer = SessionWriter::append_to(&parsed.path, id, parsed.file_len)?;
        let mut session = Session::assemble(self, writer, true)?;
        session.ledger = parsed.stats.clone();
        // The conversation's owner is born from the parsed tree over
        // the session's one buffer (from_tree is the only preloaded
        // entrance; the context is derived, never parsed).
        *crate::lock::write(&session.conversation) =
            crate::context_manager::ContextManager::from_tree(
                parsed.tree.clone(),
                session.buffer.clone(),
            );
        let selection = session.selection();
        let same_model = matches!(
            &report.resumed_model,
            Some(last) if last.provider == selection.provider
                && last.model == selection.model
                && last.thinking_level == selection.thinking_level
        );
        if !same_model {
            // Either a caller-directed switch at resume time, or a log
            // without any model_change yet — either way the session's
            // opening state is durable from here on, through the one
            // register-write site like every other switch.
            session.model_register().write(selection);
        }
        Ok((session, report))
    }
}

/// A queued user message with its born-early entry id (PROTOCOL.md v2):
/// minted at accept, announced by `message_queued` when a run is live,
/// carried into the log when the message drains, restated by
/// `user_message { entry_id }` — and handed back by `messages_discarded`
/// if a clear discards it first. One id, one closed ledger: every
/// `message_queued` id ends in exactly one `user_message` or
/// `messages_discarded`.
pub(crate) struct QueuedMessage {
    pub(crate) id: String,
    pub(crate) message: Message,
}

impl QueuedMessage {
    pub(crate) fn text(&self) -> String {
        user_text(&self.message)
    }
}

/// The run-agnostic message mailbox: the one door every user message
/// enters ([`Session::submit`], or an engine drain mid-run), emptied by
/// [`Session::pump`] — as the next run's initial prompt or, while a run
/// is in flight, as a steer injected at the next turn boundary. The
/// mailbox outlives runs, so a message submitted at any instant is never
/// lost; only the clear sites discard (abort, checkout — each only what
/// was submitted before it, the before/after rule both share).
#[derive(Clone, Default)]
pub(crate) struct Mailbox {
    queue: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<QueuedMessage>>>,
    /// True while a pump may drain at any instant (a run is live): the
    /// gate for submit-time `message_queued` notices. Idle sends never
    /// queue — they drain immediately, so `user_message` is the
    /// acknowledgment.
    live: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// A parked continue intent (the `continue` command): the
    /// worker's beat takes it and starts a run over the existing
    /// conversation with no new message.
    continue_pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The event channel for submit-time notices — weak, so holding it
    /// never keeps the stream alive (the interaction hub's discipline),
    /// and absent for direct [`Session`] consumers (no frontend, no
    /// notices). Attached by the resident worker at spawn.
    notices:
        std::sync::Arc<std::sync::OnceLock<tokio::sync::mpsc::WeakUnboundedSender<EventFrame>>>,
    /// The stream stamp for those notices (the session's id), attached
    /// with the channel.
    notice_stream: std::sync::Arc<std::sync::OnceLock<StreamId>>,
    /// Wakes the resident worker when work arrives. One permit covers any
    /// number of pushes; the queue itself is the source of truth — the
    /// signal exists only so an empty queue can be waited on.
    work: std::sync::Arc<tokio::sync::Notify>,
}

impl Mailbox {
    /// Attach the event channel for submit-time notices (the resident
    /// worker, at spawn), stamped with the session's stream.
    pub(crate) fn attach_notices(
        &self,
        events: tokio::sync::mpsc::UnboundedSender<EventFrame>,
        stream: StreamId,
    ) {
        let _ = self.notices.set(events.downgrade());
        let _ = self.notice_stream.set(stream);
    }

    /// A pump began: submissions from here until [`Self::run_ended`] are
    /// acknowledged with `message_queued`.
    pub(crate) fn run_started(&self) {
        self.live.store(true, std::sync::atomic::Ordering::Release);
    }

    /// The pump ended: submits queue for the next run again.
    pub(crate) fn run_ended(&self) {
        self.live.store(false, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn push(&self, message: Message) {
        let queued = QueuedMessage {
            id: crate::ids::new_entry_id(),
            message,
        };
        // The live snapshot races the pump's own start/end — both orders
        // resolve to a consistent ledger: a notice for a message that
        // drains immediately ends in `user_message` (the GUI drops the
        // pending row on it), and an un-noticed message drains as the
        // next prompt with `user_message` as its only acknowledgment.
        let live = self.live.load(std::sync::atomic::Ordering::Acquire);
        if live {
            self.notice_queued(queued.id.clone(), queued.text());
        }
        lock(&self.queue).push_back(queued);
        self.work.notify_one();
    }

    /// Tell the frontend a live-run submission waits. A dead or absent
    /// channel is a no-op (the frontend is gone, or there never was one).
    fn notice_queued(&self, id: String, text: String) {
        let Some(sender) = self
            .notices
            .get()
            .and_then(tokio::sync::mpsc::WeakUnboundedSender::upgrade)
        else {
            return;
        };
        // The stamp is attached with the channel; an unset stamp with a
        // live channel is unreachable (one attach sets both).
        #[allow(clippy::expect_used)]
        let stream = self
            .notice_stream
            .get()
            .expect("notice channel and stream attach together")
            .clone();
        let _ = sender.send(EventFrame {
            stream: Some(stream),
            event: SessionEvent::MessageQueued { id, text },
        });
    }

    pub(crate) fn is_empty(&self) -> bool {
        lock(&self.queue).is_empty()
    }

    /// Park a continue intent.
    fn continue_run(&self) {
        self.continue_pending
            .store(true, std::sync::atomic::Ordering::Release);
        self.work.notify_one();
    }

    /// Whether a continue intent is parked (the actor's beat check).
    pub(crate) fn has_continue(&self) -> bool {
        self.continue_pending
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// The beat's take of a parked continue intent.
    pub(crate) fn take_continue(&self) -> bool {
        self.continue_pending
            .swap(false, std::sync::atomic::Ordering::Acquire)
    }

    /// Discard everything queued, returning the pairs (the caller emits
    /// `messages_discarded` where its event flow allows).
    pub(crate) fn clear(&self) -> Vec<QueuedMessage> {
        lock(&self.queue).drain(..).collect()
    }

    /// The one clear-and-tell: discard everything queued and emit
    /// `messages_discarded` immediately, through the same notice
    /// channel `message_queued` rides (the abort site and the checkout
    /// handler both — one emitter, one timing; a dead or absent channel
    /// is a no-op, the frontend is gone or there never was one).
    pub(crate) fn clear_noticing(&self) {
        let cleared = self.clear();
        if cleared.is_empty() {
            return;
        }
        let Some(sender) = self
            .notices
            .get()
            .and_then(tokio::sync::mpsc::WeakUnboundedSender::upgrade)
        else {
            return;
        };
        // The stamp is attached with the channel; an unset stamp with a
        // live channel is unreachable (one attach sets both).
        #[allow(clippy::expect_used)]
        let stream = self
            .notice_stream
            .get()
            .expect("notice channel and stream attach together")
            .clone();
        let _ = sender.send(EventFrame {
            stream: Some(stream),
            event: SessionEvent::MessagesDiscarded {
                messages: cleared
                    .into_iter()
                    .map(|queued| tabit_protocol::DiscardedMessage {
                        text: queued.text(),
                        id: queued.id,
                    })
                    .collect(),
            },
        });
    }

    /// The one drain take: everything queued, as id-carrying pairs (the
    /// born-early ids minted at `message_queued`). The engine's loop
    /// drains this — opening batch and mid-run steers alike — and
    /// folds each under its id; the ids ride the `Steer` items back
    /// for the `user_message` events.
    fn take_all(&self) -> Vec<(String, Message)> {
        lock(&self.queue)
            .drain(..)
            .map(|queued| (queued.id, queued.message))
            .collect()
    }

    /// Whether anything is queued (the pump's run-or-idle check).
    fn has_queued(&self) -> bool {
        !lock(&self.queue).is_empty()
    }

    /// The work signal the resident worker waits on. A push before the
    /// wait stores a permit, so no wakeup can be lost.
    pub(crate) fn work_signal(&self) -> &tokio::sync::Notify {
        &self.work
    }
}

/// Submit messages to the session's mailbox from anywhere — including
/// from inside a run's event callback, where the session itself is
/// borrowed. A cheap clone of the mailbox; see [`Session::submit`] for
/// the semantics (steers the run in flight, otherwise queues for the
/// next one).
#[derive(Clone)]
pub struct MailboxHandle {
    mailbox: Mailbox,
}

impl MailboxHandle {
    /// Queue a user message. Always accepted.
    pub fn submit(&self, text: impl Into<String>) {
        self.mailbox.push(Message::user(text.into()));
    }

    /// Park a continue intent: the worker's next beat starts a run
    /// over the existing conversation with no new message.
    pub(crate) fn continue_run(&self) {
        self.mailbox.continue_run();
    }

    /// Whether a continue intent is parked (the actor's beat check).
    pub(crate) fn has_continue(&self) -> bool {
        self.mailbox.has_continue()
    }

    /// Whether anything is queued (the actor's idle check).
    pub(crate) fn is_empty(&self) -> bool {
        self.mailbox.is_empty()
    }

    /// The work signal the resident worker waits on.
    pub(crate) fn work_signal(&self) -> &tokio::sync::Notify {
        self.mailbox.work_signal()
    }
}

/// Cancel the run currently in flight. Cheap to hold; cancelling when no
/// run is in flight does nothing.
#[derive(Clone)]
pub struct AbortHandle {
    token: std::sync::Arc<std::sync::Mutex<CancellationToken>>,
    mailbox: Mailbox,
}

impl AbortHandle {
    /// Abort the current run, if any, and discard what was queued at
    /// abort time — one semantic, one site (PROTOCOL.md flag 6): the
    /// discard notice is immediate, through the mailbox's notice
    /// channel; messages arriving after this queue normally and start
    /// the next run. Aborting while idle just discards the queue.
    pub fn abort(&self) {
        lock(&self.token).cancel();
        self.mailbox.clear_noticing();
    }
}

/// The engine-side view of the mailbox: what an outer loop drains at
/// each convergence — the opening batch and mid-run steers alike (one
/// queue, one drain; ENGINE.md's not-pre-joined rule). The engine folds
/// what it drains under the born-early ids; the session never folds.
struct SessionSteers {
    mailbox: Mailbox,
}

impl rig_agent::SteeringSource for SessionSteers {
    fn drain(&self) -> Vec<(String, Message)> {
        self.mailbox.take_all()
    }

    /// The post-tool stop's queue discard (ENGINE.md, stop semantics):
    /// the stop exits through the engine, the mailbox is the session's —
    /// this is the one clear-and-tell for it, same emitter and timing
    /// as abort's (flag 6).
    fn discard_pending(&self) {
        self.mailbox.clear_noticing();
    }
}

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
    /// The persist-state notice channel (flag 8's degraded/recovered
    /// events), attached by the worker at spawn.
    persist_notices: PersistNotices,
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
    path: PathBuf,
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

impl Session {
    /// Run `prompt` through the mailbox and summarize everything that
    /// ran. Failures are loud in two places: the [`SessionEvent::RunFailed`]
    /// events and `outcome == RunOutcome::Failed` (there is no `Err`
    /// return — frontends and direct callers see the same stream).
    pub async fn prompt(&mut self, prompt: impl Into<Message>) -> RunSummary {
        self.prompt_with(prompt, &mut |_| {}).await
    }

    /// [`Session::prompt`] with a live observer: `on_event` receives each
    /// event as it is produced. The prompt enters the mailbox first, so
    /// anything submitted alongside it joins the same batch.
    pub async fn prompt_with(
        &mut self,
        prompt: impl Into<Message>,
        on_event: &mut (dyn FnMut(SessionEvent) + Send),
    ) -> RunSummary {
        self.mailbox.push(prompt.into());
        self.pump(on_event).await
    }

    /// Submit a user message to the mailbox. While an outer loop is in
    /// flight the message steers it (injected at the next turn boundary);
    /// otherwise [`Session::pump`] runs it as the next prompt. Always
    /// accepted — the mailbox is the one door every message enters, which
    /// is what makes "no message is ever lost" structural.
    pub fn submit(&self, text: impl Into<String>) {
        self.mailbox.push(Message::user(text.into()));
    }

    /// Drain the mailbox to quiescence and summarize. Every drain point
    /// takes everything queued at that instant: at idle entry the whole
    /// batch becomes one run's opening input; mid-run the engine drains
    /// the queue as steers at each turn boundary. A failed run emits
    /// [`SessionEvent::RunFailed`] and the next batch still runs; an
    /// aborted run **stops the pump** — the discard already happened at
    /// the abort site, and anything parked behind the run (a checkout)
    /// must execute at the pause point before a later message runs, so
    /// the pump returns and the caller's beat decides. A message that
    /// arrives after the abort queues normally; the beat's next pump
    /// serves it — same wire behavior, through the pause point. The
    /// drive loop for frontends ([`crate::SessionHost`]'s workers).
    pub async fn pump(&mut self, on_event: &mut (dyn FnMut(SessionEvent) + Send)) -> RunSummary {
        // A pump may drain at any instant from here to its end: submit
        // acknowledgments switch to `message_queued` (PROTOCOL.md v2).
        self.mailbox.run_started();
        let mut total = RunSummary {
            outcome: RunOutcome::Completed,
            output: String::new(),
            usage: Usage::default(),
            events: Vec::new(),
        };
        loop {
            let queued = self.mailbox.has_queued();
            let continuing = self.mailbox.take_continue();
            if !queued && !continuing {
                break;
            }
            // A continue intent with nothing queued starts the run over
            // the conversation as it stands — a no-op on an empty
            // conversation (nothing to continue). Queued messages need
            // no check here: the engine's first CONVERGE drains and
            // folds them before the first turn (one drain point).
            if !queued && crate::lock::read(&self.conversation).messages().is_empty() {
                break;
            }
            let run = self.run_one(on_event).await;
            // The last terminal decides the outcome; usage and events
            // accumulate across runs.
            total.output = run.output;
            add_usage(&mut total.usage, &run.usage);
            total.events.extend(run.events);
            total.outcome = run.outcome;
            if matches!(run.outcome, RunOutcome::Aborted) {
                break;
            }
        }
        self.mailbox.run_ended();
        total
    }

    /// One outer loop for a drained batch: stage the input, drive the
    /// engine to completion, conclude the run. Exactly one terminal event
    /// comes out (`run_finished`/`run_aborted`/`run_failed`), plus a
    /// trailing `run_failed` when durability checks fail after a terminal.
    ///
    /// The phases are named methods so each concern changes in one place:
    /// input staging (the v2 prompt barrier lands in [`Self::stage_input`]),
    /// the agent-cache check ([`Self::ensure_agent`] — the point-of-use
    /// freshness rule; a selection that cannot construct fails the run
    /// here), request assembly ([`Self::open_run`] — where the UUIDv7
    /// turn-id mint is injected), the item fold ([`Self::drive`] — where
    /// announced turn ids stamp events; v2 replay reuses the same ids
    /// verbatim from the log), and the terminal/durability epilogue
    /// ([`Self::conclude`] — where the write-behind log lands;
    /// `messages_discarded` does not land here at all, the abort site
    /// emits it immediately through the mailbox's notice channel).
    async fn run_one(&mut self, on_event: &mut (dyn FnMut(SessionEvent) + Send)) -> RunSummary {
        // Run-scoped machinery: a fresh abort token for this loop; steers
        // arrive through the run-agnostic mailbox.
        let run_token = {
            let mut slot = lock(&self.abort);
            *slot = CancellationToken::new();
            slot.clone()
        };
        let mut sink = EventSink::new(on_event);
        // The degraded-buffer guard (flag 8's second amendment): retry
        // whatever lines a previous failure stuck in the outbox; a
        // still-refusing buffer blocks this start (the first failed
        // drain ran in memory; no run proceeds twice on it).
        let guard_outcome = crate::lock::lock(&self.buffer).enqueue(&[]);
        if let Err(error) = &guard_outcome {
            self.drain_persist_transitions();
            self.fail_before_engine(
                format!("the session log is undrained and still refuses to flush: {error}"),
                &mut sink,
            );
            return RunSummary {
                outcome: RunOutcome::Failed,
                output: String::new(),
                usage: Usage::default(),
                events: sink.events,
            };
        }
        self.drain_persist_transitions();
        // The agent-cache check at run open — the single point of use.
        // A selection that validates against config but cannot be
        // constructed in this environment (client build trouble, the
        // only residual class: config is immutable per process) fails
        // here, before any turn: the frontend sees the queued
        // `user_message`s (the failed open's drain acknowledges them)
        // then `run_failed` — the same shape a provider stream error
        // takes.
        if let Err(error) = self.ensure_agent() {
            self.fail_before_engine(error.to_string(), &mut sink);
            return RunSummary {
                outcome: RunOutcome::Failed,
                output: String::new(),
                usage: Usage::default(),
                events: sink.events,
            };
        }
        let stream = self.open_run(&run_token).await;
        let driven = self.drive(stream, &run_token, &mut sink).await;
        let (outcome, output, usage) = self.conclude(driven, &mut sink);
        RunSummary {
            outcome,
            output,
            usage,
            events: sink.events,
        }
    }

    /// A run that cannot start its engine: the queued batch is still
    /// acknowledged and recorded — the engine's opening drain (the
    /// first CONVERGE in `drive.rs` is the twin), run by the session
    /// because the engine cannot — then the failure. Without this a
    /// failed open would leave its batch queued and the pump would
    /// spin on it forever; with it the failure takes the same shape a
    /// provider error does: `user_message` events, then `run_failed`.
    /// The only other pre-drain failure (a zero turn budget) is
    /// rejected when the session is built.
    fn fail_before_engine(&mut self, message: String, sink: &mut EventSink<'_>) {
        for (id, queued) in self.mailbox.take_all() {
            // Commit first, then announce — the engine's CONVERGE idiom;
            // the helper is synchronous, so no suspension can interleave,
            // but one ordering lives in the codebase, not two.
            let text = user_text(&queued);
            crate::lock::write(&self.conversation).fold_with_id(queued, id.clone());
            sink.emit(SessionEvent::UserMessage { text, entry_id: id });
        }
        if let Some(hub) = &self.interaction {
            hub.clear_pending();
        }
        sink.emit(SessionEvent::RunFailed { message });
    }

    /// Assemble the engine request for one run: the abort token and
    /// interaction capability in the tool context, the permission gate,
    /// and steering over the run-agnostic mailbox. The conversation is
    /// the shared cell — the loop's folds ARE the durable commits; the
    /// session never folds.
    async fn open_run(&self, run_token: &CancellationToken) -> rig_agent::agent::StreamingResult {
        let mut tool_context = rig_agent::tool::ToolContext::new();
        tool_context.insert(run_token.clone());
        if let Some(hub) = &self.interaction {
            tool_context.insert(hub.capability());
        }
        let mut request = self
            .agent
            .stream_chat(Vec::<Message>::new())
            .max_turns(self.max_turns)
            .tool_concurrency(TOOL_CONCURRENCY)
            // The cell IS the conversation — the history argument above
            // is vestigial when one is supplied (build_run ignores it).
            .conversation_cell(self.conversation.clone());
        if let Some(stack) = &self.run_hooks {
            request = request.add_hook(stack.clone());
        }
        request
            .steering(Arc::new(SessionSteers {
                mailbox: self.mailbox.clone(),
            }))
            .tool_context(tool_context)
            // Announced turn ids are entry ids (ENGINE.md behavior delta
            // 10): the engine mints from tabit's UUIDv7 source, so the id
            // a live `turn_started` carries is literally the id the
            // committed entry keeps in the log.
            .turn_id_source(Arc::new(crate::ids::new_entry_id) as rig_agent::TurnIdSource)
            .await
    }

    /// Drive the engine stream to its end, folding every item into events
    /// and the durable log. Aborting the run token preempts the stream;
    /// returning drops it, which cancels in-flight tool futures (their drop
    /// guards kill process trees) — every closed roundtrip is already
    /// committed, and the interrupted one never lands (roundtrips are
    /// atomic): there is nothing dangling to repair.
    async fn drive(
        &mut self,
        mut stream: rig_agent::agent::StreamingResult,
        run_token: &CancellationToken,
        sink: &mut EventSink<'_>,
    ) -> DriveOutcome {
        let mut driven = DriveOutcome {
            output: String::new(),
            usage: Usage::default(),
            aborted: false,
            failure: None,
        };
        // Tool names by correlation id: the result items carry the call's
        // internal id but not its name.
        let mut tool_names: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        // The announced id of the turn in flight: seeded by each
        // `TurnStarted`, stamps every turn-scoped event, and outlives the
        // turn's commit (its tool results arrive after `TurnCommitted`).
        let mut current_turn: Option<String> = None;
        loop {
            let item = tokio::select! {
                biased;
                _ = run_token.cancelled() => {
                    driven.aborted = true;
                    break;
                }
                item = stream.next() => match item {
                    Some(item) => item,
                    None => break,
                },
            };
            // Turn-scoped items require an announced turn; the engine
            // guarantees `TurnStarted` is the first item of every attempt
            // (ENGINE.md behavior delta 10), so arriving here without one
            // is an engine-contract violation — internal, fail loud.
            // Sanctioned crash: see the error doctrine in AGENTS.md.
            #[allow(clippy::expect_used)]
            let announce = |current: &Option<String>| {
                current
                    .clone()
                    .expect("turn-scoped stream item before any TurnStarted")
            };
            match item {
                Ok(MultiTurnStreamItem::TurnStarted { id }) => {
                    current_turn = Some(id.clone());
                    sink.emit(SessionEvent::TurnStarted { id });
                }
                Ok(MultiTurnStreamItem::TurnCommitted { id, .. }) => {
                    // The engine's own fold is the durable commit; this
                    // is the announcement only (emission-only drive —
                    // the session never folds).
                    sink.emit(SessionEvent::TurnCommitted { id });
                }
                Ok(MultiTurnStreamItem::ModelTurnRetried { .. }) => {
                    let turn_id = announce(&current_turn);
                    // A defect-discarded attempt: nothing ever folded
                    // (the turn was an engine-local), so nothing to
                    // discard — the frontend drops its provisional
                    // output.
                    sink.emit(SessionEvent::TurnRetried { turn_id });
                }
                Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                    tool_result,
                    internal_call_id,
                    entry_id,
                })) => {
                    let turn_id = announce(&current_turn);
                    self.note_tool_result(
                        tool_result,
                        internal_call_id,
                        entry_id,
                        turn_id,
                        &mut tool_names,
                        sink,
                    );
                }
                Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                    driven.output = response.output;
                    driven.usage = response.usage;
                    sink.emit(SessionEvent::RunFinished {
                        output: driven.output.clone(),
                        usage: Self::wire_usage(&driven.usage),
                        durable: self.buffer_is_clean(),
                    });
                }
                Ok(MultiTurnStreamItem::Steer { id, text }) => {
                    self.note_steer(id, text, sink);
                }
                Ok(MultiTurnStreamItem::CompletionCall(call)) => {
                    let turn_id = announce(&current_turn);
                    // The live ledger grows with the deferred zeros:
                    // the per-model slot exists (usage facts ride the
                    // records; the totals resume at the usage
                    // discussion, which updates these the same way).
                    {
                        let selection = self.selection();
                        self.ledger.add(
                            &selection.provider,
                            &selection.model,
                            selection.thinking_level.as_deref(),
                            call.usage,
                        );
                    }
                    sink.emit(SessionEvent::CompletionCall {
                        turn_id: turn_id.clone(),
                        input_tokens: call.usage.input_tokens,
                        output_tokens: call.usage.output_tokens,
                    });
                    // A truncation-class finish reason is a warning, not a
                    // failure (ENGINE.md behavior delta 9): the flow
                    // continues untouched — steers drain into the next
                    // turn, the run may end normally.
                    if call.finish_reason == Some(rig_core::completion::FinishReason::Length) {
                        sink.emit(SessionEvent::TurnTruncated { turn_id });
                    }
                }
                Ok(item) => {
                    let turn_id = announce(&current_turn);
                    if let Some(event) = stream_item_event(item, turn_id, &mut tool_names) {
                        sink.emit(event);
                    }
                }
                Err(StreamingError::Completion(error)) => {
                    driven.failure = Some(SessionError::Prompt(error.into()));
                    break;
                }
                Err(StreamingError::Prompt(error)) => {
                    driven.failure = Some(SessionError::Prompt(*error));
                    break;
                }
            }
        }
        driven
    }

    /// One executed tool call's result: staged into the open roundtrip
    /// (whose entry id rides the event — it commits, durably, when the
    /// roundtrip closes), and the event naming the tool through the
    /// correlation map its call populated. `content` is exactly the text
    /// the model saw; `status` is the execution's structured outcome.
    fn note_tool_result(
        &self,
        tool_result: rig_core::message::ToolResult,
        internal_call_id: String,
        entry_id: String,
        turn_id: String,
        tool_names: &mut std::collections::BTreeMap<String, String>,
        sink: &mut EventSink<'_>,
    ) {
        let content = result_text(&tool_result);
        let status = wire_status(&tool_result.status);
        // The entry id rides the result item (born-early at settlement;
        // the fold_all at BatchResults reuses it, so live and replay
        // name the same node).
        sink.emit(SessionEvent::ToolResult {
            turn_id,
            entry_id,
            name: tool_names
                .get(&internal_call_id)
                .cloned()
                .unwrap_or_default(),
            internal_call_id,
            content,
            status,
        });
    }

    /// A steer drained into history mid-run: one user node under the
    /// message's born-early id (the id its `message_queued` announced,
    /// parked by the drain in FIFO order), 1:1 with what the model saw.
    /// Announce one drained message (opening batch and mid-run steers
    /// alike): the engine's fold is the durable commit under the
    /// born-early id the `Steer` item carries; the session only
    /// emits — the 1:1 acknowledgment.
    fn note_steer(&self, entry_id: String, text: String, sink: &mut EventSink<'_>) {
        sink.emit(SessionEvent::UserMessage { text, entry_id });
    }

    /// The run's epilogue: exactly one terminal (the fold already emitted
    /// `run_finished`; here `run_aborted` or `run_failed`), then the
    /// context re-derivation and durability checks that can follow a
    /// terminal with a trailing `run_failed`, then the retraction of
    /// any unanswered interaction — no asker survives the run (a racing
    /// response is then a total no-op).
    fn conclude(
        &mut self,
        driven: DriveOutcome,
        sink: &mut EventSink<'_>,
    ) -> (RunOutcome, String, Usage) {
        let DriveOutcome {
            output,
            usage,
            aborted,
            failure,
        } = driven;
        self.drain_persist_transitions();
        // Nothing half-open carries across runs by construction: a
        // roundtrip folds only at BatchResults, so an aborted or
        // failed run simply never folded its in-flight turn (the
        // handler's staged local died with the drive loop).
        let mut outcome = RunOutcome::Completed;
        if aborted {
            if let Err(error) =
                crate::lock::lock(&self.buffer).enqueue(&[FileRecord::Side(SideRecord {
                    timestamp: crate::ids::now_rfc3339(),
                    kind: SideKind::Aborted,
                })])
            {
                tracing::warn!(%error, "aborted record failed to flush; queued for retry");
            }
            sink.emit(SessionEvent::RunAborted {
                output: output.clone(),
            });
            // The abort SITE (the command link, or the host's death
            // watcher, or a checkout) already discarded the
            // at-abort-time queue and said so immediately (flag 6);
            // messages arriving after the abort queue normally and
            // start the next run.
            outcome = RunOutcome::Aborted;
        } else if let Some(failure) = failure {
            sink.emit(SessionEvent::RunFailed {
                message: failure.to_string(),
            });
            outcome = RunOutcome::Failed;
        }
        if let Some(hub) = &self.interaction {
            hub.clear_pending();
        }
        (outcome, output, usage)
    }

    /// The session's mailbox as a clonable handle: submits work while a
    /// run borrows the session (frontends' actor holds one).
    pub(crate) fn mailbox_handle(&self) -> MailboxHandle {
        MailboxHandle {
            mailbox: self.mailbox.clone(),
        }
    }

    /// The read-only conversation probe (checkout validation at
    /// receive time — see [`SharedConversation`]).
    pub(crate) fn entry_id_probe(&self) -> SharedConversation {
        self.shared_conversation.clone()
    }

    /// The receive-time model validator — the checkout probe's sibling
    /// for the `model` command: validates a selection against this
    /// session's config without touching the session, so the worker
    /// can reject an unusable ref at the command (a picker's
    /// immediate feedback, even mid-run). The write itself is
    /// [`Session::set_model`], at the beat.
    pub(crate) fn model_probe(&self) -> ModelProbe {
        let config = self.config.clone();
        Arc::new(move |selection| {
            validate_selection(selection, &config).map_err(|error| error.to_string())
        })
    }

    /// A handle for aborting the current outer loop. See [`AbortHandle`].
    pub fn abort_handle(&self) -> AbortHandle {
        AbortHandle {
            token: self.abort.clone(),
            mailbox: self.mailbox.clone(),
        }
    }

    /// Attach the interaction hub. Called once by the session worker
    /// ([`crate::endpoint::spawn_worker`]) when it takes
    /// ownership — the hub is built over the worker's event channel,
    /// which exists only there.
    pub fn attach_interaction(&mut self, hub: InteractionHub) {
        self.interaction = Some(hub);
    }

    /// Point the mailbox's submit-time notices at the worker's event
    /// channel (`message_queued` for live-run submissions), stamped with
    /// the session's stream. Called by the session worker at spawn,
    /// alongside [`Self::attach_interaction`].
    pub fn attach_mailbox_notices(
        &self,
        events: tokio::sync::mpsc::UnboundedSender<EventFrame>,
        stream: StreamId,
    ) {
        self.mailbox.attach_notices(events, stream);
    }

    /// Rewind the active chain by `turns` user messages: the leaf moves to
    /// the parent of the `turns`-th-most-recent `user_message` entry (a
    /// prompt or a steer — both are valid "I should have said something
    /// else here" points), and the next prompt branches from there. The
    /// dropped entries stay in the file as a sibling branch.
    ///
    /// Idle only — `&mut self` cannot alias a run in flight. The rewind is
    /// durable on its own: a `rewound` marker lands in the log even if no
    /// prompt follows.
    pub fn rewind(&mut self, turns: usize) -> Result<RewindSummary, SessionError> {
        let branch = crate::lock::read(&self.conversation).active_branch();
        let boundaries = tabit_log::user_message_boundaries(&branch);
        if turns == 0 {
            return Err(SessionError::Config {
                message: "rewind needs at least 1 user message to drop".to_string(),
            });
        }
        let Some(target) = turns
            .checked_sub(1)
            .and_then(|offset| boundaries.len().checked_sub(1 + offset))
            .and_then(|index| boundaries.get(index))
        else {
            return Err(SessionError::Config {
                message: format!(
                    "cannot rewind {turns} user message(s): the active branch holds {}",
                    boundaries.len()
                ),
            });
        };
        // The branch point is the boundary's parent; the conversation
        // continues from there.
        self.apply_checkout(target.parent_id.as_deref())
    }

    /// Rewind to an exact entry: the active branch will end at that
    /// entry. Any **roundtrip-closed** node in the tree is a valid
    /// target, on or off the active branch (this is also how a branch
    /// switch happens); a target inside an open tool roundtrip panics
    /// (the flag-23 ruling: unsupported, revisited later). The library
    /// primitive for tree-picking frontends — [`Session::rewind`] is the
    /// user-facing form.
    pub fn rewind_to_entry(&mut self, entry_id: &str) -> Result<RewindSummary, SessionError> {
        self.apply_checkout(Some(entry_id))
    }

    /// Shared checkout mechanics: move the recorder's head (closed-path
    /// rule enforced at the door) and re-project the context from the new
    /// branch. The selection is a session preference (owner ruling
    /// 2026-08): a checkout moves the head, never the register — the
    /// model that answers next is unchanged by this move. The
    /// `checkout` side record rides the outbox like any record (flag 8
    /// — degraded notices announce a failed flush; the ruling keeps it
    /// non-barrier).
    fn apply_checkout(&mut self, to: Option<&str>) -> Result<RewindSummary, SessionError> {
        let before = {
            let branch = crate::lock::read(&self.conversation).active_branch();
            tabit_log::user_message_boundaries(&branch).len()
        };
        crate::lock::write(&self.conversation)
            .checkout(to)
            .map_err(
                |crate::context_manager::CheckoutError(target)| SessionError::Config {
                    message: format!("checkout target `{target}` is not in this session"),
                },
            )?;
        if let Err(error) =
            crate::lock::lock(&self.buffer).enqueue(&[FileRecord::Side(SideRecord {
                timestamp: crate::ids::now_rfc3339(),
                kind: SideKind::Checkout {
                    to: to.map(str::to_string),
                },
            })])
        {
            tracing::warn!(%error, "checkout record failed to flush; queued for retry");
        }
        let after = {
            let branch = crate::lock::read(&self.conversation).active_branch();
            tabit_log::user_message_boundaries(&branch).len()
        };
        Ok(RewindSummary {
            dropped: before.saturating_sub(after),
            to_entry: to.unwrap_or_default().to_string(),
        })
    }

    /// Switch the provider/model/thinking level from the next outer loop
    /// on: validate, then one register write (the `model_change` entry
    /// and the live cell, atomically — see [`ModelRegister::write`]).
    /// No agent is built here — the next run open derives it, and a
    /// selection that validates against config but fails to construct
    /// surfaces as that run's `run_failed`.
    pub fn set_model(&mut self, selection: ModelSelection) -> Result<(), SessionError> {
        validate_selection(&selection, &self.config)?;
        self.model_register().write(selection);
        Ok(())
    }

    /// Change the thinking level without changing provider/model. `None`
    /// clears it.
    pub fn set_thinking_level(&mut self, level: Option<&str>) -> Result<(), SessionError> {
        let current = self.selection();
        let selection = ModelSelection {
            provider: current.provider,
            model: current.model,
            thinking_level: level.map(str::to_string),
        };
        self.set_model(selection)
    }

    /// The active model selection (an owned clone — three strings; the
    /// cell is shared with the endpoint's receive-time writes).
    pub fn selection(&self) -> ModelSelection {
        lock(&self.selection).clone()
    }

    /// The shared register handle — the `model` command's write path at
    /// receive (validate with [`Self::model_probe`] first; the write
    /// itself cannot fail).
    pub(crate) fn model_register(&self) -> ModelRegister {
        ModelRegister {
            selection: self.selection.clone(),
            buffer: self.buffer.clone(),
        }
    }

    /// Every commit reached the disk — the `durable` verdict. The
    /// manager's folds enqueue through the same buffer; a clean buffer
    /// means every record landed.
    fn buffer_is_clean(&self) -> bool {
        crate::lock::lock(&self.buffer).pending() == 0
    }

    /// Drain any pending persist transitions into the notice events:
    /// a failed enqueue set the writer's degraded flag (true), a
    /// successful retry cleared it (false). Called at the emission
    /// points — the entry guard, conclude — so the notices ride the
    /// same channel the run's events do.
    fn drain_persist_transitions(&self) {
        let (transition, pending) = {
            let mut buffer = crate::lock::lock(&self.buffer);
            (buffer.take_degraded_transition(), buffer.pending() as u64)
        };
        let Some(degraded) = transition else {
            return;
        };
        let notices = lock(&self.persist_notices).clone();
        if let Some((sender, stream)) = notices
            && let Some(sender) = sender.upgrade()
        {
            let event = if degraded {
                tabit_protocol::SessionEvent::error_persist_degraded(
                    pending,
                    "the session log refuses to flush; records stay queued and retry",
                )
            } else {
                tabit_protocol::SessionEvent::error_persist_recovered()
            };
            let _ = sender.send(tabit_protocol::EventFrame {
                stream: Some(stream),
                event,
            });
        }
    }

    /// The clean-exit flush attempt: one more enqueue retry of
    /// anything queued (the writer's Drop also flushes; this rides the
    /// endpoint's explicit close path).
    pub(crate) fn flush_log(&self) {
        if let Err(error) = crate::lock::lock(&self.buffer).enqueue(&[]) {
            tracing::warn!(%error, "the clean-exit flush failed; lines stay queued");
        }
    }

    /// Attach the persist-state notice channel (flag 8's degraded /
    /// recovered events): the entry guard emits them through here —
    /// the mailbox-notices pattern: weak, so the channel ends with the
    /// frontend.
    pub(crate) fn attach_persist_notices(
        &self,
        sender: tokio::sync::mpsc::WeakUnboundedSender<tabit_protocol::EventFrame>,
        stream: tabit_protocol::StreamId,
    ) {
        *crate::lock::lock(&self.persist_notices) = Some((sender, stream));
    }

    /// The session id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The session file.
    pub fn path(&self) -> &Path {
        &self.path
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

    /// Convert the engine's usage record to the protocol's wire shape
    /// (the engine's richer fields — reasoning, tool-use, per-TTL splits —
    /// stay engine-internal).
    fn wire_usage(usage: &rig_core::completion::Usage) -> tabit_protocol::Usage {
        tabit_protocol::Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
        }
    }

    /// The agent-cache freshness check — the point-of-use half of the
    /// selection-is-truth rule ([`Session::set_model`] is the write
    /// half). Any future writer that swaps `selection` (config reload,
    /// say) cannot leave a stale agent serving requests, because the
    /// one reader derives rather than trusts.
    fn ensure_agent(&mut self) -> Result<(), SessionError> {
        let selection = self.selection();
        if self.agent_built_for == selection {
            return Ok(());
        }
        self.agent = Arc::new(build_agent(
            &self.model_factory,
            &self.config,
            &selection,
            &self.id,
            self.preamble.as_deref(),
            &self.tools,
        )?);
        self.agent_built_for = selection;
        Ok(())
    }

    fn assemble(
        builder: SessionBuilder,
        writer: SessionWriter,
        resumed: bool,
    ) -> Result<Self, SessionError> {
        if builder.max_turns == 0 {
            // The engine's entry contract (ENGINE.md): every outer loop
            // runs at least one turn. Rejected here — at the builder —
            // because a zero budget would otherwise fail every run
            // before its engine could drain, and the session cannot be
            // built to run at all.
            return Err(SessionError::Config {
                message: "max_turns must be at least 1 — every outer loop runs at least one turn"
                    .to_string(),
            });
        }
        let path = writer.path().to_path_buf();
        let id = writer.session_id().to_string();
        let buffer: crate::writer::SharedBuffer =
            std::sync::Arc::new(std::sync::Mutex::new(writer));
        let conversation_cell: Arc<std::sync::RwLock<ContextManager>> = Arc::new(
            std::sync::RwLock::new(ContextManager::empty(buffer.clone())),
        );
        let shared_conversation = SharedConversation {
            conversation: conversation_cell.clone(),
        };
        // The opening agent is derived from the resolved selection before
        // the struct exists (the placeholder this replaces existed only
        // to satisfy the field initializer).
        let agent = Arc::new(build_agent(
            &builder.model_factory,
            &builder.config,
            &builder.selection,
            &id,
            builder.preamble.as_deref(),
            &builder.tools,
        )?);
        let session = Self {
            config: builder.config,
            selection: Arc::new(Mutex::new(builder.selection.clone())),
            preamble: builder.preamble,
            tools: builder.tools,
            max_turns: builder.max_turns,
            model_factory: builder.model_factory,
            run_hooks: builder.run_hooks,
            agent,
            agent_built_for: builder.selection,
            conversation: conversation_cell,
            buffer,
            shared_conversation,
            persist_notices: Arc::new(Mutex::new(None)),
            ledger: crate::stats::UsageLedger::default(),
            abort: std::sync::Arc::new(std::sync::Mutex::new(CancellationToken::new())),
            mailbox: Mailbox::default(),
            path,
            id,
            resumed,
            interaction: None,
        };
        Ok(session)
    }
}

/// The shared model-selection register: the live cell plus the
/// recorder's append. [`ModelRegister::write`] records the
/// `model_change` entry and swaps the cell **in one operation, from
/// any thread** (owner ruling 2026-08: a state write happens at
/// receive; the worker derives, it does not gate) — the register's
/// one durable-write site, shared by the endpoint's `model` command,
/// `Session::set_model`, and resume's reconciliation. The append is
/// the write-behind commit: a queue enqueue with a flush attempt per
/// write (a disk that refuses degrades through the persist-state
/// machine — the degraded notice, retried on every later write — and
/// the change is durable no later than the next turn's prompt
/// barrier).
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

/// The `model` command's write path: the selection cell plus the shared
/// write buffer (the register's record enqueues like every side
/// record — write-behind, last model_change wins).
#[derive(Clone)]
pub(crate) struct ModelRegister {
    selection: Arc<Mutex<ModelSelection>>,
    buffer: crate::writer::SharedBuffer,
}

impl ModelRegister {
    /// Record + swap, atomic under the cell lock. Unconditional — a
    /// dedup guard would be machinery without a failure it prevents
    /// (repeat values are harmless under last-write-wins).
    pub(crate) fn write(&self, selection: ModelSelection) {
        let mut cell = lock(&self.selection);
        if let Err(error) = crate::lock::lock(&self.buffer).enqueue(&[register_record(&selection)])
        {
            // Write-behind: the lines stay queued and retry at every
            // later enqueue — a refusal is degradation, not loss.
            tracing::warn!(%error, "model_change record failed to flush; queued for retry");
        }
        *cell = selection;
    }
}

/// The register's side record (the one constructor, from the
/// selection).
fn register_record(selection: &ModelSelection) -> FileRecord {
    FileRecord::Side(SideRecord {
        timestamp: crate::ids::now_rfc3339(),
        kind: SideKind::ModelChange {
            provider: selection.provider.clone(),
            model: selection.model.clone(),
            thinking_level: selection.thinking_level.clone(),
        },
    })
}

/// The run-loop's event fan-out: every event reaches the live consumer and
/// the run's summary in one step, so no emission site can send without
/// recording (or the reverse).
struct EventSink<'a> {
    on_event: &'a mut (dyn FnMut(SessionEvent) + Send),
    events: Vec<SessionEvent>,
}

impl<'a> EventSink<'a> {
    fn new(on_event: &'a mut (dyn FnMut(SessionEvent) + Send)) -> Self {
        Self {
            on_event,
            events: Vec::new(),
        }
    }

    fn emit(&mut self, event: SessionEvent) {
        (self.on_event)(event.clone());
        self.events.push(event);
    }
}

/// What the drive loop learned before the stream ended: the run's final
/// output and usage, and how the stream stopped (abort preemption, or the
/// provider failure that ended it).
struct DriveOutcome {
    output: String,
    usage: Usage,
    aborted: bool,
    failure: Option<SessionError>,
}

/// Map an engine item to a session event; `None` means "not surfaced in
/// v1". Turn-scoped events carry the announced id of the turn in flight.
fn stream_item_event(
    item: MultiTurnStreamItem,
    turn_id: String,
    tool_names: &mut std::collections::BTreeMap<String, String>,
) -> Option<SessionEvent> {
    use rig_agent::streaming::StreamedAssistantContent as A;
    match item {
        MultiTurnStreamItem::StreamAssistantItem(A::Text(text)) => Some(SessionEvent::TextDelta {
            turn_id,
            text: text.text,
        }),
        MultiTurnStreamItem::StreamAssistantItem(A::ReasoningDelta { id, reasoning }) => {
            Some(SessionEvent::ReasoningDelta {
                turn_id,
                id,
                reasoning,
            })
        }
        MultiTurnStreamItem::StreamAssistantItem(A::ToolCall {
            tool_call,
            internal_call_id,
        }) => {
            tool_names.insert(internal_call_id.clone(), tool_call.function.name.clone());
            Some(SessionEvent::ToolCall {
                turn_id,
                name: tool_call.function.name,
                call_id: tool_call.id,
                arguments: Some(tool_call.function.arguments.to_string()),
                internal_call_id,
            })
        }
        MultiTurnStreamItem::StreamAssistantItem(A::Unknown(item)) => {
            Some(SessionEvent::NativeItem { turn_id, item })
        }
        MultiTurnStreamItem::StreamAssistantItem(_) => None,
        MultiTurnStreamItem::ToolExecutionCommitted { .. } => None,
        MultiTurnStreamItem::StreamUserItem(_) => None,
        // `TurnStarted`/`TurnCommitted` are handled by explicit arms in
        // `drive` — they set and read the current-turn state.
        MultiTurnStreamItem::TurnStarted { .. } | MultiTurnStreamItem::TurnCommitted { .. } => None,
        // `CompletionCall` and `ModelTurnRetried` are handled by
        // explicit arms in `drive` — the usage tracking feeds the
        // discard record (flag 22).
        MultiTurnStreamItem::CompletionCall(_) | MultiTurnStreamItem::ModelTurnRetried { .. } => {
            None
        }
        MultiTurnStreamItem::FinalResponse(_) => None, // handled by the caller
        _ => None,
    }
}

/// The text of a user message (joined text parts).
pub(crate) fn user_text(message: &Message) -> String {
    let Message::User { content } = message else {
        return String::new();
    };
    content
        .iter()
        .filter_map(|part| match part {
            rig_core::message::UserContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

/// The text of a tool result — exactly what the model saw of it (text
/// parts joined; images have no textual form).
pub(crate) fn result_text(result: &rig_core::message::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Translate the rig-level structured status into the protocol's wire
/// shape. Live results always carry one — the engine stamps every
/// execution outcome (`with_execution_status`) and the session's own
/// synthesized results set one — so `None` is a producer breaking the
/// contract, never a successful call: fail loud rather than bless it.
/// `exit_code` means exit code: the structured code passes through
/// exactly when numeric (a shell tool's exit status); other codes are
/// not exit codes and their detail already lives in the content.
/// Shared by the live fold and the replay projection — one
/// translation, one truth.
#[allow(clippy::panic)] // sanctioned crash: a status-less result is a broken producer invariant (AGENTS.md doctrine)
pub(crate) fn wire_status(
    status: &Option<rig_core::completion::ToolResultStatus>,
) -> tabit_protocol::ToolResultStatus {
    match status {
        Some(rig_core::completion::ToolResultStatus::Success) => {
            tabit_protocol::ToolResultStatus::Success
        }
        Some(rig_core::completion::ToolResultStatus::Failed { code }) => {
            tabit_protocol::ToolResultStatus::Failed {
                exit_code: code.as_deref().and_then(|code| code.parse().ok()),
            }
        }
        None => panic!("wire_status: a tool result reached the wire without a status"),
    }
}

fn cost_of(usage: &Usage, cost: &tabit_config::Cost) -> f64 {
    (usage.input_tokens as f64 / 1_000_000.0) * cost.input
        + (usage.output_tokens as f64 / 1_000_000.0) * cost.output
        + (usage.cached_input_tokens as f64 / 1_000_000.0) * cost.cache_read
        + (usage.cache_creation_input_tokens as f64 / 1_000_000.0) * cost.cache_write
}

/// Build the agent a selection resolves to. Everything except the
/// selection is fixed at assembly (factory, config, preamble, tools),
/// so this is a pure function of its arguments — the derivation the
/// cache check in [`Session::ensure_agent`] and the one-shot build in
/// [`Session::assemble`] share.
fn build_agent(
    model_factory: &ModelFactory,
    config: &TabitConfig,
    selection: &ModelSelection,
    cache_key: &str,
    preamble: Option<&str>,
    tools: &[DynamicTool],
) -> Result<Agent, SessionError> {
    let handle = (model_factory)(&selection.provider, &selection.model, cache_key)?;
    let params = crate::registry::request_params(config, selection);
    // `dynamic_tools` (even with an empty vec) moves the builder to
    // its tool-configured state, keeping one concrete type through
    // the preamble/build chain.
    let mut builder = AgentBuilder::new(handle).dynamic_tools(tools.to_vec());
    if let Some(preamble) = preamble {
        builder = builder.preamble(preamble);
    }
    // Configured request parameters are pure forwarding (reviewed
    // 2026-08): the model's knobs, nothing interpreted.
    if let Some(max_tokens) = params.max_tokens {
        builder = builder.max_tokens(max_tokens);
    }
    if let Some(temperature) = params.temperature {
        builder = builder.temperature(temperature);
    }
    // `top_p`/`top_k` have no dedicated field on the completion
    // request — they ride the same flattened `additional_params` map
    // as `extra_body`, which is the compat escape hatch and therefore
    // gets the last word over the named knobs.
    let mut additional = serde_json::Map::new();
    if let Some(top_p) = params.top_p {
        additional.insert("top_p".to_string(), serde_json::json!(top_p));
    }
    if let Some(top_k) = params.top_k {
        additional.insert("top_k".to_string(), serde_json::json!(top_k));
    }
    if let Some(extra) = params.extra_body {
        additional.extend(extra);
    }
    if !additional.is_empty() {
        builder = builder.additional_params(serde_json::Value::Object(additional));
    }
    Ok(builder.build())
}
