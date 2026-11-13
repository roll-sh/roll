//! Audit sinks (docs/permissions.md, "Audit"): the audit log streamed to a SIEM. A sink is a public
//! HTTPS URL that receives new audit rows in order, in batches of up to 200, signed as roll secrets'
//! sinks and roll's webhooks are (`X-Roll-Sink`, `X-Roll-Timestamp`, `X-Roll-Signature: sha256=`
//! hex HMAC of `"{timestamp}.{body}"`), so one receiver takes both. Each sink keeps a cursor (the
//! last row's time and id); the sweeper claims a sink in a write before delivering, so no two nodes
//! send one batch, and a failed batch is sent again 5 minutes later. Rows hold no secrets (their
//! arguments are redacted when they're written).
//!
//! roll-access has no HTTP client and no key: rolld's [`SinkTransport`] checks URLs, posts through
//! roll-net's `PublicOnly` client and seals each sink's secret with `ROLL_SECRET_KEY`.

use crate::store::{AUDIT_COLUMNS, AuditEvent, audit_row};
use crate::{Access, Error, Result, new_id, new_secret};
use code_sql::params;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS audit_sinks (
    id TEXT PRIMARY KEY,
    url TEXT NOT NULL,
    secret TEXT NOT NULL,
    cursor_at INTEGER NOT NULL,
    cursor_id TEXT NOT NULL,
    status TEXT NOT NULL,
    error TEXT,
    delivered_at INTEGER,
    running_until INTEGER NOT NULL,
    retry_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL
) WITHOUT ROWID;
";

/// Rows per delivery.
const BATCH: i64 = 200;
/// How long a claim holds, and how long a failed delivery waits.
const HOLD_MS: i64 = 2 * 60_000;
const RETRY_MS: i64 = 5 * 60_000;
/// How old an audit row is before a sink gets it: rows are written just after their request, so a
/// cursor past a newer row must not skip an older one still being written.
pub const SETTLE_MS: i64 = 30_000;
const SINKS_MAX: i64 = 10;

/// What rolld gives roll-access to deliver with.
pub trait SinkTransport: Send + Sync {
    /// Whether `url` may be a sink (https, a public host).
    fn check(&self, url: &str) -> std::result::Result<(), String>;
    /// A sink's signing secret, sealed (bound to the sink's id) and opened.
    fn seal(&self, id: &str, secret: &str) -> std::result::Result<String, String>;
    fn open(&self, id: &str, sealed: &str) -> std::result::Result<String, String>;
    /// POST `body` with `headers` (no redirects, a bounded read of the answer); `Err` unless 2xx.
    fn post(&self, url: String, headers: Vec<(String, String)>, body: Vec<u8>) -> BoxFuture<'static, std::result::Result<(), String>>;
}

#[derive(Clone, Debug, Serialize)]
pub struct Sink {
    pub id: String,
    pub url: String,
    /// `new`, `ok` or `error`.
    pub status: String,
    pub error: Option<String>,
    /// The time of the last row delivered.
    pub cursor_at: i64,
    pub delivered_at: Option<i64>,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSink {
    pub url: String,
    /// Every row the log still has (default: from now on).
    #[serde(default)]
    pub from_start: bool,
}

const COLUMNS: &str = "id, url, status, error, cursor_at, delivered_at, created_at, created_by";

fn sink_row(r: &code_sql::Row<'_>) -> code_sql::Result<Sink> {
    Ok(Sink { id: r.get(0)?, url: r.get(1)?, status: r.get(2)?, error: r.get(3)?, cursor_at: r.get(4)?, delivered_at: r.get(5)?, created_at: r.get(6)?, created_by: r.get(7)? })
}

/// `sha256=` and the hex HMAC-SHA256 of `"{timestamp}.{body}"`.
pub fn signature(secret: &str, timestamp: i64, body: &[u8]) -> String {
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// A URL's host, for logs (never its path or query: they may hold a token of the receiver's).
fn host(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| Error::Internal(e.to_string()))?
}

impl Access {
    fn transport(&self) -> Result<Arc<dyn SinkTransport>> {
        self.sinks.get().cloned().ok_or_else(|| Error::BadRequest("audit streaming isn't on this server (it needs ROLL_SECRET_KEY)".into()))
    }

    pub fn list_sinks(&self) -> Result<Vec<Sink>> {
        Ok(self.store.db.read(|c| c.query(&format!("SELECT {COLUMNS} FROM audit_sinks ORDER BY created_at"), (), sink_row))?)
    }

    /// A new sink; the second value is its signing secret, shown once.
    pub fn create_sink(&self, n: NewSink, created_by: &str) -> Result<(Sink, String)> {
        let t = self.transport()?;
        let url = n.url.trim().to_string();
        if url.len() > 2048 {
            return Err(Error::BadRequest("the URL is too long".into()));
        }
        t.check(&url).map_err(Error::BadRequest)?;
        let id = new_id("snk");
        let secret = new_secret("whsec_");
        let sealed = t.seal(&id, &secret).map_err(Error::Internal)?;
        let now = code_sql::now_ms();
        let cursor_at = if n.from_start { 0 } else { now };
        let sink = self.store.db.write(|c| {
            let count: i64 = c.query_row("SELECT COUNT(*) FROM audit_sinks", (), |r| r.get(0))?;
            if count >= SINKS_MAX {
                return Err(Error::Conflict(format!("at most {SINKS_MAX} audit sinks")));
            }
            c.execute(
                "INSERT INTO audit_sinks (id, url, secret, cursor_at, cursor_id, status, error, delivered_at, running_until, retry_at, created_at, created_by) \
                 VALUES (?, ?, ?, ?, '', 'new', NULL, NULL, 0, 0, ?, ?)",
                params![id, url, sealed, cursor_at, now, created_by],
            )?;
            Ok(c.query_row(&format!("SELECT {COLUMNS} FROM audit_sinks WHERE id = ?"), [&id], sink_row)?)
        })?;
        Ok((sink, secret))
    }

