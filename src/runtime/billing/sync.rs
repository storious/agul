use std::cmp::Ordering;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::runtime::atomic_file::replace_file;

use super::{BillingError, PriceCatalog, compare_versions};

const SYNC_STATE_SCHEMA: &str = "agul/price-sync/v1";
const PRICE_CATALOG_URL_ENV: &str = "AGUL_PRICE_CATALOG_URL";
const AUTO_SYNC_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const AUTO_SYNC_TIMEOUT: Duration = Duration::from_secs(2);
const MANUAL_SYNC_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CATALOG_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ERROR_CHARS: usize = 300;

#[derive(Clone, Debug)]
pub(crate) struct PriceCatalogStore {
    root: PathBuf,
    required_target: Option<PriceCatalog>,
}

#[derive(Clone, Debug)]
pub(crate) struct PriceSyncResult {
    pub(crate) catalog: PriceCatalog,
    pub(crate) cache_path: PathBuf,
    pub(crate) changed: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct PriceStatus {
    pub(crate) cache_root: PathBuf,
    pub(crate) configured_url: Option<String>,
    pub(crate) catalog_id: Option<String>,
    pub(crate) catalog_version: Option<String>,
    pub(crate) catalog_path: Option<PathBuf>,
    pub(crate) using_embedded: bool,
    pub(crate) stale: bool,
    pub(crate) last_attempt_at: Option<u64>,
    pub(crate) last_success_at: Option<u64>,
    pub(crate) next_check_at: Option<u64>,
    pub(crate) last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct PriceSelection {
    pub(crate) catalog: Option<PriceCatalog>,
    pub(crate) notice: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SyncState {
    schema: String,
    source_url: Option<String>,
    last_attempt_at: Option<u64>,
    last_success_at: Option<u64>,
    catalog_file: Option<String>,
    catalog_id: Option<String>,
    catalog_version: Option<String>,
    last_error: Option<String>,
}

impl PriceCatalogStore {
    pub(crate) fn discover(
        override_root: Option<&Path>,
        fallback: Option<&PriceCatalog>,
    ) -> Result<Self, BillingError> {
        let state_root = override_root
            .map(Path::to_path_buf)
            .or_else(default_state_root)
            .ok_or_else(|| BillingError::new("could not determine the user state directory"))?;
        Ok(Self {
            root: state_root.join("prices").join(catalog_scope(fallback)?),
            required_target: fallback.cloned(),
        })
    }

    pub(crate) fn configured_url(
        &self,
        override_url: Option<&str>,
    ) -> Result<Option<String>, BillingError> {
        let state = self.read_state()?;
        configured_url(override_url, &state)
    }

    pub(crate) fn status(
        &self,
        override_url: Option<&str>,
        fallback: Option<&PriceCatalog>,
    ) -> Result<PriceStatus, BillingError> {
        self.status_at(override_url, fallback, now_seconds())
    }

    pub(crate) fn sync(
        &self,
        source_url: &str,
        fallback: Option<&PriceCatalog>,
    ) -> Result<PriceSyncResult, BillingError> {
        self.sync_at(source_url, fallback, MANUAL_SYNC_TIMEOUT, now_seconds())
    }

    pub(crate) fn select_for_chat(&self, fallback: Option<PriceCatalog>) -> PriceSelection {
        let environment_url = env::var(PRICE_CATALOG_URL_ENV).ok();
        self.select_for_chat_at(
            fallback,
            now_seconds(),
            AUTO_SYNC_TIMEOUT,
            environment_url.as_deref(),
        )
    }

    fn status_at(
        &self,
        override_url: Option<&str>,
        fallback: Option<&PriceCatalog>,
        now: u64,
    ) -> Result<PriceStatus, BillingError> {
        let state = self.read_state()?;
        let cached = self.cached_catalog(&state)?;
        let configured_url = configured_url(override_url, &state)?;
        let next_check_at = configured_url.as_ref().map(|_| {
            if configured_url.as_deref() != state.source_url.as_deref() {
                now
            } else {
                state.last_attempt_at.map_or(now, |attempted_at| {
                    attempted_at.saturating_add(AUTO_SYNC_INTERVAL.as_secs())
                })
            }
        });
        let (catalog, path, using_embedded) = match cached {
            Some((catalog, path)) if cached_is_preferred(&catalog, fallback)? => {
                (Some(catalog), Some(path), false)
            }
            _ => (fallback.cloned(), None, fallback.is_some()),
        };
        Ok(PriceStatus {
            cache_root: self.root.clone(),
            configured_url,
            catalog_id: catalog.as_ref().map(|catalog| catalog.id.clone()),
            catalog_version: catalog.as_ref().map(|catalog| catalog.version.clone()),
            catalog_path: path,
            using_embedded,
            stale: catalog
                .as_ref()
                .and_then(|catalog| catalog.review_after)
                .is_some_and(|review_after| now >= review_after),
            last_attempt_at: state.last_attempt_at,
            last_success_at: state.last_success_at,
            next_check_at,
            last_error: state.last_error,
        })
    }

    fn select_for_chat_at(
        &self,
        fallback: Option<PriceCatalog>,
        now: u64,
        timeout: Duration,
        environment_url: Option<&str>,
    ) -> PriceSelection {
        let state = match self.read_state() {
            Ok(state) => state,
            Err(_) => {
                return PriceSelection {
                    catalog: fallback,
                    notice: Some("price ! · agul price status".to_string()),
                };
            }
        };
        let mut notice = None;
        let mut catalog = match self.cached_catalog(&state) {
            Ok(Some((catalog, _))) => match cached_is_preferred(&catalog, fallback.as_ref()) {
                Ok(true) => Some(catalog),
                Ok(false) => fallback.clone(),
                Err(_) => {
                    notice = Some("price ! · agul price status".to_string());
                    fallback.clone()
                }
            },
            Ok(None) => fallback.clone(),
            Err(_) => {
                notice = Some("price ! · agul price status".to_string());
                fallback.clone()
            }
        };
        let source_url = match configured_url_from(None, environment_url, &state) {
            Ok(source_url) => source_url,
            Err(_) => {
                notice = Some("price ! · agul price status".to_string());
                None
            }
        };
        let due = source_url.as_deref() != state.source_url.as_deref()
            || state.last_attempt_at.is_none_or(|attempted_at| {
                now >= attempted_at.saturating_add(AUTO_SYNC_INTERVAL.as_secs())
            });
        if due && let Some(source_url) = source_url {
            match self.sync_at(&source_url, fallback.as_ref(), timeout, now) {
                Ok(result) => {
                    if result.changed {
                        notice = Some("price ↑".to_string());
                    }
                    catalog = Some(result.catalog);
                }
                Err(_) => notice = Some("price ! · agul price status".to_string()),
            }
        }
        if notice.is_none()
            && catalog
                .as_ref()
                .and_then(|catalog| catalog.review_after)
                .is_some_and(|review_after| now >= review_after)
        {
            notice = Some("price ? · agul price status".to_string());
        }
        PriceSelection { catalog, notice }
    }

    fn sync_at(
        &self,
        source_url: &str,
        fallback: Option<&PriceCatalog>,
        timeout: Duration,
        attempted_at: u64,
    ) -> Result<PriceSyncResult, BillingError> {
        let previous = self.read_state()?;
        let cached = self.cached_catalog(&previous).ok().flatten();
        let current = match cached.as_ref().map(|(catalog, _)| catalog) {
            Some(cached) if cached_is_preferred(cached, fallback)? => Some(cached),
            _ => fallback,
        };
        let result = self.fetch_and_cache(source_url, current, timeout);
        let mut state = previous;
        state.schema = SYNC_STATE_SCHEMA.to_string();
        state.source_url = Some(source_url.to_string());
        state.last_attempt_at = Some(attempted_at);
        match &result {
            Ok(result) => {
                state.last_success_at = Some(attempted_at);
                state.catalog_file = result
                    .cache_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string);
                state.catalog_id = Some(result.catalog.id.clone());
                state.catalog_version = Some(result.catalog.version.clone());
                state.last_error = None;
            }
            Err(error) => {
                state.last_error = Some(truncate(&error.to_string(), MAX_ERROR_CHARS));
            }
        }
        if let Err(state_error) = self.write_state(&state) {
            return match result {
                Ok(_) => Err(state_error),
                Err(error) => Err(BillingError::new(format!(
                    "{error}; could not save price sync status: {state_error}"
                ))),
            };
        }
        result
    }

    fn fetch_and_cache(
        &self,
        source_url: &str,
        current: Option<&PriceCatalog>,
        timeout: Duration,
    ) -> Result<PriceSyncResult, BillingError> {
        validate_url(source_url)?;
        let client = Client::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()
            .map_err(|error| BillingError::new(format!("could not create HTTP client: {error}")))?;
        let response = client
            .get(source_url)
            .header("accept", "application/json")
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|error| BillingError::new(format!("price catalog request failed: {error}")))?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_CATALOG_BYTES)
        {
            return Err(BillingError::new("price catalog response is too large"));
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_CATALOG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| BillingError::new(format!("could not read price catalog: {error}")))?;
        if bytes.len() as u64 > MAX_CATALOG_BYTES {
            return Err(BillingError::new("price catalog response is too large"));
        }
        let json = std::str::from_utf8(&bytes)
            .map_err(|error| BillingError::new(format!("price catalog is not UTF-8: {error}")))?;
        let catalog = PriceCatalog::from_json(json)?;
        if self
            .required_target
            .as_ref()
            .is_some_and(|target| !catalog_matches_required_target(&catalog, target))
        {
            return Err(BillingError::new(format!(
                "price catalog {}@{} does not match the selected provider, origin, and model target",
                catalog.id, catalog.version
            )));
        }
        let changed = current
            .is_none_or(|current| current.id != catalog.id || current.version != catalog.version);
        if let Some(current) = current.filter(|current| current.id == catalog.id) {
            match compare_versions(&catalog.version, &current.version)? {
                Ordering::Less => {
                    return Err(BillingError::new(format!(
                        "price catalog {}@{} is older than current {}@{}",
                        catalog.id, catalog.version, current.id, current.version
                    )));
                }
                Ordering::Equal if &catalog != current => {
                    return Err(BillingError::new(format!(
                        "price catalog {}@{} changed without a new version",
                        catalog.id, catalog.version
                    )));
                }
                _ => {}
            }
        }
        let cache_path = self.write_catalog(&catalog)?;
        Ok(PriceSyncResult {
            catalog,
            cache_path,
            changed,
        })
    }

