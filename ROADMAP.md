# ROADMAP

What we have, and what to build. tabit is a **study/research project in
clean, solid, flexible agent architecture — not a "better pi"** (owner
ruling 2026-09, after re-surveying pi). pi (`~/Projects/agents/pi`;
also opencode, crush, codex, yaca) stays the feature reference and
survey source, not the target.

The founding premise — pi's core too minimal, extending it to real
needs hard — was true when the project was planned (mid-2026, writing
pi extensions hit the friction) but expired fast: pi's extension API,
CBOR wire protocol + headless server/client, session-repo backends,
harness v2, and skills all landed 2026-06 → 2026-08, right around the
founding. What remains tabit's own, and the reasons to keep building:
the native Rust stack (single binary, no Node), the egui GUI as a
first-class frontend over a frozen protocol, native subagents
(subprocess children over the frozen stdio protocol — the ONE
substrate, item 5), and full ownership of the design — its pace, its
discipline, its failure modes.

## Where we are

The foundation is complete and hardened — this is the transport/runtime layer
that pi's `pi-ai` provides, at or above its robustness:

- **rig-core**: anthropic + openai (Responses API) providers over a shared
  openai-compatible engine; pi-policy retry (408/409/429/5xx + x-should-retry,
  retry-after honored, 60s server-delay cap, jittered 0.5s·2ⁿ→8s backoff,
  default 2 retries), connect timeouts, stall warnings (a stalled stream or
  body warns every 120s and keeps waiting — never killed; owner ruling: a
  slow local server must be able to think in silence, only the user aborts),
  typed transport errors,
  per-index content-block routing with loud interleave guards, orphan
  tool-result validation, cache-usage reporting, refusal surfacing.
- **rig-agent**: the driving loop (one coroutine, ENGINE.md), the tool-phase
  hook pair, tool-panic containment, `#[rig_tool]` + `PortableTool` as the
  canonical tool surface, strict tool-arg parsing.
- Coverage per the COVERAGE.md ledger, zero warnings, wasm-clean, offline
  cassette tests, docs/policies in AGENTS.md / VENDOR.md.
- `examples/local_probe.rs` — verified live probe against LM Studio
  (OpenAI completions, OpenAI Responses, Anthropic wire formats).

Subagents ride the same substrate as pi (subprocess children; item 5
records the two rounds that got there — the in-process deviation was
built, then removed whole). Everything else follows pi's minimal path.

## What to build (in order)

### 1. Config crate (`tabit-config`)

Provider/model configuration schema, loaded from user config files.
**Shipped as `crates/tabit-config`** (TOML; loud parse/validation;
`extra_body` as the sole compat escape hatch — no compat-flag taxonomy, no
model catalog). Decisions:

- File split, all under `~/.tabit/`: `providers.toml` (providers + models —
  secret-free by construction, safe to share and edit with agent help),
  `auth.toml` (provider id -> api key; user-created, tabit never writes it),
  and `settings.toml` later (item 9). Debug overrides: `$TABIT_CONFIG`
  (providers file) and `$TABIT_AUTH` (auth file); a future CLI flag will
  outrank both.
- Provider entries: `base_url`, `api` (closed enum: anthropic-messages /
  openai-responses / openai-completions), `api_key_env` (env var *name*),
  `headers`, shared `extra_body` (merged into every request body). No
  inline keys in the provider file.
- Key resolution: `auth.toml` entry wins, else the env var named by
  `api_key_env`, else none (local endpoints run keyless; requiring a key is
  the consumer's loud decision). Command-backed auth (keychain) is a
  deferred future source.
- Per-model settings: id, display name, `reasoning`, `input` modalities,
  `context_window`, `max_tokens`, `sampling_params`, `cost` (4 required
  $/M-token rates), ordered `thinking_levels` (each a named `extra_body`
  merge — array shape so a UI can cycle), per-model `headers`/`extra_body`.
- **Request-parameter application shipped (2026-08, pure-forwarding
  ruling):** `max_tokens` and `temperature` ride the agent builder's
  dedicated knobs; `top_p`/`top_k` and the `extra_body` chain
  (provider → model → active thinking level, later wins) merge into one
  flattened `additional_params` map where `extra_body` keeps the last word
  (it is the escape hatch); provider-level `headers` ride the constructed
  client. Re-resolved on every model/thinking-level switch (the agent
  rebuild). Deliberately unwired: per-model `headers` (needs a
  client-caching decision), `context_window` (compaction — item 6,
  design ruled 2026-09; wires with implementation),
  `reasoning`/`input`/display names (model-picker UI, item 7 v2).
- `default_model` is the preferred-model slot: a bare model id (must be
  unambiguous), optional `provider` qualifier for conflicts, optional
  `thinking_level` — both wire shapes accepted (bare string or
  table). Ruled a **preference, not a hard reference**: a stale,
  ambiguous, or malformed entry never blocks startup — the registry
  warns and falls back to the first configured model
  (explicit `--model` requests and resumed-session models still fail
  loudly). The warning is stderr today; v2 moves it onto the event
  channel (`error { kind: model }` — the external-errors ruling in
  PROTOCOL.md). Reference resolution
  (`TabitConfig::resolve_model_ref`) is an exact-match lookup over an
  index registering every model under two keys — its bare id and its
  qualified `provider/model` id (model ids may contain `/`); a key with
  one registration resolves, several is an ambiguity error listing the
  candidates.
- Reference survey behind the compat decision: pi ships an explicit compat
  schema (11 thinking formats etc.); opencode absorbs quirks in per-provider
  packages/hardcoded transforms; codex ships zero compat flags and only
  speaks the Responses API. Tabit sides with codex on strictness but keeps
  `extra_body` as the generic escape hatch.
- Dynamic model listing (fetch from endpoint and merge with local config) —
  planned; design deferred until the CLI exists. The wire clients are kept
  and cassette-covered (both providers); the call is backend-only by
  construction (credentials + the front/back split), and the trigger shape
  (on-demand command vs startup push) is decided against the GUI picker
  when it asks.

### 2. Session layer

The application-level conversation layer pi builds over its agent loop:

- Session state: message history, tool call/result records, usage accounting,
  per-session model selection from config.
- Persistence: session log format (JSONL event log first — replayable,
  diff-friendly; sqlite backend later if needed).
- Session listing/resume across runs.
- **Rewind/branch shipped (format v3: the resident-state ruling)**:
  the JSONL log is a parent-linked tree of conversation nodes plus
  parentless side records; a checkout moves the in-memory head to an
  existing node (git-style — the pointer moves, nothing is copied)
  and records a `checkout` side record (durable even with no follow-up
  append), so the next prompt branches. Library level branches from
  any node (`Session::rewind_to_entry`) with the dangling repair
  covering mid-batch points; the user surface (`Session::rewind(n)`,
  CLI `--rewind <n>`) targets user-message boundaries (prompts and
  steers alike). The resident tree, head, and incrementally folded
  context are the in-session truth — nothing re-reads the file
  mid-session; projection and stats follow the active branch. Model
  selection is a session preference — the file's last `model_change`
  side record in append order (the register ruling, PROTOCOL.md v3) —
  so a checkout never moves it. Interactive branch browsing is a GUI
  feature. The CLI is print-shaped: `-p <PROMPT>` selects print mode,
  `--rewind` too, bare `tabit` errors loudly until the GUI exists.
- **Model registry shipped** (`tabit-session::ModelRegistry`): the single
  construction site for models — cached provider HTTP clients (switching
  models reuses the connection pool) and the default-selection chain:
  explicit choice > the resumed session's last model > `default_model` >
  the first configured model. A resumed reference that no longer resolves
  fails loudly. Reload and dynamic model listing merge into the registry
  when a consumer exists; per-model `sampling_params`/`thinking_levels`/
  `extra_body` application happens in the registry's build path (with
  item 6).

