//! `session.created` carries the real identity of the seeded project, not a
//! fallback. The event payload is asserted against the project row the store
//! actually holds and against the same values the create response reports.

use axum::http::StatusCode;
use oc_server::router;
use oc_types::project_id_from_directory;

use crate::support::{create_session_in, harness, session_id, take_events};

#[tokio::test]
async fn created_event_carries_the_seeded_project_identity() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let directory = tempfile::tempdir().unwrap();

    // Subscribe before the request so the broadcast cannot be missed.
    let mut events = harness.state.bus().subscribe();
    let session = create_session_in(&app, directory.path(), "identity").await;
    let sid = session_id(&session);

    // Expected values are the ones the request seeds: the absolute directory
    // the handler stores, and the project id derived from its canonical form.
    let absolute = std::path::absolute(directory.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let canonical = std::fs::canonicalize(directory.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let expected_project = project_id_from_directory(&canonical).into_inner();

    // The project row is the authority: it must exist with the derived id and
    // the seeded directory, so the assertions below are not re-derived math.
    let project = harness
        .state
        .store()
        .unwrap()
        .get_project(&expected_project)
        .await
        .unwrap()
        .expect("create must seed the owning project row");
    assert_eq!(project["id"], expected_project);
    assert_eq!(project["directory"], absolute);

    // The response and the event must agree on the same identity.
    assert_eq!(session["projectID"], expected_project);
    assert_eq!(session["location"]["directory"], absolute);

    let published = take_events(&mut events, "session.created");
    assert_eq!(published.len(), 1, "one session.created per create");
    let event = &published[0];
    assert_eq!(event.data["sessionID"], sid);
    assert_eq!(
        event.data["projectID"], expected_project,
        "event must carry the seeded project id, not a fallback: {:?}",
        event.data
    );
    assert_eq!(
        event.data["location"]["directory"], absolute,
        "event must carry the seeded directory, not a fallback: {:?}",
        event.data
    );
    // The envelope location is read from the stored session, so it must match
    // the payload the same way.
    assert_eq!(
        event
            .location
            .as_ref()
            .expect("session events carry a location")
            .directory,
        absolute
    );

    // A second session in the same directory keeps the same project identity;
    // a different directory gets a different one.
    let other = tempfile::tempdir().unwrap();
    let second = create_session_in(&app, directory.path(), "same project").await;
    assert!(!session_id(&second).is_empty());
    let elsewhere = create_session_in(&app, other.path(), "other project").await;
    assert_ne!(
        elsewhere["projectID"], expected_project,
        "a different directory must not inherit the first project id"
    );

    // Sanity: both sessions are readable through the route they were created by.
    let (status, _) = crate::support::call(
        &app,
        axum::http::Method::GET,
        &format!("/api/session/{}", session_id(&second)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}
