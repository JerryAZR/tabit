WebFetch has certificate trouble in this environment; I'll use the web reader MCP and raw.githubusercontent.com instead. Let me start with the repo tree to find the relevant files.

**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"retain_images":false,"url":"https://api.github.com/repos/sst/opencode/git/trees/main?recursive=1"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": "MCP error -500: 500 Internal Server Error: \"{\"error\":{\"code\":\"1234\",\"message\":\"Network error, error id: 20260909120808ba0ead4a40bc4d29_call_18b48036f5de42b0b1c13fe7, please try again later\"}}\"", "type": "text"}]


curl hits SSL issues too. Let me try `gh` CLI.

`gh` works. Note: `sst/opencode` redirects to `anomalyco/opencode` (the repo moved), default branch `dev`. I'll survey the tree for the relevant files.

The tree gives me the key files. Let me fetch them.

`summary.ts` is git-diff summaries, not compaction. Let me fetch the core schema/token/config files and the processor (which likely holds the trigger).

The real message schema lives in `packages/schema/src/v1/session.ts`. Fetching that plus the compaction event schema.

Now the load-bearing pieces: the processor (trigger), llm.ts (usage capture), and message-v2.ts (model-message projection / filterCompacted).

This is the heart of it. Now llm.ts (usage capture / overflow error) and message-v2.ts (history projection).

llm.ts is at the oc root, not in session/. Reading it now.

llm.ts is just streaming. The key pieces are `Session.getUsage` (session.ts), the prompt loop (prompt.ts), and error mapping (message-error.ts). Fetching those plus the negative-tokens regression test.

Now the prompt loop — where "compact" triggers the actual summarization run.

The loop's pre-flight trigger and task queue are clear. Now message-v2.ts for `filterCompactedEffect`, `latest`, and `toModelMessagesEffect`.

message-v2.ts gives me filterCompacted, latest, and fromError. Now I need the overflow classification (provider/error.ts), the reserve formula (provider/transform.ts), and run-state.ts for display context.

`updateMessage` publishes events — persistence must be an event consumer. Let me find the projector that writes rows and maintains the session-level token aggregate.

A `core/src/session/` module exists with `context-epoch.ts` — highly relevant. Fetching it plus the projector and history.

context-epoch is about the system-prompt baseline, keyed by event sequence — a different concern. Let me check the projector (where usage events persist) and history.

Persistence model confirmed. Now the display path (context %) and the compaction prompt/test.

Two compaction implementations exist — the V1 service path and a newer core `runner` path. Let me verify which is wired and how the runner tracks usage/overflow.

V2 execution is wired into the server. One last check — whether anything learns/updates model limits from overflow errors, and the step settlement tokens source.

