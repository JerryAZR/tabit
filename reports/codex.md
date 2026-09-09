Direct curl to the GitHub API fails (SSL error). Let me use the web tools instead.

**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"return_format":"text","url":"https://github.com/openai/codex/tree/main/codex-rs/core/src"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": {"title": "codex/codex-rs/core/src at main · openai/codex · GitHub", "description": "Lightweight coding agent that runs in your terminal - codex/codex-rs/core/src at main · openai/codex", "url": "https://github.com/openai/codex/tree/main/codex-rs/core/src", "content": "codex/codex-rs/core/...


The repo structure has evolved — there's now a `context_manager` directory plus `compact.rs`, `compact_remote.rs`, `compact_remote_v2.rs`, `compact_token_budget.rs`. Let me explore those.

**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"return_format":"text","url":"https://github.com/openai/codex/tree/main/codex-rs/core/src/context_manager"}
```
*Executing on server...*


**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"return_format":"text","url":"https://github.com/openai/codex/tree/main/codex-rs/core/src/context"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": {"title": "codex/codex-rs/core/src/context at main · openai/codex", "description": "Lightweight coding agent that runs in your terminal - codex/codex-rs/core/src/context at main · openai/codex", "url": "https://github.com/openai/codex/tree/main/codex-rs/core/src/context", "content": "- - A...


**Output:**
**webReader_result_summary:** [{"text": {"title": "codex/codex-rs/core/src/context_manager at main · openai/codex", "description": "Lightweight coding agent that runs in your terminal - codex/codex-rs/core/src/context_manager at main · openai/codex", "url": "https://github.com/openai/codex/tree/main/codex-rs/core/src/context_man...


SSL issues with direct fetching, but the web reader works. Let me use it for the API too.

**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"return_format":"text","url":"https://api.github.com/repos/openai/codex/contents/codex-rs/core/src/context_manager"}
```
*Executing on server...*


**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"return_format":"text","url":"https://api.github.com/repos/openai/codex/contents/codex-rs/core/src"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://api.github.com/repos/openai/codex/contents/codex-rs/core/src/context_manager", "content": "[\n{\n\"name\": \"history.rs\",\n\"path\": \"codex-rs/core/src/context_manager/history.rs\",\n\"sha\": \"cfd92973820631465ce147976578d77ab051baba\",\n\"size\": 29734,\n\"url\": \"htt...


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://api.github.com/repos/openai/codex/contents/codex-rs/core/src", "content": "[\n{\n\"name\": \"agent\",\n\"path\": \"codex-rs/core/src/agent\",\n\"sha\": \"9242c32dcf6ed8c0ab60fa4f98dfbfe7676678d8\",\n\"size\": 0,\n\"url\": \"https://api.github.com/repos/openai/codex/content...


The tree has evolved. Key files identified: `context_manager/history.rs` (29.7 KB), `context_manager/updates.rs`, `compact.rs`, `compact_remote_v2.rs`, `rollout_budget.rs`, and `client.rs` (usage). Let me fetch the raw sources in parallel.

