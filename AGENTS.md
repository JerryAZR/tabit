# AGENTS.md

Guidance for AI coding agents (and humans) working on **tabit**. This
file carries only what you need to work here: the rules, the map, the
commands. Design docs and status live elsewhere (each area names its
owning doc below); git history is the archive of how decisions got
made.

## What this is

`tabit` is an agent framework that started from a **vendored, trimmed
copy of rig 0.41.0** — borrowed as source precisely so it can be
modified freely: **it is tabit's code now** (`VENDOR.md` is the
historical record, not a constraint).

Positioning (owner ruling): tabit is a **study/research project in
agent architecture, not a pi competitor**. pi is the feature reference
and survey source, never the target — don't chase parity, and don't
add machinery because pi has it.

## Workspace map

Every tabit process is a **node**: it serves its spawner over a frozen
pipe and may spawn nodes that serve it — a frontend, a subagent, and
an extension are all nodes over one substrate. Mechanisms shared
between core and the extension SDK live in `tabit-wire`, the one crate
below both. Each crate's module docs carry its details; the named
docs carry the cross-cutting contracts:

- `tabit-providers` — provider API clients (anthropic + openai + the
  shared openai-compatible engine), streaming, message/tool types
- `tabit-engine` — the agent loop / runtime (`ENGINE.md` is the flow
  doc — consult it before any flow change)
- `tabit-derive` — the `#[rig_tool]` proc macros
- `tabit-rig` — facade re-exporting the three above
- `tabit-protocol` — the wire vocabulary (`FRONTEND.md` is the
  contract)
- `tabit-config` — providers/auth/settings file layers
- `tabit-log` — the durable conversation: log, tree, context manager,
  writer
- `tabit-wire` — the node runtime + spawned-child mechanisms
- `tabit-session` — sessions over the engine: persistence, skills,
  attachments, compaction (`COMPACTION.md`), subagents, the wire's
  serve side
- `tabit-tools` — the coding tools read/write/edit/bash (`TOOLS.md`)
- `tabit-gate` — the default permission gate: a careless-mistake
  catcher, never a security boundary
- `tabit-ext` / `tabit-ext-sdk` / `tabit-ext-install` — the extension
  host / guest SDK / installer (`EXTENSIONS.md` is the contract)
- `tabit-app` — the composition root as a library (`EMBEDDING.md` is
  the embedder contract)
- `tabit-core` — the headless backend binary; frontends spawn it,
  never the reverse. The `tabit` name is reserved for the frontend
  that ships primary.

`ROADMAP.md` carries the planned work.

## Design rules

1. **API abstraction only — no model catalog.** No model-name
   constants, no model-name-keyed branching anywhere. Users supply
   provider endpoints, model ids, and parameters via their own config;
   the framework passes them through.
2. **Front/back split.** Provider backends (wire clients, streaming,
   auth) stay strictly decoupled from front-facing logic (agents,
   sessions, tools, user config). Front-facing code never grows
   provider-specific knowledge.
3. **Modular.** One concern per crate. Cross-crate dependencies point
   downward. No feature may require reaching into another crate's
   internals.
4. **We own the code.** The rig source was vendored to be a starting
   point, not a frozen upstream copy. Rewrite, restructure, or delete
   any of it.
5. **Tests run offline.** Provider behavior is covered by cassette
   replay (httpmock) — never live network in CI/default test runs.
   Live tests are `#[ignore]`d.
6. **Fail loud, not silent.** No silent fallbacks that paper over
   missing user config. A documented, config-overridable constant is
   a default, not a fallback.
7. **Implementation quality.** Clean module boundaries — expose only
   what callers need. One purpose per function/module; if a
   description needs "and", split it. No duplicated logic for the
   same concern — extract a shared, well-named abstraction that is
   genuinely simpler than the repetition. Concern identity is the
   *output artifact*, not the input shape: two folds that consume
   different inputs but produce the same artifact are one concern —
   extract or extend, never write a sibling. Before adding any fold,
   builder, projection, or accumulator, enumerate the existing
   implementations of the same output anywhere in the workspace,
   dependencies and vendored code included, and say why this isn't
   the Nth. Prefer battle-tested algorithms/crates over hand-rolled
   ones; if you must hand-roll, document why.
8. **Canonical surfaces.** Tabit's tools are contextual
   `#[rig_tool]`s (they take `#[rig(context)] &mut ToolContext`);
   `PortableTool` remains tabit-providers' surface for non-contextual
   tools. Erasure into `DynamicTool` goes through
   `tabit_engine::tool::dynamic_contextual` (one implementation).
   OpenAI code targets the Responses API; chat completions is the
   compat-gateway wire format. Tool-call arguments parse strictly —
   truncated JSON is an error, never a silent partial call. Tool
   cancellation is token-and-detach (ENGINE.md's execution
   substrate): the token is the ask, drop is not the mechanism;
   `bash` is the reference implementation.
9. **Fighting the architecture is a stop signal.** If the work feels
   like fighting the design — wrestling the borrow checker, reaching
   for an unintuitive workaround, or ping-ponging between two designs
   — assume the design is wrong, not the code. Stop, then summarize
   for the user: the goal, the problem, and why it is hard — and ask
   for a design discussion first.
10. **All-MIT.** Nothing in the workspace is or becomes GPL.
    Frontends stay leaf consumers of the protocol (dependencies run
    frontend → backend only).
