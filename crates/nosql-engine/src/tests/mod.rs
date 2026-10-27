//! Engine tests: a harness that runs CQL text on a [`Database`] backed by temp-dir storage, and
//! suites per area. Expected values were verified on the reference images
//! (`scylladb/scylla:2026.3.3`, `cassandra:5.0.9`); rows are compared in cqlsh's text form so a
//! case can be checked by pasting the statement into `cqlsh` on the reference.

mod audit;
mod auth;
#[rustfmt::skip]
mod captured_auth;
mod captured_auth_harness;
mod captured_udf;
mod captured_udf_harness;
#[rustfmt::skip]
mod captured_cases;
mod captured_harness;
#[rustfmt::skip]
mod captured_describe;
mod captured_describe_harness;
#[rustfmt::skip]
mod captured_schema;
mod captured_schema_harness;
#[rustfmt::skip]
mod captured_statements;
mod captured_statements_harness;
#[rustfmt::skip]
mod captured_lwt;
mod captured_lwt_harness;
#[rustfmt::skip]
mod captured_masking;
mod captured_masking_harness;
mod guardrails;
mod keyspace_options;
mod table_options;
mod live;
mod lwt;
mod ddl;
mod describe;
mod derived;
mod derived_probed;
#[rustfmt::skip]
mod captured_derived;
mod captured_derived_harness;
#[rustfmt::skip]
mod captured_views;
mod captured_views_harness;
mod dml;
mod errors;
mod functions_c5;
mod masking;
mod memory;
mod mutation_fragments;
mod probed;
mod probed_harness;
mod pyrepr;
mod select;
mod rowttl;
mod service_level_use;
mod size_limits;
mod using_timeout;
mod view_options;
mod wasm;
mod system;
mod system_views;
mod trace;
mod captured_tombstones;
mod tombstone_warn;
mod udf;

use std::sync::Arc;

use bytes::Bytes;
use nosql_storage::store::{Store, StoreConfig};

use crate::{
    Database, DbConfig, Dialect, Error, ErrorCode, Param, QueryParams, Response, ResponseBody,
    Session,
};

pub const BOTH: [Dialect; 2] = [Dialect::Scylla, Dialect::Cassandra];

/// A database on fresh temp-dir storage and one session.
pub struct H {
    pub db: Arc<Database>,
    pub s: Session,
    pub store: Arc<Store>,
}

pub fn small_store_config() -> StoreConfig {
    StoreConfig {
        cache: nosql_storage::cache::CacheConfig {
            memory_bytes: 2 << 20,
            ..Default::default()
        },
        memtable_budget_bytes: 64 << 20,
        auto_compaction: false,
        maintenance_interval: std::time::Duration::from_secs(3600),
        manifest_interval: std::time::Duration::from_secs(3600),
        ..Default::default()
    }
}

impl H {
    pub async fn new(d: Dialect) -> H {
        let store = Store::start(nosql_storage::test_objects("engine"), small_store_config())
            .await
            .unwrap();
        Self::on(store, "db", d).await
    }

    pub async fn on(store: Arc<Store>, id: &str, d: Dialect) -> H {
        let handle = store
            .open(id, nosql_storage::db::DbConfig::default())
            .await
            .unwrap();
        let db = Database::open(DbConfig::new(id, d), handle).await.unwrap();
        let s = db.connect(Default::default());
        H { db, s, store }
    }

    /// A database with configuration `cfg` on `store`.
    pub async fn on_config(store: Arc<Store>, cfg: DbConfig) -> H {
        let handle = store
            .open(&cfg.id, nosql_storage::db::DbConfig::default())
            .await
            .unwrap();
        let db = Database::open(cfg, handle).await.unwrap();
        let s = db.connect(Default::default());
        H { db, s, store }
    }

    pub fn d(&self) -> Dialect {
        self.db.dialect()
    }

    pub async fn run(&mut self, cql: &str) -> Result<Response, Error> {
        self.s.query(cql, QueryParams::default()).await
    }

    pub async fn run_with(&mut self, cql: &str, values: Vec<Param>) -> Result<Response, Error> {
        self.s.query(cql, QueryParams::with_values(values)).await
    }

