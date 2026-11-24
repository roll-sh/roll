//! Notifications (docs/apps/README.md, "Notifications"): a project's events go to its targets, a
//! webhook (JSON, signed with HMAC-SHA256), a Slack or a Discord incoming webhook, or email to the
//! project's owner (on by default: a project's first service adds it, once; Render's and Vercel's).
//! Email only ever goes to the owner's own account address ([`crate::integrations::Mailbox`]),
//! never to one a caller names, so no tenant can make roll mail a stranger.
//!
//! One node (the singleton `apps:notify`) follows the event log after a cursor kept in `state`;
//! the deliveries of a page of events and the cursor's move are one transaction, so an event is
//! queued once and never skipped. Deliveries are tried at once and again after 30 s, 2 min,
//! 10 min, 30 min and 1 h, then marked failed; each try is in the target's delivery log. URLs are
//! the tenant's choice: they're sealed, fetched through `net::client` (public addresses, no
//! redirects, a timeout), HTTPS only, and what an answer says is read bounded.

use crate::model::{Delivery, Event, NotifyTarget};
use crate::{Apps, Error, Result};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// What a target can ask for; a new one gets the two that need someone.
pub const EVENTS: [&str; 38] = [
    "deploy.started",
    "deploy.failed",
    "deploy.base_updated",
    "rollout.waiting",
    "deploy.ready",
    "error.new",
    "error.regressed",
    "crash_loop",
    "deploy.active",
    "spend.alert",
    "spend.spike",
    "spend.limit",
    "alert.memory",
    "alert.cpu",
    "alert.metric",
    "alert.errors",
    "alert.latency",
    "alert.capacity",
    "alert.log",
    "cron.failed",
    "job.failed",
    "domain.active",
    "domain.expiring",
    "domain.expired",
    "domain.tls_failed",
    "volume.backup_failed",
    "volume.full",
    "form.submitted",
    "check.down",
    "heartbeat.missed",
    "deploy.vulnerable",
    "alert.slo",
    "alert.anomaly",
    "attack.mode",
    "scale.out",
    "scale.in",
    "instance.crashed",
    // Every `audit.*` entry (who changed what), for a SIEM's webhook; never an on-call page.
    "audit",
];
const DEFAULT_EVENTS: [&str; 26] = ["check.down", "heartbeat.missed", "alert.slo", "alert.anomaly", "deploy.failed", "rollout.waiting", "deploy.ready", "error.new", "error.regressed", "crash_loop", "spend.alert", "spend.spike", "spend.limit", "alert.memory", "alert.cpu", "alert.metric", "alert.errors", "alert.latency", "alert.capacity", "alert.log", "cron.failed", "domain.expiring", "domain.expired", "domain.tls_failed", "volume.backup_failed", "volume.full"];
/// Whether a target with `events` hears an event: its own kind, or for a resolution, the alert's
/// (whoever heard an alert hears it clear; there's nothing else to ask for).
fn wants(events: &[String], kind: &str, data: &Value, oncall: bool) -> bool {
    match kind {
        "alert.resolved" => data["alert"].as_str().is_some_and(|a| events.iter().any(|e| e == a)),
        // An on-call target that pages on failed deploys hears the next good one: it closes them.
        "deploy.active" if oncall => events.iter().any(|e| e == "deploy.failed" || e == "deploy.active"),
        k if k.starts_with("audit.") => !oncall && events.iter().any(|e| e == "audit"),
        k => events.iter().any(|e| e == k),
    }
}

/// Targets per project.
const MAX_TARGETS: usize = 20;
/// After the first try: the waits before the next ones.
const RETRY_S: [i64; 5] = [30, 120, 600, 1800, 3600];

