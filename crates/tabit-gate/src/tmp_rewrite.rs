//! POSIX /tmp path rewriting for Windows tool surfaces — a faithful
//! port of pi-sanity's `tmp-rewrite.ts`.
//!
//! On Windows, the bash tool runs commands through Git Bash (MSYS),
//! which transparently maps /tmp to the user's real temp directory
//! (%TEMP%). The file tools (read/write/edit) have no such mapping:
//! Node resolves "/tmp/foo" against the current drive, landing at
//! "C:\tmp\foo" — drive-root junk instead of temp.
//!
//! To keep both surfaces consistent (" /tmp always means temp"), the
//! integration rewrites POSIX-style /tmp paths in tool_call inputs to
//! the real temp dir BEFORE running permission checks. Bash input is
//! never rewritten: the shell performs its own (correct) translation,
//! including inside WSL where /tmp is the VM's own Linux temp
//! directory — rewriting that across the VM boundary would be wrong.

use serde_json::{Map, Value};

use crate::config::SanityConfig;
use crate::path_utils::{Platform, node_path};

/// Length of the POSIX temp prefix "/tmp".
const TMP_PREFIX_LEN: usize = 4;

/// Rewrite a POSIX-style /tmp path to the Windows temp directory.
///
/// Only applies on win32. Only exact "/tmp" or "/tmp/..." prefixes are
/// rewritten; lookalikes ("/tmpfoo", "C:/tmp/a", relative "tmp/a") are
/// left alone. Traversal that escapes /tmp ("/tmp/../etc/x") is
/// detected via POSIX normalization and returned UNCHANGED so the
/// permission checker evaluates (and denies) the original path.
///
/// Pure function: takes platform and tmpdir as parameters so callers
/// and tests are host-independent. Output uses win32 joining
/// regardless of host.
pub fn rewrite_posix_tmp_path(input_path: &str, tmpdir: &str, platform: Platform) -> String {
    if !platform.is_win32() {
        return input_path.to_string();
    }

    // Fast reject: not a POSIX-rooted /tmp candidate.
    if !input_path.starts_with("/tmp") {
        return input_path.to_string();
    }
    // Boundary: "/tmp" or "/tmp/..." — not "/tmpfoo".
    if input_path.len() > TMP_PREFIX_LEN && input_path.as_bytes()[TMP_PREFIX_LEN] != b'/' {
        return input_path.to_string();
    }

    // Collapse ".", "..", "//" before matching so traversal cannot
    // hide inside the rewritten prefix.
    let collapsed = node_path::posix_normalize(input_path);

    if collapsed == "/tmp" {
        return node_path::win32_join(&[tmpdir]);
    }
    if let Some(rest) = collapsed.strip_prefix("/tmp/") {
        return node_path::win32_join(&[tmpdir, rest]);
    }

    // Normalization moved the path out of /tmp (e.g. "/tmp/../etc/x"):
    // no rewrite — let the permission check see the original path.
    input_path.to_string()
}

/// Rewrite /tmp paths in a tool_call input, in place, before
/// permission checking. Path params are discovered from the configured
/// tool rules, so custom user-configured path params benefit too.
/// Bash-check params are never rewritten (the shell does its own
/// translation).
///
/// Returns true if the input was modified.
pub fn rewrite_tool_path_param(
    tool_name: &str,
    input: &mut Map<String, Value>,
    tmpdir: &str,
    config: &SanityConfig,
    platform: Platform,
) -> bool {
    if !platform.is_win32() {
        return false;
    }

    let Some(checks) = config.tools.rules.get(tool_name) else {
        return false;
    };

    let mut changed = false;
    for check in checks.iter() {
        if check.check == crate::config::CheckKind::Bash {
            continue;
        }

        let Some(value) = input.get_mut(&check.param) else {
            continue;
        };
        match value {
            Value::String(s) => {
                let rewritten = rewrite_posix_tmp_path(s, tmpdir, platform);
                if rewritten != *s {
                    *s = rewritten;
                    changed = true;
                }
            }
            Value::Array(items) => {
                let mut any_changed = false;
                for item in items.iter_mut() {
                    if let Value::String(s) = item {
                        let rewritten = rewrite_posix_tmp_path(s, tmpdir, platform);
                        if rewritten != *s {
                            *s = rewritten;
                            any_changed = true;
                        }
                    }
                }
                changed |= any_changed;
            }
            _ => {}
        }
    }
    changed
}
