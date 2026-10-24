//! Review apps (docs/code/reviews.md, "External reviewers"): how any AI reviewer (Greptile,
//! CodeRabbit, an in-house bot) reviews changes in native roll repos. An owner registers an app
//! with a webhook URL and gets a token (shown once) and a signing secret.
//!
//! - roll sends signed webhooks: `change.opened`, `change.revised`, `thread.comment` and
//!   `review.requested` (always sent to required apps), with the change and the URLs to read its
//!   diff and files. Headers: `X-Roll-Event`, `X-Roll-Delivery`, `X-Roll-Timestamp` and
//!   `X-Roll-Signature-256: sha256=<hex HMAC-SHA256(secret, "{timestamp}.{body}")>`. A delivery
//!   that doesn't get a 2xx is retried with backoff (8 attempts over ~4 hours), then marked
//!   failed; every attempt is in the delivery log.
//! - The app reviews with its token through the same `/reviews` and `/threads` endpoints people
//!   use. It can read the repos it's installed on (private ones too) and nothing else, and can't
//!   write code.
//! - A `required` app owes every new head of every change a review: until it posts one, the
//!   change has a `review_pending` blocker.
//!
//! Webhook URLs must be public addresses (no loopback, private, link-local or metadata ranges)
//! unless `Config::allow_private_webhooks` (or an open local daemon) allows them; the check runs
//! on every connection, so DNS can't be rebound around it. Redirects aren't followed.

use super::{Reviewer, ReviewerKind};
use super::store::{AppRow, DeliveryRow};
use crate::model::{Owner, RepoRow};
use crate::{Caller, Code, Error, Result, hash_token, new_id};
use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::Sha256;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

pub const EVENTS: [&str; 4] = ["change.opened", "change.revised", "thread.comment", "review.requested"];
const MAX_ATTEMPTS: i64 = 8;
/// Longest wait between attempts.
const MAX_BACKOFF_MS: u64 = 6 * 3600 * 1000;
const TIMEOUT: Duration = Duration::from_secs(10);
/// How long a node holds a delivery it's sending (well past [`TIMEOUT`]).
const CLAIM_MS: i64 = 60_000;
const MAX_APPS_PER_OWNER: usize = 50;

/// The delivery worker's state.
#[derive(Default)]
pub struct Deliveries {
    running: AtomicBool,
    notify: Notify,
    client: Mutex<Option<reqwest::Client>>,
}

pub(crate) fn app_reviewer(a: &AppRow) -> Reviewer {
    Reviewer { kind: ReviewerKind::App, name: a.name.clone(), app: Some(a.id.clone()) }
}

fn new_secret() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("system randomness");
    format!("rls_{}", hex::encode(b))
}

fn new_app_token() -> String {
    let mut b = [0u8; 24];
    getrandom::fill(&mut b).expect("system randomness");
    format!("rla_{}", hex::encode(b))
}

/// `sha256=<hex>` of HMAC-SHA256(secret, "{timestamp}.{body}").
pub fn signature(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Loopback, private, link-local, CGNAT, multicast, unspecified and documentation ranges (and
/// IPv4 inside IPv6): not where a webhook may go.
pub fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            v.is_loopback()
                || v.is_private()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_multicast()
                || v.is_broadcast()
                || v.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || o[0] >= 240
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return private_ip(IpAddr::V4(v4));
            }
            let s = v.segments();
            // IPv4 carried inside IPv6: NAT64 (64:ff9b::/96, also the local-use 64:ff9b:1::/48)
            // and 6to4 (2002::/16) reach whatever IPv4 address they embed.
            let embedded = |hi: u16, lo: u16| std::net::Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
            if (s[0] == 0x64 && s[1] == 0xff9b && (s[2..6] == [0, 0, 0, 0] || s[2] == 1)) && private_ip(IpAddr::V4(embedded(s[6], s[7]))) {
                return true;
            }
            if s[0] == 0x2002 && private_ip(IpAddr::V4(embedded(s[1], s[2]))) {
                return true;
            }
            v.is_loopback() || v.is_unspecified() || v.is_multicast() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 || s[0] == 0x2001 && s[1] == 0xdb8
        }
    }
}

