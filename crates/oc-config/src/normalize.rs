//! Canonicalization of one configuration document, including V1 aliases.
//!
//! The output keeps unknown keys (docs/14: "Unknown keys in a config document
//! are preserved and ignored, never rejected") and drops keys that the V1
//! format retired, recording a warning for each.

use serde_json::{Map, Value};

use crate::diagnostic::{Diagnostic, DiagnosticKind};

pub(crate) struct Ctx<'a> {
    pub source: Option<&'a str>,
    pub diagnostics: Vec<Diagnostic>,
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(source: Option<&'a str>) -> Self {
        Self {
            source,
            diagnostics: Vec::new(),
        }
    }

    fn push(&mut self, kind: DiagnosticKind, path: String, message: impl Into<String>) {
        self.diagnostics
            .push(Diagnostic::new(self.source, kind, path, message));
    }

    fn invalid(&mut self, path: &str) {
        self.push(
            DiagnosticKind::Invalid,
            path.to_owned(),
            "skipped malformed recognized value",
        );
    }

    fn unsupported(&mut self, path: &str, message: &str) {
        self.push(DiagnosticKind::Unsupported, path.to_owned(), message);
    }

    fn conflict(&mut self, path: &str) {
        self.push(
            DiagnosticKind::Conflict,
            path.to_owned(),
            "retained native value over legacy value",
        );
    }
}

const DROPPED_TOP: [&str; 4] = ["logLevel", "server", "layout", "subagent_depth"];
const DROPPED_EXPERIMENTAL: [&str; 5] = [
    "disable_paste_summary",
    "batch_tool",
    "openTelemetry",
    "primary_tools",
    "continue_loop_on_deny",
];

const CONSUMED_EXPERIMENTAL: [&str; 3] = ["policies", "mcp_timeout", "subagent_depth"];

const HANDLED: [&str; 52] = [
    "$schema",
    "shell",
    "model",
    "default_agent",
    "update",
    "share",
    "enterprise",
    "username",
    "timezone",
    "permissions",
    "policies",
    "agents",
    "snapshots",
    "watcher",
    "formatter",
    "lsp",
    "media",
    "tool_output",
    "mcp",
    "compaction",
    "skills",
    "commands",
    "instructions",
    "references",
    "websearch",
    "plugins",
    "worktree",
    "warming",
    "providers",
    "experimental",
    "provider",
    "agent",
    "mode",
    "plugin",
    "permission",
    "tools",
    "command",
    "reference",
    "attachment",
    "snapshot",
    "autoupdate",
    "autoshare",
    "small_model",
    "enabled_providers",
    "disabled_providers",
    "subagents",
    "observer",
    "goal",
    "memory",
    "lsp_install",
    "lanes",
    "events",
];

