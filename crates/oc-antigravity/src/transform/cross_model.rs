//! Cross-model history sanitization and tool call/response pairing guarantees.

use crate::accounts::ModelFamily;
use crate::thinking::is_valid_thought_signature;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Sanitize `contents` so turns originating from a different model family remain valid
/// and ensure every `functionCall` has an ID paired with its subsequent `functionResponse`.
pub fn sanitize_cross_model_contents(
    contents: &mut Vec<Value>,
    target_family: ModelFamily,
    tool_id_recovery: bool,
) {
    assign_missing_call_ids_and_pair(contents, tool_id_recovery);
    strip_foreign_model_artifacts(contents, target_family);
    merge_adjacent_same_role_turns(contents);
}

#[allow(clippy::too_many_lines)]
fn assign_missing_call_ids_and_pair(contents: &mut [Value], tool_id_recovery: bool) {
    let mut seq: usize = 0;
    let mut pending_by_name: HashMap<String, Vec<String>> = HashMap::new();
    // Explicit provider response IDs are authoritative: a call that is already
    // answered must not be paired a second time with another same-name response.
    let mut known_response_ids: HashSet<String> = HashSet::new();
    for msg in contents.iter() {
        let Some(parts) = msg.get("parts").and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            if let Some(id) = part
                .get("functionResponse")
                .and_then(|fr| fr.get("id"))
                .and_then(Value::as_str)
                && !id.trim().is_empty()
            {
                known_response_ids.insert(id.trim().to_owned());
            }
        }
    }
    let mut used_ids: HashSet<String> = known_response_ids.iter().cloned().collect();

    for msg in contents.iter_mut() {
        let role = msg
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(parts) = msg.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };

        for part in parts.iter_mut() {
            if role == "model"
                && let Some(fc) = part.get_mut("functionCall").and_then(Value::as_object_mut)
            {
                let name = fc
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_owned();
                let existing_id = fc
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let id = if let Some(existing) = existing_id {
                    used_ids.insert(existing.clone());
                    existing
                } else {
                    let generated = loop {
                        seq += 1;
                        let candidate = format!("ag_call_{seq}");
                        if !used_ids.contains(&candidate) {
                            break candidate;
                        }
                    };
                    fc.insert("id".to_owned(), Value::String(generated.clone()));
                    used_ids.insert(generated.clone());
                    generated
                };
                if !known_response_ids.contains(&id) {
                    pending_by_name.entry(name).or_default().push(id);
                }
            } else if role == "user"
                && tool_id_recovery
                && let Some(fr) = part
                    .get_mut("functionResponse")
                    .and_then(Value::as_object_mut)
            {
                let name = fr
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_owned();
                let has_id = fr
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty());
                if !has_id {
                    if let Some(queue) = pending_by_name.get_mut(&name) {
                        queue.retain(|id| !known_response_ids.contains(id));
                        if let Some(matched_id) = queue.first().cloned() {
                            queue.remove(0);
                            fr.insert("id".to_owned(), Value::String(matched_id.clone()));
                            known_response_ids.insert(matched_id);
                        }
                    }
                    // A response with no ID and no matching call still needs a
                    // stable unique ID; never alias all orphans to one sentry.
                    if fr
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or_default()
                        .is_empty()
                    {
                        let generated = loop {
                            seq += 1;
                            let candidate = format!("ag_call_recovered_{seq}");
                            if !used_ids.contains(&candidate) {
                                break candidate;
                            }
                        };
                        fr.insert("id".to_owned(), Value::String(generated.clone()));
                        used_ids.insert(generated.clone());
                        known_response_ids.insert(generated);
                    }
                }
            }
        }
    }
}

fn strip_foreign_model_artifacts(contents: &mut [Value], target_family: ModelFamily) {
    for msg in contents.iter_mut() {
        let Some(parts) = msg.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in parts.iter_mut() {
            let Some(obj) = part.as_object_mut() else {
                continue;
            };
            // On Claude targets, reject signatures that are clearly Gemini bypass sentinels.
            if target_family == ModelFamily::Claude
                && obj.get("thought").and_then(Value::as_bool).unwrap_or(false)
                && let Some(sig) = obj.get("thoughtSignature").and_then(Value::as_str)
                && (!is_valid_thought_signature(sig) || sig == "skip_thought_signature_validator")
            {
                obj.remove("thoughtSignature");
            }
        }
    }
}

