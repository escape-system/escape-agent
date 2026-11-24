//! Step-budget policy normalization.
//!
//! `step_budget` is a recognized section (`super::keys::HANDLED`), so it is
//! copied through normalization instead of being mistaken for an unknown key.
//! Only the shape is enforced here — each recognized key must be an unsigned
//! integer. The per-key minimums and the documented defaults live in the
//! policy that consumes the section
//! (`crates/oc-runtime/src/turn/budget.rs` `BudgetPolicy::from_merged`), which
//! keeps a single source of truth and lets a present-but-too-small value fall
//! back to its default exactly as that reader already does.
//!
//! Absent keys are **not** materialized: a config that sets one override keeps
//! the other five on their defaults, and changing a default later is not
//! shadowed by values invented at normalization time.

use serde_json::{Map, Value};

use super::copy::jp;
use super::ctx::Ctx;

/// Keys the runtime policy understands (`BudgetPolicy::from_merged`).
const KEYS: [&str; 6] = [
    "extension_steps",
    "max_extensions",
    "min_cap",
    "thrash_limit",
    "window",
    "ceiling_factor",
];

pub(super) fn normalize_step_budget(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    let Some(value) = input.get("step_budget") else {
        return;
    };
    let Some(object) = value.as_object() else {
        ctx.invalid("$.step_budget");
        return;
    };
    let mut result = Map::new();
    for key in KEYS {
        if let Some(value) = object.get(key) {
            match value.as_u64() {
                Some(value) => {
                    result.insert(key.to_owned(), Value::from(value));
                }
                None => ctx.invalid(&jp("$.step_budget", key)),
            }
        }
    }
    // Unknown sub-keys survive; the policy ignores them.
    for (key, value) in object {
        if result.contains_key(key) || KEYS.contains(&key.as_str()) {
            continue;
        }
        result.insert(key.clone(), value.clone());
    }
    if !result.is_empty() || object.is_empty() {
        out.insert("step_budget".to_owned(), Value::Object(result));
    }
}
