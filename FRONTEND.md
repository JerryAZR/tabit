# FRONTEND.md — the tabit frontend contract

This is the contract for anyone building a UI on top of the tabit
backend: what the backend provides, what it expects from you, and the
invariants your UI can rely on. Read this document alone; you should
not need the codebase to design a frontend.

Wire shapes below are the **v2 contract**. The shipped binary today
speaks v1 — no ids on events, and no replay/checkout/model/queue
events yet; v2 lands as one protocol-version bump with no
compatibility period. Design against v2.

## 1. Architecture: two processes, one pipe

You spawn the backend; you never link it as a library.

```
tabit --json [--continue | --session <path>] [--model <ref>]
```

- **stdout** carries protocol lines to you; **stdin** takes protocol
  lines from you; **stderr** carries human-oriented diagnostics you may
  ignore (never protocol data).
- **One backend process per session.** The session is chosen at spawn:
  `--continue` resumes the project's newest session, `--session <path>`
  a specific file, neither starts a fresh one. There is no
  create/list/switch-session command on the long-lived channel.
- **Local or remote, same edge.** Locally the backend is a child
  process; remotely it is the same child spawned on the far side of
  `ssh` with stdio forwarded. Nothing in the protocol distinguishes the
  two.
- **Crash isolation is the point.** Internal backend errors panic by
  design — the process dies loudly rather than running broken. Your UI
  must survive that: detect the closed stream, offer to restart the
  session, and recover the transcript via startup replay.
- To present a session picker, run the one-shot listing:
  `tabit --list --json` (spawn, read, exit — works over ssh unchanged).

## 2. Wire format

Every line in both directions is one UTF-8 JSON object, LF-terminated.
Discrimination is by `type` tag; no JSON-RPC envelope, no request ids,
no command responses — commands are fire-and-forget and their outcomes
arrive as events.

```
→ {"type":"initialize","protocol_version":2}
← {"type":"initialize_ack","protocol_version":2,"session_id":"…","session_path":"…",
   "model":{"provider":"…","model":"…","thinking_level":null}}
← {"type":"replay_started","stream":"main","total":14}
← … the transcript as finalized events …
← {"type":"replay_done","stream":"main"}
→ {"type":"message","text":"who are you?"}
← {"type":"message_queued","stream":"main","id":"019…","text":"who are you?"}
← {"type":"turn_started","stream":"main","id":"019…"}
← {"type":"text_delta","stream":"main","turn_id":"019…","text":"I'm "}
← {"type":"text_delta","stream":"main","turn_id":"019…","text":"tabit."}
← {"type":"turn_committed","stream":"main","id":"019…"}
← {"type":"run_finished","stream":"main","output":"I'm tabit.","usage":{…},"durable":true}
```

Every event frame is **flat**: the event's `type` and payload fields
sit next to `stream`. The `stream` stamp attributes concurrent
producers; today every frame carries `"main"`. **Ignore frames with
unknown `stream` values** (render nothing) and **ignore unknown event
`type`s** (skip the line) — both are forward-compatibility paths
(subagent streams and new events arrive in later versions without a
version bump).

## 3. Handshake and lifecycle

1. Your **first line** must be `initialize { protocol_version }`.
   - Match → `initialize_ack` with the session facts (id, path, active
     model), then — if you asked `{ "replay": true }` — the transcript
     replay pass, then live traffic.
   - Mismatch → `initialize_rejected { reason }` and the process exits.
2. A command sent before `initialize`, or any unparseable line, gets
   `protocol_error { message }`; **the connection stays open**. This is
   the only "error response" in the protocol.
3. To shut down: close stdin. The backend finishes any in-flight run
   (emitting its terminal event), then the event stream reaches EOF and
   the process exits 0. Close is not a kill.
4. Exit codes: `0` clean; `1` handshake rejection or a broken pipe.
   A backend that dies mid-run exits nonzero — distinguish this from a
   clean EOF before offering "restart session".

## 4. The model: runs, turns, steers, and the tree

- A **run** (outer loop) starts when a message is drained while idle:
   model turn → maybe tool calls → tool results → next model turn …
   until a turn with no tool calls. `run_finished` / `run_aborted` /
   `run_failed` end a run; exactly one terminal per run.
- A **message sent while a run is live is a steer**: it is
  acknowledged immediately (`message_queued`) and enters the
  conversation at the next turn boundary (`user_message`). It is never
  lost — the only exits from pending are draining and discard.
