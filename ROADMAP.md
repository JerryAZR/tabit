# ROADMAP

What tabit is building next. Everything shipped lives with the code
and the doc that owns it — AGENTS.md (the workspace layout and law),
ENGINE.md (the run loop), EXTENSIONS.md (the extension contract),
FRONTEND.md (the wire), TOOLS.md (the tool contracts),
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

- **Transport/runtime** (tabit-providers/tabit-engine): anthropic + openai
  (Responses API) over the shared openai-compatible engine, pi-policy
  retry, stall warnings, typed errors; the engine loop per ENGINE.md,
  the hook surface, tool-panic containment, token-and-detach
  cancellation.
- **Config** (`tabit-config`): providers/auth/settings layers, env
  replaces-not-unions, `extra_body` as the one compat escape hatch —
  no model catalog, ever.
- **Session layer** (`tabit-session`, `tabit-log`): the durable
  tree-format session log (format v5), write-behind persistence,
  rewind/branch, model registry + keyless providers, built-in
  `login`/`logout` over the world refresh (protocol v21 — the
  surgical auth.toml write, the current-world cell, every open
  session's next run open on the new world), session-level
  skills (protocol v20), the system-prompt builder, compaction +
  overflow recovery (the record below).
- **Coding tools** (`tabit-tools`): read/write/edit/bash per the
  rulings in TOOLS.md; the built-in permission gate (`tabit-gate`,
  pi-sanity's policy — wired end to end incl. the /tmp rewrite).
- **Subagents**: subprocess children are the ONE substrate — children
  are full session hosts over the frozen wire (`SpawnContext`,
  `tabit-wire`'s client); the `subagent` tool is the opinionated
  example. Live follow-ups shipped 2026-09-26: completed children
  park in the session's pool (`subagent_pool.rs`) under petname ids,
  the `followup` tool continues the same child session by id, and
  the pool collects entries idle past five parent turns (the sweep
  rides the session's turn starts — turns, never wall-clock).
- **Extensions** (`tabit-ext`, `tabit-ext-sdk`, `tabit-ext-install`):
  the host, the SDK, install/management — the checklist is complete;
  EXTENSIONS.md is the contract and the record.
- **The wire** (`tabit-protocol`, `tabit-wire`, the session edge):
  protocol v21, the node runtime (locality routing, asks, the report
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

(Live follow-ups — park, `followup` by id, the five-turn idle sweep —
shipped 2026-09-26; the edges below are the remaining ones.)

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

## Explicitly not planned

(kept in sync with AGENTS.md)

- The egui GUI (deleted 2026-09) and in-process subagents (removed
  whole — subprocess children are the ONE substrate).
- WebSocket streaming; SSE resumption/reconnect.
- Mid-conversation system messages.
- Model catalog / name-keyed behavior.
- Vendor instruction files (CLAUDE.md etc.) — AGENTS.md only;
  instruction-file walking beyond home + cwd.
- Dedicated search tools (grep/glob shapes) — `bash` with piping and
  filtering is the search surface (owner ruling, recorded 2026-09-26).
- A GPL anything (the claurst harvest is dead; all-MIT).

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
  inline images (see the attachments design record), expanded skill
  bodies — will grow session files fast. Two shapes floated:
  per-entry compression of long content vs. whole-file gzip (gzip
  streams append and read back fine; the open questions are the
  write-behind append path, the parser's byte sensitivity, and
  checkout/replay indexing). Unscheduled; the trigger is real log
  bloat once attachments land.

- **Session titles** (design sketch, awaiting frontend demand):
  an append-only side record in the session log — last-wins, the
  header stays write-once — behind a log-format minor bump; the wire
  surface is a `title` field on `session_opened` and the
  `sessions_available` catalog plus a `rename` command and a
  rename event (one protocol bump for the set). `autotitle-ext`
  already demonstrates the generation side over `model_prompt`.

## Design record: attachments (image content in user messages)

The design for user-attached images, recorded before implementation
(the original ruling predates any repo record — it lived in a session
log on another host; this section is the record now). Owner rulings
2026-10, superseding the TUI v0's text-reference expansion proposal
(the frontend proposes; the backend owns the contract — the TUI's
paste → temp file → tag insertion rides either way). **Landed
2026-10** as specified (tabit-session's `attachments.rs`, the mailbox
door, FRONTEND.md's §5); the one fork the implementation surfaced:
the engine's steer announcement carried pre-rendered text through the
providers' strict single-text `user_text()`, which silently dropped
multi-part messages from the `user_message` events — the `Steer` item
now carries the messages and the session's own text fold renders
them.

### The rulings

1. **Tags stay as-is in the user message text.** `<attachment
   path="…"/>` is the anchor — the skill-tag rule (`<skill
   name="…"/>`), unchanged.
2. **Expansion appends, per tag and in tag order: the file name +
   the image as a base64 content part.** The model reads the message
   in full (tags included) and correlates parts to tags by name. The
   expanded user message is one multi-part `Message`:
   `[text(original, tags intact), text(label naming the file),
   image(base64), …]`.
3. **The backend downscales at expansion** — before anything is
   stored or sent — so the server never rejects a big image and the
   durable record holds what the model saw (the faithful-copy
   doctrine).
4. **The first-part law: expansion only appends parts; it never
   touches the first part** (2026-10, closing the flaw the
   string-based skill expansion admitted — it folded the skill body
   INTO the authored text, destroying the authored form). A message
   enters the door as exactly one text part — the wire's `message {
   session, text }` command is text-only, a structural guarantee —
   so part[0] IS the authored text: skill blocks land as their own
   text parts (the `user_text` fold owns the join separator; parts
   carry no join punctuation), and `message_queued` /
   `messages_discarded` hand back part[0]. Double-expansion on a
   salvaged re-send is then impossible: the draft contains no
   expansion, so re-sending re-expands exactly once.

### The mechanics (verified against the tree 2026-10)

- **Expansion point: the mailbox door** (`session/mailbox.rs`'s
  `push`, receive time, before the id is minted) — the same funnel
  and pass-through rules as the skill tag: an unresolvable tag (file
  missing/unreadable/not a decodable image — type is *sniffed*, never
  extension-trusted) is left as-is with a warn; the message still
  enters. Skill and attachment expansions compose in one door pass:
  skills first (skill blocks as their own text parts, after the
  authored text — the first-part law), then attachment parts over
  the expanded message.
- **The wire never changes.** `message { session, text }` stays
  text-only; frontends send tags as plain text. `user_message`
  events stay text — the tag is the frontend-visible anchor; image
  bytes never cross the wire. No protocol version moves for this.
- **The log stores the image inline (ruling (a), the original
  ruling).** `user_message` entries carry the expanded multi-part
  `Message`; the log's `Message` is the providers' `Message`
  (`tabit-log/src/entry.rs`), which already (de)serializes image
  parts — inline base64 rides the existing schema; no blob
  side-files, no reference integrity, history self-contained (paste
  temp files may die after the door read). A format-version note
  lands with the implementation (old logs simply never contain
  parts). Size growth is real; the answer is the compression topic
  (below), not blobs.
- **Model-facing history**: the context manager's tree holds the
  `Message` verbatim, so image parts ride every later turn and every
  replay. (Implementation verifies the history view passes user
  messages through without text-joining.)
- **Downscaling**: once, at expansion. One conservative global cap —
  a documented constant, not per-provider config (rule 1): proposed
  long edge ≤ 1568px and ≤ 5MB post-encode (JPEG quality ladder when
  over). `image` crate, battle-tested.
- **Models that don't take images**: the announced `input`
  modalities (`models_available`, v21) let the frontend warn before
  sending; backend-side, request-construction failure on unsupported
  content is already a graceful terminal (ENGINE.md's taxonomy), the
  history carries forward, and a model switch repairs. No new refusal
  path.
- **Scope: raster images only** (png/jpeg/gif/webp). Other file types
  pass through untouched; documents/PDFs are a later item (the
  provider layer already models document parts).

### Corners settled at review (owner rulings 2026-10)

- The label text is the **basename** (the tag anchors the full path
  in the message text; the temp-file path is noise to the model).
- The downscale numbers (1568px / 5MB) are internal constants,
  tunable at will — the proposed values are the defaults.
- **Images ride compaction**: the summarization request sees what
  the model saw.
- Draft salvage of a queued-but-discarded message is frontend-side
  text (the wire never held parts) — no impact.

**B4 (parked): expansion is CPU/IO work on the command path.**
Attachment expansion — the file read, the decode, the downscale —
runs at receive, in the mailbox door, on the session's command
path. Paste sizes keep this modest today (a screenshot decodes in
milliseconds); if it ever shows, the parked options are
`spawn_blocking`ing the expansion or a byte cap before decode (an
over-cap file passes through like any unresolvable tag).

### The capability question (settled 2026-10): no gate — the server
### is the authority

Owner ruling: **send images unconditionally**; never consult the
declared `input` modalities to admit or refuse a run. Provider
stacks know their own capability better than user config does —
vision preprocessing/OCR upstream of text-only models makes pastes
"just work" (the Claude Code behavior), and the declared default
(text-only) would mostly fire as false refusals on undeclared but
capable models. A server that truly cannot carry the content says so
with a request error — the existing graceful terminal
(`run_failed { kind: "provider" }`, ENGINE.md's taxonomy); history
carries forward and a model switch or `checkout` repairs. Accepted
residual: a fringe server could silently drop image parts and answer
blind — undetectable from our side, the server's defect, and no
config-keyed gate would have caught it either. `input` modalities
stay advisory picker-display data (`models_available`), never a
gate. The summarization call is equally ungated — a provider that
rejects images fails the compaction pass through the same graceful
path (`compaction_failed`), never a special case.

## Design record: session_closed (the wire-level child-death
## announcement)

Landed 2026-10 (protocol v23; FRONTEND.md §6 carries the wire
facts). The rulings:

1. **One field, no reason.** `session_closed { id }`, stamped with
   the closing session's id, the LAST frame on that stream: *this
   session will never emit again; discard of frontend state is
   safe.*
2. **The vouch asymmetry.** Birth is self-announced
   (`session_opened` crosses `--parent` at the source); death is
   vouched by the spawner-side wire. A child never announces its own
   death — a crash path cannot — and one synthesis point (the lane's
   retraction at the pipe's EOF, in tabit-wire's client) cannot
   duplicate.
3. **Synthesis is wire-level mechanism, not policy**: the lane
   machinery itself, so the subagent bridge and the extension SDK's
   owned children both get it from the one mount.
4. **Ordering falls out of the pump**: the close synthesizes only
   after the child's stdout reaches EOF, so it is genuinely the
   stream's last frame. The reaper's exit observation sweeps only
   the lane's asks (a pipe-holding descendant can outlive the
   process; stranded cards cannot wait for that EOF) and never
   synthesizes — the graceful EOF and the reaper cannot
   double-announce, and the enumeration the cascade reads (the
   learning table's per-lane stamps) survives until the pump drains
   it.
5. **The cascade is backend-synthesized** (the owner's "either seems
   fine, just make sure it's clear in the contract" — backend
   chosen): process death is subtree death (tree-kill is the
   substrate), so a retracting lane closes every stamp learned
   through it, and the synthesized frames fan and relay exactly like
   child-arrived frames — they cross upstream indistinguishably.
   Contract line: a frontend discards exactly the sessions it
   receives closes for; no tree inference required.
6. **Parked children emit nothing**: run completion is not death
   (the subagent pool parks completed children for `followup`);
   closes fire only on actual process exit. User-facing sessions
   never get one — their death is the process's own, and the pipe
   close is the signal (FRONTEND.md §3).

## Design record: compaction (final form)

The one closed-area record kept here — ENGINE.md cites it for the
policy while carrying the flow facts; the wire shapes it produced
live in FRONTEND.md §6's `compaction_*` events. Amendment history:
git.

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
   protocol's contract is FRONTEND.md). The GUI deletion ruling is in
   AGENTS.md; the TUI is
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
