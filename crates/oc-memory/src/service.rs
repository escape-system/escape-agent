//! Retrieval and write paths for permanent memory.
//!
//! [`MemoryService`] wraps [`oc_store::Store`]: retrieval merges the caller's
//! visible scopes, ranks pinned rows first, removes exact and near duplicates,
//! then renders a `<memory>` block trimmed to a token budget. Writes apply the
//! 2000-character cap, the secret detector and the documented defaults.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use oc_store::{MemoryHit, MemoryQuery, NewMemory, Store};
use oc_types::MemoryId;
use serde_json::Value;

use crate::error::MemoryError;
use crate::render::{
    DEFAULT_TOKEN_BUDGET, near_duplicate, normalize_line, render_block, token_set,
};
use crate::secret::ensure_no_secret;

/// Maximum stored text length, in characters.
pub const TEXT_CAP: usize = 2000;
/// Default importance for writes that omit it.
pub const DEFAULT_IMPORTANCE: u8 = 3;
/// Default confidence for writes that omit it.
pub const DEFAULT_CONFIDENCE: f32 = 0.7;
/// Importance at which a memory is pinned into every retrieval.
pub const PINNED_IMPORTANCE: u8 = 5;
/// Retrieval never asks the store for fewer than this many rows.
pub const MIN_SEARCH_LIMIT: usize = 12;
/// Upper bound accepted by the store's search.
pub const MAX_SEARCH_LIMIT: usize = 50;
/// Default page size for [`MemoryService::list`].
pub const LIST_DEFAULT: usize = 20;
/// Maximum page size for [`MemoryService::list`].
pub const LIST_MAX: usize = 200;
/// The store caps lists at the same value; pagination cannot go beyond it.
const LIST_FETCH_CAP: usize = 200;

const SCOPES: [&str; 4] = ["global", "project", "agent", "session"];

/// One retrieval request: visible scopes, query text and rendering budget.
#[derive(Debug, Clone, Default)]
pub struct RetrieveRequest {
    /// Visible scopes as `(scope, scope_id)`, most-shared first.
    pub scopes: Vec<(String, Option<String>)>,
    /// Free-text query; empty means "rank everything visible".
    pub query: String,
    /// Restrict to these kinds.
    pub kinds: Vec<String>,
    /// Require at least one of these tags.
    pub tags: Vec<String>,
    /// Requested result count; the service asks for at least 12.
    pub limit: usize,
    /// Estimated token budget; `0` uses [`DEFAULT_TOKEN_BUDGET`].
    pub token_budget: usize,
}

/// Rendered memory block plus the ids it contains.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryBlock {
    /// `<memory>` block, or empty when nothing was retrieved.
    pub text: String,
    /// Ids rendered, in block order.
    pub ids: Vec<String>,
    /// Estimated tokens (`chars / 4`) of `text`.
    pub tokens: u64,
}

/// A memory to write. Defaults match the memory contract: kind `fact`, scope
/// `project`, importance 3, confidence 0.7, source `agent`.
#[derive(Debug, Clone, Default)]
pub struct NewMemoryInput {
    /// Memory text; required, trimmed, at most [`TEXT_CAP`] characters.
    pub text: String,
    /// One of `fact`, `decision`, `preference`, `pattern`, `gotcha`.
    pub kind: Option<String>,
    /// One of `global`, `project`, `agent`, `session`.
    pub scope: Option<String>,
    /// Project / agent / session id; ignored for `global`.
    pub scope_id: Option<String>,
    /// Free-form tags.
    pub tags: Vec<String>,
    /// 1..=5; the store clamps.
    pub importance: Option<u8>,
    /// 0.0..=1.0; the store clamps.
    pub confidence: Option<f32>,
    /// `user`, `agent`, `scribe` or `import`.
    pub source: Option<String>,
    /// Session that produced the memory.
    pub source_session: Option<String>,
    /// Message that produced the memory.
    pub source_message: Option<String>,
}

/// A list request. `cursor` is the decimal offset returned with the previous
/// page (the tools expose it as `cursor=<n>`).
#[derive(Debug, Clone, Default)]
pub struct ListRequest {
    /// Visible scopes as `(scope, scope_id)`, most-shared first.
    pub scopes: Vec<(String, Option<String>)>,
    /// Restrict to this kind.
    pub kind: Option<String>,
    /// Require at least one of these tags.
    pub tags: Vec<String>,
    /// Page size; `0` uses [`LIST_DEFAULT`].
    pub limit: usize,
    /// Offset cursor from a previous page.
    pub cursor: Option<String>,
}

