//! The compaction box (ROADMAP item 6, the 2026-09 rulings; the flow
//! facts live in ENGINE.md's compaction amendment). Its own system, a
//! black box with three doors — **pre-request** (mid-run, condition
//! B), **idle** (the beat, A ∨ B), **manual** (the `compact` command,
//! forced) — plus the overflow intercept's door (forced; the error
//! taught the window before it knocked). The engine has zero
//! compaction knowledge: the pre-request door is an opaque
//! [`PreRequestSource`] leaf the loop awaits.
//!
//! Measurement (owner ruling 2026-09): **nothing is estimated.** The
//! context size is the history view's measured total — the nearest
//! measurement-bearing node at-or-before the head (an assistant's
//! `total_tokens`, or the leading compaction node's `tokens_after`
//! in the post-compaction window). Every measured turn commits a
//! `delta_tokens` fact (`total[k] − total[k−1]`, predecessor 0 at
//! session start, the compaction node's base at a regime boundary;
//! the system prompt folds into each regime's first delta, measured)
//! — client-added text (results, user messages) rides the following
//! assistant's delta, unmeasured stretches are simply **uncounted**
//! (the error budget: one-or-a-bounded-few entries off by a few K
//! per compaction is fine; one entry off by half a window is not;
//! everything off by a few percent is not — so no chars/4 anywhere
//! in the decision path).
//!
//! Every dial and prompt text lives in [`dials`] — data, clustered
//! for review. The pass shape: cut selection is a maximization (the
//! latest boundary satisfying both hard constraints — the
//! summarization prefix `head_total − tail` under the
//! [`dials::SENT_PREFIX_FRACTION`] cap, the retained tail's delta sum
//! at or above [`dials::KEEP_TAIL_TOKENS`]); the request is that
//! prefix plus the instruction, riding the conversation's preamble
//! and toolset verbatim (prefix-cache identity — any toolset change
//! diverges the cached prefix); tool calls are forbidden by the
//! instruction and a violating response fails the pass (nothing ever
//! executes); overflow rejections and length-capped summaries shorten
//! the request one boundary and retry, floored at the empty prefix;
//! the committed node is a leaf-append whose `tokens_after` (retained
//! tail + the summary's own output tokens) is the new regime's base;
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
/// What one door invocation did. Two orthogonal facts, each variant
/// stating both (owner ruling 2026-09): **did compaction happen**
/// (≥1 pass committed) and **is the context good to continue with**
/// (fits the urgent bound, or was fine to begin with).
pub(crate) enum Outcome {
    /// Compaction happened, good to continue: at least one pass
    /// committed and the context now fits condition B.
    Compacted { passes: u32, tokens_after: u64 },
    /// The door declined to run (conditions did not hold, the window
    /// is unknown or below the support envelope). Compaction did not
    /// happen — good to continue as-is.
    Skipped,
    /// No feasible cut exists: nothing worth folding (a history
    /// shorter than the kept-tail budget). Compaction did not happen
    /// — good to continue as-is. The manual door reports this
    /// benignly; it is not a failure.
    NothingToCompact,
    /// Compaction happened (the passes named landed), but the context
    /// is **still over the urgent bound** — not good to continue
    /// without further compaction. The cannot-shrink guard, the pass
    /// cap, or (unreachable by construction) no further feasible cut.
    Oversized {
        reason: String,
        passes: u32,
        tokens_after: u64,
    },
    /// A pass errored (model failure, violation cap exhausted);
    /// earlier passes' commits stand, the failing pass persisted
    /// nothing.
    Failed { message: String, passes: u32 },
    /// Aborted mid-pass. Nothing from this pass persisted.
    Cancelled { passes: u32 },
}

/// The first retained entry's id at a boundary — a selection's own,
/// or a shortened retry's.
#[allow(clippy::indexing_slicing)] // sanctioned crash: boundaries are validated indices into this view
fn cut_child_of(history: &[SessionEntry], boundary: usize) -> &str {
    &history[boundary].id
}

/// Whether `index` (the first retained entry) sits at a valid
/// boundary: the prefix ends at a model output without tool calls (a
/// run end, a prior compaction) or at the session start. A boundary
/// after a tool-carrying output never exists — tool pairs stay whole
/// and queued-steer clusters stay in the tail by construction.
#[allow(clippy::indexing_slicing)] // sanctioned crash: callers pass in-range indices (0..history.len())
fn valid_boundary(history: &[SessionEntry], index: usize) -> bool {
    index == 0
        || match &history[index - 1].kind {
            EntryKind::Compaction { .. } => true,
            EntryKind::AssistantMessage { message, .. } => tabit_log::calls_of(message).is_empty(),
            EntryKind::UserMessage { .. } | EntryKind::ToolResult { .. } => false,
        }
}

