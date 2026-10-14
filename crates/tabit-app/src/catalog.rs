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

/// What a refresh attempt concluded — boot reports this (the wire's
/// catalog status, PROVIDERS.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefreshOutcome {
    /// Checked; the cache is already current (the pointer's version
    /// is the cache's) — the state re-stamps, the TTL restarts.
    Fresh { version: String },
    /// A new artifact verified and installed; a boot that awaited
    /// the refresh merges it immediately.
    Updated { version: String },
    /// No new data this boot: every mirror failed, a newer
    /// min_schema, a concurrent refresher holds the lock, or the
    /// pointer phase ran over budget. The LAST cache merges.
    Offline { reason: String },
}

/// The cache's version when it is within TTL, else None — the sync
/// pre-check that lets boot announce "checking" only when the
/// network is actually involved (the refresh re-checks under the
/// lock).
pub(crate) fn cache_fresh(settings: &CatalogSettings, auto_path: &Path) -> Option<String> {
    let state = read_state(&state_path(auto_path))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    (now.saturating_sub(state.fetched_at) < settings.ttl_hours * 3600).then_some(state.version)
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

/// Refresh the catalog when stale. Budgets (PROVIDERS.md): the
/// pointer phase is internally bounded (`pointer_budget_ms` — a
/// 100-byte fetch that cannot land in time means the artifact would
/// not either); the artifact phase is NOT internally bounded — the
/// boot's patience is the outer timeout in `build_registry`, and on
/// expiry the detached task keeps downloading and installs for the
/// next boot.
pub(crate) async fn refresh(settings: &CatalogSettings, auto_path: &Path) -> RefreshOutcome {
    if !settings.auto_update {
        return RefreshOutcome::Offline {
            reason: "auto_update is off".to_string(),
        };
    }
    if let Some(version) = cache_fresh(settings, auto_path) {
        return RefreshOutcome::Fresh { version };
    }

    // Serialize concurrent refreshers. The OS releases the claim on
    // any exit kind; a held lock means another process is refreshing
    // right now — skip.
    let lock_path = auto_path.with_file_name("providers.auto.lock");
    let lock = std::fs::File::create(&lock_path)
        .ok()
        .and_then(|file| file.try_lock().ok().map(|()| file));
    let Some(_lock) = lock else {
        return RefreshOutcome::Offline {
            reason: "a concurrent refresher holds the lock".to_string(),
        };
    };

    let urls: Vec<String> = if settings.urls.is_empty() {
        BUILTIN_MIRRORS.iter().map(ToString::to_string).collect()
    } else {
        settings.urls.clone()
    };
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_millis(750))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let pointer_phase = fetch_pointer(&client, &urls);
    let pointer = match tokio::time::timeout(
        std::time::Duration::from_millis(settings.pointer_budget_ms),
        pointer_phase,
    )
    .await
    {
        Ok(Ok(found)) => found,
        Ok(Err(reason)) => return RefreshOutcome::Offline { reason },
        Err(_) => {
            return RefreshOutcome::Offline {
                reason: format!("pointer over budget ({}ms)", settings.pointer_budget_ms),
            };
        }
    };
    let outcome = apply_pointer(&client, &urls, pointer, auto_path).await;
    match &outcome {
        RefreshOutcome::Offline { reason } => {
            tracing::debug!("catalog refresh: {reason}");
        }
        outcome => tracing::debug!("catalog refresh: {outcome:?}"),
    }
    outcome
}

/// The pointer phase: the first mirror whose `latest.toml` fetches
/// and parses wins.
async fn fetch_pointer(
    client: &reqwest::Client,
    urls: &[String],
) -> Result<(usize, Latest), String> {
    let mut failures = Vec::new();
    for (index, base) in urls.iter().enumerate() {
        let base = base.trim_end_matches('/');
        match get(client, &format!("{base}/{LATEST}")).await {
            Ok(raw) => {
                let raw = String::from_utf8(raw)
                    .map_err(|err| format!("{base}: pointer not UTF-8: {err}"))?;
                match toml::from_str::<Latest>(&raw) {
                    Ok(latest) => return Ok((index, latest)),
                    Err(err) => failures.push(format!("{base}: bad pointer: {err}")),
                }
            }
            Err(reason) => failures.push(reason),
        }
    }
    Err(format!("every mirror failed ({})", failures.join("; ")))
}

/// The pointer's ruling: restamp when the version is the cache's,
/// else fetch + verify + install the artifact — the pointer's mirror
/// first, the rest as fallback. Unbounded (the boot detaches past
/// its patience; the install still lands for next boot).
async fn apply_pointer(
    client: &reqwest::Client,
    urls: &[String],
    pointer: (usize, Latest),
    auto_path: &Path,
) -> RefreshOutcome {
    let (won, latest) = pointer;
    if latest.min_schema > SUPPORTED_SCHEMA {
        return RefreshOutcome::Offline {
            reason: format!(
                "catalog needs schema >= {} (this client speaks {SUPPORTED_SCHEMA})",
                latest.min_schema
            ),
        };
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let state = read_state(&state_path(auto_path));
    if state.is_some_and(|state| state.version == latest.version) {
        // Same catalog, fresh stamp — the TTL restarts.
        let _ = write_state(
            &state_path(auto_path),
            &CatalogState {
                version: latest.version.clone(),
                fetched_at: now,
            },
        );
        return RefreshOutcome::Fresh {
            version: latest.version,
        };
    }
    // The pointer's mirror first, then the rest in order.
    let order: Vec<&String> = urls.iter().skip(won).chain(urls.iter().take(won)).collect();
    let mut failures = Vec::new();
    for base in order {
        let base = base.trim_end_matches('/');
        match get(client, &format!("{base}/catalog-{}.toml", latest.version)).await {
            Ok(artifact) => {
                let digest = sha2::Sha256::digest(&artifact)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                if digest != latest.sha256 {
                    failures.push(format!("{base}: checksum mismatch"));
                    continue;
                }
                return match install(auto_path, &latest.version, &artifact) {
                    Ok(()) => {
                        let _ = write_state(
                            &state_path(auto_path),
                            &CatalogState {
                                version: latest.version.clone(),
                                fetched_at: now,
                            },
                        );
                        RefreshOutcome::Updated {
                            version: latest.version,
                        }
                    }
                    Err(reason) => RefreshOutcome::Offline { reason },
                };
            }
            Err(reason) => failures.push(reason),
        }
    }
    RefreshOutcome::Offline {
        reason: format!("artifact: every mirror failed ({})", failures.join("; ")),
    }
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
