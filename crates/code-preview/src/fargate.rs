//! Isolated work on Fargate (docs/cloud/README.md, "Isolated work"): every check and preview is
//! its own ECS task, a Firecracker microVM with a task role that can do nothing, in the isolated
//! security group (no way into the fleet's network; fleet nodes reach a preview on one port).
//!
//! A task is three containers, from a template task definition in the stack:
//!
//! - `fetch` (roll's small image): downloads the run's bundle (`roll_fetch`: the env and the tree
//!   from `Code::checkout`) through a presigned URL that lasts minutes, and unpacks it with the
//!   hostile-tree rules into a volume of the task's own storage. It holds no AWS credentials.
//! - `guard` (the same image): a check's deadline, or a preview's proxy on the fixed preview port
//!   that stops the task once the preview has been idle. Either way a task ends by itself if the
//!   node that started it is gone.
//! - `run`: the customer's image, starting once `fetch` succeeded, as `sh -c <run>` with the
//!   env sourced from the bundle.
//!
//! The image is the only thing a template can't take as an override, so a task definition is
//! registered per (template, image), named after both, and reused. Nothing secret goes into a
//! `RunTask` request (it shows in `DescribeTasks` and CloudTrail): the env travels in the bundle,
//! whose object is deleted when the run ends.

use crate::docker::Tail;
use crate::isolation::{Isolation, Job, Kind};
use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use aws_sdk_ecs::types::{
    AssignPublicIp, AwsVpcConfiguration, CapacityProviderStrategyItem, Container, ContainerOverride, DesiredStatus, EphemeralStorage,
    NetworkConfiguration, Task, TaskDefinition, TaskOverride, TaskStopCode,
};
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path as ObjPath;
use object_store::signer::{Method, Signer};
use object_store::{ObjectStoreExt, PutPayload, WriteMultipart};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tracing::{info, warn};

/// The containers of a template.
pub const FETCH: &str = "fetch";
pub const GUARD: &str = "guard";
pub const RUN: &str = "run";
/// Where `fetch` writes the env, on the `roll` volume.
pub const ENV_PATH: &str = "/roll/env";
/// How `run` starts: the env, then `sh -c <run>` (passed as `$0`).
pub const RUN_SCRIPT: &str = ". /roll/env && exec sh -c \"$0\"";

/// Keys for an endpoint of its own (tests, MinIO). `Debug` doesn't show them.
#[derive(Clone)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials(…)")
    }
}

#[derive(Clone, Debug)]
pub struct FargateConfig {
    /// The ECS cluster (name or ARN).
    pub cluster: String,
    pub subnets: Vec<String>,
    /// The isolated security group: egress to the internet, ingress only from fleet nodes on
    /// `preview_port`.
    pub security_group: String,
    /// Template task definitions (family or ARN).
    pub check_task: String,
    pub preview_task: String,
    pub sandbox_task: Option<String>,
    /// Give tasks a public IP: their only way out in subnets without a NAT gateway.
    pub public_ip: bool,
    /// Checks on Fargate Spot (one that's interrupted runs again on demand).
    pub spot: bool,
    /// The port a preview task's guard listens on, which nodes connect to.
    pub preview_port: u16,
    /// Where bundles go: `s3://<bucket>/<prefix><run>.tgz`, deleted when the run ends.
    pub bucket: String,
    pub prefix: String,
    pub region: Option<String>,
    /// ECS and CloudWatch Logs at another address (tests).
    pub endpoint: Option<String>,
    /// S3 at another address (tests, MinIO).
    pub s3_endpoint: Option<String>,
    /// Keys for those endpoints; otherwise the usual AWS chain (the node's task role).
    pub credentials: Option<Credentials>,
    /// Where bundles are packed before they're uploaded.
    pub scratch: PathBuf,
    /// How often a starting or running task is looked at.
    pub poll: Duration,
    /// How long a bundle's presigned URL works.
    pub url_expiry: Duration,
    pub limits: roll_fetch::Limits,
}

