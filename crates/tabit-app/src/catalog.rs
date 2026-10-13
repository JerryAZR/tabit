//! The model catalog manager (PROVIDERS.md): the local half of the
//! catalog channel. The catalog is generated externally (the
//! generator's CI) and downloaded; this module merges the last
//! download at boot and refreshes it in the background when stale.
//!
//! The channel's shape: a mirror list (ordered, first success wins),
//! each mirror serving a mutable `latest.toml` pointer (`version`,
//! `sha256`, `min_schema`) and immutable versioned artifacts
//! `catalog-<version>.toml` — ordinary providers-fragment syntax.
//! The download lands at `providers.auto.toml` next to the user's
//! `providers.toml` and merges UNDER user config and extension
//! fragments (a declared provider keeps its fields; catalog models
//! fill in; a declared model id wins whole).
//!
//! Failure posture (external errors fail gracefully): the channel
//! down, a corrupt artifact, a checksum mismatch — keep the last
//! cache and warn; boot is never blocked. `catalog.auto_update =
//! false` disables the channel outright.

use std::path::{Path, PathBuf};

use sha2::Digest as _;
use tabit_config::{CatalogSettings, TabitConfig};

/// The artifact schema this client consumes. A catalog whose
/// `min_schema` exceeds it is refused (loudly — a stale client must
/// not silently misread a newer catalog).
const SUPPORTED_SCHEMA: u32 = 1;

/// The pointer file every mirror serves.
const LATEST: &str = "latest.toml";
/// The built-in mirror list (PROVIDERS.md): jsDelivr first (reliable
/// from China), GitHub raw second. An empty `catalog.urls` means this
/// list; a configured list REPLACES it — the explicit-pointer rule,
/// never a silent fallthrough.
const BUILTIN_MIRRORS: &[&str] = &[
    "https://cdn.jsdelivr.net/gh/JerryAZR/tabit-catalog@catalog",
    "https://raw.githubusercontent.com/JerryAZR/tabit-catalog/catalog",
];

/// The state sidecar next to the auto file: what we have and when we
/// got it. Freshness is the fetched-at stamp, not the file's mtime
/// (a user copying the file around must not reset the clock).
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct CatalogState {
    /// The downloaded catalog's version.
    version: String,
    /// When it was fetched, epoch seconds.
    fetched_at: u64,
}

/// The `latest.toml` pointer.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Latest {
    /// The catalog version; the artifact is `catalog-<version>.toml`.
    version: String,
    /// The artifact's sha256, hex.
    sha256: String,
    /// The oldest client schema that may consume this catalog.
    min_schema: u32,
}

/// The auto file's path: next to the user config that was read, else
/// the default home location (a bare machine's catalog still lands
/// where boot looks for it).
pub(crate) fn auto_path(config_path: Option<&Path>) -> Option<PathBuf> {
    config_path
        .map(|path| path.with_file_name("providers.auto.toml"))
        .or_else(|| {
            tabit_config::home_dir().map(|home| home.join(".tabit").join("providers.auto.toml"))
        })
}

fn state_path(auto_path: &Path) -> PathBuf {
    auto_path.with_file_name("providers.auto.state.toml")
}

/// Merge the last downloaded catalog under the config (boot path).
/// A missing auto file is the normal first-run case; a corrupt one
/// warns and is skipped — the generated file is replaceable by
/// definition.
pub(crate) fn merge_auto(config: &mut TabitConfig, auto_path: &Path, warnings: &mut Vec<String>) {
    if !auto_path.is_file() {
        return;
    }
    match TabitConfig::load(auto_path) {
        Ok(catalog) => config.merge_catalog(catalog),
        Err(detail) => warnings.push(format!(
            "catalog: {} unreadable ({detail}) — ignored; a refresh will rewrite it",
            auto_path.display()
        )),
    }
}

/// Refresh the catalog when stale: the background task boot spawns.
/// Best-effort — every failure mode is a silent-ish skip (the boot
/// log carries the reason); the LAST cache is always the fallback.
/// Never blocks boot; a concurrent refresher (another session) holds
/// the lock and this one skips — the winner just wrote fresh data.
pub(crate) async fn refresh(settings: &CatalogSettings, auto_path: &Path) {
    if !settings.auto_update {
        return;
    }
    let state_path = state_path(auto_path);
    let state = read_state(&state_path);
    if let Some(state) = &state {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let fresh_for = settings.ttl_hours * 3600;
        if now.saturating_sub(state.fetched_at) < fresh_for {
            return;
        }
    }

    // Serialize concurrent refreshers. The OS releases the claim on
    // any exit kind; a held lock means another process is refreshing
    // right now — skip.
    let lock_path = auto_path.with_file_name("providers.auto.lock");
    let lock = std::fs::File::create(&lock_path)
        .ok()
        .and_then(|file| file.try_lock().ok().map(|()| file));
    let Some(_lock) = lock else {
        return;
    };

    match download(settings, auto_path, &state_path, state.as_ref()).await {
        Ok(outcome) => tracing::debug!("catalog refresh: {outcome}"),
        Err(reason) => tracing::debug!("catalog refresh skipped: {reason}"),
    }
}

