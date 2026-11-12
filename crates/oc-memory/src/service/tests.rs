//! Tests for the retrieval, write, list and touch paths.

use std::sync::Arc;

use oc_store::Store;

use crate::error::{MemoryError, SecretRule};
use crate::render::DEFAULT_TOKEN_BUDGET;

use super::{ListRequest, MemoryBlock, MemoryService, NewMemoryInput, RetrieveRequest, TEXT_CAP};

async fn service() -> MemoryService {
    MemoryService::new(Arc::new(Store::open_memory().await.expect("memory store")))
}

fn scopes(project: &str) -> Vec<(String, Option<String>)> {
    vec![
        ("global".to_owned(), None),
        ("project".to_owned(), Some(project.to_owned())),
    ]
}

async fn write(
    service: &MemoryService,
    text: &str,
    scope: &str,
    scope_id: Option<&str>,
    importance: u8,
) -> String {
    let value = service
        .write(NewMemoryInput {
            text: text.to_owned(),
            scope: Some(scope.to_owned()),
            scope_id: scope_id.map(str::to_owned),
            importance: Some(importance),
            ..NewMemoryInput::default()
        })
        .await
        .expect("write memory");
    value["id"].as_str().expect("id").to_owned()
}

#[tokio::test]
async fn temp_file_store_survives_reopen() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("memory.sqlite");
    let id = {
        let service = MemoryService::new(Arc::new(Store::open(&path).await.expect("open")));
        write(
            &service,
            "durable fact stored in a temporary sqlite file",
            "project",
            Some("prj_a"),
            3,
        )
        .await
    };

    let reopened = MemoryService::new(Arc::new(Store::open(&path).await.expect("reopen")));
    let block = reopened
        .retrieve(RetrieveRequest {
            scopes: scopes("prj_a"),
            query: "durable".to_owned(),
            ..RetrieveRequest::default()
        })
        .await
        .expect("retrieve");
    assert!(block.ids.contains(&id), "{:?}", block.ids);
}

#[tokio::test]
async fn empty_store_retrieves_an_empty_block() {
    let service = service().await;
    let block = service
        .retrieve(RetrieveRequest {
            scopes: scopes("prj_a"),
            query: "anything at all".to_owned(),
            ..RetrieveRequest::default()
        })
        .await
        .expect("retrieve");
    assert_eq!(block, MemoryBlock::default());
    assert!(block.text.is_empty());
    assert_eq!(block.tokens, 0);
}

#[tokio::test]
async fn retrieval_isolated_by_scope() {
    let service = service().await;
    let alpha = write(
        &service,
        "alpha project keeps its index in sqlite with one writer",
        "project",
        Some("prj_a"),
        3,
    )
    .await;
    let beta = write(
        &service,
        "beta project keeps its index in sqlite with one writer",
        "project",
        Some("prj_b"),
        3,
    )
    .await;
    let global = write(
        &service,
        "prefer running cargo fmt before tests in every project",
        "global",
        None,
        3,
    )
    .await;

    let block = service
        .retrieve(RetrieveRequest {
            scopes: scopes("prj_a"),
            query: "index sqlite".to_owned(),
            ..RetrieveRequest::default()
        })
        .await
        .expect("retrieve");
    assert!(block.ids.contains(&alpha), "{:?}", block.ids);
    assert!(!block.ids.contains(&beta), "{:?}", block.ids);

    let beta_block = service
        .retrieve(RetrieveRequest {
            scopes: scopes("prj_b"),
            query: "index sqlite".to_owned(),
            ..RetrieveRequest::default()
        })
        .await
        .expect("retrieve");
    assert!(beta_block.ids.contains(&beta));
    assert!(!beta_block.ids.contains(&alpha));

    let global_block = service
        .retrieve(RetrieveRequest {
            scopes: scopes("prj_b"),
            query: "cargo fmt".to_owned(),
            ..RetrieveRequest::default()
        })
        .await
        .expect("retrieve");
    assert!(global_block.ids.contains(&global));
}

