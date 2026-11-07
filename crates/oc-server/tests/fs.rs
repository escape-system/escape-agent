//! Filesystem route tests: raw reads, ranges, `ETag`s, listing, finding and
//! location confinement.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::body::to_bytes;
use axum::http::header::{
    ACCEPT_RANGES, CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_NONE_MATCH, LAST_MODIFIED,
};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use oc_server::{AppState, router};
use oc_store::Store;
use oc_types::{ServerInfo, ServerPaths};
use serde_json::Value;
use tower::ServiceExt;

struct Harness {
    _base: tempfile::TempDir,
    root: PathBuf,
    outside: PathBuf,
    state: AppState,
}

fn harness() -> Harness {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("project");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::create_dir_all(root.join("adir")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::create_dir_all(root.join("nested_dir")).unwrap();
    std::fs::write(root.join("text.txt"), "hello world").unwrap();
    std::fs::write(root.join("empty.txt"), "").unwrap();
    std::fs::write(root.join("binary.bin"), [0u8, 1, 2, 255]).unwrap();
    std::fs::write(root.join("zebra.txt"), "z").unwrap();
    std::fs::write(root.join("Alpha.txt"), "a").unwrap();
    std::fs::write(root.join("sub").join("nested.txt"), "nested").unwrap();
    std::fs::write(root.join("src").join("nested.rs"), "fn main() {}").unwrap();
    std::fs::write(root.join("docs").join("nested.md"), "# nested").unwrap();
    std::fs::write(root.join("nested_dir").join("inner.txt"), "inner").unwrap();
    std::fs::write(root.join("ignored.txt"), "ignored").unwrap();
    std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(root.join(".hidden.txt"), "hidden").unwrap();
    std::fs::write(root.join("big.bin"), vec![7u8; 1024]).unwrap();
    let outside = base.path().join("secret.txt");
    std::fs::write(&outside, "secret").unwrap();

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
        root.clone(),
    );

    Harness {
        _base: base,
        root,
        outside,
        state,
    }
}

async fn request(
    app: &axum::Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, headers, bytes.to_vec())
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    request(app, Method::GET, uri, &[]).await
}

fn json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

fn path_uri(path: &Path) -> String {
    // Test paths only contain ASCII; encode spaces defensively.
    path.to_string_lossy()
        .replace('\\', "/")
        .replace(' ', "%20")
}

