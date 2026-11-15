//! Model resolution for a child session.
//!
//! Candidate order, highest first (per `docs/05-subagents.md`):
//! call model → `models.chains.<agent>` else `models.chain` →
//! `agents.<id>.model` → `subagents.agents.<id>.model` →
//! `subagents.default_model` → parent session model.
//!
//! The merged list is deduplicated by exact reference string (the first
//! occurrence wins, so a chain entry that repeats the call reference is dropped
//! and cannot shadow it) and then filtered by [`CandidateFilter::deny`], which
//! is variant-insensitive because quota is tracked per `provider/id`.
//!
//! Selection falls through: a candidate that is malformed or absent from the
//! catalog is recorded as a [`ModelSkip`] and the next candidate is tried. A
//! typo in `models.chain` therefore degrades to the next entry instead of
//! aborting the spawn, while a list with no usable entry still fails loudly
//! with the full ordered chain and every skip.

use std::collections::{BTreeMap, BTreeSet};

/// Where a candidate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelSource {
    /// The `model` argument of the `subagent` tool call.
    Call,
    /// A `models.chains.<agent>` / `models.chain` entry
    /// ([`CandidateFilter::chain`]).
    Chain,
    /// `agents.<id>.model` or `subagents.agents.<id>.model`.
    Agent,
    /// `subagents.default_model`.
    Default,
    /// The parent session's model.
    Parent,
}

impl ModelSource {
    /// Stable wire string (`modelSource` in tool metadata).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Chain => "chain",
            Self::Agent => "agent",
            Self::Default => "default",
            Self::Parent => "parent",
        }
    }
}

impl std::fmt::Display for ModelSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry of the candidate chain, kept for error reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCandidate {
    pub reference: String,
    pub source: ModelSource,
}

impl ModelCandidate {
    #[must_use]
    pub fn new(reference: impl Into<String>, source: ModelSource) -> Self {
        Self {
            reference: reference.into(),
            source,
        }
    }
}

/// The resolved model and its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub reference: String,
    pub source: ModelSource,
    /// Candidates that were tried and rejected, in the order they were seen.
    /// Empty when the first candidate won.
    pub skipped: Vec<ModelSkip>,
}

/// Why a candidate was not usable.
///
/// The [`SkipReason::as_str`] values appear in tool errors and spawn metadata,
/// so they are stable wire strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkipReason {
    /// The reference is not `provider/id[#variant]`.
    Malformed,
    /// The reference is well-formed but absent from the catalog.
    NotAvailable,
    /// The reference was removed by [`CandidateFilter::deny`] (quota, or a
    /// caller-supplied denial such as an unhealthy model).
    Denied,
}

impl SkipReason {
    /// Stable wire string (`rejectedModels[].reason`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::NotAvailable => "not_available",
            Self::Denied => "denied",
        }
    }
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One candidate that was tried and rejected during resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSkip {
    /// The reference exactly as it appeared in the candidate list.
    pub reference: String,
    /// Where the reference came from.
    pub source: ModelSource,
    /// Why it was rejected.
    pub reason: SkipReason,
}

impl ModelSkip {
    #[must_use]
    pub fn new(reference: impl Into<String>, source: ModelSource, reason: SkipReason) -> Self {
        Self {
            reference: reference.into(),
            source,
            reason,
        }
    }
}

impl std::fmt::Display for ModelSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}, {})", self.reference, self.reason, self.source)
    }
}

/// Already-resolved config lookups. The runtime builds these from
/// `ConfigService`; the policy crate never reads config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelDefaults {
    /// `agent.<id>.model`.
    pub agent_models: BTreeMap<String, String>,
    /// `subagents.agents.<id>.model`.
    pub policy_models: BTreeMap<String, String>,
    /// `subagents.default_model`.
    pub default_model: Option<String>,
}

impl ModelDefaults {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_agent_model(mut self, agent: impl Into<String>, model: impl Into<String>) -> Self {
        self.agent_models.insert(agent.into(), model.into());
        self
    }

    #[must_use]
    pub fn with_policy_model(mut self, agent: impl Into<String>, model: impl Into<String>) -> Self {
        self.policy_models.insert(agent.into(), model.into());
        self
    }

