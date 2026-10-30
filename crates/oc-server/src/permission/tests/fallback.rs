//! Session-wide reject fan-out and the store-less in-memory fallback.

use oc_runtime::{PermissionDecision, PermissionHandler};
use oc_types::{PermissionReply, SessionId};
use serde_json::json;
use tokio::sync::broadcast;
use tokio::time::timeout;

use super::{Fixture, WAIT, ask, create_session, next_replied, request_id, wait_pending};
use crate::permission::PermissionBroker;

#[tokio::test]
async fn reject_denies_every_pending_request_of_the_session() {
    let fixture = Fixture::new().await;
    let (broker, mut events) = fixture.broker();
    let session = fixture.session_id.clone();
    let other_session = SessionId::new().into_inner();
    create_session(
        &fixture.store,
        &other_session,
        &fixture.project_id,
        std::path::Path::new("."),
    )
    .await;

    let first = tokio::spawn({
        let broker = broker.clone();
        let session = session.clone();
        async move { broker.resolve(ask(&session, "shell", &["echo a"])).await }
    });
    let second = tokio::spawn({
        let broker = broker.clone();
        let session = session.clone();
        async move { broker.resolve(ask(&session, "shell", &["echo b"])).await }
    });
    let other = tokio::spawn({
        let broker = broker.clone();
        let session = other_session.clone();
        async move { broker.resolve(ask(&session, "read", &["notes.txt"])).await }
    });

    let pending = wait_pending(&broker, &session, 2).await;
    let first_id = request_id(&pending, "shell", &["echo a"]);
    let second_id = request_id(&pending, "shell", &["echo b"]);
    wait_pending(&broker, &other_session, 1).await;

    broker
        .reply(
            &session,
            &first_id,
            PermissionReply::Reject,
            Some("no".to_owned()),
        )
        .expect("reject");

    // Both asks of the rejected session fail; the other session keeps its
    // pending request.
    assert!(matches!(
        timeout(WAIT, first).await.expect("first").expect("join"),
        PermissionDecision::Deny { .. }
    ));
    assert!(matches!(
        timeout(WAIT, second).await.expect("second").expect("join"),
        PermissionDecision::Deny { .. }
    ));
    assert!(broker.pending(&session).is_empty());
    assert_eq!(broker.pending(&other_session).len(), 1);

    let event = next_replied(&mut events, &first_id).await;
    assert_eq!(event["reply"], json!("reject"));
    let event = next_replied(&mut events, &second_id).await;
    assert_eq!(event["reply"], json!("reject"));

    // Release the untouched ask so the broker drop does not race the test.
    broker.reject_all("done");
    assert!(matches!(
        timeout(WAIT, other).await.expect("other").expect("join"),
        PermissionDecision::Deny { .. }
    ));
}

#[tokio::test]
async fn always_without_a_store_keeps_the_in_memory_fallback() {
    let (tx, mut events) = broadcast::channel(8);
    let broker = PermissionBroker::new(tx);
    let session = SessionId::new().into_inner();
    let first = tokio::spawn({
        let broker = broker.clone();
        let session = session.clone();
        async move { broker.resolve(ask(&session, "shell", &["echo ok"])).await }
    });
    let pending = wait_pending(&broker, &session, 1).await;
    let id = request_id(&pending, "shell", &["echo ok"]);
    broker
        .reply(&session, &id, PermissionReply::Always, None)
        .expect("reply");
    assert_eq!(
        timeout(WAIT, first).await.expect("ask").expect("join"),
        PermissionDecision::Allow
    );
    let event = next_replied(&mut events, &id).await;
    assert_eq!(event["reply"], json!("always"));

    // The in-memory rule answers the next ask without a pending request.
    assert_eq!(
        timeout(WAIT, broker.resolve(ask(&session, "shell", &["echo ok"])))
            .await
            .expect("memory resolve"),
        PermissionDecision::Allow
    );
    assert!(broker.pending(&session).is_empty());
}
