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
    /// A public Git repository by URL (GitHub, GitLab, any smart-HTTP host), with no app
    /// installed: each deploy fetches its branch's head (`git_source`).
    Git {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root: Option<String>,
    },
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
            // Its address is checked again where private ones may be allowed (`Apps::check_source`).
            Source::Git { url, root } => {
                crate::git_source::check_url(url, true).map_err(Error::BadRequest)?;
                if let Some(r) = root {
                    check_rel_path("source.root", r)?;
                }
                Ok(())
            }
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
/// A project's log drain (`/v1/apps/projects/{project}/drains`): where its output goes. The URL
/// and the headers' values are never returned, only the host and the headers' names.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LogDrain {
    pub id: String,
    pub project: String,
    pub host: String,
    pub created_at: i64,
    /// What it's sent: `runtime` (the instances' output and roll's lines about them), `http`
    /// (the access log, a request a line) and `build`.
    pub sources: Vec<String>,
    /// Only these environments' (empty: every one).
    pub environments: Vec<String>,
    /// Only these services' (ids; empty: every one).
    pub services: Vec<String>,
    /// Preview environments' too.
    pub previews: bool,
    /// The names of the headers it's sent with (an API key's: their values are sealed).
    pub headers: Vec<String>,
    /// How its lines go: `ndjson` (roll's own, signed), `otlp` (OpenTelemetry logs over
    /// OTLP/HTTP JSON) or `syslog` (RFC 5424 over TLS, RFC 5425's framing).
    pub format: String,
}

/// How a drain's lines can go ([`LogDrain::format`]).
pub const DRAIN_FORMATS: [&str; 3] = ["ndjson", "otlp", "syslog"];

/// What a drain can be sent.
pub const DRAIN_SOURCES: [&str; 6] = ["runtime", "http", "build", "audit", "traces", "metrics"];

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NotifyTarget {
    pub id: String,
    pub project: String,
    /// `webhook`, `slack`, `discord`, `email` (the project's owner).
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

/// A private environment's gate: the visitors' token, people signed in to roll who may open the
/// project (`signed_in`), or both, and an automation bypass (CI). Secrets as SHA-256 hex
/// digests; none is kept.
#[derive(Clone, Debug, PartialEq)]
pub struct Access {
    pub token_hash: Option<String>,
    pub bypass_hash: Option<String>,
    pub signed_in: bool,
    /// Its password's argon2id hash ([`crate::password`]), never returned.
    pub password_hash: Option<String>,
}

/// A private registry's login for a service's image (`PUT …/registry`); the password is sealed
/// and never returned.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Registry {
    pub server: String,
    pub username: String,
}

/// A private Git repository's login for a Git URL service (`PUT …/git-login`): sent only to
/// `host`, the source URL's; the password (a token) is sealed and never returned.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GitLogin {
    pub host: String,
    pub username: String,
    /// The project's secret its password is read from at each use (roll secrets), if not sealed here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// A Git URL service connected to roll's GitHub App (`PUT …/github`): the installation that sees
/// `repo` (`owner/name`). Fetches and commit statuses go through it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GitHubLink {
    pub installation: u64,
    pub repo: String,
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
    /// System packages the generated Dockerfile installs (Railpack's `aptPkgs`, Heroku's
    /// Aptfile): apt or apk names, in every stage built on an image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packages: Option<Vec<String>>,
    /// The longest its build may take, in seconds (Render's, Cloud Build's): 60 up to the node's
    /// own limit (`ROLL_APPS_BUILD_TIMEOUT`, in minutes, 30 by default; a longer one is held to
    /// it); unset: the node's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Builder {
    #[default]
    Auto,
    Dockerfile,
    /// A static site's functions, as a Node service (`apps_build::functions`).
    Functions,
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
        if let Some(p) = &self.packages {
            apps_build::packages::check(p).map_err(|why| Error::BadRequest(format!("build.{why}")))?;
        }
        if self.timeout_s.is_some_and(|t| !(60..=6 * 3600).contains(&t)) {
            return Err(Error::BadRequest("build.timeout_s is 60–21600 seconds".into()));
        }
        Ok(())
    }

    pub fn overrides(&self) -> apps_build::Overrides {
        apps_build::Overrides {
            builder: match self.builder {
                Builder::Auto => apps_build::Builder::Auto,
                Builder::Dockerfile => apps_build::Builder::Dockerfile,
                Builder::Functions => apps_build::Builder::Functions,
            },
            dockerfile: self.dockerfile.clone(),
            install: self.install.clone(),
            build: self.build.clone(),
            start: self.start.clone(),
            spa: self.spa,
            packages: self.packages.clone().unwrap_or_default(),
            build_env: Vec::new(),
            build_files: Vec::new(),
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
    /// Its backups' schedule and retention (Fly's snapshot retention, Railway's schedules);
    /// none: one a day, the newest seven kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backups: Option<VolumeBackups>,
}

/// How often a volume is backed up and how many backups are kept.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeBackups {
    /// Hours between scheduled backups, 1–168; 0: none on a schedule (by hand only).
    pub every_hours: u32,
    /// The newest kept, 1–90 (scheduled and by hand alike).
    pub keep: u32,
}

impl VolumeBackups {
    pub const DEFAULT: VolumeBackups = VolumeBackups { every_hours: 24, keep: 7 };
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
    /// A cron run that fails or times out is tried again this many times (Cloud Run Jobs'
    /// `--max-retries`), each try its own run after a backoff; `cron.failed` only for the last.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retries: Option<u8>,
    pub min_instances: u32,
    pub max_instances: u32,
    pub concurrency: u32,
    pub cpu: f64,
    pub memory_mb: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_path: Option<String>,
    pub health_timeout_s: u32,
    /// Seconds between two health checks of a running instance (its `health_path`, or a
    /// worker's `health_command`); none: the node's (10).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_every_s: Option<u32>,
    /// Failed health checks in a row that replace an instance; none: the node's (3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_failures: Option<u32>,
    /// A worker's health check (Docker's `HEALTHCHECK CMD`, Kubernetes' exec probe): a shell
    /// command run in its instance (`sh -c`), healthy on exit 0, so a consumer that hangs with
    /// its process up is replaced. It runs code in production, so changing it is a deploy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_command: Option<String>,
    pub suspend_after_s: u32,
    pub sleep_after_s: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<Volume>,
    /// When its metrics notify (`alert.*` events), over five minutes.
    pub alerts: Alerts,
    /// Its service level objective: `alert.slo` when its error budget burns too fast, and the
    /// budget left on Metrics (`…/slo`). How it's watched only: changing it is a write.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slo: Option<Slo>,
    /// Its own Prometheus endpoint, scraped every 15 s on each running instance (Fly's
    /// `[metrics]`) and shown on Metrics with what an OpenTelemetry SDK sends.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Scrape>,
    /// Floors by the clock (AWS's scheduled actions, Azure's autoscale profiles): in any of a
    /// rule's weekly windows its `min_instances` is the floor, when higher.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub schedule_scale: Vec<ScaleWindow>,
    /// Counts of its log lines (Cloud Logging's log-based metrics): each minute, the lines of its
    /// output matching `pattern` (any case) as a gauge named `name`, on Metrics and for `scale`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub log_metrics: Vec<LogMetric>,
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
    /// And on memory (Render's target memory): while its awake instances average over this
    /// percent of their memory limit for a minute another starts, and fewer stay once they'd be
    /// under it. 10–100; unset, not on memory. Where both are set, the one that asks for more
    /// instances decides.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_target: Option<u32>,
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
    /// A built service's live deployment is built again, from the same revision and without the
    /// cache, when one of its base images moves in its registry (Railway's automatic security
    /// patches). Without it the environment only says so (`base_updated`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rebuild_on_base: bool,
    /// A web service's page loaded before a deploy keeps talking to the deployment that served
    /// it, for a day (Vercel's skew protection; [`crate::skew`]).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub skew_protection: bool,
    /// A web service's hard limit (Cloud Run's, Fly's `hard_limit`): with every instance at its
    /// `concurrency` and `max_instances` of them, a request waits up to this long (1–60 s) for a
    /// slot, then gets a 429. Without it the least busy instance takes it past its concurrency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_s: Option<u32>,
    /// A web service scaled on its answers' p95 too (Heroku's response-time autoscaling): over
    /// this many ms for a window, another instance, as `cpu_target` adds one (50–60,000).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_target_ms: Option<u32>,
    /// Commands on a schedule, each a job of the live deployment at its due times (Heroku
    /// Scheduler, Vercel crons): any service's, without a cron service of its own.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<Scheduled>,
    /// A roll stream's records POSTed to a web service's path as they arrive (Cloud Run's
    /// Pub/Sub push): a group's offset committed on each 2xx (`crate::push`). At most four.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub push: Vec<Push>,
    /// Containers beside each instance (Cloud Run's sidecars, Fly's multi-container machines):
    /// in its network namespace (they reach each other on localhost), started after it, frozen,
    /// stopped and removed with it. A web service's or a worker's; at most four.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sidecars: Vec<Sidecar>,
    /// A deploy whose image has a known vulnerability this severe or worse fails before it goes
    /// live (Cloud Run's Binary Authorization on Artifact Analysis): the scan then runs before
    /// the rollout, not beside it. A rollback is never held. Changing it is a deploy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_on: Option<Severity>,
}

