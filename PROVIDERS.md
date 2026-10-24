# PROVIDERS.md

The provider-support design: standard dialects only, a downloaded
model catalog, and the seams that replace provider-specific code.
**Status: implemented (core side), except where marked.** The
generator is a separate project thread.
Owner rulings this design encodes (2026-10):

- **Rule 1 stands, sharpened — the binary ships zero provider
  knowledge.** No model catalog (models churn weekly), no static
  provider catalog either (owner ruling: "I am against shipping a
  static catalog"). Provider and model knowledge arrive through the
  update channel and user config.
- **Standard dialects only, anthropic-messages preferred.** Providers
  implement the two dialects faithfully because the dominant clients
  (claude code, the OpenAI ecosystem) cannot send anything else —
  every provider the owner runs works in unmodified claude code.
  Compatibility is the provider's server-side problem; ours is a
  faithful engine.
- **No provider-specific code, anywhere.** No name-keyed branching;
  engines consume config data generically. The reference projects'
  quirk inventories are field reports from *library* user bases
  covering the long tail of janky servers — not from tabit; our own
  observed history contains zero provider deviations that needed
  engine code.
- **`extra_body` is deleted** at all three levels (provider, model,
  thinking-level). It was copied from the references unquestioned;
  the claude-code evidence shows providers must accept standard
  request shapes, and "a user might need a day-zero parameter" is a
  hypothetical justification — dead weight under rule 12 discipline.
  `headers` and `sampling_params` stay: typed channels, not
  arbitrary-fields hatches.
- **Sync by ownership, enforced at generation time.** The generator
  owns provider entries and their models together; a provider with
  zero models fails the generator's build (details below).

## Thinking levels: plain strings, natively realized

`thinking_levels` becomes a list of **plain level names** — the levels
the model supports; omitting the list means "provider default," and
"off" needs no listing (it is the omitted thinking field, which every
provider must accept). The engines realize levels natively, mapping
the canonical level to the dialect's standard effort field
(`output_config.effort` on anthropic-messages, `reasoning_effort` on
openai-completions). A level outside the dialect's documented enum is
a loud error at request build, never a silent drop.

No per-provider thinking knowledge is maintained anywhere: the level
vocabulary is the dialect's enum, compat endpoints accept the standard
enum because the dominant clients send nothing else, and a model entry
needs only `reasoning: bool` (which model databases already carry) for
the catalog to attach the standard level list mechanically. A provider
that rejects a standard level is an observed deviation — the quirk
seam's growth rule applies.

**Open question (generator thread): level-list fidelity.** The
standard level list works because providers map the dialect's enum
to their own internals — but that mapping is not one-to-one (a
provider may map "medium" onto "high"), so the standard list can
advertise a distinction the provider does not honor. Mostly
harmless, not elegant (owner, 2026-10). The resolution direction: if
the generator can learn a provider's *actual* level vocabulary, the
catalog carries the true list per provider and the dialect enum is
the fallback when nothing better is known. To be discussed with the
generator's design.

## The quirk seam (empty vocabulary)

Each engine owns a `Quirks` data struct (tabit-providers, next to the
engine): typed fields, serde `Default` = the dialect's documented
standard, `deny_unknown_fields`. **The initial vocabulary is empty** —
an empty struct with `deny_unknown_fields` is itself correct behavior:
any quirk key errors loudly until the vocabulary exists. Growth rule:
a quirk field is added when a deviation is observed in our own traffic
(the debug log is the observation instrument) or is required by a
catalog entry the generator ships — never imported from a survey.
Adding a field is additive and minor.

`Provider` config carries `quirks: Option<toml::Table>` opaquely;
tabit-config stays a pure-data crate (the WireApi precedent). The
registry parses the table into the engine's struct at client
construction with path-precise errors at boot — a loud config error,
never a silent ignore.

Not a quirk, ever: a behavior with no correct alternative. Preferring
a nonzero `input_tokens` on the terminal `message_delta` over
`message_start`'s (rig 9b9c428e) is a bug fix and lands as one.

## The catalog channel

The catalog — provider entries (endpoint, dialect, auth env, display
name) plus their models (ids, costs, context windows, modalities,
reasoning flag) — is **generated externally and downloaded**; nothing
is compiled in. The local manager:

- At startup, if the cache is absent or older than its TTL (24h,
  config-overridable), the refresh spawns right after settings load
  and the boot **bounded-waits** on it after the extension handshakes
  (the slowest boot phase hides the network): `pointer_budget_ms`
  (default 1s — the pointer is a 100-byte fetch) plus
  `artifact_budget_ms` (default 2.5s). Within budget, a new catalog
  applies to THIS boot; over budget the task detaches, keeps
  downloading, and installs for next boot (the `pending` wire
  status). A check in flight is announced on the wire
  (`catalog_status`, v26); the resolved state and cache version ride
  `providers_available.catalog`. Subagent children never refresh —
  the parent's boot owns the network; they merge the cache as-is.
- Fetches a small mutable `latest.toml` (version + sha256) from an
  ordered mirror list (`catalog.urls` in settings.toml,
  config-overridable; defaults
  chosen for complementary failure modes: jsDelivr first — reliable
  from China — then GitHub raw). Version match with a verified
  artifact on disk: done (the TTL restarts). Else downloads the
  **immutable versioned artifact** `catalog-<version>.toml`
  (cache-forever URL, sidesteps CDN branch-cache staleness), verifies
  the sha256, and writes `~/.tabit/providers.auto.toml` atomically
  (temp + rename) under an OS sidecar lock (`std::fs::File::lock` —
  the session-store pattern; a concurrent session that loses the lock
  skips, since the winner just wrote fresh data). Each artifact fetch
  is bounded (`artifact_timeout_ms`, default 60s — a mirror that
  connects then stalls fails over like any other failure, never
  parks the refresh task holding the lock) and size-capped (32 MiB).
- The freshness invariant: the state sidecar and the artifact are
  checked TOGETHER — a stamp only counts while the artifact it
  vouches for exists and parses (a deleted or corrupt cache
  re-downloads rather than reporting fresh forever), and a stamp in
  the future is clock skew (suspect — re-check, never laundered into
  fresh by saturating arithmetic).
- `catalog.auto_update = false` disables the channel outright (the
  one network posture this adds; read-only and failure-soft).
- Channel down, artifact corrupt, or unknown schema major version:
  keep the last cache, warn. User config is untouched and always wins.

`providers.auto.toml` is an ordinary providers fragment (with a
generated-file header), merged by the one existing fragment path.
Precedence: user `providers.toml` → extension fragments →
`providers.auto.toml`. It never *creates* support: provider entries
in it are inert data (`base_url`, `api`, auth env name) that resolve
against the standard engines like any config.

**Uncataloged providers are user-declared, as today.** Live
`/models` listing was ruled out (2026-09, ROADMAP): listings return
ids only — no context windows, no costs — not useful enough to ship.
A provider the downloaded catalog doesn't know (a local server, a
user-declared endpoint) gets its models from `providers.toml` exactly
as before.

