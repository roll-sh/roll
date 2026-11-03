//! The docker backend (self-hosted and development): each sandbox is a container of the sandbox
//! image (`deploy/agent/Dockerfile`) run through the `docker` CLI, like code-preview's checks,
//! and hardened: no new privileges, every capability dropped, memory (no swap), CPU and process
//! limits, an init process, a named volume for the home directory, a network of their own where
//! sandboxes can't reach each other (never the host's), no Docker socket, and gVisor's `runsc`
//! runtime when Docker has it. Blocking the host and cloud metadata addresses from that network
//! takes a firewall rule, which roll never makes itself (docs/code/agents.md, "Sandboxes").
//!
//! The env (the machine token, the owner's provider keys) goes in through an env file only this
//! user can read, removed once `docker run` returns: never on a command line.

use super::{BackendError, BackendState, SandboxBackend, SandboxSpec, Tagged};
use async_trait::async_trait;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// The label every sandbox container carries: roll's sandbox id.
pub const LABEL: &str = "sh.roll.agents.sandbox";
/// The network sandboxes join unless the operator names another.
pub const NETWORK: &str = "roll-sandboxes";
/// The home directory of the image's user (`roll`), kept on the sandbox's volume.
pub const HOME: &str = "/home/roll";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Runtime {
    /// `runsc` when Docker has it, else Docker's default.
    Auto,
    /// Docker's default runtime (`runc`).
    Default,
    Named(String),
}

impl Runtime {
    pub fn parse(s: &str) -> Runtime {
        match s.trim() {
            "" | "auto" => Runtime::Auto,
            "default" | "runc" => Runtime::Default,
            other => Runtime::Named(other.to_string()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DockerConfig {
    pub image: String,
    pub runtime: Runtime,
    /// A Docker network the operator made to attach to, instead of [`Self::dedicated_network`].
    pub network: Option<String>,
    /// The bridge network sandboxes join otherwise: made on first use, with traffic between its
    /// containers off (`enable_icc=false`).
    pub dedicated_network: String,
    /// Cap each container's writable layer at the size's disk (`--storage-opt size=`; only on
    /// storage drivers that support it, such as overlay2 on xfs with `pquota`). The home volume
    /// isn't capped by Docker's local volume driver either way.
    pub disk_limit: bool,
    /// Map `host.docker.internal` to the host (a roll on the same machine, in development).
    pub host_gateway: bool,
    pub pids_limit: u32,
    /// Capabilities to add back after dropping them all (the image needs none).
    pub cap_add: Vec<String>,
    /// Run this instead of the image's command (tests run a stand-in image).
    pub command: Vec<String>,
}

impl Default for DockerConfig {
    fn default() -> Self {
        DockerConfig {
            image: "roll-agent-sandbox:latest".into(),
            runtime: Runtime::Auto,
            network: None,
            dedicated_network: NETWORK.into(),
            disk_limit: false,
            host_gateway: false,
            pids_limit: 1024,
            cap_add: Vec::new(),
            command: Vec::new(),
        }
    }
}

pub struct Docker {
    config: DockerConfig,
    /// What `Runtime::Auto` resolved to.
    runtime: tokio::sync::OnceCell<Option<String>>,
}

/// How long one docker command may take (`run` may pull the image).
const RUN_TIMEOUT: Duration = Duration::from_secs(600);
const TIMEOUT: Duration = Duration::from_secs(60);

/// What a docker command said: stdout, or why it failed.
async fn docker(args: &[&str], timeout: Duration) -> Result<String, String> {
    let child = Command::new("docker").args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).output();
    match tokio::time::timeout(timeout, child).await {
        Err(_) => Err(format!("docker {} took longer than {} s", args.first().unwrap_or(&""), timeout.as_secs())),
        Ok(Err(e)) => Err(format!("running docker: {e}")),
        Ok(Ok(out)) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        Ok(Ok(out)) => {
            let err: String = String::from_utf8_lossy(&out.stderr).trim().chars().take(500).collect();
            Err(format!("docker {}: {err}", args.first().unwrap_or(&"")))
        }
    }
}

fn missing(err: &str) -> bool {
    let err = err.to_ascii_lowercase();
    err.contains("no such container") || err.contains("no such object") || err.contains("no such volume")
}

pub fn container_name(sandbox_id: &str) -> String {
    format!("roll-{}", sandbox_id.replace('_', "-"))
}

/// A network a sandbox may join: never the host's, or another container's.
pub fn network_allowed(name: &str) -> bool {
    let n = name.trim();
    !n.is_empty() && n != "host" && !n.starts_with("container:")
}

pub fn volume_name(container: &str) -> String {
    format!("{container}-home")
}

/// Only names this backend makes go into a docker command.
fn check_name(name: &str) -> Result<&str, BackendError> {
    if name.starts_with("roll-sbx-") && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        Ok(name)
    } else {
        Err(BackendError::Permanent(format!("{name:?} isn't a sandbox container")))
    }
}

/// An env file (`KEY=value` lines) readable only by this user, removed when dropped.
struct EnvFile(std::path::PathBuf);

impl EnvFile {
    fn write(env: &std::collections::BTreeMap<String, String>) -> Result<EnvFile, BackendError> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut b = [0u8; 12];
        getrandom::fill(&mut b).expect("system randomness");
        let path = std::env::temp_dir().join(format!("roll-sandbox-{}.env", hex::encode(b)));
        let mut text = String::new();
        for (k, v) in env {
            // Docker's env files are one variable a line, taken literally.
            if v.contains(['\n', '\r', '\0']) {
                return Err(BackendError::Permanent(format!("{k} spans more than one line")));
            }
            text.push_str(&format!("{k}={v}\n"));
        }
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).map_err(|e| BackendError::Transient(format!("writing the env file: {e}")))?;
        let file = EnvFile(path);
        f.write_all(text.as_bytes()).map_err(|e| BackendError::Transient(format!("writing the env file: {e}")))?;
        Ok(file)
    }
}

