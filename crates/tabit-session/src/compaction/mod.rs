//! The compaction box (ROADMAP item 6, the 2026-09 rulings; the flow
//! facts live in ENGINE.md's compaction amendment). Its own system, a
//! black box with three doors — **pre-request** (mid-run, condition
//! B), **idle** (the beat, A ∨ B), **manual** (the `compact` command,
//! forced) — plus the overflow intercept's door (forced; the error
//! taught the window before it knocked). The engine has zero
//! compaction knowledge: the pre-request door is an opaque
//! [`PreRequestSource`] leaf the loop awaits.
//!
//! Every dial and prompt text lives in [`dials`] — data, clustered
//! for review. The pass shape: cut selection is a maximization (the
//! longest prefix satisfying both hard constraints — sent history
//! under the [`dials::SENT_PREFIX_FRACTION`] cap, retained tail at or
//! above [`dials::KEEP_TAIL_TOKENS`]); the request is that prefix
//! plus the instruction, riding the conversation's preamble and
//! toolset verbatim (prefix-cache identity — any toolset change
//! diverges the cached prefix); tool calls are forbidden by the
//! instruction and a violating response fails the pass (nothing ever
//! executes); overflow rejections and length-capped summaries shorten
//! the request one boundary and retry, floored at the empty prefix;
//! the post-check loop re-runs while the context is still over
//! condition B — pass N+1 is just another regular compaction over a
//! history that already begins with pass N's summary. A cancelled or
//! failed pass persists nothing; the entry write happens only at pass
//! end.

mod dials;

#[cfg(test)]
#[path = "box_tests.rs"]
mod tests;

use crate::entry::{EntryKind, SessionEntry};
use crate::lock::{read, write};
use rig_agent::agent::{Agent, AttemptOutcome, PreRequestSource};
use rig_core::completion::{CompletionError, Message, Usage};
use rig_core::streaming::StreamedAssistantContent;
use std::sync::Arc;
use tabit_config::TabitConfig;
use tabit_log::ConversationCell;
use tabit_protocol::{ModelSelection, SessionEvent};
use tokio_util::sync::CancellationToken;

/// The box's session-persistent state: what survives across doors.
/// The pass logic is stateless over it.
pub(crate) struct Compaction {
    /// The window the wall taught (an overflow error's report
    /// outranks config — it is the fresher measurement). Session-
    /// scoped; nothing persists it.
    window_cache: std::sync::Mutex<Option<u64>>,
}

impl Compaction {
    pub(super) fn new() -> Self {
        Self {
            window_cache: std::sync::Mutex::new(None),
        }
    }

    /// The wall's lesson: an overflow error reported the real window.
    pub(crate) fn note_window(&self, window: u64) {
        *crate::lock::lock(&self.window_cache) = Some(window);
    }

    fn taught_window(&self) -> Option<u64> {
        *crate::lock::lock(&self.window_cache)
    }
}

/// Which door is asking. The door selects the trigger formula; the
/// box owns the evaluation (callers carry no policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Door {
    /// Mid-run, the point the run is about to send a request —
    /// condition B only (safety; fires mid-task only when genuinely
    /// close).
    PreRequest,
    /// The beat, after the pump arm — A ∨ B (A carries the
    /// mailbox-empty requirement).
    Idle,
    /// The `compact` command — forced; the short-history skip is its
    /// only guard.
    Manual,
    /// The run epilogue's intercept — forced, and it arrives with the
    /// window the error itself reported (noted on the state before
    /// the door runs).
    Overflow,
}

/// What one door invocation did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outcome {
    /// At least one pass committed; the context estimate afterwards
    /// fit condition B (or the pass cap applied).
    Compacted { passes: u32, tokens_after: u64 },
    /// The door's conditions did not hold, the window is unknown, or
    /// nothing feasible exists — nothing ran.
    Skipped,
    /// A pass failed (possibly after earlier passes committed —
    /// `passes` says which landed). Nothing from the failing pass
    /// persisted.
    Failed { message: String, passes: u32 },
    /// Aborted mid-pass. Nothing from this pass persisted.
    Cancelled { passes: u32 },
}

/// One selected cut: the boundary index into the active branch (the
/// first retained entry), with the estimates the constraints fed on.
#[derive(Debug, Clone, PartialEq)]
struct Cut {
    boundary: usize,
}

impl Cut {
    #[allow(clippy::indexing_slicing)] // sanctioned crash: the boundary is a validated index into this branch
    fn cut_child<'a>(&self, branch: &'a [SessionEntry]) -> &'a str {
        &branch[self.boundary].id
    }
}

