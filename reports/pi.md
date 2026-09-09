I'll research pi's context-size tracking by reading its actual source on GitHub. Let me start by mapping the repo structure.

WebFetch has a certificate issue in this environment. Let me try the web reader MCP tool and curl as alternatives.

**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"retain_images":false,"url":"https://api.github.com/repos/badlogic/pi-mono/git/trees/main?recursive=1"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://api.github.com/repos/badlogic/pi-mono/git/trees/main?recursive=1", "content": "{\n\"sha\": \"34878e7cc8074f42edff6c2cdcc9828aa9b6afde\",\n\"url\": \"https://api.github.com/repos/badlogic/pi-mono/git/trees/34878e7cc8074f42edff6c2cdcc9828aa9b6afde\",\n\"tree\": [\n{\n\"path\...


I have the full repo tree. Now let me fetch the key source files: overflow.ts, compaction.ts, agent-loop.ts, and agent.ts.

**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"retain_images":false,"return_format":"text","url":"https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/ai/src/utils/overflow.ts"}
```
*Executing on server...*


**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"retain_images":false,"return_format":"text","url":"https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/coding-agent/src/core/compaction/compaction.ts"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/ai/src/utils/overflow.ts", "content": "import type { AssistantMessage } from \"../types.js\";\n/\\*\\*\n\\* Regex patterns to detect context overflow errors from different providers.\n\\*\n\\* These patterns match er...


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/coding-agent/src/core/compaction/compaction.ts", "content": "/\\*\\*\n\\* Context compaction for long sessions.\n\\*\n\\* Pure functions for compaction logic. The session manager handles I/O,\n\\* and after compactio...


I have the core compaction module. Now let me read the compaction design doc and the session manager (persistence format).

**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"retain_images":false,"return_format":"text","url":"https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/coding-agent/docs/compaction.md"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/coding-agent/docs/compaction.md", "content": "# Compaction & Branch Summarization\nLLMs have limited context windows. When conversations grow too long, pi uses compaction to summarize older content while preserving r...


The docs confirm the shape. Now I need the runtime side — where the trigger fires, how usage persists, and overflow recovery in the agent loop. Let me fetch `agent-session.ts` (large) and `agent-loop.ts`.

**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"retain_images":false,"return_format":"text","timeout":40,"url":"https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/coding-agent/src/core/agent-session.ts"}
```
*Executing on server...*


