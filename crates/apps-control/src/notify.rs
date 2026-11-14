//! Notifications (docs/apps/README.md, "Notifications"): a project's events go to its targets, a
//! webhook (JSON, signed with HMAC-SHA256), a Slack or a Discord incoming webhook.
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
pub const EVENTS: [&str; 15] = [
    "deploy.failed",
    "crash_loop",
    "deploy.active",
    "spend.alert",
    "spend.limit",
    "alert.memory",
    "alert.errors",
    "alert.latency",
    "alert.log",
    "cron.failed",
    "job.failed",
    "domain.active",
    "domain.expiring",
    "domain.expired",
    "domain.tls_failed",
];
const DEFAULT_EVENTS: [&str; 12] = ["deploy.failed", "crash_loop", "spend.alert", "spend.limit", "alert.memory", "alert.errors", "alert.latency", "alert.log", "cron.failed", "domain.expiring", "domain.expired", "domain.tls_failed"];
/// Whether a target with `events` hears an event: its own kind, or for a resolution, the alert's
/// (whoever heard an alert hears it clear; there's nothing else to ask for).
fn wants(events: &[String], kind: &str, data: &Value) -> bool {
    match kind {
        "alert.resolved" => data["alert"].as_str().is_some_and(|a| events.iter().any(|e| e == a)),
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
        if let Some(k) = o.keys().find(|k| !["kind", "url", "events", "environments", "services", "previews"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let kind = o.get("kind").and_then(Value::as_str).unwrap_or("webhook").to_string();
        let url = o.get("url").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("url is required".into()))?.trim().to_string();
        let host = self.check_target(&kind, &url)?;
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
        let (k, u, s2, ev) = (kind.clone(), url.clone(), secret.clone(), events.clone());
        let t = self.db(move |s| s.add_target(&p, &k, &host, &u, s2.as_deref(), &ev, &filters)).await?;
        let mut v = serde_json::to_value(&t).expect("JSON");
        if let Some(s) = secret {
            v["secret"] = json!(s);
        }
        Ok(v)
    }

    /// A target's events and filters from a body: what's absent is `current`'s (an edit), else
    /// the defaults (a new one). A service by its name or id, kept by id; only the project's.
    async fn target_fields(self: &Arc<Self>, project: &str, o: &serde_json::Map<String, Value>, current: Option<&NotifyTarget>) -> Result<(Vec<String>, crate::store::TargetFilters)> {
        let events: Vec<String> = match o.get("events") {
            None | Some(Value::Null) => current.map_or_else(|| DEFAULT_EVENTS.map(String::from).to_vec(), |c| c.events.clone()),
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
            "slack" if !self.cfg.allow_private => return bad("a Slack incoming webhook is on hooks.slack.com"),
            "discord" if !self.cfg.allow_private => return bad("a Discord webhook is https://discord.com/api/webhooks/…"),
            "slack" | "discord" => {}
            _ => return Err(Error::BadRequest("kind is webhook, slack or discord".into())),
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
                    let sid = e.service.clone();
                    let Ok(svc) = self.db(move |s| s.service(&sid)).await else { continue };
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
                if wants(&t.events, &e.kind, &e.data) && t.follows(&e.service, &e.environment, preview) {
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
        let name = self.db(move |s| s.service(&sid)).await.map(|s| s.name).unwrap_or_else(|_| e.service.clone());
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
            "deploy.failed" => (format!("{what}: deploy failed"), format!("The deploy{rev} failed: {}. The live deployment keeps serving.", e.data["error"].as_str().unwrap_or("no reason given"))),
            "deploy.active" => (format!("{what}: deployed"), format!("A new deployment{rev} is live.")),
            "spend.alert" | "spend.limit" => {
                let (project, usd, limit) = (e.data["project_name"].as_str().unwrap_or("a project"), e.data["usd"].as_f64().unwrap_or(0.0), e.data["limit_usd"].as_f64().unwrap_or(0.0));
                if e.kind == "spend.alert" {
                    (format!("{project}: spend alert"), format!("${usd:.2} this month, past the ${limit:.2} alert."))
                } else {
                    (format!("{project}: spending limit reached"), format!("${usd:.2} this month reached the ${limit:.2} limit: its services are stopped until the limit is raised or the month ends."))
                }
            }
            "alert.memory" => (format!("{what}: memory"), format!("Its instances used {}% of their {} MB limit for five minutes (the alert is at {}%).", e.data["value"], e.data["limit_mb"], e.data["threshold"])),
            "alert.errors" => (format!("{what}: errors"), format!("{}% of its {} requests in five minutes answered 5xx (the alert is at {}%).", e.data["value"], e.data["requests"], e.data["threshold"])),
            "alert.latency" => (format!("{what}: slow"), format!("Its p95 latency reached {} ms (the alert is at {} ms).", e.data["value"], e.data["threshold"])),
            "alert.log" => (format!("{what}: its log says {}", e.data["pattern"]), format!("{} lines in five minutes matched (the alert is at {}).", e.data["count"], e.data["threshold"])),
            "alert.resolved" => {
                let which = match e.data["alert"].as_str() {
                    Some("alert.memory") => "memory",
                    Some("alert.errors") => "errors",
                    Some("alert.latency") => "latency",
                    Some("alert.log") => "log",
                    _ => "an",
                };
                (format!("{what}: {which} alert resolved"), format!("The {which} alert cleared: the last five minutes are under its threshold."))
            }
            "cron.failed" | "job.failed" => {
                let what_ran = if e.kind == "cron.failed" { "A scheduled run" } else { "A job" };
                let how = match (e.data["status"].as_str(), e.data["exit_code"].as_i64()) {
                    (Some("timed_out"), _) => "ran past its timeout".to_string(),
                    (_, Some(c)) => format!("exited with code {c}"),
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
        let body = match t.kind.as_str() {
            // Slack reads `<…>` as links and mentions (`<!channel>`) and `&` as an entity start:
            // a commit author or an error can't ping a channel or hide a link.
            "slack" => json!({ "text": format!("*{}*\n{}{}", slack_escape(title), slack_escape(text), link.as_ref().map(|l| format!("\n<{l}|Open in roll>")).unwrap_or_default()) }),
            "discord" => json!({ "content": format!("**{title}**\n{text}{}", link.as_ref().map(|l| format!("\n{l}")).unwrap_or_default()), "allowed_mentions": { "parse": [] } }),
            _ => json!({ "type": e.kind, "at": e.at, "seq": e.seq, "service": e.service, "environment": e.environment, "deployment": e.deployment, "data": e.data, "title": title, "text": text, "url": link }),
        };
        let bytes = serde_json::to_vec(&body).expect("JSON");
        let mut req = self.notify_client.post(url).header("content-type", "application/json");
        if let Some(s) = secret {
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

/// Text for Slack's `mrkdwn`: `&`, `<` and `>` as entities, so nothing in it is a link or a
/// mention.
fn slack_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    #[test]
    fn slack_text_can_not_mention_or_link() {
        assert_eq!(super::slack_escape("<!channel> <https://evil|Approve> & co"), "&lt;!channel&gt; &lt;https://evil|Approve&gt; &amp; co");
    }
}