/// One refresh attempt over the mirror list, first success wins.
async fn download(
    settings: &CatalogSettings,
    auto_path: &Path,
    state_path: &Path,
    state: Option<&CatalogState>,
) -> Result<String, String> {
    let client = reqwest::Client::new();
    let mut failures = Vec::new();
    let urls: Vec<&str> = if settings.urls.is_empty() {
        BUILTIN_MIRRORS.to_vec()
    } else {
        settings.urls.iter().map(String::as_str).collect()
    };
    for base in urls {
        match try_mirror(&client, base, auto_path, state_path, state).await {
            Ok(outcome) => return Ok(format!("{base}: {outcome}")),
            Err(reason) => failures.push(format!("{base}: {reason}")),
        }
    }
    Err(format!("every mirror failed ({})", failures.join("; ")))
}

/// One mirror: pointer, version compare, artifact, checksum, atomic
/// install.
async fn try_mirror(
    client: &reqwest::Client,
    base: &str,
    auto_path: &Path,
    state_path: &Path,
    state: Option<&CatalogState>,
) -> Result<String, String> {
    let base = base.trim_end_matches('/');
    let latest_raw = get(client, &format!("{base}/{LATEST}")).await?;
    let latest_raw =
        String::from_utf8(latest_raw).map_err(|err| format!("pointer not UTF-8: {err}"))?;
    let latest: Latest =
        toml::from_str(&latest_raw).map_err(|err| format!("bad pointer: {err}"))?;
    if latest.min_schema > SUPPORTED_SCHEMA {
        return Err(format!(
            "catalog needs schema >= {} (this client speaks {SUPPORTED_SCHEMA})",
            latest.min_schema
        ));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    if state.is_some_and(|state| state.version == latest.version) {
        // Same catalog, fresh stamp — the TTL restarts.
        write_state(
            state_path,
            &CatalogState {
                version: latest.version,
                fetched_at: now,
            },
        )?;
        return Ok("already current".to_string());
    }
    let artifact = get(client, &format!("{base}/catalog-{}.toml", latest.version)).await?;
    let digest = sha2::Sha256::digest(&artifact);
    let digest = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if digest != latest.sha256 {
        return Err("checksum mismatch".to_string());
    }
    install(auto_path, &latest.version, &artifact)?;
    write_state(
        state_path,
        &CatalogState {
            version: latest.version.clone(),
            fetched_at: now,
        },
    )?;
    Ok(format!("installed {}", latest.version))
}

async fn get(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| format!("GET {url}: {err}"))?;
    let response = response
        .error_for_status()
        .map_err(|err| format!("GET {url}: {err}"))?;
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|err| format!("GET {url}: {err}"))
}

/// Install the artifact: validate it parses as a providers fragment
/// (a catalog that cannot parse is refused, never written), then
/// temp-write + rename so readers never see a torn file.
fn install(auto_path: &Path, version: &str, artifact: &[u8]) -> Result<(), String> {
    let body = String::from_utf8(artifact.to_vec()).map_err(|err| format!("not UTF-8: {err}"))?;
    TabitConfig::from_toml_str(&body, auto_path)
        .map_err(|err| format!("artifact does not parse: {err}"))?;
    let stamped = format!(
        "# generated by tabit's catalog manager (version {version}) — edits here are\n\
         # overwritten; override a model or provider in providers.toml instead.\n\n{body}"
    );
    let temp = auto_path.with_extension("toml.tmp");
    std::fs::write(&temp, stamped).map_err(|err| format!("write temp: {err}"))?;
    std::fs::rename(&temp, auto_path).map_err(|err| format!("rename: {err}"))?;
    Ok(())
}

fn read_state(path: &Path) -> Option<CatalogState> {
    let raw = std::fs::read_to_string(path).ok()?;
    toml::from_str(&raw).ok()
}

fn write_state(path: &Path, state: &CatalogState) -> Result<(), String> {
    let body = toml::to_string(state).map_err(|err| format!("state encodes: {err}"))?;
    std::fs::write(path, body).map_err(|err| format!("write state: {err}"))
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
