//! Host-side deterministic compaction adviser.
//!
//! The observer engine is an LLM that sees flight strips only; [`crate::observer`]
//! never hands it token usage, so it cannot judge context pressure. This pass is
//! therefore host-side and deterministic: it reads the runtime's own token
//! estimate ([`Runtime::context_pressure`]) and, when pressure crosses 70% of the
//! ceiling while the compaction guard is disarmed, enqueues one durable
//! `observer` inbox row and publishes `observer.advised`.
//!
//! Advice exists only for sessions with **no automatic protection**. The turn
//! loop arms its compaction guard when `compaction.auto` is true *or* an
//! explicit `compaction.ceiling_tokens` is configured, so either condition
//! skips advice; the message can therefore always truthfully say that
//! auto-compaction is off and no ceiling is configured.
//!
//! It never forces compaction: no `compaction` row, no repair command, no goal or
//! expectation mutation. Cooldown is stamped only after the row commits, the
//! session is woken and the event is published, so a failed wake or emit leaves
//! it unset and the next tick retries instead of going quiet for ten minutes.
//! The cooldown clock is process-local (a restart forgets the cooldown, never
//! the message).

use oc_store::{Delivery, MessageQuery, NewInbox, Order};
use oc_types::payload::EventPayload;
use oc_types::{InboxKind, MessageId, SessionId, now_millis};
use serde::Serialize;
use serde_json::{Value, json};

use crate::compaction::CompactionConfig;
use crate::runtime::{Runtime, RuntimeError};

/// Wire spelling of an inbox kind, produced by the enum's own serde representation.
///
/// [`NewInbox`] takes a raw string, so a hand-copied literal could silently
/// drift from [`InboxKind`]. Serializing the enum keeps the advice row's
/// `type` exactly what [`oc_types::InboxItem`] parses.
fn inbox_kind(kind: InboxKind) -> String {
    match serde_json::to_value(kind).expect("inbox kind serializes to JSON") {
        Value::String(spelling) => spelling,
        other => panic!("inbox kind must serialize to a JSON string, got {other}"),
    }
}

/// Minimum gap between two advice rows for the same session.
pub(crate) const ADVICE_COOLDOWN_MS: i64 = 600_000;
/// Fraction of the ceiling that counts as pressure.
pub(crate) const ADVICE_THRESHOLD_PERCENT: u64 = 70;
/// Cooldown map cap; crossing it clears the map instead of growing forever.
const MAX_COOLDOWN_ENTRIES: usize = 1024;

/// Inputs to the pure advice gate.
///
/// Deliberately excludes the compaction `trigger`: the gate compares the
/// estimate against 70% of the ceiling, and the trigger only appears in the
/// human-readable message (see [`Runtime::enqueue_advice`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdviceInput {
    pub estimated: u64,
    pub ceiling: u64,
    /// `compaction.auto`. The turn loop's guard is armed.
    pub auto: bool,
    /// `compaction.ceiling_tokens.is_some()`. The turn loop arms its guard on
    /// an explicit ceiling too, so advising would be redundant and the message
    /// could not truthfully claim the session is unprotected.
    pub ceiling_set: bool,
    pub compaction_pending: bool,
    pub last_advised_ms: Option<i64>,
    pub now_ms: i64,
}

/// Why the adviser stayed silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdviceSkip {
    /// The runtime compacts automatically: `compaction.auto` is on or an
    /// explicit `compaction.ceiling_tokens` arms the turn loop's guard.
    Auto,
    CompactionPending,
    Cooldown,
    /// A zero ceiling cannot express pressure; never advise.
    NoCeiling,
    UnderThreshold,
}

/// Advice decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Advice {
    Suggest,
    Skip(AdviceSkip),
}

/// Pure gate: no clock, no I/O.
pub(crate) fn decide(input: &AdviceInput) -> Advice {
    if input.auto || input.ceiling_set {
        return Advice::Skip(AdviceSkip::Auto);
    }
    if input.compaction_pending {
        return Advice::Skip(AdviceSkip::CompactionPending);
    }
    if input.ceiling == 0 {
        return Advice::Skip(AdviceSkip::NoCeiling);
    }
    if let Some(last) = input.last_advised_ms
        && input.now_ms.saturating_sub(last) < ADVICE_COOLDOWN_MS
    {
        return Advice::Skip(AdviceSkip::Cooldown);
    }
    let threshold = input.ceiling.saturating_mul(ADVICE_THRESHOLD_PERCENT) / 100;
    if input.estimated < threshold {
        return Advice::Skip(AdviceSkip::UnderThreshold);
    }
    Advice::Suggest
}

