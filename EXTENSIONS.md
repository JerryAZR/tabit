# EXTENSIONS.md

The extension development record. **No extension surface exists yet**
(ROADMAP item 9: WASM or script-based tool providers plus the existing
hook points). This file exists because design decisions are landing
now — in ENGINE.md, PROTOCOL.md, and the crates — that will shape how
extensions work when the surface arrives. Every entry names the
decision, where it is recorded, and what it implies for extension
authors. When the extension system is designed, this file is its
requirements input; entries graduate into the real extension docs.

Rules of the ledger:

- one decision per entry, with its ruling date and its home document;
- a decision changes here when it changes there — this file points,
  it does not fork the truth;
- anything an extension author must not do (a boundary) is stated as
  a boundary, not a suggestion.

## Interaction is the standard UI-event model (2026-08)

Ruled: the ask-pattern hub — many producers, one outbound queue (the
event channel), one inbound router (`interaction_response` by id to
the awaiting asker) — is **the** model for user interaction from
backend code. Recorded in ENGINE.md's tool-phase section and
PROTOCOL.md/FRONTEND.md §8.

Implications:

- An extension that implements a hook, or ships a tool, requests
  interaction through the same channels the core uses — no new
  plumbing, no second popup system. The wire shape
  (`interaction_request { id, title, body, options, free_text }` /
  `interaction_response { id, option?, text? }`) is generic on
  purpose: reuse it; do not invent new popup frames.
- The capability reaches sites through **contexts**: the tool body
  via `ToolContext`'s typed map (the `CancellationToken` precedent);
  the tool-call gate by hook construction. Other hook points gain
  context-carriage when a consumer exists — pause points stay
  enumerable (ENGINE.md lists them), and adding one is a design
  event, not a freedom.
- The capability type lives in tabit-session today. It moves down a
  crate only if a rig-level host needs it — dependency direction is
  architecture law, not license law, but it still points one way.

## Nothing may kill a batch (2026-08)

Ruled: extension hooks can never stop a tool batch mid-flight. The
three stop-shaped mechanisms, one each (ENGINE.md, stop taxonomy):

- **abort** — stop now. A hook constructed with the abort leaf may
  call it; semantics are identical to the user's stop button
  (`run_aborted`, queue discarded, synthesized interrupted results).
  This is the kill-switch surface for extensions.
- **post-tool `Stop`** — don't continue after this batch. The flag
  has no effect on the current batch (unstarted chains still run) and
  is fed only after the batch commits. This is the budget-cap /
  policy-cutoff surface.
- **`Skip`** — don't run this call, in-band. This is the
  per-call-deny surface.

The pre-tool `Stop` is deleted and the fail-fast machinery with it.
An extension that wants "fail-closed" semantics composes from the
table; settlement is unconditional by construction, so no extension
can introduce the stranded-question edge the old machinery carried.

## Turn-level stops never cut (2026-08)

Ruled: a hook stop lets the current turn finish naturally — it
commits, its tools execute, the results commit — and prevents the
loop into the next turn; the pending queue is discarded with notice
(`messages_discarded`), never drained into history. The design and
mechanism live in ENGINE.md's stop-semantics ruling
(pre-implementation).

Implications:

- A stop is turn-granular finality, never a cut: it cannot interrupt
  a stream, a turn, or a batch. For immediate preemption an extension
  holds the abort leaf — that remains the stop-now surface.
- Everything pending at the decision point comes back as
  `messages_discarded` drafts; what arrives after the decision starts
  the next run. The verb choice is the machine's, not the
  extension's.

## The permission system is a placeholder; extensions are its
replacement (2026-08)

Ruled: the core ships a basic permission gate only to test the
interaction path — an ask-set of exactly `bash`, "Always allow" as
session memory (never persisted, no config write-back). When the
extension system lands, the real permission system is an extension:
hooks over the same seams, exactly like `RecorderHook` is today (our
own components are first-party hook sets, not privileged). The
deletable surface is the policy; the hub, the wire shapes, and the
capability are permanent infrastructure the extension inherits.

## Tool-call policy mounts through the tool-gate seam (2026-08)

Ruled (the permission-leak review): the core's interaction path is
generic— it routes responses by id and knows no asker's vocabulary
or state. Tool-call policy (the dev-time permission gate today, the
real permission system later) is **assembly-mounted**:
`SessionBuilder::tool_gate(factory)` builds the gate per run with the
session's interaction hub; the binary provides the factory, a captured
memory makes policy state session-scoped, and the core mounts
whatever arrives beside the recorder without naming a type
(`gate.rs`— `ToolGate` is dyn-compatible because the engine's
`AgentHook` is not). Deleting the dev-time gate before release is
deleting `permission.rs` and the one assembly mount— through the
same door the real system enters.