    fn cached_catalog(
        &self,
        state: &SyncState,
    ) -> Result<Option<(PriceCatalog, PathBuf)>, BillingError> {
        let Some(file) = state.catalog_file.as_deref() else {
            return Ok(None);
        };
        let path = self.root.join(file);
        let json = fs::read_to_string(&path).map_err(|error| {
            BillingError::new(format!("could not read cached {}: {error}", path.display()))
        })?;
        let catalog = PriceCatalog::from_json(&json)?;
        Ok(Some((catalog, path)))
    }

    fn read_state(&self) -> Result<SyncState, BillingError> {
        let path = self.root.join("sync.json");
        if !path.is_file() {
            return Ok(SyncState {
                schema: SYNC_STATE_SCHEMA.to_string(),
                ..SyncState::default()
            });
        }
        let bytes = fs::read(&path).map_err(|error| {
            BillingError::new(format!("could not read {}: {error}", path.display()))
        })?;
        let state: SyncState = serde_json::from_slice(&bytes).map_err(|error| {
            BillingError::new(format!("could not parse {}: {error}", path.display()))
        })?;
        if state.schema != SYNC_STATE_SCHEMA {
            return Err(BillingError::new(format!(
                "{} has unsupported sync state schema",
                path.display()
            )));
        }
        Ok(state)
    }

