//! Manifest/discovery tests: what scans in, what refuses, what stays
//! silent — the load-time invariants of `tabit.json`.

#![cfg_attr(
    test,
    allow(
        clippy::err_expect,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::panic_in_result_fn,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

use std::path::{Path, PathBuf};

use tabit_ext::manifest::{self, Discovered};

fn test_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::OnceLock<std::sync::Mutex<u32>> = std::sync::OnceLock::new();
    let n = {
        let counter = COUNTER.get_or_init(|| std::sync::Mutex::new(0));
        let mut n = counter.lock().expect("counter lock");
        *n += 1;
        *n
    };
    let dir = std::env::temp_dir().join(format!("tabit-ext-manifest-tests/{tag}-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Install a package dir with the given manifest text (caller-owned
/// JSON so the refusal tests can write real broken packages).
fn write_package(root: &Path, name: &str, manifest_text: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("package dir");
    std::fs::write(dir.join("tabit.json"), manifest_text).expect("manifest");
}

fn scan_names(root: &Path) -> Vec<String> {
    manifest::scan(root)
        .into_iter()
        .map(|found| match found {
            Discovered::Package { dir, .. } | Discovered::Refused { dir, .. } => dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        })
        .collect()
}

#[test]
fn packages_scan_in_alphabetical_order() {
    let root = test_dir("order");
    write_package(
        &root,
        "zeta",
        r#"{"name":"zeta","version":"1","entry":["x"]}"#,
    );
    write_package(
        &root,
        "alpha",
        r#"{"name":"alpha","version":"1","entry":["x"]}"#,
    );
    assert_eq!(scan_names(&root), vec!["alpha", "zeta"]);
}

#[test]
fn a_name_that_does_not_match_its_dir_is_refused() {
    let root = test_dir("mismatch");
    write_package(
        &root,
        "mismatch",
        r#"{"name":"other","version":"1","entry":["x"]}"#,
    );
    let found = manifest::scan(&root);
    assert_eq!(found.len(), 1);
    match &found[0] {
        Discovered::Refused { reason, .. } => {
            assert!(reason.contains("does not match its path"));
        }
        Discovered::Package { .. } => panic!("mismatched name must refuse"),
    }
}

#[test]
fn an_empty_entry_is_refused() {
    let root = test_dir("noentry");
    write_package(
        &root,
        "noentry",
        r#"{"name":"noentry","version":"1","entry":[]}"#,
    );
    match &manifest::scan(&root).remove(0) {
        Discovered::Refused { reason, .. } => {
            assert!(reason.contains("declared but empty"), "{reason}")
        }
        Discovered::Package { .. } => panic!("empty entry must refuse"),
    }
}

#[test]
fn a_broken_manifest_is_refused_with_its_reason() {
    let root = test_dir("broken");
    write_package(&root, "broken", "{ not json");
    match &manifest::scan(&root).remove(0) {
        Discovered::Refused { reason, .. } => assert!(reason.contains("invalid manifest")),
        Discovered::Package { .. } => panic!("broken manifest must refuse"),
    }
}

#[test]
fn a_dir_without_a_manifest_is_not_an_extension() {
    let root = test_dir("plain");
    std::fs::create_dir_all(root.join("plain")).expect("dir");
    write_package(
        &root,
        "real",
        r#"{"name":"real","version":"1","entry":["x"],"description":"d"}"#,
    );
    assert_eq!(scan_names(&root), vec!["real"]);
}

#[test]
fn a_missing_root_is_an_empty_install() {
    let root = test_dir("absent").join("never-created");
    assert!(manifest::scan(&root).is_empty());
}

#[test]
fn scoped_names_nest_under_their_scope_directory() {
    let root = test_dir("scoped");
    write_package(
        &root,
        "@scope/alpha",
        r#"{"name":"@scope/alpha","version":"1","entry":["x"]}"#,
    );
    write_package(
        &root,
        "@scope/zeta",
        r#"{"name":"@scope/zeta","version":"1","entry":["x"]}"#,
    );
    write_package(
        &root,
        "plain",
        r#"{"name":"plain","version":"1","entry":["x"]}"#,
    );
    let found = manifest::scan(&root);
    let names: Vec<String> = found
        .iter()
        .map(|f| f.manifest().expect("all scan in").name.clone())
        .collect();
    // Order is alphabetical over the full relative path: the scope
    // sorts before the plain leaf, its leaves in their own order.
    assert_eq!(names, vec!["@scope/alpha", "@scope/zeta", "plain"]);

    // A leaf whose manifest forgets the scope refuses (the identity
    // is the path, not just the leaf).
    write_package(
        &root,
        "@other/leaf",
        r#"{"name":"leaf","version":"1","entry":["x"]}"#,
    );
    let found = manifest::scan(&root);
    match found.iter().find(|f| f.manifest().is_none()) {
        Some(Discovered::Refused { reason, .. }) => {
            assert!(reason.contains("does not match its path"), "{reason}");
        }
        _ => panic!("the scope-less leaf must refuse"),
    }
}

#[test]
fn an_absent_entry_is_a_static_package() {
    let root = test_dir("static");
    write_package(
        &root,
        "collection",
        r#"{"name":"collection","version":"1","requires":["a","b"]}"#,
    );
    match &manifest::scan(&root).remove(0) {
        Discovered::Package { manifest, .. } => {
            assert!(manifest.is_static(), "no entry = static");
            assert!(manifest.entry.is_none());
            assert_eq!(manifest.requires, vec!["a".to_string(), "b".to_string()]);
        }
        Discovered::Refused { reason, .. } => panic!("static packages scan in: {reason}"),
    }
}

#[test]
fn unmet_requirements_refuse_by_presence_not_liveness() {
    let root = test_dir("requires");
    write_package(
        &root,
        "needy",
        r#"{"name":"needy","version":"1","entry":["x"],"requires":["there"]}"#,
    );
    write_package(&root, "there", r#"{"name":"there","version":"1"}"#);

    // Mounted set carries the requirement: needy scans in.
    let mounted: std::collections::HashSet<String> = ["there".to_string()].into_iter().collect();
    let found = manifest::enforce_requires(manifest::scan(&root), &mounted);
    assert!(found[0].manifest().is_some(), "the requirement is present");

    // The requirement is gone (disabled or uninstalled): needy
    // refuses, naming it.
    let mounted: std::collections::HashSet<String> = Default::default();
    let found = manifest::enforce_requires(manifest::scan(&root), &mounted);
    match &found[0] {
        Discovered::Refused { reason, .. } => {
            assert!(reason.contains("requires extension `there`"), "{reason}");
        }
        Discovered::Package { .. } => panic!("the unmet requirement must refuse"),
    }
}
