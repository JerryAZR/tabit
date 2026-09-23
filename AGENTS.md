# AGENTS.md

Guidance for AI coding agents (and humans) working on **tabit**.

## What this is

`tabit` is an agent framework that started from a **vendored, trimmed copy of
rig 0.41.0** (upstream: `rig-rs/rig`). The rig source was borrowed as source
rather than an external crate precisely so it can be modified freely: **it is
tabit's code now** — change, extend, or delete any of it wherever that makes
sense. `VENDOR.md` is the historical record of the initial vendoring (what was
trimmed and why), not a constraint on future edits.

Positioning (owner ruling 2026-09): tabit is a **study/research project in
agent architecture, not a pi competitor**. pi is the feature reference and
survey source, never the target — don't chase parity for its own sake, and
don't add machinery because pi has it. The reasons to exist: the native Rust
stack, the egui GUI as a first-class frontend, planned in-process subagents,
and a design we fully own.

## The node model (2026-09 ruling)

Every tabit process is a **node** with one bidirectional interface:
it serves the process that spawned it over a frozen pipe, and may
spawn nodes that serve it. A subagent is a node, a frontend is a
node, an extension is a node — the vocabulary is shared, the
mechanisms are shared. Process management is a tree (spawn,
ownership, lifecycle — the spawner decides); **dataflow is a net**
over those edges: a node's events fan to all its subscribers, never
"the one and only frontend"; commands arrive from any link; asks
are answered by id from any channel — first arrival wins, late
answers are tolerated no-ops. **Mechanisms shared between core and
the SDK live in `tabit-wire` — the one crate below both, and the
only home for them** (`asks.rs` — the one pending-answer registry
every node instantiates; `client.rs` — the one child-management
mechanism; `process.rs` — the child substrate). Core never depends
on `tabit-ext-sdk`: the SDK is the guest authoring library, and
anything the host needs from it is node-mechanics that belongs in
the wire.

**Child management is one mechanism with two policies.** A node's
child frames arrive through the shared client's pump and fan to
local consumers and upstream relay, with the settle fold watching
the same stream — all of it `tabit-wire`'s `ChildHandle` (pump,
tap, fold). Core's bridge is the fixed policy (learn + relay
always on; the fold consumes the terminal, most frames ignored);
the SDK's wrapper is the general policy (registered handlers via
the shared router; relay opt-in — the SDK's "frontend" is
its parent node). What differs between the two drivers is policy,
never mechanism.

Current workspace layout:

- `crates/rig-core` — provider API clients, streaming, tools (providers kept:
  **anthropic + openai** + the shared openai-compatible engine in
  `providers/internal`)
- `crates/rig-agent` — agent loop / runtime, plus the host-service
  capabilities (`tool/services.rs`: `HostServices` — the extension
  envelope's ask + `model_prompt`, carried by contexts like
  `UserInteraction`)
- `crates/rig-derive` — `#[rig_tool]` proc macros
- `crates/rig` — facade crate re-exporting the three above
- `crates/tabit-protocol` — the shared vocabulary crate (commands,
  stamped events, handshake frames; `FRONTEND.md` is the contract),
  plus `points` — the hook-point declarations (the per-point ruling,
  2026-09: each point names its wire name, its answer type, and its
  neutral; the SDK and the host serialize the same types, no
  hand-kept wire mirror)
- `crates/tabit-config` — provider/model configuration plus the
  settings layers (`settings.toml`: the extension disable list —
  packages mount by default; the built-in gate opt-out
  (`[gate] enabled = false`) — the gate mounts by default; user +
  workspace union, `$TABIT_SETTINGS` replaces the user file; see
  `ROADMAP.md`)
- `crates/tabit-log` — the durable-conversation layer between
  providers and agents: the session log (the entry vocabulary and
  tree, format-versioned), the write-behind writer, the parser, the
  context manager (the resident tree + the model-facing history
  view), the delta-token regime compaction reads — engine-free,
  consumed by rig-agent and tabit-session