impl Apps {
    /// `POST …/projects/{project}/notifications`: a target. A webhook's signing secret is in the
    /// answer this once.
    pub async fn add_target(self: &Arc<Self>, project: &str, body: Value) -> Result<Value> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["kind", "url", "events", "environments", "services", "previews", "routing_key", "api_key", "region"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let kind = o.get("kind").and_then(Value::as_str).unwrap_or("webhook").to_string();
        // An on-call service's key: the target's secret, sealed, never shown again.
        let mut key = None;
        let (url, host) = if kind == "pagerduty" || kind == "opsgenie" {
            let field = if kind == "pagerduty" { "routing_key" } else { "api_key" };
            if let Some(k) = o.keys().find(|k| *k == "url" || (["routing_key", "api_key"].contains(&k.as_str()) && *k != field)) {
                return Err(Error::BadRequest(format!("a {kind} target takes its {field}, not {k}")));
            }
            let k = o.get(field).and_then(Value::as_str).map(str::trim).unwrap_or("");
            if k.is_empty() || k.len() > 128 || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                return Err(Error::BadRequest(format!("{field} is the key {kind} gave the integration (1–128 letters, digits, - and _)")));
            }
            key = Some(k.to_string());
            let a = &self.cfg.oncall_apis;
            let url = match (kind.as_str(), o.get("region").and_then(Value::as_str).unwrap_or("us")) {
                ("pagerduty", "us") => a.pagerduty_us.clone(),
                ("pagerduty", "eu") => a.pagerduty_eu.clone(),
                ("opsgenie", "us") => a.opsgenie_us.clone(),
                ("opsgenie", "eu") => a.opsgenie_eu.clone(),
                _ => return Err(Error::BadRequest("region is us or eu".into())),
            };
            let host = url::Url::parse(&url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
            (url, host)
        } else if o.contains_key("routing_key") || o.contains_key("api_key") || o.contains_key("region") {
            return Err(Error::BadRequest("routing_key, api_key and region are a pagerduty or opsgenie target's".into()));
        } else if kind == "email" {
            if o.contains_key("url") {
                return Err(Error::BadRequest("an email target goes to the project's owner, not to an address".into()));
            }
            if self.mailbox().is_none() {
                return Err(Error::BadRequest("email isn't set up on this deployment (ROLL_MAIL)".into()));
            }
            if self.project_owner(project).await.is_none() {
                return Err(Error::BadRequest("this project has no owner with an account to email".into()));
            }
            ("owner".to_string(), "the project's owner".to_string())
        } else {
            let url = o.get("url").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("url is required".into()))?.trim().to_string();
            let host = self.check_target(&kind, &url)?;
            (url, host)
        };
        let (events, filters) = self.target_fields(project, &o, None).await?;
        let p = project.to_string();
        {
            let this = self.clone();
            let p = p.clone();
            tokio::task::spawn_blocking(move || this.project_name(&p)).await.map_err(|e| Error::Internal(e.to_string()))??;
        }
        let pp = p.clone();
        if self.db(move |s| s.targets(&pp)).await?.len() >= MAX_TARGETS {
            return Err(Error::Conflict(format!("a project has at most {MAX_TARGETS} notification targets")));
        }
        let secret = (kind == "webhook").then(|| format!("whsec_{}", crate::model::random(32)));
        let (k, u, s2, ev) = (kind.clone(), url.clone(), secret.clone().or(key), events.clone());
        let t = self.db(move |s| s.add_target(&p, &k, &host, &u, s2.as_deref(), &ev, &filters)).await?;
        let mut v = serde_json::to_value(&t).expect("JSON");
        // A webhook's signing secret, this once; an on-call key was the caller's to begin with.
        if let Some(s) = secret {
            v["secret"] = json!(s);
        }
        Ok(v)
    }

    /// A project's first service: its owner is emailed what needs someone, once per project (a
    /// removed target stays removed). Nothing without mail, or for an operator's project.
    pub(crate) async fn offer_email(self: &Arc<Self>, project: &str) {
        if self.mailbox().is_none() || self.project_owner(project).await.is_none() {
            return;
        }
        let key = format!("apps.email_offered.{project}");
        // Claimed in one statement: two services made at once add one target.
        let first = self.db(move |s| s.claim_state(&key, "1")).await.unwrap_or(false);
        if first && let Err(e) = self.add_target(project, json!({ "kind": "email" })).await {
            tracing::warn!(project, error = %e, "apps: the owner's email target wasn't added");
        }
    }

    /// A target's events and filters from a body: what's absent is `current`'s (an edit), else
    /// the defaults (a new one). A service by its name or id, kept by id; only the project's.
    async fn target_fields(self: &Arc<Self>, project: &str, o: &serde_json::Map<String, Value>, current: Option<&NotifyTarget>) -> Result<(Vec<String>, crate::store::TargetFilters)> {
        let events: Vec<String> = match o.get("events") {
            // Absent: what it had (an edit) or the defaults (a new one); null: the defaults, as POST's.
            None => current.map_or_else(|| DEFAULT_EVENTS.map(String::from).to_vec(), |c| c.events.clone()),
            Some(Value::Null) => DEFAULT_EVENTS.map(String::from).to_vec(),
            Some(Value::Array(a)) => {
                let e: Vec<String> = a.iter().filter_map(Value::as_str).map(String::from).collect();
                if e.is_empty() || e.len() != a.len() || e.iter().any(|x| !EVENTS.contains(&x.as_str())) {
                    return Err(Error::BadRequest(format!("events are some of {}", EVENTS.join(", "))));
                }
                e
            }
            Some(_) => return Err(Error::BadRequest("events is a list".into())),
        };
        let names = |field: &str| -> Result<Option<Vec<String>>> {
            match o.get(field) {
                None => Ok(None),
                Some(Value::Null) => Ok(Some(Vec::new())),
                Some(Value::Array(a)) if a.len() <= 50 => {
                    let v: Vec<String> = a.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                    if v.len() != a.len() {
                        return Err(Error::BadRequest(format!("{field} is a list of names")));
                    }
                    Ok(Some(v))
                }
                Some(_) => Err(Error::BadRequest(format!("{field} is a list of at most 50 names"))),
            }
        };
        let environments = match names("environments")? {
            Some(v) => v,
            None => current.map(|c| c.environments.clone()).unwrap_or_default(),
        };
        // A service by its name or id, kept by id (a rename doesn't drop it); only the project's.
        let p = project.to_string();
        let mut services = current.map(|c| c.services.clone()).unwrap_or_default();
        if let Some(wanted) = names("services")? {
            services.clear();
            let pp = p.clone();
            let all = self.db(move |s| s.services(Some(&pp))).await?;
            for w in wanted {
                let svc = all.iter().find(|s| s.id == w || s.name == w).ok_or_else(|| Error::BadRequest(format!("services: the project has no service {w}")))?;
                services.push(svc.id.clone());
            }
        }
        // New targets leave previews out (each pull request's failed deploys would reach the
        // team), unless asked, or unless they name an environment that is one.
        let previews = match o.get("previews") {
            None => current.is_some_and(|c| c.previews),
            Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(Error::BadRequest("previews is true or false".into())),
        };
        Ok((events, crate::store::TargetFilters { environments, services, previews }))
    }

    /// `PATCH …/notifications/{nid}`: its events, environments, services and previews (what's
    /// absent stays). Its URL and secret don't change: a new URL is a new target.
    pub async fn update_target(self: &Arc<Self>, project: &str, id: &str, body: Value) -> Result<NotifyTarget> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["events", "environments", "services", "previews"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("{k} can't be changed (a new URL is a new target)")));
        }
        let current = self.targets(project).await?.into_iter().find(|t| t.id == id).ok_or_else(|| Error::NotFound(format!("no notification target {id}")))?;
        let (events, filters) = self.target_fields(project, &o, Some(&current)).await?;
        let (p, i) = (project.to_string(), id.to_string());
        self.db(move |s| s.update_target(&p, &i, &events, &filters)).await
    }

    /// A target's URL: HTTPS, a public host, and for Slack and Discord their webhook hosts.
    pub(crate) fn check_target(&self, kind: &str, url: &str) -> Result<String> {
        let bad = |why: &str| Err(Error::BadRequest(format!("url: {why}")));
        let u = url::Url::parse(url).map_err(|e| Error::BadRequest(format!("url: {e}")))?;
        if url.len() > 2048 {
            return bad("at most 2048 characters");
        }
        if u.scheme() != "https" && !(self.cfg.allow_private && u.scheme() == "http") {
            return bad("https only");
        }
        let Some(host) = u.host_str().map(str::to_ascii_lowercase) else { return bad("no host") };
        if !self.cfg.allow_private && crate::net::literal_private(&host) {
            return bad("a private address");
        }
        match kind {
            "webhook" => {}
            "slack" if host == "hooks.slack.com" => {}
            "discord" if (host == "discord.com" || host == "discordapp.com") && u.path().starts_with("/api/webhooks/") => {}
            // Teams' Workflows webhooks (and the connectors' old ones), Google Chat's space webhooks.
            "teams" if host.ends_with(".webhook.office.com") || host.ends_with(".logic.azure.com") || host.ends_with(".powerplatform.com") => {}
            "googlechat" if host == "chat.googleapis.com" && u.path().starts_with("/v1/spaces/") => {}
            "teams" if !self.cfg.allow_private => return bad("a Teams webhook is a Workflows (…logic.azure.com, …powerplatform.com) or …webhook.office.com URL"),
            "googlechat" if !self.cfg.allow_private => return bad("a Google Chat webhook is https://chat.googleapis.com/v1/spaces/…"),
            "teams" | "googlechat" => {}
            "slack" if !self.cfg.allow_private => return bad("a Slack incoming webhook is on hooks.slack.com"),
            "discord" if !self.cfg.allow_private => return bad("a Discord webhook is https://discord.com/api/webhooks/…"),
            "slack" | "discord" => {}
            _ => return Err(Error::BadRequest("kind is webhook, slack, discord, teams, googlechat, email, pagerduty or opsgenie".into())),
        }
        Ok(host)
    }

    pub async fn targets(self: &Arc<Self>, project: &str) -> Result<Vec<NotifyTarget>> {
        let p = project.to_string();
        self.db(move |s| s.targets(&p)).await
    }

    pub async fn delete_target(self: &Arc<Self>, project: &str, id: &str) -> Result<()> {
        let (p, i) = (project.to_string(), id.to_string());
        self.db(move |s| s.delete_target(&p, &i)).await
    }

    pub async fn deliveries(self: &Arc<Self>, project: &str, id: &str) -> Result<Vec<Delivery>> {
        let p = project.to_string();
        if !self.db(move |s| s.targets(&p)).await?.iter().any(|t| t.id == id) {
            return Err(Error::NotFound(format!("no notification target {id}")));
        }
        let i = id.to_string();
        self.db(move |s| s.deliveries(&i, 50)).await
    }

    /// `POST …/{id}/test`: a message sent now, its outcome answered (not queued, not retried).
    pub async fn test_target(self: &Arc<Self>, project: &str, id: &str) -> Result<Value> {
        let i = id.to_string();
        let Some((t, url, secret)) = self.db(move |s| s.target_secrets(&i)).await?.filter(|(t, _, _)| t.project == project) else {
            return Err(Error::NotFound(format!("no notification target {id}")));
        };
        let e = Event { seq: 0, at: code_sql::now_ms(), kind: "test".into(), service: String::new(), environment: String::new(), deployment: None, data: json!({}) };
        let delivery = format!("test_{}", crate::model::random(12));
        let (code, error) = self.send(&t, &url, secret.as_deref(), &delivery, &e, "test", "this is a test from roll apps").await;
        Ok(json!({ "ok": error.is_none(), "code": code, "error": error }))
    }

    /// The notifier: queue a page of events' deliveries with the cursor, then try what's due.
    pub(crate) async fn notifier(self: Arc<Self>) {
        loop {
            if self.deps.cluster.singleton("apps:notify") {
                self.queue_page().await;
                self.deliver_due().await;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = self.stop.cancelled() => return,
            }
        }
    }

    async fn queue_page(self: &Arc<Self>) {
        let cursor = match self.db(|s| s.get_state("notify")).await {
            Ok(Some(c)) => c.parse().unwrap_or(0),
            // First start: from now on, not the week behind.
            Ok(None) => {
                let head = self.db(|s| s.last_event_seq()).await.unwrap_or(0);
                let _ = self.db(move |s| s.set_state("notify", &head.to_string())).await;
                head
            }
            Err(_) => return,
        };
        let Ok(events) = self.db(move |s| s.events(cursor, 200)).await else { return };
        let Some(last) = events.last().map(|e| e.seq) else { return };
        let mut queued = Vec::new();
        // Whether an environment is a preview's, looked up once per pass.
        let mut ephemeral: std::collections::HashMap<(String, String), bool> = std::collections::HashMap::new();
        for e in &events {
            // A project's own events (spend) name it; a service's are its project's.
            let p = match e.data["project"].as_str().filter(|_| e.service.is_empty()) {
                Some(p) => p.to_string(),
                None => {
                    // A deleted service's own entry (`audit.deleted`) is still its project's.
                    let (sid, audit) = (e.service.clone(), e.kind.starts_with("audit."));
                    let Ok(svc) = self.db(move |s| if audit { s.service_any(&sid).map(|(s, _)| s) } else { s.service(&sid) }).await else { continue };
                    svc.project
                }
            };
            let key = (e.service.clone(), e.environment.clone());
            let preview = match ephemeral.get(&key) {
                Some(b) => *b,
                None => {
                    let (sid, en) = key.clone();
                    // One already deleted (a closed pull request's last failed deploy) is still a
                    // preview: by its name, as roll previews makes them.
                    let named = en.starts_with("pv-");
                    let b = !sid.is_empty() && self.db(move |s| s.environment(&sid, &en)).await.map_or(named, |r| r.ephemeral);
                    ephemeral.insert(key, b);
                    b
                }
            };
            for t in self.db(move |s| s.targets(&p)).await.unwrap_or_default() {
                if wants(&t.events, &e.kind, &e.data, matches!(t.kind.as_str(), "pagerduty" | "opsgenie")) && t.follows(&e.service, &e.environment, preview) {
                    queued.push((t.id.clone(), e.seq, e.kind.clone()));
                }
            }
        }
        match self.db(move |s| s.queue_deliveries(&queued, cursor, last)).await {
            Ok(true) => {}
            Ok(false) => tracing::info!("apps: another node moved the notifications cursor; reading from there"),
            Err(e) => tracing::warn!(error = %e, "apps: queueing notifications failed; tried again"),
        }
    }

    /// What's due, a few at a time (one slow target doesn't hold up the others), each try's
    /// outcome recorded with its next try.
    async fn deliver_due(self: &Arc<Self>) {
        use futures::StreamExt;
        // Claimed for longer than a try takes (10 s, eight at a time over at most 50).
        let Ok(due) = self.db(|s| s.due_deliveries(50, 120_000)).await else { return };
        futures::stream::iter(due).for_each_concurrent(8, |(id, target, seq, attempts)| async move {
            let retry = |error: Option<&str>| error.and_then(|_| RETRY_S.get(attempts as usize)).map(|s| code_sql::now_ms() + s * 1000);
            let t = target.clone();
            let (t, url, secret) = match self.db(move |s| s.target_secrets(&t)).await {
                Ok(Some(x)) => x,
                Ok(None) => {
                    let _ = self.db(move |s| s.finish_delivery(&id, None, Some("the target is gone"), None)).await;
                    return;
                }
                Err(e) => {
                    let why = format!("reading the target: {e}");
                    let next = retry(Some(&why));
                    let _ = self.db(move |s| s.finish_delivery(&id, None, Some(&why), next)).await;
                    return;
                }
            };
            let Ok(Some(e)) = self.db(move |s| s.event(seq)).await else {
                let _ = self.db(move |s| s.finish_delivery(&id, None, Some("the event is gone"), None)).await;
                return;
            };
            let (title, text) = self.describe(&e).await;
            let (code, error) = self.send(&t, &url, secret.as_deref(), &id, &e, &title, &text).await;
            let next = retry(error.as_deref());
            let _ = self.db(move |s| s.finish_delivery(&id, code, error.as_deref(), next)).await;
        })
        .await;
    }

    /// An event for a person: a title and a line.
    async fn describe(self: &Arc<Self>, e: &Event) -> (String, String) {
        let sid = e.service.clone();
        let name = self.db(move |s| s.service_any(&sid)).await.map(|(s, _)| s.name).unwrap_or_else(|_| e.service.clone());
        let what = format!("{name} in {}", e.environment);
        let rev = e.data["revision"].as_str().map(|r| format!(" ({})", &r[..r.len().min(8)])).unwrap_or_default();
        // Who asked for the deploy, when someone did.
        let by = match e.deployment.clone() {
            Some(d) => self
                .db(move |s| s.deployment(&d))
                .await
                .ok()
                .and_then(|d| d.actor)
                .map(|a| match a.strip_prefix("git:") {
                    // A commit's author is what the commit says, not who's proven.
                    Some(name) => format!(" by {name} (commit author)"),
                    None => format!(" by {a}"),
                })
                .unwrap_or_default(),
            None => String::new(),
        };
        let rev = format!("{rev}{by}");
        match e.kind.as_str() {
            k if k.starts_with("audit.") => {
                let what = if e.environment.is_empty() { name.clone() } else { what };
                let who = e.data["actor"].as_str().unwrap_or("someone");
                let mut fields = e.data.as_object().cloned().unwrap_or_default();
                fields.remove("actor");
                let said = if fields.is_empty() { String::new() } else { format!(" {}", Value::Object(fields)) };
                (format!("{what}: {}", &k[6..]), format!("{who} did {}{said}.", &k[6..]))
            }
            "deploy.failed" => (format!("{what}: deploy failed"), format!("The deploy{rev} failed: {}. The live deployment keeps serving.", e.data["error"].as_str().unwrap_or("no reason given"))),
            "deploy.active" => (format!("{what}: deployed"), format!("A new deployment{rev} is live.")),
            "rollout.waiting" => (
                format!("{what}: a rollout waits for you at {}%", e.data["share"]),
                format!("The deployment{rev} passed its step at {}% of requests and holds there until someone advances it, puts it all live or rolls it back (the service page, or `roll apps rollout`). It still rolls back by itself if it starts failing.", e.data["share"]),
            ),
            "deploy.vulnerable" => {
                let ids: Vec<&str> = e.data["ids"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                let more = e.data["new"].as_u64().unwrap_or(0).saturating_sub(ids.len() as u64);
                (
                    format!("{what}: {} new known vulnerabilities", e.data["new"]),
                    format!(
                        "The image{rev} has {} critical and {} high known vulnerabilities; new since the last scan: {}{}. Deploying without the cache picks up a fixed base image; the deployment's vulnerabilities list what to upgrade.",
                        e.data["critical"], e.data["high"], ids.join(", "), if more > 0 { format!(" and {more} more") } else { String::new() }
                    ),
                )
            }
            "deploy.base_updated" => (format!("{what}: its base image {} was updated", e.data["image"].as_str().unwrap_or("")), "A deploy now would build on the new one and get its fixes (Deploy without the cache, or `run.rebuild_on_base` to do it by itself).".to_string()),
            "deploy.ready" => (format!("{what}: built and waiting to go live"), format!("The deployment{rev} is healthy beside the live one, on its own URL ({}), and waits for a person to put it live.", e.data["url"].as_str().unwrap_or("its deployment URL"))),
            "spend.alert" | "spend.limit" => {
                let (project, usd, limit) = (e.data["project_name"].as_str().unwrap_or("a project"), e.data["usd"].as_f64().unwrap_or(0.0), e.data["limit_usd"].as_f64().unwrap_or(0.0));
                if e.kind == "spend.alert" {
                    (format!("{project}: spend alert"), format!("${usd:.2} this month, past the ${limit:.2} alert."))
                } else {
                    (format!("{project}: spending limit reached"), format!("${usd:.2} this month reached the ${limit:.2} limit: its services are stopped until the limit is raised or the month ends."))
                }
            }
            "spend.spike" => {
                let (project, usd, usual) = (e.data["project_name"].as_str().unwrap_or("a project"), e.data["usd"].as_f64().unwrap_or(0.0), e.data["usual_usd"].as_f64().unwrap_or(0.0));
                (format!("{project}: spending spiked"), format!("${usd:.2} in the last hour, against ${usual:.2} usually at this hour (the median of the same hour over the last four weeks). Its usage shows what by service."))
            }
            "alert.memory" => (format!("{what}: memory"), format!("Its instances used {}% of their {} MB limit for five minutes (the alert is at {}%).", e.data["value"], e.data["limit_mb"], e.data["threshold"])),
            "alert.cpu" => (format!("{what}: CPU"), format!("Its instances used {}% of their {} vCPU for five minutes (the alert is at {}%). Give them more (`roll apps set --cpu`), or let it scale on CPU (`--cpu-target`).", e.data["value"], e.data["cpu"], e.data["threshold"])),
            "alert.metric" => {
                let name = e.data["metric"].as_str().unwrap_or("a metric");
                let (op, t) = match e.data.get("above") {
                    Some(t) => ("above", t),
                    None => ("below", &e.data["below"]),
                };
                (format!("{what}: {name} is {op} {t}"), format!("Its {name} was {} ({op} {t}) for {} minutes running.", e.data["value"], e.data["minutes"]))
            }
            "alert.slo" => (
                format!("{what}: its error budget is burning"),
                format!(
                    "Its {} objective ({}%) is spending its error budget {}× as fast as it lasts, over the last {} and the minutes since.",
                    e.data["indicator"].as_str().unwrap_or("availability"),
                    e.data["objective"],
                    e.data["burn_rate"],
                    if e.data["window"] == "1h" { "hour" } else { "six hours" }
                ),
            ),
            "alert.errors" => (format!("{what}: errors"), format!("{}% of its {} requests in five minutes answered 5xx (the alert is at {}%).", e.data["value"], e.data["requests"], e.data["threshold"])),
            "alert.latency" => (format!("{what}: slow"), format!("Its p95 latency reached {} ms (the alert is at {} ms).", e.data["value"], e.data["threshold"])),
            "alert.capacity" => (format!("{what}: at capacity"), format!("{}% of its {} requests in five minutes found all {} instances busy ({} requests each at once). Raise its max instances (`roll apps set --max`) or its concurrency, or make it answer faster (the alert is at {}%).", e.data["value"], e.data["requests"], e.data["max_instances"], e.data["concurrency"], e.data["threshold"])),
            "heartbeat.missed" => (
                format!("{what}: heartbeat {} missed", e.data["heartbeat"].as_str().unwrap_or("")),
                format!("No ping came within {} s{}.", e.data["every_s"], e.data["last_ping_at"].as_i64().map(|t| format!(" of the last ({})", chrono::DateTime::from_timestamp_millis(t).map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default())).unwrap_or_else(|| " of it being added".into())),
            ),
            "check.down" => (
                format!("{what}: {} is down", e.data["path"].as_str().unwrap_or("a page")),
                format!("Its uptime check {} failed twice in a row: {}.", e.data["check"].as_str().unwrap_or(""), e.data["error"].as_str().unwrap_or("no answer")),
            ),
            "form.submitted" => (
                format!("{what}: a submission to {}", e.data["form"].as_str().unwrap_or("a form")),
                format!("Its fields: {}. The service's Forms panel shows what was sent.", e.data["fields"].as_array().map(|f| f.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).filter(|f| !f.is_empty()).unwrap_or_else(|| "none".into())),
            ),
            "error.new" => (format!("{what}: a new error, {}", e.data["name"].as_str().unwrap_or("an error")), "Its log has an error it hasn't had before. The Metrics tab's Errors shows it whole, with how often it happens.".to_string()),
            "error.regressed" => (format!("{what}: {} is back", e.data["name"].as_str().unwrap_or("an error")), format!("An error marked resolved is in its log again{rev}. The Metrics tab's Errors shows it; resolve it again once it's fixed, or mute it.")),
            "alert.log" => (format!("{what}: its log says {}", e.data["pattern"]), format!("{} lines in five minutes matched (the alert is at {}).", e.data["count"], e.data["threshold"])),
            "volume.full" => (format!("{what}: its volume is {}% full", e.data["pct"]), format!("It holds {:.1} GB of its {} GB: give it more room (`roll apps set --volume`) or delete what it doesn't need before it fills up.", e.data["used_bytes"].as_f64().unwrap_or(0.0) / 1_073_741_824.0, e.data["size_gb"])),
            "alert.anomaly" => {
                let (v, usual) = (&e.data["value"], &e.data["usual"]);
                match e.data["kind"].as_str() {
                    Some("traffic") => (format!("{what}: traffic dropped"), format!("{v} requests in the last hour, where this hour of the week usually has {usual}. Is something in front of it (a domain, a client, a schedule) broken?")),
                    Some("errors") => (format!("{what}: errors are unusual"), format!("{v}% of the last hour's requests failed, where this hour of the week usually sees {usual}%.")),
                    _ => (format!("{what}: slower than usual"), format!("p95 {v} ms in the last hour, where this hour of the week is usually {usual} ms.")),
                }
            }
            "deploy.started" => (format!("{what}: building"), format!("A deploy{} began building{}.", e.data["revision"].as_str().map(|r| format!(" of {}", &r[..r.len().min(8)])).unwrap_or_default(), if e.data["reused"] == true { " (an earlier build's image, no build)" } else { "" })),
            "scale.out" | "scale.in" => (
                format!("{what}: scaled {} to {} instances", if e.kind == "scale.out" { "out" } else { "in" }, e.data["to"]),
                format!("From {} instances: {}.", e.data["from"], e.data["reason"].as_str().unwrap_or("")),
            ),
            "instance.crashed" => (format!("{what}: an instance crashed"), format!("It stopped because {}. Another starts in its place.", e.data["reason"].as_str().unwrap_or("it exited"))),
            "attack.mode" => (
                format!("{what}: attack mode is on"),
                format!(
                    "{} requests in a minute (its threshold is {}): every visitor without a pass gets the challenge page until {}. End it early with `challenge_until: null`.",
                    e.data["requests"],
                    e.data["per_minute"],
                    e.data["until"].as_i64().and_then(chrono::DateTime::from_timestamp_millis).map(|d| d.format("%H:%M UTC").to_string()).unwrap_or_default()
                ),
            ),
            "volume.backup_failed" => (format!("{what}: volume backup failed"), format!("The daily backup of its volume failed: {}. The last one that worked is still there.", e.data["error"].as_str().unwrap_or("no reason given"))),
            "alert.resolved" => {
                let which = match e.data["alert"].as_str() {
                    Some("alert.memory") => "memory",
                    Some("alert.cpu") => "CPU",
                    Some("alert.metric") => e.data["metric"].as_str().unwrap_or("a metric"),
                    Some("alert.errors") => "errors",
                    Some("alert.slo") => "error budget burn",
                    Some("alert.anomaly") => "unusual behaviour",
                    Some("alert.latency") => "latency",
                    Some("alert.capacity") => "capacity",
                    Some("alert.log") => "log",
                    Some("check.down") => "uptime check",
                    _ => "an",
                };
                if e.data["alert"] == "heartbeat.missed" {
                    let name = e.data["heartbeat"].as_str().unwrap_or("");
                    (format!("{what}: heartbeat {name} is back"), format!("Heartbeat {name} pinged again."))
                } else if e.data["alert"] == "check.down" {
                    let name = e.data["check"].as_str().unwrap_or("");
                    (format!("{what}: uptime check {name} is up"), format!("Its uptime check {name} passes again."))
                } else {
                    (format!("{what}: {which} alert resolved"), format!("The {which} alert cleared: the last five minutes are under its threshold."))
                }
            }
            "cron.failed" | "job.failed" => {
                let what_ran = if e.kind == "cron.failed" { "A scheduled run" } else { "A job" };
                let how = match (e.data["status"].as_str(), e.data["exit_code"].as_i64(), e.data["path"].as_str()) {
                    (_, _, Some(p)) => match e.data["http_status"].as_u64() {
                        Some(c) => format!("calling {p} answered {c}"),
                        None => format!("calling {p} got no answer: {}", e.data["error"].as_str().unwrap_or("it didn't answer")),
                    },
                    (Some("timed_out"), _, _) => "ran past its timeout".to_string(),
                    (_, Some(c), _) => format!("exited with code {c}"),
                    _ => "didn't start".to_string(),
                };
                (format!("{what}: {} failed", if e.kind == "cron.failed" { "cron run" } else { "job" }), format!("{what_ran} {how}."))
            }
            "crash_loop" => (format!("{what}: crashing"), format!("{} crashes in 10 minutes; the last because {}.", e.data["crashes"], e.data["last_error"].as_str().unwrap_or("it exited"))),
            "domain.active" => (format!("{what}: {} is live", e.data["hostname"].as_str().unwrap_or("a domain")), "Its DNS checks out; it serves the environment now.".to_string()),
            "domain.expiring" => (
                format!("{what}: {} isn't verified", e.data["hostname"].as_str().unwrap_or("a domain")),
                format!("It's removed tomorrow unless its DNS checks out: {}", e.data["error"].as_str().unwrap_or("its records aren't there yet")),
            ),
            "domain.tls_failed" => (
                format!("{what}: {}'s certificate failed", e.data["hostname"].as_str().unwrap_or("a domain")),
                format!("Visitors over HTTPS get an error until it's issued: {}", e.data["error"].as_str().unwrap_or("its provider gave no reason")),
            ),
            "domain.expired" => (
                format!("{what}: {} was removed", e.data["hostname"].as_str().unwrap_or("a domain")),
                "It stayed unverified for 7 days, so the name is free again; add it again once its DNS is set.".to_string(),
            ),
            other => (format!("{what}: {other}"), String::new()),
        }
    }

    /// One try: the status code and, when it didn't work, why.
    /// A webhook is signed as roll code's are (`code_control::review::apps::signature`): the
    /// send time in seconds, so a retry passes a receiver's replay window, and the delivery's id.
    #[allow(clippy::too_many_arguments)]
    async fn send(&self, t: &NotifyTarget, url: &str, secret: Option<&str>, delivery: &str, e: &Event, title: &str, text: &str) -> (Option<u16>, Option<String>) {
        let link = self.cfg.web_url.as_ref().filter(|_| !e.service.is_empty()).map(|w| format!("{w}/dashboard/apps/{}?env={}", e.service, e.environment));
        if t.kind == "email" {
            // The owner as they are now (a project handed over mails its new owner).
            let Some(mail) = self.mailbox() else { return (None, Some("email isn't set up on this node".into())) };
            let Some(owner) = self.project_owner(&t.project).await else { return (None, Some("the project has no owner with an account".into())) };
            let open = link.map(|l| format!("\n\nOpen it in roll: {l}")).unwrap_or_default();
            let text = format!("{text}{open}\n\nYou get this because you own the project. Its notification settings in roll say what's sent, or remove this email.");
            return match mail.send(&owner, &format!("[roll] {title}"), &text).await {
                Ok(()) => (None, None),
                Err(err) => (None, Some(err.to_string())),
            };
        }
        if t.kind == "pagerduty" || t.kind == "opsgenie" {
            return self.send_oncall(t, url, secret.unwrap_or(""), e, title, text, link.as_deref()).await;
        }
        let body = match t.kind.as_str() {
            // Slack reads `<…>` as links and mentions (`<!channel>`) and `&` as an entity start:
            // a commit author or an error can't ping a channel or hide a link.
            "slack" => json!({ "text": format!("*{}*\n{}{}", slack_escape(title), slack_escape(text), link.as_ref().map(|l| format!("\n<{l}|Open in roll>")).unwrap_or_default()) }),
            "discord" => json!({ "content": format!("**{title}**\n{text}{}", link.as_ref().map(|l| format!("\n{l}")).unwrap_or_default()), "allowed_mentions": { "parse": [] } }),
            // An Adaptive Card (what Teams' Workflows take): its text is never a mention.
            "teams" => {
                let body = [json!({ "type": "TextBlock", "text": teams_escape(title), "weight": "Bolder", "wrap": true }), json!({ "type": "TextBlock", "text": teams_escape(text), "wrap": true })];
                let actions = link.as_ref().map(|l| vec![json!({ "type": "Action.OpenUrl", "title": "Open in roll", "url": l })]).unwrap_or_default();
                json!({ "type": "message", "attachments": [{ "contentType": "application/vnd.microsoft.card.adaptive", "content": { "$schema": "http://adaptivecards.io/schemas/adaptive-card.json", "type": "AdaptiveCard", "version": "1.4", "body": body, "actions": actions } }] })
            }
            // Google Chat reads `<users/all>` as a mention and `<url|text>` as a link: neither
            // comes from what an event says.
            "googlechat" => {
                let clean = |s: &str| s.replace(['<', '>'], "");
                json!({ "text": format!("*{}*\n{}{}", clean(title), clean(text), link.as_ref().map(|l| format!("\n<{l}|Open in roll>")).unwrap_or_default()) })
            }
            _ => json!({ "type": e.kind, "at": e.at, "seq": e.seq, "service": e.service, "environment": e.environment, "deployment": e.deployment, "data": e.data, "title": title, "text": text, "url": link }),
        };
        let bytes = serde_json::to_vec(&body).expect("JSON");
        let mut req = self.notify_client.post(url).header("content-type", "application/json");
        if let Some(s) = secret.filter(|_| t.kind == "webhook") {
            use hmac::Mac;
            let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(s.as_bytes()).expect("any key");
            let ts = (code_sql::now_ms() / 1000).to_string();
            mac.update(ts.as_bytes());
            mac.update(b".");
            mac.update(&bytes);
            let sig = mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>();
            req = req.header("x-roll-event", &e.kind).header("x-roll-delivery", delivery).header("x-roll-timestamp", ts).header("x-roll-signature-256", format!("sha256={sig}"));
        }
        match req.body(bytes).send().await {
            Ok(res) if res.status().is_success() => (Some(res.status().as_u16()), None),
            Ok(res) => {
                let code = res.status().as_u16();
                let said = String::from_utf8_lossy(&crate::net::read_bounded(res, 512).await).trim().chars().take(200).collect::<String>();
                (Some(code), Some(if said.is_empty() { format!("answered {code}") } else { format!("answered {code}: {said}") }))
            }
            Err(err) => (None, Some(err.without_url().to_string())),
        }
    }
}

