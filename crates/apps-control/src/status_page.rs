//! A project's status page (Better Stack's, Instatus'; docs/apps/README.md, "Status pages"): a
//! public page of its uptime checks and heartbeats, each up or down now and its last 90 days by
//! day, at `status-<slug>.<domain>` (and `/status.json` there). The days are kept as each check
//! runs and each heartbeat pings or is missed (`status_days`); nothing private is on it: a
//! service's and a check's names, never an address, a path, an error or a log line.

use crate::{Apps, Error, Result};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::sync::Arc;

pub const DAYS: i64 = 90;

/// What a page shows: one environment's check or heartbeat, by name.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub service: String,
    #[serde(default = "Item::production")]
    pub environment: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<String>,
}

impl Item {
    fn production() -> String {
        crate::model::PRODUCTION.into()
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Page {
    pub project: String,
    pub slug: String,
    pub title: String,
    pub items: Vec<Item>,
}

/// The reason a maintenance's silences carry (what ends them with it).
fn maintenance_reason(id: &str) -> String {
    format!("scheduled maintenance {id}")
}

pub(crate) fn check_key(service: &str, env: &str, name: &str) -> String {
    format!("check:{service}/{env}/{name}")
}

pub(crate) fn heartbeat_key(service: &str, env: &str, name: &str) -> String {
    format!("hb:{service}/{env}/{name}")
}

fn today() -> i64 {
    code_sql::now_ms() / 86_400_000
}

/// What a page shows, from its project's services and their environments (never a preview's),
/// narrowed to its items when it names any: each one's key, label and whether it's a check.
/// `running` leaves out what isn't checked now (a paused environment, one in maintenance).
fn shown(page: &Page, services: &[crate::store::ServiceRow], envs: &[crate::store::EnvRow], running: bool) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    for svc in services {
        for e in envs.iter().filter(|e| e.service == svc.id && !e.ephemeral && (!running || (!e.paused && e.maintenance.is_none()))) {
            let named = e.checks.iter().map(|c| (&c.name, true)).chain(e.heartbeats.iter().map(|h| (&h.name, false)));
            for (name, check) in named {
                if !page.items.is_empty() && !page.items.iter().any(|i| i.service == svc.id && i.environment == e.name && (if check { i.check.as_ref() } else { i.heartbeat.as_ref() }) == Some(name)) {
                    continue;
                }
                let key = if check { check_key(&svc.id, &e.name, name) } else { heartbeat_key(&svc.id, &e.name, name) };
                let label = if e.name == crate::model::PRODUCTION { format!("{} · {name}", svc.name) } else { format!("{} ({}) · {name}", svc.name, e.name) };
                out.push((key, label, check));
            }
        }
    }
    out
}

/// How long a page's rows and incidents are kept between reads (any change forgets them).
const CACHED: std::time::Duration = std::time::Duration::from_secs(10);

/// Pages' rows and incidents as last read, by project, and a generation each change moves: a
/// read that a change overtook keeps nothing (it may have read before the change).
#[derive(Default)]
pub(crate) struct StatusCache {
    generation: std::sync::atomic::AtomicU64,
    /// Active own domains → their project, as last read (when, under which `hosts_gen`): every
    /// request with a Host asks, so it's one read a second at most, never one a request.
    own: parking_lot::Mutex<Option<(std::time::Instant, u64, OwnHosts)>>,
    /// One request reads the map again; the others keep the last answer meanwhile.
    refreshing: std::sync::atomic::AtomicBool,
    pages: parking_lot::Mutex<std::collections::HashMap<String, (std::time::Instant, Arc<Shown>)>>,
}

/// Active own domains → their project.
type OwnHosts = Arc<std::collections::HashMap<String, String>>;

/// A page's rows and its incidents.
type Shown = (Vec<Row>, Vec<Value>);

impl StatusCache {
    /// Something a page shows changed: every page is read again.
    pub(crate) fn forget(&self) {
        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.pages.lock().clear();
    }
}

/// One thing shown, as the page has it: who it is, whether it's up now, and its days.
pub(crate) struct Row {
    label: String,
    up: Option<bool>,
    /// (day, checks or pings that passed, that failed or were missed).
    days: Vec<(i64, i64, i64)>,
}

impl Apps {
    /// A check's run, or a heartbeat's ping (true) or miss (false), counted on today, with what
    /// it is now (`up`): whether it was down before, when it's up now and that changed (a check
    /// back after a restart, which its memory doesn't know).
    pub(crate) async fn status_count(self: &Arc<Self>, key: String, ok: bool, up: bool) -> bool {
        let day = today();
        match self.db(move |s| s.status_day_add(&key, day, ok, up)).await {
            Ok(None) => false,
            Ok(Some(was)) => {
                self.status_cache.forget();
                was.as_deref() == Some("down")
            }
            Err(e) => {
                tracing::warn!(error = %e, "apps: a status page's day wasn't counted");
                false
            }
        }
    }

