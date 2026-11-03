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

/// One TCP connect attempt's budget — and, because `RacingResolver`
/// races ALL of a host's candidates in parallel, the whole connect
/// phase's. Sized for slow links: a lost SYN costs a 1s retransmit
/// and high-latency networks show multi-second connects. (Was 750ms,
/// which hyper-util then split EVENLY across a host's addresses —
/// 187ms per attempt on a 4-address mirror — so on a slow link every
/// candidate timed out and the connect failed outright, where a
/// Happy-Eyeballs client like curl sailed through.)
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The catalog client's DNS resolver — the connect strategy
/// hyper-util will not give us. Its connector walks a host's
/// addresses sequentially per family and splits the connect timeout
/// evenly across them (see CONNECT_TIMEOUT). So resolution itself
/// races a bare TCP connect to every candidate in parallel, each
/// with the full budget, and yields only the winner: division by
/// one, the full budget per attempt.
///
/// The probe targets port 443 — hyper's `Name` carries no port and
/// the channel speaks https. The probe's socket is dropped and
/// reqwest reconnects to the winner: one redundant bare handshake
/// per host per boot (reqwest's DNS cache and connection pool absorb
/// the rest). A single-candidate host skips the probe (nothing to
/// rank). When the probe finds no winner the UNRANKED list is
/// returned — hyper-util's own (slower, divided) strategy then runs,
/// so a mirror on a non-443 port still connects, never breaks.
#[derive(Debug, Clone, Copy)]
struct RacingResolver;

