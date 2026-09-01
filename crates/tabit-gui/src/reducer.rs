//! The GUI's state machine: a pure fold over backend messages.
//!
//! This module is deliberately framework-free (no egui types) and
//! unit-tested — the ROADMAP "GUI design contract": the view is a
//! projection of [`GuiState`], business logic never lives in it, and a
//! future framework switch rewrites only the view layer.
//!
//! Multi-session (protocol v3): the backend hosts many sessions and
//! every event carries its session id as the stream stamp. The window
//! keeps **one active view** — the transcript renders the active
//! stream only. Switching is optimistic (clear the view immediately)
//! and the backend's replay pass rebuilds it; per-session liveness
//! (running, an attention flag) rides the switcher rows. Background
//! events update liveness only — with one exception: `error` events
//! always surface (stage 1: as a notice in the active transcript; an
//! attribution imperfection accepted until multi-view).

use std::collections::VecDeque;

use tabit_protocol::{EventFrame, ModelSelection, SessionEvent, Usage};

/// One message from the backend process: a protocol frame or a
/// lifecycle fact (the child exited).
#[derive(Debug, Clone, PartialEq)]
pub enum InMsg {
    /// `initialize_ack` — the session facts.
    Ack {
        session_id: String,
        session_path: String,
        model: ModelSelection,
        /// False after a `--continue` that found nothing: the backend
        /// started fresh (absorbed, not an error).
        resumed: bool,
    },
    /// `initialize_rejected` — the connection is over.
    Rejected(String),
    /// `protocol_error` — display-only; the connection stays.
    ProtocolError(String),
    /// A stamped event.
    Event(Box<EventFrame>),
    /// The backend's stdout reached EOF and the child exited with
    /// `code` (None = killed by a signal).
    BackendExited { code: Option<i32> },
}

/// Where the window stands with its backend.
#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    /// Spawned, handshake in flight.
    Connecting,
    /// Acked and talking.
    Live,
    /// The backend is gone. `clean` = it was idle when it ended (a
    /// drain-to-EOF after stdin close); anything else is a crash
    /// surface to the user.
    Exited { clean: bool, reason: String },
}

/// Session facts from the handshake.
#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub session_id: String,
    pub session_path: String,
    pub model: ModelSelection,
    /// False when the backend absorbed a `--continue` miss and started
    /// fresh — the GUI always asks to resume, so false always means the
    /// note is warranted.
    pub resumed: bool,
}

/// One row of the session switcher: the backend's catalog (`sessions_
/// available`, `session_created`) plus the liveness this window tracks
/// from stamped events (background sessions keep running — the
/// feature-in-one-review-in-another shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    pub created_at: String,
    pub entry_count: u64,
    /// A run is live in this session (from its stamped events).
    pub running: bool,
    /// Something needs the user's eyes in this session (an error while
    /// it was in the background); cleared by switching to it.
    pub attention: bool,
}

/// One tool call the model issued, and whether its result arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallRow {
    pub name: String,
    pub call_id: String,
    pub internal_call_id: String,
    pub arguments: Option<String>,
    pub done: bool,
    /// The result once it committed: its faithful content (exactly what
    /// the model saw) and whether the execution failed.
    pub result: Option<ToolResultRow>,
}

/// One committed tool result, as displayed under its call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultRow {
    pub content: String,
    pub failed: bool,
}

/// One open interaction card, typed by its template (FRONTEND.md §8):
/// the `native:confirm` card (the permission gate's three-button ask)
/// and the `native:ask` free-text card. Several may be open at once,
/// across sessions; run terminals close them all — scoped to the
/// terminal's session. Cards never replay; the durable record is the
/// tool result. Unknown `ui_type`s are not cards — the frontend
/// cannot construct their answers — they surface as notices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionCard {
    /// `native:confirm` (the payload parsed at reduce time).
    Confirm {
        /// The session the question belongs to — answers route by it.
        session: String,
        id: String,
        title: String,
        body: String,
        /// Button labels.
        options: Vec<String>,
        /// Whether a free-text answer/explanation is invited.
        free_text: bool,
    },
    /// `native:ask`.
    Ask {
        session: String,
        id: String,
        prompt: String,
    },
}