/// Memory reads, writes and list operations over one store.
#[derive(Clone)]
pub struct MemoryService {
    store: Arc<Store>,
}

impl fmt::Debug for MemoryService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryService")
            .finish_non_exhaustive()
    }
}

impl MemoryService {
    /// Wraps a store.
    #[must_use]
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Retrieves and renders the memory block for `request`.
    ///
    /// Scopes must name their ids (`("project", Some(prj_…))`); a bare
    /// `("project", None)` is rejected so one project can never read another's
    /// memory by omission. An empty scope list is rejected for the same reason.
    pub async fn retrieve(&self, request: RetrieveRequest) -> Result<MemoryBlock, MemoryError> {
        if request.scopes.is_empty() {
            return Err(MemoryError::invalid(
                "retrieval needs at least one scope: global, project, agent or session",
            ));
        }
        for (scope, scope_id) in &request.scopes {
            validate_scope(scope, scope_id.as_deref())?;
        }

        let limit = request.limit.clamp(MIN_SEARCH_LIMIT, MAX_SEARCH_LIMIT);
        let query = request.query.trim();
        let hits = self
            .store
            .search_memories(MemoryQuery {
                scopes: request.scopes.clone(),
                query: (!query.is_empty()).then(|| query.to_owned()),
                kinds: request.kinds,
                tags: request.tags,
                limit,
                pinned_min_importance: Some(PINNED_IMPORTANCE),
            })
            .await?;

        let ranked = rank_and_dedupe(hits);
        let lines: Vec<(String, String)> = ranked
            .iter()
            .map(|hit| {
                (
                    hit.id.clone(),
                    format!(
                        "- [{}] {} ({})",
                        hit.kind,
                        normalize_line(&hit.text),
                        hit.id
                    ),
                )
            })
            .collect();
        let budget = if request.token_budget == 0 {
            DEFAULT_TOKEN_BUDGET
        } else {
            request.token_budget
        };
        Ok(render_block(block_scope(&request.scopes), &lines, budget))
    }

    /// Writes a memory, folding an equivalent row (same scope and normalized
    /// text) instead of duplicating it. Returns the stored wire row.
    pub async fn write(&self, new: NewMemoryInput) -> Result<Value, MemoryError> {
        let text = new.text.trim().to_owned();
        if text.is_empty() {
            return Err(MemoryError::invalid("memory text must not be empty"));
        }
        let chars = text.chars().count();
        if chars > TEXT_CAP {
            return Err(MemoryError::TextTooLong {
                chars,
                cap: TEXT_CAP,
            });
        }
        ensure_no_secret(&text)?;

        let kind = normalized(new.kind.as_deref(), "fact");
        if kind.is_empty() {
            return Err(MemoryError::invalid("memory kind must not be empty"));
        }
        let scope = normalized(new.scope.as_deref(), "project");
        if !SCOPES.contains(&scope.as_str()) {
            return Err(MemoryError::invalid(format!(
                "unknown memory scope {scope:?}; expected global, project, agent or session"
            )));
        }
        let scope_id = if scope == "global" {
            None
        } else {
            let id = new.scope_id.as_deref().map(str::trim).unwrap_or_default();
            if id.is_empty() {
                return Err(MemoryError::invalid(format!(
                    "scope {scope:?} needs a scope id"
                )));
            }
            Some(id.to_owned())
        };

        let source = normalized(new.source.as_deref(), "agent");
        let memory = NewMemory {
            id: MemoryId::new().into_inner(),
            scope,
            scope_id,
            kind,
            text,
            tags: new.tags,
            importance: new.importance.unwrap_or(DEFAULT_IMPORTANCE),
            confidence: new.confidence.unwrap_or(DEFAULT_CONFIDENCE),
            source,
            source_session: new.source_session,
            source_message: new.source_message,
        };
        Ok(self.store.write_memory(memory).await?)
    }