/// A count of matching log lines a minute ([`Run::log_metrics`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogMetric {
    pub name: String,
    pub pattern: String,
}

/// A floor for some hours of the week ([`Run::schedule_scale`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScaleWindow {
    pub windows: Vec<crate::freeze::Window>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    pub min_instances: u32,
}

impl ScaleWindow {
    fn on(&self, now: i64) -> bool {
        crate::freeze::Freeze { until: None, windows: self.windows.clone(), timezone: self.timezone.clone(), reason: String::new() }.on(now)
    }
}

impl Run {
    /// These settings with the floor its schedule asks for at `now` (the reaper's view).
    pub fn at(&self, now: i64) -> Run {
        let floor = self.schedule_scale.iter().filter(|w| w.on(now)).map(|w| w.min_instances).max().unwrap_or(0);
        let mut r = self.clone();
        r.min_instances = r.min_instances.max(floor.min(r.max_instances));
        r
    }
}

/// Where an app's Prometheus endpoint is ([`crate::prom`]): its port (the app's own by default)
/// and path (`/metrics`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scrape {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default = "Scrape::default_path")]
    pub path: String,
}

impl Scrape {
    fn default_path() -> String {
        "/metrics".into()
    }
}

/// A service level objective ([`crate::slo`]): `objective`% of requests succeed (not 5xx) and,
/// with `latency_ms`, the same share of minutes keep p95 under it, over `window_days`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slo {
    pub objective: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    #[serde(default = "Slo::default_window")]
    pub window_days: u32,
}

impl Slo {
    fn default_window() -> u32 {
        28
    }
}

/// How severe a known vulnerability is, as the scanner reads it (the distribution's, else NVD's).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Unknown,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn parse(s: &str) -> Severity {
        match s.to_ascii_lowercase().as_str() {
            "critical" => Severity::Critical,
            "high" => Severity::High,
            "medium" => Severity::Medium,
            "low" => Severity::Low,
            _ => Severity::Unknown,
        }
    }
}

/// One known vulnerability in a deployment's image: a package at a version an advisory names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Vulnerability {
    /// The advisory: `CVE-2024-…`, `GHSA-…`, a distribution's.
    pub id: String,
    pub severity: Severity,
    pub package: String,
    pub installed: String,
    /// The first version that fixes it, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Where in the image: the OS's packages (`debian 12.15`) or a lockfile's path.
    pub target: String,
}

/// The most of a scan's findings kept (the most severe first); the counts are of all of them.
pub const MAX_VULNERABILITIES: usize = 2000;

/// A deployment's last scan ([`Deployment::vulnerabilities`]): counts by severity.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScanSummary {
    pub critical: u32,
    pub high: u32,
    pub medium: u32,
    pub low: u32,
    pub unknown: u32,
    /// How many have a fixed version to upgrade to.
    pub fixable: u32,
    pub scanned_at: i64,
    /// What scanned it (`trivy 0.76.1`).
    pub scanner: String,
    /// Why the scan didn't finish (the counts are then zero and mean nothing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Its SBOM is kept (`…/deploys/{dep}/sbom`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sbom: bool,
}

impl ScanSummary {
    pub fn of(found: &[Vulnerability], scanner: &str, at: i64) -> ScanSummary {
        let mut s = ScanSummary { scanned_at: at, scanner: scanner.to_string(), ..Default::default() };
        for v in found {
            *match v.severity {
                Severity::Critical => &mut s.critical,
                Severity::High => &mut s.high,
                Severity::Medium => &mut s.medium,
                Severity::Low => &mut s.low,
                Severity::Unknown => &mut s.unknown,
            } += 1;
            s.fixable += v.fixed.is_some() as u32;
        }
        s
    }

    /// How many are at `floor` or worse.
    pub fn at_least(&self, floor: Severity) -> u32 {
        [(Severity::Critical, self.critical), (Severity::High, self.high), (Severity::Medium, self.medium), (Severity::Low, self.low)].iter().filter(|(s, _)| *s >= floor).map(|(_, n)| n).sum()
    }
}

/// A container beside each instance ([`Run::sidecars`]): a proxy, an agent, a collector.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sidecar {
    /// Unique in the service: a-z, 0-9 and -, at most 20.
    pub name: String,
    /// Any image a registry this node may pull from has; pinned to its digest with each
    /// deployment, so a rollback runs the same one.
    pub image: String,
    /// Run as `sh -c <command>` instead of the image's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Plain settings for it, shown as they are (at most 50): never a secret.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// The environment's own variables it gets, by name (its secrets too): none unless named.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variables: Vec<String>,
    /// 0.1–2 (default 0.25), metered with the instance.
    #[serde(default = "Sidecar::default_cpu")]
    pub cpu: f64,
    /// 64–4096 (default 128), metered with the instance.
    #[serde(default = "Sidecar::default_memory")]
    pub memory_mb: u32,
}

impl Sidecar {
    fn default_cpu() -> f64 {
        0.25
    }

    fn default_memory() -> u32 {
        128
    }
}

/// A schedule's state in an environment ([`Environment::schedules`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScheduleState {
    pub name: String,
    /// The next due time (unix ms); none without a deployment or in a paused environment.
    pub next_run_at: Option<i64>,
    /// The job it last started, and how that went.
    pub last: Option<ScheduleRun>,
    /// A path schedule's last call, and how it answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_call: Option<ScheduleCall>,
}

