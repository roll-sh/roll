//! User-defined functions and aggregates.
//!
//! - [`ddl`]: `CREATE [OR REPLACE] FUNCTION|AGGREGATE [IF NOT EXISTS]`, `DROP FUNCTION|AGGREGATE`
//!   (a [`StatementHandler`](crate::hooks::StatementHandler)), stored as `Keyspace.functions` /
//!   `aggregates` (the `system_schema.functions` / `aggregates` rows, ScyllaDB's
//!   `scylla_aggregates` for `REDUCEFUNC`), which DESCRIBE renders.
//! - [`function`]: the [`Provider`] the registry asks for user functions of a keyspace (resolution
//!   and its messages are the registry's, shared with native functions), and the functions and
//!   aggregates it builds from the metadata.
//! - [`lua`]: ScyllaDB's Lua UDFs on a vendored Lua 5.4 (`nosql-lua`), with ScyllaDB's
//!   libraries, type mapping, limits and messages.
//!
//! Both references ship with UDFs disabled (`user_defined_functions_enabled` /
//! `enable_user_defined_functions` + `experimental_features: udf`); every statement and call then
//! fails with the reference's message. Enabled, ScyllaDB runs Lua and WASM ([`wasm`]; others
//! are rejected)
//! with its own DDL rules and texts (`ddl`, "ScyllaDB"), and aggregates call the state function on
//! every row; Cassandra 5 accepts only Java, which roll stores but cannot compile or run: a call
//! that reaches a Java body fails with a `FUNCTION_FAILURE` (compatibility.md, "Out of scope").
//! Everything is checked against the UDF-enabled references by `tests/captured_udf_harness.rs`.
//!
//! Statements using user functions check `EXECUTE` on them (and on an aggregate's state, final
//! and reduce functions) through the authorizer (`crate::statements::execute`), and are evicted
//! from the prepared cache when a function of that name changes.

pub mod ddl;
pub mod function;
pub mod lua;
pub mod messages;
pub mod wasm;

use std::sync::Arc;

use crate::config::DbConfig;
use crate::database::Database;
use crate::hooks::Hooks;
use crate::statements::Plan;

pub use function::{Provider, UserAgg, UserFn};

/// Registers the DDL handler.
pub fn install(hooks: &mut Hooks, _cfg: &DbConfig) {
    hooks.statements.push(Arc::new(ddl::Handler));
}

/// The user-function provider of `db` (installed at open).
pub fn provider(db: &Arc<Database>) -> Arc<dyn crate::functions::UserFunctions> {
    Arc::new(Provider::new(db))
}

/// Drops the prepared statements that use a function named `ks.name` (any overload).
pub fn evict_prepared(db: &Database, ks: &str, name: &str) {
    db.evict_prepared(|p| {
        let uses = |fs: &[Arc<dyn crate::functions::Function>]| {
            fs.iter()
                .any(|f| f.name().keyspace == ks && f.name().name == name)
        };
        match &p.plan {
            Plan::Select(s) => uses(&s.functions),
            Plan::Modify(m) => uses(&m.functions),
            Plan::Batch(b) => b.statements.iter().any(|s| uses(&s.functions)),
            _ => false,
        }
    });
}

/// The user functions a prepared plan calls, for `EXECUTE` checks.
pub fn functions_of(plan: &Plan) -> Vec<Arc<dyn crate::functions::Function>> {
    match plan {
        Plan::Select(s) => s.functions.clone(),
        Plan::Modify(m) => m.functions.clone(),
        Plan::Batch(b) => b
            .statements
            .iter()
            .flat_map(|s| s.functions.iter().cloned())
            .collect(),
        _ => Vec::new(),
    }
}