impl Drop for EnvFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Docker {
    pub fn new(config: DockerConfig) -> Docker {
        Docker { config, runtime: tokio::sync::OnceCell::new() }
    }

    fn network(&self) -> &str {
        self.config.network.as_deref().unwrap_or(&self.config.dedicated_network)
    }

    /// The dedicated network, made if it isn't there (an operator's network must exist).
    async fn ensure_network(&self) -> Result<(), BackendError> {
        if self.config.network.is_some() {
            return Ok(());
        }
        let n = self.config.dedicated_network.as_str();
        if docker(&["network", "inspect", "-f", "{{.Name}}", n], TIMEOUT).await.is_ok() {
            return Ok(());
        }
        let label = format!("{LABEL}.network=1");
        match docker(&["network", "create", "--driver", "bridge", "--opt", "com.docker.network.bridge.enable_icc=false", "--label", &label, n], TIMEOUT).await {
            Ok(_) => Ok(()),
            Err(e) if e.contains("already exists") => Ok(()),
            Err(e) => Err(BackendError::Transient(e)),
        }
    }

    async fn runtime(&self) -> Option<String> {
        match &self.config.runtime {
            Runtime::Default => None,
            Runtime::Named(r) => Some(r.clone()),
            Runtime::Auto => self
                .runtime
                .get_or_init(|| async {
                    let runtimes = docker(&["info", "--format", "{{json .Runtimes}}"], TIMEOUT).await.unwrap_or_default();
                    runtimes.contains("\"runsc\"").then(|| "runsc".to_string())
                })
                .await
                .clone(),
        }
    }

