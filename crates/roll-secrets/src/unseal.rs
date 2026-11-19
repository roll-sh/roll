//! Starting sealed (docs/secrets.md, "Starting sealed"): rolld holds no deployment key until a
//! threshold of its `rlks_` shares (`roll secrets key split`) are posted to `/v1/secrets/unseal`,
//! one at a time, as Vault's Shamir unseal. Nothing authenticates a caller before the key
//! exists: the shares are the credential, so attempts are limited per address (in memory) and
//! failed combinations per split (in the store, so a restart doesn't reset them).
//!
//! - Pending shares live only here, zeroized on drop, each for [`SHARE_TTL`] at most. No answer,
//!   log or error carries one.
//! - A share of another split resets the progress, and so does a combination that fails (shares
//!   that don't belong together, or a key that isn't the store's): holders start over.
//! - The combined key must be the store's: [`check_key`] compares it with the key id recorded the
//!   last time roll secrets opened ([`record_key`]), else with the projects' data keys.

use crate::crypto::Keyring;
use crate::{Result, kms};
use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use code_sql::{Db, OptionalExtension as _};
use roll_keyshares::Share;
use serde_json::{Value as Json, json};
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;
use zeroize::Zeroizing;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS deployment_key (
    id INTEGER PRIMARY KEY,
    kek_id TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS unseal_failures (
    set_id TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS unseal_failures_set ON unseal_failures (set_id, at);
";

/// How long a share given waits for the rest.
pub const SHARE_TTL: Duration = Duration::from_secs(600);
/// Shares one address may post in [`ADDRESS_WINDOW`].
pub const PER_ADDRESS: usize = 10;
const ADDRESS_WINDOW: Duration = Duration::from_secs(60);
/// Failed combinations of one split in [`FAILURE_WINDOW_MS`], on every node of the store.
pub const FAILURES_PER_SET: i64 = 5;
const FAILURE_WINDOW_MS: i64 = 3_600_000;

/// `ROLL_SECRET_KEY_SEALED=<set id>:<threshold>`: which split unseals this node (`roll secrets
/// key split` prints it). Naming the split keeps the first caller from choosing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sealed {
    pub set: [u8; 8],
    pub threshold: u8,
}

impl Sealed {
    pub fn parse(text: &str) -> std::result::Result<Sealed, String> {
        let bad = || format!("ROLL_SECRET_KEY_SEALED is `<set id>:<threshold>` (16 hex characters, then 2 to 255), as roll secrets key split prints it; got `{}`", text.trim());
        let (set, threshold) = text.trim().split_once(':').ok_or_else(bad)?;
        let set = set.trim().to_ascii_lowercase();
        if set.len() != 16 {
            return Err(bad());
        }
        let mut id = [0u8; 8];
        for (i, b) in id.iter_mut().enumerate() {
            *b = u8::from_str_radix(&set[i * 2..i * 2 + 2], 16).map_err(|_| bad())?;
        }
        let threshold: u8 = threshold.trim().parse().map_err(|_| bad())?;
        if threshold < 2 {
            return Err(bad());
        }
        Ok(Sealed { set: id, threshold })
    }

    /// The split's id as `ROLL_SECRET_KEY_SEALED` names it.
    pub fn set_hex(set: &[u8; 8]) -> String {
        set.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// What the store says of a deployment key.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyCheck {
    /// It's the key recorded, or it opens a project's data key.
    Matches,
    /// Nothing to compare with yet (a new store).
    Unknown,
    Wrong,
}

/// Whether `keys` is this store's deployment key: the id recorded by [`record_key`] (the current
/// key's, or the previous one's during a rotation), else a project's data key opening with it.
pub fn check_key(db: &Db, keys: &Keyring) -> Result<KeyCheck> {
    let recorded: Option<String> = db.read(|c| c.query_row("SELECT kek_id FROM deployment_key WHERE id = 1", (), |r| r.get(0)).optional())?;
    if recorded.as_deref().is_some_and(|r| r == keys.current_id() || Some(r) == keys.previous_id()) {
        return Ok(KeyCheck::Matches);
    }
    let rows = db.read(|c| c.query("SELECT project_id, dek FROM projects LIMIT 100", (), |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))))?;
    let opens = |(p, d): &(String, String)| match kms::sealed_by(d) {
        Some(id) => id == keys.current_id() || Some(id.as_str()) == keys.previous_id(),
        None => keys.open_dek(p, d).is_ok(),
    };
    Ok(match (rows.iter().any(opens), recorded.is_none() && rows.is_empty()) {
        (true, _) => KeyCheck::Matches,
        (false, true) => KeyCheck::Unknown,
        (false, false) => KeyCheck::Wrong,
    })
}

