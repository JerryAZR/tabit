# FRONTEND.md — the tabit frontend contract

This is the contract for anyone building a UI on top of the tabit
backend: what the backend provides, what it expects from you, and the
invariants your UI can rely on. Read this document alone; you should
not need the codebase to design a frontend.

Wire shapes below are the **v3 contract**. v2 shipped (ids, replay);
v3 is the multi-session host — session-addressed commands,
`new_session`/`open_session` on the channel, the `"main"` stream alias
retired (the stream stamp is the session id). v3 lands as one
protocol-version bump with no compatibility period; always check the
ack's `protocol_version`. The one-shot JSON listing edges —
`--list --json`, `models --json` — are likewise v2-era; today `--list`
prints a human table.

## 1. Architecture: two processes, one pipe

You spawn the backend; you never link it as a library.

```
tabit --json [--continue | --session <path>] [--model <ref>]
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
- **Spawn environment.** Sessions live at `<project-root>/.tabit/
  sessions`, where project root is the nearest ancestor directory
  containing `.git`, else the cwd. Spawn the backend in the project
  directory. `--model <ref>` is `provider/model` or a bare model id
  when unambiguous; `--max-turns <n>` also exists (and applies to
  sessions created later in the same process). The `tabit` launcher
  hands the GUI its exact executable via `--tabit <path>` — "can't
  find the backend" is not a failure mode in the supported flow
  (`TABIT_BIN` remains a development override).
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
→ {"type":"initialize","protocol_version":3,"replay":true}
← {"type":"initialize_ack","protocol_version":3,"session_id":"019…","session_path":"…",
   "model":{"provider":"…","model":"…","thinking_level":null},"resumed":true}
← {"type":"sessions_available","stream":"019…","sessions":[
     {"id":"019…","created_at":"2026-08-22T…","entry_count":14}, … ]}
← {"type":"replay_started","stream":"019…","total":14}
← … the transcript as finalized events …
← {"type":"replay_done","stream":"019…"}
→ {"type":"message","session":"019…","text":"who are you?"}
← {"type":"message_queued","stream":"019…","id":"019…","text":"who are you?"}
← {"type":"turn_started","stream":"019…","id":"019…"}
← {"type":"text_delta","stream":"019…","turn_id":"019…","text":"I'm "}
← {"type":"text_delta","stream":"019…","turn_id":"019…","text":"tabit."}
← {"type":"turn_committed","stream":"019…","id":"019…"}
← {"type":"run_finished","stream":"019…","output":"I'm tabit.","usage":{…},"durable":true}
```

Every event frame is **flat**: the event's `type` and payload fields
sit next to `stream`. The `stream` stamp is the **session id** that
produced the event (the boot session's id is in the ack) — the
`"main"` alias is gone. Events from several open sessions interleave
on the connection; attribute by stamp. **Route frames by stamp**
(render the sessions you are viewing, ignore the rest) and **skip
unknown event `type`s** — both are forward-compatibility paths
(subagent streams and new events arrive later without a version bump).

## 3. Handshake, lifecycle, exit codes

1. Your **first line** must be
   `initialize { protocol_version, replay? }` (`replay` defaults to
   `false`). Match → `initialize_ack` with the session facts (id,
   path, active model, `resumed`), then — if you asked — the replay
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
   `run_failed` end a run; **exactly one terminal per run** (the
   durability case is folded into the terminal — §6).
- A **message sent while a run is live is a steer**: acknowledged
   immediately (`message_queued`), enters the conversation at the next
   turn boundary (`user_message`). Never lost — the only exits from
   pending are draining and discard. **A run failure does not clear
   pending**: after `run_failed` the mailbox keeps draining; a queued
   message starts the next run.
- **abort** preempts the run at the next await and discards what was
   queued **at abort time** (order: `run_aborted`, then
   `messages_discarded`). Messages arriving *after* the abort are not
   killed by it — they queue normally and start the next run.
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
`session_created`). A command naming an unknown or unloaded session
yields `error { kind: session }` stamped with the id you named.