    /// Runs statements that must succeed.
    pub async fn ok(&mut self, cql: &str) -> Response {
        match self.run(cql).await {
            Ok(r) => r,
            Err(e) => panic!("{}: {cql}: {e}", self.d()),
        }
    }

    pub async fn exec(&mut self, script: &str) {
        for stmt in script.split(";\n").map(str::trim).filter(|s| !s.is_empty()) {
            self.ok(stmt).await;
        }
    }

    pub async fn err(&mut self, cql: &str) -> Error {
        match self.run(cql).await {
            Ok(r) => panic!(
                "{}: expected an error for {cql}, got {:?}",
                self.d(),
                r.body
            ),
            Err(e) => e,
        }
    }

    /// The error message (asserting the code is Invalid unless given otherwise).
    pub async fn msg(&mut self, cql: &str) -> String {
        self.err(cql).await.message
    }

    /// Rows formatted as cqlsh shows them.
    pub async fn rows(&mut self, cql: &str) -> Vec<Vec<String>> {
        let r = self.ok(cql).await;
        fmt_rows(&r)
    }

    pub async fn rows_with(
        &mut self,
        cql: &str,
        p: QueryParams,
    ) -> (Vec<Vec<String>>, Option<Bytes>) {
        let r = self
            .s
            .query(cql, p)
            .await
            .unwrap_or_else(|e| panic!("{cql}: {e}"));
        let ps = r.rows().and_then(|r| r.paging_state.clone());
        (fmt_rows(&r), ps)
    }

    /// Column names of a result.
    pub async fn columns(&mut self, cql: &str) -> Vec<String> {
        let r = self.ok(cql).await;
        r.rows()
            .map(|r| r.metadata.columns.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Column (name, cql type) pairs of a result.
    pub async fn specs(&mut self, cql: &str) -> Vec<(String, String)> {
        let r = self.ok(cql).await;
        r.rows()
            .map(|r| {
                r.metadata
                    .columns
                    .iter()
                    .map(|c| (c.name.clone(), nosql_types::cql_name(&c.ty)))
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub fn fmt_rows(r: &Response) -> Vec<Vec<String>> {
    let Some(rows) = r.rows() else {
        return Vec::new();
    };
    rows.rows
        .iter()
        .map(|row| {
            row.iter()
                .zip(&rows.metadata.columns)
                .map(|(v, c)| match v {
                    None => "null".to_string(),
                    Some(b) => nosql_types::to_cqlsh(&c.ty, b),
                })
                .collect()
        })
        .collect()
}

/// `vec![vec!["a", "b"], …]` as owned strings.
pub fn table(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect()
}

pub fn int(v: i32) -> Param {
    Param::new(Bytes::copy_from_slice(&v.to_be_bytes()))
}

pub fn text(s: &str) -> Param {
    Param::new(Bytes::copy_from_slice(s.as_bytes()))
}

pub fn code(e: &Error) -> ErrorCode {
    e.code
}

pub fn body_kind(r: &Response) -> &'static str {
    match &r.body {
        ResponseBody::Void => "Void",
        ResponseBody::Rows(_) => "Rows",
        ResponseBody::SetKeyspace(_) => "SetKeyspace",
        ResponseBody::SchemaChange(_) => "SchemaChange",
    }
}

/// The NTS keyspace statement both references accept.
pub const KS: &str =
    "CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'datacenter1': 1}";

/// The server spawns engine calls on a multi-threaded runtime: every facade future must be
/// `Send` (checked at compile time).
#[allow(dead_code)]
fn facade_futures_are_send(
    s: &mut Session,
    cfg: DbConfig,
    store: crate::StoreHandle,
    hooks: crate::hooks::Hooks,
    b: crate::Batch,
) {
    fn is_send<T: Send>(_: T) {}
    is_send(s.query("x", QueryParams::default()));
    is_send(s.prepare("x", None));
    is_send(s.execute(&[], QueryParams::default()));
    is_send(s.batch(b));
    is_send(s.authenticate(&[]));
    is_send(Database::open_with(cfg, store, hooks));
}
