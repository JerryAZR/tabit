# tabit-tui-alt — the alt-screen frontend

The sibling build (`../tabit-tui-work/tui`, branch `tui/research`) is the
**native-scrollback** frontend: the omp Composer retires finalized transcript
rows into terminal scrollback via the `TerminalFrameProvider`/`HistoryBatch`
contract. This package is the **other presentation**: a full-screen
alt-screen TUI — a bounded, application-owned viewport with a pinned dock —
over the same `tabit --json` edge. Same protocol (v13), same crash doctrine,
different surface.

## Engine: stock pi-tui, not the fork

`@oh-my-pi/pi-tui` (the fork) **removed the alt-screen machinery** when it
moved to the scrollback construction: no `setLayoutRoot`, no `VStack`/
`HStack`, no layout module — its `TUI`'s `altScreen` flag is a remnant. The
alt-screen implementation lives upstream in **`@earendil-works/pi-tui`**
(0.85.x, MIT, pinned exact): `TuiAltScreen` + `setLayoutRoot` +
`VStack`/`HStack`/`ScrollView`, follow-end streaming, scrollbars, in-viewport
search, mouse selection, Kitty input, IME. It is pi's own daily-driver
fullscreen mode, so it is production-proven at exactly this job; it ships
compiled `dist` JS, which Bun consumes directly, and its optional native
helpers (win32 VT input, clipboard) degrade gracefully when absent. The
ROADMAP ruling's fallback ladder already names stock ("stock pi-tui on plain
Node"); this is that rung promoted by the presentation pivot, not a new
lineage. The fork remains the scrollback build's engine; the two frontends
share no engine, only the wire seam.

## Architecture (mirrors the §9 guide, one seam swapped)

Everything in TUI-RESEARCH §9 transfers except the presentation root:

- **Verbatim from the sibling build**: `backend.ts` (spawn/handshake/stderr
  ring, the Node port of `backend.rs`), `protocol.ts` (v13 types + lenient
  parse), `mock-backend.ts` (protocol-faithful child-process mock), and their
  tests. These are presentation-agnostic.
- **The handler-table doctrine carries**: one typed handler per wire event;
  the replay pass flows the same table (one rebuild path, FRONTEND.md §7);
  deltas coalesce (~33 ms); unknown frames are logged, never swallowed.
- **What replaces the Composer**: `root.ts` builds `TuiAltScreen` with a
  layout root — transcript `ScrollView` (follow-end, chained overscroll,
  scrollbar) over a dock `VStack` (card slot, pending queue, status, editor,
  footer). No `TerminalFrameProvider`, no `HistoryBatch`, no block-retirement
  machine: the ScrollView document holds blocks for the whole session, and
  native scrollback is untouched while the TUI lives. On exit the session
  lives in the tabit log; a respawn replays it.
- **Components-as-state** (no reducer): the mode keeps only cross-cutting
  session state (active session, running, pending queue, open cards, usage
  totals, identity maps); per-block content lives in the view — the root's
  engine components, or the test recorder. The seam is `ModeView`.

## Footer: a registry of badges (owner ruling 2026-09)

The footer is a **container of badges over the mode's pushed facts** —
never a joiner of pre-formatted strings (pi's own `setStatus` string map
validated the failure modes: it needed sanitization, alphabetical
ordering, and a separate line to stay sane). The shape:

- `FooterFacts` stays the mode's typed snapshot, pushed on every change.
- A badge is `{ id, render(facts) → string | undefined, dispose? }`,
  created by a factory that receives `{ requestRender }`. Each badge file
  owns its segment's format; the registry (`src/footer/registry.ts`) is
  the one registration site — declared order is display order and
  overflow priority. Absence is semantic: `undefined` segments are
  dropped by the container, never special-cased as empty strings.
- Two input homes by data origin: wire facts ride `FooterFacts`; local
  environment (git, network, clock) the badge sources itself — cache,
  refresh out of band, `ctx.requestRender()` when it lands (`render`
  stays synchronous). The factory form is the last machinery this design
  needs; a badge that needs a new capability grows `FooterBadgeContext`,
  never a facts back door.
