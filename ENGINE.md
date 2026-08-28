# ENGINE.md

The design record for the agent engine — the backend counterpart to
PROTOCOL.md's frontend contract. Two layers, kept strictly separate:

1. the **outer loop** — run lifecycle: when a run starts, what it is
   entered with, what it emits, how it is preempted — with the inner
   loop as a black box;
2. the **inner loop** — one run's coroutine: the turn cycle inside it.

Structure, not steps: phases, each phase's single responsibility, the
loop/leaves split, and the behavior rulings. The implementation follows
this document; changes to the loop change this document.

**Standing rule (owner):** flow-level changes consult this document
first and amend it before touching code. New flow behavior gets new
phases (or new edges) — never conditionals grown inside existing
phases, and never control flow outside the loop.
(PROTOCOL.md keeps the frontend/event view of the same loop; the
session actor implements the outer layer.)

## Layer 1 — the outer loop (the inner loop is a black box)

```mermaid
stateDiagram-v2
    Idle --> Running : work signal — mailbox non-empty, or a continue<br/>signal with a non-empty conversation —<br/>and the entry guard passes:<br/>the buffer's drain attempt succeeds<br/>(stuck lines retry here; a still-degraded<br/>buffer blocks the start)
    Running --> Idle : Done — emit run_finished
    Running --> Idle : Failed — emit run_failed
    Running --> Idle : abort preempts (token race at any await)<br/>— emit run_aborted; the ABORT SITE cleared the<br/>at-abort-time queue (the discard notice is immediate)
    Idle --> Idle : abort while idle — the abort site clears the<br/>queue (the notice is immediate; a no-op when empty)
    Idle --> Idle : checkout — the chain rewinds to entry_id;<br/>what was queued before the checkout is discarded<br/>(messages_discarded), then checked_out +<br/>a full replay pass
    Running --> Idle : checkout aborts the run mid-flight<br/>and executes at the beat — the pause point —<br/>before the next work signal
```

**The opening batch is not pre-joined.** The old Draining step (join
the queue into history behind the prompt barrier) is deleted: the
inner loop's first CONVERGE drains the opening batch itself — the same
code path as every other drain. One queue, **one** drain: a message
arriving while idle and a message steering a live run take the same
`take_all` at the loop top; batching, ordering, and the 1:1
message-to-event invariant hold across both without special cases.