/// Whether `index` (the first retained entry) sits at a valid
/// boundary: the prefix ends at a model output without tool calls (a
/// run end, a prior compaction) or at the session start. A boundary
/// after a tool-carrying output never exists — tool pairs stay whole
/// and queued-steer clusters stay in the tail by construction.
#[allow(clippy::indexing_slicing)] // sanctioned crash: callers pass in-range indices (0..branch.len())
fn valid_boundary(branch: &[SessionEntry], index: usize) -> bool {
    index == 0
        || match &branch[index - 1].kind {
            EntryKind::Compaction { .. } => true,
            EntryKind::AssistantMessage { message, .. } => tabit_log::calls_of(message).is_empty(),
            EntryKind::UserMessage { .. } | EntryKind::ToolResult { .. } => false,
        }
}

/// Run the box for one door invocation. `emit` receives the
/// compaction bracket events (the caller's channel discipline — the
/// tap live, a collector in tests).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    door: Door,
    cell: &ConversationCell,
    state: &Compaction,
    agent: &Agent,
    token: &CancellationToken,
    config: &TabitConfig,
    selection: &ModelSelection,
    preamble_chars: u64,
    mailbox_empty: bool,
    emit: &mut (dyn FnMut(SessionEvent) + Send),
) -> Outcome {
    let preamble_tokens = preamble_chars / dials::CHARS_PER_TOKEN;
    let branch = read(cell).active_branch();
    // The live context estimate: the value each pass starts from (the
    // entry's `tokens_before`), and — after the post-pass update —
    // the latest measurement the exits report as `tokens_after`.
    let mut tokens_now = context_tokens(&branch, preamble_tokens);
    // Every designed constraint needs a known window. Unknown means
    // the threshold doors skip with a warning — and the overflow
    // door's caller noted the wall's lesson before knocking, so an
    // unknown window there means the error carried no number and the
    // constraints genuinely cannot size.
    let Some(window) = resolve_window(state, config, selection) else {
        tracing::warn!(
            provider = %selection.provider,
            model = %selection.model,
            "compaction skipped: no context_window configured for the model \
             (set it in providers.toml, or let an overflow error teach it)"
        );
        return Outcome::Skipped;
    };
    if !fires(door, tokens_now, window, mailbox_empty) {
        return Outcome::Skipped;
    }
    let mut passes: u32 = 0;
    // Bounded retries of a tool-call-violating response (owner ruling:
    // throw the response away and resend — never synthesize an in-band
    // error result; each discard closes its bracket as failed so the
    // frontend drops that attempt's deltas).
    let mut violation_retries: u32 = 0;
    loop {
        let branch = read(cell).active_branch();
        let Some(cut) = select_cut(&branch, window, preamble_tokens) else {
            return match (passes, door) {
                (0, Door::Manual) => Outcome::Failed {
                    message: format!(
                        "nothing to compact: the history is shorter than the \
                         retained-tail budget ({} tokens)",
                        dials::KEEP_TAIL_TOKENS
                    ),
                    passes: 0,
                },
                // The automatic doors are silent about a skip; after
                // committed passes there is simply nothing more
                // feasible — what landed stands.
                (0, _) => Outcome::Skipped,
                (_, _) => Outcome::Compacted {
                    passes,
                    tokens_after: tokens_now,
                },
            };
        };
        let pass = passes + 1;
        let id = crate::ids::new_entry_id();
        emit(SessionEvent::CompactionStarted {
            id: id.clone(),
            pass,
        });
        match one_pass(&branch, cut.clone(), state, agent, token, &id, emit).await {
            PassOutcome::Committed { summary, usage } => {
                write(cell).commit_compaction(
                    id.clone(),
                    summary,
                    cut.cut_child(&branch).to_string(),
                    tokens_now,
                    usage,
                );
                emit(SessionEvent::CompactionFinished { id });
                passes = pass;
            }
            PassOutcome::Cancelled => {
                emit(SessionEvent::CompactionFailed {
                    id,
                    message: "cancelled".to_string(),
                });
                return Outcome::Cancelled { passes };
            }
            // A violating response is discarded and the request resent
            // (bounded): sampling variance usually corrects a one-off
            // tool call; a model that insists fails the pass. The
            // failed bracket announces the discard — the retry opens a
            // fresh one.
            PassOutcome::Violated => {
                emit(SessionEvent::CompactionFailed {
                    id,
                    message: "the summarizer attempted a tool call — the response \
                              is discarded and the request retried"
                        .to_string(),
                });
                if violation_retries < dials::VIOLATION_RETRY_CAP {
                    violation_retries += 1;
                    continue;
                }
                return Outcome::Failed {
                    message: "the summarizer attempted a tool call on every \
                              attempt — compaction state rejects every tool call"
                        .to_string(),
                    passes,
                };
            }
            PassOutcome::Failed { message } => {
                emit(SessionEvent::CompactionFailed {
                    id,
                    message: message.clone(),
                });
                return Outcome::Failed { message, passes };
            }
        }
        // The post-check loop: rerun while the context is still over
        // condition B — pass N+1 is just another regular compaction
        // over a history that already begins with pass N's summary.
        let branch = read(cell).active_branch();
        let tokens_after = context_tokens(&branch, preamble_tokens);
        if passes >= dials::MAX_PASSES || tokens_after + dials::URGENT_RESERVE_TOKENS <= window {
            return Outcome::Compacted {
                passes,
                tokens_after,
            };
        }
        if tokens_after >= tokens_now {
            // The cannot-shrink guard: a pass that committed without
            // shrinking the estimate would spin the loop forever —
            // stop loud, with the passes that did land left in place.
            return Outcome::Failed {
                message: format!(
                    "compaction cannot shrink the context further ({tokens_after} estimated \
                     tokens against a {window}-token window): a single entry may exceed the \
                     retained-tail budget, or the window is misreported"
                ),
                passes,
            };
        }
        tokens_now = tokens_after;
    }
}

