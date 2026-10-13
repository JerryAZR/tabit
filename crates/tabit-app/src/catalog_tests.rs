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