    /// Scheduled maintenances whose window began (now `maintenance`) or ended (`resolved`),
    /// each said on its page as an update. Run with [`Self::status_sweep`], every minute, on one
    /// node (`singleton:apps:maintenance`), and each step written only from the state it
    /// expects, so a node that raced it or a person's update in between makes no second step.
    pub(crate) async fn maintenance_pass(self: &Arc<Self>, now: i64) {
        if !self.deps.cluster.singleton("apps:maintenance") {
            return;
        }
        let Ok(due) = self.db(move |s| s.due_maintenance(now)).await else { return };
        for (project, id, next) in due {
            let (from, text) = if next == "maintenance" { ("scheduled", "The scheduled maintenance has begun.") } else { ("maintenance", "The scheduled maintenance is over.") };
            // One that hasn't begun yet and is already over goes straight to resolved.
            let from = if next == "resolved" { None } else { Some(from) };
            let (p2, i2) = (project.clone(), id.clone());
            if self.db(move |s| s.advance_maintenance(&project, &id, from, next, text, now)).await.unwrap_or(false) {
                self.status_cache.forget();
                if next == "resolved" {
                    let (p3, i3) = (p2.clone(), i2.clone());
                    let _ = self.db(move |s| s.end_silences_named(&p3, &maintenance_reason(&i3))).await;
                }
                // Its window quiets the alerts of what the page shows (a planned restart pages no one).
                if next == "maintenance" {
                    self.silence_for_maintenance(&p2, &i2).await;
                }
            }
        }
    }

    /// A silence, until a maintenance's window ends, for each environment its page shows.
    async fn silence_for_maintenance(self: &Arc<Self>, project: &str, id: &str) {
        let (p, i) = (project.to_string(), id.to_string());
        let Ok(Some(until)) = self.db(move |s| s.maintenance_until(&p, &i)).await else { return };
        let p = project.to_string();
        let Ok((Some(page), services, envs)) = self
            .db(move |s| {
                let services = s.services(Some(&p))?;
                let mut envs = Vec::new();
                for svc in &services {
                    envs.extend(s.environments(&svc.id)?);
                }
                Ok((s.status_page(&p)?, services, envs))
            })
            .await
        else {
            return;
        };
        let mut done = std::collections::BTreeSet::new();
        for (key, _, _) in shown(&page, &services, &envs, false) {
            // `check:<service>/<env>/<name>` or `hb:…`: the environment, once.
            let Some((svc, env)) = key.split_once(':').and_then(|(_, r)| r.split_once('/')).and_then(|(s, r)| r.split_once('/').map(|(e, _)| (s.to_string(), e.to_string()))) else { continue };
            if !done.insert((svc.clone(), env.clone())) {
                continue;
            }
            // Only what the page is about (its checks and heartbeats): a maintenance a write may
            // post never quiets the rest of production's alerts.
            let (s2, e2, reason) = (svc.clone(), env.clone(), maintenance_reason(id));
            let alerts = ["check.down".to_string(), "heartbeat.missed".to_string()];
            match self.db(move |s| s.add_silence(&s2, &e2, until, &alerts, &reason, None)).await {
                Ok(sil) => {
                    let (s3, e3, data) = (svc.clone(), env.clone(), json!({ "silence": sil.id, "until": sil.until, "alerts": sil.alerts, "reason": sil.reason }));
                    let _ = self.db(move |s| s.add_event("audit.silence", &s3, &e3, None, &data)).await;
                }
                Err(e) => tracing::warn!(service = %svc, env = %env, error = %e, "apps: a maintenance's silence wasn't made"),
            }
        }
    }

