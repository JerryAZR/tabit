# ENGINE.md

The design record for the agent engine — the backend counterpart to
PROTOCOL.md's frontend contract. Two layers, kept strictly separate:

1. the **outer loop** — run lifecycle: when a run starts, what it is
   entered with, what it emits, how it is preempted — with the inner
   loop as a black box;
2. the **inner loop** — the turn state machine inside one run.

Structure, not steps: states, each state's single responsibility, the
machine/driver split, and the behavior deltas of the redesign. The
implementation follows this document; changes to the machine change
this document.

**Standing rule (owner):** flow-level changes consult this document
first and amend it before touching code. New flow behavior gets new
states (or new edges) — never conditionals grown inside existing
states, and never driver-side control flow outside the machine.
(PROTOCOL.md keeps the frontend/event view of the same
loop; the session actor implements the outer layer.)

## Layer 1 — the outer loop (the inner loop is a black box)

```mermaid
stateDiagram-v2
    Idle --> Draining : work signal — queue non-empty
    Draining --> Running : the batch joins the history —<br/>enter the inner loop at Preparing
    Running --> Idle : Done — emit run_finished
    Running --> Idle : Failed — emit run_failed
    Running --> Idle : abort preempts (token race at any await)<br/>— emit run_aborted; the ABORT SITE cleared the<br/>at-abort-time queue (the discard notice is immediate)
    Idle --> Idle : abort while idle — the abort site clears the<br/>queue (the notice is immediate; a no-op when empty)
    Idle --> Idle : checkout — the chain rewinds to entry_id;<br/>what was queued before the checkout is discarded<br/>(messages_discarded), then checked_out +<br/>a full replay pass
    Running --> Idle : checkout aborts the run mid-flight<br/>and executes at the beat — the pause point —<br/>before the next Draining
```

The **Draining** step is the outer loop's single responsibility between
idle and running: take the whole queue, join it into the history, and
yield each message's `user_message` event (the 1:1 invariant). It is
synchronous — no await between the take and entering the inner loop —
so nothing interleaves, and batching is exact.

**Entry contract** (what the outer layer hands the black box):

- the history **already joined** with the opening batch — the outer
  loop's Draining step did the join, so the inner loop receives one
  history and sends it as-is;
- at least one turn of budget (`max_turns ≥ 1` — entering a run that
  cannot run is unrepresentable).

**Exit contract** (what the black box guarantees):

- **at least one turn runs** — control never enters and leaves without
  issuing a model call;
- exactly one terminal: `Done(response)` or `Failed(reason)` — unless
  an internal error panics, which produces no terminal by design (the
  process dies; that is the loud failure);
- the run never observes abort as a state — abort preempts it from
  outside, and the mechanism matters: the machine's stream is polled
  **only through** the outer layer's `select!`, which is biased on the
  cancel token, so the first poll after cancellation returns the
  cancelled branch without polling the machine again — the stream
  future is dropped mid-step and **no further transition can fire**
  (a token-aware tool that returns a cancelled result instead of
  dying by drop changes nothing: its result lands in a future nobody
  polls). Tools die by drop-safety; the interrupted roundtrip is
  repaired by the outer layer's context re-derivation (synthesized
  results go to the log and the next open — never back into this
  run's model conversation). The outer layer records `Aborted` and
  discards the queue.

**Outer-layer responsibilities:** queue custody (the always-queue
invariant — every message yields exactly one user event or steers the
run in flight; the only discards are the clear sites, abort and
checkout, each discarding only what was submitted before it), the
Draining step (opening-input construction), terminal-event emission,
preemption, pause-point operations (checkout). Implemented today by
the tabit-session actor (`pump`/`run_one` + the mailbox and cancel
token); documented here because the entry/exit contracts above are
what the inner machine is designed against.

**One queue, two drains:** the outer drain opens runs (Idle →
Draining → Running); the inner drain converges turn outcomes. Both
take the whole queue at their instant. A message arriving while idle
lands in the outer drain; during a run, in the inner one. No-loss and
ordering hold across both.

**Pause-point operations (checkout, ruled 2026-08 stage 2).** Some
commands rewrite the conversation itself, so they cannot run inside a
run: `checkout { entry_id }` rewinds the chain to an entry. The
command path that serves this (owner-ruled through design review):

- **The router only routes.** The host loop resolves a session
  address and forwards into the session's handler— module code
  running synchronously at the dequeue point, opaque to the router.
  Routing failures (an unknown session) are the router's only errors;
  every command's semantics— validation included— live in the
  handler.
