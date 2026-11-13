//! Instances as Fargate tasks (docs/apps/README.md, "The runtime"): customer code never runs on
//! a fleet node. Each task is a Firecracker microVM in the isolated subnets and the apps
//! security group, with a task role that can do nothing (or none at all).
//!
//! - **A task definition per deployment image**, registered by roll: family
//!   `roll-apps-<service>-<hash>`, the hash over the image, the Fargate size, the port, whether a
//!   start command replaces the image's own, and the stack's roles, log group and architecture.
//!   One container, `app`. A definition another process registered is found by its family and
//!   reused, so a wake never registers anything.
//! - [`Runtime::create`] registers (or finds) the definition and records the instance: nothing
//!   runs. [`Runtime::start`] is `RunTask` (awsvpc, the configured subnets and security group,
//!   `startedBy` `roll-apps-<service>`, tags `sh.roll.apps=<service>` and
//!   `sh.roll.apps.instance=<name>`), waits for RUNNING and answers the task's private address on
//!   the app's port. [`Runtime::stop`] is `StopTask`; a stopped instance stays created, and the
//!   next start is a new task. Fargate can't freeze a task, so [`Runtime::can_suspend`] is false
//!   and [`Runtime::settle`] does nothing (no CPU boost: the size is the task's).
//! - **Variables never go in a `RunTask` request** (it shows in `DescribeTasks` and CloudTrail).
//!   Each start writes them to an object of its own, `<prefix>env/<instance>-<random>.env` in
//!   the stack's bucket, and the request names only that object's ARN as the `app` container's
//!   environment file. ECS reads it with the execution role before the container starts, and
//!   roll deletes it as soon as the task is RUNNING (or stopped, or failed to start). This is
//!   code-preview's bundle with ECS as its `fetch`: an app keeps its image's own entrypoint and
//!   command (a `fetch` container would need a shell in the image to source the env, and
//!   distroless images have none), and no roll image goes in the task. An environment file holds
//!   one line per variable, so a value with a line break is refused at `create`.
//! - Output goes to CloudWatch Logs (`awslogs`, stream `apps/app/<task>`) and is followed into
//!   the instance's log until the task stops.
//! - An image from a private registry pulls with the service's login, kept as a Secrets Manager
//!   secret of its own (`roll-apps/registry/<service>`) that the task definition names as its
//!   repository credentials: the password is never in a task definition or a `RunTask`.
//! - Not yet: private networking (Cloud Map), volumes (EFS), shells (ECS Exec). A spec with a
//!   volume is refused; the project
//!   network and its aliases are ignored.

use super::aws::{Bucket, FleetConfig};
use super::{InstanceSpec, LABEL, PREFIX, RegistryLogin, Runtime, Usage};
use crate::logs::{LogSink, Stream};
use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use aws_sdk_ecs::types::{
    AssignPublicIp, AwsVpcConfiguration, CapacityProviderStrategyItem, Compatibility, Container, ContainerDefinition, ContainerOverride, CpuArchitecture,
    DesiredStatus, EnvironmentFile, EnvironmentFileType, LinuxParameters, LogConfiguration, LogDriver, NetworkConfiguration, NetworkMode, OsFamily,
    PortMapping, RuntimePlatform, Tag, Task, TaskDefinitionStatus, TaskField, TaskOverride, TransportProtocol,
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{info, warn};

/// The guard proxy beside a web task's app (`roll-fetch proxy`).
pub const GUARD: &str = "guard";
/// The app's container of a task.
pub const APP: &str = "app";
/// The tag naming the instance a task is.
pub const INSTANCE_TAG: &str = "sh.roll.apps.instance";
const STREAM_PREFIX: &str = "apps";
/// Seconds ECS waits after SIGTERM before SIGKILL (Fargate's longest is 120).
const STOP_TIMEOUT: i32 = 30;

/// The smallest Fargate size with at least `cpus` and `memory_mb`: (CPU units, MiB). The same
/// table as code-preview's.
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

/// The variables as an ECS environment file: `NAME=value` lines, values verbatim.
fn env_file(vars: &[(String, String)]) -> anyhow::Result<String> {
    let mut out = String::new();
    for (k, v) in vars {
        anyhow::ensure!(roll_fetch::valid_name(k), "variable {k:?} isn't a name an environment can hold");
        if v.contains(['\n', '\r', '\0']) {
            bail!("variable {k} has a line break, and on Fargate a value is one line (an ECS environment file)");
        }
        out.push_str(&format!("{k}={v}\n"));
    }
    Ok(out)
}

fn short(service: &str) -> &str {
    service.strip_prefix("svc_").unwrap_or(service)
}

/// `startedBy` of a service's tasks (ECS takes at most 36 characters).
fn started_by(service: &str) -> String {
    let s = format!("{PREFIX}{}", short(service));
    s[..s.len().min(36)].to_string()
}

fn container<'a>(t: &'a Task, name: &str) -> Option<&'a Container> {
    t.containers().iter().find(|c| c.name() == Some(name))
}

