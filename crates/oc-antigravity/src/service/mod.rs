//! High-level `AntigravityService` orchestrating OAuth login attempts, account pool management,
//! SQLite credential synchronization, token refresh caching, quota inspection, and model catalog caching.

use crate::accounts::AccountManager;
use crate::config::{AntigravityConfig, AntigravityConfig as Config, load_config};
use crate::constants::AntigravityEndpoints;
use crate::error::Result;
use crate::models::{
    DiscoveredModelCache, default_models_cache_path, load_discovered_models,
    overlay_catalog_models, resolve_antigravity_catalog_model,
};
use crate::signature_cache::SignatureCache;
use crate::storage::{AccountStorageV4, default_accounts_path};
use crate::token::TokenRefreshCoordinator;
use crate::version::VersionTracker;
use oc_store::Store;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

mod accounts;
mod oauth;
mod quotas;
mod rotation;
mod sync;
#[cfg(test)]
mod tests;
mod token;
mod types;

use oauth::OAuthAttemptEntry;
use types::CachedAccessToken;

pub(crate) use rotation::now_ms;
pub use types::{
    AccountStatusSummary, ActiveCredentials, OAuthAttemptStatusView, OAuthAttemptTime,
    OAuthAttemptView, QuotaRefreshReport,
};

const ATTEMPT_TTL_MS: i64 = 10 * 60 * 1000;

/// Shared runtime service owning Antigravity OAuth, multi-account rotation, quota & models.
#[derive(Clone, Debug)]
pub struct AntigravityService {
    inner: Arc<ServiceInner>,
}

#[derive(Debug)]
struct ServiceInner {
    cfg: RwLock<Config>,
    endpoints: AntigravityEndpoints,
    http: reqwest::Client,
    version: VersionTracker,
    accounts: StdMutex<AccountManager>,
    signature_cache: Arc<StdMutex<SignatureCache>>,
    discovered_models: StdMutex<DiscoveredModelCache>,
    models_cache_path: Option<PathBuf>,
    token_cache: StdMutex<HashMap<String, CachedAccessToken>>,
    refresh_coord: TokenRefreshCoordinator,
    oauth_attempts: AsyncMutex<HashMap<String, OAuthAttemptEntry>>,
    store: Option<Arc<Store>>,
}

impl AntigravityService {
    /// Initialize an [`AntigravityService`] from disk configuration, `antigravity-accounts.json`,
    /// signature/model caches, and optional `oc_store::Store` SQLite credentials.
    pub async fn open(
        store: Option<Arc<Store>>,
        config_dir: Option<PathBuf>,
        project_dir: Option<PathBuf>,
        endpoints: AntigravityEndpoints,
    ) -> Result<Self> {
        let cfg = load_config(project_dir.as_deref(), config_dir.as_deref())?;
        let now = now_ms();

        let accounts_path = Some(default_accounts_path(config_dir.as_deref()));
        let sig_path = Some(SignatureCache::default_path(config_dir.as_deref()));
        let models_path = Some(default_models_cache_path(config_dir.as_deref()));

        let manager = AccountManager::load(accounts_path, cfg.clone())?;
        let sig_cache = SignatureCache::load(sig_path, cfg.signature_cache.clone(), now)?;
        let discovered = models_path
            .as_deref()
            .and_then(|p| load_discovered_models(p).ok())
            .unwrap_or_default();

        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(300))
            .tcp_keepalive(Duration::from_secs(30))
            .http2_keep_alive_interval(Some(Duration::from_secs(30)))
            .http2_keep_alive_timeout(Duration::from_secs(15))
            .http2_keep_alive_while_idle(true)
            .build()?;

        let svc = Self {
            inner: Arc::new(ServiceInner {
                cfg: RwLock::new(cfg),
                endpoints,
                http,
                version: VersionTracker::new(),
                accounts: StdMutex::new(manager),
                signature_cache: Arc::new(StdMutex::new(sig_cache)),
                discovered_models: StdMutex::new(discovered),
                models_cache_path: models_path,
                token_cache: StdMutex::new(HashMap::new()),
                refresh_coord: TokenRefreshCoordinator::new(),
                oauth_attempts: AsyncMutex::new(HashMap::new()),
                store,
            }),
        };

