//! [`Backend`] on the roll nosql engine: Alternator tables are CQL tables, stored and maintained
//! exactly as ScyllaDB does it, so CQL clients see the same keyspaces, rows, views and CDC logs.
//!
//! - **Catalog** ([`catalog`]): table `T` is keyspace `alternator_T` with table `T`, its GSIs
//!   and LSIs are materialized views (`T:gsi`, `T!:lsi`) keyed by computed columns extracted
//!   from `:attrs`, its stream is the CDC log, its tags are the `scylla_tags` extension.
//!   Schema changes go through the engine's DDL commit (`system_schema` rows, storage, events);
//!   new views are backfilled by the engine's view builder.
//! - **Items** ([`items`]): rows of the base table, written through the engine's write path,
//!   so views, index tables and the CDC log are maintained atomically with the write.
//!   Read-modify-writes hold the partition's LWT lock (CQL `IF` statements take the same lock).
//! - **Streams** ([`streams`]): shards are the CDC streams of the table; records are read
//!   back from the log rows (pre-image, delta, post-image).
//! - **Internal tables** ([`internal`]): ScyllaDB's system tables, read through CQL.
//! - **TTL** ([`ttl`]): the expiration scanner.
//! - **Credentials** ([`auth`]): SigV4 secrets are the roles' salted hashes.

pub mod auth;
pub mod catalog;
pub mod internal;
pub mod items;
pub mod streams;
pub mod ttl;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use nosql_engine::Database;
use nosql_engine::response::{ChangeTarget, ChangeType, SchemaChange};
use nosql_engine::schema::ddl::{Change, commit};
use parking_lot::Mutex;

use crate::backend::*;
use crate::error::{Error, Result};
use crate::schema::TableSchema;
use crate::value::Item;

/// A table schema derived from the engine schema of one version (`None`: no such table).
type Derived = ([u8; 16], Option<Arc<TableSchema>>);

/// The engine-backed store of one database's Alternator tables.
pub struct EngineBackend {
    db: Arc<Database>,
    /// Table schemas derived from the engine schema, per schema version.
    cache: Mutex<HashMap<String, Derived>>,
    /// The last schema Alternator wrote per table (what ScyllaDB doesn't persist).
    written: Mutex<HashMap<String, TableSchema>>,
    /// `alternator_streams_increased_compatibility`.
    exact_streams: std::sync::atomic::AtomicBool,
}

pub(crate) fn engine_error(e: nosql_engine::Error) -> Error {
    Error::internal(e.message)
}

impl EngineBackend {
    pub fn new(db: Arc<Database>) -> EngineBackend {
        EngineBackend {
            db,
            cache: Mutex::new(HashMap::new()),
            written: Mutex::new(HashMap::new()),
            exact_streams: Default::default(),
        }
    }

    pub fn database(&self) -> &Arc<Database> {
        &self.db
    }

    /// The engine's base table of `name`.
    pub(crate) fn base(&self, name: &str) -> Option<Arc<nosql_engine::schema::Table>> {
        let schema = self.db.schema();
        schema
            .table(&catalog::keyspace_name(name), name)
            .filter(|t| !t.is_view())
            .cloned()
    }

    fn load(&self, name: &str) -> Option<Arc<TableSchema>> {
        let schema = self.db.schema();
        if let Some((v, s)) = self.cache.lock().get(name)
            && *v == schema.version
        {
            return s.clone();
        }
        let loaded = schema
            .keyspace(&catalog::keyspace_name(name))
            .filter(|k| !k.is_builtin())
            .and_then(|ks| catalog::load(ks, name))
            .map(|s| match self.written.lock().get(name) {
                Some(w) => catalog::merge(s, w),
                None => s,
            })
            .map(Arc::new);
        self.cache
            .lock()
            .insert(name.to_string(), (schema.version, loaded.clone()));
        loaded
    }

