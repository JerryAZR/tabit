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

## 3. Skill chips — **deferred (owner ruling 2026-10: non-critical)**

**Interim (landed)**: the slash table's skill entries format the wire's
`<skill name="…"/>` tag into the message — raw XML markers in messages
are the accepted UX for now (the backend expands them at the message
door since v20; the dropdown's type column still marks them `skill`).

**Deferred design** (kept for when chips return): the editor shows
`[skill: commit]`, backspace deletes the whole chip atomically, submit
expands to the tag — the wire never sees the chip.

**Mechanism (pi-tui, confirmed)**: text marker + ID registry + the
editor's `segmentWithMarkers` wrapper fusing the marker into one
indivisible grapheme — cursor movement, wrap, hit-testing, and backspace
all go through the segmenter, so atomicity is inherent; undo snapshots
include the registry; submit-time expansion already exists for paste
markers (`[paste #1 +123 lines]`). A skill chip is a second marker rule.
**The blocker (scouted at pi-tui 1.0.2)**: the machinery is private —
`pastes`/`expandPasteMarkers` are internal; the public surface is
plain-text `insertTextAtCursor` + `getExpandedText()`. Chips need a small
public API (`registerChip(label, expansion)`) — upstreamable (pi's own
skill autocomplete would benefit) or carried in a fork, which the
scrollback sibling already maintains.

Invocation UX: `/name` autocomplete against the session's skill catalog
inserts the chip (skills stop being display-only slash entries; the tag
format happens at submit).

## 4. Attachment blocks — v0 landed (TUI paste + backend expansion)

Ctrl+V in the editor: clipboard image → temp file (`<tmpdir>/tabit-paste/<human-id>.<ext>`)
→ an `<attachment path="…"/>` tag at the cursor (plain text — chips stay
deferred); text on the clipboard inserts as-is (pi's chain). Acquisition
rides pi-tui 1.0.2's bundled native clipboard with wl-paste/xclip
fallbacks (`src/paste-image.ts`). The backend expands the tag at the
mailbox door (master, 0d17de5): per resolvable tag, the file's basename
label plus **the image itself as a base64 content part** — raster only
(PNG/JPEG/GIF/WebP, sniffed + decode-verified, extension never trusted),
downscaled at the door (long edge ≤ 1568px, ≤ 5 MB). The wire stays
text: `user_message` events join the text parts (tags + basename labels),
so the transcript shows the attachment by name; the log and the model's
request hold exactly what the model saw (the first-part law: door
expansion only appends parts, part[0] is the authored text, salvaged
drafts re-expand exactly once). Known mismatch to trim: the TUI's sniff
also accepts BMP, which the backend leaves unexpanded (the tag passes
through).

Chips share item 3's blocker (the editor's marker machinery is private)
and its deferral. The catalog's per-model `input` modalities (v21) tell
us whether the bound model accepts images at all — an early-warning UI
can come later.

## 5. M2: child streams / focus switching — landed

**Designed in `M2-DESIGN.md`; implemented as designed.** Per-stream
`StreamState` folds (transcript, facts, queue, skills, tree, activity
atom) replacing the single-transcript fold; focus is a stream id with
the ruled laws (submit/commands/Esc route to the focused stream; Esc
aborts a running focus, walks an idle one to its parent; the draft is
discarded on switch); the region stack (transcript — editor — list) on
alt+↑/↓ plus empty-editor ↓; the subagent list as the first widget over
the projection seam (running always listed, idle hidden after 60s);
view-independent cards labeled with their stream; the footer's stream
badge. Remaining from the design's stated limits: the parent's tool card
could gain a live `↳ <current tool>` line (the frames already fold);
full no-effect-↓ semantics await a pi-tui public accessor.

## 6. Merge-back hygiene (at the end)

- `scripts/build-release.ts` is bun-shaped (`bun build --compile`) —
  decide the Node-era release shape (SEA vs. shipping `.ts` under type
  stripping).
- ROADMAP.md on main still says "omp fork of pi-tui under Bun …
  Bun-compiled exe" — amend with the stock-pi-tui-on-Node pivot.
- The merge-back itself: this worktree's branch lands as `tui/` on
  `master`.
