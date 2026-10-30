//! Document merging across discovered configuration sources.

use serde_json::{Map, Value};

pub(super) fn merge_documents(documents: &[Value]) -> Value {
    let mut merged = Map::new();
    for document in documents {
        let Some(object) = document.as_object() else {
            continue;
        };
        for (key, value) in object {
            match key.as_str() {
                "permissions" | "policies" => {
                    let target = merged
                        .entry(key.clone())
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let (Some(target), Some(values)) = (target.as_array_mut(), value.as_array())
                    {
                        target.extend(values.iter().cloned());
                    } else {
                        merged.insert(key.clone(), value.clone());
                    }
                }
                "agents" | "providers" | "commands" => {
                    let target = merged
                        .entry(key.clone())
                        .or_insert_with(|| Value::Object(Map::new()));
                    if let (Some(target), Some(values)) =
                        (target.as_object_mut(), value.as_object())
                    {
                        for (name, item) in values {
                            target.insert(name.clone(), item.clone());
                        }
                    } else {
                        merged.insert(key.clone(), value.clone());
                    }
                }
                "mcp" => merge_mcp(&mut merged, value),
                _ => {
                    merged.insert(key.clone(), value.clone());
                }
            }
        }
    }
    Value::Object(merged)
}

fn merge_mcp(merged: &mut Map<String, Value>, value: &Value) {
    let Some(source) = value.as_object() else {
        merged.insert("mcp".to_owned(), value.clone());
        return;
    };
    let target = merged
        .entry("mcp".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let Some(target) = target.as_object_mut() else {
        return;
    };
    for (key, item) in source {
        if key == "servers" && item.is_object() {
            let servers = target
                .entry(key.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if !servers.is_object() {
                *servers = Value::Object(Map::new());
            }
            if let Some(servers) = servers.as_object_mut()
                && let Some(items) = item.as_object()
            {
                for (name, server) in items {
                    servers.insert(name.clone(), server.clone());
                }
            }
        } else {
            target.insert(key.clone(), item.clone());
        }
    }
}