| command | when | effect |
|---|---|---|
| `message { session, text }` | any time | idle: starts a run — acknowledged directly by `user_message` (milliseconds; no queued event — nothing waits); running: steers at the next turn boundary, acknowledged by `message_queued { id, text }`. |
| `abort { session }` | any time | running: preempts (`run_aborted`); discards messages queued at abort time (`messages_discarded`, omitted when none). Post-abort messages queue normally and start the next run. Idle: no-op. |
| `new_session` | any time | creates a fresh session (same config, tools, and `--model`/`--max-turns` as the boot); `session_created { id, path }` follows, stamped with the new id. Nothing replays (it is empty). |
| `open_session { id }` | any time | loads the session if needed and streams a replay pass stamped with the id — the pass is the acknowledgment. Idempotent: an open session re-replays. Unknown id or unreadable file → `error { kind: session }` stamped with the id. A running session answers at its next idle beat (its in-flight run finishes first). |
| `checkout { entry_id }` | idle only | moves the active chain; see §7. Compose abort-then-checkout if a run is live. |
| `model { provider, model, thinking_level? }` | idle only | the next run uses this selection; validates against the backend's config. |
| `interaction_response { session, id, option?, text? }` | after an `interaction_request` | answers a pending request; at least one of option/text; see §8. |

`checkout` and `model` are idle-only by convention, not by error: a
frontend derives idle/run state from events (§9) and holds the command
until the terminal, or aborts first.

## 6. Events

`initialize_ack`, `initialize_rejected`, and `protocol_error` are
unstamped control frames; everything else is a stamped event.

**Queueing and transcript**

