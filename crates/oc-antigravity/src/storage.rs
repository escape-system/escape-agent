//! Account pool persistence (`antigravity-accounts.json` V4 with V1–V3 migration),
//! token hashing, atomic writes, restricted permissions, and `.gitignore` updates.

use crate::config::resolve_config_dir;
use crate::error::{AntigravityError, Result};
use crate::fingerprint::{Fingerprint, FingerprintVersion};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Latest account storage schema version.
pub const CURRENT_STORAGE_VERSION: u32 = 4;

/// Cached per-model-group quota summary stored alongside an account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QuotaGroupSnapshot {
    /// Fraction used `0.0..=1.0` or percentage `0.0..=100.0` (normalized helper converts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_fraction: Option<f64>,
    /// Percentage used (`0..=100`), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    /// Reset timestamp in epoch milliseconds, if provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_time_ms: Option<i64>,
    /// Human-readable display label derived from the live API models sharing this quota pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Canonical base model IDs covered by this quota pool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

/// Cached quota snapshot across logical groups (`claude`, `gemini-pro`, `gemini-flash`, `gemini-cli`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CachedAccountQuota {
    /// Timestamp when this quota snapshot was recorded (epoch ms).
    #[serde(default)]
    pub updated_at: i64,
    /// Per-group quota map (`claude`, `gemini-pro`, `gemini-flash`, `gemini-cli`).
    #[serde(default)]
    pub groups: BTreeMap<String, QuotaGroupSnapshot>,
}

/// Persistent metadata for a single Google Antigravity account in V4 format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountMetadataV4 {
    /// Account email (also used for cross-source deduplication).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Optional user-friendly account label (synced with `oc-store`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Google OAuth 2.0 refresh token.
    pub refresh_token: String,
    /// Explicit user project ID (`cloudaicompanionProject` override).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Managed project ID provisioned via Cloud Code Assist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_project_id: Option<String>,
    /// Timestamp when the account was added (epoch ms).
    #[serde(default)]
    pub added_at: i64,
    /// Timestamp when the account was last used (epoch ms).
    #[serde(default)]
    pub last_used: i64,
    /// User-enabled toggle (defaults to `true`).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Per-quota-key rate-limit reset timestamps (`<family>[:<model>]` -> epoch ms).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub rate_limit_reset_times: HashMap<String, i64>,
    /// Account subscription tier (e.g. `"free"`, `"pro"`, `"ultra"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tier: Option<String>,
    /// Soft quota & status cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_quota: Option<CachedAccountQuota>,
    /// Account flagged for Google verification (`403 validation_required`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub verification_required: bool,
    /// Timestamp when `verification_required` was recorded (epoch ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_required_at: Option<i64>,
    /// Reason or error message accompanying verification flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_required_reason: Option<String>,
    /// Verification URL returned by Google, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_url: Option<String>,
    /// Flag indicating OAuth refresh token was permanently revoked (`invalid_grant`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub revoked: bool,
    /// Current device fingerprint assigned to this account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<Fingerprint>,
    /// Previous device fingerprints archived prior to regeneration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fingerprint_history: Vec<FingerprintVersion>,
    /// Stable short SHA-256 hash ID derived from [`AccountMetadataV4::refresh_token`].
    #[serde(default)]
    pub token_hash: String,
    /// Linked `oc-store` credential ID if synced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_credential_id: Option<String>,
}

const fn default_true() -> bool {
    true
}

impl AccountMetadataV4 {
    /// Ensure [`Self::token_hash`] is populated from [`Self::refresh_token`].
    pub fn ensure_token_hash(&mut self) {
        if self.token_hash.is_empty() && !self.refresh_token.is_empty() {
            self.token_hash = account_token_hash(&self.refresh_token);
        }
    }
}

/// Top-level `antigravity-accounts.json` V4 persistence format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStorageV4 {
    /// Storage schema version (always [`CURRENT_STORAGE_VERSION`] on save).
    pub version: u32,
    /// Pool of configured Google accounts.
    pub accounts: Vec<AccountMetadataV4>,
    /// Fallback active account index across families.
    #[serde(default)]
    pub active_index: usize,
    /// Per-family sticky active indices (`"claude"`, `"gemini"`).
    #[serde(default)]
    pub active_index_by_family: HashMap<String, usize>,
}

impl Default for AccountStorageV4 {
    fn default() -> Self {
        Self {
            version: CURRENT_STORAGE_VERSION,
            accounts: Vec::new(),
            active_index: 0,
            active_index_by_family: HashMap::new(),
        }
    }
}

