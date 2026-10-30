use async_trait::async_trait;
use oc_lanes::{LaneService, NewTicket, classify};
use oc_store::{Delivery, NewInbox};
use oc_tools::{Tool, ToolContext, ToolError, ToolResult, util};
use oc_types::MessageId;
use serde_json::{Value, json};

use super::{clamp_priority, optional_i64};

// ---------------------------------------------------------------------------
// Coordinator lane tool
// ---------------------------------------------------------------------------

/// Coordinator tool for ordering, cancelling and notifying lane work.
#[derive(Debug, Clone)]
pub struct LaneTool {
    runtime: crate::Runtime,
}

impl LaneTool {
    /// Builds the tool over a runtime.
    #[must_use]
    pub fn new(runtime: crate::Runtime) -> Self {
        Self { runtime }
    }

    fn lanes(&self) -> Result<std::sync::Arc<LaneService>, ToolError> {
        self.runtime
            .lanes()
            .ok_or_else(|| ToolError::new("The lane service is not available."))
    }
}

#[async_trait]
impl Tool for LaneTool {
    fn name(&self) -> &'static str {
        "lane"
    }

    fn description(&self) -> &'static str {
        "Inspect and steer build lanes. `queue` submits a command as a ticket, `order` changes a queued ticket's priority, `status` returns lane or ticket state, `cancel` stops a ticket, and `notify` sends a message to another session."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["queue", "order", "status", "cancel", "notify"],
                    "description": "Lane operation to perform."
                },
                "lane": {
                    "type": "string",
                    "description": "Lane name for `queue`; defaults to the classifier result, then `default`. For `status`, restricts the snapshot to one lane."
                },
                "command": {
                    "type": "string",
                    "description": "Command string for `queue`."
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory for `queue`. Defaults to the current working directory."
                },
                "priority": {
                    "type": "integer",
                    "description": "Queue priority. Higher values run first; equal priorities stay FIFO."
                },
                "owner": {
                    "type": "string",
                    "description": "Owning session id. Defaults to the calling session."
                },
                "ticket": {
                    "type": "string",
                    "description": "Ticket id for `order`, `cancel` or `status`."
                },
                "session_id": {
                    "type": "string",
                    "description": "Target session id for `notify`."
                },
                "text": {
                    "type": "string",
                    "description": "Message text for `notify`."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let action = util::required_string(&input, "action")?;
        match action.as_str() {
            "queue" => self.queue(ctx, &input).await,
            "order" => self.order(&input).await,
            "status" => self.status(&input).await,
            "cancel" => self.cancel(&input).await,
            "notify" => self.notify(&input).await,
            other => Err(ToolError::new(format!(
                "Invalid arguments: unknown lane action `{other}`; expected queue, order, status, cancel or notify"
            ))),
        }
    }
}

impl LaneTool {
    async fn queue(&self, ctx: &ToolContext, input: &Value) -> Result<ToolResult, ToolError> {
        let lanes = self.lanes()?;
        let command = util::required_string(input, "command")?;
        let cwd = match util::optional_string(input, "cwd")? {
            Some(value) => util::resolve_from(&ctx.cwd, &value),
            None => ctx.cwd.clone(),
        };
        let priority = clamp_priority(optional_i64(input, "priority")?.unwrap_or(0));
        let owner = util::optional_string(input, "owner")?.or_else(|| Some(ctx.session_id.clone()));
        let lane =
            match util::optional_string(input, "lane")?.filter(|lane| !lane.trim().is_empty()) {
                Some(lane) => lane,
                None => classify(&command, &cwd).map_or_else(|| "default".to_owned(), |r| r.lane),
            };
        let id = lanes
            .enqueue(
                lane.clone(),
                NewTicket {
                    command,
                    cwd,
                    priority,
                    owner_session: owner,
                    label: None,
                },
            )
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        let ticket = lanes.ticket(&id).await;
        let output = json!({
            "ticket": id,
            "lane": lane,
            "ticketDetails": ticket,
        });
        Ok(ToolResult::new(
            Some(serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string())),
            None,
            Some(output),
        ))
    }

    async fn order(&self, input: &Value) -> Result<ToolResult, ToolError> {
        let lanes = self.lanes()?;
        let ticket = util::required_string(input, "ticket")?;
        let priority = optional_i64(input, "priority")?.ok_or_else(|| {
            ToolError::new("Invalid arguments: missing required argument `priority`")
        })?;
        lanes
            .reorder(&ticket, clamp_priority(priority))
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        let output = json!({ "ticket": ticket, "priority": priority });
        Ok(ToolResult::new(
            Some(serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string())),
            None,
            Some(output),
        ))
    }

    async fn status(&self, input: &Value) -> Result<ToolResult, ToolError> {
        let lanes = self.lanes()?;
        let ticket = util::optional_string(input, "ticket")?.filter(|value| !value.is_empty());
        let lane = util::optional_string(input, "lane")?.filter(|value| !value.is_empty());
        let output = if let Some(id) = ticket {
            let ticket = lanes.ticket(&id).await;
            json!({ "ticket": ticket })
        } else {
            let statuses = lanes.status(lane.as_deref()).await;
            json!({ "lanes": statuses })
        };
        Ok(ToolResult::new(
            Some(serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string())),
            None,
            Some(output),
        ))
    }

    async fn cancel(&self, input: &Value) -> Result<ToolResult, ToolError> {
        let lanes = self.lanes()?;
        let ticket = util::required_string(input, "ticket")?;
        let cancelled = lanes
            .cancel(&ticket)
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        let output = json!({ "ticket": ticket, "cancelled": cancelled });
        Ok(ToolResult::new(
            Some(serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string())),
            None,
            Some(output),
        ))
    }

    async fn notify(&self, input: &Value) -> Result<ToolResult, ToolError> {
        let session_id = util::required_string(input, "session_id")?;
        let text = util::required_string(input, "text")?;
        if text.trim().is_empty() {
            return Err(ToolError::new(
                "Invalid arguments: `text` must not be empty",
            ));
        }
        let message_id = MessageId::new().into_inner();
        let store = self.runtime.inner.config.store.clone();
        store
            .enqueue(NewInbox {
                id: message_id.clone(),
                session_id: session_id.clone(),
                kind: "synthetic".to_owned(),
                payload: json!({ "text": text, "peer": true }),
                delivery: Delivery::Steer,
            })
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        self.runtime
            .notify(&session_id)
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        let output = json!({ "sessionID": session_id, "messageID": message_id });
        Ok(ToolResult::new(
            Some(serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string())),
            None,
            Some(output),
        ))
    }
}
