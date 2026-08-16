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
- `crates/tabit-config` — provider/model configuration (see `ROADMAP.md`)
- `crates/tabit-session` — persistent sessions over the outer loop (native
  only: filesystem-backed; the rig crates keep wasm support)
- `crates/tabit-tools` — coding tools (`read`, `ls`, `bash`) as
  `#[rig_tool]` PortableTools, erasable to DynamicTools (native only)
- `crates/tabit` — the `tabit` binary: print mode (`-p <PROMPT>`,
  `--rewind <n>`) and JSON mode (`--json` — the stdio protocol edge)
  over the session actor (create / `--continue` / `--session <path>` /
  `--list`)

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
   Prefer battle-tested algorithms/crates over hand-rolled ones; if you must
   hand-roll, document why. Internal errors fail hard and loud; external
   errors fail gracefully and clearly; never swallow an error or substitute
   a default that masks the real cause.
8. **Canonical surfaces.** Tabit tools implement `PortableTool`
   (`#[rig_tool]`); the contextual `Tool`/`ToolContext` is the runtime-side
   consumption contract reached via the blanket bridge. OpenAI code targets
   the Responses API; chat completions is the compat-gateway wire format.
   Tool-call arguments parse strictly — truncated JSON is an error, never a
   silent partial call. Tool cancellation follows the contract documented
   in `tabit-tools`'s crate docs (engine owns *when*, tool owns *how*;
   drop-safety required; `bash` is the reference implementation).
9. **Fighting the architecture is a stop signal.** If the work feels like
   fighting the design — wrestling the borrow checker, reaching for an
   unintuitive workaround for a recurring error, or ping-ponging between
   two designs — assume the design is wrong, not the code. Do not "make it
   work" with a dirty hack. Stop, then summarize for the user: the goal,
   the problem, and why it is hard — and ask for a design discussion first.
10. **License split.** Backend crates are MIT; `tabit-tui` (the claurst
    harvest — see ROADMAP item 7) and the released binary are
    GPL-3.0-only. GPL code never enters an MIT crate: dependencies run
    frontend → backend only, and the protocol vocabulary lives on the MIT
    side (tabit-session).

## Environment / commands

- **Windows.** Use `python` (not `python3`); read/write files as UTF-8 explicitly.
- The green gate (verify by **exit code**, not by grepping output — a piped
  grep once masked a failing suite): `cargo fmt --check`,
  `cargo clippy --workspace --all-targets`, and
  `cargo test --workspace --no-fail-fast`. The suite runs fully offline
  (see rule 5); some tests carry upstream-marked `#[ignore]`s
  (live-network scenarios). Don't record pass counts here — they change
  constantly; run the suite for current numbers.
- In shell commands, avoid `;` chaining — it runs the next step regardless
  of the previous one's failure. Prefer `&&` (proceed only on success) or
  `||` (fallback), so a failed step can never be talked past.
- The workspace denies `panic`/`unwrap`/`expect`/indexing in shipped code;
  test code relaxes those via an identical `#![cfg_attr(test, allow(..))]`
  header at the top of each crate's lib.rs — new crates copy the current
  version from an existing crate rather than an old one.
- Coverage: `cargo llvm-cov --workspace --html --output-dir target/llvm-cov/html`.
  Every gap must be filled, justified, or explicitly deferred — the ledger
  and policy live in `COVERAGE.md`.
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
  `rmcp` (the rig-agent `rmcp` module is deleted, not just deferred).
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