/// Compute a stable 16-hex-character SHA-256 hash of a raw refresh token.
#[must_use]
pub fn account_token_hash(refresh_token: &str) -> String {
    let digest = Sha256::digest(refresh_token.as_bytes());
    hex::encode(&digest[..8])
}

/// Default location of `antigravity-accounts.json`.
#[must_use]
pub fn default_accounts_path(config_dir: Option<&Path>) -> PathBuf {
    config_dir
        .map_or_else(resolve_config_dir, PathBuf::from)
        .join("antigravity-accounts.json")
}

/// Load `antigravity-accounts.json` from `path`, transparently migrating V1/V2/V3 schemas to V4.
pub fn load_accounts(path: &Path) -> Result<AccountStorageV4> {
    if !path.exists() {
        return Ok(AccountStorageV4::default());
    }
    let raw = std::fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(AccountStorageV4::default());
    }
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|err| {
        AntigravityError::Storage(format!(
            "corrupted account storage at {}: {err}",
            path.display()
        ))
    })?;
    migrate_to_v4(value)
}

/// Atomically persist `storage` to `path` with owner-only permissions (`0600` on Unix)
/// and ensure parent `.gitignore` ignores `antigravity-accounts.json`.
pub fn save_accounts(path: &Path, storage: &AccountStorageV4) -> Result<()> {
    let mut normalized = storage.clone();
    normalized.version = CURRENT_STORAGE_VERSION;
    for acct in &mut normalized.accounts {
        acct.ensure_token_hash();
    }
    deduplicate_accounts(&mut normalized);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        ensure_gitignore(parent, path.file_name().and_then(|n| n.to_str()))?;
    }

    let tmp_path = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(&normalized)?;
    std::fs::write(&tmp_path, json)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))?;
    }

    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Migrate V1, V2, V3, or V4 JSON payloads into [`AccountStorageV4`].
pub fn migrate_to_v4(value: serde_json::Value) -> Result<AccountStorageV4> {
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(1);

    if version >= CURRENT_STORAGE_VERSION {
        let mut storage: AccountStorageV4 = serde_json::from_value(value).map_err(|err| {
            AntigravityError::Storage(format!("invalid v4 account storage: {err}"))
        })?;
        for acct in &mut storage.accounts {
            acct.ensure_token_hash();
        }
        deduplicate_accounts(&mut storage);
        return Ok(storage);
    }

    let accounts_raw = value
        .get("accounts")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut accounts = Vec::with_capacity(accounts_raw.len());
    for item in accounts_raw {
        if let Some(acct) = parse_legacy_account(&item) {
            accounts.push(acct);
        }
    }

    let active_index = value
        .get("activeIndex")
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0);
    let active_index_by_family = value
        .get("activeIndexByFamily")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let mut migrated = AccountStorageV4 {
        version: CURRENT_STORAGE_VERSION,
        accounts,
        active_index,
        active_index_by_family,
    };
    deduplicate_accounts(&mut migrated);
    Ok(migrated)
}

