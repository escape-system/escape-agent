//! Input assembly for one observer model call: active strips, recent strips,
//! the root row and the goal snapshot rendered into [`ObserverInput`].

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
        now: i64,
    ) -> ObserverInput {
        let mut active: Vec<ActiveStrip> = self
            .registry
            .active(root_id)
            .into_iter()
            .map(|strip| {
                let age_s = strip.elapsed_s(now);
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
                let took_s = strip.elapsed_s(now);
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
                .unwrap_or_else(|| "build".to_owned()),
            model: context.model,
            step: context.step,
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