/// Durable `observer.advised` payload (docs/17-interfaces.md).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ObserverAdvised {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub estimate_tokens: u64,
    pub ceiling_tokens: u64,
    pub trigger_tokens: u64,
}

impl EventPayload for ObserverAdvised {
    const TYPE: &'static str = "observer.advised";
    const DURABLE: bool = true;
    const VERSION: u32 = 1;
}

impl Runtime {
    /// Run the adviser once over the currently active tree sessions.
    ///
    /// Returns whether any advice row was enqueued, so the observer ticker can
    /// keep running while pressure settles.
    pub(crate) async fn advise_context_pressure(&self, sessions: &[String]) -> bool {
        self.advise_context_pressure_at(sessions, now_millis())
            .await
    }

    /// [`Self::advise_context_pressure`] with an explicit clock, so tests can
    /// exercise the cooldown boundary without sleeping.
    ///
    /// This is the only clock injection point in the adviser: the production
    /// tick path always passes [`now_millis`], and the gate itself stays pure.
    #[doc(hidden)]
    pub async fn advise_context_pressure_at(&self, sessions: &[String], now: i64) -> bool {
        let compaction = CompactionConfig::from_merged(self.inner.config.config.merged());
        let auto = compaction.auto;
        let ceiling_set = compaction.ceiling_tokens.is_some();
        let mut advised = false;
        for session in sessions {
            let pressure = match self.context_pressure(session).await {
                Ok(Some(pressure)) => pressure,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(session = %session, %error, "compaction advice: pressure lookup failed");
                    continue;
                }
            };
            let compaction_pending = match self.compaction_pending(session).await {
                Ok(pending) => pending,
                Err(error) => {
                    tracing::warn!(session = %session, %error, "compaction advice: pending check failed");
                    continue;
                }
            };
            let last_advised_ms = self
                .inner
                .observer_advised
                .lock()
                .expect("observer advice clock")
                .get(session)
                .copied();
            let input = AdviceInput {
                estimated: pressure.estimated,
                ceiling: pressure.ceiling,
                auto,
                ceiling_set,
                compaction_pending,
                last_advised_ms,
                now_ms: now,
            };
            if decide(&input) != Advice::Suggest {
                continue;
            }
            if let Err(error) = self.enqueue_advice(session, &pressure, now).await {
                tracing::warn!(session = %session, %error, "compaction advice enqueue failed");
                continue;
            }
            advised = true;
        }
        advised
    }

    /// A session has compaction work queued or running.
    async fn compaction_pending(&self, session: &str) -> Result<bool, RuntimeError> {
        let inbox = self.inner.config.store.list_inbox(session).await?;
        if inbox
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
        {
            return Ok(true);
        }
        let page = self
            .inner
            .config
            .store
            .list_messages(MessageQuery {
                session_id: session.to_owned(),
                limit: 20,
                order: Order::Desc,
                cursor: None,
                kind: Some("compaction".to_owned()),
            })
            .await?;
        Ok(page
            .items
            .iter()
            .any(|message| message.get("status").and_then(Value::as_str) == Some("running")))
    }

    /// Enqueue the durable advice row, wake the session, publish the event,
    /// then stamp the cooldown.
    ///
    /// The stamp is last on purpose: a failed wake or emit must not silence the
    /// adviser for ten minutes while the docs promise row and event travel
    /// together. Both failures leave the cooldown unset so the next tick
    /// retries; the durable row itself is written first and never rolled back.
    async fn enqueue_advice(
        &self,
        session: &str,
        pressure: &crate::turn::ContextPressure,
        now: i64,
    ) -> Result<(), RuntimeError> {
        let text = format!(
            "Context pressure: ~{} / {} tokens (advice at 70% of ceiling, compaction trigger {}). \
             This is advice, not a command: consider calling compact_context with a short reason, \
             or finish and hand off. Auto-compaction is off and no compaction ceiling is \
             configured.",
            pressure.estimated, pressure.ceiling, pressure.trigger
        );
        let payload = json!({
            "text": text,
            "metadata": {
                "observer": true,
                "advice": "compact",
                "estimateTokens": pressure.estimated,
                "ceilingTokens": pressure.ceiling,
                "triggerTokens": pressure.trigger,
            },
            "time": { "created": now },
        });
        self.enqueue_and_emit(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: session.to_owned(),
            kind: inbox_kind(InboxKind::Observer),
            payload,
            delivery: Delivery::Steer,
        })
        .await?;
        if let Err(error) = self.notify_local(session).await {
            tracing::warn!(
                session = %session,
                %error,
                "compaction advice: wake failed; leaving the cooldown unset so the next tick retries"
            );
            return Err(error);
        }
        let directory = self
            .inner
            .config
            .store
            .get_session(session)
            .await?
            .and_then(|value| {
                value
                    .pointer("/location/directory")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        if let Err(error) = self
            .emit(
                session,
                &directory,
                ObserverAdvised {
                    session_id: SessionId::from_wire(session.to_owned()),
                    estimate_tokens: pressure.estimated,
                    ceiling_tokens: pressure.ceiling,
                    trigger_tokens: pressure.trigger,
                },
            )
            .await
        {
            tracing::warn!(
                session = %session,
                %error,
                "compaction advice: observer.advised emit failed; leaving the cooldown unset"
            );
            return Ok(());
        }
        let mut clock = self
            .inner
            .observer_advised
            .lock()
            .expect("observer advice clock");
        if clock.len() >= MAX_COOLDOWN_ENTRIES {
            clock.clear();
        }
        clock.insert(session.to_owned(), now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> AdviceInput {
        AdviceInput {
            estimated: 80,
            ceiling: 100,
            auto: false,
            ceiling_set: false,
            compaction_pending: false,
            last_advised_ms: None,
            now_ms: 1_000_000,
        }
    }

    #[test]
    fn advice_row_uses_the_observer_inbox_kind() {
        let produced = json!(inbox_kind(InboxKind::Observer));
        assert_eq!(produced, serde_json::to_value(InboxKind::Observer).unwrap());
        // The wire value is part of the persisted row shape: a serde rename in
        // oc-types must fail a gate here, not silently re-type existing rows.
        assert_eq!(produced, json!("observer"));
    }

    #[test]
    fn suggests_at_or_above_seventy_percent() {
        assert_eq!(decide(&input()), Advice::Suggest);
        let mut at_threshold = input();
        at_threshold.estimated = 70;
        assert_eq!(decide(&at_threshold), Advice::Suggest);
        let mut below = input();
        below.estimated = 69;
        assert_eq!(decide(&below), Advice::Skip(AdviceSkip::UnderThreshold));
    }

    #[test]
    fn skips_when_auto_compaction_is_on() {
        let mut auto = input();
        auto.auto = true;
        assert_eq!(decide(&auto), Advice::Skip(AdviceSkip::Auto));
    }

    #[test]
    fn skips_when_an_explicit_ceiling_arms_the_guard() {
        let mut configured = input();
        configured.ceiling_set = true;
        assert_eq!(decide(&configured), Advice::Skip(AdviceSkip::Auto));
        // Both conditions together land on the same variant: the runtime
        // compacts either way, so one reason covers the armed guard.
        let mut both = input();
        both.auto = true;
        both.ceiling_set = true;
        assert_eq!(decide(&both), Advice::Skip(AdviceSkip::Auto));
    }

    #[test]
    fn advises_only_without_automatic_protection() {
        let mut unprotected = input();
        unprotected.auto = false;
        unprotected.ceiling_set = false;
        assert_eq!(decide(&unprotected), Advice::Suggest);
    }

    #[test]
    fn skips_while_a_compaction_is_pending() {
        let mut pending = input();
        pending.compaction_pending = true;
        assert_eq!(
            decide(&pending),
            Advice::Skip(AdviceSkip::CompactionPending)
        );
    }

    #[test]
    fn cooldown_boundary_is_exclusive() {
        let mut recent = input();
        recent.last_advised_ms = Some(1_000_000 - 599_999);
        assert_eq!(decide(&recent), Advice::Skip(AdviceSkip::Cooldown));
        let mut exact = input();
        exact.last_advised_ms = Some(1_000_000 - ADVICE_COOLDOWN_MS);
        assert_eq!(decide(&exact), Advice::Suggest);
    }

    #[test]
    fn a_zero_ceiling_never_advises() {
        let mut zero = input();
        zero.ceiling = 0;
        zero.estimated = 0;
        assert_eq!(decide(&zero), Advice::Skip(AdviceSkip::NoCeiling));
        let mut huge = input();
        huge.ceiling = u64::MAX;
        huge.estimated = u64::MAX;
        assert_eq!(decide(&huge), Advice::Suggest);
    }
}
