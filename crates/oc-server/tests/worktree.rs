//! Worktree routes through the real router and the real `git` executable.
//!
//! Tests that need git skip when it is not installed, following the convention
//! in `tests/vcs_project.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use axum::http::{Method, Request, StatusCode};
use oc_server::{AppState, router};
use oc_store::{NewWorktree, Store};
use oc_types::{ServerInfo, ServerPaths};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    _db: tempfile::TempDir,
    directory: tempfile::TempDir,
    state: AppState,
}

async fn harness() -> Harness {
    let db = tempfile::tempdir().unwrap();
    let store = Store::open(db.path().join("data.sqlite")).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let state = state_for(&directory, store);
    Harness {
        _db: db,
        directory,
        state,
    }
}

fn state_for(directory: &tempfile::TempDir, store: Store) -> AppState {
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
        None,
        directory.path().to_path_buf(),
    )
    .with_store(Arc::new(store))
}

/// Send one request, optionally with a directory header.
async fn call(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    directory: Option<&Path>,
) -> (StatusCode, Option<Value>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(directory) = directory {
        builder = builder.header(
            HeaderName::from_static("x-opencode-directory"),
            HeaderValue::from_str(&directory.to_string_lossy()).unwrap(),
        );
    }
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

fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("git {args:?} failed to spawn: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Explicit skip: return `false` with a visible note rather than silently
/// letting the test body "pass" when the prerequisite is missing.
fn require_git() -> bool {
    if git_available() {
        return true;
    }
    eprintln!("skipping worktree test: git is not installed");
    false
}

/// Worktree directories as git lists them, canonicalised for comparison.
fn porcelain_directories(repository: &Path) -> Vec<PathBuf> {
    git(repository, &["worktree", "list", "--porcelain"])
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|path| canonical(path.trim_matches('"')))
        .collect()
}

/// Initialize a repository with one commit containing `a.txt`.
fn init_repository(directory: &Path) {
    git(directory, &["init", "-q"]);
    git(directory, &["config", "user.email", "test@example.invalid"]);
    git(directory, &["config", "user.name", "Test"]);
    std::fs::write(directory.join("a.txt"), "one\ntwo\n").unwrap();
    git(directory, &["add", "."]);
    git(directory, &["commit", "-qm", "init"]);
}

/// Create the project row by creating one session in the location.
async fn project_id(app: &axum::Router) -> String {
    let (status, body) = call(
        app,
        Method::POST,
        "/api/session",
        Some(json!({"title": "worktree owner"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let (status, body) = call(app, Method::GET, "/api/project", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let projects = body.unwrap();
    let projects = projects.as_array().expect("bare array");
    assert_eq!(projects.len(), 1, "{projects:?}");
    projects[0]["id"].as_str().unwrap().to_owned()
}

fn canonical(path: impl AsRef<Path>) -> PathBuf {
    std::fs::canonicalize(path.as_ref()).unwrap_or_else(|_| path.as_ref().to_path_buf())
}

fn listed_directories(body: &Value) -> Vec<PathBuf> {
    body.as_array()
        .unwrap_or_else(|| panic!("expected bare array: {body:?}"))
        .iter()
        .map(|row| canonical(row["directory"].as_str().expect("directory")))
        .collect()
}

/// Run refresh and require 204.
async fn refresh(app: &axum::Router, project: &str) {
    let (status, body) = call(
        app,
        Method::POST,
        "/api/worktree/refresh",
        Some(json!({ "projectID": project })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");
}

/// The project's saved inventory, canonicalised.
async fn list_project(app: &axum::Router, project: &str) -> Vec<PathBuf> {
    let (status, body) = call(
        app,
        Method::GET,
        &format!("/api/worktree?projectID={project}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    listed_directories(&body.unwrap())
}

/// Persist an inventory row directly, as a pre-fix build would have.
async fn insert_row(store: &Store, project: &str, directory: &str) {
    store
        .create_worktree(NewWorktree {
            id: None,
            project_id: project.to_owned(),
            directory: directory.to_owned(),
            canonical: Some(directory.to_owned()),
            branch: None,
            name: None,
            time_created: None,
            time_updated: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn worktree_create_list_and_remove_use_real_rows() {
    if !git_available() {
        return;
    }
    let harness = harness().await;
    let root = harness.directory.path();
    init_repository(root);
    let app = router(harness.state.clone());
    let project = project_id(&app).await;
    let base = root.join(".worktrees");

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/worktree",
        Some(json!({
            "projectID": project,
            "directory": base.to_string_lossy(),
            "name": "api",
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let body = body.unwrap();
    let created = body["directory"]
        .as_str()
        .unwrap_or_else(|| panic!("expected directory: {body:?}"))
        .to_owned();
    assert!(Path::new(&created).join(".git").exists(), "{created}");
    assert_eq!(canonical(&created), canonical(base.join("api")));
    let listed = porcelain_directories(root);
    assert!(listed.contains(&canonical(&created)), "{created}");

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/worktree?projectID={project}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let listed = listed_directories(&body.unwrap());
    assert_eq!(listed, vec![canonical(&created)], "{listed:?}");

    let (status, body) = call(
        &app,
        Method::DELETE,
        "/api/worktree",
        Some(json!({"projectID": project, "directory": created})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");
    assert!(!base.join("api").exists());

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/worktree?projectID={project}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert!(listed_directories(&body.unwrap()).is_empty());

    // The row is gone, so a second delete is a 404.
    let (status, _) = call(
        &app,
        Method::DELETE,
        "/api/worktree",
        Some(json!({"projectID": project, "directory": created})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn worktree_rejects_paths_outside_the_allowed_roots() {
    if !git_available() {
        return;
    }
    let harness = harness().await;
    let root = harness.directory.path();
    init_repository(root);
    let app = router(harness.state.clone());
    let project = project_id(&app).await;

    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("escape");
    let (status, body) = call(
        &app,
        Method::POST,
        "/api/worktree",
        Some(json!({
            "projectID": project,
            "directory": target.to_string_lossy(),
            "name": "escape",
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert!(!target.exists());
    assert_eq!(body.unwrap()["_tag"], "WorktreeError");

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/worktree",
        Some(json!({
            "projectID": project,
            "directory": root.join(".worktrees").to_string_lossy(),
            "name": "../escape",
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert!(!root.join("escape").exists());

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/worktree",
        Some(json!({
            "projectID": project,
            "directory": "relative/worktrees",
            "name": "api",
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
}

#[tokio::test]
async fn worktree_unknown_project_is_404_for_every_route() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let (status, body) = call(
        &app,
        Method::GET,
        "/api/worktree?projectID=prj_missing",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "ProjectNotFoundError");

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/worktree",
        Some(json!({"projectID": "prj_missing", "name": "api"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        &app,
        Method::DELETE,
        "/api/worktree",
        Some(json!({"projectID": "prj_missing", "directory": "/tmp/nope"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/worktree/refresh",
        Some(json!({"projectID": "prj_missing"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn worktree_refresh_excludes_main_worktree_and_heals_inventory() {
    if !git_available() {
        return;
    }
    let harness = harness().await;
    let root = harness.directory.path();
    init_repository(root);
    let app = router(harness.state.clone());
    let project = project_id(&app).await;

    // The repository exists but has no linked worktree yet: the main working
    // tree is not part of the inventory.
    refresh(&app, &project).await;
    assert!(
        list_project(&app, &project).await.is_empty(),
        "refresh must not list the main working tree"
    );

    let external = root.join(".worktrees").join("external tree");
    std::fs::create_dir_all(external.parent().unwrap()).unwrap();
    let external_text = external.to_string_lossy().into_owned();
    git(
        root,
        &["worktree", "add", "--detach", "--", &external_text, "HEAD"],
    );

    refresh(&app, &project).await;
    assert_eq!(
        list_project(&app, &project).await,
        vec![canonical(&external)],
        "only the linked worktree belongs in the inventory"
    );

    // Refreshing again is idempotent: no duplicate and no main row.
    refresh(&app, &project).await;
    refresh(&app, &project).await;
    assert_eq!(
        list_project(&app, &project).await,
        vec![canonical(&external)]
    );

    // A row naming the main working tree (something a pre-fix refresh saved)
    // is refused by DELETE but healed by the next refresh. Windows spells the
    // path with different case to prove comparison is canonical, not textual.
    let store = harness.state.store().expect("store").as_ref();
    #[cfg(windows)]
    let polluted = root.to_string_lossy().to_uppercase();
    #[cfg(not(windows))]
    let polluted = root.to_string_lossy().into_owned();
    insert_row(store, &project, &polluted).await;
    assert_eq!(list_project(&app, &project).await.len(), 2);

    let (status, body) = call(
        &app,
        Method::DELETE,
        "/api/worktree",
        Some(json!({"projectID": project, "directory": polluted})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "WorktreeError");
    assert_eq!(
        list_project(&app, &project).await.len(),
        2,
        "a refused main-tree delete leaves the row in place"
    );

    refresh(&app, &project).await;
    assert_eq!(
        list_project(&app, &project).await,
        vec![canonical(&external)],
        "refresh must delete a persisted main-worktree row"
    );

    // A checkout that disappears is pruned from the inventory as before.
    std::fs::remove_dir_all(&external).unwrap();
    refresh(&app, &project).await;
    assert!(list_project(&app, &project).await.is_empty());
}

#[tokio::test]
async fn worktree_refresh_for_project_rooted_at_a_linked_worktree_excludes_main_and_self() {
    if !git_available() {
        return;
    }
    let harness = harness().await;
    let root = harness.directory.path();
    init_repository(root);
    let self_tree = root.join(".worktrees").join("self tree");
    std::fs::create_dir_all(self_tree.parent().unwrap()).unwrap();
    let self_text = self_tree.to_string_lossy().into_owned();
    git(
        root,
        &["worktree", "add", "--detach", "--", &self_text, "HEAD"],
    );

    let app = router(harness.state.clone());
    let (status, body) = call(
        &app,
        Method::POST,
        "/api/session",
        Some(json!({
            "title": "linked owner",
            "location": {"directory": self_text},
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let project = body.unwrap()["data"]["projectID"]
        .as_str()
        .unwrap()
        .to_owned();

    // `rev-parse --show-toplevel` is the linked worktree itself, not the
    // repository's main tree; refresh must still exclude both.
    refresh(&app, &project).await;
    assert!(
        list_project(&app, &project).await.is_empty(),
        "neither the main tree nor the project's own checkout is inventory"
    );

    let other = root.join(".worktrees").join("other tree");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    let other_text = other.to_string_lossy().into_owned();
    git(
        root,
        &["worktree", "add", "--detach", "--", &other_text, "HEAD"],
    );
    refresh(&app, &project).await;
    assert_eq!(list_project(&app, &project).await, vec![canonical(&other)]);
}

#[tokio::test]
async fn worktree_refresh_non_git_directory_is_400() {
    if !git_available() {
        return;
    }
    let harness = harness().await;
    let app = router(harness.state.clone());
    let project = project_id(&app).await;

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/worktree/refresh",
        Some(json!({"projectID": project})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "WorktreeError");
}

#[tokio::test]
async fn worktree_relative_base_is_rejected_before_git() {
    // Deliberately no git repository: the relative-path policy check must run
    // first, so the response is the base-directory rejection rather than a
    // repository error from a git invocation.
    let harness = harness().await;
    let app = router(harness.state.clone());
    let project = project_id(&app).await;

    let relative = Path::new("relative-worktrees").join("api");
    let (status, body) = call(
        &app,
        Method::POST,
        "/api/worktree",
        Some(json!({
            "projectID": project,
            "directory": relative.to_string_lossy(),
            "name": "api",
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    let body = body.unwrap();
    assert_eq!(body["_tag"], "WorktreeError");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("absolute")),
        "expected the absolute-directory rejection, got {body:?}"
    );
    assert!(
        list_project(&app, &project).await.is_empty(),
        "a rejected relative base must not add an inventory row"
    );
}

#[tokio::test]
async fn worktree_absolute_base_outside_the_project_creates_no_inventory_row() {
    // Also git-free: confinement is checked before any repository lookup.
    let harness = harness().await;
    let app = router(harness.state.clone());
    let project = project_id(&app).await;

    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("escape");
    let (status, body) = call(
        &app,
        Method::POST,
        "/api/worktree",
        Some(json!({
            "projectID": project,
            "directory": target.to_string_lossy(),
            "name": "escape",
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    let body = body.unwrap();
    assert_eq!(body["_tag"], "WorktreeError");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("outside")),
        "expected the confinement rejection, got {body:?}"
    );
    assert!(!target.exists(), "the rejected base must not be created");
    assert!(
        list_project(&app, &project).await.is_empty(),
        "a rejected base must not add an inventory row"
    );
}

#[tokio::test]
async fn worktree_delete_unknown_is_404_and_collection_delete_is_204() {
    if !require_git() {
        return;
    }
    let harness = harness().await;
    let root = harness.directory.path();
    init_repository(root);
    let app = router(harness.state.clone());
    let project = project_id(&app).await;
    let base = root.join(".worktrees");

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/worktree",
        Some(json!({
            "projectID": project,
            "directory": base.to_string_lossy(),
            "name": "api",
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let created = body.unwrap()["directory"]
        .as_str()
        .expect("created directory")
        .to_owned();
    assert_eq!(
        list_project(&app, &project).await,
        vec![canonical(&created)]
    );

    // Deleting a directory that is not in the inventory is a 404 and must not
    // touch the rows or the checkout.
    let unknown = base.join("never-created");
    let (status, body) = call(
        &app,
        Method::DELETE,
        "/api/worktree",
        Some(json!({
            "projectID": project,
            "directory": unknown.to_string_lossy(),
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");
    let body = body.unwrap();
    assert_eq!(body["_tag"], "WorktreeNotFoundError");
    assert_eq!(body["directory"], unknown.to_string_lossy().as_ref());
    assert_eq!(
        list_project(&app, &project).await,
        vec![canonical(&created)],
        "a 404 delete must leave the inventory intact"
    );
    assert!(Path::new(&created).join(".git").exists());

    // Deleting the collection (no directory) removes every saved worktree.
    let (status, body) = call(
        &app,
        Method::DELETE,
        "/api/worktree",
        Some(json!({ "projectID": project })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");
    assert!(
        list_project(&app, &project).await.is_empty(),
        "collection delete must empty the inventory"
    );
    assert!(
        !Path::new(&created).exists(),
        "collection delete must remove the checkout"
    );
}
