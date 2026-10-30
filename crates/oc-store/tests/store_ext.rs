//! Integration tests for the extended store APIs: forms, saved permissions,
//! instruction entries, session revert/viewed maintenance and lane tickets.
//!
//! Every test uses a private in-memory database; no network or clock
//! assumptions beyond monotonic ids/order keys supplied by the test.

#![allow(clippy::too_many_lines)] // scenario tests read top-to-bottom on purpose

use oc_store::{
    CredentialPatch, Delivery, MessageQuery, NewCredential, NewForm, NewInbox, NewMessage,
    NewSchedule, NewSession, NewSnapshot, NewWorktree, ScheduleAction, ScheduleEnds,
    ScheduleOutcome, SchedulePatch, ScheduleWhen, Store, StoreError, duration_ms,
};
use serde_json::{Value, json};

fn new_session(id: &str) -> NewSession {
    NewSession {
        id: id.to_owned(),
        parent_id: None,
        project_id: "prj_1".to_owned(),
        directory: "/tmp/one".to_owned(),
        title: Some(format!("title {id}")),
        agent: None,
        model: None,
        metadata: None,
        permissions: None,
        time_created: 1_000,
    }
}

fn new_message(session: &str, id: &str, created: i64) -> NewMessage {
    NewMessage {
        id: id.to_owned(),
        session_id: session.to_owned(),
        kind: "user".to_owned(),
        data: json!({ "text": id }),
        time_created: created,
    }
}

fn new_form(id: &str, session: &str, title: &str) -> NewForm {
    NewForm {
        id: Some(id.to_owned()),
        session_id: session.to_owned(),
        title: title.to_owned(),
        metadata: Some(json!({ "tool": "ask" })),
        fields: json!([{ "key": "name", "type": "string", "required": true }]),
    }
}

