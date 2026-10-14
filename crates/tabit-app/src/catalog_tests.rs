use super::*;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tabit-catalog-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn settings(urls: Vec<String>) -> CatalogSettings {
    CatalogSettings {
        auto_update: true,
        ttl_hours: 24,
        urls,
        pointer_budget_ms: 1000,
        artifact_budget_ms: 2500,
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
fn boot_stage(tag: &str, urls: Vec<String>, budgets: (u64, u64)) -> (PathBuf, crate::AppOptions) {
    let dir = temp_dir(tag);
    let extensions = dir.join("extensions");
    std::fs::create_dir_all(&extensions).expect("extensions");
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
    assert!(matches!(plan, crate::assemble::CatalogPlan::Check(_)));
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

    let boot = crate::world_scan(
        &options,
        TabitConfig::default(),
        Some(config_path),
        std::sync::Arc::new(tabit_config::AuthConfig::default()),
    )
    .expect("world_scan");
    assert!(boot.checking_catalog());
    let registry = crate::build_registry(boot, &runtime);
    assert!(
        matches!(
            registry.catalog_status(),
            tabit_session::CatalogStatus::Pending
        ),
        "{:?}",
        registry.catalog_status()
    );
    assert!(!auto.exists(), "the merge ran before the install");

    // The detached task keeps downloading; it installs for next boot.
    runtime.block_on(tokio::time::sleep(std::time::Duration::from_millis(1400)));
    assert!(auto.exists(), "the background refresh landed");
    unsafe { std::env::remove_var("TABIT_SETTINGS") };
    let _ = std::fs::remove_dir_all(&dir);
}
