//! Alerts on a cluster's own health (docs/search/logs-end-to-end.md, section 3; the contract in
//! docs/search/README.md, "Health alerts").
//!
//! A rule is `{metric, op, threshold, window}` over what the control plane already keeps of a
//! cluster: its request metrics (the catalog's minute ring, as `GET …/metrics` reads it) and its
//! usage (this month's cost, as `GET …/usage` prices it). Rules live in roll's catalog, not in the
//! cluster: they hold while the cluster sleeps, and nothing through its Elasticsearch API reaches
//! them.
//!
//! - **Evaluation.** Every metering pass, on one node of a fleet (the singleton `search-alerts`;
//!   every node's pass has put its minutes in the catalog by then, so a window may miss up to one
//!   pass of another node's newest requests). A window with no requests gives latency and error
//!   rates no value, which isn't a breach; its request rate is 0.
//! - **State changes.** `ok` to `firing` and back, each a compare-and-swap on the rule's `seq` in
//!   one catalog write that also queues its deliveries (the outbox, `search_alert_deliveries`,
//!   one row per target, unique per change and target): a change is delivered once, never
//!   inline. A test send queues rows the same way.
//! - **Delivery.** Rows are claimed (a compare-and-swap on `next_at`) by whichever node runs the
//!   worker: the metering pass's, or a test send's. A webhook is a signed POST through
//!   `roll_net::PublicOnly` with no redirects (a private address is refused, by name or literal),
//!   as roll code's webhooks are signed: `x-roll-signature-256: sha256=<hex HMAC-SHA256(secret,
//!   "{timestamp}.{body}")>` with `x-roll-event`, `x-roll-delivery` and `x-roll-timestamp`. A chat
//!   target posts to a channel of a roll code chat install the rule's maker owns, checked again
//!   on every send ([`AlertChat`], rolld's). [`ATTEMPTS`] tries, waits growing threefold from
//!   `SearchConfig::alert_retry` up to six hours; the newest [`KEPT_DELIVERIES`] of a rule are kept.
//!
//! Search keeps its own outbox: roll code's (`review_deliveries`) is in code's database, so a
//! search state change couldn't promise into it in its own transaction.

use crate::control::{Control, ControlError, Result};
use crate::meter::now_ms;
use crate::store::SearchCluster;
use code_sql::{OptionalExtension, Row, params};
use search_server::metrics::Stats;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Rules a cluster may have.
pub const MAX_RULES: usize = 20;
/// Targets a rule may deliver to.
pub const MAX_TARGETS: usize = 5;
/// Tries of a delivery before it's `failed`.
pub const ATTEMPTS: i64 = 8;
/// Deliveries kept per rule (the newest).
pub const KEPT_DELIVERIES: i64 = 50;
/// How long a claimed delivery is the claimer's.
const CLAIM_MS: i64 = 60_000;
/// Deliveries one worker run takes.
const BATCH: i64 = 50;
/// The longest wait between tries.
const MAX_WAIT_MS: i64 = 6 * 3600 * 1000;

/// What a rule watches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    /// The 99th (95th) percentile latency over the window, in milliseconds.
    P99Ms,
    P95Ms,
    /// 5xx answers, a percentage of requests.
    ErrorRate,
    /// 429 answers (the cluster's limits), a percentage of requests.
    ThrottleRate,
    /// Requests a second.
    RequestRate,
    /// This UTC month's cost so far, in US dollars (`GET …/usage`'s pricing); no window.
    MonthCostUsd,
}

impl Metric {
    pub const ALL: [Metric; 6] = [Metric::P99Ms, Metric::P95Ms, Metric::ErrorRate, Metric::ThrottleRate, Metric::RequestRate, Metric::MonthCostUsd];

