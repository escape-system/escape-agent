//! Four-phase JSON Schema cleaning and Gemini protobuf schema conversion for Antigravity.

use crate::constants::{EMPTY_SCHEMA_PLACEHOLDER_DESCRIPTION, EMPTY_SCHEMA_PLACEHOLDER_NAME};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

const UNSUPPORTED_KEYS: &[&str] = &[
    "$schema",
    "$id",
    "$anchor",
    "$dynamicRef",
    "$dynamicAnchor",
    "$vocabulary",
    "$comment",
    "additionalProperties",
    "unevaluatedProperties",
    "default",
    "examples",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "patternProperties",
    "propertyNames",
    "dependentRequired",
    "dependentSchemas",
    "if",
    "then",
    "else",
    "not",
    "contentMediaType",
    "contentEncoding",
    "readOnly",
    "writeOnly",
    "deprecated",
    "title",
    "$defs",
    "definitions",
];

const HINT_CONSTRAINTS: &[&str] = &[
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "pattern",
    "format",
    "multipleOf",
    "minItems",
    "maxItems",
    "uniqueItems",
];

/// Clean a JSON Schema for Antigravity / Cloud Code Assist tool declarations (keeps JSON types).
///
/// When `inject_empty_placeholder` is `true`, empty `object` parameter schemas receive
/// `_placeholder: boolean` (required by Claude tool schemas over Antigravity).
#[must_use]
pub fn clean_json_schema_for_antigravity(schema: &Value, inject_empty_placeholder: bool) -> Value {
    let mut defs = HashMap::new();
    collect_defs(schema, &mut defs);

    let mut visited = HashSet::new();
    let mut cleaned = resolve_and_clean(schema, &defs, &mut visited, 0);

    if inject_empty_placeholder {
        ensure_non_empty_object_schema(&mut cleaned);
    }
    sanitize_required_fields(&mut cleaned);
    cleaned
}

/// Convert a JSON Schema (already or freshly cleaned) into Gemini protobuf-style Schema
/// with uppercase `TYPE` enums (`OBJECT`, `STRING`, `NUMBER`, `INTEGER`, `BOOLEAN`, `ARRAY`).
#[must_use]
pub fn to_gemini_schema(schema: &Value) -> Value {
    let mut cleaned = clean_json_schema_for_antigravity(schema, false);
    uppercase_schema_types(&mut cleaned);
    cleaned
}

fn collect_defs(root: &Value, defs: &mut HashMap<String, Value>) {
    if let Some(obj) = root.as_object() {
        for key in ["$defs", "definitions"] {
            if let Some(Value::Object(map)) = obj.get(key) {
                for (name, val) in map {
                    defs.insert(format!("#/{key}/{name}"), val.clone());
                    defs.insert(name.clone(), val.clone());
                }
            }
        }
    }
}

fn resolve_and_clean(
    node: &Value,
    defs: &HashMap<String, Value>,
    visited: &mut HashSet<String>,
    depth: usize,
) -> Value {
    if depth > 24 {
        return json!({ "type": "object" });
    }
    let Some(obj) = node.as_object() else {
        return node.clone();
    };

    if let Some(Value::String(ref_path)) = obj.get("$ref") {
        return resolve_ref_node(obj, ref_path, defs, visited, depth);
    }

    let mut out = Map::new();
    apply_const_and_combinators(obj, &mut out, defs, visited, depth);
    apply_type_field(obj, &mut out);

    let mut hints = Vec::new();
    for (k, v) in obj {
        if k == "type"
            || k == "const"
            || k == "anyOf"
            || k == "oneOf"
            || k == "allOf"
            || UNSUPPORTED_KEYS.contains(&k.as_str())
        {
            continue;
        }
        if HINT_CONSTRAINTS.contains(&k.as_str()) {
            hints.push(format!("{k}={v}"));
            continue;
        }
        clean_property_entry(k, v, &mut out, defs, visited, depth);
    }
    append_constraint_hints(&mut out, &hints);

    Value::Object(out)
}

fn resolve_ref_node(
    obj: &Map<String, Value>,
    ref_path: &str,
    defs: &HashMap<String, Value>,
    visited: &mut HashSet<String>,
    depth: usize,
) -> Value {
    if !visited.insert(ref_path.to_owned()) {
        return json!({ "type": "object", "description": format!("Recursive reference ({ref_path})") });
    }
    let resolved = defs
        .get(ref_path)
        .or_else(|| ref_path.rsplit('/').next().and_then(|k| defs.get(k)))
        .cloned();
    let out = if let Some(target) = resolved {
        let mut base = resolve_and_clean(&target, defs, visited, depth + 1);
        if let Some(desc) = obj.get("description").and_then(Value::as_str)
            && let Some(base_obj) = base.as_object_mut()
        {
            base_obj
                .entry("description".to_owned())
                .or_insert_with(|| Value::String(desc.to_owned()));
        }
        base
    } else {
        json!({ "type": "string" })
    };
    visited.remove(ref_path);
    out
}