    /// The open incidents a check or heartbeat opened that nothing will close any more: its
    /// check or heartbeat gone, off the page, its environment paused or in maintenance, its
    /// service deleted. Resolved by the service's owner (anyone's, for a deleted one).
    pub(crate) async fn status_sweep(self: &Arc<Self>, services: &[crate::store::ServiceRow]) {
        let Ok(open) = self.db(|s| s.open_auto_incidents()).await else { return };
        let mut by_project: std::collections::HashMap<String, Vec<String>> = Default::default();
        for (p, k) in open {
            by_project.entry(p).or_default().push(k);
        }
        for (p, keys) in by_project {
            let p2 = p.clone();
            let Ok((Some(page), envs)) = self
                .db(move |s| {
                    let page = s.status_page(&p2)?;
                    let mut envs = Vec::new();
                    for svc in s.services(Some(&p2))? {
                        envs.extend(s.environments(&svc.id)?);
                    }
                    Ok((page, envs))
                })
                .await
            else {
                continue;
            };
            let mine: Vec<crate::store::ServiceRow> = services.iter().filter(|s| s.project == p).cloned().collect();
            let live: Vec<String> = shown(&page, &mine, &envs, true).into_iter().map(|(k, _, _)| k).collect();
            for k in keys.into_iter().filter(|k| !live.contains(k)) {
                // check:<service>/… or hb:<service>/…
                let sid = k.split_once(':').and_then(|(_, r)| r.split('/').next()).unwrap_or_default().to_string();
                if mine.iter().any(|s| s.id == sid) && !self.owns(&sid) {
                    continue;
                }
                let (p, at) = (p.clone(), code_sql::now_ms());
                if self.db(move |s| s.update_incident(&p, None, Some(&k), "resolved", "It's no longer checked.", at)).await.unwrap_or(false) {
                    self.status_cache.forget();
                }
            }
        }
    }

    /// `PUT /v1/apps/projects/{project}/status-page {title?, items?}`: made (its address kept)
    /// or changed; no items: every check and heartbeat of the project's environments.
    pub async fn put_status_page(self: &Arc<Self>, project: &str, body: Value) -> Result<Value> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["title", "items"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let title = match o.get("title") {
            None | Some(Value::Null) => "Status".to_string(),
            Some(Value::String(t)) if (1..=80).contains(&t.chars().count()) && !t.contains(['\n', '\r']) => t.clone(),
            Some(_) => return Err(Error::BadRequest("title is 1–80 characters on one line".into())),
        };
        let items: Vec<Item> = match o.get("items") {
            None | Some(Value::Null) => Vec::new(),
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| Error::BadRequest(format!("items: {e}")))?,
        };
        if items.len() > 50 || items.iter().any(|i| i.check.is_some() == i.heartbeat.is_some()) {
            return Err(Error::BadRequest("items are at most 50, each {service, environment?, check | heartbeat}".into()));
        }
        let p = project.to_string();
        let services = self.db(move |s| s.services(Some(&p))).await?;
        for i in &items {
            if !services.iter().any(|s| s.id == i.service) {
                return Err(Error::BadRequest(format!("items: {} isn't this project's service", i.service)));
            }
        }
        let (p, slug, j) = (project.to_string(), crate::model::random(10).to_ascii_lowercase(), serde_json::to_string(&items).expect("items serialize"));
        let page = self.db(move |s| s.put_status_page(&p, &slug, &title, &j)).await?;
        self.status_cache.forget();
        self.status_page_view(page)
    }

    pub async fn get_status_page(self: &Arc<Self>, project: &str) -> Result<Value> {
        let p = project.to_string();
        let page = self.db(move |s| s.status_page(&p)).await?.ok_or_else(|| Error::NotFound(format!("{project} has no status page")))?;
        self.status_page_view(page)
    }

    /// `GET …/status-page/incidents`: the last 90 days' and the open ones, newest first.
    pub async fn incidents(self: &Arc<Self>, project: &str) -> Result<Value> {
        let (p, since) = (project.to_string(), code_sql::now_ms() - DAYS * 86_400_000);
        Ok(json!({ "incidents": self.db(move |s| s.incidents(&p, since)).await? }))
    }

    /// `POST …/status-page/incidents {title, state?, text}`: a person's incident on the page.
    pub async fn post_incident(self: &Arc<Self>, project: &str, body: Value) -> Result<Value> {
        let p = project.to_string();
        if self.db(move |s| s.status_page(&p)).await?.is_none() {
            return Err(Error::NotFound(format!("{project} has no status page")));
        }
        let (title, state, text) = incident_fields(&body, true)?;
        let (p, at) = (project.to_string(), code_sql::now_ms());
        let window = maintenance_window(&body, &state, at)?;
        let id = self.db(move |s| s.open_incident_in(&p, &title, &state, &text, None, at, window)).await?.expect("a person's always opens");
        self.status_cache.forget();
        Ok(json!({ "id": id }))
    }

