# VENDOR.md

Historical record of how this workspace was seeded from the [rig](https://github.com/0xPlaygrounds/rig)
Rust AI framework. The rig source was vendored as a starting point for **tabit**
— borrowed as source rather than an external crate so it can be modified
freely. The tree is tabit's own code now; this document records the initial
vendoring state for provenance only and constrains nothing.

## Source

- Upstream: rig **0.41.0** (`0xPlaygrounds/rig`)
- Local upstream checkout used for vendoring: `C:/Users/lrzx_/Projects/Agents/rig`
  (its `HEAD` when vendoring; now `C:/Users/Jerry/Projects/agents/rig`)

## What was vendored

| Crate | Upstream path | Notes |
|---|---|---|
| `rig-core` | `crates/rig-core` | Core providers, completion/streaming contracts, tools, memory traits. Verbatim except the trims below. |
| `rig-agent` | `crates/rig-agent` | Classic agent runtime. Verbatim. |
| `rig-derive` | `crates/rig-derive` | `rig_tool` proc-macros. Verbatim except one fixture path fix. |
| `rig` | workspace root package | The facade (pure `pub use` re-exports). Trimmed of all companion crates. |

The workspace root is a virtual manifest (`members = ["crates/*"]`), with
`default-members` = the four crates above, `resolver = 3`, and upstream's
`[workspace.package]` / `[workspace.lints.clippy]` / `[workspace.dependencies]`
/ `[profile.release]` sections carried over (minus the facade self-entry).
`Cargo.lock` is committed for reproducibility.

## Trims and deviations

### rig-core providers

Only three provider families are kept in `crates/rig-core/src/providers/`:

- `anthropic/` — Anthropic Messages API (SSE streaming included)
- `openai/` — OpenAI chat-completions path **and** Responses API
- `internal/` — the shared OpenAI-chat-compatible engine (`wire.rs`,
  `adapter.rs`, `tool_call_bridge.rs`, `auth.rs`,
  `openai_chat_completions_compatible.rs`)

**Deleted providers:** azure, chatgpt, copilot, cohere, deepseek, doubleword,
gemini, groq, huggingface, hyperbolic, llamafile, minimax, mira, mistral,
moonshot, ollama, openrouter, perplexity, together, voyageai, xai, xiaomimimo,
zai.

`providers/mod.rs` keeps its module list and (useful) implementation-checklist
doc section, updated for the trimmed set. Engine items whose only callers were
deleted providers (`AuthError`, `resolve_tool_result_names`, the buffered
ChatGPT-replay helpers, etc.) were initially kept verbatim and later deleted —
the tree is owned code, not a frozen upstream copy.

### Companion crates (never vendored)

The facade has **no** companion-crate dependencies: no rig-bedrock, rig-candle,
rig-fastembed, rig-memory, rig-* vector stores, rig-gemini-grpc, rig-vertexai,
etc. Consequently the facade drops all companion `pub mod` blocks
(bedrock/candle/fastembed/gemini_grpc/helixdb/lancedb/milvus/mongodb/neo4j/
postgres/qdrant/s3vectors/scylladb/sqlite/surrealdb/vectorize/vertexai) and the
`rig_memory` re-export inside `rig::memory`. The companion feature names are
removed from `[features]` as well. Also deferred (removed): `discord-bot`,
`rmcp` (the `rmcp` re-export in `rig::tool` is omitted with a note), and
`facade-build-tests` / the `tool_facade_*` test machinery.

### Websocket

The `websocket` feature was initially retained (rig-core's optional
`tokio-tungstenite` runtime dep intact, not in default features) and later
**removed entirely**: `responses_api/websocket.rs`, the client entry points
(`responses_websocket`/`responses_websocket_builder`), the three feature
flags, the `tokio-tungstenite` dependency, and the
`openai_responses_websocket` conformance family are gone. Rationale: the
owner confirmed websocket is not needed (pi's precedent — SSE everywhere
except an optional Codex-only websocket accelerator with SSE fallback). The
Responses replay/merge helpers the websocket session used survive only as
`#[cfg(test)]` harnesses for the inline tests.

### `test_utils/streaming_conformance.rs` trim (fallback taken)

`streaming_conformance.rs` (a 2908-line test-support module compiled under the
`test-utils` feature) embeds per-provider scenario fixtures. We first tried
gating the module behind `feature = "websocket"` (since websocket was
deferred), but the anthropic cassette test `streaming_grammar.rs` — brought in
with the offline suite — needs `assert_valid_event_stream` without websocket.
Per the plan's fallback we instead **surgically trimmed the module**:

- removed the `gemini_rest`, `interactions` (gemini), `cohere`, and `ollama`
  fixture submodules and the chatgpt-backed `buffered_driver()` (its only
  callers were the deleted provider crates);
- `WIRE_FAMILIES` initially listed only `openai_chat`, `openai_responses`,
  `openai_responses_websocket`, `anthropic`; the websocket family was later
  removed with the websocket feature itself (the list now carries the three
  SSE families).

The openai + anthropic fixtures, and everything `openai/responses_api/mod.rs`
imports (`fixtures`, `ok_chunks`, `WireInput`, …), are preserved and the
module stays unconditionally available (native only, as upstream).

### Structural test fixes

- `crates/rig-core/tests/driver_adoption.rs`: `WALK_FLOOR_FILES` and the
  scope-floor list drop the `rig-bedrock` / `rig-gemini-grpc` companion entries;
  the deleted `providers/ollama.rs` scope-floor entry is replaced by the kept
  `providers/openai/completion/streaming.rs`; `SINGLE_FILE_STREAMING_MODULES`
  is empty (all its single-file providers were deleted; the const is retained
  with a note as the scan's hook for future providers).
  `foreign_adapter_files_are_not_exempt` still cites a synthetic
  `crates/rig-bedrock/src/streaming/adapter.rs` path (no file needed) — left as
  upstream wrote it.
- `crates/rig-core/tests/serde_policy_allowlist.txt`: removed the stale
  `ollama.rs` / `copilot/mod.rs` entries; kept anthropic/openai/internal ones.
- `crates/rig-derive/tests/fixtures/facade_renamed/Cargo.toml`: the facade
  path-dependency points at `../../../../rig` (upstream: the workspace root,
  which *was* the facade package; here the facade is `crates/rig`).

### Offline cassette tests (facade)

`crates/rig/tests/` carries upstream's workspace-root provider test harness for
anthropic + openai, replayed fully offline (do **not** set
`RIG_PROVIDER_TEST_MODE`):

- `anthropic.rs`, `openai.rs` (thin shells) + `common/`
  (`cassette_safety.rs`, `cassettes.rs`, `reasoning.rs`, `support.rs`)
- `providers/{anthropic,openai}/{mod.rs,support.rs,cassette/}` — the `.rs`
  cassette test modules
- `cassettes/{anthropic,openai}/` — the recorded `.yaml` interactions the
  harness resolves via `env!("CARGO_MANIFEST_DIR") + "/tests/cassettes/<provider>/<scenario>.yaml"`
- `data/` fixtures actually read at replay time:
  `camponotus_flavomarginatus_ant.jpg`, `file-id-verifiers.pdf`

Not brought: upstream's `tests/cassettes/` openrouter scenarios, `tests/data/loaders/`,
and the network-only `live/` modules. For openai, upstream's
`mod live { … }` submodules are all `#[ignore]`-marked tests requiring a real
`OPENAI_API_KEY` (plus `reqwest/multipart` for compilation); they are dropped
and `common/cassette_safety.rs`'s `PROVIDER_CASSETTE_SUITE` registry is
trimmed to anthropic + openai accordingly. The empty `mod live {}` placeholder
shells were later removed entirely.

### Dependency pins for cassette fidelity

Upstream's recorded cassettes are sensitive to transitive-version behavior, so
`Cargo.lock` pins several crates to upstream's locked versions:
`schemars`/`schemars_derive` **1.2.1** (1.2.2 changes JSON-schema ordering),
`serde_json` **1.0.150**, `serde` **1.0.228**, `aws-smithy-eventstream`
**0.60.18**, `aws-smithy-types` **1.4.3**.

The anthropic `sanitize_schema` rebuilds the schema's `required` list from
`properties.keys()`, so map insertion order is behavior: `rig-core` enables
serde_json's `preserve_order` on its real dependency (indexmap is already a
direct dep), making shipped ordering deterministic and matching the recorded
cassette bodies. (Initially this was a facade dev-dependency workaround
reproducing upstream's feature unification; it was promoted to the real
dependency so tested and shipped behavior agree.)

### Line endings

`.gitattributes` forces LF for all text files (`* text=auto eol=lf`) and marks
media fixtures binary. Cassette YAMLs are byte-sensitive (interaction splits on
`\n---\n`, scrubbed-form checks), so a CRLF checkout breaks replay. The
`file-id-verifiers.pdf` fixture must be the pristine LF blob (a Windows
`autocrlf` checkout of upstream corrupts it); it was restored from upstream's
git object store.

## Model catalog removal

The vendored providers are now a **pure API abstraction**: users supply all
model information via their own config; nothing in the vendored layer branches
on model names or ships a model catalog. Three changes were made (one commit
each):

### 1. `max_tokens` (Anthropic): caller-supplied, one plain default

- `default_max_tokens_for_model` / `default_max_tokens_with_fallback` (the
  name-keyed inference of Anthropic's required `max_tokens` from the model
  name) are deleted, along with the `AnthropicCompatibleProvider::
  default_max_tokens` extension hook and the `GenericCompletionModel::
  default_max_tokens` field.
- Current contract: requests carrying `max_tokens` pass it through verbatim;
  requests without one get `anthropic::DEFAULT_MAX_TOKENS` (65,536) — a
  plain, documented provider constant that per-model config overrides. (The
  intermediate "fail loudly when missing" contract was replaced by the
  owner's decision: a safe 64K default beats an error, and config can
  override trivially.)
- Test suites set `.max_tokens(N)` explicitly, with `N` taken from the
  recorded cassette request bodies so replay still matches byte-for-byte;
  the missing-`max_tokens` test asserts the 64K default at the
  request-conversion layer.

### 2. Mid-conversation system messages are always hoisted (Anthropic)

- `supports_mid_conversation_system_messages` (the `claude-opus-4-8`-only
  gate), `is_valid_mid_conversation_system_message`, and
  `assistant_ends_in_server_tool_block` are deleted;
  `split_system_messages_from_history` no longer takes a preserve flag.
- All system messages in chat history are hoisted/merged into the top-level
  `system` parameter (the previous default path for every non-opus-4.8
  model). Mid-history `role: "system"` entries are never emitted.

### 3. Model catalog constants stripped

All model-name `pub const` aliases are deleted from the vendored providers
and every usage (rig-core inline tests + `test_utils`, rig-agent, facade
tests, doctests, rig-derive examples) was replaced with the plain string
literal:

- anthropic: `CLAUDE_OPUS_4_6/4_7/4_8`, `CLAUDE_SONNET_4_6`,
  `CLAUDE_HAIKU_4_5` (the `ANTHROPIC_VERSION_*` constants are kept — they are
  API version headers, not models)
- openai: all `GPT_*`, `O1_*`, `O3_*`, `O4_MINI*`, `TEXT_EMBEDDING_*`,
  `DALL_E_*`, `GPT_IMAGE_*`, `TTS_1*`, `WHISPER_1`

Two pieces of **name-keyed embedding behavior** (not in the original recon)
were found in `openai/embedding.rs` and removed for consistency:
`model_dimensions_from_identifier` (inferred `ndims` from the model name;
  `ndims` is now caller-supplied only, defaulting to 0 = "don't send
  `dimensions`") and the `text-embedding-ada-002` special case that
  suppressed the `dimensions` request parameter.

`model_listing.rs` modules are kept unchanged — they call the live
`GET /v1/models` endpoint (API surface, not a catalog).

### Deleted cassette scenarios (facade)

The opus-4.8 mid-conversation system-preservation scenarios exercise the
  removed behavior and were deleted (tests + YAML):

- `anthropic/opus_4_8/messages_preserve_mid_conversation_system_role`
- `anthropic/opus_4_8/messages_preserve_system_role_after_server_tool_result`

The remaining opus_4_8 scenarios (`web_search_with_dynamic_filtering_succeeds`,
`documents_keep_leading_system_message_top_level`) do not depend on the
removed behavior and were kept. The rig-core inline tests
`opus_4_8_preserves_mid_conversation_system_message`,
`opus_4_8_preserves_mid_conversation_system_message_before_assistant_turn`,
`opus_4_8_preserves_system_message_after_assistant_server_tool_result`,
`opus_4_8_preserves_system_message_after_assistant_server_tool_use`, and
`opus_4_8_hoists_system_message_in_invalid_mid_conversation_position` were
deleted (the hoisting path is still covered by
`documents_hoist_leading_and_mid_conversation_system_messages` and
`older_anthropic_models_hoist_mid_conversation_system_message`).

## Known deviations summary

1. Providers trimmed to anthropic + openai (+ compat engine) as above.
2. Facade has no companion crates/features; `rmcp`/`discord-bot` deleted.
3. `streaming_conformance.rs` provider scenarios trimmed (fallback approach).
4. Openai network-only live tests dropped (`mod live {}` later removed).
5. `serde_json` `preserve_order` enabled on `rig-core`'s real dependency
   (insertion-ordered maps are shipped behavior; see the cassette-fidelity
   section).
6. `driver_adoption.rs` / serde allowlist / facade_renamed fixture adjusted for
   the trimmed tree (documented above).
7. Dead code left by the provider trim was deleted once the tree was owned; the
   websocket feature was removed outright. The Responses replay/merge helpers
   whose shipped callers disappeared survive as `#[cfg(test)]` harnesses for
   the inline tests. The default build compiles with zero warnings.
8. Transport layer rewritten (phase 1 hardening): the SSE reconnect /
   last-event-id machinery and the old `ExponentialBackoff` are deleted —
   providers fail the stream on the first transport error, and retry belongs
   to the request layer. A status-aware retry (pi's policy: 408/409/429/5xx +
   `x-should-retry`, `retry-after-ms`/`retry-after` incl. HTTP-date with a
   60 s server-delay cap, jittered bounded backoff, default 2 retries,
   zero-body-bytes retry boundary) plus connect/idle timeouts
   (`ClientBuilder::connect_timeout`/`idle_timeout`) were added to the
   generic `Client`. HTTP error variants preserve response headers.
9. Provider hardening (phase 1): Anthropic content blocks route per index
   with loud interleave guards (malformed streams fail, never silently
   corrupt); orphan tool results fail request conversion naming the id;
   `Usage` carries the `cache_creation` 1 h/5 min breakdown; chat-completions
   `delta.refusal` is modeled (refusal text visible, `ContentFilter`
   finish reason).
8. Doctests: all green (0 failures; 10 ignored upstream-marked).
9. Model catalog removed: no model-name constants or name-keyed behavior in
   the vendored providers (see "Model catalog removal"); callers supply model
   ids via config, and `max_tokens` via config with the 64K provider default
   when unset.

### Unconsumed-concern deletions (rig-core shrink)

Deleted outright after a workspace-wide consumer audit (no live consumers
outside the deleted concern itself; grep evidence in the task report):

- `loaders/` (epub, pdf, file, `test_fixtures.rs`) — nothing outside the
  module referenced `FileLoader`/`loaders` (facade `support.rs` carried only
  dead `allow(dead_code)` constants for a `tests/data/loaders/` fixture dir
  that was never vendored; removed with it). Features `pdf`, `epub` and deps
  `epub`, `lopdf`, `quick-xml`, `glob` removed; `assert_fs` dev-dep became
  unused and was pruned (workspace entry too).
- `rerank.rs` + `client/rerank.rs` — no provider declares the rerank
  capability (anthropic and openai both `Nothing`); only the blanket
  `RerankingClient` impl and its own mock test consumed the types.
- `transcription.rs` + `client/transcription.rs` +
  `providers/openai/transcription.rs` — the OpenAI impl had no caller outside
  the concern; deleted as a unit.
- `audio_generation.rs` + `client/audio_generation.rs` +
  `providers/openai/audio_generation.rs`, and the `image` equivalents — the
  `audio`/`image` features were non-default and enabled by nothing in the
  workspace (facade/agent only forwarded them). Features removed everywhere.
- `Capabilities` lost the `Rerank`/`Transcription` (and feature-gated
  `ImageGeneration`/`AudioGeneration`) associated types; the four blanket
  client impls and `json_utils::merge_inplace` (whose only callers were the
  image/audio request builders) went with them.
- `vector_store`: initially kept `VectorStoreIndex`/`VectorStoreIndexDyn`/
  `VectorSearchRequest`/`Filter` and `InMemoryVectorStore` (consumed by
  rig-agent RAG paths); later deleted whole with the RAG mass (see
  "RAG mass removal" below).

## RAG mass removal (ruled 2026-08)

Reviewer-round item "vendored-mass policy", resolved in three rulings:

- **Deleted — embeddings + vector stores + retrieval plumbing.** No tabit
  consumer exists or is planned: tabit's tools are `PortableTool`/
  `DynamicTool` registrations, always exposed; rig-agent's vector-retrieval
  path (`add_retrieval_index`/`snapshot_tool_defs`'s search branch) ran over
  an empty index list in every tabit run. Removed: `rig-core/src/embeddings`,
  `rig-core/src/vector_store`, `client/embeddings.rs`,
  `providers/openai/embedding.rs`, the `#[derive(Embed)]` macro and its
  `rig-derive/src/embed`, the `ToolEmbedding`/`PortableToolEmbedding`/
  `ErasedEmbeddingTool`/`RegisteredTool::Embedding` tool category,
  `always_exposed`/`add_retrievable_tools` (every tool is always exposed
  now), `Message::rag_text`, the `Embeddings` capability, `ToolServerError`
  (snapshot can no longer fail), and `get_tool_defs`/`tool_definitions`/
  `snapshot_tool_defs`'s prompt parameter. If a memory feature ever needs
  embeddings, it comes back as a purpose-built provider client, not this
  trait zoo.
- **Kept — model listing** (`model/listing.rs`, `client/model_listing.rs`,
  both provider listers, cassette-covered). Planned consumer: dynamic
  listing merged with local config in the registry (ROADMAP). The call is
  backend-only by construction (credentials + the front/back split).
- **Telemetry — trimmed to bare spans** (ruled 2026-08, same round).
  Deleted: the 2k-line GenAI semantic-conventions module
  (`telemetry/`), its `completion_parent_span!` adoption contract and
  fixtures, `CompletionSpanBuilder`/`SpanCombinator`/`ProviderResponseExt`,
  `record_model_input/output`, `system_instructions_json`, and the
  `record_telemetry_content` opt-in plumbing through the completion
  request builder, agent/runner builders, and both turn sources.
  Kept: plain `tracing` spans carrying identity only — `invoke_agent`
  (streaming only; the blocking surface's agent span existed solely as
  a recording target and died with it), per-turn `chat`/`chat_streaming`
  (operation/provider/model/agent name), per-tool `execute_tool`
  (name/id/outcome; argument and result *content* recording died with
  the flag), and the blocking `follows_from` chain. Reference survey:
  codex ships local file logs plus opt-in `[otel]` export, opencode's
  built-in OTel is flag-gated and controversial, pi ships none — none
  carries a library conventions module. If tabit ever wants
  observability it comes back app-level and config-gated (a local log
  file needs only `tracing` + a file subscriber).

## Cherry-picking from upstream (optional)

The tree is owned and diverges freely, so there is no obligation to track
upstream. If you ever want to port a specific upstream change, the original
trim list above is the map of what differs from rig 0.41.0:

1. Diff the relevant crates against the upstream tag; port the change however
   it best fits the tree as it stands by then.
2. Mind the offline-test machinery: cassette YAMLs are byte-sensitive
   (LF endings, pinned serde/schemars versions), and `driver_adoption.rs` /
   `serde_policy_allowlist.txt` encode structural expectations about the
   provider tree.
3. Re-run `cargo build && cargo test --lib --tests && cargo test --doc`.

## Upstream triage 2026-08 (`9b9c428e..abe338a7`, 75 commits)

Every commit in the range was read against this tree. The range is mostly
unreachable from tabit: ~15 provider fixes live in deleted providers,
~11 are "LOC consolidation" refactors over code we have since reshaped
(no defects on kept surfaces — the one claiming "5 defect fixes" puts
them all in deleted providers), plus type-erasure sweeps, ownership
audits, and release/CI/deps chores. `bb6b6cb7` + `57446c10` cancel
(fix + full revert).

**Adopted** (each ported with offline tests; tabit conventions kept —
the empty-text sentinel, Value-based SSE errors):

| upstream | ported as | what |
|---|---|---|
| `2bfd9724` | a58641b | streamed terminal carries the matched `stop_sequence`; empty turn stopped on a *named* sequence normalizes (unnamed stays guarded) |
| `a8e5372a` | a58641b | thinking tokens → `reasoning_tokens` via a shared `anthropic_usage_totals` (breakdown, never added to totals); listing loop breaks on `has_more` without `last_id` |
| `ffd04804` | a58641b | `model_length` maps to `FinishReason::Length` in the compat engine |
| `923e7fba` | 66eb946 | `additional_params` function tools merge into the typed tool list (chat completions) |
| `d094fe1e` | 66eb946 | n>1 streams answer from candidate 0 (`choices[].index`) |
| `2dfb3cc8`+`69aeac78` | 48c6636 | embeddings builder: slot-indexed landing; results follow input order at both levels |
| `1a6a6adc`/`91098e2a` (carrier only) | f4d785c | `CompletionCall` carries per-turn `finish_reason` — feeding tabit's `turn_truncated` warning (ENGINE.md delta 9), not upstream's fail-the-turn |

**Skipped on doctrine** (would violate AGENTS.md design rules):

- `57b4ad2b` (Length-gated drop of partial tool calls): rule 8 keeps
  call-level handling uniform — a partial call from a length cap is
  handled exactly like one from broken output (owner ruling 2026-08;
  the turn-level story is carried by `turn_truncated` instead).
- `64cb64f3`'s `max_completion_tokens` rename: keyed on gpt-5/o-series
  model names — rule 1 (no model catalog). If ever needed, it should be
  a config-declared parameter mapping.
- `d525224e` ndims tables: ndims is caller-supplied here by ruling.

**Skipped as inapplicable**: `6963ab08` (error headers end-to-end — our
retry is a from-scratch SDK-semantics port at the request layer, where
`Error::NonSuccessResponse.headers` already feeds `retry-after`/`x-should-retry`;
the fix targets consumers above normalization, which we don't have),
`841d2759` (`serialize_map_sorted` — our request prefix is built once
per session from ordered Values; no HashMap in the wire path),
`23f1cf6a` (image tool results — no image-producing tool yet),
`3de43b96` (`on_reasoning_delta` hook — events already reach frontends;
no in-process consumer), `4487ba29`/`638a6c15`/`0e1fdcd7` (raw response
access, response identity, error request-ids — no product pull),
`f27d94ab` (drop `#[non_exhaustive]` — cosmetic for internal crates),
and the consolidation/erasure/audit sweeps wholesale.

**Deferred with a home**: `4be867de` (per-breakpoint cache TTL) and
`46c436b6` (anthropic strict tools) → ROADMAP item 10 / config knobs.
The embeddings ndims-style deferral is moot — the embeddings module is
deleted (RAG mass removal below).
