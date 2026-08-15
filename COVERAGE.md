# COVERAGE.md

Coverage policy and the record of how every coverage gap has been addressed.

## Policy

Every gap is **filled** (a test exercises it), **justified** (a written reason
it cannot or should not be exercised), or **explicitly deferred** (listed
below with what a future test would need). A raw percentage is not the
contract; the ledger is. If code changes make a justified line reachable,
re-classify it — fill or delete; do not let justifications rot.

Defensive ("unreachable") arms follow a stricter rule:

- **Trivially provable dead** → deleted, not documented. A proof quote
  belongs in the deleting commit, not in a permanent comment.
- **Reachable via internal error** (our own invariant broke) → loud, hard
  failure: the request aborts with an `internal invariant violated:`
  prefixed error — nothing is substituted, nothing is swallowed. (Literal
  panics are lint-denied in shipped code; the hard error is the loudest
  permitted form.)
- **Reachable via external error** (malformed wire input, corrupted
  persisted state) → graceful, clear error naming the malformed input and
  its cause. Never a silent skip or a placeholder that looks like real data.

## Methodology

- `cargo llvm-cov --workspace --html --output-dir target/llvm-cov/html`
  (summary: `cargo llvm-cov report --summary-only`; per-line:
  `--lcov --output-path target/llvm-cov/lcov.info`).
- The whole suite runs offline (cassette replay + test doubles). Doctests
  are NOT included in these numbers (`llvm-cov` was run without
  `--doctests`); they are gated by the same CI run.
- Current state: **96.8% lines / 97.2% regions** (1,750 of 53,984 lines
  uncovered; re-measured after phase 3's ~4.4k-line shrink and the tabit-config
  addition — the percentage dip is denominator loss, absolute misses grew by
  six and were filled). The residue is itemized below. `crates/tabit-config`
  sits at 100% branch coverage; its residue is three partial-line regions in
  tiny helpers (`auth.rs` load/default-path arms, `lib.rs` home-resolution
  arm), with no fully unexecuted line or branch.

## Filled

Three passes over the workspace drove the line coverage from 87.0% to
97.3% (~250 new tests across rig-core, rig-agent, and rig-derive). The
tests themselves are the record; notable full-coverage files include
`completion/message.rs`, `http_client/retry.rs`, `http_client/multipart.rs`,
`client/*`, `embeddings/*`, `loaders/file.rs`, `vector_store/*`,
`providers/anthropic/*`, `providers/openai/{client,embedding,model_listing}`,
`providers/internal/wire.rs`, `tool/{output,portable,result}.rs`,
`json_utils.rs`, `agent/{model,tool}.rs`, `rig-derive` (in-crate unit tests
for the proc-macro grammar + `tests/embed_behavior.rs` for the Embed
codegen).

Dead code found during the passes was deleted rather than tested: the
streaming-conformance scenario grid and suite macro, `MockImageGeneratorTool`,
unused `MockHttpResponse::Error` machinery, and various dead `#[cfg(test)]`
helpers (see git history).

## Justified residue

The remaining uncovered lines fall into these categories. Where a file is
named, the classification applies to its current lcov-uncovered ranges.

1. **Assertion arms inside passing tests** (the largest bucket, ~800 lines
   across `agent/prompt_request/streaming.rs`, `test_utils/model_conformance.rs`,
   `agent/runner.rs`, `agent/run/mod.rs`, `tool/mod.rs`, provider test
   modules): `panic!`/let-else/`_ =>` arms and negative-assertion recorders
   that only execute when the enclosing test is *failing*. Executing them
   means the invariant under test broke; they are the failure message, not
   missing coverage. Panic-canary hooks (`PanicOnUnknownToolHook` etc.) are
   the same pattern in test-fixture form.
2. **Defensive arms in shipped code, post-audit**: every arm once labeled
   "unreachable by construction" has been audited. Provably dead ones were
   deleted with proofs in the audit commits (`OneOrMany::many`-after-guard
   closures, shadowed index guards, `json!`-always-object closures,
   streaming-parts `else` arms, the derive's `custom_func_path` fallback,
   the `rollback_messages` `None` guards). The survivors fall in two
   classes, each identifiable by its message: `internal invariant
   violated:`-prefixed errors (our own serialization/state invariants —
   e.g. serde `to_value` on typed content, the multipart `Mime` re-parse,
   the driver-fold Done guarantee) and external-input errors naming the
   malformed input (deserialized run state, malformed SSE frames — the
   SSE parser-error arm surfaces a named error instead of skipping).
3. **Trait-required methods on test doubles** never queried by their tests
   (`top_n_ids`/`top_n` halves of mock vector indexes, `record_debug`
   visitors, `Write::flush`, blocking-only telemetry `stream`): required
   for the trait object to exist; driving them would test the mock, not
   the product.
4. **Compile-time assertions**: `const _: fn() = || …` Send/Sync bodies in
   `tool/result.rs` and `tool/extensions.rs` never execute at runtime by
   design.
5. **tracing nondeterminism**: `trace!`/`warn!` field and message
   expressions only evaluate under an enabled subscriber and are subject
   to tracing's global callsite interest caching; dedicated TRACE-subscriber
   tests exist (anthropic completion/streaming, openai chat streaming) but
   a few body lines stay flakily uncovered in the shared test binary.
6. **Subprocess-executed tests are not attributed**: trybuild compile-fail
   cases and the `dependency_rename` fixture crates run `cargo`/`rustc` as
   subprocesses llvm-cov does not instrument (e.g. the
   contextual-tool-without-runtime-dep arm in `rig-derive`).
7. **Live-network scenarios** are `#[ignore]`d by policy (two
   prompt_request tests requiring API keys; `model_listing` live calls).
   Their request-construction and conversion code is covered offline;
   only the network leg itself is untested by design.
8. **Region/line-mapping artifacts**: closing-brace regions, multi-line
   let-chain counters, and macro-expansion line zeros on paths that
   demonstrably execute (adjacent lines covered).

## Deferred (explicit)

- `agent/runner.rs` — the blocking driver's mid-run `DriveItem` path: no
  current blocking scenario emits a `DriveItem` mid-run; a test needs a
  blocking-mode scenario that yields one to the fold.
- `client/mod.rs` `optional_env_var` `VarError::NotUnicode` branch: a
  non-Unicode env var cannot be set portably from a Rust test process;
  would need a child-process harness.
- Doctests are outside the measurement (see Methodology); they run and are
  gated in CI but do not fold into these numbers.

(Removed from this list after the defensive-arm audit: the SSE
retry-`None` branches — the premise was wrong, `ExponentialBackoff`
returns `None` on max-retries exhaustion and the close-and-surface
handling is exercised by `reconnect_gives_up_after_max_retries`; and the
eventsource-stream parser-error arm — it now surfaces a named error
rather than skipping, reachable or not.)

## Maintenance

- New shipped code arrives with tests that exercise it; new defensive arms
  get a `JUSTIFIED` note here (or a `debug_assert!`-style restructuring if
  the arm is truly impossible).
- Prefer deletion over justification: an unreachable defensive arm is a
  candidate for removal, not documentation.
- Re-run the collection after material changes and re-classify anything
  that moved from justified to reachable.