    async fn commit_plan(&self, s: &TableSchema, create: bool) -> Result<bool> {
        let db = &self.db;
        let _ddl = db.lock_ddl().await;
        let schema = db.schema();
        let ks_name = catalog::keyspace_name(&s.name);
        let existing = schema.keyspace(&ks_name);
        if create && existing.is_some() {
            return Ok(false);
        }
        if !create
            && existing
                .and_then(|k| k.tables.get(&s.name))
                .is_none_or(|t| t.id != *s.id.as_bytes())
        {
            return Ok(false);
        }
        if existing.is_some_and(|k| k.is_builtin()) {
            return Err(Error::internal(format!("{ks_name} is a system keyspace")));
        }
        let plan = catalog::plan(s, existing.map(|k| &**k), &db.config().datacenter);
        let events = if create {
            vec![
                SchemaChange::keyspace(ChangeType::Created, &ks_name),
                SchemaChange::object(ChangeType::Created, ChangeTarget::Table, &ks_name, &s.name),
            ]
        } else {
            vec![SchemaChange::object(
                ChangeType::Updated,
                ChangeTarget::Table,
                &ks_name,
                &s.name,
            )]
        };
        let change = Change {
            keyspace: Some(plan.keyspace),
            attach: plan.attach,
            drop_tables: plan.drop.clone(),
            events,
            ..Default::default()
        };
        let dropped: Vec<SchemaChange> = existing
            .map(|k| {
                k.tables
                    .values()
                    .filter(|t| plan.drop.contains(&t.table_id()))
                    .map(|t| {
                        SchemaChange::object(
                            ChangeType::Dropped,
                            ChangeTarget::Table,
                            &ks_name,
                            &t.name,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        commit(db, &schema, change).await.map_err(engine_error)?;
        auth::revoke_dropped(db, &schema, &dropped).await?;
        self.written.lock().insert(s.name.clone(), s.clone());
        // Backfill new views (writes in flight finish first, later ones maintain the view).
        if !plan.build.is_empty() {
            for o in db.hooks().observers {
                o.barrier().await;
            }
            let schema = db.schema();
            for v in &plan.build {
                let spec = nosql_engine::views::ViewSpec::view(db, &schema, &plan.base, v)
                    .map_err(engine_error)?;
                nosql_engine::views::build(db, &spec, &plan.base)
                    .await
                    .map_err(engine_error)?;
            }
        }
        Ok(true)
    }
}

#[async_trait]
impl Backend for EngineBackend {
    async fn create_table(&self, schema: TableSchema) -> Result<bool> {
        self.commit_plan(&schema, true).await
    }

    async fn delete_table(&self, name: &str) -> Result<Option<TableSchema>> {
        let db = &self.db;
        let Some(last) = self.load(name) else {
            return Ok(None);
        };
        let _ddl = db.lock_ddl().await;
        let schema = db.schema();
        let ks_name = catalog::keyspace_name(name);
        let Some(ks) = schema.keyspace(&ks_name).filter(|k| !k.is_builtin()) else {
            return Ok(None);
        };
        let change = Change {
            drop_keyspace: Some(ks_name.clone()),
            drop_tables: ks.tables.values().map(|t| t.table_id()).collect(),
            events: vec![SchemaChange::keyspace(ChangeType::Dropped, &ks_name)],
            ..Default::default()
        };
        commit(db, &schema, change).await.map_err(engine_error)?;
        auth::revoke_dropped(
            db,
            &schema,
            &[SchemaChange::keyspace(ChangeType::Dropped, &ks_name)],
        )
        .await?;
        self.written.lock().remove(name);
        Ok(Some((*last).clone()))
    }

    async fn table(&self, name: &str) -> Result<Option<Arc<TableSchema>>> {
        Ok(self.load(name))
    }

    async fn list_tables(&self) -> Result<Vec<String>> {
        let schema = self.db.schema();
        let mut out = Vec::new();
        for ks in schema.user_keyspaces() {
            let Some(name) = ks.name.strip_prefix("alternator_") else {
                continue;
            };
            if ks.tables.get(name).is_some_and(|t| !t.is_view()) {
                out.push(name.to_string());
            }
        }
        Ok(out)
    }

    async fn alter_table(&self, schema: TableSchema) -> Result<bool> {
        self.commit_plan(&schema, false).await
    }

    async fn get_item(&self, table: &TableSchema, key: &PrimaryKey) -> Result<Option<Item>> {
        let Some(t) = self.base(&table.name) else {
            return Ok(None);
        };
        items::get(&self.db, &t, key).await
    }

    async fn write_item(
        &self,
        table: &TableSchema,
        key: &PrimaryKey,
        f: WriteFn,
    ) -> Result<WriteOutcome> {
        let Some(t) = self.base(&table.name) else {
            return Err(Error::not_found(format!(
                "Requested resource not found: Table: {} not found",
                table.name
            )));
        };
        let exact = self
            .exact_streams
            .load(std::sync::atomic::Ordering::Relaxed);
        items::write(&self.db, table, &t, key, f, exact).await
    }

    fn set_streams_increased_compatibility(&self, on: bool) {
        self.exact_streams
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    async fn read_range(&self, table: &TableSchema, req: &RangeRequest) -> Result<RangePage> {
        let schema = self.db.schema();
        let ks = catalog::keyspace_name(&table.name);
        let source = match &req.index {
            None => schema.table(&ks, &table.name),
            Some(i) => table
                .index(i)
                .and_then(|idx| schema.table(&ks, &idx.view_name(&table.name))),
        };
        let Some(source) = source.cloned() else {
            return Ok(RangePage::default());
        };
        items::read_range(&self.db, &source, req).await
    }

    async fn stream_shards(&self, table: &TableSchema) -> Result<Vec<StreamShard>> {
        Ok(streams::shards(&self.db, &table.name))
    }

    async fn stream_records(
        &self,
        table: &TableSchema,
        shard: &str,
        from: u128,
        inclusive: bool,
        until_ms: i64,
        limit: usize,
    ) -> Result<Vec<StreamRecord>> {
        streams::records(&self.db, table, shard, from, inclusive, until_ms, limit).await
    }

    async fn authorize(&self, role: &str, perm: Permission, on: &Resource) -> Result<()> {
        auth::authorize(&self.db, role, perm, on)
    }

    async fn table_size_bytes(&self, table: &TableSchema) -> Result<u64> {
        let Some(t) = self.base(&table.name) else {
            return Ok(0);
        };
        Ok(self
            .db
            .storage_table(&t)
            .map(|st| st.stats().space_used)
            .unwrap_or(0))
    }

    async fn is_superuser(&self, role: &str) -> Result<bool> {
        Ok(auth::is_superuser(&self.db, role))
    }

    async fn grant_creator(&self, role: &str, on: &Resource) -> Result<()> {
        auth::grant_creator(&self.db, role, on).await
    }

    async fn internal_table(&self, keyspace: &str, table: &str) -> Result<Option<InternalTable>> {
        internal::rows(&self.db, keyspace, table).await
    }

    fn audit_may_log(&self, category: AuditCategory) -> bool {
        self.db.audit_may_log(category)
    }

    fn audit_wants(
        &self,
        category: AuditCategory,
        keyspace: &str,
        table: &str,
        user: Option<&str>,
    ) -> bool {
        self.db.audit_wants(category, keyspace, table, user)
    }

    async fn audit(&self, e: AuditEvent) {
        let event = nosql_engine::audit::Event {
            category: e.category,
            consistency: e.consistency,
            keyspace: &e.keyspace,
            table: &e.table,
            operation: &e.operation,
            error: e.error,
            user: e.user.as_deref(),
            source: e.source,
        };
        nosql_engine::audit::write(&self.db, &event).await;
    }

    async fn scheduling_group(&self, role: Option<&str>) -> String {
        match role {
            Some(r) => nosql_engine::auth::service_levels::scheduling_group(&self.db, r).await,
            None => "sl:default".into(),
        }
    }

    fn trace(&self, start: TraceStart) -> Option<Box<dyn RequestTrace>> {
        let command = start
            .parameters
            .get("query")
            .cloned()
            .unwrap_or_default();
        let t = nosql_engine::trace::begin_request(
            &self.db,
            format!("Alternator {}", start.op),
            start.parameters,
            command,
            start.tables,
            start.client,
            start.user.as_deref(),
            start.request_size,
        )?;
        Some(Box::new(EngineTrace {
            db: self.db.clone(),
            trace: t,
        }))
    }
}

/// A traced Alternator request ([`nosql_engine::trace`]).
struct EngineTrace {
    db: Arc<Database>,
    trace: nosql_engine::trace::Trace,
}

#[async_trait]
impl RequestTrace for EngineTrace {
    fn event(&mut self, activity: &str) {
        self.trace
            .event(nosql_engine::trace::Stage::Request, activity);
    }

    async fn end(self: Box<Self>, response_size: usize) {
        let EngineTrace { db, trace } = *self;
        nosql_engine::trace::end(&db, Some(trace), response_size).await;
    }
}