/// Normalize one parsed document. Returns `None` when the root cannot be used.
pub(crate) fn normalize(source: Option<&str>, input: Value) -> (Option<Value>, Vec<Diagnostic>) {
    let Value::Object(input) = input else {
        let mut ctx = Ctx::new(source);
        ctx.push(
            DiagnosticKind::Invalid,
            "$".to_owned(),
            "rejected configuration because its root is not an object",
        );
        return (None, ctx.diagnostics);
    };
    let mut ctx = Ctx::new(source);
    let mut out = Map::new();

    for key in DROPPED_TOP {
        if input.contains_key(key) {
            ctx.unsupported(&jp("$", key), "omitted unsupported legacy setting");
        }
    }

    copy_string(&input, "shell", &mut out, &mut ctx);
    if let Some(value) = input
        .get("model")
        .and_then(|value| normalize_model(value, &mut ctx, "$.model"))
    {
        out.insert("model".to_owned(), value);
    }
    copy_string(&input, "default_agent", &mut out, &mut ctx);
    normalize_update(&input, &mut out, &mut ctx);
    normalize_share(&input, &mut out, &mut ctx);

    copy_object(&input, "enterprise", &mut out, &mut ctx);
    copy_string(&input, "username", &mut out, &mut ctx);
    copy_string(&input, "timezone", &mut out, &mut ctx);

    normalize_permissions(&input, &mut out, &mut ctx);
    normalize_policies(&input, &mut out, &mut ctx);
    normalize_agents(&input, &mut out, &mut ctx);

    copy_bool(&input, "snapshots", &mut out, &mut ctx);
    if input.contains_key("snapshots") {
        // handled above
    } else if let Some(value) = input.get("snapshot") {
        match value.as_bool() {
            Some(value) => {
                out.insert("snapshots".to_owned(), Value::Bool(value));
            }
            None => ctx.invalid("$.snapshot"),
        }
    }

    copy_object(&input, "watcher", &mut out, &mut ctx);
    copy_bool_or_object(&input, "formatter", &mut out, &mut ctx);
    copy_bool_or_object(&input, "lsp", &mut out, &mut ctx);
    normalize_media(&input, &mut out, &mut ctx);
    copy_object(&input, "tool_output", &mut out, &mut ctx);
    normalize_mcp(&input, &mut out, &mut ctx);
    normalize_compaction(&input, &mut out, &mut ctx);
    normalize_skills(&input, &mut out, &mut ctx);
    normalize_commands(&input, &mut out, &mut ctx);
    normalize_string_array(&input, "instructions", &mut out, &mut ctx);
    normalize_references(&input, &mut out, &mut ctx);
    copy_bool_or_object(&input, "websearch", &mut out, &mut ctx);
    normalize_plugins(&input, &mut out, &mut ctx);
    copy_object(&input, "worktree", &mut out, &mut ctx);
    copy_bool_or_object(&input, "warming", &mut out, &mut ctx);
    normalize_providers(&input, &mut out, &mut ctx);
    normalize_experimental(&input, &mut out, &mut ctx);

    for key in ["subagents", "observer", "goal", "memory", "lanes", "events"] {
        copy_object(&input, key, &mut out, &mut ctx);
    }
    copy_bool(&input, "lsp_install", &mut out, &mut ctx);

    // Unknown keys are preserved and ignored.
    for (key, value) in &input {
        if HANDLED.contains(&key.as_str()) || DROPPED_TOP.contains(&key.as_str()) {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }

    (Some(Value::Object(out)), ctx.diagnostics)
}

fn jp(base: &str, key: &str) -> String {
    format!("{base}.{key}")
}

fn as_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

fn copy_string(input: &Map<String, Value>, key: &str, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    if let Some(value) = input.get(key) {
        match value.as_str() {
            Some(value) => {
                out.insert(key.to_owned(), Value::String(value.to_owned()));
            }
            None => ctx.invalid(&jp("$", key)),
        }
    }
}

fn copy_bool(input: &Map<String, Value>, key: &str, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    if let Some(value) = input.get(key) {
        match value.as_bool() {
            Some(value) => {
                out.insert(key.to_owned(), Value::Bool(value));
            }
            None => ctx.invalid(&jp("$", key)),
        }
    }
}

fn copy_object(input: &Map<String, Value>, key: &str, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    if let Some(value) = input.get(key) {
        match value.as_object() {
            Some(value) => {
                out.insert(key.to_owned(), Value::Object(value.clone()));
            }
            None => ctx.invalid(&jp("$", key)),
        }
    }
}

fn copy_bool_or_object(
    input: &Map<String, Value>,
    key: &str,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    if let Some(value) = input.get(key) {
        if value.is_boolean() || value.is_object() {
            out.insert(key.to_owned(), value.clone());
        } else {
            ctx.invalid(&jp("$", key));
        }
    }
}

fn normalize_string_array(
    input: &Map<String, Value>,
    key: &str,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    if let Some(value) = input.get(key) {
        match value.as_array() {
            Some(values) => {
                let mut result = Vec::new();
                for (index, item) in values.iter().enumerate() {
                    match item.as_str() {
                        Some(item) => result.push(Value::String(item.to_owned())),
                        None => ctx.invalid(&jp(&jp("$", key), &index.to_string())),
                    }
                }
                out.insert(key.to_owned(), Value::Array(result));
            }
            None => ctx.invalid(&jp("$", key)),
        }
    }
}

fn normalize_update(input: &Map<String, Value>, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    if let Some(value) = input.get("update") {
        match value.as_str() {
            Some("disable" | "notify" | "auto") => {
                out.insert("update".to_owned(), value.clone());
                return;
            }
            _ => ctx.invalid("$.update"),
        }
    }
    if let Some(value) = input.get("autoupdate") {
        let migrated = match value {
            Value::Bool(false) => Some("disable"),
            Value::Bool(true) => Some("auto"),
            Value::String(text) if text == "notify" => Some("notify"),
            _ => None,
        };
        match migrated {
            Some(value) => {
                out.insert("update".to_owned(), Value::String(value.to_owned()));
            }
            None => ctx.invalid("$.autoupdate"),
        }
    }
}

fn normalize_share(input: &Map<String, Value>, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    if let Some(value) = input.get("share") {
        match value.as_str() {
            Some("manual" | "auto" | "disabled") => {
                out.insert("share".to_owned(), value.clone());
                return;
            }
            _ => ctx.invalid("$.share"),
        }
    }
    if let Some(value) = input.get("autoshare") {
        match value.as_bool() {
            Some(true) => {
                out.insert("share".to_owned(), Value::String("auto".to_owned()));
            }
            Some(false) => {}
            None => ctx.invalid("$.autoshare"),
        }
    }
}

// ---------------------------------------------------------------------------
// Permissions and policies

fn normalize_permissions(input: &Map<String, Value>, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    let mut rules = Vec::new();
    if let Some(value) = input.get("tools") {
        rules.extend(migrate_tools(value, ctx, "$.tools"));
    }
    if let Some(value) = input.get("permission") {
        rules.extend(migrate_permission_v1(value, ctx, "$.permission"));
    }
    if let Some(value) = input.get("permissions") {
        rules.extend(normalize_rules(value, ctx, "$.permissions"));
    }
    if !rules.is_empty() || input.get("permissions").is_some_and(Value::is_array) {
        out.insert("permissions".to_owned(), Value::Array(rules));
    }
}

fn migrate_tools(value: &Value, ctx: &mut Ctx, base: &str) -> Vec<Value> {
    let Some(entries) = value.as_object() else {
        ctx.invalid(base);
        return Vec::new();
    };
    let mut rules = Vec::new();
    for (action, enabled) in entries {
        let path = jp(base, action);
        match enabled.as_bool() {
            Some(enabled) => rules.push(rule(
                &normalize_action(action),
                "*",
                if enabled { "allow" } else { "deny" },
            )),
            None => ctx.invalid(&path),
        }
    }
    rules
}

fn migrate_permission_v1(value: &Value, ctx: &mut Ctx, base: &str) -> Vec<Value> {
    let mut rules = Vec::new();
    if let Some(effect) = value.as_str() {
        match crate::permissions::Effect::parse(effect) {
            Some(effect) => rules.push(rule("*", "*", effect.as_str())),
            None => ctx.invalid(base),
        }
        return rules;
    }
    let Some(entries) = value.as_object() else {
        ctx.invalid(base);
        return rules;
    };
    for (action, value) in entries {
        let path = jp(base, action);
        if let Some(effect) = value.as_str() {
            match crate::permissions::Effect::parse(effect) {
                Some(effect) => rules.push(rule(&normalize_action(action), "*", effect.as_str())),
                None => ctx.invalid(&path),
            }
            continue;
        }
        let Some(resources) = value.as_object() else {
            ctx.invalid(&path);
            continue;
        };
        for (resource, effect) in resources {
            match effect.as_str().and_then(crate::permissions::Effect::parse) {
                Some(effect) => {
                    rules.push(rule(&normalize_action(action), resource, effect.as_str()));
                }
                None => ctx.invalid(&jp(&path, resource)),
            }
        }
    }
    rules
}

fn normalize_rules(value: &Value, ctx: &mut Ctx, base: &str) -> Vec<Value> {
    let Some(rules) = value.as_array() else {
        ctx.invalid(base);
        return Vec::new();
    };
    let mut result = Vec::new();
    for (index, item) in rules.iter().enumerate() {
        let path = jp(base, &index.to_string());
        let Some(object) = item.as_object() else {
            ctx.invalid(&path);
            continue;
        };
        let action = object.get("action").and_then(Value::as_str);
        let resource = object.get("resource").and_then(Value::as_str);
        let effect = object
            .get("effect")
            .and_then(Value::as_str)
            .and_then(crate::permissions::Effect::parse);
        match (action, resource, effect) {
            (Some(action), Some(resource), Some(effect)) => {
                result.push(rule(action, resource, effect.as_str()));
            }
            _ => ctx.invalid(&path),
        }
    }
    result
}

fn rule(action: &str, resource: &str, effect: &str) -> Value {
    serde_json::json!({
        "action": action,
        "resource": resource,
        "effect": effect,
    })
}

fn normalize_action(action: &str) -> String {
    match action {
        "write" | "patch" | "apply_patch" => "edit".to_owned(),
        "task" => "subagent".to_owned(),
        "bash" => "shell".to_owned(),
        other => other.to_owned(),
    }
}

fn normalize_policies(input: &Map<String, Value>, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    let mut policies = Vec::new();
    if input.contains_key("enabled_providers")
        && let Some(values) =
            provider_list(input.get("enabled_providers"), "enabled_providers", ctx)
    {
        policies.push(policy("provider.use", "*", "deny"));
        for provider in values {
            policies.push(policy("provider.use", &provider_id(&provider), "allow"));
        }
    }
    if let Some(values) = provider_list(input.get("disabled_providers"), "disabled_providers", ctx)
    {
        for provider in values {
            policies.push(policy("provider.use", &provider_id(&provider), "deny"));
        }
    }
    if let Some(experimental) = as_object(input.get("experimental"))
        && let Some(value) = experimental.get("policies")
    {
        policies.extend(normalize_policy_list(value, ctx, "$.experimental.policies"));
    }
    if let Some(value) = input.get("policies") {
        policies.extend(normalize_policy_list(value, ctx, "$.policies"));
    }
    if !policies.is_empty()
        || input.get("policies").is_some_and(Value::is_array)
        || as_object(input.get("experimental"))
            .and_then(|experimental| experimental.get("policies"))
            .is_some_and(Value::is_array)
    {
        out.insert("policies".to_owned(), Value::Array(policies));
    }
}

fn provider_list(value: Option<&Value>, key: &str, ctx: &mut Ctx) -> Option<Vec<String>> {
    let value = value?;
    let base = jp("$", key);
    let Some(values) = value.as_array() else {
        ctx.invalid(&base);
        return None;
    };
    let mut result = Vec::new();
    for (index, item) in values.iter().enumerate() {
        match item.as_str() {
            Some(item) => result.push(item.to_owned()),
            None => ctx.invalid(&jp(&base, &index.to_string())),
        }
    }
    Some(result)
}

fn normalize_policy_list(value: &Value, ctx: &mut Ctx, base: &str) -> Vec<Value> {
    let Some(values) = value.as_array() else {
        ctx.invalid(base);
        return Vec::new();
    };
    let mut result = Vec::new();
    for (index, item) in values.iter().enumerate() {
        let path = jp(base, &index.to_string());
        let Some(object) = item.as_object() else {
            ctx.invalid(&path);
            continue;
        };
        let action = object.get("action").and_then(Value::as_str);
        let resource = object.get("resource").and_then(Value::as_str);
        let effect = object.get("effect").and_then(Value::as_str);
        if matches!(
            action,
            Some("provider.use" | "tool.use" | "integration.use")
        ) && resource.is_some()
            && matches!(effect, Some("allow" | "deny"))
        {
            result.push(policy(
                action.unwrap_or_default(),
                resource.unwrap_or_default(),
                effect.unwrap_or_default(),
            ));
        } else {
            ctx.invalid(&path);
        }
    }
    result
}

fn policy(action: &str, resource: &str, effect: &str) -> Value {
    serde_json::json!({ "action": action, "resource": resource, "effect": effect })
}

// ---------------------------------------------------------------------------
// Model selections

fn provider_id(provider: &str) -> String {
    match provider {
        "azure-cognitive-services" => "azure".to_owned(),
        "google-vertex-anthropic" => "google-vertex".to_owned(),
        other => other.to_owned(),
    }
}

fn parse_model_ref(text: &str) -> Option<(String, String, Option<String>)> {
    let (reference, variant) = match text.split_once('#') {
        Some((reference, variant)) => {
            if variant.is_empty() || variant.contains('#') {
                return None;
            }
            (reference, Some(variant.to_owned()))
        }
        None => (text, None),
    };
    let (provider, model) = reference.split_once('/')?;
    if provider.is_empty()
        || provider.contains('/')
        || provider.contains('#')
        || model.is_empty()
        || model.contains('#')
    {
        return None;
    }
    Some((provider_id(provider), model.to_owned(), variant))
}

fn model_value(provider: &str, model: &str, variant: Option<String>) -> Value {
    let mut map = Map::new();
    map.insert("providerID".to_owned(), Value::String(provider.to_owned()));
    map.insert("model".to_owned(), Value::String(model.to_owned()));
    if let Some(variant) = variant {
        map.insert("variant".to_owned(), Value::String(variant));
    }
    Value::Object(map)
}

fn normalize_model(value: &Value, ctx: &mut Ctx, base: &str) -> Option<Value> {
    match value {
        Value::String(text) => {
            if let Some((provider, model, variant)) = parse_model_ref(text) {
                Some(model_value(&provider, &model, variant))
            } else {
                ctx.invalid(base);
                None
            }
        }
        Value::Object(object) => {
            let provider = object.get("providerID").and_then(Value::as_str);
            let model = object.get("model").and_then(Value::as_str);
            let (Some(provider), Some(model)) = (provider, model) else {
                ctx.invalid(base);
                return None;
            };
            if provider.is_empty()
                || provider.contains('/')
                || provider.contains('#')
                || model.is_empty()
                || model.contains('#')
            {
                ctx.invalid(base);
                return None;
            }
            let variant = match object.get("variant") {
                None => None,
                Some(Value::String(text)) if !text.is_empty() && !text.contains('#') => {
                    Some(text.clone())
                }
                Some(_) => {
                    ctx.invalid(&jp(base, "variant"));
                    return None;
                }
            };
            Some(model_value(&provider_id(provider), model, variant))
        }
        _ => {
            ctx.invalid(base);
            None
        }
    }
}

fn migrate_model_v1(model: &str, variant: Option<&str>) -> Option<Value> {
    let (provider, model, embedded) = parse_model_ref(model)?;
    if embedded.is_some() {
        return None;
    }
    let variant = variant.and_then(|variant| {
        if variant.is_empty() || variant.contains('#') {
            None
        } else {
            Some(variant.to_owned())
        }
    });
    Some(model_value(&provider, &model, variant))
}

#[must_use]
pub(crate) fn model_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => parse_model_ref(text).map(|(provider, model, variant)| {
            variant.map_or_else(
                || format!("{provider}/{model}"),
                |variant| format!("{provider}/{model}#{variant}"),
            )
        }),
        Value::Object(object) => {
            let provider = object.get("providerID")?.as_str()?;
            let model = object.get("model")?.as_str()?;
            let variant = object.get("variant").and_then(Value::as_str);
            Some(variant.map_or_else(
                || format!("{provider}/{model}"),
                |variant| format!("{provider}/{model}#{variant}"),
            ))
        }
        _ => None,
    }
}

