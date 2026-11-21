//! Live quota inspection and upstream model discovery across enabled accounts.

use super::sync::lock_accounts;
use super::types::QuotaRefreshReport;
use super::{AntigravityService, now_ms};
use crate::error::{AntigravityError, Result};
use crate::models::save_discovered_models;
use crate::quota::{DiscoveredModelInfo, fetch_account_quota};
use std::collections::HashMap;

impl AntigravityService {
    /// Refresh live quota and upstream model discovery across enabled accounts.
    pub async fn refresh_quotas_and_models(&self) -> Result<QuotaRefreshReport> {
        let now = now_ms();
        let indices: Vec<usize> = {
            let mgr = lock_accounts(&self.inner.accounts)?;
            (0..mgr.len())
                .filter(|&i| {
                    let a = &mgr.accounts()[i];
                    a.enabled && !a.revoked && !a.verification_required
                })
                .collect()
        };

        if indices.is_empty() {
            return Err(AntigravityError::NoAccounts(
                "no enabled Antigravity accounts available to inspect".to_owned(),
            ));
        }

        let mut merged_discovered: HashMap<String, DiscoveredModelInfo> = HashMap::new();
        let mut warnings = Vec::new();

        for idx in indices {
            let creds = match self.credentials_for_account(idx, false).await {
                Ok(c) => c,
                Err(err) => {
                    warnings.push(format!("account #{idx}: {err}"));
                    continue;
                }
            };
            match fetch_account_quota(
                &self.inner.http,
                &self.inner.endpoints,
                &self.inner.version,
                &creds.access_token,
                Some(&creds.project_id),
                now,
            )
            .await
            {
                Ok(fetched) => {
                    if let Some(w) = fetched.warning {
                        warnings.push(format!(
                            "{}: {w}",
                            creds
                                .account
                                .email
                                .as_deref()
                                .unwrap_or(&creds.account_hash)
                        ));
                    }
                    for m in fetched.discovered_models {
                        merged_discovered.entry(m.id.clone()).or_insert(m);
                    }
                    if let Ok(mut mgr) = lock_accounts(&self.inner.accounts) {
                        // Tier comes from `loadCodeAssist` (cached by `resolve_project_for_account`);
                        // `fetchAccountQuota` does not report it.
                        let tier = mgr.accounts().get(idx).and_then(|a| a.cached_tier.clone());
                        let _ = mgr.update_quota_snapshot(idx, fetched.cached_quota, tier);
                    }
                }
                Err(err) => warnings.push(format!("account #{idx}: {err}")),
            }
        }

        let discovered_count = merged_discovered.len();
        if !merged_discovered.is_empty()
            && let Ok(mut cache) = self.inner.discovered_models.lock()
        {
            cache.updated_at = now;
            cache.models = merged_discovered.into_iter().collect();
            if let Some(ref p) = self.inner.models_cache_path {
                let _ = save_discovered_models(p, &cache);
            }
        }

        Ok(QuotaRefreshReport {
            updated_at: now,
            accounts: self.list_accounts_status()?,
            discovered_model_count: discovered_count,
            warnings,
        })
    }
}