All questions are now answerable from primary source. One note before the findings: `sst/opencode` now redirects to **`anomalyco/opencode`** and the default branch is **`dev`** (there is no `main`); everything below is current `dev` as of today. Two implementations coexist: the live V1 loop in `packages/opencode/src/session/*` and a newer V2 runner in `packages/core/src/session/*` (wired into the server via `SessionExecutionLocal`, spec'd in `specs/v2/session.md`). I cover both, labeled.

---

## 1. Where the context-size number comes from

**V1 (live loop): provider-reported usage from the streaming response.** Each `step-finish` stream event carries the provider's `Usage`; `Session.getUsage` normalizes it and it is *assigned* to the assistant message:

`packages/opencode/src/session/processor.ts` (step-finish handler):
```ts
const usage = Session.getUsage({
  model: ctx.model,
  usage: value.usage ?? new Usage({}),
  metadata: value.providerMetadata,
})
ctx.assistantMessage.finish = value.reason
ctx.assistantMessage.cost += usage.cost
ctx.assistantMessage.tokens = usage.tokens     // assignment, not +=
```

`packages/opencode/src/session/session.ts` (`getUsage`): pure normalization of provider fields (`inputTokens`, `outputTokens`, `reasoningTokens`, cache read/write incl. provider-metadata fallbacks for anthropic/vertex/bedrock/venice, `total = usage.totalTokens`). Nothing is tokenized locally here.

The TUI context display also uses it — `packages/tui/src/feature-plugins/sidebar/context.tsx`:
```ts
const last = msg().findLast((item): item is AssistantMessage => item.role === "assistant" && item.tokens.output > 0)
const tokens =
  last.tokens.input + last.tokens.output + last.tokens.reasoning + last.tokens.cache.read + last.tokens.cache.write
...
percent: model?.limit.context ? Math.round((tokens / model.limit.context) * 100) : null,
```

The local tokenizer exists but is a **chars/4 estimate used only for planning, never for the trigger or display** — `packages/core/src/util/token.ts`:
```ts
const CHARS_PER_TOKEN = 4
export const estimate = (input: string) => Math.max(0, Math.round(input.length / CHARS_PER_TOKEN))
```
Its consumers: the compaction tail budget (`preserveRecentBudget`), the prune protector (`PRUNE_PROTECT`), and the V2 pre-request check (below).

**V2 (runner): local estimate of the outgoing request.** `packages/core/src/session/runner/llm.ts:222`: `if (yield* compaction.compactIfNeeded({ sessionID: session.id, entries, model, request }))` — called after building the request, before streaming. The check (`packages/core/src/session/compaction.ts`):
```ts
if (
  estimate({ system: input.request.system, messages: input.request.messages, tools: input.request.tools }) <=
    context - Math.max(output, config.buffer)
)
  return false
```
where `estimate = (value: unknown) => Token.estimate(JSON.stringify(value))` — chars/4 over the actual request payload. Spec: *"Before each provider turn, the runner estimates the complete model-visible request and compares it with the selected model's context window minus absolute reserved headroom. The reserve is the greater of the requested/model output allowance and configured `compaction.buffer`."* (`specs/v2/session.md:113`).

## 2. Deltas or cumulative; where persisted

Neither per-message deltas nor a session cumulative for context decisions — **per-message snapshots of whole-request usage** (the usage of that message's *last* model call), plus per-step records, plus a billing aggregate:

- `packages/schema/src/v1/session.ts` — `Assistant`: `tokens: Schema.Struct({ total: optional(Finite), input, output, reasoning, cache: {read, write} })`; `StepFinishPart` carries the same shape per model call. The message-level field is *replaced* on every step (assignment above), so `Assistant.tokens` always describes the final step of that message; parts keep the per-step history.
- Persistence is event-sourced into SQLite via drizzle: `updateMessage`/`updatePart` publish `MessageUpdated`/`PartUpdated` events (`session.ts:629-643`), and `packages/core/src/session/projector.ts` upserts `MessageTable`/`PartTable` rows. The session-level `SessionTable.tokens_*` columns are a **running sum maintained diff-style on step-finish parts only**:
```ts
const previous = row && usage(row.data)
const next = usage(event.data.part)
if (previous) yield* applyUsage(db, row.session_id, previous, -1)
if (next) yield* applyUsage(db, sessionID, next)
```
That aggregate is billing/cost data; no decision reads it.

## 3. Post-summarization recomputation and stale-measurement handling

**Nothing is recomputed.** The compaction assistant message is created with zeroed tokens (`packages/opencode/src/session/compaction.ts:407-412`) and then receives the *summary call's own* usage — which describes the serialized-head prompt, **not** the post-compaction prefix. That is exactly the stale-referent shape, and opencode neutralizes it three ways:

1. **The trigger only ever reads the newest finished message, and explicitly excludes summary messages.** `packages/opencode/src/session/prompt.ts:1161-1168` (pre-flight, before each model call):
```ts
if (
  lastFinished &&
  lastFinished.summary !== true &&
  (yield* compaction.isOverflow({ tokens: lastFinished.tokens, model }))
) {
  yield* compaction.create({ sessionID, agent: lastUser.agent, model: lastUser.model, auto: true })
```
and the same guard post-call in `processor.ts:491-496`: `if (!ctx.assistantMessage.summary && isOverflow({...tokens: usage.tokens...})) ctx.needsCompaction = true`.
2. **The next real model call re-measures the new prefix.** History is re-derived each loop iteration (`msgs = MessageV2.filterCompactedEffect(sessionID)`), and the model-visible shape after compaction is the reorder (comment in `message-v2.ts:578`): `[compaction-user, summary, ...retained tail..., continue-user]` — with the compaction part projecting to the literal user text `"What did we do so far?"` (`message-v2.ts:228-233`) answered by the summary message (`summary: true`, `agent: "compaction"`, tool calls forbidden while summarizing — `processor.ts:316-318`).
3. **Old measurements are never aggregated.** Retained-tail messages still carry usage measured against the pre-compaction prefix — a latent stale referent — but no code path sums them for context decisions; they only feed the billing aggregate, where "what request was billed" remains a true statement forever.

V2 makes the boundary an explicit **epoch by event sequence**: `SessionEvent.Compaction.Ended` durably stores the summary text plus the serialized recent tail; `packages/core/src/session/history.ts` drops everything before it:
```ts
compaction
  ? or(gte(SessionMessageTable.seq, compaction.seq), ...)
```
and the next provider attempt renders a fresh context baseline (`context-epoch.ts`: `SystemContext.replace` when `compaction.seq > stored.baseline_seq`). A failed compaction attempt leaves the previous boundary active (spec line 117).

The one place the stale referent *shows*: the TUI context % right after compaction displays the summary call's usage (measured on the serialized head) until the next turn's usage replaces it — cosmetic, self-correcting.

## 4. Trigger threshold and formula

**V1** — `packages/opencode/src/session/overflow.ts` (complete file, 34 lines):
```ts
const COMPACTION_BUFFER = 20_000

export function usable(input: { cfg; model; outputTokenMax? }) {
  const context = input.model.limit.context
  if (context === 0) return 0
  const reserved =
    input.cfg.compaction?.reserved ??
    Math.min(COMPACTION_BUFFER, ProviderTransform.maxOutputTokens(input.model, input.outputTokenMax))
  return input.model.limit.input
    ? Math.max(0, input.model.limit.input - reserved)
    : Math.max(0, context - ProviderTransform.maxOutputTokens(input.model, input.outputTokenMax))
}

export function isOverflow(input: { cfg; tokens; model; outputTokenMax? }) {
  if (input.cfg.compaction?.auto === false) return false
  if (input.model.limit.context === 0) return false
  const count =
    input.tokens.total || input.tokens.input + input.tokens.output + input.tokens.cache.read + input.tokens.cache.write
  return count >= usable(input)
}
```
So: **count = `total` (provider-reported) else `input+output+cache.read+cache.write` of the last finished assistant message; threshold = `limit.input − reserved` (or `context − maxOutputTokens` when no input limit); `reserved = min(20_000, maxOutputTokens)`**, with `maxOutputTokens = min(model.limit.output, 32_000) || 32_000` (`transform.ts:1468-1470`, `OUTPUT_TOKEN_MAX = 32_000`). This confirms the prior survey's min(20k, max-output) reserve. Triggered at three sites: post-step (`processor.ts`), pre-next-call (`prompt.ts:1161`), and on provider overflow errors.

**V2** — `estimate(request) > context − max(outputLimit, buffer=20_000 default)` (`core/src/session/compaction.ts:232-243`, `DEFAULT_BUFFER = 20_000`), with summary output capped at `SUMMARY_OUTPUT_TOKENS = 4_096` and a headroom sanity check `Token.estimate(summaryPrompt) > context - summaryOutput → give up`.

## 5. Overflow-error recovery and limit learning

Classification: `packages/opencode/src/provider/error.ts:175` — `if (isContextOverflow(m) || input.error.statusCode === 413 || body?.error?.code === "context_length_exceeded")` → `ContextOverflowError`; `isContextOverflow` (`packages/llm/src/provider-error.ts:36`) is a message-regex matcher plus `/^4(00|13)\s*(status code)?\s*\(no body\)/`.

V1 recovery: `processor.halt` maps `ContextOverflowError` to `ctx.needsCompaction = true` (unless `compaction.auto === false`, which surfaces the error); the loop then calls `compaction.create({..., overflow: !handle.message.finish })` — `overflow: true` precisely when the request died without a finish reason. `processCompaction` with `overflow: true` finds the previous non-compaction user message, slices history before it, compacts the head, then **replays that user message as a new message** (media attachments degraded to `[Attached ...]` placeholders) or injects a synthetic continue-prompt explaining *"The previous request exceeded the provider's size limit due to large media attachments..."*. If the compaction itself returns `"compact"` (summary request also overflows), it hard-fails: `"Conversation history too large to compact - exceeds model context limit"`. There is also a deterministic pruner (`PRUNE_MINIMUM = 20_000`, `PRUNE_PROTECT = 40_000`) that erases old tool outputs (`time.compacted` timestamp → projected as `"[Old tool result content cleared]"`).

V2 recovery: overflow detected before any durable assistant output → one `compactAfterOverflow` → rebuild the turn from the compacted history; a second overflow is terminal — `return yield* Effect.die("Post-compaction provider attempt cannot recover another overflow")` (`runner/llm.ts:368`).

**Nothing teaches the context limit.** `model.limit.context`/`limit.input` are static catalog values (models.dev via `Provider.Service`) plus user config; the reserve is a constant; no code writes back a learned limit from an overflow error (the only `limit.context` reads are in `overflow.ts` and `native-request.ts`).

---

## Verdict

**Hybrid, and deliberately so — and it avoids the stale-referent problem by construction, not by patching it.**

- Persisted usage is **cumulative-per-call snapshots** (each message's tokens describe the entire request that produced them), never per-message deltas and never a cross-message sum used for decisions. Since input tokens overlap between calls, summing them would be meaningless — opencode never tries.
- The decision rule is "**last measurement wins, with epoch exclusion**": read only the newest finished assistant's usage; exclude compaction-boundary (`summary: true`) messages from triggering; re-measure after any boundary change by simply making the next call; treat provider overflow errors as the final correction (one bounded retry). The stale referent tabit hit — a usage total recorded before compaction describing a replaced prefix — exists in opencode's data too (retained-tail messages, the summary call's own usage), but it is *unreachable* by the decision path.
- The V2 runner goes further and is the direction of travel: **decisions come from a local estimate of the actual outgoing request** (referent = the request itself, so it cannot be stale), while provider-reported usage is demoted to billing/display data, and compaction is a sequence-numbered epoch that resets the baseline.

Implication for tabit's "per-message delta tokens": deltas only solve the stale-referent problem if they are *estimates of current message content* recombined at decision time (opencode V2's approach — accept chars/4-grade accuracy), because provider-reported per-call totals are not additive. If the numbers must stay provider-reported, the opencode lesson is instead: keep totals as last-call snapshots, never aggregate them, and gate any pre-boundary measurement behind an epoch check — plus the overflow-error path as the backstop.

Sources: [sst/opencode (redirects to anomalyco/opencode)](https://github.com/sst/opencode), [session/compaction.ts](https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/session/compaction.ts), [session/overflow.ts](https://github.com/anomalyco/opencode/blob/dev/packages/opencode/src/session/overflow.ts), [core/src/session/compaction.ts](https://github.com/anomalyco/opencode/blob/dev/packages/core/src/session/compaction.ts), [specs/v2/session.md](https://github.com/anomalyco/opencode/blob/dev/specs/v2/session.md), [issue #4416 (auto compaction early)](https://github.com/anomalyco/opencode/issues/4416), [issue #32119 (context % under-reports)](https://github.com/anomalyco/opencode/issues/32119)