/// A path schedule's call: when, its status (none: it never answered, `error` says why) and how
/// long it took.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScheduleCall {
    pub at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScheduleRun {
    pub job: String,
    pub status: RunStatus,
    pub started_at: i64,
}

/// A volume's measured size ([`Environment::volume_used`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VolumeUsed {
    pub bytes: u64,
    /// When it was measured (unix ms).
    pub at: i64,
}

/// A command on a schedule ([`Run::schedules`]): a job (`crate::jobs`) at each due time, skipped
/// while its last one still runs; a paused environment's move on without making up the runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scheduled {
    /// Unique in the service: a-z, 0-9 and -, at most 32.
    pub name: String,
    /// Five fields or `@hourly`, `@daily` … ([`crate::cron::Schedule`]).
    pub schedule: String,
    /// A command, each due time a job; or (empty) `path`'s.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    /// A web service's path each due time GETs instead (Vercel's crons): through its own proxy
    /// on the owner, `Authorization: Bearer $CRON_SECRET` when the environment has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The schedule's time zone (IANA; default UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// 1–86400 (default 3600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u32>,
}

/// One of `run.push`: a topic of a roll stream delivered to a path of one environment, a record
/// a POST, each partition in order, the group's offset committed on a 2xx (at least once).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Push {
    /// The stream, by its id (`st_…`), on the service's own project.
    pub stream: String,
    pub topic: String,
    /// The consumer group whose committed offsets say what was delivered.
    pub group: String,
    /// Where each record is POSTed: an absolute path, like `/events`.
    pub path: String,
    /// The environment that gets them (default `production`): one, so two never split a group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// A topic of the same stream a record goes to after `max_attempts` failed tries, so its
    /// partition moves on; without one a record is tried until it's taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_letter: Option<String>,
    /// 1–100 (default 5), with `dead_letter`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    /// How long one POST may take: 1–600 s (default 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u32>,
}

impl Push {
    pub fn environment(&self) -> &str {
        self.environment.as_deref().unwrap_or(PRODUCTION)
    }
}

/// A topic or group name as Kafka takes one.
fn kafka_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 249 && s != "." && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// A stream's id (`st_` and 12 of a-z, 0-9): never a name.
pub fn stream_id(s: &str) -> bool {
    s.strip_prefix("st_").is_some_and(|r| r.len() == 12 && r.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
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
/// or `metric`, a gauge of its own (its Prometheus endpoint's or its OpenTelemetry SDK's: a
/// queue's depth, jobs in flight; the largest any series of it says, or their `sum`): one
/// instance per `per_instance` of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scale {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stream: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub metric: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sum: bool,
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
    /// After each step passes, its split stays until a person says (Vercel's manual rolling
    /// releases, Cloud Run's held traffic split): `POST …/deploys/{dep}/rollout`, still judged
    /// for errors meanwhile.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub manual: bool,
    /// With `run.slo`: a step fails when the new deployment spends its error budget this many
    /// times as fast as it lasts (Argo Rollouts' analysis); default 14.4, the fast burn alert's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slo_burn: Option<f64>,
    /// A step fails when the new deployment's p95 response time is more than this many percent
    /// above the old one's (Flagger's request duration check); off unless set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p95_pct: Option<u32>,
}

impl Default for Rollout {
    fn default() -> Self {
        Rollout { steps: Vec::new(), step_s: 120, error_pct: 5, manual: false, slo_burn: None, p95_pct: None }
    }
}

impl Rollout {
    pub const SLO_BURN: f64 = 14.4;
}

/// What a person says to a rollout that waits for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RolloutAction {
    /// To the next step (or all requests after the last).
    Advance,
    /// To all requests now.
    All,
    /// Back to the deployment that was live.
    Abort,
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
    /// Requests that found every instance busy at `max_instances`, at or above this share of
    /// the five minutes' requests (at least 20 of them): `alert.capacity` (Cloud Run's
    /// max-instances warning, Heroku's queue time).
    pub capacity_pct: u8,
    /// Text its output (stdout and stderr, any case) holds at least `log_count` times in the
    /// five minutes: `alert.log` (the pattern and the count, never a line).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_match: Option<String>,
    pub log_count: u32,
}

impl Default for Alerts {
    fn default() -> Self {
        Alerts { memory_pct: 90, error_pct: 5, p95_ms: 0, capacity_pct: 10, log_match: None, log_count: 10 }
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
            retries: None,
            min_instances: 0,
            max_instances: 3,
            concurrency: 50,
            cpu: 0.5,
            memory_mb: 512,
            health_path: None,
            health_timeout_s: 60,
            health_every_s: None,
            health_failures: None,
            health_command: None,
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
            memory_target: None,
            restart: Restart::Always,
            restart_tries: None,
            affinity: false,
            tcp: false,
            rollback_on_crash: false,
            rebuild_on_base: false,
            skew_protection: false,
            queue_s: None,
            latency_target_ms: None,
            schedules: Vec::new(),
            push: Vec::new(),
            sidecars: Vec::new(),
            block_on: None,
            slo: None,
            metrics: None,
            schedule_scale: Vec::new(),
            log_metrics: Vec::new(),
        }
    }
}

