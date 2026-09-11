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
- Current state: **93.61% lines / 92.77% regions** (2,643 of 41,334
  lines; re-measured after the delta-tokens compaction refactor and
  the subagent pairing round — the deferred item below, closed. The
  round's finds: the pass cap had never fired (a 410×2,000-token
  history on the 64K window pins `Oversized { passes: 16 }` — and
  taught the test that only the final pass's output rides
  `tokens_after`, earlier summaries fold into the prefixes that
  replaced them); the unrepairable in-stream summarizer failure
  (`Outcome::Failed`, the sibling of the wall's shorten-retry) is now
  pinned; and the standing subagent-override deferral closed — the
  e2e rides `model`/`cwd`/`tools` through the real spawn while its
  sibling exercises the inheritance defaults, leaving `subagent.rs`
  clean. Two dead arms deleted rather than documented: `Tree::node`
  (zero callers) and `HostLoop::worker`'s unknown-session error (the
  Nth sibling of the inline routing failure; its one caller reads the
  table directly now). The residue is the recorded classes with
  shifted line numbers — itemized in the round's section below.
  Before that: **93.56% lines / 92.71% regions** (2,658 of 41,246
  lines; re-measured after the subagent + compaction arcs — the
  deferred item below, closed. The base grew ~2,000 lines (the
  subprocess substrate, the json bridge, the compaction box) while
  the missed count fell by 113. The round filled ~30 gaps and found
  two design facts: the overflow-intercept e2e exposed that a
  measurement taken before a compaction is tainted (it counted the
  prefix the summary replaced) — the walk-back now honors a
  compaction horizon by entry id; and the multi-pass test exposed
  that cut selection summed the raw array instead of the folded
  context (prefix sums now restart at a compaction node). The
  round's original "the guard is the normal multi-pass exit" reading
  was an artifact of testing below the 64K support envelope and is
  corrected in ROADMAP item 6 — on supported windows multi-pass
  strictly shrinks and exits `Compacted`; the guard is exceptional.
  Per-file, the arc modules:
  `compaction/mod.rs` 95.9%, `compaction_doors.rs` 93.9% (the
  overflow intercept e2e'd, both outcomes), `turn.rs` 98.4%,
  `overflow.rs` 100%, `tree.rs` 100%, `subagent.rs` 93.3%,
  `subprocess.rs` 79.7% (the process-fault family, classified below).
  Residue classified in the dedicated section below. Before that:
  **93.59% lines / 92.24% regions** (2,518 of 39,269
  lines; re-measured after the frontend-protocol v4 + no-orphan-gate
  round — the subagent arc after it (SessionCwd, ephemeral
  sessions, the SpawnContext split and its suites) is not yet
  re-measured; its code arrived tested per the maintenance rule,
  the next round folds it in — `tool_result.details` with its edit/shell producers, the
  select widgets, `session_opened`, and the born-bit gate. The
  review filled thirteen gaps and deleted two provably-dead arms;
  see the dedicated section below. Before that: **93.26% lines /
  92.08% regions** (2,629 of 39,012
  lines; re-measured after the tool round — the RAG mass, telemetry,
  and the blocking surface deletions shrank the base ~22k lines while
  the tool round added read/write/edit/bash with their suites. The
  ratio holds: deleted code carried roughly its share of covered
  lines. New-code residue classified in "Coding tools (2026-09)"
  below; five gaps found there were filled in the same round.
  Before that: **93.36% lines / 94.08% regions** (4,037 of 60,818
  lines; re-measured after the loop refactor — the turn state machine
  became the ENGINE.md coroutine over a handler-owned ContextManager,
  the recorder dissolved into tabit-log, hooks went observe-only, and
  the session folds at its item arms. The ratio holds while the base
  shrinks: the refactor deleted ~2k lines of machine/recorder and
  added the tabit-log crate, whose suite covers the manager/writer/
  tree folds.
  Before that: **93.37% lines / 94.11% regions** (4,083 of 61,546
  lines; re-measured after structured-output enforcement left the
  engine (the deletion section below); the drop is bookkeeping --
  deleted code carried more covered lines than it dragged in.
  Before that: **93.45% lines / 94.21% regions** (4,184 of 63,846
  lines; re-measured after the durable-layer sweep -- atomic
  roundtrips through the one commit door, the one-pass parser, and
  the two-phase turn acceptance; see the dedicated section below.
  Before that: **93.38% lines / 94.14% regions** (4,202 of 63,493
  lines; re-measured after the pre-GUI-redesign review pass, which
  covered the write-behind and model-command rounds and fixed what
  the review found — see the dedicated section below:
  `recorder.rs` at 98.61% (was 82.22% — the lost-record arms, the
  rewind arm, and the resident-view merge gained direct tests; the
  residue is the dead-channel no-ops), `store.rs` at 85.21% (the
  residue is the unstaged-fault class plus the barrier's mid-drain
  fault arms). Before that: 93.41% lines / 94.14% regions
  re-measured after the agent-cache refactor — selection is the
  truth, the agent a stamped cache derived at run open:
  `Session::ensure_agent` (both arms), the module-level `build_agent`,
  and the run-open construction-failure arm (`run_failed` after the
  accepted message) all arrive covered, session.rs at 96.71% lines, and
  the placeholder model's never-callable arms left the ledger entirely;
  before that: 93.53% lines / 94.24% regions re-measured after v3
  stage 2 — checkout: the pause-point
  machinery, the mailbox watermark clear, and the full-re-render pass
  arrive fully covered per function, endpoint.rs at 97.61% lines;
  before that: re-measured after the v3 multi-session host — the endpoint
  worker became `SessionHost` routing to per-session workers at 97.4%
  lines, protocol v3 (session-addressed commands,
  `new_session`/`open_session`, the startup catalog, `"main"`
  retired for stream = session id), and the GUI grew the switcher and
  command-driven new-session over a single backend. Before that:
  re-measured after the GUI walking skeleton — the drop from
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

## Coding tools (2026-09)

The read/write/edit/bash round (`tabit-tools`) was measured with the
tool suites in place; six gaps found in the review were filled in the
same round (UTF-32 BOM naming, read's byte-truncation reason, big
directory listings, edit's empty-edits and empty-file arms, bash's
one-huge-line notice). Remaining residue, classified:

**Justified — environmental, not exercisable offline:**

- `shell.rs` — the *absence* branches: `Shell::Powershell` resolution
  (the no-Git-Bash machine), the broken-candidate probe fallthrough,
  the `where.exe`-spawn failure arm. The dev machine (and CI) has a
  positively identified Git Bash; the absence paths need a controlled
  PATH/registry sandbox or a test seam the design deliberately does
  not have (resolution is process-global by OnceLock, once).
- `lib.rs` (bash core) — the spawn-failure arm (`cannot start`), the
  cancellation-mid-run arm (races a fast scheduler; the pre-cancelled
  door IS tested), the wait-error arm, pipe-missing arms (a
  process-wrap internal failure), the spill-write failure arm. All
  need filesystem/process fault injection the suite does not have.
- `file_io.rs` — the fault arms: `create_dir_all` failure, parent
  metadata error, plain-write failure, temp-stage/write/persist
  failures. Same fault-injection class.
- `lib.rs` (`ask_user`) — the malformed-answer arm (`the user's
  answer could not be read`): the scripted interaction double returns
  canned outcomes; a malformed payload is a frontend defect, not a
  reachable external error in practice. Dies with ask_user's
  pre-shipping removal (owner ruling, ROADMAP item 4).

**Deferred — live-verification candidates, not CI:**

- `lib.rs` (`dynamic` / `dynamic_contextual`) — the `map_error` /
  `into_tool_output` arms inside the erasure closures: exercised end
  to end only through a real model driving a session (the direct
  ToolSet tests cover the success path and the invalid-args arm).
- `main.rs` (tabit) stays the known outlier (48.95%): CLI assembly,
  process-spawn, and live-provider glue — the deferred class since
  the CLI's first measurement, unchanged by this round.

## Frontend protocol v4 + the no-orphan gate (2026-09)

The `tool_result.details` / select-widgets / `session_opened` /
no-orphan-gate round (tabit-protocol, tabit-session, tabit-log,
tabit-tools, the tabit binary), measured with its suites in place.
Filled in this review:

- `wire.rs` (session): `user_text`'s non-user and non-text-part arms
  (a direct unit test).
- `protocol.rs`: the `replay` flag's skip-when-false serialization
  (round-trip) — the file is now fully covered.
- `truncate.rs`: the mid-character head cut (3-byte chars land the
  half budget inside a codepoint).
- `diff.rs`: distant-hunk line-counter advance, a pure deletion's
  `first_changed_line`, and the no-change base case. The gap-advance
  loop's Delete/Insert arms were **provably dead** — every changed
  index lies inside some hunk's range, so a gap between hunks is
  all-equal — deleted; the loop is arithmetic now, the proof in the
  deleting commit.
- `writer.rs`: `append_to` on a missing file (typed error naming the
  path), and the stray-partial-file recovery — `create_new` refuses
  the orphan, removes it, keeps the lines queued, the retry writes
  them whole. The `degraded()` getter was dead (nothing reads the
  flag; the transition channel carries the reports) — deleted.
- `NullBuffer`'s trait methods (takes everything, stores nothing).
- `read`'s UTF-16 BE naming.
- The permission gate's malformed-answer arm (a wrong-typed field
  fails the parse; the gate fails closed with a trace).
- Replay's empty-reasoning-block skip.
- Endpoint: a continue on an empty conversation is a no-op (no
  phantom run), `open_session` emits its model notes ahead of the
  replay, and a replay parked at close is served at the shutdown
  beat ahead of wind-down.

Remaining residue, classified:

**Justified — environmental or defensive, not exercisable offline:**

- `writer.rs` — the write-fault family: `write_all` failure (+
  `truncate_to`, its rollback helper), `materialize`'s remaining open
  faults (the stageable one — the stray file — is now a test),
  `serialize`/`rollback_to_mark` (reachable only from an
  unconstructible serde failure; the enqueue panic is the sanctioned
  crash), the drain-without-file internal-invariant error.
- `endpoint.rs` — the two `unreachable!` routing arms (sanctioned
  crashes), the checkout execution-time failure (receive-time
  verification covers the stageable causes; what remains is the
  environmental class), the `event_tx.closed()` arm (pre-existing
  classification, unchanged).
- `run.rs` — the abort-record flush-failure warn (the blocked-store
  fault family) and the engine-driven stream-item passthrough arms
  (the pre-existing family; `ToolExecutionCommitted` and the turn
  brackets now enumerated).
- `replay.rs` — the non-assistant recorded-message arm (corruption
  the parser rejects earlier) and the `AssistantContent` catch arm
  (no producer emits other variants).
- `json.rs` — `forward_events`' `None`-stream arm (`serve` calls
  `take_events` exactly once; `None` would be API misuse), plus the
  usual test-module assertion arms and test-double trait methods
  (classes 1/3).
- `shell.rs` / `lib.rs` (tools) — the platform-absence and fault arms
  carried from the coding-tools classification, unchanged by the cap
  split and the details production; ask_user's malformed-answer arm
  still dies with the tool's pre-shipping removal.
- `permission.rs` / `interaction.rs` — test-side assertion arms
  (class 1).

**Deferred — owned by the GUI worktree:**

- `tabit-gui` reducer arms (the crash-mid-run exit tail, bare
  `TurnStarted`/`TurnCommitted` grouping, the `CompletionCall`
  no-op, the malformed select-card notice): fillable through reducer
  tests, but the reducer is the parallel GUI effort's active surface
  (ROADMAP item 7) — classified with the walking-skeleton section
  until that lands.

## Subagents + compaction, the deferred re-measurement (2026-09)

The two arcs measured with their suites in place. **Filled** (the
gaps the measurement found):

- **The overflow intercept had never run** — the round's biggest
  find. Two e2e tests now drive it end to end over a windowless
  config (only the wall's report can teach the window): the
  repairable path (failed run → the bracket → the continue intent →
  the retry answers over the compacted context, file holds the
  insertion) and the unrepairable one (no feasible cut → no bracket,
  `run_failed` stands). `MockError::http` was added to the test
  doubles for the wire shape.
- **The taint bug** (found by that e2e): a measurement taken before
  a compaction counted the prefix the summary replaced — the
  walk-back trusted it, condition B stayed fired, the cannot-shrink
  guard failed the whole invocation, and the intercept reported
  failure after a successful pass. The walk now honors a compaction
  horizon (compared by entry id — UUIDv7 millisecond order; the
  RFC3339 stamps are second-precision and collide in fast
  exchanges); the unit pins both the taint and the younger-
  measurement recovery.
- **The box's policy arms**: manual door below the tail floor
  (benign `NothingToCompact` — nothing worth folding is not a
  failure), the auto doors' infeasible skip, the
  cannot-shrink guard (a huge late entry no feasible cut can move
  into the prefix — the exceptional loud stop, with what landed
  standing; NOT the normal multi-pass exit, which strictly shrinks
  and exits `Compacted` — the correction lives in ROADMAP item 6),
  the below-envelope window skip (the declared 64K support line),
  the designed multi-pass (a 100k history on a 70k window →
  `Compacted {2}` with strict shrink — the test that exposed the
  raw-array prefix sums), the length-cap
  shorten-retry and its empty-prefix failure, the in-stream overflow
  rejection retry (window learned), the empty-prefix rejection, the
  pre-request leaf driven directly (condition B through its own
  emission path), `select_cut`'s empty branch, and the
  compaction-as-boundary validity.
- **`turn.rs`**: content-after-final fault, mid-stream provider
  error, tool-call delta forwarding (the mock grammar needs the
  complete call event after its deltas — the deltas alone are the
  live view only).
- **`overflow.rs` to 100%**: the non-rejection status gate, the
  status-less envelope, the no-vocabulary body.
- **`tree.rs` to 100%**: `load_append`'s duplicate id,
  `insert_compaction`'s parentless and duplicate-id faults.
- **`context_manager`**: the seeded dangling-batch panic, the stale-
  cut and root-cut commit panics, `fold`'s tool-carrying refusal,
  the Debug impl.
- **`fold.rs`**: the empty path; the trailing-run `continue` arm was
  **provably dead** (the `rposition` split guarantees an all-result
  tail) — converted to a sanctioned crash rather than documented.
- **`subagent`**: the missing-capability refusal, all three
  `summary_result` outcomes (aborted, failed-with-reason,
  failed-unknown, empty-answer report), and the spawn failure naming
  the executable.

Remaining residue, classified:

**Justified:**

- `compaction/mod.rs` — the after-passes no-feasible-cut exit
  (197-200): unreachable through the loop's own guarantees (the
  post-pass branch re-offers the compaction boundary as feasible
  while the floor holds; non-shrink exits at the guard first) — the
  loop's total-function defense. The request-build `Err` arm
  (480-486): a serialization failure the mock cannot produce; the
  classification is shared with the covered in-stream arm.
  `assistant_text`'s non-text catch (559): no committed summary turn
  carries other content. The pre-request leaf's emit-through-notice
  body (634): needs a live tap on a firing pre-request door — the
  identical `NoticeSink::emit` is covered through the beat doors.
- `compaction_doors.rs` — `run_box`'s `ensure_agent` failure: the
  registry `build_error` class (client constructors cannot fail once
  config validation passed).
- `turn.rs` — the assembler-ingest error arm (132) and the
  trailing-delta deferral (175): protocol-violation shapes the mock
  grammar does not synthesize; `classify_error` itself is covered on
  both siblings. Test-internal assertion arms (the existing class).
- `tabit-log` — `fold.rs`'s new sanctioned crash (unreachable by the
  split); `context_manager`'s checkout Corrupt belts (338/349, the
  existing provably-dead-by-load class) and the insertion-refused
  belt behind commit's own validated construction (410).
- `notice.rs` `forward`'s dead-channel no-op, `replay.rs`'s
  non-assistant and content-catch arms, `routing.rs`'s
  route-outliving-child defense — the established frontend-gone /
  no-producer / racing-frame classes.
- `subprocess.rs`'s process-fault family: the rejected/hung
  handshake (needs a protocol-mismatched or unresponsive peer), the
  reaper's force-kill (a child ignoring the courtesy abort), the
  crash report and death detection (a child dying mid-drive), and
  the stderr-ring edges — none stageable without a fault-injection
  child the suite does not have.

**Deferred:**

- `subagent.rs`'s override plumbing (the tool's `model`/`cwd`/
  `tools` args, 180-203) and `subprocess.rs`'s `--tools`/`--session`
  builder args: exercised only through a real subprocess spawn with
  overrides — the next subprocess e2e round passes them.
- `tabit/src/json.rs`'s child-role arms (the `--parent` resolve
  paths, the open-session lookup, 1221-1261): code that runs inside
  the spawned child binary — the class-6 subprocess-execution
  attribution gap; the parent-side bridge is covered by the wire
  suite.

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
     code. `rewind_to`'s failure leg rides the same
     record-then-`observe` path as `record`'s (both covered by the
     blocked-store bootstrap test: the lost-record contract, one
     degrade announcement, `pending: 0`); staging a write fault that
     spares the load read but breaks the marker append is not portable.
   - Placeholder arms documented as unreachable by construction:
     `user_placeholder` (guarded by an `is_empty` check; `OneOrMany` has no
     empty constructor). (`UnreachableModel` was deleted with the
     agent-cache refactor — the placeholder it satisfied no longer exists.)
   - Platform-absent arms in `tabit-tools`: the PowerShell interpreter
     fallback (this machine has Git Bash), interpreter spawn failure, the
     `try_wait` OS-error arm, and the abnormal-signal exit description.
   - Engine-driven event arms in `stream_item_event`: `TurnRetried` (the
     engine emits it on the malformed-tool-args defect path — rig-agent's
     loop tests cover that engine path), `NativeItem` from `Unknown` stream
     items (no mock builder emits them), and the `FinalResponse`/
     `StreamUserItem` catch-arms the caller handles directly.
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
   - The post-run re-derivation check in `run_one` (`reload_context`
     error → trailing `RunFailed`): covered via the reload-error arm
     (`persistence_failure_fails_the_run_loudly`); a persist-*write*
     failure is no longer a run failure at all under flag 8 — the
     terminal carries `run_finished.durable` and the degrade rides the
     notice channel.

## Deferred (explicit)

- `agent/runner.rs` — the blocking driver's mid-run `DriveItem` path: no
  current blocking scenario emits a `DriveItem` mid-run; a test needs a
  blocking-mode scenario that yields one to the fold.
- `client/mod.rs` `optional_env_var` `VarError::NotUnicode` branch: a
  non-Unicode env var cannot be set portably from a Rust test process;
  would need a child-process harness.
- Doctests are outside the measurement (see Methodology); they run and are
  gated in CI but do not fold into these numbers.
- The subprocess child-role arms and the `--session` builder arg —
  see the round section above (child-role execution is the class-6
  attribution gap; `--session` needs a persisted-child spawn shape
  the built-in tool never makes).

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

- `reducer.rs` — **covered** (92.2% lines; the residue is partial
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
  the hook context, retry announces a fresh id and only the
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

## v2 slice 2 — replay (2026-08)

- `replay.rs` projection — **covered** by unit tests over synthesized
  chains: exact bracketed sequence (ids, whole texts, usage, commits),
  per-block reasoning + single accumulated text delta with in-content
  ordering, bookkeeping exclusion, failed-status structure.
- Session live-vs-replay continuity — **covered** end-to-end: replayed
  turn/user/result ids are the live-announced ids, whole texts match
  the accumulated live deltas.
- Worker pass + bridge — **covered**: the endpoint pass streams
  bracketed events and the stream continues; the wire test pins
  ack → pass → live-run ordering, whole-text history, and the
  no-replay default. The replay-before-work arm priority is exercised
  by the bridge test's back-to-back initialize+message.

## v3 — the multi-session host (2026-08)

- `endpoint.rs` host — **covered** at 97.4% lines: catalog at startup
  (presence, lazy loading, the boot's absence before materialization),
  two-session routing by id (each answer on its own stamp), new/open
  lifecycle (creation frame ordering, notes on the new stream,
  idempotent re-replay, unknown-open failure), targeted-command
  errors stamped with the targeted id, catalog failure as the carrier
  with no announcement, and the close-not-a-barrier drain (the
  dedicated worker token's race — a worker observing `cancelled`
  before the host routed the pre-close queue — was caught by exactly
  these tests). Residue: the host loop's command-channel `None` arm
  (every sender dropped while the receiver lives — the in-process
  handle-drop door; the stdio edge, the real consumer, always closes
  through `close_commands`, which IS covered) and the worker's
  `event_tx.closed()` arm below.
- The worker's `event_tx.closed()` arm (frontend dropped its entire
  handle): unchanged from the pre-v3 justification — observable only
  by dropping the receiver the test itself reads events from.
- `json.rs` bridge on v3 — **covered**: the ack-aware harness (tests
  learn the boot id from the ack, the honest client shape the
  always-explicit ruling forces), sessions_available placement
  (between ack and replay), a two-session wire round trip
  (`new_session` → `session_created` → per-stamp messages →
  `open_session` re-replay), unknown-session errors on the wire, and
  `abort_all` at EOF (every EOF-ending test).
- GUI reducer multi-session — **covered**: catalog population,
  background liveness (running dot, attention on background errors,
  never rendering into the transcript), optimistic switch + replay
  rebuild, `session_created` switch with facts, the bracket reset.
  After the owner's live pass: connection-level routing is pinned with
  the realistic stamp (creation frames arrive on the NEW session's
  stream — the original test stamped the boot stream, a frame the
  backend never produces, and masked the routing bug), creation
  mid-run switches immediately with the abandoned run's dot surviving,
  and run liveness mirrors onto rows even when the run was viewed
  live.
- Backend non-blocking pin — **covered**
  (`new_session_is_never_blocked_by_a_running_session`): creation,
  messaging, and completion of a second session while the boot run is
  provably mid-tool — the ruling that session lifecycle never waits on
  another session.
- Review-round remediation (2026-08, three clean-context reviewers) —
  **covered**: multi-session frontend death (both runs abort
  durably, both logs record it), the running-session replay wait (no
  bracket interleaves the in-flight run; the pass answers after the
  terminal), the burst-EOF death door (no unattended completion), the
  idle-abort discard made visible, post-abort message survival, the
  replay pass never marking liveness (view flag and row, background
  passes included), per-session cards surviving switches and dying
  with their own run's terminal, `run_failed` as exactly-one-terminal
  and last, the catalog's newest-first order, attention-flag
  lifecycle, and the `replay: false` wire skip.
- `main.rs` wiring closures (`host_wiring`, create/open) — the create
  path is exercised through the tabit crate's bridge tests only when
  `new_session` is driven; the production closures' failure surfaces
  are the endpoint-tested lifecycle arms. **Justified**: the closures
  are thin re-assemblies of `assemble`, whose every path (config,
  resume, degradation) is covered in `main.rs`'s own suite.

## v4 — interaction generalization + backend-level events (2026-08)

- The generic shapes are round-trip and shape-pinned in
  tabit-protocol: `interaction_request { id, ui_type, payload }` /
  `interaction_response { session, id, payload }`, the templates
  module (`native:confirm`/`native:ask` payloads, answers with
  absent-when-None fields), the optional `stream` (absent =
  backend-level), and every stamped-frame test now holds `Some(..)`.
- The hub is payload-blind and directly unit-covered (routing, the
  verbatim ui_type/payload passthrough with the stream stamp, the
  total no-op, weak-sender dismissal, terminal retraction — all on
  raw payloads). The permission gate's decision table runs unchanged
  over the confirm template (payload answers); `ask_user` runs over
  the ask template against the scripted capability.
- Actor-level: the allow/deny/always-allow/abort/death suite answers
  with payloads; the wire suite pins the v4 handshake, unstamped
  routing errors (no stream field, message names the id), and the
  backend-level catalog/creation frames.
- GUI: connection-level fold for unstamped frames (catalog,
  creation, backend errors), template-typed cards, unknown-ui_type
  notices (`unknown_and_malformed_interaction_widgets_surface_as_
  notices_not_cards` — an `ext:*` widget and a malformed native
  payload both surface as notices, no card opens); the fixtures build
  the honest unstamped shapes (the old stamped-creation fixtures were
  the fiction shape the round deletes).

## Hook surface, shipped (2026-08, round 2)

- The registration record + priority law is engine-pinned
  (`closure_records_order_by_priority_and_deny_is_absorbing`:
  stable sort by priority with registration-order tiebreak, Skip
  absorbing—the auditor-before-denier consequence asserted
  directly; `turn_finished_closures_observe_in_priority_order` pins
  the same law at the observation point). The permission actor suite
  (allow/deny/always-allow/abort/death) now runs through the value
  seam end to end—which is also the unified-context pin: the gate's
  closure asks through `ctx.interaction()`, the same capability map
  tools read. The run-context snapshot costs one ToolContext clone per
  run (`agent_dispatch_snapshot_clones_once...` counts it). The stack
  keeps the registration ids (its `Debug` lists them — attribution
  and replace-by-id consume them when consumers appear).
- `gate.rs` (ToolGate/GateHook) is deleted; the factory seam became
  `SessionBuilder::hooks(HookStack)` — a plain value.

## Review remediation (2026-08, post-checkout/hook-surface rounds)

The two-subagent review round (architecture + test quality) and what
it changed:

- **Abort is one semantic at every door.** The death watcher and
  `abort_all` previously cancelled runs without clearing the checkout
  slot — a parked checkout could execute its durable rewind after the
  frontend died. `Worker::abort` (slot first, then the cancel — the
  order closes the woken-worker race) is now the only abort path;
  pinned at the command level by `an_abort_discards_a_pending_
  checkout` and, since the abort-composition round below, end to end
  by the mid-run checkout test.
- The permission actor suite now decides by content, not by event
  presence: allow asserts the body's own output (`ran: x`), deny
  asserts the in-band denial verbatim, ask_user asserts the answer
  reached the tool. The racing answer in the abort test is sent while
  the host still lives (it reaches the handler; no error frame).
- `RunAborted` and `InteractionRequested` joined the events
  round-trip; the frame-envelope test renamed to the sample it is
  (`sampled_event_variants_survive_the_frame_envelope`) and gained
  the two new variants.
- GUI reducer: catalog re-announcement preserves liveness/attention
  (`a_catalog_reannouncement_preserves_liveness_and_attention`),
  backend-only queued notices track by id through both resolutions,
  native items fold as their own rows, a signal death reports
  "killed".
- `replay_started.total` is asserted against the pass's actual
  length (the collapse test's re-render).
- Deferred, deliberately: the GUI's optimistic switch transient and
  `Facts` drift on switcher switches (ROADMAP item 7 — the per-session
  transcript redesign); flag 11's panic arm (amended in PROTOCOL.md —
  see the flag for the rationale).

## Tool-gate seam (2026-08, the permission-leak review) — superseded

`gate.rs` (ToolGate/ToolGateFactory) was deleted by the hook-surface
round above; this section survives as history. What still holds from
it: `interaction.rs` — the hub's own suite (routing, no-op,
retraction, weak-sender dismissal) is unchanged and green; the
permission vocabulary tests live in `permission.rs` with the
vocabulary, and the actor-level permission suite (allow, deny with
reason, always-allow memory, reverse-order cards, abort and frontend
death with a card open) runs through the value seam.

## v3 stage 2 — checkout (2026-08) — parking-era record, superseded in part

The parking-era semantics this section pinned are superseded by the
abort-composition round below: mid-run checkouts abort the run (the
"executes strictly after the run's own terminal" test became the
aborts-first test), the mailbox watermark trio was replaced by the
before/after rule at receive, and the `pump_with_pause` seam is
deleted. What still holds verbatim: the idle checkout, watermark-rule
(before/after), collapse, unknown-entry, and GUI-fold coverage, and
the command-path round's pins (discard at receive, abort drops a
pending checkout, pass-before-batch beat order, the strong-sender
discipline).

