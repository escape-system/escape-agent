//! Cloud Code Assist quota & tier inspection (`/v1internal:fetchAvailableModels` &
//! `/v1internal:retrieveUserQuota`), plus model discovery metadata parsing.

use crate::accounts::canonical_quota_model_id;
use crate::constants::{AntigravityEndpoints, HeaderStyle, headers_for_style};
use crate::error::Result;
use crate::storage::{CachedAccountQuota, QuotaGroupSnapshot};
use crate::version::VersionTracker;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Model metadata discovered live from `/v1internal:fetchAvailableModels`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredModelInfo {
    /// Raw Cloud Code Assist model ID (e.g. `claude-sonnet-4-6`, `gemini-3.1-pro-high`).
    pub id: String,
    /// Optional upstream display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Context limit reported by upstream, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Maximum output tokens reported by upstream, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Whether upstream advertises reasoning/thinking support.
    #[serde(default)]
    pub supports_thinking: bool,
    /// Remaining quota fraction reported for this model (`0.0..=1.0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_fraction: Option<f64>,
    /// Quota reset timestamp in epoch ms, if reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_time_ms: Option<i64>,
}

/// Complete quota + model discovery result for one Antigravity account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AccountQuotaFetch {
    /// Cached quota summary grouped into `claude`, `gemini-pro`, `gemini-flash`, `gemini-cli`,
    /// plus `gemini-cli:<model_id>` groups for per-model Gemini CLI buckets.
    pub cached_quota: CachedAccountQuota,
    /// Individual models discovered from `/v1internal:fetchAvailableModels`.
    pub discovered_models: Vec<DiscoveredModelInfo>,
    /// Non-fatal fetch warning if one of the endpoints failed.
    pub warning: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FetchAvailableModelsResponse {
    #[serde(default)]
    models: BTreeMap<String, UpstreamModelEntry>,
    #[serde(default)]
    agent_model_sorts: Vec<UpstreamAgentModelSort>,
    #[serde(default)]
    grouped_models: BTreeMap<String, UpstreamGroupedModel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamAgentModelSort {
    #[serde(default)]
    groups: Vec<UpstreamAgentModelGroup>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamAgentModelGroup {
    #[serde(default)]
    model_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamGroupedModel {
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamModelEntry {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    supports_thinking: Option<bool>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    quota_info: Option<UpstreamQuotaInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamQuotaInfo {
    #[serde(default)]
    remaining_fraction: Option<f64>,
    #[serde(default)]
    reset_time: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RetrieveUserQuotaResponse {
    #[serde(default)]
    buckets: Vec<GeminiCliQuotaBucket>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCliQuotaBucket {
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    remaining_fraction: Option<f64>,
    #[serde(default)]
    reset_time: Option<String>,
}

/// Parse an RFC 3339 timestamp string (`YYYY-MM-DDTHH:MM:SSZ`, fractional seconds, or
/// `±HH:MM` offset) into epoch milliseconds without extra dependencies.
///
/// Fractional digits are consumed char-by-char so a non-ASCII code point can never split a
/// UTF-8 boundary, and clock/offset components are range-checked instead of overflowing.
#[must_use]
pub fn parse_rfc3339_ms(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    let (body, offset_ms) = if let Some(rest) = trimmed.strip_suffix('Z') {
        (rest, 0_i64)
    } else if let Some(plus) = trimmed.rfind('+')
        && plus > 10
    {
        let offset = parse_tz_offset_ms(&trimmed[plus + 1..], 1)?;
        (&trimmed[..plus], offset)
    } else if let Some(minus) = trimmed.rfind('-')
        && minus > 10
    {
        let offset = parse_tz_offset_ms(&trimmed[minus + 1..], -1)?;
        (&trimmed[..minus], offset)
    } else {
        return None;
    };

    let (date_part, time_part) = body.split_once('T').or_else(|| body.split_once(' '))?;
    let mut d_parts = date_part.split('-');
    let year: i64 = d_parts.next()?.parse().ok()?;
    let month: i64 = d_parts.next()?.parse().ok()?;
    let day: i64 = d_parts.next()?.parse().ok()?;
    if d_parts.next().is_some() {
        return None;
    }

    let (hms, frac_ms) = if let Some((left, frac)) = time_part.split_once('.') {
        let mut padded = String::with_capacity(3);
        for ch in frac.chars().take(3) {
            if ch.is_ascii_digit() {
                padded.push(ch);
            }
        }
        while padded.len() < 3 {
            padded.push('0');
        }
        (left, padded.parse::<i64>().unwrap_or(0))
    } else {
        (time_part, 0)
    };
    let mut t_parts = hms.split(':');
    let hour: i64 = t_parts.next()?.parse().ok()?;
    let minute: i64 = t_parts.next()?.parse().ok()?;
    let second: i64 = t_parts.next()?.parse().ok()?;
    if t_parts.next().is_some()
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }

    let days = days_from_civil(year, month, day)?;
    let utc_ms = days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1_000 + frac_ms
        - offset_ms;
    Some(utc_ms)
}

fn parse_tz_offset_ms(raw: &str, sign: i64) -> Option<i64> {
    let (h, m) = raw.split_once(':')?;
    let hours: i64 = h.parse().ok()?;
    let mins: i64 = m.parse().ok()?;
    if !(0..=23).contains(&hours) || !(0..=59).contains(&mins) {
        return None;
    }
    Some(sign * (hours * 3_600_000 + mins * 60_000))
}

fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// Fetch Antigravity Hub models & quota groups + optional Gemini CLI bucket quota.
pub async fn fetch_account_quota(
    client: &reqwest::Client,
    endpoints: &AntigravityEndpoints,
    version: &VersionTracker,
    access_token: &str,
    project_id: Option<&str>,
    now_ms: i64,
) -> Result<AccountQuotaFetch> {
    let hub_version = version.current();
    let mut groups: BTreeMap<String, QuotaGroupSnapshot> = BTreeMap::new();
    let mut discovered_models = Vec::new();
    let mut warnings = Vec::new();

    fetch_hub_quota(
        client,
        endpoints,
        &hub_version,
        access_token,
        &mut groups,
        &mut discovered_models,
        &mut warnings,
    )
    .await;

    if let Some(pid) = project_id.filter(|s| !s.trim().is_empty()) {
        fetch_gemini_cli_quota(
            client,
            endpoints,
            &hub_version,
            access_token,
            pid,
            &mut groups,
            &mut warnings,
        )
        .await;
    }

    Ok(AccountQuotaFetch {
        cached_quota: CachedAccountQuota {
            updated_at: now_ms,
            groups,
        },
        discovered_models,
        warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
    })
}

async fn fetch_hub_quota(
    client: &reqwest::Client,
    endpoints: &AntigravityEndpoints,
    hub_version: &str,
    access_token: &str,
    groups: &mut BTreeMap<String, QuotaGroupSnapshot>,
    discovered_models: &mut Vec<DiscoveredModelInfo>,
    warnings: &mut Vec<String>,
) {
    let hub_url = format!(
        "{}/v1internal:fetchAvailableModels",
        endpoints.antigravity_endpoint
    );
    let hub_headers = headers_for_style(HeaderStyle::Antigravity, hub_version);
    let mut hub_req = client
        .post(&hub_url)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({}));
    for (k, v) in hub_headers.to_pairs() {
        hub_req = hub_req.header(k, v);
    }

    match hub_req.send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(parsed) = resp.json::<FetchAvailableModelsResponse>().await {
                let mut ordered_agent_ids: Vec<String> = Vec::new();
                for sort in &parsed.agent_model_sorts {
                    for grp in &sort.groups {
                        for mid in &grp.model_ids {
                            if !ordered_agent_ids.contains(mid) {
                                ordered_agent_ids.push(mid.clone());
                            }
                        }
                    }
                }
                let has_agent_sorts = !ordered_agent_ids.is_empty();
                let mut candidate_ids: Vec<String> = if has_agent_sorts {
                    ordered_agent_ids
                } else {
                    parsed.models.keys().cloned().collect()
                };
                // Also include any tiered member of `groupedModels` whose group or sibling is present.
                if has_agent_sorts {
                    for k in parsed.models.keys() {
                        let base = canonical_quota_model_id(k);
                        if parsed.grouped_models.contains_key(&base) && !candidate_ids.contains(k) {
                            candidate_ids.push(k.clone());
                        }
                    }
                }

                for id in candidate_ids {
                    let Some(entry) = parsed.models.get(&id) else {
                        continue;
                    };
                    if entry.disabled.unwrap_or(false)
                        || is_skipped_quota_model(&id, entry.display_name.as_deref())
                    {
                        continue;
                    }
                    let rem = entry
                        .quota_info
                        .as_ref()
                        .and_then(|q| q.remaining_fraction)
                        .map(|v| v.clamp(0.0, 1.0));
                    let reset_ms = entry
                        .quota_info
                        .as_ref()
                        .and_then(|q| q.reset_time.as_deref())
                        .and_then(parse_rfc3339_ms);

                    if let Some(group) = classify_hub_quota_group(&id)
                        && let Some(r) = rem
                    {
                        merge_group_snapshot(groups, group, r, reset_ms);
                    }

                    let base_id = canonical_quota_model_id(&id);
                    let display_name = entry.display_name.clone().or_else(|| {
                        parsed
                            .grouped_models
                            .get(&base_id)
                            .and_then(|g| g.display_name.clone())
                    });

                    discovered_models.push(DiscoveredModelInfo {
                        id,
                        display_name,
                        max_tokens: entry.max_tokens,
                        max_output_tokens: entry.max_output_tokens,
                        supports_thinking: entry.supports_thinking.unwrap_or(false),
                        remaining_fraction: rem,
                        reset_time_ms: reset_ms,
                    });
                }
                populate_hub_quota_groups(groups, discovered_models);
            }
        }
        Ok(resp) => warnings.push(format!(
            "fetchAvailableModels returned HTTP {}",
            resp.status().as_u16()
        )),
        Err(err) => warnings.push(format!("fetchAvailableModels error: {err}")),
    }
}

async fn fetch_gemini_cli_quota(
    client: &reqwest::Client,
    endpoints: &AntigravityEndpoints,
    hub_version: &str,
    access_token: &str,
    project_id: &str,
    groups: &mut BTreeMap<String, QuotaGroupSnapshot>,
    warnings: &mut Vec<String>,
) {
    let cli_url = format!(
        "{}/v1internal:retrieveUserQuota",
        endpoints.gemini_cli_endpoint
    );
    let cli_headers = headers_for_style(HeaderStyle::GeminiCli, hub_version);
    let mut cli_req = client
        .post(&cli_url)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "project": project_id }));
    for (k, v) in cli_headers.to_pairs() {
        cli_req = cli_req.header(k, v);
    }

    match cli_req.send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(parsed) = resp.json::<RetrieveUserQuotaResponse>().await {
                for bucket in parsed.buckets {
                    let Some(rem) = bucket.remaining_fraction.map(|v| v.clamp(0.0, 1.0)) else {
                        continue;
                    };
                    let reset_ms = bucket.reset_time.as_deref().and_then(parse_rfc3339_ms);
                    merge_group_snapshot(groups, "gemini-cli", rem, reset_ms);
                    // Per-model buckets stay visible so one exhausted model cannot hide
                    // the healthy ones behind the aggregate `gemini-cli` minimum.
                    if let Some(model_id) = bucket.model_id.map(|m| m.trim().to_owned())
                        && !model_id.is_empty()
                    {
                        merge_group_snapshot(
                            groups,
                            &format!("gemini-cli:{model_id}"),
                            rem,
                            reset_ms,
                        );
                    }
                }
            }
        }
        Ok(resp) => warnings.push(format!(
            "retrieveUserQuota returned HTTP {}",
            resp.status().as_u16()
        )),
        Err(err) => warnings.push(format!("retrieveUserQuota error: {err}")),
    }
}

