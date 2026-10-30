use crate::support::seed_session;
use oc_store::{Store, StoreError};
use serde_json::json;

#[tokio::test]
async fn instruction_entries_set_replace_and_remove() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;

    store
        .set_instruction_entry("ses_a", "beta", Some("\"two\""))
        .await
        .expect("put beta");
    store
        .set_instruction_entry("ses_a", "alpha", Some("{\"n\":1}"))
        .await
        .expect("put alpha");
    let entries = store.instruction_entries("ses_a").await.expect("list");
    assert_eq!(
        entries,
        vec![
            json!({ "key": "alpha", "value": { "n": 1 } }),
            json!({ "key": "beta", "value": "two" }),
        ]
    );

    store
        .set_instruction_entry("ses_a", "alpha", Some("plain text"))
        .await
        .expect("replace with non-json");
    let replaced = store.instruction_entries("ses_a").await.expect("list");
    assert_eq!(replaced[0]["key"], "alpha");
    assert_eq!(replaced[0]["value"], "plain text");

    store
        .set_instruction_entry("ses_a", "beta", None)
        .await
        .expect("remove beta");
    store
        .set_instruction_entry("ses_a", "beta", None)
        .await
        .expect("remove beta again");
    let remaining = store.instruction_entries("ses_a").await.expect("list");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["key"], "alpha");

    assert!(matches!(
        store
            .set_instruction_entry("ses_missing", "alpha", Some("1"))
            .await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store.instruction_entries("ses_missing").await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store.set_instruction_entry("ses_a", "", Some("1")).await,
        Err(StoreError::Invalid(_))
    ));
}
