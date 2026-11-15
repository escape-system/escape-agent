//! `Google Antigravity` quota projection: the account refresh and the window
//! summary built from it.

use oc_antigravity::AccountStatusSummary;
use oc_llm::{ProviderRateLimit, QuotaCapability};

use crate::AppState;

use super::types::{ProviderLocalUsage, ProviderQuotaEntry, QuotaWindowEntry};

pub(super) struct AntigravityFetchData {
    pub(super) accounts: Vec<AccountStatusSummary>,
    pub(super) warnings: Vec<String>,
    pub(super) updated_at_ms: Option<i64>,
    pub(super) error: Option<String>,
}

pub(super) async fn refresh_antigravity_if_needed(
    state: &AppState,
    force_refresh: bool,
) -> Option<AntigravityFetchData> {
    let svc = state.antigravity()?;
    // Only usable accounts are worth an upstream refresh: a pool holding a
    // single disabled/revoked account must not be probed at all.
    if force_refresh && svc.has_usable_accounts() {
        return match svc.refresh_quotas_and_models().await {
            Ok(report) => Some(AntigravityFetchData {
                accounts: report.accounts,
                warnings: report.warnings,
                updated_at_ms: Some(report.updated_at),
                error: None,
            }),
            Err(err) => {
                let fallback_accounts = svc.list_accounts_status().unwrap_or_default();
                Some(AntigravityFetchData {
                    accounts: fallback_accounts,
                    warnings: Vec::new(),
                    updated_at_ms: None,
                    error: Some(err.to_string()),
                })
            }
        };
    }

    let accounts = svc.list_accounts_status().unwrap_or_default();
    let newest = accounts
        .iter()
        .filter_map(|acc| acc.quota.as_ref().map(|q| q.updated_at))
        .max();
    Some(AntigravityFetchData {
        accounts,
        warnings: Vec::new(),
        updated_at_ms: newest,
        error: None,
    })
}

pub(super) fn build_antigravity_entry(
    provider_id: String,
    name: String,
    connected: bool,
    ag_refresh: Option<&AntigravityFetchData>,
    rate_limit: Option<ProviderRateLimit>,
    local_usage: Option<ProviderLocalUsage>,
) -> ProviderQuotaEntry {
    let capability = QuotaCapability::LiveApi;
    let capability_note = Some(
        "Fetches live Cloud Code Assist pool quotas (Claude, Gemini Pro, Gemini Flash) per connected Google account."
            .to_owned(),
    );

    let Some(ag) = ag_refresh else {
        return ProviderQuotaEntry {
            provider_id,
            name,
            connected: false,
            connection_source: "none".to_owned(),
            capability,
            capability_note,
            status: "unavailable".to_owned(),
            fetched_at_ms: None,
            cached: None,
            has_management_key: None,
            management_key_source: None,
            key_label: None,
            rate_limit_rule: None,
            windows: Vec::new(),
            balances: Vec::new(),
            spend_summary: None,
            rate_limit,
            accounts: Vec::new(),
            keys: Vec::new(),
            local_usage,
            warnings: Vec::new(),
            error: None,
        };
    };

    let usable = ag
        .accounts
        .iter()
        .filter(|acc| acc.enabled && !acc.revoked && !acc.verification_required)
        .count();
    let windows = antigravity_summary_windows(&ag.accounts);
    let status = if ag.error.is_some() {
        "error"
    } else if usable == 0 {
        "not_connected"
    } else if !ag.warnings.is_empty() {
        "partial"
    } else {
        "ok"
    }
    .to_owned();

    ProviderQuotaEntry {
        provider_id,
        name,
        // A pool of disabled/revoked/verification-blocked accounts is not a
        // connection, even though `accounts` is non-empty.
        connected: connected && usable > 0,
        connection_source: if usable == 0 {
            "none".to_owned()
        } else {
            "oauth".to_owned()
        },
        capability,
        capability_note,
        status,
        fetched_at_ms: ag.updated_at_ms,
        cached: None,
        has_management_key: None,
        management_key_source: None,
        key_label: ag
            .accounts
            .iter()
            .filter(|a| a.enabled && !a.revoked && !a.verification_required)
            .find(|a| a.active_claude || a.active_gemini)
            .and_then(|a| a.email.clone()),
        rate_limit_rule: None,
        windows,
        balances: Vec::new(),
        spend_summary: None,
        rate_limit,
        accounts: ag.accounts.clone(),
        keys: Vec::new(),
        local_usage,
        warnings: ag.warnings.clone(),
        error: ag.error.clone(),
    }
}

/// Percent windows for the pool's primary **usable** account.
///
/// Only enabled, non-revoked, non-verification-blocked accounts can produce
/// windows; a pool whose every account is unusable yields none rather than
/// reporting the quota of an account the user cannot use.
fn antigravity_summary_windows(accounts: &[AccountStatusSummary]) -> Vec<QuotaWindowEntry> {
    let primary = accounts
        .iter()
        .find(|acc| {
            acc.enabled
                && !acc.revoked
                && !acc.verification_required
                && (acc.active_claude || acc.active_gemini)
        })
        .or_else(|| {
            accounts
                .iter()
                .find(|acc| acc.enabled && !acc.revoked && !acc.verification_required)
        });
    let Some(account) = primary else {
        return Vec::new();
    };
    let Some(quota) = &account.quota else {
        return Vec::new();
    };

    let order = ["claude", "gemini-pro", "gemini-flash", "gemini-cli"];
    let mut keys: Vec<String> = quota.groups.keys().cloned().collect();
    keys.sort_by_key(|k| {
        order
            .iter()
            .position(|known| known == k)
            .unwrap_or(order.len())
    });

    let mut windows = Vec::with_capacity(keys.len());
    for group in keys {
        let Some(snap) = quota.groups.get(&group) else {
            continue;
        };
        let remaining_pct = snap.remaining_fraction.map(|frac| {
            let pct = if frac <= 1.0 { frac * 100.0 } else { frac };
            pct.clamp(0.0, 100.0)
        });
        let used_pct = snap
            .used_percent
            .map(|pct| pct.clamp(0.0, 100.0))
            .or_else(|| remaining_pct.map(|rem| (100.0 - rem).clamp(0.0, 100.0)));
        let remaining_pct =
            remaining_pct.or_else(|| used_pct.map(|used| (100.0 - used).clamp(0.0, 100.0)));

        windows.push(QuotaWindowEntry {
            id: group.clone(),
            label: antigravity_group_label(&group),
            group_key: Some(antigravity_group_key(&group)),
            unit: "percent".to_owned(),
            limit: Some(100.0),
            used: used_pct,
            remaining: remaining_pct,
            used_percent: used_pct,
            remaining_percent: remaining_pct,
            limited: remaining_pct.map(|rem| rem <= 0.0),
            reset_at_ms: snap.reset_time_ms,
            reset_label: None,
        });
    }
    windows
}

/// Stable localization token for a quota pool group.
fn antigravity_group_key(group: &str) -> String {
    match group {
        "claude" | "gemini-pro" | "gemini-flash" | "gemini-cli" => group.to_owned(),
        _ => "other".to_owned(),
    }
}

fn antigravity_group_label(group: &str) -> String {
    match group {
        "claude" => "Claude / GPT-OSS Pool".to_owned(),
        "gemini-pro" => "Gemini Pro Pool".to_owned(),
        "gemini-flash" => "Gemini Flash & Hydrogen Pool".to_owned(),
        "gemini-cli" => "Gemini CLI Pool".to_owned(),
        other => other.to_owned(),
    }
}
