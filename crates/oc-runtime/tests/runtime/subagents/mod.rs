//! Subagent lifecycle: foreground/background spawns, limits, talk, digest,
//! key rotation, partitions, roster barriers, and coordinator routing.

use std::sync::Arc;

use oc_store::{MessageQuery, Order, Store};
use serde_json::Value;

mod coordinator;
mod lifecycle;
mod messaging;
mod models_keys;
mod partitions_roster;

pub(super) async fn session_messages(store: &Store, session: &str) -> Vec<Value> {
    let mut cursor = None;
    let mut out = Vec::new();
    loop {
        let page = store
            .list_messages(MessageQuery {
                session_id: session.to_owned(),
                limit: 200,
                order: Order::Asc,
                cursor,
                kind: None,
            })
            .await
            .expect("messages");
        out.extend(page.items);
        match page.cursor.next {
            Some(next) => cursor = Some(next),
            None => return out,
        }
    }
}

#[allow(dead_code)]
fn _arc_marker(_: Arc<Store>) {}
