//! What roll apps stores and answers (docs/apps/http-api.md, "Shapes"), with the rules every
//! value follows. Parsing happens here, at the boundary: the rest of the crate gets checked
//! values.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};

/// A service's or an environment's name: 1–32 characters of `[a-z0-9-]`, not starting or
/// ending with `-`. Names go into hostnames and container names, so this is strict.
pub fn check_name(what: &str, name: &str) -> Result<()> {
    let ok = (1..=32).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(Error::BadRequest(format!("{what} is 1–32 characters of a-z, 0-9 and -, not starting or ending with -")))
    }
}

pub const PRODUCTION: &str = "production";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Source {
    /// A roll code repo (`owner/name`); GitHub repos are mirrored into roll code.
    Repo {
        repo: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root: Option<String>,
    },
    /// A public image, pinned by digest at deploy time.
    Image { image: String },
    /// `roll up`: each deploy carries its own tree.
    Upload,
}

impl Source {
    pub fn check(&self) -> Result<()> {
        match self {
            Source::Repo { repo, root } => {
                let ok = repo.split_once('/').is_some_and(|(o, n)| !o.is_empty() && !n.is_empty() && !n.contains('/')) && repo.len() <= 200;
                if !ok {
                    return Err(Error::BadRequest("source.repo is owner/name".into()));
                }
                if let Some(r) = root {
                    check_rel_path("source.root", r)?;
                }
                Ok(())
            }
            Source::Image { image } => {
                let ok = !image.is_empty() && image.len() <= 255 && image.bytes().all(|b| b.is_ascii_graphic()) && !image.starts_with('-');
                if !ok {
                    return Err(Error::BadRequest("source.image is an image reference".into()));
                }
                // dockerd fetches the registry a tenant names: never one on this host or a
                // private network (it would speak plain HTTP to loopback, and say what answered).
                if let Some(host) = registry_host(image)
                    && private_host(host)
                {
                    return Err(Error::BadRequest(format!("source.image: {host} isn't a public registry")));
                }
                Ok(())
            }
            Source::Upload => Ok(()),
        }
    }

    /// Whether deploys build (everything but a public image).
    pub fn builds(&self) -> bool {
        !matches!(self, Source::Image { .. })
    }
}

/// The registry host an image reference names (`ghcr.io`, `registry:5000`), none for Docker Hub.
pub fn registry_host(image: &str) -> Option<&str> {
    let (first, rest) = image.split_once('/')?;
    (!rest.is_empty() && (first.contains('.') || first.contains(':') || first == "localhost")).then_some(first)
}

/// A registry as one name, to compare: its host, lowercase, Docker Hub's names as `docker.io`
/// (`https://index.docker.io/v1/`, `registry-1.docker.io`).
pub fn registry_key(server: &str) -> String {
    let s = server.trim();
    let s = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).unwrap_or(s);
    let host = s.split('/').next().unwrap_or(s).to_ascii_lowercase();
    match host.as_str() {
        "" | "docker.io" | "index.docker.io" | "registry-1.docker.io" | "registry.hub.docker.com" => "docker.io".into(),
        _ => host,
    }
}

/// An image's repository, without its tag or digest, as one name to compare
/// (`nginx:1.27` → `docker.io/library/nginx`).
pub fn image_repository(image: &str) -> String {
    let image = image.trim();
    let no_digest = image.split('@').next().unwrap_or(image);
    let (head, last) = no_digest.rsplit_once('/').unwrap_or(("", no_digest));
    let name = last.split(':').next().unwrap_or(last);
    let path = if head.is_empty() { name.to_string() } else { format!("{head}/{name}") };
    let (registry, rest) = match registry_host(&path) {
        Some(h) => (registry_key(h), path[h.len() + 1..].to_string()),
        None => ("docker.io".to_string(), path.clone()),
    };
    let rest = if registry == "docker.io" && !rest.contains('/') { format!("library/{rest}") } else { rest };
    format!("{registry}/{}", rest.to_ascii_lowercase())
}

/// The registry an image is pulled from, as [`registry_key`].
pub fn image_registry(image: &str) -> String {
    registry_key(registry_host(image).unwrap_or("docker.io"))
}

/// A host on this machine or a private network: loopback, private, link-local, unique-local
/// addresses and `localhost`.
pub(crate) fn private_host(host: &str) -> bool {
    let name = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h).trim_start_matches('[').trim_end_matches(']');
    if name.eq_ignore_ascii_case("localhost") || name.to_ascii_lowercase().ends_with(".localhost") {
        return true;
    }
    name.parse().is_ok_and(private_ip)
}

/// Anything but a public unicast address: loopback, private, shared (CGNAT), link-local,
/// `0/8`, the IETF block `192.0.0/24`, benchmarking, reserved `240/4`, broadcast, multicast,
/// documentation; for IPv6 also unique-local, NAT64 (`64:ff9b::/96`), 6to4 (`2002::/16`),
/// IPv4-compatible and -mapped addresses (judged by their IPv4 address).
pub(crate) fn private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(a) => {
            let [o0, o1, o2, _] = a.octets();
            a.is_loopback()
                || a.is_private()
                || a.is_link_local()
                || a.is_unspecified()
                || a.is_broadcast()
                || a.is_multicast()
                || a.is_documentation()
                || o0 == 0
                || o0 >= 240
                || (o0 == 100 && (o1 & 0xc0) == 64)
                || (o0 == 192 && o1 == 0 && o2 == 0)
                || (o0 == 198 && (o1 & 0xfe) == 18)
        }
        std::net::IpAddr::V6(a) => {
            let seg = a.segments();
            if let Some(v4) = a.to_ipv4_mapped() {
                return private_ip(v4.into());
            }
            // NAT64 and IPv4-compatible addresses carry an IPv4 address in their last 32 bits.
            let embedded = std::net::Ipv4Addr::new((seg[6] >> 8) as u8, seg[6] as u8, (seg[7] >> 8) as u8, seg[7] as u8);
            if (seg[0] == 0x64 && seg[1] == 0xff9b && seg[2..6] == [0, 0, 0, 0]) || seg[..6] == [0, 0, 0, 0, 0, 0] {
                return a.is_loopback() || a.is_unspecified() || private_ip(embedded.into());
            }
            a.is_loopback()
                || a.is_unspecified()
                || a.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80
                || seg[0] == 0x2002
                || (seg[0] == 0x2001 && seg[1] == 0x0db8)
        }
    }
}

/// Something that happened to a service, in the event log (`GET /v1/apps/events`): written with
/// the change it records, read after a cursor.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Event {
    pub seq: i64,
    pub at: i64,
    /// `deploy.active`, `deploy.failed`, `crash_loop`.
    pub kind: String,
    pub service: String,
    pub environment: String,
    pub deployment: Option<String>,
    pub data: serde_json::Value,
}