- **abort** preempts the run at the next await (`run_aborted` carries
  whatever assistant text had arrived) and clears every pending
  message (`messages_discarded`).
- The session is an **append-only tree** of entries. You render the
  **active chain** (one root-to-leaf path); `checkout` moves the leaf
  to any entry in the tree — including one on an abandoned branch —
  and the next append becomes a new branch (`git checkout <hash>`, not
  "rewind n").
- **You hold the active branch only.** The rest of the tree is backend
  truth; if you ever offer branch browsing, it comes as a backend
  query, not local state.

## 5. Commands

All commands are total — there is no rejection. Outcomes are events.

| command | when | effect |
|---|---|---|
| `message { text }` | any time | idle: starts a run; running: steers at the next turn boundary. Immediate ack: `message_queued { id, text }`. |
| `abort` | any time | running: preempts (`run_aborted`); always clears pending messages (`messages_discarded`). |
| `checkout { entry_id }` | idle only | moves the active chain to any entry; see §7 for the follow-up events. Compose abort-then-checkout if a run is live. |
| `model { provider, model, thinking_level? }` | idle only | the next run uses this selection; validates against the backend's config. |
| `interaction_response { id, option, text? }` | after an `interaction_request` | answers a pending request; see §8. |

`checkout` and `model` are idle-only by convention, not by error: a
frontend derives idle/run state from events (§6), and simply holds the
command until the terminal, or aborts first.

## 6. Events

`initialize_ack`, `initialize_rejected`, and `protocol_error` are
unstamped control frames; everything else is a stamped event.

**Queueing and transcript**

| event | payload | when |
|---|---|---|
| `message_queued` | `id`, `text` | the moment `message` is accepted. `id` is the message's entry id, minted here. |
| `user_message` | `entry_id`, `text` | the message drains into a run (opening batch or steer boundary) and becomes history. |
| `messages_discarded` | `messages: [{ id, text }]` | every mailbox clear: abort (both its interleavings) and checkout. Salvage them as drafts; the backend keeps no copy. |
| `turn_started` | `id` | a model turn begins; `id` is the turn's entry id, minted here and reused at commit. |
| `text_delta` | `turn_id`, `text` | assistant text. Full-text once in replay. |
| `reasoning_delta` | `turn_id`, `id`, `reasoning` | model reasoning; `id` correlates blocks within the turn (a turn may interleave several). |
| `tool_call` | `turn_id`, `name`, `call_id`, `internal_call_id`, `arguments` | the model issued a complete tool call, before execution. `arguments` is the raw JSON string when parseable. |
| `tool_result` | `turn_id`, `entry_id`, `name`, `internal_call_id` | one tool body finished; its result committed. |
| `completion_call` | `turn_id`, `input_tokens`, `output_tokens` | one model request finished; usage is final for it. |
| `turn_committed` | `id` | the turn is durable history. Same id as `turn_started`. |
| `turn_retried` | `turn_id` | the turn was discarded before commit (e.g. malformed tool-call arguments); drop its provisional groups — a fresh `turn_started` follows. |
| `native_item` | `item` (opaque JSON) | a provider-native output the backend does not model. Render or skip. |

**Run terminals** (exactly one per run)

| event | payload | meaning |
|---|---|---|
| `run_finished` | `output`, `usage`, `durable` | the run completed. `durable: false` means persistence failed after completion — the conversation continues but the tail is not on disk; surface this. |
| `run_aborted` | `output` | aborted; `output` is the partial assistant text. |
| `run_failed` | `kind`, `message` | `kind` ∈ `provider` (stream/transport errors), `budget` (max turns), `stopped` (engine stopped early: hook terminate, empty conversation, malformed-tool-call retry exhaustion), `durability` (persist failure after `run_finished`). |

**Session navigation and configuration**

| event | payload | when |
|---|---|---|
| `checked_out` | `entry_id`, `base_id` | checkout succeeded. `base_id` is where your old chain and the new chain diverge (see §7). |
| `checkout_failed` | `message` | target entry does not exist in this session. |
| `model_changed` | `provider`, `model`, `thinking_level` | a `model` command (or startup) set the selection for the next run. |
| `model_error` | `message` | the selection did not resolve in config. |

**Replay** (brackets; content is finalized events from the catalog
above — full-text deltas, same ids as live)

| event | payload | when |
|---|---|---|
| `replay_started` | `total` | a replay pass begins (startup with `replay: true`, or after `checked_out`). `total` = entries to come. |
| `replay_done` | — | the pass ends; live traffic (or quiescence) follows. |

