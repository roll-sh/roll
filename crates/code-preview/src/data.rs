//! A preview's data: a branch of the repo's roll Postgres project and of its roll Redis
//! database, so every change runs against a private copy of real data.

use async_trait::async_trait;
use std::sync::Arc;

/// A branch and how the preview's container reaches it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataBranch {
    pub id: String,
    pub url: String,
}

#[async_trait]
pub trait DataBranches: Send + Sync {
    /// Branch `project`'s `main` as `name` (or return the branch if it already exists).
    async fn postgres_branch(&self, project: &str, name: &str) -> anyhow::Result<DataBranch>;
    async fn delete_postgres(&self, project: &str, branch_id: &str) -> anyhow::Result<()>;
    /// Branch Redis database `db` as `name` (or return it if it already exists).
    async fn redis_branch(&self, db: &str, name: &str) -> anyhow::Result<DataBranch>;
    async fn delete_redis(&self, id: &str) -> anyhow::Result<()>;
}

/// The daemon's own Postgres and Redis control planes.
pub struct RollData {
    pub postgres: Arc<pg_control::Control>,
    pub redis: Arc<redis_control::Control>,
}

#[async_trait]
impl DataBranches for RollData {
    async fn postgres_branch(&self, project_id: &str, name: &str) -> anyhow::Result<DataBranch> {
        let pg = &self.postgres;
        let project = pg.store.project(project_id)?.ok_or_else(|| anyhow::anyhow!("Postgres project {project_id} not found"))?;
        let branch = match pg.store.branch_by_name(project_id, name)? {
            Some(b) => b,
            None => pg.create_branch(project_id, name, None, None).await.map_err(|e| anyhow::anyhow!("branching Postgres: {e}"))?,
        };
        let ep = pg.store.endpoint_for_branch(&branch.id)?.ok_or_else(|| anyhow::anyhow!("Postgres branch {} has no endpoint", branch.id))?;
        Ok(DataBranch { id: branch.id, url: pg.connection_uri(&project, &ep.id) })
    }

    async fn delete_postgres(&self, project_id: &str, branch_id: &str) -> anyhow::Result<()> {
        self.postgres.delete_branch(project_id, branch_id).await.map_err(|e| anyhow::anyhow!("deleting Postgres branch: {e}"))
    }

    async fn redis_branch(&self, db: &str, name: &str) -> anyhow::Result<DataBranch> {
        let d = match self.redis.store.db_by_name(name)? {
            Some(d) => d,
            None => self.redis.branch(db, name, None).await.map_err(|e| anyhow::anyhow!("branching Redis: {e}"))?,
        };
        let conn = self.redis.connection(&d.id, &d.password);
        let url = conn["redis_url"].as_str().unwrap_or_default().to_string();
        Ok(DataBranch { id: d.id, url })
    }

    async fn delete_redis(&self, id: &str) -> anyhow::Result<()> {
        self.redis.delete(id).await.map_err(|e| anyhow::anyhow!("deleting Redis branch: {e}"))
    }
}

/// Connection URLs are written for clients on the host; a container reaches the host as
/// `host.docker.internal`.
pub fn for_container(url: &str) -> String {
    for local in ["@127.0.0.1:", "@localhost:", "@0.0.0.0:", "@[::1]:"] {
        if let Some(i) = url.find(local) {
            return format!("{}@host.docker.internal:{}", &url[..i], &url[i + local.len()..]);
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_loopback_hosts_only() {
        assert_eq!(
            for_container("postgres://roll:pw@127.0.0.1:55432/main?options=endpoint%3Dep_1"),
            "postgres://roll:pw@host.docker.internal:55432/main?options=endpoint%3Dep_1"
        );
        assert_eq!(for_container("redis://default:t@localhost:56380"), "redis://default:t@host.docker.internal:56380");
        assert_eq!(for_container("redis://default:t@db.roll.sh:6379"), "redis://default:t@db.roll.sh:6379");
    }
}