**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"return_format":"text","url":"https://raw.githubusercontent.com/openai/codex/main/codex-rs/core/src/context_manager/history.rs"}
```
*Executing on server...*


**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"return_format":"text","url":"https://raw.githubusercontent.com/openai/codex/main/codex-rs/core/src/context_manager/updates.rs"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://raw.githubusercontent.com/openai/codex/main/codex-rs/core/src/context_manager/history.rs", "content": "use crate::codex::TurnContext;\nuse crate::context\\_manager::normalize;\nuse crate::event\\_mapping::is\\_contextual\\_user\\_message\\_content;\nuse crate::truncate::Tr...


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://raw.githubusercontent.com/openai/codex/main/codex-rs/core/src/context_manager/updates.rs", "content": "use crate::codex::PreviousTurnSettings;\nuse crate::codex::TurnContext;\nuse crate::environment\\_context::EnvironmentContext;\nuse crate::shell::Shell;\nuse codex\\_exec...


The updates.rs file is about settings injection, not token tracking. Now let me find `TokenUsage`/`TokenUsageInfo` in the protocol crate.

**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"return_format":"text","url":"https://api.github.com/repos/openai/codex/contents/codex-rs/protocol/src"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://api.github.com/repos/openai/codex/contents/codex-rs/protocol/src", "content": "[\n{\n\"name\": \"account.rs\",\n\"path\": \"codex-rs/protocol/src/account.rs\",\n\"sha\": \"d9aa9ebf5732a4c22a9afc45f888c9b5c115df98\",\n\"size\": 9130,\n\"url\": \"https://api.github.com/repos...


**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"return_format":"text","url":"https://raw.githubusercontent.com/openai/codex/main/codex-rs/protocol/src/protocol.rs"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://raw.githubusercontent.com/openai/codex/main/codex-rs/protocol/src/protocol.rs", "content": "//! Defines the protocol for a Codex session between a client and an agent.\n//!\n//! Uses a SQ (Submission Queue) / EQ (Event Queue) pattern to asynchronously communicate\n//! betw...


I now have the `TokenUsage`/`TokenUsageInfo` definitions. Next I need the auto-compact trigger and thresholds, which moved out of the old `codex.rs` (repo has been refactored into `codex_thread.rs` / `state/` / `agent/` / `context/`). Let me check those directories.

**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"return_format":"text","url":"https://api.github.com/repos/openai/codex/contents/codex-rs/core/src/context"}
```
*Executing on server...*


**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"return_format":"text","url":"https://api.github.com/repos/openai/codex/contents/codex-rs/core/src/state"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://api.github.com/repos/openai/codex/contents/codex-rs/core/src/context", "content": "[\n{\n\"name\": \"approved_command_prefix_saved.rs\",\n\"path\": \"codex-rs/core/src/context/approved_command_prefix_saved.rs\",\n\"sha\": \"c4b7324631e5201d2eb82dd3ad6377b3038a49fe\",\n\"si...


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://api.github.com/repos/openai/codex/contents/codex-rs/core/src/state", "content": "[\n{\n\"name\": \"mod.rs\",\n\"path\": \"codex-rs/core/src/state/mod.rs\",\n\"sha\": \"f3ebc7225da027b92d6aac31bfaf4377ca82a7da\",\n\"size\": 289,\n\"url\": \"https://api.github.com/repos/open...


**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"return_format":"text","url":"https://api.github.com/repos/openai/codex/contents/codex-rs/core/src/state"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://api.github.com/repos/openai/codex/contents/codex-rs/core/src/state", "content": "[\n{\n\"name\": \"mod.rs\",\n\"path\": \"codex-rs/core/src/state/mod.rs\",\n\"sha\": \"f3ebc7225da027b92d6aac31bfaf4377ca82a7da\",\n\"size\": 289,\n\"url\": \"https://api.github.com/repos/open...


**🌐 Z.ai Built-in Tool: webReader**

**Input:**
```json
{"return_format":"text","url":"https://api.github.com/repos/openai/codex/contents/codex-rs/core/src"}
```
*Executing on server...*