fn tag<'a>(t: &'a Task, key: &str) -> Option<&'a str> {
    t.tags().iter().find(|x| x.key() == Some(key)).and_then(|x| x.value())
}

/// The task's private address: on its container, or on its network interface attachment.
fn private_ip(t: &Task) -> Option<IpAddr> {
    let on_container = t.containers().iter().flat_map(|c| c.network_interfaces()).find_map(|n| n.private_ipv4_address());
    let on_eni = || {
        t.attachments()
            .iter()
            .filter(|a| a.r#type() == Some("ElasticNetworkInterface"))
            .flat_map(|a| a.details())
            .find(|d| d.name() == Some("privateIPv4Address"))
            .and_then(|d| d.value())
    };
    on_container.or_else(on_eni)?.parse().ok()
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

fn stopped(t: &Task) -> bool {
    t.last_status() == Some("STOPPED")
}

struct Instance {
    spec: InstanceSpec,
    task_def: String,
    task: Option<Started>,
}

/// The task an instance's last start launched.
#[derive(Clone)]
struct Started {
    arn: String,
    /// Its environment file, until the task has read it.
    env_key: Option<String>,
    addr: Option<SocketAddr>,
    /// The last look, for `running`.
    seen: Option<(Instant, bool)>,
    /// True once its log has been followed to the end.
    drained: watch::Receiver<bool>,
}

pub struct Fargate {
    cfg: FleetConfig,
    me: Weak<Fargate>,
    ecs: aws_sdk_ecs::Client,
    logs: aws_sdk_cloudwatchlogs::Client,
    /// Private registry logins, as the tasks' repository credentials.
    secrets: aws_sdk_secretsmanager::Client,
    bucket: Bucket,
    registry: reqwest::Client,
    /// Task definitions by family. Held while one is registered, so instances of a new image at
    /// once register it once.
    defs: tokio::sync::Mutex<HashMap<String, String>>,
    instances: Mutex<HashMap<String, Instance>>,
    /// Tasks `list` found that no instance here started (an earlier process's): `remove` stops
    /// them.
    found: Mutex<HashMap<String, Vec<String>>>,
}

impl Fargate {
    pub async fn new(cfg: FleetConfig) -> anyhow::Result<Arc<Fargate>> {
        let sdk = cfg.sdk().await;
        let mut registry = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent("roll-apps");
        if cfg.registry.is_none() {
            registry = registry.dns_resolver(Arc::new(crate::net::PublicOnly));
        }
        let (registry, bucket) = (registry.build()?, cfg.bucket()?);
        Ok(Arc::new_cyclic(|me| Fargate {
            me: me.clone(),
            ecs: aws_sdk_ecs::Client::new(&sdk),
            logs: aws_sdk_cloudwatchlogs::Client::new(&sdk),
            secrets: aws_sdk_secretsmanager::Client::new(&sdk),
            bucket,
            registry,
            cfg,
            defs: tokio::sync::Mutex::default(),
            instances: Mutex::default(),
            found: Mutex::default(),
        }))
    }

    /// The service's registry login as a Secrets Manager secret
    /// (`roll-apps/registry/<service>`, `{"username", "password"}` as ECS reads it), made or
    /// brought up to date: its ARN.
    async fn registry_secret(&self, service: &str, login: &RegistryLogin) -> anyhow::Result<String> {
        let name = format!("roll-apps/registry/{}", short(service));
        let value = serde_json::json!({ "username": login.username, "password": login.password }).to_string();
        let made = self
            .secrets
            .create_secret()
            .name(&name)
            .description("roll apps: a service's private registry login")
            .secret_string(&value)
            .tags(aws_sdk_secretsmanager::types::Tag::builder().key(LABEL).value(service).build())
            .send()
            .await;
        match made {
            Ok(out) => out.arn.context("CreateSecret returned no ARN"),
            Err(e) if e.as_service_error().is_some_and(|e| e.is_resource_exists_exception()) => {
                let out = self.secrets.put_secret_value().secret_id(&name).secret_string(&value).send().await.map_err(|e| anyhow::anyhow!("updating {name}: {}", aws_sdk_secretsmanager::error::DisplayErrorContext(e)))?;
                out.arn.context("PutSecretValue returned no ARN")
            }
            Err(e) => Err(anyhow::anyhow!("storing {name}: {}", aws_sdk_secretsmanager::error::DisplayErrorContext(e))),
        }
    }

    /// The task definition for `spec`: found or registered once, then reused.
    async fn task_def(&self, spec: &InstanceSpec) -> anyhow::Result<String> {
        let (cpu, memory) = fargate_size(spec.cpu, spec.memory_mb);
        let shell = spec.command.is_some();
        let c = &self.cfg;
        // A private image pulls with the service's registry secret (ECS reads it with the
        // execution role); its ARN is part of the task definition.
        let credentials = match &spec.registry {
            Some(login) => Some(self.registry_secret(&spec.service, login).await?),
            None => None,
        };
        let key = format!("{}\n{cpu}\n{memory}\n{:?}\n{shell}\n{}\n{:?}\n{}\n{}\n{}\n{}\n{:?}\n{:?}", spec.image, spec.port, c.execution_role, c.task_role, c.log_group, c.arch, c.guard_image, c.port, spec.idle_stop_s, credentials);
        let hash = hex::encode(Sha256::digest(key));
        let family = format!("{PREFIX}{}-{}", short(&spec.service), &hash[..16]);
        let mut defs = self.defs.lock().await;
        if let Some(arn) = defs.get(&family) {
            return Ok(arn.clone());
        }
        // Registered by this process before, or by another: same family, same image.
        let found = match self.ecs.describe_task_definition().task_definition(&family).send().await {
            Ok(out) => out
                .task_definition
                .filter(|d| d.status() == Some(&TaskDefinitionStatus::Active))
                .filter(|d| d.container_definitions().iter().any(|c| c.name() == Some(APP) && c.image() == Some(spec.image.as_str())))
                .and_then(|d| d.task_definition_arn),
            Err(_) => None,
        };
        let arn = match found {
            Some(arn) => arn,
            None => {
                let logs = LogConfiguration::builder()
                    .log_driver(LogDriver::Awslogs)
                    .options("awslogs-group", &c.log_group)
                    .options("awslogs-region", c.region.as_deref().unwrap_or("us-east-1"))
                    .options("awslogs-stream-prefix", STREAM_PREFIX)
                    .build()?;
                let mut app = ContainerDefinition::builder()
                    .name(APP)
                    .image(&spec.image)
                    .essential(true)
                    .stop_timeout(STOP_TIMEOUT)
                    .linux_parameters(LinuxParameters::builder().init_process_enabled(true).build())
                    .log_configuration(logs);
                if shell {
                    app = app.entry_point("sh").entry_point("-c");
                }
                if let Some(arn) = &credentials {
                    app = app.repository_credentials(aws_sdk_ecs::types::RepositoryCredentials::builder().credentials_parameter(arn).build()?);
                }
                // Nodes reach a web task only on the one port the isolated subnets admit: roll's
                // guard proxy forwards it to the app's own port (containers of a task share
                // localhost), and ends the task once it's idle well past the service's sleep, so
                // one whose node went away still stops.
                let mut containers = vec![];
                if let Some(p) = spec.port {
                    let idle = spec.idle_stop_s.unwrap_or(315_360_000).to_string();
                    let guard = ContainerDefinition::builder()
                        .name(GUARD)
                        .image(&c.guard_image)
                        .essential(true)
                        .command("proxy")
                        .command("--listen")
                        .command(c.port.to_string())
                        .command("--to")
                        .command(p.to_string())
                        .command("--idle")
                        .command(idle)
                        .port_mappings(PortMapping::builder().container_port(c.port as i32).protocol(TransportProtocol::Tcp).build())
                        .log_configuration(
                            LogConfiguration::builder()
                                .log_driver(LogDriver::Awslogs)
                                .options("awslogs-group", &c.log_group)
                                .options("awslogs-region", c.region.as_deref().unwrap_or("us-east-1"))
                                .options("awslogs-stream-prefix", STREAM_PREFIX)
                                .build()?,
                        )
                        .build();
                    containers.push(guard);
                }
                containers.insert(0, app.build());
                let out = self
                    .ecs
                    .register_task_definition()
                    .family(&family)
                    .network_mode(NetworkMode::Awsvpc)
                    .requires_compatibilities(Compatibility::Fargate)
                    .cpu(cpu.to_string())
                    .memory(memory.to_string())
                    .execution_role_arn(&c.execution_role)
                    .set_task_role_arn(c.task_role.clone())
                    .runtime_platform(RuntimePlatform::builder().cpu_architecture(CpuArchitecture::from(c.arch.as_str())).operating_system_family(OsFamily::Linux).build())
                    .set_container_definitions(Some(containers))
                    .tags(Tag::builder().key(LABEL).value(&spec.service).build())
                    .send()
                    .await
                    .with_context(|| format!("registering a task definition for {}", spec.image))?;
                let arn = out.task_definition.and_then(|d| d.task_definition_arn).context("RegisterTaskDefinition returned no ARN")?;
                info!(%family, image = %spec.image, "apps: registered a task definition");
                arn
            }
        };
        defs.insert(family, arn.clone());
        Ok(arn)
    }

    async fn describe(&self, arn: &str) -> anyhow::Result<Option<Task>> {
        let out = self.ecs.describe_tasks().cluster(&self.cfg.cluster).tasks(arn).send().await.context("DescribeTasks")?;
        Ok(out.tasks.and_then(|t| t.into_iter().next()))
    }

    async fn stop_task(&self, arn: &str, reason: &str) {
        if let Err(e) = self.ecs.stop_task().cluster(&self.cfg.cluster).task(arn).reason(reason).send().await {
            warn!(task = arn, error = %e, "apps: StopTask");
        }
    }

    /// Tasks meant to run as `started_by` (or all of the cluster's), with their tags.
    async fn tasks(&self, started_by: Option<&str>) -> anyhow::Result<Vec<Task>> {
        let mut arns = Vec::new();
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
            arns.extend(r.task_arns.unwrap_or_default());
            token = r.next_token;
            if token.is_none() {
                break;
            }
        }
        let mut out = Vec::new();
        for chunk in arns.chunks(100) {
            let r = self.ecs.describe_tasks().cluster(&self.cfg.cluster).set_tasks(Some(chunk.to_vec())).include(TaskField::Tags).send().await.context("DescribeTasks")?;
            out.extend(r.tasks.unwrap_or_default());
        }
        Ok(out)
    }

    /// Look at a task until it's RUNNING or STOPPED, for at most `limit`.
    async fn until_up(&self, arn: &str, limit: Duration) -> anyhow::Result<Option<Task>> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(t) = self.describe(arn).await?
                && (stopped(&t) || t.last_status() == Some("RUNNING"))
            {
                return Ok(Some(t));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(self.cfg.poll).await;
        }
    }

    /// Follow the task's `app` log into `log` until the task has stopped and its last lines are
    /// in.
    async fn follow(self: Arc<Self>, arn: String, log: LogSink, drained: watch::Sender<bool>) {
        let id = arn.rsplit('/').next().unwrap_or(&arn).to_string();
        let stream = format!("{STREAM_PREFIX}/{APP}/{id}");
        let out = log.stream(Stream::Stdout);
        let mut token: Option<String> = None;
        let mut last_look = false;
        let mut misses = 0;
        loop {
            // Every page there is now (a page is at most 10,000 events).
            for _ in 0..16 {
                let r = self
                    .logs
                    .get_log_events()
                    .log_group_name(&self.cfg.log_group)
                    .log_stream_name(&stream)
                    .start_from_head(true)
                    .set_next_token(token.clone())
                    .send()
                    .await;
                let Ok(r) = r else { break };
                for e in r.events() {
                    if let Some(m) = e.message() {
                        out.line(m.trim_end_matches(['\n', '\r']));
                    }
                }
                let next = r.next_forward_token().map(str::to_string);
                let done = r.events().is_empty() || next.is_none() || next == token;
                if next.is_some() {
                    token = next;
                }
                if done {
                    break;
                }
            }
            if last_look {
                break;
            }
            match self.describe(&arn).await {
                // The last lines can reach CloudWatch a moment after the task stopped.
                Ok(Some(t)) if stopped(&t) => last_look = true,
                Ok(Some(_)) => misses = 0,
                Ok(None) => break,
                Err(_) => {
                    misses += 1;
                    if misses > 30 {
                        break;
                    }
                }
            }
            tokio::time::sleep(self.cfg.poll).await;
        }
        let _ = drained.send(true);
    }

    fn started(&self, name: &str) -> Option<Started> {
        self.instances.lock().get(name).and_then(|i| i.task.clone())
    }

    /// Delete a task's environment file once ECS has read it (or never will).
    async fn drop_env(&self, name: &str, arn: &str) {
        let key = self.instances.lock().get_mut(name).and_then(|i| i.task.as_mut()).filter(|s| s.arn == arn).and_then(|s| s.env_key.take());
        if let Some(k) = key {
            self.bucket.delete(&k).await;
        }
    }

    /// The image's digest from its registry: a manifest `HEAD`, anonymous or with the
    /// registry's pull token (anonymous, or for `login`), or `login` itself on a registry that
    /// asks for Basic.
    async fn digest(&self, image: &str, login: Option<&RegistryLogin>) -> anyhow::Result<String> {
        let r = ImageRef::parse(image);
        let base = self.cfg.registry.clone().unwrap_or_else(|| format!("https://{}", r.registry));
        let url = format!("{}/v2/{}/manifests/{}", base.trim_end_matches('/'), r.repo, r.reference);
        let accept = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.manifest.v1+json";
        let mut resp = self.registry.head(&url).header("accept", accept).send().await.with_context(|| format!("asking {} for {image}", r.registry))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let challenge = resp.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
            resp = match (challenge.starts_with("Basic"), login) {
                (true, Some(l)) => self.registry.head(&url).header("accept", accept).basic_auth(&l.username, Some(&l.password)).send().await?,
                _ => {
                    let token = self.token(&challenge, login).await.with_context(|| format!("{} asked for a login", r.registry))?;
                    self.registry.head(&url).header("accept", accept).bearer_auth(token).send().await?
                }
            };
        }
        if !resp.status().is_success() {
            bail!("{} answered {} for {image}", r.registry, resp.status());
        }
        let digest = resp.headers().get("docker-content-digest").and_then(|v| v.to_str().ok()).unwrap_or_default();
        anyhow::ensure!(digest.starts_with("sha256:") && digest.len() == 71, "{} gave no digest for {image}", r.registry);
        Ok(format!("{}@{digest}", r.name))
    }

    /// A pull token, from a `Bearer realm=…,service=…,scope=…` challenge: anonymous, or for
    /// `login` (Basic to the token service, as `docker login` does).
    async fn token(&self, challenge: &str, login: Option<&RegistryLogin>) -> anyhow::Result<String> {
        let params = challenge.strip_prefix("Bearer ").context("not a bearer challenge")?;
        let get = |k: &str| {
            params.split(',').find_map(|p| {
                let (n, v) = p.trim().split_once('=')?;
                (n == k).then(|| v.trim_matches('"').to_string())
            })
        };
        let realm = get("realm").context("no realm")?;
        let mut url = reqwest::Url::parse(&realm)?;
        if self.cfg.registry.is_none() && (url.scheme() != "https" || url.host_str().is_none_or(crate::model::private_host)) {
            bail!("the token service {realm} isn't a public https address");
        }
        for k in ["service", "scope"] {
            if let Some(v) = get(k) {
                url.query_pairs_mut().append_pair(k, &v);
            }
        }
        let mut req = self.registry.get(url);
        if let Some(l) = login {
            req = req.basic_auth(&l.username, Some(&l.password));
        }
        let resp = req.send().await?.error_for_status()?;
        anyhow::ensure!(resp.content_length().is_none_or(|n| n <= 64 << 10), "the token answer is too large");
        let body = resp.bytes().await?;
        anyhow::ensure!(body.len() <= 64 << 10, "the token answer is too large");
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        body["token"].as_str().or(body["access_token"].as_str()).map(str::to_string).context("no token in the answer")
    }
}

