//! Tabit extension installation (ROADMAP item 9, task 6; the design
//! record lives in EXTENSIONS.md's install entry). **The directory
//! is the truth** — no registry, no lockfile, no source tracking:
//! each package's manifest carries the facts, hand-placed and
//! npm-installed packages are deliberately indistinguishable once on
//! disk, and update is `tabit install <source>` again.
//!
//! One path for every source: resolve to a staging directory
//! (`path:` copies, `git:` clones `--depth 1`, `npm:` fetches the
//! registry metadata then the tarball and unpacks it, stripping the
//! npm `package/` prefix), validate the manifest there, pull missing
//! name-only `requires` by npm name (refusing cycles), then move
//! everything into place — a failed install never leaves a half
//! package behind. Names may be scoped (`@scope/pkg`), installing to
//! nested `<root>/@scope/pkg/` — the identity is the path relative
//! to the root.

// serde_json's `Value` indexing returns Null for missing keys — it
// never panics — and the registry JSON is navigated positionally.
#![allow(clippy::indexing_slicing)]

use tabit_ext::manifest::{MANIFEST_NAME, Manifest};

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The default npm registry; `$TABIT_NPM_REGISTRY` overrides (the
/// e2e drives a fake — the suite stays offline).
pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

/// One installable source, as the CLI names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `npm:<name>` (latest) or `npm:<name>@<exact version>`.
    Npm {
        name: String,
        version: Option<String>,
    },
    /// `git:<repo url>` — `clone --depth 1`, package root at the
    /// repo top, default branch.
    Git { url: String },
    /// `path:<dir>` — local development; copies into the root.
    Path { dir: PathBuf },
}

impl Source {
    /// Parse a CLI source string. Unknown schemes are a loud error
    /// naming the three shapes.
    pub fn parse(text: &str) -> Result<Source, String> {
        let (scheme, rest) = text.split_once(':').ok_or_else(|| {
            format!("unknown install source `{text}` — try npm:<pkg>, git:<repo>, or path:<dir>")
        })?;
        match scheme {
            "npm" => {
                if rest.is_empty() {
                    return Err("npm: needs a package name".to_string());
                }
                match rest.rsplit_once('@') {
                    // A leading @ is the scope, not a version pin.
                    Some((name, version)) if !name.is_empty() && !version.is_empty() => {
                        Ok(Source::Npm {
                            name: name.to_string(),
                            version: Some(version.to_string()),
                        })
                    }
                    _ => Ok(Source::Npm {
                        name: rest.to_string(),
                        version: None,
                    }),
                }
            }
            "git" if !rest.is_empty() => Ok(Source::Git {
                url: rest.to_string(),
            }),
            "path" if !rest.is_empty() => Ok(Source::Path {
                dir: PathBuf::from(rest),
            }),
            other => Err(format!(
                "unknown install source scheme `{other}` — try npm:<pkg>, git:<repo>, or path:<dir>"
            )),
        }
    }
}

/// One package on disk, as `list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    /// No entry: contributes scan facts only, runs no code.
    pub is_static: bool,
    pub requires: Vec<String>,
}

/// An install's outcome: everything that landed, the primary first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub packages: Vec<String>,
}

/// The installer over one extensions root and one registry base.
pub struct Installer {
    root: PathBuf,
    registry: String,
    /// Staging counter — unique directories inside the root (a
    /// dot-prefix keeps them out of the scan's reports; a manifest is
    /// what makes a directory a package).
    stage_counter: std::sync::atomic::AtomicU64,
}

