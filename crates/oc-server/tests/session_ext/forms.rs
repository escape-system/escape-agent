use axum::http::{Method, StatusCode};
use oc_server::router;
use serde_json::{Value, json};

use crate::support::{call, create_session, harness, session_id, take_events};

#[allow(clippy::too_many_lines)] // one linear lifecycle scenario; splitting hides the created-then-replied order
#[tokio::test]
async fn forms_report_lifecycle_and_conflicts() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "forms"})).await;
    let sid = session_id(&session);
    let mut events = harness.state.bus().subscribe();
    let form = json!({
        "id": "frm_demo",
        "title": "Pick one",
        "metadata": {"source": "test"},
        "fields": [{"key": "answer", "type": "string", "required": true}],
    });
    let (status, created) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/form"),
        Some(form.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create form: {created:?}");
    let created = created.unwrap()["data"].clone();
    assert_eq!(created["id"], "frm_demo");
    assert_eq!(created["sessionID"], sid);
    assert!(created.get("state").is_none(), "Form.Info has no state");

    let published = take_events(&mut events, "form.created");
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].data, json!({ "form": created }));
    assert!(published[0].durable.is_none(), "form events are ephemeral");
    assert_eq!(
        published[0].location.as_ref().unwrap().directory,
        session["location"]["directory"].as_str().unwrap()
    );

    let (status, duplicate) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/form"),
        Some(form),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(duplicate.unwrap()["_tag"], "ConflictError");

    let (status, list) = call(&app, Method::GET, &format!("/api/session/{sid}/form"), None).await;
    assert_eq!(status, StatusCode::OK);
    let list = list.unwrap();
    assert_eq!(list["data"].as_array().unwrap().len(), 1);

    let (status, detail) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/form/frm_demo"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        detail.unwrap()["data"]["state"],
        json!({"status": "pending"})
    );

    let (status, invalid) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/form/frm_demo/reply"),
        Some(json!({"answer": {"unknown": "x"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid.unwrap()["_tag"], "FormInvalidAnswerError");
    assert!(
        take_events(&mut events, "form.replied").is_empty(),
        "a rejected reply must not publish"
    );

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/form/frm_demo/reply"),
        Some(json!({"answer": {"answer": "yes"}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "reply: {body:?}");

    let published = take_events(&mut events, "form.replied");
    assert_eq!(published.len(), 1);
    assert_eq!(
        published[0].data,
        json!({ "id": "frm_demo", "sessionID": sid, "answer": {"answer": "yes"} })
    );
    assert!(published[0].durable.is_none(), "form events are ephemeral");

    let (_, detail) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/form/frm_demo"),
        None,
    )
    .await;
    let detail = detail.unwrap()["data"].clone();
    assert_eq!(detail["state"]["status"], "answered");
    assert_eq!(detail["state"]["answer"], json!({"answer": "yes"}));

    let (status, settled) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/form/frm_demo/reply"),
        Some(json!({"answer": {"answer": "yes"}})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(settled.unwrap()["_tag"], "FormAlreadySettledError");
    assert!(
        take_events(&mut events, "form.replied").is_empty(),
        "a settled form must not publish a second reply"
    );

    let (status, missing) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/form/frm_absent"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing.unwrap()["_tag"], "FormNotFoundError");
}

#[tokio::test]
async fn form_cancel_stores_message() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "cancel"})).await;
    let sid = session_id(&session);
    let mut events = harness.state.bus().subscribe();
    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/form"),
        Some(json!({
            "title": "Cancel me",
            "fields": [{"key": "answer", "type": "string"}],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, list) = call(&app, Method::GET, &format!("/api/session/{sid}/form"), None).await;
    let form_id = list.unwrap()["data"][0]["id"].as_str().unwrap().to_owned();

    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/session/{sid}/form/{form_id}?message=not%20now"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let published = take_events(&mut events, "form.cancelled");
    assert_eq!(published.len(), 1);
    assert_eq!(
        published[0].data,
        json!({ "id": form_id, "sessionID": sid })
    );
    assert!(published[0].durable.is_none(), "form events are ephemeral");
    assert_eq!(
        published[0].location.as_ref().unwrap().directory,
        session["location"]["directory"].as_str().unwrap()
    );

    let (status, detail) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/form/{form_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let state = detail.unwrap()["data"]["state"].clone();
    assert_eq!(state["status"], "cancelled");
    assert_eq!(state["message"], "not now");
}

#[tokio::test]
async fn form_create_rejects_duplicate_keys_and_bad_field_shapes() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "validation"})).await;
    let sid = session_id(&session);

    let cases = [
        (
            json!({"title": "empty", "fields": []}),
            "fields must not be empty",
        ),
        (
            json!({
                "title": "duplicate",
                "fields": [
                    {"key": "answer", "type": "string"},
                    {"key": "answer", "type": "string"},
                ],
            }),
            "duplicate field key: answer",
        ),
        (
            json!({"title": "kind", "fields": [{"key": "answer", "type": "wat"}]}),
            "field answer has unknown type \"wat\"",
        ),
        (
            json!({"title": "external", "fields": [{"key": "answer", "type": "external"}]}),
            "external field answer needs a url",
        ),
    ];
    for (body, message) in cases {
        let (status, response) = call(
            &app,
            Method::POST,
            &format!("/api/session/{sid}/form"),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{message}");
        let response = response.unwrap();
        assert_eq!(response["_tag"], "BadRequestError");
        assert_eq!(response["message"], message);
    }

    let (status, list) = call(&app, Method::GET, &format!("/api/session/{sid}/form"), None).await;
    assert_eq!(status, StatusCode::OK, "{list:?}");
    assert_eq!(list.unwrap()["data"], json!([]));
}

