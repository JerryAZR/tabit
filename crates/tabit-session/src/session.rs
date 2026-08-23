//! The session facade: owns the entry log, the model selection, and the
//! outer loop's policy, and consumes the rig-agent item stream as its
//! driver.
//!
//! User messages enter through one door — the run-agnostic mailbox
//! ([`Session::submit`]) — and are drained by [`Session::pump`]: as the
//! next run's initial prompt, or — while a run is in flight — as a steer
//! injected at the next turn boundary. Because the mailbox outlives runs,
//! a message submitted at any instant is never lost; only abort discards
//! queued messages. Each pump iteration is one outer loop: the user
//! message is recorded, the rig-agent engine runs the turns (with a
//! recorder hook persisting every completed assistant turn and tool
//! result as it happens), and the item stream is folded into the
//! serializable event list a frontend consumes. After every run — success
//! or failure — the in-memory context is re-derived from the log, which
//! stays the single source of truth. Permissions and extensions later
//! plug into this same seam.

use crate::entry::{EntryKind, SessionEntry};
use crate::error::SessionError;
use crate::interaction::InteractionHub;
use crate::lock::lock;
use crate::model::validate_selection;
use crate::permission::PermissionHook;
use crate::projection;
use crate::recorder::{RecorderHook, SessionRecorder};
use crate::registry::ModelRegistry;
use crate::store::{Repair, SessionStore, SessionWriter, chain_from};
use futures::StreamExt;
use rig_agent::agent::{Agent, AgentBuilder, ModelHandle};
use rig_agent::agent::{MultiTurnStreamItem, StreamingError};
use rig_agent::completion::{Message, Usage};
use rig_agent::streaming::{StreamedUserContent, StreamingChat};
use rig_agent::tool::DynamicTool;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    /// Repairs applied to the session file itself.
    pub file_repairs: Vec<Repair>,
    /// How many interrupted tool calls had synthetic results appended.
    pub repaired_tool_calls: usize,
    /// The model selection the session resumed with (from the last
    /// `model_change` entry, if any).
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
}

/// Builds the model behind a selection: `(provider, model)` ids to a
/// type-erased handle. Overridable for callers that construct models
/// themselves (and for tests).
pub type ModelFactory = Arc<dyn Fn(&str, &str) -> Result<ModelHandle, SessionError> + Send + Sync>;