impl Installer {
    pub fn new(root: impl Into<PathBuf>, registry: impl Into<String>) -> Installer {
        Installer {
            root: root.into(),
            registry: registry.into(),
            stage_counter: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Install one source and its missing requirements. Returns every
    /// package that landed (the primary first, then pulled
    /// dependencies in resolution order).
    pub fn install(&self, source: &Source) -> Result<Installed, String> {
        std::fs::create_dir_all(&self.root).map_err(|error| {
            format!(
                "cannot create the extensions root {}: {error}",
                self.root.display()
            )
        })?;
        let mut landed = Vec::new();
        let mut visited = HashSet::new();
        self.install_recursive(
            source,
            &mut landed,
            &mut visited,
            /* is_dependency = */ false,
        )?;
        Ok(Installed { packages: landed })
    }

    /// The recursion: install the source, then its requires. A
    /// dependency resolves by npm name when not already on disk; the
    /// `visited` set refuses cycles.
    fn install_recursive(
        &self,
        source: &Source,
        landed: &mut Vec<String>,
        visited: &mut HashSet<String>,
        is_dependency: bool,
    ) -> Result<(), String> {
        // Resolve and validate into a staging directory first — and
        // never leave it behind: a failed install (fetch, validation,
        // cycle, anything) cleans its stage, so the root carries only
        // whole packages (the disk-is-the-truth corollary: no
        // half-facts).
        let stage = self.next_stage();
        match self.install_staged(source, &stage, landed, visited, is_dependency) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&stage);
                Err(error)
            }
        }
    }

    fn install_staged(
        &self,
        source: &Source,
        stage: &Path,
        landed: &mut Vec<String>,
        visited: &mut HashSet<String>,
        _is_dependency: bool,
    ) -> Result<(), String> {
        self.resolve_to(source, stage)?;
        let manifest = self.validate(stage)?;
        if !visited.insert(manifest.name.clone()) {
            return Err(format!(
                "dependency cycle at `{}` — the installer refuses to loop",
                manifest.name
            ));
        }
        // Dependencies first (a package's requirement must be on disk
        // before the loader will mount it), then this package lands.
        for required in &manifest.requires {
            if self.package_dir(required).join(MANIFEST_NAME).is_file() {
                continue; // present: disk is the truth, skip
            }
            let dependency = Source::Npm {
                name: required.clone(),
                version: None,
            };
            self.install_recursive(&dependency, landed, visited, true)?;
        }
        self.place(&manifest.name, stage)?;
        landed.push(manifest.name.clone());
        Ok(())
    }

    /// Fill a staging directory from one source.
    fn resolve_to(&self, source: &Source, stage: &Path) -> Result<(), String> {
        std::fs::create_dir_all(stage).map_err(|error| {
            format!("cannot create the staging dir {}: {error}", stage.display())
        })?;
        match source {
            Source::Path { dir } => copy_tree(dir, stage)
                .map_err(|error| format!("cannot copy {}: {error}", dir.display())),
            Source::Git { url } => git_clone(url, stage),
            Source::Npm { name, version } => self.npm_fetch(name, version.as_deref(), stage),
        }
    }

    /// The npm channel: metadata (for the version's tarball URL),
    /// then the tarball, unpacked with the npm `package/` prefix
    /// stripped. Scoped names URL-encode their slash.
    fn npm_fetch(&self, name: &str, version: Option<&str>, stage: &Path) -> Result<(), String> {
        let encoded = name.replace('/', "%2F");
        let url = format!("{}/{encoded}", self.registry.trim_end_matches('/'));
        let client = reqwest::blocking::Client::new();
        let metadata: serde_json::Value = client
            .get(&url)
            .send()
            .map_err(|error| format!("cannot reach the registry at {}: {error}", self.registry))?
            .error_for_status()
            .map_err(|error| format!("the registry has no `{name}`: {error}"))?
            .json()
            .map_err(|error| format!("the registry's answer for `{name}` is not JSON: {error}"))?;
        let resolved = match version {
            Some(want) => want.to_string(),
            None => metadata["dist-tags"]["latest"]
                .as_str()
                .ok_or_else(|| format!("`{name}` has no latest tag to install"))?
                .to_string(),
        };
        let tarball = metadata["versions"][&resolved]["dist"]["tarball"]
            .as_str()
            .ok_or_else(|| format!("`{name}` has no tarball for version {resolved}"))?
            .to_string();
        let bytes = client
            .get(&tarball)
            .send()
            .map_err(|error| format!("cannot fetch the tarball for `{name}`: {error}"))?
            .error_for_status()
            .map_err(|error| format!("the tarball for `{name}` failed to download: {error}"))?
            .bytes()
            .map_err(|error| format!("cannot read the tarball for `{name}`: {error}"))?
            .to_vec();
        unpack_tarball(&bytes, stage).map_err(|error| format!("cannot unpack `{name}`: {error}"))
    }

    /// The staged manifest against the layout invariants. The name
    /// defines the install target (scoped names nest).
    fn validate(&self, stage: &Path) -> Result<Manifest, String> {
        let path = stage.join(MANIFEST_NAME);
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("the package has no {MANIFEST_NAME} at its root: {error}"))?;
        let manifest: Manifest =
            serde_json::from_str(&text).map_err(|error| format!("invalid manifest: {error}"))?;
        if manifest.name.is_empty() {
            return Err("the manifest name must not be empty".to_string());
        }
        for part in manifest.name.split('/') {
            if part.is_empty()
                || part == "."
                || part == ".."
                || part.contains('\\')
                || part.contains(':')
            {
                return Err(format!(
                    "the manifest name `{}` is not a valid path",
                    manifest.name
                ));
            }
        }
        if manifest
            .entry
            .as_ref()
            .is_some_and(|entry| entry.is_empty())
        {
            return Err(
                "entry is declared but empty (omit entry for a static package)".to_string(),
            );
        }
        Ok(manifest)
    }

    /// Where a package name lives (scoped names nest).
    fn package_dir(&self, name: &str) -> PathBuf {
        self.root.join(name.replace('\\', "/"))
    }

    /// Stage → place, replacing anything already there (update is
    /// reinstall): the old directory is swapped aside and removed
    /// only after the new one lands.
    fn place(&self, name: &str, stage: &Path) -> Result<(), String> {
        let target = self.package_dir(name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        let retired = self.next_stage_with_prefix("retired");
        if target.exists()
            && std::fs::rename(&target, &retired).is_err()
            && std::fs::remove_dir_all(&target).is_err()
        {
            return Err(format!(
                "cannot replace the existing package at {}",
                target.display()
            ));
        }
        std::fs::rename(stage, &target).map_err(|error| {
            format!(
                "cannot move the package into place at {}: {error}",
                target.display()
            )
        })?;
        let _ = std::fs::remove_dir_all(&retired);
        Ok(())
    }

    fn next_stage(&self) -> PathBuf {
        self.next_stage_with_prefix("stage")
    }

    fn next_stage_with_prefix(&self, prefix: &str) -> PathBuf {
        let n = self
            .stage_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.root
            .join(format!(".{prefix}-{}-{n}", std::process::id()))
    }

    /// Every package on disk, in scan order (refusals included as
    /// themselves — `list` is a report, not a gate).
    pub fn list(&self) -> Vec<(Listed, Option<String>)> {
        tabit_ext::manifest::scan(&self.root)
            .into_iter()
            .map(|found| match found {
                tabit_ext::manifest::Discovered::Package { manifest, .. } => (
                    Listed {
                        name: manifest.name.clone(),
                        version: manifest.version.clone(),
                        description: manifest.description.clone(),
                        is_static: manifest.is_static(),
                        requires: manifest.requires.clone(),
                    },
                    None,
                ),
                tabit_ext::manifest::Discovered::Refused { dir, reason } => {
                    let name = dir
                        .strip_prefix(&self.root)
                        .unwrap_or(&dir)
                        .to_string_lossy()
                        .replace('\\', "/");
                    (
                        Listed {
                            name,
                            version: String::new(),
                            description: None,
                            is_static: false,
                            requires: Vec::new(),
                        },
                        Some(reason),
                    )
                }
            })
            .collect()
    }

    /// Remove one package. **v1 refuses while direct dependents
    /// remain**, naming them — one linear pass over the manifests;
    /// uninstall those first (the confirmed transitive teardown is a
    /// deferred follow-up).
    pub fn uninstall(&self, name: &str) -> Result<(), String> {
        let dir = self.package_dir(name);
        if !dir.join(MANIFEST_NAME).is_file() {
            return Err(format!("no package named `{name}` is installed"));
        }
        let dependents: Vec<String> = self
            .list()
            .into_iter()
            .filter(|(listed, refused)| {
                refused.is_none() && listed.requires.iter().any(|r| r == name)
            })
            .map(|(listed, _)| listed.name)
            .collect();
        if !dependents.is_empty() {
            let named: Vec<String> = dependents.iter().map(|d| format!("`{d}`")).collect();
            return Err(format!(
                "`{name}` is still required by {} — uninstall {} first",
                named.join(", "),
                named.join(", ")
            ));
        }
        std::fs::remove_dir_all(&dir)
            .map_err(|error| format!("cannot remove {}: {error}", dir.display()))?;
        // Tidy an emptied scope directory.
        if let Some(parent) = dir.parent()
            && parent.starts_with(&self.root)
            && parent != self.root
        {
            let _ = std::fs::remove_dir(parent); // fails if non-empty: correct
        }
        Ok(())
    }
}

