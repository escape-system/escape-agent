//! Scenario tests through the real router.

use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, COOKIE, HOST, LOCATION, ORIGIN, SET_COOKIE,
    WWW_AUTHENTICATE,
};
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use http_body_util::BodyExt;
use oc_server::{AppState, router};
use oc_types::{EventEnvelope, ServerInfo, ServerPaths};
use serde_json::{Value, json};
use tower::ServiceExt;

fn state(password: Option<&str>) -> AppState {
    AppState::new(
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
        password.map(str::to_owned),
        std::env::temp_dir(),
    )
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn info_has_the_upstream_shape() {
    let response = router(state(None))
        .oneshot(
            Request::builder()
                .uri("/api/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["version"], "0.0.0-test");
    assert_eq!(body["pid"], 4242);
    assert!(body["paths"]["tmp"].is_string());
    assert!(body.get("capabilities").is_none());
}

#[tokio::test]
async fn auth_required_and_accepted() {
    let app = router(state(Some("secret")));

    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        unauthorized.headers().get(WWW_AUTHENTICATE).unwrap(),
        "Basic realm=\"Secure Area\""
    );
    let body = json_body(unauthorized).await;
    assert_eq!(body["_tag"], "UnauthorizedError");

    let token = BASE64.encode("opencode:secret");
    let authorized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/info")
                .header(AUTHORIZATION, format!("Basic {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authorized.status(), StatusCode::OK);

    let query_token = token
        .replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D");
    let via_query = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/info?auth_token={query_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(via_query.status(), StatusCode::OK);
}

#[tokio::test]
async fn host_header_must_name_a_served_host() {
    let app = router(state(None));

    for host in ["evil.example", "localhost.evil.example"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/info")
                    .header(HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "accepted {host}");
        assert_eq!(json_body(response).await["_tag"], "ForbiddenError");
    }

    for host in ["127.0.0.1:4096", "localhost:4096", "[::1]:4096"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/info")
                    .header(HOST, host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "rejected {host}");
    }
}

#[tokio::test]
async fn location_is_a_bare_public_info_value() {
    let response = router(state(None))
        .oneshot(
            Request::builder()
                .uri("/api/location")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert!(body["directory"].is_string());
    assert!(body["project"]["id"].as_str().unwrap().starts_with("prj_"));
    assert!(body["project"]["canonical"].is_string());
    assert!(body.get("location").is_none());
    assert!(body.get("data").is_none());
}

#[tokio::test]
async fn location_honours_the_query_and_rejects_missing_directories() {
    let app = router(state(None));
    let directory = tempfile::tempdir().unwrap();
    let uri = format!(
        "/api/location?location%5Bdirectory%5D={}",
        directory.path().to_string_lossy().replace('\\', "%5C")
    );
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(
        body["directory"].as_str().unwrap(),
        directory.path().to_string_lossy()
    );

    let missing = tempfile::tempdir().unwrap().path().join("gone");
    let uri = format!(
        "/api/location?location%5Bdirectory%5D={}",
        missing.to_string_lossy().replace('\\', "%5C")
    );
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = json_body(response).await;
    assert_eq!(body["_tag"], "LocationNotFoundError");
    assert!(body["location"]["directory"].is_string());
}

#[tokio::test]
async fn sse_first_frame_is_connected_and_live_events_follow() {
    let state = state(None);
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/event")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    let mut body = response.into_body();

    let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let first = String::from_utf8(first.into_data().unwrap().to_vec()).unwrap();
    assert!(first.starts_with("data: "), "unexpected frame: {first:?}");
    assert!(first.contains("\"type\":\"server.connected\""));
    assert!(!first.contains("\"created\""));

    state.publish(EventEnvelope::now("test.event", json!({"ok": true})));
    let second = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let second = String::from_utf8(second.into_data().unwrap().to_vec()).unwrap();
    assert!(
        second.contains("\"type\":\"test.event\""),
        "unexpected: {second:?}"
    );
    assert!(second.contains("\"created\""));
}

#[tokio::test]
async fn sse_filters_live_events_by_type() {
    let state = state(None);
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/event?types=test.keep&types=test.other,test.comma")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();

    let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let first = String::from_utf8(first.into_data().unwrap().to_vec()).unwrap();
    assert!(first.contains("\"type\":\"server.connected\""));

    state.publish(EventEnvelope::now("test.drop", json!({})));
    state.publish(EventEnvelope::now("test.keep", json!({})));
    state.publish(EventEnvelope::now("test.other", json!({})));
    state.publish(EventEnvelope::now("test.comma", json!({})));

    let mut types = Vec::new();
    for _ in 0..3 {
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        let value: Value =
            serde_json::from_str(text.strip_prefix("data: ").expect("data frame").trim_end())
                .unwrap();
        types.push(value["type"].as_str().unwrap().to_owned());
    }
    assert_eq!(types, vec!["test.keep", "test.other", "test.comma"]);
}

#[tokio::test]
async fn sse_overflow_ends_the_stream() {
    let state = state(None);
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/event")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();

    // Drain the mandatory first frame, then overflow the 4096-slot broadcast
    // backlog without polling: the subscriber must be closed, not resynced.
    let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8(first.into_data().unwrap().to_vec())
            .unwrap()
            .contains("\"type\":\"server.connected\"")
    );
    for index in 0..5000 {
        state.publish(EventEnvelope::now("test.flood", json!(index)));
    }

    let next = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap();
    assert!(next.is_none(), "lagged stream must end");
}

#[tokio::test]
async fn pairing_requires_auth_and_hands_out_a_session_token() {
    let app = router(state(Some("secret")));

    let anonymous = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/pair")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let pair = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/pair")
                .header(
                    AUTHORIZATION,
                    format!("Basic {}", BASE64.encode("opencode:secret")),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(pair.status(), StatusCode::OK);
    let body = json_body(pair).await;
    let code = body["code"].as_str().unwrap().to_owned();
    assert_eq!(body["expires_in"], 300);

    let connect = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/auth/connect/{code}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(connect.status(), StatusCode::OK);
    let body = json_body(connect).await;
    let token = body["token"].as_str().unwrap().to_owned();
    assert_ne!(token, BASE64.encode("opencode:secret"));
    assert!(oc_server::session_token::verify("secret", &token));

    // Session tokens authenticate anywhere the password does: query and Basic.
    let via_query = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/info?auth_token={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(via_query.status(), StatusCode::OK);

    let via_basic = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/info")
                .header(
                    AUTHORIZATION,
                    format!("Basic {}", BASE64.encode(format!("opencode:{token}"))),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(via_basic.status(), StatusCode::OK);

    let replay = app
        .oneshot(
            Request::builder()
                .uri(format!("/auth/connect/{code}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn pairing_without_a_password_is_unchanged() {
    let app = router(state(None));

    let pair = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/pair")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(pair.status(), StatusCode::OK);
    let code = json_body(pair).await["code"].as_str().unwrap().to_owned();

    let connect = app
        .oneshot(
            Request::builder()
                .uri(format!("/auth/connect/{code}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(connect.status(), StatusCode::OK);
    assert_eq!(json_body(connect).await["token"], "");
}

#[tokio::test]
async fn html_pairing_sets_origin_bound_cookie() {
    let app = router(state(Some("secret")).with_port(4096));

    let pair = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/pair")
                .header(
                    AUTHORIZATION,
                    format!("Basic {}", BASE64.encode("opencode:secret")),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(pair.status(), StatusCode::OK);
    let code = json_body(pair).await["code"].as_str().unwrap().to_owned();

    let connect = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/auth/connect/{code}"))
                .header(ACCEPT, "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(connect.status(), StatusCode::FOUND);
    assert_eq!(connect.headers().get(LOCATION).unwrap(), "/");
    let set_cookie = connect.headers().get(SET_COOKIE).unwrap().to_str().unwrap();
    let cookie = set_cookie.split(';').next().unwrap().to_owned();
    assert!(cookie.starts_with("opencode_session_4096="));
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Lax"));

    let authorized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/info")
                .header(COOKIE, &cookie)
                .header(ORIGIN, "http://127.0.0.1:4096")
                .header(HOST, "127.0.0.1:4096")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authorized.status(), StatusCode::OK);

    let cross_origin = app
        .oneshot(
            Request::builder()
                .uri("/api/info")
                .header(COOKIE, cookie)
                .header(ORIGIN, "http://evil.example")
                .header(HOST, "127.0.0.1:4096")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cross_origin.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn static_serving_uses_spa_and_cache_rules() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("_assets")).unwrap();
    std::fs::write(dir.path().join("index.html"), "<html>ok</html>").unwrap();
    std::fs::write(dir.path().join("_assets/app-abc.js"), "console.log(1)").unwrap();

    let app = router(state(None).with_web_dist(dir.path()));

    let root = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(root.status(), StatusCode::OK);
    assert_eq!(root.headers().get(CONTENT_TYPE).unwrap(), "text/html");
    assert_eq!(root.headers().get("cache-control").unwrap(), "no-cache");

    let asset = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_assets/app-abc.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(asset.status(), StatusCode::OK);
    assert_eq!(
        asset.headers().get("cache-control").unwrap(),
        "public, max-age=31536000, immutable"
    );

    let spa = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/settings?tab=memory")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(spa.status(), StatusCode::OK);
    let bytes = to_bytes(spa.into_body(), 1 << 20).await.unwrap();
    assert_eq!(&bytes[..], b"<html>ok</html>");

    let missing = app
        .oneshot(
            Request::builder()
                .uri("/_assets/missing.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn static_directory_paths_are_404_not_not_ready() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("_assets")).unwrap();
    std::fs::create_dir_all(dir.path().join("icons")).unwrap();
    std::fs::write(dir.path().join("index.html"), "<html>ok</html>").unwrap();
    let app = router(state(None).with_web_dist(dir.path()));

    for uri in ["/_assets", "/icons"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn static_serving_does_not_escape_dist() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "<html>ok</html>").unwrap();
    std::fs::write(dir.path().join("secret.txt"), "secret").unwrap();

    let response = router(state(None).with_web_dist(dir.path()))
        .oneshot(
            Request::builder()
                .uri("/%2e%2e%2fsecret.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert_eq!(&bytes[..], b"<html>ok</html>");
}

#[tokio::test]
async fn unknown_api_and_spa_fallbacks_are_distinct() {
    let app = router(state(None));

    let api = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(api.status(), StatusCode::NOT_FOUND);
    let body = json_body(api).await;
    assert_eq!(body["_tag"], "NotFoundError");

    let page = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        page.headers().get(CONTENT_TYPE).unwrap(),
        "text/plain; charset=utf-8"
    );

    let method = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(method.status(), StatusCode::METHOD_NOT_ALLOWED);
}
