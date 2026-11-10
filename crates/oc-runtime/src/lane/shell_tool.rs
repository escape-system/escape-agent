use std::fmt::Write as _;
use std::time::Duration;

use async_trait::async_trait;
use oc_lanes::{LaneError, NewTicket, TicketState, classify};
use oc_tools::{ShellTool, Tool, ToolContext, ToolError, ToolResult, util};
use serde_json::{Value, json};

use super::output::{Tail, read_tail, status_exit, status_name, tail_body};
use super::{DEFAULT_TIMEOUT_MS, NO_TIMEOUT, clamp_priority, optional_i64};

// ---------------------------------------------------------------------------
// Shared-run timeout decision
// ---------------------------------------------------------------------------

/// What a timed-out wait means for the ticket it was waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutAction {
    /// The owner or another subscriber is still waiting: the run continues.
    KeepRunning,
    /// Nobody is interested any more, so the ticket is no longer running.
    Stopped,
}

/// Maps [`oc_lanes::LaneService::unsubscribe`] to the user-visible outcome.
///
/// `still_running` is the service's answer: `true` means an owner or another
/// subscriber remains and the shared run was deliberately left alone.
fn timeout_action(still_running: bool) -> TimeoutAction {
    if still_running {
        TimeoutAction::KeepRunning
    } else {
        TimeoutAction::Stopped
    }
}

// ---------------------------------------------------------------------------
// Shell tool with lane routing
// ---------------------------------------------------------------------------

/// Lane-aware replacement for [`oc_tools::ShellTool`].
///
/// The registry key stays `shell`. Classified build/test commands become lane
/// tickets; unclassified commands and explicit `background` requests delegate
/// to the plain shell tool.
#[derive(Debug, Clone)]
pub struct ShellLaneTool {
    runtime: crate::Runtime,
    shell: ShellTool,
}

impl ShellLaneTool {
    /// Builds the tool over a runtime that may have a lane service attached.
    #[must_use]
    pub fn new(runtime: crate::Runtime) -> Self {
        Self {
            runtime,
            shell: ShellTool,
        }
    }
}

