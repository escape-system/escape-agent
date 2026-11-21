//! End-to-end transformation of `oc_llm::LlmRequest` into a Cloud Code Assist
//! `v1internal` request envelope.

use crate::accounts::ModelFamily;
use crate::config::AntigravityConfig;
use crate::constants::{ANTIGRAVITY_SYSTEM_INSTRUCTION, HeaderStyle};
use crate::signature_cache::SignatureCache;
use crate::thinking::{
    close_tool_loop_for_thinking, needs_thinking_recovery, repair_contents_thinking,
};
use crate::transform::claude::transform_claude_request;
use crate::transform::cross_model::sanitize_cross_model_contents;
use crate::transform::gemini::transform_gemini_request;
use crate::transform::model_resolver::{
    ResolvedTargetModel, ThinkingMode, resolve_antigravity_model,
};
use oc_llm::{ContentPart, LlmMessage, LlmRequest, Role};
use serde_json::{Map, Value, json};

/// Complete Cloud Code Assist envelope plus resolved routing metadata.
#[derive(Debug, Clone)]
pub struct TransformedRequest {
    /// Target model metadata (`actual_model`, `family`, `header_style`, `thinking`).
    pub resolved: ResolvedTargetModel,
    /// Full v1internal JSON payload ready to POST to `/v1internal:streamGenerateContent?alt=sse`.
    pub envelope: Value,
    /// Stable session key used for prompt caching & signature lookups.
    pub session_key: String,
    /// Generated `requestId` (`agent-<uuid>`).
    pub request_id: String,
}

/// Build the full Cloud Code Assist v1internal envelope from `request`.
#[allow(clippy::too_many_arguments)]
pub fn build_cloud_code_request(
    request: &LlmRequest,
    project_id: &str,
    session_key: &str,
    cfg: &AntigravityConfig,
    override_style: Option<HeaderStyle>,
    signature_cache: Option<&mut SignatureCache>,
    force_strip_thinking: bool,
    now_ms: i64,
) -> TransformedRequest {
    let variant = request.model.variant.as_ref().map(|v| v.id.as_str());
    let mut resolved = resolve_antigravity_model(&request.model.model.id, variant, cfg.cli_first);

    // Respect variant raw settings/body thinkingConfig / thinkingLevel / thinkingBudget options if supplied.
    if let Some(var) = request.model.variant.as_ref() {
        if let Some(settings) = var.settings.as_ref() {
            apply_catalog_variant_options(&mut resolved, settings);
        }
        if let Some(body) = var.body.as_ref() {
            apply_catalog_variant_options(&mut resolved, body);
        }
    }

    if let Some(style) = override_style {
        apply_style_override(&mut resolved, style);
    }

    let mut contents = messages_to_cloud_code_contents(&request.messages);

    // Cross-model hygiene + tool ID pairing.
    sanitize_cross_model_contents(&mut contents, resolved.family, cfg.tool_id_recovery);

    let thinking_enabled =
        !force_strip_thinking && !matches!(resolved.thinking, ThinkingMode::Disabled);
    let effective_thinking = if force_strip_thinking {
        ThinkingMode::Disabled
    } else {
        resolved.thinking.clone()
    };

    if cfg.session_recovery && needs_thinking_recovery(&contents, resolved.family) {
        repair_contents_thinking(
            &mut contents,
            resolved.family,
            thinking_enabled,
            cfg.keep_thinking && !force_strip_thinking,
            session_key,
            signature_cache,
            now_ms,
        );
        if resolved.family == ModelFamily::Claude
            && needs_thinking_recovery(&contents, resolved.family)
        {
            close_tool_loop_for_thinking(&mut contents);
        }
    } else {
        repair_contents_thinking(
            &mut contents,
            resolved.family,
            thinking_enabled,
            cfg.keep_thinking && !force_strip_thinking,
            session_key,
            signature_cache,
            now_ms,
        );
    }

    if cfg.disable_synthetic_thought_signature {
        remove_synthetic_thought_signatures(&mut contents);
    }

    let mut req_obj = Map::new();
    req_obj.insert("contents".to_owned(), Value::Array(contents));
    insert_system_instruction(&mut req_obj, &resolved, request);
    insert_tools_and_gen_config(&mut req_obj, request, session_key);

    match resolved.family {
        ModelFamily::Claude => transform_claude_request(
            &mut req_obj,
            &effective_thinking,
            cfg.claude_tool_hardening,
            cfg.tool_id_recovery,
        ),
        ModelFamily::Gemini => transform_gemini_request(&mut req_obj, &effective_thinking),
    }

    let request_id = format!("agent-{}", generate_simple_uuid());
    let user_agent = match resolved.header_style {
        HeaderStyle::Antigravity => "antigravity",
        HeaderStyle::GeminiCli => "gemini-cli",
    };

    let envelope = json!({
        "project": project_id,
        "model": resolved.actual_model,
        "request": Value::Object(req_obj),
        "requestType": "agent",
        "userAgent": user_agent,
        "requestId": request_id
    });

    TransformedRequest {
        resolved,
        envelope,
        session_key: session_key.to_owned(),
        request_id,
    }
}

