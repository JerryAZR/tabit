# COMPACTION.md — the compaction policy record

The final-form policy record for compaction + overflow recovery.
ENGINE.md carries the flow facts and cites this file for the policy;
the wire shapes the work produced live in FRONTEND.md §6's
`compaction_*` events. Amendment history: git.

- **The box** (`tabit-session/src/compaction/`): its own system, a
  black box with three doors — **pre-request** (every model call in a
  run, the first included; condition B), **idle** (the beat, A ∨ B,
  mailbox empty), and the **manual `compact` command** (parked at
  receive, served at the beat ahead of any queued batch; a slot, not
  a queue — a newer parks-replaces, abort clears it). No engine flags,
  no compaction knowledge outside the box.
- **Trigger**: **A** `context > 75%·max ∧ mailbox empty`; **B**
  `context > max − 32K` (the two-turn reserve; no mailbox gate —
  urgent is urgent). Idle checks A ∨ B; the seam checks only B. The
  disjunction makes idle ≤ seam at every window by construction.
  The pause point is the **mechanism**, never a threshold preference
  (owner ruling 2026-09-29): a reference-style "compact at 50%
  before a follow-up" would land on the same pause point — the only
  delta is the cap. Unchanged for now; the shape, if ever wanted, is
  a command-line override for the idle cap (`IDLE_FRACTION`), not a
  new door.
- **Measurement, never estimation (the delta regime)**: every
  assistant commit stamps `delta_tokens = total[k] − total[k−1]`
  (predecessor: the previous measured assistant in the regime, the
  leading compaction's `tokens_after`, or 0 at start). Client-added
  text rides the following assistant's delta. A compaction appends
  as a **leaf at the head** carrying `tokens_after` (retained tail +
  the summary's output tokens), persisted — a measurement-bearing
  node. Reads: current context = the nearest measurement at-or-before
  the head over the raw branch; cut selection = one suffix-delta
  pass. Zero-usage turns commit no delta; a below-predecessor total
  re-anchors. Chars/4 is deleted from the decision path. An
  unmeasured context (no turn ever reported) skips loudly, the
  unknown-window skip's sibling.
- **The request**: appended to the real conversation — same preamble,
  same toolset (prefix-cache identity: the tools ARE offered, that is
  what makes the prefix cache-identical), the instruction riding in
  the user message; **rejects every tool call**
  — a violating response is discarded and the request resent, bounded
  by the retry cap, each discard announced as `compaction_retried`
  (`compaction_failed` only when the cap is exhausted).
- **Cut selection**: the latest valid boundary satisfying sent-prefix
  < 75% of the window ∧ tail ≥ `KEEP_TAIL`; blocks are post-text
  boundaries (after assistants without tool calls, or after a
  compaction node — pass N+1 may cut right after pass N's node).
  Rejection or a
  length-capped summary moves the cut one block up and retries.
  Multi-pass is just another regular compaction (pass N+1's history
  already carries pass N's summary); tail overshoot is normal when
  history ≫ window.
- **Outcome**: `Compacted` (happened ∧ fits), `NothingToCompact`
  (benign), `Oversized { reason, passes, tokens_after }` (not good to
  continue — the guard, the pass cap, or infeasibility), `Failed`,
  `Cancelled`, `Skipped` (benign — an unknown or below-envelope
  window, an unmeasured context, or conditions not met). The
  intercept parks a retry only on `Compacted`.
- **The envelope**: windows below **64K** skip loudly; unknown windows
  skip the thresholds while overflow recovery still works — the wall
  teaches the window from the typed transport error, learned for the
  session.
- **The record in the file**: one entry per pass, append-only,
  carrying the cut identity (the first tail entry's id) + the summary
  + `tokens_after`; the loader re-parents through it on replay.
  Rewind to pre-compaction nodes yields the full-history branch —
  compaction never deletes, realized as tree topology.
- **Dials are data**: every threshold and prompt text lives in the
  dials file — review and polish happen in one place.