fn apply_const_and_combinators(
    obj: &Map<String, Value>,
    out: &mut Map<String, Value>,
    defs: &HashMap<String, Value>,
    visited: &mut HashSet<String>,
    depth: usize,
) {
    if let Some(const_val) = obj.get("const") {
        out.insert("enum".to_owned(), Value::Array(vec![const_val.clone()]));
        let inferred = match const_val {
            Value::Bool(_) => "boolean",
            Value::Number(n) if n.is_i64() => "integer",
            Value::Number(_) => "number",
            _ => "string",
        };
        out.insert("type".to_owned(), Value::String(inferred.to_owned()));
    }

    for union_key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(variants)) = obj.get(union_key)
            && let Some(merged) = flatten_union_variants(variants, defs, visited, depth)
            && let Some(merged_obj) = merged.as_object()
        {
            for (k, v) in merged_obj {
                out.insert(k.clone(), v.clone());
            }
        }
    }

    if let Some(Value::Array(branches)) = obj.get("allOf") {
        for branch in branches {
            let cleaned_branch = resolve_and_clean(branch, defs, visited, depth + 1);
            if let Some(b_obj) = cleaned_branch.as_object() {
                for (k, v) in b_obj {
                    if k == "properties"
                        && let Some(Value::Object(existing_props)) = out.get_mut("properties")
                        && let Some(b_props) = v.as_object()
                    {
                        for (pk, pv) in b_props {
                            existing_props.insert(pk.clone(), pv.clone());
                        }
                        continue;
                    }
                    out.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
    }
}

fn apply_type_field(obj: &Map<String, Value>, out: &mut Map<String, Value>) {
    let Some(type_val) = obj.get("type") else {
        return;
    };
    match type_val {
        Value::String(s) => {
            out.insert("type".to_owned(), Value::String(s.clone()));
        }
        Value::Array(arr) => {
            let first = arr
                .iter()
                .filter_map(Value::as_str)
                .find(|s| *s != "null")
                .unwrap_or("string");
            out.insert("type".to_owned(), Value::String(first.to_owned()));
        }
        _ => {}
    }
}

fn clean_property_entry(
    key: &str,
    val: &Value,
    out: &mut Map<String, Value>,
    defs: &HashMap<String, Value>,
    visited: &mut HashSet<String>,
    depth: usize,
) {
    match key {
        "properties" => {
            if let Some(props) = val.as_object() {
                let mut cleaned_props = Map::new();
                for (pk, pv) in props {
                    cleaned_props
                        .insert(pk.clone(), resolve_and_clean(pv, defs, visited, depth + 1));
                }
                out.insert("properties".to_owned(), Value::Object(cleaned_props));
            }
        }
        "items" => {
            if let Some(arr) = val.as_array() {
                if let Some(first) = arr.first() {
                    out.insert(
                        "items".to_owned(),
                        resolve_and_clean(first, defs, visited, depth + 1),
                    );
                }
            } else {
                out.insert(
                    "items".to_owned(),
                    resolve_and_clean(val, defs, visited, depth + 1),
                );
            }
        }
        "enum" => {
            if let Some(arr) = val.as_array() {
                let converted: Vec<Value> = arr
                    .iter()
                    .map(|item| match item {
                        Value::String(s) => Value::String(s.clone()),
                        other => Value::String(other.to_string()),
                    })
                    .collect();
                // Protobuf/Gemini `enum` values are always strings; keep `type` consistent
                // when upstream schema originally tagged an integer/number enum.
                if !converted.is_empty() {
                    out.insert("type".to_owned(), Value::String("string".to_owned()));
                }
                out.insert("enum".to_owned(), Value::Array(converted));
            }
        }
        _ => {
            out.insert(key.to_owned(), val.clone());
        }
    }
}

fn append_constraint_hints(out: &mut Map<String, Value>, hints: &[String]) {
    if hints.is_empty() {
        return;
    }
    let existing_desc = out
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let hint_str = format!("Constraints: {}", hints.join(", "));
    let combined = if existing_desc.is_empty() {
        hint_str
    } else {
        format!("{existing_desc} ({hint_str})")
    };
    out.insert("description".to_owned(), Value::String(combined));
}

fn flatten_union_variants(
    variants: &[Value],
    defs: &HashMap<String, Value>,
    visited: &mut HashSet<String>,
    depth: usize,
) -> Option<Value> {
    let non_null: Vec<&Value> = variants
        .iter()
        .filter(|v| v.get("type").and_then(Value::as_str) != Some("null"))
        .collect();
    let candidate = non_null.first().copied().or_else(|| variants.first())?;
    let mut cleaned = resolve_and_clean(candidate, defs, visited, depth + 1);
    if non_null.len() > 1
        && let Some(obj) = cleaned.as_object_mut()
    {
        let existing = obj
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let summary = format!("Accepts {} alternative schemas", non_null.len());
        let desc = if existing.is_empty() {
            summary
        } else {
            format!("{existing} ({summary})")
        };
        obj.insert("description".to_owned(), Value::String(desc));
    }
    Some(cleaned)
}

fn ensure_non_empty_object_schema(node: &mut Value) {
    let Some(obj) = node.as_object_mut() else {
        return;
    };
    let is_obj = obj
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.eq_ignore_ascii_case("object"));
    if is_obj {
        let empty_props = obj
            .get("properties")
            .and_then(Value::as_object)
            .is_none_or(Map::is_empty);
        if empty_props {
            obj.insert(
                "properties".to_owned(),
                json!({
                    EMPTY_SCHEMA_PLACEHOLDER_NAME: {
                        "type": "boolean",
                        "description": EMPTY_SCHEMA_PLACEHOLDER_DESCRIPTION
                    }
                }),
            );
            obj.insert(
                "required".to_owned(),
                json!([EMPTY_SCHEMA_PLACEHOLDER_NAME]),
            );
        }
    }
}

fn sanitize_required_fields(node: &mut Value) {
    let Some(obj) = node.as_object_mut() else {
        return;
    };
    if let Some(Value::Object(props)) = obj.get("properties").cloned()
        && let Some(Value::Array(req)) = obj.get_mut("required")
    {
        req.retain(|item| item.as_str().is_some_and(|k| props.contains_key(k)));
        if req.is_empty() {
            obj.remove("required");
        }
    } else if obj
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        obj.remove("required");
    }

    if let Some(Value::Object(props)) = obj.get_mut("properties") {
        for val in props.values_mut() {
            sanitize_required_fields(val);
        }
    }
    if let Some(items) = obj.get_mut("items") {
        sanitize_required_fields(items);
    }
}

