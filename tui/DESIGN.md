# tabit-tui — the alt-screen frontend

This package is tabit's primary terminal frontend: a full-screen,
application-owned viewport with a pinned dock over the
`tabit-core --json` stdio edge. It speaks protocol **v27** (the mirrored
types live in `src/protocol.ts`; FRONTEND.md owns the contract) and keeps
the backend's crash doctrine: restore the terminal, report the failure,
and let durable session replay drive recovery.

## Engine: stock pi-tui, not the fork

`@oh-my-pi/pi-tui` (the fork) **removed the alt-screen machinery** when it
moved to the scrollback construction: no `setLayoutRoot`, no `VStack`/
`HStack`, no layout module — its `TUI`'s `altScreen` flag is a remnant. The
alt-screen implementation lives upstream in **`@earendil-works/pi-tui`**
(MIT, pinned exactly in `package.json`): `TuiAltScreen` + `setLayoutRoot` +
`VStack`/`HStack`/`ScrollView`, follow-end streaming, scrollbars, in-viewport
search, mouse selection, Kitty input, IME. It is pi's own daily-driver
fullscreen mode, so it is production-proven at exactly this job; it ships
compiled `dist` JS, which plain Node consumes directly, and its optional native
helpers (win32 VT input, clipboard) degrade gracefully when absent. The
ROADMAP ruling's fallback ladder already names stock ("stock pi-tui on plain
Node"); this is that rung promoted by the presentation pivot, not a new
lineage. The fork remains the scrollback build's engine; the two frontends
share no engine, only the wire seam.

## Architecture

The presentation-independent transport and the alt-screen view meet at the
mode/view seam:

- **Transport and protocol**: `backend.ts` (spawn/stderr ring),
  `protocol.ts` (v27 types + lenient parse), `mock-backend.ts`
  (protocol-faithful child-process mock), and their offline tests.
- **The handler-table doctrine carries**: one typed handler per wire event;
  the replay pass flows the same table (one rebuild path, FRONTEND.md §7);
  deltas coalesce (~33 ms); unknown frames are logged, never swallowed.
- **What replaces the Composer**: `root.ts` builds `TuiAltScreen` with a
  layout root — transcript `ScrollView` (follow-end, chained overscroll,
  scrollbar) over a dock `VStack` (card slot, pending queue, status,
  notice line, editor, subagent list, footer). No `TerminalFrameProvider`, no `HistoryBatch`, no block-retirement
  machine: the ScrollView document holds blocks for the whole session, and
  native scrollback is untouched while the TUI lives. On exit the session
  lives in the tabit log; relaunching with `-c`/`--continue` replays it.
- **Components-as-state** (no reducer): the mode keeps per-stream session
  state plus cross-stream catalogs and focus; per-block content lives in
  the view — the root's engine components, or the test recorder. The seam
  is `ModeView`.

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

## Divergence ledger (historical, vs the native-scrollback research)

1. Engine: stock `@earendil-works/pi-tui` instead of the fork — the fork
   dropped the alt-screen API this product is (reason above).
2. Presentation root: alt-screen layout instead of Composer/frame-provider —
   the scrollback retirement contract, its invariants, and its tests do not
   apply; engine-side viewport search/scrollbars/selection arrive in exchange.
3. Everything else mirrors: backend seam, handler table, component porting
   (stock exports the same component names the sibling adapts), toolchain
   (plain Node — native type stripping, `node --test` — exact pins, tsc
   strict, offline mock-driven tests).

## Session tree: client-built, checkout now (owner ruling 2026-09)

