# M2 — stream views, focus, and the subagent list

The design record for milestone 2 (child streams / focus switching).
Every rule here was settled in discussion; open questions are listed at
the end and nowhere else. When a rule changes, change it here first.

## Backend facts the design builds on (the wire contract, not our choice)

- A subagent child is a **full session host** (this binary in `--json`
  child role) on the shared node. It announces itself: its first stamped
  line is `session_opened` with `id` (the child session's ULID — a
  machine id, **no name**), `parent` (parent session id), `parent_call`
  (the spawning tool call's id — pairs the child stream with the exact
  open tool-call block in the parent's transcript), plus `cwd`/`model`/
  `resumed`. Child frames arrive relayed, stamped `[stream, ttl]`, run
  lifecycle per stream.
- **Parked vs. reaped is unobservable** — no pool frames cross the wire.
  A completed child's stream stays open; a followup is a new run on the
  same stream.
- Children are **ephemeral** (no log): on session resume the parent
  transcript replays its tool cards, but child streams are gone. Child
  views are live-session-only.
- Commands carry `session` (the stream id): `message`, `abort`,
  `compact`, `model`, … route per stream already.

## Core architecture

1. **One transcript per stream.** mode.ts's single shared transcript
   dies. A `Map<stream, StreamView>`: each `StreamView` owns its
   transcript blocks, its per-session facts (cwd, model, compaction,
   skills — already folds per-stream), and its **activity atom** (see
   the list widget). Every stream folds its frames **continuously,
   focus-independent** (the backend runs every stream independently; the
   TUI consumes independently — codex's buffered-threads model).
2. **Focus is a stream id** (root is the default). AltRoot renders the
   focused stream's blocks; components stay alive per stream, so scroll
   position and collapse state survive switching. Views are **kept in
   memory** until the backend gains a reaped event — the list's display
   policy is a separate question from view lifetime (owner ruling).
3. **The parent's view of a child is its subagent tool card** — no child
   frames merge into the parent's transcript anymore (M2 removes the
   current merged folding; codex and opencode both converged on this).
   A live status line inside the card (`↳ <current tool>`) is a later
   refinement; the frames already fold.
4. **Interaction cards are view-independent** (owner ruling): the card
   slot lives in the dock, not in any transcript view; a card from any
   stream surfaces immediately, labeled with its stream when non-root.
   Answers route by card id — no focus interaction.

## Focus laws (owner rulings)

- **Editor submit routes to the focused stream** — `message { session,
  text }` carries it; steering a focused child costs nothing. **Switching
  stream focus discards the editor draft** (owner ruling: travel only if
  the editor had a clear-all shortcut; pi-tui 1.0.2's editor actions top
  out at delete-to-line-start/end, so there is none — a stranded draft
  could only be sent to the wrong stream). Region-focus moves (editor ↔
  list ↔ transcript) never touch the draft.
- **Esc**: focused stream running → `abort` that stream; focused stream
  idle and not root → return to its **parent** (generic walk-up, never
  hardcoded to root); root idle → nothing. Quit stays ctrl+d/ctrl+c on
  an empty editor; Ctrl+C keeps its global interrupt shape.
- **Slash commands**: session-scoped ones (`/model`, `/compact`,
  `/rewind`, `/tree`) route to the focused stream; backend-level ones
  (`/new`, `/login`, `/logout`, `/sessions`) stay global. Uniform rule.
- The **footer names the focused stream** (codex's "watching" label)
  with its cwd/model; the `@` completion root follows the focused
  stream's cwd.

## Region navigation (owner ruling: alt+arrows are spatial)

Focusable regions form a vertical stack: **transcript — editor —
subagent list** (the list exists only when it has entries).

- `alt+↑` / `alt+↓` move **region focus**, skipping absent regions:
  alt+↑ from the editor → transcript; alt+↓ from the transcript →
  editor; alt+↓ from the editor → the list (when present).
- **Plain ↓ in an empty editor** also moves focus into the list (the
  no-effect-↓ rule, claude-code's model, owner ruling). Restricted to
  the empty editor because pi-tui 1.0.2 keeps history-browsing and
  autocomplete-open state private — at a non-empty end-of-text boundary
  we cannot tell a true no-op ↓ from "load newer draft" or "move the
  suggestion highlight". Widening to the full rule needs one public
  accessor from pi (upstream-worthy); no design change when it lands.
- **Within the focused region, plain arrows are local** (owner ruling):
  editor-focused → cursor/history (today's behavior); list-focused →
  selection movement. Direction semantics are the widget's: ↑/↓ for a
  vertical list, four directions for a grid.
- In the list: **Enter** switches the stream focus to the selected
  subagent and returns region focus to the editor (now bound to that
  stream). **Esc / alt+↑** returns region focus to the editor without
  switching.
- **A focused region that disappears yields the editor** (owner ruling,
  generic): if the focused region goes away for any reason — the list's
  last entry hides, the transcript region is removed — region focus
  returns to the editor. The list-emptying case is one instance.
- The transcript region takes plain scroll keys (↑/↓, PgUp/PgDn);
  richer transcript interaction is a later milestone, the region exists
  now so the navigation model is complete.

## The subagent list widget (owner ruling: a swappable seam)

- **The seam**: the stream registry projects entries
  `{ stream, parent, title, state }`; a dock widget consumes the
  projection and emits `focus(stream)` intents. The core (views, focus
  laws, folding) never knows which widget is mounted — a picker in the
  card slot or a grid can replace the first widget without touching it.
- **First widget**: a **vertical list below the editor** (owner ruling),
  one row per child stream, visible whenever it has entries; ↑/↓ move
  the selection. Capped at ~5 visible rows, scrolling beyond.
- **Row**: status dot · task text · state word.
  - **Title**: the parent tool call's `task` argument (first line,
    truncated), linked via `parent_call` — the user's language, owner
    ruling. The pool petname is LLM-facing and joins the row only when
    `session_opened` later carries a name (backend change).
  - **State atom** (priority, highest wins): `waiting` (open interaction
    card on that stream) → tool name (open `tool_call` sans result; `N
    tools` when several) → `thinking` (reasoning block open) →
    `running` (run open, model writing) → `idle` (terminal completed —
    the honest word; parked/reaped unobservable) → `failed`/`aborted`.
    Dot colors: green running, red waiting/failed, dim idle.
  - The atom derives in the per-stream fold (run_started/terminals,
    reasoning open/close, tool_call/tool_result, card open/settle); the
    list re-projects on any fold event touching a listed stream.
- **Visibility policy** (owner ruling): running children are always
  listed; idle/failed/aborted entries linger **60 seconds, time-based**
  (a per-entry timer re-projects the list; chosen over turn-based as the
  simpler of the two offered), then hide. Hiding never drops the kept
  view — a followup run makes the stream running and it reappears.
- **Keybindings**: `tui.app.regionUp` (alt+↑), `tui.app.regionDown`
  (alt+↓) join the registry, `tui.toml`-overridable, listed in `/help`.

## Explicit limits (stated, not solved)

- Parked vs. reaped unobservable → rows say `idle`, never "parked".
- No child replay on resume (ephemeral) → after `--continue`, the
  parent's tool cards replay; no child views exist until children run
  again.
- Hidden idle views are unreachable until they run again — the kept
  views are memory, not navigation. A "show all" widget is a possible
  later shape over the same seam.
- Extension-spawned grandchildren (deeper nesting) render flat in the
  list; indentation is a later refinement.

## Open questions

(none — the three from the design discussion were ruled: drafts are
discarded on stream switch (no editor clear-all exists), a vanished
focused region yields the editor generically, the first widget is a
vertical list.)
