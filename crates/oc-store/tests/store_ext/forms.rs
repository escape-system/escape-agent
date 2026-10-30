use crate::support::{ids, seed_session};
use oc_store::{NewForm, Store, StoreError};
use serde_json::json;

fn new_form(id: &str, session: &str, title: &str) -> NewForm {
    NewForm {
        id: Some(id.to_owned()),
        session_id: session.to_owned(),
        title: title.to_owned(),
        metadata: Some(json!({ "tool": "ask" })),
        fields: json!([{ "key": "name", "type": "string", "required": true }]),
    }
}
#[tokio::test]
async fn form_lifecycle_conflicts_and_listing() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;
    seed_session(&store, "ses_b").await;

    let created = store
        .create_form(new_form("frm_a", "ses_a", "Pick a name"))
        .await
        .expect("create form");
    assert_eq!(
        created,
        json!({
            "id": "frm_a",
            "sessionID": "ses_a",
            "title": "Pick a name",
            "metadata": { "tool": "ask" },
            "fields": [{ "key": "name", "type": "string", "required": true }],
        })
    );
    assert!(created.get("state").is_none(), "Form.Info has no state");

    store
        .create_form(new_form("frm_b", "ses_a", "Second"))
        .await
        .expect("second form");

    let listed = store
        .list_forms(Some("ses_a"))
        .await
        .expect("list session forms");
    assert_eq!(ids(&listed), vec!["frm_b", "frm_a"]);
    assert!(
        store
            .list_forms(Some("ses_b"))
            .await
            .expect("other session")
            .is_empty()
    );
    assert_eq!(store.list_forms(None).await.expect("all forms").len(), 2);

    let detail = store.get_form("ses_a", "frm_a").await.expect("get form");
    assert_eq!(detail["id"], "frm_a");
    assert_eq!(detail["state"], json!({ "status": "pending" }));
    assert!(matches!(
        store.get_form("ses_b", "frm_a").await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store
            .reply_form("ses_a", "frm_a", &json!({ "answer": "no" }))
            .await,
        Err(StoreError::Invalid(_))
    ));

    store
        .reply_form("ses_a", "frm_a", &json!({ "answer": { "name": "Ada" } }))
        .await
        .expect("reply");
    let answered = store
        .get_form("ses_a", "frm_a")
        .await
        .expect("get answered");
    assert_eq!(
        answered["state"],
        json!({ "status": "answered", "answer": { "name": "Ada" } })
    );
    assert!(matches!(
        store
            .reply_form("ses_a", "frm_a", &json!({ "answer": { "name": "Grace" } }))
            .await,
        Err(StoreError::Conflict(_))
    ));
    assert!(matches!(
        store.cancel_form("ses_a", "frm_a").await,
        Err(StoreError::Conflict(_))
    ));
    assert_eq!(
        ids(&store.list_forms(Some("ses_a")).await.expect("pending")),
        vec!["frm_b"]
    );

    store
        .cancel_form_with_message("ses_a", "frm_b", Some("client closed"))
        .await
        .expect("cancel with message");
    let cancelled = store.get_form("ses_a", "frm_b").await.expect("cancelled");
    assert_eq!(
        cancelled["state"],
        json!({ "status": "cancelled", "message": "client closed" })
    );
    assert!(matches!(
        store.cancel_form_with_message("ses_a", "frm_b", None).await,
        Err(StoreError::Conflict(_))
    ));
    assert!(
        store
            .list_forms(Some("ses_a"))
            .await
            .expect("no pending")
            .is_empty()
    );

    assert!(matches!(
        store
            .create_form(new_form("frm_a", "ses_a", "duplicate"))
            .await,
        Err(StoreError::Conflict(_))
    ));
    assert!(matches!(
        store
            .reply_form("ses_a", "frm_missing", &json!({ "answer": {} }))
            .await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store
            .create_form(NewForm {
                id: Some("form_bad".to_owned()),
                ..new_form("frm_unused", "ses_a", "bad id")
            })
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_form(NewForm {
                fields: json!([]),
                ..new_form("frm_empty", "ses_a", "empty")
            })
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_form(NewForm {
                metadata: Some(json!("not-an-object")),
                ..new_form("frm_meta", "ses_a", "bad metadata")
            })
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_form(new_form("frm_orphan", "ses_missing", "no session"))
            .await,
        Err(StoreError::NotFound(_))
    ));

    let generated = store
        .create_form(NewForm {
            id: None,
            metadata: None,
            ..new_form("frm_unused", "ses_b", "generated")
        })
        .await
        .expect("generated id");
    let generated_id = generated["id"].as_str().expect("id").to_owned();
    assert!(generated_id.starts_with("frm_"));
    assert_eq!(generated_id.len(), 30);
    assert!(
        generated.get("metadata").is_none(),
        "absent metadata is omitted"
    );
}