fn classify_hub_quota_group(model_id: &str) -> Option<&'static str> {
    let lower = model_id.to_ascii_lowercase();
    if lower.contains("claude") {
        Some("claude")
    } else if lower.contains("gemini") && (lower.contains("flash") || lower.contains("hydrogen")) {
        Some("gemini-flash")
    } else if lower.contains("gemini") {
        Some("gemini-pro")
    } else {
        None
    }
}

fn merge_group_snapshot(
    groups: &mut BTreeMap<String, QuotaGroupSnapshot>,
    group: &str,
    remaining_fraction: f64,
    reset_time_ms: Option<i64>,
) {
    let used = ((1.0 - remaining_fraction) * 100.0).clamp(0.0, 100.0);
    groups
        .entry(group.to_owned())
        .and_modify(|existing| {
            let prev_rem = existing.remaining_fraction.unwrap_or(1.0);
            if remaining_fraction < prev_rem {
                existing.remaining_fraction = Some(remaining_fraction);
                existing.used_percent = Some(used);
                existing.reset_time_ms = reset_time_ms.or(existing.reset_time_ms);
            }
        })
        .or_insert(QuotaGroupSnapshot {
            remaining_fraction: Some(remaining_fraction),
            used_percent: Some(used),
            reset_time_ms,
            label: None,
            models: Vec::new(),
        });
}