The tree is **constructed on the frontend from the wire itself** — no
backend lane needed. Entry ids are stable across live and replay (the
replay projects the log's own entry ids), so parenting every newly seen
entry on the current chain head, and moving the head on
`checked_out.entry_id`, accumulates every branch this process witnessed.
A fresh attach sees only its replay's chain — a degenerate tree (a list)
— until the backend names heads explicitly. The deferred piece is
**branch enumeration for pre-attach history** (pi's `getTree()` reads the
whole file; our wire re-emits the active chain only) — the owner resolves
that separately; when it lands it slots into `SessionTree` without
touching the view.

- **`SessionTree`** (`src/session-tree.ts`): the store. Chain nodes are
  exactly the events carrying entry ids — user messages, turns
  (`turn_started`'s id), tool results, compaction passes. Each add is
  create-or-advance: a known id (a replay re-walking the shared prefix)
  only moves the head, never re-parents. Turn previews grow only while
  the turn is open (`replay_begin` closes the open turn — a re-announce
  never doubles). `turn_retried` drops the subtree and falls the head
  back to the surviving parent.
- **The checkout action is real today**: the card's enter sends the
  `checkout` command (the backend composes abort first — a rewind has
  declared the run's continuation obsolete) and closes; `checked_out` +
  the replay pass are the re-render, and the store unions the re-walk
  with the off-path branch it just accumulated. Enter on the head is
  pi's "already at this point" — it closes without sending.
- **`TreeCardView`** (`src/components/tree-card.ts`): pi's
  tree-selector language — `│`/`├─`/`└─` prefix cells (connectors only
  where a parent actually branches; chains render as lists), accent `•`
  on the active path, typed content (`user:`/`assistant:`/muted tool
  call/compaction marker), a bounded viewport windowed on the cursor,
  and the `(n/total)` status line. Keys move through the engine's select
  actions (`tui.select.up/down/confirm/cancel`, cursor left/right to
  page). Hosted in the dock's card slot (ctrl+t or `/tree`): while open
  the input listener stands down, exactly like interaction cards; an
  incoming ask displaces it, and a pending ask keeps the tree from
  opening.

## Implementation status

The walking skeleton, input/history/keybindings, interaction cards,
session-tree checkout, model and thinking pickers, auth cards, attachment
paste, manual compaction, and per-stream child/focus handling have landed.
`tui/ROADMAP.md` is the canonical record of those slices and the ordered
remainder: non-critical QoL, skill/attachment chips waiting on pi-tui's
public marker API, and platform/distribution follow-ups. This design doc
owns the architecture, not a second milestone checklist.

## TUI configuration (owner ruling 2026-09)

TOML is the config format — humans edit it. The TUI owns
`~/.tabit/tui.toml` (backend-untouched, like `settings.toml` is
TUI-untouched); `[keys]` carries keybinding overrides first, later
sections take future TUI settings (exit-print behavior, …).

## Release shape (assembled by `scripts/pack.ts`)

The publishable family (owner ruling 2026-10): **`tabit-agent` on npm** —
the app as a **single-file esbuild bundle** (`bin/tabit.js`; plain JS for
startup speed, the sources live on GitHub) with bin `tabit` — shipping
`.ts` sources is impossible by Node policy (type-stripping is refused
under `node_modules`). pi-tui stays an external dependency: its napi
clipboard prebuilds load by path from its own package. The core rides in
one os/cpu-gated platform package per matrix entry
(`tabit-agent-windows-x64`, `tabit-agent-linux-x64` — win32 spells out
to windows in the name: npm's spam filter blocked
`tabit-agent-win32-x64` at first publish, 2026-10; darwin-arm64 joins
after a mac test), each carrying the cargo release binary. The meta's
`optionalDependencies` pin the platform packages at the same version,
injected at pack time so the dev checkout never resolves unpublished
names: **one version line for the bundle**, bumped by TUI and core
changes alike — the strict protocol handshake made atomic at the
registry. The resolution ladder finds the installed core through node
resolution (the platform-package rung); `--print-backend` proves the
installed artifact headless. The dev rungs (repo cargo builds) and the
mock stay dev-only — the bundle simply doesn't contain the mock, and
asking for it fails loud.

Publication is implemented: `scripts/pack.ts` assembles the platform family
and is the one publish path, and `.github/workflows/release.yml` builds the
Windows/Linux matrix and publishes with npm trusted publishing and
provenance. crates.io stays the backend-only channel (`cargo install
tabit-core` — the PATH rung marries it to any TUI install; the report's
version check fails loud on skew).

**The local install IS the published install**: `npm publish` only
uploads the tarballs `npm pack` already produced, so `npm i -g
dist/tabit-agent-*.tgz dist/tabit-agent-windows-x64-*.tgz` exercises the
exact registry path (the explicit platform tarball satisfies the
optional dependency offline — unpublished, it would 404 and be silently
skipped, leaving the core missing). Reinstalling the same version over
itself works — iterate on bugfixes without version bumps; publish once
the artifact passes the human pass.

## Deliberate defaults (until overruled)

- **Launch semantics (owner rule)**: no args starts a **new** session;
  `-c`/`--continue` resumes the project's newest — continuation is always
  explicit, never the silent default.
- Esc aborts a running root stream; on a child stream it always returns
  focus to the parent, running or idle. An idle root's Esc falls through
  to the editor. Ctrl+C aborts the focused stream's active run, otherwise
  defers to the editor (copy), and can quit when the editor is empty.
  Exact bindings and `~/.tabit/tui.toml` overrides live in
  `src/keybindings.ts`.
- Interaction cards replace-focus in the dock (pi's editor-replacement
  pattern); unknown `ui_type`s render an explicit cannot-answer notice —
  never a fabricated answer (FRONTEND.md §8).
- Dark-implicit theme (engine defaults plus the semantic styling in
  `src/theme.ts`); all-MIT, no GPL anywhere.
- **`@` completion stays pi-faithful** (owner ruling 2026-09): accepting a
  directory consumes the `@` and closes the list — continuing deeper is
  the two-Tab rhythm (the manual path prefix works without `@`). The
  known deviation stays parked: re-inserting `@` on directory accept
  would auto-continue the list (Claude Code style); applied only if the
  interaction ever proves a problem in use.
