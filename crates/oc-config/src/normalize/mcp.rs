//! MCP server and timeout normalization, including V1 server migration.

use serde_json::{Map, Value};

use super::copy::{as_object, jp};
use super::ctx::Ctx;

pub(super) fn positive(value: &Value) -> Option<u64> {
    value.as_u64().filter(|value| *value > 0)
}

pub(super) fn normalize_mcp(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    let experimental = as_object(input.get("experimental"));
    let experimental_timeout = experimental.and_then(|value| value.get("mcp_timeout"));
    let mcp = input.get("mcp");
    if mcp.is_some_and(|value| !value.is_object()) {
        ctx.invalid("$.mcp");
        return;
    }
    if mcp.is_none() && experimental_timeout.is_none() {
        return;
    }

    let mut timeout = Map::new();
    if let Some(value) = experimental_timeout {
        match positive(value) {
            Some(value) => {
                timeout.insert("catalog".to_owned(), Value::from(value));
                timeout.insert("execution".to_owned(), Value::from(value));
            }
            None => ctx.invalid("$.experimental.mcp_timeout"),
        }
    }

    let mut servers = Map::new();
    if let Some(object) = as_object(mcp) {
        for (name, value) in object {
            let path = jp("$.mcp", name);
            if name == "timeout" && !is_direct_legacy_mcp(value) {
                normalize_mcp_timeout(value, &mut timeout, ctx, &path);
                continue;
            }
            if name == "servers" && !is_direct_legacy_mcp(value) {
                match value.as_object() {
                    Some(entries) => {
                        for (server_name, item) in entries {
                            let path = jp(&path, server_name);
                            if let Some(server) = normalize_server(item, ctx, &path) {
                                servers.insert(server_name.clone(), server);
                            }
                        }
                    }
                    None => ctx.invalid(&path),
                }
                continue;
            }
            if is_enabled_only_mcp(value) {
                ctx.unsupported(&path, "omitted enabled-only legacy MCP entry");
                continue;
            }
            if let Some(server) = migrate_server_v1(value, ctx, &path) {
                servers.insert(name.clone(), server);
            }
        }
    }

    let mut mcp_out = Map::new();
    if !timeout.is_empty() {
        mcp_out.insert("timeout".to_owned(), Value::Object(timeout));
    }
    if !servers.is_empty()
        || as_object(mcp)
            .and_then(|mcp| mcp.get("servers"))
            .is_some_and(Value::is_object)
    {
        mcp_out.insert("servers".to_owned(), Value::Object(servers));
    }
    if !mcp_out.is_empty() || mcp.is_some_and(Value::is_object) {
        out.insert("mcp".to_owned(), Value::Object(mcp_out));
    }
}

pub(super) fn is_direct_legacy_mcp(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|object| object.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "local" || kind == "remote")
}

pub(super) fn is_enabled_only_mcp(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        !object.contains_key("type") && object.get("enabled").is_some_and(Value::is_boolean)
    })
}

pub(super) fn normalize_mcp_timeout(
    value: &Value,
    timeout: &mut Map<String, Value>,
    ctx: &mut Ctx,
    base: &str,
) {
    if let Some(value) = positive(value) {
        for key in ["startup", "catalog", "execution"] {
            timeout.insert(key.to_owned(), Value::from(value));
        }
        return;
    }
    let Some(object) = value.as_object() else {
        ctx.invalid(base);
        return;
    };
    for key in ["startup", "catalog", "execution"] {
        if let Some(value) = object.get(key) {
            match positive(value) {
                Some(value) => {
                    timeout.insert(key.to_owned(), Value::from(value));
                }
                None => ctx.invalid(&jp(base, key)),
            }
        }
    }
}

pub(super) fn normalize_server_timeout(object: &mut Map<String, Value>, ctx: &mut Ctx, base: &str) {
    let Some(value) = object.get("timeout").cloned() else {
        return;
    };
    if value.is_object() {
        return;
    }
    match positive(&value) {
        Some(value) => {
            object.insert(
                "timeout".to_owned(),
                serde_json::json!({ "startup": value, "catalog": value, "execution": value }),
            );
        }
        None => ctx.invalid(&jp(base, "timeout")),
    }
}

pub(super) fn normalize_server(value: &Value, ctx: &mut Ctx, base: &str) -> Option<Value> {
    let mut object = value.as_object().cloned().or_else(|| {
        ctx.invalid(base);
        None
    })?;
    match object.get("type").and_then(Value::as_str) {
        Some("local") => {
            if !object.get("command").is_some_and(|value| {
                value
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_string))
            }) {
                ctx.invalid(base);
                return None;
            }
        }
        Some("remote") => {
            if !object.get("url").is_some_and(Value::is_string) {
                ctx.invalid(base);
                return None;
            }
        }
        _ => {
            ctx.invalid(base);
            return None;
        }
    }
    normalize_server_timeout(&mut object, ctx, base);
    Some(Value::Object(object))
}

pub(super) fn migrate_server_v1(value: &Value, ctx: &mut Ctx, base: &str) -> Option<Value> {
    let mut object = value.as_object().cloned().or_else(|| {
        ctx.invalid(base);
        None
    })?;
    let kind = object.get("type").and_then(Value::as_str)?.to_owned();
    if kind != "local" && kind != "remote" {
        ctx.invalid(base);
        return None;
    }
    if kind == "local"
        && !object.get("command").is_some_and(|value| {
            value
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string))
        })
    {
        ctx.invalid(base);
        return None;
    }
    if kind == "remote" && !object.get("url").is_some_and(Value::is_string) {
        ctx.invalid(base);
        return None;
    }
    if let Some(enabled) = object.remove("enabled") {
        match enabled.as_bool() {
            Some(enabled) => {
                object.insert("disabled".to_owned(), Value::Bool(!enabled));
            }
            None => ctx.invalid(&jp(base, "enabled")),
        }
    }
    if let Some(oauth) = object.remove("oauth") {
        match oauth {
            Value::Bool(false) => {
                object.insert("oauth".to_owned(), Value::Bool(false));
            }
            Value::Object(oauth) => {
                let mut migrated = Map::new();
                for (from, to) in [
                    ("clientId", "client_id"),
                    ("clientSecret", "client_secret"),
                    ("scope", "scope"),
                    ("callbackPort", "callback_port"),
                    ("redirectUri", "redirect_uri"),
                ] {
                    if let Some(value) = oauth.get(from) {
                        migrated.insert(to.to_owned(), value.clone());
                    }
                }
                object.insert("oauth".to_owned(), Value::Object(migrated));
            }
            _ => ctx.invalid(&jp(base, "oauth")),
        }
    }
    normalize_server_timeout(&mut object, ctx, base);
    Some(Value::Object(object))
}
