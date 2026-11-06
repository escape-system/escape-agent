//! Integration tests for the permanent-memory surface.

use oc_store::{MemoryQuery, NewMemory, Store, StoreError};
use serde_json::json;
use tempfile::TempDir;

fn new_memory(
    id: &str,
    scope: &str,
    scope_id: Option<&str>,
    kind: &str,
    text: &str,
    importance: u8,
) -> NewMemory {
    NewMemory {
        id: id.to_owned(),
        scope: scope.to_owned(),
        scope_id: scope_id.map(str::to_owned),
        kind: kind.to_owned(),
        text: text.to_owned(),
        tags: Vec::new(),
        importance,
        confidence: 0.7,
        source: "user".to_owned(),
        source_session: None,
        source_message: None,
    }
}

fn scoped(scopes: Vec<(&str, Option<&str>)>) -> Vec<(String, Option<String>)> {
    scopes
        .into_iter()
        .map(|(scope, id)| (scope.to_owned(), id.map(str::to_owned)))
        .collect()
}

fn ids_of(hits: &[oc_store::MemoryHit]) -> Vec<String> {
    hits.iter().map(|hit| hit.id.clone()).collect()
}

#[tokio::test]
async fn write_dedupes_and_upserts() {
    let store = Store::open_memory().await.expect("store");
    let first = store
        .write_memory(NewMemory {
            tags: vec!["rust".to_owned(), " rust ".to_owned(), String::new()],
            importance: 3,
            confidence: 0.5,
            ..new_memory(
                "mem_a",
                "global",
                None,
                "fact",
                "  Use SQLite\n for  memory ",
                3,
            )
        })
        .await
        .expect("first write");
    assert_eq!(first["id"], "mem_a");
    assert_eq!(first["tags"], json!(["rust"]));
    assert_eq!(first["importance"], 3);
    assert_eq!(first["text"], "Use SQLite\n for  memory");

    let second = store
        .write_memory(NewMemory {
            id: "mem_b".to_owned(),
            importance: 5,
            confidence: 0.9,
            ..new_memory("mem_b", "global", None, "fact", "use sqlite for memory", 5)
        })
        .await
        .expect("dedupe");
    assert_eq!(
        second["id"], "mem_a",
        "normalized duplicate folds into first"
    );
    assert_eq!(second["importance"], 5);
    let confidence = second["confidence"].as_f64().expect("confidence");
    assert!((confidence - 0.9).abs() < 1e-6);

    let all = store
        .list_memories(MemoryQuery {
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("list");
    assert_eq!(all.len(), 1);

    let conflict = store
        .write_memory(NewMemory {
            text: "a different fact".to_owned(),
            ..new_memory("mem_a", "global", None, "fact", "a different fact", 2)
        })
        .await;
    assert!(matches!(conflict, Err(StoreError::Conflict(_))));

    let invalid_scope = store
        .write_memory(NewMemory {
            scope: "galaxy".to_owned(),
            ..new_memory("mem_x", "global", None, "fact", "text", 2)
        })
        .await;
    assert!(matches!(invalid_scope, Err(StoreError::Invalid(_))));

    let empty = store
        .write_memory(new_memory("mem_y", "global", None, "fact", "   ", 2))
        .await;
    assert!(matches!(empty, Err(StoreError::Invalid(_))));
}

async fn seed_scoped(store: &Store) {
    store
        .write_memory(NewMemory {
            tags: vec!["cargo".to_owned(), "ci".to_owned()],
            ..new_memory(
                "mem_gotcha",
                "global",
                None,
                "gotcha",
                "cargo test locks the target directory",
                3,
            )
        })
        .await
        .expect("global");
    store
        .write_memory(NewMemory {
            tags: vec!["sqlite".to_owned()],
            ..new_memory(
                "mem_decision",
                "project",
                Some("prj_1"),
                "decision",
                "use a single writer for sqlite",
                4,
            )
        })
        .await
        .expect("project one");
    store
        .write_memory(NewMemory {
            tags: vec!["sqlite".to_owned()],
            ..new_memory(
                "mem_other",
                "project",
                Some("prj_2"),
                "fact",
                "sqlite lives in another project",
                2,
            )
        })
        .await
        .expect("project two");
}

#[tokio::test]
async fn search_respects_scopes_tags_and_kinds() {
    let store = Store::open_memory().await.expect("store");
    seed_scoped(&store).await;

    let visible = scoped(vec![("global", None), ("project", Some("prj_1"))]);
    let hits = store
        .search_memories(MemoryQuery {
            scopes: visible.clone(),
            query: Some("sqlite".to_owned()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("scoped search");
    assert_eq!(ids_of(&hits), vec!["mem_decision"]);

    let everywhere = store
        .search_memories(MemoryQuery {
            query: Some("sqlite".to_owned()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("unscoped search");
    assert_eq!(everywhere.len(), 2);

    let by_tag = store
        .search_memories(MemoryQuery {
            scopes: visible,
            tags: vec!["ci".to_owned()],
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("tag search");
    assert_eq!(ids_of(&by_tag), vec!["mem_gotcha"]);

    let by_kind = store
        .search_memories(MemoryQuery {
            kinds: vec!["decision".to_owned()],
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("kind search");
    assert_eq!(ids_of(&by_kind), vec!["mem_decision"]);
}

#[tokio::test]
async fn search_sanitizes_free_text_and_blank_queries() {
    let store = Store::open_memory().await.expect("store");
    seed_scoped(&store).await;

    let visible = scoped(vec![("global", None), ("project", Some("prj_1"))]);
    let punctuation = store
        .search_memories(MemoryQuery {
            scopes: visible.clone(),
            query: Some("cargo test!! (locks)".to_owned()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("fts-safe query");
    assert_eq!(ids_of(&punctuation), vec!["mem_gotcha"]);

    let blank = store
        .search_memories(MemoryQuery {
            scopes: visible,
            query: Some("   ".to_owned()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("blank query");
    assert_eq!(blank.len(), 2);
}

#[tokio::test]
async fn soft_delete_hides_memories_until_rewritten() {
    let store = Store::open_memory().await.expect("store");
    store
        .write_memory(new_memory(
            "mem_forget",
            "global",
            None,
            "fact",
            "temporary fact",
            3,
        ))
        .await
        .expect("write");

    assert!(store.forget_memory("mem_forget").await.expect("forget"));
    assert!(!store.forget_memory("mem_forget").await.expect("repeat"));
    assert!(!store.forget_memory("mem_missing").await.expect("missing"));

    let hits = store
        .search_memories(MemoryQuery {
            query: Some("temporary".to_owned()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("search");
    assert!(hits.is_empty());
    let listed = store
        .list_memories(MemoryQuery::default())
        .await
        .expect("list");
    assert!(listed.is_empty());

    let rewritten = store
        .write_memory(new_memory(
            "mem_new_id",
            "global",
            None,
            "fact",
            "temporary fact",
            4,
        ))
        .await
        .expect("rewrite");
    assert_eq!(rewritten["id"], "mem_forget");
    let listed = store
        .list_memories(MemoryQuery::default())
        .await
        .expect("list after rewrite");
    assert_eq!(listed.len(), 1);
}

#[tokio::test]
async fn scribe_write_cannot_resurrect_a_forgotten_memory() {
    let store = Store::open_memory().await.expect("store");
    store
        .write_memory(new_memory(
            "mem_forgotten",
            "global",
            None,
            "gotcha",
            "cargo test locks the target directory",
            3,
        ))
        .await
        .expect("seed");
    assert!(store.forget_memory("mem_forgotten").await.expect("forget"));

    let scribe = store
        .write_memory(NewMemory {
            id: "mem_scribe_retry".to_owned(),
            importance: 5,
            source: "scribe".to_owned(),
            ..new_memory(
                "mem_scribe_retry",
                "global",
                None,
                "gotcha",
                "cargo test locks the target directory",
                5,
            )
        })
        .await;
    assert!(
        matches!(scribe, Err(StoreError::Conflict(_))),
        "a scribe rewrite of a forgotten memory must not report success: {scribe:?}"
    );
    let listed = store
        .list_memories(MemoryQuery::default())
        .await
        .expect("list");
    assert!(
        listed.is_empty(),
        "the forgotten memory must stay hidden: {listed:?}"
    );
    let search = store
        .search_memories(MemoryQuery {
            query: Some("cargo".to_owned()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("search");
    assert!(
        search.is_empty(),
        "the forgotten memory must stay unsearchable: {search:?}"
    );

    let rewritten = store
        .write_memory(new_memory(
            "mem_user_rewrite",
            "global",
            None,
            "gotcha",
            "cargo test locks the target directory",
            4,
        ))
        .await
        .expect("user rewrite");
    assert_eq!(rewritten["id"], "mem_forgotten");
    assert_eq!(rewritten["importance"], 4);
    let listed = store
        .list_memories(MemoryQuery::default())
        .await
        .expect("list after rewrite");
    assert_eq!(
        listed.len(),
        1,
        "an explicit rewrite restores the row: {listed:?}"
    );
    let search = store
        .search_memories(MemoryQuery {
            query: Some("cargo".to_owned()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("search after rewrite");
    assert_eq!(ids_of(&search), vec!["mem_forgotten"]);
}

#[tokio::test]
async fn search_limit_and_pinned_rows() {
    let store = Store::open_memory().await.expect("store");
    for index in 0..5 {
        store
            .write_memory(new_memory(
                &format!("mem_match_{index}"),
                "global",
                None,
                "fact",
                &format!("alpha note number {index}"),
                2,
            ))
            .await
            .expect("match");
    }
    store
        .write_memory(new_memory(
            "mem_pinned",
            "global",
            None,
            "preference",
            "pinned unrelated note",
            5,
        ))
        .await
        .expect("pinned");

    let pinned = store
        .search_memories(MemoryQuery {
            query: Some("alpha".to_owned()),
            limit: 2,
            pinned_min_importance: Some(5),
            ..Default::default()
        })
        .await
        .expect("pinned search");
    assert_eq!(pinned.len(), 2);
    assert_eq!(pinned[0].id, "mem_pinned", "pinned rows are ordered first");

    let all = store
        .search_memories(MemoryQuery {
            query: Some("alpha".to_owned()),
            limit: 20,
            pinned_min_importance: Some(5),
            ..Default::default()
        })
        .await
        .expect("wide search");
    assert_eq!(all.len(), 6);

    for index in 2..5 {
        store
            .write_memory(new_memory(
                &format!("mem_pinned_{index}"),
                "global",
                None,
                "preference",
                &format!("pinned extra note {index}"),
                5,
            ))
            .await
            .expect("pinned extra");
    }
    let pinned_cap = store
        .search_memories(MemoryQuery {
            query: Some("alpha".to_owned()),
            limit: 20,
            pinned_min_importance: Some(5),
            ..Default::default()
        })
        .await
        .expect("pinned cap search");
    assert_eq!(
        pinned_cap.len(),
        8,
        "five matches plus at most three pinned rows"
    );

    for index in 0..60 {
        store
            .write_memory(new_memory(
                &format!("mem_bulk_{index}"),
                "global",
                None,
                "fact",
                &format!("bulk note {index}"),
                1,
            ))
            .await
            .expect("bulk");
    }
    let capped = store
        .search_memories(MemoryQuery {
            limit: 1_000,
            ..Default::default()
        })
        .await
        .expect("capped");
    assert_eq!(capped.len(), 50, "search limit is capped at 50");
}

#[tokio::test]
async fn touch_bumps_uses_and_last_used() {
    let store = Store::open_memory().await.expect("store");
    for index in 0..2 {
        store
            .write_memory(new_memory(
                &format!("mem_touch_{index}"),
                "global",
                None,
                "fact",
                &format!("touchable fact {index}"),
                3,
            ))
            .await
            .expect("write");
    }
    let ids = vec!["mem_touch_0".to_owned(), "mem_touch_1".to_owned()];
    store.touch_memories(&ids, 50_000).await.expect("touch");
    let listed = store
        .list_memories(MemoryQuery {
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("list");
    for memory in &listed {
        assert_eq!(memory["uses"], 1);
        assert_eq!(memory["lastUsed"], 50_000);
    }

    store
        .touch_memories(&ids, 60_000)
        .await
        .expect("second touch");
    let listed = store
        .list_memories(MemoryQuery {
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("list again");
    for memory in &listed {
        assert_eq!(memory["uses"], 2);
        assert_eq!(memory["lastUsed"], 60_000);
    }
    store
        .touch_memories(&[], 70_000)
        .await
        .expect("empty touch");
}

#[tokio::test]
async fn reopen_keeps_memories_and_index() {
    let directory = TempDir::new().expect("temp dir");
    let path = directory.path().join("memory.db");
    let store = Store::open(&path).await.expect("open");
    let written = store
        .write_memory(NewMemory {
            tags: vec!["persistent".to_owned()],
            ..new_memory(
                "mem_persist",
                "project",
                Some("prj_1"),
                "decision",
                "persistent memory value",
                4,
            )
        })
        .await
        .expect("write");
    assert_eq!(written["id"], "mem_persist");
    drop(store);

    let reopened = Store::open(&path).await.expect("reopen");
    let hits = reopened
        .search_memories(MemoryQuery {
            scopes: scoped(vec![("project", Some("prj_1"))]),
            query: Some("persistent".to_owned()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("search after reopen");
    assert_eq!(ids_of(&hits), vec!["mem_persist"]);
    assert_eq!(hits[0].tags, vec!["persistent".to_owned()]);

    let rewritten = reopened
        .write_memory(new_memory(
            "mem_other_id",
            "project",
            Some("prj_1"),
            "decision",
            "Persistent   memory value",
            5,
        ))
        .await
        .expect("dedupe after reopen");
    assert_eq!(rewritten["id"], "mem_persist");
    let listed = reopened
        .list_memories(MemoryQuery {
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("list");
    assert_eq!(listed.len(), 1);
}

#[tokio::test]
async fn deleting_a_session_removes_session_scoped_memories_and_preserves_project_memories() {
    let store = Store::open_memory().await.expect("store");
    store
        .create_session(oc_store::NewSession {
            id: "ses_clean".to_owned(),
            parent_id: None,
            project_id: "prj_clean".to_owned(),
            directory: "/tmp/clean".to_owned(),
            title: Some("clean".to_owned()),
            agent: None,
            model: None,
            metadata: None,
            permissions: None,
            time_created: 1,
        })
        .await
        .expect("session");

    store
        .write_memory(new_memory(
            "mem_ses_clean",
            "session",
            Some("ses_clean"),
            "fact",
            "ephemeral session memory removed on delete",
            3,
        ))
        .await
        .expect("session memory");
    store
        .write_memory(new_memory(
            "mem_prj_keep",
            "project",
            Some("prj_clean"),
            "fact",
            "durable project memory kept across session delete",
            4,
        ))
        .await
        .expect("project memory");

    store.delete_session("ses_clean").await.expect("delete");

    let listed = store
        .list_memories(MemoryQuery {
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], "mem_prj_keep");
}
