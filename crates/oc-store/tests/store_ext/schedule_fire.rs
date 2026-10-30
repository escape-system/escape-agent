use crate::support::{ids, schedule_prompt, scheduled_in, seed_session};
use oc_store::{NewSchedule, ScheduleEnds, ScheduleOutcome, ScheduleWhen, Store};

#[tokio::test]
async fn schedule_due_query_mark_fired_and_resume() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;

    let slow = store
        .create_schedule(scheduled_in(
            "ses_a",
            Some("slow"),
            "5 minutes",
            "slow",
            1_000_000,
        ))
        .await
        .expect("slow schedule");
    let quick = store
        .create_schedule(scheduled_in(
            "ses_a",
            Some("quick"),
            "1 minute",
            "quick",
            1_000_000,
        ))
        .await
        .expect("quick schedule");
    let paused = store
        .create_schedule(scheduled_in(
            "ses_a",
            Some("paused"),
            "1 minute",
            "paused",
            1_000_000,
        ))
        .await
        .expect("paused schedule");
    store
        .pause_schedule(paused["id"].as_str().unwrap())
        .await
        .expect("pause")
        .expect("existing");
    let limited = store
        .create_schedule(NewSchedule {
            session_id: "ses_a".to_owned(),
            name: Some("twice".to_owned()),
            when: ScheduleWhen::Every {
                every: "1 hour".to_owned(),
                tz: None,
            },
            ends: Some(ScheduleEnds {
                after: None,
                at: None,
                times: Some(2),
            }),
            action: schedule_prompt("twice"),
            time_created: 1_000_000,
        })
        .await
        .expect("limited schedule");
    let ending = store
        .create_schedule(NewSchedule {
            session_id: "ses_a".to_owned(),
            name: None,
            when: ScheduleWhen::Every {
                every: "1 hour".to_owned(),
                tz: None,
            },
            ends: Some(ScheduleEnds {
                after: None,
                at: Some("8200000".to_owned()),
                times: None,
            }),
            action: schedule_prompt("ending"),
            time_created: 1_000_000,
        })
        .await
        .expect("ending schedule");
    assert_eq!(ending["endsAt"], 8_200_000);
    assert_eq!(ending["nextDueAt"], 4_600_000);

    let slow_id = slow["id"].as_str().unwrap().to_owned();
    let quick_id = quick["id"].as_str().unwrap().to_owned();
    let limited_id = limited["id"].as_str().unwrap().to_owned();
    let ending_id = ending["id"].as_str().unwrap().to_owned();

    assert!(
        store
            .due_schedules(1_000_000, 10)
            .await
            .expect("nothing due")
            .is_empty()
    );
    assert_eq!(
        ids(&store.due_schedules(1_060_000, 10).await.expect("quick due")),
        vec![quick_id.clone()]
    );
    assert_eq!(
        ids(&store.due_schedules(1_300_000, 10).await.expect("both due")),
        vec![quick_id.clone(), slow_id.clone()]
    );
    assert_eq!(
        ids(&store.due_schedules(1_300_000, 1).await.expect("limit one")),
        vec![quick_id.clone()]
    );

    let fired = store
        .mark_schedule_fired(&quick_id, ScheduleOutcome::Late, Some(1_360_000), 1)
        .await
        .expect("mark late")
        .expect("existing");
    assert_eq!(fired["lastOutcome"], "late");
    assert_eq!(fired["runCount"], 1);
    assert_eq!(fired["nextDueAt"], 1_360_000);
    assert_eq!(fired["status"], "active");
    assert!(fired["lastFiredAt"].as_i64().unwrap() > 0);
    assert!(fired.get("remainingRuns").is_none());
    assert_eq!(
        ids(&store.due_schedules(1_300_000, 10).await.expect("slow only")),
        vec![slow_id.clone()]
    );

    let spent = store
        .mark_schedule_fired(&slow_id, ScheduleOutcome::Fired, None, 1)
        .await
        .expect("mark done")
        .expect("existing");
    assert_eq!(spent["status"], "done");
    assert_eq!(spent["lastOutcome"], "fired");
    assert_eq!(spent["runCount"], 1);
    assert!(spent.get("nextDueAt").is_none());

    let first = store
        .mark_schedule_fired(&limited_id, ScheduleOutcome::Fired, Some(8_200_000), 1)
        .await
        .expect("mark limited")
        .expect("existing");
    assert_eq!(first["remainingRuns"], 1);
    assert_eq!(first["status"], "active");
    assert_eq!(first["nextDueAt"], 8_200_000);
    let second = store
        .mark_schedule_fired(&limited_id, ScheduleOutcome::Fired, Some(11_800_000), 2)
        .await
        .expect("mark exhausted")
        .expect("existing");
    assert_eq!(second["remainingRuns"], 0);
    assert_eq!(second["status"], "done");
    assert_eq!(second["runCount"], 2);
    assert!(second.get("nextDueAt").is_none());

    let past_limit = store
        .mark_schedule_fired(&ending_id, ScheduleOutcome::Fired, Some(9_000_000), 1)
        .await
        .expect("mark past end")
        .expect("existing");
    assert_eq!(past_limit["status"], "done");
    assert!(past_limit.get("nextDueAt").is_none());

    assert!(
        store
            .mark_schedule_fired("sch_missing", ScheduleOutcome::Fired, None, 1)
            .await
            .expect("mark missing")
            .is_none()
    );

    let paused_info = store
        .resume_schedule(paused["id"].as_str().unwrap())
        .await
        .expect("resume one-shot")
        .expect("existing");
    assert_eq!(paused_info["status"], "active");
    assert_eq!(paused_info["nextDueAt"], 1_060_000);

    let recurring = store
        .create_schedule(NewSchedule {
            session_id: "ses_a".to_owned(),
            name: None,
            when: ScheduleWhen::Every {
                every: "1 hour".to_owned(),
                tz: None,
            },
            ends: None,
            action: schedule_prompt("recurring"),
            time_created: 1_000_000,
        })
        .await
        .expect("recurring schedule");
    let recurring_id = recurring["id"].as_str().unwrap().to_owned();
    store
        .pause_schedule(&recurring_id)
        .await
        .expect("pause recurring")
        .expect("existing");
    let resumed = store
        .resume_schedule(&recurring_id)
        .await
        .expect("resume recurring")
        .expect("existing");
    assert_eq!(resumed["status"], "active");
    assert!(resumed["nextDueAt"].as_i64().unwrap() > 1_000_000_000_000);
}

// ---------------------------------------------------------------------------
// File snapshots
// ---------------------------------------------------------------------------