impl InteractionCard {
    /// The request id (answers address it).
    pub fn id(&self) -> &str {
        match self {
            InteractionCard::Confirm { id, .. } | InteractionCard::Ask { id, .. } => id,
        }
    }

    /// The session the question belongs to.
    pub fn session(&self) -> &str {
        match self {
            InteractionCard::Confirm { session, .. } | InteractionCard::Ask { session, .. } => {
                session
            }
        }
    }
}

/// One arrival-ordered piece of a turn. The wire interleaves
/// reasoning, text, and tool calls as they happen; the transcript
/// must render exactly that order (bucketing by type relocates tool
/// calls above text that arrived first — owner report).
#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Reasoning { id: String, text: String },
    Text(String),
    ToolCall(ToolCallRow),
}

/// An assistant turn: arrival-ordered segments.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TurnGroup {
    pub segments: Vec<Segment>,
}

/// A mid-run submission waiting for its turn boundary. `id` is the
/// backend's born-early entry id, learned from `message_queued` (local
/// sends start `None`; the upgrade pairs by text — symmetric for
/// duplicates — until the backend's id lands).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingMessage {
    pub id: Option<String>,
    pub text: String,
}

/// One renderable transcript row.
#[derive(Debug, Clone, PartialEq)]
pub enum Group {
    /// A user message that entered history. `entry_id` is the
    /// checkout target — the interim rewind affordance sends it back
    /// as `checkout { session, entry_id }` ("cut here": the chain
    /// ends at this message).
    User { text: String, entry_id: String },
    /// Assistant output (provisional while a run is live — v1 has no
    /// commit signal; v2's `turn_committed` will make it explicit).
    Turn(TurnGroup),
    /// A session-level note the user must see (failures, protocol
    /// errors). `error` marks it for error styling.
    Notice { text: String, error: bool },
    /// A provider-native item, kept opaque.
    Native { item: String },
}

/// The whole window state. Reduce with [`GuiState::reduce`]; query
/// from the view. No egui types in here, ever.
#[derive(Debug, Clone, PartialEq)]
pub struct GuiState {
    pub phase: Phase,
    pub facts: Option<Facts>,
    /// The stream the transcript renders — the active session's id
    /// (set at the handshake, at `session_created`, and by the user's
    /// switch). Events stamped otherwise update liveness only.
    pub active: String,
    /// Every known session: the startup catalog plus sessions created
    /// over this connection.
    pub sessions: Vec<SessionRow>,
    pub transcript: Vec<Group>,
    /// Steers sent mid-run, waiting for the turn boundary — resolved
    /// exactly by id: `message_queued` announces the id at submit,
    /// `user_message`/`messages_discarded` carrying it drops the row.
    /// Idle sends never enter it (they drain immediately;
    /// `user_message` is the acknowledgment). View-local: switching
    /// away drops the display (the backend keeps the queue).
    pub pending: VecDeque<PendingMessage>,
    /// True from the first `user_message` of a run to its terminal
    /// (on the active stream). Replay passes contain `user_message`s
    /// but never a terminal — liveness writes are suppressed inside a
    /// pass bracket (see `replaying`).
    pub running: bool,
    /// Streams with an open replay pass (started/stopped by the
    /// brackets). A pass is history, not liveness: events inside it
    /// must not mark a session running — there is no terminal coming
    /// to settle the flag.
    pub replaying: Vec<String>,
    /// Open interaction cards, oldest first, across ALL sessions —
    /// cards never replay (FRONTEND.md §8) and never survive their
    /// run terminal, so losing them at a view switch (the stage-1
    /// shape) made a parked permission card unrecoverable and
    /// deadlocked the session's own replay pass (the pass waits for
    /// the run; the run waits on the now-invisible card). The view
    /// renders the active stream's cards; background cards raise the
    /// row's attention flag.
    pub interactions: Vec<InteractionCard>,
    /// Sum of `run_finished` usage across runs.
    pub usage: Usage,
    /// The backend refused the handshake (e.g. first-run setup
    /// needed) — never retried automatically; the reason is the
    /// message to show.
    pub handshake_rejected: bool,
}

