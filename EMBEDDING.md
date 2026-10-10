# EMBEDDING.md — building your own agent app on the tabit stack

The in-process contract for embedders: the library tiers beneath the
`tabit-core` binary. The binary is one consumer (argv in, two I/O
arms out); this doc is for everyone else — your app, your I/O, the
same stack. For the subprocess world (frontends, the frozen wire),
read FRONTEND.md; for packages, EXTENSIONS.md; for the architecture,
AGENTS.md. Native targets only — the tabit crates are
filesystem-backed by design (the rig crates beneath keep wasm).

## The three tiers

1. **A session, one call** — `tabit-session`: durable (or ephemeral)
   conversation, tools, compaction, skills. You drive runs and read
   the whole summary.
2. **A session host, streaming** — `tabit-session`: events as they
   happen, interaction cards, abort and steering through commands.
   The binary's print mode is a complete example of this tier.
3. **The composition** — `tabit-app`: the opinionated assembly (the
   default toolset, the extension world, the gate, the host data) as
   presets over `AppOptions`. This is what `tabit-core` mounts; an
   embedder mounts the same to get a batteries-included agent
   without writing the glue.

## Tier 1 — a session in five lines

```rust
let config = Arc::new(TabitConfig::from_toml_str(PROVIDERS_TOML, Path::new("providers.toml"))?);
let auth = Arc::new(AuthConfig::default());
let store = SessionStore::project_default();
let mut session = SessionBuilder::new(store, config, auth, Some(ModelSelection::new("p", "m")))?
    .ephemeral("C:/work/project")?;              // NullBuffer: nothing touches disk
let run = session.prompt("explain this repository").await;
println!("{}", run.output);
```

`prompt()` awaits the whole outer loop (every turn, every tool
roundtrip) and returns the summary; `prompt_with(prompt, &mut |event|
…)` is the same run with a live per-event observer. Everything else
on the builder is opt-in: `dynamic_tool`, `hooks`, `subagents`,
`skills`, `max_turns`, `model_factory` (the escape hatch from
tabit-config construction). `examples/minimal.rs` is this tier,
runnable.

**Async contracts worth knowing:**

- **Cancel via abort, never by dropping the future.** The sanctioned
  cancel is the abort path (the host's `Abort` command, or the
  cancellation token through the engine) — it lands a `run_aborted`
  terminal and the pump returns cleanly. Dropping a `prompt()`/
  `prompt_with()` future mid-run is unsupported: the mailbox's run
  bookkeeping is left open and the run has no terminal.
- **Commits are write-behind.** Each tool-use roundtrip commits into
  an outbox and the flush is attempted inline (KB-scale; the executor
  thread blocks for the write's duration — deliberate, invisible at
  this scale). A failed flush is not fatal: the entries stay pending
  in memory, every later commit retries them, and
  `persist_degraded`/`persist_recovered` notices report the state —
  only a force-stop while degraded loses the pending entries.

## Tier 2 — the session host, streaming

`SessionHost::spawn(session, notes, wiring, data)` gives you the
handle both binary arms ride: `handle.message(...)` submits,
`handle.next_event().await` streams stamped events, and
`handle.command_link()` sends commands — `Abort` mid-run,
`InteractionResponse` to answer a card the gate opened. Cards arrive
as `interaction_request` events carrying a `ui_type` and payload
(the templates live in `tabit-protocol`); an answer is one command.
`examples/host_cards.rs` drives one prompt end to end over the host,
buffers the response, and answers a `select_one` card from stdin.

## Tier 3 — the composition

`tabit-app` is the binary's assembly, linkable:

```rust
let options = AppOptions { tools: Some("read".into()), ..Default::default() };
// config by value; auth as Arc
let (registry, launchable) = world_registry(&options, config, Arc::new(auth))?;
let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
let mounted = mount_world(launchable, &runtime);
let (session, notes) = assemble(&options, &registry, &store, ContinueMiss::Fail, Some(mounted.clone()))?;
```

That is the whole boot: the default toolset behind your filter, the
extension world (scan, providers fragments, skills, the supervisor),
the permission gate, and the session — the same objects `tabit-core`
serves. `core_tools()`, `host_data()`, and `host_node()` fill in the
rest.

**The child role.** Subagent children self-spawn the current
executable with `--json` child-role flags (the "no exceptions"
ruling). Two ways to make that correct in your app:

1. Dispatch the child role in your `main` to
   `tabit_app::serve_json_stdio(&options, config_result, auth_result)`
   — your binary then speaks the frozen wire exactly as `tabit-core`
   does, and self-spawn just works. The function never returns; it
   owns the process exit (rejection frames and the served code).
2. Or leave `SubagentParts::exe` pointed at a real `tabit-core` —
   your app's children are tabit-core processes serving the same
   sessions model.

## Where the lines are

- `tabit-session` is mechanism with no policy — the gate, the
  toolset, the extension world are `tabit-app`'s opinions.
- The rig crates beneath (`tabit-providers`, `tabit-engine`) remain usable
  directly; `tabit_engine`'s `runner_over` is the upgrade path from a
  plain rig agent to a tabit session's durable conversation.
- Sessions are native-only (filesystem, OS entropy). Providers ride
  the rig stack and keep its portability.