const DISPLAY_TIER_SUFFIXES: &[&str] = &[
    " (Minimal)",
    " (Low)",
    " (Medium)",
    " (High)",
    " (XHigh)",
    " (Extra High)",
    " (Extra-High)",
    " (Extra)",
    " (Thinking)",
];

fn is_skipped_quota_model(raw_id: &str, display_name: Option<&str>) -> bool {
    raw_id.ends_with("-disabled")
        || raw_id.starts_with("chat_")
        || raw_id.starts_with("tab_")
        || raw_id.ends_with("-tiered")
        || raw_id.contains("-tiered-")
        || raw_id.ends_with("-sonar")
        || raw_id.ends_with("-image")
        || (display_name.is_none() && raw_id.contains("image"))
}

fn clean_quota_model_label(base_id: &str, display_name: Option<&str>) -> String {
    if let Some(raw) = display_name.map(str::trim).filter(|s| !s.is_empty()) {
        let mut cleaned = raw;
        for suffix in DISPLAY_TIER_SUFFIXES {
            if let Some(stripped) = cleaned.strip_suffix(suffix) {
                cleaned = stripped.trim();
            }
        }
        if !cleaned.is_empty() {
            return cleaned.to_owned();
        }
    }
    base_id.to_owned()
}

fn summarize_bucket_labels(labels: &[String]) -> String {
    match labels.len() {
        0 => "Antigravity Pool".to_owned(),
        1 => labels[0].clone(),
        2 => format!("{} / {}", labels[0], labels[1]),
        3 => format!("{} / {} / {}", labels[0], labels[1], labels[2]),
        n => format!("{} / {} (+{})", labels[0], labels[1], n - 2),
    }
}