- **Pending intent is shared; the session is not.** The handler owns
  the per-session pending state: the mailbox (messages), a
  pending-checkout slot, a replay-request flag, the cancel token, the
  interaction hub. The worker task alone owns the session, so the one
  forced boundary is: session mutations wait for the worker's beat.
  Everything else can act at receive.
- **Checkout, at receive:** validate the target against the
  module's own id truth (a resident set the recorder maintains— every
  id the append-only file has ever held, dropped branches included;
  a bad target errors immediately, even mid-run) **— then abort**
  (ruled 2026-08: checkout composes abort, it does not wait on the
  run — the clear inside the abort IS the discard-at-receive: what
  `message_queued` announced and nothing drained comes back as
  `messages_discarded` right away, and the cancel ends the run at
  its next await point; what already entered the conversation is
  history the rewind drops) **— then park in the slot**
  (a slot, not a queue: a newer checkout replaces an older—
  concurrently parked checkouts are one intent re-aimed,
  and the collapse is lossless since each receive-clear already took
  everything pending before it).
- **Abort is drop-all-pending-intent:** cancel the run, clear the
  messages (the notice is immediate, through the mailbox's notice
  channel — one emitter with `message_queued`), clear the checkout
  slot. A discarded pending checkout emits nothing— no `checked_out`
  follows; the abort is the marker.
- **The beat** (idle wake, loop-top after a pump, the pre-close
  drain— one drain point in code): serve a parked **pass** (a read
  of the chain as it stands— reads and rewinds requested ahead of a
  message answer ahead of it; a message's inclusion in a pass is
  decided solely by whether it drained before the beat), then take
  the **checkout slot** (rewind— an execution-time failure is a
  no-op plus an `error` event; verification already caught the
  common failure at receive), then batch messages. The pump returns
  on an aborted outcome, so a checkout that aborted a run always
  executes at this beat before a later message starts a batch on the
  old chain.
- **Replay is a read that rides the beat only for emission
  coherence:** its pass shares the session's event stream with the
  run's frames, unmergeable without a per-session sequence number.
  The stage-4 seq primitive lifts it into a wait-free read— served
  at receive from a published chain snapshot, like any other read.
  Reads never hold writes: messages keep flowing while a pass is
  parked.

## Layer 2 — the inner loop (one run's turn machine)

Entry is **Preparing**: it takes the existing history and constructs
the request. Every later iteration is preceded by exactly one drain
and one decision. **Every** turn outcome — final, broken, tools, and
failure — converges at the same drain, because steering is
independent of everything the model does.

```mermaid
stateDiagram-v2
    [*] --> Preparing : enter — one history,<br/>drained and joined by the outer loop
    Preparing --> ModelTurn : request issued
    ModelTurn --> FinalTurn : committed, no tool calls
    ModelTurn --> BrokenTurn : typed defect — never committed
    ModelTurn --> ExecutingTools : committed, calls<br/>(admission scan at entry)
    ModelTurn --> DrainingSteers : provider/transport error (flagged)<br/>the queue still drains
    FinalTurn --> DrainingSteers
    BrokenTurn --> DrainingSteers
    ExecutingTools --> DrainingSteers : results appended
    DrainingSteers --> Deciding
    Deciding --> Preparing : loop — another turn<br/>(steers/tools/retry budget left)
    Deciding --> Done : final turn, queue silent
    Deciding --> Failed : terminal error / budgets exhausted / terminating
    Done --> [*]
    Failed --> [*]
```

**The drain is unconditional.** Steering messages are information the
user sent *for the model* — extra context, or a correction when the
user noticed the model on (or about to be on) a wrong path. A
model-side failure does not invalidate them, so nothing except abort
(a user action) ever discards or strands them: every outcome drains
the queue into history first, and a run that then fails carries that
history forward — the messages are recorded, surfaced as events, and
seen by the next attempt. There are **no bypass edges**.

### Error taxonomy (what a model turn can fail with)