`usage` objects are `{ input_tokens, output_tokens, total_tokens,
cached_input_tokens, cache_creation_input_tokens }` (u64 each; cache
fields are accounting breakdowns, `total = input + output`).

## 7. Replay and checkout: how transcript state moves

**Startup replay.** Send `initialize { protocol_version, replay: true
}`. After the ack: `replay_started { total }` → every context entry of
the active chain as finalized events, in chain order (`user_message`
per user entry; per assistant entry: `turn_started`, full-text deltas,
its `tool_call`s and `tool_result`s, `completion_call`,
`turn_committed`; `model_changed` for model-change entries) →
`replay_done`. Branch siblings are excluded by construction. Replayed
ids are the log's ids — identical to what a live consumer of the same
history would have seen.

**Checkout.** `checked_out { entry_id, base_id }` names the divergence
between your current chain and the new one. Then:

1. Drop every group you hold **after `base_id`**.
2. Apply the `replay_started` … `replay_done` pass that follows — it
   contains exactly the suffix you never had (the new chain from
   `base_id` to `entry_id`).
3. When the target is an ancestor of your leaf (the common "rewind"),
   the suffix is empty: nothing follows `checked_out` but the (empty)
   brackets.

Checkout also clears pending messages (`messages_discarded`) — steers
belong to the context they were sent into; the user decides what to
resend onto the new branch.

## 8. Interaction requests (reserved)

The blocking pop-up shape, generic over permission prompts and future
ask-the-user tools. One at a time; the run is paused until answered.

```
← {"type":"interaction_request","stream":"main","id":"019…","title":"Run command?",
   "body":"rm -rf target","options":[{"label":"Allow"},{"label":"Always allow"},
   {"label":"Deny"}],"free_text":true}
→ {"type":"interaction_response","id":"019…","option":"Deny","text":"never delete build dirs"}
```

`free_text: true` invites an optional explanation; when present it is
delivered to the model (a denial reason shapes the retry), not just
logged. This surface is reserved — no current tool triggers it — but
build the widget against this shape.

## 9. Invariants you may rely on

- **The message ledger.** Every `message_queued { id }` ends in
  exactly one of `user_message { entry_id: id }` or an entry in
  `messages_discarded` — never both, never neither, including across
  abort and checkout.
- **The turn ledger.** Every `turn_started { id }` ends in
  `turn_committed { id }`, or is discarded (`turn_retried { turn_id }`
  or a run terminal without commit). Discarded ids are never reused.
- **One live turn at a time** per stream. Deltas arrive strictly in
  transcript order; a run's terminal arrives after all of its events.
- **Ids are backend-minted UUIDv7 strings.** You never generate ids;
  you learn them from events (`message_queued`, `turn_started`) or
  from replay anchors (`user_message.entry_id`,
  `turn_committed.id`, `tool_result.entry_id`).
- **Ordering is total.** One connection, one ordered stream. There are
  no concurrent event sources today (the `stream` stamp is forward
  preparation).
- **Recovery is replay.** After a backend crash or restart, the same
  initialize-with-replay gives you the whole active chain with the same
  ids. Your transcript state rebuilds exactly; only pending messages
  are lost (they were never history — salvage them as drafts before
  restarting if you want them).
- **Idle/running is derivable**: running from the first `user_message`
  of a run until its terminal event; idle otherwise. Startup (after
  `replay_done`) is idle.

## 10. Limits and non-features (honest list)

- **No backpressure yet.** The backend buffers unboundedly for a slow
  reader; keep reading (drain into your UI queue) or memory grows.
- **No replay pagination.** The whole active chain arrives at startup,
  however large. Cursors are a future addition.
- **No session management on the channel.** Listing is the one-shot
   `tabit --list --json`; switching sessions is spawning another
   backend.
- **No event timestamps.** Entries carry wall-clock times in the
  session file, but events do not carry them on the wire — see open
  questions.
- **One consumer.** Events go to the single connected frontend; there
  is no fan-out or event replay to a second attach.

## 11. Open questions (known and unsettled)

1. **Event timestamps.** A transcript UI plausibly wants at least
   per-entry wall-clock times (`user_message`, `turn_committed`).
   Nothing carries them today; adding them is a wire addition.
2. **Backpressure.** What a stalled reader should experience (block
   the model? drop deltas?) — needed before long-running GUI use.
3. **Subagent streams.** Sibling `stream` ids and their event subset —
   reserved, unspecified.
4. **`--list --json` shape.** One-shot, but its row schema is not
   written down yet.