/// Resolves names, refusing private addresses.
pub(crate) struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = addrs.into_iter().filter(|a| !private_ip(a.ip())).collect();
            if public.is_empty() {
                return Err(format!("{host} resolves to a private address").into());
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Is `url`'s host a private address written out (an IP literal, or `localhost`)? Names that
/// resolve to one are refused when connecting ([`PublicOnly`]).
pub(crate) fn literal_private_host(url: &url::Url) -> bool {
    let literal = match url.host() {
        Some(url::Host::Ipv4(v)) => Some(IpAddr::V4(v)),
        Some(url::Host::Ipv6(v)) => Some(IpAddr::V6(v)),
        Some(url::Host::Domain(d)) => d.parse::<IpAddr>().ok(),
        None => None,
    };
    literal.is_some_and(private_ip) || url.host_str().is_some_and(|h| h.eq_ignore_ascii_case("localhost") || h.to_ascii_lowercase().ends_with(".localhost"))
}

pub(crate) fn check_url(raw: &str) -> Result<url::Url> {
    let u = url::Url::parse(raw.trim()).map_err(|e| Error::Invalid(format!("webhook_url: {e}")))?;
    if !matches!(u.scheme(), "https" | "http") {
        return Err(Error::Invalid("webhook_url must be http(s)".into()));
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err(Error::Invalid("webhook_url needs a host".into()));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(Error::Invalid("webhook_url can't carry credentials; verify X-Roll-Signature-256 instead".into()));
    }
    if raw.len() > 2000 {
        return Err(Error::Invalid("webhook_url is too long".into()));
    }
    Ok(u)
}

fn app_name(s: &str) -> Result<String> {
    let s = s.trim();
    if s.is_empty() || s.len() > 64 || !s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
        return Err(Error::Invalid("name: 1-64 of [A-Za-z0-9-_.]".into()));
    }
    Ok(s.to_string())
}

impl Code {
    /// An app token's caller (`rla_…`), whatever the daemon's auth mode: apps are always scoped.
    pub(crate) fn app_caller(&self, token: &str) -> Result<Option<Caller>> {
        if !token.starts_with("rla_") {
            return Ok(None);
        }
        let Some(a) = self.db.app_by_token(&hash_token(token))? else { return Err(Error::Unauthorized) };
        if !a.active {
            return Err(Error::Forbidden(format!("review app {} is disabled", a.name)));
        }
        Ok(Some(Caller::App { id: a.id, owner_id: a.owner_id, name: a.name, repos: Arc::new(a.repos) }))
    }

    /// Apps installed on `r`, active or not (callers check `active`).
    pub(crate) fn apps_for_repo(&self, r: &RepoRow) -> Result<Vec<AppRow>> {
        Ok(self.db.apps(Some(&r.owner_id))?.into_iter().filter(|a| a.repos.is_empty() || a.repos.contains(&r.id)).collect())
    }

    fn app_json(&self, a: &AppRow) -> Value {
        let repos: Vec<String> =
            a.repos.iter().filter_map(|id| self.db.repo_by_id(id).ok().flatten()).map(|r| r.full_name()).collect();
        let owner = self.db.owner_by_id(&a.owner_id).ok().flatten().map(|o| o.name).unwrap_or_default();
        json!({
            "id": a.id,
            "owner": owner,
            "name": a.name,
            "webhook_url": a.webhook_url,
            "events": a.events,
            "repos": repos,
            "all_repos": a.repos.is_empty(),
            "required": a.required,
            "active": a.active,
            "created_at": a.created_at,
            "updated_at": a.updated_at,
        })
    }

    fn manage_owner(&self, caller: &Caller, owner: &str) -> Result<Owner> {
        let o = self.owner_or_404(owner)?;
        match caller {
            Caller::Anonymous => Err(Error::Unauthorized),
            _ if Self::manages(caller, &o.id) => Ok(o),
            _ => Err(Error::Forbidden(format!("not allowed to manage {}'s review apps", o.name))),
        }
    }

    fn app_repos(&self, o: &Owner, v: Option<&Value>) -> Result<Option<Vec<String>>> {
        let Some(v) = v.filter(|v| !v.is_null()) else { return Ok(None) };
        let names: Vec<String> = serde_json::from_value(v.clone()).map_err(|_| Error::Invalid("repos is a list of repo names".into()))?;
        let mut ids = Vec::new();
        for n in names {
            let name = n.rsplit('/').next().unwrap_or(&n);
            if let Some((owner, _)) = n.split_once('/')
                && !owner.eq_ignore_ascii_case(&o.name)
            {
                return Err(Error::Invalid(format!("{n} isn't one of {}'s repos", o.name)));
            }
            let r = self.db.repo(&o.id, name)?.ok_or_else(|| Error::NotFound(format!("repository {}/{name}", o.name)))?;
            if !ids.contains(&r.id) {
                ids.push(r.id);
            }
        }
        Ok(Some(ids))
    }