    /// `POST …/status-page/incidents/{id}/updates {state, text}`: what's happened since;
    /// `resolved` closes it.
    pub async fn update_incident(self: &Arc<Self>, project: &str, id: &str, body: Value) -> Result<()> {
        let (_, state, text) = incident_fields(&body, false)?;
        // Under way is a scheduled maintenance's: an incident without a window would say
        // "Under scheduled maintenance" with nothing to end it.
        if state == "maintenance" {
            let (p, i) = (project.to_string(), id.to_string());
            if !self.db(move |s| s.incident_has_window(&p, &i)).await? {
                return Err(Error::BadRequest("maintenance is a scheduled maintenance's state: post one with state scheduled, starts_at and ends_at".into()));
            }
        }
        let (p, i, at, resolved) = (project.to_string(), id.to_string(), code_sql::now_ms(), state == "resolved");
        match self.db(move |s| s.update_incident(&p, Some(&i), None, &state, &text, at)).await? {
            true => {
                self.status_cache.forget();
                // A maintenance ended early: its silences end with it.
                if resolved {
                    let (p, reason) = (project.to_string(), maintenance_reason(id));
                    let _ = self.db(move |s| s.end_silences_named(&p, &reason)).await;
                }
                Ok(())
            }
            false => Err(Error::NotFound(format!("no incident {id}"))),
        }
    }

    /// A check going down or a heartbeat missed (`open`), or either back: the incident of its
    /// key on the project's status page, if it has one.
    /// Only for what the page shows (its items, never a preview's), under the label it shows.
    pub(crate) async fn status_incident(self: &Arc<Self>, service: &str, key: String, open: bool) {
        let sid = service.to_string();
        let Ok((page, services, envs)) = self
            .db(move |s| {
                let svc = s.service(&sid)?;
                let envs = s.environments(&svc.id)?;
                Ok((s.status_page(&svc.project)?, vec![svc], envs))
            })
            .await
        else {
            return;
        };
        let Some(page) = page else { return };
        let (p, at) = (page.project.clone(), code_sql::now_ms());
        let r = if open {
            let Some((_, label, _)) = shown(&page, &services, &envs, false).into_iter().find(|(k, _, _)| *k == key) else { return };
            let title = format!("{label} is down");
            self.db(move |s| s.open_incident(&p, &title, "investigating", "Our checks found it down.", Some(&key), at)).await.map(|_| ())
        } else {
            self.db(move |s| s.update_incident(&p, None, Some(&key), "resolved", "It's back up.", at)).await.map(|_| ())
        };
        self.status_cache.forget();
        if let Err(e) = r {
            tracing::warn!(service, error = %e, "apps: a status page's incident wasn't kept");
        }
    }

    pub async fn delete_status_page(self: &Arc<Self>, project: &str) -> Result<()> {
        let p = project.to_string();
        let freed = self.db(move |s| s.delete_status_page(&p)).await?;
        self.status_cache.forget();
        self.hosts_changed();
        self.release_domains(&freed.into_iter().collect::<Vec<_>>()).await;
        Ok(())
    }

    fn status_page_view(&self, page: Page) -> Result<Value> {
        Ok(json!({ "title": page.title, "items": page.items, "url": self.url(&format!("status-{}", page.slug)) }))
    }

    /// The page for a host (`status-<slug>.<domain>`), if one has it.
    pub(crate) async fn status_page_for_host(self: &Arc<Self>, host: &str) -> Option<Page> {
        let name = host.split(':').next()?.to_ascii_lowercase();
        let slug = name.strip_suffix(&format!(".{}", self.cfg.domain))?.strip_prefix("status-")?.to_string();
        if slug.is_empty() || !slug.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        self.db(move |s| s.status_page_by_slug(&slug)).await.ok().flatten()
    }

