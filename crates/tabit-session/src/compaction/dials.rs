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

/// How many times a tool-call-violating summarizer response is
/// discarded and the request resent before the pass fails (owner
/// ruling: throw the response away and retry — sampling variance
/// usually corrects a one-off; never synthesize an in-band error
/// result).
pub const VIOLATION_RETRY_CAP: u32 = 1;

/// The pass cap: the largest legitimate multi-pass is a **model
/// switch importing a larger regime's history** — a nearly-full 1M
/// context switched to a 128K model needs ~10–11 passes (each pass
/// takes at most a prefix-cap of the *current* window; the wall only
/// bounds growth within one regime, a correction of the earlier
/// "8 rounds can't exist" argument). 16 covers that with headroom;
/// revisit when 2M models arrive (a 2M → 128K switch would need ~21
/// — bump to 32 then). Secondarily it bounds estimation ping-pong
/// the cannot-shrink guard's `>=` cannot see (strict-by-a-drip
/// shrink). Hitting it is `Oversized`: compaction happened, not good
/// to continue.
pub const MAX_PASSES: u32 = 16;

/// The declared support envelope for compaction, in tokens: **64K**.
/// The bare contradiction line is 57,344 (`URGENT_RESERVE` +
/// `KEEP_TAIL` + `SUMMARY_MAX_TOKENS`) — below it, condition B
/// (`context ≤ window − URGENT_RESERVE`) is unsatisfiable by
/// construction, the kept-tail floor plus a maximal summary already
/// exceeding what B demands. Rounded up to 64K to leave room for
/// real work beyond the bare dial sum (owner ruling 2026-09: state
/// what we support rather than adaptively re-scale the ruled
/// constants — the dials target real windows, 256K–1M; a
/// below-envelope window skips loudly, the unknown-window skip's
/// sibling).
pub const MIN_SUPPORTED_WINDOW: u64 = 65_536;

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