### 3. System prompt builder + skills & AGENTS.md discovery

- **v1 shipped** (`tabit-session::build_system_prompt`): a minimal,
  stable prompt — short base identity + `<environment_context>` (cwd,
  platform, UTC date) + discovered instruction files wrapped in
  `<project_context>`. Built once per process, never rebuilt mid-session:
  byte-stability keeps provider prompt caches valid, and date-level
  staleness is accepted (people work overnight). No opinionated
  guardrails or guidelines in the base prompt.
- Discovery policy (decided): **AGENTS.md only** (no CLAUDE.md or other
  vendor files); **no directory walking** — the home level
  (`~/.tabit/AGENTS.md`, falling back to `~/.agents/AGENTS.md`) plus the
  cwd file, cwd last so closest wins; **no size cap**; subdirectories
  are the model's job (the base prompt tells it to check for AGENTS.md
  as it descends). This replaces the CLI's stopgap `PREAMBLE`.
- Skills discovery: still to add — `SKILL.md` files with frontmatter
  (name, description), discovered from user-level and workspace-level
  directories, exposed as an on-demand listing (load-on-trigger, not
  always-inlined) in the same prompt module.
- Prompt contributions from extensions (item 9) mount at session
  build under the same byte-stability rule — changing the prompt is
  the user's explicit reload decision, never a silent mid-run event
  (EXTENSIONS.md). This is where mid-conversation system messages
  would tempt us — they are unsupported by design; everything hoists
  into the preamble (AGENTS.md).

### 4. Coding tools

The standard toolset as `#[rig_tool]` implementations, via the
PortableTool→DynamicTool erasure. **Toolset ruled to pi's four — read,
write, edit, bash** (2026-09): no glob/grep/ls — the model reaches those
through bash (`ls` deleted with the ruling). **All four shipped**
(2026-09) — `read` (paging + truncation module), `write` (overwrite
flag, parent creation, atomic store), `edit` (exact-match, partial
application, tests-first), `bash` (registration-time Git-for-Windows
detection, tail-truncate + spill). `ask_user` rides along as frontend
interaction-test scaffolding — **removed before shipping** (owner
ruling: it exists to exercise the interaction capability, not as a
product tool). Permission/approval rides on the existing hook system
(as a first-party extension — see item 9).

**Write rulings (2026-09):** `write(path, content, overwrite?)` —
creates freely; overwrites only with `overwrite: true` (the model
expresses intent, never inferred). A missing path always writes
(overwrite governs existing paths only — a false overwrite must not
veto a create); an existing path without the flag fails with the
file's size and both ways forward. Results name the branch (`Created`
vs `Overwrote (N bytes, was M bytes)`) plus `created K parent dirs`.
No read-before-write enforcement (Claude-Code-only quirk; pi has none).
Mutations serialize through `file_io`: a process-wide per-path lock
registry (the engine's tool phase is concurrency-bounded by design —
ENGINE.md — so same-path calls can interleave; the session wires
`TOOL_CONCURRENCY = 4`, rig's library default of 1 never applies to
tabit) and an atomic
store (temp-file + `tempfile::NamedTempFile::persist` for overwrites;
readers never lock — the atomic store is what makes a torn read
impossible). Read-before-*edit* likewise stays prompt-level.

**Edit rulings (2026-09, built tests-first):** `edit(path, edits[])`
with pi's multi-edit shape — every edit matched against the file's
current bytes in LF space (the one sanctioned normalization; LLMs read
bytes, not visuals — a miss means a stale view), dominant line ending
and BOM preserved on store, no other fuzzy matching. Edits apply
**independently**: matched ones land, failures are reported by index
(empty / not found / N occurrences / conflict) so the model resends
only the failures — pi's all-or-nothing rejected. No `replace_all`
(the duplicate count is the guidance; intentional global renames go
through write or bash). Overlapping edits: identical replacements are
agreement (both apply, the change lands once); anything else
conflicts and rejects both as a named pair — equality is the check,
order-simulation can agree while scrambling spans. All-fail calls are
an error naming every failure; nothing is written.

**Read + truncation rulings (2026-09, after surveying pi; amended
2026-09 to a single size cap):** one shared truncation module — 50
KiB, whole lines only; no line cap ("lines" mean nothing to a model —
a newline is just another byte). Two mechanisms, one per tool family:
**read pages** — the head plus continuation notices carrying the next
offset (the file is already on disk; never spilled); **bash keeps both
ends** — first and last lines with the middle omitted — **and spills
the full output** to a never-deleted `%TMP%\tabit-bash-*.log` (notice
points at it), because the dropped middle is unrecoverable without
re-running a command that may be slow or side-effecting. The spill
lives in the tool, not a result hook (the policy is dialect-specific;
a generic engine-level backstop cap becomes justified only when
third-party extension tools exist — noted at item 9). Read's other rulings: directories list inline
(header + sorted names, `/` on dirs — no size/mtime columns); UTF-16/32
BOMs are named in the error (Windows tooling writes them), binaries
rejected loudly (pi silently lossy-decodes — not copied); UTF-8 BOM
stripped (what read shows is what edit will match); empty files say so;
paths stay verbatim. **The caps are a dial, not doctrine**: 50 KiB ≈
12k tokens is the per-call budget today; 64/128 KiB is sanctioned
growth as contexts grow. **Image reads shipped (2026-09)** (owner:
real coding need — frontend/GUI work wants vision): PNG/JPEG/GIF/WebP
by magic bytes, whole-file image content parts (base64; both
providers' tool-result wires carry them, and the session log persists
the parts — replay reconstructs exactly what the model saw), capped
at 3 MiB raw (the ~5 MiB base64 provider ceiling; over-cap is a loud
rejection with guidance). No resize/conversion in v1 — that needs an
image-processing dependency, deferred until a consumer asks.
Video and other media when models support them.

**Shell ruling (2026-09, correctness over coverage):** the shell tool is
decided once per process at registration — `bash` only where a
Git-for-Windows install is *positively identified*: the `git.exe` on PATH
in Git-for-Windows placements (`<root>\cmd\git.exe`,
`<root>\mingw64\bin\git.exe`), the installer's
`SOFTWARE\GitForWindows` registry declaration (HKCU before HKLM — the
authoritative system-vs-user answer), or the installer's default
directories; the surviving candidate is spawn-probed (`bash -c "exit 0"`,
2s cap) before acceptance. Every miss registers the `powershell` tool
instead — a wrong bash (WSL's `System32\bash.exe` launcher, a
Cygwin/MSYS2 root with different path mapping) is worse than no bash, so
there is deliberately no bare-`bash.exe`-on-PATH source. Each tool's
description names the dialect the model is writing in; the old
per-invocation `where bash` first-hit often spawned WSL bash while the
tool described itself as bash.

### 5. Native subagents