**The degraded-buffer guard (the weak barrier; ruled 2026-08).** At
run entry the buffer gets one drain attempt — `enqueue(&[])`, an empty
batch, which is by construction a pure retry of any stuck lines. Ok
proceeds (clean, or just recovered: `persist_recovered`); Err blocks
the start gracefully (`persist_degraded` — an external condition, not
a crash). Semantics: **the first failed drain runs in memory** — the
prompt fold's enqueue fails, the conversation proceeds anyway, the
lines stay queued and retry on every later enqueue — **but the next
start is blocked** until a drain succeeds. At most one run proceeds on
undrained state, and only the first. This supersedes the old prompt
barrier (flag 8's second amendment): no run is ever refused for the
disk *before* trying, and none runs twice on the same degraded buffer.

**Entry contract** (what the outer layer hands the black box):
the conversation (a `ContextManager`) and `max_turns ≥ 1` — entering a
run that cannot run is unrepresentable. The mailbox may be empty (a
continue run); the conversation must not be (nothing to continue).

**Exit contract** (what the black box guarantees):

- **at least one turn runs** — control never enters and leaves without
  issuing a model call (by construction: the first pass of the loop
  can exit at nothing — no streak is set, no error is pending, and the
  budget is ≥ 1);
- exactly one terminal: `Done(response)` or `Failed(reason)` — unless
  an internal error panics, which produces no terminal by design (the
  process dies; that is the loud failure);
- the run never observes abort as a state — abort preempts it from
  outside: the actor's `select!` is biased on the cancel token, so the
  run future is dropped mid-await and **no code after the await runs**.
  Drop-safety is structural: every conversation write in the loop is
  an atomic commit (below), so the conversation always stands at a
  roundtrip boundary when the future dies — the interrupted turn was a
  loop local and never entered anything. The outer layer records
  `Aborted` and clears the queue.

**Outer-layer responsibilities:** queue custody (the always-queue
invariant — every message yields exactly one user event or steers the
run in flight; the only discards are the clear sites, abort and
checkout, each discarding only what was submitted before it), the
work-signal and entry-guard, terminal-event emission, preemption,
pause-point operations (checkout). Implemented by the tabit-session
actor (`pump`/`run_one` + the mailbox and cancel token).

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
  conversation's id truth (the tree — every entry the append-only file
  has ever held, dropped branches included; a bad target errors
  immediately, even mid-run) **— then abort** (ruled 2026-08: checkout
  composes abort, it does not wait on the run — the clear inside the
  abort IS the discard-at-receive: what `message_queued` announced and
  nothing drained comes back as `messages_discarded` right away, and
  the cancel ends the run at its next await point; what already
  entered the conversation is history the rewind drops) **— then park
  in the slot** (a slot, not a queue: a newer checkout replaces an
  older— concurrently parked checkouts are one intent re-aimed,
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

## Layer 2 — the inner loop (one run's coroutine)

The turn state machine is **deleted** (ruled 2026-08, the loop
refactor): it was a coroutine re-encoded as a state enum plus a
feeding protocol — one feeder, no external events choosing
transitions, and all genuinely asynchronous things (abort, steers,
tool chains) already lived outside it. The run is now **one async
function** — a coroutine for real — that holds `&mut ContextManager`
for its lifetime and receives the shared leaves (steer source, cancel
token, interaction hub, event channel) as parameters. The
conversation's source of truth is the `ContextManager` (tabit-log):
`fold` / `fold_all` / `messages`, context derived per read and never
stored, batches verified whole and committed whole. There is no
engine-side history copy, no parked-turn slot, and no session-side
mirror.

```
run(conversation: &mut ContextManager, leaves, budget) -> Outcome

state carried across iterations — run locals, the whole of it:
  defect_streak, provider_streak   // consecutive failed attempts
  pending_error                    // classified failure of the last attempt
  terminating                      // reason; settable ONLY post-tools
  turns_used                       // committed turns only

loop {
  // ── CONVERGE ── the one drain; reaching it is the convergence.
  //   Every SETTLE branch falls through to here, so the drain is
  //   unconditional by shape — a bypass edge would need a `continue`
  //   that skips the loop top, which is not writable.
  if terminating {                    // the turn that set it finished
                                      // and committed naturally; a stop
                                      // never drains
    mailbox.clear_with_notice();      // messages_discarded — the stop is final
    exit Failed(stopped);
  }
  steers = mailbox.take_all();
  for s in steers {
    conversation.fold(user(s));       // [WRITE] the first iteration's fold
    events.emit(user_message(s));     // IS the prompt commit
  }
  if !steers.is_empty() { defect_streak = 0; provider_streak = 0; }
                                      // a steering user is their own breaker

  // ── DECIDE ── the one policy site; every loop-or-exit conditional
  //   lives here. The first pass cannot exit: nothing is set and
  //   budget >= 1 — at-least-one-turn by construction.
  if pending_error is terminal     { exit Failed(pending_error); }
  if defect_streak   > DEFECT_CAP  { exit Failed(defects_exhausted); }
  if provider_streak > RETRY_CAP   { exit Failed(retries_exhausted); }
  if turns_used      >= budget     { exit Failed(budget_exhausted); }

  // ── PREPARE ──
  history = conversation.messages();  // [READ] the request IS the history;
                                      // no prompt/context split
  turn_id = ids.mint();               // announced ids are never reused
  events.emit(TurnStarted { turn_id });

  // ── MODEL ── stream deltas → events as they arrive; hooks observe.
  //   `cancel` races every await below: on abort the run future is
  //   dropped, nothing after the await runs, and the conversation is
  //   at a roundtrip boundary — every WRITE here is an atomic commit.
  outcome = model.call(history).await;

  // ── SETTLE ──
  match classify(outcome) {
    Defect =>                          // malformed tool-call arguments
      events.emit(ModelTurnRetried { turn_id });
      defect_streak += 1;              // the turn was a local: nothing
      continue;                        // folded, nothing to un-fold
    ProviderError(class, e) =>
      pending_error = (class, e);
      if class is retryable { provider_streak += 1; }
      continue;
    Turn { choice, .. } => {
      hooks.model_turn_finished(choice);   // observe-only
      if choice carries no tool calls {
        if !choice.is_empty() { conversation.fold(assistant(choice)); }  // [WRITE]
        events.emit(TurnCommitted { turn_id });
        turns_used += 1;
        exit Done(response_from(choice));  // a steer arriving now is the
      }                                    // NEXT run's opening batch
      calls   = admit(choice.calls);       // name scan; unknown → in-band
      results = execute(calls).await;      // chains on the sidecar (below)
      conversation.fold_all([assistant(choice), results]);  // [WRITE] atomic
      events.emit(TurnCommitted { turn_id });
      turns_used += 1;
      if hooks.requested_stop { terminating = reason; }  // set AFTER the batch
      continue;
    }
  }
}
```

**The write sites are the whole durability story.** Exactly three:
the drain's user folds, the final `fold`, the roundtrip `fold_all` —
each an atomic verify-then-commit (records enqueue as one batch, tree
grows in the same operation). Every await point therefore sees the
conversation at a roundtrip boundary, which is why abort can simply
drop the future. The in-flight turn — from MODEL completion to its
`fold`/`fold_all` — is a **loop local**: it enters nothing, so a turn
that dies (defect, abort, failure) needs no un-folding, no discard
machinery, and no pending slot anywhere.

**Entry ids and announced ids are one value**: the assistant
`Message`'s `id` (minted by the engine from the injected id source,
announced by `TurnStarted` before the first content byte) becomes the
assistant entry's id at commit. `TurnStarted { id } … TurnCommitted
{ id }` bracket a turn for live and replay alike; an attempt that dies
leaves its announced id uncommitted. **`TurnCommitted` is the sole
commit event** — final folds and roundtrips alike (the old
`RoundtripClosed` is deleted: it always fired beside `TurnCommitted`
and only existed as the session door's trigger, and the door is now
inline in `fold_all`).

### Error taxonomy (what a model turn can fail with)

| class | examples | handling |
|---|---|---|
| model-side defect | tool-call arguments that cannot be parsed | discarded as a local; `ModelTurnRetried`; bounded streak; steers reset it |
| model-side mistake | a tool name not in the registry | admission scan: an in-band synthetic result tells the model; never stops the run |
| retryable provider/transport | rate-limit, transient connection failures, timeouts | drained, then bounded retry through the normal loop |
| terminal provider | auth failure, permanent quota, context overflow | drained, then exit-Failed — history (with steers) carries forward |
| internal (ours) | our own invariants | **panic and hard stop** — a development bug; the process dies loud. Not a loop path and not a terminal: there is nothing graceful to do with ourselves |
| request construction | a provider cannot carry the content (e.g. a video attachment on Anthropic) | surfaced as a **terminal** error through the drain — implementation judgment: it can stem from *user content* (external input), so it fails gracefully rather than panicking |

A drained steer resets every retry streak, for the same reason as the
defect streak: new user input changes the situation, and the budgets
bound unattended loops. Retry budgets are small named constants.
Streaks are run locals: fresh at every run entry (every run is an
attended start — a message or an explicit continue signal); if
continue is ever driven *automatically*, that driver needs its own
cross-run bound.

### Hooks (observe-only; ruled 2026-08)

The model-turn veto is deleted with the machine that housed it:
`RetryRequest`, `ModelTurnAction`, and the parked-then-accept protocol
are gone (upstream rig vocabulary, never consumed by tabit — the
"continue with correction" case is a steer, the "retry fresh" case is
unsupported until discussed; PROTOCOL.md flag 31 keeps the hook-action
inventory open). Model-turn hooks observe the completed turn and
return nothing. The remaining action surfaces (tool-call `Skip` /
`Rewrite` / post-tool `Stop`, completion-call `active_tools`) are the
stop-taxonomy and tool-phase machinery below.

### The loop/leaves split

| the loop (control) | the leaves (shared cells) |
|---|---|
| classification of the turn result | steer source / mailbox: `take_all` at the loop top only |
| the one drain; the one policy site | cancel token: races every await, never a state |
| conversation writes (`fold`, `fold_all`) | interaction hub: asks register/resolve; run terminals clear |
| budget, streaks, `terminating`, `turns_used` | event channel: write-only, frontend-facing |
| loop-or-exit decisions | the conversation: **owned by the session's
handler** (`ContextManager` behind the session's `RwLock`); the loop
streams its history in and its durable items out — the handler folds
at the item arms |

**Every shared cell has one owner, named.** Whoever else may read it
is stated; nobody else may write it. The table above is the contract
— a seam with two writers (or an unnamed owner) is a design bug, not
a gap to patch around. Today:

| cell | owner | readers (never writers) |
|---|---|---|
| the conversation (tree + head) | the session's handler, via `ContextManager` | the receive-time probes (checkout validation) — read-only |
| the write buffer | the session's `SessionWriter` (shared handle) | the manager's commits, the session's side records — both enqueue through it |
| the mailbox | the session actor | the loop's steer drain (take at the loop top) |
| the cancel token | the abort handle | every await in the run (raced, never read as state) |
| the interaction hub | the session actor | tool gates/bodies (asks) |
| the model register | the `ModelRegister` (receive-time write) | run open (reads the selection) |
| the persist-degraded flag | the writer (set/clear on its enqueue outcome) | the session, draining transitions at the guard and conclude |

**The probe's read is tree-truth.** The probe's `contains` names
committed nodes only: a checkout target must be a committed,
roundtrip-closed node (flag 23's rule), so an id that is announced
but not yet folded (a queued message, an in-flight turn) is not a
valid target — and that is correct: you can only rewind to a
committed checkpoint. The read is race-free by the `RwLock`
discipline (folds grow the tree in one write-hold; a probe reads
through `read()`, so it can never observe a partially folded state).
A checkout composes abort, so its application at the beat runs
against a quiescent tree — the only writes after abort are the
session's side records, never tree growth.

**The persist-degraded flag's one clear site is the entry guard's
retry.** A stuck buffer retries at every later enqueue and at run
entry; the flag clears when a retry drains. There is deliberately no
user-facing "retry persistence" command — the entry guard is the
only retry site until a frontend asks for one (then it is designed,
not grown).

What the session consumes is conversation truth (`manager.messages()`)
and frontend feed (events) — it mirrors nothing. Side records
(`model_change`, `checkout`, `aborted`, …) are the session's own
enqueues through its shared buffer handle.

### What the loop refactor deletes

- the `RunState` enum, the `AgentRunStep` protocol, and the
  machine/driver split they forced;
- `TurnParked`, two-phase acceptance, `veto_turn`, and every parallel
  flag (`pending_final`, `pending_response`, `retry_requested`,
  `steers_drained`, `last_outcome`) — locals now;
- the engine's `Context` type and its public fold surface (the
  one-context-builder fold lives inside `ContextManager::messages`);
- the recorder: its door (the manager's `fold_all`), its pending
  roundtrip slot (the loop local), its item mirror (nothing mirrors),
  and its register/notice machinery (session-side enqueues and the
  entry guard);
- `RoundtripClosed` and the session-side commit choreography it
  drove;
- the serde/suspension discipline (nothing serializes a run; a
  coroutine has no state to serialize).

## Implementation judgments (refactor landing)

Recorded where the code had to pick; revisit on review:

- **Request-construction failures** are terminal errors, not panics
  (see the taxonomy row) — they can stem from user content.
- **Zero budget is rejected at run construction** with a clear
  configuration error; the at-least-one-turn invariant makes "a run
  that cannot run" unrepresentable, so `max_turns(0)` is not a run
  shape.
- **Provider-error identity survives the exit**: the loop stores the
  classified error, but the exit restores the original
  `Completion`-shaped error, so consumers keep matching the
  provider's own error type.
- **A steer arriving during the final turn** exits the run (`Done`) —
  the steer opens the next run at the work signal (ruled 2026-08: one
  less thing to check, identical behavior).
- **Empty finals fold nothing and record nothing** — one decision
  site (the loop), which closes PROTOCOL.md flag 29 by deletion.
- **Usage facts are deferred** (owner ruling 2026-08): assistant
  entries the manager constructs carry zeros from one named site;
  discard billing (flags 25/27) returns with that discussion.

## The tool phase (loop-side subsystem, ruled 2026-08)

The batch's execution is a designed subsystem of SETTLE — specified
here, not accreted.

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

**The execution substrate (ruled 2026-08; shipped).** Tool bodies
never poll on the session's executor. Every body dispatches through
the single `dispatch_tool` boundary onto a process-wide sidecar
runtime, and the chain awaits the completion handle: harness
responsiveness — abort preemption, interaction routing, sibling
chains, event flow — is structural, never borrowed from tool-body
behavior. A body may block (it occupies a sidecar worker) or hang
(it leaks a sidecar task); the harness is unaffected either way.
Cancellation follows three layers: **the token is the ask** — abort
detaches the sidecar task (its result lands nowhere) and the body's
token observation or timeout ends it; drop is no longer the
mechanism, though a body dropped at task end still cleans up as a
backstop; **bounded bodies are the expectation** — settlement
already assumes every chain is bounded by its own timeout or the
user; **process death is the backstop**. There is no safe
thread-kill in Rust, so a grace period buys reporting, not
preemption; a true outside-kill belongs to process or WASM-guest
substrates (EXTENSIONS.md). Hooks are NOT isolated — they are quick
policy callables polled on the session's executor by contract; the
future guest runtime brings its own substrate. On wasm the sidecar
does not exist and bodies poll inline, cooperatively. Neither
reference isolates (pi awaits an uncooperative tool forever;
opencode's interrupt waits for a blocked tool to yield) — a JS
single thread forced their hand; Rust's real threads make the
guarantee cheap.

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
| don't continue after this batch | **post-tool `Stop` → the `terminating` flag** | no effect on the current batch — unstarted chains still run; the flag is fed only after `fold_all` commits, so the tool phase is flag-blind by construction. The loop top exits `run_failed(stopped)` and **discards the pending queue with notice** (the stop-semantics ruling, below). |
| don't run this call | **`Skip`** | in-band synthetic result; the model is told; siblings unaffected. |

The pre-tool `Stop` action is deleted (its niches compose from
`Skip` + abort), and the fail-fast machinery with it (`first_error`,
the start-gate flag, lowest-index-error selection,
drain-vs-drop): `run_single_tool` has no error path, and settlement
is unconditional — nothing exists that could strand a parked ask.

**Interaction — the ask pattern.** One hub (the actor's third shared
leaf beside the mailbox and abort): an ask registers a oneshot in the
pending map, emits `interaction_request` on the event channel,
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

**Background execution (ruled 2026-08; reserved — not in the first
release).** Provider APIs model tool calls synchronously — the next
request must carry results matching the turn's calls — so a call can
never stay open past settlement. Backgrounding, if it lands, rides
the sealed batch in-band: the body returns immediately with an **id
as its result** (durable, replayable; the roundtrip closes on time);
a **query tool** reads state/result from a session-scoped registry
(a `ToolContext` capability); and on completion the registry submits
a **user-role message carrying the result to the run-agnostic
mailbox** — a steer mid-run, a new run at idle (several completions
batch at the drain for free). The registry owns the detached task's
lifetime — the one sanctioned exception to drop-cancellation: not
the call's future, not the run-scoped token; process death kills the
work, and a post-restart query answers unknown-id honestly.
Out-of-band `tool_result`s (a call answered after its batch settled)
are prohibited by construction: settlement stays unconditional, the
roundtrip atomic, repair and cut points untouched. Injected
completion messages are ordinary `UserMessage` entries — durable,
replayable, branch-safe; a wire marker for styling is a frontend
refinement deferred to implementation.

Pause points are enumerable — only context-carrying sites can ask
(today: the tool-call gate by construction, the tool body via
`ToolContext`); other hook points gain the capability when a
consumer exists.

## The durable conversation (ruled 2026-08; the loop refactor)

Everything the model saw lands in the session log, one roundtrip at
a time, through the `ContextManager` (tabit-log) — the single owner
of the tree, the buffer handle, and the derived context. A
**roundtrip** is an assistant turn plus its complete tool batch:
`fold_all` verifies it whole (every call answered exactly once),
enqueues its records as one all-or-nothing batch, and grows the tree
in the same operation — all of it lands or none of it does, so a
file with tool calls but no results is unrepresentable by
construction.

**Commit sites:**

- **the drain folds** (the loop top): the opening batch and every
  steer commit as user nodes, in drain order;
- **the final fold**: a tool-free accepted turn commits at
  classification (empty finals fold nothing — one decision site);
- **the roundtrip** (`fold_all`): assistant + results, atomic;
- **side records**: session facts (`model_change`, `checkout`,
  `aborted`, …) enqueued by the session through its own shared
  buffer handle — order-significant, never part of the tree.

**The write buffer's one behavior** (the writer's contract): enqueue
a batch, all-or-nothing into the outbox, then attempt the write —
flush; on failure revert the file to its clean prefix and keep the
lines queued (every later enqueue retries them); on success drop the
lines. The `Err` is a report, not an undo. Degradation surfaces at
the run-entry guard (flag 8's second amendment). Usage facts are
deferred; discards record nothing (flags 25/27 parked).

**The load pass is one fold.** Opening a session parses the file once
— header, tree (with head), selection register, and the computed
cumulative stats (all branches; abandoned spend is still spend). The
context is *derived* on every read (`messages()`), never parsed,
never stored. Raw records are not retained. A torn tail or any
structural violation (a dangling roundtrip, a checkout to a
mid-roundtrip node, a broken parent link) fails loud at open — the
repair pass is deleted because atomic roundtrips made the shapes it
papered over unrepresentable, and a repair that survives its regime
hides real bugs.

**Checkout targets must be closed paths.** A checkout moves the head
to a node; the branch ending there must be roundtrip-closed (a user
message, a call-free assistant turn, or the last result of a complete
batch). A target inside an open roundtrip — an assistant whose calls
were never answered on that branch, a batch's interior result —
panics ("revisit later"; owner ruling, flag 23). `rewind(n)` targets
user messages and is unaffected.

## Turn-level stop semantics (ruled 2026-08; implemented by the loop)

A hook stop means: the current turn **finishes naturally** — it
streams to completion, commits, its tools execute, the results commit
(`fold_all`) — and the run does not loop into the next turn. The
`terminating` flag is settable only at that post-commit site, so the
loop-top check can fire only after a completed turn (at-least-one-turn
holds absolutely; pre-turn stops do not exist). On the stop exit the
pending queue is **discarded with notice** (`messages_discarded`),
never drained — the discard is what makes a stop final: the mailbox
keeps serving after a run failure, so a stopped run with a live queue
would otherwise bounce straight into a new run, defeating the stop.
Stops join abort as the only queue-discard sites.