/// Where a project's events go (`…/projects/{project}/notifications`): a webhook (JSON, signed),
/// a Slack or a Discord incoming webhook. The URL is sealed; only its host shows.
/// A project's log drain (`/v1/apps/projects/{project}/drains`): where its runtime output goes.
/// The URL is never returned, only its host.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LogDrain {
    pub id: String,
    pub project: String,
    pub host: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NotifyTarget {
    pub id: String,
    pub project: String,
    /// `webhook`, `slack`, `discord`.
    pub kind: String,
    /// The URL's host, for recognising it (the URL itself is a secret).
    pub host: String,
    pub events: Vec<String>,
    /// Only these environments' events (names); empty: every environment's.
    pub environments: Vec<String>,
    /// Only these services' events (ids); empty: every service's.
    pub services: Vec<String>,
    /// Whether preview (ephemeral) environments' events come too.
    pub previews: bool,
    pub created_at: i64,
}

impl NotifyTarget {
    /// Whether an event of `service` in `env` (empty: the project's own, as spend) is this
    /// target's to send; `ephemeral`: the environment is a preview's.
    pub fn follows(&self, service: &str, env: &str, ephemeral: bool) -> bool {
        if service.is_empty() {
            return true;
        }
        (self.services.is_empty() || self.services.iter().any(|s| s == service))
            && (self.environments.is_empty() || self.environments.iter().any(|e| e == env))
            && (self.previews || !ephemeral || self.environments.iter().any(|e| e == env))
    }
}

/// One try of sending one event to one target.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Delivery {
    pub id: String,
    pub event: i64,
    pub kind: String,
    /// `pending` (more tries coming), `delivered`, `failed`.
    pub status: String,
    pub attempts: u32,
    pub code: Option<u16>,
    pub error: Option<String>,
    pub at: i64,
}

/// A private environment's gate: the visitors' token and an automation bypass (CI), as SHA-256
/// hex digests; neither secret is kept.
#[derive(Clone, Debug, PartialEq)]
pub struct Access {
    pub token_hash: String,
    pub bypass_hash: Option<String>,
}

/// A private registry's login for a service's image (`PUT …/registry`); the password is sealed
/// and never returned.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Registry {
    pub server: String,
    pub username: String,
}

/// A relative path inside a tree: no `..`, not absolute, plain characters.
pub fn check_rel_path(what: &str, p: &str) -> Result<()> {
    let ok = !p.is_empty()
        && p.len() <= 200
        && !p.starts_with('/')
        && p.split('/').all(|seg| !seg.is_empty() && seg != ".." && seg != ".")
        && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./@+".contains(&b));
    if ok { Ok(()) } else { Err(Error::BadRequest(format!("{what} is a relative path inside the tree"))) }
}

