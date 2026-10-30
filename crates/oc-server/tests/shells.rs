//! `/api/shell` and persistent-PTY alias scenario tests.
//!
//! Shell commands and terminals spawn real OS processes; tests that cannot
//! spawn on the host report a skip. A static lock serialises tests because the
//! shell manager is process-global.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, Request, StatusCode};
use oc_pty::{PtyConfig, PtyManager};
use oc_server::{AppState, router};
use oc_store::Store;
use oc_types::{ServerInfo, ServerPaths};
use serde_json::{Value, json};
use tokio::time::timeout;
use tower::ServiceExt;

const WAIT: Duration = Duration::from_secs(10);

static SHELL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Harness {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    _db: tempfile::TempDir,
    _dir: tempfile::TempDir,
    state: AppState,
    manager: Arc<PtyManager>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        oc_server::data::shells::manager().shutdown();
        self.manager.shutdown();
    }
}

async fn harness() -> Harness {
    let guard = SHELL_LOCK.lock().await;
    let db = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(db.path().join("shells.sqlite")).await.unwrap();
    let manager = Arc::new(PtyManager::new(PtyConfig::default()));
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
        dir.path().to_path_buf(),
    )
    .with_store(Arc::new(store))
    .with_pty(Arc::clone(&manager));
    Harness {
        _guard: guard,
        _db: db,
        _dir: dir,
        state,
        manager,
    }
}