fn ids(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .map(|value| value["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

async fn seed_session(store: &Store, id: &str) {
    store
        .create_session(new_session(id))
        .await
        .expect("create session");
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

#[tokio::test]
async fn saved_permissions_are_idempotent_and_updatable() {
    let store = Store::open_memory().await.expect("memory store");

    let first = store
        .save_permission_rule("prj_1", "read", "/tmp/a")
        .await
        .expect("save");
    assert_eq!(first["projectID"], "prj_1");
    assert_eq!(first["action"], "read");
    assert_eq!(first["resource"], "/tmp/a");
    assert!(
        first["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("psv_"))
    );
    let created = first["time"]["created"].as_i64().expect("created");
    let id = first["id"].as_str().expect("id").to_owned();

    let again = store
        .save_permission_rule("prj_1", "read", "/tmp/a")
        .await
        .expect("idempotent save");
    assert_eq!(again["id"], id.as_str());
    assert_eq!(again["time"]["created"], created);
    assert_eq!(
        store
            .list_saved_permissions(Some("prj_1"))
            .await
            .expect("list")
            .len(),
        1
    );

    store
        .save_permission_rule("prj_1", "write", "/tmp/b")
        .await
        .expect("second rule");
    store
        .save_permission_rule("prj_2", "read", "/tmp/a")
        .await
        .expect("other project");
    assert_eq!(
        store
            .list_saved_permissions(Some("prj_1"))
            .await
            .expect("project list")
            .len(),
        2
    );
    assert_eq!(
        store
            .list_saved_permissions(None)
            .await
            .expect("all list")
            .len(),
        3
    );

    let updated = store
        .update_saved_permission(&id, Some("prj_1"), Some("edit"), None)
        .await
        .expect("update")
        .expect("present");
    assert_eq!(updated["id"], id.as_str());
    assert_eq!(updated["action"], "edit");
    assert_eq!(updated["resource"], "/tmp/a");
    assert!(
        store
            .update_saved_permission(&id, Some("prj_2"), Some("deny"), None)
            .await
            .expect("wrong project")
            .is_none()
    );
    assert!(matches!(
        store
            .update_saved_permission(&id, None, Some("write"), Some("/tmp/b"))
            .await,
        Err(StoreError::Conflict(_))
    ));
    assert!(
        store
            .update_saved_permission("psv_missing", None, Some("read"), None)
            .await
            .expect("missing")
            .is_none()
    );

    assert!(store.delete_saved_permission(&id).await.expect("delete"));
    assert!(
        !store
            .delete_saved_permission(&id)
            .await
            .expect("delete again")
    );
    assert_eq!(
        store
            .list_saved_permissions(Some("prj_1"))
            .await
            .expect("after delete")
            .len(),
        1
    );
}

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

fn new_worktree(id: &str, project: &str, directory: &str, created: i64) -> NewWorktree {
    NewWorktree {
        id: Some(id.to_owned()),
        project_id: project.to_owned(),
        directory: directory.to_owned(),
        canonical: Some(directory.to_owned()),
        branch: Some(format!("branch-{id}")),
        name: Some(id.to_owned()),
        time_created: Some(created),
        time_updated: None,
    }
}

#[tokio::test]
async fn worktrees_create_list_filter_and_remove() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_one").await;
    let mut second = new_session("ses_two");
    second.project_id = "prj_2".to_owned();
    second.directory = "/tmp/two".to_owned();
    store.create_session(second).await.expect("second project");

    let older = store
        .create_worktree(new_worktree("wkt_a", "prj_1", "/tmp/wt-a", 100))
        .await
        .expect("create oldest");
    assert_eq!(older, json!({ "directory": "/tmp/wt-a" }));
    let newer = store
        .create_worktree(new_worktree("wkt_b", "prj_1", "/tmp/wt-b", 200))
        .await
        .expect("create newest");
    assert_eq!(newer, json!({ "directory": "/tmp/wt-b" }));
    let other = store
        .create_worktree(new_worktree("wkt_c", "prj_2", "/tmp/wt-a", 150))
        .await
        .expect("same directory under another project");
    assert_eq!(other, json!({ "directory": "/tmp/wt-a" }));

    let directories = |values: &[Value]| -> Vec<String> {
        values
            .iter()
            .map(|value| value["directory"].as_str().unwrap_or_default().to_owned())
            .collect()
    };

    assert_eq!(
        directories(&store.list_worktrees(None).await.expect("list all")),
        vec!["/tmp/wt-b", "/tmp/wt-a", "/tmp/wt-a"]
    );
    assert_eq!(
        directories(
            &store
                .list_worktrees(Some("prj_1"))
                .await
                .expect("list project")
        ),
        vec!["/tmp/wt-b", "/tmp/wt-a"]
    );
    assert_eq!(
        store.get_worktree("wkt_a").await.expect("get worktree"),
        Some(json!({ "directory": "/tmp/wt-a" }))
    );
    assert_eq!(
        store
            .get_worktree("wkt_missing")
            .await
            .expect("get missing"),
        None
    );

    assert!(matches!(
        store
            .create_worktree(new_worktree("wkt_a", "prj_1", "/tmp/elsewhere", 300))
            .await,
        Err(StoreError::Conflict(_))
    ));
    assert!(matches!(
        store
            .create_worktree(new_worktree("wkt_d", "prj_1", "/tmp/wt-a", 300))
            .await,
        Err(StoreError::Conflict(_))
    ));
    assert!(matches!(
        store
            .create_worktree(new_worktree("wkt_e", "prj_missing", "/tmp/wt-e", 300))
            .await,
        Err(StoreError::NotFound(_))
    ));

    assert!(store.delete_worktree("wkt_a").await.expect("delete"));
    assert!(!store.delete_worktree("wkt_a").await.expect("re-delete"));
    assert!(
        store
            .delete_worktree_by_directory("prj_2", "/tmp/wt-a")
            .await
            .expect("delete by directory")
    );
    assert!(
        !store
            .delete_worktree_by_directory("prj_2", "/tmp/wt-a")
            .await
            .expect("re-delete by directory")
    );
    assert_eq!(
        directories(
            &store
                .list_worktrees(Some("prj_1"))
                .await
                .expect("list after delete")
        ),
        vec!["/tmp/wt-b"]
    );
}

fn schedule_prompt(prompt: &str) -> ScheduleAction {
    ScheduleAction::Prompt {
        prompt: prompt.to_owned(),
    }
}

fn scheduled_in(
    session: &str,
    name: Option<&str>,
    duration: &str,
    prompt: &str,
    created: i64,
) -> NewSchedule {
    NewSchedule {
        session_id: session.to_owned(),
        name: name.map(str::to_owned),
        when: ScheduleWhen::In {
            r#in: duration.to_owned(),
            tz: None,
        },
        ends: None,
        action: schedule_prompt(prompt),
        time_created: created,
    }
}

fn cron_when(expression: &str, tz: Option<&str>) -> ScheduleWhen {
    ScheduleWhen::Cron {
        cron: expression.to_owned(),
        tz: tz.map(str::to_owned),
    }
}

#[test]
fn schedule_duration_parsing_and_cron_math() {
    assert_eq!(duration_ms("90 seconds").expect("seconds"), 90_000);
    assert_eq!(duration_ms("5 minutes").expect("minutes"), 300_000);
    assert_eq!(duration_ms("2 hours").expect("hours"), 7_200_000);
    assert_eq!(duration_ms("1 day").expect("days"), 86_400_000);
    assert_eq!(duration_ms("1 hour 30 minutes").expect("pairs"), 5_400_000);
    assert_eq!(duration_ms("120").expect("plain seconds"), 120_000);
    assert_eq!(duration_ms("  10 secs ").expect("trimmed"), 10_000);
    for bad in [
        "",
        "soon",
        "0 seconds",
        "-5 minutes",
        "1 hour 30",
        "1.5 hours",
    ] {
        assert!(
            matches!(duration_ms(bad), Err(StoreError::Invalid(_))),
            "{bad:?} must be rejected"
        );
    }

    let one_shot = ScheduleWhen::In {
        r#in: "5 minutes".to_owned(),
        tz: None,
    };
    assert_eq!(one_shot.first_due(1_000).expect("in due"), 301_000);
    assert_eq!(
        one_shot.next_after(10_000_000, 301_000).expect("in next"),
        None
    );

    let every = ScheduleWhen::Every {
        every: "1 minute".to_owned(),
        tz: None,
    };
    assert_eq!(every.first_due(0).expect("every due"), 60_000);
    assert_eq!(
        every.next_after(185_000, 60_000).expect("every next"),
        Some(240_000)
    );
    assert_eq!(
        every.next_after(50_000, 60_000).expect("future previous"),
        Some(60_000)
    );
    assert!(
        matches!(
            ScheduleWhen::Every {
                every: "30 seconds".to_owned(),
                tz: None,
            }
            .first_due(0),
            Err(StoreError::Invalid(_))
        ),
        "every must be at least a minute"
    );

    assert_eq!(
        ScheduleWhen::At {
            at: "1970-01-01T00:05:00Z".to_owned(),
            tz: None,
        }
        .first_due(0)
        .expect("rfc3339"),
        300_000
    );
    assert_eq!(
        ScheduleWhen::At {
            at: "300000".to_owned(),
            tz: None,
        }
        .first_due(299_999)
        .expect("epoch millis"),
        300_000
    );
    assert_eq!(
        ScheduleWhen::At {
            at: "1970-01-01T00:00:01.250Z".to_owned(),
            tz: None,
        }
        .first_due(0)
        .expect("fraction"),
        1_250
    );
    assert_eq!(
        ScheduleWhen::At {
            at: "1970-01-01T05:30:00+05:30".to_owned(),
            tz: None,
        }
        .first_due(-1)
        .expect("explicit offset"),
        0
    );
    assert_eq!(
        ScheduleWhen::At {
            at: "1970-01-01T05:30:00".to_owned(),
            tz: Some("+05:30".to_owned()),
        }
        .first_due(-1)
        .expect("wall clock in offset"),
        0
    );
    for bad in [
        "1970-01-01T00:00:00Z",
        "2026-02-30T00:00:00Z",
        "2026-01-01 25:00:00Z",
        "2026-01-01T00:00:60Z",
        "not a time",
    ] {
        assert!(
            matches!(
                ScheduleWhen::At {
                    at: bad.to_owned(),
                    tz: None,
                }
                .first_due(0),
                Err(StoreError::Invalid(_))
            ),
            "{bad:?} must be rejected"
        );
    }
    assert!(matches!(
        ScheduleWhen::In {
            r#in: "5 minutes".to_owned(),
            tz: Some("Europe/Berlin".to_owned()),
        }
        .first_due(0),
        Err(StoreError::Invalid(_))
    ));

    assert_eq!(
        cron_when("*/5 * * * *", None)
            .first_due(0)
            .expect("five-minute cron"),
        300_000
    );
    assert_eq!(
        cron_when("0 9 * * *", Some("+02:00"))
            .first_due(0)
            .expect("offset cron"),
        25_200_000
    );
    assert_eq!(
        cron_when("0 0 * * 4", None)
            .first_due(0)
            .expect("weekday cron"),
        604_800_000
    );
    assert_eq!(
        cron_when("0,30 9-10 * * 1-5", None)
            .first_due(0)
            .expect("list and range"),
        32_400_000
    );
    assert_eq!(
        cron_when("*/5 * * * *", None)
            .next_after(0, 0)
            .expect("cron next"),
        Some(300_000)
    );
    assert!(matches!(
        cron_when("0 0 31 2 *", None).first_due(0),
        Err(StoreError::Invalid(_))
    ));
    for bad in ["0 0 * *", "60 * * * *", "*/0 * * * *", "0 0 * * MON"] {
        assert!(
            matches!(
                cron_when(bad, None).first_due(0),
                Err(StoreError::Invalid(_))
            ),
            "{bad:?} must be rejected"
        );
    }
}

#[tokio::test]
async fn schedule_create_list_get_update_pause_delete() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;
    seed_session(&store, "ses_b").await;

    let created = store
        .create_schedule(scheduled_in(
            "ses_a",
            Some("wake"),
            "5 minutes",
            "hello",
            1_000_000,
        ))
        .await
        .expect("create schedule");
    let id = created["id"].as_str().expect("schedule id").to_owned();
    assert!(id.starts_with("sch_"), "id has the wire prefix: {id}");
    assert_eq!(
        created,
        json!({
            "id": id.clone(),
            "sessionID": "ses_a",
            "name": "wake",
            "when": { "type": "in", "in": "5 minutes", "tz": "UTC" },
            "tz": "UTC",
            "action": { "type": "prompt", "prompt": "hello" },
            "status": "active",
            "nextDueAt": 1_300_000,
            "runCount": 0,
            "time": { "created": 1_000_000, "updated": 1_000_000 },
        })
    );

    let unnamed = store
        .create_schedule(scheduled_in("ses_a", None, "1 minute", "second", 1_500_000))
        .await
        .expect("unnamed schedule");
    assert!(unnamed.get("name").is_none());

    let repeated = store
        .create_schedule(NewSchedule {
            session_id: "ses_b".to_owned(),
            name: Some("hourly".to_owned()),
            when: ScheduleWhen::Every {
                every: "2 hours".to_owned(),
                tz: None,
            },
            ends: Some(ScheduleEnds {
                after: None,
                at: None,
                times: Some(3),
            }),
            action: schedule_prompt("tick"),
            time_created: 2_000_000,
        })
        .await
        .expect("repeating schedule");
    assert_eq!(repeated["nextDueAt"], 9_200_000);
    assert_eq!(repeated["remainingRuns"], 3);
    assert!(repeated.get("endsAt").is_none());

    let all = store.list_schedules(None).await.expect("list all");
    assert_eq!(
        ids(&all),
        vec![
            id.clone(),
            unnamed["id"].as_str().unwrap().to_owned(),
            repeated["id"].as_str().unwrap().to_owned(),
        ]
    );
    assert_eq!(
        store
            .list_schedules(Some("ses_a"))
            .await
            .expect("list session")
            .len(),
        2
    );
    assert!(
        store
            .list_schedules(Some("ses_missing"))
            .await
            .expect("list missing")
            .is_empty()
    );

    assert_eq!(
        store.get_schedule(&id).await.expect("get schedule"),
        Some(created)
    );
    assert!(
        store
            .get_schedule("sch_missing")
            .await
            .expect("get missing")
            .is_none()
    );

    let patched = store
        .update_schedule(
            &id,
            SchedulePatch {
                name: Some("renamed".to_owned()),
                prompt: Some("again".to_owned()),
                ..SchedulePatch::default()
            },
        )
        .await
        .expect("patch name")
        .expect("existing");
    assert_eq!(patched["name"], "renamed");
    assert_eq!(patched["action"]["prompt"], "again");
    assert_eq!(patched["nextDueAt"], 1_300_000);
    assert_eq!(patched["time"]["created"], 1_000_000);

    let rescheduled = store
        .update_schedule(
            &id,
            SchedulePatch {
                when: Some(ScheduleWhen::In {
                    r#in: "10 minutes".to_owned(),
                    tz: None,
                }),
                ..SchedulePatch::default()
            },
        )
        .await
        .expect("patch when")
        .expect("existing");
    assert_eq!(rescheduled["when"]["in"], "10 minutes");
    assert_eq!(rescheduled["when"]["tz"], "UTC");
    assert!(rescheduled["nextDueAt"].as_i64().unwrap() > 1_300_000);

    let limited = store
        .update_schedule(
            &id,
            SchedulePatch {
                ends: Some(ScheduleEnds {
                    after: None,
                    at: None,
                    times: Some(2),
                }),
                ..SchedulePatch::default()
            },
        )
        .await
        .expect("patch ends")
        .expect("existing");
    assert_eq!(limited["remainingRuns"], 2);
    assert_eq!(limited["status"], "active");

    let paused = store
        .pause_schedule(&id)
        .await
        .expect("pause")
        .expect("existing");
    assert_eq!(paused["status"], "paused");
    assert_eq!(
        store
            .pause_schedule(&id)
            .await
            .expect("pause twice")
            .expect("existing")["status"],
        "paused"
    );
    assert_eq!(
        store
            .resume_schedule(&id)
            .await
            .expect("resume")
            .expect("existing")["status"],
        "active"
    );
    assert_eq!(
        store
            .resume_schedule(&id)
            .await
            .expect("resume twice")
            .expect("existing")["status"],
        "active"
    );
    assert!(
        store
            .pause_schedule("sch_missing")
            .await
            .expect("pause missing")
            .is_none()
    );
    assert!(
        store
            .resume_schedule("sch_missing")
            .await
            .expect("resume missing")
            .is_none()
    );
    assert!(
        store
            .update_schedule("sch_missing", SchedulePatch::default())
            .await
            .expect("patch missing")
            .is_none()
    );

    assert!(store.delete_schedule(&id).await.expect("delete"));
    assert!(!store.delete_schedule(&id).await.expect("delete twice"));
    assert!(
        store
            .get_schedule(&id)
            .await
            .expect("get deleted")
            .is_none()
    );
}

#[tokio::test]
async fn schedule_create_validation_and_limits() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;

    assert!(matches!(
        store
            .create_schedule(scheduled_in("ses_missing", None, "5 minutes", "hi", 0))
            .await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store
            .create_schedule(scheduled_in("ses_a", None, "soon", "hi", 0))
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_schedule(scheduled_in("ses_a", None, "5 minutes", "", 0))
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_schedule(NewSchedule {
                session_id: "ses_a".to_owned(),
                name: None,
                when: ScheduleWhen::In {
                    r#in: "5 minutes".to_owned(),
                    tz: Some("Asia/Tokyo".to_owned()),
                },
                ends: None,
                action: schedule_prompt("hi"),
                time_created: 0,
            })
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_schedule(NewSchedule {
                session_id: "ses_a".to_owned(),
                name: None,
                when: cron_when("nope", None),
                ends: None,
                action: schedule_prompt("hi"),
                time_created: 0,
            })
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_schedule(NewSchedule {
                session_id: "ses_a".to_owned(),
                name: None,
                when: ScheduleWhen::At {
                    at: "1970-01-01T00:00:00Z".to_owned(),
                    tz: None,
                },
                ends: None,
                action: schedule_prompt("hi"),
                time_created: 60_000,
            })
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_schedule(NewSchedule {
                session_id: "ses_a".to_owned(),
                name: None,
                when: ScheduleWhen::In {
                    r#in: "5 minutes".to_owned(),
                    tz: None,
                },
                ends: Some(ScheduleEnds {
                    after: None,
                    at: None,
                    times: Some(0),
                }),
                action: schedule_prompt("hi"),
                time_created: 0,
            })
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store
            .create_schedule(NewSchedule {
                session_id: "ses_a".to_owned(),
                name: None,
                when: ScheduleWhen::In {
                    r#in: "5 minutes".to_owned(),
                    tz: None,
                },
                ends: Some(ScheduleEnds {
                    after: Some("1 minute".to_owned()),
                    at: None,
                    times: None,
                }),
                action: schedule_prompt("hi"),
                time_created: 0,
            })
            .await,
        Err(StoreError::Invalid(_))
    ));

    let bounded = store
        .create_schedule(NewSchedule {
            session_id: "ses_a".to_owned(),
            name: None,
            when: ScheduleWhen::In {
                r#in: "5 minutes".to_owned(),
                tz: None,
            },
            ends: Some(ScheduleEnds {
                after: Some("1 day".to_owned()),
                at: None,
                times: None,
            }),
            action: schedule_prompt("hi"),
            time_created: 1_000_000,
        })
        .await
        .expect("end after");
    assert_eq!(bounded["endsAt"], 1_000_000 + 86_400_000);
    assert_eq!(bounded["nextDueAt"], 1_300_000);

    let exhausted = store
        .create_schedule(NewSchedule {
            session_id: "ses_a".to_owned(),
            name: None,
            when: ScheduleWhen::In {
                r#in: "1 hour".to_owned(),
                tz: None,
            },
            ends: None,
            action: schedule_prompt("hi"),
            time_created: 1_000_000,
        })
        .await
        .expect("active schedule");
    let done = store
        .update_schedule(
            exhausted["id"].as_str().unwrap(),
            SchedulePatch {
                ends: Some(ScheduleEnds {
                    after: None,
                    at: Some("2000000".to_owned()),
                    times: None,
                }),
                ..SchedulePatch::default()
            },
        )
        .await
        .expect("patch past end")
        .expect("existing");
    assert_eq!(done["status"], "done");
    assert!(done.get("nextDueAt").is_none());
    assert_eq!(done["endsAt"], 2_000_000);

    let offset = store
        .create_schedule(NewSchedule {
            session_id: "ses_a".to_owned(),
            name: None,
            when: ScheduleWhen::In {
                r#in: "5 minutes".to_owned(),
                tz: Some("+05:30".to_owned()),
            },
            ends: None,
            action: schedule_prompt("hi"),
            time_created: 0,
        })
        .await
        .expect("offset schedule");
    assert_eq!(offset["tz"], "+05:30");
    assert_eq!(offset["when"]["tz"], "+05:30");
    assert_eq!(offset["nextDueAt"], 300_000);

    let at = store
        .create_schedule(NewSchedule {
            session_id: "ses_a".to_owned(),
            name: Some("at".to_owned()),
            when: ScheduleWhen::At {
                at: "1970-01-01T00:10:00Z".to_owned(),
                tz: None,
            },
            ends: None,
            action: schedule_prompt("hi"),
            time_created: 0,
        })
        .await
        .expect("at schedule");
    assert_eq!(at["nextDueAt"], 600_000);
    assert_eq!(at["when"]["type"], "at");
}

#[tokio::test]
async fn schedule_due_query_mark_fired_and_resume() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;

    let slow = store
        .create_schedule(scheduled_in(
            "ses_a",
            Some("slow"),
            "5 minutes",
            "slow",
            1_000_000,
        ))
        .await
        .expect("slow schedule");
    let quick = store
        .create_schedule(scheduled_in(
            "ses_a",
            Some("quick"),
            "1 minute",
            "quick",
            1_000_000,
        ))
        .await
        .expect("quick schedule");
    let paused = store
        .create_schedule(scheduled_in(
            "ses_a",
            Some("paused"),
            "1 minute",
            "paused",
            1_000_000,
        ))
        .await
        .expect("paused schedule");
    store
        .pause_schedule(paused["id"].as_str().unwrap())
        .await
        .expect("pause")
        .expect("existing");
    let limited = store
        .create_schedule(NewSchedule {
            session_id: "ses_a".to_owned(),
            name: Some("twice".to_owned()),
            when: ScheduleWhen::Every {
                every: "1 hour".to_owned(),
                tz: None,
            },
            ends: Some(ScheduleEnds {
                after: None,
                at: None,
                times: Some(2),
            }),
            action: schedule_prompt("twice"),
            time_created: 1_000_000,
        })
        .await
        .expect("limited schedule");
    let ending = store
        .create_schedule(NewSchedule {
            session_id: "ses_a".to_owned(),
            name: None,
            when: ScheduleWhen::Every {
                every: "1 hour".to_owned(),
                tz: None,
            },
            ends: Some(ScheduleEnds {
                after: None,
                at: Some("8200000".to_owned()),
                times: None,
            }),
            action: schedule_prompt("ending"),
            time_created: 1_000_000,
        })
        .await
        .expect("ending schedule");
    assert_eq!(ending["endsAt"], 8_200_000);
    assert_eq!(ending["nextDueAt"], 4_600_000);

    let slow_id = slow["id"].as_str().unwrap().to_owned();
    let quick_id = quick["id"].as_str().unwrap().to_owned();
    let limited_id = limited["id"].as_str().unwrap().to_owned();
    let ending_id = ending["id"].as_str().unwrap().to_owned();

    assert!(
        store
            .due_schedules(1_000_000, 10)
            .await
            .expect("nothing due")
            .is_empty()
    );
    assert_eq!(
        ids(&store.due_schedules(1_060_000, 10).await.expect("quick due")),
        vec![quick_id.clone()]
    );
    assert_eq!(
        ids(&store.due_schedules(1_300_000, 10).await.expect("both due")),
        vec![quick_id.clone(), slow_id.clone()]
    );
    assert_eq!(
        ids(&store.due_schedules(1_300_000, 1).await.expect("limit one")),
        vec![quick_id.clone()]
    );

    let fired = store
        .mark_schedule_fired(&quick_id, ScheduleOutcome::Late, Some(1_360_000), 1)
        .await
        .expect("mark late")
        .expect("existing");
    assert_eq!(fired["lastOutcome"], "late");
    assert_eq!(fired["runCount"], 1);
    assert_eq!(fired["nextDueAt"], 1_360_000);
    assert_eq!(fired["status"], "active");
    assert!(fired["lastFiredAt"].as_i64().unwrap() > 0);
    assert!(fired.get("remainingRuns").is_none());
    assert_eq!(
        ids(&store.due_schedules(1_300_000, 10).await.expect("slow only")),
        vec![slow_id.clone()]
    );

    let spent = store
        .mark_schedule_fired(&slow_id, ScheduleOutcome::Fired, None, 1)
        .await
        .expect("mark done")
        .expect("existing");
    assert_eq!(spent["status"], "done");
    assert_eq!(spent["lastOutcome"], "fired");
    assert_eq!(spent["runCount"], 1);
    assert!(spent.get("nextDueAt").is_none());

    let first = store
        .mark_schedule_fired(&limited_id, ScheduleOutcome::Fired, Some(8_200_000), 1)
        .await
        .expect("mark limited")
        .expect("existing");
    assert_eq!(first["remainingRuns"], 1);
    assert_eq!(first["status"], "active");
    assert_eq!(first["nextDueAt"], 8_200_000);
    let second = store
        .mark_schedule_fired(&limited_id, ScheduleOutcome::Fired, Some(11_800_000), 2)
        .await
        .expect("mark exhausted")
        .expect("existing");
    assert_eq!(second["remainingRuns"], 0);
    assert_eq!(second["status"], "done");
    assert_eq!(second["runCount"], 2);
    assert!(second.get("nextDueAt").is_none());

    let past_limit = store
        .mark_schedule_fired(&ending_id, ScheduleOutcome::Fired, Some(9_000_000), 1)
        .await
        .expect("mark past end")
        .expect("existing");
    assert_eq!(past_limit["status"], "done");
    assert!(past_limit.get("nextDueAt").is_none());

    assert!(
        store
            .mark_schedule_fired("sch_missing", ScheduleOutcome::Fired, None, 1)
            .await
            .expect("mark missing")
            .is_none()
    );

    let paused_info = store
        .resume_schedule(paused["id"].as_str().unwrap())
        .await
        .expect("resume one-shot")
        .expect("existing");
    assert_eq!(paused_info["status"], "active");
    assert_eq!(paused_info["nextDueAt"], 1_060_000);

    let recurring = store
        .create_schedule(NewSchedule {
            session_id: "ses_a".to_owned(),
            name: None,
            when: ScheduleWhen::Every {
                every: "1 hour".to_owned(),
                tz: None,
            },
            ends: None,
            action: schedule_prompt("recurring"),
            time_created: 1_000_000,
        })
        .await
        .expect("recurring schedule");
    let recurring_id = recurring["id"].as_str().unwrap().to_owned();
    store
        .pause_schedule(&recurring_id)
        .await
        .expect("pause recurring")
        .expect("existing");
    let resumed = store
        .resume_schedule(&recurring_id)
        .await
        .expect("resume recurring")
        .expect("existing");
    assert_eq!(resumed["status"], "active");
    assert!(resumed["nextDueAt"].as_i64().unwrap() > 1_000_000_000_000);
}

// ---------------------------------------------------------------------------
// File snapshots
// ---------------------------------------------------------------------------

fn new_snapshot(id: &str, session: &str, message: &str, seq: i64, path: &str) -> NewSnapshot {
    NewSnapshot {
        id: format!("snp_{id}"),
        session_id: session.to_owned(),
        message_id: message.to_owned(),
        seq,
        tool_call_id: format!("call_{id}"),
        path: path.to_owned(),
        existed: true,
        skipped: false,
        content: None,
        time_created: 1_000 + seq,
    }
}

#[tokio::test]
async fn snapshots_insert_list_ordering_and_blob_roundtrip() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;
    seed_session(&store, "ses_b").await;

    store
        .insert_snapshot(NewSnapshot {
            content: Some(b"alpha".to_vec()),
            ..new_snapshot("1", "ses_a", "msg_1", 10, "/tmp/one/a.txt")
        })
        .await
        .expect("insert first");
    store
        .insert_snapshot(NewSnapshot {
            content: Some(b"beta".to_vec()),
            ..new_snapshot("2", "ses_a", "msg_1", 10, "/tmp/one/b.txt")
        })
        .await
        .expect("insert same-seq second");
    store
        .insert_snapshot(NewSnapshot {
            existed: false,
            ..new_snapshot("3", "ses_a", "msg_2", 12, "/tmp/one/new.txt")
        })
        .await
        .expect("insert new file");
    store
        .insert_snapshot(NewSnapshot {
            skipped: true,
            ..new_snapshot("4", "ses_a", "msg_2", 12, "/tmp/one/locked.bin")
        })
        .await
        .expect("insert skipped");
    store
        .insert_snapshot(NewSnapshot {
            content: Some(Vec::new()),
            existed: false,
            ..new_snapshot("6", "ses_a", "msg_2", 12, "/tmp/one/empty.txt")
        })
        .await
        .expect("insert empty blob");
    store
        .insert_snapshot(NewSnapshot {
            content: Some(b"other".to_vec()),
            ..new_snapshot("5", "ses_b", "msg_x", 11, "/tmp/two/a.txt")
        })
        .await
        .expect("insert other session");

    let rows = store.snapshots_after("ses_a", 0).await.expect("list");
    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        ids,
        ["snp_1", "snp_2", "snp_3", "snp_4", "snp_6"],
        "seq ascending, then id ascending"
    );

    assert_eq!(rows[0].session_id, "ses_a");
    assert_eq!(rows[0].message_id, "msg_1");
    assert_eq!(rows[0].seq, 10);
    assert_eq!(rows[0].tool_call_id, "call_1");
    assert_eq!(rows[0].path, "/tmp/one/a.txt");
    assert!(rows[0].existed);
    assert!(!rows[0].skipped);
    assert_eq!(rows[0].content.as_deref(), Some(b"alpha".as_slice()));
    assert_eq!(rows[0].time_created, 1_010);

    assert_eq!(
        rows[1].content.as_deref(),
        Some(b"beta".as_slice()),
        "second row at the same seq keeps its own bytes"
    );

    let new_file = &rows[2];
    assert!(!new_file.existed);
    assert!(new_file.content.is_none());

    let skipped = &rows[3];
    assert!(skipped.existed);
    assert!(skipped.skipped);
    assert!(skipped.content.is_none(), "skipped rows keep metadata only");
    assert_eq!(skipped.path, "/tmp/one/locked.bin");
    assert_eq!(skipped.tool_call_id, "call_4");

    let empty = &rows[4];
    assert!(!empty.existed);
    assert_eq!(
        empty.content.as_deref(),
        Some(b"".as_slice()),
        "an empty BLOB stays distinct from NULL"
    );

    let boundary = store
        .snapshots_after("ses_a", 12)
        .await
        .expect("inclusive boundary");
    let tail_ids: Vec<&str> = boundary.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        tail_ids,
        ["snp_3", "snp_4", "snp_6"],
        "the boundary seq itself is included"
    );
    assert!(
        store
            .snapshots_after("ses_a", 13)
            .await
            .expect("past the tail")
            .is_empty()
    );
    assert_eq!(
        store
            .snapshots_after("ses_a", 10)
            .await
            .expect("first boundary")
            .len(),
        5
    );

    let other = store
        .snapshots_after("ses_b", 0)
        .await
        .expect("other session");
    assert_eq!(ids_of_rows(&other), ["snp_5"]);
}