impl Run {
    /// The checked settings, with what the type implies filled in (a worker runs at least one
    /// instance; a volume allows one).
    pub fn normalized(mut self) -> Result<Run> {
        if self.alerts.memory_pct > 100 || self.alerts.error_pct > 100 || self.alerts.capacity_pct > 100 {
            return Err(Error::BadRequest("alerts.memory_pct, alerts.error_pct and alerts.capacity_pct are 0–100 (0 is off)".into()));
        }
        if self.alerts.log_match.as_ref().is_some_and(|m| m.trim().is_empty() || m.len() > 200) || !(1..=10_000).contains(&self.alerts.log_count) {
            return Err(Error::BadRequest("alerts.log_match is 1–200 characters, alerts.log_count 1–10000".into()));
        }
        let bad = |m: &str| Err(Error::BadRequest(format!("run.{m}")));
        if self.schedules.len() > 20 {
            return bad("schedules is at most 20");
        }
        for (i, sc) in self.schedules.iter().enumerate() {
            if sc.name.is_empty() || sc.name.len() > 32 || !sc.name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') || self.schedules[..i].iter().any(|o| o.name == sc.name) {
                return bad("schedules' names are unique: a-z, 0-9 and -, at most 32");
            }
            crate::cron::Schedule::parse(&sc.schedule, sc.timezone.as_deref()).map_err(|e| Error::BadRequest(format!("run.schedules.{}: {e}", sc.name)))?;
            match &sc.path {
                Some(p) => {
                    if !sc.command.is_empty() {
                        return bad("a schedule runs a command or calls a path, not both");
                    }
                    if self.kind != RunType::Web || self.tcp {
                        return bad("a schedule's path is for a web service (it's called over HTTP)");
                    }
                    if !p.starts_with('/') || p.len() > 2000 || !p.bytes().all(|b| b.is_ascii_graphic()) {
                        return bad("a schedule's path is an absolute path, like /api/cron");
                    }
                }
                None if sc.command.trim().is_empty() || sc.command.len() > 2000 || sc.command.contains('\0') => return bad("schedules' commands are 1–2000 characters"),
                None => {}
            }
            if sc.timeout_s.is_some_and(|t| !(1..=86_400).contains(&t)) {
                return bad("schedules' timeout_s is 1–86400");
            }
        }
        if !self.push.is_empty() && (self.kind != RunType::Web || self.tcp) {
            return bad("push is a web service's (each record is an HTTP POST)");
        }
        if self.push.len() > 4 {
            return bad("push is at most four");
        }
        for (i, p) in self.push.iter().enumerate() {
            if !stream_id(&p.stream) || !kafka_name(&p.topic) || !kafka_name(&p.group) {
                return bad("push is [{ stream: a stream's id (st_…), topic, group: a-z, A-Z, 0-9, . _ -, path }]");
            }
            if !p.path.starts_with('/') || p.path.len() > 2000 || !p.path.bytes().all(|b| b.is_ascii_graphic()) {
                return bad("push's path is an absolute path, like /events");
            }
            if let Some(e) = &p.environment {
                check_name("push's environment", e)?;
            }
            if p.dead_letter.as_deref().is_some_and(|d| !kafka_name(d) || d == p.topic) {
                return bad("push's dead_letter is another topic of the stream");
            }
            if p.max_attempts.is_some_and(|m| p.dead_letter.is_none() || !(1..=100).contains(&m)) {
                return bad("push's max_attempts is 1–100, with a dead_letter");
            }
            if p.timeout_s.is_some_and(|t| !(1..=600).contains(&t)) {
                return bad("push's timeout_s is 1–600");
            }
            if self.push[..i].iter().any(|o| o.stream == p.stream && o.group == p.group && o.topic == p.topic) {
                return bad("push has one rule per stream, topic and group");
            }
        }
        if !self.sidecars.is_empty() && !matches!(self.kind, RunType::Web | RunType::Worker) {
            return bad("sidecars are a web service's or a worker's");
        }
        if let Some(s) = &self.slo
            && (!(90.0..=99.999).contains(&s.objective) || !(1..=60).contains(&s.window_days) || s.latency_ms.is_some_and(|ms| !(1..=60_000).contains(&ms)))
        {
            return bad("slo is {objective: 90–99.999, latency_ms?: 1–60000, window_days?: 1–60}");
        }
        if self.log_metrics.len() > 10 {
            return bad("log_metrics is at most 10");
        }
        for (i, m) in self.log_metrics.iter().enumerate() {
            let name_ok = !m.name.is_empty() && m.name.len() <= 100 && m.name.bytes().enumerate().all(|(j, b)| b.is_ascii_alphabetic() || b == b'_' || (j > 0 && (b.is_ascii_digit() || b == b'.')));
            if !name_ok || m.pattern.is_empty() || m.pattern.chars().count() > 200 || self.log_metrics[..i].iter().any(|o| o.name == m.name) {
                return bad("log_metrics are {name: a metric's name (unique), pattern: 1–200 characters}");
            }
        }
        if self.schedule_scale.len() > 10 {
            return bad("schedule_scale is at most 10 rules");
        }
        for w in &self.schedule_scale {
            if w.windows.is_empty() || w.min_instances > self.max_instances.max(1) {
                return bad("schedule_scale's rules are {windows: [{from, to}], timezone?, min_instances: up to max_instances}");
            }
            crate::freeze::Freeze { until: None, windows: w.windows.clone(), timezone: w.timezone.clone(), reason: String::new() }.check().map_err(|e| Error::BadRequest(format!("run.schedule_scale: {e}")))?;
        }
        if let Some(m) = &self.metrics {
            if !matches!(self.kind, RunType::Web | RunType::Worker) {
                return bad("metrics is a web service's or a worker's (it's read from its running instances)");
            }
            if !m.path.starts_with('/') || m.path.len() > 200 || !m.path.bytes().all(|b| b.is_ascii_graphic()) {
                return bad("metrics.path is an absolute path, like /metrics");
            }
            if m.port == Some(0) || (m.port.is_none() && (self.kind != RunType::Web || self.tcp)) {
                return bad("metrics.port is the port its endpoint listens on (a worker's or a TCP service's has no other)");
            }
        }
        if self.slo.is_some() && (self.kind != RunType::Web || self.tcp) {
            return bad("slo is a web service's (it's read from its requests)");
        }
        if self.block_on == Some(Severity::Unknown) {
            return bad("block_on is critical, high, medium or low");
        }
        if self.sidecars.len() > 4 {
            return bad("sidecars is at most 4");
        }
        let var_name = |k: &str| !k.is_empty() && k.len() <= 128 && !k.as_bytes()[0].is_ascii_digit() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        for (i, sc) in self.sidecars.iter().enumerate() {
            if sc.name.is_empty() || sc.name.len() > 20 || !sc.name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') || self.sidecars[..i].iter().any(|o| o.name == sc.name) {
                return bad("sidecars' names are unique: a-z, 0-9 and -, at most 20");
            }
            if sc.image.is_empty() || sc.image.len() > 255 || !sc.image.bytes().all(|b| b.is_ascii_graphic()) {
                return bad("a sidecar's image is an image reference");
            }
            if sc.command.as_ref().is_some_and(|c| c.trim().is_empty() || c.len() > 2000 || c.contains('\0')) {
                return bad("a sidecar's command is 1–2000 characters");
            }
            if sc.env.len() > 50 || sc.variables.len() > 50 {
                return bad("a sidecar has at most 50 env settings and 50 variables");
            }
            if let Some(k) = sc.env.keys().chain(sc.variables.iter()).find(|k| !var_name(k)) {
                return Err(Error::BadRequest(format!("run.sidecars.{}: {k:?} isn't a variable name", sc.name)));
            }
            if sc.env.values().any(|v| v.len() > 4096 || v.contains('\0') || v.contains('\n')) {
                return bad("a sidecar's env values are one line of at most 4096 characters");
            }
            if !(0.1..=2.0).contains(&sc.cpu) || !(64..=4096).contains(&sc.memory_mb) {
                return bad("a sidecar's cpu is 0.1–2, its memory_mb 64–4096");
            }
        }
        if self.queue_s.is_some_and(|q| !(1..=60).contains(&q)) {
            return bad("queue_s is 1–60 seconds");
        }
        if self.latency_target_ms.is_some_and(|t| !(50..=60_000).contains(&t)) {
            return bad("latency_target_ms is 50–60,000");
        }
        let r = &self.rollout;
        if r.steps.len() > 5 || r.steps.iter().any(|s| !(1..=99).contains(s)) || r.steps.windows(2).any(|w| w[0] >= w[1]) {
            return bad("rollout.steps is up to five increasing percents, 1–99");
        }
        if !(1..=3600).contains(&r.step_s) {
            return bad("rollout.step_s is 1–3600");
        }
        if r.slo_burn.is_some_and(|b| !(1.0..=1000.0).contains(&b)) {
            return bad("rollout.slo_burn is 1–1000");
        }
        if r.p95_pct.is_some_and(|p| !(1..=1000).contains(&p)) {
            return bad("rollout.p95_pct is 1–1000");
        }
        if !(1..=100).contains(&r.error_pct) {
            return bad("rollout.error_pct is 1–100");
        }
        if self.kind != RunType::Worker {
            // A web service scales on its requests, a cron run is one: only a worker follows a
            // backlog (dropped, so a worker can become a web service).
            self.scale = None;
        }
        if let Some(sc) = self.scale.as_ref().filter(|s| !s.metric.is_empty()) {
            // The app's own gauge, by its name (Prometheus' rules); read from running instances,
            // so at least one runs.
            let name_ok = sc.metric.len() <= 200 && sc.metric.bytes().enumerate().all(|(i, b)| b.is_ascii_alphabetic() || b == b'_' || b == b':' || (i > 0 && (b.is_ascii_digit() || b == b'.')));
            if !name_ok || !sc.stream.is_empty() || !sc.group.is_empty() || !(1..=1_000_000_000).contains(&sc.per_instance) {
                return bad("scale is { metric: a gauge's name, sum?: bool, per_instance: 1–1e9 }, or a stream's group");
            }
            if self.min_instances == 0 {
                return bad("scaling on a metric needs min_instances of at least 1: a stopped worker reports nothing");
            }
        } else if let Some(sc) = &self.scale {
            // A stream by its id (`st_…`), never a name; a group as Kafka names one.
            if !stream_id(&sc.stream) || !kafka_name(&sc.group) || !(1..=1_000_000_000).contains(&sc.per_instance) {
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
        if self.health_every_s.is_some_and(|s| !(2..=300).contains(&s)) {
            return bad("health_every_s is 2–300");
        }
        if self.health_failures.is_some_and(|n| !(1..=10).contains(&n)) {
            return bad("health_failures is 1–10");
        }
        if let Some(c) = &self.health_command {
            if self.kind != RunType::Worker {
                return bad("health_command is for a worker (a web service's health is its health_path)");
            }
            if c.trim().is_empty() || c.len() > 1000 || c.contains('\0') {
                return bad("health_command is a command of at most 1000 characters");
            }
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
        if let Some(t) = self.memory_target {
            if !(10..=100).contains(&t) {
                return bad("memory_target is 10–100 (percent of an instance's memory)");
            }
            if self.kind != RunType::Web {
                return bad("memory_target is for a web service (a worker follows its work with scale)");
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
            if let Some(b) = v.backups
                && (b.every_hours > 168 || !(1..=90).contains(&b.keep))
            {
                return bad("volume.backups is {every_hours: 0–168 (0: by hand only), keep: 1–90}");
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
                if self.retries.is_some_and(|r| r > 5) {
                    return bad("retries is 0–5");
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
    /// Its step passed and it waits for a person (`rollout.manual`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub waiting: bool,
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
    /// The private Git repository login its source fetches with (never the password).
    pub git_login: Option<GitLogin>,
    /// roll's GitHub App, when the service is connected to it.
    pub github: Option<GitHubLink>,
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
    /// What wants its owner's eye, for a list or a canvas card (Render's, Railway's): a crash loop,
    /// the project capped by its spend limit, maintenance, its latest deploy failed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attention: Vec<&'static str>,
    /// Settings, variables, links or the project's secrets changed since the live deployment:
    /// a redeploy applies them (Railway's staged changes, on every list and card).
    pub needs_deploy: bool,
    /// A preview's (made for a branch or a pull request, deleted with it).
    pub ephemeral: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Environment {
    #[serde(flatten)]
    pub summary: EnvironmentSummary,
    pub service: String,
    pub branch: Option<String>,
    pub instances: Vec<Instance>,
    pub domains: Vec<Domain>,
    pub port: Option<u16>,
    /// A TCP service's address (`host:port`), once it has its port.
    pub tcp: Option<String>,
    /// Visitors need the environment's access token (`PUT … {access: {token}}`), or to sign in.
    pub private: bool,
    /// People signed in to roll who may open the project get in (`access.signed_in`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub signed_in: bool,
    /// Visitors may type its password (`access.password`; never returned).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub password: bool,
    /// A cron service's next scheduled run (unix ms), once it has a deployment.
    pub next_run_at: Option<i64>,
    /// Its schedules (`run.schedules`) as they stand: the next due time and the job each last
    /// started (Heroku Scheduler's "Next due", Vercel's last run).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<ScheduleState>,
    /// How full its volume was when last measured (hourly), when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_used: Option<VolumeUsed>,
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
    /// Its instances send OpenTelemetry traces to roll (`OTEL_*` set at each deploy).
    pub traces: bool,
    /// Its pages measure their Web Vitals for roll (`…/vitals`).
    pub vitals: bool,
    /// Its deploys are reported to the Git host (commit statuses, GitHub Deployments); false:
    /// someone else reports them (roll previews, for its `pv-` environments: one voice on a
    /// pull request).
    pub report: bool,
    /// The label it answers to under its project's preview domain (`<label>.preview.acme.dev`),
    /// when roll previews gave it one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_host: Option<String>,
    /// A preview's pages carry roll previews' feedback button for visitors who came in through
    /// its access link.
    pub feedback: bool,
    /// In maintenance (Render's maintenance mode): its public names answer a 503 page with the
    /// message, and nothing wakes. Null: serving.
    pub maintenance: Option<Maintenance>,
    /// Paused (Render's Suspend): nothing runs, wakes, deploys or is scheduled until it's resumed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
    /// Deploys are held after this rollback: pushes, hooks and tag moves aren't put live until a
    /// person deploys, redeploys or promotes, or resumes them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_after: Option<String>,
    /// A person holds its deploys (`PUT … {held: true}`): pushes, hooks and tag moves build to
    /// `ready` (a web service without a volume) for them to put live, or wait.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub held: bool,
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
    /// Paths another service of the project answers ([]: none).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<Route>,
    /// Its uptime checks ([]: none), and how each stands on its owner.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<Check>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub check_states: Vec<CheckState>,
    /// Its heartbeats ([]: none; their tokens never), and how each stands.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub heartbeats: Vec<Heartbeat>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub heartbeat_states: Vec<HeartbeatState>,
    /// Only in the PUT's answer that added them: each new heartbeat's ping URL, its token in it.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub heartbeat_urls: std::collections::BTreeMap<String, String>,
    /// Its split test: a share of its visitors served by another environment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split: Option<Split>,
    /// When it's paused by the clock (`pause_schedule`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause_schedule: Option<crate::freeze::PauseSchedule>,
    /// Where a share of its requests is copied, their answers dropped (`mirror`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mirror: Option<Split>,
    /// HTML of its own in every page (Netlify's snippet injection).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub snippets: Vec<Snippet>,
    /// Its runtime config (Vercel's Edge Config): values its instances read at `ROLL_CONFIG_URL`
    /// without a deploy, and the version they're at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_version: Option<i64>,
    /// Its deploy freeze, and whether it's on now ([`crate::freeze`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freeze: Option<crate::freeze::Freeze>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub frozen: bool,
    /// What its code may reach: anything (`open`), or roll and the listed hosts (`roll`).
    pub egress: Egress,
    /// Hosts its egress proxy refused lately (its code tried them; they aren't listed).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub egress_refused: Vec<crate::runtime::egress::Refused>,
    /// A worker that follows its work: the backlog it last read (null before the first read)
    /// and the instances that wants.
    pub scaling: Option<Scaling>,
    /// A push deploys only once its commit's checks pass (Railway's "Wait for CI").
    pub wait_for_ci: bool,
    /// The push waiting for its checks, if one is.
    pub waiting_for_ci: Option<String>,
    /// Its builds get no secret values (`egress` `roll`: a build has the default network, so it
    /// isn't given what it could send out).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub build_closed: bool,
    /// Base images the live deployment was built on that have moved since in their registries:
    /// a build now would get their fixes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub base_updated: Vec<String>,
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
    /// Clients by the address the fleet vouches for: addresses or networks (`203.0.113.7`,
    /// `203.0.113.0/24`, an IPv6 `/n`), at most 100, kept canonical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ips: Vec<String>,
    /// A request header, cookie or query parameter there (and containing `contains`, case aside).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<FieldMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cookie: Option<FieldMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<FieldMatch>,
    /// `limit`'s: each client's requests a minute through this rule, and how many at once
    /// (default: a minute's worth).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_minute: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u32>,
}

/// A named part of a request a firewall rule reads ([`FirewallRule::header`]): there, or there
/// and containing `contains` (case aside).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldMatch {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
}

impl FieldMatch {
    fn check(&mut self, what: &str, lower_name: bool) -> Result<()> {
        let n = self.name.trim();
        if n.is_empty() || n.len() > 128 || !n.bytes().all(|b| b.is_ascii_graphic() && !b";,=\"".contains(&b)) {
            return Err(Error::BadRequest(format!("firewall: a {what}'s name is 1–128 characters, no spaces")));
        }
        self.name = if lower_name { n.to_ascii_lowercase() } else { n.to_string() };
        if let Some(c) = &self.contains {
            if c.is_empty() || c.len() > 256 {
                return Err(Error::BadRequest(format!("firewall: a {what}'s contains is 1–256 characters")));
            }
            self.contains = Some(c.to_ascii_lowercase());
        }
        Ok(())
    }

    /// Whether `values` (the request's under this name) has one, holding `contains` if it says.
    fn takes<'a>(&self, mut values: impl Iterator<Item = &'a str>) -> bool {
        match &self.contains {
            None => values.next().is_some(),
            Some(c) => values.any(|v| v.to_ascii_lowercase().contains(c.as_str())),
        }
    }
}

/// What a firewall rule reads of a request ([`FirewallRule::matches`]).
pub struct Asked<'a> {
    pub method: &'a str,
    /// As [`normal_path`] gives it.
    pub path: &'a str,
    pub user_agent: &'a str,
    /// The address the fleet vouches for.
    pub ip: Option<std::net::IpAddr>,
    pub headers: &'a axum::http::HeaderMap,
    pub query: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FirewallAction {
    Allow,
    Deny,
    /// Let in at `per_minute` a client (Vercel's rate-limit rules), a 429 past it.
    Limit,
    /// Let in a browser that has passed a proof of work this hour (Vercel's Attack Challenge
    /// Mode, `challenge.rs`); anything else gets the page that works it out.
    Challenge,
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
        if self.ips.len() > 100 {
            return bad("a rule names at most 100 addresses or networks");
        }
        let mut ips = Vec::with_capacity(self.ips.len());
        for c in &self.ips {
            let c = c.trim();
            let net = c
                .parse::<ipnet::IpNet>()
                .or_else(|_| c.parse::<std::net::IpAddr>().map(ipnet::IpNet::from))
                .map_err(|_| Error::BadRequest(format!("firewall: {c:?} isn't an address or a network (a.b.c.d/n, an IPv6 /n)")))?;
            ips.push(net.trunc().to_string());
        }
        self.ips = ips;
        // Header names are any case; a cookie's and a query parameter's are as written.
        for (m, what, lower) in [(&mut self.header, "header", true), (&mut self.cookie, "cookie", false), (&mut self.query, "query parameter", false)] {
            if let Some(m) = m {
                m.check(what, lower)?;
            }
        }
        if self.path.is_none() && self.methods.is_empty() && self.user_agent.is_none() && self.ips.is_empty() && self.header.is_none() && self.cookie.is_none() && self.query.is_none() {
            return bad("a rule matches on a path, methods, a user agent, addresses, a header, a cookie or a query parameter");
        }
        if self.action == FirewallAction::Challenge && !self.methods.is_empty() && self.methods.iter().any(|m| m != "GET") {
            return bad("a challenge is a page a browser opens: GET only");
        }
        match (self.action, self.per_minute) {
            (FirewallAction::Limit, Some(n)) => RateLimit { per_minute: n, burst: self.burst }.check().map_err(|_| Error::BadRequest("firewall: a limit is 1–100000 a minute, its burst 1–100000".into()))?,
            (FirewallAction::Limit, None) => return bad("a limit rule says per_minute"),
            _ if self.per_minute.is_some() || self.burst.is_some() => return bad("per_minute and burst are a limit rule's"),
            _ => {}
        }
        Ok(self)
    }

    /// Whether every field it gives matches. Paths compare in any case and without a trailing
    /// slash (`/admin/*` takes `/admin` too); HEAD is a GET; addresses by the one the fleet
    /// vouches for (none: no address rule matches); a cookie and a query parameter as the
    /// request carries them, decoded.
    pub fn matches(&self, a: &Asked<'_>) -> bool {
        let path_ok = self.path.as_deref().is_none_or(|p| path_matches(p, a.path));
        let method = if a.method.eq_ignore_ascii_case("HEAD") { "GET" } else { a.method };
        let method_ok = self.methods.is_empty() || self.methods.iter().any(|m| m.eq_ignore_ascii_case(method));
        let ua_ok = self.user_agent.as_deref().is_none_or(|u| a.user_agent.to_ascii_lowercase().contains(u));
        let ip_ok = self.ips.is_empty() || a.ip.is_some_and(|ip| self.ips.iter().any(|n| n.parse::<ipnet::IpNet>().is_ok_and(|n| n.contains(&ip))));
        let header_ok = self.header.as_ref().is_none_or(|m| m.takes(a.headers.get_all(m.name.as_str()).iter().filter_map(|v| v.to_str().ok())));
        let cookie_ok = self.cookie.as_ref().is_none_or(|m| {
            let jar: Vec<&str> = a.headers.get_all(axum::http::header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|c| c.split(';')).collect();
            m.takes(jar.iter().filter_map(|kv| kv.trim().split_once('=')).filter(|(k, _)| *k == m.name).map(|(_, v)| v))
        });
        let query_ok = self.query.as_ref().is_none_or(|m| {
            let pairs: Vec<(String, String)> = url::form_urlencoded::parse(a.query.unwrap_or("").as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
            m.takes(pairs.iter().filter(|(k, _)| *k == m.name).map(|(_, v)| v.as_str()))
        });
        path_ok && method_ok && ua_ok && ip_ok && header_ok && cookie_ok && query_ok
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

/// A path on an environment's domains answered by another service of its project (Vercel's
/// rewrites to another project, Cloud Run's URL maps): that service's environment of the same
/// name, the path and query as they came.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// A path, or a prefix ending in `*` (`/api/*`).
    pub path: String,
    /// The other service, by its name in the project.
    pub service: String,
}

/// HTML of an environment's own in every page the proxy answers (Netlify's snippet injection):
/// an analytics or chat tag without a code change. Only the owner's text, in a 200 HTML page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snippet {
    pub at: SnippetAt,
    pub html: String,
}

/// Where a snippet goes in each page: before `</head>`, or before `</body>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnippetAt {
    Head,
    BodyEnd,
}

impl Snippet {
    /// At most five, each 1 byte to 16 KiB of text.
    pub fn check(all: &[Snippet]) -> Result<()> {
        if all.len() > 5 || all.iter().any(|s| s.html.trim().is_empty() || s.html.len() > 16 << 10 || s.html.contains('\0')) {
            return Err(Error::BadRequest("snippets are at most 5 of {at: head | body_end, html: 1 byte to 16 KiB}".into()));
        }
        Ok(())
    }
}

/// An environment's split test (Netlify's split testing): `percent` of its visitors, each kept
/// by their bucket cookie, are served by the same service's `environment`, past that
/// environment's gates as a route's are (this one's let them in).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Split {
    pub environment: String,
    pub percent: u8,
}

impl Split {
    /// Whether a visitor's bucket (0–99) is in the split's share: the top of the range, so a
    /// gradual rollout's (the bottom) and a split's overlap only past 100 together.
    pub fn takes(&self, bucket: u8) -> bool {
        bucket >= 100 - self.percent.min(100)
    }
}

/// The most routes an environment has.
pub const MAX_ROUTES: usize = 20;

/// An environment's uptime checks, at most ([`Check`]).
pub const MAX_CHECKS: usize = 5;

/// An uptime check (`checks` on an environment; `crate::checks`): GET `path` every `every_s`
/// through the environment's own proxy, passing on `status` (any 2xx or 3xx when none) and, when
/// it says, an answer that contains `contains`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// Unique in the environment: a-z, 0-9 and -, at most 32.
    pub name: String,
    pub path: String,
    /// 30–3600 (default 60).
    #[serde(default = "Check::default_every")]
    pub every_s: u32,
    /// 1–60 (default 10).
    #[serde(default = "Check::default_timeout")]
    pub timeout_s: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// At most 200 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
}

impl Check {
    fn default_every() -> u32 {
        60
    }

    fn default_timeout() -> u32 {
        10
    }

    pub fn check(self) -> Result<Check> {
        let bad = |m: &str| Err(Error::BadRequest(format!("checks: {m}")));
        if self.name.is_empty() || self.name.len() > 32 || !self.name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return bad("a check's name is a-z, 0-9 and -, at most 32");
        }
        if !self.path.starts_with('/') || self.path.starts_with("//") || self.path.len() > 2000 || !self.path.bytes().all(|b| b.is_ascii_graphic()) {
            return bad("a check's path is an absolute path, like /healthz");
        }
        if !(30..=3600).contains(&self.every_s) || !(1..=60).contains(&self.timeout_s) {
            return bad("every_s is 30–3600, timeout_s 1–60");
        }
        if self.status.is_some_and(|s| !(100..=599).contains(&s)) || self.contains.as_ref().is_some_and(|c| c.is_empty() || c.len() > 200) {
            return bad("status is an HTTP status, contains 1–200 characters");
        }
        Ok(self)
    }
}

/// An environment's heartbeats, at most ([`Heartbeat`]).
pub const MAX_HEARTBEATS: usize = 10;

/// A heartbeat (`heartbeats` on an environment; `crate::heartbeats`, Healthchecks'): something
/// outside roll (a job, a backup, a device) pings `/__roll/heartbeat/<token>` on the
/// environment's host at least every `every_s`; past that and `grace_s`, it's
/// `heartbeat.missed`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// Unique in the environment: a-z, 0-9 and -, at most 32.
    pub name: String,
    /// 60–604800 (a week).
    pub every_s: u32,
    /// 0–86400 (default 300).
    #[serde(default = "Heartbeat::default_grace")]
    pub grace_s: u32,
    /// When it was added (ms): the first ping is due from then.
    #[serde(default)]
    pub added_at: i64,
    /// Its token's SHA-256 (the token itself is shown once).
    #[serde(default, skip_serializing)]
    pub token_hash: String,
}

