//! Experimental schedule route tests: CRUD, validation, pause/resume and the
//! exact wire `Schedule.Info` shape.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, Request, StatusCode};
use oc_server::{AppState, router};
use oc_store::{NewSession, Store};
use oc_types::{ServerInfo, ServerPaths};
use serde_json::{Value, json};
use tower::ServiceExt;

const SESSION: &str = "ses_schedule";

struct Harness {
    _db: tempfile::TempDir,
    state: AppState,
}

async fn harness() -> Harness {
    let db = tempfile::tempdir().unwrap();
    let directory = db.path().join("project");
    std::fs::create_dir_all(&directory).unwrap();
    let store = Store::open(db.path().join("data.sqlite")).await.unwrap();
    store
        .create_session(NewSession {
            id: SESSION.to_owned(),
            parent_id: None,
            project_id: "prj_schedule".to_owned(),
            directory: directory.to_string_lossy().into_owned(),
            title: Some("schedule test".to_owned()),
            agent: None,
            model: None,
            metadata: None,
            permissions: None,
            time_created: now_millis(),
        })
        .await
        .unwrap();

    let state = AppState::new(
        ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 4242,
            hostname: Some("127.0.0.1".to_owned()),
            urls: vec!["http://127.0.0.1:4096".to_owned()],
            paths: ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        },
        None,
        directory,
    )
    .with_store(Arc::new(store));

    Harness { _db: db, state }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

async fn call(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Option<Value>) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            builder = builder.header(CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let value = if bytes.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&bytes).unwrap())
    };
    (status, value)
}

/// A wire object must not carry explicit `null`s: absent optionals stay absent.
fn assert_no_nulls(value: &Value, path: &str) {
    match value {
        Value::Null => panic!("null at {path}"),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                assert_no_nulls(item, &format!("{path}[{index}]"));
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                assert_no_nulls(item, &format!("{path}.{key}"));
            }
        }
        _ => {}
    }
}

fn info(value: Option<Value>) -> Value {
    let body = value.expect("expected a JSON body");
    assert_no_nulls(&body, "$");
    body["data"].clone()
}

async fn create(app: &axum::Router, body: Value) -> (StatusCode, Option<Value>) {
    call(app, Method::POST, "/api/experimental/schedule", Some(body)).await
}

