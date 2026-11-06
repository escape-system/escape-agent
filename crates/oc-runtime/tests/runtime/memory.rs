//! Memory, skills, instructions deltas and the scribe.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_memory::{MemoryService, NewMemoryInput};
use oc_store::{MemoryQuery, Store};
use serde_json::{Value, json};

use crate::support::*;

async fn seed_memory(
    memory: &MemoryService,
    text: &str,
    scope: &str,
    scope_id: Option<&str>,
    importance: u8,
) {
    memory
        .write(NewMemoryInput {
            text: text.to_owned(),
            scope: Some(scope.to_owned()),
            scope_id: scope_id.map(str::to_owned),
            importance: Some(importance),
            ..NewMemoryInput::default()
        })
        .await
        .expect("memory seeded");
}

async fn all_memories(store: &Store) -> Vec<Value> {
    store
        .list_memories(MemoryQuery {
            scopes: Vec::new(),
            query: None,
            kinds: Vec::new(),
            tags: Vec::new(),
            limit: 50,
            pinned_min_importance: None,
        })
        .await
        .expect("memories")
}

fn write_skill(
    work: &Path,
    id: &str,
    name: &str,
    description: Option<&str>,
    autoinvoke: Option<bool>,
) {
    let directory = work.join(".opencode").join("skill").join(id);
    std::fs::create_dir_all(&directory).expect("skill dir");
    let mut content = format!("---\nname: {name}\n");
    if let Some(description) = description {
        let _ = writeln!(content, "description: {description}");
    }
    if let Some(autoinvoke) = autoinvoke {
        let _ = write!(content, "metadata:\n  opencode/autoinvoke: {autoinvoke}\n");
    }
    content.push_str("---\nSkill body.\n");
    std::fs::write(directory.join("SKILL.md"), content).expect("skill file");
}

