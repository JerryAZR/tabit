use super::*;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tabit-catalog-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn settings(urls: Vec<String>) -> CatalogSettings {
    // Generous budgets: no test through this helper exercises the
    // budget itself (the budget tests below set their own tight
    // values), and a tight default flakes under full-gate scheduling
    // starvation — a 32-way suite can stall a connection-refused
    // round trip past a 1s timeout.
    CatalogSettings {
        auto_update: true,
        ttl_hours: 24,
        urls,
        pointer_budget_ms: 30_000,
        artifact_budget_ms: 60_000,
        artifact_timeout_ms: 60_000,
    }
}

const CATALOG_V1: &str = r#"
[providers.zai]
base_url = "https://api.z.ai/api/anthropic"
api = "anthropic-messages"
api_key_env = "ZAI_API_KEY"

[[providers.zai.models]]
id = "glm-5.3"
context_window = 1000000
"#;

fn sha256_hex(body: &str) -> String {
    sha2::Sha256::digest(body.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn auto_path_sits_next_to_the_config_or_falls_back_to_the_home_dir() {
    let _guard = crate::TEST_ENV_LOCK.lock().expect("env lock");
    // Next to the config that was read.
    let config = Path::new("C:/cfg/providers.toml");
    assert_eq!(
        auto_path(Some(config)),
        Some(config.with_file_name("providers.auto.toml"))
    );
    // No config path: the default home location (a bare machine's
    // catalog still lands where boot looks for it). home_dir prefers
    // USERPROFILE (Windows) over HOME — stage both, restore after.
    let home = temp_dir("auto-home");
    let saved_userprofile = std::env::var_os("USERPROFILE");
    let saved_home = std::env::var_os("HOME");
    // SAFETY: serialized by TEST_ENV_LOCK.
    unsafe {
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("HOME", &home);
    }
    let resolved = auto_path(None);
    // SAFETY: restoring immediately; no other env reader runs under
    // the lock.
    unsafe {
        match &saved_userprofile {
            Some(saved) => std::env::set_var("USERPROFILE", saved),
            None => std::env::remove_var("USERPROFILE"),
        }
        match &saved_home {
            Some(saved) => std::env::set_var("HOME", saved),
            None => std::env::remove_var("HOME"),
        }
    }
    assert_eq!(
        resolved,
        Some(home.join(".tabit").join("providers.auto.toml"))
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn a_held_refresher_lock_skips_the_refresh_before_the_network() {
    let dir = temp_dir("lock-held");
    let auto = dir.join("providers.auto.toml");
    // A concurrent refresher holds the lock: the sidecar exists and
    // is claimed — the check precedes any network.
    let lock_path = auto.with_file_name("providers.auto.lock");
    let held = std::fs::File::create(&lock_path).expect("lock file");
    held.try_lock().expect("the test claims the lock");

    let outcome = refresh(&settings(vec!["http://127.0.0.1:1".to_string()]), &auto).await;

    match outcome {
        RefreshOutcome::Offline { reason } => {
            assert!(
                reason.contains("a concurrent refresher holds the lock"),
                "{reason}"
            );
        }
        other => panic!("a held lock is offline, not {other:?}"),
    }
    assert!(!auto.exists(), "nothing installed");
    drop(held);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_pointer_phase_with_no_live_mirror_is_offline() {
    let dir = temp_dir("pointer-dead");
    let auto = dir.join("providers.auto.toml");

    let outcome = refresh(&settings(vec!["http://127.0.0.1:1".to_string()]), &auto).await;

    match outcome {
        RefreshOutcome::Offline { reason } => {
            assert!(reason.contains("every mirror failed"), "{reason}");
        }
        other => panic!("a dead mirror is offline, not {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unparseable_pointer_is_offline() {
    let dir = temp_dir("bad-pointer");
    let auto = dir.join("providers.auto.toml");
    let server = httpmock::MockServer::start_async().await;
    server.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200).body("not toml = [");
    });

    let outcome = refresh(&settings(vec![server.url("")]), &auto).await;

    match outcome {
        RefreshOutcome::Offline { reason } => {
            assert!(reason.contains("bad pointer"), "{reason}");
        }
        other => panic!("a bad pointer is offline, not {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_stale_state_matching_the_pointer_restamps_without_downloading() {
    // The stale-TTL state whose version IS the pointer's: same
    // catalog, fresh stamp — the TTL restarts, the artifact never
    // re-downloads.
    let dir = temp_dir("restamp");
    let auto = dir.join("providers.auto.toml");
    // The artifact is installed and valid — the restamp is only
    // legitimate when the cache the state vouches for is actually
    // there (the missing-artifact case re-downloads).
    std::fs::write(&auto, CATALOG_V1).expect("the artifact is installed");
    write_state(
        &state_path(&auto),
        &CatalogState {
            version: "v1".to_string(),
            fetched_at: 0,
        },
    )
    .expect("the stale state writes");
    let server = httpmock::MockServer::start_async().await;
    let latest = format!(
        "version = \"v1\"\nsha256 = \"{}\"\nmin_schema = 1\n",
        sha256_hex(CATALOG_V1)
    );
    server.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200).body(&latest);
    });
    let artifact = server.mock(|when, then| {
        when.path("/catalog-v1.toml");
        then.status(500);
    });

    let outcome = refresh(&settings(vec![server.url("")]), &auto).await;

    assert_eq!(
        outcome,
        RefreshOutcome::Fresh {
            version: "v1".to_string()
        }
    );
    assert_eq!(
        artifact.calls_async().await,
        0,
        "the same version never re-downloads"
    );
    let restamped = read_state(&state_path(&auto)).expect("the state re-stamped");
    assert!(restamped.fetched_at > 0, "the TTL restarted");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unparseable_artifact_is_refused_and_never_lands() {
    // The checksum passes (the bytes ARE the pointer's); the install
    // refuses what cannot parse as a providers fragment.
    let dir = temp_dir("bad-artifact");
    let auto = dir.join("providers.auto.toml");
    let garbage = "not toml = [";
    let server = httpmock::MockServer::start_async().await;
    let latest = format!(
        "version = \"v1\"\nsha256 = \"{}\"\nmin_schema = 1\n",
        sha256_hex(garbage)
    );
    server.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200).body(&latest);
    });
    server.mock(|when, then| {
        when.path("/catalog-v1.toml");
        then.status(200).body(garbage);
    });

    let outcome = refresh(&settings(vec![server.url("")]), &auto).await;

    match outcome {
        RefreshOutcome::Offline { reason } => {
            assert!(reason.contains("artifact does not parse"), "{reason}");
        }
        other => panic!("a refused artifact is offline, not {other:?}"),
    }
    assert!(!auto.exists(), "a refused artifact never lands");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_artifact_is_offline() {
    let dir = temp_dir("artifact-404");
    let auto = dir.join("providers.auto.toml");
    let server = httpmock::MockServer::start_async().await;
    let latest = format!(
        "version = \"v1\"\nsha256 = \"{}\"\nmin_schema = 1\n",
        sha256_hex(CATALOG_V1)
    );
    server.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200).body(&latest);
    });
    server.mock(|when, then| {
        when.path("/catalog-v1.toml");
        then.status(404);
    });

    let outcome = refresh(&settings(vec![server.url("")]), &auto).await;

    match outcome {
        RefreshOutcome::Offline { reason } => {
            assert!(reason.contains("artifact: every mirror failed"), "{reason}");
        }
        other => panic!("a missing artifact is offline, not {other:?}"),
    }
    assert!(!auto.exists(), "nothing installed");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_stale_cache_downloads_verifies_and_installs() {
    let dir = temp_dir("install");
    let auto = dir.join("providers.auto.toml");
    let server = httpmock::MockServer::start_async().await;
    let latest = format!(
        "version = \"v1\"\nsha256 = \"{}\"\nmin_schema = 1\n",
        sha256_hex(CATALOG_V1)
    );
    let pointer = server.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200).body(&latest);
    });
    let artifact = server.mock(|when, then| {
        when.path("/catalog-v1.toml");
        then.status(200).body(CATALOG_V1);
    });

    refresh(&settings(vec![server.url("")]), &auto).await;

    pointer.assert_async().await;
    artifact.assert_async().await;
    let written = std::fs::read_to_string(&auto).expect("the auto file exists");
    assert!(written.contains("glm-5.3"), "{written}");
    assert!(written.starts_with("# generated"), "{written}");
    let state = std::fs::read_to_string(state_path(&auto)).expect("the state exists");
    assert!(state.contains("v1"), "{state}");

    // A second refresh with an unchanged version re-stamps without
    // re-downloading.
    let artifact = server.mock(|when, then| {
        when.path("/catalog-v1.toml");
        then.status(500);
    });
    refresh(&settings(vec![server.url("")]), &auto).await;
    assert_eq!(
        artifact.calls_async().await,
        0,
        "the artifact is not re-fetched for the same version"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_checksum_mismatch_refuses_the_install() {
    let dir = temp_dir("checksum");
    let auto = dir.join("providers.auto.toml");
    let server = httpmock::MockServer::start_async().await;
    server.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200)
            .body("version = \"v1\"\nsha256 = \"deadbeef\"\nmin_schema = 1\n");
    });
    server.mock(|when, then| {
        when.path("/catalog-v1.toml");
        then.status(200).body(CATALOG_V1);
    });

    refresh(&settings(vec![server.url("")]), &auto).await;

    assert!(!auto.exists(), "a mismatched artifact never lands");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_newer_schema_is_refused() {
    let dir = temp_dir("schema");
    let auto = dir.join("providers.auto.toml");
    let server = httpmock::MockServer::start_async().await;
    server.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200)
            .body("version = \"v9\"\nsha256 = \"00\"\nmin_schema = 99\n");
    });

    refresh(&settings(vec![server.url("")]), &auto).await;

    assert!(!auto.exists(), "a newer schema never lands");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_fresh_cache_skips_the_network() {
    let dir = temp_dir("fresh");
    let auto = dir.join("providers.auto.toml");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    std::fs::write(&auto, CATALOG_V1).expect("the artifact is installed");
    write_state(
        &state_path(&auto),
        &CatalogState {
            version: "v1".to_string(),
            fetched_at: now,
        },
    )
    .expect("state writes");
    let server = httpmock::MockServer::start_async().await;
    let pointer = server.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200);
    });

    refresh(&settings(vec![server.url("")]), &auto).await;

    assert_eq!(
        pointer.calls_async().await,
        0,
        "a fresh cache never fetches"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn auto_update_off_is_a_noop() {
    let dir = temp_dir("off");
    let auto = dir.join("providers.auto.toml");
    let server = httpmock::MockServer::start_async().await;
    let pointer = server.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200);
    });
    let mut off = settings(vec![server.url("")]);
    off.auto_update = false;

    refresh(&off, &auto).await;

    assert_eq!(pointer.calls_async().await, 0);
    assert!(!auto.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn merge_auto_fills_models_under_a_declared_provider() {
    let mut config = TabitConfig::from_toml_str(
        r#"
[providers.zai]
base_url = "https://user-override.example"
api = "anthropic-messages"

[[providers.zai.models]]
id = "glm-5.3"
name = "Mine"
"#,
        Path::new("providers.toml"),
    )
    .expect("user config");
    let dir = temp_dir("merge");
    let auto = dir.join("providers.auto.toml");
    std::fs::write(
        &auto,
        r#"
[providers.zai]
base_url = "https://catalog.example"
api = "anthropic-messages"

[[providers.zai.models]]
id = "glm-5.3"
context_window = 1000000

[[providers.zai.models]]
id = "glm-5.3-flash"

[providers.other]
base_url = "https://other.example"
api = "openai-completions"
keyless = true

[[providers.other.models]]
id = "o1"
"#,
    )
    .expect("auto file writes");
    let mut warnings = Vec::new();

    merge_auto(&mut config, &auto, &mut warnings);

    assert!(warnings.is_empty(), "{warnings:?}");
    let zai = config.provider("zai").expect("zai");
    assert_eq!(
        zai.base_url, "https://user-override.example",
        "provider-level fields are never touched"
    );
    let mine = zai.model("glm-5.3").expect("the user's model");
    assert_eq!(
        mine.name.as_deref(),
        Some("Mine"),
        "a declared model id wins whole"
    );
    assert!(mine.context_window.is_none(), "no annotation");
    assert!(
        zai.model("glm-5.3-flash").is_some(),
        "the catalog fills new ids"
    );
    assert!(
        config.provider("other").is_some(),
        "an unknown provider inserts whole"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_corrupt_auto_file_warns_and_is_skipped() {
    let mut config = TabitConfig::default();
    let dir = temp_dir("corrupt");
    let auto = dir.join("providers.auto.toml");
    std::fs::write(&auto, "not toml = [").expect("auto file writes");
    let mut warnings = Vec::new();

    merge_auto(&mut config, &auto, &mut warnings);

    assert_eq!(warnings.len(), 1, "one warning: {warnings:?}");
    assert!(config.providers.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
#[ignore = "live channel check — run manually"]
async fn live_jsdelivr_channel_end_to_end() {
    let dir = temp_dir("live");
    let auto = dir.join("providers.auto.toml");
    // An empty list means the built-in mirrors (jsDelivr, then
    // GitHub raw) — this checks the shipped coordinates themselves.
    refresh(&settings(Vec::new()), &auto).await;
    let written = std::fs::read_to_string(&auto).expect("the auto file landed");
    assert!(written.contains("[[providers."), "{written}");
    let mut config = TabitConfig::default();
    let mut warnings = Vec::new();
    merge_auto(&mut config, &auto, &mut warnings);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(config.providers.len() > 10, "{}", config.providers.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_pointer_over_budget_is_offline_and_never_fetches_the_artifact() {
    let dir = temp_dir("slow-pointer");
    let auto = dir.join("providers.auto.toml");
    let server = httpmock::MockServer::start_async().await;
    let artifact = server.mock(|when, then| {
        when.path("/catalog-v1.toml");
        then.status(200).body(CATALOG_V1);
    });
    server.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200)
            .delay(std::time::Duration::from_millis(300))
            .body(
                "version = \"v1\"
sha256 = \"00\"
min_schema = 1
",
            );
    });
    let mut slow = settings(vec![server.url("")]);
    slow.pointer_budget_ms = 50;

    let outcome = refresh(&slow, &auto).await;

    assert!(
        matches!(outcome, RefreshOutcome::Offline { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        artifact.calls_async().await,
        0,
        "a pointer over budget abandons the attempt"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_artifact_phase_has_its_own_budget_beyond_the_pointer() {
    // The pointer lands fast; the artifact takes longer than the
    // pointer budget but lands within the artifact budget — the
    // update applies.
    let dir = temp_dir("slow-artifact");
    let auto = dir.join("providers.auto.toml");
    let server = httpmock::MockServer::start_async().await;
    let latest = format!(
        "version = \"v1\"
sha256 = \"{}\"
min_schema = 1
",
        sha256_hex(CATALOG_V1)
    );
    server.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200).body(&latest);
    });
    server.mock(|when, then| {
        when.path("/catalog-v1.toml");
        then.status(200)
            .delay(std::time::Duration::from_millis(200))
            .body(CATALOG_V1);
    });
    let mut split = settings(vec![server.url("")]);
    split.pointer_budget_ms = 50;

    let outcome = refresh(&split, &auto).await;

    assert!(
        matches!(outcome, RefreshOutcome::Updated { .. }),
        "{outcome:?}"
    );
    assert!(auto.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Boot-shape test harness: a staged home, config path, and offline
/// extension root, with the catalog settings pointing at the mock.
/// The root stages two STATIC packages (no entry ⇒ no spawn): relay
/// contributes a providers fragment, relay2's fragment repeats
/// relay's provider id (the fragment-collision warning).
fn boot_stage(tag: &str, urls: Vec<String>, budgets: (u64, u64)) -> (PathBuf, crate::AppOptions) {
    let dir = temp_dir(tag);
    let extensions = dir.join("extensions");
    std::fs::create_dir_all(&extensions).expect("extensions");
    let fragment = "[providers.relay]\nbase_url = \"http://127.0.0.1:8391/v1\"\napi = \"openai-completions\"\nkeyless = true\n";
    for name in ["relay", "relay2"] {
        let package = extensions.join(name);
        std::fs::create_dir_all(&package).expect("package dir");
        std::fs::write(
            package.join("tabit.json"),
            format!("{{\"name\":\"{name}\",\"version\":\"0.1.0\"}}"),
        )
        .expect("manifest");
        std::fs::write(package.join("providers.toml"), fragment).expect("fragment");
    }
    let settings_body = format!(
        "[catalog]
urls = {:?}
pointer_budget_ms = {}
artifact_budget_ms = {}
",
        urls, budgets.0, budgets.1
    );
    let settings_path = dir.join("settings.toml");
    std::fs::write(&settings_path, settings_body).expect("settings");
    unsafe { std::env::set_var("TABIT_SETTINGS", &settings_path) };
    let mut options = crate::AppOptions {
        extensions: Some(extensions),
        ..Default::default()
    };
    // world_scan reads args.parent for the child ruling.
    let _ = &mut options;
    (dir, options)
}

fn boot_config_path(dir: &Path) -> PathBuf {
    let config = dir.join("providers.toml");
    std::fs::write(&config, "").expect("empty config");
    config
}

#[test]
fn a_child_session_never_refreshes_the_catalog() {
    // The ruling lives in catalog_plan: a child (spawned with a
    // parent) gets Cached without a network check; the parent's boot
    // owns refreshing.
    let dir = temp_dir("child-skip");
    let auto = dir.join("providers.auto.toml");
    let settings = settings(vec!["http://127.0.0.1:1".to_string()]);
    let plan = crate::assemble::catalog_plan(true, &settings, Some(&auto));
    assert!(
        matches!(plan, crate::assemble::CatalogPlan::Cached { .. }),
        "a child never checks the network"
    );
    // An adult with a stale cache checks; auto_update off disables;
    // no config path disables.
    let plan = crate::assemble::catalog_plan(false, &settings, Some(&auto));
    assert!(matches!(plan, crate::assemble::CatalogPlan::Check { .. }));
    // A cache within TTL reports its version without a network check.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    std::fs::write(&auto, CATALOG_V1).expect("the artifact is installed");
    write_state(
        &state_path(&auto),
        &CatalogState {
            version: "v1".to_string(),
            fetched_at: now,
        },
    )
    .expect("state writes");
    let plan = crate::assemble::catalog_plan(false, &settings, Some(&auto));
    assert!(
        matches!(plan, crate::assemble::CatalogPlan::Cached { version: Some(ref version) } if version == "v1"),
        "a fresh cache merges without a check"
    );
    // The child carries the cache's version too — the wire documents
    // the block's version as absent only when NO cache exists.
    let plan = crate::assemble::catalog_plan(true, &settings, Some(&auto));
    assert!(
        matches!(plan, crate::assemble::CatalogPlan::Cached { version: Some(ref version) } if version == "v1"),
        "a child merges the parent's cache and reports its version"
    );
    // And a stale cache's version rides the Check plan, so an Offline
    // outcome can name what is actually merged on disk.
    write_state(
        &state_path(&auto),
        &CatalogState {
            version: "v1".to_string(),
            fetched_at: 0,
        },
    )
    .expect("the stale state writes");
    let plan = crate::assemble::catalog_plan(false, &settings, Some(&auto));
    assert!(
        matches!(plan, crate::assemble::CatalogPlan::Check { ref cached_version, .. } if cached_version.as_deref() == Some("v1")),
        "the Check plan carries the stale cache's version"
    );
    let mut off = settings.clone();
    off.auto_update = false;
    let plan = crate::assemble::catalog_plan(false, &off, Some(&auto));
    assert!(matches!(plan, crate::assemble::CatalogPlan::Disabled));
    let plan = crate::assemble::catalog_plan(false, &settings, None);
    assert!(matches!(plan, crate::assemble::CatalogPlan::Disabled));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_slow_update_is_pending_and_installs_for_next_boot() {
    // boot_stage sets TABIT_SETTINGS (process-global): serialize
    // against the other env-staging tests.
    let _env_guard = crate::TEST_ENV_LOCK.lock().expect("env lock");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _guard = runtime.enter();
    let server = httpmock::MockServer::start();
    let latest = format!(
        "version = \"v1\"
sha256 = \"{}\"
min_schema = 1
",
        sha256_hex(CATALOG_V1)
    );
    server.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200).body(&latest);
    });
    server.mock(|when, then| {
        when.path("/catalog-v1.toml");
        then.status(200)
            .delay(std::time::Duration::from_millis(1500))
            .body(CATALOG_V1);
    });
    let (dir, options) = boot_stage("pending", vec![server.url("")], (400, 250));
    let config_path = boot_config_path(&dir);
    let auto = dir.join("providers.auto.toml");

    let mut boot = crate::world_scan(
        &options,
        TabitConfig::default(),
        Some(config_path),
        std::sync::Arc::new(tabit_config::AuthConfig::default()),
    )
    .expect("world_scan");
    assert!(boot.checking_catalog());
    // The staged static packages mount (scan facts only — no entry,
    // no launch) and reach the fragment merge.
    let launchable = boot.take_launchable();
    let names: Vec<&str> = launchable
        .packages
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(names, ["relay", "relay2"], "scan order is directory order");
    assert!(launchable.found.is_empty(), "static packages never launch");
    let registry = crate::build_registry(boot, &runtime);
    assert!(
        matches!(
            registry.catalog_status(),
            tabit_session::CatalogStatus::Pending
        ),
        "{:?}",
        registry.catalog_status()
    );
    assert!(
        registry.config().provider("relay").is_some(),
        "the package fragment merged under the user config"
    );
    assert!(!auto.exists(), "the merge ran before the install");

    // The detached task keeps downloading; it installs for next boot.
    runtime.block_on(tokio::time::sleep(std::time::Duration::from_millis(1400)));
    assert!(auto.exists(), "the background refresh landed");
    unsafe { std::env::remove_var("TABIT_SETTINGS") };
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_stale_state_with_a_missing_artifact_redownloads() {
    // The state sidecar vouches for v1, the pointer agrees — but the
    // artifact itself is GONE (deleted, cleaned, torn write). A
    // restamp would vouch for a cache that isn't there: the version
    // match must re-download, not restamp.
    let dir = temp_dir("restamp-missing");
    let auto = dir.join("providers.auto.toml");
    write_state(
        &state_path(&auto),
        &CatalogState {
            version: "v1".to_string(),
            fetched_at: 0,
        },
    )
    .expect("the stale state writes");
    let server = httpmock::MockServer::start_async().await;
    let latest = format!(
        "version = \"v1\"\nsha256 = \"{}\"\nmin_schema = 1\n",
        sha256_hex(CATALOG_V1)
    );
    server.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200).body(&latest);
    });
    let artifact = server.mock(|when, then| {
        when.path("/catalog-v1.toml");
        then.status(200).body(CATALOG_V1);
    });

    let outcome = refresh(&settings(vec![server.url("")]), &auto).await;

    assert!(
        matches!(outcome, RefreshOutcome::Updated { .. }),
        "a missing artifact re-downloads even when the version matches"
    );
    assert_eq!(artifact.calls_async().await, 1, "the artifact was fetched");
    assert!(auto.is_file(), "the artifact landed");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_fresh_state_with_a_missing_artifact_is_not_fresh() {
    // The within-TTL pre-check must vouch for the artifact too: state
    // fresh but artifact absent means the cache contributes nothing —
    // refresh, don't skip.
    let dir = temp_dir("fresh-missing");
    let auto = dir.join("providers.auto.toml");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    write_state(
        &state_path(&auto),
        &CatalogState {
            version: "v1".to_string(),
            fetched_at: now,
        },
    )
    .expect("state writes");
    let server = httpmock::MockServer::start_async().await;
    let latest = format!(
        "version = \"v1\"\nsha256 = \"{}\"\nmin_schema = 1\n",
        sha256_hex(CATALOG_V1)
    );
    server.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200).body(&latest);
    });
    server.mock(|when, then| {
        when.path("/catalog-v1.toml");
        then.status(200).body(CATALOG_V1);
    });

    let outcome = refresh(&settings(vec![server.url("")]), &auto).await;

    assert!(
        matches!(outcome, RefreshOutcome::Updated { .. }),
        "a state without its artifact is not a fresh cache"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_future_fetched_at_is_stale_not_fresh() {
    // Clock skew: a state stamped in the future must not launder into
    // permanently-fresh through saturating_sub — treat it as suspect
    // and re-check (the refresh restamps with the corrected clock).
    let dir = temp_dir("skew");
    let auto = dir.join("providers.auto.toml");
    std::fs::write(&auto, CATALOG_V1).expect("the artifact is installed");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    write_state(
        &state_path(&auto),
        &CatalogState {
            version: "v1".to_string(),
            fetched_at: now + 365 * 24 * 3600,
        },
    )
    .expect("state writes");

    assert_eq!(
        cache_fresh(&settings(Vec::new()), &auto),
        None,
        "a future stamp is suspect, not fresh"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_ttl_hours_multiplication_saturates() {
    // ttl_hours * 3600 on an absurd-but-legal settings value must not
    // panic (debug overflow) or wrap (release).
    let dir = temp_dir("ttl-overflow");
    let auto = dir.join("providers.auto.toml");
    std::fs::write(&auto, CATALOG_V1).expect("the artifact is installed");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    write_state(
        &state_path(&auto),
        &CatalogState {
            version: "v1".to_string(),
            fetched_at: now,
        },
    )
    .expect("state writes");
    let mut absurd = settings(Vec::new());
    absurd.ttl_hours = u64::MAX;

    assert_eq!(
        cache_fresh(&absurd, &auto).as_deref(),
        Some("v1"),
        "an absurd TTL saturates to still-fresh"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unusable_lock_file_is_named_not_blamed_on_a_peer() {
    // The lock sidecar path exists as a DIRECTORY: File::create fails.
    // That is a local filesystem problem to name, not "a concurrent
    // refresher holds the lock".
    let dir = temp_dir("lock-unusable");
    let auto = dir.join("providers.auto.toml");
    std::fs::create_dir(auto.with_file_name("providers.auto.lock"))
        .expect("the lock path is a directory");

    let outcome = refresh(&settings(vec!["http://127.0.0.1:1".to_string()]), &auto).await;

    match outcome {
        RefreshOutcome::Offline { reason } => assert!(
            !reason.contains("concurrent refresher"),
            "a local lock-file failure is mislabeled as a peer holding it: {reason}"
        ),
        other => panic!("an unusable lock file is offline, not {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_stalled_artifact_mirror_fails_over() {
    // The pointer's mirror accepts the artifact connection then
    // stalls: the artifact fetch must give up within the artifact
    // timeout and fail over to the next mirror — never park the
    // refresh task (and the lock it holds) for the process's life.
    let dir = temp_dir("stall");
    let auto = dir.join("providers.auto.toml");
    let stalled = httpmock::MockServer::start_async().await;
    let latest = format!(
        "version = \"v1\"\nsha256 = \"{}\"\nmin_schema = 1\n",
        sha256_hex(CATALOG_V1)
    );
    stalled.mock(|when, then| {
        when.path("/latest.toml");
        then.status(200).body(&latest);
    });
    stalled.mock(|when, then| {
        when.path("/catalog-v1.toml");
        then.status(200)
            .body(CATALOG_V1)
            .delay(std::time::Duration::from_secs(5));
    });
    let fallback = httpmock::MockServer::start_async().await;
    let fallback_artifact = fallback.mock(|when, then| {
        when.path("/catalog-v1.toml");
        then.status(200).body(CATALOG_V1);
    });
    let mut settings = settings(vec![stalled.url(""), fallback.url("")]);
    settings.artifact_timeout_ms = 50;

    let outcome =
        tokio::time::timeout(std::time::Duration::from_secs(4), refresh(&settings, &auto))
            .await
            .expect("the stall fails over instead of parking");

    assert!(
        matches!(outcome, RefreshOutcome::Updated { .. }),
        "the fallback mirror served the artifact"
    );
    assert_eq!(
        fallback_artifact.calls_async().await,
        1,
        "the fallback was used"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
