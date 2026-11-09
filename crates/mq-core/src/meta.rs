//! Metadata store: topology, users, permissions, policies and parameters as versioned JSON
//! documents. SQLite on a single node; roll postgres (crate `mq-control`) for clusters, where
//! the version counter doubles as the change feed every broker follows.

use anyhow::Context;
use async_trait::async_trait;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub mod kind {
    pub const VHOST: &str = "vhost";
    pub const USER: &str = "user";
    pub const PERMISSION: &str = "permission";
    pub const TOPIC_PERMISSION: &str = "topic_permission";
    pub const EXCHANGE: &str = "exchange";
    pub const QUEUE: &str = "queue";
    pub const BINDING: &str = "binding";
    pub const POLICY: &str = "policy";
    pub const OPERATOR_POLICY: &str = "operator_policy";
    pub const PARAMETER: &str = "parameter";
    pub const GLOBAL_PARAMETER: &str = "global_parameter";
    pub const STREAM_OFFSET: &str = "stream_offset";
    pub const TOKEN: &str = "token";
    pub const WEBHOOK: &str = "webhook";
    pub const DELAYED: &str = "delayed";
}

/// Join key parts with a separator that can't appear in AMQP names.
pub fn key(parts: &[&str]) -> String {
    parts.join("\u{1f}")
}

/// Keys of a vhost's rows start with this.
pub fn vhost_prefix(vhost: &str) -> String {
    format!("{vhost}\u{1f}")
}

/// Just past every key that starts with `vhost_prefix(vhost)`.
fn vhost_prefix_end(vhost: &str) -> String {
    format!("{vhost}\u{20}")
}

pub fn split_key(k: &str) -> Vec<&str> {
    k.split('\u{1f}').collect()
}

#[derive(Clone, Debug)]
pub struct Change {
    pub version: i64,
    pub kind: String,
    pub key: String,
    /// `None` means deleted.
    pub value: Option<Value>,
}

#[async_trait]
pub trait MetaStore: Send + Sync {
    async fn get(&self, kind: &str, key: &str) -> anyhow::Result<Option<Value>>;
    async fn put(&self, kind: &str, key: &str, value: &Value) -> anyhow::Result<i64>;
    async fn delete(&self, kind: &str, key: &str) -> anyhow::Result<()>;
    async fn list(&self, kind: &str) -> anyhow::Result<Vec<(String, Value)>>;
    /// Rows of `kind` whose key starts with `vhost` (exchanges, queues, bindings: keys
    /// `vhost\u{1f}…`). Waking a vhost reads only
    /// its own rows, however many tenants share the store.
    async fn list_vhost(&self, kind: &str, vhost: &str) -> anyhow::Result<Vec<(String, Value)>> {
        let prefix = vhost_prefix(vhost);
        Ok(self.list(kind).await?.into_iter().filter(|(k, _)| k.starts_with(&prefix)).collect())
    }
    /// Compare-and-set on the row version (`None`: the key must not exist). Returns the new
    /// version, or `None` if someone else won.
    async fn cas(&self, kind: &str, key: &str, expected: Option<i64>, value: &Value) -> anyhow::Result<Option<i64>>;
    /// Current value and version.
    async fn get_versioned(&self, kind: &str, key: &str) -> anyhow::Result<Option<(Value, i64)>>;
    /// Changes after `version` (deletes included), oldest first, and the latest version.
    async fn changes_since(&self, version: i64) -> anyhow::Result<(Vec<Change>, i64)>;
    fn describe(&self) -> String;
}

// ── in-memory (tests) ──

#[derive(Default)]
pub struct MemMeta {
    inner: Mutex<MemInner>,
}

#[derive(Default)]
struct MemInner {
    rows: BTreeMap<(String, String), (Value, i64)>,
    log: Vec<Change>,
    version: i64,
}

impl MemMeta {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }
}

