//! `roll secrets agent` (docs/secrets.md, "A local agent"): the Lambda extension's surface
//! (sdk/lambda-extension) on any machine, as AWS's Secrets Manager Agent is for EC2, ECS and EKS.
//! An app reads its environments' values from 127.0.0.1 with a token header. The agent keeps each
//! folder it was asked for and follows the folder's digest (a long poll that decrypts, issues and
//! audits nothing), reading the values again only when the folder moved, so an app's read waits on
//! roll only for a folder it hasn't read yet or one roll hasn't confirmed within the TTL.
//!
//! It logs in as the rest of `roll secrets` does (`secrets_api`): `ROLL_TOKEN`, or a workload
//! identity's login (`ROLL_IDENTITY` with an OIDC token file the platform refreshes) that renews
//! itself before it ends and after a 401 (`Relogin`). What a preview's or sandbox's identity may
//! read is roll's to decide: the agent asks for nothing more.
//!
//! A value never goes in a log, an error or a `Debug`: the agent logs request lines (no query
//! string), folders, keys, lease ids and roll's error messages.

use super::{At, Source, enc, locate, now_ms};
use crate::Api;
use anyhow::Context;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Serve an environment's values on 127.0.0.1 from a cache, as the Lambda extension does
/// (`GET /secrets?env=&path=`, `GET /secrets/KEY?env=&path=`, `GET /health`). Every request
/// carries `X-Roll-Token` with the value of `--token-file` (or `ROLL_AGENT_TOKEN`), so a server-side
/// request forgery in the app, which can't set headers, reads nothing. The agent logs in with
/// `ROLL_IDENTITY` (a workload identity, `ROLL_OIDC_TOKEN_FILE` read again at each login) or
/// `ROLL_TOKEN`.
#[derive(clap::Args)]
pub struct LocalAgent {
    /// The port on 127.0.0.1 (the agent never listens anywhere else). 2773 is the Lambda
    /// extension's; 0 picks a free one and prints it.
    #[arg(long, env = "ROLL_AGENT_PORT", default_value_t = 2773)]
    port: u16,
    /// The file holding what requests send in `X-Roll-Token`; made with a random value (0600) when
    /// it doesn't exist. Default: `$XDG_RUNTIME_DIR/roll/agent-token`. `ROLL_AGENT_TOKEN` gives the
    /// value itself instead.
    #[arg(long, env = "ROLL_AGENT_TOKEN_FILE", value_name = "FILE")]
    token_file: Option<std::path::PathBuf>,
    /// Folders read before the agent answers: `production:/,staging:/api`.
    #[arg(long, env = "ROLL_PREFETCH", value_delimiter = ',', value_name = "ENV:PATH")]
    prefetch: Vec<String>,
    /// How long a folder is answered from memory after roll last confirmed it (its digest's long
    /// poll confirms it every 25 s while roll answers): seconds, or 30s, 5m, 1h. 0 turns the cache
    /// off.
    #[arg(long, env = "ROLL_CACHE_TTL", default_value = "300")]
    ttl: String,
    /// How many folders are kept; past it, the one read longest ago goes. 0: no cap.
    #[arg(long, env = "ROLL_CACHE_SIZE", default_value_t = 1000)]
    cache_size: usize,
}

/// The header every request carries (the Lambda extension's).
const HEADER: &str = "x-roll-token";
/// How long a request waits on roll.
const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// After a failed poll or read, or a server that doesn't hold a long poll: the wait before the next.
const RETRY: Duration = Duration::from_secs(5);
/// Dynamic credentials' leases are renewed at least this often while their folder is kept (as
/// `roll run --watch` renews them).
const RENEW_EVERY: Duration = super::RENEW_EVERY;

