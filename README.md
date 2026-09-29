# tabit

A Rust agent framework and headless coding-agent backend, built from a
vendored, trimmed copy of [rig](https://github.com/0xPlaygrounds/rig) 0.41.0
(see [VENDOR.md](VENDOR.md) — the source is tabit's own now).

tabit is a study/research project in agent architecture. Its distinguishing
pieces: the native Rust stack, a **frozen-wire node model** every tabit
process shares (frontends, subagents, and extensions are all nodes over one
substrate), and durable, resumable agent sessions.

## Crate map

Foundation (vendored from rig 0.41.0):

- **`tabit-providers`** — Anthropic + OpenAI wire clients, streaming, and the
  portable completion/tool contracts.
- **`tabit-engine`** — the agent engine: turn loop, hooks, tool runtime.
- **`tabit-derive`** — the `#[rig_tool]` procedural macro.
- **`tabit-rig`** — one-dependency facade over the three above; hosts the
  provider cassette test harness.

The tabit stack:

- **`tabit-log`** — the durable conversation: entry vocabulary, tree, context
  manager, write buffer.
- **`tabit-config`** — provider/model configuration and settings layers.
- **`tabit-protocol`** — the frozen frontend-protocol vocabulary.
- **`tabit-wire`** — the node runtime: routing, asks, channels, child-process
  substrate.
- **`tabit-session`** — persistent agent sessions, compaction, skills,
  subagents.
- **`tabit-tools`** — the coding tools (`read`, `write`, `edit`, `bash`).
- **`tabit-gate`** — the default permission gate (heuristic policy, pure
  core).
- **`tabit-ext` / `tabit-ext-sdk` / `tabit-ext-install`** — the extension
  host, guest SDK, and installer.
- **`tabit-app`** — the composition root as a library: the opinionated
  assembly (toolset, extension world, gate) an embedder mounts.
- **`tabit-core`** — the headless backend binary (print mode + the JSON
  protocol edge).

## Using it

- Build your own agent app in-process: **[EMBEDDING.md](EMBEDDING.md)**
- Drive `tabit-core` as a subprocess (frontends): **[FRONTEND.md](FRONTEND.md)**
- Write an extension: **[EXTENSIONS.md](EXTENSIONS.md)**
- Architecture and the engine's state machine: **[AGENTS.md](AGENTS.md)**,
  **[ENGINE.md](ENGINE.md)**

## Development

The green gate (offline; CI runs it on Ubuntu + Windows):

```sh
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace --no-fail-fast   # or scripts/test.sh --gate
```

Provider behavior is covered by cassette replay — no live network in default
runs.

## License

MIT — see [LICENSE](LICENSE). The four crates vendored from rig additionally
carry upstream's MIT copyright notice (Playgrounds Analytics Inc.) in their
own LICENSE files.