impl FargateConfig {
    /// `None` unless `ROLL_CODE_RUNNER=fargate` (`docker`, the default, runs everything on this
    /// host). Settings (docs/cloud/README.md, "Node settings"): `ROLL_CODE_FARGATE_CLUSTER`,
    /// `_SUBNETS` (comma separated), `_SECURITY_GROUP`, `_CHECK_TASK`, `_PREVIEW_TASK`,
    /// `_SANDBOX_TASK`, `_PUBLIC_IP` (`1`), `_SPOT` (`1`), `_PREVIEW_PORT` (9970), `_BUCKET`
    /// (default `ROLL_S3_BUCKET`), `_PREFIX` (`code/runs/`), and `AWS_REGION`.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>, scratch: PathBuf) -> anyhow::Result<Option<FargateConfig>> {
        match get("ROLL_CODE_RUNNER").as_deref().map(str::trim) {
            None | Some("") | Some("docker") => return Ok(None),
            Some("fargate") => {}
            Some(other) => bail!("ROLL_CODE_RUNNER is docker or fargate, not {other}"),
        }
        let need = |k: &str| get(k).filter(|v| !v.trim().is_empty()).ok_or_else(|| anyhow!("ROLL_CODE_RUNNER=fargate needs {k}"));
        let flag = |k: &str| get(k).is_some_and(|v| v == "1" || v == "true");
        let subnets: Vec<String> = need("ROLL_CODE_FARGATE_SUBNETS")?.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        let bucket = get("ROLL_CODE_FARGATE_BUCKET").or_else(|| get("ROLL_S3_BUCKET")).ok_or_else(|| anyhow!("ROLL_CODE_RUNNER=fargate needs ROLL_S3_BUCKET"))?;
        let preview_port = match get("ROLL_CODE_FARGATE_PREVIEW_PORT") {
            Some(p) => p.parse().map_err(|_| anyhow!("ROLL_CODE_FARGATE_PREVIEW_PORT is a port"))?,
            None => 9970,
        };
        let mut prefix = get("ROLL_CODE_FARGATE_PREFIX").unwrap_or_else(|| "code/runs/".into());
        if !prefix.is_empty() && !prefix.ends_with('/') {
            prefix.push('/');
        }
        Ok(Some(FargateConfig {
            cluster: need("ROLL_CODE_FARGATE_CLUSTER")?,
            subnets,
            security_group: need("ROLL_CODE_FARGATE_SECURITY_GROUP")?,
            check_task: need("ROLL_CODE_FARGATE_CHECK_TASK")?,
            preview_task: need("ROLL_CODE_FARGATE_PREVIEW_TASK")?,
            sandbox_task: get("ROLL_CODE_FARGATE_SANDBOX_TASK").filter(|v| !v.trim().is_empty()),
            public_ip: flag("ROLL_CODE_FARGATE_PUBLIC_IP"),
            spot: flag("ROLL_CODE_FARGATE_SPOT"),
            preview_port,
            bucket,
            prefix,
            region: get("AWS_REGION").or_else(|| get("AWS_DEFAULT_REGION")),
            endpoint: None,
            s3_endpoint: None,
            credentials: None,
            scratch,
            poll: Duration::from_secs(2),
            url_expiry: Duration::from_secs(15 * 60),
            limits: roll_fetch::Limits::default(),
        }))
    }
}

/// The smallest Fargate size with at least `cpus` and `memory_mb`: (CPU units, MiB).
pub fn fargate_size(cpus: f64, memory_mb: u32) -> (u32, u32) {
    // (CPU units, smallest memory, largest, step), from the Fargate task size table.
    const SIZES: [(u32, u32, u32, u32); 7] = [
        (256, 512, 2048, 512),
        (512, 1024, 4096, 1024),
        (1024, 2048, 8192, 1024),
        (2048, 4096, 16384, 1024),
        (4096, 8192, 30720, 1024),
        (8192, 16384, 61440, 4096),
        (16384, 32768, 122880, 8192),
    ];
    let want = (cpus.max(0.0) * 1024.0).ceil() as u32;
    for (cpu, lo, hi, step) in SIZES {
        if cpu < want || memory_mb > hi {
            continue;
        }
        let mem = if memory_mb <= lo { lo } else { lo + (memory_mb - lo).div_ceil(step) * step };
        return (cpu, mem.min(hi));
    }
    (16384, 122880)
}

