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
- Current state: **93.53% lines / 94.24% regions** (4,057 of 62,724
  lines; re-measured after v3 stage 2 — checkout: the pause-point
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
  pinned by `frontend_death_drops_a_parked_checkout_instead_of_
  rewinding` (wire-order anchor: a message queued after the checkout
  proves routing before the death; the log keeps both exchanges).
- The permission actor suite now decides by content, not by event
  presence: allow asserts the body's own output (`ran: x`), deny
  asserts the in-band denial verbatim, ask_user asserts the answer
  reached the tool. The racing answer in the abort test is sent while
  the host still lives (it reaches the handler; no error frame).
- The pause-seam test submits its survivor at the first run's
  terminal — the predicate's decision point is genuinely exercised.
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
  see the flag for the rationale); flag-8's `persist_degraded`
  producer (the write-behind work item; the frontend side already
  folds it).

## Tool-gate seam (2026-08, the permission-leak review) — superseded

`gate.rs` (ToolGate/ToolGateFactory) was deleted by the hook-surface
round above; this section survives as history. What still holds from
it: `interaction.rs` — the hub's own suite (routing, no-op,
retraction, weak-sender dismissal) is unchanged and green; the
permission vocabulary tests live in `permission.rs` with the
vocabulary, and the actor-level permission suite (allow, deny with
reason, always-allow memory, reverse-order cards, abort and frontend
death with a card open) runs through the value seam.

## v3 stage 2 — checkout (2026-08)

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