/// Merge adjacent turns sharing the same `role` (`"user"` + `"user"` or `"model"` + `"model"`).
pub fn merge_adjacent_same_role_turns(contents: &mut Vec<Value>) {
    let mut merged: Vec<Value> = Vec::with_capacity(contents.len());
    for turn in contents.drain(..) {
        let role = turn.get("role").and_then(Value::as_str).unwrap_or_default();
        if let Some(last) = merged.last_mut()
            && last.get("role").and_then(Value::as_str) == Some(role)
            && let Some(new_parts) = turn.get("parts").and_then(Value::as_array)
            && let Some(last_parts) = last.get_mut("parts").and_then(Value::as_array_mut)
        {
            last_parts.extend(new_parts.iter().cloned());
            continue;
        }
        merged.push(turn);
    }
    *contents = merged;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pairs_missing_function_call_ids_and_merges_adjacent_turns() {
        let mut contents = vec![
            json!({ "role": "user", "parts": [{ "text": "one" }] }),
            json!({ "role": "user", "parts": [{ "text": "two" }] }),
            json!({ "role": "model", "parts": [{ "functionCall": { "name": "read", "args": {} } }] }),
            json!({ "role": "user", "parts": [{ "functionResponse": { "name": "read", "response": { "ok": true } } }] }),
        ];
        sanitize_cross_model_contents(&mut contents, ModelFamily::Claude, true);
        assert_eq!(contents.len(), 3);
        let call_id = contents[1]["parts"][0]["functionCall"]["id"]
            .as_str()
            .expect("call id");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["id"], call_id);
    }

    #[test]
    fn preserves_distinct_provider_ids_for_same_name_calls() {
        let mut contents = vec![
            json!({ "role": "model", "parts": [
                { "functionCall": { "id": "provider_A", "name": "read", "args": {} } },
                { "functionCall": { "id": "provider_B", "name": "read", "args": {} } }
            ] }),
            json!({ "role": "user", "parts": [
                { "functionResponse": { "id": "provider_A", "name": "read", "response": { "a": 1 } } },
                { "functionResponse": { "id": "provider_B", "name": "read", "response": { "b": 2 } } }
            ] }),
        ];
        sanitize_cross_model_contents(&mut contents, ModelFamily::Claude, true);
        assert_eq!(contents[0]["parts"][0]["functionCall"]["id"], "provider_A");
        assert_eq!(contents[0]["parts"][1]["functionCall"]["id"], "provider_B");
        assert_eq!(
            contents[1]["parts"][0]["functionResponse"]["id"],
            "provider_A"
        );
        assert_eq!(
            contents[1]["parts"][1]["functionResponse"]["id"],
            "provider_B"
        );
    }

    #[test]
    fn omitted_ids_are_unique_and_orphan_responses_are_not_aliased() {
        let mut contents = vec![
            json!({ "role": "model", "parts": [
                { "functionCall": { "name": "read", "args": {} } },
                { "functionCall": { "name": "read", "args": {} } }
            ] }),
            json!({ "role": "user", "parts": [
                { "functionResponse": { "name": "read", "response": { "a": 1 } } },
                { "functionResponse": { "name": "read", "response": { "b": 2 } } },
                { "functionResponse": { "name": "read", "response": { "orphan": true } } }
            ] }),
        ];
        sanitize_cross_model_contents(&mut contents, ModelFamily::Claude, true);
        let first = contents[0]["parts"][0]["functionCall"]["id"]
            .as_str()
            .expect("first generated call id")
            .to_owned();
        let second = contents[0]["parts"][1]["functionCall"]["id"]
            .as_str()
            .expect("second generated call id")
            .to_owned();
        assert_ne!(first, second);
        assert_eq!(contents[1]["parts"][0]["functionResponse"]["id"], first);
        assert_eq!(contents[1]["parts"][1]["functionResponse"]["id"], second);
        let orphan = contents[1]["parts"][2]["functionResponse"]["id"]
            .as_str()
            .expect("orphan response id");
        assert_ne!(orphan, first);
        assert_ne!(orphan, second);
        assert!(orphan.starts_with("ag_call_recovered_"));
    }

    #[test]
    fn known_response_id_is_not_handed_to_a_later_same_name_response() {
        let mut contents = vec![
            json!({ "role": "model", "parts": [
                { "functionCall": { "id": "real_1", "name": "read", "args": {} } },
                { "functionCall": { "id": "real_2", "name": "read", "args": {} } }
            ] }),
            json!({ "role": "user", "parts": [
                { "functionResponse": { "id": "real_1", "name": "read", "response": { "a": 1 } } },
                { "functionResponse": { "name": "read", "response": { "b": 2 } } }
            ] }),
        ];
        sanitize_cross_model_contents(&mut contents, ModelFamily::Claude, true);
        assert_eq!(contents[1]["parts"][0]["functionResponse"]["id"], "real_1");
        assert_eq!(contents[1]["parts"][1]["functionResponse"]["id"], "real_2");
    }
}
