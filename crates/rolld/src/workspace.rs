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