fn ids_of_rows(rows: &[oc_store::SnapshotRow]) -> Vec<&str> {
    rows.iter().map(|row| row.id.as_str()).collect()
}

#[tokio::test]
async fn earliest_snapshots_after_dedupes_by_path() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;

    // Two captures of `a` and `b`, two of `c` at the same seq for the id tie.
    for (id, message, seq, path) in [
        ("a1", "msg_1", 5, "/tmp/one/a.txt"),
        ("a2", "msg_2", 7, "/tmp/one/a.txt"),
        ("b1", "msg_1", 6, "/tmp/one/b.txt"),
        ("b2", "msg_2", 8, "/tmp/one/b.txt"),
        ("c2", "msg_2", 7, "/tmp/one/c.txt"),
        ("c1", "msg_2", 7, "/tmp/one/c.txt"),
    ] {
        store
            .insert_snapshot(new_snapshot(id, "ses_a", message, seq, path))
            .await
            .expect("insert snapshot");
    }

    let earliest = store
        .earliest_snapshots_after("ses_a", 0)
        .await
        .expect("earliest");
    assert_eq!(
        ids_of_rows(&earliest),
        ["snp_a1", "snp_b1", "snp_c1"],
        "one row per path: min seq, then min id"
    );
    assert_eq!(earliest[0].path, "/tmp/one/a.txt");
    assert_eq!(earliest[0].seq, 5);
    assert_eq!(earliest[2].path, "/tmp/one/c.txt");
    assert_eq!(
        earliest[2].seq, 7,
        "same-seq captures collapse to the smallest id"
    );

    let after_six = store
        .earliest_snapshots_after("ses_a", 6)
        .await
        .expect("earliest from six");
    assert_eq!(
        ids_of_rows(&after_six),
        ["snp_b1", "snp_a2", "snp_c1"],
        "boundary is inclusive and earlier per-path rows are excluded"
    );

    let after_eight = store
        .earliest_snapshots_after("ses_a", 8)
        .await
        .expect("earliest from eight");
    assert_eq!(
        ids_of_rows(&after_eight),
        ["snp_b2"],
        "paths with no capture at or after the boundary disappear"
    );

    assert_eq!(
        store
            .earliest_snapshots_after("ses_a", 0)
            .await
            .expect("unchanged")
            .len(),
        3,
        "listing is a read and never consumes rows"
    );
}