/// A template task definition and where its containers log.
struct Template {
    def: TaskDefinition,
    arn: String,
    fetched: Instant,
}

impl Template {
    /// The CloudWatch log group and stream of `container` in task `task_arn`.
    fn stream(&self, container: &str, task_arn: &str) -> Option<(String, String)> {
        let c = self.def.container_definitions().iter().find(|c| c.name() == Some(container))?;
        let opts = c.log_configuration()?.options()?;
        let group = opts.get("awslogs-group")?.clone();
        let prefix = opts.get("awslogs-stream-prefix")?;
        let id = task_arn.rsplit('/').next()?;
        Some((group, format!("{prefix}/{container}/{id}")))
    }
}

/// A job made ready by `prepare`: its bundle is uploaded.
#[derive(Clone)]
struct Prepared {
    kind: Kind,
    template: Arc<Template>,
    task_def: String,
    run: String,
    cpu: u32,
    memory: u32,
    deadline: Duration,
}

/// A task this node started for a job.
#[derive(Clone)]
struct Started {
    arn: String,
    template: Arc<Template>,
    /// The last look, for `running`.
    seen: Option<(Instant, bool)>,
}

/// What a task launch needs (checks, previews and sandboxes).
pub(crate) struct Launch<'a> {
    pub name: &'a str,
    pub group: &'a str,
    pub task_def: &'a str,
    pub spot: bool,
    pub cpu: u32,
    pub memory: u32,
    pub disk_gib: Option<i32>,
    pub containers: Vec<ContainerOverride>,
}

pub struct Fargate {
    pub(crate) cfg: FargateConfig,
    pub(crate) ecs: aws_sdk_ecs::Client,
    logs: aws_sdk_cloudwatchlogs::Client,
    s3: AmazonS3,
    templates: tokio::sync::Mutex<HashMap<String, Arc<Template>>>,
    /// Task definitions registered for (template ARN, image).
    derived: Mutex<HashMap<(String, String), String>>,
    prepared: Mutex<HashMap<String, Prepared>>,
    started: Mutex<HashMap<String, Started>>,
}

/// How long a template is used before it's read again (a stack update changes it).
const TEMPLATE_TTL: Duration = Duration::from_secs(600);
/// Bundles up to this size go up in one PUT.
const SINGLE_PUT: u64 = 64 << 20;

pub fn container_override(name: &str, command: Vec<String>) -> ContainerOverride {
    ContainerOverride::builder().name(name).set_command(Some(command)).build()
}

fn container<'a>(t: &'a Task, name: &str) -> Option<&'a Container> {
    t.containers().iter().find(|c| c.name() == Some(name))
}

/// Why a stopped task stopped, in a line.
fn stopped_reason(t: &Task) -> String {
    let mut why = t.stopped_reason().unwrap_or("it stopped").to_string();
    for c in t.containers() {
        if let Some(r) = c.reason() {
            why.push_str(&format!("; {}: {r}", c.name().unwrap_or("?")));
        }
    }
    why
}

impl Fargate {
    pub async fn new(cfg: FargateConfig) -> anyhow::Result<Arc<Fargate>> {
        std::fs::create_dir_all(&cfg.scratch).with_context(|| format!("creating {}", cfg.scratch.display()))?;
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .retry_config(aws_config::retry::RetryConfig::standard().with_max_attempts(3))
            .timeout_config(aws_config::timeout::TimeoutConfig::builder().operation_timeout(Duration::from_secs(30)).connect_timeout(Duration::from_secs(5)).build());
        let region = cfg.region.clone().or_else(|| cfg.endpoint.as_ref().map(|_| "us-east-1".to_string()));
        if let Some(r) = &region {
            loader = loader.region(aws_config::Region::new(r.clone()));
        }
        if let Some(e) = &cfg.endpoint {
            loader = loader.endpoint_url(e);
        }
        if let Some(c) = &cfg.credentials {
            loader = loader.credentials_provider(aws_sdk_ecs::config::Credentials::new(&c.access_key, &c.secret_key, None, None, "roll"));
        }
        let sdk = loader.load().await;
        let mut b = AmazonS3Builder::from_env().with_bucket_name(&cfg.bucket);
        if let Some(r) = &region {
            b = b.with_region(r);
        }
        if let Some(e) = &cfg.s3_endpoint {
            b = b.with_endpoint(e).with_allow_http(e.starts_with("http://"));
        }
        if let Some(c) = &cfg.credentials {
            b = b.with_access_key_id(&c.access_key).with_secret_access_key(&c.secret_key);
        }
        let s3 = b.build().context("the bundle bucket")?;
        Ok(Arc::new(Fargate {
            ecs: aws_sdk_ecs::Client::new(&sdk),
            logs: aws_sdk_cloudwatchlogs::Client::new(&sdk),
            s3,
            cfg,
            templates: tokio::sync::Mutex::default(),
            derived: Mutex::default(),
            prepared: Mutex::default(),
            started: Mutex::default(),
        }))
    }

