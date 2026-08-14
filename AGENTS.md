# AGENTS.md

Guidance for AI coding agents (and humans) working on **tabit**.

## What this is

`tabit` is an agent framework built on a **vendored, trimmed copy of rig 0.41.0**
(upstream: `rig-rs/rig`). The workspace currently contains the vendored layer only:

- `crates/rig-core` — provider API clients, streaming, tools (providers trimmed to
  **anthropic + openai** + the shared openai-compatible engine in `providers/internal`)
- `crates/rig-agent` — agent loop / runtime
- `crates/rig-derive` — `#[rig_tool]` proc macros
- `crates/rig` — facade crate re-exporting the three above

Tabit's own agent/session/subagent layer will be built **on top of** this, as new
crates — never by editing vendored behavior beyond what the rules below allow.
See `VENDOR.md` for the full vendor record (what was trimmed, deviations, update
procedure).

## Design rules

1. **API abstraction only — no model catalog.** No model-name constants, no
   model-name-keyed branching anywhere. Users supply provider endpoints, model
   ids, and parameters (e.g. `max_tokens`) via their own config; the framework
   passes them through. Anthropic requests with no `max_tokens` fail loudly
   ("Anthropic requires `max_tokens`; set it on the completion request").
2. **Front/back split.** Provider backends (wire clients, streaming, auth — the
   vendored rig layer) stay strictly decoupled from front-facing logic (agents,
   sessions, tools, user config). New tabit features go in new crates that
   *consume* the vendored layer; they never grow provider-specific knowledge.
3. **Modular.** One concern per crate. Cross-crate dependencies point downward
   (facade → agent/derive → core). No feature may require reaching into another
   crate's internals.
4. **Vendored code stays faithful.** Trim, don't rewrite. Any deviation from
   upstream must be minimal and recorded in `VENDOR.md`.
5. **Tests run offline.** Provider behavior is covered by cassette replay
   (httpmock) — never live network in CI/default test runs. Live tests are
   `#[ignore]`d.
6. **Fail loud, not silent.** No silent fallbacks that paper over missing user
   config (the old 2048-token anthropic fallback was removed for this reason).

## Environment / commands

- **Windows.** Use `python` (not `python3`); read/write files as UTF-8 explicitly.
- Build artifacts live on `D:` via `.cargo/config.toml` (`target-dir`) — the C:
  drive is space-constrained. **Do not remove this.**
- `cargo build` / `cargo test` — workspace is green: rig-core 687, rig-agent 489,
  rig-derive 43, rig 253, doctests 57 (plus upstream-marked ignores).
- Cassettes are byte-sensitive (LF endings enforced via `.gitattributes`).

## Deferred (do not assume support)

- WebSocket streaming (`websocket` feature), companion crates (bedrock,
  gemini-grpc, vector stores, …), `discord-bot`/`rmcp`.
- Mid-conversation system messages: **unsupported by design** — always hoisted
  into the preamble.

## Open items for the owner

- The embedding `ndims`/`dimensions` params are now caller-supplied only
  (was name-inferred). Any tabit config schema should carry these explicitly.
