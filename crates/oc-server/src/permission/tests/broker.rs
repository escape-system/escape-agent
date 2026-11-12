//! `always` persistence/auto-resolution and `reject` fan-out.

use std::sync::Arc;

use oc_runtime::{PermissionDecision, PermissionHandler};
use oc_store::Store;
use oc_types::{PermissionReply, SessionId};
use serde_json::json;
use tokio::sync::broadcast;
use tokio::time::timeout;

use super::{Fixture, WAIT, ask, create_session, next_replied, request_id, wait_pending};
use crate::permission::PermissionBroker;

#[tokio::test]
async fn always_persists_the_rule_and_lists_it() {
    let fixture = Fixture::new().await;
    let (broker, mut events) = fixture.broker();
    let session = fixture.session_id.clone();
    let enqueued = |id: &str, resources: &[&str]| {
        json!({
            "id": id,
            "sessionID": session,
            "action": "shell",
            "resources": resources,
        })
    };
    broker.enqueue(
        &session,
        "per_echo",
        "shell".to_owned(),
        vec!["echo ok".to_owned()],
        enqueued("per_echo", &["echo ok"]),
    );
    broker.enqueue(
        &session,
        "per_rm",
        "shell".to_owned(),
        vec!["rm x".to_owned()],
        enqueued("per_rm", &["rm x"]),
    );
    broker
        .reply(&session, "per_echo", PermissionReply::Always, None)
        .expect("reply");

    // The event follows the committed write, so the row must exist now.
    let event = next_replied(&mut events, "per_echo").await;
    assert_eq!(event["reply"], json!("always"));
    let saved = fixture
        .store
        .list_saved_permissions(Some(&fixture.project_id))
        .await
        .expect("saved rules");
    assert!(
        saved
            .iter()
            .any(|row| row["action"] == json!("shell") && row["resource"] == json!("echo ok")),
        "saved rules: {saved:?}"
    );

    // Only the covered reply was answered; `rm x` is still pending.
    let pending = broker.pending(&session);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["id"], json!("per_rm"));
    drop(broker);
    drop(pending);

    // Restart simulation: a fresh store handle and a fresh broker.
    let project_id = fixture.project_id.clone();
    let second_session = SessionId::new().into_inner();
    create_session(
        &fixture.store,
        &second_session,
        &project_id,
        fixture.path.parent().expect("dir"),
    )
    .await;
    drop(fixture.store);
    let restarted = Arc::new(Store::open(&fixture.path).await.expect("reopened store"));
    let (tx, _rx) = broadcast::channel(8);
    let broker = PermissionBroker::with_store(tx, Arc::clone(&restarted));
    let decision = timeout(WAIT, broker.resolve(ask(&session, "shell", &["echo ok"])))
        .await
        .expect("durable resolve");
    assert_eq!(decision, PermissionDecision::Allow);
    // Another session of the same project is auto-allowed as well.
    let decision = timeout(
        WAIT,
        broker.resolve(ask(&second_session, "shell", &["echo ok"])),
    )
    .await
    .expect("cross-session resolve");
    assert_eq!(decision, PermissionDecision::Allow);
    // The uncovered resource is not: it raises a fresh request.
    let uncovered = tokio::spawn({
        let broker = broker.clone();
        let session = session.clone();
        async move { broker.resolve(ask(&session, "shell", &["rm x"])).await }
    });
    let pending = wait_pending(&broker, &session, 1).await;
    let id = request_id(&pending, "shell", &["rm x"]);
    broker
        .reply(&session, &id, PermissionReply::Reject, None)
        .expect("reject");
    assert!(matches!(
        timeout(WAIT, uncovered)
            .await
            .expect("uncovered")
            .expect("join"),
        PermissionDecision::Deny { .. }
    ));
}

#[tokio::test]
async fn always_auto_resolves_covered_pending_requests_in_the_same_session() {
    let fixture = Fixture::new().await;
    let (broker, mut events) = fixture.broker();
    let session = fixture.session_id.clone();
    let star = tokio::spawn({
        let broker = broker.clone();
        let session = session.clone();
        async move { broker.resolve(ask(&session, "shell", &["*"])).await }
    });
    let covered_ask = tokio::spawn({
        let broker = broker.clone();
        let session = session.clone();
        async move { broker.resolve(ask(&session, "shell", &["echo ok"])).await }
    });
    let uncovered_ask = tokio::spawn({
        let broker = broker.clone();
        let session = session.clone();
        async move { broker.resolve(ask(&session, "read", &["notes.txt"])).await }
    });

    let pending = wait_pending(&broker, &session, 3).await;
    let star_id = request_id(&pending, "shell", &["*"]);
    let covered_id = request_id(&pending, "shell", &["echo ok"]);
    let uncovered_id = request_id(&pending, "read", &["notes.txt"]);
    broker
        .reply(&session, &star_id, PermissionReply::Always, None)
        .expect("reply");

    assert_eq!(
        timeout(WAIT, star).await.expect("star").expect("join"),
        PermissionDecision::Allow
    );
    // The sibling covered by the `*` rule is allowed without a reply.
    assert_eq!(
        timeout(WAIT, covered_ask)
            .await
            .expect("covered")
            .expect("join"),
        PermissionDecision::Allow
    );
    let event = next_replied(&mut events, &covered_id).await;
    assert_eq!(event["reply"], json!("always"));
    // A resource of another action is not covered and stays pending.
    let pending = broker.pending(&session);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["id"], json!(uncovered_id));

    broker
        .reply(&session, &uncovered_id, PermissionReply::Reject, None)
        .expect("reject");
    assert!(matches!(
        timeout(WAIT, uncovered_ask)
            .await
            .expect("uncovered")
            .expect("join"),
        PermissionDecision::Deny { .. }
    ));
    assert!(broker.pending(&session).is_empty());
}