    pub fn name(self) -> &'static str {
        match self {
            Metric::P99Ms => "p99_ms",
            Metric::P95Ms => "p95_ms",
            Metric::ErrorRate => "error_rate",
            Metric::ThrottleRate => "throttle_rate",
            Metric::RequestRate => "request_rate",
            Metric::MonthCostUsd => "month_cost_usd",
        }
    }

    pub fn parse(s: &str) -> Option<Metric> {
        Metric::ALL.into_iter().find(|m| m.name() == s)
    }

    /// Its value over `minutes` of request metrics `window_s` long.
    pub fn of_requests(self, minutes: &[(i64, Stats)], window_s: i64) -> Option<f64> {
        let mut all = Stats::default();
        for (_, s) in minutes {
            all.merge(s);
        }
        let k = all.all();
        let n = k.requests();
        let share = |x: u64| (n > 0).then(|| x as f64 * 100.0 / n as f64);
        match self {
            Metric::P99Ms => k.latency.quantile(0.99).map(|us| us / 1000.0),
            Metric::P95Ms => k.latency.quantile(0.95).map(|us| us / 1000.0),
            Metric::ErrorRate => share(k.server_errors),
            Metric::ThrottleRate => share(k.rejected),
            Metric::RequestRate => Some(n as f64 / window_s.max(1) as f64),
            Metric::MonthCostUsd => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Gt,
    Ge,
    Lt,
    Le,
}

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Lt => "<",
            Op::Le => "<=",
        }
    }

    pub fn parse(s: &str) -> Option<Op> {
        [Op::Gt, Op::Ge, Op::Lt, Op::Le].into_iter().find(|o| o.name() == s)
    }

    pub fn holds(self, value: f64, threshold: f64) -> bool {
        match self {
            Op::Gt => value > threshold,
            Op::Ge => value >= threshold,
            Op::Lt => value < threshold,
            Op::Le => value <= threshold,
        }
    }
}

/// Where a rule's changes go.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A signed POST; the secret sealed to its rule and place.
    Webhook { url: String, secret: String },
    /// A channel of a roll code chat install (`slack` or `discord`).
    Chat { platform: String, install: String, channel: String },
}

impl Target {
    fn stored(&self) -> Value {
        match self {
            Target::Webhook { url, secret } => json!({ "type": "webhook", "url": url, "secret": secret }),
            Target::Chat { platform, install, channel } => json!({ "type": platform, "install": install, "channel": channel }),
        }
    }

    fn from_stored(v: &Value) -> Option<Target> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        match v.get("type").and_then(Value::as_str)? {
            "webhook" => Some(Target::Webhook { url: s("url")?, secret: s("secret")? }),
            p => Some(Target::Chat { platform: p.to_string(), install: s("install")?, channel: s("channel")? }),
        }
    }

    /// As answers show it: a webhook's origin only (a path may hold a token), never its secret.
    fn shown(&self) -> Value {
        match self {
            Target::Webhook { url, .. } => json!({ "type": "webhook", "url": shown_url(url) }),
            Target::Chat { platform, install, channel } => json!({ "type": platform, "install": install, "channel": channel }),
        }
    }
}

/// `url`'s scheme, host and port, and `/…` when it has more.
fn shown_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => {
            let origin = u.origin().ascii_serialization();
            if u.path() == "/" && u.query().is_none() { origin } else { format!("{origin}/…") }
        }
        Err(_) => "…".into(),
    }
}

/// A rule, as the catalog keeps it.
#[derive(Clone, Debug)]
pub struct Rule {
    pub id: String,
    pub cluster_id: String,
    pub name: String,
    pub metric: Metric,
    pub op: Op,
    pub threshold: f64,
    /// 0 for [`Metric::MonthCostUsd`].
    pub window_s: i64,
    pub targets: Vec<Target>,
    /// Who made it (`usr_…`, or `crate::remotes::OPERATOR`): chat targets post as far as they
    /// still may.
    pub made_by: String,
    pub created_at: i64,
    pub firing: bool,
    /// When it last changed state (its creation at first).
    pub since: i64,
    pub value: Option<f64>,
    pub evaluated_at: Option<i64>,
    /// Bumped by each state change.
    pub seq: i64,
}

const RULE_COLS: &str = "id, cluster_id, name, metric, op, threshold, window_s, targets, made_by, created_at, state, since, value, evaluated_at, seq";

fn rule_row(r: &Row<'_>) -> code_sql::Result<Rule> {
    let metric: String = r.get(3)?;
    let op: String = r.get(4)?;
    let targets: String = r.get(7)?;
    let state: String = r.get(10)?;
    Ok(Rule {
        id: r.get(0)?,
        cluster_id: r.get(1)?,
        name: r.get(2)?,
        metric: Metric::parse(&metric).unwrap_or(Metric::RequestRate),
        op: Op::parse(&op).unwrap_or(Op::Gt),
        threshold: r.get(5)?,
        window_s: r.get(6)?,
        targets: serde_json::from_str::<Vec<Value>>(&targets).unwrap_or_default().iter().filter_map(Target::from_stored).collect(),
        made_by: r.get(8)?,
        created_at: r.get(9)?,
        firing: state == "firing",
        since: r.get(11)?,
        value: r.get(12)?,
        evaluated_at: r.get(13)?,
        seq: r.get(14)?,
    })
}

