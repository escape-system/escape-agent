use crate::support::{cron_when, schedule_prompt, scheduled_in, seed_session};
use oc_store::{NewSchedule, ScheduleEnds, SchedulePatch, ScheduleWhen, Store, StoreError};

#[tokio::test]
async fn schedule_create_validation_and_limits() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;

    assert!(matches!(
        store
            .create_schedule(scheduled_in("ses_missing", None, "5 minutes", "hi", 0))
            .await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store
            .create_schedule(scheduled_in("ses_a", None, "soon", "hi", 0))
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_schedule(scheduled_in("ses_a", None, "5 minutes", "", 0))
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_schedule(NewSchedule {
                session_id: "ses_a".to_owned(),
                name: None,
                when: ScheduleWhen::In {
                    r#in: "5 minutes".to_owned(),
                    tz: Some("Asia/Tokyo".to_owned()),
                },
                ends: None,
                action: schedule_prompt("hi"),
                time_created: 0,
            })
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_schedule(NewSchedule {
                session_id: "ses_a".to_owned(),
                name: None,
                when: cron_when("nope", None),
                ends: None,
                action: schedule_prompt("hi"),
                time_created: 0,
            })
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_schedule(NewSchedule {
                session_id: "ses_a".to_owned(),
                name: None,
                when: ScheduleWhen::At {
                    at: "1970-01-01T00:00:00Z".to_owned(),
                    tz: None,
                },
                ends: None,
                action: schedule_prompt("hi"),
                time_created: 60_000,
            })
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_schedule(NewSchedule {
                session_id: "ses_a".to_owned(),
                name: None,
                when: ScheduleWhen::In {
                    r#in: "5 minutes".to_owned(),
                    tz: None,
                },
                ends: Some(ScheduleEnds {
                    after: None,
                    at: None,
                    times: Some(0),
                }),
                action: schedule_prompt("hi"),
                time_created: 0,
            })
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_schedule(NewSchedule {
                session_id: "ses_a".to_owned(),
                name: None,
                when: ScheduleWhen::In {
                    r#in: "5 minutes".to_owned(),
                    tz: None,
                },
                ends: Some(ScheduleEnds {
                    after: Some("1 minute".to_owned()),
                    at: None,
                    times: None,
                }),
                action: schedule_prompt("hi"),
                time_created: 0,
            })
            .await,
        Err(StoreError::Invalid(_))
    ));

    let bounded = store
        .create_schedule(NewSchedule {
            session_id: "ses_a".to_owned(),
            name: None,
            when: ScheduleWhen::In {
                r#in: "5 minutes".to_owned(),
                tz: None,
            },
            ends: Some(ScheduleEnds {
                after: Some("1 day".to_owned()),
                at: None,
                times: None,
            }),
            action: schedule_prompt("hi"),
            time_created: 1_000_000,
        })
        .await
        .expect("end after");
    assert_eq!(bounded["endsAt"], 1_000_000 + 86_400_000);
    assert_eq!(bounded["nextDueAt"], 1_300_000);

    let exhausted = store
        .create_schedule(NewSchedule {
            session_id: "ses_a".to_owned(),
            name: None,
            when: ScheduleWhen::In {
                r#in: "1 hour".to_owned(),
                tz: None,
            },
            ends: None,
            action: schedule_prompt("hi"),
            time_created: 1_000_000,
        })
        .await
        .expect("active schedule");
    let done = store
        .update_schedule(
            exhausted["id"].as_str().unwrap(),
            SchedulePatch {
                ends: Some(ScheduleEnds {
                    after: None,
                    at: Some("2000000".to_owned()),
                    times: None,
                }),
                ..SchedulePatch::default()
            },
        )
        .await
        .expect("patch past end")
        .expect("existing");
    assert_eq!(done["status"], "done");
    assert!(done.get("nextDueAt").is_none());
    assert_eq!(done["endsAt"], 2_000_000);

    let offset = store
        .create_schedule(NewSchedule {
            session_id: "ses_a".to_owned(),
            name: None,
            when: ScheduleWhen::In {
                r#in: "5 minutes".to_owned(),
                tz: Some("+05:30".to_owned()),
            },
            ends: None,
            action: schedule_prompt("hi"),
            time_created: 0,
        })
        .await
        .expect("offset schedule");
    assert_eq!(offset["tz"], "+05:30");
    assert_eq!(offset["when"]["tz"], "+05:30");
    assert_eq!(offset["nextDueAt"], 300_000);

    let at = store
        .create_schedule(NewSchedule {
            session_id: "ses_a".to_owned(),
            name: Some("at".to_owned()),
            when: ScheduleWhen::At {
                at: "1970-01-01T00:10:00Z".to_owned(),
                tz: None,
            },
            ends: None,
            action: schedule_prompt("hi"),
            time_created: 0,
        })
        .await
        .expect("at schedule");
    assert_eq!(at["nextDueAt"], 600_000);
    assert_eq!(at["when"]["type"], "at");
}