/// Record the deployment key's id when it's the store's (or the store is new), so a sealed start
/// can tell the right key from a wrong one before anything opens. A key that opens nothing here
/// isn't recorded over the store's.
pub(crate) fn record_key(db: &Db, keys: &Keyring) -> Result<()> {
    if check_key(db, keys)? == KeyCheck::Wrong {
        tracing::warn!(key = keys.current_id(), "roll secrets: this deployment key opens none of the store's data keys (a wrong ROLL_SECRET_KEY?)");
        return Ok(());
    }
    let now = db.now_ms()?;
    db.write(|c| {
        c.execute(
            "INSERT INTO deployment_key (id, kek_id, updated_at) VALUES (1, ?, ?) ON CONFLICT (id) DO UPDATE SET kek_id = excluded.kek_id, updated_at = excluded.updated_at WHERE deployment_key.kek_id <> excluded.kek_id",
            code_sql::params![keys.current_id(), now],
        )
    })?;
    Ok(())
}

struct Pending {
    at: Instant,
    index: u8,
    share: Zeroizing<String>,
}

#[derive(Default)]
struct Progress {
    shares: Vec<Pending>,
    addresses: HashMap<Option<IpAddr>, VecDeque<Instant>>,
    unsealed: bool,
}

/// A sealed node's unseal: the shares posted so far, and where the key goes once they combine.
pub struct Unsealer {
    sealed: Sealed,
    db: Db,
    progress: Mutex<Progress>,
    key: Mutex<Option<tokio::sync::oneshot::Sender<Zeroizing<String>>>>,
}

/// An answer: `{sealed, have, need}`, with `error` when the share was refused.
struct Answer(StatusCode, Json, Option<u64>);

impl IntoResponse for Answer {
    fn into_response(self) -> Response {
        let mut r = (self.0, axum::Json(self.1)).into_response();
        if let Some(s) = self.2 {
            r.headers_mut().insert("retry-after", s.into());
        }
        r
    }
}

impl Unsealer {
    /// The unsealer for `sealed`, checking keys against `db` (roll secrets' store), and where the
    /// key arrives.
    pub fn new(sealed: Sealed, db: Db) -> Result<(Arc<Unsealer>, tokio::sync::oneshot::Receiver<Zeroizing<String>>)> {
        db.ensure_schema(crate::SCHEMA)?;
        db.ensure_schema(SCHEMA)?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        Ok((Arc::new(Unsealer { sealed, db, progress: Mutex::default(), key: Mutex::new(Some(tx)) }), rx))
    }

    fn need(&self) -> usize {
        self.sealed.threshold as usize
    }

    fn status(&self, p: &mut Progress) -> Json {
        p.shares.retain(|s| s.at.elapsed() < SHARE_TTL);
        json!({ "sealed": !p.unsealed, "have": if p.unsealed { self.need() } else { p.shares.len() }, "need": self.need() })
    }

    fn refuse(&self, p: &mut Progress, status: StatusCode, error: String) -> Answer {
        let mut body = self.status(p);
        body["error"] = error.into();
        Answer(status, body, None)
    }

    /// Start over: what was given is dropped (and zeroized).
    fn reset(&self, why: &str) {
        let mut p = self.progress.lock().unwrap();
        p.shares.clear();
        tracing::warn!(reason = why, "roll secrets: unseal progress reset");
    }

