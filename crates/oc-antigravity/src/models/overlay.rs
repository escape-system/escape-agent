//! Merge built-in and live-discovered models and overlay them onto an `oc_llm::Catalog`.

use crate::constants::{ANTIGRAVITY_ENDPOINT_DAILY, ANTIGRAVITY_PROVIDER_ID};
use oc_llm::{Catalog, ModelInfo, ResolvedModel, parse_reference};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

use super::builtin::{BUILTIN_SPECS, build_snapshot_catalog, spec_to_snapshot_entry};
use super::discovery::{DiscoveredModelCache, insert_discovered_models};

/// Merge built-in Antigravity definitions with dynamically discovered upstream models.
#[must_use]
pub fn merged_antigravity_models(
    cache: Option<&DiscoveredModelCache>,
) -> BTreeMap<String, ModelInfo> {
    let live_only = cache.is_some_and(|c| c.updated_at > 0 && c.models.len() >= 4);
    let mut map = Map::new();
    for spec in BUILTIN_SPECS {
        if live_only {
            let matches_discovered = cache.is_some_and(|c| {
                c.models.keys().any(|raw_id| {
                    if is_skipped_discovered_id(raw_id) {
                        return false;
                    }
                    let cat_id = catalog_id_for_raw(raw_id);
                    if cat_id == spec.id || raw_id == spec.id {
                        return true;
                    }
                    split_tier_suffix(raw_id).is_some_and(|(base, _)| {
                        catalog_id_for_raw(base) == spec.id || base == spec.id
                    })
                })
            });
            if !matches_discovered {
                continue;
            }
        }
        map.insert(spec.id.to_owned(), spec_to_snapshot_entry(spec));
    }

    if let Some(cache) = cache {
        insert_discovered_models(&mut map, cache);
    }

    let Some(cat) = build_snapshot_catalog(&map) else {
        return BTreeMap::new();
    };
    cat.models()
        .iter()
        .cloned()
        .map(|m| (m.id.clone(), m))
        .collect()
}

/// Ensure `"google"` and its Antigravity models exist in `catalog`.
///
/// Built-in snapshots sometimes miss newer Antigravity IDs (`antigravity-*`) and
/// live-discovered models, so we insert `"google"` provider if absent and upsert
/// all Antigravity/CLI models cleanly.
pub fn overlay_catalog_models(catalog: &mut Catalog, discovered: Option<&DiscoveredModelCache>) {
    let mut map = Map::new();
    for spec in BUILTIN_SPECS {
        map.insert(spec.id.to_owned(), spec_to_snapshot_entry(spec));
    }
    let Some(sub) = build_snapshot_catalog(&map) else {
        return;
    };
    if catalog.provider(ANTIGRAVITY_PROVIDER_ID).is_none()
        && let Some(provider) = sub.provider(ANTIGRAVITY_PROVIDER_ID)
    {
        catalog.upsert_provider(provider.clone());
    }
    let merged = merged_antigravity_models(discovered)
        .into_values()
        .collect::<Vec<_>>();
    catalog.upsert_provider_models(merged);
}

/// Resolve a `google/<model>[#variant]` reference against merged built-in and
/// live-discovered Antigravity models, reusing `"google"` provider metadata
/// from `base_catalog` when present.
#[must_use]
pub fn resolve_antigravity_catalog_model(
    base_catalog: &Catalog,
    discovered: Option<&DiscoveredModelCache>,
    reference: &str,
) -> Option<ResolvedModel> {
    let parsed = parse_reference(reference).ok()?;
    if parsed.provider != ANTIGRAVITY_PROVIDER_ID {
        return None;
    }

    let mut overlay = Catalog::from_snapshot("{}").ok()?;
    if let Some(provider) = base_catalog.provider(ANTIGRAVITY_PROVIDER_ID) {
        overlay.upsert_provider(provider.clone());
    }
    overlay_catalog_models(&mut overlay, None);
    overlay_catalog_models(&mut overlay, discovered);

    if let Ok(resolved) = overlay.resolve(reference) {
        return Some(resolved);
    }

    if !parsed.id.starts_with("antigravity-") {
        let prefixed_ref = match parsed.variant {
            Some(ref var) => format!("{}/antigravity-{}#{var}", parsed.provider, parsed.id),
            None => format!("{}/antigravity-{}", parsed.provider, parsed.id),
        };
        if let Ok(resolved) = overlay.resolve(&prefixed_ref) {
            return Some(resolved);
        }
    }

    if parsed.variant.is_none()
        && let Some((base, tier)) = split_tier_suffix(&parsed.id)
    {
        let same_base_ref = format!("{}/{base}#{tier}", parsed.provider);
        if let Ok(resolved) = overlay.resolve(&same_base_ref) {
            return Some(resolved);
        }
        if !base.starts_with("antigravity-") {
            let prefixed_ref = format!("{}/antigravity-{base}#{tier}", parsed.provider);
            if let Ok(resolved) = overlay.resolve(&prefixed_ref) {
                return Some(resolved);
            }
        }
    }

    None
}

