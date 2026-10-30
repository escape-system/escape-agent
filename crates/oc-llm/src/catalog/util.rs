//! Small shared helpers (map merging, frequency, fuzzy matching).

use serde_json::{Map, Value};

pub(super) fn merge_map(base: &mut Map<String, Value>, overlay: &Map<String, Value>) {
    for (key, value) in overlay {
        match (base.get_mut(key), value) {
            (Some(Value::Object(base_object)), Value::Object(overlay_object)) => {
                merge_map(base_object, overlay_object);
            }
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

pub(super) fn most_common<T: Clone + Eq>(values: &[T]) -> Option<T> {
    let mut counts: Vec<(&T, usize)> = Vec::new();
    for value in values {
        if let Some(entry) = counts.iter_mut().find(|(candidate, _)| *candidate == value) {
            entry.1 += 1;
        } else {
            counts.push((value, 1));
        }
    }
    let mut best: Option<(&T, usize)> = None;
    for entry in counts {
        if best.is_none_or(|(_, count)| entry.1 > count) {
            best = Some(entry);
        }
    }
    best.map(|(value, _)| value.clone())
}

pub(super) fn strings(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

pub(super) fn similar(candidate: &str, query: &str) -> bool {
    similarity(candidate, query).is_some()
}

pub(super) fn similarity(candidate: &str, query: &str) -> Option<usize> {
    let candidate_lower = candidate.to_ascii_lowercase();
    let query_lower = query.to_ascii_lowercase();
    if candidate_lower == query_lower {
        Some(0)
    } else if candidate_lower.starts_with(&query_lower) {
        Some(1)
    } else if candidate_lower.contains(&query_lower) || query_lower.contains(&candidate_lower) {
        Some(2)
    } else {
        None
    }
}