async fn call(
    state: &AppState,
    method: Method,
    uri: &str,
    body: Option<Value>,
    ticket_header: bool,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            builder = builder.header(CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    if ticket_header {
        builder = builder.header("x-opencode-ticket", "1");
    }
    let response = router(state.clone())
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

fn echo_command(text: &str) -> String {
    format!("echo {text}")
}

fn sleeper() -> &'static str {
    if cfg!(windows) {
        "ping -n 10 127.0.0.1 >nul"
    } else {
        "sleep 10"
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HostShell {
    Cmd,
    PowerShell,
    Posix,
}

/// Mirrors the manager's platform-shell discovery so the test speaks the same
/// command language the spawned process will parse.
fn program_on_path(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|directory| {
        if directory.join(program).is_file() {
            return true;
        }
        #[cfg(windows)]
        {
            if std::path::Path::new(program).extension().is_none() {
                return ["exe", "cmd", "bat", "com"]
                    .iter()
                    .any(|extension| directory.join(format!("{program}.{extension}")).is_file());
            }
        }
        false
    })
}

fn host_shell() -> HostShell {
    if cfg!(windows) {
        if program_on_path("pwsh") || program_on_path("powershell") {
            HostShell::PowerShell
        } else {
            HostShell::Cmd
        }
    } else {
        HostShell::Posix
    }
}

/// Delay briefly so the running shell is observable in `GET /api/shell`, then
/// print the exported session id and exit.
fn session_id_command() -> String {
    match host_shell() {
        HostShell::Cmd => "ping -n 2 127.0.0.1 >nul & echo %OPENCODE_SESSION_ID%".to_owned(),
        HostShell::PowerShell => {
            "Start-Sleep -Milliseconds 300; Write-Output $env:OPENCODE_SESSION_ID".to_owned()
        }
        HostShell::Posix => "sleep 0.3; printf '%s\\n' \"$OPENCODE_SESSION_ID\"".to_owned(),
    }
}

async fn create_shell(state: &AppState, command: &str) -> Option<Value> {
    let (status, body) = call(
        state,
        Method::POST,
        "/api/shell",
        Some(json!({ "command": command })),
        false,
    )
    .await;
    if status != StatusCode::OK {
        eprintln!("skipping shell test: create failed with {status}: {body}");
        return None;
    }
    Some(body)
}

async fn wait_exited(state: &AppState, id: &str) -> Value {
    timeout(WAIT, async {
        loop {
            let (_, body) =
                call(state, Method::GET, &format!("/api/shell/{id}"), None, false).await;
            if body["data"]["status"] != "running" {
                return body["data"].clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("shell {id} did not exit"))
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one linear round trip; splitting hides the cursor contract
async fn shell_round_trip_cursor_and_delete() {
    let harness = harness().await;
    let mut events = harness.state.bus().subscribe();
    let Some(created) = create_shell(&harness.state, &echo_command("shell-hello")).await else {
        return;
    };
    let info = &created["data"];
    let id = info["id"].as_str().expect("shell id").to_owned();
    assert!(id.starts_with("sh_"), "{info}");
    assert!(info["file"].as_str().is_some_and(|file| {
        std::path::Path::new(file)
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("out"))
    }));
    assert!(
        info["time"]["started"]
            .as_i64()
            .is_some_and(|time| time > 0)
    );
    assert_eq!(info["metadata"], json!({}));
    assert!(created["location"]["directory"].is_string());

    // `shell.created` is published with the same info.
    let mut seen_created = false;
    for _ in 0..8 {
        let Ok(event) = timeout(Duration::from_millis(250), events.recv()).await else {
            break;
        };
        let Ok(event) = event else { continue };
        if event.kind == "shell.created" {
            assert_eq!(event.data["info"]["id"], id.as_str());
            assert!(event.location.is_some());
            seen_created = true;
            break;
        }
    }
    assert!(seen_created, "shell.created was not published");

    let exited = wait_exited(&harness.state, &id).await;
    assert_eq!(exited["status"], "exited", "{exited}");
    assert_eq!(exited["exit"], 0, "{exited}");
    assert!(exited["time"]["completed"].as_i64().is_some());

    let (status, page) = call(
        &harness.state,
        Method::GET,
        &format!("/api/shell/{id}/output"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let output = &page["data"];
    assert!(
        output["output"]
            .as_str()
            .is_some_and(|text| text.contains("shell-hello")),
        "{output}"
    );
    let size = output["size"].as_u64().expect("size");
    assert_eq!(output["cursor"].as_u64(), Some(size));
    assert_eq!(output["truncated"], false);

    // Past the end: empty page pinned to the current size.
    let (status, empty) = call(
        &harness.state,
        Method::GET,
        &format!("/api/shell/{id}/output?cursor={size}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty["data"]["output"], "");
    assert_eq!(empty["data"]["cursor"].as_u64(), Some(size));
    assert_eq!(empty["data"]["size"].as_u64(), Some(size));

    // Limited pages advance the absolute cursor.
    let (_, first) = call(
        &harness.state,
        Method::GET,
        &format!("/api/shell/{id}/output?cursor=0&limit=4"),
        None,
        false,
    )
    .await;
    assert_eq!(first["data"]["cursor"].as_u64(), Some(4));
    assert_eq!(first["data"]["size"].as_u64(), Some(size));
    assert_eq!(first["data"]["output"].as_str().unwrap().len(), 4);
    let (_, second) = call(
        &harness.state,
        Method::GET,
        &format!("/api/shell/{id}/output?cursor=4&limit=4"),
        None,
        false,
    )
    .await;
    assert_eq!(second["data"]["cursor"].as_u64(), Some(8));

    // Invalid cursors are rejected, not guessed.
    let (status, error) = call(
        &harness.state,
        Method::GET,
        &format!("/api/shell/{id}/output?cursor=abc"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["_tag"], "InvalidRequestError");

    // Exited shells are retained by `get` but absent from `list`.
    let (_, listing) = call(&harness.state, Method::GET, "/api/shell", None, false).await;
    assert!(
        !listing["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"] == id.as_str()),
        "{listing}"
    );

    let file = exited["file"].as_str().unwrap().to_owned();
    assert!(std::path::Path::new(&file).is_file());
    let (status, _) = call(
        &harness.state,
        Method::DELETE,
        &format!("/api/shell/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!std::path::Path::new(&file).exists());
    let (status, missing) = call(
        &harness.state,
        Method::GET,
        &format!("/api/shell/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["_tag"], "ShellNotFoundError");
}

#[tokio::test]
async fn deleting_a_running_shell_kills_it() {
    let harness = harness().await;
    let Some(created) = create_shell(&harness.state, sleeper()).await else {
        return;
    };
    let id = created["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(created["data"]["status"], "running");

    let (status, _) = call(
        &harness.state,
        Method::DELETE,
        &format!("/api/shell/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &harness.state,
        Method::GET,
        &format!("/api/shell/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn timeout_kills_and_reports_timeout() {
    let harness = harness().await;
    let (status, created) = call(
        &harness.state,
        Method::POST,
        "/api/shell",
        Some(json!({ "command": sleeper(), "timeout": 100 })),
        false,
    )
    .await;
    if status != StatusCode::OK {
        eprintln!("skipping shell timeout test: create failed {status}: {created}");
        return;
    }
    let id = created["data"]["id"].as_str().unwrap().to_owned();
    let exited = wait_exited(&harness.state, &id).await;
    assert_eq!(exited["status"], "timeout", "{exited}");
    assert!(exited.get("exit").is_none(), "{exited}");
    let (status, _) = call(
        &harness.state,
        Method::DELETE,
        &format!("/api/shell/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn session_shell_uses_session_directory_and_requires_session() {
    let harness = harness().await;
    let (status, session) = call(
        &harness.state,
        Method::POST,
        "/api/session",
        Some(json!({ "title": "shell session" })),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let sid = session["data"]["id"].as_str().unwrap().to_owned();

    let (status, _) = call(
        &harness.state,
        Method::POST,
        &format!("/api/session/{sid}/shell"),
        Some(json!({ "command": sleeper() })),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The shell is running with the session metadata and directory.
    let (_, listing) = call(&harness.state, Method::GET, "/api/shell", None, false).await;
    let found = listing["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["metadata"]["sessionID"] == sid.as_str())
        .cloned()
        .expect("session shell in list");
    assert_eq!(found["metadata"]["background"], true);
    assert_eq!(
        found["cwd"].as_str().unwrap(),
        session["data"]["location"]["directory"].as_str().unwrap()
    );
    let id = found["id"].as_str().unwrap().to_owned();
    let (status, _) = call(
        &harness.state,
        Method::DELETE,
        &format!("/api/shell/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, error) = call(
        &harness.state,
        Method::POST,
        "/api/session/ses_missing/shell",
        Some(json!({ "command": "echo nope" })),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error["_tag"], "SessionNotFoundError");
}

#[tokio::test]
async fn session_shell_exports_the_session_id_and_records_metadata() {
    let harness = harness().await;
    let (status, session) = call(
        &harness.state,
        Method::POST,
        "/api/session",
        Some(json!({ "title": "shell env session" })),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let sid = session["data"]["id"].as_str().unwrap().to_owned();

    let (status, body) = call(
        &harness.state,
        Method::POST,
        &format!("/api/session/{sid}/shell"),
        Some(json!({ "command": session_id_command() })),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // The endpoint answers 204, so the running shell is located by the session
    // metadata it records. It sleeps briefly, keeping that window reliable.
    let found = timeout(WAIT, async {
        loop {
            let (_, listing) = call(&harness.state, Method::GET, "/api/shell", None, false).await;
            if let Some(entry) = listing["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["metadata"]["sessionID"] == sid.as_str())
            {
                return entry.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("session shell was never listed while running");
    assert_eq!(found["metadata"]["sessionID"], sid.as_str());
    assert_eq!(found["metadata"]["background"], true);
    let id = found["id"].as_str().expect("shell id").to_owned();

    let exited = wait_exited(&harness.state, &id).await;
    assert_eq!(exited["status"], "exited", "{exited}");
    assert_eq!(exited["exit"], 0, "{exited}");

    let (status, page) = call(
        &harness.state,
        Method::GET,
        &format!("/api/shell/{id}/output"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let output = page["data"]["output"].as_str().expect("output text");
    assert!(
        output.contains(sid.as_str()),
        "shell output did not contain the session id: {page}"
    );

    let (status, _) = call(
        &harness.state,
        Method::DELETE,
        &format!("/api/shell/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one linear alias round trip
async fn persistent_pty_aliases_round_trip() {
    let harness = harness().await;
    let (status, session) = call(
        &harness.state,
        Method::POST,
        "/api/session",
        Some(json!({ "title": "terminal session" })),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let sid = session["data"]["id"].as_str().unwrap().to_owned();

    let command = if cfg!(windows) { "cmd.exe" } else { "sh" };
    let args: Vec<&str> = if cfg!(windows) {
        vec!["/c", "ping -n 10 127.0.0.1 >nul"]
    } else {
        vec!["-c", "sleep 10"]
    };
    let (status, created) = call(
        &harness.state,
        Method::POST,
        &format!("/api/experimental/session/{sid}/terminal"),
        Some(json!({
            "command": command,
            "args": args,
            "title": "terminal",
            "env": {},
            "size": { "cols": 80, "rows": 24 },
        })),
        false,
    )
    .await;
    if status != StatusCode::OK {
        eprintln!("skipping persistent PTY test: create failed {status}: {created}");
        return;
    }
    let info = &created["data"];
    let id = info["id"].as_str().expect("pty id").to_owned();
    assert_eq!(info["sessionID"], sid.as_str());
    assert_eq!(info["status"], "running");
    assert_eq!(info["size"]["cols"], 80);
    assert!(info["output"]["head"].as_u64().is_some());

    let (status, listed) = call(
        &harness.state,
        Method::GET,
        &format!("/api/experimental/session/{sid}/terminal"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"] == id.as_str()),
        "{listed}"
    );

    let (status, single) = call(
        &harness.state,
        Method::GET,
        &format!("/api/experimental/persistent-pty/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(single["data"]["sessionID"], sid.as_str());

    let (status, read) = call(
        &harness.state,
        Method::GET,
        &format!("/api/experimental/session/{sid}/terminal/read?lines=5"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(read["data"]["screen"]["text"].is_string(), "{read}");
    assert!(read["data"]["screen"]["cursor"]["x"].as_u64().is_some());

    let (status, snapshot) = call(
        &harness.state,
        Method::GET,
        &format!("/api/experimental/persistent-pty/{id}/snapshot"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(snapshot["data"]["checkpoint"].is_string());
    assert_eq!(snapshot["data"]["info"]["id"], id.as_str());

    let (status, token) = call(
        &harness.state,
        Method::POST,
        &format!("/api/experimental/persistent-pty/{id}/connect-token"),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{token}");
    assert!(
        token["data"]["ticket"]
            .as_str()
            .is_some_and(|t| !t.is_empty())
    );

    let (status, forbidden) = call(
        &harness.state,
        Method::POST,
        &format!("/api/experimental/persistent-pty/{id}/connect-token"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(forbidden["_tag"], "ForbiddenError");

    let (status, _) = call(
        &harness.state,
        Method::DELETE,
        &format!("/api/experimental/persistent-pty/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, missing) = call(
        &harness.state,
        Method::GET,
        &format!("/api/experimental/persistent-pty/{id}"),
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["_tag"], "PtyNotFoundError");
}

#[tokio::test]
async fn persistent_pty_requires_a_manager() {
    let state = AppState::new(
        ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 4242,
            hostname: None,
            urls: Vec::new(),
            paths: ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        },
        None,
        std::env::temp_dir(),
    );
    let (status, body) = call(
        &state,
        Method::GET,
        "/api/experimental/persistent-pty",
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["_tag"], "ServiceUnavailableError");
}

#[tokio::test]
async fn shell_unknown_ids_are_404() {
    let harness = harness().await;
    let (status, body) = call(
        &harness.state,
        Method::GET,
        "/api/shell/sh_missing",
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["_tag"], "ShellNotFoundError");

    let (status, _) = call(
        &harness.state,
        Method::GET,
        "/api/shell/sh_missing/output",
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        &harness.state,
        Method::DELETE,
        "/api/shell/sh_missing",
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