Subprocess children are the ONE substrate (ruled 2026-09, two rounds —
"Substrate closed" below); the initial in-process plan was removed
whole, and with it this section's old framing as "the known deviation
from pi's subprocess model" — tabit's subagents ride the same
substrate as pi's, over the frozen stdio protocol. The rulings below
carry the actual shape:

**Design rulings (2026-09, the session-surface review):**

- **Subagents are sessions** — the `SessionBuilder` surface (selection,
  preamble, toolset vec, max_turns, hooks, model factory) already carries
  every per-child knob; nothing touches the engine or ENGINE.md. Toolset
  restriction is a shorter vec; recursion depth is enforced by omitting
  the subagent tool from children.
- **Two persistence modes, one builder.** Ephemeral children (reference-
  project style: in-memory only) ride `NullBuffer` — the disk-unplugged
  contract already exists; the gap is a builder entrance and id/path
  semantics. Persisted children are ordinary session files — inspectable,
  replayable via `open_session`, lineage through the dormant
  `parent_session` header field.
- **Per-session cwd** via a per-run capability (the pattern the run token
  and interaction hub already use): `read`/`write`/`edit` become
  contextual tools resolving relative paths against the session cwd;
  `bash` sets `.current_dir`. Scoping, not sandboxing — absolute paths
  and `cd /` still go anywhere (v1 accepts this, like every reference).
- **Child events reach the frontend through an event-tap capability** —
  the InteractionHub's weak-sender pattern again: the tool body's
  `prompt_with` callback stamps child events with the child's `StreamId`.
  Ruled: children announce via **`session_opened` + an optional `parent`
  field** (one announcement truth — a subagent *is* a session; a second
  announcement event was rejected). Wire change → protocol v5 with the
  GUI changelog entry; the reducer must learn the parent branch (its
  `session_opened` handler currently sets Facts unconditionally).
- **Interaction default: parent-proxy** (ruled). The child's tool
  context receives the parent's `Arc<dyn UserInteraction>` — cards pop
  on the parent's stream and answers route through the existing rails,
  zero endpoint changes. Deny-all remains a policy option.
- **The result is a capped tool result** — child output truncated (or
  spilled, bash-style) at a subagent budget; abort never looks like
  success; usage/audit ride `tool_result.details`
  (`{child_id, outcome, turns, usage, truncated}`) — the same
  presentation-cargo channel edit and bash already use.
- **Abort linkage is required plumbing**: the body selects on the child
  pump vs the parent run token (abort detaches the sidecar task; an
  unlinked child would keep spending tokens). `bash` is the reference
  body shape.
- **Memory is generic, not permission-shaped** (ruled 2026-09): the
  permission gate is a built-in extension, not a core concept, and its
  "Always allow" state is generic session-scoped extension memory —
  such a thing exists, any tool/extension can register one, and a
  subagent session can share the parent's by handle. The exact shape
  is **deferred to item 9** (it is the extension registration surface);
  until then `PermissionMemory` stays as the gate's private state and
  subagent v1 is unblocked — parent-proxy covers correctness, and an
  interim child simply mounts its own gate.