    fn app_events(v: Option<&Value>) -> Result<Option<Vec<String>>> {
        let Some(v) = v.filter(|v| !v.is_null()) else { return Ok(None) };
        let events: Vec<String> = serde_json::from_value(v.clone()).map_err(|_| Error::Invalid("events is a list of event names".into()))?;
        if let Some(bad) = events.iter().find(|e| !EVENTS.contains(&e.as_str())) {
            return Err(Error::Invalid(format!("unknown event {bad}; events are {}", EVENTS.join(", "))));
        }
        Ok(Some(events))
    }

    fn bool_field(v: Option<&Value>, k: &str) -> Result<Option<bool>> {
        match v {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(Error::Invalid(format!("{k} must be true or false"))),
        }
    }

    /// `POST /owners/{owner}/review-apps`: the app, its token and its secret (shown once).
    pub fn create_app(&self, caller: &Caller, owner: &str, input: &Value) -> Result<Value> {
        let o = self.manage_owner(caller, owner)?;
        let name = app_name(input.get("name").and_then(Value::as_str).unwrap_or(""))?;
        let url = check_url(input.get("webhook_url").and_then(Value::as_str).ok_or_else(|| Error::Invalid("webhook_url is required".into()))?)?;
        if self.db.apps(Some(&o.id))?.len() >= MAX_APPS_PER_OWNER {
            return Err(Error::Unprocessable(format!("at most {MAX_APPS_PER_OWNER} review apps per owner")));
        }
        let t = crate::db::now();
        let a = AppRow {
            id: new_id("app"),
            owner_id: o.id.clone(),
            name: name.clone(),
            webhook_url: url.to_string(),
            events: Self::app_events(input.get("events"))?.unwrap_or_else(|| EVENTS[..3].iter().map(|s| s.to_string()).collect()),
            repos: self.app_repos(&o, input.get("repos"))?.unwrap_or_default(),
            required: Self::bool_field(input.get("required"), "required")?.unwrap_or(false),
            active: true,
            secret: new_secret(),
            created_at: t,
            updated_at: t,
        };
        let token = new_app_token();
        if !self.db.insert_app(&a, &hash_token(&token))? {
            return Err(Error::Conflict(format!("{} already has a review app called {name}", o.name)));
        }
        self.audit_app(&a, caller, "review_app_create")?;
        Ok(json!({ "app": self.app_json(&a), "token": token, "secret": a.secret }))
    }

    fn audit_app(&self, a: &AppRow, caller: &Caller, action: &str) -> Result<()> {
        let detail = json!({ "app": a.id, "name": a.name });
        let repos = if a.repos.is_empty() { self.db.repos(Some(&a.owner_id))?.into_iter().map(|r| r.id).collect() } else { a.repos.clone() };
        for r in repos {
            self.db.audit(&r, &caller.identity().unwrap_or_default(), action, &detail)?;
        }
        Ok(())
    }

    pub fn list_apps(&self, caller: &Caller, owner: &str) -> Result<Vec<Value>> {
        let o = self.manage_owner(caller, owner)?;
        Ok(self.db.apps(Some(&o.id))?.iter().map(|a| self.app_json(a)).collect())
    }

    /// An app its owner (or the admin) manages.
    fn managed_app(&self, caller: &Caller, id: &str) -> Result<AppRow> {
        let a = self.db.app(id)?.ok_or_else(|| Error::NotFound(format!("review app {id}")))?;
        match caller {
            Caller::Anonymous => Err(Error::Unauthorized),
            _ if Self::manages(caller, &a.owner_id) => Ok(a),
            // Other callers don't learn it exists.
            _ => Err(Error::NotFound(format!("review app {id}"))),
        }
    }

    pub fn get_app(&self, caller: &Caller, id: &str) -> Result<Value> {
        Ok(self.app_json(&self.managed_app(caller, id)?))
    }

