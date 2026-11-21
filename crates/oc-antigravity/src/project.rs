//! Cloud Code Assist project provisioning (`loadCodeAssist` & `onboardUser`).

use crate::constants::{
    ANTIGRAVITY_DEFAULT_PROJECT_ID, ANTIGRAVITY_GOOG_API_CLIENT, AntigravityEndpoints, HeaderStyle,
    antigravity_onboard_user_agent, default_control_plane_metadata, headers_for_style,
};
use crate::error::{AntigravityError, Result};
use crate::version::VersionTracker;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const ONBOARD_MAX_ATTEMPTS: usize = 10;
const ONBOARD_POLL_DELAY: Duration = Duration::from_secs(2);

/// Resolved Cloud Code Assist project context for an authenticated account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectContext {
    /// Effective project ID sent in request envelopes.
    pub effective_project_id: String,
    /// Managed project ID returned by `loadCodeAssist` or `onboardUser`, if any.
    pub managed_project_id: Option<String>,
    /// Active subscription/entitlement tier ID, if reported.
    pub current_tier_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadCodeAssistResponse {
    #[serde(default)]
    cloudaicompanion_project: Option<serde_json::Value>,
    #[serde(default)]
    current_tier: Option<TierInfo>,
    #[serde(default)]
    allowed_tiers: Vec<AllowedTier>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TierInfo {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AllowedTier {
    id: String,
    #[serde(default)]
    is_default: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OnboardUserResponse {
    #[serde(default)]
    done: Option<bool>,
    #[serde(default)]
    response: Option<OnboardResponsePayload>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OnboardResponsePayload {
    #[serde(default)]
    cloudaicompanion_project: Option<serde_json::Value>,
}

fn extract_project_id(val: Option<&serde_json::Value>) -> Option<String> {
    match val {
        Some(serde_json::Value::String(s)) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        Some(serde_json::Value::Object(map)) => map
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        _ => None,
    }
}

/// Query `/v1internal:loadCodeAssist` to discover an existing managed Cloud Code Assist project.
pub async fn load_managed_project(
    client: &reqwest::Client,
    endpoints: &AntigravityEndpoints,
    version: &VersionTracker,
    access_token: &str,
    explicit_project_id: Option<&str>,
) -> Result<(Option<String>, Option<String>, Vec<(String, bool)>)> {
    let hub_version = version.current();
    let metadata = default_control_plane_metadata(explicit_project_id);
    let body = serde_json::json!({ "metadata": metadata });

    let mut last_err: Option<String> = None;
    for base in endpoints.load_endpoints() {
        let url = format!("{base}/v1internal:loadCodeAssist");
        let headers = headers_for_style(HeaderStyle::Antigravity, &hub_version);
        let mut req = client
            .post(&url)
            .bearer_auth(access_token)
            .header("Content-Type", "application/json")
            .json(&body);
        for (k, v) in headers.to_pairs() {
            req = req.header(k, v);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(err) => {
                last_err = Some(format!("loadCodeAssist transport error: {err}"));
                continue;
            }
        };
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let err_text = resp.text().await.unwrap_or_default();
            last_err = Some(format!("loadCodeAssist HTTP {status}: {err_text}"));
            continue;
        }
        let parsed = match resp.json::<LoadCodeAssistResponse>().await {
            Ok(p) => p,
            Err(err) => {
                last_err = Some(format!("loadCodeAssist JSON decode error: {err}"));
                continue;
            }
        };
        let managed = extract_project_id(parsed.cloudaicompanion_project.as_ref());
        let current_tier = parsed.current_tier.map(|t| t.id);
        let allowed = parsed
            .allowed_tiers
            .into_iter()
            .map(|t| (t.id, t.is_default))
            .collect();
        return Ok((managed, current_tier, allowed));
    }

    if let Some(msg) = last_err {
        return Err(AntigravityError::Project(msg));
    }
    Ok((None, None, Vec::new()))
}

/// Provision a managed Cloud Code Assist project via `/v1internal:onboardUser`.
pub async fn onboard_managed_project(
    client: &reqwest::Client,
    endpoints: &AntigravityEndpoints,
    version: &VersionTracker,
    access_token: &str,
    tier_id: &str,
    explicit_project_id: Option<&str>,
) -> Result<Option<String>> {
    let hub_version = version.current();
    let url = format!("{}/v1internal:onboardUser", endpoints.antigravity_endpoint);

    let is_free = tier_id.eq_ignore_ascii_case("free-tier");
    let metadata = default_control_plane_metadata(if is_free { None } else { explicit_project_id });
    let mut payload = serde_json::json!({
        "tierId": tier_id,
        "metadata": metadata,
    });
    if !is_free
        && let Some(pid) = explicit_project_id
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert("cloudaicompanionProject".to_owned(), serde_json::json!(pid));
    }

    for attempt in 0..ONBOARD_MAX_ATTEMPTS {
        let resp = client
            .post(&url)
            .bearer_auth(access_token)
            .header("Content-Type", "application/json")
            .header("User-Agent", antigravity_onboard_user_agent(&hub_version))
            .header("X-Goog-Api-Client", ANTIGRAVITY_GOOG_API_CLIENT)
            .json(&payload)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let err_text = resp.text().await.unwrap_or_default();
            return Err(AntigravityError::Project(format!(
                "onboardUser failed (HTTP {status}): {err_text}"
            )));
        }

        let parsed: OnboardUserResponse = resp.json().await?;
        let managed = parsed
            .response
            .as_ref()
            .and_then(|r| extract_project_id(r.cloudaicompanion_project.as_ref()));
        if parsed.done.unwrap_or(false) || managed.is_some() {
            return Ok(managed);
        }
        if attempt + 1 < ONBOARD_MAX_ATTEMPTS {
            tokio::time::sleep(ONBOARD_POLL_DELAY).await;
        }
    }

    Ok(None)
}

/// Resolve the effective Cloud Code Assist project context for an authenticated account.
pub async fn ensure_project_context(
    client: &reqwest::Client,
    endpoints: &AntigravityEndpoints,
    version: &VersionTracker,
    access_token: &str,
    explicit_project_id: Option<&str>,
    allow_onboard: bool,
) -> Result<ProjectContext> {
    if let Some(pid) = explicit_project_id
        && !pid.trim().is_empty()
    {
        return Ok(ProjectContext {
            effective_project_id: pid.trim().to_owned(),
            managed_project_id: None,
            current_tier_id: None,
        });
    }

    let (managed, current_tier, allowed) = load_managed_project(
        client,
        endpoints,
        version,
        access_token,
        explicit_project_id,
    )
    .await?;

    if let Some(ref pid) = managed {
        return Ok(ProjectContext {
            effective_project_id: pid.clone(),
            managed_project_id: Some(pid.clone()),
            current_tier_id: current_tier,
        });
    }

    if allow_onboard && !allowed.is_empty() {
        let default_tier = allowed
            .iter()
            .find(|(_, is_def)| *is_def)
            .or_else(|| allowed.first())
            .map_or("legacy-tier", |(id, _)| id.as_str());

        if let Ok(Some(onboarded)) = onboard_managed_project(
            client,
            endpoints,
            version,
            access_token,
            default_tier,
            explicit_project_id,
        )
        .await
        {
            return Ok(ProjectContext {
                effective_project_id: onboarded.clone(),
                managed_project_id: Some(onboarded),
                current_tier_id: current_tier.or_else(|| Some(default_tier.to_owned())),
            });
        }
    }

    Ok(ProjectContext {
        effective_project_id: ANTIGRAVITY_DEFAULT_PROJECT_ID.to_owned(),
        managed_project_id: None,
        current_tier_id: current_tier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::{Json, Router};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn ensure_project_context_onboards_when_load_returns_no_project() {
        let app = Router::new()
            .route(
                "/v1internal:loadCodeAssist",
                post(|| async {
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "allowedTiers": [{ "id": "free-tier", "isDefault": true }]
                        })),
                    )
                }),
            )
            .route(
                "/v1internal:onboardUser",
                post(|| async {
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "done": true,
                            "response": {
                                "cloudaicompanionProject": { "id": "managed-proj-42" }
                            }
                        })),
                    )
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoints = AntigravityEndpoints::mock(&format!("http://127.0.0.1:{port}"));
        let ctx = ensure_project_context(
            &reqwest::Client::new(),
            &endpoints,
            &VersionTracker::new(),
            "ya29.tok",
            None,
            true,
        )
        .await
        .expect("project context");

        assert_eq!(ctx.effective_project_id, "managed-proj-42");
        assert_eq!(ctx.managed_project_id.as_deref(), Some("managed-proj-42"));
    }
}
