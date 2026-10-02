//! Read checker - validates read operations against config (a
//! faithful port of pi-sanity's `checker-read.ts`).

use crate::config::SanityConfig;
use crate::path_permission::check_read as check_read_path;
use crate::types::CheckResult;

/// Check if reading a path is allowed.
///
/// Note: This is for direct read operations, NOT bash commands.
/// Pre-checks only apply to commands, not direct file operations.
pub fn check_read(file_path: impl AsRef<str>, config: &SanityConfig) -> CheckResult {
    let file_path = file_path.as_ref();
    // Direct read operations only check path permissions
    // (pre-checks are for commands, not file operations) — against
    // the world the rule book was expanded against.
    let path_result = check_read_path(file_path, config, Some(&config.context));

    CheckResult {
        action: path_result.action,
        reason: path_result.reason,
    }
}