    /// The page whose own domain (`…/status-page/domain`) is `host`, once it checked out.
    pub(crate) async fn status_page_for_own_host(self: &Arc<Self>, host: &str) -> Option<Page> {
        let name = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h).trim_end_matches('.').to_ascii_lowercase();
        if name.is_empty() || name.len() > 253 {
            return None;
        }
        let project = self.own_status_hosts().await.get(&name)?.clone();
        self.db(move |s| s.status_page(&project)).await.ok().flatten()
    }

    async fn own_status_hosts(self: &Arc<Self>) -> OwnHosts {
        let generation = self.hosts_gen.load(std::sync::atomic::Ordering::SeqCst);
        use std::sync::atomic::Ordering::SeqCst;
        let last = self.status_cache.own.lock().clone();
        if let Some((at, g, m)) = &last
            && *g == generation
            && at.elapsed() < std::time::Duration::from_secs(1)
        {
            return m.clone();
        }
        // Someone is reading it already: the last answer until theirs is in.
        if let Some((_, _, m)) = &last
            && self.status_cache.refreshing.swap(true, SeqCst)
        {
            return m.clone();
        }
        // Cleared however this ends (a request dropped mid-read too), or the map would stay old.
        struct Done<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let read = {
            let _done = Done(&self.status_cache.refreshing);
            self.db(|s| s.active_status_domains()).await
        };
        // A failed read answers what was there (nothing, the first time) and keeps nothing.
        let Ok(rows) = read else { return last.map(|(_, _, m)| m).unwrap_or_default() };
        let m = Arc::new(rows.into_iter().collect::<std::collections::HashMap<_, _>>());
        *self.status_cache.own.lock() = Some((std::time::Instant::now(), generation, m.clone()));
        m
    }

    // ── a status page's own domain ──

    /// Where a status page's own domain's CNAME points: its `status-<slug>` name.
    async fn status_target(self: &Arc<Self>, project: &str) -> Result<String> {
        let p = project.to_string();
        let page = self.db(move |s| s.status_page(&p)).await?.ok_or_else(|| Error::NotFound(format!("{project} has no status page")))?;
        Ok(self.target(&format!("status-{}", page.slug)))
    }

    /// `GET /v1/apps/projects/{project}/status-page/domain`.
    pub async fn status_domain(self: &Arc<Self>, project: &str) -> Result<crate::model::Domain> {
        let p = project.to_string();
        let (hostname, status, token, error, tls, tls_error, created_at) = self.db(move |s| s.status_domain(&p)).await?.ok_or_else(|| Error::NotFound(format!("{project}'s status page has no domain of its own")))?;
        Ok(crate::model::Domain {
            record: self.record(&hostname),
            target: self.status_target(project).await?,
            hostname,
            kind: crate::model::DomainKind::Custom,
            status,
            token,
            error,
            tls,
            tls_error,
            created_at,
            redirect_to: None,
            port: None,
        })
    }

    /// `POST …/status-page/domain {hostname}` (status.acme.dev): the page answers there too once
    /// its DNS is verified, as a custom domain's is, with a certificate of its own. One a page (a
    /// new one replaces it, pending again).
    pub async fn set_status_domain(self: &Arc<Self>, project: &str, body: Value) -> Result<crate::model::Domain> {
        self.status_target(project).await?;
        let host = body.get("hostname").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("hostname is a name like status.acme.dev".into()))?;
        let mut reserved: Vec<&str> = self.cfg.reserved.iter().map(String::as_str).collect();
        reserved.push(&self.cfg.domain);
        let host = crate::model::check_hostname(host, &reserved)?;
        if host.starts_with("*.") {
            return Err(Error::BadRequest("a status page's domain is one name, not a wildcard".into()));
        }
        let tls = if self.deps.tls.issues() { crate::model::TlsStatus::Pending } else { crate::model::TlsStatus::None };
        let (p, h) = (project.to_string(), host.clone());
        let replaced = self.db(move |s| s.set_status_domain(&p, &h, tls)).await?;
        self.hosts_changed();
        self.release_domains(&replaced.into_iter().collect::<Vec<_>>()).await;
        self.status_domain(project).await
    }

    /// `POST …/status-page/domain/verify`: its DNS checked now (the TXT challenge, or the CNAME
    /// to the page's own name where that name is the project's alone), and its certificate.
    /// Pending ones are checked every minute too.
    pub async fn verify_status_domain(self: &Arc<Self>, project: &str) -> Result<crate::model::Domain> {
        let p = project.to_string();
        let (host, was, token, ..) = self.db(move |s| s.status_domain(&p)).await?.ok_or_else(|| Error::NotFound(format!("{project}'s status page has no domain of its own")))?;
        // Once proven it stays (as a custom domain's `verified_at` does): a DNS hiccup later
        // never takes the page off its name; only its certificate is looked at again.
        if was == crate::model::DomainStatus::Active {
            self.refresh_tls(&host).await;
            return self.status_domain(project).await;
        }
        let target = self.status_target(project).await?;
        let cnames = self.deps.resolver.cname(&host).await;
        let cname_ok = cnames.contains(&target);
        let challenge = crate::model::challenge_name(&host);
        let txts = self.deps.resolver.txt(&challenge).await;
        let shared = self.cfg.cname_target.is_some();
        let (status, error) = if txts.contains(&token) || (cname_ok && !shared) {
            (crate::model::DomainStatus::Active, None)
        } else {
            let seen = crate::model::DnsSeen { cnames, txts, host_txts: self.deps.resolver.txt(&host).await, addresses: if cname_ok { Vec::new() } else { self.deps.resolver.addresses(&host).await } };
            (crate::model::DomainStatus::Pending, Some(crate::model::domain_diagnosis(&host, &target, &challenge, &token, &seen, shared)))
        };
        let (p, err) = (project.to_string(), error.clone());
        self.db(move |s| s.set_status_domain_status(&p, status, err.as_deref())).await?;
        self.hosts_changed();
        if status == crate::model::DomainStatus::Active {
            self.refresh_tls(&host).await;
        }
        self.status_domain(project).await
    }

    /// `DELETE …/status-page/domain`.
    pub async fn delete_status_domain(self: &Arc<Self>, project: &str) -> Result<()> {
        let p = project.to_string();
        let Some(host) = self.db(move |s| s.delete_status_domain(&p)).await? else {
            return Err(Error::NotFound(format!("{project}'s status page has no domain of its own")));
        };
        self.hosts_changed();
        self.release_domains(&[host]).await;
        Ok(())
    }

    /// The minute pass's share (`apps:domains`): claims unproven past [`crate::PENDING_CLAIM_TTL`]
    /// expire, pending ones are checked, and a verified one's certificate only while it's coming
    /// (the provider isn't asked about every page every minute).
    pub(crate) async fn check_status_domains(self: &Arc<Self>) {
        let age = crate::PENDING_CLAIM_TTL.as_millis() as i64;
        if let Ok(gone) = self.db(move |s| s.expire_status_domains(s.now()? - age)).await
            && !gone.is_empty()
        {
            self.hosts_changed();
            self.release_domains(&gone).await;
        }
        let Ok(all) = self.db(|s| s.status_domains_to_check()).await else { return };
        for (project, host, status) in all {
            if status == crate::model::DomainStatus::Active {
                self.refresh_tls(&host).await;
            } else {
                let _ = self.verify_status_domain(&project).await;
            }
        }
    }

    /// What a page shows now (each up or down and its days) and its incidents: read again
    /// after [`CACHED`] or any change, so a flood of visitors reads the store once.
    async fn status_now(self: &Arc<Self>, page: &Page) -> Arc<Shown> {
        if let Some((at, v)) = self.status_cache.pages.lock().get(&page.project)
            && at.elapsed() < CACHED
        {
            return v.clone();
        }
        let generation = self.status_cache.generation.load(std::sync::atomic::Ordering::SeqCst);
        let (pg, since) = (page.clone(), today() - DAYS + 1);
        let v = self
            .db(move |s| {
                let services = s.services(Some(&pg.project))?;
                let mut envs = Vec::new();
                for svc in &services {
                    envs.extend(s.environments(&svc.id)?);
                }
                let mut rows = Vec::new();
                for (key, label, _) in shown(&pg, &services, &envs, false) {
                    let last = s.get_state(&format!("status-last:{key}"))?;
                    rows.push(Row { label, up: last.map(|l| l == "up"), days: s.status_days(&key, since)? });
                }
                Ok((rows, s.incidents(&pg.project, code_sql::now_ms() - DAYS * 86_400_000)?))
            })
            .await
            .unwrap_or_default();
        let v = Arc::new(v);
        let mut cache = self.status_cache.pages.lock();
        if self.status_cache.generation.load(std::sync::atomic::Ordering::SeqCst) == generation {
            if cache.len() >= 1000 {
                cache.clear();
            }
            cache.insert(page.project.clone(), (std::time::Instant::now(), v.clone()));
        }
        v
    }
}

