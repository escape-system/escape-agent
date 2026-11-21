//! Claude-over-Antigravity request normalization, schema placeholder injection,
//! `VALIDATED` function calling config, tool hardening, and tool call/result pairing.

use crate::constants::{CLAUDE_DESCRIPTION_PROMPT, CLAUDE_TOOL_SYSTEM_INSTRUCTION};
use crate::schema::clean_json_schema_for_antigravity;
use crate::transform::model_resolver::ThinkingMode;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

/// Apply Claude-specific transformations to a Cloud Code `request` JSON object.
pub fn transform_claude_request(
    request_obj: &mut Map<String, Value>,
    thinking: &ThinkingMode,
    tool_hardening: bool,
    tool_id_recovery: bool,
) {
    transform_claude_tools(request_obj, tool_hardening);
    configure_claude_thinking_and_tokens(request_obj, thinking);
    if tool_id_recovery && let Some(Value::Array(contents)) = request_obj.get_mut("contents") {
        repair_claude_tool_pairing(contents);
    }
}

fn transform_claude_tools(request_obj: &mut Map<String, Value>, tool_hardening: bool) {
    let Some(tools) = request_obj.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    if tools.is_empty() {
        return;
    }

    let mut has_function_declarations = false;
    for tool_entry in tools.iter_mut() {
        if let Some(decls) = tool_entry
            .get_mut("functionDeclarations")
            .and_then(Value::as_array_mut)
        {
            for decl in decls.iter_mut() {
                has_function_declarations = true;
                harden_single_claude_declaration(decl, tool_hardening);
            }
        }
    }

    if !has_function_declarations {
        return;
    }

    // Claude requires VALIDATED mode on Cloud Code to enforce schema compliance.
    let tool_cfg = request_obj
        .entry("toolConfig".to_owned())
        .or_insert_with(|| json!({}));
    if let Some(tc_obj) = tool_cfg.as_object_mut() {
        let fcc = tc_obj
            .entry("functionCallingConfig".to_owned())
            .or_insert_with(|| json!({}));
        if let Some(fcc_obj) = fcc.as_object_mut() {
            fcc_obj
                .entry("mode".to_owned())
                .or_insert_with(|| json!("VALIDATED"));
        }
    }

    if tool_hardening {
        inject_claude_tool_system_instruction(request_obj);
    }
}

fn harden_single_claude_declaration(decl: &mut Value, tool_hardening: bool) {
    let Some(obj) = decl.as_object_mut() else {
        return;
    };

    let raw_schema = obj
        .get("parameters")
        .or_else(|| obj.get("parametersJsonSchema"))
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object" }));
    let cleaned = clean_json_schema_for_antigravity(&raw_schema, true);

    if tool_hardening {
        let param_summary = summarize_schema_parameters(&cleaned);
        if !param_summary.is_empty() {
            let existing = obj
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !existing.contains("STRICT PARAMETERS:") {
                let addendum = CLAUDE_DESCRIPTION_PROMPT.replace("{params}", &param_summary);
                obj.insert(
                    "description".to_owned(),
                    Value::String(format!("{existing}{addendum}")),
                );
            }
        }
    }

    obj.remove("parametersJsonSchema");
    obj.insert("parameters".to_owned(), cleaned);
}

