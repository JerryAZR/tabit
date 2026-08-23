//! The GUI's state machine: a pure fold over backend messages.
//!
//! This module is deliberately framework-free (no egui types) and
//! unit-tested — the ROADMAP "GUI design contract": the view is a
//! projection of [`GuiState`], business logic never lives in it, and a
//! future framework switch rewrites only the view layer. v1-wire
//! caveats are marked `v2:` where the protocol's next version removes
//! the heuristic.

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

/// One tool call the model issued, and whether its result arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallRow {
    pub name: String,
    pub call_id: String,
    pub internal_call_id: String,
    pub arguments: Option<String>,
    pub done: bool,
}

/// One open interaction card (FRONTEND.md §8): a permission gate or an
/// ask-the-user body waiting for the user. Several may be open at once;
/// run terminals close them all (the closing rule — the backend has no
/// close event and needs none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionCard {
    pub id: String,
    pub title: String,
    pub body: String,
    /// Button labels.
    pub options: Vec<String>,
    /// Whether a free-text answer/explanation is invited.
    pub free_text: bool,
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
    /// A user message that entered history.
    User { text: String },
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
    pub transcript: Vec<Group>,
    /// Steers sent mid-run, waiting for the turn boundary — resolved
    /// exactly by id: `message_queued` announces the id at submit,
    /// `user_message`/`messages_discarded` carrying it drops the row.
    /// Idle sends never enter it (they drain immediately;
    /// `user_message` is the acknowledgment).
    pub pending: VecDeque<PendingMessage>,
    /// True from the first `user_message` of a run to its terminal.
    pub running: bool,
    /// Open interaction cards, oldest first.
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
            transcript: Vec::new(),
            pending: VecDeque::new(),
            running: false,
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
                    session_id,
                    session_path,
                    model,
                    resumed,
                });
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

    /// Record that a card's response was sent (optimistic close — a
    /// racing terminal already cleared it backend-side; total either
    /// way).
    pub fn interaction_answered(&mut self, id: &str) {
        self.interactions.retain(|card| card.id != id);
    }

    fn reduce_event(&mut self, frame: EventFrame) {
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
                self.transcript.push(Group::User { text });
                self.running = true;
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
                }));
            }
            SessionEvent::ToolResult {
                internal_call_id, ..
            } => {
                let turn = self.turn();
                if let Some(Segment::ToolCall(tool)) = turn.segments.iter_mut().find(
                    |s| matches!(s, Segment::ToolCall(t) if t.internal_call_id == internal_call_id),
                ) {
                    tool.done = true;
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
                self.interactions.clear();
                self.usage = add(self.usage, usage);
            }
            SessionEvent::RunAborted { .. } => {
                // Streamed partial text stays visible — the deltas the
                // user watched are the record.
                self.running = false;
                self.interactions.clear();
                // Abort discards the queued steers (backend flag 6) — they
                // will never be acknowledged, so the pending rows must not
                // linger into the next run's pairing.
                self.pending.clear();
            }
            SessionEvent::RunFailed { message } => {
                self.running = false;
                self.interactions.clear();
                self.push_notice(message, true);
            }
            SessionEvent::ReplayStarted { .. } | SessionEvent::ReplayDone => {
                // The pass's brackets: the replayed events between them
                // flow through the arms above — that is the point (one
                // set of arms renders history and live turns).
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
                title,
                body,
                options,
                free_text,
            } => {
                self.interactions.push(InteractionCard {
                    id,
                    title,
                    body,
                    options: options.into_iter().map(|o| o.label).collect(),
                    free_text,
                });
            }
            SessionEvent::NativeItem { item, .. } => {
                self.transcript.push(Group::Native {
                    item: item.to_string(),
                });
            }
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
