use std::sync::Arc;

use oc_observer::{BoxFuture, ObserverError, RepairExecutor, RepairKind, RepairOutcome};
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

use crate::runtime::RuntimeError;

/// The consumer's answer to a [`RepairCommand`].
///
/// `Ok(outcome)` is a verdict about the repair: `applied: true` only when the
/// runtime performed it, `applied: false` plus `detail` when it refused.
/// `Err(reason)` means the runtime could not even decide (store failure,
/// internal error). Both are answers; silence is not.
pub type RepairReply = oneshot::Sender<Result<RepairOutcome, String>>;

/// A repair command the observer asks the runtime to apply.
///
/// Every variant carries an optional receipt. `ChannelExecutor` fills it and
/// waits for the runtime's verdict, so a command that was merely queued is
/// never reported as a repair that happened. A command built by hand (tests,
/// tooling) leaves it `None` and nobody waits.
///
/// The enum is neither `Clone` nor `PartialEq`: a oneshot sender answers once,
/// so a duplicated command could not carry a working receipt.
#[derive(Debug)]
pub enum RepairCommand {
    /// Cancel the session's current execution.
    Interrupt {
        /// Target session.
        session_id: String,
        /// Receipt channel; set by the executor when a caller waits.
        reply: Option<RepairReply>,
    },
    /// Cancel one running tool call (or the running tool call on a session).
    CancelTool {
        /// Provider tool-call id or target session id.
        tool_call_id: String,
        /// Receipt channel; set by the executor when a caller waits.
        reply: Option<RepairReply>,
    },
    /// Restart a child session.
    RestartSubagent {
        /// Child session.
        session_id: String,
        /// Observer note.
        note: String,
        /// Receipt channel; set by the executor when a caller waits.
        reply: Option<RepairReply>,
    },
    /// Force compaction on a session.
    Compact {
        /// Target session.
        session_id: String,
        /// Receipt channel; set by the executor when a caller waits.
        reply: Option<RepairReply>,
    },
    /// Switch a session to another model.
    SwitchModel {
        /// Target session.
        session_id: String,
        /// `provider/id` or `provider/id#variant`.
        model: String,
        /// Receipt channel; set by the executor when a caller waits.
        reply: Option<RepairReply>,
    },
    /// Re-deliver text to a session.
    Repost {
        /// Target session.
        session_id: String,
        /// Text to inject.
        text: String,
        /// Receipt channel; set by the executor when a caller waits.
        reply: Option<RepairReply>,
    },
    /// Grant extra steps to the running turn of a session.
    ///
    /// The grant is a turn-loop decision (`Runtime::grant_step_budget`), so it
    /// is applied by the repair worker and answered with the real result. It
    /// publishes no wire event of its own.
    RequestBudgetExtension {
        /// Target session.
        session_id: String,
        /// Steps to grant; `None` means `step_budget.extension_steps`.
        steps: Option<u32>,
        /// Receipt channel; set by the executor when a caller waits.
        reply: Option<RepairReply>,
    },
}

impl RepairCommand {
    /// Mutable handle on this command's receipt slot, for every variant.
    fn reply_slot(&mut self) -> &mut Option<RepairReply> {
        match self {
            Self::Interrupt { reply, .. }
            | Self::CancelTool { reply, .. }
            | Self::RestartSubagent { reply, .. }
            | Self::Compact { reply, .. }
            | Self::SwitchModel { reply, .. }
            | Self::Repost { reply, .. }
            | Self::RequestBudgetExtension { reply, .. } => reply,
        }
    }

    /// Take the receipt sender, leaving `None` behind.
    ///
    /// The consumer calls this after it applied (or refused) the command and
    /// answers exactly once; a second call therefore returns `None`.
    pub fn take_reply(&mut self) -> Option<RepairReply> {
        self.reply_slot().take()
    }

    /// Attach a receipt sender.
    #[must_use]
    pub fn with_reply(mut self, reply: RepairReply) -> Self {
        *self.reply_slot() = Some(reply);
        self
    }
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
    /// Send the command and resolve with the runtime's own verdict.
    ///
    /// The wait is bounded by the caller: the observer engine wraps this future
    /// in `ObserverConfig::repair_ack_timeout_ms` and treats a timeout as
    /// *unconfirmed* (a failure). Every other way out of this function except a
    /// receipt is an error:
    ///
    /// - the worker's receiver is gone (`send` fails) -> `Executor`;
    /// - the worker dequeued the command and dropped the receipt without
    ///   answering (shutdown, panic) -> `Executor`.
    ///
    /// No path returns `Ok(RepairOutcome { applied: true, .. })` without the
    /// runtime having said so.
    fn apply<'a>(
        &'a self,
        verdict: &'a oc_observer::ObserverVerdict,
    ) -> BoxFuture<'a, Result<RepairOutcome, ObserverError>> {
        Box::pin(async move {
            let command = command_from_verdict(verdict)?;
            let (reply, receipt) = oneshot::channel();
            self.sender
                .send(command.with_reply(reply))
                .map_err(|_| ObserverError::Executor("repair worker is stopped".to_owned()))?;
            match receipt.await {
                Ok(Ok(outcome)) => Ok(outcome),
                Ok(Err(reason)) => Err(ObserverError::Executor(reason)),
                Err(_dropped) => Err(ObserverError::Executor(
                    "repair worker dropped the command without answering".to_owned(),
                )),
            }
        })
    }
}

