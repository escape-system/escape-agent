use std::sync::Arc;

use oc_observer::{BoxFuture, ObserverError, RepairExecutor, RepairKind, RepairOutcome};
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::runtime::RuntimeError;

/// A repair command the observer asks the runtime to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairCommand {
    /// Cancel the session's current execution.
    Interrupt {
        /// Target session.
        session_id: String,
    },
    /// Cancel one running tool call (or the running tool call on a session).
    CancelTool {
        /// Provider tool-call id or target session id.
        tool_call_id: String,
    },
    /// Restart a child session.
    RestartSubagent {
        /// Child session.
        session_id: String,
        /// Observer note.
        note: String,
    },
    /// Force compaction on a session.
    Compact {
        /// Target session.
        session_id: String,
    },
    /// Switch a session to another model.
    SwitchModel {
        /// Target session.
        session_id: String,
        /// `provider/id` or `provider/id#variant`.
        model: String,
    },
    /// Re-deliver text to a session.
    Repost {
        /// Target session.
        session_id: String,
        /// Text to inject.
        text: String,
    },
}

/// A connected observer repair pipe.
///
/// The engine receives the [`RepairExecutor`] half; the runtime owns the
/// receiver and applies commands from its repair worker.
#[derive(Debug)]
pub struct RepairChannel;

impl RepairChannel {
    /// Create the executor half and the command receiver.
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> (Arc<dyn RepairExecutor>, UnboundedReceiver<RepairCommand>) {
        let (sender, receiver) = unbounded_channel();
        (Arc::new(ChannelExecutor { sender }), receiver)
    }
}

struct ChannelExecutor {
    sender: UnboundedSender<RepairCommand>,
}

impl RepairExecutor for ChannelExecutor {
    fn apply<'a>(
        &'a self,
        verdict: &'a oc_observer::ObserverVerdict,
    ) -> BoxFuture<'a, Result<RepairOutcome, ObserverError>> {
        Box::pin(async move {
            let command = command_from_verdict(verdict)?;
            self.sender
                .send(command)
                .map_err(|_| ObserverError::Executor("repair worker is stopped".to_owned()))?;
            Ok(RepairOutcome {
                applied: true,
                detail: None,
            })
        })
    }
}

