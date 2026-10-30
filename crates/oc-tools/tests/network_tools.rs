//! Offline tests for the `webfetch` security guards and `todowrite` storage.
//!
//! Nothing here performs a network request: blocked URLs are rejected before a
//! client is built, and the HTML/body helpers are pure functions.

use std::path::PathBuf;

use serde_json::json;

use oc_tools::todowrite::{MAX_SESSIONS, STATUSES, TodowriteTool};
use oc_tools::webfetch::{
    self, BodyBuffer, MAX_BODY_BYTES, MAX_REDIRECTS, MAX_TIMEOUT_SECONDS, WebfetchTool,
};
use oc_tools::{Tool, ToolContext};

fn ctx(session: &str) -> ToolContext {
    ToolContext {
        session_id: session.to_string(),
        directory: PathBuf::new(),
        cwd: PathBuf::new(),
        cancel: None,
    }
}

#[tokio::test]
async fn webfetch_blocks_local_and_private_targets_without_network() {
    let blocked = [
        "http://127.0.0.1/",
        "http://127.1.2.3/",
        "http://0.0.0.0/",
        "http://10.0.0.1/",
        "http://172.16.5.4/",
        "http://192.168.1.1/",
        "http://169.254.169.254/latest/meta-data/",
        "http://100.64.0.1/",
        "http://192.0.2.10/",
        "http://192.88.99.1/",
        "http://198.18.0.1/",
        "http://224.0.0.1/",
        "http://255.255.255.255/",
        "http://0x7f000001/",
        "http://[::1]/",
        "http://[::]/",
        "http://[fe80::1]/",
        "http://[fec0::1]/",
        "http://[fc00::1]/",
        "http://[fd00:ec2::254]/",
        "http://[ff02::1]/",
        "http://[2001:db8::1]/",
        "http://[::ffff:127.0.0.1]/",
        "http://[64:ff9b::169.254.169.254]/",
        "http://localhost:8080/",
        "http://api.localhost/",
    ];
    for url in blocked {
        let error = WebfetchTool
            .execute(&ctx("ses_webfetch_blocked"), json!({ "url": url }))
            .await
            .unwrap_err();
        assert!(
            error.message.contains("blocked"),
            "{url} should be blocked, got: {}",
            error.message
        );
    }
}

#[tokio::test]
async fn webfetch_rejects_non_http_and_credential_urls() {
    for url in [
        "file:///etc/passwd",
        "ftp://example.com/file",
        "data:text/plain,hi",
    ] {
        let error = WebfetchTool
            .execute(&ctx("ses_webfetch_scheme"), json!({ "url": url }))
            .await
            .unwrap_err();
        assert!(
            error.message.contains("http:// or https://"),
            "{url}: {}",
            error.message
        );
    }

    let error = WebfetchTool
        .execute(
            &ctx("ses_webfetch_credentials"),
            json!({ "url": "http://user:secret@example.com/" }),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("credentials"), "{}", error.message);
}

#[tokio::test]
async fn webfetch_validates_input_bounds() {
    let session = ctx("ses_webfetch_bounds");
    for input in [
        json!({ "url": "http://127.0.0.1/", "timeout": 0 }),
        json!({ "url": "http://127.0.0.1/", "timeout": MAX_TIMEOUT_SECONDS + 1 }),
        json!({ "url": "http://127.0.0.1/", "timeout": "30" }),
    ] {
        let error = WebfetchTool.execute(&session, input).await.unwrap_err();
        assert!(error.message.contains("timeout"), "{}", error.message);
    }

    for input in [
        json!({ "url": "http://127.0.0.1/", "format": "pdf" }),
        json!({ "url": 42 }),
        json!({}),
        json!({ "url": "not a url" }),
    ] {
        assert!(
            WebfetchTool.execute(&session, input).await.is_err(),
            "invalid input should be rejected"
        );
    }

    assert_eq!(MAX_REDIRECTS, 5);
    assert_eq!(MAX_BODY_BYTES, 5 * 1024 * 1024);
}