    /// Soft-deletes a memory. Returns whether a live row was changed.
    pub async fn forget(&self, id: &str) -> Result<bool, MemoryError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(MemoryError::invalid("memory id must not be empty"));
        }
        Ok(self.store.forget_memory(id).await?)
    }

    /// Lists memories for the requested scopes, most important and most recent
    /// first.
    pub async fn list(&self, request: ListRequest) -> Result<Vec<Value>, MemoryError> {
        if request.scopes.is_empty() {
            return Err(MemoryError::invalid(
                "listing needs at least one scope: global, project, agent or session",
            ));
        }
        for (scope, scope_id) in &request.scopes {
            validate_scope(scope, scope_id.as_deref())?;
        }
        let offset = parse_cursor(request.cursor.as_deref())?;
        let limit = if request.limit == 0 {
            LIST_DEFAULT
        } else {
            request.limit.min(LIST_MAX)
        };
        let fetch = offset.saturating_add(limit).min(LIST_FETCH_CAP);
        if fetch <= offset {
            return Ok(Vec::new());
        }
        let kinds: Vec<String> = request
            .kind
            .into_iter()
            .map(|kind| kind.trim().to_owned())
            .filter(|kind| !kind.is_empty())
            .collect();
        let items = self
            .store
            .list_memories(MemoryQuery {
                scopes: request.scopes,
                query: None,
                kinds,
                tags: request.tags,
                limit: fetch,
                pinned_min_importance: None,
            })
            .await?;
        Ok(items.into_iter().skip(offset).take(limit).collect())
    }

    /// Bumps `uses`/`last_used` for injected memories; call once at turn end.
    pub async fn touch(&self, ids: &[String]) -> Result<(), MemoryError> {
        self.store.touch_memories(ids, now_ms()).await?;
        Ok(())
    }
}

/// Rejects scopes that would read across projects by omission.
pub(crate) fn validate_scope(scope: &str, scope_id: Option<&str>) -> Result<(), MemoryError> {
    if !SCOPES.contains(&scope) {
        return Err(MemoryError::invalid(format!(
            "unknown memory scope {scope:?}; expected global, project, agent or session"
        )));
    }
    if scope == "global" {
        return if scope_id.is_none() {
            Ok(())
        } else {
            Err(MemoryError::invalid("global memories have no scope id"))
        };
    }
    match scope_id {
        Some(id) if !id.trim().is_empty() => Ok(()),
        _ => Err(MemoryError::invalid(format!(
            "scope {scope:?} needs a scope id"
        ))),
    }
}

fn normalized(value: Option<&str>, fallback: &str) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

fn parse_cursor(cursor: Option<&str>) -> Result<usize, MemoryError> {
    match cursor.map(str::trim) {
        None | Some("") => Ok(0),
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| MemoryError::invalid("cursor is not a memory list cursor")),
    }
}

fn block_scope(scopes: &[(String, Option<String>)]) -> &str {
    scopes
        .iter()
        .find(|(scope, _)| scope == "project")
        .or_else(|| scopes.first())
        .map_or("project", |(scope, _)| scope.as_str())
}

fn pinned(hit: &MemoryHit) -> u8 {
    u8::from(hit.importance >= PINNED_IMPORTANCE)
}

/// Pinned first, then hybrid score, then importance; exact ids and near
/// duplicates (Jaccard `>= 0.85`) keep the higher-ranked entry.
fn rank_and_dedupe(mut hits: Vec<MemoryHit>) -> Vec<MemoryHit> {
    hits.sort_by(|left, right| {
        pinned(right)
            .cmp(&pinned(left))
            .then_with(|| right.score.total_cmp(&left.score))
            .then_with(|| right.importance.cmp(&left.importance))
    });

    let mut seen = HashSet::with_capacity(hits.len());
    hits.retain(|hit| seen.insert(hit.id.clone()));

    let mut kept: Vec<MemoryHit> = Vec::with_capacity(hits.len());
    let mut kept_tokens: Vec<BTreeSet<String>> = Vec::new();
    for hit in hits {
        let tokens = token_set(&hit.text);
        if kept_tokens
            .iter()
            .any(|existing| near_duplicate(existing, &tokens))
        {
            continue;
        }
        kept_tokens.push(tokens);
        kept.push(hit);
    }
    kept
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SecretRule;

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
}