    fn write_state(&self, state: &SyncState) -> Result<(), BillingError> {
        fs::create_dir_all(&self.root).map_err(|error| {
            BillingError::new(format!("could not create {}: {error}", self.root.display()))
        })?;
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|error| BillingError::new(format!("could not encode sync state: {error}")))?;
        let path = self.root.join("sync.json");
        replace_file(&path, &bytes).map_err(|error| {
            BillingError::new(format!("could not save {}: {error}", path.display()))
        })
    }

    fn write_catalog(&self, catalog: &PriceCatalog) -> Result<PathBuf, BillingError> {
        fs::create_dir_all(&self.root).map_err(|error| {
            BillingError::new(format!("could not create {}: {error}", self.root.display()))
        })?;
        let path = self.root.join(format!(
            "catalog-{}-{}.json",
            safe_component(&catalog.id),
            safe_component(&catalog.version)
        ));
        if path.is_file() {
            let existing = fs::read_to_string(&path).map_err(|error| {
                BillingError::new(format!("could not read {}: {error}", path.display()))
            })?;
            if PriceCatalog::from_json(&existing)? == *catalog {
                return Ok(path);
            }
            return Err(BillingError::new(format!(
                "cached {} already contains different data",
                path.display()
            )));
        }
        let bytes = serde_json::to_vec_pretty(catalog).map_err(|error| {
            BillingError::new(format!("could not encode price catalog: {error}"))
        })?;
        atomic_create(&path, &bytes)?;
        Ok(path)
    }
}

