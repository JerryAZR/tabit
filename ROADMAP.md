# ROADMAP

What we have, and what to build to reach feature parity with pi
(reference: `~/Projects/agents/pi`; also opencode, crush, codex, yaca).

## Where we are

The foundation is complete and hardened — this is the transport/runtime layer
that pi's `pi-ai` provides, at or above its robustness:

- **rig-core**: anthropic + openai (Responses API) providers over a shared
  openai-compatible engine; pi-policy retry (408/409/429/5xx + x-should-retry,
  retry-after honored, 60s server-delay cap, jittered 0.5s·2ⁿ→8s backoff,
  default 2 retries), connect/idle timeouts, typed transport errors,
  per-index content-block routing with loud interleave guards, orphan
  tool-result validation, cache-usage reporting, refusal surfacing.
- **rig-agent**: agent loop with lifecycle hooks, sans-IO serializable
  `AgentRun` state machine, tool-panic containment, `#[rig_tool]` +
  `PortableTool` as the canonical tool surface, strict tool-arg parsing.
- ~97% line coverage, zero warnings, wasm-clean, offline cassette tests,
  COVERAGE.md ledger, docs/policies in AGENTS.md / VENDOR.md.
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
- Reference survey behind the compat decision: pi ships an explicit compat
  schema (11 thinking formats etc.); opencode absorbs quirks in per-provider
  packages/hardcoded transforms; codex ships zero compat flags and only
  speaks the Responses API. Tabit sides with codex on strictness but keeps
  `extra_body` as the generic escape hatch.
- Embedding `ndims`/`dimensions` explicitly (see AGENTS.md open item) —
  still to add when embeddings enter the config story.
- Dynamic model listing (fetch from endpoint and merge with local config) —
  planned; design deferred until the CLI exists.

### 2. Session layer

The application-level conversation layer pi builds over its agent loop:

- Session state: message history, tool call/result records, usage accounting,
  per-session model selection from config.
- Persistence: session log format (JSONL event log first — replayable,
  diff-friendly; sqlite backend later if needed).
- Session listing/resume across runs.

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
- This is where mid-conversation system messages would tempt us — they are
  unsupported by design; everything hoists into the preamble (AGENTS.md).

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

### 7. CLI / interface layer

- **Print mode shipped** (`crates/tabit`): one prompt in, live events out,
  project-local sessions, `--continue` / `--session <path>` / `--list`,
  `--model provider/model` or `default_model` in providers.toml.
- Headless JSON/RPC mode next (scriptable, testable — same order pi
  evolved in): the same SessionEvent stream, JSON-framed on stdout.
- Interactive REPL; TUI on top only after the JSON surface is stable.

### 8. Client/server + protocol

- Split frontend/backend: a server process owning sessions, a thin client.
- Protocol = the JSON event stream from item 7, over a transport (named
  pipe / local socket / stdio).

### 9. Extensions

- Extension support: a way for users to add tools and hooks without
  forking — likely WASM or script-based tool providers plus the existing
  hook points, informed by opencode's extension/plugin design.
- Settings surface: layered config (user > workspace > flags) already partly
  from item 1; extensions register tools, hooks, and prompt contributions.

## Explicitly not planned

(kept in sync with AGENTS.md)

- WebSocket streaming (removed).
- SSE resumption / reconnect.
- rmcp integration (deleted).
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
