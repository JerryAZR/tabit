# ROADMAP

What tabit is building next. Everything shipped lives with the code
and the doc that owns it — AGENTS.md (the workspace layout, the law,
the not-planned list), ENGINE.md (the run loop), EXTENSIONS.md (the
extension contract), FRONTEND.md (the wire), TOOLS.md (the tool
contracts), COMPACTION.md (the compaction policy), COVERAGE.md (the
test ledger) — and **git history is the archive of how each decision
got made**: this file records the destination, not the journey, so a
closed or dropped area moves out. (Older commits cite this file's
pre-2026-09 numbered items; the mapping is git history.)

## Where we are

The foundation is shipped and hardened. In one line per area, each
owned by its doc: the provider/runtime stack; the config layers; the
session layer over the durable tree log (skills, compaction, the
world refresh); the coding tools and the built-in permission gate;
subprocess subagents and the pool; the extension host, SDK, and
install; the frozen wire and the node runtime; prompt caching; the
headless `tabit-core` binary. The `tabit` name is reserved for the
frontend that ships primary.

## What's next

### The frontend (the active item)

**TUI: the Node route (ruled 2026-09; survey on branch `tui/research`).**
The claurst ratatui harvest stays dead (GPL) and a Rust-native TUI
stays deferred. The terminal frontend rides the JS ecosystem: **the
omp fork of pi-tui (`@oh-my-pi/pi-tui`, MIT — Mario Zechner's own
next-gen line) under Bun**, spawning `tabit-core --json` as a child
(zero backend changes — the stdio edge the json mode already rides),
distributed via npm as per-platform optional packages (esbuild
pattern; no postinstall) carrying a Bun-compiled standalone TUI exe
plus the cargo-built core. Single repo, single tag, single version:
the lockstepped pair is the strict protocol handshake made atomic.
Fallback ladder: stock pi-tui on plain Node, then opentui (its
Node ≥ 26.4 engines floor breaks the one-line install today). Next
step: the walking-slice spike on Windows Terminal before product
commitment; the TUI enters the monorepo (`tui/`) when it graduates.

The GUI is **not planned** — the egui tree was deleted 2026-09 (the
paused frontend's sync twin kept surfacing as the exception on every
review; AGENTS.md carries the ruling). A future frontend that runs no
tokio extracts a sync core into `tabit-wire`'s client rather than
growing a twin.

### Extension follow-ups

- The **mid-run restoration slice** — ruled, unimplemented: a dead
  shadow restores the core tool at the next run open (the seam is
  named in EXTENSIONS.md).
- The **run-end hook point** (an ENGINE.md amendment; autotitle
  upgrades from `tool_result` to it when it lands).
- **Prompt contributions** — not a v1 capability; joins with the
  build-phase decision and a consumer.
- **Extension-served host verbs** — a future additive class (host
  verbs are core-served by definition, the open-vs-closed ruling).
- Install: the npm:/git: end-to-end example (every example rode
  `path:`); transitive teardown + `autoremove` on uninstall.
- Recorded v1 gap: `extension_usage` is not persisted.

### Subagent follow-ups

- **Background children**: routing and commands are already
  substrate-independent of active tools — the gap is one knob (handle
  detachment) plus the wait/cancel/list collection surface (opencode's
  shape).
- **Persisted children's lineage**: the dormant `parent_session`
  header + catalog grouping; resume addressing is by friendly name
  resolved against lineage (ruled — the id is never the model's
  addressing path).
- **The result-cap budget**: `turns`/`truncated` wait on it (the
  shipped cargo is `{child_id, outcome}` only).
- **A per-child dials selector** (`$TABIT_DIALS` / a spawn flag) —
  the shape that makes compaction rules per-consumer as data; the
  dials stay clustered consts until that consumer exists.

### Compaction follow-ups

- **The agentic-cut option**: cuts currently stay after text-only
  assistants and compaction nodes — a single long agentic run has no
  usable cut and the Overflow door fails loud rather than repair; the
  recorded future option is score-based selection (prefer a slightly
  overshooting clean post-text cut over a tool-result cut, unless the
  clean cut forces a super-long tail).
- Revisit `MAX_PASSES` at 2M models (16 today; a 2M → 128K switch
  would need ~21 — bump to 32 then).
