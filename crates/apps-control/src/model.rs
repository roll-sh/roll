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
    pub created_at: i64,
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

/// Metric alerts (Railway's monitors): each 0 is off. Judged every minute over the last five.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Alerts {
    /// Memory at or above this share of the limit, per instance, every minute of the five.
    pub memory_pct: u8,
    /// 5xx answers at or above this share of the five minutes' requests (at least 20 of them).
    pub error_pct: u8,
    /// p95 latency at or above this in three of the five minutes.
    pub p95_ms: u32,
}

impl Default for Alerts {
    fn default() -> Self {
        Alerts { memory_pct: 90, error_pct: 5, p95_ms: 0 }
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
                crate::cron::Schedule::parse(s)?;
                if self.timeout_s.is_some_and(|t| !(1..=86_400).contains(&t)) {
                    return bad("timeout_s is 1–86400");
                }
                self.min_instances = 0;
            }
            RunType::Worker => {
                self.schedule = None;
                self.min_instances = self.min_instances.max(1);
                self.max_instances = self.max_instances.max(self.min_instances);
            }
            RunType::Web => {
                self.schedule = None;
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
    /// A push deploys only once its commit's checks pass (Railway's "Wait for CI").
    pub wait_for_ci: bool,
    /// The push waiting for its checks, if one is.
    pub waiting_for_ci: Option<String>,
    pub created_at: i64,
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
/// characters, not an IP address and not under roll's own domains.
pub fn check_hostname(h: &str, reserved: &[&str]) -> Result<String> {
    let h = h.trim().trim_end_matches('.').to_ascii_lowercase();
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkKind {
    Postgres,
    Redis,
    Bucket,
    Stream,
    Queue,
    Nosql,
}

impl LinkKind {
    pub const ALL: [LinkKind; 6] = [LinkKind::Postgres, LinkKind::Redis, LinkKind::Bucket, LinkKind::Stream, LinkKind::Queue, LinkKind::Nosql];

    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Postgres => "postgres",
            LinkKind::Redis => "redis",
            LinkKind::Bucket => "bucket",
            LinkKind::Stream => "stream",
            LinkKind::Queue => "queue",
            LinkKind::Nosql => "nosql",
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
}

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
