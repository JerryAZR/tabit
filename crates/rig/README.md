# rig

Trimmed facade for the **tabit** workspace. Re-exports the portable contracts
from `rig-core`, the classic runtime from `rig-agent`, and the `rig_derive`
proc-macros, under the familiar `rig::...` paths.

This is upstream rig's `rig` facade (0.41.0) with all companion provider and
vector-store crates removed. Only `rig-core`, `rig-agent`, and `rig-derive` are
re-exported. See `../../VENDOR.md` for details.
