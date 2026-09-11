//! The manifest (`tabit.json`) and discovery: what is installed and
//! how it starts. Install-time facts only — capabilities are declared
//! live at the handshake (EXTENSIONS.md's declaration ruling), so no
//! schema file can drift from what the process serves.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The manifest's file name, at the package root.
pub const MANIFEST_NAME: &str = "tabit.json";

/// Install-time facts.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// The package's identity; must equal its directory's name (the
    /// install-layout invariant — the directory is the id).
    pub name: String,
    pub version: String,
    /// The entry command (argv). The first token names a file in the
    /// package dir when one is there, else resolves on the OS path.
    pub entry: Vec<String>,
    /// One line for catalogs and reports.
    #[serde(default)]
    pub description: Option<String>,
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
    fn dir(&self) -> &Path {
        match self {
            Discovered::Package { dir, .. } => dir,
            Discovered::Refused { dir, .. } => dir,
        }
    }
}

/// Scan an extensions root: every child directory carrying a
/// `tabit.json`. A missing root is an empty install (nothing
/// scanned); a bad package is refused in the report — never skipped
/// silently, never fatal to its neighbors.
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
        let manifest_path = dir.join(MANIFEST_NAME);
        if !manifest_path.is_file() {
            continue;
        }
        found.push(match read_manifest(&manifest_path) {
            Ok(manifest) => validate(&dir, manifest),
            Err(error) => Discovered::Refused {
                dir,
                reason: error.to_string(),
            },
        });
    }
    // Deterministic order — alphabetical by directory.
    found.sort_by(|a, b| a.dir().cmp(b.dir()));
    found
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
fn validate(dir: &Path, manifest: Manifest) -> Discovered {
    let dir_name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if manifest.name != dir_name {
        return Discovered::Refused {
            dir: dir.to_path_buf(),
            reason: format!(
                "manifest name `{}` does not match its directory `{dir_name}`",
                manifest.name
            ),
        };
    }
    if manifest.entry.is_empty() {
        return Discovered::Refused {
            dir: dir.to_path_buf(),
            reason: "entry command is empty".to_string(),
        };
    }
    Discovered::Package {
        dir: dir.to_path_buf(),
        manifest,
    }
}