#[allow(clippy::too_many_lines)] // one linear CRUD scenario; splitting hides the create-then-patch order
#[tokio::test]
async fn every_schedule_crud_and_wire_shape() {
    let harness = harness().await;
    let app = router(harness.state);

    let (status, body) = create(
        &app,
        json!({
            "sessionID": SESSION,
            "name": "heartbeat",
            "when": {"every": "2 hours"},
            "prompt": "check the inbox",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let created = info(body);
    let id = created["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("sch_"), "{id}");
    assert_eq!(created["sessionID"], SESSION);
    assert_eq!(created["name"], "heartbeat");
    assert_eq!(created["when"]["type"], "every");
    assert_eq!(created["when"]["every"], "2 hours");
    assert!(created["when"]["tz"].is_string());
    assert!(created["tz"].is_string());
    assert_eq!(
        created["action"],
        json!({"type": "prompt", "prompt": "check the inbox"})
    );
    assert_eq!(created["status"], "active");
    assert_eq!(created["runCount"], 0);
    assert!(created["nextDueAt"].is_i64());
    assert!(created["nextDueAt"].as_i64().unwrap() > now_millis());
    assert!(created["time"]["created"].is_i64());
    assert!(created["time"]["updated"].is_i64());
    for absent in ["endsAt", "remainingRuns", "lastFiredAt", "lastOutcome"] {
        assert!(
            created.get(absent).is_none(),
            "{absent} must stay absent: {created}"
        );
    }

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/schedule?sessionID={SESSION}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert_no_nulls(&body, "$");
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["id"], id);

    let (status, body) = call(&app, Method::GET, "/api/experimental/schedule", None).await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert_no_nulls(&body, "$");
    assert_eq!(body["data"][0]["id"], id);

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/schedule/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info(body)["id"], id);

    let (status, body) = call(
        &app,
        Method::PATCH,
        &format!("/api/experimental/schedule/{id}"),
        Some(json!({
            "name": "renamed",
            "when": {"every": "3 hours"},
            "prompt": "updated prompt",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let patched = info(body);
    assert_eq!(patched["id"], id);
    assert_eq!(patched["name"], "renamed");
    assert_eq!(patched["when"]["type"], "every");
    assert_eq!(patched["when"]["every"], "3 hours");
    assert!(patched["when"]["tz"].is_string());
    assert_eq!(patched["action"]["prompt"], "updated prompt");
    assert_eq!(patched["status"], "active");

    let (status, body) = call(
        &app,
        Method::DELETE,
        &format!("/api/experimental/schedule/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_none());

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/schedule/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "ScheduleNotFoundError");
}

#[tokio::test]
async fn in_and_cron_schedules_and_ends() {
    let harness = harness().await;
    let app = router(harness.state);
    let before = now_millis();

    let (status, body) = create(
        &app,
        json!({
            "sessionID": SESSION,
            "when": {"in": "5 minutes"},
            "prompt": "one shot",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let one_shot = info(body);
    assert_eq!(one_shot["when"]["type"], "in");
    assert_eq!(one_shot["when"]["in"], "5 minutes");
    assert!(one_shot["when"]["tz"].is_string());
    let due = one_shot["nextDueAt"].as_i64().unwrap();
    assert!((due - (before + 5 * 60_000)).abs() < 30_000, "{due}");
    assert!(one_shot.get("remainingRuns").is_none());

    let (status, body) = create(
        &app,
        json!({
            "sessionID": SESSION,
            "name": "cron",
            "when": {"cron": "*/5 * * * *"},
            "prompt": "cron tick",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let cron = info(body);
    assert_eq!(cron["when"]["type"], "cron");
    assert_eq!(cron["when"]["cron"], "*/5 * * * *");
    assert!(cron["when"]["tz"].is_string());
    let due = cron["nextDueAt"].as_i64().unwrap();
    assert!(due > before && due <= before + 5 * 60_000 + 5_000, "{due}");

    let (status, body) = create(
        &app,
        json!({
            "sessionID": SESSION,
            "when": {"every": "1 hour"},
            "ends": {"times": 3},
            "prompt": "three times",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let limited = info(body);
    assert_eq!(limited["remainingRuns"], 3);
    assert!(limited.get("endsAt").is_none());
}

#[tokio::test]
async fn invalid_input_answers_400() {
    let harness = harness().await;
    let app = router(harness.state);

    let cases = [
        json!({"sessionID": SESSION, "when": {"in": "not a duration"}, "prompt": "x"}),
        json!({"sessionID": SESSION, "when": {"every": "0 minutes"}, "prompt": "x"}),
        json!({"sessionID": SESSION, "when": {"cron": "not cron"}, "prompt": "x"}),
        json!({"sessionID": SESSION, "when": {"cron": "*/5 * * * *", "tz": "Mars/Olympus"}, "prompt": "x"}),
        json!({"sessionID": SESSION, "when": {"in": "5 minutes"}, "prompt": "   "}),
        json!({"sessionID": SESSION, "when": {"in": "5 minutes", "every": "1 hour"}, "prompt": "x"}),
        json!({"sessionID": SESSION, "when": {}, "prompt": "x"}),
        json!({"sessionID": SESSION, "when": {"every": "1 hour"}, "ends": {"times": 0}, "prompt": "x"}),
    ];
    for body in cases {
        let (status, response) = create(&app, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {response:?}");
        assert_eq!(response.unwrap()["_tag"], "BadRequestError", "{body}");
    }
}

#[tokio::test]
async fn unknown_session_and_schedule_answer_404() {
    let harness = harness().await;
    let app = router(harness.state);

    let (status, body) = create(
        &app,
        json!({"sessionID": "ses_missing", "when": {"in": "5 minutes"}, "prompt": "x"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "SessionNotFoundError");

    let (status, body) = call(
        &app,
        Method::GET,
        "/api/experimental/schedule?sessionID=ses_missing",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "SessionNotFoundError");

    for (method, suffix) in [
        (Method::GET, ""),
        (Method::PATCH, ""),
        (Method::DELETE, ""),
        (Method::POST, "/pause"),
        (Method::POST, "/resume"),
    ] {
        let body = (method == Method::PATCH).then(|| json!({"name": "x"}));
        let (status, response) = call(
            &app,
            method.clone(),
            &format!("/api/experimental/schedule/sch_missing{suffix}"),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {suffix}");
        assert_eq!(
            response.unwrap()["_tag"],
            "ScheduleNotFoundError",
            "{method} {suffix}"
        );
    }
}

#[tokio::test]
async fn pause_and_resume_round_trip() {
    let harness = harness().await;
    let app = router(harness.state);

    let (status, body) = create(
        &app,
        json!({"sessionID": SESSION, "when": {"every": "1 hour"}, "prompt": "tick"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let id = info(body)["id"].as_str().unwrap().to_owned();

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/experimental/schedule/{id}/pause"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let paused = info(body);
    assert_eq!(paused["status"], "paused");
    assert_eq!(paused["id"], id);

    // Pausing an already paused schedule is idempotent.
    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/experimental/schedule/{id}/pause"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info(body)["status"], "paused");

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/experimental/schedule/{id}/resume"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let resumed = info(body);
    assert_eq!(resumed["status"], "active");
    assert!(resumed["nextDueAt"].as_i64().unwrap() > now_millis());
}

#[tokio::test]
async fn every_route_requires_a_store() {
    let state = AppState::new(
        ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 4242,
            hostname: Some("127.0.0.1".to_owned()),
            urls: vec!["http://127.0.0.1:4096".to_owned()],
            paths: ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        },
        None,
        std::env::temp_dir(),
    );
    let app = router(state);

    for (method, uri, body) in [
        (Method::GET, "/api/experimental/schedule", None),
        (
            Method::POST,
            "/api/experimental/schedule",
            Some(json!({"sessionID": SESSION, "when": {"in": "5 minutes"}, "prompt": "x"})),
        ),
        (Method::GET, "/api/experimental/schedule/sch_x", None),
        (
            Method::PATCH,
            "/api/experimental/schedule/sch_x",
            Some(json!({"name": "x"})),
        ),
        (Method::DELETE, "/api/experimental/schedule/sch_x", None),
        (Method::POST, "/api/experimental/schedule/sch_x/pause", None),
        (
            Method::POST,
            "/api/experimental/schedule/sch_x/resume",
            None,
        ),
    ] {
        let (status, response) = call(&app, method, uri, body).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(
            response.unwrap()["_tag"],
            "ServiceUnavailableError",
            "{uri}"
        );
    }
}
