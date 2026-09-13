# EXTENSIONS.md

The extension development record. **The substrate is ruled (2026-09,
below) and implemented through checklist task 4 — `crates/tabit-ext`:
discovery and the enablement gate, the initialize/ack handshake,
supervision, the death policy, the tool lane, the hook lane, the
skills tables; `crates/tabit-ext-sdk`: the guest dispatcher and the
example packages (the permission gate included — it lives here now,
not in core).** Host-service frames (task 5) and install (task 6)
land with their checklist tasks (ROADMAP item 9). Every entry names
the decision, where it is recorded, and what it implies for extension
authors. Entries record **existing design decisions**; nothing about
how a particular extension is written leaks in — the contract is the
protocol.

Rules of the ledger:

- one decision per entry, with its ruling date and its home document;
- a decision changes here when it changes there — this file points,
  it does not fork the truth;
- anything an extension author must not do (a boundary) is stated as
  a boundary, not a suggestion.

## Extensions are subprocesses over a frozen pipe (2026-09, the
item-9 substrate ruling)

Ruled: an extension is an opaque executable speaking a small frozen
JSONL protocol over stdin/stdout — the subagent substrate,
generalized. The host spawns the entry command at process start,
handshakes, and from then on tool calls, hook events, interaction
frames, and host-service requests cross the pipe. **Every tabit
process boots its own host — the frontend-attached backend and every
subagent child alike (ruled 2026-09: children pick up extensions;
the child's root inherits the parent's, crossing as
`--extensions`).** One dependency law for everything external:
frontends, subagents, and extensions are all leaf consumers across
process boundaries; none load into the host's process — that law
outlaws loading into a parent's process, not a child hosting its
own set.

Boundaries and reasons:

- Not in-process language runtimes — embedding V8/QuickJS/CPython
  contradicts the single-binary identity and adds a runtime to
  maintain forever (pi can, because pi is Node). Not WASM for v1 —
  recorded as the alternative with a felt-need trigger (real
  containment/sandboxing, or hot hooks where IPC latency matters).
- **The trust model is user consent, full stop.** An extension runs
  native code with the user's OS rights — the same trust class as
  `cargo install` or an npm CLI. Declarations are honesty for the
  user and gating at load, never containment.
- Crash isolation is the process boundary: a wedged extension is a
  dead child (the reaper pattern), never a stalled host. Hook events
  that cross the pipe pay an IPC roundtrip — local-pipe latency,
  noise against second-long tool calls; a hung extension during a
  hook is the reaper's concern, not the session's.
- **Calls run in parallel; the wire is a router pair per extension
  (ruled 2026-09).** The proxy (the tool adapter) enqueues its
  request on the extension's outbound lane — the writer serializes
  frames onto stdin, and sending is cheap — and awaits its result
  from the inbound side, tagged by `call_id`: the extension may run
  calls concurrently and return them in any order. A death answers
  every waiting adapter with the failure — no call hangs on a dead
  extension. Result deltas, when a consumer exists, ride the same
  inbound router.

## Packages mount by default; disabling is the settings act
(2026-09, task 4)

An installed package mounts unless its name is in `settings.toml`'s
`[extensions] disabled` list — the default is what a user who
installed the thing wants: install was the consent (task 6's trust
prompt is the first-load UX for packages that arrived by other
means). The layers union (the debug-override family:
`$TABIT_SETTINGS` replaces the user file): user
`~/.tabit/settings.toml` plus workspace `<cwd>/.tabit/settings.toml`
— any layer naming a package disables it; disabling is the one
explicit act, and there is deliberately no re-enable override.

What "disabled" means, precisely: the package is not launched, not in
the `extensions_available` catalog, its `providers.toml` fragment
does not merge, and its skills do not join the tables — absent
everywhere, and *silent* (the user's setting is not a failure;
`tabit extensions list`, task 6, is where disabled packages become
visible). A REFUSED package (bad manifest, failed handshake) still
reports as dead whatever the settings say — a broken package is
loud; a disabled one is quiet. Children (subagent processes) re-derive
the disable list from their inherited inputs — the same env, the same
workspace cwd, the same one code path the parent booted through —
never a forked child rule.

## Declaration: manifest for install facts, handshake for
capabilities (2026-09)