impl Heartbeat {
    fn default_grace() -> u32 {
        300
    }

    pub fn check(&self) -> Result<()> {
        let bad = |m: &str| Err(Error::BadRequest(format!("heartbeats: {m}")));
        if self.name.is_empty() || self.name.len() > 32 || !self.name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return bad("a heartbeat's name is a-z, 0-9 and -, at most 32");
        }
        if !(60..=604_800).contains(&self.every_s) || self.grace_s > 86_400 {
            return bad("every_s is 60–604800, grace_s 0–86400");
        }
        Ok(())
    }
}

/// A heartbeat as it stands ([`Environment::heartbeat_states`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HeartbeatState {
    pub name: String,
    pub last_ping_at: Option<i64>,
    /// When the next ping is due by (past it and the grace: missed).
    pub due_at: i64,
    pub missed: bool,
}

/// A check as it stands on the environment's owner ([`Environment::check_states`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CheckState {
    pub name: String,
    /// None: not tried yet. False once it has failed twice in a row.
    pub up: Option<bool>,
    /// Failures in a row.
    pub failures: u32,
    pub last_at: Option<i64>,
    pub status: Option<u16>,
    pub ms: Option<u64>,
    pub error: Option<String>,
    pub down_since: Option<i64>,
}

impl Route {
    pub fn check(self) -> Result<Route> {
        let p = &self.path;
        if !p.starts_with('/') || p.len() > 512 || p.strip_suffix('*').unwrap_or(p).contains('*') || !p.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(Error::BadRequest(format!("routes: {p:?} is a path or a prefix ending in * (/api/*)")));
        }
        if self.service.is_empty() || self.service.len() > 64 {
            return Err(Error::BadRequest("routes: service is another service's name in the project".into()));
        }
        Ok(self)
    }

    /// Whether it takes `path`.
    pub fn takes(&self, path: &str) -> bool {
        match self.path.strip_suffix('*') {
            Some(prefix) => path.starts_with(prefix),
            None => path == self.path,
        }
    }
}