    /// The `docker run` arguments for a sandbox (the env comes from `env_file`).
    pub async fn run_args(&self, spec: &SandboxSpec, env_file: &str) -> Vec<String> {
        let name = container_name(&spec.id);
        let mem = format!("{}g", spec.size.memory_gb.max(1));
        let mut a: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), name.clone(), "--label".into(), format!("{LABEL}={}", spec.id), "--hostname".into(), "sandbox".into()];
        a.extend(["--init", "--restart", "unless-stopped", "--security-opt", "no-new-privileges", "--cap-drop", "ALL"].map(String::from));
        for cap in &self.config.cap_add {
            a.extend(["--cap-add".into(), cap.clone()]);
        }
        a.extend(["--pids-limit".into(), self.config.pids_limit.to_string(), "--memory".into(), mem.clone(), "--memory-swap".into(), mem, "--cpus".into(), spec.size.cpu.max(1).to_string()]);
        if let Some(r) = self.runtime().await {
            a.extend(["--runtime".into(), r]);
        }
        a.extend(["--network".into(), self.network().to_string()]);
        if self.config.disk_limit {
            a.extend(["--storage-opt".into(), format!("size={}G", spec.size.disk_gb.max(1))]);
        }
        if self.config.host_gateway {
            a.extend(["--add-host".into(), "host.docker.internal:host-gateway".into()]);
        }
        a.extend(["-v".into(), format!("{}:{HOME}", volume_name(&name)), "--env-file".into(), env_file.into(), self.config.image.clone()]);
        a.extend(self.config.command.iter().cloned());
        a
    }
}