| class | examples | handling |
|---|---|---|
| model-side defect | tool-call arguments that cannot be parsed | BrokenTurn; bounded retry; steers reset the streak |
| model-side mistake | a tool name not in the registry | admission scan at ExecutingTools entry: an in-band synthetic result tells the model; never stops the run |
| retryable provider/transport | rate-limit, transient connection failures, timeouts | drained, then bounded retry through the normal loop |
| terminal provider | auth failure, permanent quota, context overflow | drained, then exit-Failed — history (with steers) carries forward |
| internal (ours) | our own invariants | **panic and hard stop** — a development bug; the process dies loud. Not a state machine path and not a terminal: there is nothing graceful to do with ourselves |
| request construction | a provider cannot carry the content (e.g. a video attachment on Anthropic) | surfaced as a **terminal** error through the drain — implementation judgment: it can stem from *user content* (external input), so it fails gracefully rather than panicking |

A drained steer resets every retry streak, for the same reason as the
defect streak: new user input changes the situation, and the budgets
bound unattended loops. Retry budgets are small named constants.

### The machine contract

**Inputs** — the driver feeds the machine, and the contract is total:

| input | meaning | destination |
|---|---|---|
| `Final { turn }` | committed turn, no tool calls | FinalTurn |
| `Tools { turn }` | committed turn with tool calls | ExecutingTools (admission at entry) |
| `Broken { defect }` | typed malformed-tool-call defect; nothing committed | BrokenTurn |
| `error { class, reason }` | a provider/transport failure, classified per the taxonomy | flagged → DrainingSteers |
| `terminate { reason }` | a hook stopped the run | flag, read at Deciding |

**Flags owned by the machine** (the "data collected earlier" that
Deciding reads):

- `pending_error` — the classified error from the last turn, if it
  failed; Deciding retries it (budget permitting) or exits with it.
- retry streaks — consecutive failed attempts per retryable class
  (defects, retryable provider errors); each capped at a small named
  constant; reset by any committed turn and by any drained steer (a
  present, steering user is their own circuit breaker).
- `terminating` — set by hook stops; exits at the next decision.
- `steers_drained` — set by the drain; distinguishes "queue was silent"
  from "queue drained into history".
- budget — turns consumed by committed model calls only (a discarded
  turn returns its slot; a recovery retry consumes another).
- `last_outcome` — what kind of turn (if any) just ended; Deciding's
  exits are unreachable before the first outcome exists (the
  at-least-one-turn invariant, structurally).

serde/suspension remains a property of the machine (it stays
serializable), but nothing serializes a run today — there are no
compatibility constraints, only the discipline.

### State responsibilities

Each state has exactly one. Where a state was considered for splitting
during design review, the verdict is recorded.

- **Preparing** — take the existing history and construct the request.
  **The request is the history — no prompt/context split** ("just send
  the history", the same ruling that shaped `stream_chat`); consumers
  that need "the message being answered" (hooks, cancel errors) derive
  it as the history's last message, a view, not a field. Emits the
  model-call step carrying the whole history. *Self-review: the
  max-turns budget gate currently hiding in `PreparingRequest`'s
  `next_step` moves to Deciding — Preparing keeps nothing conditional.*
- **ModelTurn** — the provider turn is in flight; the driver drives it
  (request, stream, spans). The machine waits for one input of the
  five above.
- **FinalTurn** — commit the tool-free turn to history; in Tool output
  mode, apply the finalization policy (accept schema-valid text,
  re-prompt with feedback while budget remains, finalize best-effort).
  *Self-review: the output-mode conditional stays here deliberately —
  it is one policy ("what makes this turn final"), not two
  responsibilities.*
- **BrokenTurn** — discard the defective turn (it never entered
  history, on any provider), bump the defect streak. Simple path.
- **ExecutingTools** — admit the batch, hold it, receive paired
  results. Admission is a pure scan at entry: a call whose name is not
  in the registry is a *model-side mistake* — it executes as an
  in-band synthetic result naming the problem, so the model is told
  and can fix it; the run never stops on the model's own error (there
  is nothing a user could do about it). *Single concern: custody.
  Execution itself (concurrency, drop guards) is the driver's.
  Validating is deliberately not a separate state — interaction
  landed driver-side (see the tool phase below), and the machine
  deliberately gains no state for it: the machine awaits input only
  where the machine itself decides (classification, steering,
  budget), and no interaction decision is the machine's.*
- **DrainingSteers** — **the one and only drain point**: take the whole
  queue, append to history, set `steers_drained` (which resets every
  retry streak). Legality is structural — the machine offers the
  drain exactly here, so draining anywhere else is unrepresentable,
  not silently ignored.
- **Deciding** — read the flags, choose: loop (→ Preparing), Done, or
  Failed with its reason (budget / streak exhaustion / terminating).
  *Pure: no I/O, no stored state — realized as the drain's exit
  transition, but kept a named decision point so every loop-or-exit
  conditional has exactly one home. A conditional found anywhere else
  is a design bug.*
