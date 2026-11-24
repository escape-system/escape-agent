//! `session.request.envelope` — the log-only record of one model request.
//!
//! The model request is **not** a pure function of the session log. The turn
//! loop derives the base messages from `session_message` rows
//! ([`crate`-external `messages_from_store`]) and then appends volatile,
//! model-visible splices that cannot be recomputed from persisted state: the
//! memory supplement is a diff against an in-memory pinned set, the verification
//! nudge scans the live filesystem, and the stall notice is turn-local.
//!
//! This payload records the *envelope* of the request beside the transcript
//! instead of inside it. It writes **no** `session_message` row, so it cannot
//! perturb the compaction boundary, `keepID`, token estimates or the memory
//! recall query. A replaying consumer rebuilds the exact request from the rows
//! (`base_messages` + `base_digest`) plus the recorded `injections`, which are
//! replayed verbatim.
//!
//! # Digest
//!
//! `oc-types` deliberately carries no hash dependency, so digests are
//! dependency-free **FNV-1a 64-bit** values rendered as 16 lowercase hex
//! characters: offset basis `0xcbf29ce484222325`, prime `0x100000001b3`, one
//! XOR-and-multiply step per byte, wrapping in `u64`. This is a change-detector
//! for request equality, not a cryptographic hash.
//!
//! # Canonical message text
//!
//! [`digest_sequence`] hashes a sequence of strings in order, folding each
//! element's byte length in first so that `["ab", "c"]` and `["a", "bc"]` differ.
//! The turn loop feeds it one [`canonical_message`] string per base message, and
//! [`canonical_message`] joins one rendering per content part with `\u{1f}`
//! behind the role name and `\u{1e}`. The per-part renderings are:
//!
//! | part | rendering |
//! |---|---|
//! | text | the text itself |
//! | image | `[image <media_type> <data.len()> bytes]` |
//! | document | `[document <media_type> <data.len()> bytes <filename or ->]` |
//! | tool call | `[tool_call <id> <name> <input json>]` |
//! | tool result | `[tool_result <tool_call_id> <name or -> <content> error=<bool>]` |
//!
//! A consumer that recomputes the base messages from the durable rows and
//! applies the same rendering reproduces `base_digest` exactly; that is what the
//! runtime regression test asserts.

use serde::{Deserialize, Serialize};

use crate::id::{MessageId, SessionId};

use super::EventPayload;

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Canonical digest of one message: its role, then each rendered part, in order.
///
/// `parts` are the per-part renderings documented at the module level. An empty
/// message still contributes its role, so a message cannot hash to nothing.
#[must_use]
pub fn canonical_message(role: &str, parts: &[String]) -> String {
    let mut out =
        String::with_capacity(role.len() + 1 + parts.iter().map(String::len).sum::<usize>());
    out.push_str(role);
    out.push('\u{1e}');
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            out.push('\u{1f}');
        }
        out.push_str(part);
    }
    out
}

/// FNV-1a 64-bit digest of `text` as 16 lowercase hex characters.
#[must_use]
pub fn digest(text: &str) -> String {
    hex_16(fold(FNV_OFFSET, text.as_bytes()))
}

/// FNV-1a 64-bit digest of a sequence of strings, folded in order.
///
/// Each element's byte length is folded in before its bytes, so concatenation
/// ambiguity cannot make two different sequences collide. An empty sequence
/// digests to the offset basis.
#[must_use]
pub fn digest_sequence<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut hash = FNV_OFFSET;
    for part in parts {
        hash = fold(hash, &(part.len() as u64).to_le_bytes());
        hash = fold(hash, part.as_bytes());
    }
    hex_16(hash)
}

fn fold(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn hex_16(hash: u64) -> String {
    format!("{hash:016x}")
}

/// One model-visible input appended on top of the derived messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestInjection {
    /// `memory-supplement`, `output-continuation`, `verify-nudge`,
    /// `stall-notice`, `context-pressure`, `step-budget` or `max-steps`.
    ///
    /// `final-call` names a tool-less final call that carries its own text;
    /// today the tool-less call is expressed by the `max-steps` text, so the
    /// `max-steps` kind carries it.
    pub kind: String,
    /// The exact text appended to the request.
    pub text: String,
}

impl RequestInjection {
    /// Record one appended splice.
    #[must_use]
    pub fn new(kind: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            text: text.into(),
        }
    }
}

