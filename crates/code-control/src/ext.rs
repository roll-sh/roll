//! Extension points that run code: previews (branched Postgres and Redis plus a container) and
//! the checks runner. rolld plugs implementations in with [`crate::Code::set_preview_backend`]
//! and [`crate::Code::set_runner`]; without them previews and runner checks are unavailable.

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
}
