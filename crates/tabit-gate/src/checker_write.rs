//! Write checker - validates write operations against config (a
//! faithful port of pi-sanity's `checker-write.ts`).

use crate::config::SanityConfig;
use crate::path_permission::{check_write as check_write_path, default_context};
use crate::types::CheckResult;

/// Check if writing to a path is allowed.
///
/// Note: This is for direct write operations, NOT bash commands.
/// Pre-checks only apply to commands, not direct file operations.
pub fn check_write(file_path: impl AsRef<str>, config: &SanityConfig) -> CheckResult {
    let file_path = file_path.as_ref();
    // Direct write operations only check path permissions
    // (pre-checks are for commands, not file operations).
    let path_result = check_write_path(file_path, config, Some(&default_context()));

    CheckResult {
        action: path_result.action,
        reason: path_result.reason,
    }
}