- Residual edge on record: a cut landing immediately after a
  zero-usage turn lets that spanning delta absorb some cut-side
  content (needs a provider that skips usage *and* a cut in that
  exact window; bounded by one turn's content).

### Usage billing (parked 2026-08)

Two cost-truth questions parked until the usage discussion returns:
an abort mid-roundtrip discards the interrupted attempt's usage
**unbilled** (the completion ran, the frontend saw the usage event,
cumulative stats never count it — the recorder tests pin this as
current behavior, never ruled), and the discard record's usage is the
session's inference from per-turn completion tracking rather than the
engine's own fact. When the discussion returns: rule abort's discard
record (or `aborted { usage }`) versus unbilled-by-decision, and have
the engine stamp the attempt's usage on the discard item directly.

Residual from the 2026-10 billing-architecture change (the `TurnCost`
closure seam deleted; cost is computed once at the spend point and
rides the log entries as plain data): in the mid-run-switch window a
`model` command lands at receive, so its `model_change` record
precedes the in-flight run's committing turns in the file. Live,
every sink bills the run's bound selection correctly (one computed
value to the ledger, the `completion_call` event, and the durable
entry); on REPLAY/reload the parser attributes those windowed turns
to the NEW model — the dollars are the recorded ones (replay never
recomputes), only the per-model attribution shifts. The full fix is
turn entries carrying their producing selection — a format-versioned
log decision, parked here.

### Config / registry follow-ups

- **Dynamic model listing — deferred** (2026-09): `/v1/models`
  returns ids only, not useful enough to ship. Recorded direction:
  the provider catalog (curated per-model metadata) could ship as an
  extension instead of core machinery.
- **Config reload — the `reload` command** (2026-10): an additive
  first cut — re-read config/auth, new keys and providers appear, the
  model catalog re-announces (`models_available`, FRONTEND.md §6).
  Its mechanism shipped with `login`/`logout` (2026-10, protocol v21):
  the current-world cell plus the per-session world refresh is exactly
  the path `reload` reuses — what remains is re-reading the
  providers.toml layers and the command itself. One dormant
  assumption closes with it: `run.rs`'s `turn_cost` reads the rate
  card LIVE from the world config — safe today because login/logout
  swap auth only (the config Arc is shared), but a reload swaps the
  config mid-run, so it must snapshot the config into the run's bound
  pair (or re-derive the bound-at-open invoice invariant) when it
  lands. The general
  semantics — vanished providers, stale session registers, the
  skills/gate reload scope — stay parked until the command lands.
- **The models.dev catalog extension** (2026-10, recorded): an
  extension shipping an auto-updating providers fragment (the
  models.dev catalog), versioned separately from tabit itself — the
  zero-config story's other half: on a bare install it gives
  `login` its providers to scan, so first run is "install the
  catalog extension, paste a key" with no hand-written
  providers.toml.
- Per-model `headers` stay unwired (needs a client-caching decision);
  `context_window` is wired (compaction); display names /
  `reasoning` wait on a model-picker UI (with the frontend).
- Frontend-dependent model deferrals: the catalog half landed
  (v21's `models_available` boot announcement); still parked: the
  real picker, the global implicit preference (a `~/.tabit/`
  last-selected file — a registry rung below `default_model`), and
  the "selection didn't land" picker signal.

### Docs and comments sweep

Scheduled, unscheduled date: one pass over stale comments and docs —
they accrete (e.g. `assemble.rs`'s "handshake ack" phrasing surviving
the v19 report model, caught in review 2026-10). Sweep when the next
cross-cutting change touches many files anyway, not as its own event.

### ACP

**Ruled adapter-only (2026-09):** the native vocabulary stays the one
contract; the reach play is an optional `tabit-acp` leaf adapter
crate projecting stamped events onto `session/update` (the pi/
pi-acp pattern) — deferred at least until ACP v2 ships and stabilizes
through a few patch rounds. That is the re-evaluation trigger.

## Deferred until a consumer exists

- Eval harness (build when there are sessions + tools to eval).
- MCP client support / the rmcp stance (tabit-engine's `rmcp` stays
  feature-gated, off by default; verify pi's current story before
  committing).
- OAuth device-flow auth for providers.
- Orphan-result repair utility; typed `provider_status` on
  `CompletionError`.
- A modeled breakpoint/TTL caching policy (all-1h today; a contained
  edit when a felt need exists) and the completions-gateway cache key.
- **Log compression** (owner direction 2026-10): long content —
  inline images (FRONTEND.md §5's attachments), expanded skill
  bodies — will grow session files fast. Two shapes floated:
  per-entry compression of long content vs. whole-file gzip (gzip
  streams append and read back fine; the open questions are the
  write-behind append path, the parser's byte sensitivity, and
  checkout/replay indexing). Unscheduled; the trigger is real log
  bloat once attachments land.
- **Attachment expansion is CPU/IO on the command path** (parked
  2026-10): the file read, the decode, and the downscale run at
  receive, in the mailbox door. Paste sizes keep this modest today (a
  screenshot decodes in milliseconds); if it ever shows, the parked
  options are `spawn_blocking`ing the expansion or a byte cap before
  decode (an over-cap file passes through like any unresolvable tag).
- **Session titles** (design sketch, awaiting frontend demand):
  an append-only side record in the session log — last-wins, the
  header stays write-once — behind a log-format minor bump; the wire
  surface is a `title` field on `session_opened` and the
  `sessions_available` catalog plus a `rename` command and a
  rename event (one protocol bump for the set). `autotitle-ext`
  already demonstrates the generation side over `model_prompt`.