fn configured_url(
    override_url: Option<&str>,
    state: &SyncState,
) -> Result<Option<String>, BillingError> {
    let environment_url = env::var(PRICE_CATALOG_URL_ENV).ok();
    configured_url_from(override_url, environment_url.as_deref(), state)
}

fn cached_is_preferred(
    cached: &PriceCatalog,
    fallback: Option<&PriceCatalog>,
) -> Result<bool, BillingError> {
    let Some(fallback) = fallback else {
        return Ok(true);
    };
    if !catalog_matches_required_target(cached, fallback) {
        return Ok(false);
    }
    if fallback.id != cached.id {
        return Ok(true);
    }
    match compare_versions(&cached.version, &fallback.version)? {
        Ordering::Greater => Ok(true),
        Ordering::Less => Ok(false),
        Ordering::Equal if cached == fallback => Ok(true),
        Ordering::Equal => Err(BillingError::new(format!(
            "price catalog {}@{} changed without a new version",
            cached.id, cached.version
        ))),
    }
}

fn catalogs_share_target(left: &PriceCatalog, right: &PriceCatalog) -> bool {
    left.cards.iter().any(|left| {
        right.cards.iter().any(|right| {
            left.provider == right.provider
                && left.origin.trim_end_matches('/') == right.origin.trim_end_matches('/')
                && left.models.iter().any(|model| right.models.contains(model))
        })
    })
}

fn catalog_matches_required_target(catalog: &PriceCatalog, target: &PriceCatalog) -> bool {
    let card_matches_origin = |card: &super::PriceCard, expected: &super::PriceCard| {
        card.provider == expected.provider
            && card.origin.trim_end_matches('/') == expected.origin.trim_end_matches('/')
    };
    catalogs_share_target(catalog, target)
        && catalog.cards.iter().all(|card| {
            target
                .cards
                .iter()
                .any(|expected| card_matches_origin(card, expected))
        })
        && target.cards.iter().all(|expected| {
            expected.models.iter().all(|model| {
                catalog
                    .cards
                    .iter()
                    .any(|card| card_matches_origin(card, expected) && card.models.contains(model))
            })
        })
}

fn catalog_scope(fallback: Option<&PriceCatalog>) -> Result<String, BillingError> {
    let Some(fallback) = fallback else {
        return Ok("custom".to_string());
    };
    let provider = fallback
        .cards
        .first()
        .map(|card| card.provider.as_str())
        .ok_or_else(|| BillingError::new("price catalog has no provider scope"))?;
    if fallback.cards.iter().any(|card| card.provider != provider) {
        return Err(BillingError::new(
            "a provider price cache cannot span multiple providers",
        ));
    }
    Ok(safe_component(provider))
}

fn configured_url_from(
    override_url: Option<&str>,
    environment_url: Option<&str>,
    state: &SyncState,
) -> Result<Option<String>, BillingError> {
    let url = non_empty(override_url)
        .map(str::to_string)
        .or_else(|| non_empty(environment_url).map(str::to_string))
        .or_else(|| state.source_url.clone());
    if let Some(url) = url.as_deref() {
        validate_url(url)?;
    }
    Ok(url)
}

fn validate_url(value: &str) -> Result<(), BillingError> {
    let url = Url::parse(value)
        .map_err(|error| BillingError::new(format!("invalid price catalog URL: {error}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(BillingError::new(
            "price catalog URL must use http:// or https://",
        ));
    }
    Ok(())
}