- `execute_checkout` / `emit_replay` / the mailbox watermark trio
  (`arrival_watermark`, `discard_up_to`, the arrival `seq`) and the
  `pump_with_pause` seam — **fully covered** (per-function lcov check;
  `endpoint.rs` rose to 97.61% lines overall). The behavior suite:
  idle checkout (rewind + `checked_out` + bracket + the next prompt
  provably branching, asserted against the log's chain), mid-run
  parking (the checkout provably under a slow-tool run, executing
  strictly after the run's own terminal — never aborting it — with a
  steered message's history rewound away and its ledger already
  closed by its `user_message`), the watermark rule (idle burst in
  wire order: the before-message comes back as exactly one
  `messages_discarded` pair and never becomes history; the
  after-message survives to run on the rewound chain after the pass),
  concurrently parked checkouts collapsing to the last (supersession:
  one `checked_out`, one pass, the superseded target never applies)
  plus a spaced checkout executing separately (the branch switch
  back), the unknown-entry error as a total no-op (kind `checkout`,
  nothing discarded, nothing moved, conversation continues), the
  abort-then-checkout composition at the pause point, the
  `pump_with_pause` yield directly at the session level (the survivor
  is submitted at the first run's terminal, so the queue is genuinely
  non-empty when the predicate decides — a pump that ignored the
  pause would run it inside the first pump), the wire round trip
  (`checked_out` with an explicit `"base_id":null`, the pass brackets,
  the post-checkout prompt branching), and the GUI fold (the pass
  rebuilds the transcript with entry ids intact — the next checkout
  target — while liveness stays settled; a failed checkout surfaces
  as a notice).
- The worker's shutdown-arm checkout drain (`close is not a barrier`
  for checkouts) — **covered directly** by
  `a_checkout_parked_at_the_close_executes_before_wind_down`: the
  checkout routes, the close lands, and the survivor executes before
  the stream ends (with the rewind asserted against the log).
