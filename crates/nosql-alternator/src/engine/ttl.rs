//! Alternator TTL: the expiration scanner (`alternator_ttl_period_in_seconds`).
//!
//! Every period, each table with a TTL attribute is scanned and the items whose attribute is a
//! number of seconds since the epoch that has passed (and is less than five years old, as
//! DynamoDB ignores older ones) are deleted, under the item's lock and only if still expired.
//! The deletion reaches GSIs, LSIs and the stream like any delete; its stream record carries
//! DynamoDB's `userIdentity` of the TTL service (ScyllaDB's issue #11523), which the log
//! records by setting `cdc$ttl` to 0 on the deletion's row.

use std::sync::{Arc, Weak};
use std::time::Duration;

use futures::StreamExt;
use nosql_storage::read::RangeRead;

use super::EngineBackend;
use super::items::{now_secs, row_item};
use crate::backend::{Backend, Mutation, PrimaryKey, WriteFn};
use crate::error::Result;
use crate::json::Json;
use crate::number::Number;
use crate::value::Item;

/// DynamoDB doesn't expire items whose expiration time is more than five years old.
const MAX_AGE_SECS: i64 = 5 * 365 * 86_400;

/// Whether `item` has expired at `now_ms` by attribute `attr`.
pub fn expired(item: &Item, attr: &str, now_ms: i64) -> bool {
    let Some(n) = item
        .get(attr)
        .and_then(crate::storage::single_member)
        .filter(|(t, _)| *t == "N")
        .and_then(|(_, v)| v.as_str())
        .and_then(|s| Number::parse(s).ok())
    else {
        return false;
    };
    let at = |ms: i64| {
        Number::parse(&format!(
            "{}.{:03}",
            ms.div_euclid(1000),
            ms.rem_euclid(1000)
        ))
    };
    let (Ok(now), Ok(oldest)) = (at(now_ms), at(now_ms - MAX_AGE_SECS * 1000)) else {
        return false;
    };
    n < now && n > oldest
}

impl EngineBackend {
    /// One expiration pass over every table with TTL enabled; returns the items deleted.
    /// `metrics` counts it (`scylla_expiration_scan_passes`, `scan_table`, `items_deleted`).
    pub async fn expire(
        &self,
        now_ms: i64,
        metrics: Option<&crate::metrics::Metrics>,
    ) -> Result<u64> {
        let mut deleted = 0;
        for name in self.list_tables().await? {
            let Some(schema) = self.table(&name).await? else {
                continue;
            };
            let Some(attr) = schema.ttl_attribute.clone() else {
                continue;
            };
            let Some(t) = self.base(&name) else { continue };
            if let Some(m) = metrics {
                m.add("scylla_expiration_scan_table", &[], 1.0);
            }
            let st = self
                .database()
                .storage_table(&t)
                .map_err(super::engine_error)?;
            let mut stream = st.scan(RangeRead::all(now_secs()));
            let mut keys = Vec::new();
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else { break };
                for row in &chunk.rows {
                    let item = row_item(&t, &chunk.key.key, row);
                    if expired(&item, &attr, now_ms) {
                        keys.push(PrimaryKey {
                            hash: item
                                .get(&schema.key.hash.name)
                                .cloned()
                                .unwrap_or(Json::Null),
                            range: schema
                                .key
                                .range
                                .as_ref()
                                .and_then(|r| item.get(&r.name).cloned()),
                        });
                    }
                }
            }
            for key in keys {
                let attr = attr.clone();
                let f: WriteFn = Box::new(move |old: Option<&Item>| {
                    Ok(old
                        .filter(|o| expired(o, &attr, now_ms))
                        .map(|_| Mutation::Delete))
                });
                let exact = self
                    .exact_streams
                    .load(std::sync::atomic::Ordering::Relaxed);
                let out =
                    super::items::write_marked(self.database(), &schema, &t, &key, f, exact, true)
                        .await?;
                if out.old.is_some() && out.new.is_none() {
                    deleted += 1;
                }
            }
        }
        if let Some(m) = metrics {
            m.add("scylla_expiration_scan_passes", &[], 1.0);
            m.add("scylla_expiration_items_deleted", &[], deleted as f64);
        }
        Ok(deleted)
    }

    /// Runs [`EngineBackend::expire`] every `period` while the backend is alive.
    pub fn spawn_expiration(
        self: &Arc<Self>,
        period: Duration,
        metrics: Option<Arc<crate::metrics::Metrics>>,
    ) -> tokio::task::JoinHandle<()> {
        let weak: Weak<EngineBackend> = Arc::downgrade(self);
        let period = period.max(Duration::from_millis(10));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                let Some(b) = weak.upgrade() else { return };
                if let Err(e) = b
                    .expire(chrono::Utc::now().timestamp_millis(), metrics.as_deref())
                    .await
                {
                    tracing::warn!(error = %e.message, "alternator expiration pass failed");
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(v: &str) -> Item {
        let mut i = Item::new();
        i.insert("e".into(), crate::json::parse(v.as_bytes()).unwrap());
        i
    }

    #[test]
    fn expiration_as_dynamodb_decides_it() {
        let now = 1_792_540_000_000;
        assert!(expired(&item(r#"{"N":"1792539940"}"#), "e", now));
        assert!(expired(&item(r#"{"N":"179253994e1"}"#), "e", now));
        assert!(expired(&item(r#"{"N":"1792539999.5"}"#), "e", now));
        assert!(
            !expired(&item(r#"{"N":"1792540000"}"#), "e", now),
            "not before it passed"
        );
        assert!(!expired(&item(r#"{"N":"1792543600"}"#), "e", now));
        assert!(
            !expired(&item(r#"{"N":"162777600"}"#), "e", now),
            "over five years old"
        );
        assert!(
            !expired(&item(r#"{"S":"1792539940"}"#), "e", now),
            "not a number"
        );
        assert!(!expired(&item(r#"{"N":"1792539940"}"#), "other", now));
    }
}