| event | payload | when |
|---|---|---|
| `message_queued` | `id`, `text` | a `message` accepted while a run is live (a steer that waits). `id` is the message's entry id, minted here. Idle sends never produce this event. |
| `user_message` | `entry_id`, `text` | the message drains into a run (opening batch or steer boundary) and becomes history. Consecutive `user_message`s = an opening batch. |
| `messages_discarded` | `messages: [{ id, text }]` | abort (what was queued at abort time; the event arrives with the run's wind-down) and checkout (before `checked_out`). Omitted when nothing was pending. Salvage as drafts; the backend keeps no copy. |
| `turn_started` | `id` | a model turn begins; `id` is the turn's entry id, minted here and reused at commit. |
| `text_delta` | `turn_id`, `text` | assistant text; appends within the turn. Full-text exactly once in replay. |
| `reasoning_delta` | `turn_id`, `id`, `reasoning` | model reasoning; `id` correlates blocks within the turn (several may interleave; same-id deltas append). Full-text once per block id in replay. |
| `tool_call` | `turn_id`, `name`, `call_id`, `internal_call_id`, `arguments` | the model issued a complete tool call, before execution. `arguments` is the raw JSON string, or `null` when unparseable. |
| `interaction_request` | `id`, `title`, `body`, `options`, `free_text` | a tool gate (permission) or a tool body asks the user; several may be open at once. Answer with `interaction_response`; a run terminal closes the unanswered (§8). |
| `tool_result` | `turn_id`, `entry_id`, `name`, `internal_call_id`, `content`, `status` | one tool body finished; its result committed. `content` is exactly the text the model saw — already capped at the source, failure text included; render it verbatim. `status` is structure only: `success` or `failed { exit_code? }`; the detail is in `content`, not `status`. |
| `completion_call` | `turn_id`, `input_tokens`, `output_tokens` | one model request finished; usage is final for it. |
| `turn_truncated` | `turn_id` | the committed turn ended truncated: the provider cut generation at its output limit (`finish_reason: length`). Informational, never a failure — the run continues exactly as usual (steers drain into the next turn; the run may end normally). Show it as a note; a steer is how the user asks the model to go on. |
| `turn_committed` | `id` | the turn is durable history. Same id as `turn_started`. |
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
dead structure ahead of the data.

**Run terminals** (exactly one per run)

| event | payload | meaning |
|---|---|---|
| `run_finished` | `output`, `usage`, `durable` | the run completed. `output` is the **final turn's** text (your accumulated deltas are authoritative for everything else); `usage` is aggregated across the whole run (the per-request figures are the `completion_call`s). `durable: false` means persistence failed after completion — the conversation continues but the tail is not on disk; surface this. |
| `run_aborted` | `output` | aborted. `output` is the final response's text **if it had arrived** — empty for a mid-stream abort. Do not rely on it: the uncommitted turn's text lives only in the deltas you accumulated. |
| `run_failed` | `kind`, `message` | `kind` ∈ `provider` (stream/transport errors), `budget` (max turns), `stopped` (engine stopped early: hook terminate, empty conversation, malformed-tool-call retry exhaustion). Pending messages are not cleared — they drain into the next run. |

**Session navigation and configuration**

| event | payload | when |
|---|---|---|
| `sessions_available` | `sessions: [{ id, created_at, entry_count }]` | once, right after the ack's startup notes: every stored session, newest first. Minimal by ruling — a plain object, fields grow when needed. A brand-new session has no file yet and is absent until it records. |
| `session_created` | `id`, `path` | a `new_session` succeeded; stamped with the new id (its selection notes, if any, follow on the same stream). |
| `checked_out` | `entry_id`, `base_id` | checkout succeeded. `base_id` is where your old chain and the new chain diverge; `null` means from the root (drop everything). |
| `model_changed` | `entry_id`, `provider`, `model`, `thinking_level` | a `model` command (or startup) set the selection for the next run; it is a chain entry and a valid anchor. |

**Errors: one generic carrier with a `kind`.** Anything that goes
wrong outside a run terminal rides `error { kind, message, … }`. A
minimal frontend implements one handler — show the message; a rich one
switches on `kind`. Unknown kinds display generically. External
errors never travel as stderr — stderr is the internal-failure
report (§3.5); you never mine it for user-facing meaning.

| kind | extra fields | meaning |
|---|---|---|
| `model` | — | model configuration failed or degraded: a `model` command failed validation, or a startup preference (stale `default_model`, a resumed session's model gone) fell back. The fallback case is a warning — the session continues, with the fallback named in the message. |
| `session` | — | a session command failed: `open_session` named an unknown id or an unreadable file, a command targeted an unknown session, `new_session` could not build, or the startup listing failed. Stamped with the stream it concerns (the targeted id; the boot stream for untargeted outcomes). |
| `checkout` | — | the checkout target does not exist or is not a valid cut point (§7). |
| `persist_degraded` | `pending` | a log flush failed (disk full?); `pending` entries are buffered in memory and retried on every commit. Nag the user about disk space. |
| `persist_recovered` | — | the buffer drained; everything is on disk again. |

**The prompt barrier.** A turn does not start until its opening user
message is durable. If the flush fails at drain time, the batch is not
held — it is discarded, `messages_discarded` returns the texts as
drafts, and an `error { kind: persist_degraded }` explains why. No
turn ever runs on input that exists only in memory.

`run_finished { durable: false }` means the buffer was non-empty at
the terminal. The conversation continues from memory regardless; a
restart replays disk truth — buffered entries that never flushed are
lost on a force stop (the accepted limit). A degraded session heals
itself the moment the disk accepts writes again.

**Replay** (brackets; content is finalized events from the catalog
above — full-text deltas, same ids as live)

| event | payload | when |
|---|---|---|
| `replay_started` | `total` | a replay pass begins (startup with `replay: true`, or after `checked_out`). `total` = **entries** to come. |
| `replay_done` | — | the pass ends; live traffic (or quiescence) follows. |

`usage` objects are protocol-owned:
`{ input_tokens, output_tokens, total_tokens, cached_input_tokens,
cache_creation_input_tokens }` (u64; `total = input + output`; the
cache fields are accounting breakdowns aligned with the backend's
cost model). The engine tracks richer fields (reasoning, tool-use,
per-TTL splits); they stay engine-internal and never reach the wire.

## 7. Replay and checkout: how transcript state moves

**Startup replay.** Send `initialize { protocol_version, replay: true
}`. After the ack: `replay_started { total }` → the active chain's
entries as finalized events in chain order (`user_message` per user
entry; per assistant entry: `turn_started`, full-text deltas, its
`tool_call`s and `tool_result`s, `completion_call`, `turn_committed`;
`model_changed` for model-change entries) → `replay_done`. Branch
siblings are excluded by construction; ids are the log's ids, identical
to what a live consumer of the same history saw. Two honesty notes:
the chain may contain **synthesized tool results** (the backend repairs
a tool batch interrupted by a crash or abort — the model context needs
the roundtrip closed), and a replayed chain with no model entry gets a
leading `model_changed` backfill.

**Switching sessions.** Send `open_session { id }`. The full-re-render
rule (ruled; pi-proven): clear your view of the target session
optimistically, then apply the pass that follows (`replay_started` →
finalized events → `replay_done`, stamped with the id). It is the same
shape as startup replay — one transcript-rebuild path in your code,
and the seam a future streamed suffix replaces. A session whose run is
still in flight answers at its terminal (the pass then reflects
everything committed). Runs you switch away from keep running
backend-side; their events keep arriving on their own stamp — keep
reading, attribute, and re-replay when you switch back.

**Checkout.** Send `checkout { entry_id }` (idle). The event sequence
is: `messages_discarded` (if anything pending — steers belong to the
context they were sent into; salvage as drafts), then `checked_out
{ entry_id, base_id }`, then the replay brackets.

1. Drop every group you hold **after `base_id`** (`null` → drop all).
2. Apply the `replay_started` … `replay_done` pass: exactly the suffix
   you never had — the new chain from `base_id` through its **tip**
   (the tip may sit past `entry_id` by repair/backfill entries, same
   two honesty notes as startup replay).
3. An ancestor target (the common "rewind") has an empty suffix: the
   brackets arrive empty, nothing else changes.

**Valid cut points** (ruled). The atomic unit is the tool roundtrip:
an assistant turn and its complete result batch commit and rewind
together — you cannot cut in-between (partial writes from crashes or
aborts are repaired with synthesized results, never left half-open).
Checkout targets — and `base_id` values — are therefore
`user_message` entries, committed assistant turns, and `model_changed`
entries.

**Synthesized results tell the truth in their body.** A repaired tool
result's content is the sentence "tool execution was interrupted
before completing — the call may have had partial effects; verify them
before relying on anything it did". It is never a fabricated success;
render it like any tool result and the text says what happened. No
wire marker.

## 8. Interaction requests

The blocking pop-up shape, generic over permission prompts and
ask-the-user tools (one primitive, two askers — permission fires at
the tool's gate, ask-the-user tools ask from their bodies, and a tool
may ask repeatedly). Concurrent chains may hold several open requests
at once; answer them in any order.

```
← {"type":"interaction_request","stream":"019…","id":"019…","title":"Run command?",
   "body":"rm -rf target","options":[{"label":"Allow"},{"label":"Always allow"},
   {"label":"Deny"}],"free_text":true}
→ {"type":"interaction_response","session":"019…","id":"019…","option":"Deny","text":"never delete build dirs"}
```

`free_text: true` invites an optional explanation; when present it is
delivered to the model (a denial reason shapes the retry), not just
logged. Option objects are `{ label }` (a `description` field may
appear — display it when present). `interaction_response` carries at
least one of `option` (a label from the request) and `text` (the
free-text answer; an options-empty ask is answered by `text` alone).

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
  `turn_started`, `turn_committed`, `tool_result.entry_id`,
  `model_changed.entry_id`).
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
  drafts before restarting if you want them). Caveat: a fresh session's
  file materializes only at its **first user message** — if the
  backend died before any message drained, there is nothing on disk;
  restart falls back to a fresh session (a new id). Restarting with
  `--session`/`--continue` preserves the session's model (the log's
  last selection wins over config defaults) — no need to re-pass
  `--model`.