/// The trigger formulas (ruled): A = `> 75% ∧ mailbox empty`; B =
/// `> window − 32K`. Idle checks A ∨ B; pre-request checks B; manual
/// and overflow are forced.
fn fires(door: Door, context_tokens: u64, window: u64, mailbox_empty: bool) -> bool {
    let over_urgent = context_tokens + dials::URGENT_RESERVE_TOKENS > window;
    match door {
        Door::PreRequest => over_urgent,
        Door::Idle => {
            over_urgent
                || (mailbox_empty && context_tokens as f64 > dials::IDLE_FRACTION * window as f64)
        }
        Door::Manual | Door::Overflow => true,
    }
}

/// The context measurement: the newest server-reported request total
/// on the branch, plus estimated tokens for the entries appended
/// after it. Every assistant entry carries the usage its provider
/// reported (the engine's commit folds it in); `total_tokens` is each
/// provider's correct partition of everything that request processed
/// — Anthropic sums input + both cache counters + output (its
/// `input_tokens` excludes cache), OpenAI passes the wire total (its
/// prompt figure already includes cached), so summing the components
/// here would double-count on one side of that split. Zeros mean "not
/// reported" (the type's own sentinel): the walk passes such entries
/// by, estimating them, and falls back to the full estimate when no
/// turn ever measured the branch. A compaction entry ends the walk —
/// every measurement before it measured a history the summary
/// replaced.
fn context_tokens(branch: &[SessionEntry], preamble_tokens: u64) -> u64 {
    let mut tail = 0;
    for entry in branch.iter().rev() {
        match &entry.kind {
            EntryKind::AssistantMessage { usage, .. } if usage.total_tokens > 0 => {
                return usage.total_tokens + tail;
            }
            EntryKind::Compaction { .. } => {
                return preamble_tokens + tail + estimate_entry(entry);
            }
            _ => tail += estimate_entry(entry),
        }
    }
    preamble_tokens + tail
}

/// The window: the wall-taught value (fresher than config) else the
/// configured `context_window`.
fn resolve_window(
    state: &Compaction,
    config: &TabitConfig,
    selection: &ModelSelection,
) -> Option<u64> {
    state.taught_window().or_else(|| {
        config
            .provider(&selection.provider)
            .and_then(|provider| provider.model(&selection.model))
            .and_then(|model| model.context_window)
    })
}

/// Cut selection — the maximization: the **latest** boundary
/// satisfying both hard constraints. Both push the cut the same
/// direction (a later cut means a longer prefix AND a shorter tail);
/// the longest-prefix objective is the soft pull the other way —
/// compaction efficiency. The session-start boundary is the
/// always-feasible floor (nothing sent, the whole history kept).
#[allow(clippy::indexing_slicing)] // sanctioned crash: prefix_sums carries branch.len()+1 sums by construction
fn select_cut(branch: &[SessionEntry], window: u64, preamble_tokens: u64) -> Option<Cut> {
    if branch.is_empty() {
        return None;
    }
    let mut prefix_sums = Vec::with_capacity(branch.len() + 1);
    let mut total = preamble_tokens;
    prefix_sums.push(total);
    for entry in branch {
        total += estimate_entry(entry);
        prefix_sums.push(total);
    }
    let cap = (dials::SENT_PREFIX_FRACTION * window as f64) as u64;
    let all_tokens = prefix_sums[branch.len()];
    // The session-start boundary is the feasibility floor, never a
    // selection: an empty prefix summarizes nothing (the owner's
    // "just not making progress") and its insertion would parent no
    // node. The loop starts at 1.
    for index in (1..branch.len()).rev() {
        if !valid_boundary(branch, index) {
            continue;
        }
        let prefix_tokens = prefix_sums[index];
        let tail_tokens = all_tokens - prefix_tokens;
        if prefix_tokens < cap && tail_tokens >= dials::KEEP_TAIL_TOKENS {
            return Some(Cut { boundary: index });
        }
    }
    None
}