fn uppercase_schema_types(node: &mut Value) {
    let Some(obj) = node.as_object_mut() else {
        return;
    };
    if let Some(Value::String(t)) = obj.get_mut("type") {
        let mapped = match t.to_ascii_lowercase().as_str() {
            "object" => "OBJECT",
            "string" => "STRING",
            "number" => "NUMBER",
            "integer" => "INTEGER",
            "boolean" => "BOOLEAN",
            "array" => "ARRAY",
            _ => "TYPE_UNSPECIFIED",
        };
        mapped.clone_into(t);
    }
    if let Some(Value::Object(props)) = obj.get_mut("properties") {
        for val in props.values_mut() {
            uppercase_schema_types(val);
        }
    }
    if let Some(items) = obj.get_mut("items") {
        uppercase_schema_types(items);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_defs_strips_unsupported_and_injects_placeholder() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "$defs": {
                "Target": {
                    "type": ["string", "null"],
                    "minLength": 2
                }
            },
            "properties": {
                "target": { "$ref": "#/$defs/Target" }
            },
            "additionalProperties": false,
            "required": ["target", "missing_field"]
        });

        let cleaned = clean_json_schema_for_antigravity(&schema, false);
        assert!(cleaned.get("$schema").is_none());
        assert!(cleaned.get("additionalProperties").is_none());
        assert_eq!(cleaned["properties"]["target"]["type"], "string");
        assert!(
            cleaned["properties"]["target"]["description"]
                .as_str()
                .unwrap_or_default()
                .contains("minLength=2")
        );
        assert_eq!(cleaned["required"], json!(["target"]));

        let empty = clean_json_schema_for_antigravity(&json!({ "type": "object" }), true);
        assert_eq!(
            empty["properties"][EMPTY_SCHEMA_PLACEHOLDER_NAME]["type"],
            "boolean"
        );

        let gemini = to_gemini_schema(&cleaned);
        assert_eq!(gemini["type"], "OBJECT");
        assert_eq!(gemini["properties"]["target"]["type"], "STRING");
    }
}