impl Default for GuiState {
    fn default() -> Self {
        Self {
            phase: Phase::Connecting,
            facts: None,
            active: String::new(),
            sessions: Vec::new(),
            transcript: Vec::new(),
            pending: VecDeque::new(),
            running: false,
            replaying: Vec::new(),
            interactions: Vec::new(),
            usage: Usage::default(),
            handshake_rejected: false,
        }
    }
}

impl GuiState {
    /// Fold one backend message into the state.
    pub fn reduce(&mut self, msg: InMsg) {
        match msg {
            InMsg::Ack {
                session_id,
                session_path,
                model,
                resumed,
            } => {
                self.facts = Some(Facts {
                    session_id: session_id.clone(),
                    session_path,
                    model,
                    resumed,
                });
                // The boot session's stream is its id; the transcript
                // renders it.
                self.active = session_id;
                self.phase = Phase::Live;
                // The GUI always spawns with `--continue`; a fresh
                // start behind that ask gets one muted note (the pinned
                // startup contract: an empty store is not an error, but
                // it is not silent either).
                if !resumed {
                    self.push_notice("no sessions to resume — started fresh".to_string(), false);
                }
            }
            InMsg::Rejected(reason) => {
                // The full reason (a setup guide on a fresh install)
                // is transcript material; the status strip stays short.
                self.handshake_rejected = true;
                self.phase = Phase::Exited {
                    clean: false,
                    reason: "handshake rejected — see the note in the transcript".to_string(),
                };
                self.push_notice(reason, true);
            }
            InMsg::ProtocolError(text) => {
                self.push_notice(text, true);
            }
            InMsg::Event(frame) => self.reduce_event(*frame),
            InMsg::BackendExited { code } => {
                // Never overwrite the real story: a rejection or an
                // earlier exit already said why.
                if matches!(self.phase, Phase::Exited { .. }) {
                    return;
                }
                let was_running = self.running;
                self.running = false;
                let exit = match code {
                    Some(code) => format!("backend exited with code {code}"),
                    None => "backend was killed".to_string(),
                };
                // Exit 101 is the internal-error crash (FRONTEND.md
                // §3.5): never clean, whatever the timing — the stderr
                // report is what the user sends back.
                let (clean, tail) = if code == Some(101) {
                    (false, " — internal error; send back the report below")
                } else if self.facts.is_none() {
                    // Never handshook: a startup failure, not a crash
                    // mid-conversation (stderr tail explains).
                    (false, " — see stderr details")
                } else if was_running {
                    (false, " mid-run; the transcript tail was not committed")
                } else {
                    (true, " (idle — nothing was lost)")
                };
                self.phase = Phase::Exited {
                    clean,
                    reason: format!("{exit}{tail}"),
                };
            }
        }
    }

    /// Record that a message left the input box. Only mid-run sends
    /// (steers) enter the waiting display; idle sends drain
    /// immediately and are acknowledged by `user_message` directly.
    pub fn message_sent(&mut self, text: String) {
        if self.running {
            self.pending.push_back(PendingMessage { id: None, text });
        }
    }

    /// The user picked a session in the switcher: switch the view
    /// optimistically (clear it; the replay pass from the backend's
    /// `open_session` rebuilds it — the full-re-render rule, pi-proven)
    /// and seed liveness from the row. The caller sends the command.
    pub fn open_session(&mut self, id: &str) {
        let row = self.sessions.iter().find(|row| row.id == id);
        let running = row.map(|row| row.running).unwrap_or(false);
        if let Some(row) = self.sessions.iter_mut().find(|row| row.id == id) {
            row.attention = false;
        }
        self.switch_view(id.to_string(), running);
    }