- `crates/tabit-wire` — the frozen wire's client role, the shared
  node mechanisms, and the child-process substrate (the 2026-09
  extraction: share what is the same): `router.rs` is THE event
  router (register by kind or wildcard, dispatch, retract by owner —
  each callback owns its own dispatch); `asks.rs` is THE
  pending-question registry (one entry per open round-trip: an owner
  key plus a delivery closure over answered-or-orphaned — answers
  are races, the first wins); `routing.rs` is the ChildRouter
  (route-all line forwarding with learned grandchild tables — the
  Ethernet-switch model); `client.rs` spawns a tabit-core child in
  `--json` role (the child-role CLI knobs as one builder) and speaks
  the frontend protocol to it — the bounded handshake, the frame
  pump (forward-don't-re-stamp, with the router's learn/forward
  tap), the command writer, the reaper; `process.rs` (moved from
  tabit-ext) is the substrate every spawning site shares (tree-kill
  wrapping, the stderr ring, the grace reaper). Consumers: the
  subagent bridge, the extension host, and the extension SDK; the
  wire's serve side lives with the host (`tabit-session`'s edge
  module) — one server, no sharing need
- `crates/tabit-session` — persistent sessions over the outer loop (native
  only: filesystem-backed; the rig crates keep wasm support), the
  compaction box (`src/compaction/`: the pass machinery, the doors, the
  dials file — every threshold and prompt text as data), the
  skills module (`src/skills.rs`: four-source discovery, the prompt
  catalog, the confined `skill` tool), plus the
  serve side of the frozen wire (`src/edge.rs`: the json stdio edge —
  handshake serving, the command loop, the event forwarder with the
  grammar glue), the
  subagent framework (`subagent.rs`: `SpawnContext` — spawn/drive a
  subprocess child, the one substrate; `subprocess.rs`: the bridge —
  the session adapter over `tabit-wire`'s client (router taps, the
  drive fold, the ruled abort shape); the `subagent` tool is the
  opinionated example shape extensions override — ROADMAP item 5)
- `crates/tabit-tools` — coding tools (`read`, `write`, `edit`, `bash`
  — chosen at registration: verified Git Bash, else PowerShell on
  Windows) as
  contextual `#[rig_tool]`s (they read the session cwd and run token
  from the per-run `ToolContext`), erasable to DynamicTools (native
  only)
- `crates/tabit-gate` — the default permission gate: pi-sanity's
  heuristic policy ported verbatim (static checks, allow-when-unsure —
  a careless-mistake catcher, never a security boundary; brush-parser
  replaces the unbash parser) as a pure core crate. The `AgentHook`
  member, the `native:select_one` ask, and the settings.toml
  `[gate] enabled = false` opt-out assemble in the `tabit-core`
  binary — `tabit-session` stays a mechanism with no policy
- `crates/tabit-ext-install` — extension installation (ROADMAP item
  9, task 6): npm (plain registry HTTP)/git/path sources,
  stage-validate-place installs, name-only `requires` pulls, list,
  and the refusal uninstall — the directory is the single truth (no
  registry, no lockfile)
- `crates/tabit-ext` — the extension host (ROADMAP item 9): manifest
  discovery (`tabit.json` under the extensions root), the frozen
  JSONL extension pipe (initialize/ack, the tool lane, the flat
  grammar), the supervisor (launch over the
  disable-filtered scan, handshake, supervise,
  mark-dead-and-report — no mid-run respawn; the tool-call dispatch
  surface for proxy tools); the child-process substrate it spawns on
  lives in `tabit-wire` (moved 2026-09 — every spawning site shares
  it); the hook lane forwards
  engine hook events over the same pipe (policy fails open on a dead
  extension)
