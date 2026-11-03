//! Instruction discovery must follow the session's directory, not the
//! directory the server process loaded its configuration from.
//!
//! Regression: a server started in `/root/escape-agent` served that repo's
//! `AGENTS.md` to sessions whose `location.directory` was another project, so
//! child agents were told to read files that do not exist in their repo.

use std::sync::Arc;
use std::time::Duration;

use oc_store::{Delivery, NewInbox, NewSession};
use oc_types::MessageId;
use serde_json::json;

use crate::support::*;

const SERVER_TOKEN: &str = "SERVER_CONTRACT_TOKEN";
const SESSION_TOKEN: &str = "SESSION_CONTRACT_TOKEN";

#[tokio::test]
async fn instructions_follow_the_session_directory_not_the_config_directory() {
    let harness = Harness::new_with_setup(
        vec![text_script("done")],
        Arc::new(AllowAll),
        json!({}),
        None,
        |work| {
            std::fs::write(
                work.join("AGENTS.md"),
                format!("# server contract\n{SERVER_TOKEN}\n"),
            )
            .expect("server contract");
        },
    )
    .await;

    // A sibling of the server directory, so the two instruction walks share
    // only ancestors that hold no fixture files.
    let session_dir = harness.root.path().join("project");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    std::fs::write(
        session_dir.join("AGENTS.md"),
        format!("# project contract\n{SESSION_TOKEN}\n"),
    )
    .expect("session contract");

    let session = "ses_scoped_instructions";
    harness
        .store
        .create_session(NewSession {
            id: session.to_owned(),
            parent_id: None,
            project_id: "prj_scoped".to_owned(),
            directory: path(&session_dir),
            title: Some("scoped instructions".to_owned()),
            agent: Some("build".to_owned()),
            model: Some(json!({"providerID": "test", "id": "alpha"})),
            metadata: None,
            permissions: None,
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("session created");

    harness
        .store
        .enqueue(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: session.to_owned(),
            kind: "user".to_owned(),
            payload: json!({"text": "hello", "time": {"created": oc_types::now_millis()}}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueued");
    harness.runtime.notify(session).await.expect("notify");
    harness
        .runtime
        .wait_idle(session, Duration::from_secs(10))
        .await
        .expect("idle");

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1, "one provider call");
    let instructions = requests[0]
        .system
        .last()
        .expect("the turn carries an instruction part");
    assert!(
        instructions.contains(SESSION_TOKEN),
        "the session directory's AGENTS.md must be part of the prefix, got:\n{instructions}"
    );
    assert!(
        !instructions.contains(SERVER_TOKEN),
        "the server's own AGENTS.md must never leak into another project, got:\n{instructions}"
    );
}