impl SessionBuilder {
    /// Start building a session that will use `selection`. The selection is
    /// validated against the config immediately.
    pub fn new(
        store: SessionStore,
        config: Arc<TabitConfig>,
        auth: Arc<AuthConfig>,
        selection: ModelSelection,
    ) -> Result<Self, SessionError> {
        validate_selection(&selection, &config)?;
        let default_factory: ModelFactory =
            ModelRegistry::new(config.clone(), auth.clone()).factory();
        drop(auth);
        Ok(Self {
            store,
            config,
            selection,
            preamble: None,
            tools: Vec::new(),
            max_turns: DEFAULT_MAX_TURNS,
            model_factory: default_factory,
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

    /// Supply models yourself instead of through tabit config. The factory
    /// receives `(provider, model)` ids; it is consulted on session
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
        let mut writer = self.store.create(cwd);
        writer.set_opening_entry(EntryKind::ModelChange {
            provider: self.selection.provider.clone(),
            model: self.selection.model.clone(),
            thinking_level: self.selection.thinking_level.clone(),
        });
        Session::assemble(self, writer, Vec::new(), false)
    }

    /// Resume the session stored at `path`: replay entries into context,
    /// repair a dangling tool-use roundtrip, and continue with the
    /// builder's selection. Callers resolve that selection through
    /// [`ModelRegistry::default_selection`] (explicit choice > the log's
    /// last model > configured preference); when it differs from the
    /// log's last model the switch is recorded as a `model_change` entry.
    pub fn resume(self, path: &Path) -> Result<(Session, ResumeReport), SessionError> {
        let loaded = self.store.open_path(path)?;
        let mut report = ResumeReport {
            file_repairs: loaded.repairs,
            ..ResumeReport::default()
        };

        let (context, _dangling) = projection::project(&loaded.chain);
        let writer = SessionWriter::open_existing(&loaded.path)?;

        let last = projection::last_model_change(&loaded.chain);
        if let Some((provider, model, thinking_level)) = last {
            report.resumed_model = Some(ModelSelection {
                provider: provider.to_string(),
                model: model.to_string(),
                thinking_level: thinking_level.map(str::to_string),
            });
        }
        validate_selection(&self.selection, &self.config)?;
        let mut session = Session::assemble(self, writer, context, true)?;
        let same_model = matches!(
            last,
            Some((provider, model, level))
                if provider == session.selection.provider
                    && model == session.selection.model
                    && level == session.selection.thinking_level.as_deref()
        );
        if !same_model {
            // Either a caller-directed switch at resume time, or a log
            // without any model_change yet — either way the session's
            // opening state is durable from here on.
            session.recorder.record(EntryKind::ModelChange {
                provider: session.selection.provider.clone(),
                model: session.selection.model.clone(),
                thinking_level: session.selection.thinking_level.clone(),
            });
        }
        // One repair path for everyone: reload_context synthesizes results
        // for a dangling trailing roundtrip (and fails loudly if they
        // cannot be persisted) and re-derives the context from the log.
        report.repaired_tool_calls = session.reload_context()?;
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
    fn text(&self) -> String {
        user_text(&self.message)
    }
}

/// The run-agnostic message mailbox: the one door every user message
/// enters ([`Session::submit`], or an engine drain mid-run), emptied by
/// [`Session::pump`] — as the next run's initial prompt or, while a run
/// is in flight, as a steer injected at the next turn boundary. The
/// mailbox outlives runs, so a message submitted at any instant is never
/// lost; only abort discards queued messages.
#[derive(Clone, Default)]
pub(crate) struct Mailbox {
    queue: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<QueuedMessage>>>,
    /// Entry ids of steers the engine drained this run, in drain order —
    /// the fold pairs them with the run's `Steer` items (the engine has
    /// no use for tabit entry ids, so the pairing lives here; drain order
    /// is emission order, both FIFO).
    steered: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    /// Pairs discarded by a clear with no sink at hand (abort at command
    /// time); the run's conclusion flushes them as one
    /// `messages_discarded`.
    discarded: std::sync::Arc<std::sync::Mutex<Vec<QueuedMessage>>>,
    /// True while a pump may drain at any instant (a run is live): the
    /// gate for submit-time `message_queued` notices. Idle sends never
    /// queue — they drain immediately, so `user_message` is the
    /// acknowledgment.
    live: std::sync::Arc<std::sync::atomic::AtomicBool>,
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

    /// The pump ended. Steers drained but never recorded (an abort raced
    /// their `Steer` items) cannot pair anymore — drop the leftovers.
    pub(crate) fn run_ended(&self) {
        self.live.store(false, std::sync::atomic::Ordering::Release);
        lock(&self.steered).clear();
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
            stream,
            event: SessionEvent::MessageQueued { id, text },
        });
    }

    pub(crate) fn is_empty(&self) -> bool {
        lock(&self.queue).is_empty()
    }

    /// Discard everything queued, returning the pairs (the caller emits
    /// `messages_discarded` where its event flow allows).
    pub(crate) fn clear(&self) -> Vec<QueuedMessage> {
        lock(&self.queue).drain(..).collect()
    }

    /// Abort semantics: discard everything queued now (nothing more may
    /// drain), staging the pairs for the run's conclusion to emit — the
    /// discard notice rides the wind-down, after the terminal.
    pub(crate) fn abort_clear(&self) {
        let cleared = self.clear();
        lock(&self.discarded).extend(cleared);
    }

    /// The staged discard pairs, taken (the run's conclusion flushes them).
    pub(crate) fn take_staged_discards(&self) -> Vec<QueuedMessage> {
        std::mem::take(&mut *lock(&self.discarded))
    }

    /// Take the whole batch (idle entry: the worker's next run input).
    fn take_batch(&self) -> Vec<QueuedMessage> {
        lock(&self.queue).drain(..).collect()
    }

    /// The engine-side drain: the batch becomes steers. Ids park in FIFO
    /// order for the fold's `Steer` items.
    fn take_steers(&self) -> Vec<Message> {
        let batch = lock(&self.queue).drain(..).collect::<Vec<_>>();
        let mut steered = lock(&self.steered);
        batch
            .into_iter()
            .map(|queued| {
                steered.push_back(queued.id);
                queued.message
            })
            .collect()
    }

    /// The id of the next `Steer` item's message (drain order is
    /// emission order). `None` means the run drained no more steers.
    fn next_steer_id(&self) -> Option<String> {
        lock(&self.steered).pop_front()
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

    /// Whether anything is queued (the actor's idle check).
    pub(crate) fn is_empty(&self) -> bool {
        self.mailbox.is_empty()
    }

    /// Discard everything queued under abort semantics — the pairs stage
    /// for the run's conclusion to emit (see [`Mailbox::abort_clear`]).
    pub(crate) fn abort_clear(&self) {
        self.mailbox.abort_clear();
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
}

impl AbortHandle {
    /// Abort the current run, if any.
    pub fn abort(&self) {
        lock(&self.token).cancel();
    }
}

/// The engine-side view of the mailbox: what an outer loop drains at
/// each turn boundary.
struct SessionSteers {
    mailbox: Mailbox,
}

impl rig_agent::SteeringSource for SessionSteers {
    fn has_pending(&self) -> bool {
        !self.mailbox.is_empty()
    }

    fn drain(&self) -> Vec<Message> {
        self.mailbox.take_steers()
    }
}

/// A persistent, resumable conversation.
pub struct Session {
    store: SessionStore,
    config: Arc<TabitConfig>,
    selection: ModelSelection,
    preamble: Option<String>,
    tools: Vec<DynamicTool>,
    max_turns: usize,
    model_factory: ModelFactory,
    agent: Arc<Agent>,
    recorder: Arc<SessionRecorder>,
    /// Per-run cancellation token, refreshed by every outer loop; the
    /// abort handle cancels whatever run is current.
    abort: std::sync::Arc<std::sync::Mutex<CancellationToken>>,
    /// The run-agnostic message mailbox: the one door user messages enter
    /// (see [`Session::submit`]); drained by [`Session::pump`] and by the
    /// engine's turn-boundary steering.
    mailbox: Mailbox,
    context: Vec<Message>,
    /// The active chain, resident (the ruled in-memory contract: parse
    /// once per open, refresh at each context re-derivation — replay
    /// and the coming checkout read memory, nothing re-parses
    /// mid-session).
    chain: Vec<crate::entry::SessionEntry>,
    path: PathBuf,
    id: String,
    /// Whether this session continues an existing chain (`resume`) or
    /// started fresh (`create`) — reported in the handshake so a
    /// frontend that asked to resume can note a silent fresh start.
    resumed: bool,
    /// The interaction hub, attached by the session worker when it takes
    /// ownership (the hub needs the worker's event channel, which does
    /// exist until spawn). `None` for direct [`Session`] consumers:
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
    /// aborted run discards the remaining queue and stops. The drive
    /// loop for frontends ([`crate::SessionHandle`]).
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
            let batch = self.mailbox.take_batch();
            if batch.is_empty() {
                break;
            }
            let run = self.run_one(&batch, on_event).await;
            // The last terminal decides the outcome; usage and events
            // accumulate across runs.
            total.output = run.output;
            add_usage(&mut total.usage, &run.usage);
            total.events.extend(run.events);
            total.outcome = run.outcome;
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
    /// request assembly ([`Self::open_run`] — where the UUIDv7 turn-id
    /// mint is injected), the item fold ([`Self::drive`] — where announced
    /// turn ids stamp events; v2 replay reuses the same ids verbatim from
    /// the log), and the terminal/durability epilogue ([`Self::conclude`] —
    /// where the write-behind log and `messages_discarded` land).
    async fn run_one(
        &mut self,
        batch: &[QueuedMessage],
        on_event: &mut (dyn FnMut(SessionEvent) + Send),
    ) -> RunSummary {
        // Run-scoped machinery: a fresh abort token for this loop; steers
        // arrive through the run-agnostic mailbox.
        let run_token = {
            let mut slot = lock(&self.abort);
            *slot = CancellationToken::new();
            slot.clone()
        };
        let mut sink = EventSink::new(on_event);
        let history = self.stage_input(batch, &mut sink);
        let stream = self.open_run(history, &run_token).await;
        let driven = self.drive(stream, &run_token, &mut sink).await;
        let (outcome, output, usage) = self.conclude(driven, &mut sink);
        RunSummary {
            outcome,
            output,
            usage,
            events: sink.events,
        }
    }

    /// Drain-all at idle entry: the whole batch becomes this run's opening
    /// user input — one entry each, 1:1 with what the model saw — recorded
    /// first (under each message's born-early id), then handed to the
    /// engine as one conversation whose final message is the turn being
    /// sent.
    fn stage_input(&mut self, batch: &[QueuedMessage], sink: &mut EventSink<'_>) -> Vec<Message> {
        let mut history = self.context.clone();
        for queued in batch {
            self.recorder.record_as(
                &queued.id,
                EntryKind::UserMessage {
                    message: queued.message.clone(),
                },
            );
            sink.emit(SessionEvent::UserMessage {
                text: queued.text(),
                entry_id: queued.id.clone(),
            });
            history.push(queued.message.clone());
        }
        history
    }

    /// Assemble the engine request for one run: the abort token and
    /// interaction capability in the tool context, the permission gate, the
    /// recorder hook, and steering over the run-agnostic mailbox.
    async fn open_run(
        &self,
        history: Vec<Message>,
        run_token: &CancellationToken,
    ) -> rig_agent::agent::StreamingResult {
        let mut tool_context = rig_agent::tool::ToolContext::new();
        tool_context.insert(run_token.clone());
        let permission = PermissionHook::new(self.interaction.clone());
        if let Some(hub) = &self.interaction {
            tool_context.insert(hub.capability());
        }
        self.agent
            .stream_chat(history)
            .max_turns(self.max_turns)
            .tool_concurrency(TOOL_CONCURRENCY)
            .add_hook(RecorderHook(self.recorder.clone()))
            .add_hook(permission)
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
    /// guards kill process trees) — completed turns and results are already
    /// recorded, anything dangling repairs on next open, exactly like a
    /// crash.
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
                Ok(MultiTurnStreamItem::TurnCommitted { id }) => {
                    sink.emit(SessionEvent::TurnCommitted { id });
                }
                Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                    tool_result,
                    internal_call_id,
                })) => {
                    let turn_id = announce(&current_turn);
                    self.note_tool_result(
                        tool_result,
                        internal_call_id,
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
                    });
                }
                Ok(MultiTurnStreamItem::Steer { text }) => {
                    self.note_steer(text, sink);
                }
                Ok(MultiTurnStreamItem::CompletionCall(call)) => {
                    let turn_id = announce(&current_turn);
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

    /// One executed tool call's result: the durable record (whose entry id
    /// rides the event), and the event naming the tool through the
    /// correlation map its call populated. `content` is exactly the text
    /// the model saw; `status` is the execution's structured outcome.
    fn note_tool_result(
        &self,
        tool_result: rig_core::message::ToolResult,
        internal_call_id: String,
        turn_id: String,
        tool_names: &mut std::collections::BTreeMap<String, String>,
        sink: &mut EventSink<'_>,
    ) {
        let content = result_text(&tool_result);
        let status = wire_status(&tool_result.status);
        let entry_id = self.recorder.record(EntryKind::ToolResult {
            result: tool_result,
        });
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

    /// A steer drained into history mid-run: one user_message entry under
    /// the message's born-early id (the id its `message_queued` announced,
    /// parked by the drain in FIFO order), 1:1 with what the model saw.
    fn note_steer(&self, text: String, sink: &mut EventSink<'_>) {
        // The engine drains a steer before emitting its `Steer` item and
        // in the same order, so the parked-id FIFO always has this
        // item's id. An empty FIFO is an engine-contract violation —
        // internal, fail loud. Sanctioned crash (AGENTS.md doctrine).
        #[allow(clippy::expect_used)]
        let entry_id = self
            .mailbox
            .next_steer_id()
            .expect("a Steer item must follow the drain that parked its id");
        self.recorder.record_as(
            &entry_id,
            EntryKind::UserMessage {
                message: Message::user(text.clone()),
            },
        );
        sink.emit(SessionEvent::UserMessage { text, entry_id });
    }

    /// The run's epilogue: exactly one terminal (the fold already emitted
    /// `run_finished`; here `run_aborted` or `run_failed`), then the
    /// context re-derivation and durability checks that can follow a
    /// terminal with a trailing `run_failed`, then the discard flush —
    /// every message a clear took while the sink could not emit for it
    /// (abort at command time, abort's own clear) comes back as one
    /// `messages_discarded` after the terminal — then the retraction of
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
        let mut outcome = RunOutcome::Completed;
        if aborted {
            self.recorder.record(EntryKind::Aborted);
            sink.emit(SessionEvent::RunAborted {
                output: output.clone(),
            });
            // Abort means stop: discard anything queued behind the run,
            // staging the pairs for the flush below (their pending
            // displays resolve by id).
            self.mailbox.abort_clear();
            outcome = RunOutcome::Aborted;
        } else if let Some(failure) = failure {
            // The log stays the source of truth: re-derive the context,
            // and a failing reload outranks the provider error (the same
            // precedence the pre-event failure path had).
            let message = self
                .reload_context()
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| failure.to_string());
            sink.emit(SessionEvent::RunFailed { message });
            outcome = RunOutcome::Failed;
        }
        if !matches!(outcome, RunOutcome::Failed)
            && let Err(error) = self.reload_context()
        {
            sink.emit(SessionEvent::RunFailed {
                message: error.to_string(),
            });
            outcome = RunOutcome::Failed;
        }
        // Durability: a record that never reached the disk fails the run
        // even when the model answered (this `run_failed` follows the
        // terminal event — documented on the event).
        if let Some(persist_error) = self.recorder.first_error() {
            sink.emit(SessionEvent::RunFailed {
                message: persist_error,
            });
            outcome = RunOutcome::Failed;
        }
        // The discard flush: clears taken with no sink at hand (abort at
        // command time) plus this conclusion's own abort clear, after the
        // terminal — the discard notice rides the wind-down.
        let discarded: Vec<tabit_protocol::DiscardedMessage> = self
            .mailbox
            .take_staged_discards()
            .into_iter()
            .map(|queued| {
                let text = queued.text();
                tabit_protocol::DiscardedMessage {
                    id: queued.id,
                    text,
                }
            })
            .collect();
        if !discarded.is_empty() {
            sink.emit(SessionEvent::MessagesDiscarded {
                messages: discarded,
            });
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

    /// A handle for aborting the current outer loop. See [`AbortHandle`].
    pub fn abort_handle(&self) -> AbortHandle {
        AbortHandle {
            token: self.abort.clone(),
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
        let loaded = self.store.open_path(&self.path)?;
        let boundaries = projection::user_message_boundaries(&loaded.chain);
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
                    "cannot rewind {turns} user message(s): the active chain holds {}",
                    boundaries.len()
                ),
            });
        };
        // The branch point is the boundary's parent; the new chain is the
        // current chain truncated right after it.
        let new_chain = match &target.parent_id {
            Some(branch_point) => {
                let Some(end) = loaded.chain.iter().position(|e| &e.id == branch_point) else {
                    // Unreachable: the boundary sits on the chain, so its
                    // parent does too — but a hand-crafted log is not
                    // trusted to keep that promise.
                    return Err(SessionError::Corrupt {
                        path: self.path.clone(),
                        message: format!(
                            "boundary `{}` has parent `{branch_point}` outside the active chain",
                            target.id
                        ),
                    });
                };
                loaded.chain.iter().take(end + 1).cloned().collect()
            }
            None => Vec::new(),
        };
        let dropped = boundaries.len() - projection::user_message_boundaries(&new_chain).len();
        self.apply_rewind(target.parent_id.as_deref(), new_chain, dropped)
    }

    /// Rewind to an exact entry: the active chain will end at that entry.
    /// Any entry in the file is a valid target, on or off the active chain
    /// (this is also how a branch switch happens); a target that leaves a
    /// partially answered tool batch gets the same interrupted-result
    /// repair a crash gets. The library primitive for tree-picking
    /// frontends — [`Session::rewind`] is the user-facing form.
    pub fn rewind_to_entry(&mut self, entry_id: &str) -> Result<RewindSummary, SessionError> {
        let loaded = self.store.open_path(&self.path)?;
        if !loaded.entries.iter().any(|entry| entry.id == entry_id) {
            return Err(SessionError::Config {
                message: format!("no entry `{entry_id}` in this session"),
            });
        }
        let new_chain = chain_from(&loaded.entries, Some(entry_id), &loaded.path)?;
        let dropped = projection::user_message_boundaries(&loaded.chain)
            .len()
            .saturating_sub(projection::user_message_boundaries(&new_chain).len());
        self.apply_rewind(Some(entry_id), new_chain, dropped)
    }

    /// Shared rewind mechanics: validate the new chain's model against the
    /// config first (nothing is written when it does not resolve), then
    /// record the marker, reload the context onto the new chain, and
    /// re-align selection and agent with the chain's model history.
    fn apply_rewind(
        &mut self,
        branch_point: Option<&str>,
        new_chain: Vec<SessionEntry>,
        dropped: usize,
    ) -> Result<RewindSummary, SessionError> {
        let chain_model =
            projection::last_model_change(&new_chain).map(|(provider, model, thinking_level)| {
                ModelSelection {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    thinking_level: thinking_level.map(str::to_string),
                }
            });
        if let Some(selection) = &chain_model {
            validate_selection(selection, &self.config)?;
        }

        self.recorder.rewind_to(branch_point);
        if let Some(error) = self.recorder.first_error() {
            return Err(SessionError::Persist(error));
        }
        // Repairs for a dangling tail land on the new chain, at the new
        // leaf.
        self.reload_context()?;
        match chain_model {
            // The chain carries its own model history: adopt it. No new
            // entry — the chain's last model_change already says it.
            Some(selection) => {
                if selection != self.selection {
                    self.rebuild_agent(&selection)?;
                    self.selection = selection;
                }
            }
            // A chain older than any model_change: make the current
            // selection durable at the new tip, exactly like resume.
            None => {
                self.recorder.record(EntryKind::ModelChange {
                    provider: self.selection.provider.clone(),
                    model: self.selection.model.clone(),
                    thinking_level: self.selection.thinking_level.clone(),
                });
            }
        }
        Ok(RewindSummary {
            dropped,
            to_entry: branch_point.unwrap_or_default().to_string(),
        })
    }

    /// Switch the provider/model/thinking level from the next outer loop
    /// on. Recorded as a `model_change` entry.
    pub fn set_model(&mut self, selection: ModelSelection) -> Result<(), SessionError> {
        validate_selection(&selection, &self.config)?;
        self.rebuild_agent(&selection)?;
        self.recorder.record(EntryKind::ModelChange {
            provider: selection.provider.clone(),
            model: selection.model.clone(),
            thinking_level: selection.thinking_level.clone(),
        });
        self.selection = selection;
        Ok(())
    }

    /// Change the thinking level without changing provider/model. `None`
    /// clears it.
    pub fn set_thinking_level(&mut self, level: Option<&str>) -> Result<(), SessionError> {
        let selection = ModelSelection {
            provider: self.selection.provider.clone(),
            model: self.selection.model.clone(),
            thinking_level: level.map(str::to_string),
        };
        self.set_model(selection)
    }

    /// The active model selection.
    pub fn selection(&self) -> &ModelSelection {
        &self.selection
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

    /// The projected model-visible context (what the next outer loop sees).
    pub fn context(&self) -> &[Message] {
        &self.context
    }

    /// Usage and cost totals, folded from the active chain. Re-reads the
    /// session file so the answer is always consistent with what is on
    /// disk.
    pub fn stats(&self) -> Result<SessionStats, SessionError> {
        let loaded = self.store.open_path(&self.path)?;
        Ok(self.fold_stats(&loaded.chain))
    }

    /// The replay pass (PROTOCOL.md v2): the resident chain projected
    /// into finalized live events — the same shapes a live run produces,
    /// ids verbatim from the log, so a frontend renders history and live
    /// turns with one set of arms. Slice 3's cross-branch checkout
    /// reuses this over a different chain.
    pub fn replay_events(&self) -> Vec<SessionEvent> {
        crate::replay::project_events(&self.chain)
    }

    /// Re-derive the in-memory context from the log's active chain. If the
    /// chain ends on a dangling tool-use roundtrip (an interrupted run or
    /// a mid-batch branch point), repair it with synthesized results — the
    /// same fix resume applies — so the context stays replayable.
    fn reload_context(&mut self) -> Result<usize, SessionError> {
        let loaded = self.store.open_path(&self.path)?;
        let (_, dangling) = projection::project(&loaded.chain);
        let mut repaired = 0;
        if let Some(dangling) = &dangling {
            for result in projection::interrupted_results(dangling) {
                self.recorder.record(EntryKind::ToolResult { result });
            }
            repaired = dangling.calls.len();
            // A repair that cannot reach the disk leaves the log
            // unreplayable; surface it instead of projecting around it.
            if let Some(error) = self.recorder.first_error() {
                return Err(SessionError::Persist(error));
            }
        }
        let reloaded = self.store.open_path(&self.path)?;
        let (context, _) = projection::project(&reloaded.chain);
        self.context = context;
        // The chain stays resident: replay (and the coming checkout)
        // reads this, not the file.
        self.chain = reloaded.chain;
        Ok(repaired)
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

    fn fold_stats(&self, entries: &[SessionEntry]) -> SessionStats {
        let mut stats = SessionStats::default();
        // Attributed by the log's own model_change entries; assistant turns
        // before any change entry attribute to empty ids (uncosted).
        let mut current = (String::new(), String::new(), None);
        let mut per_model: Vec<ModelStats> = Vec::new();
        for entry in entries {
            match &entry.kind {
                EntryKind::ModelChange {
                    provider,
                    model,
                    thinking_level,
                } => {
                    current = (provider.clone(), model.clone(), thinking_level.clone());
                }
                EntryKind::AssistantMessage { usage, .. } => {
                    let (provider, model, level) = &current;
                    match per_model
                        .iter_mut()
                        .find(|s| &s.provider == provider && &s.model == model)
                    {
                        Some(slot) => add_usage(&mut slot.usage, usage),
                        None => per_model.push(ModelStats {
                            provider: provider.clone(),
                            model: model.clone(),
                            thinking_level: level.clone(),
                            usage: *usage,
                            cost: None,
                        }),
                    }
                    add_usage(&mut stats.total_usage, usage);
                }
                _ => continue,
            }
        }
        for model_stats in &mut per_model {
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
        }
        stats.per_model = per_model;
        stats
    }

    fn rebuild_agent(&mut self, selection: &ModelSelection) -> Result<(), SessionError> {
        let handle = (self.model_factory)(&selection.provider, &selection.model)?;
        let params = crate::registry::request_params(&self.config, selection);
        // `dynamic_tools` (even with an empty vec) moves the builder to
        // its tool-configured state, keeping one concrete type through
        // the preamble/build chain.
        let mut builder = AgentBuilder::new(handle).dynamic_tools(self.tools.clone());
        if let Some(preamble) = &self.preamble {
            builder = builder.preamble(preamble.as_str());
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
        self.agent = Arc::new(builder.build());
        Ok(())
    }

    fn assemble(
        builder: SessionBuilder,
        writer: SessionWriter,
        context: Vec<Message>,
        resumed: bool,
    ) -> Result<Self, SessionError> {
        let path = writer.path().to_path_buf();
        let id = writer.session_id().to_string();
        let recorder = Arc::new(SessionRecorder::new(writer));
        let mut session = Self {
            store: builder.store,
            config: builder.config,
            selection: builder.selection,
            preamble: builder.preamble,
            tools: builder.tools,
            max_turns: builder.max_turns,
            model_factory: builder.model_factory,
            agent: Arc::new(AgentBuilder::new(ModelHandle::new(placeholder_model())).build()),
            recorder,
            abort: std::sync::Arc::new(std::sync::Mutex::new(CancellationToken::new())),
            mailbox: Mailbox::default(),
            context,
            chain: Vec::new(),
            path,
            id,
            resumed,
            interaction: None,
        };
        let selection = session.selection.clone();
        session.rebuild_agent(&selection)?;
        Ok(session)
    }
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
        // `CompletionCall` is handled by an explicit arm in `run_one` — it
        // can carry a second, truncation-warning event beside the usage one.
        MultiTurnStreamItem::CompletionCall(_) => None,
        MultiTurnStreamItem::ModelTurnRetried { turn } => {
            Some(SessionEvent::TurnRetried { turn_id, turn })
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
/// shape. `exit_code` means exit code: the structured code passes
/// through exactly when numeric (a shell tool's exit status); other
/// codes are not exit codes and their detail already lives in the
/// content. Shared by the live fold and the replay projection — one
/// translation, one truth.
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
        None => tabit_protocol::ToolResultStatus::Success,
    }
}

fn add_usage(target: &mut Usage, source: &Usage) {
    target.input_tokens += source.input_tokens;
    target.output_tokens += source.output_tokens;
    target.total_tokens += source.total_tokens;
    target.cached_input_tokens += source.cached_input_tokens;
    target.cache_creation_input_tokens += source.cache_creation_input_tokens;
}

fn cost_of(usage: &Usage, cost: &tabit_config::Cost) -> f64 {
    (usage.input_tokens as f64 / 1_000_000.0) * cost.input
        + (usage.output_tokens as f64 / 1_000_000.0) * cost.output
        + (usage.cached_input_tokens as f64 / 1_000_000.0) * cost.cache_read
        + (usage.cache_creation_input_tokens as f64 / 1_000_000.0) * cost.cache_write
}

/// A model that is never called: every assembled session rebuilds its real
/// agent from config immediately after construction, so this exists only
/// to satisfy the field initializer.
fn placeholder_model() -> impl rig_core::completion::CompletionModel {
    UnreachableModel
}

/// See [`placeholder_model`].
struct UnreachableModel;

impl rig_core::completion::CompletionModel for UnreachableModel {
    fn completion(
        &self,
        _request: rig_core::completion::CompletionRequest,
    ) -> impl std::future::Future<
        Output = Result<
            rig_core::completion::CompletionResponse,
            rig_core::completion::CompletionError,
        >,
    > + rig_core::wasm_compat::WasmCompatSend {
        std::future::ready(Err(internal_placeholder_error()))
    }

    fn stream(
        &self,
        _request: rig_core::completion::CompletionRequest,
    ) -> impl std::future::Future<
        Output = Result<
            rig_core::streaming::StreamingCompletionResponse,
            rig_core::completion::CompletionError,
        >,
    > + rig_core::wasm_compat::WasmCompatSend {
        std::future::ready(Err(internal_placeholder_error()))
    }
}

fn internal_placeholder_error() -> rig_core::completion::CompletionError {
    rig_core::completion::CompletionError::ProviderError(
        "internal invariant violated: placeholder model was called".to_string(),
    )
}
