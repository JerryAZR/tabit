# tui-alt roadmap

The ordered work list for the alt-screen TUI (owner-ruled 2026-10).
Process: merge `master` into this worktree during development; merge back
to main as `tui/` when done. Follow pi's proven patterns (`reports/pi.md`,
the pi repo); **deviations need justification stated in advance**.

Protocol state: **v21** (synced; see FRONTEND.md's changelog).

## 1. `/model` picker — landed (f3d4699, v22 sync a7d2283)

The catalog crossed the wire in v21 (`models_available`, folded last-wins
in the mode). What remains is the UI.

**Pattern (pi)**: a selector that takes over the input area — pi swaps
its editor container; our dock already has the equivalent seam (the card
slot hosts interaction cards and the tree card, focus moves in and back
out). Search input + sparse rows + `Esc` cancels, `Enter` selects.

**Row shape (owner ruling, a justified deviation from pi's sparse
id+badge rows)**: three aligned columns —

```
✓ Mock Model          200k   mock
  claude-sonnet-4     200k   anthropic
```

- name: `model.name` ?? `id` (the contract's fallback rule); `✓` marks
  the session's current register.
- context window: compact count (`200k`), right-aligned; **blank when
  unstated** (the v11 rule: absent means unstated, never zero).
- provider: `provider.name` ?? `id`, dimmed.
- Column widths computed over the visible rows.

**Register outside the catalog** (post-`logout`, stale config): pi's
behavior, per owner ruling — no synthetic row; the `✓` simply doesn't
appear. The session's register stays visible in the footer and a
selection-less run fails loudly at open (`run_failed { kind: "model" }`),
so the UI never implies a usable current model that isn't. (FRONTEND.md
§6 suggests a synthetic picker entry; ruled unnecessary — the footer is
the current-state surface.)

**Ordering**: current first, then wire order (providers alphabetical,
models in config-file order — "display sorting is yours"; keep it
boring).

**Filter**: pi-tui's `fuzzyFilter` over `provider/id`, `id`, `name`,
provider name (pi's `model-search.ts` ranking: bare id not first, so
exact `provider/id` queries rank above proxy ids).

**Behavior**: `Enter` sends `model { session, provider, model }` with no
`thinking_level` (null = provider/model default); `model_changed` lands
the footer facts. Safe any time — a state write at receive; a run in
flight keeps its bound model. A pick from catalog rows cannot fail
validation (existence-only rule, §5).

**Deliberately out of v1** (each a later item if wanted): `/model <ref>`
direct switch; model cycling keybindings (pi's `cycleForward`); the
`/thinking` selector (pi splits it — thinking cycles `null` → the
catalog's announced `thinking_levels` names); showing cost/reasoning/
modality facts (the wire carries them; the owner-ruled row is three
columns).

## 2. `/login` (+ `/logout`)

The v21/v22 in-app auth path. Reuses #1's selector seam: `/login` lists
`providers_available`'s `auth: "none"` providers (the login widget's
targets; `env` rows are display-only — the app cannot unset a persistent
variable), a key prompt collects the secret, `login { provider, api_key }`
rides the wire, the re-announced `models_available` +
`providers_available` pair is the ack, `error { kind: "auth" }` the
failure. `/logout <provider>` is offered on `auth: "stored"` rows — one
command, idempotent.

**Decisions**: key input is **unmasked** (owner ruling — pi-tui's
`Input` has no password mode and shoulder-surfing is not the threat
model); the key must not land in the editor history or any note (the
wire's redaction ruling is backend-side; the frontend's copy discipline
is ours — the key lives only in the card's own `Input`, which is not the
history-keeping editor, and nothing echoes it post-submit). The
zero-config boot's first-run flow (empty catalog) should guide here.

## 3. Skill chips

Manual skill invocation (the wire's `<skill name="…"/>` tag) with the
chip UX: the editor shows `[skill: commit]`, backspace deletes the whole
chip atomically, submit expands to the tag — the wire never sees the
chip.

**Mechanism (pi-tui, confirmed)**: text marker + ID registry + the
editor's `segmentWithMarkers` wrapper fusing the marker into one
indivisible grapheme — cursor movement, wrap, hit-testing, and backspace
all go through the segmenter, so atomicity is inherent; undo snapshots
include the registry; submit-time expansion already exists for paste
markers (`[paste #1 +123 lines]`). A skill chip is a second marker rule.
**Open implementation question**: whether pi-tui 0.85.1 exposes marker
registration publicly or the editor needs extending (the scrollback
sibling already forks pi-tui — a known move).

Invocation UX: `/name` autocomplete against the session's skill catalog
inserts the chip (skills stop being display-only slash entries; the tag
format happens at submit).

## 4. Attachment blocks

Pasted/dragged images as chips in the editor (rides #3's mechanism).
**v0 needs no wire change** (pi's own path): clipboard image → temp file
→ a path chip in the prompt; the model reads it with the `read` tool
(already image-capable). Clipboard acquisition is the new machinery
(wl-paste / xclip / WSL powershell / pi-tui's native platform helpers —
pi's `clipboard-image.ts` is the reference). True inline image content in
`message` is a protocol decision with the backend — deferred. The
catalog's per-model `input` modalities (v21) tell us whether the bound
model accepts images at all — the UI can warn early.

## 5. M2: child streams / focus switching

The dedicated big step. Subagent child sessions already stream on their
own stamps; the TUI logs and drops them. Includes the latent dead path:
the "subagent session started" note never renders (the foreign-stamp
drop eats the child's `session_opened` before dispatch). Design first:
rendering (nested? parallel panes?), focus model, per-stream folds
(skills, usage), `parent_call` pairing with the open `subagent` tool
call.

## 6. Merge-back hygiene (at the end)

- `scripts/build-release.ts` is bun-shaped (`bun build --compile`) —
  decide the Node-era release shape (SEA vs. shipping `.ts` under type
  stripping).
- ROADMAP.md on main still says "omp fork of pi-tui under Bun …
  Bun-compiled exe" — amend with the stock-pi-tui-on-Node pivot.
- The merge-back itself: this worktree's branch lands as `tui/` on
  `master`.