#[tokio::test]
async fn form_reply_enforces_required_and_typed_answers() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "answers"})).await;
    let sid = session_id(&session);
    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/form"),
        Some(json!({
            "id": "frm_typed",
            "title": "Typed",
            "fields": [
                {"key": "answer", "type": "string", "required": true},
                {"key": "count", "type": "integer"},
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let cases = [
        (json!({}), "missing required field answer"),
        (
            json!({"answer": 1}),
            "field answer does not accept this value",
        ),
        (json!({"answer": "yes", "nope": 1}), "unknown field nope"),
    ];
    for (answer, message) in cases {
        let (status, response) = call(
            &app,
            Method::POST,
            &format!("/api/session/{sid}/form/frm_typed/reply"),
            Some(json!({"answer": answer})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{message}");
        let response = response.unwrap();
        assert_eq!(response["_tag"], "FormInvalidAnswerError");
        assert_eq!(response["message"], message);
        assert_eq!(response["id"], "frm_typed");
    }

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/form/frm_typed/reply"),
        Some(json!({"answer": {"answer": "yes", "count": 2}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let (status, detail) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/form/frm_typed"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let state = detail.unwrap()["data"]["state"].clone();
    assert_eq!(state["status"], "answered");
    assert_eq!(state["answer"], json!({"answer": "yes", "count": 2}));
}

#[tokio::test]
async fn form_list_hides_settled_forms_and_detail_is_session_scoped() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let first = session_id(&create_session(&app, json!({"title": "one"})).await);
    let second = session_id(&create_session(&app, json!({"title": "two"})).await);
    for (sid, id) in [
        (first.as_str(), "frm_first"),
        (first.as_str(), "frm_second"),
        (second.as_str(), "frm_other"),
    ] {
        let (status, body) = call(
            &app,
            Method::POST,
            &format!("/api/session/{sid}/form"),
            Some(json!({
                "id": id,
                "title": id,
                "fields": [{"key": "answer", "type": "string"}],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{id}: {body:?}");
    }

    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{first}/form/frm_first/reply"),
        Some(json!({"answer": {"answer": "done"}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, list) = call(
        &app,
        Method::GET,
        &format!("/api/session/{first}/form"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed: Vec<Value> = list.unwrap()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|form| form["id"].clone())
        .collect();
    assert_eq!(listed, vec![json!("frm_second")]);

    let (status, list) = call(
        &app,
        Method::GET,
        &format!("/api/session/{second}/form"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let listed: Vec<Value> = list.unwrap()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|form| form["id"].clone())
        .collect();
    assert_eq!(listed, vec![json!("frm_other")]);

    let (status, missing) = call(
        &app,
        Method::GET,
        &format!("/api/session/{second}/form/frm_first"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing.unwrap()["_tag"], "FormNotFoundError");

    let (status, detail) = call(
        &app,
        Method::GET,
        &format!("/api/session/{first}/form/frm_first"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail.unwrap()["data"]["state"]["status"], "answered");
}
