# ROADMAP

What we have, and what to build to reach feature parity with pi
(reference: `~/Projects/agents/pi`; also opencode, crush, codex, yaca).

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

Known deviation from pi: **native in-process subagents** instead of pi's
subprocess model. Everything else follows pi's minimal path.

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
  client-caching decision), `context_window` (compaction — item 6),
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

The standard toolset as `#[rig_tool]` implementations. **Started:
`crates/tabit-tools` ships `read`, `ls`, and `bash`** (timeout, output
caps, Git Bash preferred on Windows with PowerShell fallback, via the
PortableTool→DynamicTool erasure). Still to add: edit (with diff preview),
glob, grep, write. Permission/approval rides on the existing hook system
(as a first-party extension — see item 9).

### 5. Native subagents

The known deviation from pi's subprocess model:

- In-process subagents driven by the same `AgentRun` state machine: a parent
  spawns a child run with its own model/preamble/toolset; child streams
  progress to the parent's transcript; result returns as a tool result.
- Isolation boundaries: which tools a subagent may use, recursion depth,
  context accounting so child output can't silently blow the parent budget.

### 6. Compaction + overflow recovery

- Context compaction: summarize old turns when approaching the context
  window (pi: replace history with a summary + recent tail).
- Overflow detection and recovery: detect context-overflow errors from the
  provider, repair and retry rather than fail the session.
- Port pi's overflow heuristics when a consumer exists (deferred helper —
  see COVERAGE.md phase 4 note).
- **Ruled 2026-08: compaction gets a real design discussion before any
  code.** No coding agent (pi included) ships a genuinely robust compaction
  pass — treat pi's as a reference, not a target. The design must also cover
  the history/session-tree interaction: what a compaction entry *is* in the
  append-only tree, how checkout interacts with a compacted chain, and what
  replay reconstructs. `context_window` config stays unwired until that
  design exists.

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
- **GUI: egui, the primary frontend (decided; supersedes the TUI plan).**
  The TUI milestone (the claurst harvest, ~19K LOC) is dead/low priority —
  only reconsidered if everything else lands and a terminal frontend is
  still wanted. The GUI is an egui app (eframe shell, egui style theming)
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
  surface; until then GUI changes are minimal interim patches with the
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

### 9. Extensions

- Extension support: a way for users to add tools and hooks without
  forking — likely WASM or script-based tool providers plus the existing
  hook points, informed by opencode's extension/plugin design.
- Settings surface: layered config (user > workspace > flags) already partly
  from item 1; extensions register tools, hooks, and prompt contributions.

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
    codex/pi/opencode pattern; subagents will share the parent's key).
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

- Overflow-detection helper (port pi's `overflow.ts`).
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