**Output:**
**webReader_result_summary:** [{"text": {"url": "https://api.github.com/repos/openai/codex/contents/codex-rs/core/src", "content": "[\n{\n\"name\": \"agent\",\n\"path\": \"codex-rs/core/src/agent\",\n\"sha\": \"9242c32dcf6ed8c0ab60fa4f98dfbfe7676678d8\",\n\"size\": 0,\n\"url\": \"https://api.github.com/repos/openai/codex/content...


I have the directory layout. The repo has been refactored (`codex.rs` → `codex_thread.rs` / `thread_manager.rs` / `state/` / `agent/` / `context_manager/`). Let me try cloning shallowly so I can grep actual source, falling back to web reads if SSL blocks it.

Excellent — full source at commit `283f343`. Now I can grep directly.

The repo is heavily refactored — session logic now lives in `core/src/session/` and `core/src/context_manager/`. Let me read the key files.

Now let me find `get_total_token_usage` on `Session` and where it's updated.

This is the heart of it. Now let me read `state/session.rs` (token state accessors) and `compact.rs`.

`recompute_token_usage` after compaction is exactly the stale-measurement handling I'm looking for. Let me read the relevant sections of `session/mod.rs`.

This is the crux — `recompute_token_usage` overwrites provider usage with a local estimate after compaction. Now let me trace the trigger sites, error recovery, and persistence.

The trim-and-retry escape hatch confirmed at compact.rs:318-327. Now the auto-compact window state and usage ingestion points.

`CompactedItem` persists a `latest_token_usage_record` snapshot for resume. Let me see how replay reconstructs token state.

Resume seeds token info from the last persisted `TokenCount` event. Let me verify whether `TokenCount` events are actually persisted, and check the compaction persistence path.

`TokenCount` events are persisted. Now the compaction-response usage path and server-side compaction.

One quick check on how the UI consumes the two usage numbers, then I'll wrap up.

All questions answered. Let me clean up the temporary clone.

# Codex-rs context/token tracking survey (openai/codex @ commit `283f343`, main, 2026-09-04)

Note on drift: the repo has been refactored since the prior survey — there is no more `codex-rs/core/src/codex.rs` or `token_data.rs`. The logic now lives in `core/src/session/` (turn loop), `core/src/context_manager/history.rs` (history + token info), `core/src/state/` (session state, auto-compact window), `core/src/compact.rs` / `compact_remote_v2.rs`, and `protocol/src/openai_models.rs` (thresholds).

## 1. Where the context-size number comes from

It is a hybrid, and the load-bearing function is `ContextManager::get_total_token_usage` — `codex-rs/core/src/context_manager/history.rs:677-694`:

```rust
pub(crate) fn get_total_token_usage(&self, server_reasoning_included: bool) -> i64 {
    let last_tokens = self
        .token_info
        .as_ref()
        .map(|info| info.last_token_usage.total_tokens)
        .unwrap_or(0);
    let items_after_last_model_generated_tokens = self
        .items_after_last_model_generated_item()
        .map(estimate_item_token_count)
        .fold(0i64, i64::saturating_add);
    if server_reasoning_included {
        last_tokens.saturating_add(items_after_last_model_generated_tokens)
    } else {
        last_tokens
            .saturating_add(self.get_non_last_reasoning_items_tokens())
            .saturating_add(items_after_last_model_generated_tokens)
    }
}
```

So: **last response's reported total (absolute — it describes the whole prompt the model just saw) + local byte-heuristic estimates for items appended after the last model-emitted item** (plus estimates of prior encrypted reasoning items when the server doesn't already count them, gated by `ResponseEvent::ServerReasoningIncluded`, `turn.rs:2635`).

Per-item counts are **not stored**. They are computed on demand by `estimate_item_token_count` (history.rs:812-815): JSON-serialized bytes through `approx_tokens_from_byte_count_i64` (~4 bytes/token), with special cases for images (fixed `RESIZED_IMAGE_BYTES_ESTIMATE = 7373`), audio, encrypted reasoning (`3/4·len − 650`), and encrypted function outputs (`9/16·len`). The comment at history.rs:437-438 says it outright: "This is a coarse lower bound, not a tokenizer-accurate count."

Naming trap worth flagging: `Session::get_total_token_usage` (`session/mod.rs:1337`) is *not* `token_info.total_token_usage`. The accumulator `TokenUsageInfo` (`protocol/src/protocol.rs:2261`) has two distinct numbers:

```rust
pub struct TokenUsageInfo {
    pub total_token_usage: TokenUsage,   // cumulative accounting sum (UI/billing display)
    pub last_token_usage: TokenUsage,    // last response — the active-context anchor
    pub model_context_window: Option<i64>,
}
```

The TUI even documents the split (`tui/src/token_usage.rs:37-38`): "For `last_token_usage`, this is the latest active context size; for `total_token_usage`, this is the accumulated session total."

## 2. Deltas vs cumulative, persistence, timing

- **At response time** (`session/turn.rs:2651-2683`): `ResponseEvent::Completed { token_usage, .. }` → `record_observed_response_completed` → `record_token_usage` (builds a per-response `TokenUsageRecord` with `turn_token_usage`/`thread_token_usage` accumulators, `state/session.rs:199-235`) → `record_token_usage_info` → `update_token_info_from_usage` → `TokenUsageInfo::new_or_append` which does `total += usage; last = usage` (`protocol.rs`, `append_last_usage`).
- **Persisted in the rollout**: per-response `RolloutItem::TokenUsageRecord(record)` (`session/mod.rs:4455`) and `EventMsg::TokenCount` carrying the full `TokenUsageInfo` (`rollout/src/policy.rs:113-119` marks `TokenCount(_)` as persisted). History items persist as `ResponseItemEnvelope { item, metadata }` where the only token-related metadata field is `fallback_token_limit_override` — a truncation allowance, **not** a token count (`history/src/lib.rs:54-56`). So: usage is response-granular and persisted; item-granular counts don't exist anywhere.
- **On resume** (`session/mod.rs:1496-1503, 1696-1716`): token info is seeded from the **last persisted `TokenCount` event**; the accounting record is seeded from the last `TokenUsageRecord` **or from `CompactedItem.latest_token_usage_record`** if a compaction marker intervenes first. `CompactedItem` (`history/src/lib.rs:186-203`) carries the full replacement history plus that usage snapshot: "`thread/resume` can restore token usage totals from this field without scanning arbitrarily far past the compaction."

## 3. After compaction: explicit recompute (the stale-referent fix)

Both implementations — client-side (`compact.rs:401`) and server-side ResponsesCompact v2 (`compact_remote_v2.rs:359`) — call this immediately after installing the replacement history:

```rust
sess.replace_compacted_history(...).await;
sess.recompute_token_usage(&turn_context).await;
```

`recompute_token_usage` (`session/mod.rs:4495-4533`) **discards the provider-reported `last_token_usage` and replaces it with a local estimate of the new history**:

```rust
let Some(estimated_total_tokens) =
    history.estimate_token_count_with_base_instructions(&base_instructions) else { return; };
...
info.last_token_usage = TokenUsage {
    ...
    total_tokens: estimated_total_tokens.max(0),
};
state.set_token_info(Some(info));
```

The compaction request's own usage *was* recorded just before (compact.rs:812-820) — and it described the pre-compaction prompt (full history + appended `SUMMARIZATION_PROMPT`, same base instructions, empty toolset via `Prompt { input, base_instructions, ..Default::default() }`, compact.rs:261-290 — your prior survey's characterization still holds). `recompute_token_usage` supersedes it in the same breath, emits a fresh (persisted) `TokenCount` event, and sets the estimated prefill baseline for the new auto-compact window (`replace_annotated_history` also calls `auto_compact_window.clear_prefill()`, `state/session.rs:192`). The accounting total (`total_token_usage`) is untouched — it's lifetime spend, not context.

## 4. Auto-compact trigger and thresholds

`context_window_token_status` (`session/context_window.rs:52-121`):

- `active_context_tokens = sess.get_total_token_usage()` — i.e. **last-response usage + local appendages**, not item sums.
- Default scope (`Total`): limit = `model_info.auto_compact_token_limit()` — `openai_models.rs:515-524`: `(context_window * 9) / 10`, with any config-supplied limit clamped to `min(limit, 90%)`.
- Hard cap: `full_context_window_limit = context_window × effective_context_window_percent / 100`, default **95** (`openai_models.rs:385-386, 83-85`).
- `token_limit_reached = scope_tokens ≥ (auto_compact_limit + fallback_buffer_tokens) || full_context_window_limit_reached` (context_window.rs:104-109).
- `BodyAfterPrefix` scope subtracts the window's prefill baseline — the first **server-observed** `input_tokens` of the window (`state/auto_compact_window.rs:108-119`), falling back to an estimate until a real response arrives.

Trigger sites in `session/turn.rs`: pre-turn (`run_pre_sampling_compact`, :1093-1122) and mid-turn (:510-549):

```rust
let should_roll_over = needs_follow_up
    && (sess.take_new_context_window_request().await || token_limit_reached);
...
if should_roll_over {
    if let Err(err) = run_auto_compact(..., CompactionReason::ContextLimit, CompactionPhase::MidTurn).await { ... }
    continue;  // rebuild the prompt from the compacted history and re-sample
}
```

plus a comp-hash-change compaction on model switch (:1161+, `CompactionReason::CompHashChanged`).

## 5. Overflow recovery and where the limit comes from

- Detection: SSE `response.failed` with `code == "context_length_exceeded"` (`codex-api/src/sse/responses.rs:710-712`) → `CodexErrorDetails::ContextWindowExceeded`.
- In the sampling loop (`turn.rs:1487-1491`): `sess.set_total_tokens_full(&turn_context)` then return the error. `set_total_tokens_full` → `TokenUsageInfo::fill_to_context_window` (`protocol.rs`): sets `last_token_usage` to the full window so the **next pre-turn check trips `token_limit_reached` and forces compaction**.
- Inside compaction itself (`compact.rs:318-331`) — the trim-and-retry escape hatch, still present:

```rust
Err(e) if matches!(e.details(), CodexErrorDetails::ContextWindowExceeded) => {
    if turn_input_len > 1 {
        // Trim from the beginning to preserve cache (prefix-based) and keep recent messages intact.
        history.remove_first_item();
        retries = 0;
        continue;
    }
    sess.set_total_tokens_full(turn_context.as_ref()).await;
```

- What teaches the limit: nothing error-derived anymore — the old "parse maximum context window from the error message" code is gone from core. The window comes from `ModelInfo` served by the Codex backend `/models` endpoint (`usable_context_window()` = window × 95%, `turn_context.rs:529-533`), with an optional user config override `model_context_window` (`config/mod.rs:629`). The overflow error only *activates* the already-known limit by marking usage full.

## Verdict

**Codex is a hybrid, and it avoids the stale-referent problem by construction — but via invalidation-on-rewrite, not via per-item deltas.**

- The *active-context* number is anchored on the last provider response (inherently describes the current window as the model saw it), bridged between responses by on-demand byte-heuristic estimates of appended items. There are **no persisted per-item token counts at all** — the history itself is the source of truth; per-item numbers are recomputed from bytes whenever needed.
- The *accounting* number (`total_token_usage`) is a plain cumulative sum, persisted per response, used only for display/budget — it never feeds the compaction trigger, so it can never go stale in the harmful sense.
- The stale-referent bug tabit hit is exactly what `recompute_token_usage` exists to prevent: every site that rewrites history (compaction, both client and server-side) owns the invalidation of the measurement in the same operation, re-anchoring to a local estimate of the *new* history. Until the next real model response re-anchors it, codex deliberately accepts heuristic inaccuracy rather than keep a provider number whose referent no longer exists.
- The resume path is covered too: post-compaction, the last persisted `TokenCount` event is the recompute event describing the compacted history, and `CompactedItem.latest_token_usage_record` checkpoints accounting across the boundary.

Implication for tabit's per-item-delta idea: codex demonstrates the alternative — keep the provider anchor, make history-rewriting sites explicitly invalidate and re-anchor (estimate is fine as a bridge), and keep the cumulative accounting total strictly separate from the context measurement so it can never be misread as "context size." A per-item delta sum would also avoid the stale referent by construction (you only ever sum live items), but it is never provider-accurate; codex pays accuracy only in the window between a rewrite and the next response, and gets exact numbers everywhere else.
