# VENDOR.md

This workspace vendors a trimmed subset of the [rig](https://github.com/0xPlaygrounds/rig)
Rust AI framework for use as the foundation of **tabit**. No tabit-specific
agent/session/subagent/tool logic lives here — this tree is purely vendored
upstream code plus the trims and wiring documented below.

## Source

- Upstream: rig **0.41.0** (`0xPlaygrounds/rig`)
- Local upstream checkout used for vendoring: `C:/Users/lrzx_/Projects/Agents/rig`
  (its `HEAD` when vendored)

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
doc section, updated for the trimmed set. Some `internal` engine items
(e.g. `AuthError`, `resolve_tool_result_names`) are now dead code because only
deleted providers called them; they are kept verbatim and produce
`dead_code` warnings — harmless and intentional (the engine stays upstream-
faithful).

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

The `websocket` feature (and its rustls/native-tls variants) is **retained**
(`rig-core`'s optional `tokio-tungstenite` runtime dep is intact), but the
websocket conformance test `tests/streaming_conformance_websocket.rs` was
removed in Phase 1 and websocket is not in default features.

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
- `WIRE_FAMILIES` now lists only `openai_chat`, `openai_responses`,
  `openai_responses_websocket`, `anthropic`.

The openai + anthropic fixtures, and everything `openai/responses_api/{mod.rs,
websocket.rs}` imports (`fixtures`, `ok_chunks`, `WireInput`, …), are preserved
and the module stays unconditionally available (native only, as upstream).

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
and `mod live {}` left empty (anthropic's was already empty), noted in the
file. `common/cassette_safety.rs`'s `PROVIDER_CASSETTE_SUITES` registry is
trimmed to anthropic + openai accordingly.

### Dependency pins for cassette fidelity

Upstream's recorded cassettes are sensitive to transitive-version behavior, so
`Cargo.lock` pins several crates to upstream's locked versions:
`schemars`/`schemars_derive` **1.2.1** (1.2.2 changes JSON-schema ordering),
`serde_json` **1.0.150**, `serde` **1.0.228**, `aws-smithy-eventstream`
**0.60.18**, `aws-smithy-types` **1.4.3**.

The facade's serde_json dev-dependency additionally enables `preserve_order`.
Upstream gets serde_json map insertion-order via feature unification from some
crate in its full (companion-heavy) dev graph; our trimmed graph does not, and
the anthropic `sanitize_schema` rebuilds the schema's `required` list from
`properties.keys()`, so replay bodies must match the recorded insertion order.
This is called out in `crates/rig/Cargo.toml`.

### Build artifacts on D:

`.cargo/config.toml` sets `target-dir = "D:/cargo-target/tabit"` because the C:
drive is space-constrained. **Do not change this.**

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

### 1. `max_tokens` is pure pass-through (Anthropic)

- `default_max_tokens_for_model` / `default_max_tokens_with_fallback` (the
  name-keyed inference of Anthropic's required `max_tokens` from the model
  name) are deleted.
- The `AnthropicCompatibleProvider::default_max_tokens` extension hook is
  **kept** but its default (and the `AnthropicExt` impl) now returns `None` —
  it is an optional override point for compat providers, no longer
  name-keyed.
- New contract: the caller **must** set `max_tokens` on the completion
  request (e.g. via config / `AgentBuilder::max_tokens` /
  `CompletionRequestBuilder::max_tokens`). If it is missing and the extension
  hook does not supply one, both the non-streaming and streaming paths fail
  with:

  > `Anthropic requires \`max_tokens\`; set it on the completion request (e.g. via config)`

  There are no silent defaults anywhere.
- Test suites that previously relied on the inference now set
  `.max_tokens(N)` explicitly, with `N` taken from the recorded cassette
  request bodies so replay still matches byte-for-byte.

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
2. Facade has no companion crates/features; `rmcp`/`discord-bot` deferred.
3. `streaming_conformance.rs` provider scenarios trimmed (fallback approach).
4. Openai network-only live tests dropped (`mod live {}` kept, documented).
5. `serde_json` `preserve_order` enabled explicitly in the facade dev-deps.
6. `driver_adoption.rs` / serde allowlist / facade_renamed fixture adjusted for
   the trimmed tree (documented above).
7. `rig-core` emits ~16 `dead_code` warnings from now-unused `internal` engine
   items — kept deliberately (minimal-faithful trim).
8. Doctests: all green (0 failures; 10 ignored upstream-marked).
9. Model catalog removed: no model-name constants or name-keyed behavior in
   the vendored providers (see "Model catalog removal"); callers supply model
   ids and `max_tokens` via config.

## Updating from upstream in future

1. Diff the relevant crates against the new upstream tag and port changes
   verbatim where possible.
2. Re-apply the trims: keep only the three provider families in
   `rig-core/src/providers/` (and `mod.rs` list), keep the facade free of
   companion deps/modules, re-trim `streaming_conformance.rs` scenarios,
   `cassette_safety.rs`'s suite registry, `driver_adoption.rs` floors, and the
   serde allowlist for any new/removed provider files.
3. Re-pin the fidelity-sensitive crates (`schemars`, `serde_json`, `serde`,
   `aws-smithy-*`) to upstream's locked versions and re-run:
   `cargo build && cargo test --lib --tests && cargo test --doc`.
4. If new cassettes are recorded upstream, re-copy them with LF endings (and
   pristine binary fixtures from the git object store, never a Windows
   worktree copy).
