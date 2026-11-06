//! Scenario tests for the experimental agentic routes: memory, goals,
//! observer audit, subagent tree/messages and the lane 503.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, Request, StatusCode};
use oc_memory::MemoryService;
use oc_server::{AppState, router};
use oc_store::{NewRepair, NewSession, Store};
use oc_types::{ServerInfo, ServerPaths, now_millis};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    _db: tempfile::TempDir,
    app: axum::Router,
    store: Arc<Store>,
    root: String,
    child: String,
}

async fn harness() -> Harness {
    let db = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(db.path().join("data.sqlite")).await.unwrap());
    let directory = db.path().to_string_lossy().into_owned();
    let root = "ses_root".to_owned();
    let child = "ses_child".to_owned();
    for (id, parent) in [(root.as_str(), None), (child.as_str(), Some(root.as_str()))] {
        store
            .create_session(NewSession {
                id: id.to_owned(),
                parent_id: parent.map(str::to_owned),
                project_id: "prj_test".to_owned(),
                directory: directory.clone(),
                title: Some(format!("session {id}")),
                agent: Some("build".to_owned()),
                model: Some(json!({"providerID": "test", "id": "alpha"})),
                metadata: None,
                permissions: None,
                time_created: now_millis(),
            })
            .await
            .unwrap();
    }

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
        db.path().to_path_buf(),
    )
    .with_store(Arc::clone(&store))
    .with_memory(Arc::new(MemoryService::new(Arc::clone(&store))));
    let app = router(state);

    Harness {
        _db: db,
        app,
        store,
        root,
        child,
    }
}