**Merge granularity (as implemented).** A provider the config already
has merges at MODEL granularity: catalog models fill in under it, and
a model id the config declares wins WHOLE. The design's finer
"annotates unset fields" rule died on a real ambiguity: post-parse, an
explicit `reasoning = false` is indistinguishable from an absent key,
so field-level annotation needs a value-level config pipeline — not
worth it. Whole-model wins are explainable and sufficient: with the
catalog providing full entries, a sparse user entry is the user's own
choice (delete the entry to get the catalog's).

**The built-in mirror list ships the generator's coordinates**
(`catalog.rs::BUILTIN_MIRRORS`): jsDelivr first (reliable from China),
GitHub raw second. An empty `catalog.urls` means the built-ins; a
configured list REPLACES them — the explicit-pointer rule, never a
silent fallthrough. The generator publishes a `catalog` branch: the
default list at the root, variant lists (cn, global, openrouter) in
subdirectories — a subdirectory is just another base URL, so a
variant is chosen by pointing `catalog.urls` at it (single-select;
multi-list union was discussed and deferred as unneeded).

## The generator

Lives at `~/Projects/agents/tabit-catalog` (own repo, own CI cadence;
deps `tabit-config` from crates.io and round-trips every artifact
through `TabitConfig::from_toml_str` before stamping — the consumer
is the schema authority). Its CI publishes the `catalog` branch on
push and daily. What it provides — the contract the core side
consumes:

- `latest.toml`: `{ version, sha256, min_schema }`; the artifact
  `catalog-<version>.toml`.
- Catalog content: provider entries (`base_url`, `api`, `api_key_env`,
  display name) with their models (`id`, `cost`, `context_window`,
  `input` modalities, `reasoning`), sources merged (models.dev +
  provider listings), schema-versioned.
- The sync invariant at build time: a provider entry with zero models
  fails the generator build; a model under no provider entry is
  unrepresentable by construction.

How the generator is implemented (source polling, scheduling, mirror
publication) is designed in the generator's project space, not here.

## Config schema changes

- `extra_body` deleted (provider, model, thinking-level) — migration:
  thinking levels rewrite to plain names; shapes the levels carried
  are realized natively (or were provider-specific crutches the
  standard dialect makes unnecessary, per the claude-code evidence).
- `Provider.model_defaults`: any `Model` field as provider-level
  defaults, overridden field-by-field by the model entry — the fix
  for duplicated per-model blocks (the owner's glm thinking blocks
  were the exhibit).
- `thinking_levels`: plain strings (above).
- `[catalog]` settings: `urls` (empty = the built-in mirrors), `auto_update`, `ttl_hours`.

## Validation: how sync is enforced locally

At config load (boot and refresh share one path): every model entry's
provider resolves (loud error, as today); every quirk key parses
against the provider's engine (loud error); a provider with zero
models from all sources warns, visible in the picker — never blocks.

## Rule 1, reworded for AGENTS.md

> **API abstraction only — the binary ships no provider or model
> knowledge.** No model-name constants, no name-keyed branching
> anywhere. Provider and model knowledge enter at runtime: user
> config, extension fragments, the downloaded catalog (PROVIDERS.md).
> The two dialect engines are the whole of provider support;
> deviations are observed first, then named (the quirk seam).

## Explicitly out

- The extension contract is unchanged: fragments gain `quirks` and
  `model_defaults`; relays stay the answer for APIs that speak neither
  dialect.
- Generator internals (source polling, scheduling, mirror publication).
- Integrity beyond HTTPS + sha256 (minisign is the upgrade if ever).
