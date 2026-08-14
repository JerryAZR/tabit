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

## Design rules

1. **API abstraction only — no model catalog.** No model-name constants, no
   model-name-keyed branching anywhere. Users supply provider endpoints, model
   ids, and parameters (e.g. `max_tokens`) via their own config; the framework
   passes them through. Anthropic requests with no `max_tokens` fail loudly
   ("Anthropic requires `max_tokens`; set it on the completion request").
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
   config (the old 2048-token anthropic fallback was removed for this reason).

## Environment / commands

- **Windows.** Use `python` (not `python3`); read/write files as UTF-8 explicitly.
- `cargo build` / `cargo test` — the suite runs fully offline (see rule 5);
  some tests carry upstream-marked `#[ignore]`s (live-network scenarios).
  Don't record pass counts here — they change constantly; run the suite for
  current numbers.
- Cassettes are byte-sensitive (LF endings enforced via `.gitattributes`).

## Not planned

- WebSocket streaming (`websocket` feature), companion crates (bedrock,
  gemini-grpc, vector stores, …), `discord-bot`/`rmcp`.
- Mid-conversation system messages: **unsupported by design** — always hoisted
  into the preamble.

## Open items for the owner

- The embedding `ndims`/`dimensions` params are now caller-supplied only
  (was name-inferred). Any tabit config schema should carry these explicitly.
