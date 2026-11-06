# tabit-agent

`tabit` — the alt-screen terminal frontend for the [tabit](https://github.com/JerryAZR/tabit) agent stack. The npm package is the complete app: the TUI plus the lockstepped `tabit-core` backend binary for your platform (installed automatically via per-platform optional packages).

## Install

```sh
npm install -g tabit-agent
```

Requires **Node.js ≥ 24.2**. Platforms: Windows x64, Linux x64 (macOS arm64 coming).

## Run

```sh
tabit        # new session
tabit -c     # resume the project's newest session
tabit --help # everything else
```

On a networked first run, the backend downloads the provider/model
catalog. Run `/login` in the TUI to choose a provider and store an API
key, then `/model` to choose a model. Keys live in
`~/.tabit/auth.toml`; custom and uncataloged providers are declared in
`~/.tabit/providers.toml`.

See the repository's [README](https://github.com/JerryAZR/tabit#readme)
and [provider/catalog design](https://github.com/JerryAZR/tabit/blob/master/PROVIDERS.md)
for headless use, custom providers, sessions, extensions, and the
protocol the frontend rides.

## License

MIT