#[tokio::test]
async fn read_text_file_with_validators() {
    let harness = harness();
    let app = router(harness.state);

    let (status, headers, bytes) = get(&app, "/api/fs/read/text.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"hello world");
    assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "text/plain");
    assert_eq!(headers.get(ACCEPT_RANGES).unwrap(), "bytes");
    assert_eq!(headers.get("cache-control").unwrap(), "no-cache");

    let etag = headers.get(ETAG).unwrap().to_str().unwrap().to_owned();
    assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
    assert_eq!(etag.len(), 66, "quoted sha256 hex");
    assert!(etag[1..65].chars().all(|value| value.is_ascii_hexdigit()));
    let last_modified = headers.get(LAST_MODIFIED).unwrap().to_str().unwrap();
    assert!(last_modified.ends_with(" GMT"), "{last_modified}");

    // Conditional request, exact and weak forms.
    for candidate in [etag.clone(), format!("W/{etag}"), "*".to_owned()] {
        let (status, headers, bytes) = request(
            &app,
            Method::GET,
            "/api/fs/read/text.txt",
            &[(IF_NONE_MATCH.as_str(), candidate.as_str())],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_MODIFIED, "{candidate}");
        assert!(bytes.is_empty());
        assert_eq!(headers.get(ETAG).unwrap().to_str().unwrap(), etag);
    }

    // A non-matching validator still returns the body.
    let (status, _, bytes) = request(
        &app,
        Method::GET,
        "/api/fs/read/text.txt",
        &[(IF_NONE_MATCH.as_str(), "\"nope\"")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"hello world");
}

#[tokio::test]
async fn read_binary_uses_octet_stream() {
    let harness = harness();
    let app = router(harness.state);

    let (status, headers, bytes) = get(&app, "/api/fs/read/binary.bin").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
    assert_eq!(bytes, vec![0u8, 1, 2, 255]);

    let (status, headers, bytes) = get(&app, "/api/fs/read/empty.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert!(bytes.is_empty());
    assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "text/plain");
}

#[tokio::test]
async fn single_ranges_return_partial_content() {
    let harness = harness();
    let app = router(harness.state);

    for (range, expected, content_range) in [
        ("bytes=0-4", "hello", "bytes 0-4/11"),
        ("bytes=6-", "world", "bytes 6-10/11"),
        ("bytes=-5", "world", "bytes 6-10/11"),
        ("bytes=0-999", "hello world", "bytes 0-10/11"),
    ] {
        let (status, headers, bytes) = request(
            &app,
            Method::GET,
            "/api/fs/read/text.txt",
            &[("range", range)],
        )
        .await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT, "{range}");
        assert_eq!(bytes, expected.as_bytes(), "{range}");
        assert_eq!(
            headers.get(CONTENT_RANGE).unwrap().to_str().unwrap(),
            content_range,
            "{range}"
        );
        assert_eq!(headers.get(ACCEPT_RANGES).unwrap(), "bytes");
    }

    // Multiple and malformed ranges fall back to the full response.
    for range in ["bytes=0-1,4-5", "bytes=abc", "chunks=0-1"] {
        let (status, _, bytes) = request(
            &app,
            Method::GET,
            "/api/fs/read/text.txt",
            &[("range", range)],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{range}");
        assert_eq!(bytes, b"hello world", "{range}");
    }

    // A range beyond EOF is unsatisfiable.
    let (status, headers, bytes) = request(
        &app,
        Method::GET,
        "/api/fs/read/text.txt",
        &[("range", "bytes=99-100")],
    )
    .await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert!(bytes.is_empty());
    assert_eq!(headers.get(CONTENT_RANGE).unwrap(), "bytes */11");
}

#[tokio::test]
async fn head_has_headers_but_no_body() {
    let harness = harness();
    let app = router(harness.state);

    let (status, headers, bytes) = request(&app, Method::HEAD, "/api/fs/read/text.txt", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(bytes.is_empty());
    assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "text/plain");
    assert!(headers.get(ETAG).is_some());
    assert!(headers.get(LAST_MODIFIED).is_some());
}

#[tokio::test]
async fn read_directory_and_missing_file_errors() {
    let harness = harness();
    let app = router(harness.state);

    let (status, _, bytes) = get(&app, "/api/fs/read/sub").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let body = json(&bytes);
    assert_eq!(body["_tag"], "IsDirectoryError");
    assert_eq!(body["path"], "sub");

    let (status, _, bytes) = get(&app, "/api/fs/read/nope.txt").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let body = json(&bytes);
    assert_eq!(body["_tag"], "FileNotFoundError");
    assert_eq!(body["path"], "nope.txt");
}

#[tokio::test]
async fn paths_outside_the_location_are_rejected() {
    let harness = harness();
    let app = router(harness.state);

    let attempts = [
        "/api/fs/read/../secret.txt".to_owned(),
        "/api/fs/read/%2e%2e%2fsecret.txt".to_owned(),
        "/api/fs/read/sub/../../secret.txt".to_owned(),
        "/api/fs/read/%2e%2e/%2e%2e/secret.txt".to_owned(),
        // An absolute path outside the location never escapes the confinement.
        format!("/api/fs/read/{}", path_uri(&harness.outside)),
    ];
    for uri in attempts {
        let (status, _, bytes) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(json(&bytes)["_tag"], "FileNotFoundError", "{uri}");
    }

    // A link that points outside must not escape either. Directory junctions
    // are available on Windows without the symlink privilege.
    let outside_dir = harness.outside.with_file_name("outside");
    std::fs::create_dir_all(&outside_dir).unwrap();
    std::fs::write(outside_dir.join("secret.txt"), "secret").unwrap();
    let file_link = harness.root.join("escape.txt");
    let dir_link = harness.root.join("escape-dir");
    let mut request = None;
    if create_file_symlink(&harness.outside, &file_link) {
        request = Some("/api/fs/read/escape.txt".to_owned());
    } else if create_dir_link(&outside_dir, &dir_link) {
        request = Some("/api/fs/read/escape-dir/secret.txt".to_owned());
    }
    if let Some(uri) = request {
        let (status, _, bytes) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(json(&bytes)["_tag"], "FileNotFoundError", "{uri}");
    } else {
        eprintln!("skipping link escape assertion: no symlink privilege");
    }
}

fn create_file_symlink(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }
}

