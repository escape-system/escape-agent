use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use oc_llm::LlmEvent;

use crate::support::{Harness, Script, assistants, call, wait_until_active};

#[tokio::test]
async fn delete_cancels_a_hanging_provider_call_before_removing_rows() {
    let harness = Harness::new(vec![Script::HangAfter(vec![LlmEvent::StepStart])], None).await;
    let (status, _) = harness.prompt("hang").await;
    assert_eq!(status, StatusCode::OK);
    wait_until_active(&harness).await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while assistants(&harness.messages().await).is_empty() {
        assert!(
            Instant::now() < deadline,
            "hanging step never created an assistant message"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let started = Instant::now();
    let (status, body) = call(
        &harness.app,
        Method::DELETE,
        &format!("/api/session/{}", harness.session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_none(), "{body:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "DELETE waited on the provider stream for {:?}",
        started.elapsed()
    );
    assert!(
        harness
            .runtime
            .as_ref()
            .expect("runtime")
            .active()
            .is_empty(),
        "the provider call survived DELETE"
    );

    let (status, body) = call(
        &harness.app,
        Method::GET,
        &format!("/api/session/{}", harness.session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.expect("error")["_tag"], "SessionNotFoundError");

    harness.runtime.expect("runtime").shutdown().await;
}