    async fn failures(&self) -> Result<(i64, i64)> {
        let (db, set) = (self.db.clone(), Sealed::set_hex(&self.sealed.set));
        blocking(move || {
            let now = db.now_ms()?;
            let (n, oldest) = db.read(|c| c.query_row("SELECT COUNT(*), MIN(at) FROM unseal_failures WHERE set_id = ? AND at > ?", code_sql::params![set, now - FAILURE_WINDOW_MS], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?))))?;
            Ok((n, oldest.map_or(0, |o| o + FAILURE_WINDOW_MS - now)))
        })
        .await
    }

    async fn failed(&self, why: &str) {
        self.reset(why);
        let (db, set) = (self.db.clone(), Sealed::set_hex(&self.sealed.set));
        let wrote = blocking(move || {
            let now = db.now_ms()?;
            db.write(|c| {
                c.execute("DELETE FROM unseal_failures WHERE at <= ?", [now - FAILURE_WINDOW_MS])?;
                c.execute("INSERT INTO unseal_failures (set_id, at) VALUES (?, ?)", code_sql::params![set, now])
            })?;
            Ok(())
        })
        .await;
        if let Err(e) = wrote {
            tracing::error!("roll secrets: recording a failed unseal: {e}");
        }
    }

    /// One share from `from`.
    async fn submit(&self, from: Option<IpAddr>, text: &str) -> Answer {
        {
            let mut p = self.progress.lock().unwrap();
            if p.unsealed {
                return Answer(StatusCode::OK, self.status(&mut p), None);
            }
            // Addresses that went quiet go, so many of them can't grow this without bound.
            if p.addresses.len() > 1024 {
                p.addresses.retain(|_, t| t.back().is_some_and(|t| t.elapsed() < ADDRESS_WINDOW));
            }
            let tries = p.addresses.entry(from).or_default();
            while tries.front().is_some_and(|t| t.elapsed() >= ADDRESS_WINDOW) {
                tries.pop_front();
            }
            if tries.len() >= PER_ADDRESS {
                let wait = tries.front().map_or(ADDRESS_WINDOW, |t| ADDRESS_WINDOW.saturating_sub(t.elapsed()));
                let mut a = self.refuse(&mut p, StatusCode::TOO_MANY_REQUESTS, format!("too many shares from this address: try again in {} s", wait.as_secs() + 1));
                a.2 = Some(wait.as_secs() + 1);
                return a;
            }
            tries.push_back(Instant::now());
        }
        match self.failures().await {
            Ok((n, wait_ms)) if n >= FAILURES_PER_SET => {
                let wait = (wait_ms.max(0) / 1000 + 1) as u64;
                let mut a = self.refuse(&mut self.progress.lock().unwrap(), StatusCode::TOO_MANY_REQUESTS, format!("{n} failed unseals of this split in the last hour: try again in {wait} s"));
                a.2 = Some(wait);
                return a;
            }
            Ok(_) => {}
            Err(e) => return self.refuse(&mut self.progress.lock().unwrap(), StatusCode::SERVICE_UNAVAILABLE, format!("the store can't be read: {e}")),
        }
        let shares = {
            let mut p = self.progress.lock().unwrap();
            self.status(&mut p);
            let share = match Share::parse(p.shares.len() + 1, text) {
                Ok(s) => s,
                // A typo: say so, and keep what the others gave.
                Err(_) => return self.refuse(&mut p, StatusCode::BAD_REQUEST, "that isn't a roll key share, or it's damaged (mistyped or cut short?)".into()),
            };
            if (share.set, share.threshold) != (self.sealed.set, self.sealed.threshold) {
                drop(p);
                self.reset("a share of another split than ROLL_SECRET_KEY_SEALED names");
                return self.refuse(&mut self.progress.lock().unwrap(), StatusCode::BAD_REQUEST, "that share is of another split than this node's (ROLL_SECRET_KEY_SEALED): every share given so far was dropped".into());
            }
            if p.shares.iter().any(|s| s.index == share.index) {
                return self.refuse(&mut p, StatusCode::BAD_REQUEST, format!("share #{} was already given", share.index));
            }
            p.shares.push(Pending { at: Instant::now(), index: share.index, share: Zeroizing::new(text.trim().to_string()) });
            if p.shares.len() < self.need() {
                tracing::info!(have = p.shares.len(), need = self.need(), "roll secrets: unseal share accepted");
                return Answer(StatusCode::OK, self.status(&mut p), None);
            }
            std::mem::take(&mut p.shares)
        };
        // Enough: combine, and check the key against the store.
        let texts: Vec<&str> = shares.iter().map(|s| s.share.as_str()).collect();
        let key = match roll_keyshares::combine(&texts) {
            Ok(k) => k,
            Err(e) => {
                self.failed(&format!("the shares don't combine: {e}")).await;
                return self.refuse(&mut self.progress.lock().unwrap(), StatusCode::BAD_REQUEST, "these shares don't combine into the key they were split from (one was altered): every share was dropped".into());
            }
        };
        drop(shares);
        let key = match std::str::from_utf8(&key).ok().map(|k| Zeroizing::new(k.trim().to_string())) {
            Some(k) => k,
            None => {
                self.failed("the combined key isn't text").await;
                return self.refuse(&mut self.progress.lock().unwrap(), StatusCode::BAD_REQUEST, "the shares give a key that isn't a deployment key: every share was dropped".into());
            }
        };
        let keys = match Keyring::new(&key, None) {
            Ok(k) => k,
            Err(_) => {
                self.failed("the combined key is shorter than 32 characters").await;
                return self.refuse(&mut self.progress.lock().unwrap(), StatusCode::BAD_REQUEST, "the shares give a key that isn't a deployment key: every share was dropped".into());
            }
        };
        let db = self.db.clone();
        match blocking(move || check_key(&db, &keys)).await {
            Ok(KeyCheck::Wrong) => {
                self.failed("the combined key isn't the store's deployment key").await;
                return self.refuse(&mut self.progress.lock().unwrap(), StatusCode::BAD_REQUEST, "the shares give a key that isn't this store's deployment key (an older split?): every share was dropped".into());
            }
            Ok(KeyCheck::Unknown) => tracing::info!("roll secrets: unsealed with a key the store has nothing to check against yet (a new store)"),
            Ok(KeyCheck::Matches) => tracing::info!("roll secrets: unsealed"),
            Err(e) => {
                self.reset("the store couldn't be read to check the key");
                return self.refuse(&mut self.progress.lock().unwrap(), StatusCode::SERVICE_UNAVAILABLE, format!("the key couldn't be checked against the store ({e}): every share was dropped"));
            }
        }
        let mut p = self.progress.lock().unwrap();
        p.unsealed = true;
        if let Some(tx) = self.key.lock().unwrap().take() {
            let _ = tx.send(key);
        }
        Answer(StatusCode::OK, self.status(&mut p), None)
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| crate::Error::Internal(e.to_string()))?
}