pub async fn run(api: &Api, args: &super::AtArgs, a: LocalAgent) -> anyhow::Result<()> {
    let token = request_token(a.token_file.as_deref())?;
    let ttl = Duration::from_millis(crate::fallback::duration_ms(&a.ttl).context("--ttl")?.max(0) as u64);
    let (at, project) = locate(api, args).await?;
    let mut prefetch = vec![];
    for p in a.prefetch.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        let (env, path) = p.split_once(':').unwrap_or((p, ""));
        anyhow::ensure!(!env.is_empty(), "--prefetch is ENV:PATH,…: {p:?} names no environment");
        prefetch.push(Folder { env: env.to_string(), path: if path.is_empty() { "/".into() } else { path.to_string() } });
    }
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, a.port)).await.with_context(|| format!("127.0.0.1:{} is taken: pick another with --port", a.port))?;
    let addr = listener.local_addr()?;
    let store = Arc::new(Store { api: api.with_header(None), project: format!("/v1/secrets/projects/{}", enc(&project)), ttl, size: a.cache_size, slots: Default::default() });
    if !prefetch.is_empty() {
        let reads = prefetch.iter().map(|f| {
            let store = store.clone();
            async move {
                match tokio::time::timeout(READ_TIMEOUT, store.read(f)).await {
                    Ok(Ok(r)) if r.problems.is_empty() => {}
                    Ok(Ok(r)) => eprintln!("roll secrets agent: prefetch {f}: {}", unresolved(f, &r)),
                    Ok(Err(e)) => eprintln!("roll secrets agent: prefetch {f}: {e:#}"),
                    Err(_) => eprintln!("roll secrets agent: prefetch {f}: no answer in {} s", READ_TIMEOUT.as_secs()),
                }
            }
        });
        futures::future::join_all(reads).await;
    }
    let agent = Arc::new(Agent { store, token, defaults: defaults(&at) });
    let router = axum::Router::new()
        .route("/secrets", axum::routing::get(folder_values))
        .route("/secrets/{key}", axum::routing::get(key_value))
        .route("/health", axum::routing::get(health))
        .layer(axum::middleware::from_fn_with_state(agent.clone(), guard))
        .with_state(agent);
    eprintln!("roll secrets agent {}: listening on {addr} for {} ({project})", env!("CARGO_PKG_VERSION"), at.label.as_deref().unwrap_or(&project));
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// The environment and folder a request that names none reads: `-e`, `ROLL_ENV` or the setup's
/// (never the built-in `development`), and `--path`'s folder, else `/`.
fn defaults(at: &At) -> (Option<String>, String) {
    ((at.from.1 != Source::Default).then(|| at.env.clone()), at.path.clone())
}

/// What requests must send: `ROLL_AGENT_TOKEN`, else the token file's, made with 32 random bytes
/// (0600) when it doesn't exist yet. Only the file's path is ever printed.
fn request_token(file: Option<&std::path::Path>) -> anyhow::Result<String> {
    if let Some(t) = std::env::var("ROLL_AGENT_TOKEN").ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        return Ok(t);
    }
    let file = match file {
        Some(f) => f.to_path_buf(),
        None => std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).map(|d| std::path::PathBuf::from(d).join("roll").join("agent-token")).context("where's the request token? --token-file FILE (made when it doesn't exist) or ROLL_AGENT_TOKEN")?,
    };
    match std::fs::read_to_string(&file) {
        Ok(t) => {
            let t = t.trim().to_string();
            anyhow::ensure!(!t.is_empty(), "{} is empty: remove it and the agent makes one, or write a token in it", file.display());
            Ok(t)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut b = [0u8; 32];
            getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("system randomness: {e}"))?;
            let t: String = b.iter().map(|x| format!("{x:02x}")).collect();
            if let Some(dir) = file.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            }
            super::write_private(&file, &format!("{t}\n"))?;
            eprintln!("roll secrets agent: made {} (0600); requests send its contents in X-Roll-Token", file.display());
            Ok(t)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
    }
}

/// One environment's folder.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Folder {
    env: String,
    path: String,
}

impl std::fmt::Display for Folder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.env, self.path)
    }
}