- `crates/tabit-ext-sdk` — the extension SDK, the guest side of the
  same pipe: authors register tools, consultations, and watched event
  kinds; the SDK owns the loop (handshake from the registration,
  every invocation on its own worker thread — handlers block, ask,
  emit, command, concurrently — and the unconditional drain) and
  hands each handler one context (command, emit, ask over the
  grammar, complete, the cancelled poll). Shares the host's wire
  types (the 2026-09 sharing ruling: one wire, one set of shapes;
  EXTENSIONS.md stays the contract for other languages, the
  conformance tests keep crate and docs honest). Ships the example
  extensions (`echo-ext`, `shadow-ext`, the clash pair, `lmstudio-ext` —
  the provider relay speaking LM Studio's native REST API behind a
  `providers.toml` fragment; `autotitle-ext` — the `model_prompt`
  attribution demo) as its bins — `gate-ext` was deleted 2026-09
  (the gate returns as the built-in `tabit-gate`; examples will ride
  the extension SDK when it is developed)
- `crates/tabit-gui` — the egui frontend (`tabit-gui` binary; spawns
  a `tabit-core --json` child, resolved as its sibling binary or via
  `TABIT_CORE_BIN`; reducer/view contract in ROADMAP item 7).
  Its `CHANGELOG.md` is the frontend protocol's changelog —
  every `PROTOCOL_VERSION` bump or frontend-observable change (wire
  or behavior) gets an entry in the same commit; FRONTEND.md stays
  the frozen mechanics contract, TOOLS.md its companion for the
  built-in tool `details` shapes and interaction templates)
- `crates/tabit-core` — the backend binary (`tabit-core`): headless,
  no UI and no frontend references — frontends spawn it, never the
  other way. Print mode (`-p <PROMPT>`, `--rewind <n>`) and JSON
  mode (`--json` — the stdio protocol edge) over the session host
  (create / `--continue` / `--session <path>` / `--list`). The
  `tabit` name is reserved for the frontend that ships primary
  (2026-09: the TUI candidates outpace the GUI; no in-repo binary
  carries it yet)

## Design rules

1. **API abstraction only — no model catalog.** No model-name constants, no
   model-name-keyed branching anywhere. Users supply provider endpoints, model
   ids, and parameters via their own config; the framework passes them through.
   The one required-with-default parameter: Anthropic requests with no
   `max_tokens` get `anthropic::DEFAULT_MAX_TOKENS` (65,536) — a plain
   provider constant, overridable per model via config.
2. **Front/back split.** Provider backends (wire clients, streaming, auth) stay
   strictly decoupled from front-facing logic (agents, sessions, tools, user
   config). Front-facing code never grows provider-specific knowledge.
3. **Modular.** One concern per crate. Cross-crate dependencies point downward
   (facade → agent/derive → core). No feature may require reaching into another
   crate's internals.
4. **We own the code.** The rig source was vendored to be a starting point, not
   a frozen upstream copy. Feel free to rewrite, restructure, or delete any of
   it. `VENDOR.md` documents the initial state for provenance only.
5. **Tests run offline.** Provider behavior is covered by cassette replay
   (httpmock) — never live network in CI/default test runs. Live tests are
   `#[ignore]`d.
6. **Fail loud, not silent.** No silent fallbacks that paper over missing user
   config. A documented provider constant (like `DEFAULT_MAX_TOKENS`) is a
   default, not a fallback — it is visible, named, and config-overridable.
7. **Implementation quality.** Clean module boundaries — expose only what
   callers need; a change in one module shouldn't force changes in many
   others. One purpose per function/module; if a description needs "and",
   split it. No duplicated logic for the same concern — extract a shared,
   well-named abstraction that is genuinely simpler than the repetition.
   Concern identity is the *output artifact*, not the input shape: two
   folds that consume different inputs (engine stream items vs. log
   records) but produce the same artifact (the model-facing context) are
   one concern — extract or extend, never write a sibling. Before adding
   any fold, builder, projection, or accumulator, enumerate the existing
   implementations of the same output anywhere in the workspace,
   dependencies and vendored code included, and say why this isn't the
   Nth. Prefer battle-tested algorithms/crates over hand-rolled ones; if
   you must hand-roll, document why. Internal errors fail hard and loud;
   external errors fail gracefully and clearly; never swallow an error
   or substitute a default that masks the real cause.