/// Convert a `provider/model[#variant]` reference or a
/// `{providerID, model, variant?}` object into the normalized model selection.
/// Invalid input yields `None`; callers without a diagnostic context (markdown
/// frontmatter) report it themselves.
pub(crate) fn model_selection(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => {
            let (provider, model, variant) = parse_model_ref(text)?;
            Some(model_value(&provider, &model, variant))
        }
        Value::Object(object) => {
            let provider = object.get("providerID")?.as_str()?;
            let model = object.get("model")?.as_str()?;
            if provider.is_empty()
                || provider.contains('/')
                || provider.contains('#')
                || model.is_empty()
                || model.contains('/')
                || model.contains('#')
            {
                return None;
            }
            let variant = match object.get("variant") {
                None => None,
                Some(Value::String(text)) if !text.is_empty() && !text.contains('#') => {
                    Some(text.clone())
                }
                Some(_) => return None,
            };
            Some(model_value(&provider_id(provider), model, variant))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Agents and commands

fn normalize_agents(input: &Map<String, Value>, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    let mut agents = Map::new();
    if let Some(value) = input.get("agent") {
        match value.as_object() {
            Some(entries) => {
                for (id, item) in entries {
                    let path = jp("$.agent", id);
                    if let Some(agent) = migrate_agent_v1(item, None, ctx, &path) {
                        agents.insert(id.clone(), agent);
                    }
                }
            }
            None => ctx.invalid("$.agent"),
        }
    }
    if let Some(value) = input.get("mode") {
        match value.as_object() {
            Some(entries) => {
                for (id, item) in entries {
                    let path = jp("$.mode", id);
                    if let Some(agent) = migrate_agent_v1(item, Some("primary"), ctx, &path) {
                        if agents.contains_key(id) {
                            ctx.conflict(&jp("$.agents", id));
                        }
                        agents.insert(id.clone(), agent);
                    }
                }
            }
            None => ctx.invalid("$.mode"),
        }
    }
    if let Some(value) = input.get("small_model") {
        match value.as_str().and_then(parse_model_ref) {
            Some((provider, model, variant)) => {
                let mut title = agents
                    .remove("title")
                    .and_then(|value| value.as_object().cloned())
                    .unwrap_or_default();
                if !title.contains_key("model") {
                    title.insert("model".to_owned(), model_value(&provider, &model, variant));
                }
                agents.insert("title".to_owned(), Value::Object(title));
            }
            None => ctx.unsupported(
                "$.small_model",
                "omitted unsupported legacy model reference",
            ),
        }
    }
    if let Some(value) = input.get("agents") {
        match value.as_object() {
            Some(entries) => {
                for (id, item) in entries {
                    let path = jp("$.agents", id);
                    if let Some(agent) = normalize_agent_v2(item, ctx, &path) {
                        if agents.contains_key(id) {
                            ctx.conflict(&path);
                        }
                        agents.insert(id.clone(), agent);
                    }
                }
            }
            None => ctx.invalid("$.agents"),
        }
    }
    if !agents.is_empty() || input.get("agents").is_some_and(Value::is_object) {
        out.insert("agents".to_owned(), Value::Object(agents));
    }
}

fn migrate_agent_v1(
    value: &Value,
    forced_mode: Option<&str>,
    ctx: &mut Ctx,
    base: &str,
) -> Option<Value> {
    let object = value.as_object().or_else(|| {
        ctx.invalid(base);
        None
    })?;
    let mut out = Map::new();

    if let Some(model) = object.get("model") {
        match model.as_str() {
            Some(model) => {
                let variant = object.get("variant").and_then(Value::as_str);
                match migrate_model_v1(model, variant) {
                    Some(selection) => {
                        out.insert("model".to_owned(), selection);
                    }
                    None => ctx.unsupported(
                        &jp(base, "model"),
                        "omitted unsupported legacy model reference",
                    ),
                }
            }
            None => ctx.unsupported(
                &jp(base, "model"),
                "omitted unsupported legacy model reference",
            ),
        }
    }

    let mut body = Map::new();
    if let Some(options) = as_object(object.get("options")) {
        for (key, value) in options {
            body.insert(key.clone(), value.clone());
        }
    }
    for key in ["temperature", "top_p"] {
        if let Some(value) = object.get(key) {
            if value.is_number() {
                body.insert(key.to_owned(), value.clone());
            } else {
                ctx.invalid(&jp(base, key));
            }
        }
    }
    if !body.is_empty() {
        out.insert(
            "request".to_owned(),
            serde_json::json!({ "body": Value::Object(body) }),
        );
    }

    if let Some(value) = object.get("prompt").and_then(Value::as_str) {
        out.insert("system".to_owned(), Value::String(value.to_owned()));
    }
    if let Some(value) = object.get("description").and_then(Value::as_str) {
        out.insert("description".to_owned(), Value::String(value.to_owned()));
    }
    if let Some(mode) = forced_mode.or_else(|| object.get("mode").and_then(Value::as_str)) {
        if matches!(mode, "subagent" | "primary" | "all") {
            out.insert("mode".to_owned(), Value::String(mode.to_owned()));
        } else {
            ctx.invalid(&jp(base, "mode"));
        }
    }
    if let Some(value) = object.get("hidden").and_then(Value::as_bool) {
        out.insert("hidden".to_owned(), Value::Bool(value));
    }
    if let Some(value) = object.get("color") {
        match value.as_str() {
            Some(color) if is_hex_color(color) => {
                out.insert("color".to_owned(), Value::String(color.to_owned()));
            }
            Some(_) => {
                // V1 theme names fall back to a neutral color upstream.
                out.insert("color".to_owned(), Value::String("#aaaaaa".to_owned()));
            }
            None => ctx.invalid(&jp(base, "color")),
        }
    }
    let steps = object
        .get("steps")
        .or_else(|| object.get("maxSteps"))
        .and_then(Value::as_u64)
        .filter(|steps| *steps > 0);
    if let Some(steps) = steps {
        out.insert("steps".to_owned(), Value::from(steps));
    }
    if let Some(value) = object.get("disable").and_then(Value::as_bool) {
        out.insert("disabled".to_owned(), Value::Bool(value));
    }

    let mut permissions = Vec::new();
    if let Some(value) = object.get("tools") {
        permissions.extend(migrate_tools(value, ctx, &jp(base, "tools")));
    }
    if let Some(value) = object.get("permission") {
        permissions.extend(migrate_permission_v1(value, ctx, &jp(base, "permission")));
    }
    if !permissions.is_empty() {
        out.insert("permissions".to_owned(), Value::Array(permissions));
    }

    Some(Value::Object(out))
}

#[allow(clippy::too_many_lines)] // Field-by-field mapping keeps the accepted agent keys auditable in one place.
fn normalize_agent_v2(value: &Value, ctx: &mut Ctx, base: &str) -> Option<Value> {
    let object = value.as_object().or_else(|| {
        ctx.invalid(base);
        None
    })?;
    let mut out = Map::new();
    for (key, value) in object {
        let known = matches!(
            key.as_str(),
            "model"
                | "request"
                | "system"
                | "description"
                | "mode"
                | "hidden"
                | "color"
                | "steps"
                | "disabled"
                | "permissions"
        );
        if !known {
            out.insert(key.clone(), value.clone());
        }
    }
    if let Some(value) = object.get("model")
        && let Some(selection) = normalize_model(value, ctx, &jp(base, "model"))
    {
        out.insert("model".to_owned(), selection);
    }
    if let Some(value) = object.get("request") {
        match value.as_object() {
            Some(request) => {
                let mut normalized = Map::new();
                if let Some(headers) = request.get("headers") {
                    match headers.as_object() {
                        Some(headers) if headers.values().all(Value::is_string) => {
                            normalized.insert("headers".to_owned(), Value::Object(headers.clone()));
                        }
                        _ => ctx.invalid(&jp(&jp(base, "request"), "headers")),
                    }
                }
                if let Some(body) = request.get("body") {
                    match body.as_object() {
                        Some(body) => {
                            normalized.insert("body".to_owned(), Value::Object(body.clone()));
                        }
                        None => ctx.invalid(&jp(&jp(base, "request"), "body")),
                    }
                }
                out.insert("request".to_owned(), Value::Object(normalized));
            }
            None => ctx.invalid(&jp(base, "request")),
        }
    }
    for key in ["system", "description"] {
        if let Some(value) = object.get(key) {
            match value.as_str() {
                Some(value) => {
                    out.insert(key.to_owned(), Value::String(value.to_owned()));
                }
                None => ctx.invalid(&jp(base, key)),
            }
        }
    }
    if let Some(value) = object.get("mode") {
        match value.as_str() {
            Some(mode @ ("subagent" | "primary" | "all")) => {
                out.insert("mode".to_owned(), Value::String(mode.to_owned()));
            }
            _ => ctx.invalid(&jp(base, "mode")),
        }
    }
    if let Some(value) = object.get("hidden") {
        match value.as_bool() {
            Some(value) => {
                out.insert("hidden".to_owned(), Value::Bool(value));
            }
            None => ctx.invalid(&jp(base, "hidden")),
        }
    }
    if let Some(value) = object.get("color") {
        match value.as_str() {
            Some(color) if is_hex_color(color) => {
                out.insert("color".to_owned(), Value::String(color.to_owned()));
            }
            _ => ctx.invalid(&jp(base, "color")),
        }
    }
    if let Some(value) = object.get("steps") {
        match value.as_u64().filter(|steps| *steps > 0) {
            Some(steps) => {
                out.insert("steps".to_owned(), Value::from(steps));
            }
            None => ctx.invalid(&jp(base, "steps")),
        }
    }
    if let Some(value) = object.get("disabled") {
        match value.as_bool() {
            Some(value) => {
                out.insert("disabled".to_owned(), Value::Bool(value));
            }
            None => ctx.invalid(&jp(base, "disabled")),
        }
    }
    if let Some(value) = object.get("permissions") {
        let rules = normalize_rules(value, ctx, &jp(base, "permissions"));
        out.insert("permissions".to_owned(), Value::Array(rules));
    }
    Some(Value::Object(out))
}

fn is_hex_color(value: &str) -> bool {
    value.len() == 7 && value.starts_with('#') && value[1..].chars().all(|c| c.is_ascii_hexdigit())
}

fn normalize_commands(input: &Map<String, Value>, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    let mut commands = Map::new();
    if let Some(value) = input.get("command") {
        match value.as_object() {
            Some(entries) => {
                for (name, item) in entries {
                    let path = jp("$.command", name);
                    if let Some(command) = migrate_command_v1(item, ctx, &path) {
                        commands.insert(name.clone(), command);
                    }
                }
            }
            None => ctx.invalid("$.command"),
        }
    }
    if let Some(value) = input.get("commands") {
        match value.as_object() {
            Some(entries) => {
                for (name, item) in entries {
                    let path = jp("$.commands", name);
                    if let Some(command) = normalize_command_v2(item, ctx, &path) {
                        if commands.contains_key(name) {
                            ctx.conflict(&path);
                        }
                        commands.insert(name.clone(), command);
                    }
                }
            }
            None => ctx.invalid("$.commands"),
        }
    }
    if !commands.is_empty() || input.get("commands").is_some_and(Value::is_object) {
        out.insert("commands".to_owned(), Value::Object(commands));
    }
}

fn migrate_command_v1(value: &Value, ctx: &mut Ctx, base: &str) -> Option<Value> {
    let object = value.as_object().or_else(|| {
        ctx.invalid(base);
        None
    })?;
    let template = object.get("template").and_then(Value::as_str).or_else(|| {
        ctx.invalid(&jp(base, "template"));
        None
    })?;
    let mut out = Map::new();
    out.insert("template".to_owned(), Value::String(template.to_owned()));
    if let Some(value) = object.get("description").and_then(Value::as_str) {
        out.insert("description".to_owned(), Value::String(value.to_owned()));
    }
    if let Some(value) = object.get("agent").and_then(Value::as_str) {
        out.insert("agent".to_owned(), Value::String(value.to_owned()));
    }
    if let Some(value) = object.get("model") {
        match value.as_str() {
            Some(model) => {
                let variant = object.get("variant").and_then(Value::as_str);
                if let Some(selection) = migrate_model_v1(model, variant) {
                    out.insert("model".to_owned(), selection);
                } else {
                    ctx.unsupported(
                        &jp(base, "model"),
                        "omitted unsupported legacy model reference",
                    );
                }
            }
            None => ctx.unsupported(
                &jp(base, "model"),
                "omitted unsupported legacy model reference",
            ),
        }
    }
    if let Some(value) = object.get("subtask").and_then(Value::as_bool) {
        out.insert("subagent".to_owned(), Value::Bool(value));
    }
    Some(Value::Object(out))
}

fn normalize_command_v2(value: &Value, ctx: &mut Ctx, base: &str) -> Option<Value> {
    let object = value.as_object().or_else(|| {
        ctx.invalid(base);
        None
    })?;
    let template = object.get("template").and_then(Value::as_str).or_else(|| {
        ctx.invalid(&jp(base, "template"));
        None
    })?;
    let mut out = Map::new();
    out.insert("template".to_owned(), Value::String(template.to_owned()));
    for key in ["description", "agent"] {
        if let Some(value) = object.get(key) {
            match value.as_str() {
                Some(value) => {
                    out.insert(key.to_owned(), Value::String(value.to_owned()));
                }
                None => ctx.invalid(&jp(base, key)),
            }
        }
    }
    if let Some(value) = object.get("model")
        && let Some(selection) = normalize_model(value, ctx, &jp(base, "model"))
    {
        out.insert("model".to_owned(), selection);
    }
    for key in ["subagent", "subtask"] {
        if let Some(value) = object.get(key) {
            match value.as_bool() {
                Some(value) => {
                    out.insert(key.to_owned(), Value::Bool(value));
                }
                None => ctx.invalid(&jp(base, key)),
            }
        }
    }
    Some(Value::Object(out))
}

// ---------------------------------------------------------------------------
// MCP, compaction, references, media, skills, plugins

fn normalize_references(input: &Map<String, Value>, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    let mut references = Map::new();
    for (key, base) in [("reference", "$.reference"), ("references", "$.references")] {
        let Some(value) = input.get(key) else {
            continue;
        };
        let Some(entries) = value.as_object() else {
            ctx.invalid(base);
            continue;
        };
        for (name, item) in entries {
            let path = jp(base, name);
            if item.is_string() || item.is_object() {
                if references.contains_key(name) {
                    ctx.conflict(&jp("$.references", name));
                }
                references.insert(name.clone(), item.clone());
            } else {
                ctx.invalid(&path);
            }
        }
    }
    if !references.is_empty() || input.get("references").is_some_and(Value::is_object) {
        out.insert("references".to_owned(), Value::Object(references));
    }
}

fn normalize_media(input: &Map<String, Value>, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    for key in ["media", "attachment"] {
        let Some(value) = input.get(key) else {
            continue;
        };
        match value.as_object() {
            Some(object) => {
                out.insert("media".to_owned(), Value::Object(object.clone()));
                return;
            }
            None => {
                ctx.invalid(&jp("$", key));
            }
        }
    }
}

fn normalize_skills(input: &Map<String, Value>, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    let Some(value) = input.get("skills") else {
        return;
    };
    if let Some(values) = value.as_array() {
        let mut result = Vec::new();
        for (index, item) in values.iter().enumerate() {
            match item.as_str() {
                Some(item) => result.push(Value::String(item.to_owned())),
                None => ctx.invalid(&jp("$.skills", &index.to_string())),
            }
        }
        out.insert("skills".to_owned(), Value::Array(result));
        return;
    }
    if let Some(object) = value.as_object() {
        let mut result = Vec::new();
        for key in ["paths", "urls"] {
            if let Some(values) = object.get(key) {
                match values.as_array() {
                    Some(values) => {
                        for (index, item) in values.iter().enumerate() {
                            match item.as_str() {
                                Some(item) => result.push(Value::String(item.to_owned())),
                                None => {
                                    ctx.invalid(&jp(&jp("$.skills", key), &index.to_string()));
                                }
                            }
                        }
                    }
                    None => ctx.invalid(&jp("$.skills", key)),
                }
            }
        }
        out.insert("skills".to_owned(), Value::Array(result));
        return;
    }
    ctx.invalid("$.skills");
}

fn normalize_plugins(input: &Map<String, Value>, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    let mut plugins = Vec::new();
    if let Some(value) = input.get("plugin") {
        match value.as_array() {
            Some(entries) => {
                for (index, item) in entries.iter().enumerate() {
                    let path = jp("$.plugin", &index.to_string());
                    match item {
                        Value::String(package) => plugins.push(Value::String(package.clone())),
                        Value::Array(pair) => {
                            let package = pair.first().and_then(Value::as_str);
                            let options = pair.get(1);
                            match (package, options) {
                                (Some(package), Some(options)) => {
                                    let mut entry = Map::new();
                                    entry.insert(
                                        "package".to_owned(),
                                        Value::String(package.to_owned()),
                                    );
                                    entry.insert("options".to_owned(), options.clone());
                                    plugins.push(Value::Object(entry));
                                }
                                (Some(package), None) => {
                                    plugins.push(Value::String(package.to_owned()));
                                }
                                _ => ctx.invalid(&path),
                            }
                        }
                        _ => ctx.invalid(&path),
                    }
                }
            }
            None => ctx.invalid("$.plugin"),
        }
    }
    if let Some(value) = input.get("plugins") {
        match value.as_array() {
            Some(entries) => {
                for (index, item) in entries.iter().enumerate() {
                    let path = jp("$.plugins", &index.to_string());
                    match item {
                        Value::String(package) => plugins.push(Value::String(package.clone())),
                        Value::Object(object) => {
                            if object.get("package").is_some_and(Value::is_string) {
                                plugins.push(item.clone());
                            } else {
                                ctx.invalid(&path);
                            }
                        }
                        _ => ctx.invalid(&path),
                    }
                }
            }
            None => ctx.invalid("$.plugins"),
        }
    }
    if !plugins.is_empty()
        || input.get("plugin").is_some_and(Value::is_array)
        || input.get("plugins").is_some_and(Value::is_array)
    {
        out.insert("plugins".to_owned(), Value::Array(plugins));
    }
}

fn normalize_compaction(input: &Map<String, Value>, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    let Some(value) = input.get("compaction") else {
        return;
    };
    let Some(object) = value.as_object() else {
        ctx.invalid("$.compaction");
        return;
    };
    for key in ["tail_turns", "prune"] {
        if object.contains_key(key) {
            ctx.unsupported(
                &jp("$.compaction", key),
                "omitted unsupported legacy setting",
            );
        }
    }
    let mut result = Map::new();
    if let Some(value) = object.get("auto") {
        match value.as_bool() {
            Some(value) => {
                result.insert("auto".to_owned(), Value::Bool(value));
            }
            None => ctx.invalid("$.compaction.auto"),
        }
    }
    let mut keep_tokens = None;
    if let Some(keep) = object.get("keep").and_then(Value::as_object)
        && let Some(value) = keep.get("tokens")
    {
        match value.as_u64() {
            Some(value) => keep_tokens = Some(value),
            None => ctx.invalid("$.compaction.keep.tokens"),
        }
    }
    if let Some(value) = object.get("preserve_recent_tokens") {
        match value.as_u64() {
            Some(value) => keep_tokens = Some(value),
            None => ctx.invalid("$.compaction.preserve_recent_tokens"),
        }
    }
    if let Some(tokens) = keep_tokens {
        result.insert("keep".to_owned(), serde_json::json!({ "tokens": tokens }));
    }
    let buffer = object.get("buffer").or_else(|| object.get("reserved"));
    if let Some(buffer) = buffer {
        match buffer.as_u64() {
            Some(value) => {
                result.insert("buffer".to_owned(), Value::from(value));
            }
            None => ctx.invalid("$.compaction.buffer"),
        }
    }
    if !result.is_empty() || object.is_empty() {
        out.insert("compaction".to_owned(), Value::Object(result));
    }
}

fn positive(value: &Value) -> Option<u64> {
    value.as_u64().filter(|value| *value > 0)
}

fn normalize_mcp(input: &Map<String, Value>, out: &mut Map<String, Value>, ctx: &mut Ctx) {
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

fn is_direct_legacy_mcp(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|object| object.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "local" || kind == "remote")
}

fn is_enabled_only_mcp(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        !object.contains_key("type") && object.get("enabled").is_some_and(Value::is_boolean)
    })
}