/// `rel` under `tree`, reached only through real directories and ending in a directory (`dir`)
/// or a regular file. A pushed tree can hold symlinks, and a build root or Dockerfile reached
/// through one would be the host's.
pub fn in_tree(tree: &std::path::Path, what: &str, rel: &str, dir: bool) -> Result<std::path::PathBuf> {
    check_rel_path(what, rel)?;
    let mut at = tree.to_path_buf();
    let segs: Vec<&str> = rel.split('/').collect();
    for (i, seg) in segs.iter().enumerate() {
        at.push(seg);
        let last = i + 1 == segs.len();
        let real = std::fs::symlink_metadata(&at).is_ok_and(|m| if last && !dir { m.is_file() } else { m.is_dir() });
        if !real {
            let kind = if dir { "directory" } else { "file" };
            return Err(Error::BadRequest(format!("{what} {rel} isn't a {kind} in the tree (symlinks aren't followed)")));
        }
    }
    Ok(at)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Build {
    pub builder: Builder,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// A repo source deploys a push only when it changes a path these match (gitignore-style,
    /// from the repo's root, as CODEOWNERS): a monorepo's other apps don't redeploy this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch: Option<Vec<String>>,
    /// A single-page app (a page path with no file answers index.html), whatever the plan
    /// detected; unset: detected (a static build with a client-side router).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spa: Option<bool>,
    /// The tree's own Dockerfile's stage to build (`--target`), for a multi-stage one whose last
    /// stage isn't what runs (Fly's and Render's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Builder {
    #[default]
    Auto,
    Dockerfile,
}

impl Build {
    pub fn check(&self) -> Result<()> {
        if let Some(d) = &self.dockerfile {
            check_rel_path("build.dockerfile", d)?;
        }
        for (k, v) in [("install", &self.install), ("build", &self.build), ("start", &self.start)] {
            if v.as_ref().is_some_and(|c| c.len() > 2000 || c.contains('\0')) {
                return Err(Error::BadRequest(format!("build.{k} is a command of at most 2000 characters")));
            }
        }
        if let Some(w) = &self.watch
            && (w.len() > 50 || w.iter().any(|p| p.trim().is_empty() || p.len() > 200 || p.contains('\0')))
        {
            return Err(Error::BadRequest("build.watch is at most 50 path patterns of at most 200 characters".into()));
        }
        // A stage's name, as a Dockerfile's `FROM … AS <name>` has it (one argv word).
        if let Some(t) = &self.target
            && (t.is_empty() || t.len() > 64 || !t.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')) || t.starts_with('-'))
        {
            return Err(Error::BadRequest("build.target is a Dockerfile stage's name: letters, digits, _, - and . (at most 64)".into()));
        }
        Ok(())
    }

    pub fn overrides(&self) -> apps_build::Overrides {
        apps_build::Overrides {
            builder: match self.builder {
                Builder::Auto => apps_build::Builder::Auto,
                Builder::Dockerfile => apps_build::Builder::Dockerfile,
            },
            dockerfile: self.dockerfile.clone(),
            install: self.install.clone(),
            build: self.build.clone(),
            start: self.start.clone(),
            spa: self.spa,
            build_env: Vec::new(),
            process: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunType {
    #[default]
    Web,
    Worker,
    Cron,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    pub mount: String,
    pub size_gb: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Run {
    #[serde(rename = "type")]
    pub kind: RunType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// Runs once per deploy in a container of the new image, before anything switches (a
    /// migration): a failure fails the deploy and the old deployment keeps serving.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_deploy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// The zone a cron schedule's fields are in (IANA, `Europe/Budapest`); UTC when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u32>,
    pub min_instances: u32,
    pub max_instances: u32,
    pub concurrency: u32,
    pub cpu: f64,
    pub memory_mb: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_path: Option<String>,
    pub health_timeout_s: u32,
    pub suspend_after_s: u32,
    pub sleep_after_s: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<Volume>,
    /// When its metrics notify (`alert.*` events), over five minutes.
    pub alerts: Alerts,
    /// How a new deployment takes over: at once, or a share of requests at a time.
    pub rollout: Rollout,
    /// The app speaks HTTP/2 without TLS (h2c, prior knowledge), as a gRPC server does: the
    /// proxy and its health checks talk HTTP/2 to it (Cloud Run's `--use-http2`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub h2c: bool,
    /// A worker's instances follow its work: a stream consumer group's lag (docs/apps/README.md,
    /// "Workers that follow their work"). Between `min_instances` and `max_instances`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<Scale>,
    /// A web service answers on its public URLs (its `up.roll.site` names, its deployments'
    /// and its custom domains); false: only on the project's private network, as Render's
    /// private services.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub public: bool,
    /// How long an instance being replaced or scaled in gets: its requests in flight finish,
    /// then SIGTERM, and SIGKILL when it's up (Render's `maxShutdownDelaySeconds`). 1–300;
    /// unset, 30.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drain_s: Option<u32>,
    /// A web service also scales on CPU (Render's target CPU): while its awake instances
    /// average over this percent of their CPU for a minute another starts (up to
    /// `max_instances`), and fewer stay once they'd be under it. 10–100; unset, requests only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_target: Option<u32>,
    /// A worker that stops (Railway's restart policy): started again `always`, only when it
    /// failed (`on_failure`, up to `restart_tries` in a row), or `never`. A deploy or a restart
    /// starts fresh ones whatever it says.
    #[serde(skip_serializing_if = "Restart::is_always")]
    pub restart: Restart,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_tries: Option<u32>,
    /// A visitor keeps going to the same instance while it's up and has room (Cloud Run's
    /// session affinity: a cookie), for apps that keep sessions in memory.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub affinity: bool,
    /// Its port speaks TCP, not HTTP (Railway's TCP proxy: a game server, MQTT, a database of
    /// its own): each environment answers on a port of its own from the node's TCP range,
    /// woken and spliced per connection; its HTTP names don't serve it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub tcp: bool,
    /// A deployment that falls into a crash loop within ten minutes of going live is replaced
    /// by the one before it (its image, no build): a gradual rollout's error rollback for an
    /// at-once deploy. Never a rollback's own: no two in a row.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rollback_on_crash: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Restart {
    #[default]
    Always,
    OnFailure,
    Never,
}

impl Restart {
    fn is_always(&self) -> bool {
        *self == Restart::Always
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Restart::Always => "always",
            Restart::OnFailure => "on_failure",
            Restart::Never => "never",
        }
    }
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

/// What a worker scales on: `group`'s lag on `stream` (a roll stream in the service's project),
/// one instance per `per_instance` messages behind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scale {
    pub stream: String,
    pub group: String,
    #[serde(default = "Scale::default_per_instance")]
    pub per_instance: u64,
}

impl Scale {
    fn default_per_instance() -> u64 {
        1000
    }

    /// Instances for a backlog of `lag`, within `min`–`max`.
    pub fn instances(&self, lag: u64, min: u32, max: u32) -> u32 {
        (lag.div_ceil(self.per_instance.max(1)).min(u32::MAX as u64) as u32).clamp(min, max.max(min))
    }
}

/// The run settings an environment may set for itself (Railway's per-environment settings):
/// its size and scale, never what it runs.
pub const ENV_RUN_FIELDS: [&str; 7] = ["min_instances", "max_instances", "cpu", "memory_mb", "concurrency", "suspend_after_s", "sleep_after_s"];

/// A gradual rollout (Vercel's rolling releases, Cloud Run's traffic splits): the new
/// deployment takes each of `steps` percent of requests for `step_s` seconds, next to the old
/// one, then all of them. A step in which the new deployment answers `error_pct` or more of its
/// requests (at least 20) with a 5xx, and more than the old one does, rolls it back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Rollout {
    /// Increasing percents, 1–99, at most five; empty (the default) switches at once.
    pub steps: Vec<u8>,
    pub step_s: u32,
    pub error_pct: u8,
}

impl Default for Rollout {
    fn default() -> Self {
        Rollout { steps: Vec::new(), step_s: 120, error_pct: 5 }
    }
}

impl Rollout {
    /// The fewest requests a step judges by.
    pub const MIN_REQUESTS: u64 = 20;
}

/// Metric and log alerts (Railway's monitors): each 0 (or none) is off. Judged every minute
/// over the last five.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Alerts {
    /// Memory at or above this share of the limit, per instance, every minute of the five.
    pub memory_pct: u8,
    /// 5xx answers at or above this share of the five minutes' requests (at least 20 of them).
    pub error_pct: u8,
    /// p95 latency at or above this in three of the five minutes.
    pub p95_ms: u32,
    /// Text its output (stdout and stderr, any case) holds at least `log_count` times in the
    /// five minutes: `alert.log` (the pattern and the count, never a line).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_match: Option<String>,
    pub log_count: u32,
}

impl Default for Alerts {
    fn default() -> Self {
        Alerts { memory_pct: 90, error_pct: 5, p95_ms: 0, log_match: None, log_count: 10 }
    }
}

impl Default for Run {
    fn default() -> Self {
        Run {
            kind: RunType::Web,
            port: None,
            start: None,
            pre_deploy: None,
            schedule: None,
            timezone: None,
            timeout_s: None,
            min_instances: 0,
            max_instances: 3,
            concurrency: 50,
            cpu: 0.5,
            memory_mb: 512,
            health_path: None,
            health_timeout_s: 60,
            suspend_after_s: 30,
            sleep_after_s: 600,
            volume: None,
            alerts: Alerts::default(),
            rollout: Rollout::default(),
            h2c: false,
            scale: None,
            public: true,
            drain_s: None,
            cpu_target: None,
            restart: Restart::Always,
            restart_tries: None,
            affinity: false,
            tcp: false,
            rollback_on_crash: false,
        }
    }
}

impl Run {
    /// The checked settings, with what the type implies filled in (a worker runs at least one
    /// instance; a volume allows one).
    pub fn normalized(mut self) -> Result<Run> {
        if self.alerts.memory_pct > 100 || self.alerts.error_pct > 100 {
            return Err(Error::BadRequest("alerts.memory_pct and alerts.error_pct are 0–100 (0 is off)".into()));
        }
        if self.alerts.log_match.as_ref().is_some_and(|m| m.trim().is_empty() || m.len() > 200) || !(1..=10_000).contains(&self.alerts.log_count) {
            return Err(Error::BadRequest("alerts.log_match is 1–200 characters, alerts.log_count 1–10000".into()));
        }
        let bad = |m: &str| Err(Error::BadRequest(format!("run.{m}")));
        let r = &self.rollout;
        if r.steps.len() > 5 || r.steps.iter().any(|s| !(1..=99).contains(s)) || r.steps.windows(2).any(|w| w[0] >= w[1]) {
            return bad("rollout.steps is up to five increasing percents, 1–99");
        }
        if !(1..=3600).contains(&r.step_s) {
            return bad("rollout.step_s is 1–3600");
        }
        if !(1..=100).contains(&r.error_pct) {
            return bad("rollout.error_pct is 1–100");
        }
        if self.kind != RunType::Worker {
            // A web service scales on its requests, a cron run is one: only a worker follows a
            // backlog (dropped, so a worker can become a web service).
            self.scale = None;
        }
        if let Some(sc) = &self.scale {
            // A stream by its id (`st_…`), never a name; a group as Kafka names one.
            let stream_ok = sc.stream.strip_prefix("st_").is_some_and(|r| r.len() == 12 && r.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
            let group_ok = !sc.group.is_empty() && sc.group.len() <= 249 && sc.group != "." && sc.group != ".." && sc.group.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
            if !stream_ok || !group_ok || !(1..=1_000_000_000).contains(&sc.per_instance) {
                return bad("scale is { stream: a stream's id (st_…), group: a-z, A-Z, 0-9, . _ -, per_instance: 1–1e9 }");
            }
        }
        if !(0.25..=8.0).contains(&self.cpu) || !self.cpu.is_finite() {
            return bad("cpu is 0.25–8 vCPU");
        }
        if !(256..=16384).contains(&self.memory_mb) {
            return bad("memory_mb is 256–16384");
        }
        if self.max_instances == 0 || self.max_instances > 100 {
            return bad("max_instances is 1–100");
        }
        if self.min_instances > self.max_instances {
            return bad("min_instances is at most max_instances");
        }
        if !(1..=1000).contains(&self.concurrency) {
            return bad("concurrency is 1–1000");
        }
        if !(1..=900).contains(&self.health_timeout_s) {
            return bad("health_timeout_s is 1–900");
        }
        if self.drain_s.is_some_and(|d| !(1..=300).contains(&d)) {
            return bad("drain_s is 1–300");
        }
        if self.affinity && self.kind != RunType::Web {
            return bad("affinity is for a web service");
        }
        if self.tcp {
            if self.kind != RunType::Web {
                return bad("tcp is for a service with a port (type web)");
            }
            if self.health_path.is_some() || self.h2c || self.affinity || !self.rollout.steps.is_empty() {
                return bad("a TCP service has no HTTP: no health_path, h2c, affinity or gradual rollout (its health is its port accepting)");
            }
        }
        if self.restart != Restart::Always && self.kind != RunType::Worker {
            return bad("restart is for a worker (a web service starts on its requests, a cron run once)");
        }
        if let Some(t) = self.restart_tries {
            if self.restart != Restart::OnFailure {
                return bad("restart_tries goes with restart = on_failure");
            }
            if !(1..=100).contains(&t) {
                return bad("restart_tries is 1–100");
            }
        }
        if let Some(t) = self.cpu_target {
            if !(10..=100).contains(&t) {
                return bad("cpu_target is 10–100 (percent of an instance's CPU)");
            }
            if self.kind != RunType::Web {
                return bad("cpu_target is for a web service (a worker follows its work with scale)");
            }
        }
        if self.port == Some(0) {
            return bad("port is 1–65535");
        }
        if let Some(p) = &self.health_path
            && (!p.starts_with('/') || p.len() > 200 || !p.bytes().all(|b| b.is_ascii_graphic()))
        {
            return bad("health_path is a path starting with /");
        }
        if self.start.as_ref().is_some_and(|c| c.len() > 2000 || c.contains('\0')) {
            return bad("start is a command of at most 2000 characters");
        }
        if self.pre_deploy.as_ref().is_some_and(|c| c.trim().is_empty() || c.len() > 2000 || c.contains('\0')) {
            return bad("pre_deploy is a command of at most 2000 characters");
        }
        if let Some(v) = &self.volume {
            if !v.mount.starts_with('/') || v.mount == "/" || v.mount.len() > 200 || v.mount.contains("..") {
                return bad("volume.mount is an absolute path other than /");
            }
            if !(1..=500).contains(&v.size_gb) {
                return bad("volume.size_gb is 1–500");
            }
            self.max_instances = 1;
            self.min_instances = self.min_instances.min(1);
            if !self.rollout.steps.is_empty() {
                return bad("rollout: a service with a volume switches at once (one instance writes it)");
            }
        }
        if self.kind != RunType::Web {
            // Only requests can be split: a worker or a cron run switches at once.
            self.rollout.steps.clear();
        }
        match self.kind {
            RunType::Cron => {
                let Some(s) = &self.schedule else { return bad("schedule is required for a cron service") };
                crate::cron::Schedule::parse(s, self.timezone.as_deref())?;
                if self.timeout_s.is_some_and(|t| !(1..=86_400).contains(&t)) {
                    return bad("timeout_s is 1–86400");
                }
                self.min_instances = 0;
            }
            RunType::Worker => {
                self.schedule = None;
                self.timezone = None;
                self.min_instances = self.min_instances.max(1);
                self.max_instances = self.max_instances.max(self.min_instances);
            }
            RunType::Web => {
                self.schedule = None;
                self.timezone = None;
                // A request's time to its answer (Cloud Run's request timeout, at most an hour).
                if self.timeout_s.is_some_and(|t| !(1..=3600).contains(&t)) {
                    return bad("timeout_s is 1–3600 for a web service (the time a request has to its answer)");
                }
            }
        }
        Ok(self)
    }

    /// Merge a partial JSON object over these settings (PATCH and creation defaults).
    pub fn merged(&self, patch: Option<&serde_json::Value>) -> Result<Run> {
        merge(self, patch, "run").and_then(Run::normalized)
    }
}

/// `base` with the fields of the JSON object `patch` replaced, parsed back as `T` (unknown
/// fields are refused by `T`; a `null` field goes back to its default).
pub fn merge<T: Serialize + serde::de::DeserializeOwned>(base: &T, patch: Option<&serde_json::Value>, what: &str) -> Result<T> {
    let mut v = serde_json::to_value(base).expect("JSON");
    if let Some(patch) = patch {
        let serde_json::Value::Object(p) = patch else { return Err(Error::BadRequest(format!("{what} is an object"))) };
        let o = v.as_object_mut().expect("an object");
        for (k, val) in p {
            if val.is_null() {
                o.remove(k);
            } else {
                o.insert(k.clone(), val.clone());
            }
        }
    }
    serde_json::from_value(v).map_err(|e| Error::BadRequest(format!("{what}: {e}")))
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Scaling {
    pub backlog: Option<u64>,
    pub wanted: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Canary {
    pub deployment: String,
    pub share: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Service {
    pub id: String,
    pub project: String,
    pub name: String,
    pub source: Source,
    pub build: Build,
    pub run: Run,
    /// The private registry login its image pulls with (never the password).
    pub registry: Option<Registry>,
    /// What it links (the canvas draws them), without their variables.
    pub links: Vec<LinkRef>,
    pub created_at: i64,
    pub environments: Vec<EnvironmentSummary>,
}

/// A link's resource, as `Service.links` names it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LinkRef {
    pub kind: LinkKind,
    pub r#ref: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceState {
    None,
    Stopped,
    Suspended,
    Waking,
    Running,
    Crashed,
    Deploying,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeployRef {
    pub id: String,
    pub revision: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LatestRef {
    pub id: String,
    pub status: DeployStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EnvironmentSummary {
    pub name: String,
    pub state: InstanceState,
    pub url: Option<String>,
    pub active: Option<DeployRef>,
    pub latest: Option<LatestRef>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Environment {
    #[serde(flatten)]
    pub summary: EnvironmentSummary,
    pub service: String,
    pub branch: Option<String>,
    pub ephemeral: bool,
    pub instances: Vec<Instance>,
    pub domains: Vec<Domain>,
    pub port: Option<u16>,
    /// A TCP service's address (`host:port`), once it has its port.
    pub tcp: Option<String>,
    pub needs_deploy: bool,
    /// Visitors need the environment's access token (`PUT … {access: {token}}`).
    pub private: bool,
    /// A cron service's next scheduled run (unix ms), once it has a deployment.
    pub next_run_at: Option<i64>,
    /// Its instances keep crashing: three or more in the last ten minutes.
    pub crash_loop: Option<CrashLoop>,
    /// A gradual rollout under way: the new deployment and the percent of requests it takes.
    pub canary: Option<Canary>,
    /// Its own run settings over the service's ([`ENV_RUN_FIELDS`]); empty: the service's.
    pub run: serde_json::Map<String, serde_json::Value>,
    /// It has a deploy hook (`POST …/hook`).
    pub hook: bool,
    /// An image service's tag is watched: a new digest behind it deploys (Railway's image auto
    /// updates).
    pub follow_tag: bool,
    /// Redeploys (the same image, the new values) when the project's secrets change.
    pub redeploy_on_secrets: bool,
    /// A preview's pages carry roll previews' feedback button for visitors who came in through
    /// its access link.
    pub feedback: bool,
    /// In maintenance (Render's maintenance mode): its public names answer a 503 page with the
    /// message, and nothing wakes. Null: serving.
    pub maintenance: Option<Maintenance>,
    /// Its own pages for what roll would otherwise answer (Heroku's error and maintenance pages).
    #[serde(skip_serializing_if = "Pages::is_empty")]
    pub pages: Pages,
    /// Only these addresses get in through its public names (empty: anyone).
    pub allow_ips: Vec<IpRule>,
    /// Each client's requests a minute through its public names (null: no limit).
    pub rate_limit: Option<RateLimit>,
    /// Its firewall rules, in order ([]: none).
    pub firewall: Vec<FirewallRule>,
    /// Headers its answers carry ([]: none).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HeaderRule>,
    /// A worker that follows its work: the backlog it last read (null before the first read)
    /// and the instances that wants.
    pub scaling: Option<Scaling>,
    /// A push deploys only once its commit's checks pass (Railway's "Wait for CI").
    pub wait_for_ci: bool,
    /// The push waiting for its checks, if one is.
    pub waiting_for_ci: Option<String>,
    pub created_at: i64,
}

/// An address range let into a public service (Render's IP allow list).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IpRule {
    /// A network (`203.0.113.0/24`, `2001:db8::/32`), kept in its canonical form.
    pub cidr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One firewall rule (Vercel's firewall rules): what it matches, and whether it lets in or
/// keeps out. Every field given must match; the first rule that does decides.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirewallRule {
    pub action: FirewallAction,
    /// A path, or a prefix ending in `*` (`/admin/*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Methods (`GET`, `POST`…); empty: any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    /// Text the `User-Agent` contains (case aside).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FirewallAction {
    Allow,
    Deny,
}

/// The most rules an environment has.
pub const MAX_FIREWALL_RULES: usize = 50;

/// A request's path as an app is likely to read it, for matching rules: percent-decoded, `//`
/// one `/`, and `.`/`..` resolved (so `/%61dmin` and `/x/../admin` are `/admin`).
pub fn normal_path(raw: &str) -> String {
    let decoded = percent_encoding::percent_decode_str(raw).decode_utf8_lossy();
    let mut out: Vec<&str> = Vec::new();
    // A segment's `;params` (servlet containers read `/admin;x` as `/admin`) don't count.
    for seg in decoded.split('/').map(|s| s.split(';').next().unwrap_or("")) {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut p = format!("/{}", out.join("/"));
    if decoded.ends_with('/') && p.len() > 1 {
        p.push('/');
    }
    p
}

impl FirewallRule {
    /// Checked as given, and kept in the form it's matched in (methods upper case, the user
    /// agent lower case).
    pub fn check(mut self) -> Result<FirewallRule> {
        let bad = |m: &str| Err(Error::BadRequest(format!("firewall: {m}")));
        if let Some(p) = &self.path
            && (!p.starts_with('/') || p.len() > 512 || p.strip_suffix('*').unwrap_or(p).contains('*'))
        {
            return bad("a path starts with / (at most 512 characters, `*` only at its end)");
        }
        if self.methods.len() > 9 || self.methods.iter().any(|m| m.is_empty() || m.len() > 16 || !m.bytes().all(|b| b.is_ascii_alphabetic())) {
            return bad("methods are HTTP methods (GET, POST…)");
        }
        self.methods = self.methods.iter().map(|m| m.to_ascii_uppercase()).collect();
        if let Some(ua) = &self.user_agent {
            if ua.trim().is_empty() || ua.len() > 200 {
                return bad("user_agent is 1–200 characters");
            }
            self.user_agent = Some(ua.trim().to_ascii_lowercase());
        }
        if self.path.is_none() && self.methods.is_empty() && self.user_agent.is_none() {
            return bad("a rule matches on a path, methods or a user agent");
        }
        Ok(self)
    }

    /// `path` as [`normal_path`] gives it. Paths compare in any case and without a trailing
    /// slash (`/admin/*` takes `/admin` too); HEAD is a GET.
    pub fn matches(&self, method: &str, path: &str, user_agent: &str) -> bool {
        let path_ok = self.path.as_deref().is_none_or(|p| path_matches(p, path));
        let method = if method.eq_ignore_ascii_case("HEAD") { "GET" } else { method };
        let method_ok = self.methods.is_empty() || self.methods.iter().any(|m| m.eq_ignore_ascii_case(method));
        let ua_ok = self.user_agent.as_deref().is_none_or(|u| user_agent.to_ascii_lowercase().contains(u));
        path_ok && method_ok && ua_ok
    }
}

/// Whether `path` ([`normal_path`]'s) is `pattern`: a path, or a prefix ending in `*` (which
/// takes the prefix itself too); any case, no trailing slash.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let trim = |p: &str| if p.len() > 1 { p.trim_end_matches('/').to_ascii_lowercase() } else { p.to_ascii_lowercase() };
    let path = trim(path);
    match pattern.strip_suffix('*') {
        Some(prefix) => {
            let prefix = prefix.to_ascii_lowercase();
            path.starts_with(&prefix) || path == trim(&prefix)
        }
        None => path == trim(pattern),
    }
}

/// Headers an environment's answers carry (HSTS, a CSP, `X-Frame-Options`), on the paths a rule
/// names: Vercel's and Netlify's `headers`. The app's own header of the same name wins unless
/// the rule `replace`s it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderRule {
    /// A path, or a prefix ending in `*` (`/assets/*`); none: every path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub set: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replace: bool,
}

/// The most header rules an environment has.
pub const MAX_HEADER_RULES: usize = 20;

impl HeaderRule {
    /// Checked as given, names kept lower case. Never one that frames the answer or is roll's
    /// own (`content-length`, `set-cookie`, `x-roll-*`…).
    pub fn check(mut self) -> Result<HeaderRule> {
        let bad = |m: String| Err(Error::BadRequest(format!("headers: {m}")));
        if let Some(p) = &self.path
            && (!p.starts_with('/') || p.len() > 512 || p.strip_suffix('*').unwrap_or(p).contains('*'))
        {
            return bad("a path starts with / (at most 512 characters, `*` only at its end)".into());
        }
        if self.set.is_empty() || self.set.len() > 20 {
            return bad("a rule sets 1–20 headers".into());
        }
        const FORBIDDEN: [&str; 12] = ["content-length", "transfer-encoding", "connection", "keep-alive", "upgrade", "host", "te", "trailer", "set-cookie", "content-encoding", "proxy-authenticate", "proxy-connection"];
        let mut set = std::collections::BTreeMap::new();
        for (k, v) in &self.set {
            let name = k.trim().to_ascii_lowercase();
            if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)) {
                return bad(format!("{k:?} isn't a header name"));
            }
            if FORBIDDEN.contains(&name.as_str()) || name.starts_with("x-roll-") || name.starts_with("proxy-") {
                return bad(format!("{name} is the connection's or roll's to set"));
            }
            if v.len() > 1024 || !v.bytes().all(|b| b == b'\t' || (b' '..=b'~').contains(&b)) {
                return bad(format!("{name}'s value is at most 1024 printable characters"));
            }
            set.insert(name, v.trim().to_string());
        }
        self.set = set;
        Ok(self)
    }

    pub fn matches(&self, path: &str) -> bool {
        self.path.as_deref().is_none_or(|p| path_matches(p, path))
    }
}

/// Requests a minute each client may make through an environment's public names, with a burst
/// over that rate (a token bucket): Vercel's firewall rate limits.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    pub per_minute: u32,
    /// How many may come at once (default: a minute's worth).
    #[serde(default)]
    pub burst: Option<u32>,
}

impl RateLimit {
    pub fn check(&self) -> Result<()> {
        if !(1..=100_000).contains(&self.per_minute) {
            return Err(Error::BadRequest("rate_limit.per_minute is 1–100000".into()));
        }
        if self.burst.is_some_and(|b| !(1..=100_000).contains(&b)) {
            return Err(Error::BadRequest("rate_limit.burst is 1–100000".into()));
        }
        Ok(())
    }

    pub fn burst(&self) -> u32 {
        self.burst.unwrap_or(self.per_minute)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Maintenance {
    /// What visitors read; null: roll's own words.
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CrashLoop {
    /// In the last ten minutes.
    pub crashes: u32,
    pub last_at: i64,
    pub last_error: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Instance {
    pub id: String,
    pub deployment: String,
    pub state: InstanceState,
    pub started_at: Option<i64>,
    pub in_flight: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeployStatus {
    Queued,
    Building,
    Deploying,
    Active,
    Inactive,
    Failed,
    Canceled,
    Superseded,
    Removed,
    /// Built and health-checked, never live (a `no_traffic` deploy): its own URL runs it, and a
    /// rollback to it puts it live.
    Ready,
}

impl DeployStatus {
    pub const ALL: [DeployStatus; 10] = [
        DeployStatus::Queued,
        DeployStatus::Building,
        DeployStatus::Deploying,
        DeployStatus::Active,
        DeployStatus::Inactive,
        DeployStatus::Failed,
        DeployStatus::Canceled,
        DeployStatus::Superseded,
        DeployStatus::Removed,
        DeployStatus::Ready,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DeployStatus::Queued => "queued",
            DeployStatus::Building => "building",
            DeployStatus::Deploying => "deploying",
            DeployStatus::Active => "active",
            DeployStatus::Inactive => "inactive",
            DeployStatus::Failed => "failed",
            DeployStatus::Canceled => "canceled",
            DeployStatus::Superseded => "superseded",
            DeployStatus::Removed => "removed",
            DeployStatus::Ready => "ready",
        }
    }

    pub fn parse(s: &str) -> Option<DeployStatus> {
        DeployStatus::ALL.into_iter().find(|d| d.as_str() == s)
    }

    /// Still to finish: queued or in progress.
    pub fn pending(self) -> bool {
        matches!(self, DeployStatus::Queued | DeployStatus::Building | DeployStatus::Deploying)
    }
}

/// Why a deployment was made and who asked.
#[derive(Clone, Debug)]
pub struct By {
    pub trigger: Trigger,
    pub actor: Option<String>,
}

impl By {
    pub fn roll(trigger: Trigger) -> By {
        By { trigger, actor: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    Push,
    Manual,
    Rollback,
    Upload,
    Api,
    Settings,
    /// Another environment's live image (`POST …/promote`).
    Promote,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Push => "push",
            Trigger::Manual => "manual",
            Trigger::Rollback => "rollback",
            Trigger::Upload => "upload",
            Trigger::Api => "api",
            Trigger::Settings => "settings",
            Trigger::Promote => "promote",
        }
    }

    pub fn parse(s: &str) -> Trigger {
        match s {
            "push" => Trigger::Push,
            "rollback" => Trigger::Rollback,
            "upload" => Trigger::Upload,
            "api" => Trigger::Api,
            "settings" => Trigger::Settings,
            "promote" => Trigger::Promote,
            _ => Trigger::Manual,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Deployment {
    pub id: String,
    pub service: String,
    pub environment: String,
    pub status: DeployStatus,
    pub trigger: Trigger,
    pub revision: Option<String>,
    pub message: String,
    pub image: Option<String>,
    pub plan: Option<serde_json::Value>,
    pub rollback_of: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub ready_ms: Option<i64>,
    /// Who asked for it: a push's commit author, the token or connection that called the API
    /// (`operator` for the operator's token), none when roll did it on its own or nothing said.
    pub actor: Option<String>,
    /// What the app or the build printed when it failed (the step, its last line): shown to its
    /// owner, never in a commit status or a notification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<String>,
    /// When it was pruned (unix ms): out of what's kept, its image gone; history only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pruned_at: Option<i64>,
    /// The settings this deployment runs with (frozen when it was made).
    #[serde(skip)]
    pub run: Run,
    /// A hash of the variables it was made with (names and values), to tell when they changed.
    #[serde(skip)]
    pub vars_hash: String,
    /// When each step began (`build`, `pre_deploy`, `rollout`, then `live` or `failed`), on a
    /// single deployment's GET; the list leaves them out.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Step {
    pub step: String,
    pub at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DomainKind {
    Roll,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DomainStatus {
    Pending,
    Issuing,
    Active,
    Failed,
}

impl DomainStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DomainStatus::Pending => "pending",
            DomainStatus::Issuing => "issuing",
            DomainStatus::Active => "active",
            DomainStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> DomainStatus {
        match s {
            "issuing" => DomainStatus::Issuing,
            "active" => DomainStatus::Active,
            "failed" => DomainStatus::Failed,
            _ => DomainStatus::Pending,
        }
    }
}

/// A domain's certificate (`integrations::DomainTls`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsStatus {
    /// Nothing here issues certificates for custom domains (a node without the edge).
    None,
    Pending,
    Issued,
    Failed,
}

impl TlsStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TlsStatus::None => "none",
            TlsStatus::Pending => "pending",
            TlsStatus::Issued => "issued",
            TlsStatus::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> TlsStatus {
        match s {
            "none" => TlsStatus::None,
            "issued" => TlsStatus::Issued,
            "failed" => TlsStatus::Failed,
            _ => TlsStatus::Pending,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Domain {
    pub hostname: String,
    pub kind: DomainKind,
    /// The DNS record a custom domain needs: `cname` (a subdomain), `txt` (an apex, whose
    /// provider flattens a CNAME or ALIAS there, proves itself with the TXT challenge), `none`.
    pub record: &'static str,
    pub status: DomainStatus,
    pub target: String,
    pub token: String,
    pub error: Option<String>,
    /// Its certificate, and why it failed or what it's waiting for.
    pub tls: TlsStatus,
    pub tls_error: Option<String>,
    pub created_at: i64,
    /// Requests to it are sent, `308`, to this hostname of the same environment (www → apex),
    /// path and query kept.
    pub redirect_to: Option<String>,
}

/// A hostname a user may add: lowercase labels of `[a-z0-9-]`, at least two, at most 253
/// characters, not an IP address and not under roll's own domains; or `*.` and one (a wildcard:
/// every name one label under it).
pub fn check_hostname(h: &str, reserved: &[&str]) -> Result<String> {
    let h = h.trim().trim_end_matches('.').to_ascii_lowercase();
    if let Some(base) = h.strip_prefix("*.") {
        // One `*`, leftmost: what a wildcard certificate covers.
        if base.contains('*') {
            return Err(Error::BadRequest(format!("{h:?}: a wildcard is one `*.` in front of a hostname")));
        }
        return check_hostname(base, reserved).map(|b| format!("*.{b}"));
    }
    let labels: Vec<&str> = h.split('.').collect();
    let ok = h.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| (1..=63).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !l.starts_with('-') && !l.ends_with('-'))
        && !labels.last().is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()));
    if !ok {
        return Err(Error::BadRequest(format!("{h:?} isn't a hostname")));
    }
    if let Some(d) = reserved.iter().find(|d| !d.is_empty() && (h == **d || h.ends_with(&format!(".{d}")))) {
        return Err(Error::BadRequest(format!("{h} is under {d}, which roll manages")));
    }
    Ok(h)
}

/// One allow-list entry as given: a network or bare address (a string, or `{cidr,
/// description?}`), kept canonical (`10.1.2.3/8` is `10.0.0.0/8`).
pub fn ip_rule(v: &serde_json::Value) -> Result<IpRule> {
    let (cidr, description) = match v {
        serde_json::Value::String(c) => (c.as_str(), None),
        serde_json::Value::Object(o) => {
            if let Some(k) = o.keys().find(|k| !["cidr", "description"].contains(&k.as_str())) {
                return Err(Error::BadRequest(format!("allow_ips: {k} isn't a field")));
            }
            let d = match o.get("description") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(d)) if d.chars().count() <= 200 => Some(d.trim().to_string()).filter(|d| !d.is_empty()),
                Some(_) => return Err(Error::BadRequest("allow_ips: a description is at most 200 characters".into())),
            };
            (o.get("cidr").and_then(serde_json::Value::as_str).unwrap_or(""), d)
        }
        _ => return Err(Error::BadRequest("allow_ips holds networks".into())),
    };
    let c = cidr.trim();
    let net = c
        .parse::<ipnet::IpNet>()
        .or_else(|_| c.parse::<std::net::IpAddr>().map(ipnet::IpNet::from))
        .map_err(|_| Error::BadRequest(format!("allow_ips: {c:?} isn't a network (a.b.c.d/n, an IPv6 /n, or an address)")))?;
    Ok(IpRule { cidr: net.trunc().to_string(), description })
}

/// Where a custom domain's TXT challenge goes: `_roll-challenge.<host>`, a wildcard's on the
/// name under it (DNS providers rarely take a `*` mid-name; a TXT name holds several values).
pub fn challenge_name(host: &str) -> String {
    format!("_roll-challenge.{}", host.strip_prefix("*.").unwrap_or(host))
}

/// The wildcard that would cover `host` (`a.example.com` → `*.example.com`), if it has one.
pub fn covering_wildcard(host: &str) -> Option<String> {
    let (_, rest) = host.split_once('.')?;
    rest.contains('.').then(|| format!("*.{rest}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkKind {
    Postgres,
    Redis,
    Bucket,
    Stream,
    Queue,
    Nosql,
    /// Another web service of the project: its private and public URLs (apps' own, not a
    /// [`crate::integrations::Resources`] kind).
    Service,
}

impl LinkKind {
    pub const ALL: [LinkKind; 7] = [LinkKind::Postgres, LinkKind::Redis, LinkKind::Bucket, LinkKind::Stream, LinkKind::Queue, LinkKind::Nosql, LinkKind::Service];

    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Postgres => "postgres",
            LinkKind::Redis => "redis",
            LinkKind::Bucket => "bucket",
            LinkKind::Stream => "stream",
            LinkKind::Queue => "queue",
            LinkKind::Nosql => "nosql",
            LinkKind::Service => "service",
        }
    }

    pub fn parse(s: &str) -> Result<LinkKind> {
        LinkKind::ALL.into_iter().find(|k| k.as_str() == s).ok_or_else(|| Error::BadRequest(format!("unknown link kind {s:?}")))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Link {
    pub kind: LinkKind,
    pub r#ref: String,
    pub prefix: String,
    pub vars: Vec<String>,
}

/// An environment variable's name: `[A-Za-z_][A-Za-z0-9_]*`, at most 128 characters.
pub fn check_var_name(n: &str) -> Result<()> {
    let mut b = n.bytes();
    let ok = (1..=128).contains(&n.len()) && b.next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_') && b.all(|c| c.is_ascii_alphanumeric() || c == b'_');
    if ok { Ok(()) } else { Err(Error::BadRequest(format!("{n:?} isn't a variable name ([A-Za-z_][A-Za-z0-9_]*)"))) }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VarView {
    pub name: String,
    pub masked: String,
    pub source: &'static str,
    /// A reference variable's template (`${{ api.KEY }}`): names, never a value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// Set and run with, never shown again: not by `…/variables/values`, nor its length here.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub sealed: bool,
}

/// An environment's own HTML for roll's pages (Heroku's `ERROR_PAGE_URL` and
/// `MAINTENANCE_PAGE_URL`, stored rather than fetched): `error` for the 502, 503 and 504 a
/// visitor gets while the app can't answer or wake, `maintenance` while it's in maintenance.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Pages {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<String>,
}

/// The most a page may weigh.
pub const PAGE_MAX: usize = 64 << 10;

impl Pages {
    pub fn is_empty(&self) -> bool {
        self.error.is_none() && self.maintenance.is_none()
    }
}

/// What a sealed value shows, whatever it is.
pub const SEALED_MASK: &str = "••••••••";

/// A value as the masked list shows it: dots, between 4 and 12 of them, so its length barely
/// shows either.
pub fn mask(v: &str) -> String {
    if v.is_empty() { String::new() } else { "•".repeat(v.chars().count().clamp(4, 12)) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
    Skipped,
    TimedOut,
    Canceled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Skipped => "skipped",
            RunStatus::TimedOut => "timed_out",
            RunStatus::Canceled => "canceled",
        }
    }

    pub fn parse(s: &str) -> RunStatus {
        match s {
            "succeeded" => RunStatus::Succeeded,
            "failed" => RunStatus::Failed,
            "skipped" => RunStatus::Skipped,
            "timed_out" => RunStatus::TimedOut,
            "canceled" => RunStatus::Canceled,
            _ => RunStatus::Running,
        }
    }
}

/// A one-off command in a fresh container of an environment's active deployment (`…/jobs`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Job {
    pub id: String,
    pub deployment: String,
    pub command: String,
    /// Containers it runs, each with `ROLL_TASK_INDEX` and `ROLL_TASK_COUNT` (Cloud Run Jobs').
    pub tasks: u32,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CronRun {
    pub id: String,
    pub deployment: String,
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

/// `<prefix>_` and 12 random characters of lowercase base32.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", random(12))
}

pub fn random(n: usize) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system randomness");
    b.iter().map(|x| ALPHABET[(x & 31) as usize] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_addresses_and_registries() {
        for a in ["127.0.0.1", "10.1.2.3", "100.64.0.1", "0.1.2.3", "192.0.0.8", "198.18.0.1", "240.0.0.1", "224.0.0.1", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1", "64:ff9b::a00:1", "::7f00:1", "2002:a00:1::", "ff02::1"] {
            assert!(private_ip(a.parse().unwrap()), "{a}");
        }
        for a in ["1.1.1.1", "8.8.8.8", "2606:4700::1111", "64:ff9b::808:808"] {
            assert!(!private_ip(a.parse().unwrap()), "{a}");
        }
        assert_eq!(registry_key("https://index.docker.io/v1/"), image_registry("nginx:1.27"));
        assert_eq!(registry_key("GHCR.io"), image_registry("ghcr.io/acme/web:1"));
        assert_ne!(registry_key("ghcr.io"), image_registry("evil.example/acme/web"));
    }

    #[test]
    fn settings_are_checked_and_types_imply_their_rules() {
        let run = Run::default().merged(Some(&serde_json::json!({"type": "worker"}))).unwrap();
        assert_eq!(run.min_instances, 1, "a worker runs at least one");
        let run = Run::default().merged(Some(&serde_json::json!({"volume": {"mount": "/data", "size_gb": 5}, "max_instances": 4}))).unwrap();
        assert_eq!(run.max_instances, 1, "a volume allows one instance");
        assert!(Run::default().merged(Some(&serde_json::json!({"type": "cron"}))).is_err(), "cron needs a schedule");
        assert!(Run::default().merged(Some(&serde_json::json!({"type": "cron", "schedule": "*/5 * * * *"}))).is_ok());
        assert!(Run::default().merged(Some(&serde_json::json!({"cpu": 64}))).is_err());
        assert!(Run::default().merged(Some(&serde_json::json!({"bogus": 1}))).is_err(), "unknown fields are refused");
        assert!(check_hostname("www.example.com", &["roll.site"]).is_ok());
        assert!(check_hostname("x.up.roll.site", &["roll.site"]).is_err());
        assert!(check_hostname("localhost", &[]).is_err());
        assert!(check_hostname("1.2.3.4", &[]).is_err());
        assert!(check_name("service", "My-App").is_err());
        assert!(check_var_name("DATABASE_URL").is_ok() && check_var_name("1X").is_err());
    }
}

/// What DNS answered for a custom domain that doesn't verify ([`domain_diagnosis`]).
pub struct DnsSeen {
    pub cnames: Vec<String>,
    /// At the challenge name.
    pub txts: Vec<String>,
    /// At the hostname itself (a TXT put on the wrong name).
    pub host_txts: Vec<String>,
    pub addresses: Vec<String>,
}

/// Why `host` isn't verified, from what DNS answered: what's missing and the likely mistake
/// (another target, a proxy answering with addresses, the TXT on the wrong name or with more
/// around the token), as Vercel's "Invalid configuration" says.
pub fn domain_diagnosis(host: &str, target: &str, challenge: &str, token: &str, seen: &DnsSeen, shared: bool) -> String {
    let list = |v: &[String]| v.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    let route = if seen.cnames.iter().any(|c| c == target) {
        None
    } else if !seen.cnames.is_empty() {
        Some(format!("{host} points at {} instead of {target}", list(&seen.cnames)))
    } else if !seen.addresses.is_empty() {
        Some(format!(
            "{host} answers with addresses ({}) instead of a CNAME to {target}: if its DNS is on Cloudflare, turn the proxy off for it (DNS only); an apex domain needs its provider's CNAME flattening or ALIAS",
            list(&seen.addresses)
        ))
    } else {
        Some(format!("{host} has no CNAME to {target} yet (a new record can take a few minutes to show)"))
    };
    let proof = if seen.txts.iter().any(|t| t == token) {
        None
    } else if seen.txts.iter().any(|t| t.contains(token)) {
        Some(format!("the TXT {challenge} holds the token with more around it (quotes?): it must be exactly {token}"))
    } else if !seen.txts.is_empty() {
        Some(format!("the TXT {challenge} holds something else ({})", list(&seen.txts)))
    } else if seen.host_txts.iter().any(|t| t.contains(token)) {
        Some(format!("the TXT with the token is on {host}; it goes on {challenge}"))
    } else {
        Some(format!("no TXT {challenge} with the token yet"))
    };
    match (shared, route, proof) {
        // Behind a shared target the CNAME routes it and only the TXT proves it's yours.
        (true, None, Some(p)) => format!("{p} (the CNAME to {target} routes it; the TXT record proves it's yours)"),
        (true, Some(r), Some(p)) => format!("{p}; and {r}"),
        (false, Some(r), Some(p)) => format!("{r}; or prove it with a TXT: {p}"),
        (_, Some(r), None) | (_, None, Some(r)) => r,
        (_, None, None) => format!("{host} is set up; it's checked again in a minute"),
    }
}

#[cfg(test)]
mod domain_diagnosis_tests {
    use super::*;

    fn seen(cnames: &[&str], txts: &[&str], host_txts: &[&str], addresses: &[&str]) -> DnsSeen {
        let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect();
        DnsSeen { cnames: v(cnames), txts: v(txts), host_txts: v(host_txts), addresses: v(addresses) }
    }

    #[test]
    fn says_what_dns_answered_and_the_likely_mistake() {
        let d = |s: &DnsSeen, shared| domain_diagnosis("www.shop.com", "edge.roll.site", "_roll-challenge.www.shop.com", "tok123", s, shared);
        assert!(d(&seen(&["old.herokudns.com"], &[], &[], &[]), false).starts_with("www.shop.com points at old.herokudns.com instead of edge.roll.site"));
        assert!(d(&seen(&[], &[], &[], &["104.21.4.1"]), false).contains("turn the proxy off"));
        assert!(d(&seen(&[], &[], &["tok123"], &[]), true).starts_with("the TXT with the token is on www.shop.com; it goes on _roll-challenge.www.shop.com"));
        assert!(d(&seen(&["edge.roll.site"], &["\"tok123\""], &[], &[]), true).contains("more around it (quotes?)"));
        assert!(d(&seen(&["edge.roll.site"], &["other"], &[], &[]), true).contains("holds something else (other)"));
        assert!(d(&seen(&[], &[], &[], &[]), false).contains("no CNAME to edge.roll.site yet"));
    }
}