    #[must_use]
    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }
}

/// Optional per-call filter over the candidate list.
///
/// `chain` is the `models.chains.<agent>` / `models.chain` list, inserted after
/// the call model and before the agent rows in declared order. It is a
/// preference order, not a gate: a call model outside the chain still wins.
/// Entries are deduplicated by exact reference against the rest of the list.
///
/// `deny` entries are removed from the merged list variant-insensitively (a
/// quota is tracked per `provider/id`, so denying a model denies every variant
/// of it) and reported as [`SkipReason::Denied`].
///
/// Both lists are plain references so the policy crate keeps owning no config:
/// the runtime builds them from `models.chain` / `models.chains.<agent>` and
/// from the quota policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CandidateFilter<'a> {
    pub chain: &'a [String],
    pub deny: &'a [String],
}

impl<'a> CandidateFilter<'a> {
    #[must_use]
    pub const fn new(chain: &'a [String], deny: &'a [String]) -> Self {
        Self { chain, deny }
    }
}

/// Resolve the child's model reference.
///
/// `available` is the catalog's exact reference list (`provider/id[#variant]`).
/// An unknown variant fails membership; that candidate is skipped and the next
/// one is tried.
pub fn resolve_model(
    call: Option<&str>,
    agent: Option<&str>,
    defaults: ModelDefaults,
    parent: &str,
    available: &[String],
) -> Result<ModelChoice, crate::ModelError> {
    resolve_model_with(call, agent, defaults, parent, available, None)
}

/// [`resolve_model`] with an optional candidate filter.
///
/// The merged list is built in this order: call model, `filter.chain` entries
/// in declared order, `agents.<id>.model`, `subagents.agents.<id>.model`,
/// `subagents.default_model`, parent session model. Entries with an empty or
/// whitespace-only reference are ignored, as are repeats of an earlier exact
/// reference (first occurrence wins, so a chain entry cannot shadow the call).
/// `filter.deny` then removes matching models variant-insensitively.
///
/// The first well-formed candidate present in `available` wins. Every rejected
/// candidate is reported in [`ModelChoice::skipped`]; if the list is empty the
/// error is [`crate::ModelError::NoCandidate`], and if nothing in it is usable
/// the error is [`crate::ModelError::NoUsableCandidate`].
pub fn resolve_model_with(
    call: Option<&str>,
    agent: Option<&str>,
    defaults: ModelDefaults,
    parent: &str,
    available: &[String],
    filter: Option<&CandidateFilter<'_>>,
) -> Result<ModelChoice, crate::ModelError> {
    let ModelDefaults {
        agent_models,
        policy_models,
        default_model,
    } = defaults;

    let mut merged: Vec<ModelCandidate> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    if let Some(reference) = call {
        push_candidate(&mut merged, &mut seen, reference, ModelSource::Call);
    }
    if let Some(filter) = filter {
        for reference in filter.chain {
            push_candidate(&mut merged, &mut seen, reference, ModelSource::Chain);
        }
    }
    if let Some(agent) = agent {
        if let Some(reference) = agent_models.get(agent) {
            push_candidate(&mut merged, &mut seen, reference, ModelSource::Agent);
        }
        if let Some(reference) = policy_models.get(agent) {
            push_candidate(&mut merged, &mut seen, reference, ModelSource::Agent);
        }
    }
    if let Some(reference) = default_model {
        push_candidate(&mut merged, &mut seen, &reference, ModelSource::Default);
    }
    push_candidate(&mut merged, &mut seen, parent, ModelSource::Parent);

    if merged.is_empty() {
        return Err(crate::ModelError::NoCandidate { chain: merged });
    }

    // Denial is per model, not per variant: a quota is tracked on
    // `provider/id`, so denying the model denies every variant of it. Every
    // unusable candidate is recorded in `skipped` at its original position.
    let mut skipped: Vec<ModelSkip> = Vec::new();
    for candidate in &merged {
        let denied = filter.is_some_and(|filter| {
            filter
                .deny
                .iter()
                .any(|denied| same_model(denied, &candidate.reference))
        });
        if denied {
            skipped.push(ModelSkip::new(
                candidate.reference.clone(),
                candidate.source,
                SkipReason::Denied,
            ));
            continue;
        }
        if !well_formed(&candidate.reference) {
            skipped.push(ModelSkip::new(
                candidate.reference.clone(),
                candidate.source,
                SkipReason::Malformed,
            ));
            continue;
        }
        if let Some(exact) = lookup_available(&candidate.reference, available) {
            return Ok(ModelChoice {
                reference: exact,
                source: candidate.source,
                skipped,
            });
        }
        skipped.push(ModelSkip::new(
            candidate.reference.clone(),
            candidate.source,
            SkipReason::NotAvailable,
        ));
    }

    Err(crate::ModelError::NoUsableCandidate {
        chain: merged,
        skipped,
    })
}