/// An image reference split for the registry API.
#[derive(Debug, PartialEq)]
struct ImageRef {
    /// The reference without its tag, as Fargate takes it with `@<digest>`.
    name: String,
    registry: String,
    repo: String,
    reference: String,
}

impl ImageRef {
    fn parse(image: &str) -> ImageRef {
        let (name, reference) = match image.rsplit_once(':') {
            Some((n, t)) if !t.contains('/') => (n, t),
            _ => (image, "latest"),
        };
        let (registry, repo) = match crate::model::registry_host(name) {
            Some(h) => (h.to_string(), name[h.len() + 1..].to_string()),
            None => ("registry-1.docker.io".to_string(), name.to_string()),
        };
        let docker_hub = matches!(registry.as_str(), "registry-1.docker.io" | "docker.io" | "index.docker.io");
        let repo = if docker_hub && !repo.contains('/') { format!("library/{repo}") } else { repo };
        let registry = if docker_hub { "registry-1.docker.io".to_string() } else { registry };
        ImageRef { name: name.to_string(), registry, repo, reference: reference.to_string() }
    }
}

#[async_trait]
impl Runtime for Fargate {
    fn name(&self) -> &'static str {
        "fargate"
    }

    fn can_suspend(&self) -> bool {
        false
    }

    /// Fargate pulls at each start; this pins the image's digest, so a moving tag never changes
    /// a deployment.
    async fn pull(&self, image: &str, log: &LogSink, login: Option<&RegistryLogin>) -> anyhow::Result<String> {
        if image.contains("@sha256:") {
            return Ok(image.to_string());
        }
        log.line(format!("resolving {image}"));
        self.digest(image, login).await
    }

    async fn create(&self, spec: &InstanceSpec) -> anyhow::Result<()> {
        if spec.volume.is_some() {
            bail!("volumes aren't on the fleet yet");
        }
        env_file(&spec.vars)?;
        let task_def = self.task_def(spec).await?;
        self.instances.lock().insert(spec.name.clone(), Instance { spec: spec.clone(), task_def, task: None });
        Ok(())
    }

    async fn start(&self, name: &str, log: LogSink) -> anyhow::Result<Option<SocketAddr>> {
        let (spec, task_def, before) = {
            let m = self.instances.lock();
            let i = m.get(name).with_context(|| format!("{name} wasn't created"))?;
            (i.spec.clone(), i.task_def.clone(), i.task.clone())
        };
        if let Some(s) = before {
            if self.running(name).await {
                return Ok(s.addr);
            }
            // The last task ended by itself.
            self.drop_env(name, &s.arn).await;
        }
        let env_key = format!("{}env/{name}-{}.env", self.cfg.prefix, crate::model::random(8));
        self.bucket.put(&env_key, env_file(&spec.vars)?.into_bytes()).await?;
        let env = EnvironmentFile::builder().value(self.cfg.object_arn(&env_key)).r#type(EnvironmentFileType::S3).build()?;
        let mut app = ContainerOverride::builder().name(APP).environment_files(env);
        if let Some(cmd) = &spec.command {
            app = app.command(cmd);
        }
        let vpc = AwsVpcConfiguration::builder()
            .set_subnets(Some(self.cfg.subnets.clone()))
            .security_groups(&self.cfg.security_group)
            .assign_public_ip(if self.cfg.public_ip { AssignPublicIp::Enabled } else { AssignPublicIp::Disabled })
            .build()?;
        let tags = [(LABEL, spec.service.as_str()), (INSTANCE_TAG, name), ("sh.roll.apps.environment", &spec.env), ("sh.roll.apps.deployment", &spec.deployment)];
        let run = self
            .ecs
            .run_task()
            .cluster(&self.cfg.cluster)
            .task_definition(&task_def)
            .count(1)
            .started_by(started_by(&spec.service))
            .group(format!("{PREFIX}{}", short(&spec.service)))
            .client_token(crate::model::random(32))
            .capacity_provider_strategy(CapacityProviderStrategyItem::builder().capacity_provider("FARGATE").weight(1).build()?)
            .network_configuration(NetworkConfiguration::builder().awsvpc_configuration(vpc).build())
            .overrides(TaskOverride::builder().container_overrides(app.build()).build())
            .set_tags(Some(tags.iter().map(|(k, v)| Tag::builder().key(*k).value(*v).build()).collect()))
            .send()
            .await
            .context("RunTask");
        let launched = match run {
            Ok(out) => match (out.failures().first(), out.tasks().first().and_then(|t| t.task_arn())) {
                (None, Some(arn)) => Ok(arn.to_string()),
                (f, _) => Err(anyhow!("RunTask: {}", f.map(|f| format!("{} {}", f.reason().unwrap_or("failed"), f.detail().unwrap_or(""))).unwrap_or_else(|| "no task started".into()))),
            },
            Err(e) => Err(e),
        };
        let arn = match launched {
            Ok(arn) => arn,
            Err(e) => {
                self.bucket.delete(&env_key).await;
                return Err(e);
            }
        };
        let (drained_tx, drained) = watch::channel(false);
        let kept = match self.instances.lock().get_mut(name) {
            Some(i) => {
                i.task = Some(Started { arn: arn.clone(), env_key: Some(env_key.clone()), addr: None, seen: None, drained });
                true
            }
            None => false,
        };
        if !kept {
            self.stop_task(&arn, "roll: the instance was removed").await;
            self.bucket.delete(&env_key).await;
            bail!("{name} was removed while it started");
        }
        if let Some(me) = self.me.upgrade() {
            tokio::spawn(me.follow(arn.clone(), log, drained_tx));
        }
        let up = self.until_up(&arn, self.cfg.start_timeout).await;
        self.drop_env(name, &arn).await;
        let t = match up {
            Ok(Some(t)) => t,
            Ok(None) => {
                self.stop_task(&arn, "roll: it didn't start in time").await;
                bail!("the task didn't start within {} s", self.cfg.start_timeout.as_secs());
            }
            Err(e) => {
                self.stop_task(&arn, "roll: lost track of it while it started").await;
                return Err(e);
            }
        };
        // Stopped already: a job that ran (its exit is `wait`'s), or a task that never did.
        if stopped(&t) && container(&t, APP).and_then(Container::exit_code).is_none() {
            bail!("the task stopped before the app ran: {}", stopped_reason(&t));
        }
        let addr = match spec.port {
            Some(_) => Some(SocketAddr::new(private_ip(&t).context("the task has no private address")?, self.cfg.port)),
            None => None,
        };
        if let Some(s) = self.instances.lock().get_mut(name).and_then(|i| i.task.as_mut()).filter(|s| s.arn == arn) {
            s.addr = addr;
        }
        Ok(addr)
    }

    async fn suspend(&self, _name: &str) -> anyhow::Result<()> {
        bail!("Fargate can't freeze a task")
    }

    async fn resume(&self, _name: &str) -> anyhow::Result<()> {
        bail!("Fargate can't freeze a task")
    }

    /// `StopTask`: SIGTERM, then SIGKILL after the task definition's stop timeout (30 s; ECS
    /// takes no grace per call). The instance stays created; the next start is a new task.
    async fn stop(&self, name: &str, _grace: Duration) -> anyhow::Result<()> {
        let Some(s) = self.instances.lock().get_mut(name).and_then(|i| i.task.take()) else { return Ok(()) };
        self.stop_task(&s.arn, "roll: stopped").await;
        if let Some(k) = s.env_key {
            self.bucket.delete(&k).await;
        }
        Ok(())
    }

    async fn remove(&self, name: &str) {
        let _ = self.stop(name, Duration::ZERO).await;
        self.instances.lock().remove(name);
        let found = self.found.lock().remove(name).unwrap_or_default();
        for arn in found {
            self.stop_task(&arn, "roll: not one of this node's instances").await;
        }
    }

    async fn wait(&self, name: &str) -> anyhow::Result<i32> {
        let s = self.started(name).with_context(|| format!("{name} isn't running"))?;
        let t = loop {
            match self.describe(&s.arn).await? {
                Some(t) if stopped(&t) => break t,
                Some(_) => tokio::time::sleep(self.cfg.poll).await,
                None => bail!("the task is gone"),
            }
        };
        // Its last lines into the log before the exit is reported.
        let mut drained = s.drained.clone();
        let _ = tokio::time::timeout(self.cfg.poll * 4 + Duration::from_secs(1), drained.wait_for(|d| *d)).await;
        container(&t, APP).and_then(Container::exit_code).ok_or_else(|| anyhow!("the task stopped before the app ran: {}", stopped_reason(&t)))
    }

    async fn exited(&self, name: &str) -> Option<super::Exit> {
        let s = self.started(name)?;
        let t = self.describe(&s.arn).await.ok()??;
        let c = container(&t, APP)?;
        (c.last_status() == Some("STOPPED")).then(|| super::Exit {
            code: c.exit_code(),
            oom: c.reason().is_some_and(|r| r.contains("OutOfMemory")) || t.stopped_reason().is_some_and(|r| r.contains("OutOfMemory")),
        })
    }

    async fn running(&self, name: &str) -> bool {
        let Some(s) = self.started(name) else { return false };
        if let Some((at, up)) = s.seen
            && at.elapsed() < self.cfg.poll
        {
            return up;
        }
        let up = match self.describe(&s.arn).await {
            Ok(Some(t)) => t.last_status() == Some("RUNNING") && container(&t, APP).is_none_or(|c| c.last_status() != Some("STOPPED")),
            Ok(None) => false,
            // A failed look isn't a stopped task.
            Err(_) => return s.seen.is_none_or(|(_, up)| up),
        };
        if let Some(t) = self.instances.lock().get_mut(name).and_then(|i| i.task.as_mut()).filter(|t| t.arn == s.arn) {
            t.seen = Some((Instant::now(), up));
        }
        up
    }

    /// None: a task's CPU time and memory aren't read here. Fargate bills by size × time, so on
    /// the fleet the meter is meant to charge the allocation while running, not the cgroup
    /// readings Docker gives (with none, it records instance-seconds and no CPU or memory yet).
    async fn usage(&self, _name: &str) -> Option<Usage> {
        None
    }

    /// The instances made here, and the service's tasks still running from another process
    /// (`remove` stops those).
    async fn list(&self, service: Option<&str>) -> Vec<String> {
        let mut names: Vec<String> = self.instances.lock().iter().filter(|(_, i)| service.is_none_or(|s| i.spec.service == s)).map(|(n, _)| n.clone()).collect();
        let tasks = match self.tasks(service.map(started_by).as_deref()).await {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %format!("{e:#}"), "apps: listing tasks");
                return names;
            }
        };
        let ours: Vec<String> = self.instances.lock().values().filter_map(|i| i.task.as_ref().map(|t| t.arn.clone())).collect();
        let mut found = self.found.lock();
        for t in &tasks {
            let (Some(svc), Some(name), Some(arn)) = (tag(t, LABEL), tag(t, INSTANCE_TAG), t.task_arn()) else { continue };
            if service.is_some_and(|s| s != svc) || ours.iter().any(|a| a == arn) {
                continue;
            }
            let arns = found.entry(name.to_string()).or_default();
            if !arns.iter().any(|a| a == arn) {
                arns.push(arn.to_string());
            }
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
        names
    }

    /// Nothing yet: tasks aren't on a project network (Cloud Map is later), so a service reaches
    /// another by its public URL.
    async fn ensure_network(&self, _name: &str, _service: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Nothing: `create` refuses volumes on the fleet (EFS is later).
    async fn remove_volume(&self, _name: &str) {}

    /// Data URLs are public names on the fleet (the edge's): reached as they are.
    fn reach(&self, url: &str) -> String {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_env_files_and_image_references() {
        assert_eq!(fargate_size(0.25, 256), (256, 512));
        assert_eq!(fargate_size(1.0, 3000), (1024, 3072));
        assert_eq!(fargate_size(8.0, 16384), (8192, 16384));
        assert_eq!(env_file(&[("A".into(), "x=y z".into())]).unwrap(), "A=x=y z\n");
        let err = env_file(&[("KEY".into(), "-----BEGIN-----\nabc".into())]).unwrap_err().to_string();
        assert!(err.contains("KEY") && !err.contains("abc"), "{err}");
        assert!(env_file(&[("1X".into(), "y".into())]).is_err());
        assert_eq!(started_by("svc_abcdefghijkl"), "roll-apps-abcdefghijkl");
        let r = ImageRef::parse("nginx:1.27");
        assert_eq!((r.name.as_str(), r.registry.as_str(), r.repo.as_str(), r.reference.as_str()), ("nginx", "registry-1.docker.io", "library/nginx", "1.27"));
        let r = ImageRef::parse("ghcr.io/acme/web");
        assert_eq!((r.name.as_str(), r.registry.as_str(), r.repo.as_str(), r.reference.as_str()), ("ghcr.io/acme/web", "ghcr.io", "acme/web", "latest"));
        let r = ImageRef::parse("registry.example.com:5000/team/app:v2");
        assert_eq!((r.registry.as_str(), r.repo.as_str(), r.reference.as_str()), ("registry.example.com:5000", "team/app", "v2"));
    }
}