fn create_dir_link(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

#[tokio::test]
async fn list_returns_sorted_entries_with_metadata() {
    let harness = harness();
    let app = router(harness.state);

    let (status, _, bytes) = get(&app, "/api/fs/list").await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&bytes);
    assert!(body["location"]["directory"].is_string());
    let entries = body["data"].as_array().unwrap();

    // Directories first, then files, each group by case-insensitive name.
    let names: Vec<&str> = entries
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    let first_file = names.iter().position(|name| *name == "Alpha.txt").unwrap();
    let last_dir = names
        .iter()
        .rposition(|name| entry_dir(&body, name))
        .unwrap();
    assert!(last_dir < first_file, "directories first: {names:?}");
    assert_eq!(
        names,
        vec![
            "adir",
            "docs",
            "nested_dir",
            "src",
            "sub",
            ".gitignore",
            ".hidden.txt",
            "Alpha.txt",
            "big.bin",
            "binary.bin",
            "empty.txt",
            "ignored.txt",
            "text.txt",
            "zebra.txt",
        ]
    );

    let alpha = entries
        .iter()
        .find(|entry| entry["name"] == "Alpha.txt")
        .unwrap();
    assert_eq!(alpha["type"], "file");
    assert_eq!(alpha["size"], 1);
    assert!(alpha["modified"].is_i64());
    assert_eq!(alpha["path"], "Alpha.txt", "paths are relative to the root");
    assert_eq!(
        alpha["absolute"].as_str().unwrap(),
        harness.root.join("Alpha.txt").to_string_lossy().as_ref(),
        "absolute is the previous absolute path"
    );
    // Every listing entry carries both forms; none leaks an absolute `path`.
    let canonical_root = std::fs::canonicalize(&harness.root).unwrap();
    for entry in entries {
        let path = entry["path"].as_str().unwrap();
        assert!(!path.starts_with('/'), "absolute path in listing: {path}");
        assert_eq!(
            entry["absolute"].as_str().unwrap(),
            canonical_root.join(path).to_string_lossy().as_ref(),
            "absolute must be root-joined path"
        );
    }

    let adir = entries
        .iter()
        .find(|entry| entry["name"] == "adir")
        .unwrap();
    assert_eq!(adir["type"], "directory");
    assert_eq!(adir["size"], 0);

    // `path=` still resolves inside the location; entry paths stay relative to
    // the location root, not to the listed subdirectory.
    let sub = path_uri(&harness.root.join("sub"));
    let (status, _, bytes) = get(&app, &format!("/api/fs/list?path={sub}")).await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&bytes);
    let entries = body["data"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "nested.txt");
    assert_eq!(entries[0]["path"], "sub/nested.txt");
    assert_eq!(
        entries[0]["absolute"].as_str().unwrap(),
        harness
            .root
            .join("sub")
            .join("nested.txt")
            .to_string_lossy()
            .as_ref()
    );

    // Missing directory is a 404, never an empty list.
    let (status, _, bytes) = get(&app, "/api/fs/list?path=missing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&bytes)["_tag"], "FileNotFoundError");
}