/// The suffix delta sums of a history view: `sums[i]` is the
/// retained-tail size when the boundary is `i` — every assistant
/// entry at index ≥ i contributing its measured `delta_tokens`
/// (absent deltas are unmeasured turns: uncounted, never estimated).
/// Non-assistant entries ride their following assistant's delta by
/// construction, so they never sum separately.
fn delta_suffix_sums(history: &[SessionEntry]) -> Vec<u64> {
    let mut sums = Vec::with_capacity(history.len() + 1);
    sums.push(0);
    let mut running: u64 = 0;
    for entry in history.iter().rev() {
        if let EntryKind::AssistantMessage {
            delta_tokens: Some(delta),
            ..
        } = &entry.kind
        {
            running += delta;
        }
        sums.push(running);
    }
    sums.reverse();
    sums
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
    mailbox_empty: bool,
    emit: &mut (dyn FnMut(SessionEvent) + Send),
) -> Outcome {
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
    // The declared envelope: below it the constraints contradict (B
    // demands a context the kept-tail floor forbids), so the door
    // declines before burning a pass that provably cannot satisfy the
    // post-check — the loud statement of what we support.
    if window < dials::MIN_SUPPORTED_WINDOW {
        tracing::warn!(
            provider = %selection.provider,
            model = %selection.model,
            window,
            minimum = dials::MIN_SUPPORTED_WINDOW,
            "compaction skipped: the context window is below the supported \
             envelope ({} tokens) — the urgent bound is unsatisfiable by \
             construction below it",
            dials::MIN_SUPPORTED_WINDOW
        );
        return Outcome::Skipped;
    }
    // The live context measurement (the head node's total). An
    // unmeasured context — a fresh session, a provider that never
    // reports usage — is absence, not an estimate (the unknown-window
    // skip's sibling).
    let Some(mut tokens_now) = read(cell).measured_total() else {
        tracing::warn!(
            "compaction skipped: the context has no measurement yet — no turn \
             on the history reported usage"
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
        let history = read(cell).history();
        let tail_sums = delta_suffix_sums(&history);
        let Some(boundary) = select_cut(&history, &tail_sums, tokens_now, window) else {
            // Nothing worth folding is benign for every door — the
            // manual command reports it as a friendly note, the
            // automatic doors are silent about it. After committed
            // passes there is simply nothing more feasible — which
            // (unreachable by construction: the view always re-offers
            // the previous compaction as a feasible boundary) would
            // mean the loop gave up while still over condition B.
            return match passes {
                0 => Outcome::NothingToCompact,
                more => Outcome::Oversized {
                    reason: "no further feasible cut with the context still over \
                             the urgent bound"
                        .to_string(),
                    passes: more,
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
        match one_pass(&history, boundary, state, agent, token, &id, emit).await {
            PassOutcome::Committed {
                summary,
                usage,
                boundary,
            } => {
                // The regime's base, persisted once: the retained
                // tail's delta sum plus the summary's own measured
                // output. (The boundary is the pass's final one — a
                // shortened retry may have moved it off the selection.)
                #[allow(clippy::indexing_slicing)]
                // sanctioned crash: the pass validated this boundary against this view
                let tokens_after = tail_sums[boundary] + usage.output_tokens;
                write(cell).commit_compaction(
                    id.clone(),
                    summary,
                    cut_child_of(&history, boundary).to_string(),
                    tokens_now,
                    tokens_after,
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
        // The head is the fresh compaction node: this read is exactly
        // its persisted base.
        #[allow(clippy::expect_used)]
        // sanctioned crash: the commit one step above wrote this measurement
        let tokens_after = read(cell)
            .measured_total()
            .expect("the compaction node just committed carries the regime's base");
        // The primary exit: the context now fits the urgent bound —
        // compaction happened, good to continue.
        if tokens_after + dials::URGENT_RESERVE_TOKENS <= window {
            return Outcome::Compacted {
                passes,
                tokens_after,
            };
        }
        // The pass cap: compaction happened, but the context is still
        // oversized — the legitimate big one is a model switch
        // importing a larger regime's history (see the dial); the
        // pathological one is measurement ping-pong the guard's `>=`
        // cannot see. What landed stands; not good to continue.
        if passes >= dials::MAX_PASSES {
            return Outcome::Oversized {
                reason: format!(
                    "the pass cap ({}) reached with the context still over the \
                     urgent bound ({tokens_after} measured tokens against a \
                     {window}-token window): a model switch importing a larger \
                     regime's history, or measurement ping-pong — the passes that \
                     landed stand",
                    dials::MAX_PASSES
                ),
                passes,
                tokens_after,
            };
        }
        if tokens_after >= tokens_now {
            // The cannot-shrink guard: a pass that committed without
            // shrinking the measurement would spin the loop forever —
            // stop loud, with the passes that did land left in place.
            return Outcome::Oversized {
                reason: format!(
                    "compaction cannot shrink the context further ({tokens_after} measured \
                     tokens against a {window}-token window): a single retained tail \
                     may exceed the window, or the window is misreported"
                ),
                passes,
                tokens_after,
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
/// satisfying both hard constraints (owner ruling 2026-09: the tail
/// is the boundary's suffix delta sum; the summarization size is
/// `head_total − tail` — the measured context minus the retained
/// tail). Both push the cut the same direction (a later cut means a
/// longer prefix AND a shorter tail); the longest-prefix objective is
/// the soft pull the other way — compaction efficiency. The
/// session-start boundary is the always-feasible floor (nothing
/// sent, the whole history kept), never a selection: an empty prefix
/// summarizes nothing. Unmeasured stretches are uncounted, never
/// estimated — the prefix reads smaller and the tail reads smaller
/// by exactly the uncounted few-K (the blessed error budget), both
/// in the conservative direction.
#[allow(clippy::indexing_slicing)] // sanctioned crash: tail_sums carries history.len()+1 sums by construction
fn select_cut(
    history: &[SessionEntry],
    tail_sums: &[u64],
    head_total: u64,
    window: u64,
) -> Option<usize> {
    if history.is_empty() {
        return None;
    }
    let cap = (dials::SENT_PREFIX_FRACTION * window as f64) as u64;
    for index in (1..history.len()).rev() {
        if !valid_boundary(history, index) {
            continue;
        }
        let tail_tokens = tail_sums[index];
        let prefix_tokens = head_total.saturating_sub(tail_tokens);
        if prefix_tokens < cap && tail_tokens >= dials::KEEP_TAIL_TOKENS {
            return Some(index);
        }
    }
    None
}

/// One pass's stream outcome.
enum PassOutcome {
    Committed {
        summary: String,
        usage: Usage,
        /// The boundary the pass finally summarized from (a shortened
        /// retry may have moved it off the selection) — the retained
        /// tail starts here, and the regime's base sums from it.
        boundary: usize,
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
#[allow(clippy::indexing_slicing)] // sanctioned crash: the boundary is a validated index into this view
async fn one_pass(
    history: &[SessionEntry],
    initial_boundary: usize,
    state: &Compaction,
    agent: &Agent,
    token: &CancellationToken,
    id: &str,
    emit: &mut (dyn FnMut(SessionEvent) + Send),
) -> PassOutcome {
    let mut boundary = initial_boundary;
    loop {
        let mut view = tabit_log::fold_branch(&history[..boundary]);
        view.push(Message::user(dials::SUMMARIZATION_INSTRUCTION));
        // The live view: summary text streams as bracket deltas. Tool
        // call items pass through here too — the verdict on them is
        // the assembled classification below (the common predicate),
        // never this forwarding.
        let bracket_id = id.to_string();
        let outcome = agent
            .completion_turn(
                view,
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
                return match rejected(error, state, boundary, history) {
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
                return match rejected(error, state, boundary, history) {
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
                    match shorten(history, boundary) {
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
                    boundary,
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
    history: &[SessionEntry],
) -> Rejection {
    match error.as_context_overflow() {
        Some(overflow) => {
            // The wall teaches the window — the lesson serves the rest
            // of the session (the triggers, the next doors).
            if let Some(window) = overflow.window_tokens {
                state.note_window(window);
            }
            match shorten(history, boundary) {
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
fn shorten(history: &[SessionEntry], from: usize) -> Option<usize> {
    (0..from)
        .rev()
        .find(|index| valid_boundary(history, *index))
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