impl Rule {
    fn window(&self) -> Value {
        if self.window_s == 0 { Value::Null } else { Value::from(window_text(self.window_s)) }
    }

    /// As the API answers it.
    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "metric": self.metric.name(),
            "op": self.op.name(),
            "threshold": self.threshold,
            "window": self.window(),
            "targets": self.targets.iter().map(Target::shown).collect::<Vec<_>>(),
            "state": if self.firing { "firing" } else { "ok" },
            "since": self.since,
            "value": self.value,
            "evaluated_at": self.evaluated_at,
            "created_at": self.created_at,
        })
    }

    /// `p99_ms > 500 over 5m`.
    fn text(&self) -> String {
        let over = if self.window_s == 0 { String::new() } else { format!(" over {}", window_text(self.window_s)) };
        format!("{} {} {}{over}", self.metric.name(), self.op.name(), self.threshold)
    }
}

/// `300` as `5m`, `7200` as `2h`.
fn window_text(s: i64) -> String {
    if s % 3600 == 0 { format!("{}h", s / 3600) } else { format!("{}m", s / 60) }
}

/// `5m` or `2h` (whole minutes, 1m to 24h).
fn parse_window(s: &str) -> Option<i64> {
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = n.parse().ok()?;
    let s = match unit {
        "m" => n.checked_mul(60)?,
        "h" => n.checked_mul(3600)?,
        _ => return None,
    };
    (60..=86_400).contains(&s).then_some(s)
}

/// Delivers chat targets through roll code's chat installs (rolld's, over `code_control::Code`).
#[async_trait::async_trait]
pub trait AlertChat: Send + Sync {
    /// Whether `made_by` (`usr_…`, or `crate::remotes::OPERATOR`) may post through `install` on
    /// `platform`: an install of their own roll code owner.
    async fn may_use(&self, made_by: &str, platform: &str, install: &str) -> std::result::Result<bool, String>;
    /// Posts `text` (plain; the install escapes it) to `channel`.
    async fn post(&self, install: &str, channel: &str, text: &str) -> std::result::Result<(), String>;
}

/// The control's alert parts.
pub struct Alerts {
    chat: OnceLock<Arc<dyn AlertChat>>,
    http: reqwest::Client,
    /// A worker runs on this node.
    delivering: AtomicBool,
}

impl Alerts {
    pub fn new(allow_private: bool) -> Alerts {
        let mut b = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(10)).no_proxy();
        if !allow_private {
            b = b.dns_resolver(roll_net::PublicOnly);
        }
        Alerts { chat: OnceLock::new(), http: b.build().expect("the alerts' HTTP client"), delivering: AtomicBool::new(false) }
    }
}

/// A new rule as the API takes it.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewRule {
    pub name: Option<String>,
    pub metric: String,
    pub op: String,
    pub threshold: f64,
    pub window: Option<String>,
    pub targets: Vec<NewTarget>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewTarget {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: Option<String>,
    /// A webhook's signing secret, which the receiver also has (16 to 200 characters).
    pub secret: Option<String>,
    pub install: Option<String>,
    pub channel: Option<String>,
}

impl From<code_sql::Error> for ControlError {
    fn from(e: code_sql::Error) -> ControlError {
        ControlError::Internal(e.into())
    }
}

fn invalid(m: impl Into<String>) -> ControlError {
    ControlError::Invalid(m.into())
}

fn sealed_kind(rule: &str, i: usize) -> String {
    format!("search alert webhook secret {rule} {i}")
}

impl Control {
    /// Gives chat targets a way out (rolld's; without it they're refused).
    pub fn set_alert_chat(&self, chat: Arc<dyn AlertChat>) {
        let _ = self.alerts.chat.set(chat);
    }

    /// A webhook URL a rule may name: http(s), a host, no credentials, and not a private address
    /// written as one (a name that resolves to one fails when sent).
    fn check_webhook(&self, url: &str) -> Result<()> {
        let u = url::Url::parse(url).map_err(|_| invalid(format!("[url] isn't a URL: {}", shown_url(url))))?;
        if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none_or(str::is_empty) || !u.username().is_empty() || u.password().is_some() || url.len() > 2048 {
            return Err(invalid("[url] must be an http or https URL with a host, no credentials, at most 2048 bytes"));
        }
        if !self.cfg.alerts_allow_private && roll_net::literal_private_host(&u) {
            return Err(invalid("[url] is a private address: alerts are only sent to public ones"));
        }
        Ok(())
    }