/// One values answer. No `Debug` beyond the keys: it holds values.
struct Read {
    values: BTreeMap<String, String>,
    /// Keys that don't resolve, and why.
    problems: Vec<(String, String)>,
    /// Keys set but not given to this caller (sensitive, or an untrusted caller's).
    withheld: Vec<String>,
    leases: Vec<String>,
    /// The folder's digest, taken by roll just before it read the values.
    digest: String,
}

impl std::fmt::Debug for Read {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Read").field("keys", &self.values.keys().collect::<Vec<_>>()).field("problems", &self.problems.len()).field("leases", &self.leases).finish()
    }
}

impl Read {
    fn from(v: &Value) -> Read {
        let strings = |k: &str| v[k].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
        Read {
            values: v["values"].as_object().into_iter().flatten().map(|(k, x)| (k.clone(), x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string()))).collect(),
            problems: v["problems"].as_array().into_iter().flatten().map(|p| (p["key"].as_str().unwrap_or_default().to_string(), p["problem"].as_str().unwrap_or_default().to_string())).collect(),
            withheld: strings("withheld"),
            leases: strings("leases"),
            digest: v["digest"].as_str().unwrap_or_default().to_string(),
        }
    }
}

/// A folder as kept: its values, when roll last confirmed them, and for dynamic credentials how
/// long their leases allow.
struct Kept {
    read: Arc<Read>,
    confirmed: Instant,
    leases: Option<Leases>,
}

#[derive(Clone, Copy)]
struct Leases {
    /// Kept until a fifth of the soonest lease's life (at least a minute) is left.
    fresh_until: Instant,
    renew_at: Instant,
}

/// The folders read so far, each followed by a task of its own while it's kept.
struct Store {
    api: Api,
    /// `/v1/secrets/projects/{project}`.
    project: String,
    ttl: Duration,
    size: usize,
    slots: std::sync::Mutex<HashMap<Folder, Arc<Slot>>>,
}

