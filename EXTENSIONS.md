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

## Frontends are leaf consumers (standing)

Extensions target backend seams (hooks, tools, the interaction
capability). Frontend behavior — how cards render, when they close
(run terminals close everything), what "Always allow" persists
(nothing, today) — is protocol law (FRONTEND.md), not extension
surface. An extension influences what the user sees only through
the wire shapes above.
