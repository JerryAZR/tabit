# FRONTEND.md — the tabit frontend contract

This is the contract for anyone building a UI on top of the tabit
backend: what the backend provides, what it expects from you, and the
invariants your UI can rely on. Read this document alone; you should
not need the codebase to design a frontend.

This doc owns the **mechanics** — wire format, lifecycle, the event
vocabulary's semantics, invariants. The interpretation layer — how to
render a specific tool's `details` cargo, the interaction template
payloads — lives in **TOOLS.md**, its companion since the shapes grew
past one doc (2026-09 ruling).

Wire shapes below are the **v16 contract**. v3 was the multi-session
host — session-addressed commands, `new_session`/`open_session` on the
channel, the `"main"` stream alias retired (the stream stamp is the
session id). v4 made backend-level frames **unstamped** (§6) and
generalized interaction requests to `ui_type` + opaque payloads (§8).
v7 shipped compaction (§5/§6); v8 added the `skills_available`
startup announcement (§6); v9 added `extensions_available` — the
extension catalog with provenance and the load-time conflict reports
(§6); v10 deleted the `session_created` interim (`session_opened`
with `resumed: false` is the one announcement), typed `run_failed`
with a `kind`, and put Unix-ms timestamps on the turn brackets and
run terminals; v11 put the resolved model facts on `model_changed`
(`context_window`, `name`, `cost` — optional, absent means the config
does not state one); v12 made per-turn usage complete
(`completion_call` carries the full five-field `Usage`) and deleted
`run_finished`'s aggregated usage — per-turn is the home, sums are
the frontend's; v13 put the recorded dollars on `completion_call`
(`cost` — stamped at commit from the rates in effect, so history
survives rate changes); v14 put the compaction pass's facts on
`compaction_finished` (usage, cost, `tokens_after`); v15 reshaped the family
into the invocation envelope — `compaction_begin` → `compaction_step` × N →
`compaction_end`/`compaction_failed`, with `compaction_retried` for discarded
attempts; v16 made the session's world visible — `session_opened` and the
catalog rows carry `cwd` (rows also `path`), and `compact.directives` became
real free text (appended to the summarization instruction for that
invocation). Each version landed as one
protocol-version bump with no compatibility period; always check the
ack's `protocol_version`. (`tabit-core --list` prints a human table —
there is no JSON listing edge.)

## 1. Architecture: two processes, one pipe

You spawn the backend; you never link it as a library.

```
tabit-core --json [--continue | --session <path>] [--model <ref>]
```

- **stdout** carries protocol lines to you; **stdin** takes protocol
  lines from you; **stderr** carries human-oriented diagnostics you may
  ignore (never protocol data) — but capture it for crash reports.
- **One backend process hosts many sessions.** The spawn flags select
  the **boot session** — `--continue` resumes the project's newest
  (nothing to resume → fresh, ack `resumed: false` — §3.1),
  `--session <path>` a specific file, neither a fresh one — and the
  backend announces the catalog (`sessions_available`) after the
  handshake. Creating, listing, opening, and switching sessions are
  channel commands (`new_session`, `open_session`; §5) — never process
  tricks. One connection per backend process (ruled scope).
- **Spawn environment.** Sessions live at `<cwd>/.tabit/sessions`
  — the directory the backend was started in; there is no
  project-root discovery (do not assume a git repo). Spawn the
  backend in the project directory. `--model <ref>` is `provider/model` or a bare model id
  when unambiguous; `--max-turns <n>` also exists (and applies to
  sessions created later in the same process). The backend binary is
  `tabit-core` — installed alongside the frontend (a sibling binary),
  so "can't find the backend" is not a failure mode in the supported
  flow (`TABIT_CORE_BIN` remains a development override).
- **Local or remote, same edge.** Locally the backend is a child
  process; remotely it is the same child spawned on the far side of
  `ssh` with stdio forwarded. Nothing in the protocol distinguishes
  the two.
- **Crash isolation is the point.** Internal backend errors panic by
  design — the process dies loudly rather than running broken. Your UI
  must survive that (see §3 and §10).

### Responsibilities (who owns what)

- **The backend owns all conversation truth**: sessions (creation,
  loading, the catalog), the log and its tree, replay, run state,
  queueing, ids, model selection. A frontend is a projection plus
  input routing — every projection is rebuildable from a replay pass.
- **The frontend owns the process lifecycle, for recovery only**:
  spawning the backend, classifying death, explaining it, and the
  user-triggered respawn (which re-reads config). The backend never
  respawns itself; the frontend never manages sessions through the
  process (spawning or killing backends to create or switch) — session
  lifecycle is command-driven on the channel.
- **The transport owns ordering**: ack-before-events, one ordered
  stream per connection; frontends never reorder frames.

## 2. Wire format

Every line in both directions is one UTF-8 JSON object, LF-terminated.
Discrimination is by `type` tag; no JSON-RPC envelope, no request ids,
no command responses — commands are fire-and-forget and their outcomes
arrive as events. Input tolerance: blank lines are skipped, a trailing
`\r` is trimmed (CRLF-safe). Output is strict LF. There is **no line
size limit** — tool output can be large; buffer accordingly.