#[async_trait]
impl MetaStore for MemMeta {
    async fn get(&self, kind: &str, key: &str) -> anyhow::Result<Option<Value>> {
        Ok(self.inner.lock().rows.get(&(kind.into(), key.into())).map(|(v, _)| v.clone()))
    }
    async fn put(&self, kind: &str, key: &str, value: &Value) -> anyhow::Result<i64> {
        let mut i = self.inner.lock();
        i.version += 1;
        let v = i.version;
        i.rows.insert((kind.into(), key.into()), (value.clone(), v));
        i.log.push(Change { version: v, kind: kind.into(), key: key.into(), value: Some(value.clone()) });
        Ok(v)
    }
    async fn delete(&self, kind: &str, key: &str) -> anyhow::Result<()> {
        let mut i = self.inner.lock();
        if i.rows.remove(&(kind.into(), key.into())).is_some() {
            i.version += 1;
            let v = i.version;
            i.log.push(Change { version: v, kind: kind.into(), key: key.into(), value: None });
        }
        Ok(())
    }
    async fn list(&self, kind: &str) -> anyhow::Result<Vec<(String, Value)>> {
        Ok(self.inner.lock().rows.iter().filter(|((k, _), _)| k == kind).map(|((_, key), (v, _))| (key.clone(), v.clone())).collect())
    }
    async fn list_vhost(&self, kind: &str, vhost: &str) -> anyhow::Result<Vec<(String, Value)>> {
        let range = (kind.to_string(), vhost_prefix(vhost))..(kind.to_string(), vhost_prefix_end(vhost));
        Ok(self.inner.lock().rows.range(range).map(|((_, key), (v, _))| (key.clone(), v.clone())).collect())
    }
    async fn cas(&self, kind: &str, key: &str, expected: Option<i64>, value: &Value) -> anyhow::Result<Option<i64>> {
        let mut i = self.inner.lock();
        let cur = i.rows.get(&(kind.into(), key.into())).map(|(_, v)| *v);
        if cur != expected {
            return Ok(None);
        }
        i.version += 1;
        let v = i.version;
        i.rows.insert((kind.into(), key.into()), (value.clone(), v));
        i.log.push(Change { version: v, kind: kind.into(), key: key.into(), value: Some(value.clone()) });
        Ok(Some(v))
    }
    async fn get_versioned(&self, kind: &str, key: &str) -> anyhow::Result<Option<(Value, i64)>> {
        Ok(self.inner.lock().rows.get(&(kind.into(), key.into())).cloned())
    }
    async fn changes_since(&self, version: i64) -> anyhow::Result<(Vec<Change>, i64)> {
        let i = self.inner.lock();
        Ok((i.log.iter().filter(|c| c.version > version).cloned().collect(), i.version))
    }
    fn describe(&self) -> String {
        "memory".into()
    }
}

// ── SQLite ──

pub struct SqliteMeta {
    db: Mutex<Connection>,
    path: String,
}

impl SqliteMeta {
    pub fn open(path: &Path) -> anyhow::Result<std::sync::Arc<Self>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS meta (
               kind TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, version INTEGER NOT NULL,
               PRIMARY KEY (kind, key));
             CREATE TABLE IF NOT EXISTS meta_log (
               version INTEGER PRIMARY KEY, kind TEXT NOT NULL, key TEXT NOT NULL, value TEXT);
             CREATE TABLE IF NOT EXISTS meta_version (id INTEGER PRIMARY KEY CHECK (id = 1), version INTEGER NOT NULL);
             INSERT OR IGNORE INTO meta_version VALUES (1, 0);",
        )?;
        Ok(std::sync::Arc::new(SqliteMeta { db: Mutex::new(db), path: path.display().to_string() }))
    }

    fn bump(tx: &rusqlite::Transaction, kind: &str, key: &str, value: Option<&str>) -> rusqlite::Result<i64> {
        let v: i64 = tx.query_row("UPDATE meta_version SET version = version + 1 WHERE id = 1 RETURNING version", [], |r| r.get(0))?;
        tx.execute("INSERT INTO meta_log (version, kind, key, value) VALUES (?1, ?2, ?3, ?4)", params![v, kind, key, value])?;
        // Keep the change feed bounded; followers that fall further behind resync from a list.
        tx.execute("DELETE FROM meta_log WHERE version < ?1", params![v - 100_000])?;
        Ok(v)
    }
}

