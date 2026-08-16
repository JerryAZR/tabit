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
- Current state: **95.75% lines / 96.26% regions** (2,464 of 57,954
  lines; re-measured after the protocol rulings pass: drain-all mailbox
  batches, `prompt`/`prompt_with` unified onto `pump` with failures as
  events (`RunOutcome::Failed`, no `Err` return), deferred session-file
  creation (materializes at the first user message), the shared
  poison-lock helper, and `list()` reading a missing directory as
  empty). The rig crates' residue is unchanged, the tabit crates carry
  their own itemization below. `crates/tabit-config` sits at 100%
  branch coverage with no fully unexecuted line. The tabit crates:
  tabit-session 93.6% (session.rs) / 84.9% (store.rs), tabit-tools
  90.5%; their residue is almost entirely error arms (see "Justified
  residue" item 9).

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
9. **tabit-crate error arms** (tabit-session / tabit-tools / CLI):
   - Filesystem `.map_err` arms that need faults a test cannot portably
     create: append/write to an unlinked-open handle *succeeds* on Windows
     (verified empirically — the persist-failure test documents it), and
     disk-full / uuid-collision / `create_new` races cannot be staged.
   - Poisoned-`Mutex` arms in `recorder.rs` — reachable only after a panic
     inside the lock, which the workspace lint policy forbids in shipped
     code. `rewind_to`'s persist-failure arm shares `record()`'s
     capture-and-surface path (`note_error`, covered for `record` by the
     persist-failure test); staging a write fault that spares the load
     read but breaks the marker append is not portable.
   - Placeholder arms documented as unreachable by construction:
     `UnreachableModel` (every assembled session rebuilds its real agent
     immediately; its bodies error loudly as internal-invariant guards) and
     `user_placeholder` (guarded by an `is_empty` check; `OneOrMany` has no
     empty constructor).
   - Platform-absent arms in `tabit-tools`: the PowerShell interpreter
     fallback (this machine has Git Bash), interpreter spawn failure, the
     `try_wait` OS-error arm, and the abnormal-signal exit description.
   - Engine-driven event arms in `stream_item_event`: `TurnRetried` (needs
     a hook that rejects a turn — rig-agent's hook tests cover that engine
     path), `NativeItem` from `Unknown` stream items (no mock builder
     emits them), and the `FinalResponse`/`StreamUserItem` catch-arms the
     caller handles directly.
   - `build_model`'s `build_error` arm: provider client constructors cannot
     fail once config validation has checked URL scheme and key presence.
   - The repair-path `Persist` return in `reload_context` — the same
     first-error check `prompt()` performs post-run (covered there); the
     repair variant needs a dangling log whose writer fails exactly at
     repair time.
   - Rewind defensive arms, provably dead by load invariants, kept as loud
     `Corrupt` errors rather than deleted because the lint policy denies
     `expect` in shipped code: `chain_from`'s missing-entry walk arm
     (parent validation at parse time resolves every link) and
     `Session::rewind`'s boundary-parent-off-chain arm (the boundary comes
     from the chain, so its parent is an ancestor on it).
   - Actor defensive arms in `endpoint.rs`: the poison-recovery `lock`
     (same policy as `recorder.rs`); the commands-channel `None` arm
     (every current caller closes via the shutdown token while the
     handle itself holds a sender, so the all-senders-dropped path
     cannot be observed through the public API); the
     leftover-message re-pump branch after a pump returns (the pump's
     own loop already drains the mailbox, so the branch is belt-and-
     braces for a message landing in the exact return window); and
     `start_pump`'s `Option::take` guard (both call sites check
     `session.is_some()` first).
   - `json.rs`'s serialize-failure `continue` in `write_loop`:
     `ServerFrame` contains only strings/numbers/serde-derived types,
     so `serde_json::to_string` cannot fail (no non-string map keys, no
     unserializable floats); the arm is the minimal handling of an
     infallible `Result`. The broken-transport edges around it
     (panicking reader, erroring reader, failing writer) are exercised
     by `broken_transport_edges_fail_or_end_cleanly`.
   - Deferred-creation fault arms in `store.rs`: `ensure_open`'s
     open/serialize/writeln/flush `.map_err` arms (the `create_dir_all`
     arm IS exercised via the blocked-path test; the rest are the same
     unstaged-fault class as item 9's first bullet), and
     `append_entry`'s let-else guard (provably dead — `append` runs
     `ensure_open` first; loud rather than silently dropping records).
   - The post-run persist check in `run_one` (`first_error` →
     `RunFailed`): platform-contingent — on Windows an unlinked writer
     keeps succeeding, so the same test exercises the reload-error arm
     instead; the failing-run outcome is asserted either way by
     `persistence_failure_fails_the_run_loudly`.

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