        svc.sync_from_store().await?;
        Ok(svc)
    }

    /// Construct an isolated service for tests using temporary files or in-memory state.
    pub async fn in_memory_for_test(
        store: Option<Arc<Store>>,
        cfg: AntigravityConfig,
        endpoints: AntigravityEndpoints,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let sig_cache = SignatureCache::load(None, cfg.signature_cache.clone(), now_ms())?;
        let svc = Self {
            inner: Arc::new(ServiceInner {
                cfg: RwLock::new(cfg.clone()),
                endpoints,
                http,
                version: VersionTracker::new(),
                accounts: StdMutex::new(AccountManager::from_storage(
                    None,
                    AccountStorageV4::default(),
                    cfg,
                )),
                signature_cache: Arc::new(StdMutex::new(sig_cache)),
                discovered_models: StdMutex::new(DiscoveredModelCache::default()),
                models_cache_path: None,
                token_cache: StdMutex::new(HashMap::new()),
                refresh_coord: TokenRefreshCoordinator::new(),
                oauth_attempts: AsyncMutex::new(HashMap::new()),
                store,
            }),
        };
        svc.sync_from_store().await?;
        Ok(svc)
    }

    /// Current configuration snapshot.
    #[must_use]
    pub fn config(&self) -> AntigravityConfig {
        self.inner
            .cfg
            .read()
            .map_or_else(|_| AntigravityConfig::default(), |g| g.clone())
    }

    /// Configured upstream endpoints.
    #[must_use]
    pub fn endpoints(&self) -> &AntigravityEndpoints {
        &self.inner.endpoints
    }

    /// Runtime Hub version tracker.
    #[must_use]
    pub fn version(&self) -> &VersionTracker {
        &self.inner.version
    }

    /// Shared `reqwest::Client`.
    #[must_use]
    pub fn http_client(&self) -> &reqwest::Client {
        &self.inner.http
    }

    /// Shared thought signature cache handle.
    #[must_use]
    pub fn signature_cache(&self) -> Arc<StdMutex<SignatureCache>> {
        self.inner.signature_cache.clone()
    }

    /// Count of total configured accounts in the pool.
    #[must_use]
    pub fn account_count(&self) -> usize {
        self.inner.accounts.lock().map_or(0, |mgr| mgr.len())
    }

    /// Count of enabled, non-revoked, non-verification-blocked accounts in the pool.
    #[must_use]
    pub fn usable_account_count(&self) -> usize {
        self.inner
            .accounts
            .lock()
            .map_or(0, |mgr| mgr.usable_account_count())
    }

    /// True if at least one enabled, non-revoked, non-verification-blocked account exists.
    #[must_use]
    pub fn has_usable_accounts(&self) -> bool {
        self.usable_account_count() > 0
    }

    /// Overlay built-in + live discovered Antigravity models onto an `oc_llm::Catalog`.
    pub fn overlay_catalog(&self, catalog: &mut oc_llm::Catalog) {
        let cache = self.inner.discovered_models.lock().ok().map(|g| g.clone());
        overlay_catalog_models(catalog, cache.as_ref());
    }

    /// Return merged built-in + discovered Antigravity [`oc_llm::ModelInfo`] rows for `"google"`.
    #[must_use]
    pub fn antigravity_model_infos(&self) -> Vec<oc_llm::ModelInfo> {
        let cache = self.inner.discovered_models.lock().ok().map(|g| g.clone());
        crate::models::merged_antigravity_models(cache.as_ref())
            .into_values()
            .collect()
    }

    /// Resolve `provider/id[#variant]` against merged built-in + live-discovered Antigravity models.
    #[must_use]
    pub fn resolve_catalog_model(
        &self,
        base_catalog: &oc_llm::Catalog,
        reference: &str,
    ) -> Option<oc_llm::ResolvedModel> {
        let cache = self.inner.discovered_models.lock().ok().map(|g| g.clone());
        resolve_antigravity_catalog_model(base_catalog, cache.as_ref(), reference)
    }

    /// Return currently cached discovered models from `/v1internal:fetchAvailableModels`.
    #[must_use]
    pub fn cached_discovered_models(&self) -> DiscoveredModelCache {
        self.inner
            .discovered_models
            .lock()
            .map_or_else(|_| DiscoveredModelCache::default(), |g| g.clone())
    }

    /// Persist dirty thought signatures when `force` is true or the write interval has elapsed.
    pub(crate) fn flush_signature_cache(&self, force: bool) {
        flush_signature_cache_handle(&self.inner.signature_cache, force);
    }

    /// Fetch the Hub auto-updater manifest and refresh quotas/models in background at startup.
    pub fn spawn_background_version_check(&self) {
        let check_manifest = self.config().auto_update;
        let refresh_models = self.has_usable_accounts();
        if !check_manifest && !refresh_models {
            return;
        }
        let svc = self.clone();
        tokio::spawn(async move {
            if check_manifest {
                let _ = svc
                    .inner
                    .version
                    .refresh_from_manifest(&svc.inner.http, &svc.inner.endpoints.manifest_url)
                    .await;
            }
            if refresh_models {
                let _ = svc.refresh_quotas_and_models().await;
            }
        });
    }

    /// Spawn the periodic maintenance loop honoring `proactive_token_refresh*` and
    /// `quota_refresh_interval_minutes` (0 disables the quota arm).
    ///
    /// The loop is a no-op — no task is spawned at all — when both knobs are disabled.
    /// Each tick:
    /// 1. refreshes any access token expiring within `proactive_refresh_buffer_seconds`,
    /// 2. refreshes live quota + model discovery every `quota_refresh_interval_minutes`.
    ///
    /// Returns `true` when a maintenance task was started, `false` when it is disabled.
    pub fn spawn_background_maintenance(&self) -> bool {
        let cfg = self.config();
        let proactive_on = cfg.proactive_token_refresh;
        let quota_interval = cfg.quota_refresh_interval_minutes;
        let buffer_ms =
            i64::try_from(cfg.proactive_refresh_buffer_seconds.saturating_mul(1000)).unwrap_or(0);
        if !proactive_on && quota_interval == 0 {
            return false;
        }
        let tick_secs = if proactive_on {
            cfg.proactive_refresh_interval_seconds
        } else {
            quota_interval.saturating_mul(60)
        }
        .max(1);
        let quota_tick_secs = quota_interval.saturating_mul(60);

        let svc = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(tick_secs));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut since_quota = 0_u64;
            loop {
                ticker.tick().await;
                if proactive_on && let Err(err) = svc.proactive_refresh_tokens(buffer_ms).await {
                    tracing::debug!(error = %err, "antigravity: proactive token refresh sweep failed");
                }
                since_quota = since_quota.saturating_add(tick_secs);
                if quota_tick_secs > 0 && since_quota >= quota_tick_secs {
                    since_quota = 0;
                    if let Err(err) = svc.refresh_quotas_and_models().await {
                        tracing::debug!(error = %err, "antigravity: background quota refresh failed");
                    }
                }
                svc.flush_signature_cache(false);
            }
        });
        true
    }
}

impl Drop for ServiceInner {
    fn drop(&mut self) {
        flush_signature_cache_handle(&self.signature_cache, true);
    }
}

pub(crate) fn flush_signature_cache_handle(cache: &Arc<StdMutex<SignatureCache>>, force: bool) {
    if let Ok(mut guard) = cache.lock()
        && let Err(err) = guard.flush_if_due(now_ms(), force)
    {
        tracing::debug!(error = %err, "antigravity: failed to flush signature cache");
    }
}
