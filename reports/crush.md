Network works. Let me fetch the repo tree to find the relevant files.

Let me download the full source tarball so I can grep actual contents.

The fallback estimator is clear: provider usage first, local `len/4` estimate only when usage is zero. Now the main agent loop.

The `StopWhen` trigger and post-run summarize are clear. Now let me read `updateSessionUsage` and the full `Summarize`.

`updateSessionTokenCounters` is last-wins assignment, not accumulation — a key finding. Now the full `Summarize` function.

This is the crux — `PromptTokens = 0` after summarization. Now the session struct and persistence.

Session-level persistence confirmed. Now the DB schema — check whether messages carry any token columns.

Messages carry no token columns — only `is_summary_message`. Now let me see how history is rebuilt after summarization (`getSessionMessages`, `preparePrompt`) and the overflow-recovery path.

`getSessionMessages` truncates at the summary message. Now the overflow-recovery path and where `ContextWindow` comes from.

No runtime limit learning found. Let me verify the `Model` wrapper and check the Run entry for any pre-flight context check, plus test files confirming the trigger behavior.

All questions are answered. Let me clean up the temp files.

# Crush context-size / token-usage research

Examined source: `main` @ `6bbfa8a256f6726f1dd0d8e3fce83c6ade05b02a` (committed 2026-09-08), full tarball read locally. The LLM engine is an external module (`charm.land/fantasy`); all accounting below is in crush's own tree.

## 1. Where the context-size number comes from

Provider-reported usage from responses, refreshed after every completed model step — with a crude local estimate only when the provider reports zero. In `internal/agent/agent.go`, the stream's `OnStepFinish` (line ~1029):

```go
usage, estimated := fallbackStepUsage(stepMessages, stepResult)
a.updateSessionUsage(largeModel, &updatedSession, usage, a.openrouterCost(stepResult.ProviderMetadata), estimated)
```

`internal/agent/usage_fallback.go`:

```go
func fallbackStepUsage(messages []fantasy.Message, step fantasy.StepResult) (fantasy.Usage, bool) {
	if !usageIsZero(step.Usage) {
		return step.Usage, false
	}
	inputTokens := estimateMessageTokens(messages)
	outputTokens := estimateStepCompletionTokens(step)
	...
	return fantasy.Usage{InputTokens: inputTokens, OutputTokens: outputTokens,
		TotalTokens: inputTokens + outputTokens}, true
}
```

The fallback is `approxTokenCount(s) = (len(s)+3)/4` — a byte heuristic, no tokenizer. The `estimated` flag propagates to the session (`EstimatedUsage`) and the UI renders a `~` prefix and skips cost math for estimated steps (`internal/agent/agent.go:1969-1985`, `internal/ui/model/header.go:151-153`).

## 2. Per-message deltas or cumulative? Where persisted

**Neither accumulated deltas nor per-message counts — a last-wins cumulative snapshot on the session row.** The messages table has no token columns at all (see `internal/db/models.go` `Message` struct and `internal/db/migrations/20250424200609_initial.sql`; messages only get `is_summary_message`). Token counters live solely on the `sessions` table (`prompt_tokens`, `completion_tokens`), surfaced as `internal/session/session.go`:

```go
type Session struct {
	...
	MessageCount     int64
	PromptTokens     int64
	CompletionTokens int64
	EstimatedUsage   bool
	SummaryMessageID string
	Cost             float64
	...
}
```

The counters are **assigned, never summed** (`internal/agent/agent.go:1991-1998`):

```go
func updateSessionTokenCounters(session *session.Session, usage fantasy.Usage) {
	if usage.OutputTokens != 0 {
		session.CompletionTokens = usage.OutputTokens
	}
	if promptTokens := usage.InputTokens + usage.CacheReadTokens; promptTokens != 0 {
		session.PromptTokens = promptTokens
	}
}
```

So `PromptTokens` is the input size (incl. cache reads) of the *most recent* model call — the provider's own measurement of the entire context sent in that call. (A sqlc `AddSessionUsage` increment query exists in `internal/db/sql/sessions.sql:59` with zero callers — dead.)

## 3. Recomputation after summarization; stale-measurement handling

`Summarize` (`internal/agent/agent.go:1336-1485`) streams a summarization call over the **full old history** (`Messages: aiMsgs` from `preparePrompt(msgs...)`, no truncation), persisting the summary as a message with `IsSummaryMessage: true`. Cost accrues from that call, and then the stale measurement is explicitly discarded (`agent.go:1458-1466`):

```go
a.updateSessionUsage(largeModel, &currentSession, resp.TotalUsage, openrouterCost, false)

// Just in case, get just the last usage info.
usage := resp.Response.Usage
currentSession.SummaryMessageID = summaryMessage.ID
currentSession.CompletionTokens = summaryCompletionTokens(usage, summaryMessage)
currentSession.PromptTokens = 0
currentSession.EstimatedUsage = usageIsZero(usage)
_, err = a.sessions.Save(genCtx, currentSession)
```

