//! The compaction dials — every threshold and every prompt text as
//! data, in one file (owner ruling 2026-09: the prompt and all the
//! thresholds are data fields, clustered so review and polish happen
//! in one place). The formulas these feed are ruled in ROADMAP item 6;
//! changing a NUMBER here is tuning, changing a FORMULA is a ruling.

/// Condition A's fraction: the idle door fires when the context
/// exceeds this share of the window (and the mailbox is empty).
pub const IDLE_FRACTION: f64 = 0.75;

/// Condition B's reserve: the pre-request door fires when the context
/// exceeds `window − RESERVE` (the two-turn budget at ~16k/turn).
pub const URGENT_RESERVE_TOKENS: u64 = 32_768;

/// `KEEP_TAIL`: the retained-tail floor. A cut keeping less than this
/// is infeasible; a history shorter than this skips compaction
/// (reachable only on manual requests — the auto triggers imply a
/// context far past it).
pub const KEEP_TAIL_TOKENS: u64 = 16_384;

/// The sent-history cap: the prefix the summarization request carries
/// must stay under this share of the window, forward-guaranteeing the
/// request fits by construction (room for the instruction, the summary
/// output, and slack).
pub const SENT_PREFIX_FRACTION: f64 = 0.75;

/// The summarization call's output cap (`max_tokens` for the request —
/// the agent's configured cap is overridden for this call alone).
pub const SUMMARY_MAX_TOKENS: u64 = 8_192;

/// The token estimate divisor: chars per token, the heuristic every
/// reference uses (no tokenizer dependency).
pub const CHARS_PER_TOKEN: u64 = 4;

/// The maximum number of passes one door invocation runs before the
/// post-check loop stops as a belt alongside the cannot-shrink guard
/// (the guard alone terminates; this bounds pathological ping-pong
/// between estimation error and the provider's counting).
pub const MAX_PASSES: u32 = 8;

/// The summarization instruction — the final user message of the
/// compaction request. The request rides the real conversation's
/// prefix (same preamble, same toolset — cache identity), so the
/// persona lives here, not in a system prompt. Multi-pass needs no
/// second wording: the history the next pass carries already begins
/// with the previous summary, and the instruction says to replace it.
pub const SUMMARIZATION_INSTRUCTION: &str = r#"You are performing a context checkpoint compaction for a coding agent session. Summarize the conversation history above so another instance of the agent can continue the work without it.

Rules:
- Do NOT call any tools. Respond with the summary text only — a tool call here is an error.
- If the history above already begins with an earlier compaction summary, your output replaces it: carry forward everything still relevant, drop what is done.
- Preserve exact file paths, function names, commands, and error messages.
- Keep every section, even when empty. Prefer terse bullets over paragraphs.

Output exactly this structure:

## Goal
- What the user is trying to accomplish (one or two sentences).

## Constraints & Preferences
- Constraints, preferences, or requirements the user stated; or "(none)".

## Progress
### Done
- Completed work and verified facts.
### In Progress
- The current work and its state.
### Blocked
- Blockers and failing commands; or "(none)".

## Key Decisions
- **Decision**: the rationale.

## Next Steps
1. The immediate concrete next actions, in order.

## Critical Context
- Data, examples, or references needed to continue; or "(none)"."#;