## 10. Limits and non-features (honest list)

- **No consumer backpressure yet.** A slow reader grows backend memory
  up to an arbitrarily high tripwire cap (breach = a runaway producer
  bug; the backend dies loudly). Keep reading. No line-size limits
  either way.
- **No replay pagination.** The whole active chain arrives per pass,
  however large. Cursors are a future addition.
- **No unload or residency limits.** Opened sessions stay loaded for
  the process's life (lazy loading bounds *startup*; LRU unload is
  deferred). Model discovery is the one-shot `tabit models --json`
  (§11 note); the channel catalog (`sessions_available`) is announced
  once at startup — re-scan via `tabit --list --json` if you need a
  refresh.
- **No event timestamps.** Entries carry wall-clock times in the
  session file; events carry none on the wire (§11).
- **One consumer.** Events go to the single connected frontend; no
  fan-out, no second attach.

## 11. Open questions (known and unsettled)

1. **Event timestamps.** Transcript UIs plausibly want per-entry
   wall-clock times on `user_message`/`turn_committed` at least.
2. **Backpressure.** What a stalled reader should experience — needed
   before long-running GUI use.
3. **Interaction edge semantics — settled with the permission
   milestone (§8):** run terminals close every pending request; stale
   responses are logged no-ops; requests never replay.
4. **Subagent streams.** Sibling `stream` ids and their event subset —
   reserved, unspecified.

Settled since the review: model discovery is a one-shot
`tabit models --json` at startup plus explicit reload (the session
listing pattern — no notifications); bad-flag exits stay
exit-1-with-stderr-no-frames while session/model startup failures
reject with plain-reason frames (§3.1 — the death-classification
pin); cut points follow the roundtrip-unit rule
(§7); synthesized tool results carry no marker (§7); durability is a
write-behind log with the prompt barrier — flag 8 fully resolved; all
non-terminal errors ride the generic `error { kind }` carrier (§6).