#[test]
fn webfetch_html_conversion_and_body_cap() {
    let html = "<html><head><script>alert('x')</script><style>p{color:red}</style></head>\
                <body><h1>Title</h1><p>Hello &amp; welcome</p><ul><li>One</li></ul></body></html>";
    let text = webfetch::html_to_text(html);
    assert!(!text.contains("alert"), "{text}");
    assert!(!text.contains("color:red"), "{text}");
    assert!(text.contains("Hello & welcome"), "{text}");
    assert!(text.contains("One"), "{text}");

    let markdown = webfetch::html_to_markdown(html);
    assert!(markdown.contains("# Title"), "{markdown}");
    assert!(markdown.contains("- One"), "{markdown}");

    let mut buffer = BodyBuffer::new();
    let chunk = vec![b'x'; MAX_BODY_BYTES + 64];
    buffer.push(&chunk);
    assert_eq!(buffer.len(), MAX_BODY_BYTES);
    assert!(buffer.truncated());
    assert!(webfetch::is_blocked_ip("169.254.169.254".parse().unwrap()));
    assert!(!webfetch::is_blocked_ip("93.184.216.34".parse().unwrap()));
}

#[tokio::test]
async fn todowrite_replaces_the_whole_list_per_session() {
    let session = ctx("ses_todowrite_replace");
    let first = TodowriteTool
        .execute(
            &session,
            json!({
                "todos": [
                    { "content": "write code", "status": "in_progress" },
                    { "content": "run tests", "status": "pending" }
                ]
            }),
        )
        .await
        .unwrap();
    let output = first.output.unwrap();
    assert!(output.contains("- [~] write code"), "{output}");
    assert!(output.contains("- [ ] run tests"), "{output}");
    assert!(output.contains("0/2 completed"), "{output}");
    assert!(output.contains("In progress: write code"), "{output}");
    let metadata = first.metadata.unwrap();
    assert_eq!(metadata["todos"].as_array().unwrap().len(), 2);
    assert_eq!(metadata["todos"][0]["status"], "in_progress");

    let second = TodowriteTool
        .execute(
            &session,
            json!({ "todos": [{ "content": "done thing", "status": "completed" }] }),
        )
        .await
        .unwrap();
    let output = second.output.unwrap();
    assert!(output.contains("- [x] done thing"), "{output}");
    assert!(!output.contains("write code"), "{output}");
    assert_eq!(
        second.metadata.unwrap()["todos"].as_array().unwrap().len(),
        1
    );

    let cleared = TodowriteTool
        .execute(&session, json!({ "todos": [] }))
        .await
        .unwrap();
    assert_eq!(cleared.output.unwrap(), "Todos cleared");
    assert_eq!(
        cleared.metadata.unwrap()["todos"].as_array().unwrap().len(),
        0
    );
}

#[tokio::test]
async fn todowrite_rejects_invalid_items() {
    let session = ctx("ses_todowrite_validation");
    let cases = [
        (
            json!({ "todos": [{ "content": "x", "status": "doing" }] }),
            "status",
        ),
        (
            json!({ "todos": [{ "content": "x", "status": 1 }] }),
            "status",
        ),
        (json!({ "todos": [{ "content": "x" }] }), "status"),
        (
            json!({ "todos": [{ "content": "", "status": "pending" }] }),
            "content",
        ),
        (
            json!({ "todos": [{ "content": "   ", "status": "pending" }] }),
            "content",
        ),
        (
            json!({ "todos": [{ "content": 5, "status": "pending" }] }),
            "content",
        ),
        (json!({ "todos": [{ "status": "pending" }] }), "content"),
        (json!({ "todos": "nope" }), "array"),
        (json!({ "todos": [42] }), "object"),
        (json!({}), "todos"),
    ];
    for (input, needle) in cases {
        let error = TodowriteTool
            .execute(&session, input.clone())
            .await
            .unwrap_err();
        assert!(
            error.message.contains(needle),
            "{input} should mention `{needle}`, got: {}",
            error.message
        );
    }

    // Every declared status is accepted, including cancellation.
    for status in STATUSES {
        let result = TodowriteTool
            .execute(
                &session,
                json!({ "todos": [{ "content": "task", "status": status }] }),
            )
            .await
            .unwrap();
        let expected = format!("[{}]", marker(status));
        assert!(result.output.unwrap().contains(expected.as_str()));
    }
    assert_eq!(MAX_SESSIONS, 100);
}

fn marker(status: &str) -> &'static str {
    match status {
        "pending" => " ",
        "in_progress" => "~",
        "completed" => "x",
        _ => "-",
    }
}
