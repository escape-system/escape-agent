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

fn key_credential(id: &str, integration: &str, secret: &str, active: bool) -> NewCredential {
    NewCredential {
        id: Some(id.to_owned()),
        integration_id: integration.to_owned(),
        label: Some(format!("label-{id}")),
        value: credential_key(secret),
        active: Some(active),
    }
}

#[tokio::test]
async fn credential_pool_returns_active_first_then_id_ascending() {
    let store = Store::open_memory().await.expect("memory store");
    // Created in a deliberately scrambled order so the ordering cannot come
    // from insertion order.
    for (id, active) in [("cred_d", false), ("cred_b", true), ("cred_a", false)] {
        store
            .create_credential(key_credential(id, "int_keys", id, active))
            .await
            .expect("create credential");
    }
    store
        .create_credential(key_credential("cred_x", "int_other", "x", true))
        .await
        .expect("create other integration");

    let pool = store.credential_pool("int_keys").await.expect("pool");
    assert_eq!(
        pool.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        vec!["cred_b", "cred_a", "cred_d"]
    );
    assert_eq!(
        pool.iter().filter(|row| row.active).count(),
        1,
        "exactly one row is primary"
    );
    assert!(pool.iter().all(|row| row.integration_id == "int_keys"));
    let first = &pool[0];
    assert_eq!(first.label, "label-cred_b");
    assert_eq!(first.value, credential_key("cred_b"));

    assert!(
        store
            .credential_pool("int_missing")
            .await
            .expect("empty pool")
            .is_empty()
    );
}

#[tokio::test]
async fn set_credential_primary_clears_siblings_and_survives_reopen() {
    let directory = tempfile::TempDir::new().expect("temp dir");
    let path = directory.path().join("pool.db");
    let store = Store::open(&path).await.expect("open store");
    for (id, active) in [("cred_a", true), ("cred_b", false), ("cred_c", false)] {
        store
            .create_credential(key_credential(id, "int_keys", id, active))
            .await
            .expect("create credential");
    }

    assert!(
        store
            .set_credential_primary("int_keys", "cred_c")
            .await
            .expect("set primary")
    );
    let pool = store.credential_pool("int_keys").await.expect("pool");
    assert_eq!(
        pool.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        vec!["cred_c", "cred_a", "cred_b"],
        "the new primary sorts first"
    );
    assert_eq!(
        pool.iter().filter(|row| row.active).count(),
        1,
        "siblings were cleared in the same transaction"
    );
    assert_eq!(
        store
            .get_credential("cred_a")
            .await
            .expect("get")
            .expect("row")["active"],
        json!(false)
    );

    // Unknown ids and foreign integrations are refused without writing.
    assert!(
        !store
            .set_credential_primary("int_keys", "cred_missing")
            .await
            .expect("unknown id")
    );
    store
        .create_credential(key_credential("cred_other", "int_other", "other", true))
        .await
        .expect("create other integration");
    assert!(
        !store
            .set_credential_primary("int_keys", "cred_other")
            .await
            .expect("foreign id")
    );
    assert_eq!(
        store.credential_pool("int_keys").await.expect("pool")[0].id,
        "cred_c",
        "a refused call leaves the selection untouched"
    );

    drop(store);
    let reopened = Store::open(&path).await.expect("reopen");
    let pool = reopened.credential_pool("int_keys").await.expect("pool");
    assert_eq!(
        pool.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        vec!["cred_c", "cred_a", "cred_b"],
        "the rotation priority survived a restart"
    );
    assert!(pool[0].active);
}

#[tokio::test]
async fn deleting_the_active_credential_leaves_the_rest_of_the_pool() {
    let store = Store::open_memory().await.expect("memory store");
    for (id, active) in [("cred_a", true), ("cred_b", false)] {
        store
            .create_credential(key_credential(id, "int_keys", id, active))
            .await
            .expect("create credential");
    }
    assert!(store.delete_credential("cred_a").await.expect("delete"));
    let pool = store.credential_pool("int_keys").await.expect("pool");
    assert_eq!(
        pool.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        vec!["cred_b"],
        "the backup key is still usable"
    );
    assert_eq!(pool[0].value, credential_key("cred_b"));
}