    async fn template(&self, family: &str) -> anyhow::Result<Arc<Template>> {
        let mut cache = self.templates.lock().await;
        if let Some(t) = cache.get(family).filter(|t| t.fetched.elapsed() < TEMPLATE_TTL) {
            return Ok(t.clone());
        }
        let out = self.ecs.describe_task_definition().task_definition(family).send().await.with_context(|| format!("reading task definition {family}"))?;
        let def = out.task_definition.with_context(|| format!("task definition {family} not found"))?;
        let arn = def.task_definition_arn().unwrap_or(family).to_string();
        let t = Arc::new(Template { def, arn, fetched: Instant::now() });
        cache.insert(family.to_string(), t.clone());
        Ok(t)
    }

    /// The task definition for `image` from template `t`: registered once, then reused.
    async fn task_def(&self, t: &Template, image: &str) -> anyhow::Result<String> {
        let key = (t.arn.clone(), image.to_string());
        if let Some(arn) = self.derived.lock().get(&key) {
            return Ok(arn.clone());
        }
        let base = t.def.family().context("the template has no family")?;
        let hash = hex::encode(Sha256::digest(format!("{}\n{image}", t.arn)));
        let family = format!("{base}-{}", &hash[..16]);
        // Registered by this node before, or by another: same template, same image.
        let found = match self.ecs.describe_task_definition().task_definition(&family).send().await {
            Ok(out) => out
                .task_definition
                .filter(|d| d.container_definitions().iter().any(|c| c.name() == Some(RUN) && c.image() == Some(image)))
                .and_then(|d| d.task_definition_arn),
            Err(_) => None,
        };
        let arn = match found {
            Some(arn) => arn,
            None => {
                let mut containers = t.def.container_definitions().to_vec();
                let run = containers.iter_mut().find(|c| c.name() == Some(RUN)).with_context(|| format!("the template has no {RUN:?} container"))?;
                run.image = Some(image.to_string());
                let d = &t.def;
                let out = self
                    .ecs
                    .register_task_definition()
                    .family(&family)
                    .set_task_role_arn(d.task_role_arn().map(str::to_string))
                    .set_execution_role_arn(d.execution_role_arn().map(str::to_string))
                    .set_network_mode(d.network_mode().cloned())
                    .set_container_definitions(Some(containers))
                    .set_volumes(Some(d.volumes().to_vec()))
                    .set_requires_compatibilities(Some(d.requires_compatibilities().to_vec()))
                    .set_cpu(d.cpu().map(str::to_string))
                    .set_memory(d.memory().map(str::to_string))
                    .set_runtime_platform(d.runtime_platform().cloned())
                    .set_ephemeral_storage(d.ephemeral_storage().cloned())
                    .send()
                    .await
                    .with_context(|| format!("registering a task definition for {image}"))?;
                let arn = out.task_definition.and_then(|d| d.task_definition_arn).context("RegisterTaskDefinition returned no ARN")?;
                info!(%family, %image, "registered a task definition for isolated work");
                arn
            }
        };
        self.derived.lock().insert(key, arn.clone());
        Ok(arn)
    }

    pub(crate) fn s3(&self) -> &AmazonS3 {
        &self.s3
    }