- Shipped badges: model (display name → id), context (the other agents'
  meter — `ctx: █████░░░░░ 29.1% (291k/1.0M)`, warn >70 / error >90, pi's
  proven thresholds), cost (the total, recorded dollars, 3 significant
  digits — fixed decimals would show a false $0.000 early in a session),
  usage (the labeled token breakdown — `in 421  out 137  cached 8.1k
  (96.3%)`; token usage is independent of price, so its shape follows
  usage, not the billing legs; cache writes are ignored — owner ruling),
  state (running/idle — the one state word since the status strip went
  silent-on-idle).

**Extension-provided badges are deferred (owner ruling 2026-09)**: the
tabit-ext pipe has no lane to the frontend, and the frontend extension
architecture is the frontend owner's to design — not now, but after the
core features work. When it arrives it should carry **typed payloads
rendered by frontend-registered badges** (the interaction-template shape),
not raw strings from the extension; this registry is where those
renderers will land, so the frontend-side change stays a file drop.

## Tool cards: pi's model (owner ruling 2026-09)

The look is pi's ToolExecutionComponent, ported: each card is a
**state-tinted slab** (Box, 1×1 padding; pi's exact dark values — slate
`#282832` pending, green `#283228` done, red `#3c2828` failed) under a
blank breathing line, hosting two stacked views:

- **call** — the invocation, from args, bold near-white title on one
  line: `bash {command}` (with muted `(timeout Ns)`), `read {path}`
  (accent path, warn `:start-end` page range), `write {path}`,
  `edit {path}`; default falls back to the bold tool name. Always
  visible, collapsed and expanded alike.
- **result** — the body, from the result, themed `#808080`: bash shows
  the **tail** five lines collapsed (earlier-lines hint above, muted
  `Took Ns` at the end — client-measured, absent on replay), read shows
  nothing collapsed (the call line is the view) and full content
  expanded, write previews the written content from the args (ten lines
  collapsed), edit shows `+N added, -M deleted` collapsed and the
  colored unified diff expanded.

The renderer interface is `{ call?, result? }` — the call/result split
is why the cards read cleanly (the invocation never disappears; the
result decides its own preview). `elapsedMs` is measured by the block
between `tool_call` and `tool_result` (the wire carries no durations).
Click or Ctrl+O expands; the slab color carries success/failure, not a
text label.

## Transcript polish: pi's spacing language (owner ruling 2026-09)

The main view borrows pi's presentation rules, judged and ported:

- **One line of air, owned by the block above it.** Every block leads
  with exactly one blank line (tool slabs, user slabs, thinking, notes,
  assistant text); trailing spacers are gone. No blanket container
  rhythm — pi's rhythm is per-block-edge, never two blanks in a row.
- **The user message is a slab**: Box with 1×1 padding on pi's bubble
  gray `#343541`, near-white text — the strongest turn boundary in the
  view. The `❯` quote-prefix rendering is gone.
- **A one-cell output gutter with paddingY=0**: assistant markdown and
  thinking are inset one cell (pi's `outputPad`, default 1) and have no
  vertical padding — pi's own stated reason: vertical padding would open
  a gap before the tool slabs that follow.
- **Thinking reads as metadata**: italic gray (pi's `thinkingText`
  treatment), not dim mono; collapse/expand behavior unchanged.

Judged and not borrowed: OSC133 prompt zones (terminal automation),
markdown rendering of user input (our editor sends plain text), and
pi's streaming-state markdown transforms (our deltas already re-derive
the whole block).

## Divergence ledger (vs TUI-RESEARCH §9.3)

1. Engine: stock `@earendil-works/pi-tui` instead of the fork — the fork
   dropped the alt-screen API this product is (reason above).
2. Presentation root: alt-screen layout instead of Composer/frame-provider —
   the scrollback retirement contract, its invariants, and its tests do not
   apply; engine-side viewport search/scrollbars/selection arrive in exchange.
