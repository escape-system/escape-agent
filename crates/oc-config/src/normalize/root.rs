//! Root document dispatch and unknown-key preservation.

use serde_json::{Map, Value};

use super::agents::normalize_agents;
use super::commands::normalize_commands;
use super::compaction::normalize_compaction;
use super::copy::{
    copy_bool, copy_bool_or_object, copy_object, copy_string, jp, normalize_share,
    normalize_string_array, normalize_update,
};
use super::ctx::Ctx;
use super::experimental::normalize_experimental;
use super::keys::{DROPPED_TOP, HANDLED};
use super::mcp::normalize_mcp;
use super::model::normalize_model;
use super::permissions::normalize_permissions;
use super::policies::normalize_policies;
use super::providers::normalize_providers;
use super::resources::{normalize_media, normalize_plugins, normalize_references};
use super::skills::normalize_skills;
use crate::diagnostic::{Diagnostic, DiagnosticKind};

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
