//! Workspace projects in rolld (docs/workspace.md). Always on: not a product in `ROLL_PRODUCTS`.
//!
//! On a node of its own the data is `workspace.db` under `--home`; with the system database
//! (`ROLL_SYSTEM_DATABASE`) it's that database's `workspace` schema, which every node shares
//! without leases (each read-then-write is one transaction).

use std::path::Path;
use std::sync::Arc;

pub fn open(args: &crate::Args, home: &Path) -> anyhow::Result<Arc<roll_workspace::Workspace>> {
    let target = match args.code.cluster.system_database.as_deref().filter(|u| !u.is_empty()) {
        Some(url) => code_sql::Target::Postgres { url: url.to_string(), schema: "workspace".into() },
        None => code_sql::Target::Sqlite(home.join("workspace.db")),
    };
    let pg = code_sql::PgOptions { max_connections: args.db_budget()?.workspace, ca: args.code.code_db_ca.clone(), ..Default::default() };
    // Opening blocks on the database.
    Ok(Arc::new(tokio::task::block_in_place(|| roll_workspace::Workspace::open(&target, pg))?))
}

/// roll accounts' SCIM `Groups` keep their groups here (docs/members-v1-1.md, "SCIM Groups"): each
/// change is one workspace write, as the connection.
pub struct Directory(pub Arc<roll_workspace::Workspace>);

fn dir_error(e: roll_workspace::Error) -> roll_accounts::scim_groups::DirError {
    use roll_accounts::scim_groups::DirError;
    match e {
        roll_workspace::Error::NotFound(m) => DirError::NotFound(m),
        roll_workspace::Error::Conflict(m) => DirError::Conflict(m),
        roll_workspace::Error::TooMany(m) | roll_workspace::Error::Limit(m) => DirError::TooMany(m),
        roll_workspace::Error::BadRequest(m) => DirError::Invalid(m),
        e => DirError::Internal(e.to_string()),
    }
}

fn dir_group(g: roll_workspace::Group, members: Vec<roll_workspace::GroupMember>) -> roll_accounts::scim_groups::DirGroup {
    roll_accounts::scim_groups::DirGroup { id: g.id, name: g.name, external_id: g.external_id, members: members.into_iter().map(|m| (m.user, m.handle)).collect(), created_at: g.created_at }
}

fn counts(c: roll_workspace::Counts) -> roll_accounts::scim_groups::Counts {
    roll_accounts::scim_groups::Counts { added: c.added, removed: c.removed, skipped: c.skipped }
}

impl roll_accounts::scim_groups::Directory for Directory {
    fn list(&self, conn: &str, filter: Option<(&str, &str)>, offset: i64, count: i64) -> Result<(i64, Vec<roll_accounts::scim_groups::DirGroup>), roll_accounts::scim_groups::DirError> {
        let (total, page) = self.0.scim_groups(conn, filter, offset, count).map_err(dir_error)?;
        Ok((total, page.into_iter().map(|g| dir_group(g, vec![])).collect()))
    }

    fn get(&self, conn: &str, id: &str, members: bool) -> Result<roll_accounts::scim_groups::DirGroup, roll_accounts::scim_groups::DirError> {
        let (g, m) = self.0.scim_group(conn, id, members).map_err(dir_error)?;
        Ok(dir_group(g, m))
    }

    fn create(&self, conn: &str, name: &str, external_id: Option<&str>, members: &[String]) -> Result<(roll_accounts::scim_groups::DirGroup, roll_accounts::scim_groups::Counts), roll_accounts::scim_groups::DirError> {
        let (g, c) = self.0.scim_create(conn, name, external_id, members).map_err(dir_error)?;
        Ok((dir_group(g, vec![]), counts(c)))
    }

    fn patch(&self, conn: &str, id: &str, ops: &[roll_accounts::scim_groups::GroupOp]) -> Result<roll_accounts::scim_groups::Counts, roll_accounts::scim_groups::DirError> {
        use roll_accounts::scim_groups::GroupOp as A;
        use roll_workspace::GroupOp as W;
        let ops: Vec<W> = ops
            .iter()
            .map(|o| match o {
                A::Add(l) => W::AddMembers(l.clone()),
                A::Remove(l) => W::RemoveMembers(l.clone()),
                A::Replace(l) => W::ReplaceMembers(l.clone()),
                A::Rename(n) => W::Rename(n.clone()),
                A::ExternalId(x) => W::ExternalId(x.clone()),
            })
            .collect();
        self.0.scim_patch(conn, id, &ops).map(counts).map_err(dir_error)
    }

    fn delete(&self, conn: &str, id: &str) -> Result<(), roll_accounts::scim_groups::DirError> {
        self.0.scim_delete(conn, id).map_err(dir_error)
    }

    fn forget_connection(&self, conn: &str) -> Result<(), roll_accounts::scim_groups::DirError> {
        self.0.forget_connection_groups(conn).map(|_| ()).map_err(dir_error)
    }
}