    fn key(&self, bundle: &str) -> ObjPath {
        ObjPath::from(format!("{}{bundle}.tgz", self.cfg.prefix))
    }

    /// Pack `env` and `tree` as bundle `bundle` and upload it.
    pub(crate) async fn upload(&self, bundle: &str, env: BTreeMap<String, String>, tree: Option<&Path>) -> anyhow::Result<()> {
        let file = self.cfg.scratch.join(format!("{bundle}.tgz"));
        let (f, tree, limits) = (file.clone(), tree.map(Path::to_path_buf), self.cfg.limits);
        let packed = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let out = std::io::BufWriter::new(std::fs::File::create(&f)?);
            roll_fetch::pack(out, &env, tree.as_deref(), limits)
        })
        .await?;
        let up = match packed {
            Ok(()) => self.put_file(bundle, &file).await,
            Err(e) => Err(e),
        };
        let _ = tokio::fs::remove_file(&file).await;
        up
    }

    async fn put_file(&self, bundle: &str, file: &Path) -> anyhow::Result<()> {
        let key = self.key(bundle);
        let size = tokio::fs::metadata(file).await?.len();
        if size <= SINGLE_PUT {
            let data = tokio::fs::read(file).await?;
            self.s3.put(&key, PutPayload::from(data)).await.context("uploading the bundle")?;
            return Ok(());
        }
        let mut writer = WriteMultipart::new_with_chunk_size(self.s3.put_multipart(&key).await?, 8 << 20);
        let mut f = tokio::fs::File::open(file).await?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            writer.wait_for_capacity(4).await?;
            writer.write(&buf[..n]);
        }
        writer.finish().await.context("uploading the bundle")?;
        Ok(())
    }

    /// A URL `fetch` can GET the bundle with for `url_expiry`.
    pub(crate) async fn bundle_url(&self, bundle: &str) -> anyhow::Result<String> {
        Ok(self.s3.signed_url(Method::GET, &self.key(bundle), self.cfg.url_expiry).await.context("signing the bundle's URL")?.to_string())
    }

    pub(crate) async fn delete_bundle(&self, bundle: &str) {
        match self.s3.delete(&self.key(bundle)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => warn!(bundle, error = %e, "deleting a run's bundle"),
        }
    }

    /// The `fetch` container's command: the bundle into `tree`, the env to `ENV_PATH`.
    pub(crate) fn fetch_command(url: &str, tree: &str, chown: Option<u32>) -> Vec<String> {
        let mut c: Vec<String> = ["roll-fetch", "fetch", "--url", url, "--tree", tree, "--env", ENV_PATH].map(String::from).to_vec();
        if let Some(uid) = chown {
            c.extend(["--chown".to_string(), uid.to_string()]);
        }
        c
    }

    /// Start one task: its ARN.
    pub(crate) async fn launch(&self, l: Launch<'_>) -> anyhow::Result<String> {
        let vpc = AwsVpcConfiguration::builder()
            .set_subnets(Some(self.cfg.subnets.clone()))
            .security_groups(&self.cfg.security_group)
            .assign_public_ip(if self.cfg.public_ip { AssignPublicIp::Enabled } else { AssignPublicIp::Disabled })
            .build()?;
        let provider = if l.spot { "FARGATE_SPOT" } else { "FARGATE" };
        let mut overrides = TaskOverride::builder().cpu(l.cpu.to_string()).memory(l.memory.to_string()).set_container_overrides(Some(l.containers));
        if let Some(gib) = l.disk_gib {
            overrides = overrides.ephemeral_storage(EphemeralStorage::builder().size_in_gib(gib).build());
        }
        let out = self
            .ecs
            .run_task()
            .cluster(&self.cfg.cluster)
            .task_definition(l.task_def)
            .count(1)
            .started_by(l.name)
            .group(l.group)
            .capacity_provider_strategy(CapacityProviderStrategyItem::builder().capacity_provider(provider).weight(1).build()?)
            .network_configuration(NetworkConfiguration::builder().awsvpc_configuration(vpc).build())
            .overrides(overrides.build())
            .send()
            .await
            .context("RunTask")?;
        if let Some(f) = out.failures().first() {
            bail!("RunTask: {} {}", f.reason().unwrap_or("failed"), f.detail().unwrap_or(""));
        }
        out.tasks().first().and_then(|t| t.task_arn()).map(str::to_string).context("RunTask started no task")
    }

    pub(crate) async fn describe(&self, arn: &str) -> anyhow::Result<Option<Task>> {
        let out = self.ecs.describe_tasks().cluster(&self.cfg.cluster).tasks(arn).send().await.context("DescribeTasks")?;
        Ok(out.tasks.and_then(|t| t.into_iter().next()))
    }

    /// Describe many tasks (100 at a time, ECS's limit).
    pub(crate) async fn describe_all(&self, arns: &[String]) -> anyhow::Result<Vec<Task>> {
        let mut out = Vec::new();
        for chunk in arns.chunks(100) {
            let r = self.ecs.describe_tasks().cluster(&self.cfg.cluster).set_tasks(Some(chunk.to_vec())).send().await.context("DescribeTasks")?;
            out.extend(r.tasks.unwrap_or_default());
        }
        Ok(out)
    }

    /// Tasks meant to run: those started as `started_by`, or all of the cluster's.
    pub(crate) async fn list(&self, started_by: Option<&str>) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        let mut token = None;
        loop {
            let r = self
                .ecs
                .list_tasks()
                .cluster(&self.cfg.cluster)
                .set_started_by(started_by.map(str::to_string))
                .desired_status(DesiredStatus::Running)
                .set_next_token(token)
                .send()
                .await
                .context("ListTasks")?;
            out.extend(r.task_arns.unwrap_or_default());
            token = r.next_token;
            if token.is_none() {
                return Ok(out);
            }
        }
    }

    pub(crate) async fn stop_task(&self, arn: &str, reason: &str) {
        if let Err(e) = self.ecs.stop_task().cluster(&self.cfg.cluster).task(arn).reason(reason).send().await {
            warn!(task = arn, error = %e, "StopTask");
        }
    }

    /// Stop every task started as `name`.
    pub(crate) async fn stop_named(&self, name: &str, reason: &str) {
        match self.list(Some(name)).await {
            Ok(arns) => {
                for arn in arns {
                    self.stop_task(&arn, reason).await;
                }
            }
            Err(e) => warn!(name, error = %format!("{e:#}"), "listing a run's tasks"),
        }
    }

    /// Look at a task until `done` says so or it stopped.
    pub(crate) async fn wait(&self, arn: &str, done: impl Fn(&Task) -> bool) -> anyhow::Result<Task> {
        loop {
            if let Some(t) = self.describe(arn).await? {
                if t.last_status() == Some("STOPPED") || done(&t) {
                    return Ok(t);
                }
            }
            tokio::time::sleep(self.cfg.poll).await;
        }
    }

    /// The last lines of a container's log.
    async fn log_lines(&self, template: &Template, container: &str, arn: &str, lines: usize) -> Vec<String> {
        let Some((group, stream)) = template.stream(container, arn) else { return Vec::new() };
        match self.logs.get_log_events().log_group_name(group).log_stream_name(stream).start_from_head(false).limit(lines.clamp(1, 10_000) as i32).send().await {
            Ok(out) => out.events().iter().filter_map(|e| e.message()).map(|m| m.trim_end_matches('\n').to_string()).collect(),
            Err(e) => {
                warn!(task = arn, error = %e, "reading a run's log");
                Vec::new()
            }
        }
    }

    async fn log_into(&self, template: &Template, container: &str, arn: &str, tail: &Mutex<Tail>) {
        let mut lines = self.log_lines(template, container, arn, 500).await;
        if lines.is_empty() {
            // The last lines can reach CloudWatch a moment after the task stopped.
            tokio::time::sleep(self.cfg.poll).await;
            lines = self.log_lines(template, container, arn, 500).await;
        }
        for line in lines {
            let mut t = tail.lock();
            t.push(line.as_bytes());
            t.push(b"\n");
        }
    }

    fn prepared(&self, name: &str) -> anyhow::Result<Prepared> {
        self.prepared.lock().get(name).cloned().with_context(|| format!("{name} wasn't prepared"))
    }

    async fn launch_job(&self, name: &str, p: &Prepared, spot: bool, port: Option<u16>) -> anyhow::Result<String> {
        let url = self.bundle_url(name).await?;
        let deadline = p.deadline.as_secs().max(60).to_string();
        let guard = match (p.kind, port) {
            (Kind::Preview, Some(port)) => vec![
                "roll-fetch".into(),
                "proxy".into(),
                "--listen".into(),
                self.cfg.preview_port.to_string(),
                "--to".into(),
                port.to_string(),
                "--idle".into(),
                deadline,
            ],
            _ => vec!["roll-fetch".into(), "deadline".into(), deadline],
        };
        let containers = vec![
            container_override(FETCH, Self::fetch_command(&url, p.kind.workdir(), None)),
            container_override(GUARD, guard),
            container_override(RUN, vec![RUN_SCRIPT.to_string(), p.run.clone()]),
        ];
        let group = format!("roll-{}", p.kind.as_str());
        let arn = self.launch(Launch { name, group: &group, task_def: &p.task_def, spot, cpu: p.cpu, memory: p.memory, disk_gib: None, containers }).await?;
        self.started.lock().insert(name.to_string(), Started { arn: arn.clone(), template: p.template.clone(), seen: None });
        Ok(arn)
    }
}

