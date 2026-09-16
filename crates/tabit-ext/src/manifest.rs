//! The manifest (`tabit.json`) and discovery: what is installed and
//! how it starts. Install-time facts only — capabilities are declared
//! live at the handshake (EXTENSIONS.md's declaration ruling), so no
//! schema file can drift from what the process serves.
//!
//! Identity (2026-09, the scoped-nesting ruling): **the manifest name
//! equals the package's path relative to the root** — `pkg` lives at
//! `<root>/pkg/`, a scoped `@scope/pkg` at `<root>/@scope/pkg/` (a
//! scope directory starts with `@`, is itself never a package, and
//! exists only to hold leaves). No mangling; npm names copy-paste.
//!
//! `entry` is optional (the static-package ruling): absent means no
//! process, no handshake — the package contributes exactly the
//! scan-driven facts (skills, providers fragment, `requires`
//! presence) and announces nothing. A declared-but-empty entry is
//! still a broken manifest.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The manifest's file name, at the package root.
pub const MANIFEST_NAME: &str = "tabit.json";

/// Install-time facts.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// The package's identity; must equal its path relative to the
    /// extensions root (the install-layout invariant — the directory
    /// IS the id, and scoped names nest: `@scope/pkg`).
    pub name: String,
    pub version: String,
    /// The entry command (argv), when the package is a process. The
    /// first token names a file in the package dir when one is there,
    /// else resolves on the OS path. `None` = a static package: no
    /// spawn, no handshake, scan contributions only.
    #[serde(default)]
    pub entry: Option<Vec<String>>,
    /// One line for catalogs and reports.
    #[serde(default)]
    pub description: Option<String>,
    /// Name-only dependencies (the task-6 ruling): each must be
    /// present in the mounted set at load — presence, not liveness —
    /// and the installer pulls the missing ones by npm name.
    #[serde(default)]
    pub requires: Vec<String>,
}

impl Manifest {
    /// A static package contributes no process — skills, fragments,
    /// and requirement presence are all the scan reads from it.
    pub fn is_static(&self) -> bool {
        self.entry.is_none()
    }
}

/// Why a discovered package cannot be used. These are external
/// errors — the user's installed packages — so each is reported
/// per-package and never fatal to the others.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid manifest: {reason}")]
    Invalid { path: PathBuf, reason: String },
}

/// One scan outcome: a usable package, or a refusal with its reason.
#[derive(Debug)]
pub enum Discovered {
    Package { dir: PathBuf, manifest: Manifest },
    Refused { dir: PathBuf, reason: String },
}

impl Discovered {
    /// The package's directory (refusals included) — the sort key
    /// callers re-ordering scan output use.
    pub fn dir(&self) -> &Path {
        match self {
            Discovered::Package { dir, .. } => dir,
            Discovered::Refused { dir, .. } => dir,
        }
    }

    /// The package's manifest, when the scan accepted it.
    pub fn manifest(&self) -> Option<&Manifest> {
        match self {
            Discovered::Package { manifest, .. } => Some(manifest),
            Discovered::Refused { .. } => None,
        }
    }
}

/// Scan an extensions root: every package directory (scope dirs
/// nesting one level for `@`-prefixed names) carrying a `tabit.json`.
/// A missing root is an empty install (nothing scanned); a bad
/// package is refused in the report — never skipped silently, never
/// fatal to its neighbors.
pub fn scan(root: &Path) -> Vec<Discovered> {
    let mut found: Vec<Discovered> = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return found, // no root installed: an empty install
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                found.push(Discovered::Refused {
                    dir: root.to_path_buf(),
                    reason: format!("directory enumeration failed: {source}"),
                });
                break;
            }
        };
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('@') {
            // A scope container: never itself a package; its leaves
            // are, named `@scope/leaf`.
            found.extend(scan_scope(&dir, &name));
            continue;
        }
        if let Some(discovered) = discover(&dir, &name) {
            found.push(discovered);
        }
    }
    // Deterministic order — alphabetical by directory (the full
    // relative path, scopes included).
    found.sort_by(|a, b| a.dir().cmp(b.dir()));
    found
}

/// One scope container's leaves.
fn scan_scope(scope_dir: &Path, scope: &str) -> Vec<Discovered> {
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(scope_dir) {
        Ok(entries) => entries,
        Err(_) => return found,
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let leaf = entry.file_name().to_string_lossy().into_owned();
        if let Some(discovered) = discover(&dir, &format!("{scope}/{leaf}")) {
            found.push(discovered);
        }
    }
    found
}

/// One candidate directory against its expected identity (the path
/// relative to the root). `None` = not a package at all (no
/// manifest) — not ours to report.
fn discover(dir: &Path, expected_name: &str) -> Option<Discovered> {
    let manifest_path = dir.join(MANIFEST_NAME);
    if !manifest_path.is_file() {
        return None;
    }
    Some(match read_manifest(&manifest_path) {
        Ok(manifest) => validate(dir, expected_name, manifest),
        Err(error) => Discovered::Refused {
            dir: dir.to_path_buf(),
            reason: error.to_string(),
        },
    })
}

fn read_manifest(path: &Path) -> Result<Manifest, ManifestError> {
    let text = std::fs::read_to_string(path).map_err(|source| ManifestError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|error| ManifestError::Invalid {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })
}

/// The load-time invariants, each refusing with its reason.
fn validate(dir: &Path, expected_name: &str, manifest: Manifest) -> Discovered {
    if manifest.name != expected_name {
        return Discovered::Refused {
            dir: dir.to_path_buf(),
            reason: format!(
                "manifest name `{}` does not match its path `{expected_name}`",
                manifest.name
            ),
        };
    }
    if manifest
        .entry
        .as_ref()
        .is_some_and(|entry| entry.is_empty())
    {
        return Discovered::Refused {
            dir: dir.to_path_buf(),
            reason: "entry command is declared but empty (omit entry for a static package)"
                .to_string(),
        };
    }
    Discovered::Package {
        dir: dir.to_path_buf(),
        manifest,
    }
}

/// The load-time requirement check (the task-6 ruling): every
/// package's `requires` must name something in the **mounted set**
/// — the packages the scan accepted and the settings did not
/// disable — *presence, not liveness* (an installed-but-dead
/// requirement is the death policy's business; requirements never
/// reorder anything, because nothing links). Unmet requirements
/// become refusals with their reasons; everything else passes
/// through unchanged.
pub fn enforce_requires(found: Vec<Discovered>, mounted: &HashSet<String>) -> Vec<Discovered> {
    found
        .into_iter()
        .map(|found| match &found {
            Discovered::Package { manifest, .. } => {
                if let Some(missing) = manifest
                    .requires
                    .iter()
                    .find(|required| !mounted.contains(*required))
                {
                    Discovered::Refused {
                        dir: match &found {
                            Discovered::Package { dir, .. } => dir.clone(),
                            Discovered::Refused { dir, .. } => dir.clone(),
                        },
                        reason: format!(
                            "requires extension `{missing}`, which is not mounted \
                             (not installed, disabled, or refused)"
                        ),
                    }
                } else {
                    found
                }
            }
            Discovered::Refused { .. } => found,
        })
        .collect()
}