    /// Adds a rule to cluster `id`, made by `made_by`.
    pub async fn create_alert(&self, id: &str, made_by: &str, b: NewRule) -> Result<Value> {
        let c = self.get(id).await?;
        let metric = Metric::parse(&b.metric).ok_or_else(|| invalid(format!("[metric] is one of {}", Metric::ALL.map(Metric::name).join(", "))))?;
        let op = Op::parse(&b.op).ok_or_else(|| invalid("[op] is one of >, >=, <, <="))?;
        if !b.threshold.is_finite() {
            return Err(invalid("[threshold] must be a number"));
        }
        let window_s = match (metric, b.window.as_deref()) {
            (Metric::MonthCostUsd, None) => 0,
            (Metric::MonthCostUsd, Some(_)) => return Err(invalid("[month_cost_usd] is the month so far: it takes no [window]")),
            (_, None) => return Err(invalid("[window] is required, e.g. 5m (whole minutes, 1m to 24h)")),
            (_, Some(w)) => parse_window(w).ok_or_else(|| invalid("[window] is whole minutes from 1m to 24h, e.g. 5m or 2h"))?,
        };
        let name = b.name.unwrap_or_default();
        if name.len() > 100 || name.chars().any(char::is_control) {
            return Err(invalid("[name] is at most 100 bytes, no control characters"));
        }
        if b.targets.is_empty() || b.targets.len() > MAX_TARGETS {
            return Err(invalid(format!("[targets] holds 1 to {MAX_TARGETS} targets")));
        }
        let rule_id = format!("alr_{}", crate::control::new_id().trim_start_matches("srch_"));
        let mut targets = Vec::new();
        for (i, t) in b.targets.into_iter().enumerate() {
            match t.kind.as_str() {
                "webhook" => {
                    let (Some(url), Some(secret)) = (t.url, t.secret) else { return Err(invalid("a webhook target needs [url] and [secret]")) };
                    if t.install.is_some() || t.channel.is_some() {
                        return Err(invalid("a webhook target takes [url] and [secret] only"));
                    }
                    self.check_webhook(&url)?;
                    if !(16..=200).contains(&secret.len()) {
                        return Err(invalid("a webhook's [secret] is 16 to 200 characters (your receiver checks signatures with it)"));
                    }
                    targets.push(Target::Webhook { url, secret: self.store.seal_secret(&sealed_kind(&rule_id, i), &secret) });
                }
                "slack" | "discord" => {
                    let (Some(install), Some(channel)) = (t.install, t.channel) else { return Err(invalid("a chat target needs [install] and [channel]")) };
                    if t.url.is_some() || t.secret.is_some() {
                        return Err(invalid("a chat target takes [install] and [channel] only: never a copied webhook URL"));
                    }
                    if channel.is_empty() || channel.len() > 64 || !channel.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                        return Err(invalid("[channel] is the platform's channel id"));
                    }
                    let Some(chat) = self.alerts.chat.get() else { return Err(invalid("chat targets need roll code's chat ops on this server")) };
                    if !chat.may_use(made_by, &t.kind, &install).await.map_err(|e| ControlError::Internal(anyhow::anyhow!(e)))? {
                        return Err(invalid(format!("[install] isn't a {} install of yours (roll code's chat ops: connect one first)", t.kind)));
                    }
                    targets.push(Target::Chat { platform: t.kind, install, channel });
                }
                other => return Err(invalid(format!("a target's [type] is webhook, slack or discord, not [{other}]"))),
            }
        }
        let now = now_ms() / 1000;
        let stored = serde_json::to_string(&targets.iter().map(Target::stored).collect::<Vec<_>>()).unwrap_or_default();
        let cid = c.id.clone();
        let rule = Rule { id: rule_id, cluster_id: cid.clone(), name, metric, op, threshold: b.threshold, window_s, targets, made_by: made_by.to_string(), created_at: now, firing: false, since: now, value: None, evaluated_at: None, seq: 0 };
        let r = rule.clone();
        let added = self.store.db().write(move |x| {
            let n: i64 = x.query_row("SELECT COUNT(*) FROM search_alert_rules WHERE cluster_id = ?1", params![cid], |row| row.get(0))?;
            if n as usize >= MAX_RULES {
                return Ok::<_, code_sql::Error>(false);
            }
            x.execute(
                &format!("INSERT INTO search_alert_rules ({RULE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'ok', ?11, NULL, NULL, 0)"),
                params![r.id, r.cluster_id, r.name, r.metric.name(), r.op.name(), r.threshold, r.window_s, stored, r.made_by, r.created_at, r.since],
            )?;
            Ok(true)
        })?;
        if !added {
            return Err(ControlError::Conflict(format!("cluster [{}] has {MAX_RULES} alert rules, the most it may have: delete one first", c.name)));
        }
        Ok(rule.to_value())
    }

    async fn rule_of(&self, cluster_id: &str, rule: &str) -> Result<Rule> {
        let (cid, rid) = (cluster_id.to_string(), rule.to_string());
        let found = self.store.db().read(move |x| x.query_row(&format!("SELECT {RULE_COLS} FROM search_alert_rules WHERE id = ?1 AND cluster_id = ?2"), params![rid, cid], rule_row).optional())?;
        found.ok_or_else(|| ControlError::NotFound(format!("no alert rule [{rule}]")))
    }

    pub async fn alerts(&self, id: &str) -> Result<Value> {
        let c = self.get(id).await?;
        let cid = c.id.clone();
        let rules: Vec<Rule> = self.store.db().read(move |x| x.query(&format!("SELECT {RULE_COLS} FROM search_alert_rules WHERE cluster_id = ?1 ORDER BY created_at, id"), params![cid], rule_row))?;
        Ok(json!({ "rules": rules.iter().map(Rule::to_value).collect::<Vec<_>>() }))
    }

    pub async fn alert(&self, id: &str, rule: &str) -> Result<Value> {
        let c = self.get(id).await?;
        Ok(self.rule_of(&c.id, rule).await?.to_value())
    }

    /// Deletes a rule and what it still owed.
    pub async fn delete_alert(&self, id: &str, rule: &str) -> Result<()> {
        let c = self.get(id).await?;
        let (cid, rid) = (c.id.clone(), rule.to_string());
        let gone = self.store.db().write(move |x| {
            let n = x.execute("DELETE FROM search_alert_rules WHERE id = ?1 AND cluster_id = ?2", params![rid, cid])?;
            x.execute("DELETE FROM search_alert_deliveries WHERE rule_id = ?1", params![rid])?;
            Ok::<_, code_sql::Error>(n)
        })?;
        if gone == 0 {
            return Err(ControlError::NotFound(format!("no alert rule [{rule}]")));
        }
        Ok(())
    }

    /// Queues a test delivery to each of a rule's targets (sent by the worker, started now).
    pub async fn test_alert(&self, id: &str, rule: &str) -> Result<Value> {
        let c = self.get(id).await?;
        let r = self.rule_of(&c.id, rule).await?;
        let now = now_ms();
        let payload = payload(&c, &r, "search.alert.test", r.value, now / 1000);
        let ids: Vec<String> = (0..r.targets.len()).map(|_| format!("ald_{}", crate::control::new_id().trim_start_matches("srch_"))).collect();
        let (rid, cid, rows) = (r.id.clone(), c.id.clone(), ids.clone());
        self.store.db().write(move |x| {
            for (i, d) in rows.iter().enumerate() {
                x.execute(
                    "INSERT INTO search_alert_deliveries (id, rule_id, cluster_id, target, event, seq, payload, status, attempts, next_at, created_at) VALUES (?1, ?2, ?3, ?4, 'search.alert.test', NULL, ?5, 'pending', 0, ?6, ?7)",
                    params![d, rid, cid, i as i64, payload, now, now / 1000],
                )?;
            }
            trim_deliveries(x, &rid)
        })?;
        if let Some(me) = self.me.get().and_then(std::sync::Weak::upgrade) {
            tokio::spawn(async move { me.deliver_alerts().await });
        }
        Ok(json!({ "queued": ids }))
    }

    pub async fn alert_deliveries(&self, id: &str, rule: &str) -> Result<Value> {
        let c = self.get(id).await?;
        let r = self.rule_of(&c.id, rule).await?;
        let rid = r.id.clone();
        let rows: Vec<Value> = self.store.db().read(move |x| {
            x.query(
                "SELECT id, target, event, status, attempts, next_at, last_status, last_error, created_at, delivered_at FROM search_alert_deliveries WHERE rule_id = ?1 ORDER BY created_at DESC, id LIMIT 50",
                params![rid],
                |row| {
                    let (id, target, event, status): (String, i64, String, String) = (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?);
                    let (attempts, next_at, last_status, last_error): (i64, i64, Option<i64>, Option<String>) = (row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?);
                    let (created_at, delivered_at): (i64, Option<i64>) = (row.get(8)?, row.get(9)?);
                    Ok(json!({
                        "id": id,
                        "target": target,
                        "event": event,
                        "next_at": (status == "pending").then_some(next_at / 1000),
                        "status": status,
                        "attempts": attempts,
                        "last_status": last_status,
                        "last_error": last_error,
                        "created_at": created_at,
                        "delivered_at": delivered_at,
                    }))
                },
            )
        })?;
        Ok(json!({ "deliveries": rows }))
    }

    /// One evaluation of every rule (the metering pass's, on one node of a fleet): their values
    /// recorded, and each state change written with its deliveries in one transaction.
    pub(crate) async fn evaluate_alerts(&self) -> anyhow::Result<()> {
        if let Some(cl) = self.cluster_leases().map(|l| l.cluster().clone())
            && !tokio::task::spawn_blocking(move || cl.singleton("search-alerts")).await.unwrap_or(false)
        {
            return Ok(());
        }
        let rules: Vec<Rule> = self.store.db().read(|x| {
            x.query(&format!("SELECT {} FROM search_alert_rules r JOIN search_clusters c ON c.id = r.cluster_id WHERE c.deleted_at IS NULL ORDER BY r.cluster_id", RULE_COLS.split(", ").map(|col| format!("r.{col}")).collect::<Vec<_>>().join(", ")), params![], rule_row)
        })?;
        let now_s = now_ms() / 1000;
        let mut cluster: Option<SearchCluster> = None;
        for r in rules {
            if cluster.as_ref().is_none_or(|c| c.id != r.cluster_id) {
                cluster = self.store.get(&r.cluster_id).await?;
            }
            let Some(c) = &cluster else { continue };
            let value = match r.metric {
                Metric::MonthCostUsd => Some(self.month_cost(c, now_s).await?),
                m => {
                    // The window's minutes, the current one included.
                    let from = now_s - now_s.rem_euclid(60) - r.window_s + 60;
                    m.of_requests(&self.store.metrics_since(&c.id, 60, from).await?, r.window_s)
                }
            };
            let breach = value.is_some_and(|v| r.op.holds(v, r.threshold));
            if breach == r.firing {
                let (rid, seq) = (r.id.clone(), r.seq);
                self.store.db().write(move |x| x.execute("UPDATE search_alert_rules SET value = ?1, evaluated_at = ?2 WHERE id = ?3 AND seq = ?4", params![value, now_s, rid, seq]).map(drop))?;
                continue;
            }
            let event = if breach { "search.alert.firing" } else { "search.alert.resolved" };
            let body = payload(c, &Rule { firing: breach, since: now_s, value, ..r.clone() }, event, value, now_s);
            let ids: Vec<String> = (0..r.targets.len()).map(|_| format!("ald_{}", crate::control::new_id().trim_start_matches("srch_"))).collect();
            let (rid, cid, seq, state) = (r.id.clone(), r.cluster_id.clone(), r.seq, if breach { "firing" } else { "ok" });
            let now = now_ms();
            self.store.db().write(move |x| {
                // Another evaluator's change won: nothing more is owed.
                if x.execute("UPDATE search_alert_rules SET state = ?1, since = ?2, value = ?3, evaluated_at = ?2, seq = seq + 1 WHERE id = ?4 AND seq = ?5", params![state, now_s, value, rid, seq])? == 0 {
                    return Ok::<_, code_sql::Error>(());
                }
                for (i, d) in ids.iter().enumerate() {
                    x.execute(
                        "INSERT INTO search_alert_deliveries (id, rule_id, cluster_id, target, event, seq, payload, status, attempts, next_at, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', 0, ?8, ?9) ON CONFLICT (rule_id, seq, target) DO NOTHING",
                        params![d, rid, cid, i as i64, event, seq + 1, body, now, now_s],
                    )?;
                }
                trim_deliveries(x, &rid)
            })?;
        }
        Ok(())
    }

    /// This UTC month's cost of `c` so far, as `GET …/usage` prices it.
    async fn month_cost(&self, c: &SearchCluster, now_s: i64) -> anyhow::Result<f64> {
        let start = month_start(now_s);
        let totals = self.store.usage_since(&c.id, start, now_s).await?;
        let hours = (now_s - start.max(c.created_at)).max(0) as f64 / 3600.0;
        Ok(self.cfg.pricing.cost(&totals, hours, c.stored_bytes).total_usd)
    }

    /// Sends what's due (the metering pass's run, or a test send's): each delivery claimed
    /// first, so two nodes never send one twice at once.
    pub async fn deliver_alerts(&self) {
        if self.alerts.delivering.swap(true, Ordering::SeqCst) {
            return;
        }
        loop {
            let now = now_ms();
            let claimed: Vec<(String, String, i64, String, i64)> = match self.store.db().write(move |x| {
                let due: Vec<(String, String, i64, String, i64, i64)> = x.query(
                    "SELECT id, rule_id, target, payload, attempts, next_at FROM search_alert_deliveries WHERE status = 'pending' AND next_at <= ?1 ORDER BY next_at LIMIT ?2",
                    params![now, BATCH],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                )?;
                let mut mine = Vec::new();
                for (id, rule, target, payload, attempts, at) in due {
                    if x.execute("UPDATE search_alert_deliveries SET next_at = ?1 WHERE id = ?2 AND next_at = ?3 AND status = 'pending'", params![now + CLAIM_MS, id, at])? == 1 {
                        mine.push((id, rule, target, payload, attempts));
                    }
                }
                Ok::<_, code_sql::Error>(mine)
            }) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "claiming search alert deliveries failed");
                    break;
                }
            };
            if claimed.is_empty() {
                break;
            }
            for (id, rule, target, payload, attempts) in claimed {
                let outcome = self.send(&id, &rule, target, &payload).await;
                let tries = attempts + 1;
                let now = now_ms();
                let (status, next_at, code, error) = match &outcome {
                    Ok(code) => ("delivered", now, *code, None),
                    Err((code, e)) if tries >= ATTEMPTS => ("failed", now, *code, Some(e.clone())),
                    Err((code, e)) => ("pending", now + wait_ms(self.cfg.alert_retry, tries), *code, Some(e.clone())),
                };
                let delivered = (status == "delivered").then_some(now / 1000);
                if let Err(e) = self.store.db().write(move |x| {
                    x.execute(
                        "UPDATE search_alert_deliveries SET status = ?1, attempts = ?2, next_at = ?3, last_status = ?4, last_error = ?5, delivered_at = ?6 WHERE id = ?7",
                        params![status, tries, next_at, code, error, delivered, id],
                    )
                    .map(drop)
                }) {
                    tracing::warn!(error = %e, "recording a search alert delivery failed");
                }
            }
        }
        self.alerts.delivering.store(false, Ordering::SeqCst);
    }

    /// One try: the HTTP status a webhook answered (`None` for chat), or why it failed.
    async fn send(&self, delivery: &str, rule: &str, target: i64, payload: &str) -> std::result::Result<Option<i64>, (Option<i64>, String)> {
        let rid = rule.to_string();
        let r = match self.store.db().read(move |x| x.query_row(&format!("SELECT {RULE_COLS} FROM search_alert_rules WHERE id = ?1"), params![rid], rule_row).optional()) {
            Ok(Some(r)) => r,
            Ok(None) => return Err((None, "the rule was deleted".into())),
            Err(e) => return Err((None, format!("reading the rule failed: {e}"))),
        };
        let event: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
        match r.targets.get(target as usize) {
            None => Err((None, "the rule has no such target".into())),
            Some(Target::Webhook { url, secret }) => {
                let Some(secret) = self.store.open_secret(&sealed_kind(&r.id, target as usize), secret) else { return Err((None, "the webhook's secret can't be opened".into())) };
                let parsed = url::Url::parse(url).map_err(|_| (None, "the webhook's URL doesn't parse".to_string()))?;
                if !self.cfg.alerts_allow_private && roll_net::literal_private_host(&parsed) {
                    return Err((None, "a private address: not sent".into()));
                }
                let ts = now_ms() / 1000;
                let kind = event.get("event").and_then(Value::as_str).unwrap_or("search.alert");
                let answer = self
                    .alerts
                    .http
                    .post(parsed)
                    .header("content-type", "application/json")
                    .header("user-agent", "roll-search-alerts")
                    .header("x-roll-event", kind)
                    .header("x-roll-delivery", delivery)
                    .header("x-roll-timestamp", ts.to_string())
                    .header("x-roll-signature-256", signature(&secret, ts, payload.as_bytes()))
                    .body(payload.to_string())
                    .send()
                    .await;
                match answer {
                    // The body isn't read: the status is the answer.
                    Ok(a) if a.status().is_success() => Ok(Some(a.status().as_u16() as i64)),
                    Ok(a) => Err((Some(a.status().as_u16() as i64), format!("the webhook answered {}", a.status().as_u16()))),
                    Err(e) => Err((None, send_error(&e))),
                }
            }
            Some(Target::Chat { platform, install, channel }) => {
                let Some(chat) = self.alerts.chat.get() else { return Err((None, "chat ops aren't running on this server".into())) };
                // The maker's rights now, not when they made it.
                match chat.may_use(&r.made_by, platform, install).await {
                    Ok(true) => {}
                    Ok(false) => return Err((None, "the install is no longer one of the rule maker's".into())),
                    Err(e) => return Err((None, e)),
                }
                chat.post(install, channel, &chat_text(&event)).await.map(|()| None).map_err(|e| (None, e))
            }
        }
    }
}