/// `permission.replied{durable}`: an `always` whose saved rule fully committed
/// reports `durable: true`, and the rule really is readable after a restart.
#[tokio::test]
async fn always_reports_durable_true_when_the_rule_committed() {
    let fixture = Fixture::new().await;
    let (broker, mut events) = fixture.broker();
    let session = fixture.session_id.clone();
    let asker = tokio::spawn({
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
        timeout(WAIT, asker).await.expect("ask").expect("join"),
        PermissionDecision::Allow
    );

    let event = next_replied(&mut events, &id).await;
    assert_eq!(event["reply"], json!("always"));
    assert_eq!(event["durable"], json!(true), "event: {event}");
    let project_id = fixture.project_id.clone();
    let saved = fixture
        .store
        .list_saved_permissions(Some(&project_id))
        .await
        .expect("saved");
    assert_eq!(saved.len(), 1, "saved rules: {saved:?}");
}

/// `permission.replied{durable}`: when the durable write fails (here: the
/// session row vanished, so `persist` cannot resolve the project) the event says
/// `durable: false` and the in-memory mirror keeps answering the same ask.
#[tokio::test]
async fn always_reports_durable_false_and_still_allows_from_memory() {
    let fixture = Fixture::new().await;
    let (broker, mut events) = fixture.broker();
    let session = fixture.session_id.clone();
    let asker = tokio::spawn({
        let broker = broker.clone();
        let session = session.clone();
        async move { broker.resolve(ask(&session, "shell", &["echo ok"])).await }
    });
    let pending = wait_pending(&broker, &session, 1).await;
    let id = request_id(&pending, "shell", &["echo ok"]);
    // `persist` resolves the project through the session row; without it the
    // durable write cannot even start, which is exactly the failure it reports.
    fixture
        .store
        .delete_session(&session)
        .await
        .expect("drop session row");
    broker
        .reply(&session, &id, PermissionReply::Always, None)
        .expect("reply");

    let event = next_replied(&mut events, &id).await;
    assert_eq!(event["reply"], json!("always"));
    assert_eq!(event["durable"], json!(false), "event: {event}");
    assert_eq!(
        timeout(WAIT, asker).await.expect("ask").expect("join"),
        PermissionDecision::Allow
    );
    // Nothing was written, so no `psv_` row exists...
    let saved = fixture
        .store
        .list_saved_permissions(Some(&fixture.project_id))
        .await
        .expect("saved");
    assert!(saved.is_empty(), "unexpected durable rule: {saved:?}");
    // ...but the answer is still in effect for this process.
    assert_eq!(
        timeout(WAIT, broker.resolve(ask(&session, "shell", &["echo ok"])))
            .await
            .expect("memory resolve"),
        PermissionDecision::Allow
    );
}

/// A sibling auto-resolved by a rule that only lives in memory is not durable
/// either, so the event cannot claim otherwise.
#[tokio::test]
async fn auto_resolved_sibling_reports_durability_of_its_rule() {
    let fixture = Fixture::new().await;
    let (broker, mut events) = fixture.broker();
    let session = fixture.session_id.clone();
    let star = tokio::spawn({
        let broker = broker.clone();
        let session = session.clone();
        async move { broker.resolve(ask(&session, "shell", &["*"])).await }
    });
    let covered = tokio::spawn({
        let broker = broker.clone();
        let session = session.clone();
        async move { broker.resolve(ask(&session, "shell", &["echo ok"])).await }
    });
    let pending = wait_pending(&broker, &session, 2).await;
    let star_id = request_id(&pending, "shell", &["*"]);
    let covered_id = request_id(&pending, "shell", &["echo ok"]);
    // Force the in-memory path: no session row means `persist` fails.
    fixture
        .store
        .delete_session(&session)
        .await
        .expect("drop session row");
    broker
        .reply(&session, &star_id, PermissionReply::Always, None)
        .expect("reply");

    let event = next_replied(&mut events, &star_id).await;
    assert_eq!(event["durable"], json!(false), "star event: {event}");
    let event = next_replied(&mut events, &covered_id).await;
    assert_eq!(event["reply"], json!("always"));
    assert_eq!(event["durable"], json!(false), "covered event: {event}");
    assert_eq!(
        timeout(WAIT, star).await.expect("star").expect("join"),
        PermissionDecision::Allow
    );
    assert_eq!(
        timeout(WAIT, covered)
            .await
            .expect("covered")
            .expect("join"),
        PermissionDecision::Allow
    );
}
