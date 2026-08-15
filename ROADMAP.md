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

Provider/model configuration schema, loaded from user config files:

- Provider entries: `base_url`, `api_key_env` (name of env var — never inline
  secrets), wire format (anthropic / openai-responses / openai-completions).
- Per-model settings: model id, context window, max output tokens,
  thinking/reasoning budget. No model catalog — every value is
  caller-supplied (this is a workspace rule); the schema just carries them.
- Embedding `ndims`/`dimensions` explicitly (see AGENTS.md open item).
- Resolution + validation with loud, precise errors on missing/ambiguous
  config. Fail loud, no silent defaults.

### 2. Session layer

The application-level conversation layer pi builds over its agent loop:

- Session state: message history, tool call/result records, usage accounting,
  per-session model selection from config.
- Persistence: session log format (JSONL event log first — replayable,
  diff-friendly; sqlite backend later if needed).
- Session listing/resume across runs.

### 3. System prompt builder + skills & AGENTS.md discovery

- Compose the system prompt from parts: base prompt, environment info (cwd,
  platform, date), tool descriptions, session/config contributions.
- AGENTS.md discovery: walk cwd upward to the workspace root (and home-level
  file), respecting pi's/standard precedence; content injected into the
  preamble.
- Skills discovery: `SKILL.md` files with frontmatter (name, description);
  discovered from user-level and workspace-level directories; exposed to the
  model as on-demand instructions (load-on-trigger, not always-inlined).
- This is where mid-conversation system messages would tempt us — they are
  unsupported by design; everything hoists into the preamble (AGENTS.md).

### 4. Coding tools

The standard toolset as `#[rig_tool]` implementations:

- bash (exec with timeout/output capture), read, edit (with diff preview),
  glob, grep, write.
- Windows-first correctness (paths, encodings — UTF-8 explicitly), but
  portable.
- Permission/approval hooks ride on the existing rig-agent hook system.

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

- Headless JSON/RPC mode first (scriptable, testable — same order pi
  evolved in): one-shot prompt, streaming events on stdout.
- Interactive REPL; TUI on top only after the JSON surface is stable.
- Model/provider flags resolving through the config crate.

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

## Deferred until a consumer exists (phase 4 leftovers)

- Overflow-detection helper (port pi's `overflow.ts`).
- Orphan-result repair utility.
- Typed `provider_status` on `CompletionError`.
- Eval harness (pi has one; build when there are sessions + tools to eval).
- MCP client support — verify pi's current story before committing.
- OAuth device-flow auth for providers (optional, late).