/// `git clone --depth 1` into the stage; git's own stderr rides along
/// on failure (a machine running a coding agent has git).
fn git_clone(url: &str, stage: &Path) -> Result<(), String> {
    let mut git = std::process::Command::new("git");
    git.arg("clone").arg("--depth").arg("1").arg(url).arg(stage);
    // CREATE_NO_WINDOW: a console-less caller (the backend) would
    // otherwise flash a terminal for the clone — a real terminal shares
    // its console and never sees the flag's effect.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        git.creation_flags(0x0800_0000);
    }
    let output = git
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // A clone's work tree is the repo root: lift the package files
    // out of the `.git`-carrying stage into a clean sibling so no
    // repository metadata rides into the install.
    let clean = stage.with_extension("clean");
    std::fs::create_dir_all(&clean).map_err(|error| format!("cannot stage: {error}"))?;
    for entry in std::fs::read_dir(stage)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = clean.join(&name);
        if from.is_dir() {
            copy_tree(&from, &to).map_err(|e| e.to_string())?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| e.to_string())?;
        }
    }
    std::fs::remove_dir_all(stage).map_err(|e| e.to_string())?;
    std::fs::rename(&clean, stage).map_err(|e| e.to_string())?;
    Ok(())
}

/// Unpack an npm tarball: gzipped tar, every path under a `package/`
/// prefix (stripped); anything escaping the stage is refused.
fn unpack_tarball(bytes: &[u8], stage: &Path) -> Result<(), String> {
    let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let relative = path.strip_prefix("package").map_err(|_| {
            format!(
                "`{}` does not sit under the npm package/ prefix",
                path.display()
            )
        })?;
        if relative.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        }) {
            return Err(format!("`{}` escapes the package root", path.display()));
        }
        if relative.as_os_str().is_empty() {
            continue;
        }
        entry
            .unpack(stage.join(relative))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// A recursive copy (path installs and the git work-tree lift).
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let source = entry.path();
        let target = to.join(&name);
        if source.is_dir() {
            copy_tree(&source, &target)?;
        } else {
            std::fs::copy(&source, &target)?;
        }
    }
    Ok(())
}