**Framework-first (ruled 2026-09):** the delivery is the FRAMEWORK —
session spawning made easy — and the `subagent` tool is one
opinionated model-facing shape extensions are expected to override.
The split (shipped): the standard `SessionBuilder` flow IS the child
API (per-agent preamble — the caller builds it for the child's own
cwd; toolset as whatever Vec the caller builds — allow-lists,
deny-lists, empty; model, budget, hooks all per-child), plus exactly
two new mechanics a worker normally provides: `SpawnContext::announce`
(the parent-carrying session_opened) and `SpawnContext::drive` (event
forwarding under the abort leash — the one recipe extensions must not
hand-roll). The example tool adds: `model` ("provider/model" or bare
id), `cwd` (scoped: its tools AND its prompt follow), `tools`
(allow-list; unknown names error loudly), recursion by omission.
**Substrate closed (2026-09, two rounds — PROTOCOL.md flag 33):**
subprocess children are the ONE substrate. Round one shipped
in-process v1 then built subprocess beside it; the routing work
exposed the cost — child-specific consumption code existed only
because an in-process child is a session without a worker — and the
owner's correction removed in-process entirely ("maintaining
something we don't need; worse, complicating the design for what's
useless"). **Shipped:** `SessionCwd` (contextual tools), the
builder's `ephemeral` entrance (plain session machinery the child's
`--ephemeral` rides), the child-role flags (`--parent`, `--tools`,
`--ephemeral`; ordinary `--session`/`--continue`/`--model`/
`--max-turns` for persisted children), the bridge (`subprocess.rs`:
self-spawn under a Job Object/process group with the child cwd as
the process cwd, frames forwarded as-is, the ruled abort shape),
the router (`routing.rs`: route-all line forwarding, learned tables
for deep trees — no abort machinery; propagation is the tool's job,
the run-token leash, per the codex/opencode survey), and the
example tool spawning subprocess children (`task`/`model`/`cwd`/
`tools`; extensions override via `SpawnContext`'s spawn/drive pair).
Every session command works on a child structurally — the child is
a full session host. Protocol v5/v6 (`session_opened.parent`, empty
path = ephemeral; the interaction tag realigned). Deferred:
persisted children's lineage (`parent_session` header + catalog
grouping), the result-cap refinement, nested-transcript GUI
rendering, and background children — deferred to the **background
tool execution** discussion (owner note 2026-09: routing and
commands are already substrate-independent of active tools — the
registry, forwarding, and the child's mailbox all key to the child
process's lifetime, not to a tool call — so the gap is one knob,
handle detachment, plus the wait/cancel/list collection surface in
opencode's shape). No extension-substrate
assumptions (item 9 owns that).

### 6. Compaction + overflow recovery

**SHIPPED (2026-09, the rulings below implemented whole):** the box
(`tabit-session/src/compaction/`), the dials file, the three doors
(pre-request leaf, the beat doors, the `compact` command), the
overflow intercept (session-side; the wall teaches the window), the
`compaction` tree node (session format v4 — v3 files still load),
the insertion fold (walkers stop at the compaction, included), typed
overflow classification at the rig-core transport layer, and protocol
v7 (the bracket events + the command). The flow facts live in
ENGINE.md's compaction amendment.

**Amended 2026-09 — the trigger measures, it does not estimate
(owner: "cache hit + input + output is the history size in context —
if the data exists, there's no need to estimate").** The data existed
in the schema all along (`assistant_message.usage`) but the write
sites deferred it (the 2026-08 usage-deferral); the ruling paid that
debt: every assistant commit — the FINAL fold and the roundtrip fold
— carries the turn's provider-reported usage onto the entry. The
trigger input is the branch walk-back: the newest reported
`total_tokens` plus chars/4 estimates for only the tail appended
after it. `total_tokens` is the provider-correct partition of
everything that request processed (Anthropic sums input + both cache
counters + output; OpenAI's prompt figure already includes cached —
summing components per-side would double-count one of them). A
compaction entry ends the walk (every earlier measurement measured a
history the summary replaced); zeros mean "not reported" (the type's
sentinel) and the walk passes them by to the last real measurement; a
branch nothing measured falls to the full estimate. A compaction
**taints older measurements** (found by the coverage round's
overflow-intercept e2e): a request that ran before the insertion
counted the old prefix — its total is an overcount now, and trusting
it kept condition B fired until the cannot-shrink guard failed a
successful pass. The walk honors a compaction horizon compared by
entry id (UUIDv7 millisecond order; the RFC3339 stamps are
second-precision and collide in fast exchanges): only younger
measurements count. The box's `last_usage` session state is deleted
with its justification — the entry IS the measurement, so it
survives reload by construction, and reloaded stats count the same
numbers the live ledger does. The chars/4 heuristic remains only
where no server number exists: the unmeasured tail and
cut-selection arithmetic. The multi-pass post-check's real exit for
a converged history is the cannot-shrink guard (the maximization
plus the tail floor make consecutive passes converge; the pass cap
is the belt), pinned by its own test. Complexity (same ruling):
O(history) is fine — binary search would need a tree re-shape for a
size the context window bounds anyway, and the beat's common case is
a walk-back with no serialization at all.

- Context compaction: summarize old turns when approaching the context
  window (pi: replace history with a summary + recent tail).
- Overflow detection and recovery: detect context-overflow errors from
  the provider, repair and retry rather than fail the session —
  detection is **typed classification at the transport layer** (ruled
  2026-09 with the cut-selection loop below — we own the
  anthropic/openai wire clients, so no regex port; the old
  pi-`overflow.ts` port deferral, and the COVERAGE.md note it pointed
  at, are dead).
- **Ruled 2026-08: compaction gets a real design discussion before any
  code.** No coding agent (pi included) ships a genuinely robust compaction
  pass — treat pi's as a reference, not a target. The design must also cover
  the history/session-tree interaction: what a compaction entry *is* in the
  append-only tree, how checkout interacts with a compacted chain, and what
  replay reconstructs. All three questions are answered by the 2026-09
  rulings below (the record, the insertion, the flow).
  `context_window` config wires with the implementation.
- **Reference survey (2026-09, the design discussion's evidence base):**
  all five references (pi, codex, opencode, crush, yaca) roll their own but
  converge on one skeleton — threshold from real provider usage minus a
  reserve, a cut with a retained recent tail, a structured handoff summary,
  an overflow-error backstop. The check runs between model calls everywhere
  (the tool-roundtrip seam — the only point with fresh usage numbers),
  never after individual tool calls; codex compacts mid-turn at the seam,
  pi/yaca compact at the run boundary, crush stops the run to summarize.
  Two payload camps: **codex appends the summarization prompt to the REAL
  conversation** (same system prompt, empty toolset — the request prefix
  `compact.rs:282-286` — so it rides the prompt cache and the history goes
  verbatim); **pi/opencode/yaca send standalone requests** with the
  conversation lossily serialized to text (tool results truncated ~2k
  chars; a cache hit is impossible by construction, and pi explicitly
  disables cache writes). Post-compaction, both camps pay a full cache
  re-write — the replacement context is a new prefix. The only
  standard-shaped thing is OpenAI's server-side Responses compaction
  (codex negotiates it as a provider capability); nobody has our
  rewind/branch tree, so the tree interaction is ours to design.
- **Ruled 2026-09 — two seams, two thresholds (owner):** the trigger is
  checked at two seams with different jobs. The **pre-request seam**
  (the point you are about to send a request to the model — between
  model calls, mid-run; defined precisely in the own-system ruling
  below) carries the high threshold — "compact now, or the next few
  calls will exceed the context window and fail" (safety; fires
  mid-task only when genuinely close). The **outer-loop idle seam**
  (after run end, back at idle) carries the lower threshold —
  "summarize at a natural pause point, make room for the next task"
  (compacting at 75% when the model has finished its work beats
  waiting for the urgent bound mid-task; the numbers are finalized in
  the trigger-formula ruling below).
- **Ruled 2026-09 — in-conversation summarization (codex-style, owner):**
  the compaction request is appended to the real conversation — same
  preamble, **no tools**, the instruction riding in the user message
  (swapping in a summarizer system prompt is what breaks the prefix
  cache; the user prompt does all the work). The request prefix-rides
  the existing prompt cache and the history goes verbatim — no
  serialize-to-text loss. Reason on record: input-cost savings ("a penny
  is a penny" — cache-read dominates real coding sessions, so the saving
  is admittedly small). The reconstructed-request style stays the
  recorded alternative: the request shape is one construction site, and
  the cut/projection machinery is shared by both styles, so switching
  later is contained.
- **Ruled 2026-09 — trigger numbers and queue conditions (owner):** the
  idle seam fires at **75%** of the window, and only when the steer
  queue is empty — a waiting message means not actually idle, and the
  user never waits behind a summary. Messages arriving *during*
  compaction queue normally and run after it (always-queue gives this
  structurally). The seam compaction ignores queued messages entirely:
  they stay queued; compaction lands before request prep, and the
  queue drains at request prep as it always does — ordering only, no
  explicit coordination (ruled 2026-09). The seam reserve is bounded by the **two-turn budget**
  (owner correction of the one-turn derivation): the check fires
  discretely one seam *after* the crossing turn, so the worst case at
  fire time is threshold + one full turn of growth, and the compaction
  call must still fit its prompt and summary output in the window —
  reserve ≥ one turn's growth + summary room ≈ two turns. Reference
  reserves for calibration: pi/yaca 16,384 absolute; opencode
  min(20k, max output tokens); crush 20k absolute above a 200k window,
  20% of the window below it; codex 90% soft / 95% hard — plus a
  runtime escape hatch (on overflow *during* compaction, trim the
  oldest item and retry), evidence the fixed reserves under-provision
  the compaction call and get patched at runtime instead.
- **Ruled 2026-09 — the final trigger formula (owner):** two conditions
  over the configured window `max` — **A**: `context > 75%·max ∧
  mailbox empty`; **B**: `context > max − 32K`. Idle compaction checks
  both (A ∨ B); seam compaction checks only B. The disjunction makes
  the idle bound never exceed the seam bound at every window size by
  construction: at 128k the two coincide; below it idle rides the
  urgent bound (75% would leave too little room); above it idle gets
  the gentle window. B carries no mailbox gate — urgent is urgent (a
  queued message behind an over-window context waits for the
  compaction; the alternative is running it into the wall). The 32K
  reserve is the two-turn budget at ~16k/turn.
- **Ruled 2026-09 — compaction state rejects all tool calls (owner):**
  the compaction request keeps the exact same preamble and toolset —
  prefix-cache identity, since any toolset change (emptying it, or
  trimming to a subset like `read`) diverges the cached prefix at the
  tools position — forbids calls in the instruction, and **rejects
  every tool call** made in compaction state. Entry and query are
  settled by the own-system ruling below (the two doors). The
  rejection's response shape, settled 2026-09 (owner: "not allowing
  tool calls doesn't mean ignore tool calls" — discard-and-retry
  preferred over synthesizing an in-band error result): a violating
  response is **thrown away and the request resent**, bounded by
  `VIOLATION_RETRY_CAP`; each discard closes its bracket as
  `compaction_failed` so the frontend drops that attempt's deltas.
- **Ruled 2026-09 — the cut-selection loop (owner, high-level; agenda
  items 2+5 merged into it):** one procedure, three points.
  (1) **Initial cut:** keep at least `KEEP_TAIL` (an internal
  configurable dial — tokens or percentage) of recent history as the
  retained tail, while ensuring the history **sent** for compaction is
  < 75% of the window. The request is the conversation **prefix up to
  the cut** plus the summarization instruction — never the full
  conversation: prefix caching covers whatever prefix is sent (the hit
  is on the longest common prefix, so dropping the tail does not
  forfeit the cache), and the <75% bound forward-guarantees the
  request fits by construction even when compaction fires over-window
  — the forward-looking cut, not codex's backwards trim-and-retry.
  Both hard constraints push the cut the **same direction** — up the
  history, a shorter prefix: sent < 75% caps how late the cut can sit,
  and tail ≥ `KEEP_TAIL` caps it too (a later cut means a shorter
  tail). The **longest-prefix** objective is the soft pull in the
  *opposite* direction — compaction efficiency: summarize as much as
  one request can carry. Selection = the latest valid boundary
  satisfying both. The session-start cut satisfies both vacuously, so
  infeasibility reduces to a history shorter than `KEEP_TAIL` itself —
  skip compaction, reachable only on MANUAL requests (the auto
  triggers imply a context far past `KEEP_TAIL`). Tail overshoot
  beyond `KEEP_TAIL` is **normal, not granularity-only**: when
  history ≫ window (3M history, 1M window) the cut sits at the 75%
  cap and the 2.25M remainder stays as tail — the post-check rerun
  then makes several passes, each summarizing another ≤75%-of-window
  chunk until the context fits.
  (2) **Rejection and length-cap alike:** on a server rejection of the
  cut point — any non-transient, engine-visible error (the typed
  classification; the transient family already rides the pi-policy
  retry) — or a length-capped summary (protocol-complete but
  information-incomplete: the summary could not fit what the prefix
  contained), move the cut one block up the history (a shorter
  request) and try again. Blocks are the valid cut boundaries
  (no-tool-call outputs).
  (3) **Post-check loop:** after compaction, immediately re-check the
  trigger against the new context (preamble + summary + retained
  tail); still too big → rerun the loop — pass N+1 is **just another
  regular compaction**: the walked history already carries pass N's
  summary as its first item, so there is no previous-summary
  machinery (ruled 2026-09; pi's update pattern is not adopted).
  Sub-decisions left to the dig: the retry loops' termination — the
  rejection/length-cap loop has a natural floor (the empty prefix,
  past which the pass fails loud), the post-check rerun needs the
  cannot-shrink guard (yaca's rule) — and the rerun threshold
  (leaning: condition B again).
- **Ruled 2026-09 — the compaction record, file and tree (owner):**
  the session file gets ONE compaction entry per pass, append-only —
  the kept tail is already in the file, so the entry carries the cut
  identity: **the id of the message immediately after the cut point**
  (the first tail entry), its own parent being the node before the
  cut, plus the summary payload. The runtime tree performs a real
  **insertion**: the compaction node becomes the parent of the
  cut-point child and the child of the original cut-point parent, and
  history walkers **stop at the compaction, included** — the walked
  context is [summary] + retained tail; everything before the
  insertion stays in the file and the tree, un-walked. In file terms
  the insertion is *derived*: the tail entry's record keeps its
  original parent, and the loader re-parents it through the
  compaction entry on replay (append-only preserved; the derivation
  lives in the one fold the parser and the resident context share; a
  record whose parent ≠ its cut child's file parent is a loud parse
  error). Consequences: the **head does not move** at compaction —
  later appends attach to the unchanged head and their effective
  chain routes through the insertion; **checkout/rewind to
  pre-compaction nodes yields the full-history branch** (the
  compaction is not on that path — "compaction never deletes"
  realized as tree topology, no projection machinery; rewind-to-X
  itself is degenerate but consistent, and the user surface never
  lands there); **multi-pass composes as successive insertions**
  (each pass's entry parents the node before its own cut and names
  its own cut child — no entry is rewritten); a **torn compaction
  entry loses only the pass** — reload yields the full history, the
  write-behind contract's accepted loss. Still open (implementation-
  time): the entry's exact payload fields (tokens-before, usage, pass sequence,
  the instruction for audit), how the summary enters the
  model-facing context (leaning: a user-role wrapper message, the
  references' pattern), and the session-format version bump.
- **Ruled 2026-09 — compaction is its own system (owner):** a
  dedicated procedure in tabit-session beside `run_one` — a focused
  black box to the rest of the system. The box owns the trigger
  evaluation (the A/B formulas), cut selection, the request assembly
  (prefix-truncated, same preamble and toolset), the shorten-retry
  and multi-pass loops, and the compaction entry write; no flags in
  the engine, no compaction knowledge anywhere else. Its interface is
  **two doors** — the caller names the seam, the box picks the
  formula: (1) the **pre-request point** — the point you are about to
  send a request to the model, every request in a run, the first
  included (a resumed over-window session is caught at run start);
  the pre-flight overflow case is this same check, not a separate
  path — condition B; (2) **idle** — between runs in the session
  actor, condition A ∨ B. Out of the box: its own events, the
  compaction entry, and the compacted context the run or the next
  prompt continues from. The ruled queue behaviors are structural
  (the pump isn't running, so the mailbox waits; the summary turn
  records as one entry, never a message pair). Whether the
  pre-request seam rides the existing tool-phase hook pair or a new
  pre-request edge is the ENGINE.md amendment's first decision —
  which precedes code (rule 11).
- **Ruled 2026-09 — pre-implementation clearances (owner):**
  (1) **Events:** compaction start/end events, with the summary
  streaming as text deltas on the session's stream (the frontend is
  already a streaming consumer; a silent multi-second call is the bad
  UX). The events, the `compact` command, and the protocol version
  bump land together, with FRONTEND.md and the GUI changelog.
  (2) **Abort:** compaction is a long async operation — the main flow
  stays responsive while it runs. An abort does the usual mailbox
  discard plus terminating the compaction stream; a cancelled or
  failed pass persists nothing.
  (3) **Manual compaction:** a third door — a `compact` command type
  carrying the session id and optional compaction directives; the
  frontend's presentation of it is its own business. The
  short-history skip is its guard.
  (4) **Unknown `context_window` — the wall teaches the window:**
  every designed constraint needs a known window, and the overflow
  error carries it — Anthropic: `prompt is too long: X tokens > Y
  tokens maximum`; OpenAI: `maximum context length is N tokens … you
  requested M tokens (… in the messages, … in the completion)` — and
  the typed transport error preserves `{status}` and `{message}`, so
  the numbers are reachable. An unknown window therefore skips the
  threshold triggers (with a warning) while overflow recovery still
  functions, learning the real window from the error; the learned
  window serves the rest of the session.
  (5) **Dials are data:** the instruction prompt text and every
  threshold (75%, 32K, `KEEP_TAIL`, the summary output cap) are data
  fields clustered in one or a few files — review and polish happen
  in one place.
  (6) **Multi-pass is just another regular compaction** (amended into
  the cut-selection ruling above — no previous-summary machinery).
  (7) **Ordering, not coordination** (amended into the trigger
  ruling above — compaction lands before request prep; the queue
  drains at request prep as always).
- **Open agenda (quick thoughts recorded 2026-09, owner — each gets a
  deep dive; leanings marked):** (1) trigger conditions — **settled**
  (formula above); the token-counting input stays a leaning (last-turn
  provider usage, pi's four-component sum, plus chars/4 of trailing
  messages). (2) **cut points — merged into the cut-selection loop
  ruling above** (valid boundaries are after model outputs without
  tool calls; queued steers cluster before the first user message;
  flexible tail budget via `KEEP_TAIL`). (3) **settled** — compaction
  state rejects all tool calls (ruling above). (4) **flow fit —
  three separate designs, not to be mixed**: the session file
  **settled** (the compaction record ruling above), the runtime
  session tree & state **settled** (the insertion ruling above), and
  the execution flow **settled** (the own-system ruling above, a
  black box with two doors; the ENGINE.md amendment precedes code). (5) **overflow recovery — merged into the cut-selection
  loop ruling above** (pre-flight fit by construction + rejection
  shortening + the post-check loop).

### 7. CLI / interface layer

- **Print mode shipped** (`crates/tabit`): one prompt in, live events out,
  project-local sessions, `-p <PROMPT>` / `--continue` /
  `--session <path>` / `--list` / `--rewind <n>`, `--model provider/model`
  or `default_model` in providers.toml. The GUI is the default mode
  (shipped — bare `tabit [path]` launches it);
  `-p` and `--rewind` opt out into print mode.
- The protocol's design record — locked decisions plus every open
  flag with options — lives in PROTOCOL.md; flags are resolved in
  discussion order there.
- **Frontend architecture (decided, v1 shipped): frontend-through-protocol.**
  One typed vocabulary. Commands are fire-and-forget with total
  semantics — `message { text }` (steers the run in flight, or starts
  one) and `abort` (aborts + discards the queue) — nothing can be
  rejected, so there are no ids and no request/response; outcomes are
  events. Every event is stamped with a `StreamId` ("main" today;
  subagents mint siblings). Typed serde enums over in-process channels
  (`SessionHandle` actor in tabit-session), serialized only at a
  transport edge (LF-JSONL on stdio). Tagged frames, not JSON-RPC 2.0;
  versioned `initialize` handshake at the stdio edge. Informed by codex
  (single-table protocol crates, thread stamps), pi (ids optional,
  clients run on events), claurst (the channel seam across three
  frontends), and the protocol-design discussions that eliminated
  acks/rejections as cases that cannot fire.
- **JSON mode shipped** (`--json`): the first protocol consumer and its
  test harness — `initialize`/`message`/`abort` in, stamped events out
  on stdout, human banners on stderr, stay-alive between runs. The
  always-queue refactor underneath: a run-agnostic `Mailbox` replaces
  the run-scoped steer slot (messages can never be lost — the only
  discard is abort), `pump`/`run_one` extracted from `prompt_with`,
  `RunFailed` joins the event vocabulary, and print mode drives the same
  `SessionHandle` path.
- **Rulings folded in** (post-JSON-mode pass): every drain point takes
  the whole queue at that instant (idle entry batches all pending
  messages into one run's opening input; the engine drains the rest as
  steers at turn boundaries); `prompt`/`prompt_with` are thin wrappers
  over `submit` + `pump` — failures are events and
  `RunOutcome::Failed`, no `Err` return (one drive path, one contract);
  session files materialize at the first user message (a session that
  never runs leaves nothing on disk — no header-only orphans, and
  `--list` reads a missing sessions directory as empty);
  `StreamingChat::stream_chat` takes a full conversation — the final
  message is the turn being sent, callers add messages to history
  before the call, and retries resend the same list verbatim;
  malformed tool-call arguments are a model-side defect — the turn is
  discarded (never entering history on any provider) and the request
  retried once, exhaustion fails the run with history clean (PROTOCOL.md
  flag 21, recorded with the outer-loop diagram).
- **Model command shipped** (stage 3, 2026-08): `model { session,
  provider, model, thinking_level? }` switches a session's selection —
  the register write under the session-preference ruling, and **a
  state write, not conversation intent**: the whole command happens
  at receive. Validate against config (the `ModelProbe` handle —
  immediate `error { kind: model }` for a bad ref), then one shared
  register write (`ModelRegister`: the `model_change` entry and the
  live selection cell, atomically, from any thread — the recorder's
  append is internally locked, and the planned write-behind log turns
  it into a queue enqueue with a flush attempt per write), then
  `model_changed` (one construction site shared with the replay
  passes). The worker is uninvolved — no park, no wake, no beat
  ordering, and abort has nothing to say about it: the next run open
  derives the agent, every pass announces the cell. The GUI grows a
  minimal test field (`provider/model` free text); the real picker
  waits for a models-list command (deferred with the redesign).
  Deferred with it: the global implicit preference (`~/.tabit/`
  last-selected file + registry rung below `default_model`) and the
  "selection didn't land" picker signal (open note in PROTOCOL.md).
- **GUI: egui, the primary frontend (decided).** The TUI milestone
  (the claurst harvest, ~19K LOC) is dead — GPL, ruled out; the
  terminal frontend found its own non-ratatui track (the TUI ruling
  below). The GUI is an egui app (eframe shell, egui style theming)
  speaking the item-7 protocol over the existing stdio edge: it spawns
  one `tabit --json` child process — the multi-session host (PROTOCOL.md
  v3): sessions are created, opened, and switched by channel commands,
  never by process tricks (the GUI-respawn interim is deleted). Process
  separation is the point, twice over: internal errors panic by doctrine,
  and the GUI must survive a backend crash (restart the session, keep UI
  state); and it is exactly the vscode-remote shape — SSH remote is the
  same child spawned on the far side of `ssh`, stdio forwarded, no new
  transport (this likely retires item 8's named-pipe/local-socket plan).
  Widget ecosystem (surveyed 2026-08): markdown via `egui_commonmark`
  (actively maintained, GitHub-flavored extensions); syntax highlighting
  via `syntect` (egui's own code-editor demo is the pattern); diffs over
  the `similar` crate with a hand-rolled viewer. An embedded terminal
  (interactive bash) has no battle-tested egui widget — `egui_term` /
  `egui_tty` (Ghostty's VT engine) are candidates; defer until an
  interactive PTY is a real requirement. Transcript list, input editor,
  and overlays are ours on egui layout primitives.
  **Build order** (the GUI is the owner's feedback instrument, so it
  starts before the v2 backend completes): `tabit-protocol` extraction
  → walking-skeleton GUI on the shipped v1 wire (spawn `tabit --json`,
  transcript, input, steer, abort, crash handling) → v2 backend slices
  land behind it, the GUI growing each slice (ids → turn anchors,
  replay → restart-safe transcript, checkout → rewind buttons, model
  command → picker, write-behind → degraded banner).
  **Redesign at the polish phase (ruled 2026-08, owner, after the v3
  review round).** The walking skeleton served its purpose; its
  reducer's state model — single-session globals with multi-session
  semantics bolted on as conditionals — cracked repeatedly
  (session_created dropped by the stream check, replay passes poisoning
  liveness, cards dying at view switches), the same seam each time.
  The **trigger**: after checkout, `model`, and write-behind's
  per-session seq land — the remaining events that touch reducer
  surface; the trigger has since fired (checkout, model, and
  write-behind's per-session seq all landed) and the redesign is in
  progress on the `tabit-gui-work` worktree (branch `gui/redesign`,
  kicked off 2026-09) — until it lands, master-side GUI changes are
  minimal interim patches with the
  seams marked, not investments in the doomed shape. The **scope**:
  the state model and view layer are redesigned; `backend.rs`
  (process/pipes/handshake, bug-free through v3) and the InMsg
  vocabulary carry over. The new state model is dictated by the
  protocol: a per-session projection (`session_id → {transcript,
  running, pending, cards}`) plus a thin connection layer (phase,
  facts, catalog), with attribution-by-stamp as the fold's primary
  dimension and the learned event classes as its dispatch table
  (connection-level vs stream-scoped vs bracket-suppressed vs
  liveness). **Preconditions**: a short design record for the state
  model precedes code (the GUI's ENGINE.md equivalent), tests derive
  from it, and fixtures build frames through shared `tabit-protocol`
  builders so a fiction shape (a frame the backend cannot produce)
  cannot compile. Known stage-1 behaviors deferred to the redesign
  (2026-08, live testing + review): switching back to a mid-run
  session shows an empty transcript until that run's terminal (the
  optimistic clear waits for the replay pass, which correctly parks
  behind the run — the parked-replay ruling), and `Facts` follows
  only `session_created` — a switcher switch leaves the status strip
  naming the previous session's model until the opened session's
  register announcement arrives with its pass (deterministic since
  the register ruling; for an in-flight session the pass still parks
  behind the run's terminal — the same window as the transcript).
  Both die with the per-session projection.
- **TUI: back on, the Node route (ruled 2026-09 after the research
  round; full survey in TUI-RESEARCH.md, branch `tui/research`).**
  The claurst ratatui harvest stays dead (GPL) and a Rust-native
  TUI stays deferred (a later claurst-ideas rewrite remains open);
  the terminal frontend rides the JS ecosystem instead: **the omp
  fork of pi-tui (`@oh-my-pi/pi-tui`, MIT — Mario Zechner's own
  next-gen line, not a third-party fork) under Bun**, spawning
  `tabit --json` as a child process (the stdio edge GUI, print, and
  JSON mode already ride — zero backend changes), distributed via
  the npm registry as per-platform optional packages (esbuild
  pattern; no postinstall) carrying a Bun-compiled standalone TUI
  exe plus the cargo-built core — the installer is whatever the
  user has (`npm i -g` / `bun i -g`), no JS runtime at run time.
  Single repo, single tag, single version: the lockstepped pair is
  the strict protocol handshake made atomic. Fallback ladder: stock
  pi-tui on plain Node, then opentui (its Node ≥ 26.4 engines floor
  breaks the one-line install today). Next: the §7 walking-slice
  spike on Windows Terminal before product commitment; the TUI
  enters the monorepo (`tui/`) when it graduates.
- **Framework: egui (ruled 2026-08, after evaluation).** Runner-up
  iced (its Elm architecture matches our reducer split natively) loses
  on ecosystem for our exact surfaces — no markdown widget, no list
  virtualization, no terminal story, thinner agent-training corpus.
  Webview stacks (Tauri) rejected on the opencode lesson: system
  WebKit rendering skew drove them to bundling Chromium; a browser
  bundle or a JS boundary both cost more than egui's ceiling costs us.
  Slint (license complexity), Xilem (not ready), gtk4-rs (Windows
  story), Flutter (language boundary) dismissed. Revisit triggers: an
  interactive terminal becomes core (xterm.js is unmatched), or egui's
  text ceiling proves too low for the transcript quality wanted. The
  reducer stays framework-free and pure, so a future switch rewrites
  only the view layer.
- **Entry-point architecture (ruled): `tabit` is a launcher, the GUI
  spawns the core.** `tabit [path]` spawns `tabit-gui <path>`
  detached — own process group on Unix, detach flags on Windows, the
  vscode survive-the-terminal trick — and exits immediately; `-p` /
  `--json` keep their foreground modes; bare `tabit` stops erroring
  and opens the GUI. Per window the GUI owns one `tabit --json` child
  per session: crash isolation follows the panic doctrine, and local
  and ssh spawning are the same shape. Singleton handoff (vscode's
  running-instance IPC) deliberately deferred — each launch is an
  independent window.
- **GUI design contract (ruled for the polish pass).** Reducer/view
  separation is strict: the reducer is pure, framework-free, and
  unit-tested; the egui pass is a projection containing no business
  logic. Theming via crates over egui's data-driven style
  (egui-elegance-class tools), never hand-rolled color tweaks at call
  sites. Rich rendering behind single-function seams (plain text now;
  egui_commonmark / syntect swap in later). View-only state lives in
  its own churnable display struct, never in the reducer. The
  transcript renders through ScrollArea's viewport pattern from day
  one. **Ecosystem-first rule: before hand-rolling anything
  non-trivial in tabit-gui — theming, markdown, terminal emulation,
  docks, toasts — pause and research existing crates, or ask the
  owner to search.**
- **License (decided): all-MIT.** The GPL split existed only to admit the
  claurst harvest; with the TUI dead there is no GPL dependency and no
  reason to go GPL (enforcement isn't free either). AGENTS.md rule 10
  updated to match. Frontends stay leaf consumers of the protocol —
  dependency direction remains one-way by architecture, not license.

### 8. Client/server + protocol

- The protocol is the item-7 vocabulary, defined once and shared by every
  transport: in-process channels first, stdio JSONL with the JSON mode,
  named pipe / local socket only when a remote client exists. The
  vocabulary lives in **`crates/tabit-protocol`** (extracted from
  tabit-session; flag 13) — engine-free, protocol-owned shapes, so
  frontends (the egui GUI included) share the serde types without
  touching persistence internals.
- **ACP (Agent Client Protocol) ruled adapter-only (2026-09; survey in
  PROTOCOL.md flag 32):** the native vocabulary stays the one contract —
  ACP is too little where tabit is deliberately rich (steering, the
  checkout tree, subagent streams, custom widget UI, durability
  signals), too much where tabit is deliberately lean (the JSON-RPC
  envelope, capability negotiation, auth/MCP/plan/slash-command
  machinery), and the wrong shape for the in-process GUI consumer. The
  reach play is an optional `tabit-acp` adapter crate — a leaf frontend
  projecting stamped events onto `session/update`, the pi/pi-acp
  pattern — deferred **at least until ACP v2 ships and stabilizes
  through a few patch rounds**; that is the re-evaluation trigger.

### 9. Extensions

- Extension support: a way for users to add tools and hooks without
  forking — likely WASM or script-based tool providers plus the existing
  hook points, informed by opencode's extension/plugin design.
- Settings surface: layered config (user > workspace > flags) already partly
  from item 1; extensions register tools, hooks, and prompt contributions.
- **Session-scoped extension memory** (shape designed here, deferred from
  the subagent rulings): generic state a tool or extension registers,
  shareable with subagent sessions by handle; the permission gate's
  "Always allow" set is the first consumer (today the ad-hoc
  `PermissionMemory`). Open rulings: entry keying (typed vs named) and
  the durability split (session-scoped state in the map; "always allow
  globally" belongs to user config).

### 10. Prompt caching (required before release)

- **Shipped (2026-08) — all-1h, one policy site** (owner ruling: keep it
  simple now; a modeled policy is a contained edit later). The full
  policy lives in `ModelRegistry::build` (`tabit-session/registry.rs`),
  nothing else needs touching to change it:
  - Anthropic: `with_automatic_caching_1h()` — the API owns breakpoint
    placement and moves it forward every turn (rig-core's automatic mode
    was already vendored; the 0.41 code carried per-breakpoint TTL, so
    the old note about upstream `4be867de` is moot). 1h over 5m: the 2x
    write premium buys survival across interactive gaps and >5m tool
    turns; reads are 0.1x and refresh free under either TTL.
  - OpenAI Responses: caching is server-side automatic; we only pin
    routing — `prompt_cache_key` = the session's stable id (the
    codex/pi/opencode pattern). **Subagents get their own keys**
    (ruled 2026-09): a child's context shares only the base prompt
    with its parent, so a shared key buys little without an inherit
    mode while concentrating every child's divergent suffix on one
    cache route; if an inherit-conversation mode ever lands, key
    sharing is its design question, priced then.
    Per-model `with_cache_key` in rig-core, clamped to 64 code points,
    explicit request-level `additional_params` wins.
  - Chat-completions gateway: no key (third parties vary in what they
    accept).
- Deferred until a felt need: a modeled breakpoint/TTL policy (mixed
  1h-prefix/5m-tail only protects the static prefix — after a 5m lapse
  the whole message history re-writes), the completions-gateway key,
  OpenAI's `prompt_cache_retention` (Responses-only, unused by codex/
  opencode/pi). Usage-side parsing (cache read/creation tokens, TTL
  breakdown) already ships in rig-core.
- Falls out of the v2 backend slices (write-behind log + prompt barrier)
  where the static prefix becomes an explicit unit.

## Explicitly not planned

(kept in sync with AGENTS.md)

- WebSocket streaming (removed).
- SSE resumption / reconnect.
- rmcp integration (kept, feature-gated, off by default — decide later
  whether tabit ships an MCP client; low priority).
- Mid-conversation system messages.
- Model catalog / name-keyed behavior.
- Vendor instruction files (CLAUDE.md etc.) — AGENTS.md only.
- Instruction-file directory walking — home (`~/.tabit` → `~/.agents`
  fallback) and cwd only.

## Deferred until a consumer exists (phase 4 leftovers)

- Orphan-result repair utility.
- Typed `provider_status` on `CompletionError`.
- Eval harness (pi has one; build when there are sessions + tools to eval).
- MCP client support — verify pi's current story before committing.
- OAuth device-flow auth for providers (optional, late).

## Deferred round: post-review architecture remediation (2026-08)

Recorded from the five-reviewer fresh-eyes pass; ruled to wait until the
top findings (rmcp stance, stop semantics, entry-id ownership, the
dual-fold clarification) are settled:

- **Split `tabit-session/src/session.rs`** (~1,850 lines, ten concerns:
  mailbox, abort, steers, the Session core, SharedConversation,
  ModelRegister, EventSink, DriveOutcome, the item→event translation,
  assembly helpers) into own modules — done (2026-08): `session.rs`
  became a `session/` directory (`mod.rs` core + `mailbox`, `builder`,
  `run`, `rewind`, `selection`, `persist`, `assemble`, `wire`), a pure
  code move with `pub(super)` as the exact pre-split visibility and
  `ModelStats`/`SessionStats` joining the usage ledger in `stats.rs`.
- **A named notice-channel abstraction** for the ~10 copy-pasted
  weak-sender `EventFrame` emission sites (one documented home for the
  termination discipline) — done (2026-08): `notice.rs`'s `NoticeSink`
  (channel + stream stamp as one value) and `NoticeSlot` (the
  attach-once cell). The mailbox's two-`OnceLock` attach invariant and
  its `expect` are unrepresentable now, and persist's `Mutex<Option>`
  died with the verification that no re-attach exists (attach happens
  exactly once, at worker spawn). The hub's ask keeps its dismissal
  semantics through `emit`'s liveness return.
- **One home for the call/result pairing walk** — the same
  every-call-answered-exactly-once verification exists in
  `tabit-log/fold.rs`, `tabit-session/parser.rs`, and
  `ContextManager::fold_all_entry` — resolved (2026-08) the other way:
  the unified commit made closedness a *local* property, so the walks
  died instead of merging. `fold_all_entry` keeps the one full
  validation (the commit batch); every other site checks only a tail —
  `tail_is_closed` walks back one batch's span, serving the live
  checkout door and the parser's torn-tail check. The parser runs one
  streaming pass under a documented threat model: torn tail, bad JSON,
  dangling parents, and unknown checkout targets are detected;
  mid-file corruption that stays valid-JSON-with-valid-parentage is
  trusted away (one-blob commits make it unproducible by the app, and
  below-app damage severe enough breaks JSON or parentage first).
  `path_is_closed`, `validate_node_order`, the side-record interleave
  check, and the parser's per-checkout and final-head walks are all
  deleted.
- **The dual-fold unification** — done (2026-08, commits `188ed17` +
  `a9e7cf0`): one durable `ContextManager` behind the session's cell,
  the engine's folds are the durable commits, the session
  emission-only. The mid-run readability constraint held (brief
  write holds; the checkout probe reads between folds).
- **rig-core vendored-mass policy** — resolved (2026-08, rulings in
  VENDOR.md "RAG mass removal"): embeddings + vector stores + retrieval
  plumbing deleted (no consumer, none planned); model listing kept
  (cassette-covered, the planned registry consumer); telemetry trimmed
  to bare identity spans (the GenAI conventions module and the
  content-recording opt-in are gone).

## Deferred round 2: engine surface trims + test review (2026-08)

Recorded from the public-API discussion after the conversation
unification; ruled to wait until the discussion series concludes:

- **Batch the steer announcement into one yield** — done (2026-08,
  `ee58a93`): one `Steer { batch }` item per drain, the fold and the
  yield sharing one uninterrupted poll; ENGINE.md carries the rule
  (**a suspension never sits between a commit and its announcement**)
  and the channel split (stream = progress; the mailbox's notice
  channel = ledger).
- **Delete `ConversationMemory` wholesale** — done (2026-08,
  `06ccb0b`): the module, the knobs, the load/append pair, the
  `memory_handle` threading, the error variant, the facade re-export,
  and the memory test families. `build_run` lost its only-for-memory
  `history_override` parameter.
- **Drop `PromptResponse.messages` and the `entry_len` window** — done
  (2026-08, `e24d2b9`): outcomes only; the conversation is the
  transcript. Error paths lost their embedded history copies the same
  day. Callers migrated to the cell door (conformance harness, parity
  tests — now comparing the durable conversations both surfaces fold —
  cassette suites, `Chat::chat`'s mirror).
- **Parity-test review** (owner lens: "if you need two things to work
  identically, first consider whether there should be two at all") —
  reviewed 2026-08. Findings: the loop is ONE implementation
  (`drive_agent`); blocking/streaming differ only in `TurnSource`, so
  the parity family guards the adapter seam, not a duplicated loop —
  no collapse available there. The lens does catch two things:
  1. **The blocking surface has zero tabit consumers** — resolved the
     deletion way (2026-08): the `Prompt` trait, `PromptRequest`
     typestate, `AgentRunner::run`, `UnaryTurnSource` + the blocking
     `follows_from` chain, and the facade re-exports are deleted; the
     streaming surface is the one execution surface (`fold_stream` is
     the outcome fold for in-crate consumers; `MockTurn::
     into_stream_events` bridges unary-scripted mock scenarios onto
     it). `PromptError` stays — it is the streaming error payload the
     session wraps. Cassette suites followed the same split: blocking
     twins deleted with their recordings (the cassette-safety check
     enumerated every orphan); single-turn wire-mapping smokes now
     drive the unary provider path directly (same cassettes, same
     request bodies).
  2. The ~8 ad-hoc blocking/streaming builder pairs in
     `runner_tests.rs` — moot: the pairs and their parity family died
     with the blocking surface.
