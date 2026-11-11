//! roll code checks and previews: `roll.toml`, checks in containers (on every change head and
//! post-merge on the default branch), and previews that branch the repo's roll Postgres and
//! Redis and serve the change at `http://<change>.<preview domain>`. See
//! docs/code/architecture.md ("Changes, work items, checks, previews").
//!
//! Customer code runs through an [`Isolation`]: Docker on this host, or its own Fargate task
//! ([`fargate`]) on the fleet, where the agents' sandboxes run as Fargate tasks too
//! ([`sandbox`]). docs/cloud/README.md, "Isolated work".

pub mod config;
pub mod data;
pub mod docker;
pub mod fargate;
pub mod isolation;
pub mod preview;
pub mod runner;
pub mod sandbox;
pub mod service;

pub use config::RollToml;
pub use data::{DataBranch, DataBranches, RollData};
pub use fargate::{Fargate, FargateConfig};
pub use isolation::{Isolation, Job, Kind};
pub use preview::{PreviewConfig, Previews, route};
pub use runner::CheckRunner;
pub use service::Service;

use code_control::Code;
use std::sync::Arc;

/// Plug checks and previews into roll code and start following changes and landings.
/// Wrap the API router with [`route`] (`axum::middleware::from_fn_with_state(previews, route)`)
/// so preview hosts reach their containers.
pub fn start(code: &Arc<Code>, runner: Arc<CheckRunner>, previews: Arc<Previews>) -> tokio::task::JoinHandle<()> {
    previews.attach(code);
    code.set_preview_backend(previews);
    code.set_runner(runner.clone());
    // The merge queue tests batches with the runner: only now can it start.
    if let Err(e) = code.start_queues() {
        tracing::warn!(error = %e, "starting merge queues");
    }
    Service::new(code.clone(), runner).start()
}
