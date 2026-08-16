# PROTOCOL.md

The design record for the frontend protocol (ROADMAP item 7/8): what is
locked, and every open flag with its analysis and options, so nothing
gets re-derived. Flags are resolved by discussion in list order; a
resolved flag records its decision and stays as history.

## Locked design

- **Commands are fire-and-forget with total semantics** — `message`
  (steers the run in flight, or starts one), `abort` (aborts + discards
  the queue; no-op idle). No ids, no request/response, no rejection
  cases: every rejection case we could construct was a buggy client or
  better served by total semantics.
- **Every drain point takes the whole queue**: idle entry batches all
  pending messages into one run's opening input; the engine drains the
  rest as steers at turn boundaries. Invariant: every `message` yields
  exactly one `user_message` event; the only discard is abort.
- **Failures are events**: `RunFailed` + `RunOutcome::Failed`; the run
  path has no `Err` return (`prompt`/`prompt_with` are thin wrappers
  over `submit` + `pump`).
- **Events are stamped** `EventFrame { stream, event }`; v1 stamps
  `"main"` on everything (concurrent producers — subagents — mint
  siblings later; retrofitting stamps is a breaking sweep).
- **`stream_chat` takes a conversation**: the final message is the turn
  being sent (the engine's own rule for every turn); callers add
  messages to history before the call, retries resend verbatim. An
  empty conversation fails loudly at send.
- **Session files materialize at the first user message** (header +
  opening `model_change` + the message); a session that never runs
  leaves nothing on disk; `--list` reads a missing sessions directory as
  empty.
- **Handshake only at a serialized edge**: `initialize` →
  `initialize_ack` (session facts) or `initialize_rejected` + exit 1;
  unparseable lines / premature commands → `protocol_error`, connection
  stays. Tagged JSON lines, not JSON-RPC 2.0.
- **Termination contract**: `close_commands()` (or dropping every
  sender) ends the actor after the in-flight run; the event stream then
  closes. Close is not a barrier — commands already queued are honored.

## Open flags (discussion order)

### 1. Actor session ping-pong vs a resident loop — RULING WANTED

The actor holds `Option<Session>`, moves it into each spawned pump
task, and takes it back over a channel; three coordinated branches keep
the invariant (command arm spawns pumps, return arm restarts on
leftovers, the leftover check covers the handoff window).

Options:
- **Keep ping-pong** — the session is actor-reachable between runs
  (future idle commands: `set_model`, `rewind`, stats), at the cost of
  the handoff window, the `Option` dance, and the untested leftover
  branch (flag 7).
- **Resident loop** — one task owns the session forever: `loop { wait
  for work / command; pump }`. Message and abort never needed the
  actor at all (they are shared-state mutations: mailbox + cancel
  token — exactly how print mode's Esc watcher works), so the handle
  can submit them directly and the "actor" shrinks to the resident
  worker plus a work-notification in the mailbox. Deletes the return
  channel, the `Option`, the leftover branch (flag 7 dies
  structurally), and one of the two termination mechanisms (flag 4
  dies). Cost: idle-time session commands (none exist yet) must route
  through the worker's wait loop, and the mailbox grows a
  `tokio::sync::Notify`.

Recommendation: resident loop — the only commands that must land
mid-run are exactly the two that shared state already serves.

### 2. `run_one` failure epilogue — mechanical

Four sequential outcome blocks (aborted / stream-failure /
reload-failure / persist-failure) with a `!Failed` guard on the reload.
A `fail(..)` helper flattens it. No semantic change.

### 3. `run_one` length — cosmetic

~150 lines: recording + batch, engine fold, epilogue. The fold body can
extract beside `stream_item_event`.

### 4. Dual termination mechanisms — dies with flag 1

Shutdown token + channel-close arm; the token exists because
`tokio::UnboundedSender` has no `close()`. The resident loop (flag 1)
needs only the token.

### 5. "Close is not a barrier" — document

Commands sent before `close_commands()` run; the actor's dequeue is the
boundary. Correct, tested; deserves the contract written on
`close_commands` itself.

### 6. Twin abort clears — document the proof

The actor's Abort handler and `run_one`'s aborted branch both clear the
mailbox; each covers a different interleaving (abort between runs vs
mid-run). Without a comment pair this reads like removable duplication.

### 7. Untested leftover branch — dies with flag 1

Load-bearing (a message in the pump-handoff window only runs because of
it), coverage-justified, no deterministic test. The resident loop
removes the window entirely; otherwise stage a slow-tool test.

### 8. Terminal events are not terminal — RULING WANTED

`RunFailed` can follow `RunFinished` (post-run persistence failure). A
frontend whose read loop stops at the first terminal silently misses
durability failures.

Options: accept and make "read to stream end" the law; or fold
durability into the terminal (`run_finished { durable: false }` +
a single follow-up), keeping "one terminal per run" true.

Recommendation: one terminal per run — the invariant is worth more
than the event's simplicity.

### 9. Empty conversation rides `PromptCancelled` — rename

An empty history is not a cancellation; the variant name misleads. Add
a dedicated error variant or a malformed-input home.

### 10. `list()` platform divergence — documented, tested

Windows reads a blocked store path as empty (`NotFound`), Linux errors
(`ENOTDIR`). Inherent; the write side fails loudly everywhere.

### 11. Empty `pump()` reports `Completed` — document or forbid

Direct `pump()` calls on an empty mailbox return a vacuous `Completed`.
`prompt_with` cannot hit it. Document, or make it unrepresentable.

### 12. Rapid-message batching is scheduler-nondeterministic — FIX, cheap

Two quick messages may batch into one run or form two, depending on
pump-vs-actor scheduling. No-loss holds, but tests assert weakly and
scripts get no guarantee. Fix: when work arrives while idle, drain
already-queued commands before starting the pump — pipelined messages
then always batch. Recommendation: do it.

### 13. The protocol borrows engine types — RULING WANTED

`RunFinished { usage: rig_core::Usage }` and `NativeItem { Value }`
put engine shapes on the wire: engine refactors churn the protocol
silently, and `NativeItem` is provider knowledge leaking into
frontends. Options: protocol-owned slim types (our own `Usage`, typed
or explicitly-opaque native items), or accept rig-core as the shared
vocabulary crate (it is ours). Recommendation: own the types — the
protocol is the foundation; the engine is an implementation detail.

### 14. `RunFailed` is stringly — small

A display string, not a kind; frontends cannot branch
retryable-vs-fatal without string matching. Add a small kind enum
(`provider`, `budget`, `durability`, `internal`).

### 15. Unbounded event channel — ledger with a trigger

A stalled frontend grows memory mid-run. Accepted at v1; the TUI
milestone needs the real backpressure answer.

### 16. Ack-before-events ordering is causal, not structural — cheap fix

The bridge holds because the reader sends the ack before any command; a
reordered line breaks it silently. Structural fix: the forwarder starts
only after the handshake completes.

### 17. Mid-run test staging needs real sleeps — test infra

The `slow_tool` pattern (300ms per mid-run test). Engine-level
awaitable mock turns (a `Notify`-armed stream event) remove the latency
and the flake surface.

### 18. Callback type ergonomics — small

`&mut (dyn FnMut(SessionEvent) + Send)` is awkward at call sites; the
`Send` is forced by spawning. By-value `impl FnMut(SessionEvent) +
Send` or an owned box reads better.

### 19. Exit conventions differ by mode — unify

JSON mode returns `i32`; print mode signals via `Err` that `main`
converts. Two paths for one concept.

### 20. CI clippy skew — infra, now 3-for-3

Every code push needed a follow-up for a lint the local toolchain
lacks. Fix the machine: match the local toolchain to CI (and record it
in AGENTS.md), or pin both.

## Resolved

(none yet — flags move here with their decision and the commit that
implemented it)
