//! Access-token acquisition: cache lookup, refresh + durable persist, project resolution.

use super::sync::lock_accounts;
use super::types::{ActiveCredentials, CachedAccessToken};
use super::{AntigravityService, now_ms};
use crate::accounts::{ModelFamily, SelectedAccount};
use crate::auth::{
    OAuthAuthDetails, RefreshParts, access_token_expired, format_refresh_parts, parse_refresh_parts,
};
use crate::constants::{ANTIGRAVITY_METHOD_ID, HeaderStyle};
use crate::error::{AntigravityError, Result};
use crate::project::ensure_project_context;
use crate::storage::{AccountMetadataV4, account_token_hash};
use oc_store::CredentialPatch;
use serde_json::{Value, json};
use std::time::Duration;

impl AntigravityService {
    /// Ensure an account at `index` has a valid access token and effective project ID.
    pub async fn credentials_for_account(
        &self,
        index: usize,
        force_refresh: bool,
    ) -> Result<ActiveCredentials> {
        let now = now_ms();
        let (acct, packed_refresh) = {
            let mgr = lock_accounts(&self.inner.accounts)?;
            let a = mgr.accounts().get(index).cloned().ok_or_else(|| {
                AntigravityError::NoAccounts(format!("account index {index} not found"))
            })?;
            let packed = format_refresh_parts(&RefreshParts {
                refresh_token: a.refresh_token.clone(),
                project_id: a.project_id.clone(),
                managed_project_id: a.managed_project_id.clone(),
            });
            (a, packed)
        };

        let cached = if force_refresh {
            None
        } else {
            self.inner
                .token_cache
                .lock()
                .ok()
                .and_then(|tc| tc.get(&acct.token_hash).cloned())
        };

        let auth_details = OAuthAuthDetails {
            refresh: packed_refresh.clone(),
            access: cached.as_ref().map(|c| c.access.clone()),
            expires: cached.as_ref().map(|c| c.expires),
        };

        let access_token = if access_token_expired(&auth_details, now) {
            self.refresh_and_persist_token(index, &acct, &packed_refresh, &auth_details, now)
                .await?
        } else {
            cached.map(|c| c.access).unwrap_or_default()
        };

        let project_id = self
            .resolve_project_for_account(&acct, &access_token)
            .await?;

        Ok(ActiveCredentials {
            account_index: index,
            account_hash: acct.token_hash.clone(),
            access_token,
            project_id,
            account: acct,
        })
    }