fn atomic_create(path: &Path, bytes: &[u8]) -> Result<(), BillingError> {
    let parent = path
        .parent()
        .ok_or_else(|| BillingError::new("price cache path has no parent"))?;
    let temporary = parent.join(format!(
        ".{}-{}-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("price"),
        std::process::id(),
        now_nanos()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| {
                BillingError::new(format!("could not create {}: {error}", temporary.display()))
            })?;
        file.write_all(bytes).map_err(|error| {
            BillingError::new(format!("could not write {}: {error}", temporary.display()))
        })?;
        file.sync_all().map_err(|error| {
            BillingError::new(format!("could not flush {}: {error}", temporary.display()))
        })?;
        if path.exists() {
            return Err(BillingError::new(format!(
                "price cache already exists: {}",
                path.display()
            )));
        }
        fs::rename(&temporary, path).map_err(|error| {
            BillingError::new(format!("could not finish {}: {error}", path.display()))
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn safe_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn truncate(value: &str, limit: usize) -> String {
    let mut characters = value.chars();
    let mut result = characters.by_ref().take(limit).collect::<String>();
    if characters.next().is_some() {
        result.push('…');
    }
    result
}

fn default_state_root() -> Option<PathBuf> {
    if let Some(path) = env::var_os("LOCALAPPDATA").or_else(|| env::var_os("APPDATA")) {
        return Some(PathBuf::from(path).join("Agul"));
    }
    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(path).join("agul"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state/agul"))
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::thread;

    use tempfile::TempDir;

    use super::*;

    const TEST_NOW: u64 = 1_788_000_000;

    #[test]
    fn valid_catalog_is_atomically_cached_and_reported() {
        let root = TempDir::new().expect("temporary state");
        let store = deepseek_store(&root);
        let catalog = revised_catalog("2026-08-28");
        let server = FakeServer::start(vec![Reply::json(&catalog)]);

        let result = store
            .sync_at(
                &server.url(),
                Some(&PriceCatalog::builtin_deepseek_usd()),
                Duration::from_secs(2),
                TEST_NOW,
            )
            .unwrap();
        server.finish();

        assert!(result.changed);
        assert!(result.cache_path.is_file());
        let status = store
            .status_at(None, None, TEST_NOW + 1)
            .expect("cached status");
        assert_eq!(status.catalog_id.as_deref(), Some("deepseek-official-usd"));
        assert_eq!(status.catalog_version.as_deref(), Some("2026-08-28"));
        assert_eq!(status.last_attempt_at, Some(TEST_NOW));
        assert_eq!(status.last_success_at, Some(TEST_NOW));
        assert_eq!(
            status.next_check_at,
            Some(TEST_NOW + AUTO_SYNC_INTERVAL.as_secs())
        );
        assert_eq!(status.catalog_path.as_ref(), Some(&result.cache_path));
        assert!(!status.using_embedded);
        assert_eq!(
            cache_file_names(&store),
            vec!["catalog-deepseek-official-usd-2026-08-28.json", "sync.json",]
        );
    }

    #[test]
    fn same_version_mutation_and_invalid_upgrade_keep_the_last_good_catalog() {
        let root = TempDir::new().expect("temporary state");
        let store = deepseek_store(&root);
        let original = revised_catalog("2026-08-28");
        let first = FakeServer::start(vec![Reply::json(&original)]);
        let first_url = first.url();
        let first_result = store
            .sync_at(
                &first_url,
                Some(&PriceCatalog::builtin_deepseek_usd()),
                Duration::from_secs(2),
                TEST_NOW,
            )
            .unwrap();
        first.finish();

        let mut mutation = original.clone();
        mutation.cards[0].bands[0]
            .rates
            .cache_miss_input_nanos_per_million += 1;
        let second = FakeServer::start(vec![Reply::json(&mutation)]);
        let error = store
            .sync_at(
                &second.url(),
                Some(&PriceCatalog::builtin_deepseek_usd()),
                Duration::from_secs(2),
                TEST_NOW + 100,
            )
            .unwrap_err();
        second.finish();
        assert!(error.to_string().contains("changed without a new version"));

        let mut invalid = revised_catalog("2026-08-29");
        invalid.cards[0].schedule[1].start_minute_utc = 200;
        let invalid_json = serde_json::to_vec(&invalid).unwrap();
        let third = FakeServer::start(vec![Reply::raw_json(invalid_json)]);
        let third_url = third.url();
        let error = store
            .sync_at(
                &third_url,
                Some(&PriceCatalog::builtin_deepseek_usd()),
                Duration::from_secs(2),
                TEST_NOW + 200,
            )
            .unwrap_err();
        third.finish();
        assert!(error.to_string().contains("overlap"));

        let status = store.status_at(None, None, TEST_NOW + 201).unwrap();
        assert_eq!(status.catalog_version.as_deref(), Some("2026-08-28"));
        assert_eq!(status.catalog_path.as_ref(), Some(&first_result.cache_path));
        assert_eq!(status.last_success_at, Some(TEST_NOW));
        assert_eq!(status.last_attempt_at, Some(TEST_NOW + 200));
        assert_eq!(status.configured_url.as_deref(), Some(third_url.as_str()));
        assert!(status.last_error.as_deref().unwrap().contains("overlap"));
        assert_eq!(
            fs::read_to_string(&first_result.cache_path).unwrap(),
            serde_json::to_string_pretty(&original).unwrap()
        );
        assert_eq!(
            cache_file_names(&store),
            vec!["catalog-deepseek-official-usd-2026-08-28.json", "sync.json",]
        );
    }

    #[test]
    fn automatic_checks_are_bounded_by_the_daily_interval_and_keep_chat_working() {
        assert_eq!(AUTO_SYNC_TIMEOUT, Duration::from_secs(2));
        let root = TempDir::new().expect("temporary state");
        let store = deepseek_store(&root);
        let server = FakeServer::start(vec![Reply::failure(), Reply::failure()]);
        store
            .write_state(&SyncState {
                schema: SYNC_STATE_SCHEMA.to_string(),
                source_url: Some(server.url()),
                ..SyncState::default()
            })
            .unwrap();
        let fallback = PriceCatalog::builtin_deepseek_usd();

        let first =
            store.select_for_chat_at(Some(fallback.clone()), TEST_NOW, AUTO_SYNC_TIMEOUT, None);
        assert_eq!(first.catalog, Some(fallback.clone()));
        assert_eq!(first.notice.as_deref(), Some("price ! · agul price status"));
        assert_eq!(server.requests(), 1);

        let quiet = store.select_for_chat_at(
            Some(fallback.clone()),
            TEST_NOW + 1,
            AUTO_SYNC_TIMEOUT,
            None,
        );
        assert_eq!(quiet.catalog, Some(fallback.clone()));
        assert_eq!(quiet.notice, None);
        assert_eq!(server.requests(), 1);

        let due = store.select_for_chat_at(
            Some(fallback.clone()),
            TEST_NOW + AUTO_SYNC_INTERVAL.as_secs(),
            AUTO_SYNC_TIMEOUT,
            None,
        );
        assert_eq!(due.catalog, Some(fallback));
        assert_eq!(due.notice.as_deref(), Some("price ! · agul price status"));
        assert_eq!(server.requests(), 2);
        server.finish();
    }

    #[test]
    fn changing_the_source_is_due_immediately_then_throttled() {
        let root = TempDir::new().expect("temporary state");
        let store = deepseek_store(&root);
        store
            .write_state(&SyncState {
                schema: SYNC_STATE_SCHEMA.to_string(),
                source_url: Some("http://127.0.0.1/old.json".to_string()),
                last_attempt_at: Some(TEST_NOW),
                ..SyncState::default()
            })
            .unwrap();
        let server = FakeServer::start(vec![Reply::failure()]);
        let new_url = server.url();
        let fallback = PriceCatalog::builtin_deepseek_usd();

        let first = store.select_for_chat_at(
            Some(fallback.clone()),
            TEST_NOW + 1,
            AUTO_SYNC_TIMEOUT,
            Some(&new_url),
        );
        assert_eq!(first.notice.as_deref(), Some("price ! · agul price status"));
        assert_eq!(server.requests(), 1);

        let second = store.select_for_chat_at(
            Some(fallback),
            TEST_NOW + 2,
            AUTO_SYNC_TIMEOUT,
            Some(&new_url),
        );
        assert_eq!(second.notice, None);
        assert_eq!(server.requests(), 1);
        let status = store.status_at(Some(&new_url), None, TEST_NOW + 2).unwrap();
        assert_eq!(
            status.next_check_at,
            Some(TEST_NOW + 1 + AUTO_SYNC_INTERVAL.as_secs())
        );
        server.finish();
    }

    #[test]
    fn no_configured_source_creates_no_sync_state_or_notice() {
        let root = TempDir::new().expect("temporary state");
        let store = deepseek_store(&root);
        let fallback = PriceCatalog::builtin_deepseek_usd();

        let selection =
            store.select_for_chat_at(Some(fallback.clone()), TEST_NOW, AUTO_SYNC_TIMEOUT, None);

        assert_eq!(selection.catalog, Some(fallback.clone()));
        assert_eq!(selection.notice, None);
        assert!(!store.root.exists());

        let status = store.status_at(None, Some(&fallback), TEST_NOW).unwrap();
        assert_eq!(status.next_check_at, None);
    }

    #[test]
    fn atomically_replaces_existing_sync_state() {
        let root = TempDir::new().expect("temporary state");
        let store = deepseek_store(&root);
        store
            .write_state(&SyncState {
                schema: SYNC_STATE_SCHEMA.to_string(),
                source_url: Some("http://127.0.0.1/first.json".to_string()),
                last_attempt_at: Some(TEST_NOW),
                ..SyncState::default()
            })
            .unwrap();
        store
            .write_state(&SyncState {
                schema: SYNC_STATE_SCHEMA.to_string(),
                source_url: Some("http://127.0.0.1/second.json".to_string()),
                last_attempt_at: Some(TEST_NOW + 1),
                ..SyncState::default()
            })
            .unwrap();

        let state = store.read_state().unwrap();
        assert_eq!(
            state.source_url.as_deref(),
            Some("http://127.0.0.1/second.json")
        );
        assert_eq!(state.last_attempt_at, Some(TEST_NOW + 1));
        assert_eq!(cache_file_names(&store), vec!["sync.json"]);
    }

    #[test]
    fn immutable_catalog_create_preserves_an_existing_file() {
        let root = TempDir::new().expect("temporary state");
        let path = root.path().join("catalog.json");
        fs::write(&path, b"original").unwrap();

        let error = atomic_create(&path, b"replacement").unwrap_err();

        assert!(error.to_string().contains("price cache already exists"));
        assert_eq!(fs::read(&path).unwrap(), b"original");
    }

    #[test]
    fn configured_source_without_an_attempt_is_due_now() {
        let root = TempDir::new().expect("temporary state");
        let store = deepseek_store(&root);
        store
            .write_state(&SyncState {
                schema: SYNC_STATE_SCHEMA.to_string(),
                source_url: Some("http://127.0.0.1/catalog.json".to_string()),
                ..SyncState::default()
            })
            .unwrap();

        let status = store
            .status_at(None, Some(&PriceCatalog::builtin_deepseek_usd()), TEST_NOW)
            .unwrap();
        assert_eq!(status.next_check_at, Some(TEST_NOW));
    }

    #[test]
    fn newer_embedded_fallback_supersedes_an_older_cache() {
        let root = TempDir::new().expect("temporary state");
        let store = deepseek_store(&root);
        let fallback = PriceCatalog::builtin_deepseek_usd();
        let cached = revised_catalog("2026-08-26");
        let cached_path = store.write_catalog(&cached).unwrap();
        store
            .write_state(&SyncState {
                schema: SYNC_STATE_SCHEMA.to_string(),
                catalog_file: cached_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string),
                catalog_id: Some(cached.id.clone()),
                catalog_version: Some(cached.version.clone()),
                ..SyncState::default()
            })
            .unwrap();

        let selection =
            store.select_for_chat_at(Some(fallback.clone()), TEST_NOW, AUTO_SYNC_TIMEOUT, None);
        assert_eq!(selection.catalog, Some(fallback.clone()));
        assert_eq!(selection.notice, None);

        let status = store.status_at(None, Some(&fallback), TEST_NOW).unwrap();
        assert_eq!(status.catalog_version.as_deref(), Some("2026-08-27.1"));
        assert!(status.using_embedded);
        assert_eq!(status.catalog_path, None);
    }

    #[test]
    fn provider_caches_do_not_share_state_or_sources() {
        let root = TempDir::new().expect("temporary state");
        let deepseek = PriceCatalog::builtin_deepseek_usd();
        let deepseek_store =
            PriceCatalogStore::discover(Some(root.path()), Some(&deepseek)).unwrap();
        deepseek_store
            .write_state(&SyncState {
                schema: SYNC_STATE_SCHEMA.to_string(),
                source_url: Some("http://127.0.0.1/deepseek.json".to_string()),
                ..SyncState::default()
            })
            .unwrap();
        let glm = PriceCatalog::builtin_glm_cny();
        let glm_store = PriceCatalogStore::discover(Some(root.path()), Some(&glm)).unwrap();

        let status = glm_store.status_at(None, Some(&glm), TEST_NOW).unwrap();

        assert_eq!(
            deepseek_store.root,
            root.path().join("prices").join("deepseek")
        );
        assert_eq!(glm_store.root, root.path().join("prices").join("glm"));
        assert_eq!(status.catalog_id.as_deref(), Some("glm-official-cny"));
        assert_eq!(status.configured_url, None);
        assert!(status.using_embedded);
    }

    #[test]
    fn provider_sync_rejects_a_catalog_for_another_target_before_caching_it() {
        let root = TempDir::new().expect("temporary state");
        let store = deepseek_store(&root);
        let wrong = PriceCatalog::builtin_glm_cny();
        let server = FakeServer::start(vec![Reply::json(&wrong)]);

        let error = store
            .sync_at(
                &server.url(),
                Some(&PriceCatalog::builtin_deepseek_usd()),
                Duration::from_secs(2),
                TEST_NOW,
            )
            .unwrap_err();
        server.finish();

        assert!(
            error
                .to_string()
                .contains("does not match the selected provider, origin, and model target")
        );
        assert_eq!(cache_file_names(&store), vec!["sync.json"]);
        assert!(
            store
                .status_at(None, None, TEST_NOW)
                .unwrap()
                .last_error
                .as_deref()
                .unwrap()
                .contains("does not match")
        );
    }

    #[test]
    fn provider_target_rejects_mixed_catalogs_but_allows_new_models_on_the_same_origin() {
        let target = PriceCatalog::builtin_deepseek_usd();
        let mut mixed = target.clone();
        let mut glm = PriceCatalog::builtin_glm_cny();
        mixed.cards.push(glm.cards.remove(0));
        assert!(!catalog_matches_required_target(&mixed, &target));

        let mut expanded = target.clone();
        expanded.cards[0].models.push("deepseek-next".to_string());
        assert!(catalog_matches_required_target(&expanded, &target));

        let glm_target = PriceCatalog::builtin_glm_cny();
        let mut incomplete = glm_target.clone();
        incomplete.cards[0]
            .models
            .retain(|model| model == "glm-5.2");
        assert!(!catalog_matches_required_target(&incomplete, &glm_target));
        assert!(!cached_is_preferred(&incomplete, Some(&glm_target)).unwrap());
    }

    #[test]
    fn custom_cache_accepts_a_catalog_without_a_provider_preset() {
        let root = TempDir::new().expect("temporary state");
        let store = PriceCatalogStore::discover(Some(root.path()), None).unwrap();
        let catalog = PriceCatalog::builtin_glm_cny();
        let server = FakeServer::start(vec![Reply::json(&catalog)]);

        let result = store
            .sync_at(&server.url(), None, Duration::from_secs(2), TEST_NOW)
            .unwrap();
        server.finish();

        assert_eq!(store.root, root.path().join("prices").join("custom"));
        assert_eq!(result.catalog, catalog);
    }

    fn deepseek_store(root: &TempDir) -> PriceCatalogStore {
        PriceCatalogStore::discover(
            Some(root.path()),
            Some(&PriceCatalog::builtin_deepseek_usd()),
        )
        .unwrap()
    }

    fn revised_catalog(version: &str) -> PriceCatalog {
        let mut catalog = PriceCatalog::builtin_deepseek_usd();
        catalog.version = version.to_string();
        catalog
    }

    fn cache_file_names(store: &PriceCatalogStore) -> Vec<String> {
        let mut names = fs::read_dir(&store.root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    struct Reply {
        status: &'static str,
        body: Vec<u8>,
    }

    impl Reply {
        fn json(catalog: &PriceCatalog) -> Self {
            Self::raw_json(serde_json::to_vec(catalog).unwrap())
        }

        fn raw_json(body: Vec<u8>) -> Self {
            Self {
                status: "200 OK",
                body,
            }
        }

        fn failure() -> Self {
            Self {
                status: "503 Service Unavailable",
                body: b"unavailable".to_vec(),
            }
        }
    }

    struct FakeServer {
        url: String,
        requests: Arc<AtomicUsize>,
        handle: thread::JoinHandle<()>,
    }

    impl FakeServer {
        fn start(replies: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/catalog.json", listener.local_addr().unwrap());
            let requests = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&requests);
            let handle = thread::spawn(move || {
                for reply in replies {
                    let (mut stream, _) = listener.accept().unwrap();
                    read_headers(&mut stream);
                    counter.fetch_add(1, AtomicOrdering::SeqCst);
                    let headers = format!(
                        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        reply.status,
                        reply.body.len()
                    );
                    stream.write_all(headers.as_bytes()).unwrap();
                    stream.write_all(&reply.body).unwrap();
                    stream.flush().unwrap();
                }
            });
            Self {
                url,
                requests,
                handle,
            }
        }

        fn url(&self) -> String {
            self.url.clone()
        }

        fn requests(&self) -> usize {
            self.requests.load(AtomicOrdering::SeqCst)
        }

        fn finish(self) {
            self.handle.join().unwrap();
        }
    }

    fn read_headers(stream: &mut TcpStream) {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 512];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "request ended before headers");
            bytes.extend_from_slice(&buffer[..count]);
        }
    }
}