#[tokio::test]
async fn delete_snapshots_after_range() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;
    seed_session(&store, "ses_b").await;

    for (id, seq) in [("s3", 3), ("s5", 5), ("s7", 7), ("s9", 9)] {
        store
            .insert_snapshot(new_snapshot(id, "ses_a", "msg_1", seq, "/tmp/one/a.txt"))
            .await
            .expect("insert session row");
    }
    store
        .insert_snapshot(new_snapshot("other", "ses_b", "msg_1", 5, "/tmp/two/a.txt"))
        .await
        .expect("insert other row");

    assert_eq!(
        store
            .delete_snapshots_after("ses_a", 5)
            .await
            .expect("delete from five"),
        3,
        "seq >= boundary is removed"
    );
    assert_eq!(
        ids_of_rows(&store.snapshots_after("ses_a", 0).await.expect("remaining")),
        ["snp_s3"]
    );
    assert_eq!(
        store
            .delete_snapshots_after("ses_a", 100)
            .await
            .expect("delete past the tail"),
        0
    );
    assert_eq!(
        store
            .delete_snapshots_after("ses_a", 0)
            .await
            .expect("delete everything"),
        1
    );
    assert!(
        store
            .snapshots_after("ses_a", 0)
            .await
            .expect("empty")
            .is_empty()
    );
    assert_eq!(
        ids_of_rows(&store.snapshots_after("ses_b", 0).await.expect("other")),
        ["snp_other"],
        "another session's rows are untouched"
    );
}

