//! Session inbox items and delivery modes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::{MessageId, SessionId};

/// `Session.Inbox.Delivery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    Steer,
    Queue,
}

/// `Session.Inbox.Item.type` plus this project's additive kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InboxKind {
    User,
    Synthetic,
    Compaction,
    Move,
    /// A message pushed into a subagent session by a peer (docs/05).
    Peer,
    /// An observer instruction (docs/08).
    Observer,
}

/// `Session.Inbox.Info`: an enqueued prompt awaiting delivery.
///
/// `payload` stays a raw JSON value; its shape is selected by [`Self::kind`]
/// (`Session.Inbox.UserPayload`, `SyntheticPayload`, `CompactionPayload`,
/// `MovePayload`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxItem {
    pub id: MessageId,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub time: InboxTime,
    #[serde(rename = "type")]
    pub kind: InboxKind,
    pub payload: Value,
    pub delivery: Delivery,
}

/// `Session.Inbox.Info.time`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxTime {
    pub created: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_inbox_item_round_trips() {
        let fixture = serde_json::json!({
            "id": "msg_0123456789abABCDEFGHIJKLMN",
            "sessionID": "ses_0123456789abABCDEFGHIJKLMN",
            "time": {"created": 1_730_000_000_000_i64},
            "type": "user",
            "payload": {
                "text": "hello",
                "files": [{
                    "data": "aGk=",
                    "mime": "text/plain",
                    "source": {"type": "inline"}
                }],
                "metadata": {"draft": true}
            },
            "delivery": "steer"
        });
        let parsed: InboxItem = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(parsed.kind, InboxKind::User);
        assert_eq!(parsed.delivery, Delivery::Steer);
        assert_eq!(serde_json::to_value(&parsed).unwrap(), fixture);
        assert!(!serde_json::to_string(&parsed).unwrap().contains("null"));
    }

    #[test]
    fn compaction_item_round_trips_with_empty_payload() {
        let fixture = serde_json::json!({
            "id": "msg_0123456789abABCDEFGHIJKLMN",
            "sessionID": "ses_0123456789abABCDEFGHIJKLMN",
            "time": {"created": 1},
            "type": "compaction",
            "payload": {},
            "delivery": "queue"
        });
        let parsed: InboxItem = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), fixture);
    }

    #[test]
    fn additive_kinds_serialize_lowercase() {
        for (kind, wire) in [
            (InboxKind::Peer, "peer"),
            (InboxKind::Observer, "observer"),
            (InboxKind::Move, "move"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), wire);
        }
    }
}