```
→ {"type":"initialize","protocol_version":15,"replay":true}
← {"type":"initialize_ack","protocol_version":15,"session_id":"019…"}
← {"type":"session_opened","stream":"019…","id":"019…","path":"…",
   "model":{"provider":"…","model":"…","thinking_level":null},"resumed":true}
← {"type":"sessions_available","sessions":[
     {"id":"019…","created_at":"2026-08-22T…","entry_count":14}, … ]}
← {"type":"model_changed","stream":"019…","provider":"…","model":"…","thinking_level":null,"context_window":200000,"name":"…","cost":{"input":1.0,"output":4.0,"cache_read":0.1,"cache_write":0.4}}
← {"type":"replay_started","stream":"019…","total":14}
← … the transcript as finalized events …
← {"type":"replay_done","stream":"019…"}
→ {"type":"message","session":"019…","text":"who are you?"}
← {"type":"user_message","stream":"019…","entry_id":"019…","text":"who are you?"}
← {"type":"turn_started","stream":"019…","id":"019…","started_at_ms":1763312345678}
← {"type":"text_delta","stream":"019…","turn_id":"019…","text":"I'm "}
← {"type":"text_delta","stream":"019…","turn_id":"019…","text":"tabit."}
← {"type":"turn_committed","stream":"019…","id":"019…","completed_at_ms":1763312347890}
← {"type":"run_finished","stream":"019…","output":"I'm tabit.","durable":true,"started_at_ms":1763312345000,"completed_at_ms":1763312348000}
```

The example's send lands while the session is idle (after
`replay_done`), so it is acknowledged directly by `user_message` — no
`message_queued` exists for idle sends (§5); a send while a run is
live is the queued case.