/// `session.request.envelope` — durable, log-only.
///
/// It is emitted once per assistant row, after `session.step.started` commits and
/// before the first byte of the provider response is requested. It is not a
/// message: no `session_message` row is written for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRequestEnvelope {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    /// Zero-based step ordinal inside the turn.
    pub step: u32,
    /// Provider that served the request.
    pub provider: String,
    /// Model that served the request.
    pub model: String,
    /// Tool names in the exact order they were sent.
    pub tools: Vec<String>,
    /// Digest of the joined system text; the text itself is not duplicated.
    pub system_digest: String,
    /// Number of messages derived from `session_message` before any splice.
    pub base_messages: u32,
    /// Digest over the base message texts, in order (see [`canonical_message`]).
    pub base_digest: String,
    /// The splices appended on top of the base messages, in request order.
    ///
    /// Omitted when empty, never `null`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub injections: Vec<RequestInjection>,
    /// Creation time in epoch milliseconds.
    pub created: i64,
}

impl_event_payload!(SessionRequestEnvelope, "session.request.envelope", true, 1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_digest_matches_the_published_vectors() {
        // Well-known FNV-1a 64-bit vectors.
        assert_eq!(digest(""), "cbf29ce484222325");
        assert_eq!(digest("a"), "af63dc4c8601ec8c");
        assert_eq!(digest("abc").len(), 16);
        assert_ne!(digest("abc"), digest("abd"));
        assert!(
            digest("abc")
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "lowercase hex only"
        );
    }

    #[test]
    fn digest_sequence_is_order_and_length_sensitive() {
        assert_eq!(digest_sequence([]), "cbf29ce484222325");
        assert_ne!(
            digest_sequence(["ab", "c"]),
            digest_sequence(["a", "bc"]),
            "length folding keeps the sequence unambiguous"
        );
        assert_eq!(digest_sequence(["a", "b"]), digest_sequence(["a", "b"]));
        assert_ne!(digest_sequence(["a", "b"]), digest_sequence(["b", "a"]));
    }

    #[test]
    fn canonical_message_carries_the_role_even_without_parts() {
        assert_eq!(canonical_message("user", &[]), "user\u{1e}");
        assert_eq!(
            canonical_message("user", &["first".to_owned(), "second".to_owned()]),
            "user\u{1e}first\u{1f}second"
        );
        assert_ne!(
            canonical_message("user", &[]),
            canonical_message("assistant", &[])
        );
    }

    #[test]
    fn envelope_wire_shape_matches_the_contract() {
        let envelope = SessionRequestEnvelope {
            session_id: SessionId::from_wire("ses_0123456789abABCDEFGHIJKLMN"),
            assistant_message_id: MessageId::from_wire("msg_0123456789abABCDEFGHIJKLMN"),
            step: 3,
            provider: "test".to_owned(),
            model: "alpha".to_owned(),
            tools: vec!["read".to_owned(), "grep".to_owned()],
            system_digest: "0123456789abcdef".to_owned(),
            base_messages: 7,
            base_digest: "fedcba9876543210".to_owned(),
            injections: Vec::new(),
            created: 1_700_000_000_000,
        };
        let data = envelope.data();
        assert_eq!(
            data,
            serde_json::json!({
                "sessionID": "ses_0123456789abABCDEFGHIJKLMN",
                "assistantMessageID": "msg_0123456789abABCDEFGHIJKLMN",
                "step": 3,
                "provider": "test",
                "model": "alpha",
                "tools": ["read", "grep"],
                "systemDigest": "0123456789abcdef",
                "baseMessages": 7,
                "baseDigest": "fedcba9876543210",
                "created": 1_700_000_000_000_i64
            }),
            "empty injections are omitted, never null"
        );
        assert!(!data.to_string().contains("null"));

        let with_injections = SessionRequestEnvelope {
            injections: vec![
                RequestInjection::new("step-budget", "one step remains"),
                RequestInjection::new("max-steps", "answer now"),
            ],
            ..envelope
        };
        let data = with_injections.data();
        assert_eq!(data["injections"][0]["kind"], "step-budget");
        assert_eq!(data["injections"][0]["text"], "one step remains");
        assert_eq!(data["injections"][1]["kind"], "max-steps");
        let parsed: SessionRequestEnvelope =
            serde_json::from_value(data).expect("round-trips through the wire");
        assert_eq!(parsed, with_injections);
        assert_eq!(
            <SessionRequestEnvelope as EventPayload>::TYPE,
            "session.request.envelope"
        );
        assert_eq!(<SessionRequestEnvelope as EventPayload>::VERSION, 1);
        const { assert!(<SessionRequestEnvelope as EventPayload>::DURABLE) };
    }
}