/// A scheduled maintenance's window from `starts_at` and `ends_at` (unix ms): in the future, at
/// most 90 days ahead and a week long. Only a new `scheduled` incident has one.
fn maintenance_window(body: &Value, state: &str, now: i64) -> Result<Option<(i64, i64)>> {
    let (from, until) = (body.get("starts_at").and_then(Value::as_i64), body.get("ends_at").and_then(Value::as_i64));
    if state != "scheduled" {
        return match (from, until) {
            (None, None) => Ok(None),
            _ => Err(Error::BadRequest("starts_at and ends_at are a scheduled maintenance's (state scheduled)".into())),
        };
    }
    match (from, until) {
        (Some(f), Some(u)) if f > now && f <= now + 90 * 86_400_000 && u > f && u - f <= 7 * 86_400_000 => Ok(Some((f, u))),
        _ => Err(Error::BadRequest("a scheduled maintenance takes starts_at (in the next 90 days) and ends_at (after it, at most a week later), unix ms".into())),
    }
}

/// The states a new incident may be posted in (`scheduled`: a maintenance with its window).
pub(crate) const NEW_STATES: [&str; 5] = ["investigating", "identified", "monitoring", "resolved", "scheduled"];
/// The states an update may move one to (`maintenance`: a scheduled one under way).
pub(crate) const UPDATE_STATES: [&str; 5] = ["investigating", "identified", "monitoring", "maintenance", "resolved"];