- Command-path round (owner design review, ruled): the router only
  routes— resolve-and-forward into per-session handlers—and the
  behavior suite now pins the final semantics end to end: discard at
  receive (the parking test's steer dies at the checkout's handler,
  the notice beating the terminal, never becoming history), abort
  drops a pending checkout (run_aborted with no checked_out and no
  pass; the chain unmoved), the pass-before-batch beat order (a read
  requested after a message still answers ahead of it, excluding the
  not-yet-drained message), the collapse (slot replace; off-chain
  switch through the recorder's id set), the immediate
  unknown-entry error (validation lives in the handler at receive),
  and the abort-then-checkout composition (directional now: abort
  first works, checkout first dies with the abort). The strong-sender
  bug the round introduced and fixed in flight: the delivery surface
  initially held a strong event-channel sender, and since the host's
  routing table outlives the workers, the stream could never close—
  the handler now holds a weak sender per the channel-lifetime
  discipline (the hub, the mailbox notices), pinned by every
  EOF-reading test hanging until it was fixed.

## Abort composition (2026-08, owner ruling: checkout composes abort)

The root-cause pass over the death-door bug: abort's staging
machinery existed because the handler was once mute (no event
channel); the checkout round gave it one and made immediate emission
the pattern, but abort was never retrofitted. Deleted with this
round:

- **Staging is gone.** `Mailbox::clear_noticing` is the one
  clear-and-tell (abort site and checkout handler both), emitted
  through the same notice channel `message_queued` rides. The staged
  vec, `take_staged_discards`, `flush_staged_discards`, and the
  run-conclusion flush are deleted; the discard notice now precedes
  the abort terminal on the wire (pinned by
  `abort_mid_run_discards_the_queue_and_ends_the_run` and
  `a_post_abort_message_survives_and_runs`).
- **Checkout aborts first** (`a_checkout_during_a_run_aborts_it_
  then_rewinds_at_the_beat`: discard at receive → `run_aborted` →
  `checked_out` at the beat → the pass; the branch prompt then runs
  on the rewound chain). The polite-parking ruling is superseded
  (PROTOCOL.md stage 2); `pump_with_pause` and its session-level test
  are deleted — the pump returns on an aborted outcome, so the beat
  serves the rewind before any later batch. The pre-close survivor
  test carries over unchanged (`a_checkout_parked_at_the_close_
  executes_before_wind_down`).
- **The death×checkout window is now the abort transit** — a
  microscopic race between the beat serving the rewind and the death
  door dropping it, both outcomes log-consistent (documented in
  PROTOCOL.md's abort bullet). The dedicated death-door test from the
  remediation round dissolved with the parking window it pinned; the
  death door itself stays pinned by the card-open and multi-run
  death tests.
- Test hygiene: `temp_store` tags are unique per test again — a tag
  collision (`endpoint-checkout-abort` ×2, `rewind-root` ×2
  pre-existing) made two tests delete each other's session files
  mid-run, surfacing as a mysterious one-off hang and a
  file-not-found.

## Pre-GUI-redesign review pass (2026-08)

The quality + coverage round over the agent-cache, model-command,
write-behind, and prompt-caching commits. Re-measured: 93.38% lines /
94.14% regions. What the review found and did:

- **The barrier was not disk-atomic** (fixed). `commit_barrier` reused
  `append_entry`, which drains per entry — a mid-batch flush failure
  left earlier batch entries durable while the batch was un-committed
  in memory, and the reload leaf (the last entry in file order) would
  resurrect the discarded messages as history. The barrier now buffers
  the whole batch and flushes once (`buffer_entry` split out of
  `append_entry`), and the rollback truncates the file back to the
  pre-barrier offset (`rollback_barrier`). Pinned by
  `a_barrier_rollback_removes_batch_entries_that_reached_the_disk`
  (file length, offset, leaf, next-append cleanliness, reload chain).
- **The torn-write rollback was silently dead on Windows** (fixed,
  found by the test above). `set_len` on an append-mode handle fails
  with access-denied (Rust maps `append(true)` to `FILE_APPEND_DATA`
  only; the `write` flag is ignored in that position). Both rollback
  sites — `drain`'s torn-line truncation and the barrier's — now ride
  `truncate_to`'s separate write handle (the same pattern the repair
  path already used).
- **A provably dead arm was deleted, not documented**: the
  degrade-without-error message in `observe` (a drain that reports no
  error has emptied the outbox, so `pending > 0` always rides an
  error — the error alone decides the state now).
- **Filled**: the lost-record contract (blocked store: empty id, one
  degrade announcement, `pending: 0`, `is_clean` stays true — lost is
  not pending); the resident view's file+buffer merge under degrade
  (durable entry + buffered tail, chain recomputed through the
  buffer-time leaf); the unknown-model arm of `request_params`; the
  anthropic automatic-caching wire shape (`automatic_caching_pins_
  one_top_level_directive_on_the_wire` — top-level `ephemeral`/`1h`,
  no per-block markers; the flag is tabit's default policy, so the
  wire contract is pinned); the session threads its id as the
  factory cache key (assembly and model-switch derivations).
- **Newly justified residue**: `commit_barrier`'s mid-drain fault arm
  needs an open-but-failing write, which no portable test can stage
  (an unlinked handle keeps succeeding on Windows) — the rollback
  mechanics are unit-covered directly, and the bootstrap-failure leg
  (blocked store) is endpoint-covered. `recorder.rs`'s remaining three
  lines are `send_notice`'s dead-channel no-ops (weak-upgrade failure,
  absent stream) — the established frontend-gone pattern, a no-op by
  design. `store.rs`'s `ensure_open` fault arms and the
  serialize-failure arms join the existing unstaged-fault class.