/// What an event is to an on-call service: an incident to open (with its severity) or to close,
/// and which incident (one per service, environment and alert, so a repeat updates it). None:
/// nothing to page about.
fn oncall_of(e: &Event) -> Option<(bool, String, &'static str)> {
    let key = |k: &str| format!("roll:{}/{}/{k}", e.service, e.environment);
    Some(match e.kind.as_str() {
        // A check's or heartbeat's own incident, by its name: two checks are two pages.
        "alert.resolved" if e.data["alert"] == "check.down" => (false, key(&format!("check.down/{}", e.data["check"].as_str()?)), "info"),
        "alert.resolved" if e.data["alert"] == "heartbeat.missed" => (false, key(&format!("heartbeat.missed/{}", e.data["heartbeat"].as_str()?)), "info"),
        "alert.resolved" => (false, key(e.data["alert"].as_str()?), "info"),
        "check.down" => (true, key(&format!("check.down/{}", e.data["check"].as_str()?)), "error"),
        "heartbeat.missed" => (true, key(&format!("heartbeat.missed/{}", e.data["heartbeat"].as_str()?)), "error"),
        // A deploy that worked closes the failed one's.
        "deploy.active" => (false, key("deploy.failed"), "info"),
        "domain.active" => return None,
        "crash_loop" | "spend.limit" => (true, key(&e.kind), "critical"),
        "deploy.failed" | "alert.errors" | "alert.slo" | "cron.failed" | "job.failed" | "volume.backup_failed" | "domain.tls_failed" | "domain.expired" => (true, key(&e.kind), "error"),
        "form.submitted" => (true, format!("{}/{}", key(&e.kind), e.data["submission"].as_str().unwrap_or("")), "info"),
        "test" => (true, format!("roll:test:{}", e.at), "info"),
        _ => (true, key(&e.kind), "warning"),
    })
}