**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"retain_images":false,"return_format":"text","url":"https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/agent/src/agent-loop.ts"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/agent/src/agent-loop.ts", "content": "/**\n* Agent loop that works with AgentMessage throughout.\n* Transforms to Message[] only at the LLM call boundary.\n*/\nimport {\ntype AssistantMessage,\ntype Context,\nEventSt...


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/coding-agent/src/core/agent-session.ts", "content": "/**\n* AgentSession - Core abstraction for agent lifecycle and session management.\n*\n* This class is shared between all run modes (interactive, print, rpc).\n* I...


I have the core overflow/compaction logic. Now let me get the session-manager (persistence + `getLatestCompactionEntry`) and the `Usage` type definition.

**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"no_cache":true,"retain_images":false,"return_format":"text","timeout":40,"url":"https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/coding-agent/src/core/session-manager.ts"}
```
*Executing on server...*


**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"retain_images":false,"return_format":"text","url":"https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/ai/src/types.ts"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/ai/src/types.ts", "content": "import type { AssistantMessageEventStream } from \"./utils/event-stream.js\";\nexport type { AssistantMessageEventStream } from \"./utils/event-stream.js\";\nexport type KnownApi =\n| \"...


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://raw.githubusercontent.com/badlogic/pi-mono/main/packages/coding-agent/src/core/session-manager.ts", "content": "```\nimport type { AgentMessage } from \"@earendil-works/pi-agent-core\";\nimport { type ImageContent, type Message, type TextContent, type Usage, uuidv7 } from ...


The two flavors suggest the web reader served cached snapshots from different commits (npm scope `@mariozechner` vs `@earendil-works`). Let me pin fetches to the exact tree SHA I got from the API (`34878e7...`) so all quotes come from one consistent snapshot.

Curl hits the same TLS interception issue. Since this is read-only public content, I'll retry with `-k` (skip cert verification) — safe here because I'm only fetching public GitHub data, not entering credentials.

The file was written to Git Bash's /tmp which Windows Python can't see. Let me use a Windows-visible path.

The repo has been renamed/transferred — GitHub says "Moved Permanently". Let me follow the redirect to find the new home.

The repo moved to `earendil-works/pi` (pushed 2026-09-08). My earlier "mariozechner"-flavored reads were stale cache; the `@earendil-works` one was current. Let me get the current tree and download the relevant files pinned to one SHA.

Now the layout is clear — the repo was heavily refactored (compaction now lives in `packages/agent/src/harness/`). Let me download all relevant files pinned to this tree SHA so quotes are consistent.

All files downloaded. Now let me read the core accounting files.

Now the main compaction module.

This confirms the core accounting functions. Now let me read the session context builder and entry types — key for the stale-measurement question.

This reveals a usage ledger (`UsageRow`/`scanUsage`) alongside message entries, and `buildContextEntries` rebuilding context from the compaction boundary. Now let me find the compaction-decision logic and any stale-usage guards in the harness.

The stale-measurement guards exist in the current agent-session.ts. Let me read those regions.

Let me continue reading the post-compaction handling and the getContextUsage region.

Now let me trace the call sites, imports, and helpers (`getLatestCompactionEntry`, `estimateMessagesTokens`, where `_checkCompaction` fires).

Now the session manager — persistence format, `appendCompaction`, `getLatestCompactionEntry`, and how messages with usage are persisted.

Now overflow.ts and where usage attaches to assistant messages in the agent loop.

Now the current compaction doc and the new harness's usage ledger.

The doc mentions an in-run between-turns check. Let me verify where that lives and get the coding-agent's compaction/index.ts re-exports.

The coding-agent has its own compaction.ts. Let me read its accounting core and prepareCompaction.

Let me find where usage rows are inserted in the harness and how SessionStats is derived, plus who calls prepareCompactionThreshold.

Let me look at the usage-totals helper, the sqlite usage ledger, and the regression tests documenting the stale/zero-usage behavior.

A few final verifications: per-response usage semantics in a provider, and whether anything "learns" the context limit.

Let me verify there's no local tokenizer dependency, then clean up the temp files.

Research complete. Here is the full report.

# How pi tracks context size / token usage around compaction

**Repo note first:** `badlogic/pi-mono` has moved — GitHub redirects to **`earendil-works/pi`** (npm scope renamed `@mariozechner/*` → `@earendil-works/*`). Everything below is from current `main`, tree SHA `6160683a4a8012f0d1cd30c145df18b4ca6f5176` (all file paths repo-relative to it). There are effectively **two generations of engine** side by side: the shipping coding agent (`packages/coding-agent/src/core/agent-session.ts` + classic agent loop) and a newer harness (`packages/agent/src/harness/…`) with a durable state machine and a usage ledger. Both use the same accounting model.

## 1. Where the context-size number comes from

**Provider-reported usage from each response, carried on the assistant message — with a chars/4 heuristic fallback. No tokenizer anywhere** (no tiktoken/gpt-tokenizer in `packages/ai/package.json`).

`packages/coding-agent/src/core/compaction/compaction.ts` (identical twin in `packages/agent/src/harness/compaction/compaction.ts` and `packages/ai/src/utils/estimate.ts`):

```ts
/** Calculate total context tokens from usage.
 * Uses the native totalTokens field when available, falls back to computing from components. */
export function calculateContextTokens(usage: Usage): number {
	return usage.totalTokens || usage.input + usage.output + usage.cacheRead + usage.cacheWrite;
}
```

The anchor is the **last valid assistant message's usage** plus a heuristic for messages after it:

```ts
/** Estimate context tokens from messages, using the last assistant usage when available.
 * If there are messages after the last usage, estimate their tokens with estimateTokens. */
export function estimateContextTokens(messages: AgentMessage[]): ContextUsageEstimate {
	const usageInfo = getLastAssistantUsageInfo(messages);
	if (!usageInfo) { /* pure estimateTokens() over all messages */ }
	const usageTokens = calculateContextTokens(usageInfo.usage);
	...trailingTokens via estimateTokens (chars/4, images = 4800 chars)...
	return { tokens: usageTokens + trailingTokens, ... };
}
```

`Usage.totalTokens` is per-request (Anthropic computes it: `packages/ai/src/api/anthropic-messages.ts` — "Anthropic doesn't provide total_tokens, compute from components": `input + output + cacheRead + cacheWrite`). Aborted/error/all-zero-usage messages are skipped as anchors (`getAssistantUsage`).

## 2. Per-message deltas or cumulative totals? Where persisted

**Cumulative, snapshot-per-response — stored per assistant message, but each stored value describes the whole request context at that point, not a delta.** Every assistant message carries its own response `Usage`; on `message_end` the whole message is persisted:

- `packages/coding-agent/src/core/agent-session.ts` (~line 689): `this.sessionManager.appendMessage(event.message);`
- `packages/coding-agent/src/core/session-manager.ts`: `appendMessage()` writes a `SessionMessageEntry { type: "message", id, parentId, timestamp, message }` — **one JSON object per line in the session `.jsonl`** (tree-linked via `id`/`parentId`), so `usage` is embedded inside each persisted assistant message.
- Compaction is persisted as its own entry:
```ts
export interface CompactionEntry<T = unknown> extends SessionEntryBase {
	type: "compaction";
	summary: string;
	firstKeptEntryId: string;
	tokensBefore: number;
	details?: T;
	/** Usage from the LLM call(s) that generated this summary, if available */
	usage?: Usage;
	fromHook?: boolean;
}
```

The newer harness additionally writes a **separate append-only usage ledger** in the same transaction as each response (`packages/agent/src/harness/session/types.ts` `UsageRow { id, seq, usage, entryId?, adjustment: boolean }`; inserted in `runtime/drive/response.ts` via `insertUsage(usageRow)`; SQL backend `packages/session-backends/sqlite-node/.../usage-ledger.ts`). But that ledger feeds **session totals/billing (`SessionStats.usage`) only** — context size is still the per-assistant-message usage anchor. Same split in the classic path: `/stats` totals sum every message's usage (`src/core/usage-totals.ts`), while context size reads only the last one.

## 3. After compaction — recomputation and stale-measurement handling

After a compaction, context is rebuilt as `[compaction entry] + kept entries (from firstKeptEntryId) + later entries` (`buildContextEntries` in `session-manager.ts`), and `agent.state.messages` is replaced (`agent-session.ts` ~2383). Critically, **pi hits exactly your stale-referent bug and handles it explicitly, in three places** (`packages/coding-agent/src/core/agent-session.ts`):

**(a) Trigger guard** (`_checkCompaction`, ~2173):
```ts
// Skip compaction checks if this assistant message is older than the latest
// compaction boundary. This prevents a stale pre-compaction usage/error
// from retriggering compaction on the first prompt after compaction.
const compactionEntry = getLatestCompactionEntry(this.sessionManager.getBranch());
const assistantIsFromBeforeCompaction =
	compactionEntry !== null && assistantMessage.timestamp <= new Date(compactionEntry.timestamp).getTime();
if (assistantIsFromBeforeCompaction) return false;
```

**(b) Estimate-anchor guard** (~2239, for error/zero-usage messages whose estimate may anchor on a kept pre-compaction assistant):
```ts
// Verify the usage source is post-compaction. Kept pre-compaction messages
// have stale usage reflecting the old (larger) context and would falsely
// trigger compaction right after one just finished.
const usageMsg = messages[estimate.lastUsageIndex];
if (compactionEntry && usageMsg.role === "assistant" &&
	(usageMsg as AssistantMessage).timestamp <= new Date(compactionEntry.timestamp).getTime()) {
	return false;
}
```

**(c) UI/percent** (`getContextUsage()`, ~3420): if no valid assistant usage exists **after** the latest compaction entry, it returns `{ tokens: null, percent: null }` — context size is *unknown*, not guessed from a stale number:
```ts
// After compaction, the last assistant usage reflects pre-compaction context size.
// We can only trust usage from an assistant that responded after the latest compaction.
// If no such assistant exists, context token count is unknown until the next LLM response.
```

The stale check is deliberately applied only to usage-backed numbers — the pure chars/4 estimate over the rebuilt `[summary + kept]` context measures the *actual* current context and needs no guard. The next compaction's `tokensBefore` is recomputed from the rebuilt context (`prepareCompaction`: `estimateContextTokens(buildSessionContext(pathEntries).messages).tokens`; docs/compaction.md: "Pi also recalculates `tokensBefore` from the rebuilt session context … so the token count reflects the actual pre-compaction context being replaced"). The harness generation replaces timestamps with a positional guard — `prepareCompactionThreshold` (`runtime/drive/structural.ts` ~1136): "Prepare threshold compaction only when no newer compaction already guards this trigger" — skip if `newestCompactionIndex >= triggerIndex`. A regression test (`test/suite/regressions/8328-zero-usage-auto-compaction.test.ts`) pins the zero-usage/estimate fallback.

## 4. What feeds the trigger threshold

```ts
export const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = {
	enabled: true, reserveTokens: 16384, keepRecentTokens: 20000,
};
export function shouldCompact(contextTokens: number, contextWindow: number, settings: CompactionSettings): boolean {
	if (!settings.enabled) return false;
	return contextTokens > contextWindow - settings.reserveTokens;
}
```

`contextTokens` is `estimateContextTokens(...).tokens` (last assistant usage + trailing estimate), checked at three points in the classic path: **between tool batches** (in-run — `_compactBeforeNextAssistantResponse` wired into the agent loop's `prepareNextTurnWithContext` hook), **after a run ends** (`_handlePostAgentRun` → `_checkCompaction`), and **before each new prompt**. The summarization request itself is standalone serialized text (`serializeConversation`, tool results truncated to 2000 chars) with `cacheRetention: "none"` and a fresh routing session id ("Summaries are standalone requests, so isolate routing and avoid cache writes that cannot be reused" — harness `compaction.ts` `createSummaryRequestOptions`).

## 5. Context-overflow recovery and what "teaches" the limit

`packages/ai/src/utils/overflow.ts` detects three families: **error-text regex** (~25 provider patterns, with a `NON_OVERFLOW_PATTERNS` exclusion for rate-limit-style messages), **silent overflow** (`stopReason === "stop" && usage.input + usage.cacheRead > contextWindow` — z.ai), and **length-stop truncation** (`"length" && output === 0 && input+cacheRead >= 0.99 × contextWindow` — Xiaomi MiMo). Recovery (`_checkCompaction`): remove the failed assistant from live agent state (it stays in session history but out of retry context), `_runAutoCompaction("overflow", willRetry=true)`, then `agent.continue()` re-sends the turn — **exactly once** (`_overflowRecoveryAttempted`, reset on the next user message). A *successful* response that merely exceeded the window compacts without retry.

**Nothing learns the limit.** The context window is declared metadata — `model.contextWindow` from the model registry/settings — and overflow detection compares against it. I found no mechanism that adjusts a learned limit from observed errors (consistent with tabit's "no model catalog" rule, this is user/provider-supplied config).

## Verdict

**Pi's context accounting is hybrid: a per-response cumulative provider usage anchor + chars/4 heuristic for the tail — never per-message deltas.** Each persisted assistant message stores its own response usage (so the data *is* per-message), but each value is a snapshot of the entire request context at that turn; nothing is ever subtracted or re-summed across messages.

**And no — pi does not paper over the stale-measurement problem; it hit it and wrote explicit guards.** The design choice that admits the bug is structurally visible: compaction *replaces a prefix* while *retaining tail messages whose embedded usage describes the pre-replacement context* (kept entries keep their original `usage` — same class of bug you found with persisted totals). Pi's answer is an **epoch boundary**: the latest compaction entry invalidates every usage at-or-before it (timestamp/position comparison), context size becomes explicitly `null`/"unknown" until the next response re-teaches it, and threshold compaction can only be triggered by a post-compaction usage. The cost of that choice is a knowledge gap right after compaction (UI shows no context %; the pure-heuristic estimate is the only fallback and is trusted because it measures the rebuilt context rather than referencing a replaced prefix).

For your per-message-delta design: pi is evidence that the reference project keeps cumulative anchors and pays for boundary-validity guards at every read site (trigger, estimate, UI, and the next compaction's `tokensBefore`) — deltas that are re-summed after a compaction would make the boundary handling structural instead of a check duplicated at each consumer, but pi has not made that move; its newer harness doubled down on validity guards (positional `newestCompactionIndex >= triggerIndex`) plus a separate billing-only ledger.