#[async_trait]
impl MetaStore for SqliteMeta {
    async fn get(&self, kind: &str, key: &str) -> anyhow::Result<Option<Value>> {
        Ok(self.get_versioned(kind, key).await?.map(|(v, _)| v))
    }
    async fn put(&self, kind: &str, key: &str, value: &Value) -> anyhow::Result<i64> {
        let text = value.to_string();
        let mut db = self.db.lock();
        let tx = db.transaction()?;
        let v = Self::bump(&tx, kind, key, Some(&text))?;
        tx.execute(
            "INSERT INTO meta (kind, key, value, version) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (kind, key) DO UPDATE SET value = excluded.value, version = excluded.version",
            params![kind, key, text, v],
        )?;
        tx.commit()?;
        Ok(v)
    }
    async fn delete(&self, kind: &str, key: &str) -> anyhow::Result<()> {
        let mut db = self.db.lock();
        let tx = db.transaction()?;
        if tx.execute("DELETE FROM meta WHERE kind = ?1 AND key = ?2", params![kind, key])? > 0 {
            Self::bump(&tx, kind, key, None)?;
        }
        tx.commit()?;
        Ok(())
    }
    async fn list(&self, kind: &str) -> anyhow::Result<Vec<(String, Value)>> {
        let db = self.db.lock();
        let mut st = db.prepare_cached("SELECT key, value FROM meta WHERE kind = ?1 ORDER BY key")?;
        let rows = st.query_map(params![kind], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            let (k, v) = r?;
            out.push((k, serde_json::from_str(&v)?));
        }
        Ok(out)
    }
    async fn list_vhost(&self, kind: &str, vhost: &str) -> anyhow::Result<Vec<(String, Value)>> {
        let db = self.db.lock();
        let mut st = db.prepare_cached("SELECT key, value FROM meta WHERE kind = ?1 AND key >= ?2 AND key < ?3 ORDER BY key")?;
        let rows = st.query_map(params![kind, vhost_prefix(vhost), vhost_prefix_end(vhost)], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            let (k, v) = r?;
            out.push((k, serde_json::from_str(&v)?));
        }
        Ok(out)
    }
    async fn cas(&self, kind: &str, key: &str, expected: Option<i64>, value: &Value) -> anyhow::Result<Option<i64>> {
        let text = value.to_string();
        let mut db = self.db.lock();
        let tx = db.transaction()?;
        let cur: Option<i64> = tx.query_row("SELECT version FROM meta WHERE kind = ?1 AND key = ?2", params![kind, key], |r| r.get(0)).optional()?;
        if cur != expected {
            return Ok(None);
        }
        let v = Self::bump(&tx, kind, key, Some(&text))?;
        tx.execute(
            "INSERT INTO meta (kind, key, value, version) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (kind, key) DO UPDATE SET value = excluded.value, version = excluded.version",
            params![kind, key, text, v],
        )?;
        tx.commit()?;
        Ok(Some(v))
    }
    async fn get_versioned(&self, kind: &str, key: &str) -> anyhow::Result<Option<(Value, i64)>> {
        let db = self.db.lock();
        let row: Option<(String, i64)> = db
            .query_row("SELECT value, version FROM meta WHERE kind = ?1 AND key = ?2", params![kind, key], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        Ok(match row {
            Some((v, ver)) => Some((serde_json::from_str(&v)?, ver)),
            None => None,
        })
    }
    async fn changes_since(&self, version: i64) -> anyhow::Result<(Vec<Change>, i64)> {
        let db = self.db.lock();
        let latest: i64 = db.query_row("SELECT version FROM meta_version WHERE id = 1", [], |r| r.get(0))?;
        let mut st = db.prepare_cached("SELECT version, kind, key, value FROM meta_log WHERE version > ?1 ORDER BY version")?;
        let rows = st.query_map(params![version], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?)))?;
        let mut out = Vec::new();
        for r in rows {
            let (v, kind, key, value) = r?;
            out.push(Change { version: v, kind, key, value: value.map(|s| serde_json::from_str(&s)).transpose()? });
        }
        Ok((out, latest))
    }
    fn describe(&self) -> String {
        format!("sqlite ({})", self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn exercise(m: &dyn MetaStore) {
        m.put("q", "a", &json!({"x": 1})).await.unwrap();
        m.put("q", "b", &json!(2)).await.unwrap();
        assert_eq!(m.get("q", "a").await.unwrap(), Some(json!({"x": 1})));
        assert_eq!(m.list("q").await.unwrap().len(), 2);
        let (_, v) = m.get_versioned("q", "a").await.unwrap().unwrap();
        assert!(m.cas("q", "a", Some(v), &json!(3)).await.unwrap().is_some());
        assert!(m.cas("q", "a", Some(v), &json!(4)).await.unwrap().is_none());
        assert!(m.cas("q", "new", None, &json!(5)).await.unwrap().is_some());
        m.delete("q", "b").await.unwrap();
        let (changes, latest) = m.changes_since(0).await.unwrap();
        assert_eq!(changes.last().unwrap().value, None);
        assert_eq!(changes.last().unwrap().version, latest);
        for k in [key(&["v", "x"]), key(&["v", "y", "z"]), key(&["v2", "x"]), key(&["", "x"]), "v".into(), "vx".into()] {
            m.put("e", &k, &json!(1)).await.unwrap();
        }
        let keys = |v: Vec<(String, Value)>| v.into_iter().map(|(k, _)| k).collect::<Vec<_>>();
        assert_eq!(keys(m.list_vhost("e", "v").await.unwrap()), [key(&["v", "x"]), key(&["v", "y", "z"])]);
        assert_eq!(keys(m.list_vhost("e", "").await.unwrap()), [key(&["", "x"])]);
        assert!(m.list_vhost("q", "v").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn mem() {
        exercise(MemMeta::new().as_ref()).await;
    }

    #[tokio::test]
    async fn sqlite() {
        let p = std::env::temp_dir().join(format!("mq-meta-{}.db", crate::rand_u64()));
        exercise(SqliteMeta::open(&p).unwrap().as_ref()).await;
        std::fs::remove_file(p).ok();
    }
}