#[tokio::test]
async fn list_navigates_outside_the_location() {
    let harness = harness();
    let app = router(harness.state);

    let outside_dir = tempfile::tempdir().unwrap();
    std::fs::write(outside_dir.path().join("visible.txt"), "visible").unwrap();

    let uri = format!("/api/fs/list?path={}", path_uri(outside_dir.path()));
    let (status, _, bytes) = get(&app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&bytes);
    let entries = body["data"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "visible.txt");
    assert_eq!(entries[0]["type"], "file");

    let canonical = std::fs::canonicalize(outside_dir.path()).unwrap();
    let expected_path = canonical.join("visible.txt");
    let expected = expected_path.to_string_lossy().into_owned();
    let stripped = expected.strip_prefix(r"\\?\").map(str::to_owned);
    let expected = stripped.unwrap_or(expected);
    assert_eq!(entries[0]["absolute"].as_str().unwrap(), expected.as_str());
    // Outside the location there is no relative form, so `path` falls back to
    // the absolute text instead of inventing a fake relative path.
    assert_eq!(entries[0]["path"].as_str().unwrap(), expected.as_str());

    // Listing may leave the location; reads stay confined.
    let uri = format!(
        "/api/fs/read/{}",
        path_uri(&outside_dir.path().join("visible.txt"))
    );
    let (status, _, bytes) = get(&app, &uri).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&bytes)["_tag"], "FileNotFoundError");

    // A missing directory outside the location is still a 404.
    let missing = outside_dir.path().join("gone");
    let uri = format!("/api/fs/list?path={}", path_uri(&missing));
    let (status, _, bytes) = get(&app, &uri).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&bytes)["_tag"], "FileNotFoundError");
}

#[tokio::test]
async fn location_registers_the_project_it_resolves() {
    let db = tempfile::tempdir().unwrap();
    let store = Store::open(db.path().join("data.sqlite")).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
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
        directory.path().to_path_buf(),
    )
    .with_store(Arc::new(store));
    let app = router(state);

    let uri = format!(
        "/api/location?location%5Bdirectory%5D={}",
        directory
            .path()
            .to_string_lossy()
            .replace('\\', "%5C")
            .replace(' ', "%20")
    );
    let (status, _, bytes) = get(&app, &uri).await;
    assert_eq!(status, StatusCode::OK);
    let location = json(&bytes);
    let id = location["project"]["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("prj_"), "unexpected project id: {id}");
    let canonical = location["project"]["canonical"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!canonical.is_empty());

    let (status, _, bytes) = get(&app, "/api/project").await;
    assert_eq!(status, StatusCode::OK);
    let projects = json(&bytes);
    let listed = projects
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["id"].as_str() == Some(id.as_str()))
        .unwrap_or_else(|| panic!("project {id} missing from {projects:?}"));
    assert_eq!(listed["canonical"].as_str(), Some(canonical.as_str()));
    assert_eq!(listed["directory"], location["directory"]);
}

fn entry_dir(body: &Value, name: &str) -> bool {
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["name"] == name && entry["type"] == "directory")
}

