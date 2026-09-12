//! Skills mounts: an enabled package's `skills/` directory linked
//! into the user's skills directory (`~/.tabit/skills/<name>/`,
//! EXTENSIONS.md's package layout). The link — not a fifth discovery
//! source — is how extension-shipped skills reach the model: the
//! session's four-source ladder stays unaware that extensions exist
//! (the front/back split), while the package's skills ride the same
//! discovery, catalog, and confined `skill` tool as the user's own.
//!
//! Mount rules, one slot per package (the slot is the extension's
//! name, so two packages cannot claim one slot):
//!
//! - missing slot → create the link;
//! - slot already resolving to this package's `skills/` → no-op
//!   (idempotent across boots, parents and children alike);
//! - dangling link or junction (the package moved or was reinstalled
//!   elsewhere) → replace it;
//! - any other existing entry (the user's own directory, another
//!   package's live link) → **the existing entry wins**, warned —
//!   the host never overwrites the user's files.
//!
//! On Windows the link is a directory symlink where the privilege
//! allows it and a junction otherwise (junctions need no developer
//! mode and resolve identically for discovery and confinement).

use std::path::Path;

/// Link every package's skills directory into `skills_home`
/// (`~/.tabit/skills`). `packages` carries `(name, package dir)` for
/// the packages the host is launching (enablement already applied — a
/// disabled package ships nothing). Returns the warnings; a package
/// without a `skills/` directory is simply not a skills shipper.
pub fn link(packages: &[(String, std::path::PathBuf)], skills_home: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    for (name, dir) in packages {
        let source = dir.join("skills");
        if !source.is_dir() {
            continue;
        }
        let slot = skills_home.join(name);
        // Idempotency: a slot already resolving to this source is the
        // mounted state (canonicalization resolves symlinks and
        // junctions alike).
        match (std::fs::canonicalize(&slot), std::fs::canonicalize(&source)) {
            (Ok(slot_real), Ok(source_real)) if slot_real == source_real => continue,
            _ => {}
        }
        if let Some(reason) = occupied(&slot) {
            if reason.is_some() {
                // Dangling link/junction: the package it pointed at is
                // gone (uninstalled or moved) — replacing loses nothing.
                if let Err(error) = remove_link(&slot) {
                    warnings.push(format!(
                        "extension `{name}`: its stale skills slot {} cannot be removed: {error}",
                        slot.display()
                    ));
                    continue;
                }
            } else {
                warnings.push(format!(
                    "extension `{name}`: skills slot {} already exists — the existing entry wins",
                    slot.display()
                ));
                continue;
            }
        }
        if let Err(error) = std::fs::create_dir_all(skills_home) {
            warnings.push(format!(
                "extension `{name}`: cannot create the skills directory {}: {error}",
                skills_home.display()
            ));
            return warnings;
        }
        if let Err(reason) = make_link(&source, &slot) {
            warnings.push(format!(
                "extension `{name}`: cannot mount skills at {}: {reason}",
                slot.display()
            ));
        }
    }
    warnings
}

/// Whether `slot` holds something, and whether that something is a
/// link (symlink or junction) — `Some(None)` = occupied by a real
/// entry, `Some(Some(reason))` = a dangling link whose target is gone,
/// `None` = free.
fn occupied(slot: &Path) -> Option<Option<String>> {
    let meta = std::fs::symlink_metadata(slot).ok()?;
    let is_link = meta.file_type().is_symlink() || is_junction(slot);
    if !is_link {
        return Some(None);
    }
    match std::fs::canonicalize(slot) {
        Ok(_) => Some(None), // a live link: an existing entry
        Err(_) => Some(Some("dangling".to_string())),
    }
}

/// Remove a link (directory symlink or junction) without touching its
/// target. Directory-shaped links remove as dirs; a file symlink (the
/// Unix shape) removes as a file.
fn remove_link(slot: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir(slot) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::remove_file(slot),
    }
}

#[cfg(windows)]
fn is_junction(path: &Path) -> bool {
    junction::exists(path).unwrap_or(false)
}