/// One entry's token estimate: serialized chars /
/// [`dials::CHARS_PER_TOKEN`] — the heuristic every reference uses.
/// Only the unmeasured needs it: the tail after the newest
/// measurement, cut-selection arithmetic, and branches no server ever
/// measured (seeds, zero-usage reports).
fn estimate_entry(entry: &SessionEntry) -> u64 {
    #[allow(clippy::expect_used)] // sanctioned crash: log payloads always serialize
    fn json_tokens(value: &impl serde::Serialize) -> u64 {
        serde_json::to_string(value)
            .expect("a log payload that cannot serialize could not have been written")
            .len() as u64
            / dials::CHARS_PER_TOKEN
    }
    match &entry.kind {
        EntryKind::UserMessage { message } => json_tokens(message),
        EntryKind::AssistantMessage { message, .. } => json_tokens(message),
        EntryKind::ToolResult { result } => json_tokens(result),
        EntryKind::Compaction { summary, .. } => summary.len() as u64 / dials::CHARS_PER_TOKEN,
    }
}

/// One pass's stream outcome.
enum PassOutcome {
    Committed {
        summary: String,
        usage: Usage,
    },
    /// The model attempted a tool call: the response is discarded; the
    /// caller decides whether to resend (bounded) or fail.
    Violated,
    Failed {
        message: String,
    },
    Cancelled,
}

/// One pass: the request is the walked prefix plus the instruction,
/// consumed and classified through the common path
/// ([`Agent::completion_turn`] — the engine's assembly, one exposed
/// consumer). The pass's own policy is all that remains here: the
/// violation verdicts, the length-cap/overflow shortening, the
/// empty-summary guard. The rejection/length-cap retry loop lives
/// here — each retry moves the cut one boundary earlier (a strictly
/// shorter request), floored at the empty prefix.
#[allow(clippy::indexing_slicing)] // sanctioned crash: the boundary is a validated index into this branch
async fn one_pass(
    branch: &[SessionEntry],
    initial_cut: Cut,
    state: &Compaction,
    agent: &Agent,
    token: &CancellationToken,
    id: &str,
    emit: &mut (dyn FnMut(SessionEvent) + Send),
) -> PassOutcome {
    let mut boundary = initial_cut.boundary;
    loop {
        let mut history = tabit_log::fold_branch(&branch[..boundary]);
        history.push(Message::user(dials::SUMMARIZATION_INSTRUCTION));
        // The live view: summary text streams as bracket deltas. Tool
        // call items pass through here too — the verdict on them is
        // the assembled classification below (the common predicate),
        // never this forwarding.
        let bracket_id = id.to_string();
        let outcome = agent
            .completion_turn(
                history,
                Some(dials::SUMMARY_MAX_TOKENS),
                token.cancelled(),
                &mut |item| {
                    if let StreamedAssistantContent::Text(delta) = item {
                        emit(SessionEvent::CompactionDelta {
                            id: bracket_id.clone(),
                            text: delta.text,
                        });
                    }
                },
            )
            .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            // The request itself failed to build or open: classify the
            // same way as an in-stream failure.
            Err(error) => {
                return match rejected(error, state, boundary, branch) {
                    Rejection::Shorten(shortened) => {
                        boundary = shortened;
                        continue;
                    }
                    Rejection::Fail(message) => PassOutcome::Failed { message },
                };
            }
        };
        match outcome {
            AttemptOutcome::Cancelled => return PassOutcome::Cancelled,
            // A broken tool call is still an attempted tool call — the
            // same violation verdict (the response is discarded and
            // retried; nothing executes either way).
            AttemptOutcome::MalformedToolCall { .. } => return PassOutcome::Violated,
            AttemptOutcome::Failed(error) => {
                return match rejected(error, state, boundary, branch) {
                    Rejection::Shorten(shortened) => {
                        boundary = shortened;
                        continue;
                    }
                    Rejection::Fail(message) => PassOutcome::Failed { message },
                };
            }
            AttemptOutcome::Completed {
                turn,
                finish_reason,
            } => {
                // The canonical predicate: tools offered, nothing
                // executed — a tool-carrying response fails the pass
                // (the caller's bounded discard-and-retry handles it).
                if turn.carries_tools() {
                    return PassOutcome::Violated;
                }
                // A length-capped summary is protocol-complete but
                // information-incomplete: it could not fit what the
                // prefix contained — treated exactly like a rejection
                // (ruled).
                if finish_reason == Some(rig_core::completion::FinishReason::Length) {
                    match shorten(branch, boundary) {
                        Some(shortened) => {
                            boundary = shortened;
                            continue;
                        }
                        None => {
                            return PassOutcome::Failed {
                                message: "the summary hit the output cap even at the \
                                          shortest prefix — raise the model's output \
                                          limit or shrink the retained tail"
                                    .to_string(),
                            };
                        }
                    }
                }
                let summary = assistant_text(&turn);
                if summary.trim().is_empty() {
                    return PassOutcome::Failed {
                        message: "the summarizer returned an empty summary".to_string(),
                    };
                }
                return PassOutcome::Committed {
                    summary,
                    usage: turn.usage,
                };
            }
        }
    }
}