/// Append a candidate unless its reference is blank or already present.
///
/// Exact-string dedup only: `ModelSource` still records the first origin, so a
/// chain entry that repeats the call reference is dropped, not preferred.
fn push_candidate(
    merged: &mut Vec<ModelCandidate>,
    seen: &mut BTreeSet<String>,
    reference: &str,
    source: ModelSource,
) {
    if reference.trim().is_empty() || !seen.insert(reference.to_owned()) {
        return;
    }
    merged.push(ModelCandidate::new(reference, source));
}

/// Exact catalog reference or the dot-to-hyphen alias `resolve_model` has
/// always accepted.
fn lookup_available(reference: &str, available: &[String]) -> Option<String> {
    if let Some(exact) = available.iter().find(|model| *model == reference) {
        return Some(exact.clone());
    }
    let normalized_chosen = normalize_model_ref(reference);
    available
        .iter()
        .find(|model| normalize_model_ref(model) == normalized_chosen)
        .cloned()
}

fn normalize_model_ref(reference: &str) -> String {
    match reference.split_once('#') {
        Some((base, variant)) => format!(
            "{}#{}",
            base.to_ascii_lowercase().replace('.', "-"),
            variant
        ),
        None => reference.to_ascii_lowercase().replace('.', "-"),
    }
}

/// `provider/id` or `provider/id#variant`.
fn well_formed(reference: &str) -> bool {
    let (base, variant) = match reference.split_once('#') {
        Some((base, variant)) => (base, Some(variant)),
        None => (reference, None),
    };
    if variant.is_some_and(|variant| variant.is_empty() || variant.contains('#')) {
        return false;
    }
    match base.split_once('/') {
        Some((provider, id)) => !provider.is_empty() && !id.is_empty(),
        None => false,
    }
}

/// Equality on the model (variant-insensitive): quota is tracked per
/// `provider/id`, so denying a model removes all of its variants.
fn same_model(left: &str, right: &str) -> bool {
    normalize_model_ref(model_base(left)) == normalize_model_ref(model_base(right))
}