Key points:
- `PromptTokens = 0` — the pre-summary input total (which described the now-replaced prefix) is wiped; note `updateSessionUsage` on the line above had just set it from the summarize call's own input (which measured the *old* full context) — that interim value is deliberately clobbered in the same save.
- `CompletionTokens` is re-seeded from the summarize response's own output tokens, or `approxTokenCount` of the summary text when usage is zero (`summaryCompletionTokens`, `agent.go:2000-2005`), with `EstimatedUsage = usageIsZero(usage)`.
- The context referent changes in the same write: history is truncated at the summary message and its role rewritten (`agent.go:1702-1713`):

```go
if session.SummaryMessageID != "" {
	...
	if summaryMsgIndex != -1 {
		msgs = msgs[summaryMsgIndex:]
		msgs[0].Role = message.User
	}
}
```

Old messages stay in the DB but are never sent again. There is **no recompute-from-messages**: the next real model call's provider-reported input becomes the fresh measurement of the new post-summary context. Stale handling = zero the referent-detached number and re-seed with facts about the summary itself, in the transaction that establishes the new referent.

## 4. Summarize trigger threshold and formula

A `StopWhen` condition evaluated between steps of the agent run (`internal/agent/agent.go:1039-1060`):

```go
cw := int64(largeModel.CatwalkCfg.ContextWindow)
// If context window is unknown (0), skip auto-summarize
// to avoid immediately truncating custom/local models.
if cw == 0 {
	return false
}
tokens := currentSession.CompletionTokens + currentSession.PromptTokens
remaining := cw - tokens
var threshold int64
if cw > largeContextWindowThreshold {
	threshold = largeContextWindowBuffer
} else {
	threshold = int64(float64(cw) * smallContextWindowRatio)
}
if (remaining <= threshold) && !a.disableAutoSummarize {
	shouldSummarize = true
	return true
}
```

with constants (`agent.go:56-60`): `largeContextWindowThreshold = 200_000`, `largeContextWindowBuffer = 20_000`, `smallContextWindowRatio = 0.2`. I.e. trigger when remaining ≤ 20k tokens (windows > 200k) or ≤ 20% of the window — your prior 20k/20% survey holds. It genuinely **stops the run**: the stop-condition halts the stream, then `Summarize` runs, and if the assistant had unfinished tool calls the original prompt is re-queued with `"The previous session was interrupted because it got too long, the initial user request was: `%s`"` (`agent.go:1210`). The window itself is static config — catwalk catalog / user config (`internal/config/config.go:169`) / local-endpoint discovery (`internal/discover/ollama.go`, `lmstudio.go`, etc.). Disable via `disable_auto_summarize` (`internal/config/config.go:365`).

## 5. Context-overflow recovery and limit learning

**None on current main.** I searched for every plausible overflow string ("too long", "context_length", "token limit", etc.): no code matches provider context-overflow errors, and nothing ever adjusts `ContextWindow` at runtime. If overflow happens anyway, it lands in the generic provider-error path that persists `FinishReasonError` and aborts the run (`agent.go:1157-1196`). Protection is entirely preemptive (the StopWhen above, plus the `cw == 0` skip so unknown windows never auto-truncate). The UI context percentage just divides stored counters by the *configured* window (`internal/ui/model/header.go:148-156`); a stale/wrong config shows a wrong number and nothing learns from it.

## Verdict

**Cumulative, last-wins — and it avoids the stale-referent problem by construction of where the number lives, not by using deltas.**

Crush's usage lives in one mutable cell pair on the session row, overwritten by every provider response, with a single writer path. It is never attached to persisted conversation entries, so no measurement can outlive the prefix it described. The compaction code owns both sides atomically: it changes the referent (`SummaryMessageID` + history truncation) and invalidates/re-seeds the measurement (`PromptTokens = 0`, `CompletionTokens` = summary output, `EstimatedUsage` marker) in one `sessions.Save`. The accepted trade-offs: post-summary context size is understated until the next call re-measures (summary tokens sit only in `CompletionTokens`), and zero-usage providers get a marked `len/4` estimate rather than silence.

For tabit's design question: your bug (a usage total persisted on an entry whose prefix the summary replaced) is precisely the shape crush's design excludes. If tabit keeps provider-reported totals, they must describe "the call that produced them," not "the context" — a prefix-describing total is only valid while the prefix is unchanged, so it may only live somewhere compaction provably invalidates. Per-message "delta tokens" fix it only if the delta is stored as a per-call fact on the entry *produced by* that call (the assistant message of that turn) and the context size is re-derived by summing deltas over the current (post-summary) message set — which is a hybrid: provider-reported per-response usage, summed at read time, with compaction shrinking the sum naturally because the replaced entries are skipped. That hybrid gets crush's freshness without crush's single-writer discipline requirement, at the cost of one re-derivation fold (a concern your AGENTS.md rule 7 says to unify with any existing context fold).