impl Apps {
    /// One try at PagerDuty's Events API v2 or Opsgenie's Alert API: open or close the event's
    /// incident by its key.
    #[allow(clippy::too_many_arguments)]
    async fn send_oncall(&self, t: &NotifyTarget, url: &str, key: &str, e: &Event, title: &str, text: &str, link: Option<&str>) -> (Option<u16>, Option<String>) {
        let Some((open, dedup, severity)) = oncall_of(e) else { return (None, None) };
        let source = if e.service.is_empty() { "roll apps".to_string() } else { format!("roll apps: {} in {}", e.service, e.environment) };
        let req = if t.kind == "pagerduty" {
            let mut body = json!({ "routing_key": key, "event_action": if open { "trigger" } else { "resolve" }, "dedup_key": dedup });
            if open {
                body["payload"] = json!({ "summary": title.chars().take(1024).collect::<String>(), "source": source, "severity": severity, "component": e.service, "group": e.environment, "class": e.kind, "custom_details": { "text": text, "data": e.data } });
                if let Some(l) = link {
                    body["links"] = json!([{ "href": l, "text": "Open in roll" }]);
                }
            }
            self.notify_client.post(url).json(&body)
        } else {
            let auth = format!("GenieKey {key}");
            if open {
                let priority = match severity {
                    "critical" => "P1",
                    "error" => "P2",
                    "warning" => "P3",
                    _ => "P5",
                };
                let description = match link {
                    Some(l) => format!("{text}\n\nOpen in roll: {l}"),
                    None => text.to_string(),
                };
                let body = json!({ "message": title.chars().take(130).collect::<String>(), "alias": dedup.chars().take(512).collect::<String>(), "description": description.chars().take(15_000).collect::<String>(), "priority": priority, "source": "roll", "entity": source, "tags": [e.environment, e.kind] });
                self.notify_client.post(url).header("authorization", auth).json(&body)
            } else {
                let alias: String = url::form_urlencoded::byte_serialize(dedup.as_bytes()).collect();
                self.notify_client.post(format!("{}/{alias}/close?identifierType=alias", url.trim_end_matches('/'))).header("authorization", auth).json(&json!({ "source": "roll" }))
            }
        };
        match req.send().await {
            Ok(res) if res.status().is_success() => (Some(res.status().as_u16()), None),
            // Closing what was never opened (or already closed) is done.
            Ok(res) if !open && res.status() == reqwest::StatusCode::NOT_FOUND => (Some(404), None),
            Ok(res) => {
                let code = res.status().as_u16();
                let said = String::from_utf8_lossy(&crate::net::read_bounded(res, 512).await).trim().chars().take(200).collect::<String>();
                (Some(code), Some(if said.is_empty() { format!("answered {code}") } else { format!("answered {code}: {said}") }))
            }
            Err(err) => (None, Some(err.without_url().to_string())),
        }
    }
}