    async fn refresh_and_persist_token(
        &self,
        index: usize,
        acct: &AccountMetadataV4,
        packed_refresh: &str,
        auth_details: &OAuthAuthDetails,
        now: i64,
    ) -> Result<String> {
        let refreshed = match self
            .inner
            .refresh_coord
            .refresh_access_token(
                &self.inner.http,
                &self.inner.endpoints.oauth_token_url,
                auth_details,
                now,
            )
            .await
        {
            Ok(r) => r,
            Err(err) => {
                if matches!(err, AntigravityError::TokenRefresh { revoked: true, .. })
                    && let Ok(mut mgr) = lock_accounts(&self.inner.accounts)
                {
                    let _ = mgr.mark_revoked(index);
                }
                tracing::warn!(
                    account_index = index,
                    revoked = matches!(err, AntigravityError::TokenRefresh { revoked: true, .. }),
                    error = %err,
                    "antigravity: access token refresh failed"
                );
                return Err(err);
            }
        };

        if let Ok(mut tc) = self.inner.token_cache.lock() {
            tc.insert(
                account_token_hash(&refreshed.refresh_token),
                CachedAccessToken {
                    access: refreshed.access.clone(),
                    expires: refreshed.expires,
                },
            );
        }

        let (target_store_id, refresh_val) = if let Some(ref rotated) = refreshed.rotated_refresh {
            let new_parts = parse_refresh_parts(rotated);
            let sid = lock_accounts(&self.inner.accounts)?
                .apply_token_rotation(index, new_parts.refresh_token)?;
            (sid, rotated.clone())
        } else {
            (acct.store_credential_id.clone(), packed_refresh.to_owned())
        };

        if let (Some(store), Some(cid)) = (&self.inner.store, target_store_id) {
            let value = json!({
                "type": "oauth",
                "methodID": ANTIGRAVITY_METHOD_ID,
                "refresh": refresh_val,
                "access": refreshed.access,
                "expires": refreshed.expires,
                "metadata": self
                    .preserved_credential_metadata(&cid, acct.email.as_deref())
                    .await,
            });
            // The refresh token grant already rotated upstream state, so the durable write
            // must land: retry once, then surface the failure instead of losing it.
            let mut last_err = None;
            for attempt in 0..2 {
                match store
                    .update_credential(
                        &cid,
                        CredentialPatch {
                            label: None,
                            value: Some(value.clone()),
                            active: None,
                        },
                    )
                    .await
                {
                    Ok(Some(_)) => {
                        last_err = None;
                        break;
                    }
                    Ok(None) => {
                        last_err = Some(AntigravityError::Storage(format!(
                            "credential `{cid}` vanished while persisting refreshed token"
                        )));
                    }
                    Err(err) => last_err = Some(AntigravityError::Store(err)),
                }
                if attempt == 0 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            if let Some(err) = last_err {
                tracing::error!(
                    credential = %cid,
                    error = %err,
                    "antigravity: failed to persist refreshed OAuth token; credential row is stale"
                );
                return Err(err);
            }
        }

        Ok(refreshed.access)
    }

    /// Load the `metadata` object already stored on `credential_id` so a token refresh
    /// keeps `email` / `projectId` / `managedProjectId` instead of replacing the value.
    async fn preserved_credential_metadata(
        &self,
        credential_id: &str,
        store_email: Option<&str>,
    ) -> Value {
        let stored = match &self.inner.store {
            Some(store) => store
                .get_credential(credential_id)
                .await
                .ok()
                .flatten()
                .and_then(|row| row.pointer("/value/metadata").cloned()),
            None => None,
        };
        let mut meta = match stored {
            Some(Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        // Backfill email from the pool when the row predates metadata sync.
        let has_email = meta
            .get("email")
            .and_then(Value::as_str)
            .is_some_and(|e| !e.trim().is_empty());
        if !has_email && let Some(email) = store_email {
            meta.insert("email".to_owned(), json!(email));
        }
        Value::Object(meta)
    }

    async fn resolve_project_for_account(
        &self,
        acct: &AccountMetadataV4,
        access_token: &str,
    ) -> Result<String> {
        if let Some(ref explicit) = acct.project_id {
            return Ok(explicit.clone());
        }
        if let Some(ref managed) = acct.managed_project_id {
            return Ok(managed.clone());
        }
        let ctx = ensure_project_context(
            &self.inner.http,
            &self.inner.endpoints,
            &self.inner.version,
            access_token,
            None,
            true,
        )
        .await?;
        // `loadCodeAssist` also reports the account's subscription tier; persist both so
        // `list_accounts_status()[i].tier` and later quota refreshes can use them.
        if let Ok(mut mgr) = lock_accounts(&self.inner.accounts)
            && let Some(index) = mgr
                .accounts()
                .iter()
                .position(|a| a.token_hash == acct.token_hash)
        {
            let _ = mgr.cache_project_context(
                index,
                ctx.managed_project_id.clone(),
                ctx.current_tier_id.clone(),
            );
        }
        Ok(ctx.effective_project_id)
    }

    /// Acquire credentials for any usable account (used by control-plane calls such as `google_search`).
    pub async fn acquire_any_token_for_control_plane(&self) -> Result<ActiveCredentials> {
        let now = now_ms();
        let selected: Option<SelectedAccount> = {
            let mut mgr = lock_accounts(&self.inner.accounts)?;
            mgr.select_for_request(
                ModelFamily::Gemini,
                HeaderStyle::Antigravity,
                "gemini-2.5-flash",
                now,
                &[],
            )
        };
        if let Some(sel) = selected {
            return self.credentials_for_account(sel.index, false).await;
        }
        // Fallback: try the first non-disabled, non-revoked, non-verification-locked account
        // if all accounts are temporarily throttled. Never bypass disabled/revoked/ToS locks.
        let fallback_idx = {
            let mgr = lock_accounts(&self.inner.accounts)?;
            mgr.accounts()
                .iter()
                .position(|a| a.enabled && !a.revoked && !a.verification_required)
        };
        if let Some(idx) = fallback_idx {
            return self.credentials_for_account(idx, false).await;
        }
        Err(AntigravityError::NoAccounts(
            "no Google Antigravity account authenticated; run `opencode auth login`".to_owned(),
        ))
    }

    /// Refresh every enabled account whose access token expires within `buffer_ms`.
    ///
    /// Failures are collected per account so one revoked or unreachable account cannot
    /// stop the sweep; the first error is returned only when every attempt failed.
    pub async fn proactive_refresh_tokens(&self, buffer_ms: i64) -> Result<usize> {
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

        let mut refreshed = 0;
        let mut first_err: Option<AntigravityError> = None;
        for idx in indices {
            let needs_refresh = {
                let mgr = match lock_accounts(&self.inner.accounts) {
                    Ok(m) => m,
                    Err(err) => {
                        first_err.get_or_insert(err);
                        continue;
                    }
                };
                let Some(acct) = mgr.accounts().get(idx) else {
                    continue;
                };
                let cached = self
                    .inner
                    .token_cache
                    .lock()
                    .ok()
                    .and_then(|tc| tc.get(&acct.token_hash).cloned());
                match cached {
                    Some(token) => token.expires <= now + buffer_ms,
                    None => true,
                }
            };
            if !needs_refresh {
                continue;
            }
            match self.credentials_for_account(idx, true).await {
                Ok(_) => refreshed += 1,
                Err(err) => {
                    first_err.get_or_insert(err);
                }
            }
        }

        if refreshed == 0
            && let Some(err) = first_err
        {
            return Err(err);
        }
        Ok(refreshed)
    }
}