fn store_only_app(store: Arc<Store>, directory: &std::path::Path) -> axum::Router {
    router(
        AppState::new(
            ServerInfo {
                version: "0.0.0-test".to_owned(),
                pid: 4242,
                hostname: None,
                urls: vec![],
                paths: ServerPaths {
                    tmp: std::env::temp_dir().to_string_lossy().into_owned(),
                },
                capabilities: None,
            },
            None,
            directory.to_path_buf(),
        )
        .with_store(store),
    )
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

fn data(value: Option<Value>) -> Value {
    value.expect("body")["data"].clone()
}

#[tokio::test]
async fn memory_crud_search_and_errors() {
    let harness = harness().await;
    let (status, body) = call(
        &harness.app,
        Method::POST,
        "/api/experimental/memory",
        Some(json!({
            "sessionID": harness.root,
            "text": "Use pnpm for installs",
            "kind": "decision",
            "tags": ["tooling"],
            "importance": 4,
            "confidence": 0.9
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "write failed: {body:?}");
    let item = data(body);
    let id = item["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("mem_"));
    assert_eq!(item["scope"], "project");
    assert_eq!(item["scopeID"], "prj_test");
    assert_eq!(item["kind"], "decision");
    assert_eq!(item["importance"], 4);

    let (status, body) = call(
        &harness.app,
        Method::GET,
        "/api/experimental/memory?sessionID=ses_root",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data(body).as_array().unwrap().len(), 1);

    let (status, body) = call(
        &harness.app,
        Method::GET,
        "/api/experimental/memory?sessionID=ses_root&query=pnpm",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "search failed: {body:?}");
    let hits = data(body);
    let hit = &hits.as_array().unwrap()[0];
    assert_eq!(hit["id"], id.as_str());
    assert!(hit["score"].is_number());
    assert_eq!(hit["text"], "Use pnpm for installs");

    let (status, _) = call(
        &harness.app,
        Method::DELETE,
        &format!("/api/experimental/memory/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = call(
        &harness.app,
        Method::DELETE,
        &format!("/api/experimental/memory/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "second delete: {body:?}");

    let (status, body) = call(
        &harness.app,
        Method::POST,
        "/api/experimental/memory",
        Some(json!({"sessionID": harness.root, "text": "api_key=abc123"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "secret: {body:?}");

    let long = "x".repeat(oc_memory::TEXT_CAP + 1);
    let (status, body) = call(
        &harness.app,
        Method::POST,
        "/api/experimental/memory",
        Some(json!({"sessionID": harness.root, "text": long})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "too long: {body:?}");
}

#[tokio::test]
async fn memory_route_is_503_without_service() {
    let db = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(db.path().join("data.sqlite")).await.unwrap());
    let app = store_only_app(Arc::clone(&store), db.path());
    let (status, body) = call(
        &app,
        Method::GET,
        "/api/experimental/memory?scope=global",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let body = body.unwrap();
    assert_eq!(body["_tag"], "ServiceUnavailableError");
    assert_eq!(body["message"], "memory service not attached");
}

#[tokio::test]
async fn goal_set_get_patch_history_conflicts() {
    let harness = harness().await;
    let (status, body) = call(
        &harness.app,
        Method::POST,
        "/api/experimental/goal",
        Some(json!({
            "sessionID": harness.root,
            "objective": "Ship the HTTP surface",
            "criteria": ["tests pass"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "set failed: {body:?}");
    let goal = data(body);
    let first = goal["id"].as_str().unwrap().to_owned();
    assert!(first.starts_with("gol_"));
    assert_eq!(goal["status"], "active");
    assert_eq!(goal["criteria"][0], "tests pass");
    assert_eq!(goal["model"], "test/alpha");

    let (status, body) = call(
        &harness.app,
        Method::GET,
        "/api/experimental/goal?sessionID=ses_child",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data(body)["id"], first.as_str());

    let (status, body) = call(
        &harness.app,
        Method::PATCH,
        &format!("/api/experimental/goal/{first}"),
        Some(json!({"status": "done", "rounds": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch failed: {body:?}");
    let patched = data(body);
    assert_eq!(patched["status"], "done");
    assert_eq!(patched["rounds"], 2);

    let (status, body) = call(
        &harness.app,
        Method::GET,
        "/api/experimental/goal/history?sessionID=ses_root",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data(body).as_array().unwrap().len(), 1);

    let (status, _) = call(
        &harness.app,
        Method::PATCH,
        "/api/experimental/goal/gol_missing",
        Some(json!({"status": "paused"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        &harness.app,
        Method::GET,
        "/api/experimental/goal?sessionID=ses_missing",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(&harness.app, Method::GET, "/api/experimental/goal", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn goal_conflicts_on_reactivation_and_active_root() {
    let harness = harness().await;
    let (status, body) = call(
        &harness.app,
        Method::POST,
        "/api/experimental/goal",
        Some(json!({
            "sessionID": harness.root,
            "objective": "First goal",
            "criteria": ["done"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "set failed: {body:?}");
    let first = data(body)["id"].as_str().unwrap().to_owned();
    let (status, body) = call(
        &harness.app,
        Method::PATCH,
        &format!("/api/experimental/goal/{first}"),
        Some(json!({"status": "done"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch failed: {body:?}");

    let (status, body) = call(
        &harness.app,
        Method::POST,
        "/api/experimental/goal",
        Some(json!({
            "sessionID": harness.root,
            "objective": "Second goal",
            "criteria": ["done"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "second set failed: {body:?}");
    let second = data(body)["id"].as_str().unwrap().to_owned();
    assert!(second.starts_with("gol_"));
    assert_ne!(second, first);

    let (status, body) = call(
        &harness.app,
        Method::PATCH,
        &format!("/api/experimental/goal/{first}"),
        Some(json!({"status": "active"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "reactivation: {body:?}");
    assert_eq!(body.unwrap()["_tag"], "ConflictError");

    // The second goal is still active; another active goal must conflict.
    let (status, body) = call(
        &harness.app,
        Method::POST,
        "/api/experimental/goal",
        Some(json!({
            "sessionID": harness.root,
            "objective": "Third goal",
            "criteria": []
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "active conflict: {body:?}");
}

#[tokio::test]
async fn observer_audit_keeps_durable_repairs_without_engine() {
    let harness = harness().await;
    harness
        .store
        .record_repair(NewRepair {
            id: "obr_test".to_owned(),
            root_id: harness.root.clone(),
            target_id: "tool_1".to_owned(),
            kind: "retry_tool".to_owned(),
            reason: "the tool failed".to_owned(),
            args_hash: "hash".to_owned(),
            attempt: 1,
            time_created: now_millis(),
        })
        .await
        .unwrap();

    let (status, body) = call(
        &harness.app,
        Method::GET,
        "/api/experimental/observer?sessionID=ses_root",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let audit = data(body);
    assert_eq!(audit["enabled"], false);
    assert_eq!(audit["repairs"].as_array().unwrap().len(), 1);
    assert!(audit.get("budget").is_none());

    let (status, body) = call(
        &harness.app,
        Method::GET,
        "/api/experimental/observer/repairs?sessionID=ses_root&limit=5",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let repairs = data(body);
    assert_eq!(repairs.as_array().unwrap().len(), 1);
    assert_eq!(repairs[0]["id"], "obr_test");

    let (status, _) = call(
        &harness.app,
        Method::GET,
        "/api/experimental/observer?sessionID=ses_missing",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn subagent_tree_and_peer_message() {
    let harness = harness().await;
    let (status, body) = call(
        &harness.app,
        Method::GET,
        "/api/experimental/subagent/tree?sessionID=ses_child",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tree failed: {body:?}");
    let tree = data(body);
    assert_eq!(tree["root"], "ses_root");
    let nodes = tree["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0]["id"], "ses_root");
    assert!(nodes[0].get("parentID").is_none());
    assert_eq!(nodes[0]["childCount"], 1);
    assert_eq!(nodes[1]["id"], "ses_child");
    assert_eq!(nodes[1]["parentID"], "ses_root");
    assert_eq!(nodes[1]["depth"], 1);
    assert_eq!(nodes[1]["agent"], "build");
    assert_eq!(nodes[1]["model"], "test/alpha");

    let (status, _) = call(
        &harness.app,
        Method::GET,
        "/api/experimental/subagent/tree?sessionID=ses_missing",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = call(
        &harness.app,
        Method::POST,
        "/api/experimental/subagent/ses_child/message",
        Some(json!({"text": "check the tests", "delivery": "queue"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "message failed: {body:?}");
    let item = data(body);
    assert!(item["id"].as_str().unwrap().starts_with("msg_"));
    assert_eq!(item["sessionID"], "ses_child");
    assert_eq!(item["type"], "synthetic");
    assert_eq!(item["payload"]["text"], "check the tests");
    assert_eq!(item["payload"]["peer"], true);
    assert_eq!(item["delivery"], "queue");

    let inbox = harness.store.list_inbox(&harness.child).await.unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0]["id"], item["id"]);

    let (status, _) = call(
        &harness.app,
        Method::POST,
        "/api/experimental/subagent/ses_missing/message",
        Some(json!({"text": "hello"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn lane_reports_unattached_service() {
    let harness = harness().await;
    let (status, body) = call(&harness.app, Method::GET, "/api/experimental/lane", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let body = body.unwrap();
    assert_eq!(body["_tag"], "ServiceUnavailableError");
    assert_eq!(body["message"], "lane service not attached");
}

#[tokio::test]
async fn observer_check_requires_runtime_and_known_session() {
    let harness = harness().await;

    let (status, body) = call(
        &harness.app,
        Method::POST,
        "/api/experimental/observer/check",
        Some(json!({ "sessionID": harness.root })),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "ServiceUnavailableError");

    let (status, body) = call(
        &harness.app,
        Method::POST,
        "/api/experimental/observer/check",
        Some(json!({ "sessionID": "ses_missing" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "SessionNotFoundError");
}

#[tokio::test]
async fn observer_check_without_observer_is_503() {
    use oc_config::{ConfigService, LoadOptions};
    use oc_llm::{Catalog, LlmClient, SharedLlmClient};
    use oc_runtime::{Runtime, RuntimeConfig};
    use oc_server::{BusSink, PermissionBroker};
    use oc_tools::ToolRegistry;

    let db = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(db.path().join("data.sqlite")).await.unwrap());
    let session = "ses_observer";
    store
        .create_session(NewSession {
            id: session.to_owned(),
            parent_id: None,
            project_id: "prj_observer".to_owned(),
            directory: db.path().to_string_lossy().into_owned(),
            title: Some("observer check".to_owned()),
            agent: Some("build".to_owned()),
            model: Some(json!({"providerID": "test", "id": "alpha"})),
            metadata: None,
            permissions: None,
            time_created: now_millis(),
        })
        .await
        .unwrap();

    let home = db.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let config = Arc::new(
        ConfigService::load(
            LoadOptions::new(db.path())
                .with_env("HOME", home.to_string_lossy().into_owned())
                .with_env("USERPROFILE", home.to_string_lossy().into_owned())
                .with_env(
                    "XDG_CONFIG_HOME",
                    db.path().join("config").to_string_lossy().into_owned(),
                )
                .with_env(
                    "ProgramData",
                    db.path().join("programdata").to_string_lossy().into_owned(),
                )
                .with_env(
                    "OPENCODE_CONFIG",
                    db.path()
                        .join("missing.json")
                        .to_string_lossy()
                        .into_owned(),
                )
                .with_env("OPENCODE_CONFIG_CONTENT", "{}"),
        )
        .expect("config loads"),
    );
    let catalog = Arc::new(Catalog::from_snapshot("{}").expect("catalog"));
    let state = AppState::new(
        ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 4242,
            hostname: None,
            urls: vec![],
            paths: ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        },
        None,
        db.path().to_path_buf(),
    )
    .with_store(Arc::clone(&store))
    .with_config(Arc::clone(&config))
    .with_catalog(Arc::clone(&catalog));

    let broker = Arc::new(PermissionBroker::from_state(&state));
    let sink = Arc::new(BusSink::from_state(&state));
    let runtime = Arc::new(Runtime::new_with_model_client(
        RuntimeConfig {
            store: Arc::clone(&store),
            config: Arc::clone(&config),
            catalog: Arc::clone(&catalog),
            client: Arc::new(SharedLlmClient::new(LlmClient::new().expect("client"))),
            tools: Arc::new(ToolRegistry::with_defaults()),
            sink,
            permissions: broker.clone(),
        },
        Arc::new(NoopModel),
    ));
    let app = router(
        state
            .with_runtime(Arc::clone(&runtime))
            .with_permissions(broker),
    );

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/experimental/observer/check",
        Some(json!({ "sessionID": session })),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "ServiceUnavailableError");
}

/// Never called by the observer-check route; it only satisfies [`RuntimeConfig`].
struct NoopModel;

impl oc_runtime::ModelClient for NoopModel {
    fn stream(
        &self,
        _request: oc_llm::LlmRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<oc_llm::LlmStream, oc_llm::LlmError>> {
        Box::pin(async {
            Ok(futures_util::StreamExt::boxed(
                futures_util::stream::empty::<oc_llm::LlmEvent>(),
            ))
        })
    }
}
