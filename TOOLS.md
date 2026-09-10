# TOOLS.md — the built-in tool and interaction shapes

The companion to FRONTEND.md for the interpretation layer: what a
frontend does with a **specific** tool's `tool_result.details` cargo,
and with the interaction templates the built-in askers use. FRONTEND.md
owns the mechanics — frames, streams, the event vocabulary, the generic
`tool_result` / `interaction_request` contract; this doc owns the
shapes keyed by `tool_result.name` and `interaction_request.ui_type`.

The rule both docs share (FRONTEND.md §6): **dispatch on the name,
degrade to generic rendering when the shape is absent or unknown** —
structure never gates function, `content` always renders on its own.
`details` is derived presentation cargo — structure only, never prose;
the human-readable story lives in `content`, which stays the faithful
copy of what the model saw.

## `tool_result.details` by tool name

Today's producers — `read` and `write` emit no cargo (their `content`
is the whole story); unknown names have no cargo by definition.

### `edit`

The unified diff of the applied edit batch, plus per-edit outcomes.

```json
{
  "diff": {
    "first_changed_line": 2,
    "hunks": [{
      "old_start": 1, "old_lines": 3,
      "new_start": 1, "new_lines": 3,
      "lines": [
        { "kind": "context", "text": "alpha" },
        { "kind": "removed", "text": "beta" },
        { "kind": "added",   "text": "BETA" },
        { "kind": "context", "text": "gamma" }
      ]
    }]
  },
  "outcomes": [{ "index": 0, "applied": true }]
}
```

- `hunks[].lines[].kind` is `context` | `removed` | `added`; render as
  a diff view. `first_changed_line` is 1-based, for scroll-to.
- `outcomes[i].applied` is per edit in the order the call listed them
  — a partial application (some `false`) means the rest are reported
  by index: the model fixes and resends only the failed ones, and a
  renderer marks exactly those.

### `bash`

Cargo only when output was truncated and spilled; a clean or small
run has none.

```json
{
  "truncated": true,
  "output_lines": 200,
  "total_lines": 5403,
  "omitted_lines": 5203,
  "total_bytes": 190112,
  "spill_path": "C:\\Users\\…\\AppData\\Local\\Temp\\tabit-bash-out-…"
}
```

`content` carries the visible head; `spill_path` names the temp file
holding the full output — link or open it, the frontend reads the file
itself. A failed command has **no** cargo: the exit code rides
`status: { "failed": { "exit_code": n } }` and the prose (already in
`content`) names it; signal kills have no code at all.

### `subagent`

The child-session facts of one delegation, on the completed arm.

```json
{
  "child_id": "0192uuidv7child",
  "outcome": "completed",
  "turns": 3,
  "usage": { "input_tokens": 9, "output_tokens": 40, "total_tokens": 49 }
}
```

- `child_id` is the child's **session id** — the same value that
  stamps the child's whole event stream and that its `session_opened`
  announce carries as `id`. The result is the post-hoc half of the
  pairing; the announce is the streaming half:
- **The pairing.** The child's `session_opened` carries `parent`
  (the spawning session) and `parent_call` (the spawning tool call's
  `internal_call_id`) — so the announce, which fires at child boot
  before any run, pairs the child with the exact open `tool_call`
  event the frontend already holds. Exact under concurrent subagent
  calls, where arrival order cannot disambiguate. The aborted/failed
  arms return error text with no cargo — the announce already made
  the link, so nothing is lost.
- `turns` counts the child's `turn_started` events; `usage` is the
  child's aggregate across its run (same object shape as
  `run_finished.usage`).

## Interaction templates

`tabit-protocol::templates` owns the names and payload schemas; they
are prebuilt asks, not core types — the core's interaction vocabulary
is routing only (`id`, `ui_type`, opaque payload; FRONTEND.md §8 owns
the frame-pair mechanics and the closing rule). Two widgets cover the
native surface (2026-09 ruling — there is no separate confirm or ask
card; both were special cases of these and keeping them named would
mislead future developers into believing in a special UI that does
not exist):

- `native:select_one` — given multiple choices, select exactly one,
  with optional free text. Request `{ title, body, options: [{ label,
  description? }], free_text }`, answer `{ selected: [label], text? }`.
  **The permission gate's allow/always/deny card is this template
  with its own labels.**
- `native:select_any` — given multiple choices, select zero or more,
  with optional free text. Request `{ title, body, options: [{ label,
  description? }], free_text }`, answer `{ selected: [label, ...],
  text? }` (0..n). With zero options given this is the free-text ask;
  **the `ask_user` tool is this template** (it may pass options when
  the model offers choices).

Both share the one `SelectAnswer` shape: `selected` echoes the chosen
labels (exactly one for `select_one`), `text` carries the free-text
answer when invited. A `free_text` answer is delivered to the model
when present (a denial reason shapes the retry), not just logged.

`ui_type` namespaces: `native:*` renders in every conforming
frontend; `ext:<id>:*` types arrive with extensions. Unknown types:
report, don't swallow — surface a notice, never fabricate an answer
(the same rule FRONTEND.md §2 states for unknown events).
