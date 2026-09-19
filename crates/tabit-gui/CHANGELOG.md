# Frontend protocol changelog

What moved in the frontend protocol, newest first — the companion to
FRONTEND.md (the frozen contract). Read FRONTEND.md to build a
frontend; read this to keep one current.

Two entry kinds, because the contract is both the wire and the
expectations around it (the ISA rule: an instruction set is also the
software/hardware contract, not just the encodings):

- **wire** — a shape changed: events, commands, handshake frames,
  payload schemas. Parse-level; an old frontend may fail to compile or
  misread.
- **behavior** — no shape changed, but what a frontend may *assume*
  did (ordering, lifecycle, durability). An old frontend keeps
  parsing and quietly builds on a dead assumption.

Every `PROTOCOL_VERSION` bump — and every additive change a frontend
could observe — gets an entry here in the same commit.

## v14 (current)

### wire: `compaction_finished` carries the pass's facts (2026-09)

Protocol version 14. Compaction is a request — its spend is spend
like any turn's, and its result is a length a frontend wants to
meter. `compaction_finished` gains `usage` (the summarization
request's report; multi-pass runs emit one bracket per pass, each
carrying its own), `cost` (the recorded dollars — same invoice
ruling as `completion_call.cost`), and `tokens_after` (the
post-compaction context length: the summary's output tokens plus the
retained tail's delta sum — the base the next turn starts from). The
replay marker projects the same values from the session file's entry.
Backend fix riding along: the live ledger now bills the summarization
spend at commit — previously only the parser billed it on reload, so
live stats undercounted compaction until the session was reopened
(pinned live-vs-reload in the endpoint test).

## v13

### wire: `completion_call` carries the recorded cost (2026-09)

Protocol version 13, the invoice ruling: the dollars a turn cost are a
fact stamped at commit (rates in effect × the provider's report), not
something to re-derive at read. `completion_call` gains `cost`
(optional; absent when the provider reported nothing or the model has
no rate card). Consequences: a frontend's session-total dollars are
sums of these recorded values — after a rate change and a resume, the
history still shows what was actually spent (replay carries the same
recorded value from the session file); `model_changed.cost`'s rates
are the **current** card, for what future turns will cost. The session
file format moved to 6.1 (same major): `assistant_message` and
`compaction` entries carry the stamp, so the backend's own stats
(print mode's epilogue, closing stats) report recorded dollars instead
of re-deriving from today's rates. Backend-side, one injected resolver
(tabit-session computes, tabit-log stamps — the log layer stays
config-free) feeds the entry stamp, the ledger, and the wire event.

## v12

### wire: per-turn usage is complete; run-level aggregation deleted (2026-09)

Protocol version 12. Usage's home is the fresh server report:
`completion_call` now carries the full five-field `Usage` (input,
output, total, cached-read, cache-write — the cache legs were missing,
so client-side per-turn cost was incomputable for cached providers).
`run_finished`'s aggregated `usage` is **gone** — summing turn usages
within a run is bookkeeping the display side does over the per-turn
events (owner ruling: per-turn is the natural home; the run-end report
would need extra bookkeeping). With `model_changed.cost`'s rates (v11),
per-turn and per-session dollars are computable at display. Side
effect worth knowing: sums over turns now count aborted and failed
runs — the old run-terminal sum silently dropped them (the GUI's
session total migrated and no longer undercounts). Backend internals
followed the ruling: `RunSummary` and the drive loop no longer
aggregate usage at all; the stats ledger (log-derived, feeds print
mode and closing stats) is unchanged.

## v11

### wire: `model_changed` carries the resolved model facts (2026-09)

Protocol version 11. The register announcement now resolves the model
record against config and carries what it finds: `context_window`
(tokens — a context meter's denominator), `name` (the configured
display name; fall back to the model id), and `cost`
(`{ input, output, cache_read, cache_write }`, USD per million
tokens). All three are optional and absent means the config does not
state it — never zero, and never an error: a register left stale by a
config edit announces the ids with no facts, and the next validated
switch repairs it. Owner ruling: one version bump carried the whole
record (max_tokens deliberately stays off — no reported need). Both
doors announce the same shape: the replay pass's lead (boot, open,
re-replay, checkout) and the `model` command's outcome. The GUI does
not render the facts yet; the picker still follows the ids.

## v10

### behavior: tool results carry `details` off the model path (2026-09)

The tool-result data model is now pi's shape (owner ruling):
`content` is what the model sees — text/media the tool pre-formatted
itself — and `details` is bookkeeping for the frontend and hooks.
Concretely: detail-bearing tools (edit, bash overflow, subagent) no
longer deliver the JSON cargo as a second content block stringified
into the model's transcript; it rides the result's `details` field
(frontend-visible exactly as before — `tool_result.details` on the
wire is unchanged, no protocol bump). The GUI's `content` rendering
no longer shows a JSON tail for those tools.

### behavior: `subagent` details slimmed to the pairing fact (2026-09)

The subagent tool's result cargo is now
`{ "child_id", "outcome" }` — `turns` and `usage` are gone (owner
ruling: bookkeeping the model-side flow has no use for). The pairing
contract is unchanged: `child_id` is still the child's session id,
matching the `session_opened` announce's `id`/`parent_call` link. A
frontend that displayed turns/usage per delegation now finds them
absent; nothing else moved (TOOLS.md updated in the same commit).

### wire: `session_created` deleted — one announcement shape (2026-09)

Protocol version 10. The v4 interim is gone (its "kept one version,
then deleted" window ran out five versions ago; the review round's
ruling executed it): `new_session` now announces through the stamped
`session_opened { resumed: false }` — the same shape the boot,
`open_session`, and subagent children already used. Consequences for
a frontend: **delete your `session_created` handler** (the GUI
reducer's new-session fold — row + view switch — moved into its
`session_opened` arm, conditioned on "not the boot, not a listed
session"); the new session's selection notes now **follow** the
announce on its stream (`new_session`'s emission order unified with
`open_session`'s — announce, then notes); and the fresh-start note
("no sessions to resume") is the **boot announce's** fact, not every
`resumed: false` announce's.

### wire: `run_failed` typed; brackets carry Unix-ms timestamps (2026-09)

Same bump, two additive moves (codex's shapes, the review ruling):
`run_failed` gains `kind` — an open string by the `error`-kind law,
well-known values `provider` (the provider stream errored mid-run),
`model` (the run could not open; retry needs a model switch),
`persist` (the log refused to flush; the run never began), `engine`
(the internal residual, incl. a subagent child dying) — retry-vs-fatal
decisions branch on it instead of matching message text. The turn
brackets (`turn_started.started_at_ms`,
`turn_committed.completed_at_ms`) and the run terminals
(`started_at_ms`/`completed_at_ms`) carry Unix-millisecond stamps —
live runs stamp at emission, replay stamps from the entry's recorded
time (a replayed turn's two stamps coincide). The old §11 open
question is settled by this. The reducer ignores the fields today
(the redesign owns the rendering).

## v9

### wire: `extensions_available` — the extension catalog at startup (2026-09)

Protocol version 9. Extension tools ship (ROADMAP item 9, task 2):
installed packages (subprocesses over the frozen JSONL pipe) declare
tools at the handshake; the backend assembles them into the
model-facing toolset — flat names, one name one tool, an extension
tool **replacing** a core tool of the same name (reported, the signal
is mandatory) and a peer collision refusing the newcomer (the
incumbent named). The frontend surface is one new event:
`extensions_available { extensions: [{ name, version, description?,
dir, status, reason?, tools, hooks }], conflicts: [{ kind,
extension, tool, incumbent? }] }` — unstamped, backend-level,
announced once right after `skills_available`, only when discovery
found at least one extension (a refused package counts as
discovered: it reports as `dead` with its reason). Extension tool
invocation is no new shape: an ordinary `tool_call`/`tool_result`
pair attributed by the model-facing name. The reducer marks the seam
(same interim as the skills catalog; the redesign worktree owns the
extension surface).

### wire: `extensions_available` `skills`/`providers` fields — added then removed (2026-09, same week)

Task 4 briefly added per-extension `skills`/`providers` arrays to the
catalog entry; both were removed before any release. Skills attribute
by their original package paths in `skills_available` (in-memory
tables, provenance by location), and provider fragments have no
frontend consumer. v9's shape is as documented above; the reducer's
no-op arm never noticed either way.

## v8

### wire: `skills_available` — the skills catalog at startup (2026-09)

Protocol version 8. Skills ship (ROADMAP item 3): the four-source
discovery (home `~/.agents/skills` → `~/.tabit/skills`, workspace
`.agents/skills` → `.tabit/skills`, merge with override on name
collision), the prompt catalog, and the confined `skill` tool. The
frontend surface is one new event: `skills_available { skills: [{
name, description, location, level }] }` — unstamped, backend-level,
announced once right after `sessions_available`, only when discovery
found at least one skill. A new event kind is parse-breaking for
frontends compiled against the shared protocol crate (an unknown
variant fails the enum parse), which is why the version moved where
the `parent_call` field did not. Skill invocation is no new shape:
the model calls the `skill` tool, an ordinary `tool_call`/
`tool_result` pair. Master's reducer marks the seam (the redesign
worktree owns the real panel).

## v7

### wire: `session_opened.parent_call` — the subagent pairing (2026-09, additive; no version bump)

A subagent child's announce now carries `parent_call`: the spawning
tool call's `internal_call_id`, pairing the child with the exact open
`tool_call` event a frontend already holds — exact under concurrent
subagent calls, where arrival order cannot disambiguate (previously
the pairing was inferable only at result time, via the `subagent`
result's `details.child_id`). Absent whenever `parent` is; old
frontends ignore the field. The one enabling engine change: a
model-turn dispatch now hands tool bodies their correlation id as
typed context (`rig_agent::tool::InternalCallId`) — absent for
executions outside a model turn.

### docs: the contract split — TOOLS.md (2026-09)

The per-tool interpretation layer moved out of FRONTEND.md into
TOOLS.md: every built-in tool's `tool_result.details` shape (edit's
diff + outcomes, bash's truncation/spill, subagent's child-session
facts) and the interaction template payload schemas (the two
`native:*` widgets and their askers). Content moved, not changed —
FRONTEND.md stays the mechanics contract and points across.

### wire: the compaction bracket and the `compact` command (2026-09)

Protocol version 7. Compaction (ROADMAP item 6) ships its frontend
surface: the `compact { session, directives? }` command (the manual
door — forced compaction; idle runs it at the beat, running parks it)
and the event bracket
`compaction_started { id, pass }` → `compaction_delta { id, text }`
(streamed summary) → `compaction_finished { id }`, with
`compaction_failed { id, message }` for a failed or cancelled pass.
The bracket `id` is the pass's eventual compaction-entry id. Not a
run terminal — a mid-run bracket simply lands between turns. A
replayed session renders `compaction_finished` markers at each
boundary; history before a compaction still replays (the file never
deletes). Master's reducer no-ops the bracket (the redesign worktree
owns the real rendering); FRONTEND.md §5/§6 carry the contract.

### behavior: context compaction is live (2026-09)

Two automatic doors now run the same machinery with no frontend
input: after a run ends at idle (condition A: over 75% of the
configured `context_window` with an empty mailbox) and before any
request that would exceed `window − 32K` (condition B — a mid-run
bracket is this door firing). An overflow rejection mid-run also
compacts (the error itself teaches the window) and the conversation
retries — visible as `run_failed` followed by a fresh run, with the
bracket in between. Models without a configured `context_window`
skip the automatic doors (overflow recovery still works); set it in
providers.toml to enable them.

## v6

### wire: the interaction event's tag is `interaction_request` (2026-09)

Protocol version 6. A contract-alignment fix found by the TUI spike:
the event variant was named `InteractionRequested`, so the derived
wire tag was **`interaction_requested`** while FRONTEND.md §6/§8 —
the frozen contract — documents `interaction_request`. The variant is
renamed and the wire now emits the contract's name. Typed clients
(the GUI) recompiled without noticing — matched pairs never see the
tag; hand-rolled clients parsing per FRONTEND.md were right all
along.

### behavior: children are command-addressable; abort stops subtrees (2026-09)

Same protocol version. With the subprocess substrate (details in
ROADMAP item 5 and PROTOCOL.md flag 33), a subagent child is a full
session host in its own process — every session command works on a
child structurally, forwarded as one wire line: `message` steers a
live child (the `message_queued` ack on the child's own stream — a
subagent view is a steerable view), `interaction_response` answers
its cards, `checkout`/`model`/`continue` consume as on any session.
Abort is a **subtree stop**: aborting a session stops its in-flight
descendants — every descendant's `run_aborted` flushes on its own
stream; instances are never destroyed; aborting a child by id leaves
the parent's run alive. (The cascade rides the run token each tool
already holds — no framework walk; children with no active tool call
survive, the background model's basis.)

## v5

### wire: `session_opened.parent` — subagent children announce through the same door (2026-09)

Protocol version 5. `session_opened` gains an optional `parent`
(the spawning session's id), present only for subagent children —
one "session became visible" shape stays the truth for every path.
Two more facts ride along: `path` is **empty for an ephemeral
session** (subagent children are in-memory only today — nothing to
open or replay), and a child's whole run streams on **its own stream
stamp** (`user_message`, deltas, `tool_call`s, terminal) while the
spawner's transcript carries only the subagent `tool_call` /
`tool_result` pair. The tool result's `details` for
`name == "subagent"` carry `{ child_id, outcome, turns, usage }`.
*Migration:* the reducer now branches on `parent` — a child
announcement must never overwrite the active session's Facts (the
bridge is in; a nested-transcript view is future GUI work, and child
streams drop like any unknown background stream until then). The ack
carries `protocol_version: 5`. The full event walkthrough — safe and
unsafe assumptions, the `details` shape, abort behavior — is
`SUBAGENTS.md` in this crate.

## v4


### wire: the shell tools are `details` producers; shell cap drops to 16 KiB (2026-09)

Second `details` producer: a truncated `bash` / `powershell`
`tool_result` now carries
`{ truncated, output_lines, total_lines, omitted_lines, total_bytes, spill_path }`
alongside the faithful-copy `content` (which still ends with the
`Full output: <path>` notice — frontends without details support
degrade to it). `spill_path` is the whole contract: the frontend reads
or displays that file itself; it lives in the backend machine's temp
dir and is never deleted by us. Details appear only when the output
truncated. The shell output cap also tightens from 50 KiB to 16 KiB
(read keeps 50) — legitimate command output past that is mostly noise,
and the full text survives in the spill file.
*Migration:* the truncated-output card dispatches on
`name == "bash" | "powershell"`; offer the spill file via
`details.spill_path` when present, fall back to the `content` notice
when absent.

### wire: `tool_result.details` — presentation cargo (2026-09, 8c6ca84)

`tool_result` gains an optional `details` object: derived, structured
facts computed where the file is, dispatched on the event's existing
`name` field (no discriminator inside). `content` stays the faithful
copy — exactly what the model saw; `details` never duplicates prose,
it structures the same facts. First producer: the edit tool — a
unified diff (`similar`'s change model: hunks of
context/removed/added lines with old/new start+count) plus per-edit
accept/reject outcomes with reasons. Absent or unknown `details`
degrades to `content` rendering. A per-tool event taxonomy (a
`tool_diff` event) was explicitly rejected as non-scaling.
*Migration:* fixtures constructing `SessionEvent::ToolResult` gain
`details: None` (or a real details value to test the rich path). The
diff card dispatches on `name == "edit"`.

### wire: `session_opened`; the ack shrinks to protocol facts (2026-09, 00c8e40)

Every session becoming visible — the boot (at spawn), a
`new_session`, an `open_session` — is announced by one
`session_opened { id, path, model, resumed }` event, stamped with the
session's own stream. `initialize_ack` drops `session_path`, `model`,
and `resumed`; it now carries only `protocol_version` and
`session_id` (the boot id, needed to address commands). The boot
session is no longer a special case — one "session became visible"
handler serves every path. `session_created` is superseded (kept one
version for in-flight frontends, then deleted; new sessions are
announced with `resumed: false`).
*Migration:* stop reading session facts from the ack; fold
`session_opened` into the same state those facts fed (Facts, the
status strip). The fresh-start note (`resumed: false` after
`--continue`) moves with it.

### wire: `select_one` / `select_any` replace `confirm` and `ask` (2026-09, b4e55b9)

Two widgets cover the native interaction surface; `native:confirm`
and `native:ask` are deleted (keeping them named would imply a
special UI that does not exist). `native:select_one` — given
choices, select exactly one, optional free text (the permission
gate's allow/always/deny is this). `native:select_any` — select zero
or more, optional free text; **zero options given is the old
free-text ask**. Both share one answer shape:
`{ selected: [label, ...], text? }` — exactly one label for
select_one. Request shapes: `{ title, body, options: [{ label,
description? }], free_text }`.
*Migration:* `ui::CONFIRM` → `ui::SELECT_ONE`, `ui::ASK` →
`ui::SELECT_ANY`; answers move from `{ option, text? }` to
`{ selected: [label], text? }`. A select_any card may carry options
— the old ask-card rendering (free text only) must grow an option
list.

### behavior: sessions with no user message never touch disk (2026-09, 1c70113 + ebf5b8c)

A session file materializes only when a user message is enqueued —
opening a session, changing its model any number of times, and
closing it leaves nothing behind (previously the clean-exit flush
wrote a header-only file). The catalog consequence: an
opened-then-closed empty session never appears in
`sessions_available` at all. Model switches on a fresh session are
live in the register immediately but reach the file only with the
first user message's commit.
*Migration:* none required — but any frontend workaround for
header-only orphans (filtering empty sessions from the switcher)
can be deleted.