#[cfg(unix)]
fn is_junction(_path: &Path) -> bool {
    false
}

/// Create the slot pointing at `source`. Windows tries the true
/// symlink first (the honest shape where the privilege allows it) and
/// falls back to the junction — both resolve through canonicalization,
/// so discovery and the `skill` tool's confinement treat them alike.
#[cfg(windows)]
fn make_link(source: &Path, slot: &Path) -> Result<(), String> {
    use std::os::windows::fs::symlink_dir;
    if let Err(error) = symlink_dir(source, slot) {
        return junction::create(source, slot)
            .map_err(|junction_error| format!("symlink: {error}; junction: {junction_error}"));
    }
    Ok(())
}

#[cfg(unix)]
fn make_link(source: &Path, slot: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(source, slot).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tabit-ext-skills-{tag}-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn package(root: &Path, name: &str, skills: &[(&str, &str)]) -> PathBuf {
        let dir = root.join(name);
        for (skill, body) in skills {
            let skill_dir = dir.join("skills").join(skill);
            std::fs::create_dir_all(&skill_dir).expect("skill dir");
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!("---\nname: {skill}\ndescription: the {skill} skill\n---\n{body}\n"),
            )
            .expect("SKILL.md");
        }
        std::fs::create_dir_all(&dir).expect("package dir");
        dir
    }

    fn read_through(home: &Path, name: &str, skill: &str) -> String {
        std::fs::read_to_string(home.join(name).join(skill).join("SKILL.md"))
            .expect("the link resolves to the package's skill")
    }

    #[test]
    fn mounts_a_packages_skills_and_is_idempotent() {
        let root = scratch("mount");
        let pkg = package(&root, "shipper", &[("demo", "body text")]);
        let home = root.join("skills-home");
        let warnings = link(&[("shipper".into(), pkg.clone())], &home);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(read_through(&home, "shipper", "demo").contains("body text"));

        // The second boot (the child's, or the next start) is a no-op.
        let again = link(&[("shipper".into(), pkg)], &home);
        assert!(again.is_empty(), "{again:?}");
        assert!(home.join("shipper").is_dir());
    }

    #[test]
    fn the_users_own_entry_wins_over_the_slot() {
        let root = scratch("clash");
        let pkg = package(&root, "shipper", &[("demo", "body")]);
        let home = root.join("skills-home");
        std::fs::create_dir_all(home.join("shipper")).expect("the user's dir");
        std::fs::write(home.join("shipper/OWNED"), "the user's file").expect("file");
        let warnings = link(&[("shipper".into(), pkg)], &home);
        assert_eq!(warnings.len(), 1);
        let warning = &warnings[0];
        assert!(warning.contains("already exists"), "{warning}");
        assert!(home.join("shipper/OWNED").is_file(), "untouched");
    }

    #[test]
    fn a_dangling_slot_is_replaced() {
        let root = scratch("dangling");
        let gone = root.join("gone-package");
        std::fs::create_dir_all(gone.join("skills")).expect("old package");
        let home = root.join("skills-home");
        std::fs::create_dir_all(&home).expect("home");
        // Mount the gone package, then delete it — the slot dangles.
        let first = link(&[("shipper".into(), gone.clone())], &home);
        assert!(first.is_empty(), "{first:?}");
        std::fs::remove_dir_all(&gone).expect("uninstall");
        let pkg = package(&root, "shipper", &[("demo", "fresh body")]);
        let warnings = link(&[("shipper".into(), pkg)], &home);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(read_through(&home, "shipper", "demo").contains("fresh body"));
    }

    #[test]
    fn a_package_without_skills_mounts_nothing() {
        let root = scratch("bare");
        let pkg = package(&root, "bare", &[]);
        let home = root.join("skills-home");
        let warnings = link(&[("bare".into(), pkg)], &home);
        assert!(warnings.is_empty());
        assert!(!home.join("bare").exists(), "no slot, no link");
        assert!(!home.exists(), "not even the home is created");
    }
}