- **Pre-existing, re-confirmed** (uncovered lines in the re-measure,
  unchanged classes): `Session::rewind`'s boundary-parent-off-chain
  Corrupt arm, the `stream_item_event` Unknown/catch arms,
  `wire_status`'s sanctioned panic, the endpoint's `unreachable!`
  routing guards and the parked-replay-at-close arm, `registry.rs`'s
  `build_error` arm (client constructors cannot fail post-validation),
  and the model-note emission at worker spawn.
- **Reported to the owner, not fixed (design call)**: a worker-task
  panic is swallowed — the stream ends without a terminal and the
  clean-exit flush is skipped, but the process survives, against the
  fail-loud doctrine. Closing it means a policy for propagating join
  errors (abort the process, or synthesize a terminal), a host-level
  flow change that should be ruled rather than slipped in.

## Format v3 — the resident-state refactor (2026-08)

The owner-ruled redesign (the "fix it properly" pass): the file splits
into conversation nodes (id + parent, the tree) and parentless side
records; the recorder owns the resident state (whole tree, head
pointer, incrementally folded context, record sequence); nothing
re-reads the file mid-session (the post-run reload, `load_resident`,
`effective_leaf`, `chain_from`, and the `EntryKind` bookkeeping
variants are all deleted); checkout is a head-pointer move; the drain
is one blob write (rollback = truncate to the durable offset or clear
— no loop, no offset bookkeeping in the barrier). Re-measured:
**93.42% lines / 94.16% regions** — above the pre-refactor state
despite the format break, with `recorder.rs` at 96.8%, `entry.rs`
100%, `session.rs` 98.1%.