/// Variant-insensitive part of a reference. A free function (not a closure):
/// rustc cannot generalize a closure with a free input lifetime to `for<'a>`.
fn model_base(reference: &str) -> &str {
    reference
        .split_once('#')
        .map_or(reference, |(base, _)| base)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Vec<String> {
        [
            "google/gemini-3-pro",
            "test/alpha",
            "test/beta",
            "test/gamma",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    fn refs(list: &[String]) -> Vec<&str> {
        list.iter().map(String::as_str).collect()
    }

    #[test]
    fn call_wins_over_the_chain() {
        let available = catalog();
        let chain = vec!["test/gamma".to_owned()];
        let filter = CandidateFilter::new(&chain, &[]);
        let choice = resolve_model_with(
            Some("test/alpha"),
            None,
            ModelDefaults::new(),
            "test/beta",
            &available,
            Some(&filter),
        )
        .expect("call wins");
        assert_eq!(choice.reference, "test/alpha");
        assert_eq!(choice.source, ModelSource::Call);
        assert_eq!(choice.source.as_str(), "call");
        assert!(choice.skipped.is_empty(), "{:?}", choice.skipped);
    }

    #[test]
    fn chain_entries_come_before_agent_default_and_parent() {
        let available = catalog();
        let chain = vec!["test/gamma".to_owned(), "test/beta".to_owned()];
        let filter = CandidateFilter::new(&chain, &[]);
        let defaults = ModelDefaults::new()
            .with_agent_model("general", "test/alpha")
            .with_policy_model("general", "test/alpha")
            .with_default_model("test/alpha");
        let choice = resolve_model_with(
            None,
            Some("general"),
            defaults,
            "test/alpha",
            &available,
            Some(&filter),
        )
        .expect("the first chain entry wins");
        assert_eq!(choice.reference, "test/gamma");
        assert_eq!(choice.source, ModelSource::Chain);
        assert_eq!(choice.source.as_str(), "chain");
        assert!(choice.skipped.is_empty(), "{:?}", choice.skipped);

        // Declared order is preserved, not sorted.
        let chain = vec!["test/beta".to_owned(), "test/gamma".to_owned()];
        let filter = CandidateFilter::new(&chain, &[]);
        let choice = resolve_model_with(
            None,
            None,
            ModelDefaults::new(),
            "test/alpha",
            &available,
            Some(&filter),
        )
        .expect("declared order wins");
        assert_eq!(choice.reference, "test/beta");
        assert_eq!(choice.source, ModelSource::Chain);
    }

    #[test]
    fn unavailable_call_falls_through_to_the_chain() {
        let available = catalog();
        let chain = vec!["test/gamma".to_owned()];
        let filter = CandidateFilter::new(&chain, &[]);
        let choice = resolve_model_with(
            Some("test/retired"),
            None,
            ModelDefaults::new(),
            "test/alpha",
            &available,
            Some(&filter),
        )
        .expect("an unavailable call falls through");
        assert_eq!(choice.reference, "test/gamma");
        assert_eq!(choice.source, ModelSource::Chain);
        assert_eq!(
            choice.skipped,
            vec![ModelSkip::new(
                "test/retired",
                ModelSource::Call,
                SkipReason::NotAvailable
            )]
        );
    }

    #[test]
    fn malformed_call_falls_through() {
        let available = catalog();
        let chain = vec!["test/gamma".to_owned()];
        let filter = CandidateFilter::new(&chain, &[]);
        let choice = resolve_model_with(
            Some("nonsense"),
            None,
            ModelDefaults::new(),
            "test/alpha",
            &available,
            Some(&filter),
        )
        .expect("a malformed call falls through");
        assert_eq!(choice.reference, "test/gamma");
        assert_eq!(
            choice.skipped,
            vec![ModelSkip::new(
                "nonsense",
                ModelSource::Call,
                SkipReason::Malformed
            )]
        );
        assert_eq!(choice.skipped[0].reason.as_str(), "malformed");
    }

    #[test]
    fn deny_is_variant_insensitive_and_is_reported() {
        let available = catalog();
        let deny = vec!["test/beta".to_owned()];
        let filter = CandidateFilter::new(&[], &deny);
        let choice = resolve_model_with(
            Some("test/beta#low"),
            None,
            ModelDefaults::new(),
            "test/gamma",
            &available,
            Some(&filter),
        )
        .expect("the variant of a denied model is denied too");
        assert_eq!(choice.reference, "test/gamma");
        assert_eq!(choice.source, ModelSource::Parent);
        assert_eq!(
            choice.skipped,
            vec![ModelSkip::new(
                "test/beta#low",
                ModelSource::Call,
                SkipReason::Denied
            )]
        );
        assert_eq!(choice.skipped[0].reason.as_str(), "denied");
    }

    #[test]
    fn duplicate_reference_keeps_the_highest_source() {
        let available = catalog();
        let chain = vec!["test/alpha".to_owned(), "test/beta".to_owned()];
        let filter = CandidateFilter::new(&chain, &[]);
        let choice = resolve_model_with(
            Some("test/alpha"),
            None,
            ModelDefaults::new(),
            "",
            &available,
            Some(&filter),
        )
        .expect("the call reference wins its duplicate");
        assert_eq!(choice.reference, "test/alpha");
        assert_eq!(choice.source, ModelSource::Call);
        assert!(choice.skipped.is_empty(), "{:?}", choice.skipped);

        // The duplicate is gone from the chain, not merely ignored on pick.
        let error = resolve_model_with(
            Some("test/alpha"),
            None,
            ModelDefaults::new(),
            "",
            &[],
            Some(&filter),
        )
        .expect_err("nothing is available");
        match error {
            crate::ModelError::NoUsableCandidate { chain, skipped } => {
                assert_eq!(
                    chain,
                    vec![
                        ModelCandidate::new("test/alpha", ModelSource::Call),
                        ModelCandidate::new("test/beta", ModelSource::Chain),
                    ]
                );
                assert_eq!(
                    skipped,
                    vec![
                        ModelSkip::new("test/alpha", ModelSource::Call, SkipReason::NotAvailable),
                        ModelSkip::new("test/beta", ModelSource::Chain, SkipReason::NotAvailable),
                    ]
                );
            }
            other @ crate::ModelError::NoCandidate { .. } => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn empty_candidate_list_is_no_candidate() {
        let available = catalog();
        let error = resolve_model(None, None, ModelDefaults::new(), "", &available)
            .expect_err("no candidate at all");
        match error {
            crate::ModelError::NoCandidate { chain } => assert!(chain.is_empty(), "{chain:?}"),
            other @ crate::ModelError::NoUsableCandidate { .. } => {
                panic!("unexpected error: {other:?}")
            }
        }

        // A deny list that empties the chain is still no candidate.
        let deny = vec!["test/alpha".to_owned()];
        let filter = CandidateFilter::new(&[], &deny);
        let error = resolve_model_with(
            Some("test/alpha"),
            None,
            ModelDefaults::new(),
            "",
            &available,
            Some(&filter),
        )
        .expect_err("everything was denied");
        assert!(
            matches!(error, crate::ModelError::NoUsableCandidate { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn all_bad_candidates_report_the_chain_and_every_skip() {
        let available = catalog();
        let chain = vec!["test/retired".to_owned()];
        let filter = CandidateFilter::new(&chain, &[]);
        let error = resolve_model_with(
            Some("nonsense"),
            None,
            ModelDefaults::new(),
            "",
            &available,
            Some(&filter),
        )
        .expect_err("nothing is usable");
        let message = match &error {
            crate::ModelError::NoUsableCandidate { chain, skipped } => {
                assert_eq!(
                    chain,
                    &vec![
                        ModelCandidate::new("nonsense", ModelSource::Call),
                        ModelCandidate::new("test/retired", ModelSource::Chain),
                    ]
                );
                assert_eq!(
                    skipped,
                    &vec![
                        ModelSkip::new("nonsense", ModelSource::Call, SkipReason::Malformed),
                        ModelSkip::new(
                            "test/retired",
                            ModelSource::Chain,
                            SkipReason::NotAvailable
                        ),
                    ]
                );
                error.to_string()
            }
            other @ crate::ModelError::NoCandidate { .. } => panic!("unexpected error: {other:?}"),
        };
        assert_eq!(
            message,
            "no usable model candidate; skipped: nonsense (malformed, call), \
             test/retired (not_available, chain); candidates: nonsense (call), \
             test/retired (chain)"
        );
        assert_eq!(SkipReason::Malformed.as_str(), "malformed");
        assert_eq!(SkipReason::NotAvailable.as_str(), "not_available");
        assert_eq!(SkipReason::Denied.as_str(), "denied");
    }

    #[test]
    fn dot_separated_version_alias_resolves() {
        let mut available = catalog();
        available.push("anthropic/claude-sonnet-4-5".to_owned());
        let choice = resolve_model(
            Some("anthropic/claude-sonnet-4.5"),
            None,
            ModelDefaults::new(),
            "test/alpha",
            &available,
        )
        .expect("dot-separated version alias resolves");
        assert_eq!(choice.reference, "anthropic/claude-sonnet-4-5");
        assert_eq!(choice.source, ModelSource::Call);
        assert!(choice.skipped.is_empty(), "{:?}", choice.skipped);
        assert_eq!(
            refs(&available),
            [
                "google/gemini-3-pro",
                "test/alpha",
                "test/beta",
                "test/gamma",
                "anthropic/claude-sonnet-4-5",
            ]
        );
    }
}