    pub fn update_app(&self, caller: &Caller, id: &str, patch: &Value) -> Result<Value> {
        let mut a = self.managed_app(caller, id)?;
        let o = self.db.owner_by_id(&a.owner_id)?.ok_or_else(|| Error::NotFound("owner".into()))?;
        if let Some(n) = patch.get("name").and_then(Value::as_str) {
            a.name = app_name(n)?;
        }
        if let Some(u) = patch.get("webhook_url").and_then(Value::as_str) {
            a.webhook_url = check_url(u)?.to_string();
        }
        if let Some(e) = Self::app_events(patch.get("events"))? {
            a.events = e;
        }
        if let Some(r) = self.app_repos(&o, patch.get("repos"))? {
            a.repos = r;
        }
        if let Some(b) = Self::bool_field(patch.get("required"), "required")? {
            a.required = b;
        }
        if let Some(b) = Self::bool_field(patch.get("active"), "active")? {
            a.active = b;
        }
        if !self.db.update_app(&a)? {
            return Err(Error::Conflict(format!("{} already has a review app called {}", o.name, a.name)));
        }
        self.audit_app(&a, caller, "review_app_update")?;
        Ok(self.app_json(&a))
    }

    pub fn delete_app(&self, caller: &Caller, id: &str) -> Result<()> {
        let a = self.managed_app(caller, id)?;
        self.audit_app(&a, caller, "review_app_delete")?;
        self.db.delete_app(&a.id)?;
        Ok(())
    }

    /// A new token and secret; the old ones stop working.
    pub fn rotate_app(&self, caller: &Caller, id: &str) -> Result<Value> {
        let a = self.managed_app(caller, id)?;
        let (token, secret) = (new_app_token(), new_secret());
        self.db.rotate_app(&a.id, &secret, &hash_token(&token))?;
        self.audit_app(&a, caller, "review_app_rotate")?;
        Ok(json!({ "token": token, "secret": secret }))
    }

    pub fn app_deliveries(&self, caller: &Caller, id: &str) -> Result<Vec<Value>> {
        let a = self.managed_app(caller, id)?;
        Ok(self.db.deliveries(&a.id, 100)?.iter().map(delivery_json).collect())
    }

    pub fn redeliver(&self, caller: &Caller, id: &str, delivery: &str) -> Result<Value> {
        let a = self.managed_app(caller, id)?;
        let d = self.db.delivery(delivery)?.filter(|d| d.app_id == a.id).ok_or_else(|| Error::NotFound(format!("delivery {delivery}")))?;
        self.db.redeliver(&d.id)?;
        self.kick_deliveries();
        Ok(delivery_json(&self.db.delivery(&d.id)?.ok_or_else(|| Error::NotFound(format!("delivery {delivery}")))?))
    }

    // ── deliveries ──

    /// Queue a webhook to `app` (and wake the worker).
    pub(crate) fn deliver(&self, r: &RepoRow, app: &AppRow, event: &str, mut data: Value) {
        let id = new_id("dl");
        let api = format!("{}/v1/code", self.config.api_url.trim_end_matches('/'));
        let repo_api = format!("{api}/repos/{}", r.full_name());
        let mut urls = json!({ "repo": repo_api, "reviews": null, "threads": null });
        if let Some(c) = data.get("change") {
            let (base, head) = (c["base"].as_str().unwrap_or(""), c["head"].as_str().unwrap_or(""));
            let cid = c["id"].as_str().unwrap_or("");
            urls = json!({
                "repo": repo_api,
                "change": format!("{api}/changes/{cid}"),
                "compare": format!("{repo_api}/compare/{base}...{head}"),
                "diff": format!("{repo_api}/compare/{base}...{head}?format=diff"),
                "blob": format!("{repo_api}/blob/{head}/"),
                "reviews": format!("{api}/changes/{cid}/reviews"),
                "threads": format!("{api}/changes/{cid}/threads"),
                "web": format!("{}/changes/{}", self.web_url(r), c["number"]),
            });
        }
        data["event"] = json!(event);
        data["delivery"] = json!(id);
        data["app"] = json!({ "id": app.id, "name": app.name });
        data["repo"] = json!({ "id": r.id, "full_name": r.full_name(), "default_branch": r.default_branch, "visibility": r.visibility });
        data["urls"] = urls;
        if let Err(e) = self.db.insert_delivery(&id, &app.id, event, &data.to_string()) {
            tracing::warn!(app = %app.id, error = %e, "queueing a review app delivery");
            return;
        }
        self.kick_deliveries();
    }