/// Why a send failed, without the URL (it may hold a token).
fn send_error(e: &reqwest::Error) -> String {
    let mut s = if e.is_timeout() {
        "timed out".to_string()
    } else if e.is_connect() {
        "couldn't connect".to_string()
    } else {
        "the request failed".to_string()
    };
    let mut src: Option<&dyn std::error::Error> = std::error::Error::source(e);
    while let Some(c) = src {
        if c.to_string().contains("private") {
            s.push_str(": a private address, not sent");
            break;
        }
        src = c.source();
    }
    s
}

/// `sha256=<hex HMAC-SHA256(secret, "{timestamp}.{body}")>`, as roll code signs its webhooks.
pub fn signature(secret: &str, timestamp: i64, body: &[u8]) -> String {
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// The wait before try `tries + 1`: `base` × 3^(tries − 1), at most six hours.
fn wait_ms(base: Duration, tries: i64) -> i64 {
    let base = base.as_millis().min(MAX_WAIT_MS as u128) as i64;
    let mut w = base.max(1);
    for _ in 1..tries {
        w = w.saturating_mul(3).min(MAX_WAIT_MS);
    }
    w
}

/// The first second of `now_s`'s UTC month.
fn month_start(now_s: i64) -> i64 {
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm), and back.
    let days = now_s.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5;
    (days - day) * 86_400
}