fn normalize_mcp_timeout(
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

fn normalize_server_timeout(object: &mut Map<String, Value>, ctx: &mut Ctx, base: &str) {
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

fn normalize_server(value: &Value, ctx: &mut Ctx, base: &str) -> Option<Value> {
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

fn migrate_server_v1(value: &Value, ctx: &mut Ctx, base: &str) -> Option<Value> {
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

fn normalize_providers(input: &Map<String, Value>, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    let mut providers = Map::new();
    if let Some(value) = input.get("provider") {
        match value.as_object() {
            Some(entries) => {
                for (name, item) in entries {
                    let path = jp("$.provider", name);
                    if let Some(provider) = normalize_provider_v1(item, ctx, &path) {
                        providers.insert(provider_id(name), provider);
                    }
                }
            }
            None => ctx.invalid("$.provider"),
        }
    }
    if let Some(value) = input.get("providers") {
        match value.as_object() {
            Some(entries) => {
                for (name, item) in entries {
                    let path = jp("$.providers", name);
                    match item.as_object() {
                        Some(_) => {
                            if providers.contains_key(name) {
                                ctx.conflict(&path);
                            }
                            providers.insert(name.clone(), item.clone());
                        }
                        None => ctx.invalid(&path),
                    }
                }
            }
            None => ctx.invalid("$.providers"),
        }
    }
    if !providers.is_empty() || input.get("providers").is_some_and(Value::is_object) {
        out.insert("providers".to_owned(), Value::Object(providers));
    }
}

fn normalize_provider_v1(value: &Value, ctx: &mut Ctx, base: &str) -> Option<Value> {
    let mut object = value.as_object().cloned().or_else(|| {
        ctx.invalid(base);
        None
    })?;
    // V1 `npm` is the package specifier; V2 calls it `package`.
    if let Some(value) = object.remove("npm") {
        match value.as_str() {
            Some(package) => {
                object
                    .entry("package".to_owned())
                    .or_insert_with(|| Value::String(package.to_owned()));
            }
            None => ctx.invalid(&jp(base, "npm")),
        }
    }
    // V1 `options` carried request overlays; split them into headers/body/settings.
    if let Some(options) = object.remove("options") {
        let Some(options) = options.as_object() else {
            ctx.invalid(&jp(base, "options"));
            return Some(Value::Object(object));
        };
        if let Some(headers) = options.get("headers") {
            if headers
                .as_object()
                .is_some_and(|headers| headers.values().all(Value::is_string))
            {
                object
                    .entry("headers".to_owned())
                    .or_insert_with(|| headers.clone());
            } else {
                ctx.invalid(&jp(&jp(base, "options"), "headers"));
            }
        }
        if let Some(body) = options.get("body") {
            if body.is_object() {
                object
                    .entry("body".to_owned())
                    .or_insert_with(|| body.clone());
            } else {
                ctx.invalid(&jp(&jp(base, "options"), "body"));
            }
        }
        let mut settings = Map::new();
        for (key, value) in options {
            if key != "headers" && key != "body" {
                settings.insert(key.clone(), value.clone());
            }
        }
        if !settings.is_empty() {
            object
                .entry("settings".to_owned())
                .or_insert_with(|| Value::Object(settings));
        }
    }
    Some(Value::Object(object))
}

fn normalize_experimental(input: &Map<String, Value>, out: &mut Map<String, Value>, ctx: &mut Ctx) {
    let Some(value) = input.get("experimental") else {
        return;
    };
    let Some(object) = value.as_object() else {
        ctx.invalid("$.experimental");
        return;
    };
    let mut result = Map::new();
    for key in DROPPED_EXPERIMENTAL {
        if object.contains_key(key) {
            ctx.unsupported(
                &jp("$.experimental", key),
                "omitted unsupported legacy setting",
            );
        }
    }
    for key in [
        "portable_shell_scanner",
        "todo",
        "background_compaction",
        "media_blob",
        "image_generate",
        "schedules",
    ] {
        if let Some(value) = object.get(key) {
            match value.as_bool() {
                Some(value) => {
                    result.insert(key.to_owned(), Value::Bool(value));
                }
                None => ctx.invalid(&jp("$.experimental", key)),
            }
        }
    }
    for key in ["subagent_depth", "location_idle_timeout"] {
        if let Some(value) = object.get(key) {
            match positive(value) {
                Some(value) => {
                    result.insert(key.to_owned(), Value::from(value));
                }
                None => ctx.invalid(&jp("$.experimental", key)),
            }
        }
    }
    // Unknown experimental keys survive; consumed keys do not.
    for (key, value) in object {
        if result.contains_key(key)
            || CONSUMED_EXPERIMENTAL.contains(&key.as_str())
            || DROPPED_EXPERIMENTAL.contains(&key.as_str())
        {
            continue;
        }
        result.insert(key.clone(), value.clone());
    }
    if !result.is_empty() || object.is_empty() {
        out.insert("experimental".to_owned(), Value::Object(result));
    }
}