fn insert_system_instruction(
    req_obj: &mut Map<String, Value>,
    resolved: &ResolvedTargetModel,
    request: &LlmRequest,
) {
    let mut sys_parts = Vec::new();
    if resolved.header_style == HeaderStyle::Antigravity {
        sys_parts.push(json!({ "text": ANTIGRAVITY_SYSTEM_INSTRUCTION }));
    }
    for s in &request.system {
        if !s.trim().is_empty() {
            sys_parts.push(json!({ "text": s }));
        }
    }
    if !sys_parts.is_empty() {
        req_obj.insert(
            "systemInstruction".to_owned(),
            json!({
                "role": "user",
                "parts": sys_parts
            }),
        );
    }
}

/// Conservative ceiling for a single Cloud Code output request. Models such as
/// Gemini 4 Hydrogen advertise a 262,144-token output limit; asking for that
/// whole budget up front is rejected upstream / stalls the turn. Callers may
/// request less (their value is honored); anything above is clamped to this cap
/// and to the model's own output limit.
pub const MAX_OUTPUT_TOKENS_CAP: u64 = 32_768;

/// Resolve the effective `maxOutputTokens` sent upstream.
fn effective_max_output_tokens(requested: Option<u64>, model_limit: u64) -> u64 {
    let base = requested.unwrap_or(model_limit.max(8192));
    if model_limit == 0 {
        return base.min(MAX_OUTPUT_TOKENS_CAP);
    }
    base.min(model_limit).min(MAX_OUTPUT_TOKENS_CAP)
}

fn remove_synthetic_thought_signatures(contents: &mut [Value]) {
    for msg in contents.iter_mut() {
        let Some(parts) = msg.get_mut("parts").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in parts.iter_mut() {
            let is_sentinel = part
                .get("thoughtSignature")
                .and_then(Value::as_str)
                .is_some_and(|sig| sig == crate::constants::SKIP_THOUGHT_SIGNATURE);
            if is_sentinel && let Some(obj) = part.as_object_mut() {
                obj.remove("thoughtSignature");
            }
        }
    }
}

fn insert_tools_and_gen_config(
    req_obj: &mut Map<String, Value>,
    request: &LlmRequest,
    session_key: &str,
) {
    if !request.tools.is_empty() {
        let decls: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema
                })
            })
            .collect();
        req_obj.insert(
            "tools".to_owned(),
            json!([{ "functionDeclarations": decls }]),
        );
        if let Some(choice) = request
            .options
            .tool_choice
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let fcc = match choice.to_ascii_lowercase().as_str() {
                "none" => json!({ "mode": "NONE" }),
                "auto" => json!({ "mode": "AUTO" }),
                "required" | "any" => json!({ "mode": "ANY" }),
                _ => json!({
                    "mode": "ANY",
                    "allowedFunctionNames": [choice]
                }),
            };
            req_obj.insert(
                "toolConfig".to_owned(),
                json!({ "functionCallingConfig": fcc }),
            );
        }
    }

    let mut gen_cfg = Map::new();
    if let Some(temp) = request.options.temperature {
        gen_cfg.insert("temperature".to_owned(), json!(temp));
    }
    if let Some(top_p) = request.options.top_p {
        gen_cfg.insert("topP".to_owned(), json!(top_p));
    }
    gen_cfg.insert(
        "maxOutputTokens".to_owned(),
        json!(effective_max_output_tokens(
            request.options.max_output_tokens,
            request.model.model.limit.output,
        )),
    );
    if !gen_cfg.is_empty() {
        req_obj.insert("generationConfig".to_owned(), Value::Object(gen_cfg));
    }

    if !session_key.trim().is_empty() {
        req_obj.insert("sessionId".to_owned(), json!(session_key));
    }
}

