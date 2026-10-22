//! A database: one tenant's schema, storage tables, prepared statements, events and hooks.
//!
//! Opening attaches the `system_schema` storage tables first, reads the user keyspaces from them,
//! then attaches every user table (storage keeps replayed commit log frames of unattached tables
//! in memory until then). The schema is an immutable snapshot swapped under `ddl` — a single
//! writer, so schema changes are serialised and every change gets a strictly newer timestamp.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use nosql_storage::manifest::TableId;
use nosql_storage::read::{Partition, RangeRead};
use nosql_types::Dialect;
use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;

use crate::config::DbConfig;
use crate::error::{Error, Result};
use crate::functions::{Registry as Functions, UserFunctions};
use crate::hooks::Hooks;
use crate::response::{ClientInfo, Event, SchemaChange};
use crate::schema::persist::{self, SchemaTables};
use crate::schema::{Keyspace, Schema, Table};
use crate::session::Session;
use crate::statements::Prepared;
use crate::system;

/// The storage database of a tenant (opened by the node's `nosql_storage::store::Store`).
pub type StoreHandle = Arc<nosql_storage::db::Database>;

/// Memtable size of user tables before a flush.
const MEMTABLE_FLUSH_BYTES: u64 = 32 << 20;
/// Prepared statements kept per database.
const PREPARED_CAPACITY: usize = 20_000;

/// A monotonic microsecond clock: the default write timestamp.
#[derive(Debug, Default)]
pub struct Clock {
    last: AtomicI64,
}

impl Clock {
    pub fn now_micros(&self) -> i64 {
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);
        let mut prev = self.last.load(Ordering::Relaxed);
        loop {
            let next = wall.max(prev + 1);
            match self
                .last
                .compare_exchange_weak(prev, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return next,
                Err(p) => prev = p,
            }
        }
    }

    /// The last timestamp handed out.
    pub fn last(&self) -> i64 {
        self.last.load(Ordering::Relaxed)
    }

    /// At least `ts + 1` from now on (schema writes stay ordered after explicit timestamps).
    pub fn advance_past(&self, ts: i64) {
        self.last.fetch_max(ts, Ordering::Relaxed);
    }
}

pub(crate) struct PreparedCache {
    map: HashMap<[u8; 16], Arc<Prepared>>,
    order: VecDeque<[u8; 16]>,
}

/// A tenant database.
pub struct Database {
    pub(crate) cfg: DbConfig,
    pub(crate) store: StoreHandle,
    schema: RwLock<Arc<Schema>>,
    pub(crate) schema_tables: SchemaTables,
    builtin_parts: BTreeMap<String, Vec<Partition>>,
    pub(crate) ddl: tokio::sync::Mutex<()>,
    pub(crate) prepared: Mutex<PreparedCache>,
    events: broadcast::Sender<Event>,
    pub(crate) virtuals: system::Registry,
    pub(crate) functions: Functions,
    pub(crate) user_functions: RwLock<Option<Arc<dyn UserFunctions>>>,
    pub(crate) hooks: RwLock<Hooks>,
    pub(crate) clock: Clock,
    locks: Vec<tokio::sync::Mutex<()>>,
    pub(crate) started_at: i64,
    /// Roles, permissions and service levels (`crate::auth`).
    pub(crate) auth: crate::auth::State,
    /// Trace probability, slow query log (`crate::trace`).
    pub(crate) tracing: crate::trace::State,
    /// Connected clients (`system.clients`, `system_views.clients`).
    pub(crate) clients: crate::system::clients::Clients,
    /// Live settings, the read-only flag, recent warnings (`crate::live`).
    pub(crate) live: crate::live::Live,
    /// The platform masking policy (`crate::masking`).
    pub(crate) masking: crate::masking::State,
    /// ScyllaDB's per-partition rate limit counters (`crate::rate_limit`).
    pub(crate) rate_limits: crate::rate_limit::Limiter,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("id", &self.cfg.id)
            .field("dialect", &self.cfg.dialect)
            .finish()
    }
}

