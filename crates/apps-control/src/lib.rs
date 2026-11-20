//! roll apps (docs/apps/README.md): services in workspace projects, built from roll code repos,
//! uploads or public images, deployed as immutable deployments into environments, and run as
//! instances that scale to zero and wake on the first request.
//!
//! - [`store`]: the catalog (`code_sql`).
//! - [`deploy`]: the deploy queue per service and environment: build, roll out, switch.
//! - [`live`]: an environment's instances on the service's owner: wake, suspend, stop, scale.
//! - [`runtime`]: where instances run (Docker; a fake in tests) and how images are built.
//! - [`public`]: the waking proxy for services' hostnames and ports.
//! - [`api`]: `/v1/apps` (docs/apps/http-api.md).
//!
//! On a fleet every service has one owner, the holder of the lease `apps:service:<id>`, which
//! runs its deploys, instances, cron and meter; other nodes forward to it.

pub mod api;
pub mod challenge;
pub mod backups;
pub mod hooks;
pub mod tags;
pub mod sites;
pub mod skew;
pub mod site_rules;
pub mod tunnel;
pub mod tcp;
pub mod refs;
pub mod feedback;
pub mod scale;
pub mod registry;
pub mod config;
pub mod cron;
pub mod ci_hosts;
pub mod deploy;
pub mod errors;
pub mod explain;
pub mod drains;
pub mod export;
pub mod git_source;
pub mod image_upload;
pub mod images;
pub mod integrations;
pub mod jobs;
pub mod live;
pub mod log_where;
pub mod logs;
pub mod metrics;
pub mod model;
pub mod net;
pub mod notify;
pub mod otlp;
pub mod public;
pub mod push;
pub mod resp_cache;
pub mod runtime;
pub mod shell;
pub mod spend;
pub mod store;
pub mod traces;
pub mod visit;
pub mod vitals;
pub mod heartbeats;
pub mod slo;
pub mod prom;
pub mod edge;
pub mod password;
pub mod freeze;
pub mod status_page;
pub mod vulns;
pub mod checks;
pub mod forms;
pub mod track;
pub mod traffic;

use integrations::{DomainTls, Resolver, Resources, SecretSource, Sources};
use live::{EnvLive, LiveConfig};
use logs::{LogHub, Stream};
use metrics::Prices;
use model::*;
use parking_lot::Mutex;
use runtime::{Builder, Runtime};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a host's answer is kept on a node that didn't change it ([`Apps::env_for_host`]).
const HOST_TTL: Duration = Duration::from_secs(1);
/// How long a deleted service is kept, stopped, for a restore (Railway's 48 hours).
pub const DELETED_KEPT_MS: i64 = 48 * 3600 * 1000;
use store::{EnvRow, ServiceRow, Store};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    TooLarge(String),
    /// Allowed into the route, but not to what it would show (another product's rule).
    #[error("{0}")]
    Forbidden(String),
    /// For a moment only (the service is moving between nodes): a 503, worth a retry.
    #[error("{0}")]
    Unavailable(String),
    #[error("apps database: {0}")]
    Db(#[from] code_sql::Error),
    #[error("{0}")]
    Internal(String),
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Internal(e.to_string())
    }
}

/// How long a custom domain may wait for its DNS before the claim is dropped.
pub const PENDING_CLAIM_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// The ownership key of a service.
pub fn lease_key(service: &str) -> String {
    format!("apps:service:{service}")
}

#[derive(Clone, Debug)]
pub struct AppsConfig {
    pub home: PathBuf,
    /// Generated hostnames are `<slug>.<domain>`.
    pub domain: String,
    /// How generated URLs are written: scheme, and the port when it isn't the scheme's own.
    pub scheme: String,
    pub url_port: Option<u16>,
    /// A loopback-free port of its own per environment, from this range (one node only).
    pub ports: Option<(u16, u16)>,
    /// TCP services' ports (`run.tcp`), and the host clients reach them at (one node only).
    pub tcp_ports: Option<(u16, u16)>,
    pub tcp_host: String,
    /// Domains users can't add as custom domains (roll's own).
    pub reserved: Vec<String>,
    /// What custom domains point a CNAME at (default: the environment's own hostname).
    pub cname_target: Option<String>,
    pub build_timeout: Duration,
    /// How often a push's build looks for a newer push that replaces it.
    pub supersede_every: Duration,
    pub live: LiveConfig,
    pub prices: Prices,
    /// The biggest `roll up` upload.
    pub upload_max: usize,
    /// The dashboard (roll code's `web_url`): a private preview's 401 page links into it.
    pub web_url: Option<String>,
    /// How long a sign-in-gated environment trusts a visitor's session before it asks again
    /// whether they may still open the project ([`integrations::Visitors`]).
    pub visitor_recheck: Duration,
    /// Where an instance sends its traces (OTLP/HTTP): the apps listener's base (public traffic's,
    /// never the API's: `http://127.0.0.1:7080`, made reachable from instances by the runtime), or
    /// the public apps address on a fleet. None: traces can't be turned on.
    pub otlp_base: Option<String>,
    /// URLs a tenant chooses may be private addresses and plain HTTP: notification targets, and
    /// registries whose names resolve to one (local development and tests only:
    /// `ROLL_APPS_ALLOW_PRIVATE`).
    pub allow_private: bool,
    /// PagerDuty's Events API v2 and Opsgenie's Alert API, per region (`us`, `eu`): fixed, never
    /// a tenant's URL ([`notify`]'s on-call targets). Tests point them at their own servers.
    pub oncall_apis: OncallApis,
    /// Between two passes over spend limits (`spend`).
    pub spend_every: Duration,
    /// A failed cron run's first retry waits this long, each next one twice as long (10 min at most).
    pub cron_retry_after: Duration,
    /// Where ECR's API is asked for a registry login's token, instead of each region's (tests'
    /// fake). The operator's setting, never a tenant's.
    pub ecr_endpoint: Option<String>,
    /// Built deployments each environment keeps to roll back to; older ones' images go
    /// ([`Apps::prune`]).
    pub keep_deployments: usize,
    /// And every one that ended within this (draining, skew protection, a quick rollback).
    pub keep_recent: Duration,
}

impl AppsConfig {
    pub fn new(home: PathBuf) -> AppsConfig {
        AppsConfig {
            home,
            domain: "apps.localhost".into(),
            scheme: "http".into(),
            url_port: Some(7080),
            ports: None,
            tcp_ports: None,
            tcp_host: "127.0.0.1".into(),
            reserved: Vec::new(),
            cname_target: None,
            build_timeout: Duration::from_secs(30 * 60),
            supersede_every: Duration::from_secs(3),
            live: LiveConfig::default(),
            prices: Prices::default(),
            upload_max: 512 << 20,
            web_url: None,
            visitor_recheck: Duration::from_secs(300),
            otlp_base: None,
            allow_private: false,
            oncall_apis: OncallApis::default(),
            spend_every: Duration::from_secs(30),
            cron_retry_after: Duration::from_secs(30),
            ecr_endpoint: None,
            keep_deployments: 10,
            keep_recent: Duration::from_secs(24 * 3600),
        }
    }
}

/// What roll apps runs on.
pub struct Deps {
    pub runtime: Arc<dyn Runtime>,
    pub builder: Arc<dyn Builder>,
    pub sources: Option<Arc<dyn Sources>>,
    pub secrets: Arc<dyn SecretSource>,
    pub resources: Arc<dyn Resources>,
    pub resolver: Arc<dyn Resolver>,
    /// Custom domains' certificates.
    pub tls: Arc<dyn DomainTls>,
    pub objects: Option<pg_storage::Objects>,
    pub cluster: Arc<roll_cluster::Cluster>,
    pub workspace: Option<Arc<roll_workspace::Workspace>>,
}

pub struct Apps {
    pub cfg: AppsConfig,
    pub store: Store,
    pub deps: Deps,
    pub logs: Arc<LogHub>,
    lives: Mutex<HashMap<String, Arc<EnvLive>>>,
    queues: Mutex<HashMap<String, Arc<Notify>>>,
    cancels: Mutex<HashMap<String, CancellationToken>>,
    /// Jobs running on this node: id → its environment (`env_key`). Bounded per environment and
    /// per node, and canceled when the environment goes from this node.
    running_jobs: Mutex<HashMap<String, String>>,
    /// Cron runs this process runs now (a takeover's cleanup spares them).
    /// Cron runs going on this node: run id → (service, environment).
    pub(crate) running_runs: Mutex<HashMap<String, (String, String)>>,
    /// What each public host named lately ([`Apps::env_for_host`]): when, at which
    /// `hosts_gen`, and the answer.
    hosts: Mutex<HashMap<String, HostAnswer>>,
    hosts_gen: std::sync::atomic::AtomicU64,
    /// Job containers this node runs at once, whatever jobs they belong to ([`jobs`]).
    job_slots: Arc<tokio::sync::Semaphore>,
    /// Services whose project reached its spend limit this month (`spend`), as last read.
    capped: Mutex<std::collections::HashSet<String>>,
    /// Held while the capped services are read and applied ([`Apps::refresh_capped`]).
    capped_refresh: tokio::sync::Mutex<()>,
    /// roll previews' notes for a preview's feedback button ([`Apps::set_feedback_notes`]).
    notes: std::sync::OnceLock<Arc<dyn integrations::FeedbackNotes>>,
    /// Where a name under a project's preview domain no environment holds goes (roll previews).
    preview_router: std::sync::OnceLock<Arc<dyn integrations::PreviewRouter>>,
    /// roll previews' branch pushes ([`Apps::set_git_pushes`]).
    pub(crate) pushes: std::sync::OnceLock<Arc<dyn integrations::GitPushes>>,
    /// roll's GitHub App ([`Apps::set_github_app`]).
    github: std::sync::OnceLock<Arc<dyn integrations::GitHubApp>>,
    /// Email to projects' owners (`notify`), once rolld has accounts.
    mail: std::sync::OnceLock<Arc<dyn integrations::Mailbox>>,
    explainer: std::sync::OnceLock<Arc<dyn integrations::Explainer>>,
    /// ECR logins' tokens by service: (the token, when it expires, unix ms).
    ecr_tokens: Mutex<HashMap<String, (String, i64)>>,
    /// Instances' OIDC tokens by `<service>/<env>/<aud>`: (the token, when it expires, unix ms).
    pub(crate) oidc_tokens: Mutex<HashMap<String, (String, i64)>>,
    pushers: push::Pushers,
    /// Who may still open a project's environments ([`Apps::set_visitors`]).
    pub(crate) visitors: std::sync::OnceLock<Arc<dyn integrations::Visitors>>,
    /// Log drains whose batch came back (the drainer sends the next).
    drain_done: Arc<Mutex<Vec<String>>>,
    /// Projects whose drains changed on this node: the drainer reads theirs again (only theirs).
    pub(crate) drains_dirty: Mutex<std::collections::HashSet<String>>,
    /// Previews' runtime lines on their way to the drainer ([`Apps::drain_preview_lines`]), and
    /// its end of the channel until it starts.
    pub(crate) preview_lines: tokio::sync::mpsc::Sender<drains::PreviewBatch>,
    pub(crate) preview_lines_rx: Mutex<Option<tokio::sync::mpsc::Receiver<drains::PreviewBatch>>>,
    /// Status pages' rows and incidents as last read ([`status_page::serve`]).
    pub(crate) status_cache: status_page::StatusCache,
    /// When this node last swept status pages' stale incidents (unix ms).
    status_swept: std::sync::atomic::AtomicI64,
    /// Private environments' token hashes, by environment, as the owner last read or set them.
    access: Mutex<HashMap<String, Option<Access>>>,
    /// Bumped by every change to the access cache: a cold read caches what it read only if
    /// nothing changed meanwhile (a token rotated while it read would otherwise come back).
    access_gen: std::sync::atomic::AtomicU64,
    /// Signed-in visitors' sessions this node has looked up, by their hash.
    visits: Mutex<HashMap<String, visit::Seen>>,
    /// Web Vitals beacons kept this minute, by environment: (minute, count).
    pub(crate) vitals_seen: Mutex<HashMap<String, (i64, u32)>>,
    /// Custom events taken this minute, per environment ([`track`]).
    pub(crate) events_seen: Mutex<HashMap<String, (i64, u32)>>,
    /// Form submissions taken this minute, per environment ([`forms`]).
    pub(crate) forms_seen: Mutex<HashMap<String, (i64, u32)>>,
    /// Uptime checks' last results here, on the owner ([`checks`]), by environment and name.
    pub(crate) check_states: Mutex<HashMap<String, checks::Run>>,
    /// A pass of daily rescans is running, and when live deployments were last looked at for
    /// one ([`vulns`]).
    pub(crate) rescanning: std::sync::atomic::AtomicBool,
    pub(crate) rescan_looked: std::sync::atomic::AtomicI64,
    /// When each environment's log was last looked at for new errors ([`errors`]).
    pub(crate) errors_seen: Mutex<HashMap<String, i64>>,
    /// Manual rollouts waiting for a person, by deployment: what they say, and the wake.
    pub(crate) rollout_waits: Mutex<HashMap<String, deploy::RolloutGate>>,
    /// Path schedules whose call is still out (`sched:<service>/<env>/<name>`): the next due
    /// time is skipped, as a command's is while its job runs.
    pub(crate) calling: Mutex<std::collections::HashSet<String>>,
    /// Each environment's `needs_deploy` as last worked out (it decrypts every variable and asks
    /// each linked product for its own): when, the settings generation and active deployment it
    /// was for, and the answer. Good for a minute, or until either moves.
    needs: Mutex<HashMap<String, NeedsDeploy>>,
    /// Bumped by every change a deployment's fingerprint reads (settings, variables, links).
    settings_gen: std::sync::atomic::AtomicU64,
    /// A service changed (a deploy moved, someone changed something): its id, for the pages
    /// that follow it (`GET …/changes`). On the owner, as every such change is.
    changes: tokio::sync::broadcast::Sender<String>,
    /// The hourly volume backups are running (on their own task).
    backing_up: std::sync::atomic::AtomicBool,
    /// Watched tags are being checked (on their own task).
    watching: std::sync::atomic::AtomicBool,
    watching_bases: std::sync::atomic::AtomicBool,
    watching_secrets: std::sync::atomic::AtomicBool,
    /// Environments whose project secrets are long-polled here ([`Apps::watch_secrets_of`]), by
    /// [`env_key`]: one task each, on the owner.
    pub(crate) secret_watches: Mutex<std::collections::HashSet<String>>,
    /// Scaling workers' backlogs are being read (on their own task).
    scaling: std::sync::atomic::AtomicBool,
    /// The App's queued events are being run on this node.
    github_running: std::sync::atomic::AtomicBool,
    /// Web services' answers a shared cache may keep, per environment (`resp_cache`).
    pub(crate) responses: resp_cache::ResponseCache,
    /// Images made by `/__roll/image` on this node ([`images::ImageCache`]).
    pub(crate) images: images::ImageCache,
    /// Static sites' file indexes, by image digest, as this node read them.
    sites: Mutex<HashMap<String, Arc<sites::Site>>>,
    /// Per environment, for the live site's key: its earlier static deployments' site keys.
    earlier_sites: Mutex<HashMap<String, sites::EarlierSites>>,
    /// Each environment's deployments a request may name ([`skew`]), by its live one.
    pub(crate) skew: Mutex<HashMap<String, skew::Earlier>>,
    /// Long polls of each environment's runtime config held on this node ([`public`]).
    pub(crate) config_waiters: Mutex<HashMap<String, usize>>,
    /// `(service, name)` → the id of the service of that name in its project, as of when.
    service_names: Mutex<HashMap<(String, String), NameLookup>>,
    /// A site being loaded, per key: one load at a time (the rest wait for it).
    site_loads: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Environments in a crash loop whose event is written (one per loop).
    looping: Mutex<std::collections::HashSet<String>>,
    /// For notification targets: public addresses only, no redirects.
    notify_client: reqwest::Client,
    /// A static site's proxy rewrites to other sites (`_redirects`' `https://… 200`): public
    /// addresses only, no redirects followed, 30 s.
    pub(crate) site_proxy_client: reqwest::Client,
    /// Scrapes of instances' own Prometheus endpoints ([`prom`]): loopback, no redirects, no
    /// proxy; and which environments' last scrape failed (said once).
    pub(crate) scrape_client: reqwest::Client,
    pub(crate) scrape_errors: Mutex<std::collections::HashSet<String>>,
    /// The key edge functions' `next` addresses are signed with ([`edge`]), once read; and which
    /// edge functions were said not to run (once each).
    pub(crate) edge_key: Mutex<Option<Vec<u8>>>,
    pub(crate) edge_noted: Mutex<std::collections::HashSet<String>>,
    /// Password tries per environment and client, in the last ten minutes ([`password`]).
    pub(crate) password_tries: Mutex<HashMap<password::Client, (i64, u32)>>,
    /// Shells open over HTTP (the dashboard's terminal), by id.
    sessions: Mutex<HashMap<String, Arc<shell::Session>>>,
    /// WebSocket shells open per environment (`env_key`), counted with the HTTP sessions.
    ws_shells: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicUsize>>>,
    /// Open tunnels per environment ([`tunnel::MAX_TUNNELS`]).
    tunnels: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicUsize>>>,
    pub(crate) client: hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, axum::body::Body>,
    /// HTTP/2 without TLS (prior knowledge), for h2c apps ([`model::Run::h2c`]).
    pub(crate) client_h2: hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, axum::body::Body>,
    stop: CancellationToken,
}

/// Where on-call targets are sent ([`AppsConfig::oncall_apis`]).
#[derive(Clone, Debug)]
pub struct OncallApis {
    pub pagerduty_us: String,
    pub pagerduty_eu: String,
    pub opsgenie_us: String,
    pub opsgenie_eu: String,
}

impl Default for OncallApis {
    fn default() -> Self {
        OncallApis {
            pagerduty_us: "https://events.pagerduty.com/v2/enqueue".into(),
            pagerduty_eu: "https://events.eu.pagerduty.com/v2/enqueue".into(),
            opsgenie_us: "https://api.opsgenie.com/v2/alerts".into(),
            opsgenie_eu: "https://api.eu.opsgenie.com/v2/alerts".into(),
        }
    }
}

pub(crate) fn env_key(service: &str, env: &str) -> String {
    format!("{service}/{env}")
}

/// A hostname label from `s`: lowercase, `[a-z0-9-]`, at most `max`, and never `--`: that's
/// how a deployment's own hostname (`<id>--<slug>`) is told apart, so no environment's slug may
/// look like another's deployment.
fn label(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }) {
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    out.truncate(max);
    out.trim_matches('-').to_string()
}

/// The environment's variables' fingerprint (names and values, with what the settings set): a
/// change means the active deployment no longer matches. Settings count by what differs from
/// their defaults, so a setting roll adds later (at its default) moves no service's fingerprint.
/// `2:` marks this form; a deployment's hash without it is from before ([`Apps::needs_deploy`]).
fn fingerprint(vars: &[(String, String)], svc: &ServiceRow) -> String {
    format!("2:{}", fingerprint_of(vars, &set_only(&svc.build, &Build::default()), &set_only(&svc.run, &Run::default())))
}

/// `value` as JSON without what equals `default` (objects compared key by key, nested too).
fn set_only<T: serde::Serialize>(value: &T, default: &T) -> Value {
    fn strip(v: &Value, d: &Value) -> Option<Value> {
        match (v, d) {
            (Value::Object(vo), Value::Object(dob)) => {
                let kept: serde_json::Map<String, Value> = vo.iter().filter_map(|(k, x)| strip(x, dob.get(k).unwrap_or(&Value::Null)).map(|x| (k.clone(), x))).collect();
                (!kept.is_empty()).then_some(Value::Object(kept))
            }
            _ if v == d => None,
            _ => Some(v.clone()),
        }
    }
    let (v, d) = (serde_json::to_value(value).expect("JSON"), serde_json::to_value(default).expect("JSON"));
    strip(&v, &d).unwrap_or(Value::Object(Default::default()))
}

