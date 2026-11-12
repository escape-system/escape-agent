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

/// Epoch milliseconds, matching `last_used` units so tests can age rows without
/// reaching into the store.
fn wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_millis()
        .try_into()
        .expect("epoch millis fit in i64")
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

/// `T3`: the recency term must follow the documented `1 / (1 + age_days / 30)`
/// scale, not the old `1 / (1 + age_days)`.
///
/// Ages are established through `last_used` (a completed `touch_memories`), so
/// the only unknown is the millisecond drift between the touch and the search.
/// That drift is negligible against a 30-day scale, so the 5e-5 tolerance is
/// generous.
#[tokio::test]
async fn recency_decays_on_the_documented_thirty_day_scale() {
    const MS_PER_DAY: i64 = 86_400_000;
    const W_RECENCY: f64 = 0.25;

    let store = Store::open_memory().await.expect("store");
    // Three rows with identical importance and no tags, so the score differs
    // only by the recency term.
    for id in ["mem_fresh", "mem_week", "mem_month"] {
        store
            .write_memory(new_memory(
                id,
                "global",
                None,
                "fact",
                &format!("decay probe {id}"),
                3,
            ))
            .await
            .expect("write");
    }
    // One minute of slack keeps `fresh` from sitting exactly at the search's
    // `now_ms()`; either way the age is ~0 against a 30-day scale.
    let now = wall_clock_ms() - 60_000;
    store
        .touch_memories(&["mem_fresh".to_owned()], now)
        .await
        .expect("touch fresh");
    store
        .touch_memories(&["mem_week".to_owned()], now - 7 * MS_PER_DAY)
        .await
        .expect("touch week");
    store
        .touch_memories(&["mem_month".to_owned()], now - 30 * MS_PER_DAY)
        .await
        .expect("touch month");

    let hits = store
        .search_memories(MemoryQuery {
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("search");
    let recency_of = |id: &str| {
        hits.iter()
            .find(|hit| hit.id == id)
            .unwrap_or_else(|| panic!("missing {id} in {hits:?}"))
            .score
    };
    // Equal importance of 3 contributes 3 * 0.35 in every row.
    let base = 3.0 * 0.35;
    let fresh = recency_of("mem_fresh") - base;
    let week = recency_of("mem_week") - base;
    let month = recency_of("mem_month") - base;

    // A 30-day-old memory keeps half of the recency weight; the old formula
    // would have given ~1/31, thirty times less.
    let expected_month = W_RECENCY * 0.5;
    assert!(
        (month - expected_month).abs() < 5e-5,
        "30 days must halve the recency term: expected {expected_month}, got {month}"
    );
    let expected_week = W_RECENCY / (1.0 + 7.0 / 30.0);
    assert!(
        (week - expected_week).abs() < 5e-5,
        "7 days must follow 1/(1+7/30): expected {expected_week}, got {week}"
    );
    // The old `1/(1+age_days)` formula scored a week-old memory at 0.125 of the
    // recency weight (0.03125); the documented scale keeps it near 0.2.
    assert!(
        week > W_RECENCY / 4.0,
        "a 7-day-old memory must score above the old 0.125 factor: got {week}"
    );
    assert!(
        fresh <= W_RECENCY + 1e-9 && fresh > W_RECENCY * 0.999,
        "a just-touched memory scores the full recency weight: got {fresh}"
    );
    assert!(
        fresh > week && week > month,
        "recency must decrease with age: fresh={fresh} week={week} month={month}"
    );
}

/// Anti-drift guard for `T3`: the FTS branch and the plain-ranked branch must
/// compute the same recency for the same row.
///
/// The two probe rows have the same token count and the same term frequency for
/// the query token, so FTS5 assigns them the same bm25. Therefore
/// `fts_score - plain_score` is a row-independent constant (the bm25 term), and
/// the recency *difference* between the rows can be compared across branches
/// without knowing bm25.
#[tokio::test]
async fn both_search_branches_agree_on_the_recency_term() {
    const MS_PER_DAY: i64 = 86_400_000;
    const W_RECENCY: f64 = 0.25;

    let store = Store::open_memory().await.expect("store");
    store
        .write_memory(new_memory(
            "mem_drift_fresh",
            "global",
            None,
            "fact",
            "drift probe alpha",
            2,
        ))
        .await
        .expect("fresh write");
    store
        .write_memory(new_memory(
            "mem_drift_old",
            "global",
            None,
            "fact",
            "drift probe beta",
            2,
        ))
        .await
        .expect("old write");
    let now = wall_clock_ms() - 60_000;
    store
        .touch_memories(&["mem_drift_fresh".to_owned()], now)
        .await
        .expect("touch fresh");
    store
        .touch_memories(&["mem_drift_old".to_owned()], now - 30 * MS_PER_DAY)
        .await
        .expect("touch old");

    let ranked = store
        .search_memories(MemoryQuery {
            query: Some("drift".to_owned()),
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("fts branch");
    let plain = store
        .search_memories(MemoryQuery {
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("plain branch");
    assert_eq!(ranked.len(), 2, "{ranked:?}");
    assert_eq!(plain.len(), 2, "{plain:?}");
    let find = |hits: &[oc_store::MemoryHit], id: &str| {
        hits.iter()
            .find(|hit| hit.id == id)
            .unwrap_or_else(|| panic!("missing {id} in {hits:?}"))
            .score
    };

    let fts_fresh = find(&ranked, "mem_drift_fresh");
    let fts_old = find(&ranked, "mem_drift_old");
    let plain_fresh = find(&plain, "mem_drift_fresh");
    let plain_old = find(&plain, "mem_drift_old");

    // bm25 is identical for both rows up to FTS5's floating-point noise, so the
    // branch offset must be too. The tolerance is loose because the term itself
    // is ~1e-6 here and depends on the corpus statistics, not on recency.
    let offset_fresh = fts_fresh - plain_fresh;
    let offset_old = fts_old - plain_old;
    assert!(
        (offset_fresh - offset_old).abs() < 1e-5,
        "branches must agree per row: offsets {offset_fresh} vs {offset_old}"
    );

    // Both branches must show the 30-day recency drop: half of W_RECENCY.
    let expected_drop = W_RECENCY * 0.5;
    for (label, drop) in [
        ("fts", fts_fresh - fts_old),
        ("plain", plain_fresh - plain_old),
    ] {
        assert!(
            (drop - expected_drop).abs() < 5e-5,
            "{label} branch recency drop over 30 days: expected {expected_drop}, got {drop}"
        );
    }

    // And the old row's absolute recency must match the documented formula in
    // the plain branch (the FTS branch adds the unknown bm25 on top).
    let expected_old = 2.0 * 0.35 + W_RECENCY * 0.5;
    assert!(
        (plain_old - expected_old).abs() < 5e-5,
        "plain branch old-row score: expected {expected_old}, got {plain_old}"
    );
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

/// `T9`: a forgotten memory must not keep accruing `uses` / `last_used`.
#[tokio::test]
async fn touch_skips_forgotten_memories() {
    let store = Store::open_memory().await.expect("store");
    store
        .write_memory(new_memory(
            "mem_live",
            "global",
            None,
            "fact",
            "live fact keeps usage",
            3,
        ))
        .await
        .expect("live");
    store
        .write_memory(new_memory(
            "mem_gone",
            "global",
            None,
            "fact",
            "forgotten fact must not accrue usage",
            3,
        ))
        .await
        .expect("forgotten");
    assert!(store.forget_memory("mem_gone").await.expect("forget"));

    let ids = vec!["mem_live".to_owned(), "mem_gone".to_owned()];
    store.touch_memories(&ids, 50_000).await.expect("touch");
    store
        .touch_memories(&ids, 60_000)
        .await
        .expect("touch again");

    // The forgotten row is invisible to `list_memories`, so read it straight
    // from the store's read path via the raw row helper's public surface: the
    // rewrite path returns the row regardless of `deleted`.
    let listed = store
        .list_memories(MemoryQuery {
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("list");
    assert_eq!(listed.len(), 1, "only the live memory is listed");
    assert_eq!(listed[0]["id"], "mem_live");
    assert_eq!(listed[0]["uses"], 2);
    assert_eq!(listed[0]["lastUsed"], 60_000);

    // Re-writing the forgotten row (a user rewrite, not a scribe retry)
    // restores it, which is how the test observes its counters afterwards.
    let restored = store
        .write_memory(new_memory(
            "mem_resurrect",
            "global",
            None,
            "fact",
            "forgotten fact must not accrue usage",
            3,
        ))
        .await
        .expect("user rewrite restores the row");
    assert_eq!(restored["id"], "mem_gone");
    assert_eq!(
        restored["uses"], 0,
        "the forgotten row must never have been touched: {restored:?}"
    );
    assert!(
        restored.get("lastUsed").is_none(),
        "a forgotten row must not gain last_used: {restored:?}"
    );
}

/// `T9`: unknown ids stay a silent skip (a session-scoped memory can vanish
/// between retrieval and turn end) and an empty slice is a no-op.
#[tokio::test]
async fn touch_ignores_unknown_ids_and_empty_slices() {
    let store = Store::open_memory().await.expect("store");
    store
        .write_memory(new_memory(
            "mem_known",
            "global",
            None,
            "fact",
            "known fact",
            3,
        ))
        .await
        .expect("known");

    store
        .touch_memories(&[], 10_000)
        .await
        .expect("empty slice is a no-op");
    store
        .touch_memories(&["mem_absent".to_owned()], 20_000)
        .await
        .expect("unknown ids are skipped, not an error");
    store
        .touch_memories(&["mem_known".to_owned(), "mem_absent".to_owned()], 30_000)
        .await
        .expect("mixed ids");

    let listed = store
        .list_memories(MemoryQuery {
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["uses"], 1, "only the known row was bumped");
    assert_eq!(listed[0]["lastUsed"], 30_000);
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