impl reqwest::dns::Resolve for RacingResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), 443))
                .await?
                .collect();
            let winner = match addrs.as_slice() {
                [] => None,
                [only] => Some(*only),
                _ => race_connect(&addrs, CONNECT_TIMEOUT).await,
            };
            // Port 0: reqwest fills in the URI's port (explicit, or
            // the scheme's default).
            let ranked: Vec<std::net::SocketAddr> = match winner {
                Some(addr) => vec![std::net::SocketAddr::new(addr.ip(), 0)],
                None => addrs
                    .iter()
                    .map(|addr| std::net::SocketAddr::new(addr.ip(), 0))
                    .collect(),
            };
            Ok(Box::new(ranked.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Race bare TCP connects to every candidate, each with the full
/// timeout; first success wins and the losers are dropped with their
/// sockets. `select_ok` is the one combinator: first `Ok`, or the
/// last error when all fail. No winner: None — the caller falls back
/// to the unranked list.
async fn race_connect(
    addrs: &[std::net::SocketAddr],
    timeout: std::time::Duration,
) -> Option<std::net::SocketAddr> {
    let attempts = addrs.iter().map(|addr| {
        Box::pin(async move {
            match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await {
                Ok(Ok(_stream)) => Ok(*addr),
                Ok(Err(err)) => Err(err),
                Err(_) => Err(std::io::Error::from(std::io::ErrorKind::TimedOut)),
            }
        })
    });
    futures::future::select_ok(attempts)
        .await
        .ok()
        .map(|(addr, _losers)| addr)
}

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
    // A stamp in the future is clock skew — suspect, not fresh:
    // re-check, and the refresh restamps with the corrected clock
    // (saturating_sub would launder it into fresh for the skew's
    // whole duration).
    if state.fetched_at > now {
        return None;
    }
    let ttl = settings.ttl_hours.saturating_mul(3600);
    if now - state.fetched_at >= ttl {
        return None;
    }
    // The state vouches for the artifact: within TTL only counts
    // while the catalog file itself is there and parses — a missing
    // or corrupt artifact re-downloads, never rides the stamp.
    installed_artifact(auto_path)?;
    Some(state.version)
}

/// The cache's version as `merge_auto` will use it: the state's
/// version, but only while the artifact it vouches for parses — a
/// missing or corrupt artifact contributes nothing to the merge, so
/// it contributes nothing to the wire's announced version either.
pub(crate) fn installed_version(auto_path: &Path) -> Option<String> {
    let state = read_state(&state_path(auto_path))?;
    installed_artifact(auto_path)?;
    Some(state.version)
}

/// The artifact is installed and parses.
fn installed_artifact(auto_path: &Path) -> Option<()> {
    tabit_config::TabitConfig::load(auto_path).ok().map(|_| ())
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
/// boot's patience (`pointer_budget_ms` + `artifact_budget_ms` in
/// `build_registry`) bounds how long BOOT waits — on expiry the
/// detached task keeps downloading and installs for the next boot.
/// Inside the task every FETCH is bounded instead: connects race
/// under `CONNECT_TIMEOUT`, each pointer fetch gets
/// `pointer_timeout_ms`, each artifact fetch `artifact_timeout_ms` —
/// a mirror that connects and stalls fails over like any other
/// failure, never parks the task (and the lock it holds) for the
/// process's life.
pub(crate) async fn refresh(settings: &CatalogSettings, auto_path: &Path) -> RefreshOutcome {
    if !settings.auto_update {
        return RefreshOutcome::Offline {
            reason: "auto_update is off".to_string(),
        };
    }
    if let Some(version) = cache_fresh(settings, auto_path) {
        return RefreshOutcome::Fresh { version };
    }

    // A bare machine: the config dir may not exist yet — the refresh
    // spawns before the session store creates it. The lock, the
    // artifact, and the state sidecar all land in it; create it first.
    if let Some(parent) = auto_path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        return RefreshOutcome::Offline {
            reason: format!(
                "the catalog directory {} is unusable: {err}",
                parent.display()
            ),
        };
    }

    // Serialize concurrent refreshers. The OS releases the claim on
    // any exit kind; a held lock means another process is refreshing
    // right now — skip.
    let lock_path = auto_path.with_file_name("providers.auto.lock");
    let file = match std::fs::File::create(&lock_path) {
        Ok(file) => file,
        Err(err) => {
            return RefreshOutcome::Offline {
                reason: format!(
                    "the refresh lock {} is unusable: {err}",
                    lock_path.display()
                ),
            };
        }
    };
    if let Err(err) = file.try_lock() {
        return RefreshOutcome::Offline {
            reason: match err {
                std::fs::TryLockError::WouldBlock => {
                    "a concurrent refresher holds the lock".to_string()
                }
                _ => format!(
                    "the refresh lock {} is unusable: {err}",
                    lock_path.display()
                ),
            },
        };
    }
    let _lock = file;

    let urls: Vec<String> = if settings.urls.is_empty() {
        BUILTIN_MIRRORS.iter().map(ToString::to_string).collect()
    } else {
        settings.urls.clone()
    };
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .dns_resolver(RacingResolver)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let pointer = match fetch_pointer(&client, &urls, settings).await {
        Ok(found) => found,
        Err(reason) => return RefreshOutcome::Offline { reason },
    };
    let outcome = apply_pointer(&client, &urls, settings, pointer, auto_path).await;
    match &outcome {
        RefreshOutcome::Offline { reason } => {
            tracing::debug!("catalog refresh: {reason}");
        }
        outcome => tracing::debug!("catalog refresh: {outcome:?}"),
    }
    outcome
}

/// The pointer phase: the first mirror whose `latest.toml` fetches
/// and parses wins. Each fetch is bounded (`pointer_timeout_ms` —
/// the artifact phase's `artifact_timeout_ms` counterpart): a mirror
/// that connects then stalls fails over to the next instead of
/// parking the refresh (and the lock it holds).
async fn fetch_pointer(
    client: &reqwest::Client,
    urls: &[String],
    settings: &CatalogSettings,
) -> Result<(usize, Latest), String> {
    let fetch_timeout = std::time::Duration::from_millis(settings.pointer_timeout_ms);
    let mut failures = Vec::new();
    for (index, base) in urls.iter().enumerate() {
        let base = base.trim_end_matches('/');
        let fetch =
            tokio::time::timeout(fetch_timeout, get(client, &format!("{base}/{LATEST}"))).await;
        match fetch {
            Ok(Ok(raw)) => {
                let raw = match String::from_utf8(raw) {
                    Ok(raw) => raw,
                    Err(err) => {
                        failures.push(format!("{base}: pointer not UTF-8: {err}"));
                        continue;
                    }
                };
                match toml::from_str::<Latest>(&raw) {
                    Ok(latest) => return Ok((index, latest)),
                    Err(err) => failures.push(format!("{base}: bad pointer: {err}")),
                }
            }
            Ok(Err(reason)) => failures.push(reason),
            Err(_) => failures.push(format!(
                "{base}: pointer fetch stalled ({}ms)",
                settings.pointer_timeout_ms
            )),
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
    settings: &CatalogSettings,
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
    let timeout = std::time::Duration::from_millis(settings.artifact_timeout_ms);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let state = read_state(&state_path(auto_path));
    if state.is_some_and(|state| state.version == latest.version)
        && installed_artifact(auto_path).is_some()
    {
        // Same catalog, the artifact verified present — fresh stamp,
        // the TTL restarts. A state without its artifact falls
        // through to the download: the stamp must never vouch for a
        // cache that isn't there.
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
        // The per-fetch timeout is the stall guard: a mirror that
        // connects then parks must not hold the refresh (and its
        // lock) hostage — it fails over like any other failure.
        let fetch = tokio::time::timeout(
            timeout,
            get(client, &format!("{base}/catalog-{}.toml", latest.version)),
        )
        .await;
        match fetch {
            Ok(Ok(artifact)) => {
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
            Ok(Err(reason)) => failures.push(reason),
            Err(_) => failures.push(format!(
                "{base}: artifact fetch stalled ({}ms)",
                settings.artifact_timeout_ms
            )),
        }
    }
    RefreshOutcome::Offline {
        reason: format!("artifact: every mirror failed ({})", failures.join("; ")),
    }
}

/// The catalog is a generated providers.toml — tens of KB today,
/// bounded growth by design. A mirror serving an unbounded body is
/// broken or hostile; cap the fetch.
const MAX_FETCH_BYTES: u64 = 32 * 1024 * 1024;

async fn get(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| format!("GET {url}: {err}"))?;
    let mut response = response
        .error_for_status()
        .map_err(|err| format!("GET {url}: {err}"))?;
    if let Some(len) = response.content_length()
        && len > MAX_FETCH_BYTES
    {
        return Err(format!(
            "GET {url}: the body declares {len} bytes, over the {MAX_FETCH_BYTES}-byte cap"
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| format!("GET {url}: {err}"))?
    {
        if body.len() as u64 + chunk.len() as u64 > MAX_FETCH_BYTES {
            return Err(format!(
                "GET {url}: the body exceeds the {MAX_FETCH_BYTES}-byte cap"
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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
