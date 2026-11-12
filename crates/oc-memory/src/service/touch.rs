//! Usage accounting for injected memories.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::MemoryError;

use super::MemoryService;

impl MemoryService {
    /// Bumps `uses`/`last_used` for injected memories; call once at turn end.
    pub async fn touch(&self, ids: &[String]) -> Result<(), MemoryError> {
        self.store.touch_memories(ids, now_ms()).await?;
        Ok(())
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}