#[async_trait]
impl Tool for ShellLaneTool {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command and return its output. Quote file paths containing spaces or special characters. Prefer dedicated tools over shell commands when possible. Build and test commands are serialized through build lanes and wait for their turn before returning. When output is large, the full result is saved to a file and a truncated preview is returned. Commands accept an optional timeout; a timeout of 0 disables it."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command string to execute"
                },
                "workdir": {
                    "type": "string",
                    "description": "Working directory to execute the command in. Defaults to the current working directory. When possible, avoid changing directories in the command and set the working directory here instead."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Timeout in milliseconds. Set to 0 to disable the timeout. Defaults to 300000 for lane-routed commands."
                },
                "background": {
                    "type": "boolean",
                    "description": "Run the command in the background and return immediately (useful for dev servers and long-running builds). Background execution is not available in phase 1."
                },
                "lane": {
                    "type": "string",
                    "description": "Explicit lane override. When omitted, build/test commands are classified into cargo/node/python/… lanes automatically."
                },
                "priority": {
                    "type": "integer",
                    "description": "Queue priority inside the lane. Higher values run first; equal priorities stay FIFO."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    #[allow(clippy::too_many_lines)] // Delegate/enqueue/wait/report are one flow.
    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let command = util::required_string(&input, "command")?;
        let workdir_input = util::optional_string(&input, "workdir")?;
        let timeout_ms = util::optional_u64(&input, "timeout")?.unwrap_or(DEFAULT_TIMEOUT_MS);
        let explicit_lane =
            util::optional_string(&input, "lane")?.filter(|lane| !lane.trim().is_empty());
        let priority = optional_i64(&input, "priority")?.unwrap_or(0);
        let background = util::optional_bool(&input, "background")?.unwrap_or(false);

        let cwd = match workdir_input {
            Some(value) => util::resolve_from(&ctx.cwd, &value),
            None => ctx.cwd.clone(),
        };
        if background {
            return self.shell.execute(ctx, input).await;
        }

        let request = explicit_lane.map_or_else(
            || classify(&command, &cwd),
            |lane| {
                Some(oc_lanes::LaneRequest {
                    lane,
                    command: command.clone(),
                })
            },
        );
        let Some(request) = request else {
            return self.shell.execute(ctx, input).await;
        };
        let Some(lanes) = self.runtime.lanes() else {
            return self.shell.execute(ctx, input).await;
        };

        // Only cargo work coalesces: the tree revision describes a git tree,
        // and a node/gpu ticket must not join a cargo run by accident.
        let tree_rev = if request.lane.starts_with("cargo:") {
            crate::lane::tree_rev(&cwd.to_string_lossy()).await
        } else {
            None
        };
        let queued = lanes
            .enqueue_detailed(
                request.lane.clone(),
                NewTicket {
                    command: request.command,
                    cwd,
                    priority: clamp_priority(priority),
                    owner_session: Some(ctx.session_id.clone()),
                    label: None,
                    tree_rev,
                },
            )
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        let ticket_id = queued.id;

        let timeout = if timeout_ms == 0 {
            NO_TIMEOUT
        } else {
            Duration::from_millis(timeout_ms).min(NO_TIMEOUT)
        };
        let ticket = match lanes.wait(&ticket_id, timeout).await {
            Ok(ticket) => ticket,
            Err(LaneError::Timeout { .. }) => {
                // Dropping out of a shared run must not cancel it: the owner
                // and every other subscriber still need the result.
                // `unsubscribe` cancels only when nobody is left interested,
                // so a slow subscriber can never kill another agent's build.
                let still_running = lanes
                    .unsubscribe(&ticket_id, &ctx.session_id)
                    .await
                    .unwrap_or(false);
                let metadata = json!({
                    "lane": request.lane,
                    "ticket": ticket_id,
                    "status": "timeout",
                    "subscribed": queued.subscribed,
                });
                let message = match timeout_action(still_running) {
                    TimeoutAction::KeepRunning => format!(
                        "Stopped waiting for lane ticket {ticket_id} after {timeout_ms} ms, but the run continues for its owner and other subscribers. Poll `lane status {ticket_id}` or call `lane wait` to collect the result."
                    ),
                    TimeoutAction::Stopped => format!(
                        "Lane ticket {ticket_id} did not finish within {timeout_ms} ms and is no longer running. Retry with a larger timeout if the command is expected to take longer."
                    ),
                };
                return Err(ToolError::new(message).with_metadata(metadata));
            }
            Err(error) => return Err(ToolError::new(error.to_string())),
        };

        let status = status_name(&ticket.state);
        let mut metadata = json!({
            "lane": ticket.lane,
            "ticket": ticket.id,
            "status": status,
            "truncated": false,
        });
        if let Some(exit) = status_exit(&ticket.state)
            && let Some(object) = metadata.as_object_mut()
        {
            object.insert("exit".to_owned(), json!(exit));
        }
        let path = ticket.output_path.clone();
        if let Some(path) = &path
            && let Some(object) = metadata.as_object_mut()
        {
            object.insert(
                "outputPath".to_owned(),
                Value::String(path.display().to_string()),
            );
        }
        let tail = match path.as_deref().map(read_tail) {
            Some(Ok(tail)) => tail,
            Some(Err(error)) => {
                return Err(ToolError::new(format!(
                    "Unable to read lane output for {}: {error}",
                    ticket.id
                ))
                .with_metadata(metadata));
            }
            None => Tail {
                text: String::new(),
                truncated: false,
            },
        };
        if let Some(object) = metadata.as_object_mut() {
            object.insert("truncated".to_owned(), json!(tail.truncated));
        }
        let mut body = tail_body(tail);

        match &ticket.state {
            TicketState::Cancelled => {
                return Err(
                    ToolError::new(format!("Lane ticket {} was cancelled", ticket.id))
                        .with_metadata(metadata),
                );
            }
            TicketState::Failed { exit, message } => {
                let _ = write!(body, "\n\nExited with code {exit}: {message}");
            }
            _ => {}
        }

        Ok(ToolResult::new(Some(body), None, Some(metadata)))
    }
}

#[cfg(test)]
mod tests {
    use super::{TimeoutAction, timeout_action};

    /// A still-live shared run is never cancelled by a subscriber leaving:
    /// the owner and the other subscribers still need the result.
    #[test]
    fn a_live_shared_run_keeps_running() {
        assert_eq!(timeout_action(true), TimeoutAction::KeepRunning);
    }

    /// When `unsubscribe` reports that nobody is left interested it has
    /// already cancelled the ticket, so the caller hears "stopped".
    #[test]
    fn the_last_interested_party_stops_the_run() {
        assert_eq!(timeout_action(false), TimeoutAction::Stopped);
    }
}