8. **Canonical surfaces.** Tabit's tools are contextual
   `#[rig_tool]`s (they take `#[rig(context)] &mut ToolContext` —
   the session cwd, the run token, capabilities); `PortableTool`
   remains rig-core's surface for non-contextual tools. Erasure into
   `DynamicTool` goes through `rig_agent::tool::dynamic_contextual`
   (one implementation). OpenAI code targets
   the Responses API; chat completions is the compat-gateway wire format.
   Tool-call arguments parse strictly — truncated JSON is an error, never a
   silent partial call. Tool cancellation is token-and-detach (ENGINE.md's
   execution substrate): bodies poll on the sidecar runtime, abort detaches
   the task and the token is the ask — drop is no longer the mechanism;
   `bash` is the reference implementation, `subagent` follows its shape
   (the child's leash is the parent's token).
9. **Fighting the architecture is a stop signal.** If the work feels like
   fighting the design — wrestling the borrow checker, reaching for an
   unintuitive workaround for a recurring error, or ping-ponging between
   two designs — assume the design is wrong, not the code. Do not "make it
   work" with a dirty hack. Stop, then summarize for the user: the goal,
   the problem, and why it is hard — and ask for a design discussion first.
10. **All-MIT.** The GPL split existed only to admit the claurst TUI
    harvest; that frontend is dead (see ROADMAP item 7), so nothing in the
    workspace is GPL and nothing will be. Frontends stay leaf consumers of
    the protocol (dependencies run frontend → backend only) — architecture
    hygiene, not license law.
11. **Flow changes go through ENGINE.md.** Flow-level changes (turn
    loop, run lifecycle, steering, failure handling) consult
    `ENGINE.md` first and amend it before touching code. New flow
    behavior gets new states or edges — never conditionals grown inside
    existing states, never driver-side control flow outside the
    machine.
12. **Bugs are design questions.** Patching the symptom is step one,
    never the deliverable: before calling a bug fixed, ask why it was
    structurally possible — what design choice admitted it, what
    constraint a workaround served and whether that constraint still
    exists (constraints die quietly; verify, then delete the machinery
    they justified), and whether one semantic is being re-assembled at
    several sites that should share a single home. The death-door
    checkout bug was three abort doors re-assembling
    drop-all-pending-intent, split by a discard-staging workaround
    built when the handler could not emit events — obsolete the day it
    could, deleted only after the second bug. The same audit applies
    *proactively*: when a change removes or alters a mechanism's
    justification (sync → write-behind, eager → lazy, one writer →
    queue), the machinery that justification built is re-derived in
    the same change. Elaborating machinery to preserve it — adding a
    buffer, flag, or second pass so an existing mechanism keeps
    working under a new regime — is the stop signal: the mechanism is
    usually dead weight the regime change just exposed.

## Reporting

Status summaries state the **reason** each mechanism exists, not just
what it did. "The session re-derives context from the log after every
run because persistence was synchronous" dies in one read; "the
session keeps its resident chain" launders implementation into
architecture-sounding nouns and breaks the owner's review — the
summary is the owner's review surface. A mechanism you cannot give a
reason for appears in the summary as reason-less: that is the
finding, not a phrasing problem. Gate results report internal
consistency, never design fit (see the gate bullet below).

## Environment / commands

- **Windows.** Use `python` (not `python3`); read/write files as UTF-8 explicitly.
- **Hang diagnosis.** Every sync lock claim funnels through
  `tabit_log::lock` (the claim contract — order, no re-entrancy, no
  guard across an await — is documented there). `TABIT_LOCK_TRACE=1`
  logs each acquire/release; `TABIT_LOCK_TIMEOUT=<secs>` bounds each
  claim and panics with a waiting-for report (who holds what, from
  which site) instead of hanging silently. Diagnose a hung test by
  running it alone under both variables.