Implications:

- An extension providing tool-call policy implements `ToolGate` and is
  mounted by the assembly; it never patches the session or the engine
  hook chain.
- Policy state (grants, denials) is the gate's own— held in the
  factory's captured memory, runtime-only (see the interaction-state
  entry below).
- The gate may ask through the hub or decide statically; skipping with
  an explanatory message is the in-band denial channel.

## Interaction state is runtime-only (2026-08)

Ruled: interaction requests never persist and never replay. The
durable record of an interaction is the **tool result** — the answer
or denial the model saw. Extensions building multi-step user flows
must encode durable state in tool results (the model-visible,
replayable channel), not in card state. A restart mid-flow
synthesizes interrupted results and the flow re-derives from history.

## Answers address a question; steers address the model (2026-08)

Two channels that must not be conflated: an interaction answer
resolves one pending request (routed by id to one asker); a steer
(`message`) joins history at the next turn boundary for the model.
A free-text denial reason rides the answer (it becomes the denial
the model sees); it is not a steer. Extensions with user-facing
input must pick the channel by what the input addresses.

## Tool cancellation is inherited, not extended (standing)

The tabit-tools cancellation contract — engine owns *when*, tool
owns *how*, drop-safety required — governs extension tools
unchanged. An extension tool that parks on an interaction is parked
inside its own future: drop is the cancellation, exactly as for
`bash`. Extension tools must be drop-safe under the same contract.

## Tool bodies never stall the harness (2026-08)

Ruled and shipped: tool bodies poll on an isolated sidecar runtime,
never on the session's executor — harness responsiveness (abort,
interaction routing, sibling chains) is structural and does not
depend on tool-body behavior. Home: ENGINE.md's tool phase.

Implications:

- An extension tool may block or misbehave; it can leak a sidecar
  task but cannot stall the session. Cancellation is token-and-
  detach: the token is the ask, bounded bodies are the expectation,
  process death is the backstop. There is no force-kill for native
  in-process tools — write bodies that observe the token or bound
  themselves.
- Hooks are not isolated: hook closures are quick policy callables
  polled on the session's executor. A blocking hook stalls the
  session — this is contract, not oversight.
- The substrate choice matters for the extension-format decision:
  WASM guests are the only truly preemptible tool runtime (epoch
  interruption gives a real grace-then-kill); native-loaded
  extensions inherit the cooperative ceiling above.

## Background tools stay in-band (2026-08)

Ruled (reserved — not in the first release): a tool that backgrounds
work returns an id immediately as its result; the real result reaches
the model through a query tool and/or a user-role message submitted
to the mailbox on completion. A call never stays open past
settlement — provider APIs require matching results on the next
request. Home: ENGINE.md's tool phase.

Implications:

- The background registry is session-scoped (a `ToolContext`
  capability or a construction-captured `Arc`); the detached task is
  owned by the registry, not the call's future — the one sanctioned
  exception to the drop-cancellation contract.
- Completion injections are ordinary user-role messages. Do not
  invent a new frame or a late `tool_result` channel; the sealed
  batch is not negotiable.

## Prompt changes are the user's cache decision (2026-08)

Ruled: the system prompt stays byte-stable for a built session's
life. An extension's prompt contribution mounts at session build —
the same extension mount and seal as tools and hooks (ENGINE.md's
hook-surface ruling) — and there is no per-turn rebuild. (pi
rebuilds the prompt per turn so extension tool-appends take effect;
tabit trades that away deliberately: a silent mid-run cache
invalidation is a cost nobody chose.) No mid-run extension loading
is planned.

Changing the prompt is a deliberate user action with a known cost:
install/configure the extension, let the current task finish
(compact if wanted), then reload — the GUI respawns the backend,
which re-reads config, auth, and sessions (PROTOCOL.md's startup &
recovery ruling), and replay restores the transcript with the same
ids. The cache miss lands where the user chose it.

Implications:

- Prompt contributions hoist into the preamble at build;
  mid-conversation system messages stay unsupported by design.
- Extensions load at process boot, like config — a mid-process
  `new_session` does not pick up a newly installed extension;
  reload is the one pickup path.
- A session resumed after reload keeps its history and model (the
  log wins); only the preamble changes.

## Frontends are leaf consumers (standing)

Extensions target backend seams (hooks, tools, the interaction
capability). Frontend behavior — how cards render, when they close
(run terminals close everything), what "Always allow" persists
(nothing, today) — is protocol law (FRONTEND.md), not extension
surface. An extension influences what the user sees only through
the wire shapes above.
