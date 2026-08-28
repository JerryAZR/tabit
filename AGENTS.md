# AGENTS.md

Guidance for AI coding agents (and humans) working on **tabit**.

## What this is

`tabit` is an agent framework that started from a **vendored, trimmed copy of
rig 0.41.0** (upstream: `rig-rs/rig`). The rig source was borrowed as source
rather than an external crate precisely so it can be modified freely: **it is
tabit's code now** — change, extend, or delete any of it wherever that makes
sense. `VENDOR.md` is the historical record of the initial vendoring (what was
trimmed and why), not a constraint on future edits.

Current workspace layout:

- `crates/rig-core` — provider API clients, streaming, tools (providers kept:
  **anthropic + openai** + the shared openai-compatible engine in
  `providers/internal`)
- `crates/rig-agent` — agent loop / runtime
- `crates/rig-derive` — `#[rig_tool]` proc macros
- `crates/rig` — facade crate re-exporting the three above
- `crates/tabit-protocol` — the frontend protocol vocabulary (commands,
  stamped events, handshake frames; `FRONTEND.md` is the contract)
- `crates/tabit-config` — provider/model configuration (see `ROADMAP.md`)
- `crates/tabit-session` — persistent sessions over the outer loop (native
  only: filesystem-backed; the rig crates keep wasm support)
- `crates/tabit-tools` — coding tools (`read`, `ls`, `bash`,
  `ask_user` — the body is one interaction roundtrip) as
  `#[rig_tool]` PortableTools, erasable to DynamicTools (native only)
- `crates/tabit-gui` — the egui frontend (`tabit-gui` binary; the
  `tabit` launcher detach-spawns it; reducer/view contract in ROADMAP
  item 7)
- `crates/tabit` — the `tabit` binary: bare `tabit [path]` is the
  launcher mode (detach-spawns the GUI and exits — the supported
  entry point), print mode (`-p <PROMPT>`, `--rewind <n>`) and JSON
  mode (`--json` — the stdio protocol edge) over the session host
  (create / `--continue` / `--session <path>` / `--list`)

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
8. **Canonical surfaces.** Tabit tools implement `PortableTool`
   (`#[rig_tool]`); the contextual `Tool`/`ToolContext` is the runtime-side
   consumption contract reached via the blanket bridge. OpenAI code targets
   the Responses API; chat completions is the compat-gateway wire format.
   Tool-call arguments parse strictly — truncated JSON is an error, never a
   silent partial call. Tool cancellation is token-and-detach (ENGINE.md's
   execution substrate): bodies poll on the sidecar runtime, abort detaches
   the task and the token is the ask — drop is no longer the mechanism;
   `bash` is the reference implementation.
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

- The embedding `ndims`/`dimensions` params are now caller-supplied only
  (was name-inferred). Any tabit config schema should carry these explicitly.
