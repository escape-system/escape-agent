//! Built-in Antigravity & Gemini CLI model definitions, thinking variants,
//! and cached `/v1internal:fetchAvailableModels` discovery overlays for `oc-llm::Catalog`.

mod builtin;
mod discovery;
mod overlay;

pub use builtin::builtin_antigravity_models;
pub use discovery::{
    DiscoveredModelCache, default_models_cache_path, load_discovered_models, save_discovered_models,
};
pub use overlay::{
    merged_antigravity_models, overlay_catalog_models, resolve_antigravity_catalog_model,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::DiscoveredModelInfo;
    use oc_llm::Catalog;

    #[test]
    fn builtin_models_include_gemini4_claude55_and_gemini3_variants() {
        let models = builtin_antigravity_models();

        let h2 = models
            .get("antigravity-gemini-4-hydrogen")
            .expect("gemini 4 hydrogen");
        let h2_vars: Vec<&str> = h2.variants.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(h2_vars, vec!["low", "medium", "high", "xhigh"]);
        assert_eq!(h2.limit.output, 262_144);

        let argon = models
            .get("antigravity-gemini-4-argon")
            .expect("gemini 4 argon");
        let argon_vars: Vec<&str> = argon.variants.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(argon_vars, vec!["low", "medium", "high"]);

        let flash38 = models
            .get("antigravity-gemini-3.8-flash")
            .expect("gemini 3.8 flash");
        let f38_vars: Vec<&str> = flash38.variants.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(f38_vars, vec!["low", "medium", "high"]);

        let opus55 = models
            .get("antigravity-claude-opus-5-5")
            .expect("claude opus 5.5");
        let opus55_vars: Vec<&str> = opus55.variants.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(opus55_vars, vec!["low", "medium", "high"]);

        let sonnet55 = models
            .get("antigravity-claude-sonnet-5-5")
            .expect("claude sonnet 5.5");
        let sonnet55_vars: Vec<&str> = sonnet55.variants.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(sonnet55_vars, vec!["low", "medium", "high"]);

        assert!(models.contains_key("antigravity-gpt-oss-120b-medium"));

        let g3 = models.get("antigravity-gemini-3.1-pro").expect("3.1 pro");
        let g3_vars: Vec<&str> = g3.variants.iter().map(|v| v.id.as_str()).collect();
        assert!(g3_vars.contains(&"low"));
        assert!(g3_vars.contains(&"high"));

        let g3_flash = models.get("antigravity-gemini-3-flash").expect("3 flash");
        let flash_vars: Vec<&str> = g3_flash.variants.iter().map(|v| v.id.as_str()).collect();
        assert!(flash_vars.contains(&"minimal"));
        assert!(flash_vars.contains(&"high"));

        let opus = models
            .get("antigravity-claude-opus-4-6-thinking")
            .expect("opus thinking");
        assert!(!opus.variants.is_empty());
        assert!(models.contains_key("gemini-2.5-flash"));
    }

    #[test]
    fn merged_models_collapses_tiered_variants_and_skips_internal_disabled() {
        let mut cache = DiscoveredModelCache::default();
        for (id, display, frac) in [
            ("gemini-3-pro-high", None, Some(1.0)),
            (
                "gemini-4-hydrogen-xhigh",
                Some("Gemini 4 Hydrogen (XHigh)"),
                Some(0.96),
            ),
            (
                "claude-opus-5-5-medium",
                Some("Claude Opus 5.5 (Medium)"),
                Some(0.5),
            ),
            ("gemini-4-carbon-disabled", Some("Gemini 4 Carbon"), None),
            ("chat_20706", None, Some(1.0)),
            ("tab_flash_lite_preview", None, Some(1.0)),
            ("gemini-3.8-flash-tiered", None, Some(0.9)),
            ("gemini-5-nova-low", Some("Gemini 5 Nova (Low)"), Some(1.0)),
            (
                "gemini-5-nova-high",
                Some("Gemini 5 Nova (High)"),
                Some(1.0),
            ),
        ] {
            cache.models.insert(
                id.to_owned(),
                DiscoveredModelInfo {
                    id: id.to_owned(),
                    display_name: display.map(str::to_owned),
                    max_tokens: None,
                    max_output_tokens: None,
                    supports_thinking: true,
                    remaining_fraction: frac,
                    reset_time_ms: None,
                },
            );
        }
        cache.models.insert(
            "gemini-exp-new".to_owned(),
            DiscoveredModelInfo {
                id: "gemini-exp-new".to_owned(),
                display_name: Some("Gemini Experimental".to_owned()),
                max_tokens: Some(500_000),
                max_output_tokens: Some(16_000),
                supports_thinking: false,
                remaining_fraction: Some(0.9),
                reset_time_ms: None,
            },
        );

        let merged = merged_antigravity_models(Some(&cache));
        assert!(!merged.contains_key("antigravity-gemini-3-pro-high"));
        assert!(!merged.contains_key("antigravity-gemini-4-hydrogen-xhigh"));
        assert!(!merged.contains_key("antigravity-claude-opus-5-5-medium"));
        assert!(!merged.contains_key("antigravity-gemini-4-carbon-disabled"));
        assert!(!merged.contains_key("antigravity-chat_20706"));
        assert!(!merged.contains_key("antigravity-tab_flash_lite_preview"));
        assert!(!merged.contains_key("antigravity-gemini-3.8-flash-tiered"));
        assert!(merged.contains_key("antigravity-gemini-exp-new"));

        let nova = merged
            .get("antigravity-gemini-5-nova")
            .expect("collapsed dynamic tiered model");
        let nova_vars: Vec<&str> = nova.variants.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(nova_vars, vec!["low", "high"]);
        assert_eq!(nova.name, "Gemini 5 Nova (Antigravity)");
    }

    #[test]
    fn live_discovery_overrides_builtin_limits_but_keeps_missing_values() {
        let mut cache = DiscoveredModelCache::default();
        for (id, ctx, out) in [
            ("gemini-4-hydrogen-low", Some(262_144), None),
            ("gemini-4-hydrogen-high", Some(262_144), Some(32_768)),
            ("gemini-4-argon-high", None, Some(0)),
        ] {
            cache.models.insert(
                id.to_owned(),
                DiscoveredModelInfo {
                    id: id.to_owned(),
                    display_name: None,
                    max_tokens: ctx,
                    max_output_tokens: out,
                    supports_thinking: true,
                    remaining_fraction: Some(1.0),
                    reset_time_ms: None,
                },
            );
        }
        let merged = merged_antigravity_models(Some(&cache));
        let hydrogen = merged
            .get("antigravity-gemini-4-hydrogen")
            .expect("hydrogen");
        assert_eq!(hydrogen.limit.context, 262_144, "live context wins");
        assert_eq!(hydrogen.limit.output, 32_768, "live output wins");
        let argon = merged.get("antigravity-gemini-4-argon").expect("argon");
        assert_eq!(
            argon.limit.context, 1_048_576,
            "absent live value keeps the floor"
        );
        assert_eq!(
            argon.limit.output, 65_536,
            "zero live value keeps the floor"
        );
    }

    #[test]
    fn overlay_populates_google_provider_and_dynamic_resolution() {
        let mut cat = Catalog::from_snapshot("{}").expect("empty");
        overlay_catalog_models(&mut cat, None);
        let resolved = cat
            .resolve("google/antigravity-gemini-3.1-pro#high")
            .expect("resolve");
        assert_eq!(resolved.model.id, "antigravity-gemini-3.1-pro");

        let mut cache = DiscoveredModelCache::default();
        cache.models.insert(
            "gemini-5-ultra".to_owned(),
            DiscoveredModelInfo {
                id: "gemini-5-ultra".to_owned(),
                display_name: Some("Gemini 5 Ultra".to_owned()),
                max_tokens: Some(2_000_000),
                max_output_tokens: Some(65_536),
                supports_thinking: true,
                remaining_fraction: Some(1.0),
                reset_time_ms: None,
            },
        );
        let dyn_res = resolve_antigravity_catalog_model(
            &cat,
            Some(&cache),
            "google/antigravity-gemini-5-ultra",
        )
        .expect("dynamic lookup");
        assert_eq!(dyn_res.model.id, "antigravity-gemini-5-ultra");

        let legacy_tier = resolve_antigravity_catalog_model(
            &cat,
            Some(&cache),
            "google/antigravity-gemini-4-hydrogen-xhigh",
        )
        .expect("tier suffix lookup");
        assert_eq!(legacy_tier.model.id, "antigravity-gemini-4-hydrogen");
        assert_eq!(
            legacy_tier.variant.as_ref().map(|v| v.id.as_str()),
            Some("xhigh")
        );
    }
}