/// Derive dynamic quota pools directly from the live `/v1internal:fetchAvailableModels`
/// entries without relying on a hardcoded 3-pool list.
///
/// Tier variants of the same base model collapse to `canonical_quota_model_id`, and base
/// models that share the same `(coarse_family, remaining_bp, reset_time_ms)` (when
/// `reset_time_ms` is present) coalesce into one dynamic `pool:<id>` entry carrying its
/// human-readable `label` and `models` list. Models with distinct quota limits or reset
/// times (such as `Gemini 4 Argon` vs `Gemini 4 Hydrogen`) automatically form separate
/// quota pools.
pub fn populate_hub_quota_groups(
    groups: &mut BTreeMap<String, QuotaGroupSnapshot>,
    models: &[DiscoveredModelInfo],
) {
    struct BaseModelQuota {
        id: String,
        label: String,
        remaining: f64,
        reset_time_ms: Option<i64>,
    }

    let mut base_models: BTreeMap<String, BaseModelQuota> = BTreeMap::new();
    for info in models {
        if is_skipped_quota_model(&info.id, info.display_name.as_deref()) {
            continue;
        }
        let Some(rem) = info.remaining_fraction.map(|v| v.clamp(0.0, 1.0)) else {
            continue;
        };
        let base_id = canonical_quota_model_id(&info.id);
        if base_id.is_empty() {
            continue;
        }
        let label = clean_quota_model_label(&base_id, info.display_name.as_deref());
        base_models
            .entry(base_id.clone())
            .and_modify(|existing| {
                if rem < existing.remaining {
                    existing.remaining = rem;
                }
                if existing.reset_time_ms.is_none() {
                    existing.reset_time_ms = info.reset_time_ms;
                }
            })
            .or_insert(BaseModelQuota {
                id: base_id,
                label,
                remaining: rem,
                reset_time_ms: info.reset_time_ms,
            });
    }

    // Group base models that share the same family + live quota state + reset timestamp.
    // When `reset_time_ms` is absent, keep each base model in its own bucket so unrelated
    // models are never merged by guesswork.
    let mut buckets: BTreeMap<(String, i64, Option<i64>, String), Vec<BaseModelQuota>> =
        BTreeMap::new();
    for (_, bm) in base_models {
        let family = if bm.id.contains("claude") || bm.id.contains("gpt-oss") {
            "claude".to_owned()
        } else if bm.id.contains("gemini") {
            "gemini".to_owned()
        } else {
            bm.id.clone()
        };
        #[allow(clippy::cast_possible_truncation)]
        let rem_bp = (bm.remaining * 1000.0).round() as i64;
        let disambiguator = if bm.reset_time_ms.is_some() {
            String::new()
        } else {
            bm.id.clone()
        };
        buckets
            .entry((family, rem_bp, bm.reset_time_ms, disambiguator))
            .or_default()
            .push(bm);
    }

    for ((_, _, reset_time_ms, _), entries) in buckets {
        let Some(first) = entries.first() else {
            continue;
        };
        let min_rem = entries
            .iter()
            .map(|e| e.remaining)
            .fold(first.remaining, f64::min);
        let used = ((1.0 - min_rem) * 100.0).clamp(0.0, 100.0);
        let model_ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
        let mut unique_labels: Vec<String> = Vec::new();
        for e in &entries {
            if !unique_labels.contains(&e.label) {
                unique_labels.push(e.label.clone());
            }
        }
        let label = summarize_bucket_labels(&unique_labels);
        let key = format!("pool:{}", first.id);
        groups.insert(
            key,
            QuotaGroupSnapshot {
                remaining_fraction: Some(min_rem),
                used_percent: Some(used),
                reset_time_ms,
                label: Some(label),
                models: model_ids,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::{Json, Router};
    use tokio::net::TcpListener;

    #[test]
    fn parses_rfc3339_utc() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:05.250Z"), Some(5250));
        assert_eq!(
            parse_rfc3339_ms("2026-11-14T00:00:00Z"),
            Some(1_794_614_400_000)
        );
        // Explicit offset is normalized to UTC.
        assert_eq!(
            parse_rfc3339_ms("2026-11-14T00:00:00+02:00"),
            Some(1_794_614_400_000 - 7_200_000)
        );
        // Short/long fractional parts are padded/truncated, never sliced by byte.
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.5Z"), Some(500));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.123456Z"), Some(123));
    }

    #[test]
    fn parse_rfc3339_rejects_multibyte_fraction_without_panicking() {
        // A non-ASCII code point in the fractional part used to be byte-sliced (`&padded[..3]`).
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.ΩΩΩZ"), Some(0));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.1Ω2Z"), Some(120));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00.ΩZ"), Some(0));
    }

    #[test]
    fn parse_rfc3339_rejects_out_of_range_clock_components() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T25:99:99Z"), None);
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:99:00Z"), None);
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:99Z"), None);
        assert_eq!(parse_rfc3339_ms("1970-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00+25:00"), None);
    }

    #[test]
    fn parse_rfc3339_rejects_malformed_shapes() {
        assert_eq!(parse_rfc3339_ms(""), None);
        assert_eq!(parse_rfc3339_ms("2026-11-14"), None);
        assert_eq!(parse_rfc3339_ms("not-a-date"), None);
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00"), None);
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00Z"), None);
    }

    #[tokio::test]
    async fn fetches_hub_models_and_gemini_cli_quota() {
        let app = Router::new()
            .route(
                "/v1internal:fetchAvailableModels",
                post(|| async {
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "models": {
                                "claude-sonnet-4-6": {
                                    "displayName": "Claude Sonnet 4.6",
                                    "maxTokens": 200_000,
                                    "maxOutputTokens": 64_000,
                                    "quotaInfo": { "remainingFraction": 0.25, "resetTime": "2026-11-14T00:00:00Z" }
                                },
                                "gemini-3-flash": {
                                    "supportsThinking": true,
                                    "quotaInfo": { "remainingFraction": 0.80 }
                                }
                            }
                        })),
                    )
                }),
            )
            .route(
                "/v1internal:retrieveUserQuota",
                post(|| async {
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "buckets": [
                                { "modelId": "gemini-2.5-pro", "remainingFraction": 0.60 }
                            ]
                        })),
                    )
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoints = AntigravityEndpoints::mock(&format!("http://127.0.0.1:{port}"));
        let res = fetch_account_quota(
            &reqwest::Client::new(),
            &endpoints,
            &VersionTracker::new(),
            "ya29.tok",
            Some("proj-1"),
            100_000,
        )
        .await
        .expect("quota fetch");

        assert!(res.warning.is_none());
        assert_eq!(res.discovered_models.len(), 2);
        let claude = res.cached_quota.groups.get("claude").expect("claude");
        assert!((claude.used_percent.unwrap_or_default() - 75.0).abs() < 0.01);
        let cli = res.cached_quota.groups.get("gemini-cli").expect("cli");
        assert!((cli.used_percent.unwrap_or_default() - 40.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn gemini_cli_per_model_buckets_stay_visible_next_to_the_aggregate() {
        let app = Router::new()
            .route(
                "/v1internal:fetchAvailableModels",
                post(|| async { (StatusCode::OK, Json(serde_json::json!({ "models": {} }))) }),
            )
            .route(
                "/v1internal:retrieveUserQuota",
                post(|| async {
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "buckets": [
                                { "modelId": "gemini-2.5-pro", "remainingFraction": 0.0 },
                                { "modelId": "gemini-2.5-flash", "remainingFraction": 0.9 },
                                { "remainingFraction": 0.5 }
                            ]
                        })),
                    )
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoints = AntigravityEndpoints::mock(&format!("http://127.0.0.1:{port}"));
        let res = fetch_account_quota(
            &reqwest::Client::new(),
            &endpoints,
            &VersionTracker::new(),
            "ya29.tok",
            Some("proj-1"),
            100_000,
        )
        .await
        .expect("quota fetch");

        let groups = &res.cached_quota.groups;
        let aggregate = groups.get("gemini-cli").expect("aggregate group");
        assert!(
            (aggregate.used_percent.unwrap_or_default() - 100.0).abs() < 0.01,
            "aggregate keeps the min-remaining merge: {aggregate:?}"
        );

        let pro = groups
            .get("gemini-cli:gemini-2.5-pro")
            .expect("per-model pro bucket");
        assert!((pro.used_percent.unwrap_or_default() - 100.0).abs() < 0.01);
        let flash = groups
            .get("gemini-cli:gemini-2.5-flash")
            .expect("per-model flash bucket");
        assert!(
            (flash.used_percent.unwrap_or_default() - 10.0).abs() < 0.01,
            "healthy model must not be hidden by the exhausted one: {flash:?}"
        );
        // A bucket without `modelId` contributes only to the aggregate group.
        assert!(!groups.contains_key("gemini-cli:"));
    }

    #[test]
    fn dynamic_hub_quota_groups_separate_argon_hydrogen_and_claude_pools() {
        let mut groups = BTreeMap::new();
        let models = vec![
            DiscoveredModelInfo {
                id: "gemini-4-argon-low".into(),
                display_name: Some("Gemini 4 Argon (Low)".into()),
                max_tokens: Some(1_048_576),
                max_output_tokens: Some(65_535),
                supports_thinking: true,
                remaining_fraction: Some(0.81),
                reset_time_ms: Some(1_795_891_100_000),
            },
            DiscoveredModelInfo {
                id: "gemini-4-argon-high".into(),
                display_name: Some("Gemini 4 Argon (High)".into()),
                max_tokens: Some(1_048_576),
                max_output_tokens: Some(65_535),
                supports_thinking: true,
                remaining_fraction: Some(0.81),
                reset_time_ms: Some(1_795_891_100_000),
            },
            DiscoveredModelInfo {
                id: "gemini-4-hydrogen-high".into(),
                display_name: Some("Gemini 4 Hydrogen (High)".into()),
                max_tokens: Some(1_048_576),
                max_output_tokens: Some(65_535),
                supports_thinking: true,
                remaining_fraction: Some(0.01),
                reset_time_ms: Some(1_795_297_700_000),
            },
            DiscoveredModelInfo {
                id: "gemini-4-neon-xhigh".into(),
                display_name: Some("Gemini 4 Neon (Extra High)".into()),
                max_tokens: Some(1_048_576),
                max_output_tokens: Some(65_535),
                supports_thinking: true,
                remaining_fraction: Some(0.01),
                reset_time_ms: Some(1_795_297_700_000),
            },
            DiscoveredModelInfo {
                id: "claude-opus-5-5-high".into(),
                display_name: Some("Claude Opus 5.5 (High)".into()),
                max_tokens: Some(200_000),
                max_output_tokens: Some(64_000),
                supports_thinking: true,
                remaining_fraction: Some(0.50),
                reset_time_ms: Some(1_795_891_100_000),
            },
        ];

        populate_hub_quota_groups(&mut groups, &models);

        let argon = groups.get("pool:gemini-4-argon").expect("argon pool");
        assert_eq!(argon.label.as_deref(), Some("Gemini 4 Argon"));
        assert!((argon.remaining_fraction.unwrap_or_default() - 0.81).abs() < 1e-6);
        assert_eq!(argon.models, vec!["gemini-4-argon"]);

        let hydrogen = groups
            .get("pool:gemini-4-hydrogen")
            .expect("hydrogen+neon pool");
        assert_eq!(
            hydrogen.label.as_deref(),
            Some("Gemini 4 Hydrogen / Gemini 4 Neon")
        );
        assert!((hydrogen.remaining_fraction.unwrap_or_default() - 0.01).abs() < 1e-6);
        assert_eq!(hydrogen.models, vec!["gemini-4-hydrogen", "gemini-4-neon"]);

        let claude = groups.get("pool:claude-opus-5-5").expect("claude 5.5 pool");
        assert_eq!(claude.label.as_deref(), Some("Claude Opus 5.5"));
        assert!((claude.remaining_fraction.unwrap_or_default() - 0.50).abs() < 1e-6);
    }
}