    /// Point the transcript at `id` with a clean slate. One path for
    /// the switcher and `session_created` (a brand-new session is
    /// empty — nothing replays, the clean slate IS its state). Cards
    /// survive the switch — they are per-session live state, never
    /// replay content, and the freshly-viewed session's cards must
    /// render again (a parked permission card would otherwise
    /// deadlock its own replay pass: the pass waits for the run, the
    /// run waits on the now-invisible card).
    fn switch_view(&mut self, id: String, running: bool) {
        self.active = id;
        self.transcript.clear();
        self.pending.clear();
        self.running = running;
    }

    /// The startup catalog: replace the rows, keeping the liveness
    /// this window already knows (background runs and attention flags
    /// survive a re-announcement).
    fn replace_catalog(&mut self, sessions: Vec<tabit_protocol::AvailableSession>) {
        self.sessions = sessions
            .into_iter()
            .map(|session| SessionRow {
                running: self
                    .sessions
                    .iter()
                    .find(|row| row.id == session.id)
                    .map(|row| row.running)
                    .unwrap_or(false),
                attention: self
                    .sessions
                    .iter()
                    .find(|row| row.id == session.id)
                    .map(|row| row.attention)
                    .unwrap_or(false),
                id: session.id,
                created_at: session.created_at,
                entry_count: session.entry_count,
            })
            .collect();
    }

    /// A `new_session` succeeded. Today the only creator is the user's
    /// own command, so the view switches to the fresh session; when
    /// subagent children mint sessions (stage 4), this is the seam
    /// that decides view-stealing versus background rows. The frame
    /// carries the new session's facts — selection included.
    fn session_created(&mut self, id: String, path: String, model: ModelSelection) {
        // A brand-new session: empty (no replay comes), so the switch
        // is complete the moment it happens.
        self.sessions.push(SessionRow {
            id: id.clone(),
            created_at: String::new(),
            entry_count: 0,
            running: false,
            attention: false,
        });
        if let Some(facts) = self.facts.as_mut() {
            facts.session_id = id.clone();
            facts.session_path = path;
            facts.model = model;
        }
        self.switch_view(id, false);
    }