fn fingerprint_of(vars: &[(String, String)], build: &Value, run: &Value) -> String {
    let mut h = Sha256::new();
    let mut sorted: Vec<&(String, String)> = vars.iter().collect();
    sorted.sort();
    for (k, v) in sorted {
        h.update(k.as_bytes());
        h.update([0]);
        h.update(v.as_bytes());
        h.update([0]);
    }
    h.update(build.to_string().as_bytes());
    h.update(run.to_string().as_bytes());
    hex(&h.finalize()[..12])
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// What makes up an environment's variables, highest precedence last.
/// An environment's `needs_deploy`, worked out: when, at which settings generation, for which
/// active deployment and secrets' digest, and the answer.
type NeedsDeploy = (std::time::Instant, u64, String, Option<String>, bool);

/// A service id looked up by name, as of when (none: no such web service).
type NameLookup = (std::time::Instant, Option<String>);
/// A host's kept answer: when, at which generation, and the service, environment and deployment.
type HostAnswer = (Instant, u64, Option<HostTarget>);

/// What a hostname names: the service, the environment, a deployment's own (`<dep>--<slug>`),
/// and another of the app's ports when it's a custom domain sent there.
pub type HostTarget = (String, String, Option<String>, Option<u16>);

/// Another service's values a reference may read, and which of them are sealed.
type ServiceValues = (HashMap<String, String>, BTreeSet<String>);

/// An environment's snippets joined for the proxy: before `</head>`, before `</body>`.
fn snippets_of(all: &[model::Snippet]) -> Option<(bytes::Bytes, bytes::Bytes)> {
    let join = |at: model::SnippetAt| bytes::Bytes::from(all.iter().filter(|s| s.at == at).map(|s| s.html.as_str()).collect::<String>());
    (!all.is_empty()).then(|| (join(model::SnippetAt::Head), join(model::SnippetAt::BodyEnd)))
}

pub struct ResolvedVars {
    pub vars: Vec<(String, String)>,
    pub sources: HashMap<String, &'static str>,
    /// References that named nothing ([`refs`]), as `VAR: service.NAME`: empty, and the deploy
    /// says so.
    pub unresolved: Vec<String>,
    /// `roll://` references that didn't resolve, as `VAR (roll://…): why`: left out, and a deploy
    /// fails on them ([`ResolvedVars::check_refs`]); a listing just shows them unresolved.
    pub refused: Vec<String>,
    /// Each `roll://` variable's reference: what staleness is judged by (a value's change is the
    /// secrets digest's to say), so a listing and a deploy hash the same.
    pub ref_texts: BTreeMap<String, String>,
    /// Never shown again: the environment's own sealed variables, and references that read one.
    pub sealed: BTreeSet<String>,
    /// Those roll secrets gave (a secret is one whatever its name or tag), and of them those it
    /// says are plain (`SecretVar::plain`: a linked resource's host, port, user or database),
    /// which logs may show but for a URL's password.
    pub from_secrets: BTreeSet<String>,
    pub plain_secrets: BTreeSet<String>,
}

impl ResolvedVars {
    /// A deploy's check: every `roll://` reference resolved.
    pub fn check_refs(&self) -> Result<()> {
        match self.refused.first() {
            Some(r) => Err(Error::Conflict(format!("{r} didn't resolve{}", if self.refused.len() > 1 { format!(" (and {} more)", self.refused.len() - 1) } else { String::new() }))),
            None => Ok(()),
        }
    }
}

impl ResolvedVars {
    /// What its logs never show ([`logs::Redact`]): sealed and sensitive values, a link's
    /// credentials (by their names), and the password in any URL that carries one.
    pub fn secret_values(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (k, v) in &self.vars {
            if self.is_secret(k) {
                out.push(v.clone());
            }
            // A URL's password, as it's written in the URL (percent-encoded) and as it reads.
            if let Some(p) = url::Url::parse(v).ok().and_then(|u| u.password().map(str::to_string)) {
                let decoded = percent_encoding::percent_decode_str(&p).decode_utf8_lossy().into_owned();
                if decoded != p {
                    out.push(decoded);
                }
                out.push(p);
            }
        }
        out
    }

    /// Whether a variable is a secret by what it is: sealed, a roll secrets value that isn't
    /// plain, a link's credential by name, or a URL that carries a password.
    pub fn is_secret(&self, k: &str) -> bool {
        let credential = ["PASSWORD", "SECRET", "TOKEN", "API_KEY", "PRIVATE_KEY", "ACCESS_KEY"].iter().any(|w| k.to_ascii_uppercase().contains(w));
        let source = self.sources.get(k).copied();
        // What roll secrets gave, unless the service's own variable took the name since.
        let secret = source == Some("secret") && self.from_secrets.contains(k);
        self.sealed.contains(k) || (secret && !self.plain_secrets.contains(k)) || (credential && source == Some("link"))
    }

    /// The variables a closed environment's build may have: none that is a secret
    /// ([`Self::is_secret`]), none that carries a URL password, and none that holds a secret
    /// value of eight characters or more (a reference that copied one).
    pub fn without_secrets(&self) -> Vec<(String, String)> {
        let long: Vec<String> = self.vars.iter().filter(|(k, _)| self.is_secret(k)).map(|(_, v)| v.clone()).filter(|v| v.len() >= 8).collect();
        self.vars
            .iter()
            .filter(|(k, v)| !self.is_secret(k) && url::Url::parse(v).ok().and_then(|u| u.password().map(|_| ())).is_none() && !long.iter().any(|s| v.contains(s.as_str())))
            .cloned()
            .collect()
    }
}

impl Apps {
    pub fn new(cfg: AppsConfig, store: Store, deps: Deps) -> Arc<Apps> {
        let cfg_allow = cfg.allow_private;
        let logs = LogHub::new(deps.objects.clone());
        let (preview_lines, preview_lines_rx) = tokio::sync::mpsc::channel(drains::PREVIEW_BATCHES);
        Arc::new(Apps {
            cfg,
            store,
            deps,
            logs,
            lives: Mutex::new(HashMap::new()),
            queues: Mutex::new(HashMap::new()),
            cancels: Mutex::new(HashMap::new()),
            running_jobs: Mutex::new(HashMap::new()),
            running_runs: Mutex::default(),
            hosts: Mutex::default(),
            hosts_gen: Default::default(),
            job_slots: Arc::new(tokio::sync::Semaphore::new(jobs::MAX_CONTAINERS)),
            capped: Mutex::new(Default::default()),
            capped_refresh: tokio::sync::Mutex::new(()),
            notes: std::sync::OnceLock::new(),
            preview_router: std::sync::OnceLock::new(),
            pushes: std::sync::OnceLock::new(),
            github: std::sync::OnceLock::new(),
            mail: std::sync::OnceLock::new(),
            explainer: std::sync::OnceLock::new(),
            ecr_tokens: Mutex::new(HashMap::new()),
            oidc_tokens: Mutex::new(HashMap::new()),
            pushers: push::Pushers::default(),
            visitors: std::sync::OnceLock::new(),
            drain_done: Arc::new(Mutex::new(Vec::new())),
            drains_dirty: Mutex::new(Default::default()),
            preview_lines,
            preview_lines_rx: Mutex::new(Some(preview_lines_rx)),
            status_cache: Default::default(),
            status_swept: std::sync::atomic::AtomicI64::new(0),
            access: Mutex::new(HashMap::new()),
            access_gen: std::sync::atomic::AtomicU64::new(0),
            visits: Mutex::new(HashMap::new()),
            vitals_seen: Mutex::new(HashMap::new()),
            events_seen: Mutex::new(HashMap::new()),
            forms_seen: Mutex::new(HashMap::new()),
            check_states: Mutex::new(HashMap::new()),
            rescanning: Default::default(),
            rescan_looked: Default::default(),
            errors_seen: Mutex::new(HashMap::new()),
            rollout_waits: Mutex::new(HashMap::new()),
            calling: Mutex::new(std::collections::HashSet::new()),
            needs: Mutex::new(HashMap::new()),
            settings_gen: std::sync::atomic::AtomicU64::new(0),
            changes: tokio::sync::broadcast::channel(256).0,
            backing_up: std::sync::atomic::AtomicBool::new(false),
            watching: std::sync::atomic::AtomicBool::new(false),
            watching_bases: std::sync::atomic::AtomicBool::new(false),
            watching_secrets: std::sync::atomic::AtomicBool::new(false),
            secret_watches: Mutex::new(std::collections::HashSet::new()),
            scaling: std::sync::atomic::AtomicBool::new(false),
            github_running: std::sync::atomic::AtomicBool::new(false),
            responses: resp_cache::ResponseCache::default(),
            images: images::ImageCache::default(),
            sites: Mutex::new(HashMap::new()),
            earlier_sites: Mutex::new(HashMap::new()),
            skew: Mutex::new(HashMap::new()),
            config_waiters: Mutex::new(HashMap::new()),
            site_loads: Mutex::new(HashMap::new()),
            service_names: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            ws_shells: Mutex::new(HashMap::new()),
            tunnels: Mutex::new(HashMap::new()),
            notify_client: net::client(cfg_allow, Duration::from_secs(10)),
            site_proxy_client: net::client(cfg_allow, Duration::from_secs(30)),
            scrape_client: reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).no_proxy().timeout(Duration::from_secs(10)).build().expect("a client"),
            scrape_errors: Mutex::new(Default::default()),
            edge_key: Mutex::new(None),
            edge_noted: Mutex::new(Default::default()),
            password_tries: Mutex::new(HashMap::new()),
            looping: Mutex::new(std::collections::HashSet::new()),
            client: hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build_http(),
            client_h2: hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).http2_only(true).build_http(),
            stop: CancellationToken::new(),
        })
    }

    /// Run a store call off the async runtime (it blocks on its database).
    pub async fn db<T: Send + 'static>(self: &Arc<Self>, f: impl FnOnce(&Store) -> Result<T> + Send + 'static) -> Result<T> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || f(&this.store)).await.map_err(|e| Error::Internal(e.to_string()))?
    }

    pub fn owns(&self, service: &str) -> bool {
        self.deps.cluster.owns(&lease_key(service))
    }

    // ── services ──

    fn project_name(&self, project: &str) -> Result<String> {
        match &self.deps.workspace {
            Some(ws) => match ws.get(roll_workspace::Scope::All, project) {
                Ok(p) => Ok(p.name),
                Err(roll_workspace::Error::NotFound(_)) => Err(Error::BadRequest(format!("no project {project}"))),
                Err(e) => Err(Error::Internal(e.to_string())),
            },
            None => Ok(project.strip_prefix("prj_").unwrap_or(project).to_string()),
        }
    }

    /// `actor`: who asked (the first deployment's).
    /// A source's shape, and a Git URL's address against this node's private-address rule.
    fn check_source(&self, source: &Source) -> Result<()> {
        source.check()?;
        if let Source::Git { url, .. } = source {
            git_source::check_url(url, self.cfg.allow_private).map_err(Error::BadRequest)?;
        }
        Ok(())
    }

    pub async fn create_service(self: &Arc<Self>, body: Value, actor: Option<String>) -> Result<Service> {
        let o = body.as_object().ok_or_else(|| Error::BadRequest("the body is an object".into()))?;
        for k in o.keys() {
            if !["project", "name", "source", "build", "run", "variables", "sealed", "registry", "git_login", "links", "branch", "x", "y"].contains(&k.as_str()) {
                return Err(Error::BadRequest(format!("unknown field {k}")));
            }
        }
        let project = o.get("project").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("project is a workspace project id".into()))?.to_string();
        let name = o.get("name").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("name is required".into()))?.to_string();
        check_name("a service name", &name)?;
        let source: Source = serde_json::from_value(o.get("source").cloned().ok_or_else(|| Error::BadRequest("source is required".into()))?).map_err(|e| Error::BadRequest(format!("source: {e}")))?;
        self.check_source(&source)?;
        let build: Build = merge(&Build::default(), o.get("build"), "build")?;
        build.check()?;
        let run = Run::default().merged(o.get("run"))?;
        // Production's variables, set before its first deploy (an app that needs DATABASE_URL
        // shouldn't crash-loop first): checked now, before anything is made.
        let variables = match o.get("variables") {
            None | Some(Value::Null) => None,
            Some(Value::Object(m)) => {
                for (k, v) in m {
                    check_var_name(k)?;
                    if !v.is_string() {
                        return Err(Error::BadRequest(format!("variables.{k} is a string")));
                    }
                }
                Some(Value::Object(m.clone()))
            }
            Some(_) => return Err(Error::BadRequest("variables is an object of names to values".into())),
        };
        // Which of them are sealed from the start (a template's generated secrets).
        let sealed: Vec<Value> = match o.get("sealed") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) if a.iter().all(|n| n.as_str().is_some_and(|n| variables.as_ref().is_some_and(|v| v.get(n).is_some()))) => a.clone(),
            Some(_) => return Err(Error::BadRequest("sealed is an array of names in variables".into())),
        };
        // The project's databases (and other resources) to link before the first deploy:
        // `[{kind, ref, prefix?}]`, `PUT …/links/{kind}/{ref}`'s.
        let links: Vec<(String, String, Value)> = match o.get("links") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) if a.len() <= 20 => a
                .iter()
                .map(|l| {
                    let kind = l["kind"].as_str().ok_or_else(|| Error::BadRequest("links[].kind is required".into()))?;
                    let r = l["ref"].as_str().ok_or_else(|| Error::BadRequest("links[].ref is required".into()))?;
                    Ok((kind.to_string(), r.to_string(), json!({ "prefix": l.get("prefix").cloned().unwrap_or(Value::Null) })))
                })
                .collect::<Result<_>>()?,
            Some(_) => return Err(Error::BadRequest("links is a list of at most 20 {kind, ref, prefix?}".into())),
        };
        // A private image's login, before its first pull (`PUT …/registry`'s body).
        let registry = match o.get("registry") {
            None | Some(Value::Null) => None,
            Some(r @ Value::Object(_)) if matches!(source, Source::Image { .. }) => Some(r.clone()),
            Some(Value::Object(_)) => return Err(Error::BadRequest("registry is for an image source".into())),
            Some(_) => return Err(Error::BadRequest("registry is {username, password, server?}".into())),
        };
        // A private repository's login, for the branch check below and every fetch after
        // (`PUT …/git-login`'s body).
        let git_login = match o.get("git_login") {
            None | Some(Value::Null) => None,
            Some(g @ Value::Object(m)) if matches!(source, Source::Git { .. }) => {
                // Checked as `PUT …/git-login` checks it, before anything is sent with it.
                let ok = |k: &str, max: usize| m.get(k).and_then(Value::as_str).filter(|v| !v.is_empty() && v.len() <= max && !v.contains(['\0', '\n', '\r']));
                match (ok("username", 200), ok("password", 4096)) {
                    (Some(u), Some(p)) if m.len() == 2 => Some((g.clone(), (u.to_string(), p.to_string()))),
                    _ => return Err(Error::BadRequest("git_login is {username, password}, each on one line (1–200 and 1–4096 characters)".into())),
                }
            }
            Some(Value::Object(_)) => return Err(Error::BadRequest("git_login is for a Git URL source".into())),
            Some(_) => return Err(Error::BadRequest("git_login is {username, password}".into())),
        };
        let (x, y) = (o.get("x").and_then(Value::as_f64), o.get("y").and_then(Value::as_f64));
        // A paused project makes nothing new (its first deploy would be refused anyway).
        let (p, month) = (project.clone(), crate::spend::month_of(code_sql::now_ms()).0);
        if self.db(move |s| s.project_capped(&p, month)).await? {
            return Err(Error::Conflict("this project reached its spending limit for the month: raise the limit to add services".into()));
        }
        let this = self.clone();
        let pname = {
            let p = project.clone();
            tokio::task::spawn_blocking(move || this.project_name(&p)).await.map_err(|e| Error::Internal(e.to_string()))??
        };
        // A repo's production environment follows its default branch, or the one named (its
        // first deploy is that branch's head, not the default's).
        let named = match o.get("branch") {
            None | Some(Value::Null) => None,
            Some(Value::String(b)) if matches!(source, Source::Repo { .. } | Source::Git { .. }) && !b.is_empty() && b.len() <= 255 => Some(b.clone()),
            Some(_) => return Err(Error::BadRequest("branch is a repo or Git source's branch name".into())),
        };
        let mut await_connection = false;
        let branch = match (&source, &self.deps.sources) {
            (Source::Repo { repo, .. }, Some(src)) => {
                let _ = src.resolve(repo, named.as_deref()).await.map_err(|e| Error::BadRequest(format!("repo {repo}: {e}")))?;
                Some(match named {
                    Some(b) => b,
                    None => self.default_branch(repo).await,
                })
            }
            (Source::Repo { repo, .. }, None) => return Err(Error::BadRequest(format!("roll code isn't on this node, so {repo} can't be a source"))),
            // A Git URL's branch must exist now (its default when none is named), as a repo's must.
            (Source::Git { url, .. }, _) => {
                let remote = git_source::Remote::new(self.cfg.allow_private, url, git_login.as_ref().map(|(_, l)| l.clone()));
                match git_source::head(&remote, named.as_deref()).await {
                    Ok((_, b)) => Some(b),
                    // A private repository on roll's GitHub App's host: made without its first
                    // deploy, to be connected to the App (the operator's) or given a login. The
                    // App isn't asked here: whether it sees a repository is no tenant's to learn.
                    Err(_) if git_login.is_none() && self.github.get().and_then(|a| a.host()).is_some_and(|h| git_host(url).as_ref() == Some(&h)) => {
                        await_connection = true;
                        Some(named.clone().unwrap_or_else(|| "main".into()))
                    }
                    Err(e) => return Err(Error::BadRequest(format!("{url}: {e}"))),
                }
            }
            _ => None,
        };
        let row = ServiceRow { id: new_id("svc"), project: project.clone(), name: name.clone(), source: source.clone(), build, run, created_at: self.store.now()? };
        let r = row.clone();
        self.db(move |s| s.create_service(&r)).await?;
        if let Some(ws) = self.deps.workspace.clone() {
            let (id, project) = (row.id.clone(), project.clone());
            let spot = (x.unwrap_or(0.0), y.unwrap_or(0.0));
            let placed = tokio::task::spawn_blocking(move || {
                let (x, y) = if spot == (0.0, 0.0) { free_spot(&ws, &project) } else { spot };
                ws.place(roll_workspace::Scope::All, &project, roll_workspace::Kind::Service, &id, x, y)
            })
            .await;
            if let Ok(Err(e)) = placed {
                tracing::warn!(service = %row.id, error = %e, "apps: couldn't place the service on its project's canvas");
            }
        }
        self.ensure_environment(&row, PRODUCTION, Some(branch.as_deref()), Some(false), &pname).await?;
        if let Some(set) = variables {
            self.change_variables(&row.id, PRODUCTION, json!({ "set": set, "seal": sealed })).await?;
        }
        for (kind, r, body) in links {
            // A link that doesn't check out makes no service: nothing to clean up by hand.
            if let Err(e) = self.put_link(&row.id, &kind, &r, body).await {
                let _ = self.purge_service(&row.id).await;
                return Err(e);
            }
        }
        if let Some((g, _)) = git_login
            && let Err(e) = self.put_git_login(&row.id, g).await
        {
            let _ = self.purge_service(&row.id).await;
            return Err(e);
        }
        if let Some(r) = registry
            && let Err(e) = self.put_registry(&row.id, r).await
        {
            // A login that doesn't check out makes no service: nothing to clean up by hand.
            let _ = self.purge_service(&row.id).await;
            return Err(e);
        }
        // The project's owner hears what needs someone from its first service on.
        self.offer_email(&row.project).await;
        // A repo, a Git URL or an image deploys at once; an upload waits for `roll up`, a private
        // repository on the GitHub App's host for its connection or login.
        if !matches!(source, Source::Upload) && !await_connection {
            self.deploy(&row.id, PRODUCTION, json!({}), By { trigger: Trigger::Api, actor: actor.clone() }, None, None).await?;
        }
        self.service(&row.id).await
    }

    async fn default_branch(&self, repo: &str) -> String {
        match &self.deps.sources {
            Some(s) => s.default_branch(repo).await.unwrap_or_else(|_| "main".into()),
            None => "main".into(),
        }
    }

    async fn ensure_environment(self: &Arc<Self>, svc: &ServiceRow, env: &str, branch: Option<Option<&str>>, ephemeral: Option<bool>, project_name: &str) -> Result<EnvRow> {
        check_name("an environment name", env)?;
        // `<service>-<project>[-<env>]`, the project left out when it has the service's name
        // (`roll up` names both after the directory: `web`, not `web-web`).
        let who = if label(project_name, 56) == svc.name { svc.name.clone() } else { format!("{}-{}", svc.name, project_name) };
        let base = if env == PRODUCTION { who } else { format!("{who}-{env}") };
        let base = label(&base, 56);
        let mut slugs = vec![base.clone()];
        for _ in 0..4 {
            slugs.push(format!("{base}-{}", random(4)));
        }
        let (id, env_s, branch) = (svc.id.clone(), env.to_string(), branch.map(|b| b.map(str::to_string)));
        let ports = self.cfg.ports;
        let (sid, e) = (svc.id.clone(), env.to_string());
        let new = self.db(move |s| Ok(s.environment(&sid, &e).is_err())).await?;
        let row = self.db(move |s| s.put_environment(&id, &env_s, branch.as_ref().map(|b| b.as_deref()), ephemeral, &slugs, ports)).await?;
        // A new environment's secrets have somewhere to go (best effort: none is no secrets).
        if new && !row.ephemeral
            && let Err(err) = self.deps.secrets.ensure(&svc.project, env).await
        {
            tracing::warn!(service = %svc.id, env, error = %err, "apps: roll secrets has no environment for it yet");
        }
        Ok(row)
    }

    pub async fn services(self: &Arc<Self>, project: Option<String>) -> Result<Vec<Service>> {
        let rows = self.db(move |s| s.services(project.as_deref())).await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(self.view(r).await?);
        }
        Ok(out)
    }

    pub async fn service(self: &Arc<Self>, id: &str) -> Result<Service> {
        let id = id.to_string();
        let row = self.db(move |s| s.service(&id)).await?;
        self.view(row).await
    }

    async fn view(self: &Arc<Self>, r: ServiceRow) -> Result<Service> {
        let id = r.id.clone();
        let envs = self.db(move |s| s.environments(&id)).await?;
        let mut summaries = Vec::new();
        for e in &envs {
            summaries.push(self.summary(e).await?);
        }
        let id = r.id.clone();
        let (registry, git_login, github, links) = self.db(move |s| Ok((s.registry(&id)?, s.git_login(&id)?, s.git_github(&id)?, s.links(&id)?))).await?;
        let links = links.into_iter().map(|(kind, r#ref, _)| LinkRef { kind, r#ref }).collect();
        Ok(Service { id: r.id, project: r.project, name: r.name, source: r.source, build: r.build, run: r.run, registry, git_login, github, links, created_at: r.created_at, environments: summaries })
    }

    /// `PUT …/registry`: the login a service's image pulls with. `server` defaults to the image's
    /// registry (Docker Hub when it names none).
    pub async fn put_registry(self: &Arc<Self>, service: &str, body: Value) -> Result<Service> {
        let o = body.as_object().cloned().unwrap_or_default();
        let ecr = o.get("kind").and_then(Value::as_str) == Some("ecr");
        let fields: &[&str] = if ecr { &["kind", "access_key_id", "secret_access_key", "region"] } else { &["kind", "server", "username", "password"] };
        if let Some(k) = o.keys().find(|k| !fields.contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        if o.get("kind").is_some_and(|k| !matches!(k.as_str(), Some("basic" | "ecr"))) {
            return Err(Error::BadRequest("kind is basic (a username and password) or ecr (an AWS access key)".into()));
        }
        let text = |k: &str, max: usize| -> Result<Option<String>> {
            match o.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(v)) if !v.is_empty() && v.len() <= max && !v.contains('\0') => Ok(Some(v.clone())),
                _ => Err(Error::BadRequest(format!("{k} is 1–{max} characters"))),
            }
        };
        let sid = service.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let Source::Image { image } = &svc.source else { return Err(Error::Conflict("only an image source pulls from a registry".into())) };
        if ecr {
            // An access key, exchanged for ECR's 12-hour token at each pull that needs a fresh
            // one: a login that never expires on roll's side (Render's ECR credentials).
            let host = registry_host(image).unwrap_or("");
            let Some(from_host) = crate::model::ecr_region(host) else { return Err(Error::BadRequest(format!("{image} isn't in an ECR registry (<account>.dkr.ecr.<region>.amazonaws.com)"))) };
            let key_ok = |v: &str| (16..=128).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
            let (Some(key), Some(secret)) = (text("access_key_id", 128)?.filter(|k| key_ok(k)), text("secret_access_key", 128)?) else {
                return Err(Error::BadRequest("an ecr login is {kind: \"ecr\", access_key_id, secret_access_key, region?}".into()));
            };
            let region = text("region", 32)?.unwrap_or_else(|| from_host.to_string());
            if region != from_host {
                return Err(Error::BadRequest(format!("region {region} isn't {from_host}, the registry's")));
            }
            let (sid, server) = (service.to_string(), host.to_string());
            self.db(move |s| s.set_registry(&sid, &server, &key, &secret, Some(&region))).await?;
            self.ecr_tokens.lock().remove(service);
            let login = self.pull_login(service, image).await?;
            self.registry_changed(service, login.as_ref()).await;
            return self.service(service).await;
        }
        let (username, password) = (text("username", 200)?, text("password", 4096)?);
        let (Some(username), Some(password)) = (username, password) else { return Err(Error::BadRequest("username and password are required".into())) };
        // A login goes only to the registry the image is pulled from (deploys check it again).
        let server = match text("server", 255)? {
            Some(s) if registry_key(&s) == image_registry(image) => s,
            Some(s) => return Err(Error::BadRequest(format!("server {s} isn't {}, the registry source.image is pulled from", image_registry(image)))),
            None => registry_host(image).map(str::to_string).unwrap_or_else(|| "https://index.docker.io/v1/".into()),
        };
        let sid = service.to_string();
        let login = crate::runtime::RegistryLogin { server: server.clone(), username: username.clone(), password: password.clone() };
        self.db(move |s| s.set_registry(&sid, &server, &username, &password, None)).await?;
        self.ecr_tokens.lock().remove(service);
        // Wakes and new instances pull with it from now on, not at the next deploy.
        self.registry_changed(service, Some(&login)).await;
        self.service(service).await
    }

    /// `PUT …/git-login`: the login a Git URL service's private repository is fetched with (a
    /// deploy token, app password or access token as `password`), sent only to the source URL's
    /// host. The URL itself stays without credentials.
    pub async fn put_git_login(self: &Arc<Self>, service: &str, body: Value) -> Result<Service> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["username", "password", "secret"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let text = |k: &str, max: usize| -> Result<String> {
            match o.get(k) {
                Some(Value::String(v)) if !v.is_empty() && v.len() <= max && !v.contains(['\0', '\n', '\r']) => Ok(v.clone()),
                _ => Err(Error::BadRequest(format!("{k} is 1–{max} characters on one line"))),
            }
        };
        let username = text("username", 200)?;
        // The password itself (sealed here), or the name of the project's secret that holds it,
        // read at each fetch (a rotated token just works; roll secrets audits the read).
        let (password, secret) = match (o.contains_key("password"), o.contains_key("secret")) {
            (true, false) => (text("password", 4096)?, None),
            (false, true) => {
                let name = text("secret", 128)?;
                check_var_name(&name)?;
                (String::new(), Some(name))
            }
            _ => return Err(Error::BadRequest("a Git login takes a password, or the secret that holds it (not both)".into())),
        };
        let sid = service.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let Source::Git { url, .. } = &svc.source else { return Err(Error::Conflict("only a Git URL source fetches with a login".into())) };
        let host = git_host(url).ok_or_else(|| Error::BadRequest("the source URL has no host".into()))?;
        let sid = service.to_string();
        self.db(move |s| s.set_git_login(&sid, &host, &username, &password, secret.as_deref())).await?;
        self.service(service).await
    }

    pub async fn delete_git_login(self: &Arc<Self>, service: &str) -> Result<Service> {
        let sid = service.to_string();
        self.db(move |s| s.delete_git_login(&sid)).await?;
        self.service(service).await
    }

    /// `PUT …/github`: connect a Git URL service on the App's host to roll's GitHub App, through
    /// the installation that sees its repository (admin: installations span tenants). Fetches
    /// then use the installation's short-lived tokens, and deploys post commit statuses.
    pub async fn put_github(self: &Arc<Self>, service: &str) -> Result<Service> {
        let app = self.github.get().cloned().ok_or_else(|| Error::Conflict("roll's GitHub App isn't set up on this node".into()))?;
        let sid = service.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let Source::Git { url, .. } = &svc.source else { return Err(Error::Conflict("only a Git URL source connects to GitHub".into())) };
        let host = app.host().ok_or_else(|| Error::Conflict("roll's GitHub App isn't set up on this node".into()))?;
        let (owner, repo) = github_repo(url, &host).ok_or_else(|| Error::BadRequest(format!("{url} isn't a repository on {host}")))?;
        let installation = app
            .installation_for(&owner, &repo)
            .await
            .map_err(|e| Error::Internal(format!("asking GitHub: {e:#}")))?
            .ok_or_else(|| Error::Conflict(format!("roll's GitHub App isn't installed on {owner}/{repo}: install it there first")))?;
        let (sid, full) = (service.to_string(), format!("{owner}/{repo}"));
        self.db(move |s| s.set_git_github(&sid, installation, &full)).await?;
        self.service(service).await
    }

    pub async fn delete_github(self: &Arc<Self>, service: &str) -> Result<Service> {
        let sid = service.to_string();
        self.db(move |s| s.delete_git_github(&sid)).await?;
        self.service(service).await
    }

    /// One of roll's GitHub App's verified `push` or `pull_request` deliveries (roll code passes
    /// them on, signature checked): queued for each Git URL service connected to its repository
    /// through its installation, run on the service's owner as the service's own hook's would be
    /// (so a connected service needs no hook). Only what deciding reads is kept.
    pub fn github_event(&self, event: &str, body: &[u8]) {
        // In the webhook's own call, before GitHub hears back: a crash after the answer loses
        // nothing (the store is the queue).
        let Ok(p) = serde_json::from_slice::<Value>(body) else { return };
        let (Some(repo), Some(installation)) = (p["repository"]["full_name"].as_str(), p["installation"]["id"].as_u64()) else { return };
        let Ok(services) = self.store.connected_to(installation, repo) else { return };
        if services.is_empty() {
            return;
        }
        let pr = &p["pull_request"];
        let kept = match event {
            "push" => json!({ "ref": p["ref"], "after": p["after"], "head_commit": { "message": p["head_commit"]["message"] } }),
            "pull_request" => json!({ "action": p["action"], "number": p["number"], "pull_request": {
                "number": pr["number"], "title": pr["title"],
                "head": { "ref": pr["head"]["ref"], "sha": pr["head"]["sha"], "repo": { "id": pr["head"]["repo"]["id"] } },
                "base": { "repo": { "id": pr["base"]["repo"]["id"] } },
            } }),
            _ => return,
        }
        .to_string();
        for svc in services {
            if let Err(err) = self.store.queue_github_event(&svc, event, &kept) {
                tracing::warn!(error = %err, "apps: a GitHub App event couldn't be queued");
            }
        }
    }

    /// Run the queued App events of the services this node owns (each claimed before it runs).
    pub(crate) async fn run_github_events(self: &Arc<Self>) {
        // An hour unrun (a service that's gone, an owner that never came): dropped.
        let _ = self.db(|s| s.expire_github_events(code_sql::now_ms() - 3_600_000)).await;
        let _ = self.db(|s| s.expire_visits(code_sql::now_ms())).await;
        // Each service this node owns, its own oldest first: one nobody owns yet (asleep; the
        // keeper gives it an owner) never holds up the rest.
        let Ok(waiting) = self.db(|s| s.github_event_services()).await else { return };
        let mut queued = Vec::new();
        for service in waiting.into_iter().filter(|s| self.owns(s)) {
            if let Ok(rows) = self.db(move |s| s.github_events_of(&service)).await {
                queued.extend(rows);
            }
        }
        for (id, service, event, body) in queued {
            let claimed = id.clone();
            if !self.db(move |s| s.take_github_event(&claimed)).await.unwrap_or(false) {
                continue;
            }
            let sid = service.clone();
            let Ok(svc) = self.db(move |s| s.service(&sid)).await else { continue };
            let Ok(body) = serde_json::from_str::<Value>(&body) else { continue };
            let w = hooks::Webhook { body, event: Some(event) };
            match self.git_event(&service, &svc, &w, None, "github app").await {
                Ok(hooks::HookOutcome::Ignored(why)) => self.logs.sink(format!("env:{service}/production"), crate::logs::Stream::System).line(format!("GitHub: {why}")),
                Ok(_) => {}
                Err(e) => self.logs.sink(format!("env:{service}/production"), crate::logs::Stream::System).line(format!("GitHub: {e}")),
            }
        }
    }

    /// `POST …/cache/purge {paths?, tags?}`: the environment's kept answers for those paths (a
    /// trailing `*` is a prefix; at most 100) and with those tags (at most 100), or all of them.
    /// Runs on the owner, which keeps them.
    pub async fn purge_cache(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Value> {
        self.service_env(service, env).await?;
        let paths = match body.get("paths") {
            None | Some(Value::Null) => None,
            Some(Value::Array(a)) if !a.is_empty() && a.len() <= 100 => Some(
                a.iter()
                    .map(|p| p.as_str().filter(|p| p.starts_with('/') && p.len() <= 2048).map(str::to_string).ok_or_else(|| Error::BadRequest("paths are absolute paths (`/blog/*` for a prefix)".into())))
                    .collect::<Result<Vec<_>>>()?,
            ),
            Some(_) => return Err(Error::BadRequest("paths is 1–100 paths, or left out for everything".into())),
        };
        let tags = match body.get("tags") {
            None | Some(Value::Null) => None,
            Some(Value::Array(a)) if !a.is_empty() && a.len() <= 100 => Some(
                a.iter()
                    .map(|t| {
                        t.as_str()
                            .map(|t| t.trim().to_ascii_lowercase())
                            .filter(|t| !t.is_empty() && t.len() <= crate::resp_cache::MAX_TAG && !t.contains(',') && !t.chars().any(char::is_whitespace))
                            .ok_or_else(|| Error::BadRequest("tags are what the app's Cache-Tag or Surrogate-Key named (no commas or spaces, at most 1,024 bytes)".into()))
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
            Some(_) => return Err(Error::BadRequest("tags is 1–100 tags, or left out".into())),
        };
        let n = self.responses.purge(&env_key(service, env), paths.as_deref(), tags.as_deref());
        Ok(json!({ "purged": n }))
    }

    /// Where roll's GitHub App is, once rolld has it. Set once.
    pub fn set_github_app(&self, app: Arc<dyn integrations::GitHubApp>) {
        let _ = self.github.set(app);
    }

    /// A Git URL service's GitHub connection still for this URL's repository, with the App.
    pub(crate) async fn github_for(self: &Arc<Self>, service: &str, url: &str) -> Option<(Arc<dyn integrations::GitHubApp>, u64, String, String)> {
        let app = self.github.get().cloned()?;
        let sid = service.to_string();
        let link = self.db(move |s| s.git_github(&sid)).await.ok().flatten()?;
        let (owner, repo) = github_repo(url, &app.host()?)?;
        link.repo.eq_ignore_ascii_case(&format!("{owner}/{repo}")).then_some((app, link.installation, owner, repo))
    }

    /// The repository a Git URL service fetches from: through roll's GitHub App when it's
    /// connected (a short-lived installation token), else with its login when the login is for
    /// this URL's host (a login never goes to another host).
    pub(crate) async fn git_remote<'a>(self: &Arc<Self>, service: &str, url: &'a str) -> git_source::Remote<'a> {
        if let Some((app, installation, owner, repo)) = self.github_for(service, url).await {
            match app.git_login(installation, &owner, &repo).await {
                Ok(login) => return git_source::Remote::new(self.cfg.allow_private, url, Some(login)),
                Err(e) => tracing::warn!(service, error = %e, "apps: roll's GitHub App gave no token; fetching without it"),
            }
        }
        let sid = service.to_string();
        let login = self.db(move |s| s.git_login_secret(&sid)).await.ok().flatten().filter(|(l, _)| Some(&l.host) == git_host(url).as_ref());
        let login = match login {
            // Read from the project's production secrets (what the service runs with), each use.
            Some((l, _)) if l.secret.is_some() => {
                let key = l.secret.clone().expect("matched");
                let sid = service.to_string();
                let project = self.db(move |s| s.service(&sid)).await.ok().map(|s| s.project);
                let read = match project {
                    Some(p) => self.deps.secrets.resolve(&p, crate::model::PRODUCTION, service).await.ok(),
                    None => None,
                };
                match read.and_then(|vars| vars.into_iter().find(|v| v.name == key)) {
                    Some(v) => Some((l.username, v.value)),
                    None => {
                        tracing::warn!(service, secret = %key, "apps: a Git login's secret couldn't be read; fetching without it");
                        None
                    }
                }
            }
            Some((l, password)) => Some((l.username, password)),
            None => None,
        };
        git_source::Remote::new(self.cfg.allow_private, url, login)
    }

    pub async fn delete_registry(self: &Arc<Self>, service: &str) -> Result<Service> {
        let sid = service.to_string();
        self.db(move |s| s.delete_registry(&sid)).await?;
        self.registry_changed(service, None).await;
        self.service(service).await
    }

    /// A service's registry login changed (None: removed, or its registry changed): wherever
    /// the runtime keeps a copy, and in what its environments make instances of.
    async fn registry_changed(&self, service: &str, login: Option<&crate::runtime::RegistryLogin>) {
        for l in self.lives().into_iter().filter(|l| l.service == service) {
            l.set_registry(login);
        }
        self.deps.runtime.registry_login_changed(service, login).await;
    }

    pub fn url(&self, slug: &str) -> String {
        let port = self.cfg.url_port.filter(|p| !((self.cfg.scheme == "http" && *p == 80) || (self.cfg.scheme == "https" && *p == 443)));
        match port {
            Some(p) => format!("{}://{slug}.{}:{p}", self.cfg.scheme, self.cfg.domain),
            None => format!("{}://{slug}.{}", self.cfg.scheme, self.cfg.domain),
        }
    }

    async fn summary(self: &Arc<Self>, e: &EnvRow) -> Result<EnvironmentSummary> {
        let sid = e.service.clone();
        let svc_row = self.db(move |s| s.service(&sid)).await.ok();
        let public = svc_row.as_ref().map(|s| s.run.public).unwrap_or(true);
        let (svc, env) = (e.service.clone(), e.name.clone());
        let recent = self.db(move |s| s.deployments(&svc, &env, 1, None)).await?;
        let active = match &e.active {
            Some(a) => {
                let a = a.clone();
                let d = self.db(move |s| s.deployment(&a)).await.ok();
                d.map(|d| DeployRef { id: d.id, revision: d.revision, created_at: d.created_at })
            }
            None => None,
        };
        let latest = recent.first().map(|d| LatestRef { id: d.id.clone(), status: d.status });
        let state = if latest.as_ref().is_some_and(|l| l.status.pending()) {
            InstanceState::Deploying
        } else {
            // Deployed but not loaded on this node yet (or between a switch and its commit):
            // nothing runs, which is asleep, not "not deployed".
            match self.lives.lock().get(&env_key(&e.service, &e.name)).map(|l| l.state()) {
                Some(InstanceState::None) | None if e.active.is_some() => InstanceState::Stopped,
                Some(s) => s,
                None => InstanceState::None,
            }
        };
        let mut attention = Vec::new();
        if self.lives.lock().get(&env_key(&e.service, &e.name)).is_some_and(|l| l.crash_loop().is_some()) {
            attention.push("crash_loop");
        }
        if self.capped(&e.service) {
            attention.push("capped");
        }
        if e.paused {
            attention.push("paused");
        }
        if e.held_by.is_some() {
            attention.push("held");
        }
        if e.maintenance.is_some() {
            attention.push("maintenance");
        }
        if latest.as_ref().is_some_and(|l| l.status == DeployStatus::Failed) {
            attention.push("deploy_failed");
        }
        // Cached for a minute per environment ([`Apps::needs_deploy`]), so a list stays cheap.
        let needs_deploy = match (&e.active, svc_row) {
            (Some(a), Some(svc)) => self.needs_deploy(&svc.for_env(e).unwrap_or(svc), e, a).await?,
            _ => false,
        };
        Ok(EnvironmentSummary { name: e.name.clone(), state, url: public.then(|| self.url(&e.slug)), active, latest, attention, needs_deploy, ephemeral: e.ephemeral })
    }

    pub async fn patch_service(self: &Arc<Self>, id: &str, body: Value) -> Result<Service> {
        let o = body.as_object().ok_or_else(|| Error::BadRequest("the body is an object".into()))?.clone();
        for k in o.keys() {
            if !["name", "source", "build", "run"].contains(&k.as_str()) {
                return Err(Error::BadRequest(format!("unknown field {k}")));
            }
        }
        let sid = id.to_string();
        let mut row = self.db(move |s| s.service(&sid)).await?;
        if let Some(n) = o.get("name") {
            let n = n.as_str().ok_or_else(|| Error::BadRequest("name is a string".into()))?;
            check_name("a service name", n)?;
            row.name = n.to_string();
        }
        let (mut forget_login, mut forget_git) = (false, false);
        if let Some(s) = o.get("source") {
            let source: Source = serde_json::from_value(s.clone()).map_err(|e| Error::BadRequest(format!("source: {e}")))?;
            self.check_source(&source)?;
            // A registry login belongs to its registry: another one never receives it.
            let registry = |s: &Source| match s {
                Source::Image { image } => Some(image_registry(image)),
                _ => None,
            };
            forget_login = registry(&row.source) != registry(&source);
            // So does a Git login its repository: one given for a repository is never used for
            // another, even on the same host.
            let git = |s: &Source| match s {
                Source::Git { url, .. } => git_repo(url),
                _ => None,
            };
            forget_git = git(&row.source) != git(&source);
            row.source = source;
        }
        row.build = merge(&row.build, o.get("build"), "build")?;
        row.build.check()?;
        row.run = row.run.merged(o.get("run"))?;
        let r = row.clone();
        self.db(move |s| {
            s.update_service(&r)?;
            if forget_login {
                s.delete_registry(&r.id)?;
            }
            if forget_git {
                s.delete_git_login(&r.id)?;
                s.delete_git_github(&r.id)?;
            }
            Ok(())
        })
        .await?;
        if forget_login {
            self.registry_changed(id, None).await;
        }
        self.service(id).await
    }

    /// `DELETE …/services/{id}`: the service stops and is hidden, kept for
    /// [`DELETED_KEPT_MS`] (its environments, volumes, images, backups, domains and name) for
    /// [`Apps::restore_service`]; the prune sweep deletes it for good after that. A deleted
    /// service's delete deletes it for good now.
    pub async fn delete_service(self: &Arc<Self>, id: &str, actor: Option<String>) -> Result<()> {
        let sid = id.to_string();
        let (_, deleted) = self.db(move |s| s.service_any(&sid)).await?;
        if deleted.is_some() {
            return self.purge_service(id).await;
        }
        // Nothing it queued builds or goes live behind the delete.
        let sid = id.to_string();
        for d in self.db(move |s| s.pending_deployments(&sid)).await? {
            self.cancel(id, &d.environment, &d.id).await.ok();
        }
        let (sid, now) = (id.to_string(), self.store.now()?);
        if !self.db(move |s| s.set_deleted(&sid, Some(now))).await? {
            return Ok(());
        }
        // From here no request, hostname, hook or sweep finds it: what runs stops.
        self.hosts_changed();
        let sid = id.to_string();
        for e in self.db(move |s| s.environments(&sid)).await? {
            self.teardown_live(id, &e.name).await;
        }
        for name in self.deps.runtime.list(Some(id)).await {
            self.deps.runtime.remove(&name).await;
        }
        self.settings_changed();
        self.audit("deleted", id, "", actor, json!({})).await;
        self.unplace(id).await;
        Ok(())
    }

    /// `POST …/services/{id}/restore`: a deleted service back as it was, its environments'
    /// live deployments loaded again (each wakes or starts as it would after a restart).
    pub async fn restore_service(self: &Arc<Self>, id: &str, actor: Option<String>) -> Result<Service> {
        let sid = id.to_string();
        let (row, deleted) = self.db(move |s| s.service_any(&sid)).await?;
        if deleted.is_none() {
            return Err(Error::Conflict(format!("{} isn't deleted", row.name)));
        }
        let sid = id.to_string();
        if !self.db(move |s| s.set_deleted(&sid, None)).await? {
            return Err(Error::Conflict(format!("{} isn't deleted", row.name)));
        }
        self.hosts_changed();
        self.settings_changed();
        if let Some(ws) = self.deps.workspace.clone() {
            let (sid, project) = (id.to_string(), row.project.clone());
            let _ = tokio::task::spawn_blocking(move || {
                let (x, y) = free_spot(&ws, &project);
                ws.place(roll_workspace::Scope::All, &project, roll_workspace::Kind::Service, &sid, x, y)
            })
            .await;
        }
        self.audit("restored", id, "", actor, json!({})).await;
        self.resume_service(id).await;
        self.service(id).await
    }

    /// The deleted services (of a project, or all), newest first: when each was deleted and
    /// when it goes for good.
    pub async fn deleted_services(self: &Arc<Self>, project: Option<String>) -> Result<Value> {
        let rows = self.db(move |s| s.deleted_services(project.as_deref())).await?;
        Ok(rows
            .into_iter()
            .map(|(r, at)| json!({ "id": r.id, "project": r.project, "name": r.name, "deleted_at": at, "purge_at": at + DELETED_KEPT_MS }))
            .collect())
    }

    /// The service's place on its project's canvas goes (a restore places it again).
    async fn unplace(self: &Arc<Self>, id: &str) {
        if let Some(ws) = self.deps.workspace.clone() {
            let sid = id.to_string();
            let _ = tokio::task::spawn_blocking(move || {
                if let Some(p) = ws.list(roll_workspace::Scope::All).ok().and_then(|ps| ps.into_iter().find(|p| p.resources.iter().any(|r| r.kind == roll_workspace::Kind::Service && r.r#ref == sid))) {
                    let _ = ws.remove(roll_workspace::Scope::All, &p.id, roll_workspace::Kind::Service, &sid);
                }
            })
            .await;
        }
    }

    /// Delete a service for good, and everything that belongs to it.
    pub async fn purge_service(self: &Arc<Self>, id: &str) -> Result<()> {
        let sid = id.to_string();
        let envs = self.db(move |s| s.environments(&sid)).await?;
        let sid = id.to_string();
        let (row, _) = self.db(move |s| s.service_any(&sid)).await?;
        let project = row.project.clone();
        for e in &envs {
            self.teardown_live(id, &e.name).await;
        }
        // Each environment's volume goes with it (a last backup first, kept a week).
        for e in &envs {
            let had = row.for_env(e).map(|s| s.run.volume.is_some()).unwrap_or(row.run.volume.is_some());
            self.last_backup(id, &e.name, had).await;
            self.deps.runtime.remove_volume(&volume_name(id, &e.name)).await;
        }
        for name in self.deps.runtime.list(Some(id)).await {
            self.deps.runtime.remove(&name).await;
        }
        self.deps.runtime.forget_service(id).await;
        // Links to it from the project's other services go (their next deploy drops the URLs).
        let sid = id.to_string();
        if self.db(move |s| s.delete_links_to(LinkKind::Service, &sid)).await? > 0 {
            self.settings_changed();
        }
        // Its links' own credentials are revoked with it.
        let sid = id.to_string();
        for (kind, r, _, minted) in self.db(move |s| s.links_minted(&sid)).await? {
            if let Some(m) = minted {
                self.revoke_link(id, kind, &r, &m.kept).await;
            }
        }
        let sid = id.to_string();
        let deps = self.db(move |s| s.deployments_all(&sid)).await?;
        for d in deps {
            if let Some(img) = &d.image {
                self.deps.builder.remove_image(img).await;
                if let Some(k) = sites::key(img) {
                    self.drop_site(&k).await;
                }
            }
            let id = d.id.clone();
            if let Ok(Some(key)) = self.db(move |s| s.deployment_upload(&id)).await {
                self.drop_upload(&d.id, &key).await;
            }
            self.logs.delete(&format!("build:{}", d.id)).await;
        }
        let sid = id.to_string();
        let domains = self.db(move |s| s.custom_domains_of(&sid, None)).await?;
        let sid = id.to_string();
        self.db(move |s| s.delete_service(&sid)).await?;
        // Its hostnames name nothing now.
        self.hosts_changed();
        // Its environments' networks, where no other service of the project has that environment.
        for e in &envs {
            self.drop_network_if_unused(&project, &e.name, id).await;
        }
        self.release_domains(&domains).await;
        // The project's closed builds' builder, with its last service (a deleted one counts).
        let p = project.clone();
        if self.db(move |s| Ok(s.services(Some(&p))?.is_empty() && s.deleted_services(Some(&p))?.is_empty())).await.unwrap_or(false) {
            self.deps.runtime.remove_build_egress(&project).await;
        }
        self.unplace(id).await;
        self.deps.cluster.retire(&lease_key(id));
        Ok(())
    }

    // ── environments ──

    pub async fn environments(self: &Arc<Self>, service: &str) -> Result<Vec<Environment>> {
        let sid = service.to_string();
        self.db(move |s| s.service(&sid)).await?;
        let sid = service.to_string();
        let rows = self.db(move |s| s.environments(&sid)).await?;
        let mut out = Vec::new();
        for r in rows {
            out.push(self.environment_view(r).await?);
        }
        Ok(out)
    }

    pub async fn environment(self: &Arc<Self>, service: &str, env: &str) -> Result<Environment> {
        let (sid, e) = (service.to_string(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        self.environment_view(row).await
    }

    /// `GET …/pending`: what the next deploy changes (Railway's staged changes). The
    /// environment's own variables added, removed or changed since its live deployment went
    /// live (names only, from the snapshot kept at its switch; null when it has none, a
    /// deployment from before snapshots), and the run settings the service sets that the live
    /// deployment doesn't run with. Links and project secrets move `needs_deploy` too, unnamed.
    pub async fn pending(self: &Arc<Self>, service: &str, env: &str) -> Result<Value> {
        let (svc, row) = self.service_env(service, env).await?;
        let Some(active) = row.active.clone() else { return Ok(json!({ "variables": null, "run": [], "secrets": null })) };
        let a = active.clone();
        let (dep, then, digest_then) = self.db(move |s| Ok((s.deployment(&a)?, s.snapshot(&a)?, s.secrets_digest(&a)?))).await?;
        // The project's secrets since it went live (null: no digest to compare).
        let secrets = match (digest_then, self.deps.secrets.digest(&svc.project, env).await.ok().flatten()) {
            (Some(then), Some(now)) => Value::Bool(then != now),
            _ => Value::Null,
        };
        let (sid, e) = (service.to_string(), env.to_string());
        let now = self.db(move |s| s.variables(&sid, &e)).await?;
        let variables = then.map(|then| {
            let then = then.vars;
            let added: Vec<&String> = now.keys().filter(|k| !then.contains_key(*k)).collect();
            let removed: Vec<&String> = then.keys().filter(|k| !now.contains_key(*k)).collect();
            let changed: Vec<&String> = now.iter().filter(|(k, v)| then.get(*k).is_some_and(|t| t != *v)).map(|(k, _)| k).collect();
            json!({ "added": added, "removed": removed, "changed": changed })
        });
        // Only what the service sets: the deployment also holds what its build worked out (a
        // Dockerfile's port, a plan's start command).
        // As the environment runs it: the service's settings with its own over them.
        let want = serde_json::to_value(svc.for_env(&row).map_or(svc.run.clone(), |s| s.run)).unwrap_or_default();
        let has = serde_json::to_value(&dep.run).unwrap_or_default();
        let run: Vec<&String> = want.as_object().into_iter().flatten().filter(|(k, v)| has.get(k.as_str()) != Some(*v)).map(|(k, _)| k).collect();
        Ok(json!({ "variables": variables, "run": run, "secrets": secrets }))
    }

    /// Whether the environment's settings, variables or links changed since its active
    /// deployment `active`: cached ([`Apps::needs`]) since working it out is the expensive part of
    /// an environment's read, and the dashboard reads it every few seconds.
    async fn needs_deploy(self: &Arc<Self>, svc: &ServiceRow, r: &EnvRow, active: &str) -> Result<bool> {
        use std::sync::atomic::Ordering::SeqCst;
        let key = env_key(&r.service, &r.name);
        let seen = self.settings_gen.load(SeqCst);
        // The project's secrets moved: worked out again at once, not after the minute.
        let digest = self.deps.secrets.digest(&svc.project, &r.name).await.ok().flatten();
        if let Some((at, g, a, d, v)) = self.needs.lock().get(&key)
            && *g == seen
            && a == active
            && *d == digest
            && at.elapsed() < Duration::from_secs(60)
        {
            return Ok(*v);
        }
        let a = active.to_string();
        let dep = self.db(move |s| s.deployment(&a)).await?;
        let v = match self.listed_vars(svc, r, &dep.id).await {
            Ok(v) if dep.vars_hash.starts_with("2:") => fingerprint(&v.vars_for_hash(), svc) != dep.vars_hash,
            // From before settings counted by what they set: any setting added since moves its
            // hash, so what `/pending` names decides (variables, run settings, secrets).
            Ok(_) => self.pending(&r.service, &r.name).await.is_ok_and(|p| {
                let vars = p["variables"].as_object().is_some_and(|m| m.values().any(|l| l.as_array().is_some_and(|a| !a.is_empty())));
                vars || p["run"].as_array().is_some_and(|a| !a.is_empty()) || p["secrets"] == Value::Bool(true)
            }),
            Err(_) => false,
        };
        // Kept only if nothing changed while it was worked out.
        if self.settings_gen.load(SeqCst) == seen {
            self.needs.lock().insert(key, (std::time::Instant::now(), seen, active.to_string(), digest, v));
        }
        Ok(v)
    }

    /// `service` changed: whoever follows it reads it again.
    pub fn changed(&self, service: &str) {
        // What a host names may have changed with it (made private, a domain, an environment).
        self.hosts_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = self.changes.send(service.to_string());
    }

    pub fn subscribe_changes(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.changes.subscribe()
    }

    /// What its instances are doing, in memory: a follower compares it each second (instances
    /// wake, sleep and crash without anyone changing anything).
    pub fn instances_digest(&self, service: &str) -> String {
        let mut lives: Vec<Arc<EnvLive>> = self.lives.lock().values().filter(|l| l.service == service).cloned().collect();
        lives.sort_by(|a, b| a.env.cmp(&b.env));
        let mut out = String::new();
        for l in lives {
            out.push_str(&format!("{}:{:?}:{:?};", l.env, l.state(), l.canary()));
            for i in l.instances() {
                out.push_str(&format!("{}={:?},", i.name, i.state()));
            }
        }
        out
    }

    /// Something a deployment's fingerprint reads changed: every cached `needs_deploy` is stale.
    /// Forget every host's kept answer ([`Apps::env_for_host`]).
    pub(crate) fn hosts_changed(&self) {
        self.hosts_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn settings_changed(&self) {
        self.settings_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.hosts_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    async fn environment_view(self: &Arc<Self>, r: EnvRow) -> Result<Environment> {
        let summary = self.summary(&r).await?;
        let (instances, crash_loop, canary, scaling) = match self.lives.lock().get(&env_key(&r.service, &r.name)) {
            Some(l) => (
                l.instances().iter().map(|i| Instance { id: i.name.clone(), deployment: i.deployment.clone(), state: i.state(), started_at: i.started_at(), in_flight: i.in_flight() }).collect(),
                l.crash_loop(),
                l.canary().map(|(deployment, share)| crate::model::Canary { waiting: self.rollout_waits.lock().contains_key(&deployment), deployment, share }),
                l.scaling().map(|(backlog, wanted)| crate::model::Scaling { backlog, wanted }),
            ),
            None => (Vec::new(), None, None, None),
        };
        let domains = self.domains(&r.service, &r.name).await?;
        let sid = r.service.clone();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let svc = svc.for_env(&r).unwrap_or(svc);
        let next_run_at = match (&svc.run.schedule, &r.active) {
            (Some(sched), Some(_)) if svc.run.kind == RunType::Cron => crate::cron::Schedule::parse(sched, svc.run.timezone.as_deref()).ok().and_then(|s| s.next_after(code_sql::now_ms())),
            _ => None,
        };
        let gate = self.access(&r.service, &r.name).await?;
        let (private, signed_in, password) = (gate.is_some(), gate.as_ref().is_some_and(|a| a.signed_in), gate.as_ref().is_some_and(|a| a.password_hash.is_some()));
        let (sid, e) = (r.service.clone(), r.name.clone());
        let waiting_for_ci = self.db(move |s| s.ci_waits()).await?.into_iter().find(|(w, _)| w.service == sid && w.env == e).map(|(w, _)| w.sha);
        let (sid, e) = (r.service.clone(), r.name.clone());
        let config = self.db(move |s| s.runtime_config(&sid, &e)).await?;
        let (sid, e) = (r.service.clone(), r.name.clone());
        let rotated = self.db(move |s| s.runtime_config_rotated(&sid, &e)).await?;
        Ok(Environment {
            summary,
            service: r.service.clone(),
            branch: r.branch.clone(),
            instances,
            domains,
            port: r.port,
            tcp: r.tcp_port.filter(|_| svc.run.tcp && svc.run.public && !private).map(|p| format!("{}:{p}", self.cfg.tcp_host)),
            private,
            signed_in,
            password,
            next_run_at,
            schedules: self.schedule_states(&svc, &r).await,
            volume_used: match svc.run.volume {
                Some(_) => self.volume_usage(&r.service, &r.name).await.map(|(bytes, at)| crate::model::VolumeUsed { bytes, at }),
                None => None,
            },
            crash_loop,
            canary,
            run: r.run.clone(),
            hook: r.hook_hash.is_some(),
            follow_tag: r.follow_tag,
            redeploy_on_secrets: r.redeploy_on_secrets,
            traces: {
                let (sid, e) = (r.service.clone(), r.name.clone());
                self.db(move |s| s.trace_token(&sid, &e)).await.ok().flatten().is_some()
            },
            feedback: r.feedback,
            vitals: r.vitals,
            report: r.report,
            preview_host: {
                let (sid, e) = (r.service.clone(), r.name.clone());
                self.db(move |s| s.preview_host(&sid, &e)).await.ok().flatten()
            },
            maintenance: r.maintenance.clone().map(|m| crate::model::Maintenance { message: Some(m).filter(|m| !m.is_empty()) }),
            paused: r.paused,
            held_after: r.held_by.clone().filter(|h| h != crate::deploy::MANUAL_HOLD),
            held: r.held_by.as_deref() == Some(crate::deploy::MANUAL_HOLD),
            pages: r.pages.clone(),
            allow_ips: r.allow_ips.clone(),
            rate_limit: r.rate_limit,
            firewall: r.firewall.clone(),
            headers: r.headers.clone(),
            routes: r.routes.clone(),
            check_states: self.check_states_of(&r.service, &r.name, &r.checks),
            checks: r.checks.clone(),
            heartbeat_states: self.heartbeat_states_of(&r.service, &r.name, &r.heartbeats).await,
            heartbeats: r.heartbeats.clone(),
            split: r.split.clone(),
            mirror: r.mirror.clone(),
            snippets: r.snippets.clone(),
            config_version: config.as_ref().map(|(_, v)| *v),
            config_rotated_at: rotated,
            config: config.map(|(c, _)| c).filter(|c| !c.is_null()),
            pause_schedule: r.pause_schedule.clone(),
            frozen: r.freeze.as_ref().is_some_and(|f| f.on(code_sql::now_ms())),
            freeze: r.freeze.clone(),
            heartbeat_urls: Default::default(),
            egress_refused: if r.egress.closed() { self.deps.runtime.egress_refused(&closed_network_name(&svc.project, &r.name), &r.egress.hosts) } else { Vec::new() },
            egress: r.egress.clone(),
            scaling,
            wait_for_ci: r.wait_for_ci,
            waiting_for_ci,
            build_closed: r.egress.closed(),
            base_updated: match r.active.clone() {
                // What the watch found for the live deployment (a newer one starts with nothing).
                Some(a) => {
                    let k = format!("base_moved:{}/{}", r.service, r.name);
                    let v: Option<Value> = self.db(move |s| s.get_state(&k)).await.ok().flatten().and_then(|v| serde_json::from_str(&v).ok());
                    v.filter(|v| v["deployment"] == a.as_str()).and_then(|v| serde_json::from_value(v["images"].clone()).ok()).unwrap_or_default()
                }
                None => Vec::new(),
            },
            created_at: r.created_at,
        })
    }

    /// `PUT …/environments/{env}`: make it (a preview environment, a staging one) or change its
    /// branch.
    /// A new generated name (`<hostname>.<domain>`): the old one stops answering at once, here
    /// and within a second on other nodes. Where custom domains' CNAMEs name it (no fixed
    /// `cname_target`), it's refused while the environment has any: they'd stop routing.
    async fn set_hostname(self: &Arc<Self>, service: &str, env: &str, h: &str) -> Result<()> {
        let (sid, e) = (service.to_string(), env.to_string());
        let (row, domains) = self.db(move |s| Ok((s.environment(&sid, &e)?, s.domains(&sid, &e)?))).await?;
        if row.slug == h {
            return Ok(());
        }
        if self.cfg.cname_target.is_none() && domains.iter().any(|d| d.kind == DomainKind::Custom) {
            return Err(Error::Conflict(format!("custom domains point at {}.{}: remove them first, or add them again after", row.slug, self.cfg.domain)));
        }
        let (sid, e, h2) = (service.to_string(), env.to_string(), h.to_string());
        self.db(move |s| s.set_slug(&sid, &e, &h2)).await?;
        self.changed(service);
        Ok(())
    }

    /// `GET …/environments/{env}/diff?from=`: how `env` differs from `from` (Railway's
    /// environment sync): its own variables' names missing here, extra here, or with another
    /// value (never a value; sealed ones can't be compared), and each setting that differs, with
    /// the `PUT …/environments/{env}` body that makes it `from`'s. A grant reads `from` too, and
    /// sees which values differ only with reveal on both (a value set here and compared would
    /// tell `from`'s).
    pub async fn environment_diff(self: &Arc<Self>, service: &str, env: &str, from: &str, grant: Option<&roll_auth::Grant>) -> Result<Value> {
        if from == env {
            return Err(Error::BadRequest("from is another environment".into()));
        }
        let (to_env, from_env) = (self.environment(service, env).await?, self.environment(service, from).await?);
        let mut values = true;
        if let Some(g) = grant {
            let sid = service.to_string();
            let project = self.db(move |s| s.service(&sid)).await?.project;
            let on = |e: &str| roll_auth::scope::Resource { product: "apps".into(), id: Some(service.to_string()), project: Some(project.clone()), environment: Some(e.to_string()), ..Default::default() };
            if g.allows(&on(from), roll_auth::scope::Action::Read) != roll_auth::scope::Outcome::Allow {
                return Err(Error::Forbidden(format!("comparing with {from} needs read on {from}")));
            }
            values = [from, env].iter().all(|e| g.allows(&on(e), roll_auth::scope::Action::Reveal) == roll_auth::scope::Outcome::Allow);
        }
        let (sid, e, f) = (service.to_string(), env.to_string(), from.to_string());
        let (here, there, sealed_here, sealed_there) = self.db(move |s| Ok((s.variables(&sid, &e)?, s.variables(&sid, &f)?, s.sealed_variables(&sid, &e)?, s.sealed_variables(&sid, &f)?))).await?;
        let missing: Vec<Value> = there.keys().filter(|k| !here.contains_key(*k)).map(|k| json!({ "name": k, "sealed": sealed_there.contains(k) })).collect();
        let extra: Vec<&String> = here.keys().filter(|k| !there.contains_key(*k)).collect();
        let differ: Vec<&String> = here.iter().filter(|(k, v)| !sealed_here.contains(*k) && !sealed_there.contains(*k) && there.get(*k).is_some_and(|t| t != *v)).map(|(k, _)| k).collect();
        let (a, b) = (serde_json::to_value(&from_env).expect("JSON"), serde_json::to_value(&to_env).expect("JSON"));
        let mut settings = Vec::new();
        for k in crate::model::ENV_RUN_FIELDS {
            let (x, y) = (from_env.run.get(k).cloned().unwrap_or(Value::Null), to_env.run.get(k).cloned().unwrap_or(Value::Null));
            if x != y {
                settings.push(json!({ "field": format!("run.{k}"), "from": x, "to": y, "put": { "run": { k: x } } }));
            }
        }
        for k in ["wait_for_ci", "follow_tag", "redeploy_on_secrets", "traces", "vitals", "report", "egress", "allow_ips", "rate_limit", "firewall", "headers", "routes", "checks"] {
            let (x, y) = (a.get(k).cloned().unwrap_or(Value::Null), b.get(k).cloned().unwrap_or(Value::Null));
            if x != y {
                settings.push(json!({ "field": k, "from": x, "to": y, "put": { k: x } }));
            }
        }
        for (k, x, y) in [("error", &from_env.pages.error, &to_env.pages.error), ("maintenance", &from_env.pages.maintenance, &to_env.pages.maintenance)] {
            if x != y {
                // Whether each has its own page (its HTML only in the body that copies it).
                settings.push(json!({ "field": format!("pages.{k}"), "from": x.is_some(), "to": y.is_some(), "put": { "pages": { k: x } } }));
            }
        }
        Ok(json!({
            "from": from,
            "to": env,
            "variables": { "missing": missing, "extra": extra, "differ": if values { json!(differ) } else { Value::Null } },
            "settings": settings,
        }))
    }

    /// The values of `names` in `from` for `PATCH …/variables {copy_from, copy}`: its own
    /// variables, never a sealed one (deploying the copy would read it, and that can take less
    /// than reading it where it is).
    pub async fn copied_variables(self: &Arc<Self>, service: &str, from: &str, names: &[String]) -> Result<BTreeMap<String, String>> {
        let (sid, f) = (service.to_string(), from.to_string());
        let (have, sealed) = self.db(move |s| {
            s.environment(&sid, &f)?;
            Ok((s.variables(&sid, &f)?, s.sealed_variables(&sid, &f)?))
        })
        .await?;
        let mut out = BTreeMap::new();
        for n in names {
            if sealed.contains(n) {
                return Err(Error::BadRequest(format!("{n} is sealed in {from}: set it here by hand")));
            }
            let v = have.get(n).ok_or_else(|| Error::BadRequest(format!("{n} isn't one of {from}'s own variables")))?;
            out.insert(n.clone(), v.clone());
        }
        Ok(out)
    }

    pub async fn put_environment(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Environment> {
        let o = body.as_object().cloned().unwrap_or_default();
        for k in o.keys() {
            if !["branch", "ephemeral", "access", "variables_from", "wait_for_ci", "run", "follow_tag", "redeploy_on_secrets", "feedback", "maintenance", "pages", "allow_ips", "rate_limit", "firewall", "headers", "routes", "checks", "heartbeats", "split", "mirror", "snippets", "config", "freeze", "pause_schedule", "paused", "held", "hostname", "traces", "egress", "vitals", "report", "preview_host"].contains(&k.as_str()) {
                return Err(Error::BadRequest(format!("unknown field {k}")));
            }
        }
        // The new settings are checked before anything is written: a 400 changes nothing.
        let hostname = match o.get("hostname") {
            None | Some(Value::Null) => None,
            Some(Value::String(h)) => {
                crate::model::check_slug(h)?;
                Some(h.clone())
            }
            Some(_) => return Err(Error::BadRequest("hostname is the environment's name on the roll domain".into())),
        };
        // Hold deploys (`true`: pushes, hooks and tag moves build to `ready` for a person to put
        // live) or resume them (`false`: a person's hold, or a rollback's).
        let hold = match o.get("held") {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => return Err(Error::BadRequest("held is true (deploys wait for a person) or false (they resume)".into())),
        };
        let paused = match o.get("paused") {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => return Err(Error::BadRequest("paused is a boolean".into())),
        };
        // One DNS label under the project's preview domain, or null.
        let preview_label: Option<Option<String>> = match o.get("preview_host") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(l)) if l.len() <= 63 && !l.is_empty() && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') => Some(Some(l.clone())),
            Some(_) => return Err(Error::BadRequest("preview_host is one DNS label (lowercase letters, digits and -, at most 63), or null".into())),
        };
        // Maintenance: true or {message?} turns it on, false or null off.
        let maintenance: Option<Option<String>> = match o.get("maintenance") {
            None => None,
            Some(Value::Null | Value::Bool(false)) => Some(None),
            Some(Value::Bool(true)) => Some(Some(String::new())),
            Some(Value::Object(m)) => {
                if let Some(k) = m.keys().find(|k| k.as_str() != "message") {
                    return Err(Error::BadRequest(format!("maintenance.{k} isn't a field")));
                }
                match m.get("message") {
                    None | Some(Value::Null) => Some(Some(String::new())),
                    Some(Value::String(t)) if t.chars().count() <= 500 => Some(Some(t.trim().to_string())),
                    Some(_) => return Err(Error::BadRequest("maintenance.message is at most 500 characters".into())),
                }
            }
            Some(_) => return Err(Error::BadRequest("maintenance is true, {message?} or false".into())),
        };
        // Its own pages: each field HTML (at most 64 KiB) or null for roll's; the rest stay.
        let pages_patch = match o.get("pages") {
            None => None,
            Some(Value::Object(p)) => {
                if let Some(k) = p.keys().find(|k| !matches!(k.as_str(), "error" | "maintenance")) {
                    return Err(Error::BadRequest(format!("pages.{k} isn't a page (error, maintenance)")));
                }
                for (k, v) in p {
                    match v {
                        Value::Null => {}
                        Value::String(h) if !h.trim().is_empty() && h.len() <= crate::model::PAGE_MAX => {}
                        _ => return Err(Error::BadRequest(format!("pages.{k} is HTML of at most 64 KiB, or null for roll's"))),
                    }
                }
                Some(p.clone())
            }
            Some(_) => return Err(Error::BadRequest("pages is {error?, maintenance?}".into())),
        };
        // Who gets in through its public names: networks (`a.b.c.d/n`, a bare address), or
        // null / [] for anyone.
        let allow_ips = match o.get("allow_ips") {
            None => None,
            Some(Value::Null) => Some(Vec::new()),
            Some(Value::Array(a)) if a.len() <= 100 => Some(a.iter().map(crate::model::ip_rule).collect::<Result<Vec<_>>>()?),
            Some(_) => return Err(Error::BadRequest("allow_ips is at most 100 networks, or null".into())),
        };
        let rate_limit = match o.get("rate_limit") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(v) => {
                let l: crate::model::RateLimit = serde_json::from_value(v.clone()).map_err(|e| Error::BadRequest(format!("rate_limit is {{per_minute, burst?}} or null: {e}")))?;
                l.check()?;
                Some(Some(l))
            }
        };
        let firewall = match o.get("firewall") {
            None => None,
            Some(Value::Null) => Some(Vec::new()),
            Some(Value::Array(a)) if a.len() <= crate::model::MAX_FIREWALL_RULES => Some(
                a.iter()
                    .map(|r| serde_json::from_value::<crate::model::FirewallRule>(r.clone()).map_err(|e| Error::BadRequest(format!("firewall: {e}"))).and_then(crate::model::FirewallRule::check))
                    .collect::<Result<Vec<_>>>()?,
            ),
            Some(_) => return Err(Error::BadRequest(format!("firewall is at most {} rules, or null", crate::model::MAX_FIREWALL_RULES))),
        };
        let headers = match o.get("headers") {
            None => None,
            Some(Value::Null) => Some(Vec::new()),
            Some(Value::Array(a)) if a.len() <= crate::model::MAX_HEADER_RULES => Some(
                a.iter()
                    .map(|r| serde_json::from_value::<crate::model::HeaderRule>(r.clone()).map_err(|e| Error::BadRequest(format!("headers: {e}"))).and_then(crate::model::HeaderRule::check))
                    .collect::<Result<Vec<_>>>()?,
            ),
            Some(_) => return Err(Error::BadRequest(format!("headers is at most {} rules, or null", crate::model::MAX_HEADER_RULES))),
        };
        // Paths another service of the project answers: each named service must be there (and
        // not this one: it would answer itself).
        let routes = match o.get("routes") {
            None => None,
            Some(Value::Null) => Some(Vec::new()),
            Some(Value::Array(a)) if a.len() <= crate::model::MAX_ROUTES => {
                let rs = a
                    .iter()
                    .map(|r| serde_json::from_value::<crate::model::Route>(r.clone()).map_err(|e| Error::BadRequest(format!("routes: {e}"))).and_then(crate::model::Route::check))
                    .collect::<Result<Vec<_>>>()?;
                for r in &rs {
                    match self.project_service(service, &r.service).await {
                        Some(id) if id == service => return Err(Error::BadRequest(format!("routes: {} is this service", r.service))),
                        Some(_) => {}
                        None => return Err(Error::BadRequest(format!("routes: the project has no service named {}", r.service))),
                    }
                }
                Some(rs)
            }
            Some(_) => return Err(Error::BadRequest(format!("routes is at most {} routes, or null", crate::model::MAX_ROUTES))),
        };
        // Uptime checks: unique names, at most MAX_CHECKS.
        // Paused by the clock: weekly windows, as a freeze's.
        let pause_schedule = match o.get("pause_schedule") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(v) => {
                let p: crate::freeze::PauseSchedule = serde_json::from_value(v.clone()).map_err(|e| Error::BadRequest(format!("pause_schedule: {e}")))?;
                p.check()?;
                Some(Some(p))
            }
        };
        // Runtime config: values its instances read without a deploy (null: none).
        let config = match o.get("config") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(v @ Value::Object(m)) => {
                let key = |k: &str| (1..=128).contains(&k.len()) && k.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
                if m.len() > 200 || v.to_string().len() > 64 << 10 || !m.keys().all(|k| key(k)) {
                    return Err(Error::BadRequest("config is an object of at most 200 keys (letters, digits, _ . -; up to 128 each), 64 KiB in all".into()));
                }
                Some(Some(v.clone()))
            }
            Some(_) => return Err(Error::BadRequest("config is an object (or null)".into())),
        };
        // Snippets: HTML of its own in every page (null or []: none).
        let snippets = match o.get("snippets") {
            None => None,
            Some(Value::Null) => Some(Vec::new()),
            Some(v) => {
                let all: Vec<crate::model::Snippet> = serde_json::from_value(v.clone()).map_err(|e| Error::BadRequest(format!("snippets: {e}")))?;
                crate::model::Snippet::check(&all)?;
                Some(all)
            }
        };
        // Traffic mirroring: another environment of this service, as a split names one.
        let mirror = match o.get("mirror") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(v) => {
                let m: crate::model::Split = serde_json::from_value(v.clone()).map_err(|e| Error::BadRequest(format!("mirror: {e}")))?;
                if !(1..=100).contains(&m.percent) || m.environment == env {
                    return Err(Error::BadRequest("mirror is {environment: another environment, percent: 1–100}".into()));
                }
                let (sid, other) = (service.to_string(), m.environment.clone());
                if self.db(move |s| s.environment(&sid, &other)).await.is_err() {
                    return Err(Error::BadRequest(format!("mirror: this service has no environment {}", m.environment)));
                }
                Some(Some(m))
            }
        };
        // A deploy freeze: on until lifted, until a time, or in weekly windows.
        let freeze = match o.get("freeze") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(v) => {
                let f: crate::freeze::Freeze = serde_json::from_value(v.clone()).map_err(|e| Error::BadRequest(format!("freeze: {e}")))?;
                f.check()?;
                Some(Some(f))
            }
        };
        // A split test: another deployed-or-not environment of this service, never itself.
        let split = match o.get("split") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(v) => {
                let sp: crate::model::Split = serde_json::from_value(v.clone()).map_err(|e| Error::BadRequest(format!("split: {e}")))?;
                if !(1..=99).contains(&sp.percent) {
                    return Err(Error::BadRequest("split.percent is 1–99".into()));
                }
                if sp.environment == env {
                    return Err(Error::BadRequest("split.environment is another environment".into()));
                }
                let (sid, other) = (service.to_string(), sp.environment.clone());
                if self.db(move |s| s.environment(&sid, &other)).await.is_err() {
                    return Err(Error::BadRequest(format!("split: this service has no environment {}", sp.environment)));
                }
                Some(Some(sp))
            }
        };
        // Heartbeats: kept ones keep their tokens; a new one's token is in this answer, once.
        let heartbeats = match o.get("heartbeats") {
            None => None,
            Some(v) => {
                let (sid, e) = (service.to_string(), env.to_string());
                let current = self.db(move |s| s.environment(&sid, &e)).await.map(|r| r.heartbeats).unwrap_or_default();
                Some(self.heartbeats_of(v, &current)?)
            }
        };
        let checks = match o.get("checks") {
            None => None,
            Some(Value::Null) => Some(Vec::new()),
            Some(Value::Array(a)) if a.len() <= crate::model::MAX_CHECKS => {
                let cs = a
                    .iter()
                    .map(|c| serde_json::from_value::<crate::model::Check>(c.clone()).map_err(|e| Error::BadRequest(format!("checks: {e}"))).and_then(crate::model::Check::check))
                    .collect::<Result<Vec<_>>>()?;
                if cs.iter().enumerate().any(|(i, c)| cs[..i].iter().any(|o| o.name == c.name)) {
                    return Err(Error::BadRequest("checks: names are unique".into()));
                }
                Some(cs)
            }
            Some(_) => return Err(Error::BadRequest(format!("checks is at most {} checks, or null", crate::model::MAX_CHECKS))),
        };
        // What its code may reach: `{mode: "roll", hosts?}` closes its network, null or
        // `{mode: "open"}` opens it.
        let egress = match o.get("egress") {
            None => None,
            Some(Value::Null) => Some(crate::model::Egress::default()),
            Some(v) => Some(serde_json::from_value::<crate::model::Egress>(v.clone()).map_err(|e| Error::BadRequest(format!("egress is {{mode: \"roll\" | \"open\", hosts?}} or null: {e}")))?.check()?),
        };
        if egress.as_ref().is_some_and(crate::model::Egress::closed) && !self.deps.runtime.closes_egress() {
            return Err(Error::Conflict("egress \"roll\" needs a runtime that can close an environment's network (Docker on this host); this node's can't".into()));
        }
        let branch = match o.get("branch") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(b)) if !b.is_empty() && b.len() <= 200 && !b.contains("..") => Some(Some(b.clone())),
            Some(_) => return Err(Error::BadRequest("branch is a branch name or null".into())),
        };
        let ephemeral = match o.get("ephemeral") {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => return Err(Error::BadRequest("ephemeral is a boolean".into())),
        };
        // A private environment: its hostname answers only visitors with the token (a preview's
        // link), people signed in to roll who may open the project (`signed_in`), or either.
        // `bypass_sha256`: an automation's secret (a CI run's end-to-end tests), only its SHA-256.
        let access: Option<Option<Access>> = match o.get("access") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::Object(a)) => {
                if let Some(k) = a.keys().find(|k| !["token", "bypass_sha256", "signed_in", "password"].contains(&k.as_str())) {
                    return Err(Error::BadRequest(format!("access.{k} isn't a field")));
                }
                let signed_in = match a.get("signed_in") {
                    None | Some(Value::Null) => false,
                    Some(Value::Bool(b)) => *b,
                    Some(_) => return Err(Error::BadRequest("access.signed_in is a boolean".into())),
                };
                // A password visitors type (Vercel's password protection): its argon2id hash only.
                let password = match a.get("password") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(p)) if (8..=200).contains(&p.chars().count()) => Some(crate::password::hash(p).await.map_err(Error::Internal)?),
                    Some(_) => return Err(Error::BadRequest("access.password is 8–200 characters".into())),
                };
                let token = match a.get("token") {
                    None | Some(Value::Null) if signed_in || password.is_some() => None,
                    Some(Value::String(t)) if (20..=200).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)) => Some(t.as_str()),
                    _ => return Err(Error::BadRequest("access.token is 20–200 of A-Z, a-z, 0-9, - and _ (or access.signed_in is true, or there's a password)".into())),
                };
                if signed_in && self.cfg.web_url.is_none() {
                    return Err(Error::Conflict("signing in needs roll's dashboard address on this node (web_url)".into()));
                }
                let bypass = match a.get("bypass_sha256") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(h)) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => Some(h.to_ascii_lowercase()),
                    Some(_) => return Err(Error::BadRequest("access.bypass_sha256 is a SHA-256 in hex".into())),
                };
                Some(Some(Access { token_hash: token.map(crate::public::token_hash), bypass_hash: bypass, signed_in, password_hash: password }))
            }
            Some(_) => return Err(Error::BadRequest("access is {token?, signed_in?, password?, bypass_sha256?} or null".into())),
        };
        // A new environment may start with another's variables (cloning staging from production).
        let variables_from = match o.get("variables_from") {
            None | Some(Value::Null) => None,
            Some(Value::String(f)) if f != env => Some(f.clone()),
            Some(_) => return Err(Error::BadRequest("variables_from is another environment's name".into())),
        };
        let (sid, e) = (service.to_string(), env.to_string());
        let existed = self.db(move |s| s.environment(&sid, &e)).await.is_ok();
        let copied = match (&variables_from, existed) {
            (Some(f), false) => {
                let (sid, f) = (service.to_string(), f.clone());
                // Not the sealed ones (Railway's): deploying the copy would read them, and that
                // can take less than deploying where they are.
                Some(self.db(move |s| {
                    s.environment(&sid, &f)?;
                    let sealed = s.sealed_variables(&sid, &f)?;
                    Ok(s.variables(&sid, &f)?.into_iter().filter(|(k, _)| !sealed.contains(k)).collect::<BTreeMap<_, _>>())
                })
                .await?)
            }
            _ => None,
        };
        let sid = service.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let pname = {
            let this = self.clone();
            let p = svc.project.clone();
            tokio::task::spawn_blocking(move || this.project_name(&p)).await.map_err(|e| Error::Internal(e.to_string()))??
        };
        // The rest checked before anything is written too (a 400 changes nothing): run, the
        // switches, against the environment as it is (or will be made).
        let (sid, e) = (service.to_string(), env.to_string());
        let existing = self.db(move |s| s.environment(&sid, &e)).await.ok();
        if let Some(patch) = o.get("run") {
            let Value::Object(p) = patch else { return Err(Error::BadRequest("run is an object".into())) };
            if let Some(k) = p.keys().find(|k| !crate::model::ENV_RUN_FIELDS.contains(&k.as_str())) {
                return Err(Error::BadRequest(format!("run.{k} is the service's; an environment sets {}", crate::model::ENV_RUN_FIELDS.join(", "))));
            }
            if let Some(mut row) = existing.clone() {
                for (k, v) in p {
                    if v.is_null() {
                        row.run.remove(k);
                    } else {
                        row.run.insert(k.clone(), v.clone());
                    }
                }
                svc.for_env(&row).map_err(|e| Error::BadRequest(e.to_string().replace("no longer fit", "don't fit")))?;
            }
        }
        for k in ["feedback", "follow_tag", "wait_for_ci", "redeploy_on_secrets", "traces", "vitals", "report"] {
            if !matches!(o.get(k), None | Some(Value::Null | Value::Bool(_))) {
                return Err(Error::BadRequest(format!("{k} is a boolean")));
            }
        }
        if o.get("traces") == Some(&Value::Bool(true)) && self.cfg.otlp_base.is_none() {
            return Err(Error::Conflict("traces aren't set up on this node (no OTLP address)".into()));
        }
        if o.get("feedback") == Some(&Value::Bool(true)) && !ephemeral.unwrap_or(existing.as_ref().is_some_and(|r| r.ephemeral)) {
            return Err(Error::BadRequest("feedback is for a preview environment (ephemeral)".into()));
        }
        if o.get("follow_tag") == Some(&Value::Bool(true)) && !matches!(&svc.source, Source::Image { image } if !image.contains('@')) {
            return Err(Error::BadRequest("follow_tag is for an image service whose image has a tag (not a digest)".into()));
        }
        // Only a roll code repo's pushes wait for their commit's checks (its statuses are roll's).
        // A roll code repo's checks are roll's; a Git URL's are GitHub's, read through roll's
        // GitHub App once the service is connected to it.
        // GitLab's and Bitbucket's with the service's Git login.
        let readable = match &svc.source {
            Source::Git { url, .. } => self.github_for(&svc.id, url).await.is_some() || (crate::ci_hosts::host_of(url).is_some() && self.git_remote(&svc.id, url).await.login().is_some()),
            _ => false,
        };
        if o.get("wait_for_ci") == Some(&Value::Bool(true)) && !matches!(&svc.source, Source::Repo { .. }) && !readable {
            return Err(Error::BadRequest("wait_for_ci is for a roll code repo's service, a Git URL service on roll's GitHub App (PUT …/github), or one on GitLab or Bitbucket with a Git login: another Git host's checks aren't roll's to read (deploy from the workflow after its tests instead: sdk/apps-github-action, with the environment's branch unset)".into()));
        }
        // A name another environment has is refused before anything is made or changed.
        if let Some(h) = hostname.clone() {
            let (sid, e) = (service.to_string(), env.to_string());
            if self.db(move |s| s.environment_by_slug(&h)).await?.is_some_and(|r| r.service != sid || r.name != e) {
                return Err(Error::Conflict(format!("{} is taken", hostname.as_deref().unwrap_or(""))));
            }
        }
        self.ensure_environment(&svc, env, branch.as_ref().map(|b| b.as_deref()), ephemeral, &pname).await?;
        if let Some(vars) = copied.filter(|v| !v.is_empty()) {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.change_variables(&sid, &e, &vars, &[], &BTreeSet::new())).await?;
        }
        if let Some(on) = hold {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_hold(&sid, &e, on.then_some(crate::deploy::MANUAL_HOLD))).await?;
            self.changed(service);
        }
        if let Some(h) = hostname {
            self.set_hostname(service, env, &h).await?;
        }
        // Its own size and scale: each field replaces the service's, null goes back to it.
        if let Some(patch) = o.get("run") {
            let Value::Object(p) = patch else { return Err(Error::BadRequest("run is an object".into())) };
            if let Some(k) = p.keys().find(|k| !crate::model::ENV_RUN_FIELDS.contains(&k.as_str())) {
                return Err(Error::BadRequest(format!("run.{k} is the service's; an environment sets {}", crate::model::ENV_RUN_FIELDS.join(", "))));
            }
            let (sid, e) = (service.to_string(), env.to_string());
            let mut row = self.db(move |s| s.environment(&sid, &e)).await?;
            for (k, v) in p {
                if v.is_null() {
                    row.run.remove(k);
                } else {
                    row.run.insert(k.clone(), v.clone());
                }
            }
            svc.for_env(&row).map_err(|e| Error::BadRequest(e.to_string().replace("no longer fit", "don't fit")))?;
            let (sid, e, run) = (service.to_string(), env.to_string(), row.run);
            self.db(move |s| s.set_env_run(&sid, &e, &run)).await?;
            self.settings_changed();
        }
        match o.get("feedback") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(on)) => {
                let (sid, e) = (service.to_string(), env.to_string());
                let row = self.db(move |s| s.environment(&sid, &e)).await?;
                if *on && !row.ephemeral {
                    return Err(Error::BadRequest("feedback is for a preview environment (ephemeral)".into()));
                }
                let (sid, e, on) = (service.to_string(), env.to_string(), *on);
                self.db(move |s| s.set_feedback(&sid, &e, on)).await?;
            }
            Some(_) => return Err(Error::BadRequest("feedback is a boolean".into())),
        }
        if let Some(on) = paused {
            self.set_env_paused(service, env, on).await?;
            // A person's say holds over the schedule until its window ends ("held").
            let k = format!("sched-paused:{service}/{env}");
            let _ = self.db(move |s| s.set_state(&k, "held")).await;
            self.changed(service);
        }
        if let Some(m) = maintenance {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_maintenance(&sid, &e, m.as_deref())).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        if let Some(patch) = pages_patch {
            let (sid, e) = (service.to_string(), env.to_string());
            let set: Vec<(String, Option<String>)> = patch.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string))).collect();
            self.db(move |s| s.patch_pages(&sid, &e, &set)).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        if let Some(rules) = allow_ips {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_allow_ips(&sid, &e, &rules)).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        if let Some(rules) = firewall {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_firewall(&sid, &e, &rules)).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        // Instances are made from the next deploy on with it (a network of their own, the proxy's
        // variables); what runs keeps its way out until then.
        if let Some(eg) = egress {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_egress(&sid, &e, &eg)).await?;
            self.changed(service);
        }
        if let Some(rules) = headers {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_headers(&sid, &e, &rules)).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        if let Some(p) = pause_schedule {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_pause_schedule(&sid, &e, p.as_ref())).await?;
            self.changed(service);
        }
        if let Some(c) = config {
            let (sid, e, c2) = (service.to_string(), env.to_string(), c);
            // Audited with the environment's change (`audit.environment`, `config` among what
            // changed); its instances see it at their next read.
            self.db(move |s| s.set_runtime_config(&sid, &e, c2.as_ref())).await?;
            self.changed(service);
        }
        if let Some(all) = snippets {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_snippets(&sid, &e, &all)).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        if let Some(m) = mirror {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_mirror(&sid, &e, m.as_ref())).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        if let Some(f) = freeze {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_freeze(&sid, &e, f.as_ref())).await?;
            self.changed(service);
        }
        if let Some(sp) = split {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_split(&sid, &e, sp.as_ref())).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        if let Some(routes) = routes {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_routes(&sid, &e, &routes)).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        if let Some(checks) = checks {
            let (sid, e, names) = (service.to_string(), env.to_string(), checks.iter().map(|c| c.name.clone()).collect::<Vec<_>>());
            self.db(move |s| s.set_checks(&sid, &e, &checks)).await?;
            // A removed check's state goes with it.
            let prefix = format!("{}/", crate::env_key(service, env));
            self.check_states.lock().retain(|k, _| !k.strip_prefix(&prefix).is_some_and(|n| !names.iter().any(|x| x == n)));
            self.changed(service);
        }
        if let Some(limit) = rate_limit {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_rate_limit(&sid, &e, limit.as_ref())).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        // The label it answers to under the project's preview domain (roll previews sets it).
        if let Some(label) = preview_label {
            let (sid, e, p) = (service.to_string(), env.to_string(), svc.project.clone());
            self.db(move |s| s.set_preview_host(&sid, &e, &p, label.as_deref())).await?;
            self.hosts_changed();
        }
        // Its deploys' reports to the Git host, from the next one on.
        if let Some(Value::Bool(on)) = o.get("report") {
            let (sid, e, on) = (service.to_string(), env.to_string(), *on);
            self.db(move |s| s.set_report(&sid, &e, on)).await?;
            self.changed(service);
        }
        // Web Vitals: the proxy tags its pages from the next request on.
        if let Some(Value::Bool(on)) = o.get("vitals") {
            let (sid, e, on) = (service.to_string(), env.to_string(), *on);
            self.db(move |s| s.set_vitals(&sid, &e, on)).await?;
            self.live(service, env).forget_gate();
            self.changed(service);
        }
        if let Some(Value::Bool(on)) = o.get("redeploy_on_secrets") {
            let (sid, e, on) = (service.to_string(), env.to_string(), *on);
            self.db(move |s| s.set_redeploy_on_secrets(&sid, &e, on)).await?;
            // Watched from now on (a watch already running ends itself once it's off).
            if on {
                self.watch_secrets_of(service).await;
            }
        }
        // Traces: a token of the environment's own (kept sealed: it goes into every deploy's
        // variables), or none. Its variables reach instances at the next deploy.
        match o.get("traces") {
            Some(Value::Bool(true)) => {
                let (sid, e) = (service.to_string(), env.to_string());
                self.db(move |s| s.ensure_trace_token(&sid, &e)).await?;
                self.live(service, env).forget_gate();
                self.changed(service);
            }
            Some(Value::Bool(false)) => {
                let (sid, e) = (service.to_string(), env.to_string());
                self.db(move |s| s.delete_trace_token(&sid, &e)).await?;
                self.live(service, env).forget_gate();
                self.changed(service);
            }
            _ => {}
        }
        match o.get("follow_tag") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(on)) => {
                if *on && !matches!(&svc.source, Source::Image { image } if !image.contains('@')) {
                    return Err(Error::BadRequest("follow_tag is for an image service whose image has a tag (not a digest)".into()));
                }
                let (sid, e, on) = (service.to_string(), env.to_string(), *on);
                self.db(move |s| s.set_follow_tag(&sid, &e, on)).await?;
            }
            Some(_) => return Err(Error::BadRequest("follow_tag is a boolean".into())),
        }
        match o.get("wait_for_ci") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(on)) => {
                let (sid, e, on) = (service.to_string(), env.to_string(), *on);
                self.db(move |s| s.set_wait_for_ci(&sid, &e, on)).await?;
            }
            Some(_) => return Err(Error::BadRequest("wait_for_ci is a boolean".into())),
        }
        if let Some(access) = access {
            let (sid, e, a) = (service.to_string(), env.to_string(), access.clone());
            self.db(move |s| s.set_access(&sid, &e, a.as_ref())).await?;
            self.set_access_cache(service, env, Some(access));
        }
        let mut new_urls = Vec::new();
        if let Some((beats, tokens)) = heartbeats {
            let (sid, e) = (service.to_string(), env.to_string());
            let fresh: Vec<String> = tokens.iter().map(|(n, _)| n.clone()).collect();
            let slug = self
                .db(move |s| {
                    s.set_heartbeats(&sid, &e, &beats)?;
                    // A new one (a name used before, an environment made again) starts with no ping.
                    for name in &fresh {
                        crate::heartbeats::forget(s, &sid, &e, name)?;
                    }
                    Ok(s.environment(&sid, &e)?.slug)
                })
                .await?;
            // Where each new heartbeat pings: shown this once (only its token's hash is kept).
            new_urls = tokens.into_iter().map(|(name, token)| (name, format!("{}{}{token}", self.url(&slug), crate::heartbeats::PATH))).collect();
            self.changed(service);
        }
        let mut view = self.environment(service, env).await?;
        view.heartbeat_urls.extend(new_urls);
        Ok(view)
    }

    /// Tear an environment down: instances, volume, domains, variables; deployments stay in
    /// history as `removed`.
    pub async fn delete_environment(self: &Arc<Self>, service: &str, env: &str) -> Result<()> {
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| s.environment(&sid, &e)).await?;
        for d in self.store.pending_deployments(service)?.into_iter().filter(|d| d.environment == env) {
            self.cancel(service, env, &d.id).await.ok();
        }
        // The row goes first: from here no request finds the environment, so nothing makes its
        // live state (or a container, or the volume) again behind the teardown.
        let (sid, e) = (service.to_string(), env.to_string());
        let domains = self.db(move |s| s.custom_domains_of(&sid, Some(&e))).await?;
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| s.delete_environment(&sid, &e)).await?;
        self.hosts_changed();
        self.release_domains(&domains).await;
        self.teardown_live(service, env).await;
        self.set_access_cache(service, env, None);
        // And whatever its live state didn't know (an earlier process's instances, a sandbox or
        // waker, a run): left on its network, the network and its relays would stay.
        for name in self.deps.runtime.list_env(service, env).await {
            self.deps.runtime.remove(&name).await;
        }
        // Its last backup before the volume goes (kept a week), its instances stopped first.
        let sid = service.to_string();
        let had = self.db(move |s| s.service(&sid)).await.is_ok_and(|s| s.run.volume.is_some());
        self.last_backup(service, env, had).await;
        self.deps.runtime.remove_volume(&volume_name(service, env)).await;
        let sid = service.to_string();
        if let Ok(svc) = self.db(move |s| s.service(&sid)).await {
            self.drop_network_if_unused(&svc.project, env, service).await;
        }
        self.logs.forget(&format!("env:{service}/{env}"));
        Ok(())
    }

    /// The project environment's network, once `leaving` was its last service there.
    async fn drop_network_if_unused(self: &Arc<Self>, project: &str, env: &str, leaving: &str) {
        let (p, e, l) = (project.to_string(), env.to_string(), leaving.to_string());
        if let Ok(false) = self.db(move |s| s.env_in_use(&p, &e, &l)).await {
            self.deps.runtime.remove_network(&network_name(project, env)).await;
            self.deps.runtime.remove_network(&closed_network_name(project, env)).await;
        }
    }

    /// A private environment's gate (none: public), cached on its owner.
    pub async fn access(self: &Arc<Self>, service: &str, env: &str) -> Result<Option<Access>> {
        if let Some(a) = self.access.lock().get(&env_key(service, env)) {
            return Ok(a.clone());
        }
        let seen = self.access_gen.load(std::sync::atomic::Ordering::SeqCst);
        let (sid, e) = (service.to_string(), env.to_string());
        let a = self.db(move |s| s.access(&sid, &e)).await?;
        let mut cache = self.access.lock();
        if self.access_gen.load(std::sync::atomic::Ordering::SeqCst) == seen {
            cache.insert(env_key(service, env), a.clone());
        }
        Ok(a)
    }

    /// Change the access cache after the store has the change: `Some` caches it, `None` forgets.
    pub(crate) fn set_access_cache(&self, service: &str, env: &str, access: Option<Option<Access>>) {
        let mut cache = self.access.lock();
        self.access_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match access {
            Some(a) => cache.insert(env_key(service, env), a),
            None => cache.remove(&env_key(service, env)),
        };
    }

    async fn teardown_live(&self, service: &str, env: &str) {
        self.cancel_jobs(service, env);
        self.responses.clear(&env_key(service, env));
        let live = self.lives.lock().remove(&env_key(service, env));
        if let Some(l) = live {
            l.shutdown().await;
        }
    }

    /// The live state of an environment on this node (made on first use).
    pub fn live(self: &Arc<Self>, service: &str, env: &str) -> Arc<EnvLive> {
        self.lives
            .lock()
            .entry(env_key(service, env))
            .or_insert_with(|| {
                let l = EnvLive::new(service, env, self.deps.runtime.clone(), self.cfg.live.clone(), self.logs.sink(format!("env:{service}/{env}"), Stream::System));
                l.set_capped(self.capped.lock().contains(service));
                l
            })
            .clone()
    }

    pub fn lives(&self) -> Vec<Arc<EnvLive>> {
        self.lives.lock().values().cloned().collect()
    }

    pub async fn sleep(self: &Arc<Self>, service: &str, env: &str) -> Result<Environment> {
        self.environment(service, env).await?;
        self.live(service, env).sleep().await;
        self.environment(service, env).await
    }

    pub async fn wake(self: &Arc<Self>, service: &str, env: &str) -> Result<Environment> {
        self.environment(service, env).await?;
        self.load_live(service, env).await?;
        let live = self.live(service, env);
        live.acquire().await.map_err(|e| Error::Conflict(e.to_string()))?;
        self.environment(service, env).await
    }

    /// `POST …/instances/{name}/restart`: just that one, replaced ([`EnvLive::restart_instance`]).
    pub async fn restart_instance(self: &Arc<Self>, service: &str, env: &str, name: &str) -> Result<Environment> {
        self.environment(service, env).await?;
        let live = self.load_live(service, env).await?;
        live.restart_instance(name).await.map_err(|e| if e.starts_with("no instance") { Error::NotFound(e) } else { Error::Conflict(e) })?;
        self.environment(service, env).await
    }

    pub async fn restart(self: &Arc<Self>, service: &str, env: &str) -> Result<Environment> {
        self.environment(service, env).await?;
        self.load_live(service, env).await?;
        self.live(service, env).restart().await;
        self.environment(service, env).await
    }

    /// Make sure this node's live state knows the environment's active deployment (after a
    /// restart or a takeover).
    pub async fn load_live(self: &Arc<Self>, service: &str, env: &str) -> Result<Arc<EnvLive>> {
        if let Some(live) = self.lives.lock().get(&env_key(service, env)).cloned()
            && live.template().is_some()
        {
            return Ok(live);
        }
        // Only an environment that exists gets live state.
        let (sid, e) = (service.to_string(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        // Only the service's owner has live state: a request that reached this node as the
        // service moved away mustn't take what the next owner kept (a runtime the nodes share),
        // nor start instances here that nothing will stop (one this node alone runs).
        if !self.owns(service) {
            return Err(Error::Unavailable("the service is moving to another node; try again in a moment".into()));
        }
        let live = self.live(service, env);
        // Paused before anything starts: an owner taking over keeps it paused.
        live.set_paused(row.paused);
        let Some(active) = row.active.clone() else { return Ok(live) };
        let dep = self.db(move |s| s.deployment(&active)).await?;
        let sid = service.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let tpl = self.template(&svc, &row, &dep).await?;
        live.adopt(Arc::new(tpl)).await;
        Ok(live)
    }

    // ── variables ──

    /// An instance's variables, `roll://` references read (a deploy).
    pub async fn resolve_vars(self: &Arc<Self>, svc: &ServiceRow, env: &EnvRow, deployment: &str) -> Result<ResolvedVars> {
        self.resolve_vars_as(svc, env, deployment, true).await
    }

    /// The same for a listing or a staleness check: a `roll://` reference is its text, never read
    /// (no secret decrypted, no read audited as the service for a person's page view).
    pub async fn listed_vars(self: &Arc<Self>, svc: &ServiceRow, env: &EnvRow, deployment: &str) -> Result<ResolvedVars> {
        self.resolve_vars_as(svc, env, deployment, false).await
    }

    async fn resolve_vars_as(self: &Arc<Self>, svc: &ServiceRow, env: &EnvRow, deployment: &str, read_refs: bool) -> Result<ResolvedVars> {
        let mut vars: BTreeMap<String, (String, &'static str)> = BTreeMap::new();
        let port = svc.run.port.unwrap_or(8080);
        let roll = [
            ("PORT", port.to_string()),
            ("ROLL_SERVICE", svc.name.clone()),
            ("ROLL_SERVICE_ID", svc.id.clone()),
            ("ROLL_PROJECT", svc.project.clone()),
            ("ROLL_ENVIRONMENT", env.name.clone()),
            ("ROLL_DEPLOYMENT", deployment.to_string()),
            ("ROLL_PUBLIC_URL", self.url(&env.slug)),
            ("ROLL_PRIVATE_HOST", format!("{}.{}.internal", svc.name, env.name)),
        ];
        for (k, v) in roll {
            vars.insert(k.to_string(), (v, "roll"));
        }
        // What a repo's deployment was built from (Railway's `RAILWAY_GIT_*`, Render's
        // `RENDER_GIT_*`): an error tracker's release, a log's version.
        if let (Source::Repo { repo, .. }, Some(sources)) = (&svc.source, self.deps.sources.as_ref()) {
            if !deployment.is_empty() {
                let d = deployment.to_string();
                if let Ok(Some(sha)) = self.db(move |s| s.deployment(&d)).await.map(|d| d.revision) {
                    vars.insert("ROLL_GIT_COMMIT".into(), (sha, "roll"));
                }
            }
            let branch = match &env.branch {
                Some(b) => Some(b.clone()),
                None => sources.default_branch(repo).await.ok(),
            };
            if let Some(b) = branch {
                vars.insert("ROLL_GIT_BRANCH".into(), (b, "roll"));
            }
        }
        // A Git URL's: its environment's branch was resolved when it was made.
        if let Source::Git { url, .. } = &svc.source {
            if !deployment.is_empty() {
                let d = deployment.to_string();
                if let Ok(Some(sha)) = self.db(move |s| s.deployment(&d)).await.map(|d| d.revision) {
                    vars.insert("ROLL_GIT_COMMIT".into(), (sha, "roll"));
                }
            }
            if let Some(b) = &env.branch {
                vars.insert("ROLL_GIT_BRANCH".into(), (b.clone(), "roll"));
            }
            vars.insert("ROLL_GIT_URL".into(), (url.clone(), "roll"));
        }
        // Traces turned on: the OpenTelemetry SDK's own variables, to this environment's address.
        let mut trace_endpoint = false;
        if let Some(base) = &self.cfg.otlp_base {
            let (sid, e) = (svc.id.clone(), env.name.clone());
            if let Some(token) = self.db(move |s| s.trace_token(&sid, &e)).await? {
                let url = format!("{}{}{}/{}/{token}", base.trim_end_matches('/'), crate::public::OTLP, svc.id, env.name);
                vars.insert("OTEL_EXPORTER_OTLP_ENDPOINT".into(), (self.deps.runtime.reach(&url), "roll"));
                vars.insert("OTEL_EXPORTER_OTLP_PROTOCOL".into(), ("http/protobuf".into(), "roll"));
                vars.insert("OTEL_TRACES_EXPORTER".into(), ("otlp".into(), "roll"));
                // Traces and metrics go to roll; logs only when the app says so
                // (`OTEL_LOGS_EXPORTER=otlp` of its own): most apps also print them, and the
                // environment's log would hold each twice.
                vars.insert("OTEL_METRICS_EXPORTER".into(), ("otlp".into(), "roll"));
                vars.insert("OTEL_LOGS_EXPORTER".into(), ("none".into(), "roll"));
                vars.insert("OTEL_SERVICE_NAME".into(), (svc.name.clone(), "roll"));
                // Its token is a credential: never shown, as a sealed variable.
                trace_endpoint = true;
            }
        }
        // Runtime config: where its instances read it (a token URL on this listener, as traces'
        // intake), when the environment has one; sealed, as a credential.
        let mut config_url = false;
        if let Some(base) = &self.cfg.otlp_base {
            let (sid, e) = (svc.id.clone(), env.name.clone());
            if let Some(token) = self.db(move |s| s.runtime_config_token(&sid, &e)).await? {
                let url = format!("{}{}{}/{}/{token}", base.trim_end_matches('/'), crate::public::CONFIG, svc.id, env.name);
                vars.insert("ROLL_CONFIG_URL".into(), (self.deps.runtime.reach(&url), "roll"));
                config_url = true;
            }
        }
        // A site's functions service: where its edge functions' `context.next()` reaches the
        // site (this listener, as instances reach it; `crate::edge`), set at start so the runtime
        // makes it reachable.
        if svc.build.builder == crate::model::Builder::Functions
            && let Some(base) = &self.cfg.otlp_base
        {
            vars.insert("ROLL_EDGE_BASE".into(), (self.deps.runtime.reach(base.trim_end_matches('/')), "roll"));
            // Its site's runtime config, as `context.config` (the site is the project's service
            // whose functions service this is, `<site>-fn`).
            let (p, me) = (svc.project.clone(), svc.name.clone());
            let site = self.db(move |s| s.services(Some(&p))).await?.into_iter().find(|s| s.build.builder != crate::model::Builder::Functions && format!("{}-fn", s.name).chars().take(40).collect::<String>() == me);
            if let Some(site) = site {
                let (sid, e) = (site.id.clone(), env.name.clone());
                if let Some(token) = self.db(move |s| s.runtime_config_token(&sid, &e)).await? {
                    let url = format!("{}{}{}/{}/{token}", base.trim_end_matches('/'), crate::public::CONFIG, site.id, env.name);
                    vars.insert("ROLL_CONFIG_URL".into(), (self.deps.runtime.reach(&url), "roll"));
                    config_url = true;
                }
            }
        }
        let sid = svc.id.clone();
        let links = self.db(move |s| s.links_minted(&sid)).await?;
        for (kind, r, prefix, minted) in links {
            // A preview (an ephemeral environment) never gets a resource's production variables:
            // roll previews sets the branched ones itself, by name; another service's URLs stay.
            if env.ephemeral && kind != LinkKind::Service {
                continue;
            }
            let read = self.link_values(svc, kind, &r, &env.name, minted).await;
            match read {
                Ok(vs) => {
                    for (k, v) in vs {
                        // Another service's names are the project network's: nothing to rewrite.
                        let v = if kind == LinkKind::Service { v } else { self.deps.runtime.reach(&v) };
                        vars.insert(format!("{prefix}{k}"), (v, "link"));
                    }
                }
                Err(e) => tracing::warn!(service = %svc.id, kind = kind.as_str(), r#ref = %r, error = %e, "apps: a link's variables couldn't be read"),
            }
        }
        let mut sensitive = BTreeSet::new();
        let (mut from_secrets, mut plain_secrets) = (BTreeSet::new(), BTreeSet::new());
        match self.deps.secrets.resolve(&svc.project, &env.name, &svc.id).await {
            Ok(secrets) => {
                for s in secrets {
                    if s.sensitive {
                        sensitive.insert(s.name.clone());
                    }
                    from_secrets.insert(s.name.clone());
                    if s.plain {
                        plain_secrets.insert(s.name.clone());
                    }
                    // A linked resource's address, rewritten for the container as apps' own links.
                    let v = if s.linked { self.deps.runtime.reach(&s.value) } else { s.value };
                    vars.insert(s.name, (v, "secret"));
                }
            }
            Err(e) => return Err(Error::Internal(format!("the project's {} secrets couldn't be read: {e}", env.name))),
        }
        let (sid, e) = (svc.id.clone(), env.name.clone());
        let (own, mut sealed) = self.db(move |s| Ok((s.variables(&sid, &e)?, s.sealed_variables(&sid, &e)?))).await?;
        if trace_endpoint {
            sealed.insert("OTEL_EXPORTER_OTLP_ENDPOINT".into());
        }
        if config_url {
            sealed.insert("ROLL_CONFIG_URL".into());
        }
        // Where its instances read roll's OIDC tokens (`run.oidc_audiences`): a token URL on this
        // listener, sealed, as runtime config's; the first audience said too.
        if let (Some(base), Some(first)) = (&self.cfg.otlp_base, svc.run.oidc_audiences.first()) {
            let (sid, e) = (svc.id.clone(), env.name.clone());
            let token = self.db(move |s| s.oidc_token(&sid, &e)).await?;
            let url = format!("{}{}{}/{}/{token}", base.trim_end_matches('/'), crate::public::OIDC, svc.id, env.name);
            vars.insert("ROLL_OIDC_URL".into(), (self.deps.runtime.reach(&url), "roll"));
            vars.insert("ROLL_OIDC_AUDIENCE".into(), (first.clone(), "roll"));
            sealed.insert("ROLL_OIDC_URL".into());
        }
        for (k, v) in own {
            vars.insert(k, (v, "service"));
        }
        // `roll://` references to its own environment's secrets, read as the service (audited
        // there), sealed: a value from roll secrets is shown only as they reveal it. One that
        // doesn't resolve fails the deploy; a preview's (copied from its base, naming the base's
        // environment) is left out, as an untrusted preview's secrets are.
        let mut refused = Vec::new();
        let refs: Vec<(String, String)> = vars.iter().filter(|(_, (v, s))| *s == "service" && crate::model::secret_ref_env(v).is_some()).map(|(k, (v, _))| (k.clone(), v.clone())).collect();
        let mut ref_texts = BTreeMap::new();
        for (k, r) in refs {
            ref_texts.insert(k.clone(), r.clone());
            if !read_refs {
                // Left out of a preview as a deploy leaves it out, so both hash the same.
                let ours = matches!(crate::model::secret_ref_env(&r), Some(Ok(e)) if e == env.name) && crate::model::secret_ref_folder_ok(&r, &svc.id);
                if env.ephemeral && !ours {
                    vars.remove(&k);
                    continue;
                }
                vars.insert(k.clone(), (r, "secret"));
                from_secrets.insert(k.clone());
                sealed.insert(k);
                continue;
            }
            let read = match crate::model::secret_ref_env(&r) {
                Some(Ok(e)) if e == env.name && !crate::model::secret_ref_folder_ok(&r, &svc.id) => Err("it names another service's folder".to_string()),
                Some(Ok(e)) if e == env.name => self.deps.secrets.resolve_ref(&svc.project, &env.name, &svc.id, &r).await.map_err(|e| e.to_string()),
                Some(Ok(e)) => Err(format!("it names the {e} environment's secret, not {}'s", env.name)),
                Some(Err(e)) => Err(e.to_string()),
                None => continue,
            };
            match read {
                Ok(v) => {
                    vars.insert(k.clone(), (v, "secret"));
                    from_secrets.insert(k.clone());
                    sealed.insert(k);
                }
                Err(why) if env.ephemeral => {
                    tracing::info!(service = %svc.id, env = %env.name, variable = %k, "apps: a preview's roll:// reference is left out: {why}");
                    vars.remove(&k);
                }
                Err(why) => {
                    vars.remove(&k);
                    refused.push(format!("{k} ({r}): {why}"));
                }
            }
        }
        // A sensitive secret is as a sealed variable (unless the service's own takes its name).
        sealed.extend(sensitive.into_iter().filter(|k| vars.get(k).is_some_and(|(_, s)| *s == "secret")));
        // Reference variables, one level deep: against what resolved above, and other services'.
        let unresolved = self.fill_references(svc, &env.name, &mut vars, &mut sealed).await;
        let sources = vars.iter().map(|(k, (_, s))| (k.clone(), *s)).collect();
        Ok(ResolvedVars { vars: vars.into_iter().map(|(k, (v, _))| (k, v)).collect(), sources, unresolved, refused, ref_texts, sealed, from_secrets, plain_secrets })
    }

    /// Fill the `${{ … }}` in the service's own variables ([`refs`]); what named nothing. A
    /// reference that reads a sealed value is sealed too (`sealed`): it would show it.
    async fn fill_references(self: &Arc<Self>, svc: &ServiceRow, env: &str, vars: &mut BTreeMap<String, (String, &'static str)>, sealed: &mut BTreeSet<String>) -> Vec<String> {
        let referring: Vec<String> = vars.iter().filter(|(_, (v, s))| *s == "service" && v.contains("${{")).map(|(k, _)| k.clone()).collect();
        if referring.is_empty() {
            return Vec::new();
        }
        let own: HashMap<String, String> = vars.iter().map(|(k, (v, _))| (k.clone(), v.clone())).collect();
        // Another service's: by name in this project, the same environment.
        let mut others: HashMap<String, Option<ServiceValues>> = HashMap::new();
        for k in &referring {
            for (r, _) in refs::refs(&vars[k].0) {
                if let Some(name) = r.service.filter(|n| !others.contains_key(*n) && *n != svc.name) {
                    let found = self.service_values(&svc.project, name, env).await;
                    others.insert(name.to_string(), found);
                }
            }
        }
        let mut unresolved = Vec::new();
        let own_sealed = sealed.clone();
        for k in referring {
            let mut reads_sealed = false;
            let (filled, missing) = refs::fill(&vars[&k].0, |r| match r.service {
                None => own.get(r.var).cloned().inspect(|_| reads_sealed |= own_sealed.contains(r.var)),
                Some(n) if n == svc.name => own.get(r.var).cloned().inspect(|_| reads_sealed |= own_sealed.contains(r.var)),
                Some(n) => others.get(n).and_then(|o| o.as_ref()).and_then(|(o, s)| o.get(r.var).cloned().inspect(|_| reads_sealed |= s.contains(r.var))),
            });
            unresolved.extend(missing.into_iter().map(|m| format!("{k}: {m}")));
            if reads_sealed {
                sealed.insert(k.clone());
            }
            vars.insert(k, (filled, "reference"));
        }
        unresolved
    }

    /// What another service of `project` offers a reference in `env`: its own variables, and
    /// `PORT`, `ROLL_PRIVATE_HOST` and `ROLL_PUBLIC_URL`, and which of them are sealed. None: no
    /// such service or environment.
    async fn service_values(self: &Arc<Self>, project: &str, name: &str, env: &str) -> Option<ServiceValues> {
        let (p, n, e) = (project.to_string(), name.to_string(), env.to_string());
        let (svc, row, own, sealed) = self
            .db(move |s| {
                let Some(svc) = s.services(Some(&p))?.into_iter().find(|x| x.name == n) else { return Ok(None) };
                let row = s.environment(&svc.id, &e)?;
                let own = s.variables(&svc.id, &e)?;
                let sealed = s.sealed_variables(&svc.id, &e)?;
                Ok(Some((svc, row, own, sealed)))
            })
            .await
            .ok()
            .flatten()?;
        let mut out: HashMap<String, String> = own.into_iter().collect();
        let port = match row.active.clone() {
            Some(a) => self.db(move |s| s.deployment(&a)).await.ok().and_then(|d| d.run.port),
            None => None,
        };
        out.insert("PORT".into(), port.or(svc.run.port).unwrap_or(8080).to_string());
        out.insert("ROLL_PRIVATE_HOST".into(), format!("{}.{env}.internal", svc.name));
        if svc.run.public {
            out.insert("ROLL_PUBLIC_URL".into(), self.url(&row.slug));
        }
        Some((out, sealed))
    }

    /// Before values with references are stored: every other service they name is the
    /// project's, and the caller may see its values (a token's `reveal` on it, as reading them
    /// would need): a reference is a way to read them.
    pub async fn check_references(self: &Arc<Self>, project: &str, own: &str, env: &str, values: &[&str], who: integrations::Asker<'_>) -> Result<()> {
        let mut names: Vec<&str> = values.iter().flat_map(|v| refs::refs(v)).filter_map(|(r, _)| r.service).filter(|n| *n != own).collect();
        names.sort();
        names.dedup();
        for name in names {
            let (p, n) = (project.to_string(), name.to_string());
            let Some(target) = self.db(move |s| Ok(s.services(Some(&p))?.into_iter().find(|x| x.name == n))).await? else {
                return Err(Error::BadRequest(format!("${{{{ {name}.… }}}} names no service of this project")));
            };
            // A grant decides whenever there is one: rolld's gate puts the operator marker beside
            // an operator's token, narrowed by its rules, so the marker alone would read as all.
            if let Some(g) = who.grant {
                let resource = roll_auth::scope::Resource { product: "apps".into(), id: Some(target.id.clone()), project: Some(project.to_string()), environment: Some(env.to_string()), ..Default::default() };
                if g.allows(&resource, roll_auth::scope::Action::Reveal) != roll_auth::scope::Outcome::Allow {
                    return Err(Error::Forbidden(format!("a reference to {name}'s variables needs reveal on {name}")));
                }
            }
        }
        Ok(())
    }

    /// What reads a job's output or opens a shell needs when the environment has values from
    /// roll secrets (a job may print them, a shell reads them all): roll secrets' own rule for
    /// The web service named `name` in `service`'s project (a site's `service:` rewrite), looked
    /// up at most every 30 s (a miss every 2 s).
    pub(crate) async fn project_service(self: &Arc<Self>, service: &str, name: &str) -> Option<String> {
        let key = (service.to_string(), name.to_string());
        // A miss a moment only: a service made just now answers at once.
        if let Some((at, id)) = self.service_names.lock().get(&key)
            && at.elapsed() < Duration::from_secs(if id.is_some() { 30 } else { 2 })
        {
            return id.clone();
        }
        let (sid, n) = key.clone();
        let id = self
            .db(move |s| {
                let svc = s.service(&sid)?;
                Ok(s.services(Some(&svc.project))?.into_iter().find(|x| x.name == n && x.run.kind == RunType::Web).map(|x| x.id))
            })
            .await
            .ok()
            .flatten();
        let mut m = self.service_names.lock();
        if m.len() > 10_000 {
            m.clear();
        }
        m.insert(key, (std::time::Instant::now(), id.clone()));
        id
    }

    /// revealing them.
    pub async fn check_secret_reveal(self: &Arc<Self>, service: &str, env: &str, who: integrations::Asker<'_>) -> Result<()> {
        let (svc, row) = self.service_env(service, env).await?;
        let r = self.listed_vars(&svc, &row, row.active.as_deref().unwrap_or("")).await?;
        if r.sources.values().any(|s| *s == "secret") && !self.deps.secrets.may_reveal(who, &svc.project, env).await {
            return Err(Error::Forbidden(format!("this environment's values come from roll secrets, which you may not reveal in {env} (a shell or a job's output shows them)")));
        }
        Ok(())
    }

    /// The environment's variables: masked, or (`reveal`: who asks) in the clear. A value from
    /// roll secrets is shown only to whom roll secrets would show it; otherwise the reveal is a
    /// 403 naming them, never a partial answer.
    pub async fn variables(self: &Arc<Self>, service: &str, env: &str, reveal: Option<integrations::Asker<'_>>) -> Result<Value> {
        let (svc, row) = self.service_env(service, env).await?;
        let r = self.listed_vars(&svc, &row, row.active.as_deref().unwrap_or("")).await?;
        if let Some(who) = reveal {
            // Sensitive ones are left out for everyone, so they need no one's leave.
            let secret: Vec<&String> = r.sources.iter().filter(|(k, s)| **s == "secret" && !r.sealed.contains(*k)).map(|(k, _)| k).collect();
            if !secret.is_empty() && !self.deps.secrets.may_reveal(who, &svc.project, env).await {
                return Err(Error::Forbidden(format!("{} come from roll secrets, which you may not reveal in {env}", secret.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", "))));
            }
            // A sealed one is never in the clear, to anyone.
            let m: serde_json::Map<String, Value> = r.vars.into_iter().filter(|(k, _)| !r.sealed.contains(k)).map(|(k, v)| (k, Value::String(v))).collect();
            return Ok(Value::Object(m));
        }
        // A reference's template is names, not a value: shown, and what an edit starts from.
        let (sid, e) = (service.to_string(), env.to_string());
        let (raw, files) = self.db(move |s| Ok((s.variables(&sid, &e)?, s.variable_files(&sid, &e)?))).await?;
        let vars: Vec<VarView> = r
            .vars
            .iter()
            .map(|(k, v)| {
                let source = r.sources.get(k).copied().unwrap_or("service");
                // roll's own values aren't secret.
                let sealed = r.sealed.contains(k);
                let masked = if sealed { SEALED_MASK.to_string() } else if source == "roll" { v.clone() } else { mask(v) };
                let reference = (source == "reference").then(|| raw.get(k).cloned()).flatten();
                VarView { name: k.clone(), masked, source, reference, sealed, file: files.get(k).cloned() }
            })
            .collect();
        Ok(json!({ "vars": vars }))
    }

    pub async fn change_variables(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Value> {
        self.service_env(service, env).await?;
        let o = body.as_object().ok_or_else(|| Error::BadRequest("the body is {set?, unset?}".into()))?;
        let mut set = BTreeMap::new();
        if let Some(s) = o.get("set") {
            for (k, v) in s.as_object().ok_or_else(|| Error::BadRequest("set is an object of names to values".into()))? {
                check_var_name(k)?;
                let v = v.as_str().ok_or_else(|| Error::BadRequest(format!("{k}'s value is a string")))?;
                if v.len() > 64 << 10 || v.contains('\0') {
                    return Err(Error::BadRequest(format!("{k}'s value is at most 64 KiB, without NUL")));
                }
                // A roll secrets reference: its own environment's (another's needs whoever set
                // it bound to it, roll secrets' maker references: not here yet).
                match crate::model::secret_ref_env(v) {
                    Some(Ok(e)) if e != env => {
                        return Err(Error::BadRequest(format!("{k}: {v} is the {e} environment's secret; a variable's roll:// reference names its own environment (roll://{env}/…)")));
                    }
                    Some(Ok(_)) if !crate::model::secret_ref_folder_ok(v, service) => {
                        return Err(Error::BadRequest(format!("{k}: {v} is another service's folder (/services/<id> is each service's own)")));
                    }
                    Some(Err(e)) => return Err(Error::BadRequest(format!("{k}: {e}"))),
                    _ => {}
                }
                set.insert(k.clone(), v.to_string());
            }
        }
        let mut unset = Vec::new();
        if let Some(u) = o.get("unset") {
            for n in u.as_array().ok_or_else(|| Error::BadRequest("unset is a list of names".into()))? {
                unset.push(n.as_str().ok_or_else(|| Error::BadRequest("unset is a list of names".into()))?.to_string());
            }
        }
        // Seal: set and run with, never shown again (Railway's sealed variables).
        let mut seal = BTreeSet::new();
        if let Some(n) = o.get("seal") {
            for n in n.as_array().ok_or_else(|| Error::BadRequest("seal is a list of names".into()))? {
                seal.insert(n.as_str().ok_or_else(|| Error::BadRequest("seal is a list of names".into()))?.to_string());
            }
        }
        // Kept as files (Render's secret files): a name to an absolute path, or null for the
        // environment again.
        let mut files: Vec<(String, Option<String>)> = Vec::new();
        if let Some(f) = o.get("files") {
            for (k, p) in f.as_object().ok_or_else(|| Error::BadRequest("files is an object of names to paths (or null)".into()))? {
                files.push((k.clone(), match p {
                    Value::Null => None,
                    Value::String(p) => Some(check_file_path(p)?),
                    _ => return Err(Error::BadRequest(format!("files.{k} is an absolute path, or null"))),
                }));
            }
        }
        if set.len() + unset.len() + seal.len() + files.len() > 500 {
            return Err(Error::BadRequest("at most 500 variables at once".into()));
        }
        let (sid, e) = (service.to_string(), env.to_string());
        let (have, mut kept) = self.db(move |s| Ok((s.variables(&sid, &e)?, s.variable_files(&sid, &e)?))).await?;
        if let Some(n) = seal.iter().find(|n| unset.contains(n) || !(set.contains_key(*n) || have.contains_key(*n))) {
            return Err(Error::BadRequest(format!("{n} isn't one of the environment's own variables to seal")));
        }
        if let Some((n, _)) = files.iter().find(|(n, p)| p.is_some() && (unset.contains(n) || !(set.contains_key(n) || have.contains_key(n)))) {
            return Err(Error::BadRequest(format!("{n} isn't one of the environment's own variables to keep as a file")));
        }
        for (n, p) in &files {
            match p {
                Some(p) => kept.insert(n.clone(), p.clone()),
                None => kept.remove(n),
            };
        }
        for n in &unset {
            kept.remove(n);
        }
        if kept.len() > MAX_FILES {
            return Err(Error::BadRequest(format!("at most {MAX_FILES} variables as files")));
        }
        let mut paths: Vec<&String> = kept.values().collect();
        paths.sort();
        if paths.windows(2).any(|w| w[0] == w[1]) {
            return Err(Error::BadRequest("two variables can't be the same file".into()));
        }
        let (sid, e, gone) = (service.to_string(), env.to_string(), unset.clone());
        self.db(move |s| s.change_variables(&sid, &e, &set, &unset, &seal)).await?;
        // Unset ones' files go with them.
        for (n, p) in files.into_iter().chain(gone.into_iter().map(|n| (n, None))) {
            let (sid, e) = (service.to_string(), env.to_string());
            self.db(move |s| s.set_variable_file(&sid, &e, &n, p.as_deref())).await?;
        }
        self.variables(service, env, None).await
    }

    async fn service_env(self: &Arc<Self>, service: &str, env: &str) -> Result<(ServiceRow, EnvRow)> {
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await
    }

    // ── links ──

    pub async fn links(self: &Arc<Self>, service: &str) -> Result<Vec<Link>> {
        let sid = service.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let sid = service.to_string();
        let rows = self.db(move |s| s.links_minted(&sid)).await?;
        let mut out = Vec::new();
        for (kind, r, prefix, minted) in rows {
            let names = self.link_values(&svc, kind, &r, "production", minted).await;
            let vars = names.map(|v| v.into_iter().map(|(k, _)| format!("{prefix}{k}")).collect()).unwrap_or_default();
            out.push(Link { kind, r#ref: r, prefix, vars });
        }
        Ok(out)
    }

    pub async fn put_link(self: &Arc<Self>, service: &str, kind: &str, r#ref: &str, body: Value) -> Result<Link> {
        let kind = LinkKind::parse(kind)?;
        let sid = service.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        // A service's URLs are named for it unless asked otherwise (`API_URL`): a bare `URL`
        // says nothing.
        let named = || {
            let rr = r#ref.to_string();
            async move {
                let missing = format!("no service {rr} in this project");
                let target = self.db(move |s| s.service(&rr)).await.map_err(|_| Error::BadRequest(missing))?;
                let mut p: String = target.name.to_ascii_uppercase().replace('-', "_");
                if p.starts_with(|c: char| c.is_ascii_digit()) {
                    p.insert_str(0, "SERVICE_");
                }
                Ok::<String, Error>(format!("{p}_"))
            }
        };
        let prefix = match body.get("prefix") {
            None | Some(Value::Null) if kind == LinkKind::Service => named().await?,
            Some(Value::String(p)) if p.is_empty() && kind == LinkKind::Service => named().await?,
            None | Some(Value::Null) => String::new(),
            Some(Value::String(p)) if p.is_empty() => String::new(),
            Some(Value::String(p)) => {
                check_var_name(p)?;
                if p.ends_with('_') { p.clone() } else { format!("{p}_") }
            }
            Some(_) => return Err(Error::BadRequest("prefix is a variable name prefix".into())),
        };
        // Credentials the link makes for itself, once: a link that's there already keeps its own
        // (setting its prefix again never makes another).
        let (sid, r) = (service.to_string(), r#ref.to_string());
        let had = self.db(move |s| s.links_minted(&sid)).await?.into_iter().find(|(k, rr, _, _)| *k == kind && *rr == r).and_then(|(_, _, _, m)| m);
        let fresh = had.is_none();
        let minted = match had {
            Some(m) => Some(m),
            None => self.deps.resources.link(kind, r#ref, service).await.map_err(|e| Error::BadRequest(e.to_string()))?,
        };
        let vars = self.link_values(&svc, kind, r#ref, "production", minted.clone()).await.map_err(|e| Error::BadRequest(e.to_string()))?;
        let (sid, r, p, m) = (service.to_string(), r#ref.to_string(), prefix.clone(), minted.clone());
        let vars = match self.db(move |s| s.put_link(&sid, kind, &r, &p, m.as_ref())).await {
            Ok(true) => vars,
            // Another request made the link first, with credentials of its own: those stay,
            // and what this one made goes again.
            Ok(false) => {
                if let Some(m) = minted.as_ref().filter(|_| fresh) {
                    self.revoke_link(service, kind, r#ref, &m.kept).await;
                }
                let (sid, r) = (service.to_string(), r#ref.to_string());
                self.db(move |s| s.links_minted(&sid)).await?.into_iter().find(|(k, rr, _, _)| *k == kind && *rr == r).and_then(|(_, _, _, m)| m).map_or(vars, |m| m.vars)
            }
            Err(e) => {
                // Not kept: what was just made for it goes again.
                if let Some(m) = minted.as_ref().filter(|_| fresh) {
                    let _ = self.deps.resources.unlink(kind, r#ref, &m.kept).await;
                }
                return Err(e);
            }
        };
        Ok(Link { kind, r#ref: r#ref.to_string(), vars: vars.into_iter().map(|(k, _)| format!("{prefix}{k}")).collect(), prefix })
    }

    /// A link's variables now, as `service` gets them in `env`: another service's URLs (apps'
    /// own), else the resource's through [`crate::integrations::Resources`].
    /// What a link sets now: the resource's variables read again (an endpoint, a login that was
    /// rotated), with the credentials the link made for itself over them. A minted link whose
    /// resource can't be read now keeps what it made.
    async fn link_values(self: &Arc<Self>, service: &ServiceRow, kind: LinkKind, r#ref: &str, env: &str, minted: Option<integrations::Minted>) -> anyhow::Result<Vec<(String, String)>> {
        let Some(m) = minted else { return self.link_vars(service, kind, r#ref, env).await };
        let mut out = self.link_vars(service, kind, r#ref, env).await.unwrap_or_default();
        for (k, v) in m.vars {
            match out.iter_mut().find(|(have, _)| *have == k) {
                Some(slot) => slot.1 = v,
                None => out.push((k, v)),
            }
        }
        Ok(out)
    }

    async fn link_vars(self: &Arc<Self>, service: &ServiceRow, kind: LinkKind, r#ref: &str, env: &str) -> anyhow::Result<Vec<(String, String)>> {
        if kind != LinkKind::Service {
            return self.deps.resources.link_vars(kind, r#ref).await;
        }
        let (rr, e) = (r#ref.to_string(), env.to_string());
        let (target, row) = self.db(move |s| Ok((s.service(&rr)?, s.environment(&rr, &e).ok()))).await.map_err(|_| anyhow::anyhow!("no service {ref} in this project"))?;
        // Checked at each read: a service moved to another project stops being reachable.
        anyhow::ensure!(target.project == service.project && target.id != service.id, "no service {ref} in this project");
        anyhow::ensure!(target.run.kind == RunType::Web, "{} isn't a web service: only a web service has URLs", target.name);
        // The port its live deployment listens on (a Dockerfile's EXPOSE, an environment's own).
        let active = row.as_ref().and_then(|r| r.active.clone());
        let port = match active {
            Some(a) => self.db(move |s| s.deployment(&a)).await.ok().and_then(|d| d.run.port),
            None => None,
        };
        let port = port.or(target.run.port).unwrap_or(8080);
        let mut v = vec![("URL".to_string(), format!("http://{}.{env}.internal:{port}", target.name))];
        if let Some(row) = row.filter(|_| target.run.public) {
            v.push(("PUBLIC_URL".to_string(), self.url(&row.slug)));
        }
        Ok(v)
    }

    pub async fn delete_link(self: &Arc<Self>, service: &str, kind: &str, r#ref: &str) -> Result<()> {
        let kind = LinkKind::parse(kind)?;
        let sid = service.to_string();
        let minted = self.db(move |s| s.links_minted(&sid)).await?.into_iter().find(|(k, rr, _, _)| *k == kind && rr == r#ref).and_then(|(_, _, _, m)| m);
        let (sid, r) = (service.to_string(), r#ref.to_string());
        self.db(move |s| s.delete_link(&sid, kind, &r)).await?;
        if let Some(m) = minted {
            self.revoke_link(service, kind, r#ref, &m.kept).await;
        }
        Ok(())
    }

    /// Revoke a link's own credentials (a failure is in the log: the link is gone either way).
    async fn revoke_link(self: &Arc<Self>, service: &str, kind: LinkKind, r#ref: &str, kept: &str) {
        if let Err(e) = self.deps.resources.unlink(kind, r#ref, kept).await {
            tracing::warn!(service, kind = kind.as_str(), r#ref, error = %e, "apps: a link's credentials couldn't be revoked");
        }
    }

    // ── domains ──

    fn target(&self, slug: &str) -> String {
        self.cfg.cname_target.clone().unwrap_or_else(|| format!("{slug}.{}", self.cfg.domain))
    }

    /// The records a custom domain needs. A CNAME to an environment's own target proves the
    /// hostname is its owner's; a shared target (`cname_target`, Cloudflare for SaaS) is every
    /// tenant's, so there the TXT challenge proves it and the CNAME only routes ("both").
    fn record(&self, host: &str) -> &'static str {
        match record_for(host) {
            "cname" if self.cfg.cname_target.is_some() => "both",
            r => r,
        }
    }

    pub async fn domains(self: &Arc<Self>, service: &str, env: &str) -> Result<Vec<Domain>> {
        let (sid, e) = (service.to_string(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        let (sid, e) = (service.to_string(), env.to_string());
        let mut custom = self.db(move |s| s.domains(&sid, &e)).await?;
        let target = self.target(&row.slug);
        let issues = self.deps.tls.issues();
        for d in &mut custom {
            d.target = target.clone();
            d.record = self.record(&d.hostname);
            if !issues {
                (d.tls, d.tls_error) = (TlsStatus::None, None);
            }
        }
        let own = Domain {
            hostname: format!("{}.{}", row.slug, self.cfg.domain),
            record: "none",
            kind: DomainKind::Roll,
            status: DomainStatus::Active,
            target: String::new(),
            token: String::new(),
            error: None,
            // The edge's wildcard covers it.
            tls: if self.cfg.scheme == "https" { TlsStatus::Issued } else { TlsStatus::None },
            tls_error: None,
            created_at: row.created_at,
            redirect_to: None,
            port: None,
        };
        Ok(std::iter::once(own).chain(custom).collect())
    }

    pub async fn add_domain(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Domain> {
        // What a hostname names is about to change: no host answer kept from before.
        self.hosts_changed();
        let host = body.get("hostname").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("hostname is required".into()))?;
        let mut reserved: Vec<&str> = self.cfg.reserved.iter().map(String::as_str).collect();
        reserved.push(&self.cfg.domain);
        let host = check_hostname(host, &reserved)?;
        if host.starts_with("*.") && self.deps.tls.issues() && !self.deps.tls.wildcards() {
            return Err(Error::BadRequest(format!("{host}: a wildcard's certificate needs DNS-01, which this node's certificates don't do yet; add the names it would cover one by one")));
        }
        let (sid, e) = (service.to_string(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        let tls = if self.deps.tls.issues() { TlsStatus::Pending } else { TlsStatus::None };
        let (sid, e, h) = (service.to_string(), env.to_string(), host.clone());
        let mut d = self.db(move |s| s.add_domain(&sid, &e, &h, tls)).await?;
        self.hosts_changed();
        // Only now, accepted and claimed, does the hostname go to the certificate's provider.
        if let Some((tls, tls_error)) = self.refresh_tls(&host).await {
            (d.tls, d.tls_error) = (tls, tls_error);
        }
        d.target = self.target(&row.slug);
        d.record = self.record(&d.hostname);
        Ok(d)
    }

    /// Ask the certificate's provider where a custom domain's certificate is, and keep the answer.
    /// `None`: the provider doesn't say (the edge reports on its own) or nothing issues.
    async fn refresh_tls(self: &Arc<Self>, host: &str) -> Option<(TlsStatus, Option<String>)> {
        if !self.deps.tls.issues() {
            return None;
        }
        let (tls, error) = match self.deps.tls.ensure(host).await {
            Ok(Some(state)) => state,
            Ok(None) => return None,
            // The provider didn't answer: still pending, saying why; checked again later.
            Err(e) => (TlsStatus::Pending, Some(format!("{e:#}"))),
        };
        let (h, err) = (host.to_string(), error.clone());
        self.db(move |s| s.set_domain_tls(&h, tls, err.as_deref())).await.ok()?;
        Some((tls, error))
    }

    /// A custom domain's certificate state, from whatever issues it (the edge's reports).
    pub async fn set_domain_tls(self: &Arc<Self>, host: &str, tls: TlsStatus, error: Option<String>) -> Result<bool> {
        let h = host.to_ascii_lowercase();
        self.db(move |s| s.set_domain_tls(&h, tls, error.as_deref())).await
    }

    /// Every active custom domain's hostname (what the edge holds certificates for).
    pub async fn active_custom_domains(self: &Arc<Self>) -> Result<Vec<String>> {
        self.db(|s| s.active_custom_domains()).await
    }

    /// Check a custom domain's DNS now (a CNAME to its target, or the TXT challenge), and where
    /// its certificate is.
    pub async fn verify_domain(self: &Arc<Self>, service: &str, env: &str, host: &str) -> Result<Domain> {
        // What a hostname names is about to change: no host answer kept from before.
        self.hosts_changed();
        let h = host.to_ascii_lowercase();
        let (sid, e) = (service.to_string(), env.to_string());
        let row = self.db(move |s| s.environment(&sid, &e)).await?;
        let found = {
            let h = h.clone();
            self.db(move |s| s.domain_owner(&h)).await?
        };
        let Some((_, _, mut d)) = found.filter(|(s, e, _)| s == service && e == env) else {
            return Err(Error::NotFound(format!("{h} isn't a domain of {service} in {env}")));
        };
        let target = self.target(&row.slug);
        let cnames = self.deps.resolver.cname(&h).await;
        let cname_ok = cnames.contains(&target);
        let challenge = crate::model::challenge_name(&h);
        let txts = self.deps.resolver.txt(&challenge).await;
        let txt_ok = txts.contains(&d.token);
        let shared = self.cfg.cname_target.is_some();
        let (status, error) = if txt_ok || (cname_ok && !shared) {
            (DomainStatus::Active, None)
        } else {
            // What DNS answered instead, and the likely mistake in it.
            let seen = crate::model::DnsSeen {
                cnames,
                txts,
                host_txts: self.deps.resolver.txt(&h).await,
                addresses: if cname_ok { Vec::new() } else { self.deps.resolver.addresses(&h).await },
            };
            (DomainStatus::Pending, Some(crate::model::domain_diagnosis(&h, &target, &challenge, &d.token, &seen, shared)))
        };

        let (hh, err) = (h.clone(), error.clone());
        self.db(move |s| s.set_domain_status(&hh, status, err.as_deref())).await?;
        self.hosts_changed();

        if let Some((tls, tls_error)) = self.refresh_tls(&h).await {
            (d.tls, d.tls_error) = (tls, tls_error);
        }
        d.status = status;
        d.error = error;
        d.target = target;
        d.record = self.record(&d.hostname);
        Ok(d)
    }

    // ── a project's preview domain (docs/apps/README.md, "A project's preview domain") ──

    /// Where a project's preview domain's CNAME points (a name of roll's that reaches it).
    fn preview_target(&self, project: &str) -> String {
        self.target(&format!("previews-{}", project.strip_prefix("prj_").unwrap_or(project)))
    }

    fn preview_domain_view(&self, project: &str, (hostname, status, token, error, created_at): store::PreviewDomainRow) -> Domain {
        Domain {
            record: self.record(&hostname),
            target: self.preview_target(project),
            hostname,
            kind: DomainKind::Custom,
            status,
            token,
            error,
            // Plain HTTP until wildcard certificates (DNS-01) are issued here.
            tls: TlsStatus::None,
            tls_error: None,
            created_at,
            redirect_to: None,
            port: None,
        }
    }

    /// `GET /v1/apps/projects/{project}/preview-domain`.
    pub async fn preview_domain(self: &Arc<Self>, project: &str) -> Result<Domain> {
        let p = project.to_string();
        let row = self.db(move |s| s.preview_domain(&p)).await?.ok_or_else(|| Error::NotFound(format!("{project} has no preview domain")))?;
        Ok(self.preview_domain_view(project, row))
    }

    /// `POST /v1/apps/projects/{project}/preview-domain {hostname}`: a wildcard (`*.preview.acme.dev`)
    /// its preview environments answer under, by their `preview_host`; pending until its DNS is
    /// verified, as a custom domain is. One a project (a new one replaces it).
    pub async fn set_preview_domain(self: &Arc<Self>, project: &str, body: Value) -> Result<Domain> {
        self.project_name(project)?;
        let host = body.get("hostname").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("hostname is a wildcard like *.preview.acme.dev".into()))?;
        let mut reserved: Vec<&str> = self.cfg.reserved.iter().map(String::as_str).collect();
        reserved.push(&self.cfg.domain);
        let host = check_hostname(host, &reserved)?;
        if !host.starts_with("*.") {
            return Err(Error::BadRequest("hostname is a wildcard like *.preview.acme.dev (its previews are the names one label under it)".into()));
        }
        let (p, h) = (project.to_string(), host.clone());
        self.db(move |s| s.set_preview_domain(&p, &h)).await?;
        self.hosts_changed();
        self.preview_domain(project).await
    }

    /// `POST …/preview-domain/verify`: its DNS checked as a custom domain's is (the TXT challenge,
    /// or the CNAME to its own target where that target is the project's alone).
    pub async fn verify_preview_domain(self: &Arc<Self>, project: &str) -> Result<Domain> {
        let p = project.to_string();
        let (host, _, token, _, _) = self.db(move |s| s.preview_domain(&p)).await?.ok_or_else(|| Error::NotFound(format!("{project} has no preview domain")))?;
        let target = self.preview_target(project);
        let cnames = self.deps.resolver.cname(&host).await;
        let cname_ok = cnames.contains(&target);
        let challenge = crate::model::challenge_name(&host);
        let txts = self.deps.resolver.txt(&challenge).await;
        let shared = self.cfg.cname_target.is_some();
        let (status, error) = if txts.contains(&token) || (cname_ok && !shared) {
            (DomainStatus::Active, None)
        } else {
            let seen = crate::model::DnsSeen { cnames, txts, host_txts: self.deps.resolver.txt(&host).await, addresses: if cname_ok { Vec::new() } else { self.deps.resolver.addresses(&host).await } };
            (DomainStatus::Pending, Some(crate::model::domain_diagnosis(&host, &target, &challenge, &token, &seen, shared)))
        };
        let (p, err) = (project.to_string(), error.clone());
        self.db(move |s| s.set_preview_domain_status(&p, status, err.as_deref())).await?;
        self.hosts_changed();
        self.preview_domain(project).await
    }

    /// `DELETE …/preview-domain`.
    pub async fn delete_preview_domain(self: &Arc<Self>, project: &str) -> Result<()> {
        let p = project.to_string();
        if !self.db(move |s| s.delete_preview_domain(&p)).await? {
            return Err(Error::NotFound(format!("{project} has no preview domain")));
        }
        self.hosts_changed();
        Ok(())
    }

    pub async fn delete_domain(self: &Arc<Self>, service: &str, env: &str, host: &str) -> Result<()> {
        // What a hostname names is about to change: no host answer kept from before.
        self.hosts_changed();
        let (sid, e, h) = (service.to_string(), env.to_string(), host.to_ascii_lowercase());
        self.db(move |s| s.delete_domain(&sid, &e, &h)).await?;
        self.hosts_changed();
        self.release_domains(&[host.to_ascii_lowercase()]).await;
        Ok(())
    }

    /// Tell the certificate's provider these hostnames are gone (their rows already are, so the
    /// claim is free whatever it answers).
    async fn release_domains(&self, hosts: &[String]) {
        for h in hosts {
            if let Err(e) = self.deps.tls.remove(h).await {
                tracing::warn!(host = %h, error = %e, "apps: a removed domain is still at its certificate's provider");
            }
        }
    }

    /// Remove the custom domains still unverified `older_than` after they were added: the
    /// hostname is free for its real owner. Returns them.
    pub async fn expire_domain_claims(self: &Arc<Self>, older_than: Duration) -> Result<Vec<String>> {
        // What a hostname names is about to change: no host answer kept from before.
        self.hosts_changed();
        let age = older_than.as_millis() as i64;
        // The database's clock, which stamped them.
        let expired = self.db(move |s| s.expire_domain_claims(s.now()? - age)).await?;
        self.hosts_changed();
        let hosts: Vec<String> = expired.into_iter().map(|(svc, env, h)| {
            tracing::info!(service = %svc, env = %env, host = %h, "apps: an unverified custom domain expired");
            h
        }).collect();
        self.release_domains(&hosts).await;
        Ok(hosts)
    }

    /// `PATCH …/domains/{host} {redirect_to?, port?}`: send a custom domain's requests to another
    /// of the environment's hostnames (its own, or another custom domain that doesn't redirect),
    /// or (`null`) serve them again; `port`: to another of the app's ports, published from the
    /// next deploy (`null`: the service's own). A field left out stays as it is.
    pub async fn patch_domain(self: &Arc<Self>, service: &str, env: &str, host: &str, body: Value) -> Result<Domain> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !matches!(k.as_str(), "redirect_to" | "port")) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let h = host.to_ascii_lowercase();
        let all = self.domains(service, env).await?;
        if !all.iter().any(|d| d.kind == DomainKind::Custom && d.hostname == h) {
            return Err(Error::NotFound(format!("{h} isn't a custom domain of {service} in {env}")));
        }
        let port = match o.get("port") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(v) => match v.as_u64() {
                Some(p) if (1..=65535).contains(&p) => Some(Some(p as u16)),
                _ => return Err(Error::BadRequest("port is the app's port this hostname goes to (1–65535), or null".into())),
            },
        };
        if let Some(Some(_)) = port {
            let (sid, e) = (service.to_string(), env.to_string());
            let (svc, row) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await?;
            if svc.run.tcp || svc.run.kind != crate::model::RunType::Web {
                return Err(Error::BadRequest("port is for a web service's custom domain".into()));
            }
            // Its instances answer rolld through their sandbox's socket, on their own port.
            if row.egress.closed() {
                return Err(Error::Conflict("a closed environment's instances are reached on their own port only".into()));
            }
        }
        let to = match o.get("redirect_to") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(t)) => Some(Some(t.trim().trim_end_matches('.').to_ascii_lowercase())),
            Some(_) => return Err(Error::BadRequest("redirect_to is a hostname or null".into())),
        };
        if let Some(Some(t)) = &to {
            if t.starts_with("*.") {
                return Err(Error::BadRequest("redirect_to is one hostname, not a wildcard".into()));
            }
            let target = all.iter().find(|d| &d.hostname == t).ok_or_else(|| Error::BadRequest(format!("{t} isn't a hostname of this environment")))?;
            if *t == h || target.redirect_to.is_some() {
                return Err(Error::BadRequest("redirect_to is another hostname of the environment that serves it (no loops or chains)".into()));
            }
            if all.iter().any(|d| d.redirect_to.as_deref() == Some(h.as_str())) {
                return Err(Error::BadRequest(format!("another domain redirects to {h}: it serves, so it can't redirect")));
            }
        }
        let hh = h.clone();
        self.db(move |s| {
            if let Some(t) = &to {
                s.set_redirect(&hh, t.as_deref())?;
            }
            if let Some(p) = port {
                s.set_domain_port(&hh, p)?;
            }
            Ok(())
        })
        .await?;
        // What a hostname names changed: no host answer kept from before.
        self.hosts_changed();
        self.domains(service, env).await?.into_iter().find(|d| d.hostname == h).ok_or_else(|| Error::NotFound(h.clone()))
    }

    /// Where an active custom domain redirects, if it does.
    pub async fn redirect_for_host(self: &Arc<Self>, host: &str) -> Option<String> {
        let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host.ends_with(&format!(".{}", self.cfg.domain)) {
            return None;
        }
        self.active_domain(&host).await.and_then(|(_, _, d)| d.redirect_to)
    }

    /// The active custom domain a hostname is: its own, else the wildcard one label above it.
    async fn active_domain(self: &Arc<Self>, host: &str) -> Option<(String, String, Domain)> {
        let exact = host.to_string();
        if let Some(d) = self.db(move |s| s.serving_domain(&exact)).await.ok().flatten().filter(|(_, _, d)| d.status == DomainStatus::Active) {
            return Some(d);
        }
        let wild = crate::model::covering_wildcard(host)?;
        self.db(move |s| s.serving_domain(&wild)).await.ok().flatten().filter(|(_, _, d)| d.status == DomainStatus::Active)
    }

    /// Write the audit trail's entry for a change someone made (`audit.<what>`): who, and what
    /// changed, never a value. Best effort: the change already happened.
    pub async fn audit(self: &Arc<Self>, what: &str, service: &str, env: &str, actor: Option<String>, mut data: Value) {
        data["actor"] = json!(actor);
        let (kind, sid, e) = (format!("audit.{what}"), service.to_string(), env.to_string());
        // To the drains that take `audit` too, as it's written (a SIEM's copy; best effort as
        // drains are: webhooks follow the event log, durably).
        let text = json!({ "event": kind, "data": data }).to_string();
        if let Err(err) = self.db(move |s| s.add_event(&kind, &sid, &e, None, &data)).await {
            tracing::warn!(service, error = %err, "apps: an audit entry wasn't written");
        } else {
            let line = crate::logs::LogLine { t: code_sql::now_ms(), stream: crate::logs::Stream::System, instance: None, deployment: None, text, level: None, trace: None };
            self.logs.to_drains(&format!("audit:{service}/{env}"), line);
        }
        self.changed(service);
    }

    /// `GET /v1/apps/services/{id}/activity`: its audit trail, newest first.
    pub async fn activity(self: &Arc<Self>, service: &str, all: bool) -> Result<Vec<Event>> {
        let sid = service.to_string();
        self.db(move |s| {
            s.service(&sid)?;
            s.activity(&sid, 200, all)
        })
        .await
    }

    /// The environment a request's `Host` names: a generated hostname, or an active custom domain.
    /// The service and environment a hostname is, and the deployment when it's one's own
    /// (`<deployment>--<slug>`: the id without `dep_`).
    pub async fn env_for_host(self: &Arc<Self>, host: &str) -> Option<HostTarget> {
        // Every request asks: kept a second (another node's change shows within it), and
        // forgotten at once on any change here. It was two queries a request, about 150 µs of the
        // proxy's 260 (`docker.rs::the_proxys_own_cost_is_measured`, release).
        // Kept by the name itself (no port, any case, no trailing dot), and only a name that is
        // one (253 bytes at most) and names something: anyone can send any Host, and a miss isn't
        // worth a slot.
        let key = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h).trim_end_matches('.').to_ascii_lowercase();
        if key.is_empty() || key.len() > 253 {
            return None;
        }
        let generation = self.hosts_gen.load(std::sync::atomic::Ordering::SeqCst);
        if let Some((at, g, v)) = self.hosts.lock().get(&key).cloned()
            && g == generation
            && at.elapsed() < HOST_TTL
        {
            return v;
        }
        let v = self.env_for_host_now(host).await;
        if v.is_some() {
            let mut cache = self.hosts.lock();
            if cache.len() >= 10_000 {
                cache.clear();
            }
            cache.insert(key, (Instant::now(), generation, v.clone()));
        }
        v
    }

    async fn env_for_host_now(self: &Arc<Self>, host: &str) -> Option<HostTarget> {
        let found = self.host_target(host).await?;
        // A private service has no public names: the same 404 as an address nobody has, read
        // from its settings now (making it private takes effect at once, not with a deploy).
        let sid = found.0.clone();
        // A TCP service's port isn't HTTP: its names don't serve it.
        self.db(move |s| s.service(&sid)).await.ok().is_some_and(|svc| svc.run.public && !svc.run.tcp).then_some(found)
    }

    async fn host_target(self: &Arc<Self>, host: &str) -> Option<HostTarget> {
        let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if let Some(label) = host.strip_suffix(&self.cfg.domain).and_then(|s| s.strip_suffix('.')).filter(|s| !s.contains('.')) {
            let slug = label.to_string();
            if let Some(e) = self.db(move |s| s.environment_by_slug(&slug)).await.ok().flatten() {
                return Some((e.service, e.name, None, None));
            }
            let (dep, slug) = label.split_once("--").filter(|(d, s)| d.len() == 12 && d.bytes().all(|b| b.is_ascii_alphanumeric()) && !s.is_empty())?;
            let (dep, slug) = (format!("dep_{dep}"), slug.to_string());
            return self.db(move |s| s.environment_by_slug(&slug)).await.ok().flatten().map(|e| (e.service, e.name, Some(dep), None));
        }
        if let Some((s, e, d)) = self.active_domain(&host).await {
            return Some((s, e, None, d.port));
        }
        // One label under a project's active preview domain: the environment that answers to it
        // (any other label is none of roll apps': rolld's own previews may serve it).
        let (label, rest) = host.split_once('.')?;
        let (wild, label) = (format!("*.{rest}"), label.to_string());
        self.db(move |s| s.preview_target(&wild, &label)).await.ok().flatten().map(|(s, e)| (s, e, None, None))
    }

    /// What the environment's public names let through ([`live::Gate`]): the live state's copy,
    /// read from the store the first time after a change. Unreadable: the store's error.
    pub(crate) async fn gate(self: &Arc<Self>, live: &EnvLive) -> Result<Arc<live::Gate>> {
        if let Some(g) = live.gate() {
            return Ok(g);
        }
        let seen = live.gate_gen();
        let (sid, e) = (live.service.clone(), live.env.clone());
        let (row, traces) = self.db(move |s| Ok((s.environment(&sid, &e)?, s.traces_on(&sid, &e)?))).await?;
        let allow = row.allow_ips.iter().filter_map(|r| r.cidr.parse::<ipnet::IpNet>().ok()).collect();
        // The environment's challenge key, made the first time a rule needs it (every node reads the same).
        let challenge_key = match row.firewall.iter().any(|r| r.action == crate::model::FirewallAction::Challenge) {
            true => {
                let (k, v) = (format!("challenge_key:{}/{}", live.service, live.env), crate::model::random(32));
                Some(self.db(move |s| s.state_or(&k, &v)).await?.into_bytes())
            }
            false => None,
        };
        let g = Arc::new(live::Gate { maintenance: row.maintenance, allow, rate: row.rate_limit, firewall: row.firewall, pages: row.pages, preview: row.ephemeral, headers: row.headers, routes: row.routes, traces, vitals: row.vitals, challenge_key, split: row.split, mirror: row.mirror, snippets: snippets_of(&row.snippets) });
        live.set_gate(g.clone(), seen);
        Ok(g)
    }

    /// Where a preview's feedback button reads what's been said (roll previews, once it's on this
    /// node). Set once.
    pub fn set_feedback_notes(&self, notes: Arc<dyn integrations::FeedbackNotes>) {
        let _ = self.notes.set(notes);
    }

    pub(crate) fn feedback_notes(&self) -> Option<Arc<dyn integrations::FeedbackNotes>> {
        self.notes.get().cloned()
    }

    /// Where names under a project's preview domain that no environment holds go (roll previews'
    /// router, for its previews on roll's own runtime). Set once.
    pub fn set_preview_router(&self, router: Arc<dyn integrations::PreviewRouter>) {
        let _ = self.preview_router.set(router);
    }

    /// The router for `host` when it's one label under a project's active preview domain that no
    /// environment answers to.
    pub(crate) async fn preview_router_for(self: &Arc<Self>, host: &str) -> Option<Arc<dyn integrations::PreviewRouter>> {
        let router = self.preview_router.get().cloned()?;
        let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h).trim_end_matches('.').to_ascii_lowercase();
        let (_, rest) = host.split_once('.')?;
        let wild = format!("*.{rest}");
        self.db(move |s| s.preview_domain_active(&wild)).await.ok().filter(|on| *on).map(|_| router)
    }

    /// Where a Git URL service's pushes to branches it doesn't follow go (roll previews). Set once.
    pub fn set_git_pushes(&self, pushes: Arc<dyn integrations::GitPushes>) {
        let _ = self.pushes.set(pushes);
    }

    /// Who may still open a project's environments (roll-accounts). Set once.
    pub fn set_visitors(&self, v: Arc<dyn integrations::Visitors>) {
        let _ = self.visitors.set(v);
    }

    /// How email reaches people (roll-accounts' mailer). Set once.
    pub fn set_mailbox(&self, mail: Arc<dyn integrations::Mailbox>) {
        let _ = self.mail.set(mail);
    }

    /// The mailbox, when mail goes out.
    pub(crate) fn mailbox(&self) -> Option<Arc<dyn integrations::Mailbox>> {
        self.mail.get().filter(|m| m.on()).cloned()
    }

    /// The project's owner (`usr_…`); none for the operator's projects or without a workspace.
    pub(crate) async fn project_owner(&self, project: &str) -> Option<String> {
        let ws = self.deps.workspace.clone()?;
        let p = project.to_string();
        tokio::task::spawn_blocking(move || ws.get(roll_workspace::Scope::All, &p).ok().map(|p| p.owner)).await.ok().flatten().filter(|o| !o.is_empty())
    }

    /// Where a preview's feedback goes (the preview in roll's dashboard), for a visitor who came
    /// in through its access link, when roll previews turned the button on and the dashboard is
    /// known.
    pub(crate) async fn feedback_target(self: &Arc<Self>, service: &str, env: &str) -> Option<String> {
        let web = self.cfg.web_url.clone()?;
        let preview = env.strip_prefix("pv-")?.to_string();
        let (sid, e) = (service.to_string(), env.to_string());
        let (svc, row) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?))).await.ok()?;
        (row.ephemeral && row.feedback).then(|| format!("{}/dashboard/projects/{}/previews?preview={preview}", web.trim_end_matches('/'), svc.project))
    }

    /// What a deployment's own URL runs: that deployment of a web service, built and not
    /// removed, in this environment. A service with a volume has none (one instance writes it).
    pub async fn deployment_template(self: &Arc<Self>, service: &str, env: &str, dep: &str) -> Result<live::Template> {
        let (sid, e, d) = (service.to_string(), env.to_string(), dep.to_string());
        let (svc, row, d) = self.db(move |s| Ok((s.service(&sid)?, s.environment(&sid, &e)?, s.deployment(&d)?))).await?;
        if d.service != service || d.environment != env || d.image.is_none() || d.status == DeployStatus::Removed {
            return Err(Error::NotFound(format!("no deployment {dep} to run here")));
        }
        if d.run.kind != RunType::Web {
            return Err(Error::Conflict("only a web service's deployments have their own URL".into()));
        }
        if d.run.volume.is_some() {
            return Err(Error::Conflict("a service with a volume has no deployment URLs: one instance writes the volume".into()));
        }
        self.template(&svc.for_env(&row)?, &row, &d).await
    }

    // ── logs, metrics, usage ──

    /// An environment's last finished minute in Prometheus' text format (Fly's metrics, Render's
    /// metrics streams): gauges labelled with the service, its id and the environment.
    pub async fn metrics_prometheus(self: &Arc<Self>, service: &str, env: &str) -> Result<String> {
        let (svc, _) = self.service_env(service, env).await?;
        // The last finished minute once it's stored (the meters are written every 10 s).
        let now = code_sql::now_ms();
        let minute = now / 60_000 - if now % 60_000 >= 15_000 { 1 } else { 2 };
        let (sid, e) = (service.to_string(), env.to_string());
        let r = self.db(move |s| s.minutes(&sid, &e, minute)).await?.into_iter().find(|r| r.minute == minute).unwrap_or(store::MinuteRow { minute, ..Default::default() });
        // Label values are names and ids roll made ([a-z0-9-_]), quoted as the format wants.
        let esc = |v: &str| v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
        let labels = format!("service=\"{}\",service_id=\"{}\",environment=\"{}\"", esc(&svc.name), esc(&svc.id), esc(env));
        let mut out = String::new();
        let mut gauge = |name: &str, help: &str, value: f64, extra: &str| {
            if !out.contains(&format!("# TYPE {name} ")) {
                out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
            }
            out.push_str(&format!("{name}{{{labels}{extra}}} {value}\n"));
        };
        gauge("roll_apps_requests_per_minute", "Requests in the last finished minute.", r.requests as f64, "");
        gauge("roll_apps_errors_per_minute", "5xx answers in the last finished minute.", r.errors as f64, "");
        for (q, v) in [("0.5", r.p50_ms), ("0.95", r.p95_ms), ("0.99", r.p99_ms)] {
            gauge("roll_apps_latency_ms", "Request latency in the last finished minute, by quantile.", v, &format!(",quantile=\"{q}\""));
        }
        gauge("roll_apps_cpu_seconds_per_minute", "CPU time its instances used in the last finished minute.", r.cpu, "");
        gauge("roll_apps_memory_bytes", "Memory its instances held.", r.memory_bytes as f64, "");
        gauge("roll_apps_instances", "Instances that were up.", r.instances as f64, "");
        gauge("roll_apps_cold_starts_per_minute", "Cold starts in the last finished minute.", r.cold_starts as f64, "");
        gauge("roll_apps_cold_start_ms", "Their mean time to answer.", r.cold_start_ms, "");
        gauge("roll_apps_egress_bytes_per_minute", "Bytes answered in the last finished minute.", r.egress_bytes as f64, "");
        gauge("roll_apps_over_capacity_per_minute", "Requests in the last finished minute that found every instance busy at max_instances.", r.over_capacity as f64, "");
        Ok(out)
    }

    /// `GET …/environments/{env}/slo`: its objective (the live deployment's, else the service's)
    /// against its window: each indicator's total, failures, share and budget left, and the burn
    /// alert firing now. Minutes for the last week, hours before (an hour's latency fails whole
    /// when its slowest minute did). Null when it has none.
    pub async fn slo_report(self: &Arc<Self>, service: &str, env: &str) -> Result<Value> {
        let (svc, row) = self.service_env(service, env).await?;
        let slo = match row.active.clone() {
            Some(dep) => self.db(move |s| s.deployment(&dep)).await?.run.slo,
            None => svc.for_env(&row)?.run.slo,
        };
        let Some(slo) = slo else { return Ok(json!({ "slo": null })) };
        let now_min = code_sql::now_ms() / 60_000;
        let since = now_min - slo.window_days as i64 * 24 * 60;
        let (sid, e) = (service.to_string(), env.to_string());
        let (minutes, hours) = self.db(move |s| Ok((s.minutes(&sid, &e, since)?, s.hours(&sid, &e, since / 60)?))).await?;
        let minutes: Vec<_> = minutes.into_iter().filter(|r| r.minute < now_min).collect();
        // Hours only before the first minute kept (a rolled-up hour isn't counted twice).
        let first = minutes.first().map_or(now_min, |r| r.minute);
        let hours: Vec<_> = hours.into_iter().filter(|h| (h.minute + 1) * 60 <= first).collect();
        let show = |t: crate::slo::Tally| json!({ "total": t.total, "bad": t.bad, "sli": (t.total > 0).then(|| 100.0 * (t.total - t.bad) as f64 / t.total as f64), "budget_left": t.budget_left(slo.objective) });
        let avail = crate::slo::availability(&minutes);
        let avail = crate::slo::Tally { total: avail.total + crate::slo::availability(&hours).total, bad: avail.bad + crate::slo::availability(&hours).bad };
        let latency = slo.latency_ms.map(|ms| {
            let (m, h) = (crate::slo::latency(&minutes, ms, 1), crate::slo::latency(&hours, ms, 60));
            show(crate::slo::Tally { total: m.total + h.total, bad: m.bad + h.bad })
        });
        let burning = crate::slo::burning(&slo, &minutes, now_min).map(|(i, rate, w)| json!({ "indicator": i, "burn_rate": (rate * 10.0).round() / 10.0, "window": w }));
        Ok(json!({ "slo": slo, "availability": show(avail), "latency": latency, "burning": burning }))
    }

    /// Pause or resume an environment (`PUT … {paused}`, its pause schedule): at once on its
    /// owner, what runs stops; resumed, the next tick starts what it wants again.
    pub(crate) async fn set_env_paused(self: &Arc<Self>, service: &str, env: &str, on: bool) -> Result<()> {
        let (sid, e) = (service.to_string(), env.to_string());
        self.db(move |s| s.set_paused(&sid, &e, on)).await?;
        // Resumed: its secrets are watched again (a watch ends itself while it's paused).
        if !on {
            self.watch_secrets_of(service).await;
        }
        // At once on its owner: what runs stops, and the next tick keeps no floor; resumed,
        // the next tick (or request) starts what it wants again.
        let live = self.live(service, env);
        live.set_paused(on);
        if on {
            live.sleep().await;
            // What runs to its end (a job, a cron run) stops too: nothing of it runs.
            self.cancel_jobs(service, env);
            let runs: Vec<String> = self.running_runs.lock().iter().filter(|(_, (s, e))| s == service && e == env).map(|(id, _)| id.clone()).collect();
            for id in runs {
                if let Some(t) = self.cancels.lock().get(&id) {
                    t.cancel();
                }
            }
        } else {
            live.tick().await;
        }
        self.changed(service);
        Ok(())
    }

    /// An environment's metrics over the last `minutes`: a point a minute for up to a week, a
    /// point an hour past that (up to 90 days: the week's minutes grouped into hours, older hours
    /// from the rollups). `step` says which, in minutes.
    pub async fn metrics(self: &Arc<Self>, service: &str, env: &str, minutes: i64) -> Result<Value> {
        self.service_env(service, env).await?;
        let minutes = minutes.clamp(5, 90 * 24 * 60);
        let step: i64 = if minutes > 7 * 24 * 60 { 60 } else { 1 };
        let now_min = code_sql::now_ms() / 60_000;
        let since = (now_min - minutes + 1) / step * step;
        let (sid, e) = (service.to_string(), env.to_string());
        let mut minute_rows = self.db(move |s| s.minutes(&sid, &e, since)).await?;
        if let Some(l) = self.lives.lock().get(&env_key(service, env))
            && let Some(cur) = l.meter.lock().current()
        {
            minute_rows.retain(|r| r.minute != cur.minute);
            minute_rows.push(cur);
        }
        if step > 1 {
            let (sid, e) = (service.to_string(), env.to_string());
            minute_rows.extend(self.db(move |s| s.hours(&sid, &e, since / 60)).await?);
        }
        // Each point: its counts summed, the rest their peak.
        let mut rows: HashMap<i64, store::MinuteRow> = HashMap::new();
        for r in minute_rows {
            let at = r.minute / step * step;
            let p = rows.entry(at).or_insert_with(|| store::MinuteRow { minute: at, ..Default::default() });
            p.requests += r.requests;
            p.errors += r.errors;
            p.cold_starts += r.cold_starts;
            p.egress_bytes += r.egress_bytes;
            p.over_capacity += r.over_capacity;
            p.p50_ms = p.p50_ms.max(r.p50_ms);
            p.p95_ms = p.p95_ms.max(r.p95_ms);
            p.p99_ms = p.p99_ms.max(r.p99_ms);
            p.cpu = p.cpu.max(r.cpu);
            p.memory_bytes = p.memory_bytes.max(r.memory_bytes);
            p.instances = p.instances.max(r.instances);
            p.cold_start_ms = p.cold_start_ms.max(r.cold_start_ms);
        }
        let mut out: BTreeMap<&str, Value> = BTreeMap::new();
        out.insert("step", step.into());
        let mut series: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
        for m in (since..=now_min).step_by(step as usize) {
            let r = rows.remove(&m).unwrap_or(store::MinuteRow { minute: m, ..Default::default() });
            let out = &mut series;
            out.entry("minutes").or_default().push(m.into());
            out.entry("requests").or_default().push(r.requests.into());
            out.entry("errors").or_default().push(r.errors.into());
            out.entry("p50_ms").or_default().push(r.p50_ms.into());
            out.entry("p95_ms").or_default().push(r.p95_ms.into());
            out.entry("p99_ms").or_default().push(r.p99_ms.into());
            out.entry("cpu").or_default().push(r.cpu.into());
            out.entry("memory_bytes").or_default().push(r.memory_bytes.into());
            out.entry("instances").or_default().push(r.instances.into());
            out.entry("cold_starts").or_default().push(r.cold_starts.into());
            out.entry("cold_start_ms").or_default().push(r.cold_start_ms.into());
            out.entry("egress_bytes").or_default().push(r.egress_bytes.into());
            out.entry("over_capacity").or_default().push(r.over_capacity.into());
        }
        out.extend(series.into_iter().map(|(k, v)| (k, Value::Array(v))));
        // Each instance's own last hour (on its owner, from the reaper's samples).
        let live = self.lives.lock().get(&env_key(service, env)).cloned();
        let by_instance = live.map(|l| l.by_instance()).unwrap_or_default();
        out.insert("by_instance", serde_json::to_value(by_instance).expect("JSON"));
        Ok(serde_json::to_value(out).expect("JSON"))
    }

    pub async fn usage(self: &Arc<Self>, service: &str, env: Option<&str>, days: i64) -> Result<Value> {
        let days = days.clamp(1, 93);
        let now_h = code_sql::now_ms() / 3_600_000;
        let today = now_h / 24;
        let since = (today - days + 1) * 24;
        let (sid, e) = (service.to_string(), env.map(str::to_string));
        let rows = self.db(move |s| s.usage(&sid, e.as_deref(), since)).await?;
        let mut by_day: BTreeMap<i64, store::UsageAdd> = BTreeMap::new();
        for (hour, u) in rows {
            let d = by_day.entry(hour / 24).or_default();
            d.cpu_seconds += u.cpu_seconds;
            d.gb_seconds += u.gb_seconds;
            d.requests += u.requests;
            d.egress_bytes += u.egress_bytes;
            d.build_seconds += u.build_seconds;
        }
        let p = self.cfg.prices;
        let mut total = 0.0;
        let days: Vec<Value> = ((today - days + 1)..=today)
            .map(|d| {
                let u = by_day.remove(&d).unwrap_or_default();
                let usd = p.cost(&u);
                total += usd;
                json!({ "day": d, "cpu_seconds": u.cpu_seconds, "gb_seconds": u.gb_seconds, "requests": u.requests, "egress_bytes": u.egress_bytes, "build_seconds": u.build_seconds, "usd": usd })
            })
            .collect();
        Ok(json!({ "days": days, "total_usd": total }))
    }

    /// Write every environment's finished minutes (and, when `all`, the current ones).
    pub async fn flush_meters(self: &Arc<Self>, all: bool) {
        let now = code_sql::now_ms();
        for l in self.lives() {
            let minutes = l.meter.lock().take(now, all);
            if minutes.is_empty() {
                continue;
            }
            let (svc, env) = (l.service.clone(), l.env.clone());
            let _ = self
                .db(move |s| {
                    s.put_minutes(&svc, &env, &minutes.iter().map(|m| m.row()).collect::<Vec<_>>())?;
                    for m in &minutes {
                        s.add_usage(&svc, &env, m.minute / 60, &m.usage())?;
                    }
                    Ok(())
                })
                .await;
        }
        self.logs.flush().await;
    }

    pub fn overview(&self) -> Value {
        json!({ "runtime": self.deps.runtime.name(), "domain": self.cfg.domain, "prices": self.cfg.prices, "secrets": self.deps.secrets.name() })
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.stop.clone()
    }
}

impl ResolvedVars {
    /// The variables that decide whether a deployment is stale (not roll's per-deployment ones:
    /// a new commit is a new deployment already).
    fn vars_for_hash(&self) -> Vec<(String, String)> {
        self.vars
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "ROLL_DEPLOYMENT" | "ROLL_GIT_COMMIT" | "ROLL_GIT_BRANCH"))
            .map(|(k, v)| (k.clone(), self.ref_texts.get(k).cloned().unwrap_or_else(|| v.clone())))
            .collect()
    }
}

/// An apex (`example.com`, `example.co.uk`) can't hold a CNAME: it proves itself with the TXT
/// challenge. Anything deeper is a CNAME.
fn record_for(host: &str) -> &'static str {
    let labels = host.split('.').count();
    let second_level = ["co", "com", "org", "net", "ac", "gov", "edu"];
    let apex = labels == 2 || (labels == 3 && second_level.contains(&host.split('.').nth(1).unwrap_or("")));
    if apex { "txt" } else { "cname" }
}

pub fn volume_name(service: &str, env: &str) -> String {
    format!("roll-apps-vol-{}-{env}", service.strip_prefix("svc_").unwrap_or(service))
}

/// The most variables an environment keeps as files.
pub const MAX_FILES: usize = 20;

/// A path a variable may be written to in an instance: absolute, at most 256 characters, no `..`
/// or empty segment, no control character, not in the system's own trees (`/proc`, `/sys`,
/// `/dev`), and not a folder.
pub fn check_file_path(p: &str) -> Result<String> {
    let bad = || Error::BadRequest(format!("{p:?} isn't a path a file can go to (absolute, e.g. /etc/secrets/key.json; not under /proc, /sys or /dev)"));
    let segs: Vec<&str> = p.split('/').skip(1).collect();
    if !p.starts_with('/') || p.len() > 256 || p.ends_with('/') || p.chars().any(char::is_control) || segs.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return Err(bad());
    }
    if matches!(segs.first(), Some(&("proc" | "sys" | "dev"))) {
        return Err(bad());
    }
    Ok(p.to_string())
}

pub fn network_name(project: &str, env: &str) -> String {
    format!("roll-apps-net-{}-{env}", project.strip_prefix("prj_").unwrap_or(project))
}

/// The project environment's network with no route out (its services with `egress` `roll`):
/// they reach each other there, and the open ones on [`network_name`]. Its `.` is a character no
/// environment's name has, so it's never another environment's open network.
pub fn closed_network_name(project: &str, env: &str) -> String {
    format!("roll-apps-closed-{}.{env}", project.strip_prefix("prj_").unwrap_or(project))
}

/// A free spot on a project's canvas for a new card (the same grid the dashboard uses).
fn free_spot(ws: &roll_workspace::Workspace, project: &str) -> (f64, f64) {
    let taken: Vec<(f64, f64)> = ws.get(roll_workspace::Scope::All, project).map(|p| p.resources.iter().map(|r| (r.x, r.y)).collect()).unwrap_or_default();
    let (w, h) = (208.0, 96.0);
    for row in 0..50 {
        for col in 0..3 {
            let (x, y) = (48.0 + col as f64 * (w + 56.0), 48.0 + row as f64 * (h + 44.0));
            if taken.iter().all(|(tx, ty)| (tx - x).abs() >= w + 16.0 || (ty - y).abs() >= h + 24.0) {
                return (x, y);
            }
        }
    }
    (48.0, 48.0)
}

/// `owner` and `repo` of a Git URL on `host` (`https://github.com/owner/repo[.git]`), if it's one.
fn github_repo(url: &str, host: &str) -> Option<(String, String)> {
    if git_host(url)?.as_str() != host {
        return None;
    }
    let u = url::Url::parse(url).ok()?;
    let path = u.path().trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/')).then(|| (owner.to_string(), repo.to_string()))
}

/// A Git URL's repository, to compare: its host and path, without a trailing `/` or `.git`.
fn git_repo(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let path = u.path().trim_end_matches('/');
    Some(format!("{}{}", git_host(url)?, path.strip_suffix(".git").unwrap_or(path)))
}

/// A Git URL's host, lowercase (with its port when it names one): where its login is sent.
fn git_host(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    Some(match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    })
}

#[cfg(test)]
mod fingerprint_tests {
    use super::*;

    #[test]
    fn only_what_differs_from_the_defaults_counts() {
        let mut run = Run::default();
        assert_eq!(set_only(&run, &Run::default()), json!({}));
        run.memory_mb = 1024;
        run.alerts.error_pct = 10;
        // A nested setting counts alone: a sibling roll adds later doesn't change this.
        assert_eq!(set_only(&run, &Run::default()), json!({"memory_mb": 1024, "alerts": {"error_pct": 10}}));
    }
}