    /// Start the delivery worker (once) or wake it.
    pub(crate) fn kick_deliveries(&self) {
        let d = &self.deliveries;
        if d.running.swap(true, Ordering::SeqCst) {
            d.notify.notify_one();
            return;
        }
        let Some(code) = self.me.upgrade() else {
            d.running.store(false, Ordering::SeqCst);
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            d.running.store(false, Ordering::SeqCst);
            return;
        }
        let weak = Arc::downgrade(&code);
        drop(code);
        tokio::spawn(async move {
            loop {
                let Some(code) = weak.upgrade() else { return };
                let wait = match code.run_deliveries().await {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::warn!(error = %e, "review app deliveries");
                        Duration::from_secs(5)
                    }
                };
                // Wait without holding the `Code`, so it can be dropped.
                tokio::select! {
                    _ = code.deliveries.notify.notified() => {}
                    _ = tokio::time::sleep(wait) => {}
                }
                drop(code);
            }
        });
    }

    fn delivery_client(&self) -> Result<reqwest::Client> {
        let mut c = self.deliveries.client.lock();
        if let Some(c) = c.as_ref() {
            return Ok(c.clone());
        }
        let mut b = reqwest::Client::builder().timeout(TIMEOUT).redirect(reqwest::redirect::Policy::none()).user_agent("roll-webhooks");
        if !self.private_webhooks_allowed() {
            b = b.dns_resolver(PublicOnly).no_proxy();
        }
        let client = b.build().map_err(|e| Error::Internal(e.into()))?;
        *c = Some(client.clone());
        Ok(client)
    }

    fn private_webhooks_allowed(&self) -> bool {
        self.config.allow_private_webhooks.unwrap_or(self.config.admin_token.is_none())
    }

    /// Send what's due; how long until the next one is.
    async fn run_deliveries(&self) -> Result<Duration> {
        let client = self.delivery_client()?;
        let due = self.db.due_deliveries(code_store::now_ms(), 50)?;
        let mut sends = Vec::new();
        for d in due {
            // Claim it first (a compare-and-swap on `next_at`): with several nodes sending, each
            // delivery is attempted by one of them. The claim lasts past the request's timeout;
            // if this node dies mid-send, the delivery is due again after it.
            if !self.db.claim_delivery(&d.id, d.next_at, code_store::now_ms() + CLAIM_MS)? {
                continue;
            }
            sends.push(self.send_delivery(&client, d));
        }
        futures::future::join_all(sends).await;
        let next = self.db.next_delivery_at()?;
        Ok(match next {
            Some(at) => Duration::from_millis((at - code_store::now_ms()).clamp(0, 60_000) as u64),
            None => Duration::from_secs(60),
        })
    }

    async fn send_delivery(&self, client: &reqwest::Client, d: DeliveryRow) {
        let attempts = d.attempts + 1;
        // Deliveries go to review apps (`app_…`) and repo webhooks (`wh_…`) alike.
        let target: std::result::Result<Option<(&str, bool, String, String)>, String> = if d.app_id.starts_with("wh_") {
            self.db.webhook(&d.app_id).map(|h| h.map(|h| ("webhook", h.active, h.url, h.secret))).map_err(|e| e.to_string())
        } else {
            self.db.app(&d.app_id).map(|a| a.map(|a| ("app", a.active, a.webhook_url, a.secret))).map_err(|e| e.to_string())
        };
        let result = match target {
            Ok(Some((_, true, url, secret))) => self.post(client, &url, &secret, &d).await,
            Ok(Some((what, false, ..))) => Err((None, format!("the {what} is disabled"), None)),
            Ok(None) => Err((None, "the app or webhook was deleted".to_string(), None)),
            Err(e) => Err((None, e, None)),
        };
        let r = match result {
            Ok((status, body)) => self.db.finish_delivery(&d.id, "delivered", attempts, d.next_at, Some(status as i64), None, Some(&body)),
            Err((status, err, body)) => {
                let base = self.config.review_app_retry_ms.max(1);
                let delay = base.saturating_mul(3u64.saturating_pow((attempts - 1) as u32)).min(MAX_BACKOFF_MS);
                let state = if attempts >= MAX_ATTEMPTS { "failed" } else { "pending" };
                let next = code_store::now_ms() + delay as i64;
                self.db.finish_delivery(&d.id, state, attempts, next, status.map(|s| s as i64), Some(&err), body.as_deref())
            }
        };
        if let Err(e) = r {
            tracing::warn!(delivery = %d.id, error = %e, "recording a review app delivery");
        }
    }

    /// POST a delivery: the status and the start of the response body, or what went wrong.
    async fn post(&self, client: &reqwest::Client, webhook_url: &str, secret: &str, d: &DeliveryRow) -> std::result::Result<(u16, String), (Option<u16>, String, Option<String>)> {
        let url = check_url(webhook_url).map_err(|e| (None, e.to_string(), None))?;
        if !self.private_webhooks_allowed() && literal_private_host(&url) {
            return Err((None, "webhook_url is a private address".into(), None));
        }
        let ts = crate::db::now();
        let body = d.payload.as_bytes().to_vec();
        let sig = signature(secret, ts, &body);
        let resp = client
            .post(url)
            .header("content-type", "application/json")
            .header("x-roll-event", &d.event)
            .header("x-roll-delivery", &d.id)
            .header("x-roll-timestamp", ts.to_string())
            .header("x-roll-signature-256", sig)
            .body(body)
            .send()
            .await
            .map_err(|e| (None, e.to_string(), None))?;
        let status = resp.status().as_u16();
        let ok = resp.status().is_success();
        let body = response_start(resp).await;
        if ok { Ok((status, body)) } else { Err((Some(status), format!("HTTP {status}"), Some(body))) }
    }
}