/// What an environment's code may reach (docs/apps/README.md, "Egress"; roll previews'
/// `egress`): anything, or only roll (its links, the project's other services there) and
/// `hosts`, through its egress proxy.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Egress {
    pub mode: EgressMode,
    /// With `roll`: hosts outside roll it may still reach on 443 and 80 (`api.stripe.com`, or
    /// `*.sentry.io` for its subdomains).
    #[serde(default)]
    pub hosts: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EgressMode {
    #[default]
    Open,
    Roll,
}

/// The most hosts an environment's egress lists.
pub const MAX_EGRESS_HOSTS: usize = 20;

impl Egress {
    /// No route out but roll's.
    pub fn closed(&self) -> bool {
        self.mode == EgressMode::Roll
    }

    /// Checked as given: host names (lower case, no IP addresses), `*.` only as a prefix, only
    /// with `roll`.
    pub fn check(mut self) -> Result<Egress> {
        let bad = |m: String| Err(Error::BadRequest(format!("egress: {m}")));
        if self.hosts.len() > MAX_EGRESS_HOSTS {
            return bad(format!("at most {MAX_EGRESS_HOSTS} hosts"));
        }
        for h in &mut self.hosts {
            *h = h.trim().trim_end_matches('.').to_ascii_lowercase();
        }
        if let Some(h) = self.hosts.iter().find(|h| !egress_host_ok(h)) {
            return bad(format!("{h:?} isn't a host name (api.stripe.com, or *.sentry.io for its subdomains)"));
        }
        if !self.hosts.is_empty() && !self.closed() {
            return bad("hosts are for mode \"roll\" (with \"open\" everything is reachable)".into());
        }
        self.hosts.sort();
        self.hosts.dedup();
        Ok(self)
    }
}