/// An incident's or update's fields: `title` (a new one's), `state` ([`NEW_STATES`],
/// [`UPDATE_STATES`]) and `text`, each bounded, as text.
fn incident_fields(body: &Value, new: bool) -> Result<(String, String, String)> {
    let o = body.as_object().cloned().unwrap_or_default();
    let allowed: &[&str] = if new { &["title", "state", "text", "starts_at", "ends_at"] } else { &["state", "text"] };
    if let Some(k) = o.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(Error::BadRequest(format!("unknown field {k}")));
    }
    let text_of = |k: &str, max: usize, need: bool| -> Result<String> {
        match o.get(k) {
            Some(Value::String(t)) if !t.trim().is_empty() && t.chars().count() <= max => Ok(t.trim().to_string()),
            None if !need => Ok(String::new()),
            _ => Err(Error::BadRequest(format!("{k} is 1–{max} characters"))),
        }
    };
    let title = if new { text_of("title", 120, true)? } else { String::new() };
    let states: &[&str] = if new { &NEW_STATES } else { &UPDATE_STATES };
    let state = match o.get("state").and_then(Value::as_str) {
        None if new => "investigating".to_string(),
        Some(s) if states.contains(&s) => s.to_string(),
        _ => return Err(Error::BadRequest(format!("state is {}", states.join(", ")))),
    };
    Ok((title, state, text_of("text", 2000, true)?))
}

