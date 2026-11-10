use crate::support::ids;
use oc_store::{Store, StoreError};
use serde_json::json;

#[tokio::test]
async fn lane_tickets_round_trip_newest_first() {
    let store = Store::open_memory().await.expect("memory store");

    store
        .save_lane_ticket(&json!({
            "id": "lan_1",
            "lane": "cargo",
            "command": "cargo test",
            "cwd": "C:/work",
            "priority": 5,
            "state": { "state": "queued" },
            "queuedMs": 100,
        }))
        .await
        .expect("save queued");
    store
        .save_lane_ticket(&json!({
            "id": "lan_1",
            "lane": "cargo",
            "command": "cargo test",
            "cwd": "C:/work",
            "priority": 5,
            "ownerSession": "ses_a",
            "state": { "state": "running" },
            "queuedMs": 100,
            "startedMs": 120,
        }))
        .await
        .expect("save running");
    store
        .save_lane_ticket(&json!({
            "id": "lan_1",
            "lane": "cargo",
            "command": "cargo test",
            "cwd": "C:/work",
            "priority": 5,
            "ownerSession": "ses_a",
            "state": { "state": "failed", "exit": 3, "message": "boom" },
            "queuedMs": 100,
            "startedMs": 120,
            "finishedMs": 200,
            "outputPath": "C:/work/out.txt",
        }))
        .await
        .expect("save failed");
    store
        .save_lane_ticket(&json!({
            "id": "lan_2",
            "lane": "network",
            "command": "curl example.com",
            "cwd": "C:/work",
            "priority": 0,
            "state": { "state": "succeeded", "exit": 0 },
            "queuedMs": 300,
        }))
        .await
        .expect("save second");

    let tickets = store.lane_tickets(0).await.expect("list");
    assert_eq!(ids(&tickets), vec!["lan_2", "lan_1"]);
    assert_eq!(
        tickets[1],
        json!({
            "id": "lan_1",
            "lane": "cargo",
            "command": "cargo test",
            "cwd": "C:/work",
            "priority": 5,
            "ownerSession": "ses_a",
            "state": { "state": "failed", "exit": 3, "message": "boom" },
            "queuedMs": 100,
            "startedMs": 120,
            "finishedMs": 200,
            "outputPath": "C:/work/out.txt",
        })
    );
    assert_eq!(
        tickets[0]["state"],
        json!({ "state": "succeeded", "exit": 0 })
    );
    assert!(tickets[0].get("ownerSession").is_none());
    assert!(tickets[0].get("outputPath").is_none());

    assert_eq!(store.lane_tickets(1).await.expect("limit").len(), 1);
    assert!(matches!(
        store
            .save_lane_ticket(&json!({
                "id": "lan_3",
                "lane": "cargo",
                "command": "true",
                "cwd": "C:/work",
                "priority": 0,
                "state": { "state": "exploded" },
                "queuedMs": 1,
            }))
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store.save_lane_ticket(&json!({ "id": "lan_4" })).await,
        Err(StoreError::Invalid(_))
    ));
}

/// Boot restore reads every row, oldest first, and gets the full persisted
/// shape back — including rows beyond the paged-history limit.
#[tokio::test]
async fn lane_tickets_all_returns_every_row_oldest_first() {
    let store = Store::open_memory().await.expect("memory store");
    store
        .save_lane_ticket(&json!({
            "id": "tkt_3",
            "lane": "cargo:/work",
            "command": "cargo test -p oc-x",
            "cwd": "/work",
            "priority": 2,
            "ownerSession": "ses_c",
            "state": { "state": "queued" },
            "queuedMs": 300,
        }))
        .await
        .expect("save queued");
    store
        .save_lane_ticket(&json!({
            "id": "tkt_1",
            "lane": "cargo:/work",
            "command": "cargo build",
            "cwd": "/work",
            "priority": 0,
            "state": { "state": "running" },
            "queuedMs": 100,
            "startedMs": 120,
        }))
        .await
        .expect("save running");
    store
        .save_lane_ticket(&json!({
            "id": "tkt_2",
            "lane": "network",
            "command": "curl example.com",
            "cwd": "/work",
            "priority": 0,
            "state": { "state": "succeeded", "exit": 0 },
            "queuedMs": 200,
        }))
        .await
        .expect("save succeeded");

    let all = store.lane_tickets_all().await.expect("all rows");
    assert_eq!(ids(&all), vec!["tkt_1", "tkt_2", "tkt_3"]);
    assert_eq!(all[0]["state"], json!({ "state": "running" }));
    assert_eq!(all[0]["startedMs"], json!(120));
    assert_eq!(all[2]["ownerSession"], "ses_c");

    // The paged history is newest first and bounded; the restore query is not.
    let page = store.lane_tickets(0).await.expect("page");
    assert_eq!(ids(&page), vec!["tkt_3", "tkt_2", "tkt_1"]);
    assert_eq!(store.lane_tickets(1).await.expect("limit").len(), 1);
}