- **Recorder suite (new file)**: tree/head/context growth at record
  time, checkout as a pointer move with re-projection and branching,
  the trailing-checkout reload, the one-pass load fold (tree, head,
  register, order-sensitive repairs), the head-invariant corruption
  check, and the barrier's validate-then-commit (a refused batch
  touches nothing resident).
- **Store suite rewritten to the sink contract**: the writer is
  structure-blind (records arrive pre-constructed); pinned are
  materialization order (header, first record), the single-write
  drain and clean-prefix accounting, the failed barrier popping the
  batch whole, and fault staging via the blocked-store trick (the
  old `file = None` sabotage is defeated by design now — materialize
  retries and would recreate the file).
- **Session suite**: contract updates pinned rather than papered
  over — a log deleted under a live run no longer fails the run
  (memory is authoritative; the loss is realized at the next open),
  the opening register rides the first barrier and an explicit
  switch supersedes it while pending, and resume's reconciliation
  record follows the synthesized repair in file order.
- **Justified residue, unchanged classes**: `store.rs` (85.0%) keeps
  the unstaged-fault arms (open/serialize/write failures a portable
  test cannot create), `recorder.rs` keeps the dead-channel no-ops
  and poison arms, `registry.rs` keeps `build_error` (client
  constructors cannot fail post-validation), and the
  `Projector::finish` whole-chain path is load/checkout-shaped (the
  incremental folds are the covered production path).

