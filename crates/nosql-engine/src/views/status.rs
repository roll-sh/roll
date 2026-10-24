//! View and index build status tables. Views and indexes are backfilled before `CREATE`
//! answers, so every one is built:
//!
//! | table | ScyllaDB | Cassandra |
//! |---|---|---|
//! | `system.built_views` | every view, index views included | materialized views (`status_replicated`) |
//! | `system.view_build_status_v2`, `system_distributed.view_build_status` | `SUCCESS` per view and host | materialized views (`system_distributed` only) |
//! | `system."IndexInfo"` | indexes with an index view | every index |

use std::sync::Arc;

use nosql_types::Dialect;

use crate::error::Result;
use crate::schema::Table;
use crate::system::{Registry, VRow, VirtualContext, VirtualTable, val};

/// Registers the build status tables.
pub fn register(r: &mut Registry, d: Dialect) {
    r.add("system", "built_views", Arc::new(BuiltViews));
    r.add(
        "system_distributed",
        "view_build_status",
        Arc::new(BuildStatus),
    );
    if d.is_scylla() {
        r.add("system", "view_build_status_v2", Arc::new(BuildStatus));
    }
    r.add("system", "IndexInfo", Arc::new(IndexInfo));
    if d.is_scylla() {
        crate::cdc::tables::register(r);
    }
}

/// `(keyspace, view, is an index view)` of every view.
fn views(ctx: &VirtualContext) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    for ks in ctx.schema.user_keyspaces() {
        let mvs = super::ddl::materialized_views(ks);
        for t in ks.tables.values().filter(|t| t.is_view()) {
            out.push((ks.name.clone(), t.name.clone(), !mvs.contains_key(&t.name)));
        }
    }
    out
}

struct BuiltViews;

impl VirtualTable for BuiltViews {
    fn rows(&self, _t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
        let scylla = ctx.cfg.dialect.is_scylla();
        Ok(views(ctx)
            .into_iter()
            .filter(|(_, _, index)| scylla || !index)
            .map(|(ks, v, _)| {
                let mut row = vec![
                    ("keyspace_name", val::text(&ks)),
                    ("view_name", val::text(&v)),
                ];
                if !scylla {
                    row.push(("status_replicated", val::boolean(true)));
                }
                row
            })
            .collect())
    }
}

struct BuildStatus;

impl VirtualTable for BuildStatus {
    fn rows(&self, _t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
        let scylla = ctx.cfg.dialect.is_scylla();
        let host = ctx.cfg.host_id();
        Ok(views(ctx)
            .into_iter()
            .filter(|(_, _, index)| scylla || !index)
            .map(|(ks, v, _)| {
                vec![
                    ("keyspace_name", val::text(&ks)),
                    ("view_name", val::text(&v)),
                    ("host_id", val::uuid(&host)),
                    ("status", val::text("SUCCESS")),
                ]
            })
            .collect())
    }
}

struct IndexInfo;

impl VirtualTable for IndexInfo {
    fn rows(&self, _t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
        let scylla = ctx.cfg.dialect.is_scylla();
        let mut out = Vec::new();
        for ks in ctx.schema.user_keyspaces() {
            for t in ks.tables.values() {
                for i in &t.indexes {
                    if scylla && !ks.tables.contains_key(&format!("{}_index", i.name)) {
                        continue;
                    }
                    out.push(vec![
                        ("table_name", val::text(&ks.name)),
                        ("index_name", val::text(&i.name)),
                    ]);
                }
            }
        }
        Ok(out)
    }
}
