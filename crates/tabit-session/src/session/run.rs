//! The outer loop: pump, one-run orchestration, the engine drive, and
//! the item-to-event fold — plus the run's report types and the event
//! fan-out every emission goes through.

use super::mailbox::SessionSteers;
use super::wire::{result_details, result_text, user_text, wire_status, wire_usage};
use super::{Session, TOOL_CONCURRENCY};
use crate::entry::{FileRecord, SideKind, SideRecord};
use crate::error::SessionError;
use crate::lock::lock;
use crate::stats::add_usage;
use futures::StreamExt;
use rig_agent::agent::{MultiTurnStreamItem, StreamingError};
use rig_agent::completion::{Message, Usage};
use rig_agent::streaming::{StreamedUserContent, StreamingChat};
use std::sync::Arc;
use tabit_protocol::SessionEvent;
use tokio_util::sync::CancellationToken;

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
        // the buffered log before anything drains in — a still-refusing
        // flush blocks this start (the first failed drain ran in
        // memory; no run proceeds twice on it). An unborn session's
        // probe skips under the no-orphan gate: nothing was ever owed
        // to the disk, so there is nothing to recover — the trade the
        // gate buys (a fresh session's first turn runs against a full
        // disk and degrades at its own commit instead).
        let guard_outcome = crate::lock::lock(&self.buffer).enqueue(&[]);
        if let Err(error) = &guard_outcome {
            self.drain_persist_transitions();
            self.fail_before_engine(
                format!(
                    "the session log is undrained and still refuses to flush: {error} \
                     — the message is kept; free the log and retry to answer it"
                ),
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
            self.fail_before_engine(
                format!(
                    "{error} — the message is kept; switch the model and retry to \
                     answer it"
                ),
                &mut sink,
            );
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
        tool_context.insert(rig_agent::tool::SessionCwd(self.cwd.clone()));
        if let Some(hub) = &self.interaction {
            tool_context.insert(hub.capability());
        }
        // The cell IS the conversation (ENGINE.md, the unified
        // conversation): the run folds the session's one durable
        // manager, and the opening message — if any — arrives through
        // the steering drain at the loop's first convergence.
        let mut request = self
            .agent
            .stream_over(self.conversation.clone())
            .max_turns(self.max_turns)
            .tool_concurrency(TOOL_CONCURRENCY);
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
                        usage: wire_usage(&driven.usage),
                        durable: self.buffer_is_clean(),
                    });
                }
                Ok(MultiTurnStreamItem::Steer { batch }) => {
                    // The whole batch is already committed (the fold and
                    // the yield share one poll); announce every pair in
                    // one synchronous loop — an abort cannot split it.
                    for (entry_id, text) in batch {
                        sink.emit(SessionEvent::UserMessage { text, entry_id });
                    }
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
        let details = result_details(&tool_result);
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
            details,
        });
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