#[tokio::test]
async fn snapshots_cascade_when_the_session_is_deleted() {
    let store = Store::open_memory().await.expect("memory store");
    seed_session(&store, "ses_a").await;
    seed_session(&store, "ses_b").await;

    store
        .insert_snapshot(new_snapshot("a1", "ses_a", "msg_1", 1, "/tmp/one/a.txt"))
        .await
        .expect("insert a1");
    store
        .insert_snapshot(new_snapshot("a2", "ses_a", "msg_2", 2, "/tmp/one/b.txt"))
        .await
        .expect("insert a2");
    store
        .insert_snapshot(new_snapshot("b1", "ses_b", "msg_1", 1, "/tmp/two/a.txt"))
        .await
        .expect("insert b1");

    store.delete_session("ses_a").await.expect("delete session");

    assert!(
        store
            .snapshots_after("ses_a", 0)
            .await
            .expect("cascaded list")
            .is_empty()
    );
    assert!(
        store
            .earliest_snapshots_after("ses_a", 0)
            .await
            .expect("cascaded earliest")
            .is_empty()
    );
    assert_eq!(
        ids_of_rows(&store.snapshots_after("ses_b", 0).await.expect("survivor")),
        ["snp_b1"]
    );

    let missing = store
        .insert_snapshot(new_snapshot("x", "ses_missing", "msg_1", 1, "/tmp/x"))
        .await;
    assert!(
        matches!(missing, Err(StoreError::NotFound(_))),
        "an unknown session is reported, not silently orphaned: {missing:?}"
    );
}