async fn status(State(u): State<Arc<Unsealer>>) -> Answer {
    Answer(StatusCode::OK, u.status(&mut u.progress.lock().unwrap()), None)
}

async fn unseal(State(u): State<Arc<Unsealer>>, req: Request) -> Answer {
    let from = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
    #[derive(serde::Deserialize)]
    struct Body {
        share: String,
    }
    let body = axum::body::to_bytes(req.into_body(), 16 << 10).await.ok().map(|b| Zeroizing::new(b.to_vec()));
    let Some(share) = body.and_then(|b| serde_json::from_slice::<Body>(&b).ok()).map(|b| Zeroizing::new(b.share)) else {
        return u.refuse(&mut u.progress.lock().unwrap(), StatusCode::BAD_REQUEST, "post {\"share\": \"rlks_…\"}".into());
    };
    u.submit(from, &share).await
}

/// What a sealed node serves: the unseal, and 503 `sealed` for everything else (health
/// included, so load balancers keep it out).
pub fn sealed_router(u: Arc<Unsealer>) -> Router {
    Router::new().route("/v1/secrets/unseal", get(status).post(unseal)).with_state(u).fallback(|| async {
        let body = json!({ "error": "rolld is sealed: its deployment key is split into shares (ROLL_SECRET_KEY_SEALED); roll secrets unseal gives it one", "sealed": true, "status": "sealed" });
        (StatusCode::SERVICE_UNAVAILABLE, axum::Json(body))
    })
}

