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
- **Reachable via internal error** (our own invariant broke) → **panic,
  fail hard and loud** (AGENTS.md's error doctrine): the process dies
  rather than running in a broken state. The engine refactor converts
  the existing `internal invariant violated:`-prefixed error arms to
  deliberate panics.
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
- Current state: **94.27% lines / 94.99% regions** (3,320 of 57,978
  lines; re-measured after the GUI walking skeleton — the drop from
  95.56/96.15 is the `tabit-gui` crate's justified gaps, below; the
  rest of the workspace is unchanged. Before that: re-measured after
  the `tabit-protocol` extraction — the
  frontend vocabulary moved out of tabit-session into its own
  engine-free crate, gaining a protocol-owned five-field `Usage` (the
  engine's richer fields convert at the wire) and 100% coverage of its
  executable lines (`events.rs`/`usage.rs` are pure declarations with
  no instrumented lines; their serde shapes are pinned by round-trip
  tests). Before that: re-measured after the engine refactor — `AgentRun` rebuilt as
  the ENGINE.md machine: 8.5K lines removed with the invalid-tool-call
  hook machinery, the prompt/context split, and the three scattered
  drains; `run/tests.rs` rewritten as the machine's contract suite;
  the invalid-recovery test suites deleted with their contract. The
  new machine's residue is protocol-violation arms, the same class as
  the old ones). Prior passes: flag-21 (typed `MalformedToolCall`
  defect signal across all three providers, turn-discard retry) and
  the protocol rulings pass (drain-all mailbox, failures as events,
  deferred session-file creation, `stream_chat` taking a full
  conversation). The rig crates' residue is unchanged, the tabit
  crates carry their own itemization below. `crates/tabit-config` sits
  at 100% branch coverage with no fully unexecuted line. The tabit
  crates: tabit-session 93.6% (session.rs) / 84.9% (store.rs),
  tabit-tools 90.5%; their residue is almost entirely error arms (see
  "Justified residue" item 9).

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
   `AgentRun::discard_turn`'s called-in-wrong-state arm joins this class —
   the driver only calls it with a turn in flight (`AwaitingModel`), the
   same protocol-violation shape as its sibling `retry_model_turn`.
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
   contextual-tool-without-runtime-dep arm in `rig-derive`). The crash
   contract (`tests/crash.rs`) joins this class: the panic hook and
   injection branch run in the spawned `tabit` child, asserted there by
   exit code 101, the stderr report, and empty stdout.
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
   - The resident worker's `event_tx.closed()` arm in `endpoint.rs`
     (frontend dropped its entire handle): observable only by dropping
     the receiver the test itself reads events from — nothing remains
     to assert through. Its sibling termination paths (explicit
     `close_commands`, including the queued-work-honoring re-check)
     are exercised by every `drain()`-based test.
   - `tabit-protocol`'s `to_wire_line` expect: `ServerFrame` and
     `SessionCommand` contain only strings/numbers/serde-derived
     types, so `serde_json::to_string` cannot fail (no non-string map
     keys, no unserializable floats); the round-trip tests hold the
     invariant. This one policy replaced the per-site fallbacks (the
     old `write_loop` skip, `send_message`'s empty line). The
     broken-transport edges around the caller
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

## tabit-gui (walking skeleton)

- `reducer.rs` — **covered** (91.4% lines; the residue is partial
  field combinations in `add` and `Facts` paths).
- `app.rs`, `theme.rs`, `main.rs` — **justified**: egui rendering,
  the eframe event loop, and window construction have no offline
  unit-test surface; verification is the owner's end-to-end pass (the
  GUI exists precisely because tests cannot verify UX — ROADMAP item
  7's rationale for building it before the v2 backend).
- `backend.rs` — **justified**: spawns a real `tabit --json` child
  and owns OS pipes/threads; exercisable only in a live session.
  The protocol parse it performs is covered by tabit-protocol's
  round-trip tests; the launch path (`tabit` launcher detach-spawn)
  likewise needs a desktop session.

## Interaction (permission + ask_user, 2026-08; remediation pass 2026-08)

- `tabit-session/src/interaction.rs` — **covered**: routing, total
  no-op, retraction, weak-sender dismissal, session memory, prompt
  shape (unit) plus actor-level end-to-end tests (allow, deny,
  always-allow, ask_user round-trip, abort-with-card-open, two
  concurrent cards answered in reverse order, frontend death incl. the
  durable abort-time synthesized tail).
- `tabit-session/src/permission.rs` — **covered directly** since the
  remediation pass: the policy is extracted as `gate()` and its whole
  decision table is unit-pinned (non-asked tools cardless, no-frontend
  fail-closed naming why, Allow runs, Always allow runs + remembered
  cardless, Deny delivers its reason verbatim, terminal-retracted ask
  fails closed) — the actor tests were event-presence-only and could
  not distinguish allow from deny.
- `tabit-tools ask_user` — **covered directly** since the remediation
  pass: all four outcomes against a scripted `UserInteraction` double
  (text verbatim, option named, dismissal in-band, no-frontend error).
  The actor-level round-trip remains as the seam test.
- `tabit/bin print-mode stdin reader` (`main.rs` watcher thread,
  card rendering incl. the FIFO card queue) — **JUSTIFIED**: owns real
  stdin; `parse_answer` is unit-covered (numbered buttons + reason,
  free text, fail-closed empties).
- json bridge `InteractionResponse` passthrough — rides the generic
  `ClientFrame::Command => link.send(command)` arm, unchanged by this
  feature; the command itself is round-trip covered in tabit-protocol
  and link-routed covered in tabit-session.
- `app.rs` cards panel / `answer()` — **justified** under the existing
  GUI skeleton policy above (view code; owner e2e pass); the reducer
  state behind it is unit-covered (cards, terminals, the
  `turn_truncated` notice, abort clearing pending steers).

## v2 slice 1 — ids, brackets, tool status, error carrier (2026-08)

- Turn announcement (rig-agent `drive_agent`, `TurnStarted`/
  `TurnCommitted` items, hook-context id) — **covered** by the
  streaming tests: announcement-before-content, ids reach
  `ModelTurnFinished` hooks, retry announces a fresh id and only the
  accepted attempt commits.
- Session fold stamps + id continuity (`announced_turn_ids_are_the_
  log_entry_ids`) — **covered** end-to-end: announced ids are the
  reloaded log's `assistant_message` entry ids (UUIDv7-shaped, proving
  the injected mint is in force), events stamped, `tool_result`
  entry ids match their entries, truncation names its turn.
- Mailbox born-early ids — **covered**: batch ids are the log's user
  entry ids; the steer round trip (queued acknowledgment → resolved by
  `user_message` with the same id → the log entry keeps it) and the
  abort flush (discard after the terminal, ids matching the
  acknowledgment) are endpoint-tested. The drain-parked-id FIFO's
  empty arm is the sanctioned crash (engine contract).
- `tool_result` content+status — **covered** (`tool_roundtrip` success
  shape, `failing_tool_results_carry_status_and_content` for
  `failed { exit_code }` + faithful content; bash's structured code
  pinned in tabit-tools; disposition mapping unit-tested in rig-core).
- Startup degradations — **covered** (`default_selection_*` note
  assertions; endpoint first-frame; bridge ack-then-note ordering).
  The `push`/`pump` liveness race arms are documented both-orders-safe
  (ledger stays closed either way) — **JUSTIFIED** not to test the
  race itself: the invariant is order-independence, not one ordering.
