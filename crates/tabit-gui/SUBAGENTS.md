# Subagent events — a frontend handoff (protocol v5)

What your frontend sees when the model calls the `subagent` tool.
The contract rows live in FRONTEND.md (`session_opened`,
`tool_result.details`); this walks through the live wire, event by
event, and names the assumptions that are safe and the ones that are
not. Reference implementation of the assertions: the e2e test
`a_subagent_answers_as_the_tools_result_and_streams_its_own_events`
(tabit-session, `subagent_tests.rs`).

The model-facing tool takes optional `model` ("provider/model"),
`cwd` (scope the child elsewhere — its tools *and* its instructions
follow), and `tools` (an allow-list; e.g. `["read", "bash"]` for a
read-only researcher). All three ride the `tool_call` arguments when
present — render them in the call row if you show arguments.

## The shape of a subagent run

A subagent is a **second session** running inside the parent's tool
call. From the wire's point of view: a new stream appears mid-run,
runs a whole conversation of its own, and ends — while the parent's
transcript shows only one `tool_call`/`tool_result` pair.

```
parent stream                          child stream (own stamp)
──────────────────────────────         ──────────────────────────────
user_message      "go"
turn_started
tool_call         name="subagent"
                  arguments={"task": "..."}
                                       session_opened  parent=<parent id>
                                                       path=""   (ephemeral)
                                       user_message    "..."  (the task)
                                       turn_started
                                       text_delta …    (its thinking)
                                       tool_call       (its own tools!)
                                       tool_result
                                       run_finished    output="…"
tool_result        name="subagent"
                   content=<child's final answer>
                   details={child_id, outcome, turns, usage}
run_finished
```

## What to key on

- **`session_opened.parent` is set** → this is a subagent child, not a
  user session. It must never touch your session facts, never create
  a switcher row, never count as "resumed". (The interim reducer
  bridge already branches on this — keep that branch when you build
  the real view.)
- **`path` is empty** → ephemeral. There is no file, no catalog entry,
  nothing to `open_session`. It will never appear in
  `sessions_available`, live or later.
- The child's stream id is the `id` from its own `session_opened`, and
  it equals `details.child_id` on the parent's tool result — that is
  your correlation key for nesting the child transcript under the
  parent's `subagent` tool_call row.

## `details` for `name == "subagent"` (success only)

```json
{
  "child_id": "019…",
  "outcome": "completed",
  "turns": 3,
  "usage": { "input_tokens": 4200, "output_tokens": 180, "total_tokens": 4380 }
}
```

The `content` field is the child's final answer, verbatim — render it
like any tool output. **Failure has no details**: a failed child is a
`tool_result` with `status: failed` and the reason in `content`
("the subagent failed: …"). **Abort has no tool result at all** —
the interrupted roundtrip never folds; the parent's `run_aborted` is
the truth, and the child's own `run_aborted` (usually) precedes it.

## Safe assumptions

- **Within a stream, order is total** — the child's events arrive in
  conversation order exactly like any session's.
- **The announcement precedes every child event.** No frame on the
  child's stamp arrives before its `session_opened`.
- **The child's `run_finished` is sent before the parent's
  `tool_result` is produced** (the tool returns only after the child's
  pump ends). Treat *cross-stream* ordering as unspecified anyway —
  both streams interleave freely on one channel, exactly like two
  opened sessions already do.

## Unsafe assumptions (do not build on these)

- **Replay**: never. Ephemeral children do not project — a replayed
  parent shows the `subagent` tool pair and nothing of the child's
  transcript. Live-only.
- **A terminal per child on abort**: best-effort. On parent abort the
  child's `run_aborted` usually rides the wire, but the parent is the
  authority; don't wait on the child's terminal to unblock anything.
- **Interaction provenance**: a child's permission ask is proxied
  through the **parent's** hub — the `interaction_request` arrives
  stamped with the parent's stream id and looks exactly like the
  parent's own ask. v1 gives you no way to tell which session's tool
  is asking. Answer it on the parent stream, as usual.
- **Tool vocabulary inside the child**: unrestricted — the child
  calls `read`/`bash`/… like anyone else; its `tool_call` /
  `tool_result` frames carry `details` per the normal per-tool shapes
  (edit's diff, bash's spill, …).

## Suggested rendering (v1 GUI)

A collapsible group under the parent's `subagent` tool_call row,
keyed by `child_id`: title = the task (from the tool_call arguments),
body = the child stream's transcript (same renderers as any
transcript), footer = `turns` + usage from `details`. Unknown
`session_opened` parents or unannounced streams degrade to nothing —
today the reducer already drops them, so shipping the nested view is
purely additive.

## Not in v5 (design notes, do not assume)

Persisted children (a real file, catalog presence, replay via
`open_session`, `parent_session` lineage), per-child model/cwd
overrides, and a result-size cap beyond the child's own `max_tokens`
are deferred — see ROADMAP item 5. Nothing here promises any of them.
