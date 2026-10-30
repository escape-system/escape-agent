//! Behavioral tests for schedule trigger math and end-limit validation.
//!
//! Every instant is passed explicitly, so the assertions do not depend on the
//! wall clock. The one store-backed test uses a private temp SQLite file.

use oc_store::{
    NewSchedule, NewSession, ScheduleAction, ScheduleEnds, ScheduleOutcome, ScheduleWhen, Store,
    StoreError, duration_ms,
};
use tempfile::TempDir;

fn new_session(id: &str, created: i64) -> NewSession {
    NewSession {
        id: id.to_owned(),
        parent_id: None,
        project_id: "prj_1".to_owned(),
        directory: "/tmp/one".to_owned(),
        title: Some(format!("title {id}")),
        agent: None,
        model: None,
        metadata: None,
        permissions: None,
        time_created: created,
    }
}

fn cron(expression: &str) -> ScheduleWhen {
    ScheduleWhen::Cron {
        cron: expression.to_owned(),
        tz: None,
    }
}

fn every(interval: &str) -> ScheduleWhen {
    ScheduleWhen::Every {
        every: interval.to_owned(),
        tz: None,
    }
}

fn scheduled(
    session: &str,
    when: ScheduleWhen,
    ends: Option<ScheduleEnds>,
    created: i64,
) -> NewSchedule {
    NewSchedule {
        session_id: session.to_owned(),
        name: None,
        when,
        ends,
        action: ScheduleAction::Prompt {
            prompt: "wake".to_owned(),
        },
        time_created: created,
    }
}

#[test]
fn cron_day_and_weekday_restrictions_are_ored() {
    // 1970-01-01 is a Thursday. With both day-of-month and weekday fields
    // restricted, cron fires on either match, so the first Monday (Jan 5)
    // wins over the first day-of-month match (Feb 1).
    assert_eq!(
        cron("0 0 1 * 1").first_due(0).expect("or rule"),
        345_600_000
    );
    assert_eq!(
        cron("0 0 1 * *").first_due(0).expect("day only"),
        2_678_400_000
    );
    assert_eq!(
        cron("0 0 * * 1").first_due(0).expect("weekday only"),
        345_600_000
    );

    // Both directions of the OR rule still hold on later days: just after
    // Feb 1 the weekday branch (Feb 2, a Monday) is earlier than Mar 1…
    assert_eq!(
        cron("0 0 1 * 1")
            .first_due(31 * 86_400_000)
            .expect("weekday branch"),
        32 * 86_400_000
    );
    // …while one millisecond before Mar 1 the day-of-month branch is earlier
    // than the following Monday (Mar 2).
    assert_eq!(
        cron("0 0 1 * 1")
            .first_due(59 * 86_400_000 - 1)
            .expect("day branch"),
        59 * 86_400_000
    );
}

#[test]
fn cron_weekday_seven_is_sunday() {
    // 1970-01-04 is the first Sunday: three days after the epoch.
    let sunday_seven = cron("0 0 * * 7").first_due(0).expect("seven");
    assert_eq!(sunday_seven, 259_200_000);
    assert_eq!(sunday_seven, cron("0 0 * * 0").first_due(0).expect("zero"));
    assert_eq!(
        sunday_seven,
        cron("0 0 * * 0,7").first_due(0).expect("zero and seven")
    );
    // `0,7` is one weekday, not the whole week.
    assert_ne!(
        sunday_seven,
        cron("0 0 * * *").first_due(0).expect("every weekday")
    );
}

#[test]
fn duration_rejects_overflow_negative_and_odd_units() {
    assert_eq!(duration_ms("1 week").expect("week"), 604_800_000);
    assert_eq!(duration_ms("500 ms").expect("millis"), 500);
    assert_eq!(duration_ms("1 hour 30 minutes").expect("pairs"), 5_400_000);

    for bad in [
        // A zero amount in a pair is as invalid as a bare zero duration.
        "1 hour 0 minutes",
        // Negative durations.
        "-1 hours",
        // A trailing amount without its unit.
        "1 hour 30",
        // Unknown units are rejected rather than guessed.
        "1 fortnight",
        // Unit multiplication and plain seconds both overflow i64.
        "9223372036854775807 seconds",
        "9223372036854775807",
        "9223372036854775807 weeks",
    ] {
        assert!(
            matches!(duration_ms(bad), Err(StoreError::Invalid(_))),
            "{bad:?} must be rejected"
        );
    }

    // A repeating trigger may not fire faster than once a minute.
    assert!(matches!(
        every("30 seconds").first_due(0),
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(every("1 minute").first_due(0).expect("minimum"), 60_000);
}

#[tokio::test]
async fn schedule_ends_must_not_precede_the_first_fire_and_times_exhaust() {
    let directory = TempDir::new().expect("temp dir");
    let path = directory.path().join("schedule-when.db");
    let store = Store::open(&path).await.expect("open temp store");
    let created = 1_000_000;
    store
        .create_session(new_session("ses_a", created))
        .await
        .expect("session");

    // The first firing is an hour out, so a one-minute end has already passed.
    let error = store
        .create_schedule(scheduled(
            "ses_a",
            every("1 hour"),
            Some(ScheduleEnds {
                after: Some("1 minute".to_owned()),
                at: None,
                times: None,
            }),
            created,
        ))
        .await
        .expect_err("ends before the first firing");
    assert!(matches!(&error, StoreError::Invalid(_)), "{error:?}");
    assert!(
        error.to_string().contains("ends before it first fires"),
        "{error}"
    );

    // A run-count limit below the first firing is fine and reports the budget.
    let counted = store
        .create_schedule(scheduled(
            "ses_a",
            every("1 hour"),
            Some(ScheduleEnds {
                after: None,
                at: None,
                times: Some(1),
            }),
            created,
        ))
        .await
        .expect("one-run schedule");
    assert_eq!(counted["status"], "active");
    assert_eq!(counted["remainingRuns"], 1);
    assert_eq!(counted["nextDueAt"], created + 3_600_000);
    let id = counted["id"].as_str().expect("schedule id").to_owned();

    // Spending the only allowed run exhausts the schedule.
    let done = store
        .mark_schedule_fired(&id, ScheduleOutcome::Fired, Some(created + 7_200_000), 1)
        .await
        .expect("mark fired")
        .expect("existing schedule");
    assert_eq!(done["status"], "done");
    assert_eq!(done["remainingRuns"], 0);
    assert!(done.get("nextDueAt").is_none());
    assert_eq!(done["lastOutcome"], "fired");
    assert_eq!(done["runCount"], 1);
}
