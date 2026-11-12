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
                if ok { Ok(()) } else { Err(Error::BadRequest("source.image is an image reference".into())) }
            }
            Source::Upload => Ok(()),
        }
    }

    /// Whether deploys build (everything but a public image).
    pub fn builds(&self) -> bool {
        !matches!(self, Source::Image { .. })
    }
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
}

impl Default for Run {
    fn default() -> Self {
        Run {
            kind: RunType::Web,
            port: None,
            start: None,
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
        }
    }
}

impl Run {
    /// The checked settings, with what the type implies filled in (a worker runs at least one
    /// instance; a volume allows one).
    pub fn normalized(mut self) -> Result<Run> {
        let bad = |m: &str| Err(Error::BadRequest(format!("run.{m}")));
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
        if let Some(v) = &self.volume {
            if !v.mount.starts_with('/') || v.mount == "/" || v.mount.len() > 200 || v.mount.contains("..") {
                return bad("volume.mount is an absolute path other than /");
            }
            if !(1..=500).contains(&v.size_gb) {
                return bad("volume.size_gb is 1–500");
            }
            self.max_instances = 1;
            self.min_instances = self.min_instances.min(1);
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
pub struct Service {
    pub id: String,
    pub project: String,
    pub name: String,
    pub source: Source,
    pub build: Build,
    pub run: Run,
    pub created_at: i64,
    pub environments: Vec<EnvironmentSummary>,
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
    pub created_at: i64,
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
}

impl DeployStatus {
    pub const ALL: [DeployStatus; 9] = [
        DeployStatus::Queued,
        DeployStatus::Building,
        DeployStatus::Deploying,
        DeployStatus::Active,
        DeployStatus::Inactive,
        DeployStatus::Failed,
        DeployStatus::Canceled,
        DeployStatus::Superseded,
        DeployStatus::Removed,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    Push,
    Manual,
    Rollback,
    Upload,
    Api,
    Settings,
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
        }
    }

    pub fn parse(s: &str) -> Trigger {
        match s {
            "push" => Trigger::Push,
            "rollback" => Trigger::Rollback,
            "upload" => Trigger::Upload,
            "api" => Trigger::Api,
            "settings" => Trigger::Settings,
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
    /// The settings this deployment runs with (frozen when it was made).
    #[serde(skip)]
    pub run: Run,
    /// A hash of the variables it was made with (names and values), to tell when they changed.
    #[serde(skip)]
    pub vars_hash: String,
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
    pub created_at: i64,
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
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Skipped => "skipped",
            RunStatus::TimedOut => "timed_out",
        }
    }

    pub fn parse(s: &str) -> RunStatus {
        match s {
            "succeeded" => RunStatus::Succeeded,
            "failed" => RunStatus::Failed,
            "skipped" => RunStatus::Skipped,
            "timed_out" => RunStatus::TimedOut,
            _ => RunStatus::Running,
        }
    }
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
