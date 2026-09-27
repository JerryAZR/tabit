//! Path permission checking using config — a faithful port of
//! pi-sanity's `path-permission.ts`. Expands variables and evaluates
//! paths against permission rules.

mod glob_matcher;

use crate::config::{PermissionSection, SanityConfig};
use crate::path_utils::{Platform, preprocess_runtime_path};
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

/// The standalone process-cwd context: cwd, home, tmpdir, no repo
/// resolution (`{{REPO}}` falls back to cwd — it is never probed;
/// owner ruling 2026-09-27). Production checks never build this: the
/// rule book carries its own world (`SanityConfig::context`),
/// expanded and matched against the same facts. This constructor
/// serves the load itself, the bash walker's default, and tests.
pub fn default_context() -> PathContext {
    PathContext {
        cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string()),
        home: home::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_string()),
        tmpdir: std::env::temp_dir().to_string_lossy().into_owned(),
        repo: None,
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
/// `checkPathPermission`). The stored patterns must already be
/// preprocessed (the loader's one site — see
/// [`crate::path_utils::preprocess_config_pattern`]); the runtime
/// path is normalized here, the single path-side site.
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
    // Patterns are matched AS STORED: they are preprocessed exactly
    // once, at load (the loader's job) — checking never preprocesses.
    // The runtime path above is the only side normalized here, so the
    // two preprocessing sites (load for patterns, check for paths)
    // never meet.
    for override_rule in &permission.overrides {
        for pattern in &override_rule.path {
            if matches_glob(&normalized_file_path, pattern, context.platform) {
                result = PathCheckResult {
                    action: override_rule.action,
                    reason: override_rule.reason.clone(),
                    matched_pattern: Some(pattern.clone()),
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
