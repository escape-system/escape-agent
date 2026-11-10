use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use oc_lanes::{LaneError, LaneService, NewTicket, Ticket, classify};
use oc_store::{Delivery, NewInbox};
use oc_tools::{Tool, ToolContext, ToolError, ToolResult, util};
use oc_types::MessageId;
use serde_json::{Value, json};

use super::{DEFAULT_TIMEOUT_MS, NO_TIMEOUT, clamp_priority, optional_i64};

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// JSON Schema shared by every [`LaneTool`] instance.
///
/// Separate from [`LaneTool::schema`] so the advertised actions and parameters
/// can be asserted without constructing a runtime.
fn lane_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["queue", "order", "status", "wait", "cancel", "notify"],
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
                "description": "Ticket id for `order`, `cancel`, `status` or `wait`."
            },
            "timeout_ms": {
                "type": "integer",
                "minimum": 0,
                "description": "How long `wait` blocks, in milliseconds. Defaults to 300000 (5 minutes); 0 waits until the ticket is terminal. A timeout returns the current ticket view with `timedOut: true` and never cancels the run."
            },
            "session_id": {
                "type": "string",
                "description": "Target session id for `notify`."
            },
            "text": {
                "type": "string",
                "description": "Message text for `notify`."
            },
            "tree_rev": {
                "type": "string",
                "description": "Opaque caller-supplied tree state for `queue`; identical (lane, command, cwd, tree_rev) submissions coalesce into one run. The reply's `subscribed: true` means the run was already in flight; its result still arrives through `lane wait`."
            }
        },
        "required": ["action"],
        "additionalProperties": false
    })
}

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

    fn lanes(&self) -> Result<Arc<LaneService>, ToolError> {
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
        "Inspect and steer build lanes. `queue` submits a command as a ticket, `order` changes a queued ticket's priority, `status` returns lane or ticket state, `wait` blocks until a ticket is terminal, `cancel` stops a ticket, and `notify` sends a message to another session. Identical `queue` submissions (same lane, command, cwd and tree_rev) reuse the active ticket and just add the caller as a subscriber; `subscribed: true` means the run was already in flight and its result is still delivered by `lane wait`."
    }

    fn schema(&self) -> Value {
        lane_schema()
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let action = util::required_string(&input, "action")?;
        match action.as_str() {
            "queue" => self.queue(ctx, &input).await,
            "order" => self.order(&input).await,
            "status" => self.status(&input).await,
            "wait" => self.wait(&input).await,
            "cancel" => self.cancel(&input).await,
            "notify" => self.notify(&input).await,
            other => Err(ToolError::new(format!(
                "Invalid arguments: unknown lane action `{other}`; expected queue, order, status, wait, cancel or notify"
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
        let queued = lanes
            .enqueue_detailed(
                lane,
                NewTicket {
                    command,
                    cwd,
                    priority,
                    owner_session: owner,
                    label: None,
                    tree_rev: util::optional_string(input, "tree_rev")?,
                },
            )
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        let output = json!({
            "id": queued.id,
            "subscribed": queued.subscribed,
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

    async fn wait(&self, input: &Value) -> Result<ToolResult, ToolError> {
        let lanes = self.lanes()?;
        let ticket = util::required_string(input, "ticket")?;
        let timeout = wait_timeout(input)?;
        let result = lanes.wait(&ticket, timeout).await;
        let current = if matches!(&result, Err(LaneError::Timeout { .. })) {
            lanes.ticket(&ticket).await
        } else {
            None
        };
        let output = wait_result(result, current.as_ref())?;
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
        self.runtime
            .enqueue_and_emit(NewInbox {
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

// ---------------------------------------------------------------------------
// `wait` mapping
// ---------------------------------------------------------------------------

/// Wait budget for `lane.wait`: `timeout_ms` is optional, defaults to
/// [`DEFAULT_TIMEOUT_MS`], and `0` means wait until the ticket is terminal.
fn wait_timeout(input: &Value) -> Result<Duration, ToolError> {
    let timeout_ms = util::optional_u64(input, "timeout_ms")?.unwrap_or(DEFAULT_TIMEOUT_MS);
    Ok(if timeout_ms == 0 {
        NO_TIMEOUT
    } else {
        Duration::from_millis(timeout_ms).min(NO_TIMEOUT)
    })
}

/// Maps a [`LaneService::wait`] outcome to the tool reply.
///
/// A completed ticket becomes the same `{ticket}` view `status` returns. A
/// [`LaneError::Timeout`] is information, not a failure: the caller gets the
/// ticket's current view plus `timedOut: true`, and the run is left alone
/// (`wait` never cancels). Every other error stays a tool error.
///
/// `current` is the ticket snapshot fetched after a timeout; `None` means the
/// service no longer knows the ticket, which the caller reports as a `null`
/// view rather than inventing state.
fn wait_result(
    result: Result<Ticket, LaneError>,
    current: Option<&Ticket>,
) -> Result<Value, ToolError> {
    match result {
        Ok(ticket) => Ok(json!({ "ticket": ticket })),
        Err(LaneError::Timeout { .. }) => Ok(json!({ "ticket": current, "timedOut": true })),
        Err(error) => Err(ToolError::new(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use oc_lanes::{LaneError, Ticket, TicketState};
    use serde_json::{Value, json};

    use super::{DEFAULT_TIMEOUT_MS, NO_TIMEOUT, lane_schema, wait_result, wait_timeout};

    fn ticket(state: TicketState) -> Ticket {
        Ticket {
            id: "tkt_1".to_owned(),
            lane: "cargo:escape-agent".to_owned(),
            command: "cargo test -p oc-runtime".to_owned(),
            cwd: PathBuf::from("/tmp/escape-agent"),
            priority: 0,
            owner_session: Some("ses_test".to_owned()),
            state,
            queued_ms: 1_700_000_000_000,
            started_ms: Some(1_700_000_000_100),
            finished_ms: None,
            output_path: None,
            tree_rev: None,
            subscribers: Vec::new(),
        }
    }

    /// The advertised schema must agree with dispatch: `wait` is a real action
    /// and it accepts `ticket` + an integer `timeout_ms`.
    #[test]
    fn schema_advertises_wait_with_a_timeout() {
        let schema = lane_schema();
        let actions = schema["properties"]["action"]["enum"]
            .as_array()
            .expect("action enum");
        assert!(
            actions.iter().any(|action| action == "wait"),
            "wait missing from {actions:?}"
        );
        assert_eq!(schema["properties"]["timeout_ms"]["type"], json!("integer"));
        assert_eq!(schema["properties"]["timeout_ms"]["minimum"], json!(0));
        assert_eq!(schema["properties"]["ticket"]["type"], json!("string"));
    }

    #[test]
    fn wait_timeout_defaults_and_zero_waits_until_terminal() {
        assert_eq!(
            wait_timeout(&json!({})).expect("default"),
            Duration::from_millis(DEFAULT_TIMEOUT_MS)
        );
        assert_eq!(
            wait_timeout(&json!({ "timeout_ms": 0 })).expect("zero"),
            NO_TIMEOUT
        );
        assert_eq!(
            wait_timeout(&json!({ "timeout_ms": 7 })).expect("explicit"),
            Duration::from_millis(7)
        );
    }

    /// A finished ticket is the plain `status` view: no timeout marker.
    #[test]
    fn a_terminal_ticket_has_no_timeout_marker() {
        let output = wait_result(Ok(ticket(TicketState::Succeeded { exit: 0 })), None)
            .expect("terminal ticket");
        assert_eq!(output["ticket"]["id"], json!("tkt_1"));
        assert_eq!(output["ticket"]["state"]["state"], json!("succeeded"));
        assert!(output.get("timedOut").is_none(), "{output}");
    }

    /// A timeout is information: `timedOut: true` plus the live ticket view,
    /// and no tool error. The running state proves the run was left alone.
    #[test]
    fn a_wait_timeout_returns_the_current_view_instead_of_an_error() {
        let output = wait_result(
            Err(LaneError::Timeout {
                id: "tkt_1".to_owned(),
                timeout_ms: 5,
            }),
            Some(&ticket(TicketState::Running)),
        )
        .expect("a timeout is not a tool failure");
        assert_eq!(output["timedOut"], json!(true));
        assert_eq!(output["ticket"]["id"], json!("tkt_1"));
        assert_eq!(output["ticket"]["state"]["state"], json!("running"));
    }

    /// Every other lane error keeps the usual tool-error mapping.
    #[test]
    fn other_lane_errors_stay_tool_errors() {
        let error = wait_result(Err(LaneError::UnknownTicket("tkt_9".to_owned())), None)
            .expect_err("an unknown ticket is a failure");
        assert!(error.message.contains("unknown ticket `tkt_9`"), "{error}");
    }

    /// Guard the view shape against accidental `Value` drift.
    #[test]
    fn the_success_view_is_an_object_with_one_ticket_key() {
        let output = wait_result(Ok(ticket(TicketState::Succeeded { exit: 0 })), None)
            .expect("terminal ticket");
        let object: &serde_json::Map<String, Value> = output.as_object().expect("object");
        assert_eq!(object.len(), 1);
        assert!(object.contains_key("ticket"));
    }
}