/// How much of a response body a delivery keeps.
pub const LAST_RESPONSE_BYTES: usize = 2048;

/// The start of a response body (at most [`LAST_RESPONSE_BYTES`], cut at a character), reading
/// no more than that from the receiver.
async fn response_start(mut resp: reqwest::Response) -> String {
    let mut buf = Vec::with_capacity(256);
    while buf.len() < LAST_RESPONSE_BYTES {
        match tokio::time::timeout(Duration::from_secs(5), resp.chunk()).await {
            Ok(Ok(Some(chunk))) => buf.extend_from_slice(&chunk[..chunk.len().min(LAST_RESPONSE_BYTES - buf.len())]),
            _ => break,
        }
    }
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    while s.len() > LAST_RESPONSE_BYTES {
        s.pop();
    }
    s
}

pub(crate) fn delivery_json(d: &DeliveryRow) -> Value {
    json!({
        "id": d.id,
        "event": d.event,
        "state": d.state,
        "attempts": d.attempts,
        "last_status": d.last_status,
        "last_error": d.last_error,
        "last_response": d.last_response,
        "created_at": d.created_at,
        "delivered_at": d.delivered_at,
        "next_at": (d.state == "pending").then_some(d.next_at / 1000),
        "payload": serde_json::from_str::<Value>(&d.payload).unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges() {
        for ip in ["127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "fc00::1", "fe80::1", "::ffff:127.0.0.1", "::ffff:10.0.0.1", "224.0.0.1", "255.255.255.255",
                   // NAT64 and 6to4 wrapping private IPv4 (the metadata address among them).
                   "64:ff9b::a9fe:a9fe", "64:ff9b::7f00:1", "64:ff9b:1::a00:1", "2002:a9fe:a9fe::1", "2002:7f00:1::"] {
            assert!(private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "140.82.112.3", "2606:4700::1111", "64:ff9b::808:808", "2002:808:808::1"] {
            assert!(!private_ip(ip.parse().unwrap()), "{ip}");
        }
        let u = |s: &str| url::Url::parse(s).unwrap();
        assert!(literal_private_host(&u("http://127.0.0.1:8080/x")));
        assert!(literal_private_host(&u("https://[::1]/x")));
        assert!(literal_private_host(&u("http://LOCALHOST/x")));
        assert!(literal_private_host(&u("http://api.localhost/x")));
        assert!(literal_private_host(&u("http://169.254.169.254/latest")));
        assert!(!literal_private_host(&u("https://api.openai.com/v1")));
    }

    #[test]
    fn urls() {
        assert!(check_url("https://hooks.example.com/roll").is_ok());
        assert!(check_url("ftp://x").is_err());
        assert!(check_url("https://user:pw@x.com/").is_err());
        assert!(check_url("not a url").is_err());
    }

    #[test]
    fn signatures_are_stable() {
        let s = signature("secret", 1700000000, b"{}");
        assert!(s.starts_with("sha256=") && s.len() == 7 + 64);
        assert_eq!(s, signature("secret", 1700000000, b"{}"));
        assert_ne!(s, signature("secret", 1700000001, b"{}"));
        assert_ne!(s, signature("other", 1700000000, b"{}"));
    }
}