- **Done / Failed** — terminals. Done carries the `PromptResponse`;
  Failed carries the reason and the full history.

### The machine/driver split

| machine (control) | driver (data) |
|---|---|
| classification of the turn result | issuing requests, spans, telemetry |
| stage transitions | forwarding stream items to the consumer |
| steering points (when) | fetching from the `SteeringSource` (what), feeding `steer()` |
| budget, streak, terminating flags | tool dispatch: concurrency, drop guards |
| loop-or-exit decision | hook invocation, memory append at Done |
| — | turn identity: minting from the injected id source, announcing `TurnStarted`, publishing the id on the hook context (delta 10) |

### What the redesign deletes

- `ready_for_steering()` as a public runtime gate (structure replaces
  the check).
- `steer()`'s commit-the-pending-final-turn special case (FinalTurn
  commits at classification).
- The driver's three inline drains and its streak counter.
- `AwaitingAdvance`'s five-way conditional arm.
- `discard_turn` as a public transition (BrokenTurn's entry action).
- **The `InvalidToolCall` hook and its pause machinery** — the
  choices (`Fail`/`Retry`/`Repair`/`Skip`/`Stop`), the
  `ResolvingToolCalls` pause state, and `max_invalid_tool_call_retries`.
  Unknown tool names are handled in-band at admission; if a
  validation-time extension point is ever needed (permissions), it
  will be designed fresh, not restored in this form.
- Every failure path that skips the drain — there are none.
- Graceful handling of internal errors — they panic (see the
  taxonomy).

### Behavior deltas (documented, intended)

1. **One drain point** (was three), at the convergence — and it is
   unconditional: failures drain too; only abort discards.
2. **Retryable provider errors** (rate-limit, transient transport)
   are retried through the normal loop, bounded, with steers riding
   along — not an immediate hard stop.
3. **Exhaustion only fires on a silent queue** — a drained steer
   resets the streaks (owner ruling).
4. **Unknown tool names never stop or pause the run** — an in-band
   synthetic result tells the model; no hook question, no
   `UnknownToolCall` failure.
5. **The final turn commits at classification**, not lazily on the
   first steer.
6. **`max_turns` fires at Deciding** (after the drain), same observable
   outcome, one exit site instead of two.
7. **Hook stops unify** as the `terminating` flag read at Deciding
   (and at classification for pre-turn stops).
8. **Internal errors panic** — the process dies loud instead of
   degrading gracefully through the machine.
9. **Output-budget truncation is a warning, not a failure** (ruled
   2026-08, upstream-triage discussion): when a committed turn's
   terminal reports a truncation-class finish reason (`Length`), the
   engine surfaces a turn warning to the frontend and the flow proceeds
   exactly as usual — steers drain into the next turn, the
   another-turn check is unchanged, the run may end normally. Partial
   tool calls keep their own uniform path: in-band errors, never
   conditioned on the turn's finish reason (a partial call from a
   length cap is handled exactly like one from broken model output —
   no cause-based carve-out).