struct Slot {
    /// Held while a request reads the folder from roll, so requests for one folder read it once.
    kept: tokio::sync::Mutex<Option<Kept>>,
    used: std::sync::Mutex<Instant>,
    follow: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Store {
    fn fresh(&self, k: &Kept, now: Instant) -> bool {
        now < k.confirmed + self.ttl && k.leases.is_none_or(|l| now < l.fresh_until)
    }

    /// The folder's values: from memory while fresh, else from roll (and kept, unless a key doesn't
    /// resolve, or a dynamic credential's lease can't be renewed).
    async fn read(self: &Arc<Self>, f: &Folder) -> anyhow::Result<Arc<Read>> {
        if self.ttl.is_zero() {
            return Ok(Arc::new(self.fetch(f).await?));
        }
        let slot = self.slot(f);
        let mut kept = slot.kept.lock().await;
        if let Some(k) = kept.as_ref().filter(|k| self.fresh(k, Instant::now())) {
            return Ok(k.read.clone());
        }
        let read = Arc::new(self.fetch(f).await?);
        *kept = self.keep(&read).await;
        if kept.is_some() {
            self.follow(f, &slot);
        }
        Ok(read)
    }

    /// The slot for `f`, made (evicting the one read longest ago past the cap) if it's new.
    fn slot(&self, f: &Folder) -> Arc<Slot> {
        let mut slots = self.slots.lock().expect("slots");
        let now = Instant::now();
        if let Some(s) = slots.get(f) {
            *s.used.lock().expect("used") = now;
            return s.clone();
        }
        if self.size > 0 && slots.len() >= self.size {
            let oldest = slots.iter().min_by_key(|(_, s)| *s.used.lock().expect("used")).map(|(k, _)| k.clone());
            if let Some(gone) = oldest.and_then(|k| slots.remove(&k))
                && let Some(h) = gone.follow.lock().expect("follow").take()
            {
                h.abort();
            }
        }
        let s = Arc::new(Slot { kept: Default::default(), used: std::sync::Mutex::new(now), follow: Default::default() });
        slots.insert(f.clone(), s.clone());
        s
    }

    async fn fetch(&self, f: &Folder) -> anyhow::Result<Read> {
        let v = self.api.call(Method::GET, &format!("{}/environments/{}/values?path={}&digest=1", self.project, enc(&f.env), enc(&f.path)), None).await?;
        Ok(Read::from(&v))
    }

    /// `read` as kept from now, or nothing when it mustn't be.
    async fn keep(&self, read: &Arc<Read>) -> Option<Kept> {
        if !read.problems.is_empty() {
            return None;
        }
        let leases = if read.leases.is_empty() { None } else { Some(self.renew(&read.leases).await?) };
        Some(Kept { read: read.clone(), confirmed: Instant::now(), leases })
    }

    /// Renews each lease (its link's TTL from now), which says when it ends: the values answer
    /// doesn't. Nothing when one can't be renewed or has under a minute left.
    async fn renew(&self, leases: &[String]) -> Option<Leases> {
        let mut life: Option<Duration> = None;
        for id in leases {
            match self.api.call(Method::POST, &format!("{}/leases/{}/renew", self.project, enc(id)), None).await {
                Ok(v) => {
                    let left = Duration::from_millis((v["expires_at"].as_i64().unwrap_or(0) - now_ms()).max(0) as u64);
                    life = Some(life.map_or(left, |l| l.min(left)));
                }
                Err(e) => {
                    eprintln!("roll secrets agent: renewing lease {id}: {e:#}");
                    return None;
                }
            }
        }
        let life = life?;
        let now = Instant::now();
        let fresh = life.checked_sub((life / 5).max(Duration::from_secs(60)))?;
        Some(Leases { fresh_until: now + fresh, renew_at: now + (life / 2).min(RENEW_EVERY) })
    }

    /// Follows `f`'s digest while it's kept (one task per folder).
    fn follow(self: &Arc<Self>, f: &Folder, slot: &Arc<Slot>) {
        let mut h = slot.follow.lock().expect("follow");
        if h.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        let (me, f, slot) = (self.clone(), f.clone(), slot.clone());
        *h = Some(tokio::spawn(async move { me.following(&f, &slot).await }));
    }

    /// Long-polls the digest: unchanged confirms the kept values for another TTL; moved reads them
    /// again. Leases are renewed when due. Ends when the folder stops being kept.
    async fn following(&self, f: &Folder, slot: &Slot) {
        loop {
            let (digest, leases, lease_times) = match slot.kept.lock().await.as_ref() {
                Some(k) => (k.read.digest.clone(), k.read.leases.clone(), k.leases),
                None => return,
            };
            let renew_at = lease_times.map(|l| tokio::time::Instant::from_std(l.renew_at));
            let asked = Instant::now();
            tokio::select! {
                _ = tokio::time::sleep_until(renew_at.unwrap_or_else(tokio::time::Instant::now)), if renew_at.is_some() => {
                    let renewed = self.renew(&leases).await;
                    let mut kept = slot.kept.lock().await;
                    // Unless it was read again meanwhile, with leases of its own.
                    if kept.as_ref().is_some_and(|k| k.read.digest == digest) {
                        match renewed {
                            Some(l) => kept.iter_mut().for_each(|k| k.leases = Some(l)),
                            // Its lease ends as it was: the next request after that reads it again.
                            None => {
                                *kept = None;
                                return;
                            }
                        }
                    }
                }
                polled = self.poll(f, &digest) => match polled {
                    Ok(now) if now == digest => {
                        if let Some(k) = slot.kept.lock().await.as_mut().filter(|k| k.read.digest == digest) {
                            k.confirmed = Instant::now();
                        }
                        // A server that answers at once (no long poll): not asked again at once.
                        if asked.elapsed() < Duration::from_secs(1) {
                            tokio::time::sleep(RETRY).await;
                        }
                    }
                    Ok(_) => match self.fetch(f).await {
                        Ok(read) => {
                            let read = Arc::new(read);
                            let next = self.keep(&read).await;
                            let stop = next.is_none();
                            *slot.kept.lock().await = next;
                            eprintln!("roll secrets agent: {f} changed; {}", if stop { "not kept (read again on its next request)" } else { "read again" });
                            if stop {
                                return;
                            }
                        }
                        Err(e) => {
                            eprintln!("roll secrets agent: {f} changed, but reading it failed ({e:#}); kept as it was until its TTL");
                            tokio::time::sleep(RETRY).await;
                        }
                    },
                    Err(e) => {
                        eprintln!("roll secrets agent: following {f}: {e:#}");
                        tokio::time::sleep(RETRY).await;
                    }
                },
            }
        }
    }

    /// The folder's digest once it moves from `since`, or after 25 s unchanged.
    async fn poll(&self, f: &Folder, since: &str) -> anyhow::Result<String> {
        let url = format!("{}/environments/{}/digest?path={}&since={}&wait=25", self.project, enc(&f.env), enc(&f.path), enc(since));
        let v = tokio::time::timeout(Duration::from_secs(40), self.api.call(Method::GET, &url, None)).await.map_err(|_| anyhow::anyhow!("no answer in 40 s"))??;
        Ok(v["digest"].as_str().unwrap_or_default().to_string())
    }
}

struct Agent {
    store: Arc<Store>,
    token: String,
    /// The environment and folder a request that names none reads.
    defaults: (Option<String>, String),
}

/// Equal, in time that doesn't depend on where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Every request but `/health` carries the token, and none comes through a proxy (`Forwarded`,
/// `X-Forwarded-For`: AWS's agent refuses them too). Each request is logged as a line, without its
/// query string; every answer is `no-store`.
async fn guard(State(a): State<Arc<Agent>>, req: Request, next: axum::middleware::Next) -> Response {
    let started = Instant::now();
    let (method, path) = (req.method().clone(), req.uri().path().to_string());
    let carried = req.headers().get(HEADER).is_some_and(|v| same(v.as_bytes(), a.token.as_bytes()));
    let proxied = req.headers().contains_key("x-forwarded-for") || req.headers().contains_key(header::FORWARDED);
    let mut res = if path != "/health" && !carried {
        refuse(StatusCode::UNAUTHORIZED, "unauthorized", "unauthorized")
    } else if proxied {
        refuse(StatusCode::FORBIDDEN, "forwarded", "requests through a proxy are refused")
    } else {
        next.run(req).await
    };
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    eprintln!("{method} {path} {} {}ms", res.status().as_u16(), started.elapsed().as_millis());
    res
}

/// An answer that isn't the values: `{"error", "code"}`.
struct Refusal(StatusCode, &'static str, String);

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (self.0, axum::Json(json!({ "error": self.2, "code": self.1 }))).into_response()
    }
}

