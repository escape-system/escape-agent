//! Persisted `/v1internal:fetchAvailableModels` cache and discovery merge helpers.

use crate::config::resolve_config_dir;
use crate::error::Result;
use crate::quota::DiscoveredModelInfo;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::overlay::{
    TIER_ORDER, catalog_id_for_raw, discovered_display_name, discovered_family,
    discovered_snapshot_entry, is_legacy_gemini3_tier, is_skipped_discovered_id, split_tier_suffix,
    strip_display_tier_suffix,
};

/// Persisted cache of models discovered from `/v1internal:fetchAvailableModels`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredModelCache {
    /// Timestamp when the cache was last updated (epoch ms).
    pub updated_at: i64,
    /// Upstream models keyed by raw Cloud Code model ID.
    pub models: BTreeMap<String, DiscoveredModelInfo>,
}

/// Default location for `antigravity-models.json`.
#[must_use]
pub fn default_models_cache_path(config_dir: Option<&Path>) -> PathBuf {
    config_dir
        .map_or_else(resolve_config_dir, PathBuf::from)
        .join("antigravity-models.json")
}

/// Load persisted discovered model cache if present.
pub fn load_discovered_models(path: &Path) -> Result<DiscoveredModelCache> {
    if !path.is_file() {
        return Ok(DiscoveredModelCache::default());
    }
    let raw = std::fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(DiscoveredModelCache::default());
    }
    Ok(serde_json::from_str(&raw)?)
}

/// Persist discovered model cache atomically.
pub fn save_discovered_models(path: &Path, cache: &DiscoveredModelCache) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(cache)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub(super) fn insert_discovered_models(map: &mut Map<String, Value>, cache: &DiscoveredModelCache) {
    let mut tiered_groups: BTreeMap<String, Vec<(&str, &DiscoveredModelInfo)>> = BTreeMap::new();

    for (raw_id, info) in &cache.models {
        if is_skipped_discovered_id(raw_id) {
            continue;
        }
        let catalog_id = catalog_id_for_raw(raw_id);
        if map.contains_key(&catalog_id) {
            // A built-in spec is only the cold-start floor: Cloud Code's live
            // limits win, so a context window it shrinks or grows never stays
            // frozen at the compiled-in number.
            apply_live_limits(map, &catalog_id, info);
            continue;
        }
        if map.contains_key(raw_id) {
            continue;
        }
        if let Some((base, tier)) = split_tier_suffix(raw_id) {
            let base_catalog_id = catalog_id_for_raw(base);
            if map.contains_key(&base_catalog_id) {
                apply_live_limits(map, &base_catalog_id, info);
                continue;
            }
            if map.contains_key(base) || is_legacy_gemini3_tier(base) {
                continue;
            }
            tiered_groups
                .entry(base.to_owned())
                .or_default()
                .push((tier, info));
            continue;
        }
        insert_single_discovered_model(map, &catalog_id, raw_id, info);
    }

    for (base, entries) in tiered_groups {
        if entries.len() >= 2 || base.starts_with("gemini-") || base.starts_with("claude-") {
            insert_tiered_discovered_group(map, &base, &entries);
        } else if let Some((_, info)) = entries.first() {
            let catalog_id = catalog_id_for_raw(&info.id);
            insert_single_discovered_model(map, &catalog_id, &info.id, info);
        }
    }
}

/// Overwrite a built-in entry's `limit.context` / `limit.output` with the
/// non-zero values Cloud Code reported. Absent or zero live values keep the
/// built-in number; nothing is invented.
fn apply_live_limits(map: &mut Map<String, Value>, catalog_id: &str, info: &DiscoveredModelInfo) {
    let Some(limit) = map
        .get_mut(catalog_id)
        .and_then(|entry| entry.get_mut("limit"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    if let Some(context) = info.max_tokens.filter(|value| *value > 0) {
        limit.insert("context".to_owned(), json!(context));
    }
    if let Some(output) = info.max_output_tokens.filter(|value| *value > 0) {
        limit.insert("output".to_owned(), json!(output));
    }
}

fn insert_single_discovered_model(
    map: &mut Map<String, Value>,
    catalog_id: &str,
    raw_id: &str,
    info: &DiscoveredModelInfo,
) {
    let family = discovered_family(raw_id);
    let display = discovered_display_name(raw_id, info.display_name.as_deref());
    let context = info.max_tokens.unwrap_or(if family == "claude" {
        200_000
    } else {
        1_048_576
    });
    let output = info.max_output_tokens.unwrap_or(64_000);
    let reasoning = if info.supports_thinking {
        json!({ "supported": true, "effort": ["low", "high"] })
    } else {
        json!({ "supported": false })
    };
    map.insert(
        catalog_id.to_owned(),
        discovered_snapshot_entry(catalog_id, &display, family, &reasoning, context, output),
    );
}

fn insert_tiered_discovered_group(
    map: &mut Map<String, Value>,
    base: &str,
    entries: &[(&str, &DiscoveredModelInfo)],
) {
    let catalog_id = catalog_id_for_raw(base);
    let mut sorted_tiers: Vec<&str> = Vec::new();
    for &known in TIER_ORDER {
        if entries.iter().any(|(t, _)| *t == known) {
            sorted_tiers.push(known);
        }
    }
    let sample = entries[0].1;
    let family = discovered_family(base);
    let max_ctx = entries
        .iter()
        .filter_map(|(_, info)| info.max_tokens)
        .max()
        .unwrap_or(if family == "claude" {
            200_000
        } else {
            1_048_576
        });
    let max_out = entries
        .iter()
        .filter_map(|(_, info)| info.max_output_tokens)
        .max()
        .unwrap_or(64_000);
    let supports_thinking = entries.iter().any(|(_, info)| info.supports_thinking);
    let base_display = sample
        .display_name
        .as_deref()
        .map_or(base, strip_display_tier_suffix);
    let display = format!("{base_display} (Antigravity)");
    let reasoning = if supports_thinking && !sorted_tiers.is_empty() {
        json!({ "supported": true, "effort": sorted_tiers })
    } else {
        json!({ "supported": supports_thinking })
    };
    map.insert(
        catalog_id.clone(),
        discovered_snapshot_entry(&catalog_id, &display, family, &reasoning, max_ctx, max_out),
    );
}
