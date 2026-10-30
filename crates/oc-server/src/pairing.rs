//! Single-use pairing codes, 5 minute TTL, in-memory.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;

const TTL: Duration = Duration::from_secs(300);
const MAX_ENTRIES: usize = 1000;

#[derive(Debug)]
pub struct PairingStore {
    inner: Mutex<HashMap<String, Instant>>,
}

impl Default for PairingStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Issue a new code, pruning expired entries first.
    pub fn issue(&self) -> String {
        let mut bytes = [0_u8; 16];
        rand::rng().fill(&mut bytes);
        let code = URL_SAFE_NO_PAD.encode(bytes);
        let now = Instant::now();

        let mut map = self.inner.lock().expect("pairing mutex poisoned");
        map.retain(|_, expires| *expires > now);
        if map.len() >= MAX_ENTRIES
            && let Some(oldest) = map.keys().next().cloned()
        {
            map.remove(&oldest);
        }
        map.insert(code.clone(), now + TTL);
        code
    }

    /// Consume a code. Returns false if unknown or expired.
    pub fn redeem(&self, code: &str) -> bool {
        let now = Instant::now();
        let mut map = self.inner.lock().expect("pairing mutex poisoned");
        match map.remove(code) {
            Some(expires) => expires > now,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_single_use() {
        let store = PairingStore::new();
        let code = store.issue();
        assert!(store.redeem(&code));
        assert!(!store.redeem(&code));
    }

    #[test]
    fn unknown_code_is_rejected() {
        assert!(!PairingStore::new().redeem("nope"));
    }
}
