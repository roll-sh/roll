//! Stream catalog in SQLite (`stream.db`): streams, API keys, topics, the object-storage chunk
//! index, committed offsets, transactions and usage. Partition data itself is in the logs.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use stream_log::{AbortedTxn, Chunk};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Create topics on first use (Metadata requests that allow it, and produce).
    pub auto_create_topics: bool,
    /// Partitions for auto-created topics and CreateTopics with `-1`.
    pub default_partitions: i32,
    /// Upper bound on partitions across all topics.
    pub max_partitions: i32,
    /// Byte-rate limits, 0 = unlimited. Exceeding them throttles clients (KIP-219).
    pub ingress_bytes_per_s: u64,
    pub egress_bytes_per_s: u64,
    /// Default `retention.ms` for new topics (-1 = forever; `None` = broker default).
    pub retention_ms: Option<i64>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_create_topics: true,
            default_partitions: 1,
            max_partitions: 10_000,
            ingress_bytes_per_s: 0,
            egress_bytes_per_s: 0,
            retention_ms: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StreamRow {
    pub id: String,
    pub name: String,
    pub region: String,
    pub created_at: i64,
    pub settings: Settings,
}

/// SCRAM credentials (PLAIN is verified against the same salted password).
#[derive(Clone, Debug, Serialize)]
pub struct Credential {
    /// The SASL username (an API key id).
    pub username: String,
    pub stream_id: String,
    pub label: String,
    pub created_at: i64,
    #[serde(skip)]
    pub salt: Vec<u8>,
    #[serde(skip)]
    pub iterations: u32,
    #[serde(skip)]
    pub keys: ScramKeys,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScramKeys {
    pub stored_256: Vec<u8>,
    pub server_256: Vec<u8>,
    pub stored_512: Vec<u8>,
    pub server_512: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct TopicRow {
    pub id: Uuid,
    pub stream_id: String,
    pub name: String,
    pub partitions: i32,
    pub configs: BTreeMap<String, String>,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OffsetRow {
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
    pub leader_epoch: i32,
    pub metadata: Option<String>,
    pub committed_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TxnRow {
    pub stream_id: String,
    pub txn_id: String,
    pub producer_id: i64,
    pub epoch: i16,
    pub state: String,
    pub timeout_ms: i32,
    /// `topic name → partitions`.
    pub partitions: BTreeMap<String, Vec<i32>>,
    pub groups: Vec<String>,
    pub started_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct UsageRow {
    pub hour: i64,
    pub bytes_in: i64,
    pub bytes_out: i64,
    pub records_in: i64,
    pub requests: i64,
    pub puts: i64,
    pub gets: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct StorageSample {
    pub at: i64,
    pub local_bytes: i64,
    pub remote_bytes: i64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE streams (
  id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, region TEXT NOT NULL, created_at INTEGER NOT NULL,
  settings TEXT NOT NULL
);
CREATE TABLE credentials (
  username TEXT PRIMARY KEY, stream_id TEXT NOT NULL REFERENCES streams(id) ON DELETE CASCADE,
  label TEXT NOT NULL, created_at INTEGER NOT NULL, salt BLOB NOT NULL, iterations INTEGER NOT NULL,
  stored_256 BLOB NOT NULL, server_256 BLOB NOT NULL, stored_512 BLOB NOT NULL, server_512 BLOB NOT NULL
);
CREATE TABLE topics (
  id TEXT PRIMARY KEY, stream_id TEXT NOT NULL REFERENCES streams(id) ON DELETE CASCADE,
  name TEXT NOT NULL, partitions INTEGER NOT NULL, configs TEXT NOT NULL, created_at INTEGER NOT NULL,
  UNIQUE (stream_id, name)
);
CREATE TABLE partitions (
  topic_id TEXT NOT NULL REFERENCES topics(id) ON DELETE CASCADE, partition INTEGER NOT NULL,
  log_start INTEGER NOT NULL, PRIMARY KEY (topic_id, partition)
);
CREATE TABLE chunks (
  topic_id TEXT NOT NULL REFERENCES topics(id) ON DELETE CASCADE, partition INTEGER NOT NULL,
  first INTEGER NOT NULL, last INTEGER NOT NULL, max_ts INTEGER NOT NULL, bytes INTEGER NOT NULL,
  key TEXT NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY (topic_id, partition, first)
);
CREATE TABLE aborted_txns (
  topic_id TEXT NOT NULL REFERENCES topics(id) ON DELETE CASCADE, partition INTEGER NOT NULL,
  producer_id INTEGER NOT NULL, first INTEGER NOT NULL, last INTEGER NOT NULL
);
CREATE INDEX aborted_txns_partition ON aborted_txns (topic_id, partition, last);
CREATE TABLE offsets (
  stream_id TEXT NOT NULL REFERENCES streams(id) ON DELETE CASCADE, group_id TEXT NOT NULL,
  topic TEXT NOT NULL, partition INTEGER NOT NULL, "offset" INTEGER NOT NULL, leader_epoch INTEGER NOT NULL,
  metadata TEXT, committed_at INTEGER NOT NULL, PRIMARY KEY (stream_id, group_id, topic, partition)
);
CREATE TABLE groups (
  stream_id TEXT NOT NULL REFERENCES streams(id) ON DELETE CASCADE, group_id TEXT NOT NULL,
  protocol_type TEXT NOT NULL, generation INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  PRIMARY KEY (stream_id, group_id)
);
CREATE TABLE txns (
  stream_id TEXT NOT NULL REFERENCES streams(id) ON DELETE CASCADE, txn_id TEXT NOT NULL,
  producer_id INTEGER NOT NULL, epoch INTEGER NOT NULL, state TEXT NOT NULL, timeout_ms INTEGER NOT NULL,
  partitions TEXT NOT NULL, groups TEXT NOT NULL, started_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  PRIMARY KEY (stream_id, txn_id)
);
CREATE TABLE meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE usage (
  stream_id TEXT NOT NULL REFERENCES streams(id) ON DELETE CASCADE, hour INTEGER NOT NULL,
  bytes_in INTEGER NOT NULL, bytes_out INTEGER NOT NULL, records_in INTEGER NOT NULL,
  requests INTEGER NOT NULL, puts INTEGER NOT NULL, gets INTEGER NOT NULL, PRIMARY KEY (stream_id, hour)
);
CREATE TABLE storage_samples (
  stream_id TEXT NOT NULL REFERENCES streams(id) ON DELETE CASCADE, at INTEGER NOT NULL,
  local_bytes INTEGER NOT NULL, remote_bytes INTEGER NOT NULL
);
CREATE INDEX storage_samples_stream ON storage_samples (stream_id, at);
"#];

fn stream_row(r: &Row) -> rusqlite::Result<StreamRow> {
    let settings: String = r.get(4)?;
    Ok(StreamRow {
        id: r.get(0)?,
        name: r.get(1)?,
        region: r.get(2)?,
        created_at: r.get(3)?,
        settings: serde_json::from_str(&settings).unwrap_or_default(),
    })
}

fn credential_row(r: &Row) -> rusqlite::Result<Credential> {
    Ok(Credential {
        username: r.get(0)?,
        stream_id: r.get(1)?,
        label: r.get(2)?,
        created_at: r.get(3)?,
        salt: r.get(4)?,
        iterations: r.get(5)?,
        keys: ScramKeys { stored_256: r.get(6)?, server_256: r.get(7)?, stored_512: r.get(8)?, server_512: r.get(9)? },
    })
}

fn uuid_col(r: &Row, i: usize) -> rusqlite::Result<Uuid> {
    let s: String = r.get(i)?;
    Uuid::parse_str(&s).map_err(|e| rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e)))
}

fn topic_row(r: &Row) -> rusqlite::Result<TopicRow> {
    let configs: String = r.get(4)?;
    Ok(TopicRow {
        id: uuid_col(r, 0)?,
        stream_id: r.get(1)?,
        name: r.get(2)?,
        partitions: r.get(3)?,
        configs: serde_json::from_str(&configs).unwrap_or_default(),
        created_at: r.get(5)?,
    })
}

fn txn_row(r: &Row) -> rusqlite::Result<TxnRow> {
    let partitions: String = r.get(6)?;
    let groups: String = r.get(7)?;
    Ok(TxnRow {
        stream_id: r.get(0)?,
        txn_id: r.get(1)?,
        producer_id: r.get(2)?,
        epoch: r.get(3)?,
        state: r.get(4)?,
        timeout_ms: r.get(5)?,
        partitions: serde_json::from_str(&partitions).unwrap_or_default(),
        groups: serde_json::from_str(&groups).unwrap_or_default(),
        started_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
}

const STREAM_COLS: &str = "id, name, region, created_at, settings";
const CREDENTIAL_COLS: &str =
    "username, stream_id, label, created_at, salt, iterations, stored_256, server_256, stored_512, server_512";
const TOPIC_COLS: &str = "id, stream_id, name, partitions, configs, created_at";
const TXN_COLS: &str =
    "stream_id, txn_id, producer_id, epoch, state, timeout_ms, partitions, groups, started_at, updated_at";

fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("serialisable")
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
        )?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
            tx.commit()?;
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn with<T>(&self, f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>) -> anyhow::Result<T> {
        let mut c = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(f(&mut c)?)
    }

    // ── streams ──

    pub fn insert_stream(&self, s: &StreamRow) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                &format!("INSERT INTO streams ({STREAM_COLS}) VALUES (?1, ?2, ?3, ?4, ?5)"),
                params![s.id, s.name, s.region, s.created_at, json(&s.settings)],
            )
            .map(drop)
        })
    }

    pub fn streams(&self) -> anyhow::Result<Vec<StreamRow>> {
        self.with(|c| {
            let mut s = c.prepare(&format!("SELECT {STREAM_COLS} FROM streams ORDER BY created_at"))?;
            s.query_map([], stream_row)?.collect()
        })
    }

    pub fn update_settings(&self, id: &str, settings: &Settings) -> anyhow::Result<()> {
        self.with(|c| c.execute("UPDATE streams SET settings = ?2 WHERE id = ?1", params![id, json(settings)]).map(drop))
    }

    pub fn delete_stream(&self, id: &str) -> anyhow::Result<()> {
        self.with(|c| c.execute("DELETE FROM streams WHERE id = ?1", [id]).map(drop))
    }

    // ── credentials ──

    pub fn insert_credential(&self, k: &Credential) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                &format!("INSERT INTO credentials ({CREDENTIAL_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"),
                params![
                    k.username,
                    k.stream_id,
                    k.label,
                    k.created_at,
                    k.salt,
                    k.iterations,
                    k.keys.stored_256,
                    k.keys.server_256,
                    k.keys.stored_512,
                    k.keys.server_512
                ],
            )
            .map(drop)
        })
    }

    pub fn credential(&self, username: &str) -> anyhow::Result<Option<Credential>> {
        self.with(|c| {
            c.query_row(&format!("SELECT {CREDENTIAL_COLS} FROM credentials WHERE username = ?1"), [username], credential_row)
                .optional()
        })
    }

    pub fn credentials(&self, stream_id: &str) -> anyhow::Result<Vec<Credential>> {
        self.with(|c| {
            let mut s = c.prepare(&format!(
                "SELECT {CREDENTIAL_COLS} FROM credentials WHERE stream_id = ?1 ORDER BY created_at"
            ))?;
            s.query_map([stream_id], credential_row)?.collect()
        })
    }

    pub fn delete_credential(&self, stream_id: &str, username: &str) -> anyhow::Result<bool> {
        self.with(|c| {
            c.execute("DELETE FROM credentials WHERE stream_id = ?1 AND username = ?2", [stream_id, username]).map(|n| n > 0)
        })
    }

    // ── topics ──

    pub fn insert_topic(&self, t: &TopicRow) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                &format!("INSERT INTO topics ({TOPIC_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"),
                params![t.id.to_string(), t.stream_id, t.name, t.partitions, json(&t.configs), t.created_at],
            )
            .map(drop)
        })
    }

    pub fn topics(&self) -> anyhow::Result<Vec<TopicRow>> {
        self.with(|c| {
            let mut s = c.prepare(&format!("SELECT {TOPIC_COLS} FROM topics ORDER BY created_at, name"))?;
            s.query_map([], topic_row)?.collect()
        })
    }

    pub fn set_partitions(&self, topic: &Uuid, partitions: i32) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute("UPDATE topics SET partitions = ?2 WHERE id = ?1", params![topic.to_string(), partitions]).map(drop)
        })
    }

    pub fn set_configs(&self, topic: &Uuid, configs: &BTreeMap<String, String>) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute("UPDATE topics SET configs = ?2 WHERE id = ?1", params![topic.to_string(), json(configs)]).map(drop)
        })
    }

    pub fn delete_topic(&self, topic: &Uuid) -> anyhow::Result<()> {
        self.with(|c| c.execute("DELETE FROM topics WHERE id = ?1", [topic.to_string()]).map(drop))
    }

    // ── partition state ──

    pub fn log_start(&self, topic: &Uuid, partition: i32) -> anyhow::Result<i64> {
        self.with(|c| {
            c.query_row(
                "SELECT log_start FROM partitions WHERE topic_id = ?1 AND partition = ?2",
                params![topic.to_string(), partition],
                |r| r.get(0),
            )
            .optional()
            .map(|v| v.unwrap_or(0))
        })
    }

    pub fn set_log_start(&self, topic: &Uuid, partition: i32, log_start: i64) -> anyhow::Result<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO partitions (topic_id, partition, log_start) VALUES (?1, ?2, ?3)
                 ON CONFLICT (topic_id, partition) DO UPDATE SET log_start = MAX(log_start, excluded.log_start)",
                params![topic.to_string(), partition, log_start],
            )?;
            tx.execute(
                "DELETE FROM aborted_txns WHERE topic_id = ?1 AND partition = ?2 AND last < ?3",
                params![topic.to_string(), partition, log_start],
            )?;
            tx.commit()
        })
    }

    pub fn chunks(&self, topic: &Uuid, partition: i32) -> anyhow::Result<Vec<Chunk>> {
        self.with(|c| {
            let mut s = c.prepare(
                "SELECT first, last, max_ts, bytes, key FROM chunks WHERE topic_id = ?1 AND partition = ?2 ORDER BY first",
            )?;
            s.query_map(params![topic.to_string(), partition], |r| {
                Ok(Chunk { first: r.get(0)?, last: r.get(1)?, max_timestamp: r.get(2)?, bytes: r.get::<_, i64>(3)? as u64, key: r.get(4)? })
            })?
            .collect()
        })
    }

    pub fn insert_chunk(&self, topic: &Uuid, partition: i32, ch: &Chunk, now: i64) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO chunks (topic_id, partition, first, last, max_ts, bytes, key, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![topic.to_string(), partition, ch.first, ch.last, ch.max_timestamp, ch.bytes as i64, ch.key, now],
            )
            .map(drop)
        })
    }

    /// Replace the chunks starting at `firsts` with `added`, atomically (compaction).
    pub fn replace_chunks(&self, topic: &Uuid, partition: i32, firsts: &[i64], added: &[Chunk], now: i64) -> anyhow::Result<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            for f in firsts {
                tx.execute(
                    "DELETE FROM chunks WHERE topic_id = ?1 AND partition = ?2 AND first = ?3",
                    params![topic.to_string(), partition, f],
                )?;
            }
            for ch in added {
                tx.execute(
                    "INSERT OR REPLACE INTO chunks (topic_id, partition, first, last, max_ts, bytes, key, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![topic.to_string(), partition, ch.first, ch.last, ch.max_timestamp, ch.bytes as i64, ch.key, now],
                )?;
            }
            tx.commit()
        })
    }

    pub fn delete_chunks(&self, topic: &Uuid, partition: i32, firsts: &[i64]) -> anyhow::Result<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            for f in firsts {
                tx.execute(
                    "DELETE FROM chunks WHERE topic_id = ?1 AND partition = ?2 AND first = ?3",
                    params![topic.to_string(), partition, f],
                )?;
            }
            tx.commit()
        })
    }

    /// Bytes in object storage per stream.
    pub fn remote_bytes(&self) -> anyhow::Result<BTreeMap<String, i64>> {
        self.with(|c| {
            let mut s = c.prepare(
                "SELECT t.stream_id, COALESCE(SUM(c.bytes), 0) FROM chunks c JOIN topics t ON t.id = c.topic_id GROUP BY t.stream_id",
            )?;
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
        })
    }

    pub fn aborted(&self, topic: &Uuid, partition: i32) -> anyhow::Result<Vec<AbortedTxn>> {
        self.with(|c| {
            let mut s = c.prepare(
                "SELECT producer_id, first, last FROM aborted_txns WHERE topic_id = ?1 AND partition = ?2 ORDER BY first",
            )?;
            s.query_map(params![topic.to_string(), partition], |r| {
                Ok(AbortedTxn { producer_id: r.get(0)?, first_offset: r.get(1)?, last_offset: r.get(2)? })
            })?
            .collect()
        })
    }

    pub fn insert_aborted(&self, topic: &Uuid, partition: i32, a: &AbortedTxn) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO aborted_txns (topic_id, partition, producer_id, first, last) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![topic.to_string(), partition, a.producer_id, a.first_offset, a.last_offset],
            )
            .map(drop)
        })
    }

    // ── offsets ──

    pub fn commit_offsets(&self, stream: &str, group: &str, rows: &[OffsetRow]) -> anyhow::Result<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            {
                let mut s = tx.prepare_cached(
                    r#"INSERT INTO offsets (stream_id, group_id, topic, partition, "offset", leader_epoch, metadata, committed_at)
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                       ON CONFLICT (stream_id, group_id, topic, partition) DO UPDATE SET
                         "offset" = excluded."offset", leader_epoch = excluded.leader_epoch,
                         metadata = excluded.metadata, committed_at = excluded.committed_at"#,
                )?;
                for r in rows {
                    s.execute(params![stream, group, r.topic, r.partition, r.offset, r.leader_epoch, r.metadata, r.committed_at])?;
                }
            }
            tx.commit()
        })
    }

    pub fn offsets(&self, stream: &str, group: &str) -> anyhow::Result<Vec<OffsetRow>> {
        self.with(|c| {
            let mut s = c.prepare_cached(
                r#"SELECT topic, partition, "offset", leader_epoch, metadata, committed_at FROM offsets
                   WHERE stream_id = ?1 AND group_id = ?2 ORDER BY topic, partition"#,
            )?;
            s.query_map([stream, group], |r| {
                Ok(OffsetRow {
                    topic: r.get(0)?,
                    partition: r.get(1)?,
                    offset: r.get(2)?,
                    leader_epoch: r.get(3)?,
                    metadata: r.get(4)?,
                    committed_at: r.get(5)?,
                })
            })?
            .collect()
        })
    }

    /// All offsets of a stream as `(group, row)`.
    pub fn all_offsets(&self, stream: &str) -> anyhow::Result<Vec<(String, OffsetRow)>> {
        self.with(|c| {
            let mut s = c.prepare(
                r#"SELECT group_id, topic, partition, "offset", leader_epoch, metadata, committed_at FROM offsets
                   WHERE stream_id = ?1 ORDER BY group_id, topic, partition"#,
            )?;
            s.query_map([stream], |r| {
                Ok((
                    r.get(0)?,
                    OffsetRow {
                        topic: r.get(1)?,
                        partition: r.get(2)?,
                        offset: r.get(3)?,
                        leader_epoch: r.get(4)?,
                        metadata: r.get(5)?,
                        committed_at: r.get(6)?,
                    },
                ))
            })?
            .collect()
        })
    }

    pub fn offset_groups(&self, stream: &str) -> anyhow::Result<Vec<String>> {
        self.with(|c| {
            let mut s = c.prepare(
                "SELECT group_id FROM offsets WHERE stream_id = ?1 UNION SELECT group_id FROM groups WHERE stream_id = ?1 ORDER BY 1",
            )?;
            s.query_map([stream], |r| r.get(0))?.collect()
        })
    }

    /// Delete a group's offsets: all of them, or only the given partitions.
    pub fn delete_offsets(&self, stream: &str, group: &str, partitions: Option<&[(String, i32)]>) -> anyhow::Result<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            match partitions {
                None => {
                    tx.execute("DELETE FROM offsets WHERE stream_id = ?1 AND group_id = ?2", [stream, group])?;
                    tx.execute("DELETE FROM groups WHERE stream_id = ?1 AND group_id = ?2", [stream, group])?;
                }
                Some(ps) => {
                    for (t, p) in ps {
                        tx.execute(
                            "DELETE FROM offsets WHERE stream_id = ?1 AND group_id = ?2 AND topic = ?3 AND partition = ?4",
                            params![stream, group, t, p],
                        )?;
                    }
                }
            }
            tx.commit()
        })
    }

    pub fn delete_topic_offsets(&self, stream: &str, topic: &str) -> anyhow::Result<()> {
        self.with(|c| c.execute("DELETE FROM offsets WHERE stream_id = ?1 AND topic = ?2", [stream, topic]).map(drop))
    }

    /// Delete offsets committed before `cutoff` for the given (empty) groups.
    pub fn expire_offsets(&self, stream: &str, groups: &[String], cutoff: i64) -> anyhow::Result<usize> {
        self.with(|c| {
            let tx = c.transaction()?;
            let mut n = 0;
            for g in groups {
                n += tx.execute(
                    "DELETE FROM offsets WHERE stream_id = ?1 AND group_id = ?2 AND committed_at < ?3",
                    params![stream, g, cutoff],
                )?;
            }
            tx.commit()?;
            Ok(n)
        })
    }

    pub fn save_group(&self, stream: &str, group: &str, protocol_type: &str, generation: i32, now: i64) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO groups (stream_id, group_id, protocol_type, generation, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (stream_id, group_id) DO UPDATE SET protocol_type = excluded.protocol_type,
                   generation = excluded.generation, updated_at = excluded.updated_at",
                params![stream, group, protocol_type, generation, now],
            )
            .map(drop)
        })
    }

    /// `(protocol type, generation)` of a persisted group.
    pub fn group(&self, stream: &str, group: &str) -> anyhow::Result<Option<(String, i32)>> {
        self.with(|c| {
            c.query_row(
                "SELECT protocol_type, generation FROM groups WHERE stream_id = ?1 AND group_id = ?2",
                [stream, group],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
        })
    }

    // ── transactions ──

    pub fn save_txn(&self, t: &TxnRow) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                &format!("INSERT OR REPLACE INTO txns ({TXN_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"),
                params![
                    t.stream_id,
                    t.txn_id,
                    t.producer_id,
                    t.epoch,
                    t.state,
                    t.timeout_ms,
                    json(&t.partitions),
                    json(&t.groups),
                    t.started_at,
                    t.updated_at
                ],
            )
            .map(drop)
        })
    }

    pub fn txns(&self) -> anyhow::Result<Vec<TxnRow>> {
        self.with(|c| {
            let mut s = c.prepare(&format!("SELECT {TXN_COLS} FROM txns"))?;
            s.query_map([], txn_row)?.collect()
        })
    }

    pub fn delete_txn(&self, stream: &str, txn_id: &str) -> anyhow::Result<()> {
        self.with(|c| c.execute("DELETE FROM txns WHERE stream_id = ?1 AND txn_id = ?2", [stream, txn_id]).map(drop))
    }

    /// Reserve `n` producer ids; returns the first.
    pub fn allocate_producer_ids(&self, n: i64) -> anyhow::Result<i64> {
        self.with(|c| {
            let tx = c.transaction()?;
            let next: i64 = tx
                .query_row("SELECT value FROM meta WHERE key = 'next_producer_id'", [], |r| r.get(0))
                .optional()?
                .unwrap_or(1000);
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('next_producer_id', ?1) ON CONFLICT (key) DO UPDATE SET value = ?1",
                [next + n],
            )?;
            tx.commit()?;
            Ok(next)
        })
    }

    // ── usage ──

    pub fn add_usage(&self, stream: &str, u: &UsageRow) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO usage (stream_id, hour, bytes_in, bytes_out, records_in, requests, puts, gets)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (stream_id, hour) DO UPDATE SET bytes_in = bytes_in + excluded.bytes_in,
                   bytes_out = bytes_out + excluded.bytes_out, records_in = records_in + excluded.records_in,
                   requests = requests + excluded.requests, puts = puts + excluded.puts, gets = gets + excluded.gets",
                params![stream, u.hour, u.bytes_in, u.bytes_out, u.records_in, u.requests, u.puts, u.gets],
            )
            .map(drop)
        })
    }

    pub fn usage_since(&self, stream: &str, since: i64) -> anyhow::Result<Vec<UsageRow>> {
        self.with(|c| {
            let mut s = c.prepare(
                "SELECT hour, bytes_in, bytes_out, records_in, requests, puts, gets FROM usage
                 WHERE stream_id = ?1 AND hour >= ?2 ORDER BY hour",
            )?;
            s.query_map(params![stream, since], |r| {
                Ok(UsageRow {
                    hour: r.get(0)?,
                    bytes_in: r.get(1)?,
                    bytes_out: r.get(2)?,
                    records_in: r.get(3)?,
                    requests: r.get(4)?,
                    puts: r.get(5)?,
                    gets: r.get(6)?,
                })
            })?
            .collect()
        })
    }

    pub fn add_storage_sample(&self, stream: &str, s: &StorageSample) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO storage_samples (stream_id, at, local_bytes, remote_bytes) VALUES (?1, ?2, ?3, ?4)",
                params![stream, s.at, s.local_bytes, s.remote_bytes],
            )
            .map(drop)
        })
    }

    pub fn storage_since(&self, stream: &str, since: i64) -> anyhow::Result<Vec<StorageSample>> {
        self.with(|c| {
            let mut s = c.prepare(
                "SELECT at, local_bytes, remote_bytes FROM storage_samples WHERE stream_id = ?1 AND at >= ?2 ORDER BY at",
            )?;
            s.query_map(params![stream, since], |r| {
                Ok(StorageSample { at: r.get(0)?, local_bytes: r.get(1)?, remote_bytes: r.get(2)? })
            })?
            .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }

    fn stream(s: &Store, id: &str) {
        s.insert_stream(&StreamRow { id: id.into(), name: id.into(), region: "local".into(), created_at: 0, settings: Settings::default() })
            .unwrap();
    }

    #[test]
    fn streams_topics_and_cascade() {
        let s = store();
        stream(&s, "st_a");
        let id = Uuid::from_u128(7);
        let mut configs = BTreeMap::new();
        configs.insert("retention.ms".to_string(), "1000".to_string());
        s.insert_topic(&TopicRow { id, stream_id: "st_a".into(), name: "orders".into(), partitions: 3, configs, created_at: 1 }).unwrap();
        s.set_partitions(&id, 6).unwrap();
        let t = &s.topics().unwrap()[0];
        assert_eq!((t.partitions, t.configs["retention.ms"].as_str()), (6, "1000"));
        let ch = Chunk { first: 0, last: 9, max_timestamp: 5, bytes: 100, key: "k".into() };
        s.insert_chunk(&id, 0, &ch, 0).unwrap();
        assert_eq!(s.chunks(&id, 0).unwrap(), vec![ch]);
        assert_eq!(s.remote_bytes().unwrap()["st_a"], 100);
        s.set_log_start(&id, 0, 5).unwrap();
        s.set_log_start(&id, 0, 3).unwrap(); // never moves back
        assert_eq!(s.log_start(&id, 0).unwrap(), 5);
        s.delete_stream("st_a").unwrap();
        assert!(s.topics().unwrap().is_empty());
        assert!(s.chunks(&id, 0).unwrap().is_empty());
    }

    #[test]
    fn offsets_roundtrip_and_expiry() {
        let s = store();
        stream(&s, "st_a");
        let row = |p, o, at| OffsetRow { topic: "t".into(), partition: p, offset: o, leader_epoch: 0, metadata: Some("m".into()), committed_at: at };
        s.commit_offsets("st_a", "g", &[row(0, 10, 100), row(1, 20, 200)]).unwrap();
        s.commit_offsets("st_a", "g", &[row(0, 11, 300)]).unwrap();
        assert_eq!(s.offsets("st_a", "g").unwrap(), vec![row(0, 11, 300), row(1, 20, 200)]);
        assert_eq!(s.offset_groups("st_a").unwrap(), vec!["g".to_string()]);
        assert_eq!(s.expire_offsets("st_a", &["g".into()], 250).unwrap(), 1);
        s.delete_offsets("st_a", "g", Some(&[("t".into(), 0)])).unwrap();
        assert!(s.offsets("st_a", "g").unwrap().is_empty());
    }

    #[test]
    fn producer_ids_are_unique() {
        let s = store();
        let a = s.allocate_producer_ids(100).unwrap();
        let b = s.allocate_producer_ids(100).unwrap();
        assert_eq!(b, a + 100);
    }
}