/// Translate a validated repair verdict into a runtime command.
///
/// `retry_tool` has no runtime command yet, so it is reported as an explicit
/// executor error (the engine then publishes the failure and returns the
/// reservation). An absent `switch_model` model, `repost` text or
/// `request_budget_extension` step count is left empty/`None`; the runtime
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
            reply: None,
        }),
        RepairKind::RestartSubagent => Ok(RepairCommand::RestartSubagent {
            session_id: target,
            note: args
                .and_then(|args| args.get("note"))
                .and_then(Value::as_str)
                .map_or_else(|| verdict.reason.clone(), str::to_owned),
            reply: None,
        }),
        RepairKind::CompactContext => Ok(RepairCommand::Compact {
            session_id: target,
            reply: None,
        }),
        RepairKind::SwitchModel => Ok(RepairCommand::SwitchModel {
            session_id: target,
            // An absent model means "use observer.escalation_model"; the
            // runtime resolves it when the command is applied.
            model: args
                .and_then(|args| args.get("model"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            reply: None,
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
            reply: None,
        }),
        RepairKind::RequestBudgetExtension => Ok(RepairCommand::RequestBudgetExtension {
            session_id: target,
            // Absent or non-positive means `step_budget.extension_steps`; the
            // worker resolves it from the merged config when it applies.
            steps: args
                .and_then(|args| args.get("steps"))
                .and_then(Value::as_u64)
                .and_then(|steps| u32::try_from(steps).ok())
                .filter(|&steps| steps > 0),
            reply: None,
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
        assert!(
            matches!(&command, RepairCommand::SwitchModel { session_id, model, .. }
                if session_id == "ses_target" && model.is_empty()),
            "unexpected command: {command:?}"
        );
    }

    #[test]
    fn switch_model_keeps_an_explicit_model() {
        let command = command_from_verdict(&verdict(
            RepairKind::SwitchModel,
            json!({"model": "test/beta"}),
        ))
        .expect("explicit model");
        assert!(
            matches!(&command, RepairCommand::SwitchModel { model, .. } if model == "test/beta"),
            "unexpected command: {command:?}"
        );
    }

    #[test]
    fn repost_without_text_maps_to_empty_text() {
        let command = command_from_verdict(&verdict(RepairKind::Repost, json!({})))
            .expect("missing text is resolved from the transcript");
        assert!(
            matches!(&command, RepairCommand::Repost { text, .. } if text.is_empty()),
            "unexpected command: {command:?}"
        );
    }

    #[test]
    fn repost_keeps_an_explicit_text() {
        let command =
            command_from_verdict(&verdict(RepairKind::Repost, json!({"text": "try again"})))
                .expect("explicit text");
        assert!(
            matches!(&command, RepairCommand::Repost { text, .. } if text == "try again"),
            "unexpected command: {command:?}"
        );
    }

    #[test]
    fn a_budget_extension_maps_to_its_own_command_with_the_requested_steps() {
        let command = command_from_verdict(&verdict(
            RepairKind::RequestBudgetExtension,
            json!({"steps": 5}),
        ))
        .expect("budget request");
        assert!(
            matches!(
                &command,
                RepairCommand::RequestBudgetExtension { steps: Some(5), .. }
            ),
            "unexpected command: {command:?}"
        );
    }

    #[test]
    fn a_non_positive_step_count_is_left_to_the_policy_default() {
        for args in [json!({}), json!({"steps": 0}), json!({"steps": -3})] {
            let command =
                command_from_verdict(&verdict(RepairKind::RequestBudgetExtension, args.clone()))
                    .expect("budget request");
            assert!(
                matches!(
                    &command,
                    RepairCommand::RequestBudgetExtension { steps: None, .. }
                ),
                "args {args} must defer to step_budget.extension_steps: {command:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_receipt_can_be_taken_exactly_once() {
        let (reply, receipt) = oneshot::channel();
        let mut command = RepairCommand::CancelTool {
            tool_call_id: "call_1".to_owned(),
            reply: None,
        }
        .with_reply(reply);
        let sender = command.take_reply().expect("first take yields the sender");
        assert!(command.take_reply().is_none(), "a receipt answers once");
        sender
            .send(Ok(RepairOutcome {
                applied: true,
                detail: None,
            }))
            .expect("receiver still waiting");
        let outcome = receipt.await.expect("the reply arrives");
        assert_eq!(outcome.map(|outcome| outcome.applied), Ok(true));
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