#[async_trait]
impl SandboxBackend for Docker {
    fn name(&self) -> &'static str {
        "docker"
    }

    async fn create(&self, spec: &SandboxSpec) -> Result<String, BackendError> {
        if let Some(n) = &self.config.network
            && !network_allowed(n)
        {
            return Err(BackendError::Permanent(format!("sandboxes never join the {n} network")));
        }
        if let Some(name) = self.find(&spec.id).await? {
            return Ok(name);
        }
        if !network_allowed(&self.config.dedicated_network) {
            return Err(BackendError::Permanent(format!("sandboxes never join the {} network", self.config.dedicated_network)));
        }
        self.ensure_network().await?;
        let env = EnvFile::write(&spec.env)?;
        let args = self.run_args(spec, &env.0.to_string_lossy()).await;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match docker(&args, RUN_TIMEOUT).await {
            Ok(_) => Ok(container_name(&spec.id)),
            // A run that lost a race with another: the container is there.
            Err(e) if e.contains("is already in use") => Ok(container_name(&spec.id)),
            Err(e) if e.contains("Unable to find image") || e.contains("pull access denied") || e.contains("unknown runtime") || e.contains("invalid") => Err(BackendError::Permanent(e)),
            Err(e) => Err(BackendError::Transient(e)),
        }
    }

    async fn find(&self, sandbox_id: &str) -> Result<Option<String>, BackendError> {
        let name = container_name(sandbox_id);
        match docker(&["inspect", "-f", "{{.Name}}", check_name(&name)?], TIMEOUT).await {
            Ok(_) => Ok(Some(name)),
            Err(e) if missing(&e) => Ok(None),
            Err(e) => Err(BackendError::Transient(e)),
        }
    }

    async fn tagged(&self) -> Result<Vec<Tagged>, BackendError> {
        let label = format!("label={LABEL}");
        let format = format!("{{{{.Names}}}} {{{{.Label \"{LABEL}\"}}}}");
        let out = docker(&["ps", "-a", "--filter", &label, "--format", &format], TIMEOUT).await.map_err(BackendError::Transient)?;
        Ok(out
            .lines()
            .filter_map(|l| l.trim().split_once(' '))
            .filter(|(name, _)| check_name(name).is_ok())
            .map(|(name, id)| Tagged { backend_id: name.to_string(), sandbox_id: id.trim().to_string() })
            .collect())
    }

    async fn status(&self, backend_id: &str) -> Result<BackendState, BackendError> {
        let out = match docker(&["inspect", "-f", "{{.State.Status}} {{.State.ExitCode}}", check_name(backend_id)?], TIMEOUT).await {
            Ok(o) => o,
            Err(e) if missing(&e) => return Ok(BackendState::Gone),
            Err(e) => return Err(BackendError::Transient(e)),
        };
        let (status, code) = out.split_once(' ').unwrap_or((&out, "0"));
        Ok(match status {
            "running" => BackendState::Running,
            "created" => BackendState::Creating,
            "restarting" | "removing" => BackendState::Busy,
            "exited" | "paused" => BackendState::Stopped,
            "dead" => BackendState::Failed(format!("the container died (exit code {code})")),
            other => BackendState::Failed(format!("docker reports {other:?}")),
        })
    }

    async fn start(&self, backend_id: &str) -> Result<(), BackendError> {
        docker(&["start", check_name(backend_id)?], TIMEOUT).await.map(drop).map_err(|e| if missing(&e) { BackendError::Permanent(e) } else { BackendError::Transient(e) })
    }

    async fn stop(&self, backend_id: &str) -> Result<(), BackendError> {
        docker(&["stop", "-t", "10", check_name(backend_id)?], TIMEOUT).await.map(drop).map_err(|e| if missing(&e) { BackendError::Permanent(e) } else { BackendError::Transient(e) })
    }

    async fn delete(&self, backend_id: &str) -> Result<(), BackendError> {
        let name = check_name(backend_id)?;
        match docker(&["rm", "-f", "-v", name], TIMEOUT).await {
            Ok(_) => {}
            Err(e) if missing(&e) => {}
            Err(e) => return Err(BackendError::Transient(e)),
        }
        match docker(&["volume", "rm", "-f", &volume_name(name)], TIMEOUT).await {
            Ok(_) => Ok(()),
            Err(e) => Err(BackendError::Transient(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_args_are_hardened_and_carry_no_secrets() {
        let d = Docker::new(DockerConfig { runtime: Runtime::Named("runsc".into()), ..Default::default() });
        let spec = SandboxSpec {
            id: "sbx_abc".into(),
            name: "box".into(),
            size: super::super::default_sizes()[0].clone(),
            env: [("ROLL_AGENT_TOKEN".to_string(), "rlm_secret".to_string())].into(),
        };
        let a = d.run_args(&spec, "/tmp/x.env").await.join(" ");
        for want in [
            "--name roll-sbx-abc",
            "--security-opt no-new-privileges",
            "--cap-drop ALL",
            "--pids-limit 1024",
            "--memory 4g --memory-swap 4g",
            "--cpus 2",
            "--runtime runsc",
            "--init",
            "-v roll-sbx-abc-home:/home/roll",
            "--env-file /tmp/x.env",
            "--network roll-sandboxes",
        ] {
            assert!(a.contains(want), "{want}: {a}");
        }
        for never in ["--privileged", "--network host", "docker.sock", "rlm_secret", "--cap-add", "--storage-opt", "host-gateway"] {
            assert!(!a.contains(never), "{never}: {a}");
        }
        let capped = Docker::new(DockerConfig { disk_limit: true, network: Some("theirs".into()), ..Default::default() });
        let a = capped.run_args(&spec, "/tmp/x.env").await.join(" ");
        assert!(a.contains("--storage-opt size=8G") && a.contains("--network theirs") && !a.contains("roll-sandboxes"), "{a}");
        assert!(check_name("roll-sbx-abc").is_ok());
        for bad in ["other", "roll-sbx-a b", "roll-sbx-a;rm"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        assert!(network_allowed("roll-sandboxes"));
        for bad in ["host", "container:x", ""] {
            assert!(!network_allowed(bad), "{bad}");
        }
        let host = Docker::new(DockerConfig { network: Some("host".into()), ..Default::default() });
        assert!(matches!(host.create(&spec).await, Err(BackendError::Permanent(_))));
    }
}