fn opt_trimmed_str(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(serde_json::Value::as_str)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn parse_legacy_rate_limits(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> HashMap<String, i64> {
    let mut out = HashMap::new();
    if let Some(rl) = obj.get("rateLimitResetTimes").and_then(|v| v.as_object()) {
        for (k, v) in rl {
            if let Some(ts) = v.as_i64() {
                out.insert(k.clone(), ts);
            }
        }
    }
    if obj
        .get("isRateLimited")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && let Some(ts) = obj
            .get("rateLimitResetTime")
            .and_then(serde_json::Value::as_i64)
    {
        out.insert("claude".to_owned(), ts);
        out.insert("gemini-antigravity".to_owned(), ts);
    }
    out
}

fn parse_legacy_account(item: &serde_json::Value) -> Option<AccountMetadataV4> {
    let obj = item.as_object()?;
    let refresh_token =
        opt_trimmed_str(obj, "refreshToken").or_else(|| opt_trimmed_str(obj, "refresh_token"))?;
    let mut acct = AccountMetadataV4 {
        email: opt_trimmed_str(obj, "email"),
        label: opt_trimmed_str(obj, "label"),
        refresh_token: refresh_token.clone(),
        project_id: obj
            .get("projectId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        managed_project_id: obj
            .get("managedProjectId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        added_at: obj
            .get("addedAt")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        last_used: obj
            .get("lastUsed")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
        enabled: obj
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        rate_limit_reset_times: parse_legacy_rate_limits(obj),
        cached_tier: obj
            .get("cachedTier")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        cached_quota: obj
            .get("cachedQuota")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        verification_required: obj
            .get("verificationRequired")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        verification_required_at: obj
            .get("verificationRequiredAt")
            .and_then(serde_json::Value::as_i64),
        verification_required_reason: obj
            .get("verificationRequiredReason")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        verification_url: obj
            .get("verificationUrl")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        revoked: obj
            .get("revoked")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        fingerprint: obj
            .get("fingerprint")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        fingerprint_history: obj
            .get("fingerprintHistory")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default(),
        token_hash: account_token_hash(&refresh_token),
        store_credential_id: obj
            .get("storeCredentialId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    };
    acct.ensure_token_hash();
    Some(acct)
}

fn deduplicate_accounts(storage: &mut AccountStorageV4) {
    let mut by_key: HashMap<String, usize> = HashMap::new();
    let mut deduped: Vec<AccountMetadataV4> = Vec::with_capacity(storage.accounts.len());

    for mut acct in storage.accounts.drain(..) {
        acct.ensure_token_hash();
        let key = acct
            .email
            .as_ref()
            .filter(|e| !e.trim().is_empty())
            .map_or_else(
                || format!("tok:{}", acct.refresh_token),
                |e| format!("email:{}", e.trim().to_ascii_lowercase()),
            );

        if let Some(&idx) = by_key.get(&key) {
            if acct.last_used >= deduped[idx].last_used {
                // Keep store link & added_at if newer entry lacks them.
                if acct.store_credential_id.is_none() {
                    acct.store_credential_id
                        .clone_from(&deduped[idx].store_credential_id);
                }
                if acct.added_at == 0 {
                    acct.added_at = deduped[idx].added_at;
                }
                deduped[idx] = acct;
            }
        } else {
            let idx = deduped.len();
            by_key.insert(key, idx);
            deduped.push(acct);
        }
    }

    storage.accounts = deduped;
    if storage.accounts.is_empty() {
        storage.active_index = 0;
        storage.active_index_by_family.clear();
    } else {
        storage.active_index = storage.active_index.min(storage.accounts.len() - 1);
        for idx in storage.active_index_by_family.values_mut() {
            *idx = (*idx).min(storage.accounts.len() - 1);
        }
    }
}

fn ensure_gitignore(dir: &Path, filename: Option<&str>) -> Result<()> {
    let Some(name) = filename else {
        return Ok(());
    };
    let gitignore_path = dir.join(".gitignore");
    let existing = if gitignore_path.is_file() {
        std::fs::read_to_string(&gitignore_path)?
    } else {
        String::new()
    };
    if existing.lines().any(|line| line.trim() == name) {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(name);
    updated.push('\n');
    std::fs::write(gitignore_path, updated)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn migrates_v1_and_deduplicates_by_email() {
        let v1 = serde_json::json!({
            "version": 1,
            "activeIndex": 0,
            "accounts": [
                {
                    "email": "Alice@example.com",
                    "refreshToken": "1//old",
                    "lastUsed": 100,
                    "isRateLimited": true,
                    "rateLimitResetTime": 9999
                },
                {
                    "email": "alice@example.com",
                    "refreshToken": "1//new",
                    "lastUsed": 200
                }
            ]
        });
        let migrated = migrate_to_v4(v1).expect("migrate");
        assert_eq!(migrated.version, 4);
        assert_eq!(migrated.accounts.len(), 1);
        assert_eq!(migrated.accounts[0].refresh_token, "1//new");
        assert!(!migrated.accounts[0].token_hash.is_empty());
    }

    #[test]
    fn save_and_load_creates_gitignore_and_atomic_json() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("antigravity-accounts.json");
        let storage = AccountStorageV4 {
            version: 4,
            accounts: vec![AccountMetadataV4 {
                email: Some("bob@example.com".to_owned()),
                label: Some("work".to_owned()),
                refresh_token: "1//bob".to_owned(),
                project_id: Some("p1".to_owned()),
                managed_project_id: None,
                added_at: 1000,
                last_used: 2000,
                enabled: true,
                rate_limit_reset_times: HashMap::new(),
                cached_tier: None,
                cached_quota: None,
                verification_required: false,
                verification_required_at: None,
                verification_required_reason: None,
                verification_url: None,
                revoked: false,
                fingerprint: None,
                fingerprint_history: Vec::new(),
                token_hash: String::new(),
                store_credential_id: None,
            }],
            active_index: 0,
            active_index_by_family: HashMap::new(),
        };

        save_accounts(&path, &storage).expect("save");
        let loaded = load_accounts(&path).expect("load");
        assert_eq!(loaded.accounts.len(), 1);
        assert_eq!(loaded.accounts[0].token_hash, account_token_hash("1//bob"));
        let gitignore = std::fs::read_to_string(dir.path().join(".gitignore")).expect("gitignore");
        assert!(gitignore.contains("antigravity-accounts.json"));
    }
}
