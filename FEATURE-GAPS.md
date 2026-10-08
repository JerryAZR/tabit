# FEATURE-GAPS.md

Feature-gap analysis: tabit's backend vs. pi's "minimal feature-complete" set
(survey of `../pi`, 2026-09). pi is the survey source, never the target —
its core deliberately excludes subagents, todos, plan mode, MCP, permission
gates, and background tasks (they ship as example extensions), so tabit's
core is already ahead on those axes. The gaps below are where tabit trails
pi's minimal set. For later review and ruling; nothing here is approved work.

## Tier 1 — real gaps in pi's minimal set

1. ~~**Images across the wire.**~~ **Landed 2026-10, by a better
   route than this gap assumed.** Everything below the protocol already
   worked (`UserContent::Image`, provider serialization, `read`'s image
   arm). The resolution: user-attach images ride **inline
   `<attachment path="..."/>` tags** in the text-only wire, expanded at
   the session's message door (tags stay as anchors; basename label +
   image parts append in tag order; the log records the expanded bytes;
   no protocol bump). Oversized images downscale at the door under one
   conservative global cap (1568px long edge, 5 MB post-encode) — no
   per-model config. Remaining sub-item: `read`'s image arm still
   rejects over-cap images with guidance rather than downscaling (v1's
   deliberate deferral; revisit only if it bites in practice).
2. **OAuth credential flows.** pi ships OAuth + PKCE/device-code for
   anthropic, github-copilot, openai-codex, openrouter, xai, etc., with a
   credential store and per-request token refresh. tabit is API keys only
   (auth.toml/env). The largest single gap; a market-segment decision
   (API-key users never hit it), not a completeness one.
3. **Session tree: fork + branch summary.** pi has fork/clone and
   `branchWithSummary()` (an AI summary primes the new branch). tabit's
   checkout covers rewind + branch-switch within one file; there is no
   fork-copy op and no summary on checkout.
4. **Session export.** pi: self-contained HTML export, JSONL import, gist
   share. tabit: the log file is the only artifact.
5. **Model listing on the wire.** Provider listing clients exist and are
   cassette-tested, but no protocol command exposes them — FRONTEND.md
   assumes the frontend reads the user's config to build a picker.

## Tier 2 — pi models it in the backend; tabit doesn't (smaller)

- **User-bash-into-context** (`!command` recorded as a
  `BashExecutionMessage` so the model sees what the user ran).
- **Cache warmer** — pi re-pings before cache TTL expiry (with a decision
  hook); tabit mounts automatic 1h caching but never defends it.
- **Cumulative stats surface** — per-turn cost events exist; the FRONTEND.md
  `stats` command is still noted-future; pi has `/session` totals.
- **Assistant-error auto-retry visibility** — pi emits
  `auto_retry_start/end`; tabit retries silently (turn cap 1 +
  request-layer backoff).

## Tier 3 — pi doesn't have these either (not required for the bar)

- **Web search/fetch** — pi has none. tabit has fragments:
  `ToolDefinition::web_search()` exists but is never called; Anthropic
  server-tool parsing is receive-tolerant only. Wiring the hosted tool is a
  small option pi doesn't offer.
- **PDF reading in `read`** — pi's read is images-only too. tabit's document
  vocabulary + provider serialization are done; only the tool path is
  missing.
- **Todo/plan tools** — pi: example extension. tabit: the `think` builtin
  exists unmounted. Both worlds' answer: ship an example extension.
- **Background/fire-and-forget subagents** — pi's subagent example is
  synchronous too. tabit parallelism is `tool_concurrency` (default 1).
- **Client-side fallback chains** — neither has them.
- **MCP** — pi: explicitly none, by design. tabit: feature-gated engine
  module, deferred (ROADMAP).

## Read

The only gap blocking a frontend from feature-completeness is **Tier 1.1
(images over the wire)** — vocabulary, providers, and the read tool are
done, so it is a protocol version bump (attachment shapes in `Message` /
`UserMessage`), the `wire.rs` fold, and FRONTEND.md. **Tier 1.5 (model
listing)** is the next wire-shaped one. **Tier 1.2 (OAuth)** is the only
large item.