## The durable-layer sweep (2026-08): atomic roundtrips, the one door

The owner-ruled redesign that closed the recorded sweep: the engine
accepts turns in two phases (park -> hooks -> accept/veto -- vetoes
precede the fold, `pop_last_assistant` is deleted), the session
commits each tool-use roundtrip **atomically** through one commit door
(validate -> write -> grow), `Conversation`/`DanglingToolCalls`/
`interrupted_results` and both repair passes are deleted (a file
written only at commit boundaries cannot hold a half-open roundtrip --
a torn or dangling tail fails the open loud), the parser is one pass
producing the whole resident state (tree+head, context, register,
cumulative stats -- raw records are not retained), and the tree/writer
are extracted to their own files. Re-measured: **93.45% lines /
94.21% regions** -- at the pre-sweep level despite the format-work,
with the new modules at `parser.rs` 95.6%, `tree.rs` 94.1%,
`stats.rs` 100%, `recorder.rs` 96.0%, `session.rs` 98.2%.

- **Tree suite (new file)**: appends attach at the head and advance
  it, the stale-parent sanctioned crash, branch switching with the
  abandoned branch retained, the load-time head invariant and
  duplicate-id rejection, the broken-walk fault.
- **Writer suite (new file)**: the no-orphan gate (creation and
  pre-population touch nothing; the first drain materializes
  header+init+batch in order), the clean-prefix/durable-offset
  accounting, the two verbs' failure policies (write-behind keeps its
  lines, gated pops the batch whole), `append_to` resuming at the
  file's end.