The manifest (`tabit.json`) carries install-time facts only — name,
version, entry command + args, one-line description. Capabilities are
declared live at the handshake (initialize → ack: tools with
name/description/schema, hook points) — the initialize/ack pattern
every tabit edge already uses. What the process serves is what it
declared; no schema file drifts. **The manifest is also the home for
any future host-required metadata (ruled 2026-09): when the host
needs a new install-time fact, it becomes a manifest field — not a
second config file. Refinements to the package shape (e.g. one
extension declaring multiple subprocesses) are deferred; v1 is one
entry command per package.** **Prompt contributions are not a v1
capability (ruled 2026-09): nothing consumes them, and the prompt
build phase may itself be refactored (a custom-prompt knob, richer
builders) — an extension mounting now could be thrown away with that
refactor. They join when the build-phase decision lands with a
consumer; the byte-stability law below already governs whatever
that future is.**

## Vocabulary grows additively; altering shapes is a version
boundary (2026-09, post-release topic deferred)

Changes that ADD vocabulary (new frame types, new optional fields)
keep past extensions working as-is — an old extension simply never
receives what it did not declare, and the host ignores what it does
not know. Changes that ALTER existing shapes (renaming, removing,
re-typing) are the compatibility boundary and bump the extension
protocol version, rejected at the ack. Until external extensions
exist, host and SDK version as one workspace — no skew is possible;
the full versioning story is a topic after the first release.

## Model-facing names are flat; identity is the pair (2026-09)

The model sees the declared tool name only — no prefix, no namespace
noise. The internal identity is *(extension id, tool name)*: the key
for usage accounting, the load-time conflict report, and the wire
catalog `extensions_available` (the `skills_available` family — each
loaded extension with its tools by provenance, so a frontend can
attribute without the model ever seeing a prefix; shipped skills
attribute by their original package paths in `skills_available`).