/// The assembled turn's text (canonical order puts all text ahead of
/// any trailing items; the concatenation covers non-canonical shapes
/// too).
fn assistant_text(turn: &rig_agent::agent::ModelTurn) -> String {
    use rig_core::message::AssistantContent;
    turn.choice
        .iter()
        .filter_map(|item| match item {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

/// What a request-level failure means for the retry loop.
#[derive(Debug)]
enum Rejection {
    /// A strictly earlier boundary to resend from (`shorten`'s result
    /// is below `from` by construction).
    Shorten(usize),
    /// The pass fails; nothing persists.
    Fail(String),
}

/// Classify a request-level failure: an overflow rejection teaches
/// the window and shortens; everything else fails the pass.
fn rejected(
    error: CompletionError,
    state: &Compaction,
    boundary: usize,
    branch: &[SessionEntry],
) -> Rejection {
    match error.as_context_overflow() {
        Some(overflow) => {
            // The wall teaches the window — the lesson serves the rest
            // of the session (the triggers, the next doors).
            if let Some(window) = overflow.window_tokens {
                state.note_window(window);
            }
            match shorten(branch, boundary) {
                Some(shortened) => Rejection::Shorten(shortened),
                None => Rejection::Fail(
                    "the compaction request overflows the context window even with \
                     an empty prefix — a single entry exceeds the window"
                        .to_string(),
                ),
            }
        }
        None => Rejection::Fail(error.to_string()),
    }
}

/// The latest valid boundary strictly before `from`, if any.
fn shorten(branch: &[SessionEntry], from: usize) -> Option<usize> {
    (0..from).rev().find(|index| valid_boundary(branch, *index))
}

/// The pre-request door leaf: the opaque async callable the engine
/// awaits between DECIDE and PREPARE. Carries everything the box
/// needs at the point of use — the agent and selection snapshotted at
/// run open (a mid-run model switch reaches the next run's doors, the
/// same snapshot rule the subagent capability keeps). Abort drops the
/// run future and with it this leaf's interior — the stream dies by
/// drop, nothing persists.
pub(crate) struct PreRequestDoor {
    pub(crate) cell: ConversationCell,
    pub(crate) state: Arc<Compaction>,
    pub(crate) agent: Arc<Agent>,
    pub(crate) config: Arc<TabitConfig>,
    pub(crate) selection: ModelSelection,
    pub(crate) preamble_chars: u64,
    pub(crate) token: CancellationToken,
    /// The frontend channel's weak, pre-stamped handle — `None` for a
    /// session with no host attached (a direct consumer); the bracket
    /// drops, the compaction still runs.
    pub(crate) notice: Option<crate::notice::NoticeSink>,
}

impl PreRequestSource for PreRequestDoor {
    fn at_door(&self) -> rig_core::wasm_compat::WasmBoxedFuture<'_, ()> {
        Box::pin(async move {
            let notice = self.notice.clone();
            let mut emit = move |event: SessionEvent| {
                if let Some(notice) = &notice {
                    notice.emit(event);
                }
            };
            let _ = run(
                Door::PreRequest,
                &self.cell,
                &self.state,
                &self.agent,
                &self.token,
                &self.config,
                &self.selection,
                self.preamble_chars,
                // Condition B carries no mailbox requirement (urgent
                // is urgent); the queue defers to compaction by
                // ordering alone — the drain sits at the loop's
                // CONVERGE either side of this door.
                false,
                &mut emit,
            )
            .await;
        })
    }
}
