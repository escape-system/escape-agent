//! Usage cost calculation and numeric token conversions.

use crate::event::Usage;

use super::types::{Cost, CostTierKind};

/// Cost in USD. Mirrors upstream `SessionUsage.calculateCost`.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "token counts stay far below f64's 2^53 exact-integer range"
)]
pub fn calculate_cost(bands: &[Cost], usage: &Usage) -> f64 {
    let context = usage.non_cached_input_tokens.unwrap_or(0)
        + usage.cache_read_input_tokens.unwrap_or(0)
        + usage.cache_write_input_tokens.unwrap_or(0);
    let tier = bands
        .iter()
        .filter(|band| {
            band.tier
                .is_some_and(|tier| tier.kind == CostTierKind::Context && context > tier.size)
        })
        .max_by_key(|band| band.tier.map_or(0, |tier| tier.size));
    let band = tier.or_else(|| bands.iter().find(|band| band.tier.is_none()));
    let Some(band) = band else {
        return 0.0;
    };
    let safe = |value: Option<u64>| value.unwrap_or(0) as f64;
    let visible_output = usage.visible_output_tokens();
    (safe(usage.non_cached_input_tokens) * band.input
        + (visible_output as f64 + safe(usage.reasoning_tokens)) * band.output
        + safe(usage.cache_read_input_tokens) * band.cache.read
        + safe(usage.cache_write_input_tokens) * band.cache.write)
        / 1_000_000.0
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "catalog limits are non-negative token counts; fractional/NaN input clamps to zero"
)]
pub(super) fn count(value: f64) -> u64 {
    if value.is_finite() {
        value.max(0.0) as u64
    } else {
        0
    }
}

pub(super) fn unsigned(value: i64) -> u64 {
    u64::try_from(value.max(0)).unwrap_or(0)
}