#[tokio::test]
async fn peek_at_another_project_without_an_id_is_rejected() {
    let service = service().await;
    let error = service
        .retrieve(RetrieveRequest {
            scopes: vec![("project".to_owned(), None)],
            ..RetrieveRequest::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(error, MemoryError::Invalid(_)), "{error:?}");

    let error = service
        .retrieve(RetrieveRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(error, MemoryError::Invalid(_)), "{error:?}");
}

#[tokio::test]
async fn pinned_memories_render_first_even_without_a_match() {
    let service = service().await;
    let pinned = write(
        &service,
        "the release process requires two approvals from the maintainers",
        "project",
        Some("prj_a"),
        5,
    )
    .await;
    let matched = write(
        &service,
        "retrieval scores come from bm25 plus importance and recency",
        "project",
        Some("prj_a"),
        2,
    )
    .await;

    let block = service
        .retrieve(RetrieveRequest {
            scopes: scopes("prj_a"),
            query: "retrieval".to_owned(),
            ..RetrieveRequest::default()
        })
        .await
        .expect("retrieve");
    assert_eq!(block.ids.first(), Some(&pinned), "{:?}", block.ids);
    assert!(block.ids.contains(&matched));
}

#[tokio::test]
async fn preferences_rank_above_standard_facts() {
    let service = service().await;
    let pref = service
        .write(NewMemoryInput {
            text: "User prefers async tokio and functional stream pipelines".to_owned(),
            kind: Some("preference".to_owned()),
            scope: Some("project".to_owned()),
            scope_id: Some("prj_a".to_owned()),
            importance: Some(3),
            ..NewMemoryInput::default()
        })
        .await
        .expect("write pref");
    let fact = service
        .write(NewMemoryInput {
            text: "Pipelines module is implemented in src/stream_pipelines.rs".to_owned(),
            kind: Some("fact".to_owned()),
            scope: Some("project".to_owned()),
            scope_id: Some("prj_a".to_owned()),
            importance: Some(4),
            ..NewMemoryInput::default()
        })
        .await
        .expect("write fact");

    let block = service
        .retrieve(RetrieveRequest {
            scopes: scopes("prj_a"),
            query: "pipelines".to_owned(),
            ..RetrieveRequest::default()
        })
        .await
        .expect("retrieve");
    let pref_id = pref["id"].as_str().expect("pref id").to_owned();
    let fact_id = fact["id"].as_str().expect("fact id").to_owned();
    assert_eq!(block.ids.first(), Some(&pref_id));
    assert!(block.ids.contains(&fact_id));
}

#[tokio::test]
async fn token_budget_trims_whole_lines() {
    let service = service().await;
    let facts = [
        "budgettoken one: the scheduler routes cargo builds through a single lane so the target directory lock never blocks unrelated compilers",
        "budgettoken two: session prompts are cached by hashing every instruction source in canonical order before the provider request is built",
        "budgettoken three: sqlite runs in wal mode with a busy timeout so long reads never stall the single writer actor",
        "budgettoken four: the observer records timeline spans without copying message bodies and prunes them on a retention schedule",
        "budgettoken five: tool output larger than the cap is written to a data directory file and referenced by path in metadata",
        "budgettoken six: permission rules are evaluated last match wins over the agent permissions after config composition",
    ];
    for text in facts {
        write(&service, text, "project", Some("prj_a"), 3).await;
    }

    let block = service
        .retrieve(RetrieveRequest {
            scopes: scopes("prj_a"),
            query: "budgettoken".to_owned(),
            token_budget: 60,
            ..RetrieveRequest::default()
        })
        .await
        .expect("retrieve");
    assert!(!block.text.is_empty());
    assert!(block.ids.len() < 6);
    assert!(block.tokens <= 60, "{} tokens", block.tokens);
    assert!(block.text.ends_with("</memory>"));
    assert_eq!(block.text.lines().count(), block.ids.len() + 2);

    let empty = service
        .retrieve(RetrieveRequest {
            scopes: scopes("prj_a"),
            query: "budgettoken".to_owned(),
            token_budget: 1,
            ..RetrieveRequest::default()
        })
        .await
        .expect("retrieve");
    assert!(empty.text.is_empty());
    assert!(empty.ids.is_empty());

    let full = service
        .retrieve(RetrieveRequest {
            scopes: scopes("prj_a"),
            query: "budgettoken".to_owned(),
            ..RetrieveRequest::default()
        })
        .await
        .expect("retrieve");
    assert_eq!(full.ids.len(), 6);
    assert!(full.tokens <= DEFAULT_TOKEN_BUDGET as u64);
}

#[tokio::test]
async fn near_duplicates_keep_the_higher_ranked_memory() {
    let service = service().await;
    let kept = write(
        &service,
        "alpha beta gamma delta epsilon zeta eta theta iota kappa",
        "project",
        Some("prj_a"),
        4,
    )
    .await;
    let dropped = write(
        &service,
        "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda",
        "project",
        Some("prj_a"),
        3,
    )
    .await;

    let block = service
        .retrieve(RetrieveRequest {
            scopes: scopes("prj_a"),
            query: "alpha".to_owned(),
            ..RetrieveRequest::default()
        })
        .await
        .expect("retrieve");
    assert_eq!(block.ids, vec![kept]);
    assert_ne!(block.ids, vec![dropped]);
}

#[tokio::test]
async fn secret_patterns_are_rejected_without_echoing() {
    let service = service().await;
    let cases = [
        (
            "deploy with sk-proj-abcdefghijklmnopqrstuvwxyz today",
            SecretRule::OpenAiKey,
        ),
        (
            "token ghp_1234567890abcdefghijklmnopqrstuvwx for github",
            SecretRule::GitHubToken,
        ),
        (
            "slack bot uses xoxb-1234567890-abcdefghijklmnop here",
            SecretRule::SlackToken,
        ),
        (
            "aws key AKIAIOSFODNN7EXAMPLE lives here",
            SecretRule::AwsAccessKey,
        ),
        (
            "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
            SecretRule::Jwt,
        ),
        (
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----",
            SecretRule::PrivateKey,
        ),
        (
            "staging password=hunter2 belongs in the keychain",
            SecretRule::CredentialAssignment,
        ),
    ];

    for (text, expected) in cases {
        let error = service
            .write(NewMemoryInput {
                text: text.to_owned(),
                ..NewMemoryInput::default()
            })
            .await
            .unwrap_err();
        match error {
            MemoryError::Secret { rule } => assert_eq!(rule, expected),
            other => panic!("expected a secret rejection, got {other:?}"),
        }
        let message = error_text(&service, text).await;
        assert!(message.contains(expected.id()), "{message}");
        for token in text.split_whitespace() {
            if token.len() > 12 {
                assert!(!message.contains(token), "leaked {token} in {message}");
            }
        }
    }
}

async fn error_text(service: &MemoryService, text: &str) -> String {
    service
        .write(NewMemoryInput {
            text: text.to_owned(),
            ..NewMemoryInput::default()
        })
        .await
        .unwrap_err()
        .to_string()
}

#[tokio::test]
async fn write_defaults_deduplicate_and_apply_the_cap() {
    let service = service().await;
    let value = service
        .write(NewMemoryInput {
            text: "  prefer boring designs over clever abstractions  ".to_owned(),
            scope: Some("project".to_owned()),
            scope_id: Some("prj_a".to_owned()),
            ..NewMemoryInput::default()
        })
        .await
        .expect("write");
    assert_eq!(value["kind"], "fact");
    assert_eq!(value["importance"], 3);
    assert!((value["confidence"].as_f64().expect("confidence") - 0.7).abs() < 1e-6);
    assert_eq!(value["source"], "agent");
    assert_eq!(
        value["text"],
        "prefer boring designs over clever abstractions"
    );
    let id = value["id"].as_str().expect("id").to_owned();
    assert!(id.starts_with("mem_"), "{id}");

    let folded = service
        .write(NewMemoryInput {
            text: "prefer boring designs over clever abstractions".to_owned(),
            scope: Some("project".to_owned()),
            scope_id: Some("prj_a".to_owned()),
            importance: Some(5),
            ..NewMemoryInput::default()
        })
        .await
        .expect("fold");
    assert_eq!(folded["id"].as_str(), Some(id.as_str()));
    assert_eq!(folded["importance"], 5);

    let too_long = "x".repeat(TEXT_CAP + 1);
    let error = service
        .write(NewMemoryInput {
            text: too_long,
            ..NewMemoryInput::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        MemoryError::TextTooLong {
            chars,
            cap: TEXT_CAP
        } if chars == TEXT_CAP + 1
    ));

    let empty = service.write(NewMemoryInput::default()).await.unwrap_err();
    assert!(matches!(empty, MemoryError::Invalid(_)));
}

#[tokio::test]
async fn list_and_forget_follow_soft_delete_semantics() {
    let service = service().await;
    let first = write(
        &service,
        "first memory about cargo build lanes",
        "project",
        Some("prj_a"),
        4,
    )
    .await;
    let second = write(
        &service,
        "second memory about sqlite writer queues",
        "project",
        Some("prj_a"),
        3,
    )
    .await;
    let third = write(
        &service,
        "third memory about fts5 tokenization",
        "project",
        Some("prj_a"),
        2,
    )
    .await;

    let page = service
        .list(ListRequest {
            scopes: scopes("prj_a"),
            limit: 2,
            ..ListRequest::default()
        })
        .await
        .expect("list");
    assert_eq!(page.len(), 2);
    assert_eq!(page[0]["id"].as_str(), Some(first.as_str()));
    assert_eq!(page[1]["id"].as_str(), Some(second.as_str()));

    let next = service
        .list(ListRequest {
            scopes: scopes("prj_a"),
            limit: 2,
            cursor: Some("2".to_owned()),
            ..ListRequest::default()
        })
        .await
        .expect("list");
    assert_eq!(next.len(), 1);
    assert_eq!(next[0]["id"].as_str(), Some(third.as_str()));

    let other_project = service
        .list(ListRequest {
            scopes: vec![("project".to_owned(), Some("prj_b".to_owned()))],
            ..ListRequest::default()
        })
        .await
        .expect("list");
    assert!(other_project.is_empty());

    assert!(service.forget(&second).await.expect("forget"));
    assert!(!service.forget(&second).await.expect("forget twice"));

    let remaining = service
        .list(ListRequest {
            scopes: scopes("prj_a"),
            ..ListRequest::default()
        })
        .await
        .expect("list");
    assert_eq!(remaining.len(), 2);
    assert!(
        remaining
            .iter()
            .all(|row| row["id"].as_str() != Some(second.as_str()))
    );

    let block = service
        .retrieve(RetrieveRequest {
            scopes: scopes("prj_a"),
            query: "sqlite writer queues".to_owned(),
            ..RetrieveRequest::default()
        })
        .await
        .expect("retrieve");
    assert!(block.ids.is_empty(), "{:?}", block.ids);

    let bad_cursor = service
        .list(ListRequest {
            scopes: scopes("prj_a"),
            cursor: Some("not-a-cursor".to_owned()),
            ..ListRequest::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(bad_cursor, MemoryError::Invalid(_)));
}

#[tokio::test]
async fn touch_bumps_usage() {
    let service = service().await;
    let id = write(
        &service,
        "touch accounting happens once per turn",
        "project",
        Some("prj_a"),
        3,
    )
    .await;
    service
        .touch(std::slice::from_ref(&id))
        .await
        .expect("touch");
    let rows = service
        .list(ListRequest {
            scopes: scopes("prj_a"),
            ..ListRequest::default()
        })
        .await
        .expect("list");
    assert_eq!(rows[0]["uses"], 1);
}
