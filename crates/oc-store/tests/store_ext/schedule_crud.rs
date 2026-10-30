use crate::support::{ids, schedule_prompt, scheduled_in, seed_session};
use oc_store::{NewSchedule, ScheduleEnds, SchedulePatch, ScheduleWhen, Store};
use serde_json::json;

#[tokio::test]
async fn schedule_create_list_get_update_pause_delete() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;
    seed_session(&store, "ses_b").await;

    let created = store
        .create_schedule(scheduled_in(
            "ses_a",
            Some("wake"),
            "5 minutes",
            "hello",
            1_000_000,
        ))
        .await
        .expect("create schedule");
    let id = created["id"].as_str().expect("schedule id").to_owned();
    assert!(id.starts_with("sch_"), "id has the wire prefix: {id}");
    assert_eq!(
        created,
        json!({
            "id": id.clone(),
            "sessionID": "ses_a",
            "name": "wake",
            "when": { "type": "in", "in": "5 minutes", "tz": "UTC" },
            "tz": "UTC",
            "action": { "type": "prompt", "prompt": "hello" },
            "status": "active",
            "nextDueAt": 1_300_000,
            "runCount": 0,
            "time": { "created": 1_000_000, "updated": 1_000_000 },
        })
    );

    let unnamed = store
        .create_schedule(scheduled_in("ses_a", None, "1 minute", "second", 1_500_000))
        .await
        .expect("unnamed schedule");
    assert!(unnamed.get("name").is_none());

    let repeated = store
        .create_schedule(NewSchedule {
            session_id: "ses_b".to_owned(),
            name: Some("hourly".to_owned()),
            when: ScheduleWhen::Every {
                every: "2 hours".to_owned(),
                tz: None,
            },
            ends: Some(ScheduleEnds {
                after: None,
                at: None,
                times: Some(3),
            }),
            action: schedule_prompt("tick"),
            time_created: 2_000_000,
        })
        .await
        .expect("repeating schedule");
    assert_eq!(repeated["nextDueAt"], 9_200_000);
    assert_eq!(repeated["remainingRuns"], 3);
    assert!(repeated.get("endsAt").is_none());

    let all = store.list_schedules(None).await.expect("list all");
    assert_eq!(
        ids(&all),
        vec![
            id.clone(),
            unnamed["id"].as_str().unwrap().to_owned(),
            repeated["id"].as_str().unwrap().to_owned(),
        ]
    );
    assert_eq!(
        store
            .list_schedules(Some("ses_a"))
            .await
            .expect("list session")
            .len(),
        2
    );
    assert!(
        store
            .list_schedules(Some("ses_missing"))
            .await
            .expect("list missing")
            .is_empty()
    );

    assert_eq!(
        store.get_schedule(&id).await.expect("get schedule"),
        Some(created)
    );
    assert!(
        store
            .get_schedule("sch_missing")
            .await
            .expect("get missing")
            .is_none()
    );

    let patched = store
        .update_schedule(
            &id,
            SchedulePatch {
                name: Some("renamed".to_owned()),
                prompt: Some("again".to_owned()),
                ..SchedulePatch::default()
            },
        )
        .await
        .expect("patch name")
        .expect("existing");
    assert_eq!(patched["name"], "renamed");
    assert_eq!(patched["action"]["prompt"], "again");
    assert_eq!(patched["nextDueAt"], 1_300_000);
    assert_eq!(patched["time"]["created"], 1_000_000);

    let rescheduled = store
        .update_schedule(
            &id,
            SchedulePatch {
                when: Some(ScheduleWhen::In {
                    r#in: "10 minutes".to_owned(),
                    tz: None,
                }),
                ..SchedulePatch::default()
            },
        )
        .await
        .expect("patch when")
        .expect("existing");
    assert_eq!(rescheduled["when"]["in"], "10 minutes");
    assert_eq!(rescheduled["when"]["tz"], "UTC");
    assert!(rescheduled["nextDueAt"].as_i64().unwrap() > 1_300_000);

    let limited = store
        .update_schedule(
            &id,
            SchedulePatch {
                ends: Some(ScheduleEnds {
                    after: None,
                    at: None,
                    times: Some(2),
                }),
                ..SchedulePatch::default()
            },
        )
        .await
        .expect("patch ends")
        .expect("existing");
    assert_eq!(limited["remainingRuns"], 2);
    assert_eq!(limited["status"], "active");

    let paused = store
        .pause_schedule(&id)
        .await
        .expect("pause")
        .expect("existing");
    assert_eq!(paused["status"], "paused");
    assert_eq!(
        store
            .pause_schedule(&id)
            .await
            .expect("pause twice")
            .expect("existing")["status"],
        "paused"
    );
    assert_eq!(
        store
            .resume_schedule(&id)
            .await
            .expect("resume")
            .expect("existing")["status"],
        "active"
    );
    assert_eq!(
        store
            .resume_schedule(&id)
            .await
            .expect("resume twice")
            .expect("existing")["status"],
        "active"
    );
    assert!(
        store
            .pause_schedule("sch_missing")
            .await
            .expect("pause missing")
            .is_none()
    );
    assert!(
        store
            .resume_schedule("sch_missing")
            .await
            .expect("resume missing")
            .is_none()
    );
    assert!(
        store
            .update_schedule("sch_missing", SchedulePatch::default())
            .await
            .expect("patch missing")
            .is_none()
    );

    assert!(store.delete_schedule(&id).await.expect("delete"));
    assert!(!store.delete_schedule(&id).await.expect("delete twice"));
    assert!(
        store
            .get_schedule(&id)
            .await
            .expect("get deleted")
            .is_none()
    );
}