fn summarize_schema_parameters(schema: &Value) -> String {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return String::new();
    };
    let required: HashSet<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    props
        .iter()
        .map(|(name, prop)| {
            let kind = prop.get("type").and_then(Value::as_str).unwrap_or("any");
            if required.contains(name.as_str()) {
                format!("{name}: {kind} (required)")
            } else {
                format!("{name}: {kind}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn inject_claude_tool_system_instruction(request_obj: &mut Map<String, Value>) {
    let sys = request_obj
        .entry("systemInstruction".to_owned())
        .or_insert_with(|| json!({ "role": "user", "parts": [] }));
    let Some(sys_obj) = sys.as_object_mut() else {
        return;
    };
    sys_obj
        .entry("role".to_owned())
        .or_insert_with(|| json!("user"));
    let parts = sys_obj
        .entry("parts".to_owned())
        .or_insert_with(|| json!([]));
    if let Some(arr) = parts.as_array_mut() {
        let already = arr.iter().any(|p| {
            p.get("text")
                .and_then(Value::as_str)
                .is_some_and(|t| t.contains("CRITICAL TOOL USAGE INSTRUCTIONS"))
        });
        if !already {
            arr.push(json!({ "text": CLAUDE_TOOL_SYSTEM_INSTRUCTION }));
        }
    }
}

fn configure_claude_thinking_and_tokens(
    request_obj: &mut Map<String, Value>,
    thinking: &ThinkingMode,
) {
    let gen_cfg = request_obj
        .entry("generationConfig".to_owned())
        .or_insert_with(|| json!({}));
    let Some(gen_obj) = gen_cfg.as_object_mut() else {
        return;
    };

    match thinking {
        ThinkingMode::Budget(budget) if *budget > 0 => {
            gen_obj.insert(
                "thinkingConfig".to_owned(),
                json!({
                    "includeThoughts": true,
                    "thinkingBudget": *budget
                }),
            );
            // Anthropic/Claude requires max_tokens > thinking_budget.
            let min_max = u64::from(*budget).saturating_add(4096);
            let existing = gen_obj
                .get("maxOutputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if existing <= u64::from(*budget) {
                gen_obj.insert("maxOutputTokens".to_owned(), json!(min_max.max(64_000)));
            }
        }
        _ => {
            gen_obj.remove("thinkingConfig");
        }
    }
}

/// Guarantee that every `functionCall` in Claude history is matched by a `functionResponse`
/// with matching ID before any unrelated user message intervenes.
fn repair_claude_tool_pairing(contents: &mut Vec<Value>) {
    let mut seq = 0usize;
    let mut i = 0;

    while i < contents.len() {
        if contents[i].get("role").and_then(Value::as_str) != Some("model") {
            i += 1;
            continue;
        }

        let call_pairs = extract_and_tag_model_calls(&mut contents[i], &mut seq);
        if call_pairs.is_empty() {
            i += 1;
            continue;
        }

        let next_is_user = contents
            .get(i + 1)
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            == Some("user");

        if next_is_user {
            repair_user_turn_responses(&mut contents[i + 1], call_pairs, &mut seq);
        } else {
            contents.insert(i + 1, build_synthetic_error_turn(&call_pairs));
        }
        i += 2;
    }
}

fn extract_and_tag_model_calls(turn: &mut Value, seq: &mut usize) -> Vec<(String, String)> {
    let mut call_pairs = Vec::new();
    let Some(parts) = turn.get_mut("parts").and_then(Value::as_array_mut) else {
        return call_pairs;
    };
    for part in parts {
        let Some(fc) = part.get_mut("functionCall").and_then(Value::as_object_mut) else {
            continue;
        };
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
            existing
        } else {
            *seq += 1;
            let gen_id = format!("toolu_ag_{seq}");
            fc.insert("id".to_owned(), json!(gen_id));
            gen_id
        };
        call_pairs.push((id, name));
    }
    call_pairs
}

fn build_synthetic_error_turn(call_pairs: &[(String, String)]) -> Value {
    let resp_parts: Vec<Value> = call_pairs
        .iter()
        .map(|(id, name)| {
            json!({
                "functionResponse": {
                    "id": id,
                    "name": name,
                    "response": { "error": "Tool call interrupted before returning a result." }
                }
            })
        })
        .collect();
    json!({
        "role": "user",
        "parts": resp_parts
    })
}

fn repair_user_turn_responses(
    turn: &mut Value,
    call_pairs: Vec<(String, String)>,
    seq: &mut usize,
) {
    let mut resp_ids = HashSet::new();
    let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
    for (id, name) in &call_pairs {
        by_name.entry(name.clone()).or_default().push(id.clone());
    }

    let Some(parts) = turn.get_mut("parts").and_then(Value::as_array_mut) else {
        return;
    };

    for part in parts.iter_mut() {
        let Some(fr) = part
            .get_mut("functionResponse")
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        let name = fr
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_owned();
        let existing = fr
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let final_id = if let Some(id) = existing {
            id
        } else {
            let assigned = by_name
                .get_mut(&name)
                .and_then(|q| (!q.is_empty()).then(|| q.remove(0)))
                .unwrap_or_else(|| {
                    *seq += 1;
                    format!("toolu_ag_{seq}")
                });
            fr.insert("id".to_owned(), json!(assigned));
            assigned
        };
        resp_ids.insert(final_id);
    }

    for (id, name) in call_pairs {
        if !resp_ids.contains(&id) {
            parts.push(json!({
                "functionResponse": {
                    "id": id,
                    "name": name,
                    "response": { "error": "Tool call interrupted before returning a result." }
                }
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_validated_mode_placeholder_hardening_and_pairs_orphaned_call() {
        let mut req = json!({
            "contents": [
                { "role": "model", "parts": [{ "functionCall": { "name": "ping", "args": {} } }] },
                { "role": "model", "parts": [{ "text": "next turn" }] }
            ],
            "tools": [{
                "functionDeclarations": [{
                    "name": "ping",
                    "description": "Ping service",
                    "parameters": { "type": "object", "properties": {} }
                }]
            }],
            "generationConfig": { "maxOutputTokens": 4096 }
        });

        let obj = req.as_object_mut().expect("obj");
        transform_claude_request(obj, &ThinkingMode::Budget(32768), true, true);

        assert_eq!(
            obj["toolConfig"]["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
        let decl = &obj["tools"][0]["functionDeclarations"][0];
        assert_eq!(
            decl["parameters"]["properties"]["_placeholder"]["type"],
            "boolean"
        );
        assert!(
            decl["description"]
                .as_str()
                .unwrap_or_default()
                .contains("STRICT PARAMETERS:")
        );
        assert_eq!(obj["generationConfig"]["maxOutputTokens"], 64_000);

        let contents = obj["contents"].as_array().expect("contents");
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[1]["role"], "user");
        assert!(contents[1]["parts"][0]["functionResponse"].is_object());
    }
}