impl Database {
    /// Opens a database on its storage: attaches the schema tables, loads the schema, attaches
    /// every table.
    pub async fn open(cfg: DbConfig, store: StoreHandle) -> anyhow::Result<Arc<Database>> {
        let mut hooks = Hooks::default();
        crate::hooks::install(&mut hooks, &cfg);
        Self::open_with(cfg, store, hooks).await
    }

    /// [`Database::open`] with explicit hooks.
    pub async fn open_with(
        cfg: DbConfig,
        store: StoreHandle,
        hooks: Hooks,
    ) -> anyhow::Result<Arc<Database>> {
        let d = cfg.dialect;
        let builtin: Vec<Arc<Keyspace>> = system::builtin_keyspaces(&cfg)
            .into_iter()
            .map(Arc::new)
            .collect();
        let system_schema = builtin
            .iter()
            .find(|k| k.name == "system_schema")
            .ok_or_else(|| anyhow::anyhow!("no system_schema"))?;
        let schema_tables = SchemaTables::new(d, system_schema);
        // 1. The schema tables.
        let mut stored: BTreeMap<String, Vec<Partition>> = BTreeMap::new();
        for (name, t) in &schema_tables.tables {
            let st = crate::storage::storage_table(
                &store,
                t.table_id(),
                crate::storage::table_options(t, d, 4 << 20),
            )
            .await?;
            let parts = st.scan(RangeRead::all(now_secs())).partitions().await?;
            stored.insert(name.clone(), parts);
        }
        // 2. The schema.
        let user =
            persist::load(&schema_tables, &stored).map_err(|e| anyhow::anyhow!("{}", e.message))?;
        let mut keyspaces: BTreeMap<String, Arc<Keyspace>> = BTreeMap::new();
        for k in &builtin {
            keyspaces.insert(k.name.clone(), k.clone());
        }
        // Tables whose schema id was dropped before live on a later storage generation.
        let dropped = store.manifest().dropped;
        for mut k in user {
            for t in k.tables.values_mut() {
                Arc::make_mut(t).assign_storage(&dropped);
            }
            keyspaces.insert(k.name.clone(), Arc::new(k));
        }
        let schema = Arc::new(Schema::new(keyspaces));
        // 3. Every user table.
        for ks in schema.user_keyspaces() {
            for t in ks.tables.values() {
                crate::storage::storage_table(
                    &store,
                    t.table_id(),
                    crate::storage::table_options(t, d, MEMTABLE_FLUSH_BYTES),
                )
                .await?;
            }
        }
        // 4. Stored tables of the built-in keyspaces (the auth tables).
        for ks in &builtin {
            for t in ks.tables.values() {
                if t.kind == crate::schema::TableKind::Regular {
                    crate::storage::storage_table(
                        &store,
                        t.table_id(),
                        crate::storage::table_options(t, d, 4 << 20),
                    )
                    .await?;
                }
            }
        }
        // Virtual keyspaces (Cassandra's `system_views`, …) are listed in `system_virtual_schema`
        // only.
        let builtin_refs: Vec<&Keyspace> = builtin
            .iter()
            .filter(|k| k.kind != crate::schema::KeyspaceKind::Virtual)
            .map(|k| k.as_ref())
            .collect();
        let builtin_parts = persist::builtin_partitions(&builtin_refs, &schema_tables);
        let (events, _) = broadcast::channel(1024);
        let live = crate::live::Live::new(cfg.settings.clone());
        let masking = crate::masking::State::new(cfg.masking.clone());
        let hooks = Hooks {
            authorizer: crate::hooks::ReadOnly::wrap(hooks.authorizer),
            ..hooks
        };
        let db = Arc::new(Database {
            functions: Functions::new(d),
            virtuals: system::Registry::new(d),
            cfg,
            store,
            schema: RwLock::new(schema),
            schema_tables,
            builtin_parts,
            ddl: tokio::sync::Mutex::new(()),
            prepared: Mutex::new(PreparedCache {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
            events,
            user_functions: RwLock::new(None),
            hooks: RwLock::new(hooks),
            clock: Clock::default(),
            locks: (0..256).map(|_| tokio::sync::Mutex::new(())).collect(),
            started_at: now_secs(),
            auth: crate::auth::State::default(),
            tracing: Default::default(),
            clients: Default::default(),
            live,
            masking,
            rate_limits: Default::default(),
        });
        db.set_user_functions(crate::udf::provider(&db));
        crate::auth::bootstrap(&db)
            .await
            .map_err(|e| anyhow::anyhow!("auth bootstrap: {}", e.message))?;
        db.spawn_row_expiration();
        Ok(db)
    }

    /// A session for one connection.
    pub fn connect(self: &Arc<Self>, client: ClientInfo) -> Session {
        Session::new(self.clone(), client)
    }

    /// A session carrying an existing connection's state (keyspace, role, client): for servers
    /// that run each request of a connection on its own session (`Session` methods take
    /// `&mut self`). Creating one is an `Arc` clone plus moving `state`.
    pub fn session(self: &Arc<Self>, state: crate::session::SessionState) -> Session {
        Session::with_state(self.clone(), state)
    }

    pub fn dialect(&self) -> Dialect {
        self.cfg.dialect
    }

    pub fn config(&self) -> &DbConfig {
        &self.cfg
    }

    /// Schema, topology and status events.
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Sends an event to every subscriber (the node's `STATUS_CHANGE` / `TOPOLOGY_CHANGE`).
    pub fn publish(&self, event: Event) {
        let _ = self.events.send(event);
    }

    /// Flushes every memtable to SSTables and writes the manifest.
    pub async fn flush(&self) -> anyhow::Result<()> {
        self.store.flush().await
    }

    /// The current schema snapshot.
    pub fn schema(&self) -> Arc<Schema> {
        self.schema.read().clone()
    }

    pub fn functions(&self) -> &Functions {
        &self.functions
    }

    /// Installs the user-defined function provider (UDF subsystem).
    pub fn set_user_functions(&self, f: Arc<dyn UserFunctions>) {
        *self.user_functions.write() = Some(f);
    }

    /// Replaces the hooks (tests and embedding). The authorizer is wrapped in
    /// [`crate::hooks::ReadOnly`], as at open.
    pub fn set_hooks(&self, hooks: Hooks) {
        let hooks = Hooks {
            authorizer: crate::hooks::ReadOnly::wrap(hooks.authorizer),
            ..hooks
        };
        *self.hooks.write() = hooks;
    }

    pub fn hooks(&self) -> Hooks {
        self.hooks.read().clone()
    }

    pub fn store(&self) -> &StoreHandle {
        &self.store
    }

    /// The storage table of `t` (attached at open or creation).
    pub fn storage_table(&self, t: &Table) -> Result<Arc<nosql_storage::table::Table>> {
        self.store.get_table(t.table_id()).ok_or_else(|| {
            Error::server(format!("table {}.{} is not attached", t.keyspace, t.name))
        })
    }

    /// The default write timestamp (microseconds).
    pub fn timestamp(&self) -> i64 {
        self.clock.now_micros()
    }

    /// The lock stripe of a partition (read-before-write: counters, list operations by index,
    /// LWT). Lock stripes in ascending order to avoid deadlocks.
    pub fn stripe(&self, table: &[u8; 16], token: i64) -> usize {
        let mut h = token as u64 ^ 0x9E37_79B9_7F4A_7C15;
        for b in table {
            h = h.rotate_left(5) ^ *b as u64;
        }
        (h % self.locks.len() as u64) as usize
    }

    pub async fn lock_stripe(&self, stripe: usize) -> tokio::sync::MutexGuard<'_, ()> {
        self.locks[stripe % self.locks.len()].lock().await
    }

    /// Serialises a schema change made outside CQL (Alternator's tables) with DDL statements.
    pub async fn lock_ddl(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.ddl.lock().await
    }

    /// Locks one partition.
    pub async fn lock_partition(
        &self,
        table: &[u8; 16],
        token: i64,
    ) -> tokio::sync::MutexGuard<'_, ()> {
        self.lock_stripe(self.stripe(table, token)).await
    }

    /// Generated `system_schema` rows of the built-in keyspaces, for `table`.
    pub(crate) fn builtin_schema_rows(&self, table: &str) -> &[Partition] {
        self.builtin_parts
            .get(table)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Publishes a new schema, broadcasts `change` and drops prepared statements on changed
    /// tables.
    pub(crate) fn install_schema(&self, schema: Schema, changes: &[SchemaChange]) {
        let schema = Arc::new(schema);
        *self.schema.write() = schema.clone();
        {
            let mut cache = self.prepared.lock();
            cache.map.retain(|_, p| p.is_valid(&schema));
            let live: std::collections::HashSet<[u8; 16]> = cache.map.keys().copied().collect();
            cache.order.retain(|id| live.contains(id));
        }
        for c in changes {
            let _ = self.events.send(Event::SchemaChange(c.clone()));
        }
    }

    /// Writes rows of several tables atomically.
    pub(crate) async fn apply(
        &self,
        updates: Vec<(TableId, nosql_storage::PartitionUpdate)>,
    ) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let first = updates[0].0;
        let write_type = if updates.len() == 1 { "SIMPLE" } else { "BATCH" };
        let r = if updates.len() == 1 {
            let (id, u) = updates.into_iter().next().unwrap_or_else(|| unreachable!());
            let t = self
                .store
                .get_table(id)
                .ok_or_else(|| Error::server(format!("table {id} is not attached")))?;
            t.apply(u).await
        } else {
            self.store.apply_batch(updates).await
        };
        r.map_err(|e| self.write_error(e, first, write_type))
    }

    /// A storage write error for the client. Memtables that stay full for the write timeout
    /// ([`nosql_storage::memory::Overloaded`]) time the write out as both references do (at
    /// `ONE`; statements restate it at their consistency).
    fn write_error(&self, e: anyhow::Error, table: TableId, write_type: &str) -> Error {
        if e.downcast_ref::<nosql_storage::memory::Overloaded>().is_none() {
            return Error::internal(e);
        }
        let schema = self.schema();
        let name = schema
            .keyspaces
            .values()
            .flat_map(|k| k.tables.values())
            .find(|t| t.table_id() == table)
            .map(|t| format!("{}.{}", t.keyspace, t.name))
            .unwrap_or_default();
        crate::schema::ddl::write_timed_out_in(
            self.dialect(),
            &name,
            crate::error::Consistency::One,
            write_type,
        )
    }

    pub(crate) fn cache_prepared(&self, p: Arc<Prepared>) {
        let mut cache = self.prepared.lock();
        if !cache.map.contains_key(&p.id) {
            cache.order.push_back(p.id);
        }
        cache.map.insert(p.id, p);
        while cache.order.len() > PREPARED_CAPACITY {
            if let Some(old) = cache.order.pop_front() {
                cache.map.remove(&old);
            }
        }
    }

    /// The cached prepared statements, oldest first (Cassandra's `system.prepared_statements`).
    pub(crate) fn prepared_statements(&self) -> Vec<Arc<Prepared>> {
        let cache = self.prepared.lock();
        cache
            .order
            .iter()
            .filter_map(|id| cache.map.get(id).cloned())
            .collect()
    }

    pub(crate) fn get_prepared(&self, id: &[u8]) -> Option<Arc<Prepared>> {
        let key: [u8; 16] = id.try_into().ok()?;
        self.prepared.lock().map.get(&key).cloned()
    }

    /// Drops the cached prepared statements matching `stale` (a changed user function).
    pub(crate) fn evict_prepared(&self, stale: impl Fn(&Prepared) -> bool) {
        let mut cache = self.prepared.lock();
        cache.map.retain(|_, p| !stale(p));
        let live: std::collections::HashSet<[u8; 16]> = cache.map.keys().copied().collect();
        cache.order.retain(|id| live.contains(id));
    }

    pub(crate) fn memtable_flush_bytes(&self) -> u64 {
        MEMTABLE_FLUSH_BYTES
    }

    /// Rows of a generated table.
    pub(crate) fn virtual_rows(&self, t: &Table) -> Result<Vec<system::VRow>> {
        let Some(v) = self.virtuals.get(&t.keyspace, &t.name) else {
            return Ok(Vec::new());
        };
        let schema = self.schema();
        let ctx = system::VirtualContext {
            db: self,
            cfg: &self.cfg,
            schema: &schema,
            started_at: self.started_at,
        };
        v.rows(t, &ctx)
    }

    /// The session-independent part of a request's "now".
    pub(crate) fn now_secs(&self) -> i64 {
        now_secs()
    }
}

pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