/// Text for Slack's `mrkdwn`: `&`, `<` and `>` as entities, so nothing in it is a link or a
/// mention.
fn slack_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// An Adaptive Card's text renders Markdown links (`[Approve](https://…)`): a commit author's
/// name or an error can't make one (a zero-width space between `]` and `(`; nothing shows).
fn teams_escape(s: &str) -> String {
    s.replace("](", "]\u{200B}(")
}

#[cfg(test)]
mod tests {
    #[test]
    fn audit_entries_go_to_who_asks_and_never_page() {
        let ev = |e: &[&str]| e.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let data = serde_json::json!({});
        assert!(super::wants(&ev(&["audit"]), "audit.variables", &data, false));
        assert!(!super::wants(&ev(&["audit"]), "audit.variables", &data, true), "never an on-call page");
        assert!(!super::wants(&super::DEFAULT_EVENTS.map(String::from), "audit.variables", &data, false), "not by default");
    }

    #[test]
    fn a_teams_card_can_not_link() {
        assert!(!super::teams_escape("[Approve](https://evil)").contains("]("));
    }

    #[test]
    fn slack_text_can_not_mention_or_link() {
        assert_eq!(super::slack_escape("<!channel> <https://evil|Approve> & co"), "&lt;!channel&gt; &lt;https://evil|Approve&gt; &amp; co");
    }
}
