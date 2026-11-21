//! Input assembly for one observer model call: active strips, recent strips,
//! the root row and the goal snapshot rendered into [`ObserverInput`].

use crate::clock::EngineNow;
use crate::goal::GoalSnapshot;
use crate::input::{ActiveStrip, ObserverInput, RecentStrip, RootInput, TreeRow};

use super::ObserverEngine;
use super::wake::is_strip_overdue;

impl ObserverEngine {
    pub(super) fn build_input(
        &self,
        root_id: &str,
        tree: &[TreeRow],
        goal: Option<&GoalSnapshot>,
        now: EngineNow,
    ) -> ObserverInput {
        let mut active: Vec<ActiveStrip> = self
            .registry
            .active(root_id)
            .into_iter()
            .map(|strip| {
                let age_s = strip.elapsed_s(now.mono_ms);
                let overdue = is_strip_overdue(&strip, tree, now);
                ActiveStrip {
                    sid: strip.session_id,
                    agent: strip.agent,
                    tool: strip.tool,
                    why: strip.why,
                    age_s,
                    expect_s: strip.expect_s,
                    intervene_s: strip.intervene_s,
                    state: strip.state,
                    overdue,
                }
            })
            .collect();
        active.sort_by_key(|strip| {
            (
                !strip.overdue,
                i64::from(strip.intervene_s).saturating_sub(strip.age_s),
            )
        });

        let recent: Vec<RecentStrip> = self
            .registry
            .recent(root_id, 12)
            .into_iter()
            .map(|strip| {
                let took_s = strip.elapsed_s(now.mono_ms);
                let id = (!strip.tool_call_id.is_empty()).then_some(strip.tool_call_id);
                RecentStrip {
                    id,
                    sid: strip.session_id,
                    tool: strip.tool,
                    outcome: strip.outcome,
                    took_s,
                    why: strip.why,
                }
            })
            .collect();

        let root_row = tree.iter().find(|row| row.sid == root_id);
        let context = self.with_root(root_id, |state| state.root_context.clone());
        let stopped_by_user = self.user_stopped(root_id, tree);
        let root = RootInput {
            id: root_id.to_owned(),
            agent: context
                .agent
                .or_else(|| root_row.map(|row| row.agent.clone()))
                .unwrap_or_else(|| "engineer".to_owned()),
            model: context.model,
            step: context.step,
            step_cap: context.step_cap,
            steps_remaining: context.steps_remaining,
            state: context
                .state
                .or_else(|| root_row.map(|row| row.state.clone()))
                .unwrap_or_else(|| {
                    if active.is_empty() {
                        "idle".to_owned()
                    } else {
                        "busy".to_owned()
                    }
                }),
            stopped_by_user,
        };

        ObserverInput {
            goal: goal.map(GoalSnapshot::to_input),
            root,
            active,
            recent,
            tree: tree.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::clock::{EngineNow, SystemClock};
    use crate::config::ObserverConfig;
    use crate::engine::notice::RepairOutcome;
    use crate::engine::traits::BoxFuture;
    use crate::error::ObserverError;
    use crate::input::RootContext;
    use crate::verdict::ObserverVerdict;

    use super::*;

    struct NeverCalled;

    impl crate::ObserverModel for NeverCalled {
        fn judge<'a>(
            &'a self,
            _input: &'a str,
            _cfg: &'a ObserverConfig,
        ) -> BoxFuture<'a, Result<String, ObserverError>> {
            Box::pin(async { Err(ObserverError::Model("not used".to_owned())) })
        }
    }

    impl crate::RepairExecutor for NeverCalled {
        fn apply<'a>(
            &'a self,
            _verdict: &'a ObserverVerdict,
        ) -> BoxFuture<'a, Result<RepairOutcome, ObserverError>> {
            Box::pin(async { Err(ObserverError::Executor("not used".to_owned())) })
        }
    }

    impl crate::ObserverNotifier for NeverCalled {
        fn notify(&self, _event: crate::ObserverNotice) {}
    }

    #[test]
    fn root_step_budget_reaches_the_observer_input() {
        let noop = Arc::new(NeverCalled);
        let engine = ObserverEngine::new(
            ObserverConfig::default(),
            Arc::new(SystemClock),
            noop.clone(),
            noop.clone(),
            noop,
        );
        engine.set_root_context(
            "ses_root",
            RootContext {
                agent: Some("engineer".to_owned()),
                model: Some("opencode-go/hy4-flash-free".to_owned()),
                step: 45,
                step_cap: Some(50),
                steps_remaining: Some(5),
                state: Some("busy".to_owned()),
            },
        );

        let input = engine.build_input("ses_root", &[], None, EngineNow::from_clock(&SystemClock));
        assert_eq!(input.root.step_cap, Some(50));
        assert_eq!(input.root.steps_remaining, Some(5));

        let rendered = crate::input::render(&input, ObserverConfig::default().max_input_tokens);
        assert!(
            rendered.contains("\"stepCap\":50"),
            "rendered observer input must publish the cap: {rendered}"
        );
        assert!(
            rendered.contains("\"stepsRemaining\":5"),
            "rendered observer input must publish the remaining steps: {rendered}"
        );
    }
}