10. **Every model-call attempt is announced** (ruled 2026-08, v2 slice 1).
   At the moment an attempt commits — the same point the driver advances
   `previous_model`, after every hook stop and preparation failure, so an
   attempt that is announced is a request that will actually be issued —
   the driver mints a turn id from the run's id source, publishes it on
   the hook context (hooks observe the id of the turn in flight; the
   session's recorder stamps the committed entry with it), and emits a
   `TurnStarted { id }` item before any content of the attempt. The id
   source is injected: the engine's default is its short random ids, and
   consumers that key durable records on turn identity inject their own
   mint (tabit injects UUIDv7, so announced ids and log entry ids are
   literally the same value). Announced ids are never reused — a
   retried or failed attempt announces again with a fresh id, and an
   attempt that never commits leaves its id uncommitted (the frontend
   already discards provisional output on `TurnRetried`/abort). The
   announcement precedes the first content byte, so consumers learn a
   turn began before first-token latency elapses. The matching **commit
   edge is announced too**: a turn accepted into run history emits
   `TurnCommitted { id }` (after model-turn hooks resolve to advance),
   so live and replay bracket a turn with the same shapes — a turn
   discarded by a retry hook, a stop, a provider failure, or an abort
   never commits, and its announced id stays uncommitted.

## Implementation judgments (refactor landing)

Recorded where the code had to pick; revisit on review:

- **Request-construction failures** are terminal errors, not panics (see
  the taxonomy row) — they can stem from user content.
- **Zero budget is rejected at run construction** with a clear
  configuration error; the at-least-one-turn invariant makes "a run that
  cannot run" unrepresentable, so `max_turns(0)` is not a run shape.
- **Provider-error identity survives the decision**: the machine stores
  the classified error as a `PromptError`, but the driver restores the
  original `Completion`-shaped error at the exit, so consumers keep
  matching the provider's own error type.
- **The final turn commits at classification** and the Done candidate
  response is built then; a drained steer discards the candidate and
  re-opens the run.
- `AgentRunStep::Done` carries `Box<PromptResponse>` (the step enum's
  size is dominated by it).

## The tool phase (driver-side, ruled 2026-08)

The batch's execution is a designed driver subsystem — the machine's
`ExecutingTools` stays custody-only, but what happens under it is
specified here, not accreted.

**The chain is the unit.** Each admitted call runs one independent
chain: **gate → body → post** —

- *gate* — the `ToolCall` hook chain (argument rewrites, skips, the
  permission ask);
- *body* — dispatch of the tool itself (concurrency-bounded), which may
  park on user asks (the `ToolContext` interaction capability; a tool
  may ask any number of times);
- *post* — the `ToolResult` hook chain.

Chains run independently — in call order at `tool_concurrency` 1,
bounded-concurrent above it — with **no phase barriers**: one chain
parked on a permission card must not head-of-line-block its harmless
siblings (ruled: 1-in-10 denied says nothing about the other 9). A
parked gate occupies a concurrency slot; a two-pool split (gates
exempt from the body budget) is the named refinement if card-heavy
batches ever starve execution.

**The batch is a sealed unit once launched.** Launch (admission +
the upfront model tool-call events) → run (chains) → settle (collect
everything, surface and commit results in call order,
unconditionally). Settlement cannot be stranded: every chain is
bounded by its own timeout or the user.

**Stop taxonomy (ruled)** — three stop-shaped needs, one mechanism
each; **nothing may kill a batch**:

| need | mechanism | semantics |
|---|---|---|
| stop now | **abort** (the token leaf) | preempts at any await; `run_aborted`; queue discarded; unanswered calls get synthesized interrupted results. Callable by the user, frontends, and any hook constructed with the leaf. |
| don't continue after this batch | **post-tool `Stop` → the `terminating` flag** | no effect on the current batch — unstarted chains still run; the flag is fed only after `tool_results` commits, so the tool phase is flag-blind by construction. Steers still drain; `Deciding`'s another-turn check is overridden → `run_failed(reason)`; history carries forward. |
| don't run this call | **`Skip`** | in-band synthetic result; the model is told; siblings unaffected. |

The pre-tool `Stop` action is deleted (its niches compose from
`Skip` + abort), and the fail-fast machinery with it (`first_error`,
the start-gate flag, lowest-index-error selection,
drain-vs-drop): `run_single_tool` has no error path, and settlement
is unconditional — nothing exists that could strand a parked ask.

**Interaction — the ask pattern.** One hub (the actor's third shared
leaf beside the mailbox and abort): an ask registers a oneshot in
the pending map, emits `interaction_request` on the event channel,
and awaits; `interaction_response` routes by id (unknown id or dead
receiver: log and drop — total semantics, like abort-while-idle).
Two sites share the one primitive: the gate (a permission hook
constructed with the hub handle — deny maps to `Skip`) and the body
(`ToolContext` capability). Questions die with their chains — drop
is the cancellation — and run terminals clear the pending map; the
frontend closes cards on run terminals (no close event exists or is
needed: every unanswered question's death coincides with a run
terminal, structurally). Interaction requests never persist or
replay; the durable record is the tool result (the answer or denial
the model saw).

Pause points are enumerable — only context-carrying sites can ask
(today: the tool-call gate by construction, the tool body via
`ToolContext`); other hook points gain the capability when a
consumer exists.

## The hook surface — closure registration (ruled 2026-08;
shipped)

`AgentHook` (the nine-method observer trait) is retired as the public
authoring surface. It solved an extension-authoring problem—a
coherent multi-event observer with shared state—inside the
dispatch mechanism, at dispatch-layer cost: the wide defaulted
interface (every real consumer implements exactly one method), the
RPITIT dyn-incompatibility, and the private `DynAgentHook`/`HookStack`
erasure layer that translates bundles back into per-event callables.
Dispatch wants callables; bundling is the extension's concern.

