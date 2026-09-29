# tabit-rig

Facade for the **tabit** workspace. Re-exports the portable contracts
from `tabit-providers`, the classic runtime from `tabit-engine`, and the `tabit_derive`
proc-macros, under one `tabit_rig::...` namespace.

This is upstream rig's `rig` facade (0.41.0) with all companion provider and
vector-store crates removed. Only `tabit-providers`, `tabit-engine`, and `tabit-derive` are
re-exported. See `../../VENDOR.md` for details.
