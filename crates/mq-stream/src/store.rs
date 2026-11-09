//! Consumer offsets (`StoreOffset`/`QueryOffset`) and publisher sequences, persisted in the
//! broker's metadata store.
//!
//! Stores are coalesced: the latest value per key is written by a background flusher, and
//! reads see pending values first, so clients can store offsets after every chunk without a
//! metadata write each time. Entries go away with their stream.

use mq_core::meta::{self, MetaStore};
use parking_lot::Mutex;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Notify;
use tracing::warn;

/// Metadata kind of stored consumer offsets: key `[vhost, stream, reference]`, `{"offset": n}`.
pub const OFFSET_KIND: &str = mq_core::meta::kind::STREAM_OFFSET;
/// Metadata kind of publisher sequences: key `[vhost, stream, reference]`, `{"sequence": n}`.
pub const PUBLISHER_KIND: &str = "stream_publisher";

#[derive(Default)]
struct State {
    /// (kind, key) → value not yet handed to the flusher.
    pending: HashMap<(&'static str, String), u64>,
    /// Being written right now.
    inflight: HashMap<(&'static str, String), u64>,
    /// Streams that have entries, so deleting other queues costs nothing.
    streams: HashSet<(String, String)>,
}

pub struct Store {
    meta: Arc<dyn MetaStore>,
    state: Mutex<State>,
    wake: Notify,
}

impl Store {
    pub async fn open(meta: Arc<dyn MetaStore>) -> Arc<Store> {
        let mut st = State::default();
        for kind in [OFFSET_KIND, PUBLISHER_KIND] {
            match meta.list(kind).await {
                Ok(rows) => {
                    for (k, _) in rows {
                        if let [vh, s, ..] = meta::split_key(&k)[..] {
                            st.streams.insert((vh.to_string(), s.to_string()));
                        }
                    }
                }
                Err(e) => warn!(error = %e, kind, "listing stream offsets failed"),
            }
        }
        let store = Arc::new(Store { meta, state: Mutex::new(st), wake: Notify::new() });
        tokio::spawn(flusher(Arc::downgrade(&store)));
        store
    }

    fn put(&self, kind: &'static str, vhost: &str, stream: &str, reference: &str, v: u64) {
        let mut st = self.state.lock();
        st.pending.insert((kind, meta::key(&[vhost, stream, reference])), v);
        st.streams.insert((vhost.to_string(), stream.to_string()));
        drop(st);
        self.wake.notify_one();
    }

    async fn get(&self, kind: &'static str, vhost: &str, stream: &str, reference: &str, field: &str) -> Option<u64> {
        let k = (kind, meta::key(&[vhost, stream, reference]));
        {
            let st = self.state.lock();
            if let Some(v) = st.pending.get(&k).or_else(|| st.inflight.get(&k)) {
                return Some(*v);
            }
        }
        match self.meta.get(kind, &k.1).await {
            Ok(v) => v.and_then(|v| v.get(field).and_then(|x| x.as_u64())),
            Err(e) => {
                warn!(error = %e, "reading stream offset failed");
                None
            }
        }
    }

    pub fn store_offset(&self, vhost: &str, stream: &str, reference: &str, offset: u64) {
        self.put(OFFSET_KIND, vhost, stream, reference, offset);
    }

    pub async fn query_offset(&self, vhost: &str, stream: &str, reference: &str) -> Option<u64> {
        self.get(OFFSET_KIND, vhost, stream, reference, "offset").await
    }

    pub fn store_sequence(&self, vhost: &str, stream: &str, reference: &str, seq: u64) {
        self.put(PUBLISHER_KIND, vhost, stream, reference, seq);
    }

    pub async fn sequence(&self, vhost: &str, stream: &str, reference: &str) -> u64 {
        self.get(PUBLISHER_KIND, vhost, stream, reference, "sequence").await.unwrap_or(0)
    }

    /// Forget everything stored for a deleted stream.
    pub async fn stream_deleted(&self, vhost: &str, stream: &str) {
        let prefix = meta::key(&[vhost, stream, ""]);
        {
            let mut st = self.state.lock();
            if !st.streams.remove(&(vhost.to_string(), stream.to_string())) {
                return;
            }
            st.pending.retain(|(_, k), _| !k.starts_with(&prefix));
        }
        for kind in [OFFSET_KIND, PUBLISHER_KIND] {
            let Ok(rows) = self.meta.list(kind).await else { continue };
            for (k, _) in rows.into_iter().filter(|(k, _)| k.starts_with(&prefix)) {
                if let Err(e) = self.meta.delete(kind, &k).await {
                    warn!(error = %e, "deleting stream offset failed");
                }
            }
        }
    }

    /// Write everything pending now.
    pub async fn flush(&self) {
        let batch = {
            let mut st = self.state.lock();
            let batch = std::mem::take(&mut st.pending);
            st.inflight.extend(batch.iter().map(|(k, v)| (k.clone(), *v)));
            batch
        };
        for ((kind, key), v) in &batch {
            let value = if *kind == OFFSET_KIND { json!({ "offset": v }) } else { json!({ "sequence": v }) };
            if let Err(e) = self.meta.put(kind, key, &value).await {
                warn!(error = %e, "storing stream offset failed");
            }
        }
        let mut st = self.state.lock();
        for (k, v) in batch {
            if st.inflight.get(&k) == Some(&v) {
                st.inflight.remove(&k);
            }
        }
    }
}

async fn flusher(store: std::sync::Weak<Store>) {
    loop {
        let Some(s) = store.upgrade() else { return };
        // Wait for work without keeping the store alive.
        let notified = {
            let s = s.clone();
            async move { s.wake.notified().await }
        };
        drop(s);
        if tokio::time::timeout(std::time::Duration::from_secs(5), notified).await.is_err() {
            continue;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let Some(s) = store.upgrade() else { return };
        s.flush().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mq_core::meta::MemMeta;

    #[tokio::test]
    async fn offsets_roundtrip_and_cleanup() {
        let meta = MemMeta::new();
        let s = Store::open(meta.clone()).await;
        assert_eq!(s.query_offset("/", "s", "app").await, None);
        s.store_offset("/", "s", "app", 41);
        s.store_offset("/", "s", "app", 42);
        assert_eq!(s.query_offset("/", "s", "app").await, Some(42));
        s.flush().await;
        let s2 = Store::open(meta.clone()).await;
        assert_eq!(s2.query_offset("/", "s", "app").await, Some(42));
        s2.store_sequence("/", "s", "p1", 9);
        s2.flush().await;
        assert_eq!(s2.sequence("/", "s", "p1").await, 9);
        s2.stream_deleted("/", "s").await;
        assert_eq!(s2.query_offset("/", "s", "app").await, None);
        assert_eq!(s2.sequence("/", "s", "p1").await, 0);
    }
}