/// A host an egress may list: a DNS name of two labels or more (never an address), or `*.` and
/// one (its subdomains). roll previews' `egress_hosts` take the same.
pub fn egress_host_ok(h: &str) -> bool {
    let name = h.strip_prefix("*.").unwrap_or(h);
    let labels: Vec<&str> = name.split('.').collect();
    name.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
        && !labels.last().is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
}

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
    /// How to fix a known failure (a private image's login, more memory, a lockfile): read from
    /// its error, never stored ([`fix_for`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// When it was pruned (unix ms): out of what's kept, its image gone; history only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pruned_at: Option<i64>,
    /// Its image's last scan for known vulnerabilities, once one ran (`…/vulnerabilities` lists them).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vulnerabilities: Option<ScanSummary>,
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
    /// The app's port this hostname is sent to, when it's another than the service's own
    /// (Railway's target port, Fly's services): an admin UI on 9000 beside the app on 8080.
    /// Health checks and wakes stay on the main port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// A hostname a user may add: lowercase labels of `[a-z0-9-]`, at least two, at most 253
/// characters, not an IP address and not under roll's own domains; or `*.` and one (a wildcard:
/// every name one label under it).
/// Generated names nobody takes: they'd read as roll's own on its shared domain.
const RESERVED_LABELS: [&str; 12] = ["www", "api", "app", "admin", "dashboard", "status", "mail", "roll", "edge", "cdn", "docs", "help"];