- The green gate (verify by **exit code**, not by grepping output — a piped
  grep once masked a failing suite): `cargo fmt --check`,
  `cargo clippy --workspace --all-targets`, and
  `cargo test --workspace --no-fail-fast`. A hung suite fails the gate
  instead of parking it forever: `scripts/test.sh` bounds the test legs
  (`TABIT_TEST_TIMEOUT` seconds, default 1200) and prints how to find the
  hang; CI bounds the tests step at 20 minutes. The suite runs fully
  offline
  (see rule 5); some tests carry upstream-marked `#[ignore]`s
  (live-network scenarios). Don't record pass counts here — they change
  constantly; run the suite for current numbers. The gate proves
  **internal consistency** — code, tests, and docs agree with each
  other — and nothing more; artifacts written in one sitting are
  mutually consistent even when the design is wrong. Never report gate
  results as evidence that a design is right.
- Scripted or regex mass-edits of source files are a last resort, and
  the result is read back before the next build. The compiler is not a
  reviewer.
- In shell commands, avoid `;` chaining — it runs the next step regardless
  of the previous one's failure. Prefer `&&` (proceed only on success) or
  `||` (fallback), so a failed step can never be talked past.
- Error doctrine: **internal errors (bugs, invariants, unexpected
  state) fail hard and loud — they panic** — so they are noticed and
  fixed; not crashing means the app runs in a broken state that could
  damage the user's system. **External errors (invalid user input,
  missing files, network failures, unavailable extensions) fail
  gracefully and clearly** as typed errors. The crash-family lints
  (`panic`, `unwrap_used`, `expect_used`, `indexing_slicing`,
  `unreachable`, …) are warnings — they prompt a second look, they do
  not forbid the sanctioned crash; `dbg_macro`, `todo!`,
  `unimplemented!()` stay forbidden (leftovers, not failure
  handling). Test code relaxes the warnings via an identical
  `#![cfg_attr(test, allow(..))]` header at the top of each crate's
  lib.rs — new crates copy the current version from an existing crate
  rather than an old one.
- Coverage: `cargo llvm-cov --workspace --html --output-dir target/llvm-cov/html`.
  Every gap must be filled, justified, or explicitly deferred — the ledger
  and policy live in `COVERAGE.md`.
- `scripts/test.sh` is the gate's runner: filtered report (totals,
  failing tests with panic blocks, compile errors) with cargo's own
  exit codes; `--gate` runs all three legs, and any extra args pass
  through to cargo test (e.g. `-p crate filter`, or
  `--target-dir target-test` when the GUI holds a lock on
  `target\debug`). Prefer it over hand-rolled `cargo test | grep`
  pipelines.
- Cassettes are byte-sensitive (LF endings enforced via `.gitattributes`).
- CI rides the latest stable toolchain; keep the local one current
  (`rustup update`) — if CI clippy fails on a lint local passes, that is
  skew, not a flake: update first, then fix.

## Terminology

- **Outer loop** — what the user feels: prompt → agent thinks → calls tools →
  repeat until done. One outer loop = one `AgentRun`. The engine's state
  machine (states, responsibilities, machine/driver split) is designed in
  `ENGINE.md`.
- **Turn** — one model call within a run.
- **Tool-use roundtrip** — the boundary between a model turn's tool calls and
  the next model call (execute tools → feed results back). This is where
  steering, permission checks, and future extension hooks intervene.

## Not planned

- WebSocket streaming: **removed** — HTTP SSE only.
- Companion crates (bedrock, gemini-grpc, vector stores, …), `discord-bot`,
  `rmcp` (the rig-agent `rmcp` module is **kept, feature-gated, off by
  default** — MCP is a bad protocol, but some services are only
  reachable through it; whether tabit ships an MCP client is a later
  decision, low priority).
- Mid-conversation system messages: **unsupported by design** — always hoisted
  into the preamble.
- SSE reconnect/resumption for completion streams (retry belongs at the
  request layer, only before any body bytes are consumed).
- Vendor instruction files (CLAUDE.md etc.): **AGENTS.md only**.
- Instruction-file directory walking: home (`~/.tabit/AGENTS.md` with a
  `~/.agents/AGENTS.md` fallback) and cwd only — no upward/child scans.

## Open items for the owner

