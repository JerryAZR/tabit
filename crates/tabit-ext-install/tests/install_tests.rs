//! Installer tests, fully offline: `path:` copies, `git:` clones a
//! locally-built repository, `npm:` drives an httpmock fake registry
//! serving real gzipped-tarball fixtures — the whole channel without
//! the network. The requires matrix (pull, skip-present, cycle) and
//! the refusal uninstall ride the same fakes.

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

use std::io::Write as _;
use std::path::{Path, PathBuf};

use httpmock::MockServer;
use tabit_ext_install::{Installed, Installer, Source};

fn test_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::OnceLock<std::sync::Mutex<u32>> = std::sync::OnceLock::new();
    let n = {
        let counter = COUNTER.get_or_init(|| std::sync::Mutex::new(0));
        let mut n = counter.lock().expect("counter lock");
        *n += 1;
        *n
    };
    let dir = std::env::temp_dir().join(format!("tabit-install-tests/{tag}-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Write a package directory: manifest fields + a payload file.
fn write_package(dir: &Path, name: &str, extra: &str, entry: Option<&str>, requires: &[&str]) {
    std::fs::create_dir_all(dir).expect("package dir");
    let entry_json = match entry {
        Some(entry) => format!(r#""entry":["{entry}"],"#),
        None => String::new(),
    };
    let requires_json = if requires.is_empty() {
        String::new()
    } else {
        let list = requires
            .iter()
            .map(|r| format!("\"{r}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#""requires":[{list}],"#)
    };
    std::fs::write(
        dir.join("tabit.json"),
        format!(r#"{{"name":"{name}","version":"1.0.0",{entry_json}{requires_json}"description":"the test package"}}"#),
    )
    .expect("manifest");
    std::fs::write(dir.join("payload.txt"), extra).expect("payload");
}

/// Build a real npm-shaped tarball (gzipped tar, `package/` prefix).
fn npm_tarball(files: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, body) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("package/{name}"), body.as_bytes())
            .expect("append");
    }
    let tar = builder.into_inner().expect("tar bytes");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar).expect("gzip");
    gz.finish().expect("gzip finish")
}

/// Serve one package on the fake registry at `latest`.
fn serve_npm(server: &MockServer, name: &str, files: &[(&str, &str)]) {
    let tarball = npm_tarball(files);
    let tarball_path = format!("/tarballs/{name}.tgz");
    let tarball_url = format!("http://127.0.0.1:{}{}", server.port(), tarball_path);
    server.mock(move |when, then| {
        when.method(httpmock::Method::GET)
            .path(format!("/{}", name.replace('/', "%2F")));
        then.status(200).json_body(serde_json::json!({
            "dist-tags": {"latest": "1.0.0"},
            "versions": {"1.0.0": {"dist": {"tarball": tarball_url}}},
        }));
    });
    server.mock(move |when, then| {
        when.method(httpmock::Method::GET).path(tarball_path);
        then.status(200).body(tarball.clone());
    });
}

fn installer(root: &Path, registry: &MockServer) -> Installer {
    Installer::new(root, format!("http://127.0.0.1:{}", registry.port()))
}

fn installed_names(installed: &Installed) -> &[String] {
    &installed.packages
}

// ---- sources

#[test]
fn a_path_install_copies_into_place() {
    let dir = test_dir("path");
    let source = dir.join("source");
    write_package(&source, "simple", "payload body", Some("run.exe"), &[]);
    let root = dir.join("extensions");
    Installer::new(&root, "http://127.0.0.1:1")
        .install(&Source::Path {
            dir: source.clone(),
        })
        .expect("installs");
    let target = root.join("simple");
    assert!(target.join("tabit.json").is_file());
    assert_eq!(
        std::fs::read_to_string(target.join("payload.txt")).expect("payload"),
        "payload body"
    );
    // Reinstall replaces (update = reinstall): the payload swap is
    // observable.
    std::fs::write(source.join("payload.txt"), "second body").expect("payload v2");
    let result = Installer::new(&root, "http://127.0.0.1:1")
        .install(&Source::Path { dir: source })
        .expect("reinstalls");
    assert_eq!(installed_names(&result), &["simple".to_string()]);
    assert_eq!(
        std::fs::read_to_string(root.join("simple/payload.txt")).expect("payload"),
        "second body"
    );
}

#[test]
fn a_npm_install_fetches_metadata_tarball_and_unpacked() {
    let server = MockServer::start();
    serve_npm(
        &server,
        "tabit-demo",
        &[
            (
                "tabit.json",
                r#"{"name":"tabit-demo","version":"1.0.0","entry":["node","main.js"]}"#,
            ),
            ("main.js", "console.log('hi')"),
        ],
    );
    let dir = test_dir("npm");
    let root = dir.join("extensions");
    let installed = installer(&root, &server)
        .install(&Source::parse("npm:tabit-demo").expect("parses"))
        .expect("installs");
    assert_eq!(installed_names(&installed), &["tabit-demo".to_string()]);
    assert!(root.join("tabit-demo/main.js").is_file());
    // The npm package/ prefix is stripped.
    assert!(!root.join("tabit-demo/package").exists());
}

#[test]
fn a_scoped_npm_name_installs_nested() {
    let server = MockServer::start();
    serve_npm(
        &server,
        "@scope/tool",
        &[(
            "tabit.json",
            r#"{"name":"@scope/tool","version":"1.0.0","entry":["run"]}"#,
        )],
    );
    let dir = test_dir("scoped-npm");
    let root = dir.join("extensions");
    installer(&root, &server)
        .install(&Source::parse("npm:@scope/tool").expect("parses"))
        .expect("installs");
    assert!(root.join("@scope/tool/tabit.json").is_file());
}

#[test]
fn a_git_install_clones_and_drops_the_repository_metadata() {
    let dir = test_dir("git");
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    write_package(&repo, "from-git", "git payload", Some("run"), &[]);
    let output = std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&repo)
        .output()
        .expect("git init");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let commands: Vec<Vec<&str>> = vec![vec!["add", "."], vec!["commit", "-q", "-m", "init"]];
    for command in &commands {
        let output = std::process::Command::new("git")
            .args(command)
            .current_dir(&repo)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "{}: {}",
            command[0],
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let root = dir.join("extensions");
    let source_url = format!("path:{}", repo.display());
    // A git source with a local path (git accepts plain paths; depth
    // is advisory on local clones and the assertion is the install).
    Installer::new(&root, "http://127.0.0.1:1")
        .install(&Source::Git {
            url: repo.display().to_string(),
        })
        .expect("installs");
    let _ = source_url;
    assert!(root.join("from-git/tabit.json").is_file());
    assert!(
        !root.join("from-git/.git").exists(),
        "no repository metadata rides into the install"
    );
}

// ---- requires

#[test]
fn missing_requirements_pull_by_npm_name_and_present_ones_skip() {
    let server = MockServer::start();
    serve_npm(
        &server,
        "dep",
        &[(
            "tabit.json",
            r#"{"name":"dep","version":"1.0.0","entry":["run"]}"#,
        )],
    );
    let dir = test_dir("requires");
    let root = dir.join("extensions");
    // `already` is on disk: disk is the truth, no pull.
    write_package(
        &root.join("already"),
        "already",
        "present",
        Some("run"),
        &[],
    );
    let source = dir.join("source");
    write_package(&source, "needy", "body", Some("run"), &["dep", "already"]);
    let installed = Installer::new(&root, format!("http://127.0.0.1:{}", server.port()))
        .install(&Source::Path { dir: source })
        .expect("installs");
    assert_eq!(
        installed_names(&installed),
        &["dep".to_string(), "needy".to_string()]
    );
    assert!(
        root.join("dep/tabit.json").is_file(),
        "the dependency was pulled"
    );
}

#[test]
fn a_dependency_cycle_refuses() {
    let server = MockServer::start();
    serve_npm(
        &server,
        "a",
        &[(
            "tabit.json",
            r#"{"name":"a","version":"1","entry":["x"],"requires":["b"]}"#,
        )],
    );
    serve_npm(
        &server,
        "b",
        &[(
            "tabit.json",
            r#"{"name":"b","version":"1","entry":["x"],"requires":["a"]}"#,
        )],
    );
    let dir = test_dir("cycle");
    let root = dir.join("extensions");
    let error = installer(&root, &server)
        .install(&Source::parse("npm:a").expect("parses"))
        .expect_err("the cycle refuses");
    assert!(error.contains("cycle"), "{error}");
    // And nothing landed: the failed install leaves no half tree.
    let listed = Installer::new(&root, format!("http://127.0.0.1:{}", server.port())).list();
    assert!(listed.is_empty(), "{listed:?}");
}

// ---- list / uninstall

#[test]
fn list_reports_statics_and_broken_packages() {
    let dir = test_dir("list");
    let root = dir.join("extensions");
    write_package(&root.join("proc"), "proc", "x", Some("run"), &[]);
    write_package(&root.join("bundle"), "bundle", "x", None, &["proc"]);
    std::fs::create_dir_all(root.join("broken")).expect("dir");
    std::fs::write(root.join("broken/tabit.json"), "not json").expect("broken");
    let listed = Installer::new(&root, "http://127.0.0.1:1").list();
    let names: Vec<(String, Option<String>)> = listed
        .into_iter()
        .map(|(listed, refused)| (listed.name, refused))
        .collect();
    assert_eq!(names.len(), 3, "{names:?}");
    assert_eq!(names[0].0, "broken", "scan order");
    assert!(
        names[0]
            .1
            .as_deref()
            .is_some_and(|r| r.contains("invalid manifest")),
        "{names:?}"
    );
    assert_eq!(names[1], ("bundle".to_string(), None));
    assert_eq!(names[2], ("proc".to_string(), None));
    let bundle = Installer::new(&root, "http://127.0.0.1:1")
        .list()
        .into_iter()
        .find(|(listed, _)| listed.name == "bundle")
        .expect("listed");
    assert!(bundle.0.is_static);
}

#[test]
fn a_manifestless_package_refuses_at_validate() {
    // The front door of the never-leaves-a-half-package contract: a
    // source with no tabit.json at its root refuses before anything
    // lands, and the root stays clean.
    let dir = test_dir("validate-miss");
    let source = dir.join("source");
    std::fs::create_dir_all(&source).expect("dirs");
    std::fs::write(source.join("loose.txt"), "not a package").expect("loose file");
    let root = dir.join("extensions");
    let error = Installer::new(&root, "http://127.0.0.1:1")
        .install(&Source::Path { dir: source })
        .expect_err("a manifestless dir is not a package");
    assert!(
        error.contains("no tabit.json at its root"),
        "the refusal names the missing manifest: {error}"
    );
    assert!(
        !root.join("loose.txt").is_file(),
        "nothing of the failed install landed"
    );
}

#[test]
fn uninstalling_a_name_that_was_never_installed_names_it() {
    let dir = test_dir("uninstall-miss");
    let root = dir.join("extensions");
    std::fs::create_dir_all(&root).expect("dirs");
    let error = Installer::new(&root, "http://127.0.0.1:1")
        .uninstall("ghost")
        .expect_err("nothing is installed under that name");
    assert!(
        error.contains("no package named `ghost` is installed"),
        "{error}"
    );
}

#[test]
fn uninstall_refuses_while_direct_dependents_remain() {
    let dir = test_dir("uninstall-refusal");
    let root = dir.join("extensions");
    write_package(&root.join("base"), "base", "x", Some("run"), &[]);
    write_package(&root.join("top"), "top", "x", Some("run"), &["base"]);
    let error = Installer::new(&root, "http://127.0.0.1:1")
        .uninstall("base")
        .expect_err("the dependent blocks");
    assert!(error.contains("`top`"), "{error}");
    assert!(root.join("base").exists(), "nothing was removed");
    // The dependent-free uninstall works.
    Installer::new(&root, "http://127.0.0.1:1")
        .uninstall("top")
        .expect("uninstalls");
    Installer::new(&root, "http://127.0.0.1:1")
        .uninstall("base")
        .expect("uninstalls");
    assert!(!root.join("base").exists());
}

#[test]
fn uninstalling_a_scoped_leaf_tidies_the_emptied_scope() {
    let dir = test_dir("uninstall-scope");
    let root = dir.join("extensions");
    write_package(
        &root.join("@scope/tool"),
        "@scope/tool",
        "x",
        Some("run"),
        &[],
    );
    Installer::new(&root, "http://127.0.0.1:1")
        .uninstall("@scope/tool")
        .expect("uninstalls");
    assert!(!root.join("@scope").exists(), "the emptied scope is tidied");
}

// ---- source parsing

#[test]
fn sources_parse_their_shapes() {
    assert_eq!(
        Source::parse("npm:thing").expect("parses"),
        Source::Npm {
            name: "thing".to_string(),
            version: None,
        }
    );
    assert_eq!(
        Source::parse("npm:thing@1.2.3").expect("parses"),
        Source::Npm {
            name: "thing".to_string(),
            version: Some("1.2.3".to_string()),
        }
    );
    // A scoped name's leading @ is the scope, not a version pin.
    assert_eq!(
        Source::parse("npm:@scope/thing").expect("parses"),
        Source::Npm {
            name: "@scope/thing".to_string(),
            version: None,
        }
    );
    assert!(matches!(
        Source::parse("git:https://example.com/r.git").expect("parses"),
        Source::Git { .. }
    ));
    assert!(matches!(
        Source::parse("path:C:/somewhere").expect("parses"),
        Source::Path { .. }
    ));
    assert!(Source::parse("ftp:x").is_err());
    assert!(Source::parse("npm:").is_err());
}
