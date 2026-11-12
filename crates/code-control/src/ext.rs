//! Extension points that run code: previews (branched Postgres and Redis plus a container) and
//! the checks runner. rolld plugs implementations in with [`crate::Code::set_preview_backend`]
//! and [`crate::Code::set_runner`]; without them previews and runner checks are unavailable.
//! [`SecretEnv`] is roll secrets, for checks, sandboxes and the `secret_leak` blocker
//! ([`crate::Code::set_secret_env`]).

use crate::model::{Change, Check, Preview, RepoRow};
use async_trait::async_trait;
use std::path::Path;

/// Creates and tears down a change's preview.
#[async_trait]
pub trait PreviewBackend: Send + Sync {
    /// Start (or update) the preview of `change` from the tree materialized at `dir`.
    async fn create(&self, repo: &RepoRow, change: &Change, dir: &Path) -> anyhow::Result<Preview>;
    async fn delete(&self, repo: &RepoRow, change: &Change) -> anyhow::Result<()>;
}

/// Runs the checks a repo's `roll.toml` declares against a materialized tree.
#[async_trait]
pub trait Runner: Send + Sync {
    /// The checks to run for this tree, as queued [`Check`]s.
    async fn plan(&self, repo: &RepoRow, dir: &Path) -> anyhow::Result<Vec<Check>>;
    /// Run one check to completion.
    async fn run(&self, repo: &RepoRow, change: Option<&Change>, dir: &Path, check: &Check) -> anyhow::Result<Check>;
    /// Why the runner can't use this `roll.toml` text (its `[checks.*]` tables), if it can't.
    /// The landing settings only read `[queue]` and `[main]`, so without this a broken check
    /// table would look healthy in the queue's settings until a run failed.
    fn validate(&self, _text: &str) -> Option<String> {
        None
    }
    /// Remove what a previous process of this node left running; called before checks resume.
    async fn remove_leftovers(&self) {}
}

/// An environment's secrets from roll secrets (docs/secrets.md, "Consumption"). Values come back
/// resolved; the caller puts them in a process's environment and nowhere else (no records, no
/// logs). Every read is in the project's audit log as `consumer`.
#[async_trait]
pub trait SecretEnv: Send + Sync {
    /// `environment` (`env`, or `env:/folder`) of the workspace project that has repo
    /// `owner/name` on its canvas. Refuses a protected environment and unresolved references.
    async fn for_repo(&self, repo: &str, environment: &str, consumer: &str) -> anyhow::Result<Vec<(String, String)>>;
    /// The same for workspace project `project` (an id or a name), which must have one of
    /// `owner`'s repos on its canvas (sandboxes).
    async fn for_owner(&self, owner: &str, project: &str, environment: &str, consumer: &str) -> anyhow::Result<Vec<(String, String)>>;
    /// The (key, environment) of every secret, in any environment of the projects that have repo
    /// `owner/name` on their canvas, whose value is one of `candidates` (matched by fingerprint;
    /// values never come back). Empty for a repo on no project's canvas.
    async fn leaks(&self, repo: &str, candidates: Vec<String>) -> anyhow::Result<Vec<(String, String)>>;
}
