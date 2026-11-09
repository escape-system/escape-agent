//! Identifier generation, compatible with the upstream 26-character format.
//!
//! Upstream: 6 bytes of `(unix_ms * 0x1000 + counter)` rendered as 12 hex
//! characters, followed by 14 base62 characters. Sessions use the bitwise
//! complement so newer sessions sort first; events and messages do not.

use std::fmt;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn raw(descending: bool) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed) & 0x0fff;
    let value = millis.wrapping_mul(0x1000).wrapping_add(counter);
    let value = if descending { !value } else { value };

    let mut out = String::with_capacity(26);
    write!(out, "{:012x}", value & 0xffff_ffff_ffff).expect("writing to a String cannot fail");

    let mut rng = rand::rng();
    for _ in 0..14 {
        let idx = rng.random_range(0..BASE62.len());
        out.push(BASE62[idx] as char);
    }
    out
}

macro_rules! id_type {
    ($name:ident, $prefix:literal, $constructor:ident, $descending:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Generate a new identifier with this type's prefix.
            #[must_use]
            pub fn new() -> Self {
                Self(format!("{}{}", $prefix, raw($descending)))
            }

            /// Construct from an already-prefixed wire value.
            #[must_use]
            pub fn from_wire(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Borrow the wire representation.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume into the wire representation.
            #[must_use]
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

id_type!(EventId, "evt_", new, false);
id_type!(SessionId, "ses_", new, true);
id_type!(MessageId, "msg_", new, false);
id_type!(PermissionRequestId, "per_", new, false);
id_type!(SavedPermissionId, "psv_", new, true);
id_type!(ProjectId, "prj_", new, true);
id_type!(ObservationId, "obs_", new, false);
id_type!(GoalId, "gol_", new, true);
id_type!(MemoryId, "mem_", new, true);
id_type!(ContextBoxId, "box_", new, false);
id_type!(LaneId, "lan_", new, false);

/// Message ids for event-backed messages are derived by replacing the prefix.
#[must_use]
pub fn message_id_from_event(event: &EventId) -> MessageId {
    let raw = event
        .as_str()
        .strip_prefix("evt_")
        .unwrap_or(event.as_str());
    MessageId::from_wire(format!("msg_{raw}"))
}

/// Stable project id derived from a canonical directory (FNV-1a, 16 hex).
#[must_use]
pub fn project_id_from_directory(directory: &str) -> ProjectId {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in directory.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    ProjectId::from_wire(format!("prj_{hash:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_26_chars_after_prefix() {
        assert_eq!(SessionId::new().as_str().len(), 30);
        assert_eq!(EventId::new().as_str().len(), 30);
        assert_eq!(MessageId::new().as_str().len(), 30);
        assert!(SessionId::new().as_str().starts_with("ses_"));
        assert!(EventId::new().as_str().starts_with("evt_"));
        assert!(ContextBoxId::new().as_str().starts_with("box_"));
    }

    #[test]
    fn ids_are_unique() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn message_id_derives_from_event() {
        let event = EventId::from_wire("evt_0123456789abABCDEFGHIJKLMN");
        assert_eq!(
            message_id_from_event(&event).as_str(),
            "msg_0123456789abABCDEFGHIJKLMN"
        );
    }

    #[test]
    fn project_id_is_stable() {
        assert_eq!(
            project_id_from_directory("C:/work"),
            project_id_from_directory("C:/work")
        );
        assert_ne!(
            project_id_from_directory("C:/work"),
            project_id_from_directory("C:/other")
        );
    }
}