11. **Flow changes go through ENGINE.md.** Flow-level changes (turn
    loop, run lifecycle, steering, failure handling) consult
    `ENGINE.md` first and amend it before touching code. New flow
    behavior gets new states or edges — never conditionals grown
    inside existing states, never driver-side control flow outside
    the machine.
12. **Bugs are design questions.** Patching the symptom is step one,
    never the deliverable: before calling a bug fixed, ask why it was
    structurally possible — what design choice admitted it, what
    constraint a workaround served and whether that constraint still
    exists (constraints die quietly; verify, then delete the
    machinery they justified), and whether one semantic is being
    re-assembled at several sites that should share a single home.
    The same audit applies *proactively*: when a change removes or
    alters a mechanism's justification, the machinery that
    justification built is re-derived in the same change. Elaborating
    machinery to preserve it — adding a buffer, flag, or second pass
    so an existing mechanism keeps working under a new regime — is
    the stop signal: the mechanism is usually dead weight the regime
    change just exposed.

## Reporting

Status summaries state the **reason** each mechanism exists, not just
what it did — the summary is the owner's review surface. "The session
re-derives context from the log after every run because persistence
was synchronous" dies in one read; "the session keeps its resident
chain" launders implementation into architecture-sounding nouns. A
mechanism you cannot give a reason for appears in the summary as
reason-less: that is the finding, not a phrasing problem. Gate
results report internal consistency, never design fit (see the gate
bullet below).

## Environment / commands

- **Windows.** Use `python` (not `python3`); read/write files as UTF-8
  explicitly.
- The green gate (verify by **exit code**, never by grepping output):
  `bash scripts/test.sh --gate` runs `cargo fmt --check`,
  `cargo clippy --workspace --all-targets`, and
  `cargo test --workspace --no-fail-fast` with a filtered report and
  bounded test legs (`TABIT_TEST_TIMEOUT` seconds, default 1200).
  Extra args pass through to cargo test (e.g. `-p crate filter`, or
  `--target-dir target-test` when a running binary holds a lock on
  `target\debug`). The suite runs fully offline (rule 5). The gate
  proves **internal consistency** — code, tests, and docs agree with
  each other — and nothing more; never report gate results as
  evidence that a design is right.
- **Hang diagnosis.** Every sync lock claim funnels through
  `tabit_log::lock` (the claim contract is documented there).
  `TABIT_LOCK_TRACE=1` logs each acquire/release;
  `TABIT_LOCK_TIMEOUT=<secs>` bounds each claim and panics with a
  waiting-for report instead of hanging silently. Diagnose a hung
  test by running it alone under both variables.
- Error doctrine: **internal errors (bugs, invariants, unexpected
  state) fail hard and loud — they panic**; not crashing means the
  app runs broken. **External errors (invalid user input, missing
  files, network failures, unavailable extensions) fail gracefully
  and clearly** as typed errors. The crash-family lints (`panic`,
  `unwrap_used`, `expect_used`, `indexing_slicing`, `unreachable`, …)
  are warnings — they prompt a second look, they do not forbid the
  sanctioned crash; `dbg_macro`, `todo!`, `unimplemented!()` stay
  forbidden. Test code relaxes the warnings via the identical
  `#![cfg_attr(test, allow(..))]` header at the top of each crate's
  lib.rs — new crates copy the current version from an existing one.
- Coverage: `cargo llvm-cov --workspace --html --output-dir target/llvm-cov/html`.
  Every gap must be filled, justified, or explicitly deferred — the
  ledger and policy live in `COVERAGE.md`.
- Scripted or regex mass-edits of source files are a last resort, and
  the result is read back before the next build. The compiler is not
  a reviewer.
- In shell commands, avoid `;` chaining — it runs the next step
  regardless of the previous one's failure. Prefer `&&` or `||`.
- Cassettes are byte-sensitive (LF endings enforced via
  `.gitattributes`).
- CI rides the latest stable toolchain; keep the local one current
  (`rustup update`) — if CI clippy fails on a lint local passes, that
  is skew, not a flake: update first, then fix.

## Terminology

- **Outer loop** — what the user feels: prompt → agent thinks → calls
  tools → repeat until done. One outer loop = one `AgentRun`.
- **Turn** — one model call within a run.
- **Tool-use roundtrip** — the boundary between a model turn's tool
  calls and the next model call. This is where steering, permission
  checks, and extension hooks intervene.

## Not planned

(kept so ruled-out work stops being re-derived; ROADMAP.md carries
what IS planned)

- The egui GUI (deleted 2026-09) — no in-repo GUI.
- In-process subagents — subprocess children are the ONE substrate.
- WebSocket streaming — HTTP SSE only; no SSE reconnect/resumption.
- Mid-conversation system messages — hoisted into the preamble.
- Model catalog / name-keyed behavior — rule 1.
- A GPL anything — rule 10.
- Companion crates (bedrock, gemini-grpc, vector stores, …),
  `discord-bot`, an MCP client (`rmcp` stays feature-gated, off).
- Vendor instruction files (CLAUDE.md etc.) — AGENTS.md only; home +
  cwd, no directory walking.
- Dedicated search tools (grep/glob shapes) — `bash` with piping and
  filtering is the search surface (owner ruling, re-derived too many
  times).
