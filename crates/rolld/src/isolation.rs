//! Where roll code runs customer code (docs/cloud/README.md, "Isolated work"): `roll.toml`
//! checks, previews and agent sandboxes.
//!
//! `ROLL_CODE_RUNNER=docker` (the default) runs checks and previews as containers on this host,
//! as roll always has. `ROLL_CODE_RUNNER=fargate` (the fleet) runs each as its own Fargate task
//! with the `ROLL_CODE_FARGATE_*` settings, and then sandboxes may not use Docker either: they're
//! Fargate tasks too (`ROLL_SANDBOX_BACKEND=fargate`) or agent37's.

use code_control::agents::sandbox::{BackendConfig, ExternalBackend, SandboxConfig};
use code_preview::sandbox::FargateSandboxes;
use code_preview::{Fargate, FargateConfig, Isolation};
use std::path::Path;
use std::sync::Arc;
use tracing::info;

/// The node's isolation, and the sandbox backend that goes with it (filled into `sandboxes`).
pub async fn setup(home: &Path, sandboxes: &mut SandboxConfig) -> anyhow::Result<Arc<dyn Isolation>> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let Some(cfg) = FargateConfig::from_lookup(env, home.join("code").join("runs"))? else {
        // Containers carry this daemon's home, so a restart cleans up only its own leftovers.
        code_preview::docker::set_home(home);
        return Ok(Arc::new(code_preview::docker::Docker));
    };
    if matches!(sandboxes.backend, Some(BackendConfig::Docker(_))) {
        anyhow::bail!("with ROLL_CODE_RUNNER=fargate no customer code runs through a Docker socket: set ROLL_SANDBOX_BACKEND to fargate, agent37 or none");
    }
    let (cluster, subnets) = (cfg.cluster.clone(), cfg.subnets.len());
    let fargate = Fargate::new(cfg).await?;
    if matches!(sandboxes.backend, Some(BackendConfig::Fargate)) {
        let key = env("ROLL_SECRET_KEY").ok_or_else(|| anyhow::anyhow!("ROLL_SANDBOX_BACKEND=fargate needs ROLL_SECRET_KEY (it seals what a sandbox starts with)"))?;
        sandboxes.backend = Some(BackendConfig::External(ExternalBackend(Arc::new(FargateSandboxes::new(fargate.clone(), &key)?))));
    }
    info!(%cluster, subnets, "roll code: checks, previews and sandboxes run as Fargate tasks");
    Ok(fargate)
}