- **Parser suite (new file)**: the produced state (tree, head,
  context with merged batches, register, cumulative stats over all
  branches and discards), and every rejection --
  torn tail (with its line number), trailing open batch, orphan
  result, mid-batch user message, side record inside a batch, broken
  parent link, mid-batch checkout target, unknown checkout target,
  future version, empty file.
- **Recorder suite rewritten around the door**: staging and the
  atomic close (file == memory == reloaded parse), the pairing
  validation's sanctioned crash, the
  single-occupancy slot mismatches (staging, results, close, discard
  -- all caught and named), the discard record (billed, nothing
  landed), the abort drop (no trace, unbilled -- not a ruled
  discard), the deferred register riding the first barrier, checkout
  as a pointer move, the mid-roundtrip checkout panic (flag 23), the
  gated barrier's validate-then-commit, and the trailing-checkout
  reload through adopt.
- **Session suite**: the defect-exhaustion run now bills its two
  discarded attempts (flag 22: `model, user, discarded, discarded`),
  the dangling-tail resume fails loud, the mid-batch rewind panics
  and writes nothing, and the interaction suite's abort-death test
  pins the new shape (the `aborted` marker is the whole durable tail
  -- the interrupted roundtrip never landed).
- **Justified residue, new classes**: `writer.rs` (73.0%) keeps the
  write-failure arms beyond materialization -- the mid-write truncate
  rollback, the serialize-failure arm, `append_to`'s open failure --
  the unstaged-fault class the old store section carried (a portable
  test can block directory creation but not a mid-`write_all`
  failure); `parser.rs`'s one uncovered region is the final-head
  closed-path defense (unreachable while every in-order check
  passes -- defense in depth, exercised through the checkout arm);
  `recorder.rs` keeps the dead-channel no-ops and the fault arms
  behind a dead disk inside the door's write-behind core.