fn refuse(status: StatusCode, code: &'static str, message: &str) -> Response {
    Refusal(status, code, message.to_string()).into_response()
}

/// The folder a request names (`?env=&path=`), or the defaults.
fn folder(a: &Agent, q: &HashMap<String, String>) -> Result<Folder, Refusal> {
    let env = q.get("env").filter(|e| !e.is_empty()).or(a.defaults.0.as_ref()).ok_or_else(|| Refusal(StatusCode::BAD_REQUEST, "", "name the environment: ?env=production (or start the agent with -e)".into()))?;
    let path = q.get("path").filter(|p| !p.is_empty()).unwrap_or(&a.defaults.1);
    Ok(Folder { env: env.clone(), path: path.clone() })
}

/// The folder's values, or the answer that says why not: roll's 403, 404, 409, 423 and 429 as
/// they are, anything else a 502. roll's messages name keys and folders, never values.
async fn values(a: &Agent, f: &Folder) -> Result<Arc<Read>, Refusal> {
    let failed = |status: StatusCode, message: String| {
        eprintln!("roll secrets agent: {f}: {message}");
        let code = match status.as_u16() {
            403 => "forbidden",
            404 => "not_found",
            409 => "conflict",
            423 => "locked",
            429 => "rate_limited",
            _ => "unavailable",
        };
        Refusal(status, code, format!("reading {f}: {message}"))
    };
    match tokio::time::timeout(READ_TIMEOUT, a.store.read(f)).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => {
            let status = e.chain().find_map(|c| c.downcast_ref::<crate::ApiStatus>()).map(|s| s.status.as_u16()).filter(|s| [403, 404, 409, 423, 429].contains(s));
            Err(failed(status.and_then(|s| StatusCode::from_u16(s).ok()).unwrap_or(StatusCode::BAD_GATEWAY), format!("{e:#}")))
        }
        Err(_) => Err(failed(StatusCode::BAD_GATEWAY, format!("no answer from roll in {} s", READ_TIMEOUT.as_secs()))),
    }
}

