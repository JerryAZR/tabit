# tabit-providers

Provider API clients for the **tabit** workspace: the completion,
message, and tool vocabulary plus the streaming machinery every tabit
model call rides.

This is upstream rig's `rig-core` (0.41.0), vendored and trimmed — see
`../../VENDOR.md` for the trim record. Only the **anthropic** and
**openai** providers remain, over the shared openai-compatible engine
in `providers/internal`; the other providers, telemetry, and the
vector-store surface are removed. Provider behavior is covered by
cassette replay (httpmock) — the suite runs offline; live-network
scenarios carry upstream-marked `#[ignore]`s.
