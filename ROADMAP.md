# ROADMAP

What tabit is building next. Everything shipped lives with the code
and the doc that owns it — AGENTS.md (the workspace layout and law),
ENGINE.md (the run loop), EXTENSIONS.md (the extension contract),
FRONTEND.md + PROTOCOL.md (the wire), TOOLS.md (the tool contracts),
COVERAGE.md (the test ledger) — and **git history is the archive of
how each decision got made**: this file records the destination, not
the journey, so a closed area appears here only as a pointer or (for
compaction, whose policy record two docs cite) one final-form record
with no amendment history. Older commits and sibling docs reference
ten numbered items; the mapping note at the bottom says where each
went.

## What this is

`tabit` is a **study/research project in clean, solid, flexible agent
architecture — not a "better pi"** (owner ruling 2026-09). pi stays
the feature reference and survey source, never the target. The
reasons to keep building: the native Rust stack (single binary, no
Node), the frozen-wire node model every tabit process shares
(frontends, subagents, and extensions are all nodes — one substrate,
one set of mechanisms), the TUI-first frontend track, and full
ownership of the design — its pace, its discipline, its failure
modes.

## Where we are

The foundation is shipped and hardened. In build order, all closed:

- **Transport/runtime** (rig-core/rig-agent): anthropic + openai
  (Responses API) over the shared openai-compatible engine, pi-policy
  retry, stall warnings, typed errors; the engine loop per ENGINE.md,
  the hook surface, tool-panic containment, token-and-detach
  cancellation.
- **Config** (`tabit-config`): providers/auth/settings layers, env
  replaces-not-unions, `extra_body` as the one compat escape hatch —
  no model catalog, ever.
- **Session layer** (`tabit-session`, `tabit-log`): the durable
  tree-format session log (format v5), write-behind persistence,
  rewind/branch, model registry + keyless providers, session-level
  skills (protocol v20), the system-prompt builder, compaction +
  overflow recovery (the record below).