/// The page on its own host: its HTML, or `/status.json`.
pub(crate) async fn serve(apps: Arc<Apps>, page: Page, path: &str) -> Response {
    if !matches!(path, "/" | "/status.json" | "/feed.xml") {
        return (StatusCode::NOT_FOUND, "").into_response();
    }
    let now = apps.status_now(&page).await;
    let (rows, incidents) = (&now.0, &now.1);
    let since = today() - DAYS + 1;
    if path == "/feed.xml" {
        return feed(&page, incidents);
    }
    let uptime = |days: &[(i64, i64, i64)]| {
        let (ok, all) = days.iter().fold((0, 0), |(o, a), (_, g, b)| (o + g, a + g + b));
        (all > 0).then(|| (ok as f64 * 1000.0 / all as f64).round() / 10.0)
    };
    if path == "/status.json" {
        let items: Vec<Value> = rows.iter().map(|r| json!({ "name": r.label, "up": r.up, "uptime": uptime(&r.days), "days": (since..=today()).map(|d| r.day(d)).collect::<Vec<_>>() })).collect();
        return (StatusCode::OK, [(header::CACHE_CONTROL, "public, max-age=30")], axum::Json(json!({ "title": page.title, "up": rows.iter().all(|r| r.up != Some(false)), "items": items, "incidents": incidents }))).into_response();
    }
    let all_up = rows.iter().all(|r| r.up != Some(false));
    let clock = |ms: i64| chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default();
    let (mut history, mut planned) = (String::new(), String::new());
    for i in incidents {
        let open = i["resolved_at"].is_null();
        let ups: String = i["updates"].as_array().into_iter().flatten().rev().map(|u| format!("<p><b>{}</b> · {}<br>{}</p>", crate::public::escape(u["state"].as_str().unwrap_or("")), clock(u["at"].as_i64().unwrap_or(0)), crate::public::escape(u["text"].as_str().unwrap_or("")))).collect();
        // A maintenance says its window; one still ahead is listed apart, before the incidents.
        let window = match (i["scheduled"]["from"].as_i64(), i["scheduled"]["until"].as_i64()) {
            (Some(f), Some(u)) => format!("<p class=when>{} – {}</p>", clock(f), clock(u)),
            _ => String::new(),
        };
        let class = match (i["state"].as_str(), open) {
            (Some("scheduled" | "maintenance"), true) => "inc maint",
            (_, true) => "inc open",
            _ => "inc",
        };
        let article = format!("<article class=\"{class}\"><h2>{}</h2>{window}{ups}</article>", crate::public::escape(i["title"].as_str().unwrap_or("")));
        if i["state"] == "scheduled" {
            planned.push_str(&article);
        } else {
            history.push_str(&article);
        }
    }
    let esc = crate::public::escape;
    let mut list = String::new();
    for r in rows {
        let state = match r.up {
            Some(true) => "<span class=up>up</span>",
            Some(false) => "<span class=down>down</span>",
            None => "<span class=none>no data</span>",
        };
        let bars: String = (since..=today())
            .map(|d| match r.days.iter().find(|x| x.0 == d) {
                None => "<i></i>",
                Some((_, _, b)) if *b > 0 => "<i class=b></i>",
                Some(_) => "<i class=g></i>",
            })
            .collect();
        let pct = uptime(&r.days).map(|u| format!("{u}%")).unwrap_or_else(|| "–".into());
        list.push_str(&format!("<li><div class=row><b>{}</b>{state}</div><div class=bars>{bars}</div><div class=foot><span>{DAYS} days ago</span><span>{pct} up</span><span>today</span></div></li>", esc(&r.label)));
    }
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>{t}</title>\
         <style>:root{{color-scheme:dark light;--bg:#000;--fg:#ededed;--muted:#a1a1a1;--line:#ffffff24;--accent:#4c8dff;--danger:#e5484d;--none:#ffffff1a}}\
         @media (prefers-color-scheme:light){{:root{{--bg:#fff;--fg:#0a0a0a;--muted:#666;--line:#00000024;--none:#0000001a}}}}\
         body{{margin:0;background:var(--bg);color:var(--fg);font:14px system-ui,sans-serif}}main{{max-width:720px;margin:0 auto;padding:40px 16px}}\
         h1{{font-size:22px;font-weight:600;letter-spacing:-.04em;margin:0 0 6px}}.all{{color:var(--muted);margin:0 0 28px}}ul{{list-style:none;margin:0;padding:0}}\
         li{{border:1px solid var(--line);border-radius:10px;padding:14px 16px;margin:0 0 12px}}.row{{display:flex;justify-content:space-between;gap:12px;margin:0 0 10px}}\
         .up{{color:var(--accent)}}.down{{color:var(--danger)}}.none{{color:var(--muted)}}.bars{{display:flex;gap:2px}}.bars i{{flex:1;height:28px;border-radius:2px;background:var(--none)}}\
         .bars i.g{{background:var(--accent)}}.bars i.b{{background:var(--danger)}}.foot{{display:flex;justify-content:space-between;color:var(--muted);font-size:12px;margin-top:6px}}\
         .inc{{border:1px solid var(--line);border-radius:10px;padding:12px 16px;margin:0 0 12px}}.inc.open{{border-color:var(--danger)}}.inc.maint{{border-color:var(--accent)}}.inc .when{{color:var(--fg);margin:0 0 6px}}.inc h2{{font-size:14px;margin:0 0 6px}}.inc p{{color:var(--muted);margin:6px 0 0}}\
         h3{{font-size:13px;font-weight:600;margin:28px 0 10px}}footer{{color:var(--muted);font-size:12px;margin-top:24px}}</style><link rel=alternate type=application/atom+xml href=/feed.xml><main><h1>{t}</h1><p class=all>{all}</p>{plan}<ul>{list}</ul>{hist}<footer>Kept by roll · <a href=/feed.xml style=color:inherit>feed</a></footer></main>",
        hist = if history.is_empty() { String::new() } else { format!("<h3>Incidents</h3>{history}") },
        plan = if planned.is_empty() { String::new() } else { format!("<h3>Scheduled maintenance</h3>{planned}") },
        t = esc(&page.title),
        all = if rows.is_empty() {
            "Nothing to show yet."
        } else if !all_up {
            "Something is down."
        } else if incidents.iter().any(|i| i["state"] == "maintenance" && i["resolved_at"].is_null()) {
            "Under scheduled maintenance."
        } else {
            "Everything is up."
        },
    );
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=30")], html).into_response()
}

impl Row {
    /// A day as `/status.json` has it: null with nothing that day, else its counts.
    fn day(&self, d: i64) -> Value {
        self.days.iter().find(|x| x.0 == d).map_or(Value::Null, |(_, ok, bad)| json!({ "ok": ok, "bad": bad }))
    }
}

/// The page's incidents as an Atom feed (`/feed.xml`): one entry per update, newest first.
fn feed(page: &Page, incidents: &[Value]) -> Response {
    let esc = crate::public::escape;
    let iso = |ms: i64| chrono::DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)).unwrap_or_default();
    let mut entries: Vec<(i64, String)> = Vec::new();
    for i in incidents {
        for (n, u) in i["updates"].as_array().into_iter().flatten().enumerate() {
            let at = u["at"].as_i64().unwrap_or(0);
            let id = format!("{}-{n}", i["id"].as_str().unwrap_or(""));
            entries.push((at, format!("<entry><id>urn:roll:status:{id}</id><title>{}: {}</title><updated>{}</updated><content type=\"text\">{}</content></entry>", esc(i["title"].as_str().unwrap_or("")), esc(u["state"].as_str().unwrap_or("")), iso(at), esc(u["text"].as_str().unwrap_or("")))));
        }
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.0));
    let updated = entries.first().map_or_else(|| iso(code_sql::now_ms()), |e| iso(e.0));
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><feed xmlns=\"http://www.w3.org/2005/Atom\"><id>urn:roll:status:{}</id><title>{}</title><updated>{updated}</updated>{}</feed>",
        page.slug,
        esc(&page.title),
        entries.into_iter().map(|e| e.1).collect::<String>()
    );
    (StatusCode::OK, [(header::CONTENT_TYPE, "application/atom+xml; charset=utf-8"), (header::CACHE_CONTROL, "public, max-age=60")], body).into_response()
}