/// What a delivery carries.
fn payload(c: &SearchCluster, r: &Rule, event: &str, value: Option<f64>, at: i64) -> String {
    json!({
        "event": event,
        "at": at,
        "cluster": { "id": c.id, "name": c.name },
        "rule": { "id": r.id, "name": r.name, "metric": r.metric.name(), "op": r.op.name(), "threshold": r.threshold, "window": r.window() },
        "state": if event == "search.alert.test" { Value::Null } else { Value::from(if r.firing { "firing" } else { "ok" }) },
        "value": value,
        "text": format!("{} ({})", r.text(), r.name).replace(" ()", ""),
    })
    .to_string()
}

/// A delivery as a chat line.
fn chat_text(event: &Value) -> String {
    let s = |p: &[&str]| p.iter().try_fold(event, |v, k| v.get(*k)).and_then(Value::as_str).unwrap_or_default().to_string();
    let what = match event.get("event").and_then(Value::as_str) {
        Some("search.alert.firing") => "firing",
        Some("search.alert.resolved") => "resolved",
        _ => "test",
    };
    let value = event.get("value").and_then(Value::as_f64).map(|v| format!(", now {}", (v * 100.0).round() / 100.0)).unwrap_or_default();
    format!("roll search alert {what}: cluster {}: {}{value}", s(&["cluster", "name"]), s(&["text"]))
}

