use crate::support::{ids, new_message, seed_session};
use oc_store::{Delivery, MessageQuery, NewInbox, Store, StoreError};
use serde_json::json;

#[tokio::test]
async fn truncate_session_and_viewed_watermark() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;

    for (id, created) in [("msg_1", 1_100), ("msg_2", 1_101), ("msg_3", 1_102)] {
        store
            .append_message(new_message("ses_a", id, created))
            .await
            .expect("append");
    }
    for id in ["inb_1", "inb_2"] {
        store
            .enqueue(NewInbox {
                id: id.to_owned(),
                session_id: "ses_a".to_owned(),
                kind: "user".to_owned(),
                payload: json!({ "text": id }),
                delivery: Delivery::Queue,
            })
            .await
            .expect("enqueue");
    }

    assert_eq!(
        store.message_seq("ses_a", "msg_2").await.expect("seq"),
        Some(2)
    );
    assert_eq!(
        store
            .message_seq("ses_a", "msg_missing")
            .await
            .expect("seq"),
        None
    );

    // seq 1..=3 are messages, 4 and 5 are inbox items.
    assert_eq!(
        store
            .truncate_session("ses_a", 4)
            .await
            .expect("truncate inbox tail"),
        2
    );
    assert!(store.list_inbox("ses_a").await.expect("inbox").is_empty());
    assert!(
        store
            .get_message("ses_a", "msg_3")
            .await
            .expect("message")
            .is_some()
    );

    assert_eq!(
        store.truncate_session("ses_a", 2).await.expect("truncate"),
        2
    );
    assert!(
        store
            .get_message("ses_a", "msg_1")
            .await
            .expect("message")
            .is_some()
    );
    assert!(
        store
            .get_message("ses_a", "msg_2")
            .await
            .expect("message")
            .is_none()
    );
    let remaining = store
        .list_messages(MessageQuery::new("ses_a"))
        .await
        .expect("list messages");
    assert_eq!(ids(&remaining.items), vec!["msg_1"]);
    assert_eq!(
        store.truncate_session("ses_a", 100).await.expect("empty"),
        0
    );
    assert!(matches!(
        store.truncate_session("ses_missing", 0).await,
        Err(StoreError::NotFound(_))
    ));

    store
        .touch_session_viewed("ses_a", 1_700_000_000_000)
        .await
        .expect("touch viewed");
    let session = store
        .get_session("ses_a")
        .await
        .expect("get")
        .expect("present");
    assert_eq!(session["time"]["viewed"].as_i64(), Some(1_700_000_000_000));
    assert!(matches!(
        store.touch_session_viewed("ses_missing", 1).await,
        Err(StoreError::NotFound(_))
    ));
}