async fn wait_for_memories(store: &Store, source: &str) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let rows: Vec<Value> = all_memories(store)
            .await
            .into_iter()
            .filter(|row| row["source"].as_str() == Some(source))
            .collect();
        if !rows.is_empty() || Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn memory_block_appears_only_when_it_has_content() {
    let harness = Harness::new(
        vec![text_script("first"), text_script("second")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    let harness = harness.attach_memory(Arc::clone(&memory));

    harness.prompt("hi").await;
    let first = harness.model.requests()[0].system[1].clone();
    assert!(!first.contains("<memory"), "got {first:?}");
    assert!(!harness.model.requests()[0].system[0].contains("`<memory>`"));

    seed_memory(
        &memory,
        "The quiet-river lane runs the workspace build before tests.",
        "project",
        Some("prj_runtime_test"),
        4,
    )
    .await;
    harness
        .prompt("hello memory: which lane runs the quiet-river build?")
        .await;

    let requests = harness.model.requests();
    let second = &requests[1];
    assert!(second.system[1].contains("<memory scope=\"project\">"));
    assert!(second.system[1].contains("quiet-river"));
    assert!(second.system[0].contains("`<memory>`"));
}

#[tokio::test]
async fn memory_from_another_project_is_not_injected() {
    let harness = Harness::new(vec![text_script("ok")], Arc::new(AllowAll), json!({}), None).await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    seed_memory(
        &memory,
        "Cross-project secret token that must stay isolated.",
        "project",
        Some("prj_other"),
        5,
    )
    .await;
    let harness = harness.attach_memory(memory);

    harness.prompt("cross-project secret token").await;

    let system = &harness.model.requests()[0].system[1];
    assert!(!system.contains("<memory"), "got {system:?}");
    assert!(!system.contains("Cross-project secret"));
}

#[tokio::test]
async fn forgetting_a_pinned_memory_drops_it_from_the_epoch_prefix() {
    let harness = Harness::new(
        vec![text_script("one"), text_script("two"), text_script("three")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    // Importance 5 pins both memories, so retrieval includes them regardless of
    // the query and the test never depends on FTS ranking.
    seed_memory(
        &memory,
        "Pinned lemon memory that stays live.",
        "project",
        Some("prj_runtime_test"),
        5,
    )
    .await;
    seed_memory(
        &memory,
        "Forgotten apricot memory that must vanish.",
        "project",
        Some("prj_runtime_test"),
        5,
    )
    .await;
    let harness = harness.attach_memory(Arc::clone(&memory));

    harness.prompt("first turn").await;
    let first = &harness.model.requests()[0].system[1];
    assert!(first.contains("Pinned lemon"), "got {first:?}");
    assert!(first.contains("Forgotten apricot"), "got {first:?}");

    let apricot = all_memories(&harness.store)
        .await
        .into_iter()
        .find(|row| {
            row["text"]
                .as_str()
                .is_some_and(|text| text.contains("apricot"))
        })
        .and_then(|row| row["id"].as_str().map(str::to_owned))
        .expect("seeded apricot memory");
    assert!(memory.forget(&apricot).await.expect("forget"));

    harness.prompt("second turn").await;
    let second = &harness.model.requests()[1];
    assert!(
        !second.system[1].contains("Forgotten apricot"),
        "the epoch pin re-sent a forgotten memory: {:?}",
        second.system[1]
    );
    assert!(
        second.system[1].contains("Pinned lemon"),
        "the live memory must survive the re-pin: {:?}",
        second.system[1]
    );
    assert!(
        !request_contains(second, "Forgotten apricot"),
        "a forgotten memory must not ride along in the tail supplement"
    );
    assert_eq!(harness.sink.count("session.instructions.updated"), 1);

    harness.prompt("third turn").await;
    let third = &harness.model.requests()[2];
    assert_eq!(
        third.system[1], second.system[1],
        "a stable memory set must not rebuild the pinned prefix"
    );
    assert_eq!(
        harness.sink.count("session.instructions.updated"),
        1,
        "only the membership change republishes the prefix"
    );
}

#[tokio::test]
async fn skill_index_filters_and_escapes() {
    let harness = Harness::new_with_setup(
        vec![text_script("ok")],
        Arc::new(AllowAll),
        json!({"permission": {"skill": {"gamma": "deny"}}}),
        None,
        |work| {
            write_skill(
                work,
                "alpha",
                "Rust & <Build>",
                Some("Use & verify <carefully>"),
                None,
            );
            write_skill(
                work,
                "beta",
                "Beta Skill",
                Some("Manually invoked only"),
                Some(false),
            );
            write_skill(work, "gamma", "Gamma Skill", Some("Denied by config"), None);
            write_skill(work, "delta", "Delta Skill", None, None);
        },
    )
    .await;

    harness.prompt("hi").await;

    let system = &harness.model.requests()[0].system[1];
    assert!(system.contains("<available_skills>"), "got {system:?}");
    assert!(system.contains("id=\"alpha\""));
    assert!(system.contains("Rust &amp; &lt;Build&gt;"));
    assert!(system.contains("Use &amp; verify &lt;carefully&gt;"));
    assert!(!system.contains("id=\"beta\""));
    assert!(!system.contains("id=\"gamma\""));
    assert!(!system.contains("id=\"delta\""));
    assert!(!system.contains("Rust & <Build>"));
}

#[tokio::test]
async fn agents_file_change_publishes_a_delta_before_the_next_call() {
    let harness = Harness::new_with_setup(
        vec![text_script("first"), text_script("second")],
        Arc::new(AllowAll),
        json!({}),
        None,
        |work| {
            std::fs::write(work.join("AGENTS.md"), "First instruction marker.\n")
                .expect("agents file");
        },
    )
    .await;

    harness.prompt("first").await;
    assert!(harness.model.requests()[0].system[1].contains("First instruction marker."));
    assert_eq!(harness.sink.count("session.instructions.updated"), 0);

    std::fs::write(
        harness.work.join("AGENTS.md"),
        "Second instruction marker.\n",
    )
    .expect("agents update");
    harness.prompt("second").await;

    let kinds = harness.sink.kinds();
    let updated = kinds
        .iter()
        .position(|kind| kind == "session.instructions.updated")
        .expect("instructions delta");
    let last_step = kinds
        .iter()
        .rposition(|kind| kind == "session.step.started")
        .expect("step started");
    assert!(updated < last_step, "delta must precede the next request");

    let data = harness
        .sink
        .data("session.instructions.updated")
        .pop()
        .expect("delta data");
    let delta = data["delta"].as_object().expect("delta map");
    assert!(
        delta.keys().any(|key| key.starts_with("discovery:")),
        "changed source is reported: {delta:?}"
    );
    for (key, value) in delta {
        let value = value.as_str().expect("delta values are strings");
        assert!(
            value == "removed"
                || (value.len() == 16 && value.chars().all(|c| c.is_ascii_hexdigit())),
            "delta[{key}] is a hash or \"removed\", got {value:?}"
        );
    }
    assert!(
        data["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Second instruction marker.")
    );
    let envelope = harness
        .sink
        .events
        .lock()
        .expect("events")
        .iter()
        .find(|event| event.kind == "session.instructions.updated")
        .cloned()
        .expect("instruction envelope");
    let durable = envelope.durable.expect("instruction deltas are durable");
    assert!(durable.seq > 0, "durable events carry a sequence number");
    assert_eq!(durable.version, 2);
    let logged = harness
        .store
        .events_after(&harness.session, 0, 1_000)
        .await
        .expect("event log");
    let stored = logged
        .iter()
        .find(|event| event.kind == "session.instructions.updated")
        .expect("instructions delta persisted before publish");
    assert_eq!(stored.version, 2);
    assert_eq!(stored.seq, durable.seq);

    let requests = harness.model.requests();
    let second = &requests[1].system[1];
    assert!(second.contains("Second instruction marker."));
    assert!(!second.contains("First instruction marker."));
}

#[tokio::test]
async fn memory_write_tool_persists_through_the_merged_registry() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "memory_write",
                "call_write",
                json!({
                    "text": "The build lane is quiet-river.",
                    "kind": "fact",
                    "expectation": expectation()
                }),
            ),
            text_script("stored"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    let harness = harness.attach_memory(memory);

    harness.prompt("remember the build lane").await;

    let request = &harness.model.requests()[0];
    assert!(request.tools.iter().any(|tool| tool.name == "memory_write"));
    let rows = all_memories(&harness.store).await;
    assert_eq!(rows.len(), 1, "got {rows:?}");
    assert_eq!(rows[0]["source"], "agent");
    assert_eq!(rows[0]["scope"], "project");
    assert_eq!(rows[0]["scopeID"], "prj_runtime_test");
    assert_eq!(rows[0]["text"], "The build lane is quiet-river.");
}

#[tokio::test]
async fn memory_search_tool_returns_a_compact_block() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "memory_search",
                "call_search",
                json!({"query": "quiet river lane", "expectation": expectation()}),
            ),
            text_script("found it"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    seed_memory(
        &memory,
        "The quiet-river lane serializes cargo builds.",
        "project",
        Some("prj_runtime_test"),
        4,
    )
    .await;
    let harness = harness.attach_memory(memory);

    harness
        .prompt("search memory for the quiet river lane")
        .await;

    let result = request_tool_result(&harness.model.requests()[1], 0);
    assert!(
        result.contains("<memory scope=\"project\">"),
        "got {result:?}"
    );
    assert!(result.contains("quiet-river"));
    assert!(result.contains("mem_"));
}

#[tokio::test]
async fn turn_end_touches_injected_memories_once() {
    let harness = Harness::new(
        vec![text_script("done")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    seed_memory(
        &memory,
        "Pinned memory that is always injected.",
        "project",
        Some("prj_runtime_test"),
        5,
    )
    .await;
    let harness = harness.attach_memory(memory);

    harness.prompt("anything").await;

    let rows = all_memories(&harness.store).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["uses"], 1);
    assert!(rows[0]["lastUsed"].is_i64());
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn scribe_persists_candidates_in_the_session_scope() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "read",
                "call_read",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("the file says hello"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    let scribe = FakeScribe::new(
        r#"[{"text":"The runtime merges memory tools into every session registry.","kind":"fact","tags":["runtime"],"importance":3,"confidence":0.9}]"#,
    );
    let harness = harness.attach_memory(memory).attach_scribe(scribe);

    harness
        .prompt("read hello.txt and remember the runtime detail")
        .await;

    let rows = wait_for_memories(&harness.store, "scribe").await;
    assert_eq!(rows.len(), 1, "got {rows:?}");
    assert_eq!(rows[0]["scope"], "session");
    assert_eq!(rows[0]["scopeID"], harness.session);
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn auto_scribe_true_runs_the_turn_end_scribe() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "read",
                "call_read",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("the file says hello"),
        ],
        Arc::new(AllowAll),
        json!({"memory": {"auto_scribe": true}}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    let scribe = FakeScribe::new("[]");
    let prompts = Arc::clone(&scribe);
    let harness = harness.attach_memory(memory).attach_scribe(scribe);

    harness.prompt("read hello.txt").await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while prompts.prompts.lock().expect("prompts").is_empty() {
        assert!(Instant::now() < deadline, "scribe never ran");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn auto_scribe_false_skips_the_turn_end_scribe() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "read",
                "call_read",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("the file says hello"),
        ],
        Arc::new(AllowAll),
        json!({"memory": {"auto_scribe": false}}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    let scribe = FakeScribe::new(
        r#"[{"text":"The runtime merges memory tools into every session registry.","kind":"fact","tags":["runtime"],"importance":3,"confidence":0.9}]"#,
    );
    let prompts = Arc::clone(&scribe);
    let harness = harness.attach_memory(memory).attach_scribe(scribe);

    harness.prompt("read hello.txt").await;

    // The turn is already idle; give the spawned pass a bounded window so a
    // regression that ignores the flag is caught rather than timed out.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        prompts.prompts.lock().expect("prompts").is_empty(),
        "memory.auto_scribe=false must not run the scribe"
    );
    assert!(all_memories(&harness.store).await.is_empty());
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn invalid_scribe_json_does_not_fail_the_turn() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "read",
                "call_read",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("the file says hello"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    let scribe = FakeScribe::new("this is not a json array");
    let prompts = Arc::clone(&scribe);
    let harness = harness.attach_memory(memory).attach_scribe(scribe);

    harness.prompt("read hello.txt").await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while prompts.prompts.lock().expect("prompts").is_empty() {
        assert!(Instant::now() < deadline, "scribe never ran");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(all_memories(&harness.store).await.is_empty());
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn runtime_without_memory_keeps_the_old_shape() {
    let harness = Harness::new(
        vec![text_script("plain")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("hi").await;

    let request = &harness.model.requests()[0];
    assert!(!request.system[1].contains("<memory"));
    assert!(
        !request
            .tools
            .iter()
            .any(|tool| tool.name.starts_with("memory_"))
    );
    assert_eq!(harness.sink.count("session.instructions.updated"), 0);
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn fts_ranking_churn_and_mid_epoch_goal_keep_the_system_prefix_cached() {
    let harness = Harness::new(
        vec![text_script("turn one"), text_script("turn two")],
        Arc::new(AllowAll),
        json!({"memory": {"auto_scribe": false}}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    seed_memory(
        &memory,
        "Alpha indexing pipeline runs on worker shard zero.",
        "project",
        Some("prj_runtime_test"),
        3,
    )
    .await;
    let harness = harness.attach_memory(Arc::clone(&memory));

    harness.prompt("alpha indexing pipeline").await;
    let first_prefix = harness.model.requests()[0].system[1].clone();
    assert!(first_prefix.contains("Alpha indexing pipeline"));

    for index in 0..8 {
        seed_memory(
            &memory,
            &format!("Beta migration step {index} updates schema version {index}."),
            "project",
            Some("prj_runtime_test"),
            4,
        )
        .await;
    }
    harness
        .store
        .set_goal(oc_store::NewGoal {
            id: "gol_mid_epoch".to_owned(),
            root_id: harness.session.clone(),
            objective: "finish the beta migration".to_owned(),
            criteria: vec!["all beta steps pass".to_owned()],
            model: None,
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("set goal");

    harness.prompt("beta migration step schema").await;
    let requests = harness.model.requests();
    assert_eq!(
        requests[1].system[1], first_prefix,
        "live non-pinned memories and mid-epoch goals must not invalidate the system prefix"
    );
    assert_eq!(
        harness.sink.count("session.instructions.updated"),
        0,
        "no instruction delta should be published when pinned memories are still live"
    );
    assert!(
        request_contains(&requests[1], "Beta migration step"),
        "novel FTS hits are delivered in the tail supplement"
    );
    assert!(
        request_contains(&requests[1], "finish the beta migration"),
        "mid-epoch goal state is delivered in the tail supplement"
    );
}