/// Keeps a rule's newest [`KEPT_DELIVERIES`] deliveries.
fn trim_deliveries(x: &code_sql::Conn<'_>, rule: &str) -> code_sql::Result<()> {
    x.execute(
        "DELETE FROM search_alert_deliveries WHERE rule_id = ?1 AND id NOT IN (SELECT id FROM search_alert_deliveries WHERE rule_id = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2)",
        params![rule, KEPT_DELIVERIES],
    )
    .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_months_and_waits() {
        assert_eq!((parse_window("5m"), parse_window("2h"), parse_window("24h"), parse_window("0m"), parse_window("25h"), parse_window("5s")), (Some(300), Some(7200), Some(86_400), None, None, None));
        // 2026-11-27T04:00:00Z is in the month that began 2026-11-01T00:00:00Z.
        assert_eq!(month_start(1_795_752_000), 1_793_491_200);
        assert_eq!(month_start(1_793_491_200), 1_793_491_200);
        let base = Duration::from_secs(30);
        assert_eq!((wait_ms(base, 1), wait_ms(base, 2), wait_ms(base, 20)), (30_000, 90_000, MAX_WAIT_MS));
        // The format roll code's receivers check.
        assert_eq!(signature("s", 1, b"{}"), format!("sha256={}", {
            use hmac::Mac;
            let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"s").unwrap();
            m.update(b"1.{}");
            hex::encode(m.finalize().into_bytes())
        }));
    }
}