    /// Send a failed sink's batch at the next sweep rather than after its wait.
    pub fn retry_sink(&self, id: &str) -> Result<()> {
        let n = self.store.db.write(|c| Ok::<_, Error>(c.execute("UPDATE audit_sinks SET retry_at = 0 WHERE id = ?", [id])?))?;
        if n == 0 { Err(Error::NotFound(format!("no audit sink {id}"))) } else { Ok(()) }
    }

    pub fn delete_sink(&self, id: &str) -> Result<()> {
        let n = self.store.db.write(|c| Ok::<_, Error>(c.execute("DELETE FROM audit_sinks WHERE id = ?", [id])?))?;
        if n == 0 { Err(Error::NotFound(format!("no audit sink {id}"))) } else { Ok(()) }
    }

    /// A sweep (rolld runs one every 10 seconds): each sink's next batches, until none has a full
    /// one waiting (at most 25 rounds, 5,000 rows a sink). `(delivered, failed)` batches.
    pub async fn sweep_sinks(self: &Arc<Self>) -> Result<(usize, usize)> {
        let (mut ok, mut failed) = (0, 0);
        for _ in 0..25 {
            let (o, f, more) = self.sweep_round().await?;
            (ok, failed) = (ok + o, failed + f);
            if !more {
                break;
            }
        }
        Ok((ok, failed))
    }

    /// Each due sink's next batch; whether one sent a full batch (more may be waiting).
    async fn sweep_round(self: &Arc<Self>) -> Result<(usize, usize, bool)> {
        let Some(t) = self.sinks.get().cloned() else { return Ok((0, 0, false)) };
        let now = code_sql::now_ms();
        let settled = now - SETTLE_MS;
        let me = self.clone();
        let due: Vec<(String, String, String, i64, String)> = blocking(move || {
            Ok(me.store.db.read(|c| {
                c.query(
                    "SELECT id, url, secret, cursor_at, cursor_id FROM audit_sinks WHERE running_until <= ? AND retry_at <= ? ORDER BY created_at LIMIT 100",
                    params![now, now],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
            })?)
        })
        .await?;
        let (mut ok, mut failed, mut more) = (0, 0, false);
        for (id, url, sealed, at, last) in due {
            let hold = now + HOLD_MS;
            // Its batch, and a claim on it: a node that claimed it first (or moved its cursor) wins.
            let (me, id2, last2) = (self.clone(), id.clone(), last.clone());
            let batch: Option<Vec<AuditEvent>> = blocking(move || {
                let rows = me.store.db.read(|c| {
                    c.query(
                        &format!("SELECT {AUDIT_COLUMNS} FROM audit WHERE (at > ? OR (at = ? AND id > ?)) AND at <= ? ORDER BY at, id LIMIT ?"),
                        params![at, at, last2, settled, BATCH],
                        audit_row,
                    )
                })?;
                if rows.is_empty() {
                    return Ok(None);
                }
                let claimed = me.store.db.write(|c| {
                    Ok::<_, Error>(c.execute(
                        "UPDATE audit_sinks SET running_until = ? WHERE id = ? AND running_until <= ? AND cursor_at = ? AND cursor_id = ?",
                        params![hold, id2, now, at, last2],
                    )?)
                })?;
                Ok((claimed == 1).then_some(rows))
            })
            .await?;
            let Some(rows) = batch else { continue };
            let full = rows.len() as i64 == BATCH;
            let (end_at, end_id) = rows.last().map(|r| (r.at, r.id.clone())).unwrap_or((at, last));
            let sent = async {
                let secret = t.open(&id, &sealed)?;
                let body = serde_json::to_vec(&json!({ "sink": id, "source": "access", "events": rows })).map_err(|e| e.to_string())?;
                let ts = now / 1000;
                let headers = vec![
                    ("content-type".to_string(), "application/json".to_string()),
                    ("x-roll-sink".to_string(), id.clone()),
                    ("x-roll-timestamp".to_string(), ts.to_string()),
                    ("x-roll-signature".to_string(), signature(&secret, ts, &body)),
                ];
                t.post(url.clone(), headers, body).await
            }
            .await;
            let (me, id2, outcome) = (self.clone(), id.clone(), sent.clone());
            blocking(move || {
                let done = code_sql::now_ms();
                me.store.db.write(|c| {
                    match &outcome {
                        Ok(()) => c.execute(
                            "UPDATE audit_sinks SET cursor_at = ?, cursor_id = ?, status = 'ok', error = NULL, delivered_at = ?, running_until = 0, retry_at = 0 WHERE id = ? AND running_until = ?",
                            params![end_at, end_id, done, id2, hold],
                        )?,
                        Err(why) => c.execute(
                            "UPDATE audit_sinks SET status = 'error', error = ?, running_until = 0, retry_at = ? WHERE id = ? AND running_until = ?",
                            params![why.chars().take(300).collect::<String>(), done + RETRY_MS, id2, hold],
                        )?,
                    };
                    Ok::<_, Error>(())
                })
            })
            .await?;
            match sent {
                Ok(()) => {
                    ok += 1;
                    more |= full;
                }
                Err(why) => {
                    failed += 1;
                    tracing::warn!(sink = %id, host = %host(&url), "an audit delivery failed: {why}");
                }
            }
        }
        Ok((ok, failed, more))
    }
}