3. Everything else mirrors: backend seam, handler table, component porting
   (stock exports the same component names the sibling adapts), toolchain
   (Bun-first, exact pins, tsc strict, offline mock-driven tests).

## Milestones

- **M0 (this slice)**: seam + mode + root; mock-driven tests green; entry
  runs `--mock` and a real `tabit --json` against the raw transcript
  (plain cards: user, streaming markdown, collapsed reasoning, tool lines,
  select-one cards, steers, abort, crash report). Human pass on Windows
  Terminal is the M0 exit.
- **M1 — input first, then cards**: (1) editor autocomplete + input history
  (the engine ships the whole mechanism — `CombinedAutocompleteProvider`
  takes a cwd and does file completion with it, `EditorComponent` keeps
  up/down history; ours is the wiring: construct the provider with the
  session cwd from the wire, feed history on submit and from replay);
  (2) keybinding registry + user overrides in `~/.tabit/tui.toml`;
  (3) interaction-card completion — the select-one flow landed in M0,
  remaining: multi-select (`select_any`) toggling, free-text notes (the
  dock swaps in the editor), and a permission-gate pass (gate-ext) —
  cards land last because the interactive test vehicle ships with them:
  gate-ext installed in the extensions root produces real permission
  cards in real sessions (owner point 2026-09: no interactive testing,
  no landing). (Tool cards, usage row, and notes styling landed early
  during the pi-look port.)

- **M2 — navigation**: rewind surface (`checkout`), session switcher
  (`new`/`open` + catalog), child-session focus switching with steering
  (SessionFocusController analog; route-all makes children addressable).
- **M3 — context surfaces**: skills/extensions pages (currently notes-only),
  compaction surface (`compact` + live summary block), model picker (blocked
  upstream: no models-list command), streamed tool args (the one mock-ledger
  seam, needs `tool_call_delta` on the wire).

## TUI configuration (owner ruling 2026-09)

TOML is the config format — humans edit it. The TUI owns
`~/.tabit/tui.toml` (backend-untouched, like `settings.toml` is
TUI-untouched); `[keys]` carries keybinding overrides first, later
sections take future TUI settings (exit-print behavior, …).

## Release shape (assembled by `scripts/build-release.ts`)

The M0 human pass runs the shipped artifact, not the dev shim. The script
assembles the per-platform package into `dist/pkg/` exactly as the ruling
describes: the **Bun-compiled standalone TUI exe** (runtime embedded) next
to the **cargo release core**, with the npm bin shim — then `npm pack` +
`npm i -g <tarball>` install it as a real copy (junction/link installs are
not the test surface). The resolution ladder's first file rung is the
executable's own directory, so the installed pair is found with zero
configuration; `--print-backend` proves it headless. The two dev rungs
(repo cargo builds) and the mock rung remain dev-only — the mock is absent
from compiled builds by construction and fails loudly if asked for.

Publish-time-only (graduation work): the registry, the meta package's
optionalDependencies gating, the cross-target matrix, and signing. One
known watch-item for the M0 pass: stock pi-tui's optional native helpers
(Windows VT input, clipboard) must be verified inside the compiled exe —
they degrade gracefully, but if Shift+Enter or clipboard paste misbehaves,
that fallback is the suspect.

## Deliberate defaults (until overruled)

- **Launch semantics (owner rule)**: no args starts a **new** session;
  `-c`/`--continue` resumes the project's newest — continuation is always
  explicit, never the silent default.
- Esc interrupts (armed double-press semantics arrive with the keybinding
  registry); Ctrl+C aborts through the engine's input-listener seam when a
  run is active, otherwise defers to the editor (copy).
- Interaction cards replace-focus in the dock (pi's editor-replacement
  pattern); unknown `ui_type`s render an explicit cannot-answer notice —
  never a fabricated answer (FRONTEND.md §8).
- Dark-implicit theme (engine defaults + semantic styling added in M1);
  all-MIT, no GPL anywhere.