Every event frame is **flat**: the event's `type` and payload fields
sit next to `stream`. The `stream` stamp is the **session id** that
produced the event (the boot session's id is in the ack) — the
`"main"` alias is gone. Events from several open sessions interleave
on the connection; attribute by stamp. A frame with **no `stream`** is
a backend-level fact (§6 — the catalog, session
errors): fold it connection-level, never session-attributed. **Route
frames by stamp** (render the sessions you are viewing, ignore the
rest) and **skip unknown event `type`s** — both are
forward-compatibility paths (subagent streams and new events arrive
later without a version bump).

**On unknown events and streams: report, don't swallow.** Skipping is
the wire rule (never fail, never render); silently *discarding* is a
debugging trap — a frontend older than its backend loses features
with no trace. Log the raw line (or surface a quiet "unsupported
frame" indicator) when you meet an unknown `type` or `stream`. Two
strategies: clients built against the `tabit-protocol` crate recompile
with every protocol change — parse failures are loud by design, and
the handshake's version gate means a matched pair never exchanges
unknown frames; hand-rolled clients should parse leniently (to a JSON
value, switch on `type` when recognized) and log the rest.

## 3. Handshake, lifecycle, exit codes

1. Your **first line** must be
   `initialize { protocol_version, replay? }` (`replay` defaults to
   `false`). Match → `initialize_ack` with **protocol-level facts
   only** (the version and the boot session's id — everything else
   arrives by event, 2026-09 ruling: the boot session is announced
   exactly like every other). The next frame is `session_opened`
   with the boot's facts (id, path, active model, `resumed`), then
   the session catalog, then the skills catalog (v8 — only when
   discovery found something), then — if you asked — the replay
   pass, then live traffic. `resumed: false` after you asked the
   backend to resume means the store was empty and the backend
   **started fresh — an absorbed miss, not an error**; show a small
   note. Mismatch → `initialize_rejected { reason }` and the
   process exits 1. A second `initialize` after a successful handshake
   gets `protocol_error`; the connection stays open. Rejection
   reasons come in two flavors: config/auth problems carry the
   first-run setup guide (written for the user — display it);
   everything else (session unreadable, model unbuildable) carries a
   plain reason — do not treat it as a config problem.
2. A command before `initialize`, an unparseable line, or an
   empty/whitespace-only `message` text gets `protocol_error
   { message }`; **the connection stays open**. `message` texts are
   free-form otherwise (multi-line is fine — the wire is line-delimited
   JSON, and JSON escapes embedded newlines).
3. `protocol_error` / `initialize_rejected` reasons are free text for
   humans — display them, never branch on them.
4. To shut down: **close stdin**. Closing stdin is frontend death
   (ruled 2026-08 — the core dies with the frontend, regardless of
   state): an in-flight run is **aborted** (its `run_aborted` terminal
   still flushes before the stream ends), queued messages are
   discarded, and the backend exits. Interrupted results synthesize
   on the next open, exactly like a crash; the log stays durable.
5. **Exit codes: 101 is the one reliable crash signal.** `1` means
   handshake rejection (including **first-run setup failures** — no
   config file: the backend sends `initialize_rejected` whose reason
   carries a setup guide, then exits; display the reason, it is written
   for the user — and recovery is manual: the user fixes the file and
   the frontend respawns the backend; config is not re-read per request
   by design) or a pre-handshake exit with **no frames** — bad flags
   only (stderr message; every session/model startup failure arrives
   as a rejection frame instead, §3.1). `101` is an **internal
   error**: the process crashed itself
   — a panic in any task or thread ends the process, so a crashed
   backend never lingers as a zombie. Display the stderr report and
   ask the user to send it back. `0` covers one non-clean end: a broken
   pipe. Otherwise **detect crashes as EOF without a terminal event
   for the in-flight run**; capture stderr as the explanation — stderr
   is the **internal**-failure path (panics, the report the user sends
   back); external errors arrive as events (§6) and never require
   mining stderr.

## 4. The model: runs, turns, steers, and the tree

- A **run** (outer loop) starts when a message is drained while idle:
   model turn → maybe tool calls → tool results → next model turn …
   until a turn with no tool calls. `run_finished` / `run_aborted` /
   `run_failed` end a run; **exactly one terminal per run** — persist
   degrade is not a second terminal, it rides `run_finished.durable`
   and the `persist_*` kinds (§6). A prompt that cannot be made
   durable never starts a run at all: the batch comes back as drafts.
- A **message sent while a run is live is a steer**: acknowledged
   immediately (`message_queued`), enters the conversation at the next
   turn boundary (`user_message`). Never lost — the only exits from
   pending are draining and discard. **A run failure does not clear
   pending**: after `run_failed` the mailbox keeps draining; a queued
   message starts the next run.
- **abort** preempts the run at the next await and discards what was
   queued **at abort time** — the `messages_discarded` notice is
   immediate, emitted at the abort site **ahead of the run's
   `run_aborted` terminal**. Messages arriving *after* the abort are
   not killed by it — they queue normally and start the next run.
- The session is an **append-only tree** of entries. You render the
   **active chain**; `checkout` moves the leaf to any entry in the
   tree — including one on an abandoned branch — and the next append
   becomes a new branch (`git checkout <hash>`, not "rewind n").
- **You hold the active branch only.** The rest of the tree is backend
   truth.

## 5. Commands

All commands are total — there is no rejection. Outcomes are events.
Session-scoped commands **always name their session** (the boot id is
in the ack; sessions you learn from `sessions_available`/
`session_opened` — subagent children included). A command naming an unknown or unloaded session
yields `error { kind: session }` — an **unstamped, backend-level**
frame (the routing failure belongs to no session; the message names
the id — §6).

**Children are command-addressable (v6, route-all):** a subagent
child is a full session host in its own process, and a command
addressing it is forwarded as one wire line — every command consumes
exactly as on any session, because there is no child-specific
consumption anywhere. `message` steers a live child (switching to a
subagent view and steering it is normal usage; the `message_queued`
ack arrives on the child's own stream); `abort` stops that child's
subtree without killing the parent's run. Deep trees route hop by
hop through learned tables; the address is any id you saw stamped on
a frame.

| command | when | effect |
|---|---|---|
| `message { session, text }` | any time | idle: starts a run — acknowledged directly by `user_message` (milliseconds; no queued event — nothing waits); running: steers at the next turn boundary, acknowledged by `message_queued { id, text }`. |
| `abort { session }` | any time | running: preempts (`run_aborted`); discards messages queued at abort time (`messages_discarded`, omitted when none) **and any pending checkout** (§7 — no `checked_out` follows it; reset pending-rewind UI here). **Subtree stop (v6):** aborting a session stops its in-flight subagent descendants (every descendant's terminal flushes on its own stream; instances are never destroyed) — the cascade rides the run token each tool already holds, and aborting a child by id stops that child's subtree and leaves the parent's run alive. Children with no active tool call (the future background mode) survive aborts. Post-abort messages queue normally and start the next run. Idle: no-op. |
| `new_session` | any time | creates a fresh session (same config, tools, and `--model`/`--max-turns` as the boot); its `session_opened` follows — stamped with the new session's own stream, `resumed: false` (v10: one announcement shape for every path). Nothing replays (it is empty). Never waits on any session — lifecycle writes no session's file. |
| `open_session { id }` | any time | loads the session if needed and streams a replay pass stamped with the id — the pass is the acknowledgment. Idempotent: an open session re-replays. Unknown id or unreadable file → unstamped, backend-level `error { kind: session }`. Creating, loading, and switching never wait on the session you are leaving; the one wait is the opened session's **own** in-flight run — its pass arrives at that run's terminal (its live streaming renders immediately; only committed history waits). |
| `checkout { session, entry_id }` | any time | moves that session's chain to the entry (any entry in the file— an off-chain target is a branch switch); see §7. **On receipt:** the target is verified (unknown entry → immediate `error { kind: checkout }`, nothing else happens) and the still-pending messages are discarded (`messages_discarded`, handed back as drafts). The rewind itself: a run in flight is aborted first (`run_aborted` — the user rewinding has declared its continuation obsolete), then the rewind applies at the session's pause point; idle → applies immediately. |
| `model { session, provider, model, thinking_level? }` | any time | switches that session's model — the **register write**, never a chain move (§7). A **state write at receive**: the ref is validated against config (unknown provider/model → immediate `error { kind: model }`, nothing moves), then the entry and the live selection land at once and `model_changed` follows immediately — even mid-run (a run in flight finishes untouched on the model it bound at run open; the next run uses the new one). Not intent: abort never touches it, rapid switches each land (last wins). Durability: no later than the next turn (the write-behind log's prompt barrier flushes the buffer — this switch included — before any turn starts); a hard death in the window loses the switch, and resume announces the register that survived. |
| `interaction_response { session, id, payload }` | after an `interaction_request` | answers a pending request; the payload is shaped by the asking template's convention (§8) — always an answer, never a dismissal. |
| `compact { session, directives? }` | any time | **manual compaction (v7)**: runs the context-summarization pass now — the same machinery as the automatic doors, forced regardless of thresholds and guarded only by the short-history skip (a history shorter than the retained-tail budget → `compaction_failed { message }` saying so; nothing runs). Idle: runs at the session's next beat. Running: **parks** — compaction never aborts a run (it does not move the chain, nothing is made obsolete) — and runs when the run ends. Outcomes: the `compaction_*` bracket (§6). `directives` (v16) is the user's free-text guidance for this invocation — appended to the summarization instruction, never persisted, never replayed: "focus on details relevant to task X which we will start next". Abort clears a parked compact (drop-all-pending-intent — no bracket follows). |

`checkout` needs no idle-care — the backend aborts the run for it and
applies the rewind at the pause point (§7), so sending it any time is
safe; holding it client-side until the terminal is still polite (your
user sees the rewind apply sooner). `model` needs even less: it is a
state write that happens entirely at receive — no parking, no pause
point — so send it any time and expect `model_changed` (or the error)
back at once. A switch that validated but fails to construct in the
environment surfaces as the next run's `run_failed` (the run's
message names the provider) — the register keeps the choice; whether
a picker needs a distinct "didn't take" signal is an open
PROTOCOL.md note.

## 6. Events

`initialize_ack`, `initialize_rejected`, and `protocol_error` are
unstamped control frames; everything else is an event — stamped when
a session produced it, **unstamped when the backend did** (the
catalog, and every `kind: session` error — fold
those connection-level).

**Queueing and transcript**

| event | payload | when |
|---|---|---|
| `message_queued` | `id`, `text` | a `message` accepted while a run is live (a steer that waits). `id` is the message's entry id, minted here. Idle sends never produce this event. |
| `user_message` | `entry_id`, `text` | the message drains into a run (opening batch or steer boundary) and becomes history. Consecutive `user_message`s = an opening batch. |
| `messages_discarded` | `messages: [{ id, text }]` | a clear site: abort (what was queued at abort time — the notice is immediate, at the abort site, ahead of `run_aborted`) or checkout (what was submitted before the checkout; §7, ahead of `run_aborted` and `checked_out`). Omitted when nothing was pending. Salvage as drafts; the backend keeps no copy. |
| `turn_started` | `id`, `started_at_ms` | a model turn begins; `id` is the turn's entry id, minted here and reused at commit. `started_at_ms` is the turn's start, Unix milliseconds (live runs stamp at emission; replay stamps from the turn entry's recorded time, so a replayed bracket's two stamps coincide). |
| `text_delta` | `turn_id`, `text` | assistant text; appends within the turn. Full-text exactly once in replay. |
| `reasoning_delta` | `turn_id`, `id`, `reasoning` | model reasoning; `id` correlates blocks within the turn (several may interleave; same-id deltas append). Full-text once per block id in replay. |
| `tool_call` | `turn_id`, `name`, `call_id`, `internal_call_id`, `arguments` | the model issued a complete tool call, before execution. `arguments` is the raw JSON string, or `null` when unparseable. |
| `interaction_request` | `id`, `ui_type`, `payload` | a tool gate (permission) or a tool body asks the user; `ui_type` names the widget and `payload` is its cargo (§8 templates own the shapes). Several may be open at once. Answer with `interaction_response`; a run terminal closes the unanswered (§8). |
| `compaction_begin` | — | **(v15)** a compaction invocation began — the envelope for every compaction event until `compaction_end` or `compaction_failed`. No id: the stream stamp scopes it (invocations are serial per session), and events inside are contiguous and ordered. May arrive mid-run (between turns) or at idle. A door that finds nothing worth folding stays silent — no envelope. |
| `compaction_delta` | `text` | a summary text delta inside the open pass (positional: deltas between steps belong to the pass that next commits; after a `compaction_retried`, the pending deltas were the discarded attempt's and drop). |
| `compaction_step` | `id`, `usage`, `cost?` | **(v15)** one pass committed: the summary is durable as a compaction entry. `id` is the pass's entry id (born early, like turn ids) — a checkout anchor and the replay marker's correlation. `usage` is the summarization request's fresh report and `cost` its recorded dollars: spend that meters exactly like a `completion_call`'s — a frontend's totals are one fold over both event kinds. Multi-pass invocations emit one step per pass. |
| `compaction_retried` | — | **(v15)** a violating attempt was discarded and the request resent (`turn_retried`'s sibling): the attempt's deltas drop, the invocation continues. |
| `compaction_end` | `tokens_after` | **(v15)** the invocation completed (including an oversized exit — its passes stand): the model-visible context is now `[summary] + retained tail`, and `tokens_after` is the final length (the last pass's summary output plus the retained tail's delta sum — the base the next turn starts from). Everything before the cut stays in the file; the next replay pass still renders it, with the envelope at the boundary; checkout to a pre-compaction entry yields the full-history branch. |
| `compaction_failed` | `message` | the invocation failed or was cancelled: committed steps stand, nothing further commits. Not a run terminal — the run (if any) continues, and the automatic doors retry when the conditions next hold. |
| `tool_result` | `turn_id`, `entry_id`, `name`, `internal_call_id`, `content`, `status`, `details?` | one tool body finished; its result committed. `content` is exactly the text the model saw — already capped at the source, failure text included; render it verbatim. `status` is structure only: `success` or `failed { exit_code? }`; the detail is in `content`, not `status`. `details`, when present, is derived presentation cargo owned by the tool named in `name` — dispatch on `name`, degrade to `content` when absent or unknown. The per-tool shapes are TOOLS.md's (today's producers: `edit`'s diff + outcomes, `bash`'s truncation/spill, `subagent`'s child-session facts). |
| `completion_call` | `turn_id`, `usage`, `cost?` | one model request finished; its usage is final — the fresh server report (v12). The full five-field `Usage` rides here per request (cache legs included); anything aggregated (a run, a session) is your sum over these — aborted and failed runs count too. v13 adds `cost`: the dollars the turn cost, **recorded at commit** from the rates then in effect (the invoice ruling — spend already happened; a later rate cut does not rewrite it). Absent when the provider reported nothing or the model carries no rate card; the same value rides the session file, so replays after resume show exact history. `model_changed.cost` still carries the current rates, for what future turns will cost. |
| `turn_truncated` | `turn_id` | the committed turn ended truncated: the provider cut generation at its output limit (`finish_reason: length`). Informational, never a failure — the run continues exactly as usual (steers drain into the next turn; the run may end normally). Show it as a note; a steer is how the user asks the model to go on. |
| `turn_committed` | `id`, `completed_at_ms` | the turn is durable history. Same id as `turn_started`; `completed_at_ms` is the commit's time, Unix milliseconds (replay: the entry's recorded time). |
| `turn_retried` | `turn_id` | the turn was discarded before commit (e.g. malformed tool-call arguments); drop its provisional groups — a fresh `turn_started` follows. |
| `native_item` | `item` (opaque JSON) | a provider-native output the backend does not model. **Live-only**: never replayed, never an anchor. Render or skip. |

**Steer boundary ordering.** A steer's `user_message` lands strictly
between turns: after the previous turn's `turn_committed` and
`completion_call`, before the next `turn_started` (or, if it resets a
retry, before the fresh `turn_started` that follows `turn_retried`).

**Tool rendering.** `tool_result.content` is a faithful copy of what
the model saw — render it verbatim, collapsed by default (a 500-line
read is real content). Specialized views (a diff view for `edit`, a
command block for `bash`) are a view-side dispatch on the tool name,
matched in one module with a generic name+args+result card as the
fallback; the reducer never learns tool names. The dispatch extracts
when `tool_result.content` first lands on the wire, not before — no
dead structure ahead of the data. What each tool's specialized view
consumes (its `details` cargo) is TOOLS.md's table of shapes.

**Run terminals** (exactly one per run)

| event | payload | meaning |
|---|---|---|
| `run_finished` | `output`, `durable`, `started_at_ms`, `completed_at_ms` | the run completed. `output` is the **final turn's** text (your accumulated deltas are authoritative for everything else); `durable: false` means the write-behind log still holds entries (a `persist_degraded` error explains; they flush on later commits — nag about disk space, don't fail). The timestamps are Unix milliseconds — the run's start and finish (the turn brackets carry per-turn times). |
| `run_aborted` | `output`, `started_at_ms`, `completed_at_ms` | aborted. `output` is the final response's text **if it had arrived** — empty for a mid-stream abort. Do not rely on it: the uncommitted turn's text lives only in the deltas you accumulated. Timestamps as on `run_finished` (the abort's time is the completion). |
| `run_failed` | `message`, `kind`, `started_at_ms`, `completed_at_ms` | the failure in display form, with its class in `kind` (an open string, the `error`-kind law: unknown values display generically). Well-known kinds: `provider` (the provider stream errored mid-run — transport, auth at request time, a typed rejection; the common case), `model` (the run could not open — the selection validates but cannot be constructed here; retry needs a model switch, not a resend), `persist` (the session log refused to flush before the run started — it never began), `engine` (the internal residual, incl. a subagent child process dying). Persist degrade is *not* this; it rides the persist kinds and `durable`. Pending messages are not cleared — they drain into the next run. |

**Session navigation and configuration**

| event | payload | when |
|---|---|---|
| `sessions_available` | `sessions: [{ id, created_at, entry_count, path, cwd }]` | once, right after the ack's startup notes: every stored session, newest first. **Unstamped, backend-level.** Minimal by ruling — a plain object, fields grow when
   needed (v16 added `path` and `cwd`: a frontend never lists the
   directory to learn either). A brand-new session has no file yet and is absent until it records. |
| `skills_available` | `skills: [{ name, description, location, level }]` | **(v8)** once, right after `sessions_available`: every skill the four-source discovery merged (home `~/.agents`/`~/.tabit` + workspace `.agents`/`.tabit` skills dirs), the same facts the prompt catalog carries — `level` is `user` or `workspace` (which source won). **Unstamped, backend-level** (one process, one cwd, one skill set); only announced when at least one skill was discovered. Skill *invocation* is no new wire shape: the model calls the `skill` tool, an ordinary `tool_call`/`tool_result` pair on the asking session's stream. |
| `extensions_available` | `extensions: [{ name, version, description?, dir, status, reason?, tools: [{ name, description }], hooks: [string] }]`, `conflicts: [{ kind, extension, tool, incumbent? }]` | **(v9)** once, right after `skills_available`: every discovered extension with its provenance (`dir`) and standing — `status` is `alive` or `dead` (a refused handshake, a failed scan, or death since; `reason` carries why). **Unstamped, backend-level** (one process, one extension host); only announced when at least one extension was discovered — a refusal counts as discovered. **A boot-time snapshot** (2026-09 ruling): a mid-run extension death does not re-announce — stderr carries the report and the catalog stands until the next backend start. `conflicts` are the boot's name-assembly reports: `kind: "replaces_core"` (an extension tool replaced the core tool of the same name — the signal is mandatory; how loudly you present it is your call) and `kind: "refused_peer"` (the newcomer was refused, `incumbent` names the extension that holds the name). Extension tool *invocation* is no new wire shape: an ordinary `tool_call`/`tool_result` pair, attributed by the model-facing name. |
| `session_opened` | `id`, `path`, `cwd`, `model`, `resumed`, `parent?`, `parent_call?` | a session became visible in this backend — the boot (at spawn, right after the ack), a `new_session`, an `open_session`, **or a subagent child** (v5). `cwd` (v16) is the session's working
directory — the boot's is the backend's cwd, a child's is its spawn
cwd. **One announcement shape for every path** (2026-09 ruling — the ack carries protocol-level facts only; the boot is not a special case). Stamped with the session's own stream. Selection notes, if any, follow on the same stream. v5: `path` is **empty for an ephemeral session** (in memory only — nothing to open or replay; subagent children are ephemeral today), and `parent` names the spawning session for a subagent child — absent for every user-facing session. A frontend branches on `parent`: user sessions update their session facts, children render nested (or not at all) — a child announcement must never overwrite the active session's facts. The child's whole run (its `user_message`, deltas, `tool_call`s, terminal) streams on its own stamp; the spawner's transcript sees only the subagent `tool_call`/`tool_result` pair. `parent_call` (v7, additive) is the spawning tool call's `internal_call_id`: the announce pairs the child with the **exact** open `tool_call` event — exact under concurrent subagent calls, where arrival order cannot disambiguate. Absent whenever `parent` is. |
| `checked_out` | `entry_id`, `base_id` | checkout succeeded. `base_id` is `null` today: drop everything and rebuild from the replay pass that follows. A non-null `base_id` is the reserved suffix mode (keep through `base_id`, apply the pass) — treat any non-null value as "rebuild from the pass" and you stay correct. |
| `model_changed` | `provider`, `model`, `thinking_level`, `context_window?`, `name?`, `cost?` | the session's **active model** — a session preference: the file's last `model_change`, latest in time wins (a rewind never moves it). Announced live whenever the session becomes visible: ahead of every replay pass (boot, `open_session`, re-replay, after `checked_out`) — idempotent, the value repeats — and at every `model` command (a state write at receive; §5). **Never inside a pass** (state is announced, not reconstructed). The ack's `model` is the boot session's register. v11: the announcement also carries the model record resolved against config — `context_window` (tokens; a context meter's denominator), `name` (a display name; fall back to the model id), and `cost` (`{ input, output, cache_read, cache_write }`, USD per million tokens). Each field is optional and **absent means the config does not state it** (never zero); a register stale against an edited config announces the ids with no facts, and the next validated switch repairs it. |

**Errors: one generic carrier with a `kind`.** Anything that goes
wrong outside a run terminal rides `error { kind, message, … }`. A
minimal frontend implements one handler — show the message; a rich one
switches on `kind`. Unknown kinds display generically. External
errors never travel as stderr — stderr is the internal-failure
report (§3.5); you never mine it for user-facing meaning.

| kind | extra fields | meaning |
|---|---|---|
| `model` | — | model configuration degraded: a startup preference (stale `default_model`, a resumed session's model gone) fell back, or a `model` command named a ref config does not know (§5 — the immediate error on an invalid switch). A warning in the fallback case — the session continues, with the fallback named in the message. |
| `session` | — | a session command failed: `open_session` named an unknown id or an unreadable file, a command targeted an unknown session, `new_session` could not build, or the startup listing failed. **Unstamped, backend-level** — every `session`-kind error is (the failure belongs to no session; the message names the id). |
| `checkout` | — | the checkout target does not exist in the session (§7). Stamped — it names an entry inside a real session. |
| `persist_degraded` | `pending` | the write-behind log could not flush: `pending` entries are committed in memory but not on disk (disk full is the usual cause). Every later commit retries; nothing is lost unless the process is force-stopped while degraded (then the pending entries go — model output and register records; a stuck start's own messages come back as drafts when the run is refused). Nag about disk space. |
| `persist_recovered` | — | the pending entries reached the disk. |

**Write-behind persistence (shipped, PROTOCOL.md flag 8).** Commits are
memory-first: the resident state (tree, head, context) is the
in-session truth and the file is its write-behind mirror — always a
clean prefix of commit order. Entering a run, the buffer retries
whatever a previous failure stuck; a still-refusing flush blocks the
start (`run_failed`, the mailbox's messages handed back as drafts) —
no turn runs twice on a memory-only write. The gate's one accepted
trade: a session with no user message yet has nothing owed to the
disk, so its first turn runs in memory against a dead disk and the
degrade announces at the turn's own commit. `model_change` and the
`checkout`/`aborted` side records ride the buffer (the marker
classification ruling): durable no later than the next user message's
commit, and a hard death in the window loses them — hand-redoable,
and resume announces whichever register survived.

**Replay** (brackets; content is finalized events from the catalog
above — full-text deltas, same ids as live)

| event | payload | when |
|---|---|---|
| `replay_started` | `total` | a replay pass begins (startup with `replay: true`, or after `checked_out`). `total` = **events** to come between the brackets (the progress denominator). |
| `replay_done` | — | the pass ends; live traffic (or quiescence) follows. |

`usage` objects are protocol-owned:
`{ input_tokens, output_tokens, total_tokens, cached_input_tokens,
cache_creation_input_tokens }` (u64; `total = input + output`; the
cache fields are accounting breakdowns aligned with the backend's
cost model). The engine tracks richer fields (reasoning, tool-use,
per-TTL splits); they stay engine-internal and never reach the wire.

Session stats (whatever surface carries them — today the session
summary surface, later a `stats` command) are **cumulative over the
whole session file**: every committed assistant turn on every branch
(abandoned spend is still spend) plus every `discarded` attempt (a
hook-vetoed or defective turn retried — the tokens were spent, so the
log bills them; flag 22). Stats never roll back on a checkout or
rewind; the register, not the branch, owns attribution.

## 7. Replay and checkout: how transcript state moves

**Startup replay.** Send `initialize { protocol_version, replay: true
}`. After the ack: the session's `model_changed` announcement (§6),
then `replay_started { total }` → the active branch's
nodes as finalized events in branch order (`user_message` per user
node; per assistant node: `turn_started`, full-text deltas, its
`tool_call`s and `tool_result`s, `completion_call`, `turn_committed`)
→ `replay_done`. Branch
siblings are excluded by construction; ids are the log's ids, identical
to what a live consumer of the same history saw; no `model_changed`
ever appears inside the brackets (state is announced live, not
reconstructed). One honesty note:
the chain may contain **synthesized tool results** (the backend repairs
a tool batch interrupted by a crash or abort — the model context needs
the roundtrip closed).

**Switching sessions.** Send `open_session { id }`. The full-re-render
rule (ruled; pi-proven): clear your view of the target session
optimistically, then apply the pass that follows (`replay_started` →
finalized events → `replay_done`, stamped with the id). It is the same
shape as startup replay — one transcript-rebuild path in your code,
and the seam a future streamed suffix replaces. Switching never waits
on the session you are leaving; if the opened session's own run is in
flight, its live streaming renders immediately and its pass (the
committed history) arrives at that run's terminal. Runs you switch
away from keep running backend-side; their events keep arriving on
their own stamp — keep reading, attribute, and re-replay when you
switch back.

**Checkout.** Send `checkout { session, entry_id }` any time. Idle: it
applies immediately. Running: it **aborts the run first** (ruled
2026-08 — the user rewinding has declared the run's continuation
obsolete; there is no "stop now" composition to remember, the command
is its own) and the checkout executes at the session's pause point.
The success sequence on that session's stream: `messages_discarded`
(only if messages were pending — see the watermark rule below), then
`run_aborted` (only if a run was in flight), then
`checked_out { entry_id, base_id: null }`, then the `model_changed`
announcement (idempotent — the rewind never moved the register; same
code path as every pass), then the replay brackets.

1. **Drop everything you hold for that session** (`base_id` is `null`
   — full re-render, the same rule as switching sessions) and apply
   the `replay_started` … `replay_done` pass: the rewound chain
   through its **tip** (the tip may sit past `entry_id` by
   repair entries — the honesty note from startup replay).
2. The aborted run's own epilogue preceded the rewind: its
   `run_aborted` terminal lands first. Interrupted tool calls are
   repaired log-side (synthesized results close the dangling
   roundtrip) — you meet them **inside the replay pass**, not as
   events before it; the pass replaces whatever of the run entered
   history.

**What a checkout discards — the watermark rule.** A checkout
discards exactly the messages **submitted before it** (each carries a
born-early id; `messages_discarded` hands back the texts). Messages
you send *after* a checkout are input for the new branch: they stay
queued and run against the rewound chain. So you never need to
synchronize with the backend's pause point — but if you want a
message to be the new branch's first turn, sending it after you see
`checked_out` is the way to make that deterministic.

**When the discard happens: on receipt.** The still-pending messages—
exactly the ones `message_queued` announced that have not drained—
are cleared the moment the checkout is accepted, and handed back
immediately as drafts. Their fate is decided right there, not by the
finishing run's internal timing: a message you sent before the
checkout either already entered the conversation (you saw its
`user_message`— the rewind drops it with everything else after the
target) or it comes back as `messages_discarded`. Messages sent after
the checkout are input for the new branch: they queue normally and
run against the rewound chain. (One narrow race: an idle send
immediately followed by a checkout can still have its message grabbed
by the worker's batch— both outcomes leave it out of the new
branch, visibly.)

**Multiple checkouts.** Checkouts that pile up before the pause point
**collapse to the last one**— only it executes and emits
(`messages_discarded` for what was pending at its receive, one
`checked_out`, one pass); superseded checkouts emit nothing, not even
an error. Checkouts spaced across idle beats (one fully applies
before you send the next) execute one at a time. An unknown entry
errors on receipt— it never parks, so it is never superseded
either.

**Abort drops a pending checkout.** Abort is drop-all-pending-intent:
messages and any checkout still waiting for the pause point. No event
marks the discarded checkout— no `checked_out` will follow; treat
the abort as canceling it, and resend the checkout after the abort if
you still want the rewind.

**Replay vs. messages.** A pass never holds messages: they keep
flowing while a pass is parked, and at the session's beat the pass is
served **before** the next message batch— a read requested after a
message still answers ahead of it. A message's inclusion in a pass is
decided solely by whether it drained before the beat (drained → in
the pass; queued → it renders live right after `replay_done`).

**Valid cut points** (ruled). The atomic unit is the tool roundtrip:
an assistant turn and its complete result batch commit and rewind
together — you cannot cut in-between (partial writes from crashes or
aborts are repaired with synthesized results, never left half-open).
Checkout targets — and `base_id` values — are therefore
`user_message` entries and committed assistant turns. `model_change`
entries exist in the file and remain acceptable targets by totality,
but they are inert anchors: the active model is a session preference
(§6) no longer derived from the chain, so targeting one is
conversation-identical to targeting its neighbor.

**Synthesized results tell the truth in their body.** A repaired tool
result's content is the sentence "tool execution was interrupted
before completing — the call may have had partial effects; verify them
before relying on anything it did". It is never a fabricated success;
render it like any tool result and the text says what happened. No
wire marker.

## 8. Interaction requests

One generic ask, v4-shipped: any backend asker — a tool gate
(permission), a tool body (ask-the-user tools), a hook — questions
the user through one frame pair, routed by id, payloads opaque to the
core (the core's interaction vocabulary is routing only; PROTOCOL.md's
interaction-generalization ruling is the design record). Concurrent
chains may hold several open requests at once; answer them in any
order.

```
← {"type":"interaction_request","stream":"019…","id":"019…","ui_type":"native:select_one",
   "payload":{"title":"Run command?","body":"rm -rf target",
     "options":[{"label":"Allow"},{"label":"Always allow"},{"label":"Deny"}],"free_text":true}}
→ {"type":"interaction_response","session":"019…","id":"019…",
   "payload":{"selected":["Deny"],"text":"never delete build dirs"}}
```

- `interaction_request { id, ui_type, payload }` — an event, stamped
  with the asking session's stream. `id` is backend-minted (UUIDv7,
  born at acknowledgment); `ui_type` names the widget; `payload` is
  the asker's cargo.
- `interaction_response { session, id, payload }` — a command,
  session-addressed like every command. **Always an answer**: the
  frontend never expresses dismissal; that is backend-derived (see
  the closing rule).
- **`ui_type` namespaces.** `native:*` renders in every conforming
  frontend; `ext:<id>:*` types arrive with extensions. **Unknown
  types: report, don't swallow** — surface a notice, never fabricate
  an answer.
- **Templates** (`tabit-protocol::templates` owns the names and
  payload schemas; they are prebuilt asks, not core types). Two
  widgets cover the native surface (2026-09 ruling — there is no
  separate confirm or ask card; both were special cases of these and
  keeping them named would mislead future developers into believing
  in a special UI that does not exist): `native:select_one` and
  `native:select_any`, sharing the one `SelectAnswer` answer shape.
  **Their request/answer payload schemas and the built-in askers
  that use them are TOOLS.md's** (the interaction-shapes half of the
  doc split — this section keeps the mechanics).
- A `free_text` answer is delivered to the model when present (a
  denial reason shapes the retry), not just logged.

**Closing rule:** a run terminal (`run_finished` / `run_aborted` /
`run_failed`) closes every pending request — drop the cards, no
response needed. There is no close event and none is needed: an
unanswered request's death always coincides with a run terminal (a
question lives inside its tool's execution, and the run always ends
in exactly one terminal). A response racing a terminal (stale id,
dead asker) is a logged no-op on the backend — send it, never block
on the race. Requests never replay; the durable record of an
interaction is the tool result — the answer or denial the model saw.

## 9. Invariants you may rely on

- **The message ledger.** Every `message_queued { id }` ends in
  exactly one of `user_message { entry_id: id }` or an entry in
  `messages_discarded` — never both, never neither, across abort,
  checkout, the prompt barrier, and run failure.
- **The turn ledger.** Every `turn_started { id }` ends in
  `turn_committed { id }`, or is discarded (`turn_retried { turn_id }`
  or a run terminal without commit). Discarded ids are never reused.
  Uncommitted turns never enter history: after abort or failure or
  restart, replay shows committed turns only.
- **One live turn at a time** per stream. Deltas arrive strictly in
  transcript order; a run's terminal arrives after all of its events.
- **Ids are backend-minted UUIDv7 strings.** You never generate ids;
  you learn them from events and replay anchors (`user_message`,
  `turn_started`, `turn_committed`, `tool_result.entry_id`).
  (`model_changed` carries no wire id yet — §6.)
- **Ordering is total per stream.** One connection, one ordered
  stream; events of one session arrive in order, a run's terminal
  after all of its events. Events of *different* sessions interleave
  arbitrarily — attribute by stamp, never by position.
- **Idle/running is derivable**: running from the first `user_message`
  of a run until its terminal; idle otherwise. Startup (after
  `replay_done`) is idle. A queued-while-idle message keeps you idle
  until it drains.
- **Recovery is replay.** After a backend crash or restart, the same
  initialize-with-replay gives you the active chain with the same ids.
  Only pending messages are lost (they were never history — salvage as
  drafts before restarting if you want them); committed-but-unflushed
  entries can be lost to a force-stop (the write-behind window — model
  output, never user input; see §6's durability notes). Caveat: a
  fresh session's
  file materializes only at its **first user message** — if the
  backend died before any message drained, there is nothing on disk;
  restart falls back to a fresh session (a new id). Restarting with
  `--session`/`--continue` preserves the session's model (the file's
  last `model_change` wins over config defaults — your latest choice,
  even one recorded on a branch you later rewound away) — no need to
  re-pass `--model`.

## 10. Limits and non-features (honest list)

- **No consumer backpressure yet.** A slow reader grows backend memory
  up to an arbitrarily high tripwire cap (breach = a runaway producer
  bug; the backend dies loudly). Keep reading. No line-size limits
  either way.
- **No replay pagination.** The whole active chain arrives per pass,
  however large. Cursors are a future addition.
- **No unload or residency limits.** Opened sessions stay loaded for
  the process's life (lazy loading bounds *startup*; LRU unload is
  deferred). There is no model-discovery command and no in-band
  catalog refresh: `sessions_available` is announced once at startup,
  sessions created in-band announce themselves (`session_opened`),
  and sessions appearing on disk from elsewhere need a restart
  (`tabit-core --list`, a human table, exists for CLI inspection).
- **Event timestamps: the brackets carry them (v10).** `turn_started`/
  `turn_committed` and the run terminals carry Unix-ms stamps (§6);
  other events carry none — entries keep their wall-clock times in the
  session file (§10).
- **One consumer.** Events go to the single connected frontend; no
  fan-out, no second attach.

## 11. Open questions (known and unsettled)

1. **Backpressure.** What a stalled reader should experience — needed
   before long-running GUI use.
2. **Interaction edge semantics — settled with the permission
   milestone (§8):** run terminals close every pending request; stale
   responses are logged no-ops; requests never replay.
3. **Subagent streams.** Sibling `stream` ids and their event subset —
   reserved, unspecified.

Settled since the review: bad-flag exits stay
exit-1-with-stderr-no-frames while session/model startup failures
reject with plain-reason frames (§3.1 — the death-classification
pin); cut points follow the roundtrip-unit rule
(§7); synthesized tool results carry no marker (§7); all
non-terminal errors ride the generic `error { kind }` carrier (§6).
Model discovery stays config-side (`--model` refs resolve at startup;
no discovery command is shipped). The write-behind log with its prompt
barrier shipped (§6; PROTOCOL.md flag 8).
