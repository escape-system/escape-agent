use crate::support::cron_when;
use oc_store::{ScheduleWhen, StoreError, duration_ms};

#[test]
fn schedule_duration_parsing_and_cron_math() {
    assert_eq!(duration_ms("90 seconds").expect("seconds"), 90_000);
    assert_eq!(duration_ms("5 minutes").expect("minutes"), 300_000);
    assert_eq!(duration_ms("2 hours").expect("hours"), 7_200_000);
    assert_eq!(duration_ms("1 day").expect("days"), 86_400_000);
    assert_eq!(duration_ms("1 hour 30 minutes").expect("pairs"), 5_400_000);
    assert_eq!(duration_ms("120").expect("plain seconds"), 120_000);
    assert_eq!(duration_ms("  10 secs ").expect("trimmed"), 10_000);
    for bad in [
        "",
        "soon",
        "0 seconds",
        "-5 minutes",
        "1 hour 30",
        "1.5 hours",
    ] {
        assert!(
            matches!(duration_ms(bad), Err(StoreError::Invalid(_))),
            "{bad:?} must be rejected"
        );
    }

    let one_shot = ScheduleWhen::In {
        r#in: "5 minutes".to_owned(),
        tz: None,
    };
    assert_eq!(one_shot.first_due(1_000).expect("in due"), 301_000);
    assert_eq!(
        one_shot.next_after(10_000_000, 301_000).expect("in next"),
        None
    );

    let every = ScheduleWhen::Every {
        every: "1 minute".to_owned(),
        tz: None,
    };
    assert_eq!(every.first_due(0).expect("every due"), 60_000);
    assert_eq!(
        every.next_after(185_000, 60_000).expect("every next"),
        Some(240_000)
    );
    assert_eq!(
        every.next_after(50_000, 60_000).expect("future previous"),
        Some(60_000)
    );
    assert!(
        matches!(
            ScheduleWhen::Every {
                every: "30 seconds".to_owned(),
                tz: None,
            }
            .first_due(0),
            Err(StoreError::Invalid(_))
        ),
        "every must be at least a minute"
    );

    assert_eq!(
        ScheduleWhen::At {
            at: "1970-01-01T00:05:00Z".to_owned(),
            tz: None,
        }
        .first_due(0)
        .expect("rfc3339"),
        300_000
    );
    assert_eq!(
        ScheduleWhen::At {
            at: "300000".to_owned(),
            tz: None,
        }
        .first_due(299_999)
        .expect("epoch millis"),
        300_000
    );
    assert_eq!(
        ScheduleWhen::At {
            at: "1970-01-01T00:00:01.250Z".to_owned(),
            tz: None,
        }
        .first_due(0)
        .expect("fraction"),
        1_250
    );
    assert_eq!(
        ScheduleWhen::At {
            at: "1970-01-01T05:30:00+05:30".to_owned(),
            tz: None,
        }
        .first_due(-1)
        .expect("explicit offset"),
        0
    );
    assert_eq!(
        ScheduleWhen::At {
            at: "1970-01-01T05:30:00".to_owned(),
            tz: Some("+05:30".to_owned()),
        }
        .first_due(-1)
        .expect("wall clock in offset"),
        0
    );
    for bad in [
        "1970-01-01T00:00:00Z",
        "2026-02-30T00:00:00Z",
        "2026-01-01 25:00:00Z",
        "2026-01-01T00:00:60Z",
        "not a time",
    ] {
        assert!(
            matches!(
                ScheduleWhen::At {
                    at: bad.to_owned(),
                    tz: None,
                }
                .first_due(0),
                Err(StoreError::Invalid(_))
            ),
            "{bad:?} must be rejected"
        );
    }
    assert!(matches!(
        ScheduleWhen::In {
            r#in: "5 minutes".to_owned(),
            tz: Some("Europe/Berlin".to_owned()),
        }
        .first_due(0),
        Err(StoreError::Invalid(_))
    ));

    assert_eq!(
        cron_when("*/5 * * * *", None)
            .first_due(0)
            .expect("five-minute cron"),
        300_000
    );
    assert_eq!(
        cron_when("0 9 * * *", Some("+02:00"))
            .first_due(0)
            .expect("offset cron"),
        25_200_000
    );
    assert_eq!(
        cron_when("0 0 * * 4", None)
            .first_due(0)
            .expect("weekday cron"),
        604_800_000
    );
    assert_eq!(
        cron_when("0,30 9-10 * * 1-5", None)
            .first_due(0)
            .expect("list and range"),
        32_400_000
    );
    assert_eq!(
        cron_when("*/5 * * * *", None)
            .next_after(0, 0)
            .expect("cron next"),
        Some(300_000)
    );
    assert!(matches!(
        cron_when("0 0 31 2 *", None).first_due(0),
        Err(StoreError::Invalid(_))
    ));
    for bad in ["0 0 * *", "60 * * * *", "*/0 * * * *", "0 0 * * MON"] {
        assert!(
            matches!(
                cron_when(bad, None).first_due(0),
                Err(StoreError::Invalid(_))
            ),
            "{bad:?} must be rejected"
        );
    }
}