/// The same route on a node that isn't sealed (or no longer is): nothing to give.
pub fn unsealed_router() -> Router {
    let answer = || async { axum::Json(json!({ "sealed": false })) };
    Router::new().route("/v1/secrets/unseal", get(answer).post(answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt as _;

    const KEY: &str = "fake-deployment-key-for-tests-only-0000";
    const OTHER: &str = "another-fake-key-for-tests-only-11111111";

    fn sealed_for(shares: &[String]) -> Sealed {
        let s = Share::parse(1, &shares[0]).unwrap();
        Sealed { set: s.set, threshold: s.threshold }
    }

    async fn post(u: &Arc<Unsealer>, from: [u8; 4], share: &str) -> (u16, Json) {
        let mut req = Request::post("/v1/secrets/unseal").header("content-type", "application/json").body(axum::body::Body::from(json!({ "share": share }).to_string())).unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::from((from, 1234))));
        let res = sealed_router(u.clone()).oneshot(req).await.unwrap();
        let status = res.status().as_u16();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("rlks_"), "an answer echoes a share");
        (status, serde_json::from_slice(&body).unwrap())
    }

    /// A store opened with one key refuses a split of another (and keeps refusing that split
    /// once it failed often enough, across a restart); one address can't post without limit;
    /// the store's own split unseals it.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_stores_key_unseals_and_attempts_are_limited() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "unseal").unwrap();
        db.ensure_schema(crate::SCHEMA).unwrap();
        db.ensure_schema(SCHEMA).unwrap();
        record_key(&db, &Keyring::new(KEY, None).unwrap()).unwrap();
        let other: Vec<String> = roll_keyshares::split(OTHER.as_bytes(), 2, 2).unwrap();
        let (u, _) = Unsealer::new(sealed_for(&other), db.clone()).unwrap();
        for n in 0..FAILURES_PER_SET {
            let from = [10, 0, 0, n as u8];
            assert_eq!(post(&u, from, &other[0]).await, (200, json!({ "sealed": true, "have": 1, "need": 2 })));
            let (status, body) = post(&u, from, &other[1]).await;
            assert_eq!((status, &body["have"]), (400, &json!(0)), "a key that isn't the store's: {body}");
        }
        // Restarted: the split's failures are in the store.
        let (u, _) = Unsealer::new(sealed_for(&other), db.clone()).unwrap();
        assert_eq!(post(&u, [10, 0, 1, 1], &other[0]).await.0, 429);

        let mine: Vec<String> = roll_keyshares::split(KEY.as_bytes(), 3, 2).unwrap();
        let (u, key) = Unsealer::new(sealed_for(&mine), db).unwrap();
        for _ in 0..PER_ADDRESS {
            assert_eq!(post(&u, [10, 0, 2, 1], "rlks_not-a-share").await.0, 400);
        }
        assert_eq!(post(&u, [10, 0, 2, 1], &mine[0]).await.0, 429, "one address is limited");
        assert_eq!(post(&u, [10, 0, 2, 2], &mine[2]).await.1["have"], 1);
        assert_eq!(post(&u, [10, 0, 2, 2], &mine[0]).await, (200, json!({ "sealed": false, "have": 2, "need": 2 })));
        assert_eq!(key.await.unwrap().as_str(), KEY);
    }
}