- **Coding tools** (`tabit-tools`): read/write/edit/bash per the
  rulings in TOOLS.md; the built-in permission gate (`tabit-gate`,
  pi-sanity's policy — wired end to end incl. the /tmp rewrite).
- **Subagents**: subprocess children are the ONE substrate — children
  are full session hosts over the frozen wire (`SpawnContext`,
  `tabit-wire`'s client); the `subagent` tool is the opinionated
  example.
- **Extensions** (`tabit-ext`, `tabit-ext-sdk`, `tabit-ext-install`):
  the host, the SDK, install/management — the checklist is complete;
  EXTENSIONS.md is the contract and the record.
- **The wire** (`tabit-protocol`, `tabit-wire`, the session edge):
  protocol v20, the node runtime (locality routing, asks, the report
  model), the json stdio edge.
- **Prompt caching** (shipped 2026-08): all-1h Anthropic, session-id
  cache keys for OpenAI Responses, subagents keyed separately — the
  policy lives in `ModelRegistry::build`.
- **The backend binary** (`tabit-core`): headless print + json modes.
  The `tabit` name is reserved for the frontend that ships primary.

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

### Config / registry follow-ups

- **Dynamic model listing — deferred** (2026-09): `/v1/models`
  returns ids only, not useful enough to ship. Recorded direction:
  the provider catalog (curated per-model metadata) could ship as an
  extension instead of core machinery. When reload lands,
  discovery/catalog merge must be one callable step over
  `(config, auth)`, not a re-run of the initialization flow.
- Per-model `headers` stay unwired (needs a client-caching decision);
  `context_window` is wired (compaction); display names /
  `reasoning` wait on a model-picker UI (with the frontend).
- Frontend-dependent model deferrals: the models-list command and the
  real picker, the global implicit preference (a `~/.tabit/`
  last-selected file — a registry rung below `default_model`), and
  the "selection didn't land" picker signal.

### ACP

**Ruled adapter-only (2026-09):** the native vocabulary stays the one
contract; the reach play is an optional `tabit-acp` leaf adapter
crate projecting stamped events onto `session/update` (the pi/
pi-acp pattern) — deferred at least until ACP v2 ships and stabilizes
through a few patch rounds. That is the re-evaluation trigger.

## Explicitly not planned

(kept in sync with AGENTS.md)

- The egui GUI (deleted 2026-09) and in-process subagents (removed
  whole — subprocess children are the ONE substrate).
- WebSocket streaming; SSE resumption/reconnect.
- Mid-conversation system messages.
- Model catalog / name-keyed behavior.
- Vendor instruction files (CLAUDE.md etc.) — AGENTS.md only;
  instruction-file walking beyond home + cwd.
- A GPL anything (the claurst harvest is dead; all-MIT).

## Deferred until a consumer exists

- Eval harness (build when there are sessions + tools to eval).
- MCP client support / the rmcp stance (rig-agent's `rmcp` stays
  feature-gated, off by default; verify pi's current story before
  committing).
- OAuth device-flow auth for providers.
- Orphan-result repair utility; typed `provider_status` on
  `CompletionError`.
- A modeled breakpoint/TTL caching policy (all-1h today; a contained
  edit when a felt need exists) and the completions-gateway cache key.

## Design record: compaction (final form)

The one closed-area record kept here — ENGINE.md cites it for the
policy while carrying the flow facts, and PROTOCOL.md's wire flags
resolve against it. Amendment history: git.

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
  same toolset (prefix-cache identity), no tools offered, the
  instruction riding in the user message; **rejects every tool call**
  — a violating response is discarded and the request resent, bounded
  by the retry cap, each discard closing as `compaction_failed`.
- **Cut selection**: the latest valid boundary satisfying sent-prefix
  < 75% of the window ∧ tail ≥ `KEEP_TAIL`; blocks are post-text
  boundaries (after assistants without tool calls). Rejection or a
  length-capped summary moves the cut one block up and retries.
  Multi-pass is just another regular compaction (pass N+1's history
  already carries pass N's summary); tail overshoot is normal when
  history ≫ window.
- **Outcome**: `Compacted` (happened ∧ fits), `NothingToCompact`
  (benign), `Oversized { reason, passes, tokens_after }` (not good to
  continue — the guard, the pass cap, or infeasibility), `Failed`,
  `Cancelled`. The intercept parks a retry only on `Compacted`.
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

## Where the old items went

The pre-2026-09-26 roadmap carried ten numbered build items plus
done-marked remediation rounds; sibling docs and COVERAGE.md's
history still cite the numbers. The mapping:

1. **Config** → shipped (`tabit-config`; AGENTS.md's bullet). The
   unwired/deferred edges live in "Config / registry follow-ups".
2. **Session layer** → shipped (`session/`, `tabit-log`, format v5).
3. **Prompt builder + skills + AGENTS.md discovery** → shipped
   (session-level skills, protocol v20; the ladder ruling lives in
   AGENTS.md's tabit-session bullet and skills.rs).
4. **Coding tools** → shipped (`tabit-tools`; the rulings live in
   TOOLS.md) + the gate (`tabit-gate`).
5. **Native subagents** → shipped (the subprocess substrate, closed
   two rounds 2026-09); open edges in "Subagent follow-ups".
6. **Compaction + overflow recovery** → shipped; this file's design
   record above is the final form.
7. **CLI / interface** → shipped (`tabit-core` print + json; the
   protocol's design record is PROTOCOL.md, the contract
   FRONTEND.md). The GUI deletion ruling is in AGENTS.md; the TUI is
   this file's active item.
8. **Client/server + protocol** → shipped (`tabit-protocol`,
   `tabit-wire`); ACP's adapter-only ruling above.
9. **Extensions** → shipped, checklist complete; EXTENSIONS.md is
   the contract and the record; open edges above.
10. **Prompt caching** → shipped (the policy site is
    `ModelRegistry::build`).

The two "deferred round" remediation sections (2026-08) executed
completely — their record is git history and COVERAGE.md's round
sections.