**One name, one tool, resolved at host assembly.** The host builds
the model-facing toolset as a name→tool map after all handshakes and
hands the engine a conflict-free set by construction — the engine's
duplicate-name shadowing never engages. **Handshakes run
concurrently; registration is ordered (ruled 2026-09): the assembly
iterates the scan's alphabetical order over a complete snapshot, not
completion order — so which tool wins or is refused on a collision
is deterministic regardless of which extension acked first** (the
scan sorts by directory; the directory is the identity). Conflict
policy (pi's rule):

- Extension vs. core, same name: **the extension replaces**, and the
  backend makes the replacement clear — a load-time report on the
  channel. How loudly a frontend presents it is the frontend's
  choice; the signal itself is mandatory.
- Extension vs. extension, same name: the newcomer is refused, naming
  the incumbent. No silent peer precedence — the user resolves by
  disabling one.

Sibling domains carry their own rules: skills merge last-wins-with-
warn per the discovery ladder (ROADMAP item 3); providers are
user-config-wins (below).

## Extension-shipped skills ride in-memory tables (2026-09, task 4)

The extension walker (the host's one scan) produces what packages
provide: each mounted package's `skills/` tree, its entries at their
**original package paths**. Those entries fold into the process's
skills catalog — the one table the prompt's listing, the confined
`skill` tool, and the `skills_available` snapshot all read — at the
ladder's **base**: below every user and workspace source, so anything
the user already has overrides them. Nothing is written to the
filesystem (a symlink layer would only move the walk, add external
error surface, and lose the package→skill attribution a disable list
needs). Provenance is by location: an entry's path IS the package
path, so `skills_available` attributes without a second source of
truth, and disabling a package is dropping its entries from the
table. Extension-vs-extension name collisions resolve in scan order
(first package wins, the duplicate warns) — the same determinism law
as tool registration.

## Hook forwarding: the pipe lane, and policy fails open (2026-09,
task 3)

Forwarded hooks are the tool lane's sibling: `hook { hook_id, event,
payload }` out, `hook_result { hook_id, decision }` back, v1
decisions `run`, `skip { message }`, `keep` (rewrites and stops are
engine actions that carry on no wire until a consumer asks). The
payload carries the session identity, the tool, the args (and the
presentation for `tool_result`); the session identity is what
per-session policy state keys on. Mid-hook asks ride the same
interaction lift (the correlation id is the hook's). Registrations
compose in scan order through `HookStack::merge` — one priority law.

**A failing hook is treated as absence; a failed tool call is the
model-visible failure.** Dead or broken resolve identically (ruled
2026-09): a hook whose extension died, errored, or panicked resolves
with the neutral decision for its point (run / keep) — crash
isolation: one broken package cannot brick the tool phase, and the
failure is reported loudly (the host's dead standing; the SDK's
stderr) — while a tool *execution* that dies or errors is the
call's failure, model-visible. The pair of resolutions is the
ruling; neither failure is silent.

## Install, distribution, package layout (2026-09)

`tabit install npm:<package> | git:<repo> | path:<dir>`:

- npm is the distribution substrate, accessed as plain registry HTTP
  (fetch metadata, fetch tarball, unpack) — no npm CLI, no Node at
  run time, no registry of our own. git shells to `git` (a machine
  running a coding agent has it). `path:` serves local development.
- Install places the package under `~/.tabit/extensions/<name>/` —
  and it mounts (packages mount by default, the entry above);
  `tabit extensions list/uninstall` manage it; update = reinstall.
- Pickup at the **next backend start** — no mid-run loading (the
  prompt byte-stability law; installing is the user's reload/cache
  decision). Same UX as pi's reload.
- A newly installed extension prompts for trust at first load (the
  pi project-trust family): the consent IS the containment.

The package layout tabit standardizes — everything else is the
package's business:

    ~/.tabit/extensions/<name>/
      tabit.json         # manifest: name, version, entry command
      providers.toml     # optional fragment, merged at config load
      skills/            # optional; symlinked into ~/.tabit/skills/<name>/
      frontend/<target>/ # optional; the named frontend's business

No language list, by design: the protocol is the contract, the
package declares its entry command (`node main.js`, `python main.py`,
a prebuilt binary), and every language that can write LF-JSON lines
qualifies. JS/TS will dominate in practice (the npm channel is
natural); compiled extensions arrive via the esbuild pattern
(per-platform prebuilt binaries as npm optional dependencies) when a
consumer exists.

## Provider contributions are catalog fragments, not API access
(2026-09)

An extension that adds a model provider ships a **local relay**
speaking a known wire format (openai-completions or anthropic — the
two engines tabit keeps) plus a `providers.toml` fragment pointing at
the port. Fragments are **merged at config load** (the boot scans the
enabled packages' directories — one scan shared with launch and the
skills mounts, so the consumers cannot disagree) — never copied into
the user's file; user config wins on id collision; uninstall removes
the provider by removing the directory. The example is shipped:
`lmstudio-ext`, a relay speaking **LM Studio's native REST API**
upstream (deliberately not LM Studio's OpenAI-compat endpoint — the
API nothing else in tabit speaks) behind its fragment, complete-only
upstream with the relay synthesizing the SSE stream, e2e-proven
against a scripted native double.

Merge mechanics, precisely: a fragment parses and validates as an
ordinary `providers.toml` (same rules — a broken fragment is
*refused*, warned, and never kills its package's tools and hooks);
**the user's own provider id wins on collision, silently** — an
override winning is exactly what a user with both configured expects,
not a warning; a fragment colliding with an *earlier fragment* warns
(scan order decides the incumbent — the user never chose that
collision); **a fragment cannot set `default_model`** (an extension
steering the default model is not the package's call — present ones
warn and are ignored). The API key, if the relay needs one, is the
user's to fill in like any provider.

The credential line is **attribution, not protection**: every model
call the host makes runs through the one registry (retry, caching
policy) and lands in the session's accounting. Extensions get
results, never credentials.

## Host services: request-response verbs on the extension pipe
(2026-09)

The reverse direction on the same pipe: the extension calls into the
core. One envelope (request id + verb + payload → response); each
service is a designed verb that joins when a consumer exists — the
cadence every seam uses. The interaction hub is service zero of this
shape (ask → response by id — it already ships).

Verb one: **`model_prompt`** — prompt content + a model ref (or the
session's), capped `max_tokens`, complete-only (no streaming over the
pipe for v1). Usage bills to the session **tagged with the extension
identity** — spend is visible and attributed (the auto-title shape:
this verb is what makes the attribution story real).

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
- **The extension pipe's lift (task 2) mirrors the engine's
  capability verbatim**: `interaction_request { call_id, id, ui_type,
  payload }` in, `interaction_response { id, outcome }` back
  (`outcome: null` is the dismissal) — `ui_type` + opaque payload,
  so extensions use the same `native:*` templates core tools do. The
  `call_id` routes to the session whose proxy call is executing; no
  capability on that call (a non-interactive session) answers
  dismissed — fail closed, exactly as core tools behave.
- The capability reaches sites through **contexts**: the tool body
  via `ToolContext`'s typed map (the `CancellationToken` precedent);
  the tool-call gate by hook construction. Other hook points gain
  context-carriage when a consumer exists — pause points stay
  enumerable (ENGINE.md lists them), and adding one is a design
  event, not a freedom.
- The capability type lives in rig-agent
  (`crates/rig-agent/src/tool/interaction.rs`) — one crate below the
  session layer, reachable by every hook and tool site. Dependency
  direction is architecture law, not license law, but it still points
  one way.

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

## The permission gate IS an extension package (2026-08 ruling,
executed 2026-09, checklist task 3)

The core shipped a basic permission gate only to test the
interaction path — an ask-set of exactly `bash`, "Always allow" as
session memory. With the hook lane landed, the gate **moved out of
the core into `gate-ext`** (`crates/tabit-ext-sdk/src/bin/gate.rs`):
the exact policy over the same seam, `permission.rs` deleted, no
core code knows a permission exists. The hub, the wire shapes, and
the capability are the permanent infrastructure the package
inherits. The package's "Always allow" memory keys on the hook
payload's session identity (one gate process serves every session —
without the key, one session's grant would leak into another); the
session-vs-user durability split stays deferred (v1 is session-only,
as the core gate was).

## Tool-call policy mounts through the hook surface (2026-08; seam
replaced by the hook-surface round the same month)

Ruled (the permission-leak review), then re-pointed when the
hook-surface round replaced the original gate factory with closure
registration: the core's interaction path is generic — it routes
responses by id and knows no asker's vocabulary or state. Tool-call
policy (the dev-time permission gate today) is **assembly-mounted**:
`SessionBuilder::hooks(HookStack)` (`crates/tabit-session/src/
session/builder.rs`) — the binary registers closure hooks
(`HookStack::hook(spec, on::tool_call(...))` is the pre-call gate
point; `on::tool_call` is the closure surface's one event point
today, the post-result point having no closure registration yet),
and the policy's state (session-scoped grants) is captured in the
closure at mount. The gate asks through the hook context's
interaction capability (`ctx.interaction()` — the same typed
capability tool bodies read). The core mounts whatever arrives
without naming a type. Deleting or replacing the dev-time gate is
deleting `permission.rs` and the one assembly mount
(`crates/tabit/src/main.rs`) — the same door any policy enters.

Implications:

- Policy registers through the hook surface; it never patches the
  session or the engine.
- Policy state (grants, denials) is the closure's own — captured,
  runtime-only (see the interaction-state entry below).
- A policy may ask through the hub or decide statically; skipping
  with an explanatory message is the in-band denial channel.

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
  themselves. **Extension tools have it better (2026-09 substrate
  ruling): their bodies live in the extension's own process, so the
  host's force-kill exists — the reaper's tree kill, the same shape
  the subagent leash uses.**
- Hooks are not isolated: hook closures are quick policy callables
  polled on the session's executor. A blocking hook stalls the
  session — this is contract, not oversight. (Hooks forwarded to an
  extension cross the pipe; a hung extension there is the reaper's
  concern, not the session's executor.)

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
- A NEW session builds from the current extension set: it has no
  cache to miss, so picking up new tools/prompts at creation breaks
  nothing. The seal protects BUILT sessions; creation is not a
  change.
- Changing an EXISTING session's prompt is the deliberate reload —
  today the respawn path. The reserved refinement is an in-process
  session reload (the backend rebuilds the chosen session's
  build-time inputs at the beat; history and transcript untouched):
  explicitly deferred until respawns actually annoy (PROTOCOL.md's
  startup & recovery ruling). Its command-path home already exists
  — the checkout pattern (session-addressed command, parked intent,
  beat execution) minus the rewind, with an outcome event instead
  of a replay pass; the mid-run question (abort-compose like
  checkout, or idle-held like `model`) belongs to its ruling.
- A session resumed after reload keeps its history and model (the
  log wins); only the preamble changes.

## Frontends are leaf consumers (standing)

Extensions target backend seams (hooks, tools, the interaction
capability). Frontend behavior — how cards render, when they close
(run terminals close everything), what "Always allow" persists
(nothing, today) — is protocol law (FRONTEND.md), not extension
surface. An extension influences what the user sees only through
the wire shapes above.
