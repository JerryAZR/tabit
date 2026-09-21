//! Path permission checking using config — a faithful port of
//! pi-sanity's `path-permission.ts`. Expands variables and evaluates
//! paths against permission rules.

mod glob_matcher;

use crate::config::{PermissionSection, SanityConfig};
use crate::path_utils::{Platform, preprocess_config_pattern, preprocess_runtime_path};
use crate::types::Action;

// TS path-permission.ts re-exports the context type.
pub use crate::path_utils::PathContext;

/// Check result for a single path (TS `PathCheckResult`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathCheckResult {
    pub action: Action,
    pub reason: Option<String>,
    pub matched_pattern: Option<String>,
}

/// Detect a git repository root using `git rev-parse --show-toplevel`.
/// Returns `None` if not in a git repository or git is not available.
/// TS bounds the probe with `execSync`'s 1-second timeout; here
/// `wait-timeout` does the same, killing the child on expiry.
fn detect_repo() -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use wait_timeout::ChildExt;

    let mut child = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
    // rev-parse output is a single short line, well under the pipe
    // buffer, so reading after the bounded wait cannot deadlock.
    match child.wait_timeout(TIMEOUT) {
        Ok(Some(status)) if status.success() => {
            let mut out = String::new();
            let read = child
                .stdout
                .take()
                .and_then(|mut s| s.read_to_string(&mut out).ok())
                .unwrap_or(0);
            let trimmed = out.trim().to_string();
            if !trimmed.is_empty() && read > 0 {
                Some(trimmed)
            } else {
                None
            }
        }
        _ => {
            // Timeout (kill) or failure: TS `catch { return undefined }`.
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// Get the default path context using system values (TS
/// `getDefaultContext`). Attempts to detect a git repo, falling back
/// to no repo.
pub fn default_context() -> PathContext {
    PathContext {
        cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string()),
        home: home::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_string()),
        tmpdir: std::env::temp_dir().to_string_lossy().into_owned(),
        repo: detect_repo(),
        platform: Platform::native(),
    }
}

/// Check if a normalized path matches a preprocessed glob pattern.
/// Windows filesystems are case-insensitive, so matching is too (TS
/// `matchesGlob`: picomatch `dot: true`, `nocase` on win32).
pub(crate) fn matches_glob(normalized_path: &str, pattern: &str, platform: Platform) -> bool {
    glob_matcher::matches(normalized_path, pattern, platform.is_win32())
}

/// Check a path against a permission section (read/write); returns
/// the last matching override's action, or the section default (TS
/// `checkPathPermission`). Override patterns are preprocessed against
/// the runtime context here — already-preprocessed patterns (as the
/// loader emits them) preprocess idempotently to themselves, and
/// hand-built configs get the same `{{VAR}}`/tilde/env expansion the
/// loader applies.
pub fn check_path_permission(
    file_path: &str,
    permission: &PermissionSection,
    context: &PathContext,
) -> PathCheckResult {
    let mut result = PathCheckResult {
        action: permission.default,
        reason: permission.reason.clone(),
        matched_pattern: None,
    };

    // Normalize the file path once before checking.
    let normalized_file_path = preprocess_runtime_path(file_path, context);

    // Check each override in order (last match wins — no break).
    for override_rule in &permission.overrides {
        for pattern in &override_rule.path {
            // Loader-emitted patterns are already preprocessed and
            // always absolute: left untouched (their load-time
            // anchoring is load-bearing). Glob-global patterns
            // (`**/...`) are likewise matched as written, anywhere.
            // Other hand-built raw patterns get the same
            // {{VAR}}/tilde/env expansion and normalization the loader
            // applies.
            let pattern = if pattern.starts_with('/') || pattern.starts_with('*') {
                pattern.clone()
            } else {
                preprocess_config_pattern(pattern, context)
            };
            if matches_glob(&normalized_file_path, &pattern, context.platform) {
                result = PathCheckResult {
                    action: override_rule.action,
                    reason: override_rule.reason.clone(),
                    matched_pattern: Some(pattern),
                };
            }
        }
    }

    result
}

/// Check read permission for a path (TS `checkRead`; `context`
/// defaults to [`default_context`]).
pub fn check_read(
    file_path: &str,
    config: &SanityConfig,
    context: Option<&PathContext>,
) -> PathCheckResult {
    let owned;
    let ctx = match context {
        Some(ctx) => ctx,
        None => {
            owned = default_context();
            &owned
        }
    };
    check_path_permission(file_path, &config.permissions.read, ctx)
}

/// Check write permission for a path (TS `checkWrite`; `context`
/// defaults to [`default_context`]).
pub fn check_write(
    file_path: &str,
    config: &SanityConfig,
    context: Option<&PathContext>,
) -> PathCheckResult {
    let owned;
    let ctx = match context {
        Some(ctx) => ctx,
        None => {
            owned = default_context();
            &owned
        }
    };
    check_path_permission(file_path, &config.permissions.write, ctx)
}

/// Check delete permission for a path. Deletion is a write operation
/// (modifies the parent directory), so this is an alias for
/// [`check_write`] — TS `checkDelete`, deprecated there and kept as
/// the alias it is.
#[deprecated = "Use check_write directly (deletion checks write permissions)"]
pub fn check_delete(
    file_path: &str,
    config: &SanityConfig,
    context: Option<&PathContext>,
) -> PathCheckResult {
    check_write(file_path, config, context)
}