/// An environment's generated name (`<name>.<domain>`, Railway's editable domain): 3–56 of
/// a–z, 0–9 and `-`, not starting or ending with one, never `--` (a deployment's own name is
/// `<id>--<name>`), and not one of roll's.
pub fn check_slug(s: &str) -> Result<()> {
    let ok = (3..=56).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--");
    if !ok {
        return Err(Error::BadRequest("hostname is 3–56 of a-z, 0-9 and -, not starting or ending with -, without --".into()));
    }
    if RESERVED_LABELS.contains(&s) {
        return Err(Error::BadRequest(format!("{s} is roll's own")));
    }
    Ok(())
}

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
    /// A roll search cluster: an API key of its own per link ([`crate::integrations::Resources::link`]).
    Search,
    /// Another web service of the project: its private and public URLs (apps' own, not a
    /// [`crate::integrations::Resources`] kind).
    Service,
}

impl LinkKind {
    pub const ALL: [LinkKind; 8] = [LinkKind::Postgres, LinkKind::Redis, LinkKind::Bucket, LinkKind::Stream, LinkKind::Queue, LinkKind::Nosql, LinkKind::Search, LinkKind::Service];

    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Postgres => "postgres",
            LinkKind::Redis => "redis",
            LinkKind::Bucket => "bucket",
            LinkKind::Stream => "stream",
            LinkKind::Queue => "queue",
            LinkKind::Nosql => "nosql",
            LinkKind::Search => "search",
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

/// The environment a `roll://ENV[/folder…]/KEY` value names (roll secrets' references; its
/// own syntax is checked where it resolves): None when it isn't one; an error when it's
/// malformed, or names a project (a service's references are its own project's).
pub fn secret_ref_env(v: &str) -> Option<Result<String>> {
    let bad = |m: &str| Some(Err(Error::BadRequest(m.into())));
    // What roll secrets' parser would take for one (it trims) is one; only its exact form is
    // accepted, so no check here or elsewhere reads it differently.
    if !v.trim().to_ascii_lowercase().starts_with("roll://") {
        return None;
    }
    let Some(rest) = v.strip_prefix("roll://").filter(|_| v.trim() == v) else {
        return bad("a roll:// reference is written exactly: roll://ENV/KEY, lowercase, with nothing around it");
    };
    if rest.contains('@') {
        return bad("a service's roll:// reference names its own project's secrets: roll://ENV/KEY, without a project");
    }
    match rest.split_once('/') {
        Some((env, key)) if !env.is_empty() && !key.is_empty() && !key.ends_with('/') => Some(Ok(env.to_string())),
        _ => bad("a roll:// reference is roll://ENV/KEY or roll://ENV/folder/KEY"),
    }
}

/// Whether a `roll://` reference's folder may be read by `service`: one under `/services/<id>`
/// (where "Move to roll secrets" keeps a service's own) only by that service.
pub fn secret_ref_folder_ok(v: &str, service: &str) -> bool {
    let parts: Vec<&str> = v.strip_prefix("roll://").unwrap_or(v).split('/').collect();
    !(parts.len() >= 4 && parts[1] == "services" && parts[2] != service)
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
    /// Kept as a file at this path in its instances (not in their environment).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
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
    /// Which try of its due time it is (1: the first; `run.retries` more may follow a failure).
    pub attempt: u32,
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
        assert!(check_slug("shop-eu").is_ok());
        for bad in ["ab", "Shop", "-shop", "shop-", "dep--shop", "www", "shop.eu"] {
            assert!(check_slug(bad).is_err(), "{bad}");
        }
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


/// What to change for a failure roll knows (Heroku's error codes, Render's pointers): one line
/// naming the setting and its command. None for anything else: the error and its detail say it.
pub fn fix_for(error: &str, detail: Option<&str>) -> Option<String> {
    let all = format!("{error}\n{}", detail.unwrap_or("")).to_lowercase();
    let has = |w: &str| all.contains(w);
    let fix = if has("docker pull") && (has("denied") || has("unauthorized") || has("not found") || has("manifest unknown") || has("failed")) {
        "The registry wouldn't give the image: check its name and tag; a private image needs its registry login (Settings → Source, `roll apps registry`)."
    } else if has("exit 137") || has("out of memory") || has("oomkilled") || has("javascript heap out of memory") {
        "It ran out of memory: give it more (Settings → Size, `roll apps set --memory 1024`)."
    } else if has("the build took longer than") {
        "The build passed its time limit: make it lighter (a lockfile for cached installs, fewer steps), or build an image elsewhere and deploy that."
    } else if has("npm ci") && (has("package-lock") || has("lockfile") || has("in sync")) {
        "`npm ci` needs a package-lock.json that matches package.json: commit one (`npm install`), or set the install command to `npm install` (Settings → Build)."
    } else if has("frozen-lockfile") || has("lockfile needs to be updated") {
        "The lockfile doesn't match package.json: run the install locally and commit the lockfile, or drop `--frozen-lockfile` from the install command (Settings → Build)."
    } else if has("health check didn't pass") || has("didn't answer in time") || has("health check") && has("failed") {
        "It never answered its health check: it must listen on 0.0.0.0 at $PORT; if it boots slowly, raise the health timeout (Settings → Health, `roll apps set --health-timeout 120`)."
    } else if has("no start command") || has("nothing says how to start") || has("doesn't know how to start") {
        "Nothing says how to start it: set a start command (Settings → Run, `roll apps set --start \"…\"`) or a Procfile's `web:`."
    } else if has("couldn't tell how to build this") {
        "roll couldn't tell how to build it: add a Dockerfile, set the root directory to the app's, or set the build and start commands (Settings → Build)."
    } else {
        return None;
    };
    Some(fix.to_string())
}

#[cfg(test)]
mod fix_tests {
    #[test]
    fn known_failures_say_what_to_change() {
        let f = |e: &str, d: Option<&str>| super::fix_for(e, d).unwrap_or_default();
        assert!(f("docker pull ghcr.io/acme/app:1 failed", Some("error from registry: denied")).contains("registry login"));
        assert!(f("it was killed (exit 137: out of memory, or a SIGKILL)", None).contains("--memory"));
        assert!(f("the health check didn't pass within 60 s", None).contains("$PORT"));
        assert!(f("the build failed (docker build exited with 1)", Some("npm ci can only install packages when your package.json and package-lock.json are in sync")).contains("package-lock.json"));
        assert_eq!(super::fix_for("a reason nobody wrote down", None), None);
    }
}
