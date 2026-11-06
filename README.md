# tabit

A terminal coding agent and Rust agent framework, built from a vendored,
trimmed copy of [rig](https://github.com/0xPlaygrounds/rig) 0.41.0 (see
[VENDOR.md](VENDOR.md) — the source is tabit's own now).

tabit is a study/research project in agent architecture, not a pi
competitor. Its distinguishing pieces are the native Rust stack, a
**frozen-wire node model** every tabit process shares (frontends, subagents,
and extensions are all nodes over one substrate), and durable, resumable
agent sessions.

## Install and run

The primary app is the alt-screen terminal frontend, published as
[`tabit-agent`](https://www.npmjs.com/package/tabit-agent):

```sh
npm install -g tabit-agent

tabit        # start a new session in the current directory
tabit -c     # resume the current directory's newest session
tabit --help # backend override, diagnostics, and version flags
```

Requires **Node.js ≥ 24.2**. The npm package installs the lockstepped
`tabit-core` backend through per-platform optional packages. The current
release matrix is **Windows x64** and **Linux x64**; macOS arm64 is deferred
until it can be tested on a Mac.

### Provider setup

On a networked first run, tabit downloads its provider/model catalog; no
provider or model knowledge is compiled into the binary. In the TUI:

1. Run `/login`, choose a provider, and paste its API key.
2. Run `/model` to choose a model; `/thinking` selects a supported thinking
   level when the model has one.

Keys are stored in `~/.tabit/auth.toml`. Custom or uncataloged providers are
declared in `~/.tabit/providers.toml`; environment-based keys and local
keyless endpoints are supported too. See [PROVIDERS.md](PROVIDERS.md) for the
catalog and provider-configuration design.

## What ships

- **Terminal frontend (`tui/`)** — streaming transcript, tool cards,
  interaction prompts, model/thinking pickers, login/logout, clipboard image
  attachments, session-tree checkout, and focused subagent streams. Built on
  stock `pi-tui` and plain Node; see [tui/README.md](tui/README.md).
- **Headless backend (`tabit-core`)** — print mode for one-shot prompts and
  JSONL over stdio for frontends; see [FRONTEND.md](FRONTEND.md).
- **Embeddable Rust stack** — provider clients, the agent engine, durable
  sessions, compaction, skills, tools, permissions, extensions, and
  subprocess subagents; see [EMBEDDING.md](EMBEDDING.md).

For headless use without the TUI:

```sh
cargo install tabit-core
tabit-core -p "explain this repository"
tabit-core --continue -p "now summarize the important modules"
```

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
  attachments, and subagents.
- **`tabit-tools`** — the coding tools (`read`, `write`, `edit`, `bash`).
- **`tabit-gate`** — the default permission gate (heuristic policy, pure
  core).
- **`tabit-ext` / `tabit-ext-sdk` / `tabit-ext-install`** — the extension
  host, guest SDK, and installer.
- **`tabit-app`** — the composition root as a library: the opinionated
  assembly (toolset, extension world, gate) an embedder mounts.
- **`tabit-core`** — the headless backend binary (print mode + the JSON
  protocol edge).

## Documentation

- Terminal app installation and architecture: **[tui/README.md](tui/README.md)**,
  **[tui/DESIGN.md](tui/DESIGN.md)**
- Build your own agent app in-process: **[EMBEDDING.md](EMBEDDING.md)**
- Drive `tabit-core` as a subprocess: **[FRONTEND.md](FRONTEND.md)**
- Provider/catalog configuration: **[PROVIDERS.md](PROVIDERS.md)**
- Write an extension: **[EXTENSIONS.md](EXTENSIONS.md)**
- Tools, compaction, and the engine state machine: **[TOOLS.md](TOOLS.md)**,
  **[COMPACTION.md](COMPACTION.md)**, **[ENGINE.md](ENGINE.md)**
- Contributor rules and workspace layout: **[AGENTS.md](AGENTS.md)**
- Planned and deferred work: **[ROADMAP.md](ROADMAP.md)** and
  **[tui/ROADMAP.md](tui/ROADMAP.md)**

## Development

The Rust green gate is fully offline; CI runs it on Ubuntu and Windows:

```sh
bash scripts/test.sh --gate
# Equivalent individual legs:
cargo fmt --check
cargo clippy --workspace --all-targets
cargo test --workspace --no-fail-fast
```

Provider behavior is covered by cassette replay — no live network in default
runs. The TUI has separate strict typechecking and offline, mock-driven tests:

```sh
npm --prefix tui ci
npm --prefix tui run check
npm --prefix tui test
```

For a source-checkout run, build the backend and launch the TUI; its backend
resolver finds `target/debug/tabit-core` automatically:

```sh
cargo build -p tabit-core
node tui/src/cli.ts
```

## License

MIT — see [LICENSE](LICENSE). The four crates vendored from rig additionally
carry upstream's MIT copyright notice (Playgrounds Analytics Inc.) in their
own LICENSE files.
