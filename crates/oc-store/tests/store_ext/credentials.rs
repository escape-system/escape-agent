use crate::support::ids;
use oc_store::{CredentialPatch, NewCredential, Store, StoreError};
use serde_json::{Value, json};

fn credential_key(secret: &str) -> Value {
    json!({ "type": "key", "key": secret })
}

#[tokio::test]
async fn credentials_crud_activation_and_ordering() {
    let store = Store::open_memory().await.expect("memory store");

    let first = store
        .create_credential(NewCredential {
            id: Some("cred_a".to_owned()),
            integration_id: "int_z".to_owned(),
            label: None,
            value: credential_key("secret-a"),
            active: None,
        })
        .await
        .expect("create first credential");
    assert_eq!(
        first,
        json!({
            "id": "cred_a",
            "integrationID": "int_z",
            "label": "default",
            "active": true,
            "value": { "type": "key", "key": "secret-a" },
        })
    );

    // Explicitly inactive: the existing selection is kept.
    let second = store
        .create_credential(NewCredential {
            id: Some("cred_b".to_owned()),
            integration_id: "int_z".to_owned(),
            label: Some("backup".to_owned()),
            value: credential_key("secret-b"),
            active: Some(false),
        })
        .await
        .expect("create inactive credential");
    assert_eq!(second["active"], json!(false));
    assert_eq!(second["label"], json!("backup"));

    // The first credential of another integration activates by default.
    let third = store
        .create_credential(NewCredential {
            id: Some("cred_c".to_owned()),
            integration_id: "int_a".to_owned(),
            label: None,
            value: json!({ "type": "external", "methodID": "azure-cli" }),
            active: None,
        })
        .await
        .expect("create other integration credential");
    assert_eq!(third["active"], json!(true));

    let generated = store
        .create_credential(NewCredential {
            id: None,
            integration_id: "int_new".to_owned(),
            label: None,
            value: credential_key("secret-new"),
            active: Some(false),
        })
        .await
        .expect("generated id");
    let generated_id = generated["id"].as_str().expect("id string");
    assert!(generated_id.starts_with("cred_"));
    assert_eq!(generated_id.len(), "cred_".len() + 26);
    assert_eq!(generated["active"], json!(true));

    // integration asc, then id asc (ids encode creation time).
    let listed = store.list_credentials().await.expect("list");
    assert_eq!(
        ids(&listed),
        vec![
            "cred_c".to_owned(),
            generated_id.to_owned(),
            "cred_a".to_owned(),
            "cred_b".to_owned(),
        ]
    );
    let active_in_z = listed
        .iter()
        .filter(|entry| entry["integrationID"] == json!("int_z") && entry["active"] == json!(true))
        .count();
    assert_eq!(active_in_z, 1);

    let activated = store.activate_credential("cred_b").await.expect("activate");
    assert_eq!(activated["active"], json!(true));
    let listed = store.list_credentials().await.expect("relists");
    assert_eq!(
        listed
            .iter()
            .filter(
                |entry| entry["integrationID"] == json!("int_z") && entry["active"] == json!(true)
            )
            .count(),
        1
    );
    assert_eq!(
        store.get_credential("cred_a").await.expect("get").unwrap()["active"],
        json!(false)
    );

    let patched = store
        .update_credential(
            "cred_a",
            CredentialPatch {
                label: Some("rotated".to_owned()),
                value: Some(credential_key("secret-a2")),
                active: Some(true),
            },
        )
        .await
        .expect("patch")
        .expect("row exists");
    assert_eq!(patched["label"], json!("rotated"));
    assert_eq!(patched["value"], credential_key("secret-a2"));
    assert_eq!(patched["active"], json!(true));
    let listed = store.list_credentials().await.expect("relists after patch");
    assert_eq!(
        listed
            .iter()
            .filter(
                |entry| entry["integrationID"] == json!("int_z") && entry["active"] == json!(true)
            )
            .count(),
        1
    );

    assert_eq!(
        store
            .update_credential("cred_missing", CredentialPatch::default())
            .await
            .expect("patch missing"),
        None
    );
    assert_eq!(
        store
            .get_credential("cred_missing")
            .await
            .expect("get missing"),
        None
    );

    assert!(store.delete_credential("cred_b").await.expect("delete"));
    assert!(!store.delete_credential("cred_b").await.expect("re-delete"));
    assert!(matches!(
        store.activate_credential("cred_b").await,
        Err(StoreError::NotFound(_))
    ));

    assert!(matches!(
        store
            .create_credential(NewCredential {
                id: Some("cred_a".to_owned()),
                integration_id: "int_z".to_owned(),
                label: None,
                value: credential_key("duplicate"),
                active: None,
            })
            .await,
        Err(StoreError::Conflict(_))
    ));
    assert!(matches!(
        store
            .create_credential(NewCredential {
                id: Some("cred_bad".to_owned()),
                integration_id: "int_z".to_owned(),
                label: None,
                value: json!({ "key": "untyped" }),
                active: None,
            })
            .await,
        Err(StoreError::Invalid(_))
    ));
}