fn unresolved(f: &Folder, r: &Read) -> String {
    let keys: Vec<String> = r.problems.iter().map(|(k, p)| format!("{k} ({p})")).collect();
    format!("{f} doesn't resolve: {}", keys.join(", "))
}

/// `GET /secrets`: the folder's values as a JSON object, only when every key resolves.
async fn folder_values(State(a): State<Arc<Agent>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let f = match folder(&a, &q) {
        Ok(f) => f,
        Err(r) => return r.into_response(),
    };
    match values(&a, &f).await {
        Ok(r) if r.problems.is_empty() => axum::Json(&r.values).into_response(),
        Ok(r) => refuse(StatusCode::CONFLICT, "unresolved", &unresolved(&f, &r)),
        Err(r) => r.into_response(),
    }
}

/// `GET /secrets/{key}`: one value, as text.
async fn key_value(State(a): State<Arc<Agent>>, Path(key): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    let f = match folder(&a, &q) {
        Ok(f) => f,
        Err(r) => return r.into_response(),
    };
    let r = match values(&a, &f).await {
        Ok(r) => r,
        Err(r) => return r.into_response(),
    };
    if let Some((_, p)) = r.problems.iter().find(|(k, _)| *k == key) {
        return refuse(StatusCode::CONFLICT, "unresolved", &format!("{key} in {f} doesn't resolve: {p}"));
    }
    match r.values.get(&key) {
        Some(v) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], v.clone()).into_response(),
        None if r.withheld.contains(&key) => refuse(StatusCode::FORBIDDEN, "withheld", &format!("{key} in {f} isn't given to this login (sensitive, or an untrusted caller's)")),
        None => refuse(StatusCode::NOT_FOUND, "not_found", &format!("{f} has no key {key}")),
    }
}

/// `GET /health`: 200 while the agent's login works (logging in again if it has ended), else 503.
/// It needs no token (a probe can't read the file) and says nothing more.
async fn health(State(a): State<Arc<Agent>>) -> Response {
    match a.store.api.bearer().await {
        Ok(_) => axum::Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") })).into_response(),
        Err(_) => refuse(StatusCode::SERVICE_UNAVAILABLE, "login", "not logged in to roll"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read never answers past the TTL since roll last confirmed it, nor past a fifth of a
    /// dynamic credential's lease.
    #[test]
    fn a_kept_folder_is_fresh_within_its_ttl_and_its_leases() {
        let store = Store { api: Api { base: String::new(), http: reqwest::Client::new(), key: None, headers: vec![], login: None }, project: String::new(), ttl: Duration::from_secs(300), size: 0, slots: Default::default() };
        let read = Arc::new(Read::from(&json!({ "values": { "K": "fake-value" }, "digest": "d1" })));
        let now = Instant::now();
        let k = Kept { read: read.clone(), confirmed: now, leases: None };
        assert!(store.fresh(&k, now + Duration::from_secs(299)));
        assert!(!store.fresh(&k, now + Duration::from_secs(300)));
        let k = Kept { read, confirmed: now, leases: Some(Leases { fresh_until: now + Duration::from_secs(60), renew_at: now }) };
        assert!(store.fresh(&k, now + Duration::from_secs(59)));
        assert!(!store.fresh(&k, now + Duration::from_secs(61)));
        assert!(!format!("{:?}", k.read).contains("fake-value"), "Debug names keys only");
    }
}
