//! Wire request bodies accepted by the session routes.

use oc_types::{ModelRef, PermissionRule};
use serde::Deserialize;
use serde_json::{Map, Value};

/// `GET /api/session`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListParams {
    pub(super) limit: Option<usize>,
    pub(super) order: Option<String>,
    pub(super) search: Option<String>,
    #[serde(rename = "parentID")]
    pub(super) parent_id: Option<String>,
    pub(super) archived: Option<bool>,
    pub(super) directory: Option<String>,
    pub(super) project: Option<String>,
    pub(super) cursor: Option<String>,
}

/// `POST /api/session`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBody {
    #[serde(default)]
    pub(super) id: Option<String>,
    #[serde(rename = "parentID", default)]
    pub(super) parent_id: Option<String>,
    #[serde(default)]
    pub(super) title: Option<String>,
    #[serde(default)]
    pub(super) agent: Option<String>,
    #[serde(default)]
    pub(super) model: Option<ModelRef>,
    #[serde(default)]
    pub(super) location: Option<CreateLocation>,
    #[serde(default)]
    pub(super) metadata: Option<Map<String, Value>>,
    #[serde(default)]
    pub(super) permissions: Option<Vec<PermissionRule>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateLocation {
    pub(super) directory: String,
}

/// `PATCH /api/session/:sid`
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchBody {
    #[serde(default)]
    pub(super) title: Option<String>,
    #[serde(default)]
    pub(super) metadata: Option<Value>,
    #[serde(default)]
    pub(super) permissions: Option<Vec<PermissionRule>>,
    #[serde(default)]
    pub(super) archived: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct AgentBody {
    pub(super) agent: String,
}

#[derive(Debug, Deserialize)]
pub struct ModelBody {
    pub(super) model: ModelRef,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveBody {
    pub(super) directory: String,
    #[serde(default)]
    #[allow(dead_code)] // accepted for wire compatibility; move is immediate
    pub(super) delivery: Option<String>,
}

/// `POST /api/session/:sid/interrupt?resume=`
#[derive(Debug, Default, Deserialize)]
pub struct InterruptParams {
    #[serde(default)]
    #[allow(dead_code)] // no live execution to resume
    pub(super) resume: Option<bool>,
}

/// `POST /api/session/:sid/prompt`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptBody {
    #[serde(default)]
    pub(super) id: Option<String>,
    pub(super) text: String,
    #[serde(default)]
    pub(super) files: Option<Vec<Value>>,
    #[serde(default)]
    pub(super) agents: Option<Vec<Value>>,
    #[serde(default)]
    pub(super) skills: Option<Vec<Value>>,
    #[serde(default)]
    pub(super) metadata: Option<Map<String, Value>>,
    #[serde(default)]
    pub(super) delivery: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // accepted; there is no paused execution to resume
    pub(super) resume: Option<bool>,
}

/// `POST /api/session/:sid/synthetic`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyntheticBody {
    #[serde(default)]
    pub(super) id: Option<String>,
    pub(super) text: String,
    #[serde(default)]
    pub(super) description: Option<String>,
    #[serde(default)]
    pub(super) metadata: Option<Map<String, Value>>,
    #[serde(default)]
    pub(super) delivery: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(super) resume: Option<bool>,
}

/// `POST /api/session/:sid/compact`
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactBody {
    #[serde(default)]
    pub(super) id: Option<String>,
    #[serde(default)]
    pub(super) delivery: Option<String>,
}

/// `POST /api/session/:sid/command`
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandBody {
    #[serde(default)]
    pub(super) name: Option<String>,
    #[serde(default)]
    pub(super) text: Option<String>,
    #[serde(default)]
    pub(super) files: Option<Vec<Value>>,
    #[serde(default)]
    pub(super) agents: Option<Vec<Value>>,
    #[serde(default)]
    pub(super) skills: Option<Vec<Value>>,
    #[serde(default)]
    pub(super) delivery: Option<String>,
}

/// `POST /api/experimental/session/:sid/wait`
#[derive(Debug, Default, Deserialize)]
pub struct WaitParams {
    /// Seconds to wait; defaults to 30 and is clamped to 10 minutes.
    #[serde(default)]
    pub(super) timeout: Option<f64>,
}