- **Registration is a record, not a bare closure**: `{ id, priority,
  closure, metadata }`—the extension runtime stamps its owner id
  onto records when it arrives. Bare closures remain sugar (generated
  id, default priority). Attribution ("hook `perms` denied the
  call"), introspection, and replace-by-id fall out of the id. The id
  is **author-chosen** (a declared artifact, like a tool name—the
  author namespaces it, `ext:<id>:<name>` when the runtime arrives);
  it never reaches the wire, and it is never the interaction id—a
  registration names a subscription, a transaction id names one ask.
  Attribution may flow into an ask's *payload*; routing never knows
  which hook asked.
- **One shape per event**: `Fn(&RunContext, Event<'_>) ->
  BoxFuture<'static, Action>`— a single async form for all events
  (sync events ride trivial futures) and a copy-out signature: the
  future may not borrow, so closures take what they need from the
  event by value before awaiting. The shape guest adapters cross
  naturally.
- **The order law**: per event, subscribers are stably sorted by
  priority (`i32`; equal priorities fall back to registration order),
  sorted when the run seals the stack. Reference priority bands are
  extension docs, not type-level.
- **Action algebras, stated per event** (composition in sorted
  order): `on_tool_call`— **Skip is absorbing** (the first deny
  in order wins; later hooks do not see the call), **Run is neutral**
  (an early allow never stops a later gate—a hook that must
  observe denials registers earlier; priority is also visibility);
  `on_model_select`—the last selection in order wins and a stop
  is terminal; patches compose sequentially. Each event's rule is
  contract, restated under sorted order from today's semantics.
- **`observes()` dies**—not-registered is the filter.
- **One run context**: `HookContext` and `ToolContext` merge into a
  single run context (run id, turn state, scratchpad, the capability
  map with accessors) handed to hooks and tool bodies alike. "Why
  could my tool ask the user but not my hook" is removed, not
  documented; the interaction capability is the interaction round's
  generic ask.
- **Mounting and sealing**: registrations live on the session
  builder (the extension mount—tools, hooks, later skills, one
  surface); the stack seals per run. Reload applies to future
  session builds—a built session is sealed (recorded choice).
- **Failure doctrine** unchanged (AGENTS.md): extension (external)
  failures are graceful, clear, and attributed by registration id;
  the engine's own stay loud. The extension runtime wraps its
  closures accordingly—nothing below it changes.

The trait remains for the engine's internal multi-event observers;
`DynAgentHook`/`HookStack` remain the storage, now built from
records. Cross-event state is the asker's concern: captured `Arc`s
(chains run concurrently—interior mutability was always required
of `&self` too, so nothing is lost) or id-keyed correlation; the
scratchpad stays engine-internal.

## Migration map (old → new)

| current | new home |
|---|---|
| tabit-session `pump`/`run_one` + mailbox + token | Layer 1 (already implemented; documented here for the contracts) |
| `PreparingRequest` | Preparing (split only; budget gate → Deciding) |
| `CallModel { prompt, history }` step split | deleted — the step carries the whole history; "the message being answered" is a derived last-message view |
| `AwaitingModel` | ModelTurn |
| `ResolvingToolCalls` + `resolve_invalid_tool_call` + the `InvalidToolCall` hook | deleted — admission is a pure scan at ExecutingTools entry; unknown names get in-band synthetic results |
| `max_invalid_tool_call_retries` | deleted (no interactive recovery) |
| `AwaitingAdvance` (no-tool arm) | FinalTurn |
| `AwaitingAdvance` (tools arm) | ExecutingTools entry (admission scan) |
| `AwaitingAdvance` (output-tool arm) | FinalTurn's finalization policy |
| `ExecutingTools` + `tool_results` | ExecutingTools (+ closing transition) |
| `Done` / `Failed` | Done / Failed |
| driver defect path + `discard_turn` | BrokenTurn |
| driver hard-fail on provider errors | `error { class }` input → drain → bounded retry or exit |
| driver `drain_steers` × 3 | DrainingSteers |
| driver streak counter | machine `defect_streak` flag |
| driver hook-stop exits | machine `terminating` flag |
| `retry_model_turn` (hook Retry) | FinalTurn transition (rejected turn re-queues; feedback mode records it) |
| `ready_for_steering` / `steer()` commit case | deleted (structural) |
