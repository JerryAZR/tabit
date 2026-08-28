# rig-agent

`rig-agent` contains the agent runtime: builders, the driving loop, blocking
and streaming surfaces, the tool-phase hook pair, contextual tools, and
runtime integrations.

Most applications should use the root `rig` facade, where this runtime remains
enabled by default. Low-level provider and backend contracts live in
`rig-core`.

Direct users import construction and prompting explicitly:

```rust,ignore
use rig_agent::prelude::*;
use rig_core::{client::ProviderClient, providers::openai};

let client = openai::Client::from_env()?;
let agent = client.agent(openai.GPT_5_2).build();
let answer = agent.prompt("Explain ownership briefly.").await?;
```

## Models

High-level agents are concrete values: the provider model is erased once into
an opaque, cloneable `ModelHandle`. Provider authors still implement the typed
`CompletionModel` trait, and direct `completion` or `stream` calls (plus each
provider's `raw_*` escape hatches) retain their provider-specific behavior.

Replace the default on one agent value with `set_model` or
`set_model_handle`, or change one run's candidate with `using_model`. There
is deliberately **no runtime model routing**: which model answers is
configuration and session business (selection is resolved before the run; the
in-flight handle never rebinds mid-run). Concurrent runs keep independent
model and hook-stack snapshots.

Handles contain live clients, so they are deliberately not serializable;
persist an application model identifier and resolve it to a handle at runtime.
Clones share the retained model safely, while replacing an agent clone has
ordinary value semantics.

## Hooks

The hook surface is the tool pair (PROTOCOL.md flag 31): `on_tool_call` gates
each tool call — run as-is, rewrite the arguments, or skip with a reason the
model sees in-band — and `on_tool_result` observes each settled result and may
rewrite its presentation or stop the run after the batch. Hooks attached
through `AgentBuilder::add_hook` apply to every later runner from that agent;
hooks appended to a prompt or runner apply only to that run.

Portable tools implement `rig_core::tool::PortableTool` and work in both runtimes.
Classic tools that need mutable per-call state implement
`rig_agent::tool::Tool` and receive `&mut ToolContext`.

## Target support

| Tier | Target | Status |
| --- | --- | --- |
| 1 | native (linux / macOS / windows, `x86_64` and `aarch64`) | Full support, all features including `rmcp` |
| 2 | `wasm32-unknown-unknown` (browser) | Supported, with no feature flags to set; the `rmcp` feature is **not** available |
| — | `wasm32-wasip1` / `wasm32-wasip2` (WASI) | **Not supported** |
| — | `wasm32-unknown-emscripten` | Not supported |

**Building for `wasm32-unknown-unknown` is the entire opt-in** — there are no
wasm feature flags anywhere in the workspace. `rig-core` relaxes its
`WasmCompat*` bounds from the target alone.

Wasm gates name a `target_os` (`all(target_arch = "wasm32", target_os =
"unknown")`) rather than a bare `target_arch = "wasm32"`, because the latter
also matches WASI, which has no JS host. WASI itself does not build: `rig-core`
depends unconditionally on `reqwest`, which pulls `hyper`/`socket2` and a tokio
feature set WASI rejects. Supporting it would mean making `reqwest` optional and
adding a `wasi:http` client behind `rig_core::http_client` — a project, not a
`cfg` fix.

**`rmcp` is native-only.** rmcp's `ClientHandler` is declared
`Sized + Send + Sync + 'static` unconditionally — its `local` feature relaxes
the future bounds but not the handler itself — while this crate's handler owns a
tool registry whose `Arc<dyn ErasedTool>` is deliberately neither `Send` nor
`Sync` on wasm. Enabling `rmcp` on a wasm target fails with a single explanatory
`compile_error!` rather than a wall of trait errors.