fn apply_style_override(resolved: &mut ResolvedTargetModel, style: HeaderStyle) {
    resolved.header_style = style;
    let model = resolved.actual_model.clone();
    if style == HeaderStyle::GeminiCli {
        if model.starts_with("gemini-3.1-pro") && !model.ends_with("-preview") {
            "gemini-3.1-pro-preview".clone_into(&mut resolved.actual_model);
        } else if model.starts_with("gemini-3-pro") && !model.ends_with("-preview") {
            "gemini-3-pro-preview".clone_into(&mut resolved.actual_model);
        } else if model == "gemini-3-flash" {
            "gemini-3-flash-preview".clone_into(&mut resolved.actual_model);
        } else if let Some(base) = strip_antigravity_tier_for_cli(&model) {
            base.clone_into(&mut resolved.actual_model);
        }
    } else if style == HeaderStyle::Antigravity {
        let tier = match &resolved.thinking {
            ThinkingMode::GeminiLevel(lvl) => Some(lvl.as_str()),
            _ => None,
        };
        if model == "gemini-4-hydrogen" {
            resolved.actual_model = format!("gemini-4-hydrogen-{}", tier.unwrap_or("high"));
        } else if model == "gemini-4-argon" {
            resolved.actual_model = format!("gemini-4-argon-{}", tier.unwrap_or("medium"));
        } else if model == "gemini-3.8-flash" {
            resolved.actual_model = format!("gemini-3.8-flash-{}", tier.unwrap_or("medium"));
        }
    }
}

fn strip_antigravity_tier_for_cli(model: &str) -> Option<&str> {
    for prefix in ["gemini-4-hydrogen", "gemini-4-argon", "gemini-3.8-flash"] {
        if let Some(rest) = model.strip_prefix(prefix)
            && matches!(rest, "-low" | "-medium" | "-high" | "-xhigh")
        {
            return Some(prefix);
        }
    }
    None
}

fn apply_catalog_variant_options(resolved: &mut ResolvedTargetModel, options: &Map<String, Value>) {
    if let Some(lvl) = options
        .get("thinkingLevel")
        .and_then(Value::as_str)
        .or_else(|| {
            options
                .get("thinkingConfig")
                .and_then(|v| v.get("thinkingLevel"))
                .and_then(Value::as_str)
        })
    {
        if resolved.family == ModelFamily::Claude {
            let budget = match lvl.to_ascii_lowercase().as_str() {
                "minimal" | "low" => 8192,
                "high" | "max" | "xhigh" => 32768,
                _ => 16384,
            };
            resolved.thinking = ThinkingMode::Budget(budget);
        } else {
            resolved.thinking = ThinkingMode::GeminiLevel(lvl.to_owned());
        }
        return;
    }
    if let Some(budget) = options
        .get("thinkingBudget")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
    {
        resolved.thinking = ThinkingMode::Budget(budget);
        return;
    }
    if let Some(budget) = options
        .get("thinkingConfig")
        .and_then(|v| v.get("thinkingBudget"))
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
    {
        resolved.thinking = ThinkingMode::Budget(budget);
    }
}

fn messages_to_cloud_code_contents(messages: &[LlmMessage]) -> Vec<Value> {
    let mut contents = Vec::with_capacity(messages.len());

    for msg in messages {
        let role = match msg.role {
            Role::Assistant => "model",
            Role::User | Role::Tool | Role::System => "user",
        };
        let mut parts = Vec::new();
        for part in &msg.content {
            match part {
                ContentPart::Text { text } => {
                    if !text.is_empty() {
                        parts.push(json!({ "text": text }));
                    }
                }
                ContentPart::ToolCall { id, name, input } => {
                    parts.push(json!({
                        "functionCall": {
                            "id": id,
                            "name": name,
                            "args": input
                        }
                    }));
                }
                ContentPart::ToolResult {
                    tool_call_id,
                    name,
                    content,
                    is_error,
                } => {
                    let payload = if *is_error {
                        json!({ "error": content })
                    } else {
                        json!({ "result": content })
                    };
                    let mut fr = Map::new();
                    fr.insert("id".to_owned(), json!(tool_call_id));
                    if let Some(tool_name) = name {
                        fr.insert("name".to_owned(), json!(tool_name));
                    } else {
                        fr.insert("name".to_owned(), json!("tool"));
                    }
                    fr.insert("response".to_owned(), payload);
                    parts.push(json!({ "functionResponse": Value::Object(fr) }));
                }
                ContentPart::Image { media_type, data }
                | ContentPart::Document {
                    media_type, data, ..
                } => {
                    if let Some((inline_mime, b64)) = parse_data_url(data) {
                        parts.push(json!({
                            "inlineData": {
                                "mimeType": if media_type.is_empty() { inline_mime } else { media_type.as_str() },
                                "data": b64
                            }
                        }));
                    } else {
                        parts.push(json!({
                            "inlineData": {
                                "mimeType": media_type,
                                "data": data
                            }
                        }));
                    }
                }
            }
        }

        if !parts.is_empty() {
            contents.push(json!({
                "role": role,
                "parts": parts
            }));
        }
    }

    contents
}

