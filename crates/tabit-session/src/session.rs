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
use tabit_protocol::ModelSelection;
use tabit_protocol::SessionEvent;
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

/// The run-agnostic message mailbox: the one door every user message
/// enters ([`Session::submit`], or an engine drain mid-run), emptied by
/// [`Session::pump`] — as the next run's initial prompt or, while a run
/// is in flight, as a steer injected at the next turn boundary. The
/// mailbox outlives runs, so a message submitted at any instant is never
/// lost; only abort discards queued messages.
#[derive(Clone, Default)]
pub(crate) struct Mailbox {
    queue: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<Message>>>,
    /// Wakes the resident worker when work arrives. One permit covers any
    /// number of pushes; the queue itself is the source of truth — the
    /// signal exists only so an empty queue can be waited on.
    work: std::sync::Arc<tokio::sync::Notify>,
}

impl Mailbox {
    pub(crate) fn push(&self, message: Message) {
        lock(&self.queue).push_back(message);
        self.work.notify_one();
    }

    pub(crate) fn is_empty(&self) -> bool {
        lock(&self.queue).is_empty()
    }

    pub(crate) fn clear(&self) {
        lock(&self.queue).clear();
    }

    fn drain_all(&self) -> Vec<Message> {
        lock(&self.queue).drain(..).collect()
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

    /// Discard everything queued (abort semantics).
    pub(crate) fn clear(&self) {
        self.mailbox.clear();
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
        self.mailbox.drain_all()
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
        let mut total = RunSummary {
            outcome: RunOutcome::Completed,
            output: String::new(),
            usage: Usage::default(),
            events: Vec::new(),
        };
        loop {
            let batch = self.mailbox.drain_all();
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
        total
    }

    /// One outer loop for a drained batch: record every message, run the
    /// engine to completion, re-derive the context from the log, and
    /// report. Exactly one terminal event comes out
    /// (`run_finished`/`run_aborted`/`run_failed`), plus a trailing
    /// `run_failed` when durability checks fail after a terminal.
    async fn run_one(
        &mut self,
        batch: &[Message],
        on_event: &mut (dyn FnMut(SessionEvent) + Send),
    ) -> RunSummary {
        // Run-scoped machinery: a fresh abort token for this loop; steers
        // arrive through the run-agnostic mailbox.
        let run_token = {
            let mut slot = lock(&self.abort);
            *slot = CancellationToken::new();
            slot.clone()
        };
        let mut events = Vec::new();
        // Drain-all at idle entry: the whole batch becomes this run's
        // opening user input — one entry each, 1:1 with what the model
        // saw — recorded first, then handed to the engine as one
        // conversation whose final message is the turn being sent.
        let mut history = self.context.clone();
        for message in batch {
            self.recorder.record(EntryKind::UserMessage {
                message: message.clone(),
            });
            let event = SessionEvent::UserMessage {
                text: user_text(message),
            };
            on_event(event.clone());
            events.push(event);
            history.push(message.clone());
        }
        let mut tool_context = rig_agent::tool::ToolContext::new();
        tool_context.insert(run_token.clone());
        let permission = PermissionHook::new(self.interaction.clone());
        if let Some(hub) = &self.interaction {
            tool_context.insert(hub.capability());
        }
        let request = self
            .agent
            .stream_chat(history)
            .max_turns(self.max_turns)
            .tool_concurrency(TOOL_CONCURRENCY)
            .add_hook(RecorderHook(self.recorder.clone()))
            .add_hook(permission)
            .steering(std::sync::Arc::new(SessionSteers {
                mailbox: self.mailbox.clone(),
            }))
            .tool_context(tool_context);
        let mut stream = request.await;

        let mut output = String::new();
        let mut usage = Usage::default();
        // Tool names by correlation id: the result items carry the call's
        // internal id but not its name.
        let mut tool_names: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        let mut aborted = false;
        // The provider failure that ended the stream, if one did; the
        // `run_failed` event is emitted after the loop.
        let mut stream_failure: Option<SessionError> = None;
        loop {
            let item = tokio::select! {
                biased;
                _ = run_token.cancelled() => {
                    aborted = true;
                    break;
                }
                item = stream.next() => match item {
                    Some(item) => item,
                    None => break,
                },
            };
            match item {
                Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                    tool_result,
                    internal_call_id,
                })) => {
                    self.recorder.record(EntryKind::ToolResult {
                        result: tool_result,
                    });
                    let event = SessionEvent::ToolResult {
                        name: tool_names
                            .get(&internal_call_id)
                            .cloned()
                            .unwrap_or_default(),
                        internal_call_id,
                    };
                    on_event(event.clone());
                    events.push(event);
                }
                Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                    output = response.output;
                    usage = response.usage;
                    let event = SessionEvent::RunFinished {
                        output: output.clone(),
                        usage: Self::wire_usage(&usage),
                    };
                    on_event(event.clone());
                    events.push(event);
                }
                Ok(MultiTurnStreamItem::Steer { text }) => {
                    // One steer, one user_message entry: the log keeps 1:1
                    // fidelity with what the model saw.
                    self.recorder.record(EntryKind::UserMessage {
                        message: Message::user(text.clone()),
                    });
                    let event = SessionEvent::UserMessage { text };
                    on_event(event.clone());
                    events.push(event);
                }
                Ok(MultiTurnStreamItem::CompletionCall(call)) => {
                    let event = SessionEvent::CompletionCall {
                        input_tokens: call.usage.input_tokens,
                        output_tokens: call.usage.output_tokens,
                    };
                    on_event(event.clone());
                    events.push(event);
                    // A truncation-class finish reason is a warning, not a
                    // failure (ENGINE.md behavior delta 9): the flow
                    // continues untouched — steers drain into the next turn,
                    // the run may end normally.
                    if call.finish_reason == Some(rig_core::completion::FinishReason::Length) {
                        let event = SessionEvent::TurnTruncated;
                        on_event(event.clone());
                        events.push(event);
                    }
                }
                Ok(item) => {
                    if let Some(event) = stream_item_event(item, &mut tool_names) {
                        on_event(event.clone());
                        events.push(event);
                    }
                }
                Err(StreamingError::Completion(error)) => {
                    stream_failure = Some(SessionError::Prompt(error.into()));
                    break;
                }
                Err(StreamingError::Prompt(error)) => {
                    stream_failure = Some(SessionError::Prompt(*error));
                    break;
                }
            }
        }

        let mut outcome = RunOutcome::Completed;
        if aborted {
            // Dropping the stream cancels in-flight tool futures; their
            // drop guards kill process trees. Completed turns and results
            // are already recorded; anything dangling repairs on next
            // open, exactly like a crash.
            drop(stream);
            self.recorder.record(EntryKind::Aborted);
            let event = SessionEvent::RunAborted {
                output: output.clone(),
            };
            on_event(event.clone());
            events.push(event);
            // Abort means stop: discard anything queued behind the run.
            // Nothing queued was ever acknowledged by an event, so
            // nothing observable vanishes.
            self.mailbox.clear();
            outcome = RunOutcome::Aborted;
        } else if let Some(failure) = stream_failure {
            // The log stays the source of truth: re-derive the context,
            // and a failing reload outranks the provider error (the same
            // precedence the pre-event failure path had).
            let message = self
                .reload_context()
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| failure.to_string());
            let event = SessionEvent::RunFailed { message };
            on_event(event.clone());
            events.push(event);
            outcome = RunOutcome::Failed;
        }
        if !matches!(outcome, RunOutcome::Failed)
            && let Err(error) = self.reload_context()
        {
            let event = SessionEvent::RunFailed {
                message: error.to_string(),
            };
            on_event(event.clone());
            events.push(event);
            outcome = RunOutcome::Failed;
        }
        // Durability: a record that never reached the disk fails the run
        // even when the model answered (this `run_failed` follows the
        // terminal event — documented on the event).
        if let Some(persist_error) = self.recorder.first_error() {
            let event = SessionEvent::RunFailed {
                message: persist_error,
            };
            on_event(event.clone());
            events.push(event);
            outcome = RunOutcome::Failed;
        }
        // Every terminal that will be emitted has been; no asker
        // survives the run. Retract any question still standing so its
        // sender cannot linger (a racing response is then a total no-op).
        if let Some(hub) = &self.interaction {
            hub.clear_pending();
        }
        RunSummary {
            outcome,
            output,
            usage,
            events,
        }
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
    /// ([`crate::endpoint::SessionHandle::spawn`]) when it takes
    /// ownership — the hub is built over the worker's event channel,
    /// which exists only there.
    pub fn attach_interaction(&mut self, hub: InteractionHub) {
        self.interaction = Some(hub);
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
        // `dynamic_tools` (even with an empty vec) moves the builder to
        // its tool-configured state, keeping one concrete type through
        // the preamble/build chain.
        let mut builder = AgentBuilder::new(handle).dynamic_tools(self.tools.clone());
        if let Some(preamble) = &self.preamble {
            builder = builder.preamble(preamble.as_str());
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

/// Map an engine item to a session event; `None` means "not surfaced in
/// v1".
fn stream_item_event(
    item: MultiTurnStreamItem,
    tool_names: &mut std::collections::BTreeMap<String, String>,
) -> Option<SessionEvent> {
    use rig_agent::streaming::StreamedAssistantContent as A;
    match item {
        MultiTurnStreamItem::StreamAssistantItem(A::Text(text)) => {
            Some(SessionEvent::TextDelta { text: text.text })
        }
        MultiTurnStreamItem::StreamAssistantItem(A::ReasoningDelta { id, reasoning }) => {
            Some(SessionEvent::ReasoningDelta { id, reasoning })
        }
        MultiTurnStreamItem::StreamAssistantItem(A::ToolCall {
            tool_call,
            internal_call_id,
        }) => {
            tool_names.insert(internal_call_id.clone(), tool_call.function.name.clone());
            Some(SessionEvent::ToolCall {
                name: tool_call.function.name,
                call_id: tool_call.id,
                arguments: Some(tool_call.function.arguments.to_string()),
                internal_call_id,
            })
        }
        MultiTurnStreamItem::StreamAssistantItem(A::Unknown(item)) => {
            Some(SessionEvent::NativeItem { item })
        }
        MultiTurnStreamItem::StreamAssistantItem(_) => None,
        MultiTurnStreamItem::ToolExecutionCommitted { .. } => None,
        MultiTurnStreamItem::StreamUserItem(_) => None,
        // `CompletionCall` is handled by an explicit arm in `run_one` — it
        // can carry a second, truncation-warning event beside the usage one.
        MultiTurnStreamItem::CompletionCall(_) => None,
        MultiTurnStreamItem::ModelTurnRetried { turn } => Some(SessionEvent::TurnRetried { turn }),
        MultiTurnStreamItem::FinalResponse(_) => None, // handled by the caller
        _ => None,
    }
}

/// The text of a user message (joined text parts).
fn user_text(message: &Message) -> String {
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