pub(super) const TIER_ORDER: &[&str] = &["minimal", "low", "medium", "high", "xhigh"];

pub(super) fn catalog_id_for_raw(raw_id: &str) -> String {
    if raw_id.starts_with("antigravity-") {
        raw_id.to_owned()
    } else {
        format!("antigravity-{raw_id}")
    }
}

pub(super) fn is_skipped_discovered_id(raw_id: &str) -> bool {
    raw_id.ends_with("-disabled")
        || raw_id.starts_with("chat_")
        || raw_id.starts_with("tab_")
        || raw_id.ends_with("-tiered")
        || raw_id.contains("-tiered-")
        || raw_id.ends_with("-sonar")
        || raw_id.ends_with("-image")
}

pub(super) fn split_tier_suffix(raw_id: &str) -> Option<(&str, &str)> {
    for &tier in &["xhigh", "minimal", "medium", "high", "low"] {
        if let Some(base) = raw_id.strip_suffix(&format!("-{tier}")) {
            return Some((base, tier));
        }
    }
    None
}

pub(super) fn is_legacy_gemini3_tier(base: &str) -> bool {
    base == "gemini-3-pro" || base == "gemini-3.1-pro" || base == "gemini-3-flash"
}

pub(super) fn discovered_family(raw_id: &str) -> &'static str {
    let lower = raw_id.to_ascii_lowercase();
    if lower.contains("claude") {
        "claude"
    } else if lower.contains("gpt-oss") {
        "gpt-oss"
    } else {
        "gemini"
    }
}

pub(super) fn discovered_display_name(raw_id: &str, upstream: Option<&str>) -> String {
    upstream
        .filter(|s| !s.trim().is_empty())
        .map_or_else(|| format!("{raw_id} (Antigravity)"), ToOwned::to_owned)
}

pub(super) fn strip_display_tier_suffix(display: &str) -> &str {
    for suffix in [
        " (Minimal)",
        " (Low)",
        " (Medium)",
        " (High)",
        " (XHigh)",
        " (Extra)",
    ] {
        if let Some(stripped) = display.strip_suffix(suffix) {
            return stripped.trim();
        }
    }
    display.trim()
}

pub(super) fn discovered_snapshot_entry(
    catalog_id: &str,
    display: &str,
    family: &str,
    reasoning: &Value,
    context: u64,
    output: u64,
) -> Value {
    json!({
        "id": catalog_id,
        "type": "chat",
        "name": display,
        "family": family,
        "release_date": "2026-10-01",
        "status": "active",
        "modalities": {
            "input": ["text", "image/png", "application/pdf"],
            "output": ["text"]
        },
        "capabilities": {
            "tools": { "supported": true },
            "reasoning": reasoning,
            "temperature": true
        },
        "limit": {
            "context": context,
            "output": output
        },
        "cost": {
            "text": {
                "input": { "price": 0 },
                "output": { "price": 0 }
            }
        },
        "api": {
            "generate-content": {
                "base_url": ANTIGRAVITY_ENDPOINT_DAILY,
                "sdk": { "aisdk": "@ai-sdk/google" }
            }
        }
    })
}