/// Translate a validated repair verdict into a runtime command.
///
/// `retry_tool` has no runtime command yet, so it is reported as an explicit
/// executor error (the engine then publishes `observer.repair.failed`). An
/// absent `switch_model` model or `repost` text is left empty; the runtime
/// resolves those defaults when it applies the command.
fn command_from_verdict(
    verdict: &oc_observer::ObserverVerdict,
) -> Result<RepairCommand, ObserverError> {
    let target = verdict.target.clone().unwrap_or_default();
    if target.is_empty() {
        return Err(ObserverError::Executor(
            "repair verdict has no target".to_owned(),
        ));
    }
    let repair = verdict
        .repair
        .as_ref()
        .ok_or_else(|| ObserverError::Executor("repair verdict has no repair".to_owned()))?;
    let kind = repair
        .kind
        .ok_or_else(|| ObserverError::Executor("repair verdict has no kind".to_owned()))?;
    let args = repair.args.as_object();
    match kind {
        RepairKind::RetryTool => Err(ObserverError::Executor(
            "retry_tool is not wired into the runtime yet".to_owned(),
        )),
        RepairKind::CancelTool => Ok(RepairCommand::CancelTool {
            tool_call_id: target,
        }),
        RepairKind::RestartSubagent => Ok(RepairCommand::RestartSubagent {
            session_id: target,
            note: args
                .and_then(|args| args.get("note"))
                .and_then(Value::as_str)
                .map_or_else(|| verdict.reason.clone(), str::to_owned),
        }),
        RepairKind::CompactContext => Ok(RepairCommand::Compact { session_id: target }),
        RepairKind::SwitchModel => Ok(RepairCommand::SwitchModel {
            session_id: target,
            // An absent model means "use observer.escalation_model"; the
            // runtime resolves it when the command is applied.
            model: args
                .and_then(|args| args.get("model"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        RepairKind::Repost => Ok(RepairCommand::Repost {
            session_id: target,
            // An absent text means "re-deliver the last user prompt"; the
            // runtime loads it from the transcript when the command is applied.
            text: args
                .and_then(|args| args.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
    }
}

/// Resolve the model a `switch_model` repair should apply.
///
/// An explicit model always wins. An empty model falls back to
/// `observer.escalation_model`; without one the repair is a config error and
/// nothing is switched.
pub(super) fn switch_model_target(
    requested: String,
    configured: Option<String>,
    session: &str,
) -> Result<String, RuntimeError> {
    if !requested.is_empty() {
        return Ok(requested);
    }
    configured.ok_or_else(|| {
        RuntimeError::Config(format!(
            "switch_model repair for {session} has no model and \
             observer.escalation_model is not configured"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_observer::ObserverAction;
    use serde_json::json;

    fn verdict(kind: RepairKind, args: Value) -> oc_observer::ObserverVerdict {
        oc_observer::ObserverVerdict {
            action: ObserverAction::Repair,
            target: Some("ses_target".to_owned()),
            reason: "the target is stuck".to_owned(),
            message: Some("verdict message".to_owned()),
            repair: Some(oc_observer::Repair {
                kind: Some(kind),
                args,
            }),
            goal_evidence: Vec::new(),
        }
    }

    #[test]
    fn switch_model_without_args_maps_to_an_empty_model() {
        let command = command_from_verdict(&verdict(RepairKind::SwitchModel, json!({})))
            .expect("missing model is resolved by the runtime");
        assert_eq!(
            command,
            RepairCommand::SwitchModel {
                session_id: "ses_target".to_owned(),
                model: String::new(),
            }
        );
    }

    #[test]
    fn switch_model_keeps_an_explicit_model() {
        let command = command_from_verdict(&verdict(
            RepairKind::SwitchModel,
            json!({"model": "test/beta"}),
        ))
        .expect("explicit model");
        assert_eq!(
            command,
            RepairCommand::SwitchModel {
                session_id: "ses_target".to_owned(),
                model: "test/beta".to_owned(),
            }
        );
    }

    #[test]
    fn repost_without_text_maps_to_empty_text() {
        let command = command_from_verdict(&verdict(RepairKind::Repost, json!({})))
            .expect("missing text is resolved from the transcript");
        assert_eq!(
            command,
            RepairCommand::Repost {
                session_id: "ses_target".to_owned(),
                text: String::new(),
            }
        );
    }

    #[test]
    fn repost_keeps_an_explicit_text() {
        let command =
            command_from_verdict(&verdict(RepairKind::Repost, json!({"text": "try again"})))
                .expect("explicit text");
        assert_eq!(
            command,
            RepairCommand::Repost {
                session_id: "ses_target".to_owned(),
                text: "try again".to_owned(),
            }
        );
    }

    #[test]
    fn switch_model_prefers_the_explicit_model_over_the_fallback() {
        let model = switch_model_target(
            "test/beta".to_owned(),
            Some("test/escalation".to_owned()),
            "ses_target",
        )
        .expect("explicit model wins");
        assert_eq!(model, "test/beta");
    }

    #[test]
    fn empty_switch_model_uses_the_escalation_model() {
        let model = switch_model_target(
            String::new(),
            Some("test/escalation".to_owned()),
            "ses_target",
        )
        .expect("configured escalation model");
        assert_eq!(model, "test/escalation");
    }

    #[test]
    fn empty_switch_model_without_an_escalation_model_is_a_config_error() {
        let error = switch_model_target(String::new(), None, "ses_target")
            .expect_err("no configured escalation model");
        assert!(matches!(error, RuntimeError::Config(_)));
        assert!(error.to_string().contains("observer.escalation_model"));
    }
}
