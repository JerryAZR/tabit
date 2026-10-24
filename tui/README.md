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

The backend is configured through `~/.tabit/providers.toml` — see the [repository](https://github.com/JerryAZR/tabit) for provider setup, sessions, extensions, and the protocol the frontend rides.

## License

MIT