#[tokio::test]
async fn find_respects_gitignore_hidden_type_and_limit() {
    let harness = harness();
    let app = router(harness.state);

    let (status, _, bytes) = get(&app, "/api/fs/find?query=nested").await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&bytes);
    assert!(body["location"]["directory"].is_string());
    let entries = body["data"].as_array().unwrap();
    assert_eq!(entries.len(), 5, "{entries:?}");
    for entry in entries {
        assert!(entry["path"].as_str().unwrap().contains("nested"));
    }

    let (status, _, bytes) = get(&app, "/api/fs/find?query=nested&type=file").await;
    assert_eq!(status, StatusCode::OK);
    let entries = json(&bytes)["data"].as_array().unwrap().clone();
    assert_eq!(entries.len(), 4);
    assert!(entries.iter().all(|entry| entry["type"] == "file"));

    let (status, _, bytes) = get(&app, "/api/fs/find?query=nested&type=directory").await;
    assert_eq!(status, StatusCode::OK);
    let entries = json(&bytes)["data"].as_array().unwrap().clone();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "nested_dir");

    // Case-insensitive substring on the relative path.
    let (status, _, bytes) = get(&app, "/api/fs/find?query=NESTED.RS").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&bytes)["data"].as_array().unwrap().len(), 1);

    let (status, _, bytes) = get(&app, "/api/fs/find?query=nested&limit=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&bytes)["data"].as_array().unwrap().len(), 1);

    // Gitignored and hidden entries are not walked.
    for query in ["ignored", "hidden"] {
        let (status, _, bytes) = get(&app, &format!("/api/fs/find?query={query}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            json(&bytes)["data"].as_array().unwrap().is_empty(),
            "{query}"
        );
    }

    let (status, _, bytes) = get(&app, "/api/fs/find?query=nested&type=bogus").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json(&bytes)["_tag"], "SchemaError");

    let (status, _, bytes) = get(&app, "/api/fs/find?query=nested&limit=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json(&bytes)["_tag"], "SchemaError");
}

#[tokio::test]
async fn list_paths_are_relative_to_the_location_root() {
    let harness = harness();
    std::fs::create_dir_all(harness.root.join("nested").join("dir")).unwrap();
    std::fs::write(
        harness.root.join("nested").join("dir").join("file.txt"),
        "nested",
    )
    .unwrap();
    let app = router(harness.state);

    let canonical = std::fs::canonicalize(&harness.root).unwrap();
    let root_text = canonical.to_string_lossy().into_owned();
    let expected_absolute = canonical
        .join("nested")
        .join("dir")
        .join("file.txt")
        .to_string_lossy()
        .into_owned();

    // Both an absolute `path=` and a location-relative one must answer paths
    // relative to the requested location root, never to the nested directory.
    for requested in [
        path_uri(&canonical.join("nested").join("dir")),
        "nested/dir".to_owned(),
    ] {
        let (status, _, bytes) = get(&app, &format!("/api/fs/list?path={requested}")).await;
        assert_eq!(status, StatusCode::OK, "{requested}");
        let body = json(&bytes);
        let entries = body["data"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "{requested}: {entries:?}");
        for entry in entries {
            let path = entry["path"].as_str().unwrap();
            assert!(!path.starts_with('/'), "{requested}: {path}");
            assert!(!path.starts_with(&root_text), "{requested}: {path}");
            assert!(path.ends_with("nested/dir/file.txt"), "{requested}: {path}");
            assert_eq!(
                entry["absolute"].as_str().unwrap(),
                expected_absolute,
                "{requested}"
            );
            // The old behavior serialized the absolute path in `path`; these
            // assertions fail against it.
            assert_ne!(path, entry["absolute"].as_str().unwrap(), "{requested}");
        }
    }
}

#[tokio::test]
async fn find_paths_are_relative_to_the_location_root() {
    let harness = harness();
    std::fs::create_dir_all(harness.root.join("nested").join("dir")).unwrap();
    std::fs::write(
        harness.root.join("nested").join("dir").join("file.txt"),
        "nested",
    )
    .unwrap();
    let app = router(harness.state);

    let canonical = std::fs::canonicalize(&harness.root).unwrap();
    let root_text = canonical.to_string_lossy().into_owned();

    let (status, _, bytes) = get(&app, "/api/fs/find?query=file.txt&type=file").await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&bytes);
    let entries = body["data"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{entries:?}");
    for entry in entries {
        let path = entry["path"].as_str().unwrap();
        assert!(!path.starts_with('/'), "absolute path in find: {path}");
        assert!(
            !path.starts_with(&root_text),
            "root-prefixed find path: {path}"
        );
        assert_eq!(path, "nested/dir/file.txt");
        assert_eq!(
            entry["absolute"].as_str().unwrap(),
            canonical
                .join("nested")
                .join("dir")
                .join("file.txt")
                .to_string_lossy()
                .as_ref()
        );
    }
}

/// The client union is only `file | directory`; a symlink to a file must be
/// reported as a file (the old code serialized `"symlink"`).
#[cfg(unix)]
#[tokio::test]
async fn symlinked_file_reports_file_kind() {
    let harness = harness();
    std::fs::write(harness.root.join("real.txt"), "real").unwrap();
    let target = harness.root.join("real.txt");
    let link = harness.root.join("link.txt");
    if !create_file_symlink(&target, &link) {
        eprintln!("skipping symlink assertion: no symlink privilege");
        return;
    }

    let app = router(harness.state);
    let (status, _, bytes) = get(&app, "/api/fs/list").await;
    assert_eq!(status, StatusCode::OK);
    let body = json(&bytes);
    let entry = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "link.txt")
        .expect("symlink must be listed");
    assert_eq!(entry["type"], "file");
    assert_eq!(entry["path"], "link.txt");
    assert_eq!(entry["size"], "real".len());
}