## Structured-output enforcement deleted from the engine (2026-08)

The owner ruling after the sweep (ENGINE.md delta 14): output modes,
the synthetic `final_result` tool, both re-prompt policies, the
typed-prompt/extractor surface (`TypedPrompt`, `TypedPromptRequest`,
`TypedPromptResponse`, `StructuredOutputError`, `extractor.rs`,
`run_with_error_usage`, the provider composition capability and its
three provider overrides), and `finalize_streamed_choice` are deleted
-- the problem they served is gone on the providers tabit keeps, and
no tabit run ever set a schema. `output_schema` remains as pure
pass-through to the provider's native structured output
(`AgentBuilder::output_schema` -> request builder); the
`RoundtripClosed` item and the session door lose their feedback arm,
and a user message inside an open tool batch is corruption at the
parser, the door, and the closed-path check alike. Test fallout: the
output-tool collision suite, the typed/extractor suites (unit,
conformance, runtime-swap, facade cassettes), and the
structured-output cassettes trimmed to the native pass-through
smoke.


## Compaction/tree refactor + subagent pairing — the deferred re-measurement, closed (2026-09)

The delta-tokens refactor (session format v5: leaf-append compaction
nodes with a persisted `tokens_after` base, `delta_tokens` stamped at
assistant commits, the history view, `regime_total`, estimation
deleted) and the `parent_call` pairing round (`InternalCallId` at the
dispatch seam, `--parent-call`, the announce), measured with their
suites in place.

**Filled** (the gaps the measurement found):

- **The pass cap fired for the first time** —
  `the_pass_cap_bounds_pathological_regimes`: 410 rounds × 2,000
  tokens on the 64K minimum window; every pass folds the 24-round
  prefix cap (48,000 against the 49,152 fraction) and stays over the
  urgent bound, so the cap stops the loop at
  `Oversized { passes: 16, tokens_after: 52_020 }`. Writing the test
  taught the arithmetic: only the final pass's output rides
  `tokens_after` — each earlier summary folds into the prefix the
  next pass replaces.
- The unrepairable in-stream summarizer failure: a plain provider
  error (not the wall) fails the invocation — `Outcome::Failed`, no
  retry, nothing committed (the sibling of the overflow
  shorten-retry, which was already covered).
- **The subagent override deferral closed**: the subprocess e2e now
  rides the full surface (`model: "p/m"` resolving child-side, the
  explicit cwd, `tools: []` crossing as `--tools ""`) while its abort
  sibling exercises the inheritance defaults — every arm of the
  tool's policy block runs, `subagent.rs` clean.
- The failed-reason search's skip arm (a non-failure event *after*
  the failure — the search walks backwards) and `filter_tools`'s
  unknown-name refusal (unit; `DynamicTool` has no `Debug`, so a
  match, not `expect_err`).

**Deleted, not documented:**

- `Tree::node` — zero callers; the id-horizon machinery's last
  orphan, removed with that machinery's replacement.
- `HostLoop::worker`'s unknown-session error arm — the live routing
  failure errors inline in `handle` (the single home for that
  message); `worker`'s or_else duplicated it behind a door only the
  internal Replay arm used, and that caller now reads the table
  directly.

**Justified residue** (the recorded classes, line numbers current):

- `tree.rs` 112/140 — `history_to_head`'s two sanctioned corruption
  panics: a missing node is unreachable through every public door
  (inserts validate parents, the parser resolves links), and the
  cut-child-never-met crash is the corrupt-file fail-loud the
  refactor ruled.
- `fold.rs` 151 — the trailing-run sanctioned crash (unreachable by
  the `rposition` split; unchanged).
- `context_manager.rs` 374/414/425 — the non-assistant `_` arm
  (every caller passes an assistant by construction) and the checkout
  Corrupt belts (the provably-dead-by-load class).
- `compaction/mod.rs` 269-275 (the loop's total-function defense —
  the post-pass branch re-offers the compaction boundary while the
  floor holds, so a no-feasible-cut exit cannot precede the fits,
  guard, or cap exits), 533-539 (the request-build `Err` arm — the
  mock cannot produce the serialization failure; the in-stream
  sibling is now covered), 613 (no committed summary turn carries
  other content), 689 (the pre-request door's notice body — needs a
  live tap on a firing door; the identical `NoticeSink::emit` is
  covered through the beat doors).
- `compaction_doors.rs` 80-83 — `ensure_agent` (the registry
  `build_error` class).
- `endpoint.rs` 277/645 (the two `unreachable!` routing guards) and
  997-1001 (checkout execution-time failure — receive-time
  verification covers the stageable causes).
- `subprocess.rs` — the process-fault family unchanged (rejected/hung
  handshake, the reaper's force-kill, crash detection, the
  stderr-ring edges); the `--tools` builder arg now crosses in the
  e2e; `--session` stays deferred — it needs a persisted-child spawn
  shape the built-in tool never makes.
- `tabit/src/main.rs` (53.79%, the standing outlier) and `json.rs`'s
  child-role arms — the class-6 subprocess-execution attribution gap;
  the pairing round's child-boot wiring lines (`boot_parent_call`
  onward) join it: exercised by the e2e's spawned binary, unattributed
  by llvm-cov. Branch-region measurement needs a nightly toolchain
  (`-Z coverage-options=branch`) whose trybuild fixtures do not
  currently build — regions are read from llvm-cov's own summary
  instead.