    /// The backend-level fold: unstamped frames are connection facts,
    /// never session-attributed (the optional-stream ruling).
    fn reduce_backend_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::SessionsAvailable { sessions } => self.replace_catalog(sessions),
            SessionEvent::SessionCreated { id, path, model } => {
                self.session_created(id, path, model)
            }
            // Backend-level errors (routing failures, build/listing
            // failures): connection-level notices; there is no session
            // row to mark.
            SessionEvent::Error { message, .. } => self.push_notice(message, true),
            _ => {}
        }
    }

    /// Record a session's run liveness on its switcher row (every
    /// stream, viewed or not — the dot must survive a switch).
    fn mark_running(&mut self, stream: &str, running: bool) {
        if let Some(row) = self.sessions.iter_mut().find(|row| row.id == stream) {
            row.running = running;
        }
    }

    /// Record that a card's response was sent (optimistic close — a
    /// racing terminal already cleared it backend-side; total either
    /// way).
    pub fn interaction_answered(&mut self, id: &str) {
        self.interactions.retain(|card| card.id() != id);
    }

    fn reduce_event(&mut self, frame: EventFrame) {
        // Backend-level events carry no session stamp (the optional-
        // stream ruling): fold them connection-level — never
        // session-attributed. One rule for the catalog, session
        // creation, and backend errors, instead of the old
        // two-type special case.
        let stream = match frame.stream {
            Some(stream) => stream.as_str().to_string(),
            None => return self.reduce_backend_event(frame.event),
        };
        match frame.event {
            // Pass brackets, on any stream: everything between them is
            // history being rebuilt, never liveness (a pass carries
            // `user_message`s but no terminal — an unguarded fold
            // would mark the session running forever; the review
            // round's finding).
            SessionEvent::ReplayStarted { .. } => {
                if !self.replaying.iter().any(|s| s == &stream) {
                    self.replaying.push(stream.clone());
                }
            }
            SessionEvent::ReplayDone => {
                self.replaying.retain(|s| s != &stream);
            }
            _ => {}
        }
        // Run terminals close their session's cards (any stream — a
        // background card dies with its run too).
        if matches!(
            frame.event,
            SessionEvent::RunFinished { .. }
                | SessionEvent::RunAborted { .. }
                | SessionEvent::RunFailed { .. }
        ) {
            self.interactions.retain(|card| card.session() != stream);
        }
        // Liveness rides every run-scoped event, viewed or not —
        // except inside a pass bracket — so the switcher's dot
        // survives switching away mid-run.
        let in_replay = self.replaying.iter().any(|s| s == &stream);
        if !in_replay {
            match &frame.event {
                SessionEvent::UserMessage { .. } => self.mark_running(&stream, true),
                SessionEvent::RunFinished { .. }
                | SessionEvent::RunAborted { .. }
                | SessionEvent::RunFailed { .. } => {
                    self.mark_running(&stream, false);
                }
                _ => {}
            }
        }
        if stream != self.active {
            self.reduce_background(stream, frame.event);
            return;
        }
        match frame.event {
            SessionEvent::UserMessage { text, entry_id } => {
                // Resolve the pending row by id (the exact pairing);
                // fall back to the oldest row carrying this text for
                // sends whose queued notice never existed (idle sends)
                // or raced the local echo. Replayed history (v2) has no
                // pending counterpart at all.
                if let Some(position) = self
                    .pending
                    .iter()
                    .position(|p| p.id.as_deref() == Some(entry_id.as_str()))
                    .or_else(|| {
                        self.pending
                            .iter()
                            .position(|p| p.id.is_none() && p.text == text)
                    })
                {
                    self.pending.remove(position);
                }
                self.transcript.push(Group::User { text, entry_id });
                if !in_replay {
                    self.running = true;
                }
            }
            SessionEvent::MessageQueued { id, text } => {
                // The backend acknowledged a waiting message: upgrade the
                // matching local row to the id. Text pairing is
                // symmetric under duplicates (either assignment drops
                // the right rows); no local row means the GUI did not
                // echo it — track it by id regardless.
                match self
                    .pending
                    .iter()
                    .position(|p| p.id.is_none() && p.text == text)
                {
                    Some(position) => {
                        if let Some(row) = self.pending.get_mut(position) {
                            row.id = Some(id);
                        }
                    }
                    None => {
                        self.pending
                            .push_back(PendingMessage { id: Some(id), text });
                    }
                }
            }
            SessionEvent::MessagesDiscarded { messages } => {
                // Handed back for salvage; the rows leave pending (a
                // future draft box could re-home them — v1 drops).
                for discarded in messages {
                    self.pending
                        .retain(|p| p.id.as_deref() != Some(discarded.id.as_str()));
                }
            }
            SessionEvent::TurnStarted { .. } => {
                // The model call began (before the first token): open the
                // turn's group now so the view can grow it in place.
                let _ = self.turn();
            }
            SessionEvent::TurnCommitted { .. } => {
                // The turn's content is final and recorded; v1 grouping
                // needs no state change (the next `TurnStarted` or terminal
                // closes the group).
            }
            SessionEvent::TextDelta { text, .. } => {
                let turn = self.turn();
                match turn.segments.last_mut() {
                    Some(Segment::Text(buffer)) => buffer.push_str(&text),
                    _ => turn.segments.push(Segment::Text(text)),
                }
            }
            SessionEvent::ReasoningDelta { id, reasoning, .. } => {
                let turn = self.turn();
                match turn.segments.last_mut() {
                    Some(Segment::Reasoning { id: open, text }) if *open == id => {
                        text.push_str(&reasoning)
                    }
                    _ => turn.segments.push(Segment::Reasoning {
                        id,
                        text: reasoning,
                    }),
                }
            }
            SessionEvent::ToolCall {
                name,
                call_id,
                internal_call_id,
                arguments,
                ..
            } => {
                self.turn().segments.push(Segment::ToolCall(ToolCallRow {
                    name,
                    call_id,
                    internal_call_id,
                    arguments,
                    done: false,
                    result: None,
                }));
            }
            SessionEvent::ToolResult {
                internal_call_id,
                content,
                status,
                ..
            } => {
                let failed = !matches!(status, tabit_protocol::ToolResultStatus::Success);
                let turn = self.turn();
                if let Some(Segment::ToolCall(tool)) = turn.segments.iter_mut().find(
                    |s| matches!(s, Segment::ToolCall(t) if t.internal_call_id == internal_call_id),
                ) {
                    tool.done = true;
                    tool.result = Some(ToolResultRow { content, failed });
                }
            }
            SessionEvent::TurnRetried { .. } => {
                // The provisional turn is discarded wholesale; a fresh
                // one starts with the next delta.
                if matches!(self.transcript.last(), Some(Group::Turn(_))) {
                    self.transcript.pop();
                }
            }
            SessionEvent::CompletionCall { .. } => {
                // Per-request usage; the run terminal carries the
                // aggregate. v1 keeps the aggregate only.
            }
            SessionEvent::TurnTruncated { .. } => {
                // Informational (ENGINE.md behavior delta 9): the run
                // continues; the note is the user's cue that the model hit
                // its output cap — a steer asks it to go on.
                self.transcript.push(Group::Notice {
                    text: "model output was truncated (output token limit)".to_string(),
                    error: false,
                });
            }
            SessionEvent::RunFinished { usage, .. } => {
                self.running = false;
                self.usage = add(self.usage, usage);
            }
            SessionEvent::RunAborted { .. } => {
                // Streamed partial text stays visible — the deltas the
                // user watched are the record.
                self.running = false;
                // Abort discards the queued steers (backend flag 6) — they
                // will never be acknowledged, so the pending rows must not
                // linger into the next run's pairing.
                self.pending.clear();
            }
            SessionEvent::RunFailed { message } => {
                self.running = false;
                self.push_notice(message, true);
            }
            SessionEvent::ReplayStarted { .. } => {
                // The structural reset: the pass that follows rebuilds
                // the transcript from committed history, so anything the
                // view held (a switch's optimism included) goes. Cards
                // survive — they are per-session live state, never
                // replay content (FRONTEND.md §8). The run flag is
                // seeded at the switch and suppressed during the pass
                // (the bracket tracking at the top of the fold).
                self.transcript.clear();
                self.pending.clear();
            }
            SessionEvent::ReplayDone => {
                // The pass ended; the transcript is whole (live traffic
                // or quiescence follows).
            }
            SessionEvent::CheckedOut { .. } => {
                // The checkout applied; the replay bracket that follows
                // IS the state change (the same rebuild path as a view
                // switch). No independent fold — a checkout executes at
                // a pause point, so liveness is already settled.
            }
            SessionEvent::ModelChanged {
                provider,
                model,
                thinking_level,
            } => {
                if let Some(facts) = self.facts.as_mut() {
                    facts.model = tabit_protocol::ModelSelection {
                        provider,
                        model,
                        thinking_level,
                    };
                }
            }
            SessionEvent::Error { kind, message, .. } => {
                // One carrier for every non-terminal error condition: a
                // minimal handler just shows the message (the ruling);
                // unknown kinds fall back to the same generic display.
                let _ = kind;
                self.push_notice(message, true);
            }
            SessionEvent::InteractionRequested {
                id,
                ui_type,
                payload,
            } => {
                self.open_card(&stream, id, ui_type, payload);
            }
            SessionEvent::NativeItem { item, .. } => {
                self.transcript.push(Group::Native {
                    item: item.to_string(),
                });
            }
            // Consumed by the backend-level fold; unreachable on a
            // stream path.
            SessionEvent::SessionsAvailable { .. } | SessionEvent::SessionCreated { .. } => {}
        }
    }

    /// A stamped event for a session this window is not viewing.
    /// Liveness is already mirrored (before the split); background
    /// questions join the card list (the view renders the active
    /// stream's — a parked permission card must stay answerable, or
    /// the session deadlocks on its own replay pass), and `error`
    /// conditions must never vanish with the view — they land in the
    /// active transcript and mark the row.
    fn reduce_background(&mut self, stream: String, event: SessionEvent) {
        match event {
            SessionEvent::Error { message, .. } => {
                if let Some(row) = self.sessions.iter_mut().find(|row| row.id == stream) {
                    row.attention = true;
                }
                self.push_notice(message, true);
            }
            SessionEvent::InteractionRequested {
                id,
                ui_type,
                payload,
            } => {
                if let Some(row) = self.sessions.iter_mut().find(|row| row.id == stream) {
                    row.attention = true;
                }
                self.open_card(&stream, id, ui_type, payload);
            }
            _ => {}
        }
    }

    /// Open a card for an interaction request, typed by its template:
    /// `native:*` parse into cards; anything else surfaces as a notice
    /// (report-don't-swallow) — this frontend cannot construct the
    /// answer its widget would want.
    fn open_card(&mut self, stream: &str, id: String, ui_type: String, payload: serde_json::Value) {
        use tabit_protocol::templates;
        match ui_type.as_str() {
            templates::ui::SELECT_ONE => {
                match serde_json::from_value::<templates::SelectOneCard>(payload) {
                    Ok(card) => self.interactions.push(InteractionCard::Confirm {
                        session: stream.to_string(),
                        id,
                        title: card.title,
                        body: card.body,
                        options: card.options.into_iter().map(|o| o.label).collect(),
                        free_text: card.free_text,
                    }),
                    Err(_) => self.push_notice(
                        format!("a {ui_type} card arrived in a shape this frontend cannot read"),
                        true,
                    ),
                }
            }
            templates::ui::SELECT_ANY => {
                match serde_json::from_value::<templates::SelectAnyCard>(payload) {
                    Ok(card) => self.interactions.push(InteractionCard::Ask {
                        session: stream.to_string(),
                        id,
                        prompt: card.body,
                    }),
                    Err(_) => self.push_notice(
                        format!("a {ui_type} card arrived in a shape this frontend cannot read"),
                        true,
                    ),
                }
            }
            other => self.push_notice(
                format!("unsupported interaction widget `{other}` — not answered"),
                true,
            ),
        }
    }

    /// The current (trailing) turn, creating it if the last group
    /// isn't a turn.
    fn turn(&mut self) -> &mut TurnGroup {
        if !matches!(self.transcript.last(), Some(Group::Turn(_))) {
            self.transcript.push(Group::Turn(TurnGroup::default()));
        }
        // Sanctioned crash: the branch above just pushed a turn.
        #[allow(clippy::unreachable)]
        let Some(Group::Turn(turn)) = self.transcript.last_mut() else {
            unreachable!("just pushed a turn")
        };
        turn
    }

    fn push_notice(&mut self, text: String, error: bool) {
        self.transcript.push(Group::Notice { text, error });
    }
}

fn add(a: Usage, b: Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens + b.input_tokens,
        output_tokens: a.output_tokens + b.output_tokens,
        total_tokens: a.total_tokens + b.total_tokens,
        cached_input_tokens: a.cached_input_tokens + b.cached_input_tokens,
        cache_creation_input_tokens: a.cache_creation_input_tokens + b.cache_creation_input_tokens,
    }
}

#[cfg(test)]
#[path = "reducer_tests.rs"]
mod tests;