#[tokio::test]
async fn list_and_find_missing_location_is_404() {
    let harness = harness();
    let state = AppState::new(
        harness.state.info().clone(),
        None,
        harness.root.join("does-not-exist"),
    );
    let app = router(state);

    let (status, _, bytes) = get(&app, "/api/fs/list").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&bytes)["_tag"], "LocationNotFoundError");

    let (status, _, bytes) = get(&app, "/api/fs/find?query=x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&bytes)["_tag"], "LocationNotFoundError");

    let (status, _, bytes) = get(&app, "/api/fs/read/x.txt").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&bytes)["_tag"], "LocationNotFoundError");
}

/// Mirrors `MAX_READ_BYTES` in the route: the documented 64 MiB read cap.
const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;

#[tokio::test]
async fn read_oversized_regular_file_is_413() {
    let harness = harness();
    let huge = harness.root.join("huge.bin");
    let file = std::fs::File::create(&huge).unwrap();
    file.set_len(MAX_READ_BYTES + 1).unwrap();
    drop(file);

    let app = router(harness.state);

    // The size check rejects before opening, so no 64 MiB is ever buffered.
    let (status, _, bytes) = get(&app, "/api/fs/read/huge.bin").await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json(&bytes)["_tag"], "RequestTooLargeError");

    // Ranges on a regular file inside the cap still stream from the bytes read.
    let (status, headers, bytes) = request(
        &app,
        Method::GET,
        "/api/fs/read/text.txt",
        &[("range", "bytes=0-4")],
    )
    .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(bytes, b"hello");
    assert_eq!(headers.get(CONTENT_RANGE).unwrap(), "bytes 0-4/11");
}

#[tokio::test]
async fn read_follows_symlink_to_regular_file_inside_location() {
    let harness = harness();
    let target = harness.root.join("text.txt");
    let link = harness.root.join("link.txt");
    if !create_file_symlink(&target, &link) {
        eprintln!("skipping symlink assertion: no symlink privilege");
        return;
    }

    let app = router(harness.state);
    let (status, headers, bytes) = get(&app, "/api/fs/read/link.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"hello world");
    assert_eq!(headers.get(CONTENT_TYPE).unwrap(), "text/plain");
}

/// Devices and FIFOs must never be opened; `fs::read` on `/dev/zero` would
/// exhaust memory and reading a FIFO with no writer blocks forever.
#[cfg(unix)]
#[tokio::test]
async fn read_rejects_non_regular_files() {
    use std::time::Duration;

    let harness = harness();
    let info = harness.state.info().clone();
    let fifo = harness.root.join("pipe");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .is_ok_and(|status| status.success());

    let app = router(harness.state);
    if made {
        let (status, _, bytes) =
            tokio::time::timeout(Duration::from_secs(5), get(&app, "/api/fs/read/pipe"))
                .await
                .expect("a FIFO must not block the read route");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json(&bytes)["_tag"], "FileNotFoundError");

        // Listing and finding never misreport a FIFO as a regular file.
        let (status, _, bytes) = get(&app, "/api/fs/list").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !json(&bytes)["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == "pipe"),
            "FIFO leaked into the listing"
        );

        let (status, _, bytes) = get(&app, "/api/fs/find?query=pipe").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            json(&bytes)["data"].as_array().unwrap().is_empty(),
            "FIFO leaked into find"
        );
    } else {
        eprintln!("skipping FIFO assertion: mkfifo unavailable");
    }

    // The canonical device node answers like a missing file instead of
    // streaming forever.
    if Path::new("/dev/zero").exists() {
        let state = AppState::new(info, None, PathBuf::from("/dev"));
        let dev = router(state);
        let (status, _, bytes) =
            tokio::time::timeout(Duration::from_secs(5), get(&dev, "/api/fs/read/zero"))
                .await
                .expect("a device must not block the read route");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json(&bytes)["_tag"], "FileNotFoundError");
    }
}