fn parse_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    Some((mime, data))
}

fn generate_simple_uuid() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{}-{}-{}-{}-{}",
        hex::encode(&b[0..4]),
        hex::encode(&b[4..6]),
        hex::encode(&b[6..8]),
        hex::encode(&b[8..10]),
        hex::encode(&b[10..16]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::overlay_catalog_models;
    use oc_llm::Catalog;

    #[test]
    fn builds_valid_cloud_code_envelopes_for_gemini4_xhigh_and_claude55() {
        let mut cat = Catalog::from_snapshot("{}").expect("empty");
        overlay_catalog_models(&mut cat, None);
        let cfg = AntigravityConfig::default();

        let h2 = cat
            .resolve("google/antigravity-gemini-4-hydrogen#xhigh")
            .expect("resolve h2 xhigh");
        let mut req_h2 = LlmRequest::new(h2);
        req_h2.messages = vec![LlmMessage::user("hi")];
        let out_h2 =
            build_cloud_code_request(&req_h2, "proj-1", "ses-1", &cfg, None, None, false, 1);
        assert_eq!(out_h2.envelope["model"], "gemini-4-hydrogen-xhigh");
        assert_eq!(
            out_h2.envelope["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "xhigh"
        );
        assert_eq!(
            out_h2.envelope["request"]["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );

        let opus55 = cat
            .resolve("google/antigravity-claude-opus-5-5#high")
            .expect("resolve opus 5.5 high");
        let mut req_opus = LlmRequest::new(opus55);
        req_opus.messages = vec![LlmMessage::user("hi")];
        let out_opus =
            build_cloud_code_request(&req_opus, "proj-1", "ses-1", &cfg, None, None, false, 1);
        assert_eq!(out_opus.envelope["model"], "claude-opus-5-5-high");
        assert_eq!(
            out_opus.envelope["request"]["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            32768
        );
        assert_eq!(
            out_opus.envelope["request"]["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
    }

    #[test]
    fn effective_max_output_tokens_is_capped_for_huge_output_models() {
        // Gemini 4 Hydrogen advertises 262,144 output tokens: the default must
        // not forward that enormous budget upstream.
        assert_eq!(effective_max_output_tokens(None, 262_144), 32_768);
        // Explicit smaller values are respected.
        assert_eq!(effective_max_output_tokens(Some(4096), 262_144), 4096);
        // Explicit huge values are clamped to both the model limit and the cap.
        assert_eq!(effective_max_output_tokens(Some(262_144), 262_144), 32_768);
        assert_eq!(effective_max_output_tokens(Some(100_000), 4096), 4096);
        // A model with no advertised limit still gets the floor, capped.
        assert_eq!(effective_max_output_tokens(None, 0), 8192);
        assert_eq!(MAX_OUTPUT_TOKENS_CAP, 32_768);
    }

    #[test]
    fn envelope_caps_hydrogen_output_and_keeps_explicit_smaller_value() {
        let mut cat = Catalog::from_snapshot("{}").expect("empty");
        overlay_catalog_models(&mut cat, None);
        let cfg = AntigravityConfig::default();
        let h2 = cat
            .resolve("google/antigravity-gemini-4-hydrogen#high")
            .expect("resolve h2");

        let mut req = LlmRequest::new(h2.clone());
        req.messages = vec![LlmMessage::user("hi")];
        let out = build_cloud_code_request(&req, "proj", "ses", &cfg, None, None, false, 1);
        assert_eq!(
            out.envelope["request"]["generationConfig"]["maxOutputTokens"],
            32_768
        );

        req.options.max_output_tokens = Some(2048);
        let out = build_cloud_code_request(&req, "proj", "ses", &cfg, None, None, false, 1);
        assert_eq!(
            out.envelope["request"]["generationConfig"]["maxOutputTokens"],
            2048
        );
    }

    #[test]
    fn disabling_synthetic_thought_signatures_removes_only_the_sentinel() {
        let real_sig = "a-real-signature-value-12345678";
        let mut contents = vec![json!({
            "role": "model",
            "parts": [
                { "text": "thinking", "thought": true, "thoughtSignature": crate::constants::SKIP_THOUGHT_SIGNATURE },
                { "text": "answer", "thoughtSignature": real_sig }
            ]
        })];
        remove_synthetic_thought_signatures(&mut contents);
        let parts = contents[0]["parts"].as_array().expect("parts");
        assert_eq!(parts[0].get("thoughtSignature"), None);
        assert_eq!(parts[1]["thoughtSignature"], real_sig);
    }
}