#[async_trait]
impl Isolation for Fargate {
    fn name(&self) -> &'static str {
        "fargate"
    }

    async fn prepare(&self, job: &Job, tree: &Path) -> anyhow::Result<()> {
        if job.kind == Kind::Preview && job.port == Some(self.cfg.preview_port) {
            bail!("port {} is roll's own in a preview task; use another", self.cfg.preview_port);
        }
        let family = match job.kind {
            Kind::Check => &self.cfg.check_task,
            Kind::Preview => &self.cfg.preview_task,
        };
        let template = self.template(family).await?;
        let task_def = self.task_def(&template, &job.image).await?;
        let env: BTreeMap<String, String> = job.env.iter().cloned().collect();
        self.upload(&job.name, env, Some(tree)).await?;
        let (cpu, memory) = fargate_size(job.cpus, job.memory_mb);
        let p = Prepared { kind: job.kind, template, task_def, run: job.run.clone(), cpu, memory, deadline: job.deadline };
        self.prepared.lock().insert(job.name.clone(), p);
        Ok(())
    }

    async fn attach(&self, name: &str, tail: Arc<Mutex<Tail>>) -> anyhow::Result<i32> {
        let p = self.prepared(name)?;
        let mut spot = self.cfg.spot;
        loop {
            let arn = self.launch_job(name, &p, spot, None).await?;
            let t = self.wait(&arn, |_| false).await?;
            if spot && t.stop_code() == Some(&TaskStopCode::SpotInterruption) {
                spot = false;
                tail.lock().push(b"(Fargate Spot took the capacity back; running it again on demand)\n");
                continue;
            }
            if let Some(code) = container(&t, RUN).and_then(Container::exit_code) {
                self.log_into(&p.template, RUN, &arn, &tail).await;
                return Ok(code);
            }
            // It never ran: the fetch's log says why, or the task's reason.
            self.log_into(&p.template, FETCH, &arn, &tail).await;
            bail!("the task stopped before the check ran: {}", stopped_reason(&t));
        }
    }

    async fn start(&self, name: &str, port: u16) -> anyhow::Result<SocketAddr> {
        let p = self.prepared(name)?;
        let arn = self.launch_job(name, &p, false, Some(port)).await?;
        // Running, and the tree is in (the app waits for `fetch` to succeed).
        let t = self.wait(&arn, |t| t.last_status() == Some("RUNNING") && container(t, FETCH).is_some_and(|c| c.exit_code().is_some())).await?;
        let fetched = container(&t, FETCH).and_then(Container::exit_code);
        if t.last_status() == Some("STOPPED") || fetched != Some(0) {
            let log = self.log_lines(&p.template, FETCH, &arn, 20).await.join("\n");
            bail!("the preview's task stopped: {}{}", stopped_reason(&t), if log.is_empty() { String::new() } else { format!("\n\n{log}") });
        }
        self.delete_bundle(name).await;
        let ip = t
            .containers()
            .iter()
            .flat_map(|c| c.network_interfaces())
            .find_map(|n| n.private_ipv4_address())
            .context("the preview's task has no private address")?;
        let ip: IpAddr = ip.parse().context("the preview's task address")?;
        Ok(SocketAddr::new(ip, self.cfg.preview_port))
    }

    async fn running(&self, name: &str) -> bool {
        let Some(s) = self.started.lock().get(name).cloned() else { return false };
        if let Some((at, up)) = s.seen
            && at.elapsed() < self.cfg.poll
        {
            return up;
        }
        let up = match self.describe(&s.arn).await {
            Ok(Some(t)) => t.last_status() == Some("RUNNING") && container(&t, RUN).is_none_or(|c| c.last_status() != Some("STOPPED")),
            Ok(None) => false,
            // A failed look isn't a stopped task.
            Err(_) => return s.seen.is_none_or(|(_, up)| up),
        };
        if let Some(e) = self.started.lock().get_mut(name) {
            e.seen = Some((Instant::now(), up));
        }
        up
    }

    async fn logs_tail(&self, name: &str, lines: usize) -> String {
        let Some(s) = self.started.lock().get(name).cloned() else { return String::new() };
        self.log_lines(&s.template, RUN, &s.arn, lines).await.join("\n")
    }

    async fn remove(&self, name: &str) {
        self.prepared.lock().remove(name);
        let started = self.started.lock().remove(name);
        if let Some(s) = started {
            self.stop_task(&s.arn, "roll: the run ended").await;
        }
        // Any other task of the name (a launch whose answer was lost, a previous owner's preview).
        self.stop_named(name, "roll: the run ended").await;
        self.delete_bundle(name).await;
    }

    /// Nothing to do: every task ends by itself (a check's deadline, a preview's idle guard), and
    /// a preview's next start stops older tasks of its name.
    async fn remove_leftovers(&self, _kind: Kind) {}

    /// Data URLs are public names on the fleet (the edge's): reached as they are.
    fn reach(&self, url: &str) -> String {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_round_up_to_fargate_shapes() {
        assert_eq!(fargate_size(0.25, 128), (256, 512));
        assert_eq!(fargate_size(1.0, 1024), (1024, 2048));
        assert_eq!(fargate_size(1.0, 3000), (1024, 3072));
        assert_eq!(fargate_size(0.5, 9000), (2048, 9216));
        assert_eq!(fargate_size(2.0, 16 << 10), (2048, 16384));
        assert_eq!(fargate_size(8.0, 17000), (8192, 20480));
        assert_eq!(fargate_size(64.0, 1), (16384, 122880));
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        assert!(FargateConfig::from_lookup(env(&[]), PathBuf::new()).unwrap().is_none());
        assert!(FargateConfig::from_lookup(env(&[("ROLL_CODE_RUNNER", "fargate")]), PathBuf::new()).is_err());
        assert!(FargateConfig::from_lookup(env(&[("ROLL_CODE_RUNNER", "k8s")]), PathBuf::new()).is_err());
        let c = FargateConfig::from_lookup(
            env(&[
                ("ROLL_CODE_RUNNER", "fargate"),
                ("ROLL_CODE_FARGATE_CLUSTER", "roll-cloud"),
                ("ROLL_CODE_FARGATE_SUBNETS", "subnet-a, subnet-b"),
                ("ROLL_CODE_FARGATE_SECURITY_GROUP", "sg-1"),
                ("ROLL_CODE_FARGATE_CHECK_TASK", "roll-cloud-check"),
                ("ROLL_CODE_FARGATE_PREVIEW_TASK", "roll-cloud-preview"),
                ("ROLL_S3_BUCKET", "data"),
            ]),
            PathBuf::new(),
        )
        .unwrap()
        .unwrap();
        assert_eq!((c.subnets.len(), c.public_ip, c.spot, c.preview_port, c.prefix.as_str()), (2, false, false, 9970, "code/runs/"));
    }
}
