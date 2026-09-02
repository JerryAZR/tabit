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

## v5 (current)

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
carries `protocol_version: 5`.

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
