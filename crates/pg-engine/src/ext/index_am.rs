//! Index access methods of native code: what an extension's `CREATE ACCESS METHOD name TYPE
//! INDEX HANDLER handler` names, when `handler` is a C function the engine implements
//! (`IndexAmRoutine` in PostgreSQL). pgvector's ivfflat and hnsw are the ones here; another
//! index-type extension adds its routine to `routine`.
//!
//! The engine drives an index of such a method through `IndexAm`: building it over the table's
//! rows, inserting each new row's key, removing dead entries at VACUUM, costing and running
//! ordered scans. Its storage is the method's own (`AmStore`): records under the index's key
//! prefix, written as the method changes them, outside transactions, as PostgreSQL's index
//! pages are. A row an entry names is fetched and checked for visibility when a scan returns it,
//! so entries of aborted or since-updated rows are skipped as PostgreSQL skips dead heap tuples.
//! (A row keeps its data key across updates where PostgreSQL's gets a new TID: an UPDATE giving
//! a row's key another value marks the row (`AmStore::mark_updated`), and a scan returns such a
//! row only for the entry holding its current value, once: `ScanState::keep`.)

use crate::Result;
use crate::types::Value;

/// An ordered scan of an index of a native method, as a scan node runs it.
pub struct ScanState {
    scan: Box<dyn AmScan>,
    updated: std::sync::Arc<parking_lot::RwLock<std::collections::HashSet<Vec<u8>>>>,
    seen: std::collections::HashSet<Vec<u8>>,
    /// The index's first key over the table's row.
    key: crate::sql::expr::Expr,
    /// The value an entry holds for a key value (`IndexAm::entry_value`).
    form: Box<dyn Fn(&Value) -> Value>,
    /// Fetched and not yet returned: (value to check, data key, the row's bytes).
    pub pending: std::collections::VecDeque<(Option<Value>, Vec<u8>, Option<bytes::Bytes>)>,
}

impl ScanState {
    /// A scan of index `ix` of table `t` (keys bound as `keys`) ordered by the distances to
    /// `orderbys`.
    pub fn open(r: &'static AmRoutine, t: &crate::catalog::Table, ix: &crate::catalog::Index, keys: &[crate::sql::expr::Expr], orderbys: &[Value], ctx: &crate::exec::ExecCtx) -> Result<ScanState> {
        r.load(ctx);
        let am = AmIndex::of(&ctx.catalog(), t, ix, keys, ctx);
        let updated = am.store.updated()?;
        Ok(ScanState {
            scan: r.am.scan(&am, orderbys, ctx)?,
            updated,
            seen: Default::default(),
            key: keys.first().cloned().unwrap_or(crate::sql::expr::Expr::Const(Value::Null, crate::types::DataType::Text)),
            form: Box::new(move |v| r.am.entry_value(&am, v)),
            pending: Default::default(),
        })
    }

    /// The next entry: its data key, and the value to check the row against when the row
    /// was updated.
    pub fn next(&mut self, ctx: &crate::exec::ExecCtx) -> Result<Option<(Vec<u8>, Option<Value>)>> {
        let updated = self.updated.clone();
        self.scan.next(ctx, &|k| updated.read().contains(k))
    }

    /// Whether a fetched row (its columns) goes out: an updated row's only for the entry with
    /// its current value, and once.
    pub fn keep(&mut self, dkey: &[u8], check: Option<&Value>, row: &[Value], ctx: &crate::exec::ExecCtx) -> Result<bool> {
        let Some(v) = check else {
            return Ok(true);
        };
        let cur = (self.form)(&crate::sql::expr::eval(&self.key, row, ctx)?);
        let image = |v: &Value| {
            let mut out = Vec::new();
            crate::storage::row::encode_value(&mut out, v);
            out
        };
        if image(&cur) != image(v) {
            return Ok(false);
        }
        Ok(self.seen.insert(dkey.to_vec()))
    }
}

/// IndexAmRoutine's properties (amstrategies, amsupport, ...).
#[derive(Clone, Copy, Debug)]
pub struct AmProps {
    pub strategies: i32,
    pub support: i32,
    /// amoptsprocnum (0: none).
    pub opts_proc: i32,
    pub can_order: bool,
    pub can_order_by_op: bool,
    pub can_backward: bool,
    pub can_unique: bool,
    pub can_multicol: bool,
    pub optional_key: bool,
    pub search_array: bool,
    pub search_nulls: bool,
    /// amstorage: the opclass may store another type than the indexed one.
    pub storage: bool,
    pub clusterable: bool,
    pub can_include: bool,
    /// amgettuple / amgetbitmap: plain and ordered scans, bitmap scans. (A bitmap scan of a
    /// native method's index reads the heap with the index's conditions checked on each row,
    /// as GIN's and BRIN's do here: the rows are the ones the bitmap would select, rechecked.)
    pub gettuple: bool,
    pub getbitmap: bool,
}

/// An index access method of native code: its handler's routine.
pub struct AmRoutine {
    /// The handler's library and C symbol (`vector`, `ivfflathandler`).
    pub library: &'static str,
    pub handler: &'static str,
    pub props: AmProps,
    /// Its WITH options (add_int_reloption): name, default, min, max.
    pub int_options: &'static [(&'static str, i64, i64, i64)],
    pub am: &'static (dyn IndexAm + Sync),
}

impl AmRoutine {
    /// The method's library is loaded (calling its handler loads it): its parameters are
    /// defined, placeholders set before taking their values.
    pub fn load(&self, ctx: &crate::exec::ExecCtx) {
        let warnings = ctx.sess.settings.borrow_mut().load_module(self.library);
        for w in warnings {
            ctx.notice(w);
        }
    }
}

/// The routine handler `symbol` of library `lib` returns.
pub fn routine(lib: &str, symbol: &str) -> Option<&'static AmRoutine> {
    match lib {
        "vector" => crate::ext::vector::am_routine(symbol),
        "bloom" => (symbol == "blhandler").then_some(&crate::ext::bloom::ROUTINE),
        _ => None,
    }
}

/// The routine of access method handler function `handler` (pg_proc oid), when it is a C
/// function the engine implements.
pub fn routine_of(cat: &crate::catalog::Catalog, handler: u32) -> Option<&'static AmRoutine> {
    let f = cat.functions.get(&handler)?;
    if f.language != "c" {
        return None;
    }
    routine(crate::ext::native::library(f.probin.as_deref()?), &f.body)
}

/// Whether `method` is one of PostgreSQL's own index access methods (no routine lookup needed).
pub fn builtin_method(method: &str) -> bool {
    matches!(method, "btree" | "hash" | "gist" | "gin" | "spgist" | "brin")
}

/// Whether index access method `method` is native code (its indexes are the method's own
/// records, not entries the engine keeps).
pub fn is_native(cat: &crate::catalog::Catalog, method: &str) -> bool {
    routine_by_name(cat, method).is_some()
}

/// The routine of index access method `method` (by name), when it is native code.
pub fn routine_by_name(cat: &crate::catalog::Catalog, method: &str) -> Option<&'static AmRoutine> {
    if builtin_method(method) {
        return None;
    }
    let am = cat.access_methods.values().find(|a| a.name == method)?;
    routine_of(cat, am.handler)
}

/// What a scan of an index returns next, in the scan's order: an entry's row (its data key)
/// and, where `wants` says so for the data key, the entry's value of the first key column
/// (made only then: a vector's value is big, and only an updated row's is checked).
pub trait AmScan {
    fn next(&mut self, ctx: &crate::exec::ExecCtx, wants: &dyn Fn(&[u8]) -> bool) -> Result<Option<(Vec<u8>, Option<Value>)>>;
}

/// The engine's side of an index access method (IndexAmRoutine's functions).
pub trait IndexAm {
    /// amoptions: check an index's WITH options (`name=value`), with build_reloptions's errors.
    fn options(&self, opts: &[String]) -> Result<()>;

    /// ambuild: the index over the table's rows (`heap`: as many passes as it takes).
    fn build(&self, ix: &AmIndex, heap: &mut dyn BuildHeap, ctx: &crate::exec::ExecCtx) -> Result<BuildStats>;

    /// aminsert: a new row version's entry.
    fn insert(&self, ix: &AmIndex, dkey: &[u8], values: &[Value], ctx: &crate::exec::ExecCtx) -> Result<()>;

    /// ambulkdelete + amvacuumcleanup: remove the entries `dead` says are dead (it gets the
    /// entry's data key and its first value).
    fn vacuum(&self, ix: &AmIndex, dead: &mut dyn FnMut(&[u8], &Value) -> Result<bool>, ctx: &crate::exec::ExecCtx) -> Result<BuildStats>;

    /// ambeginscan + amrescan: a scan ordered by the distances to `orderbys` (one value per
    /// ORDER BY operator, NULL for a null argument).
    fn scan(&self, ix: &AmIndex, orderbys: &[Value], ctx: &crate::exec::ExecCtx) -> Result<Box<dyn AmScan>>;

    /// genericcostestimate's numIndexTuples, when amcostestimate sets it before calling it
    /// (bloom visits every tuple: the index's tuples); None: estimated from the selectivity.
    fn cost_index_tuples(&self, _ix: &AmIndex, _index_tuples: f64) -> Option<f64> {
        None
    }

    /// amcostestimate.
    fn cost(&self, ix: &AmIndex, input: &CostInput, ctx: &crate::exec::ExecCtx) -> CostOutput;

    /// The index's size in blocks (RelationGetNumberOfBlocks).
    fn blocks(&self, ix: &AmIndex, ctx: &crate::exec::ExecCtx) -> Result<u64>;

    /// The value an entry for key value `v` holds, as `scan` returns it and `vacuum` passes
    /// it to `dead` (what a row's current value is compared with): `v` itself, unless the
    /// operator class stores it changed (hnsw's normalizing classes).
    fn entry_value(&self, _ix: &AmIndex, v: &Value) -> Value {
        v.clone()
    }
}

/// A row as an index build reads it.
pub struct AmRow {
    pub dkey: Vec<u8>,
    /// Its heap block (ctid's block number) where the heap is modelled, else its position in
    /// storage order divided as evenly over the table's blocks.
    pub block: u32,
    /// The index's values of it (NULLs included).
    pub values: Vec<Value>,
}

/// The table's rows as an index build reads them (table_index_build_scan): each pass in heap
/// order, a partial index's rows only.
pub trait BuildHeap {
    /// RelationGetNumberOfBlocks of the table.
    fn blocks(&self) -> u32;
    /// A pass over the rows.
    fn scan(&mut self) -> Box<dyn Iterator<Item = Result<AmRow>> + '_>;
}

/// ambuild's result (heap tuples seen and index tuples made), or VACUUM's (the index tuples
/// left, and how many it removed).
#[derive(Clone, Copy, Debug, Default)]
pub struct BuildStats {
    pub heap_tuples: f64,
    pub index_tuples: f64,
    pub removed: f64,
}

/// What amcostestimate gets: genericcostestimate's results, computed by the planner.
#[derive(Clone, Copy, Debug, Default)]
pub struct CostInput {
    pub has_orderbys: bool,
    pub startup: f64,
    pub total: f64,
    pub selectivity: f64,
    pub correlation: f64,
    pub num_index_pages: f64,
    pub num_index_tuples: f64,
    pub spc_random_page_cost: f64,
    pub spc_seq_page_cost: f64,
    /// The index's tuples (IndexOptInfo.tuples) and the table's pages (rel->pages).
    pub index_tuples: f64,
    pub rel_pages: f64,
}

/// amcostestimate's results. `disabled`: the path counts as disabled (disabled_nodes).
#[derive(Clone, Copy, Debug, Default)]
pub struct CostOutput {
    pub startup: f64,
    pub total: f64,
    pub selectivity: f64,
    pub correlation: f64,
    pub pages: f64,
    pub disabled: bool,
}

/// An index of a native access method, as the method sees it.
pub struct AmIndex {
    pub oid: u32,
    pub name: String,
    pub table: u32,
    pub store: AmStore,
    /// WITH options, `name=value`.
    pub options: Vec<String>,
    pub columns: Vec<AmColumn>,
}

/// A key column of an index: its type (with the column's typmod) and operator class.
pub struct AmColumn {
    pub ty: crate::types::DataType,
    pub opclass: u32,
    /// The class's support functions of C code: (number, library, symbol).
    pub support: Vec<(i16, String, String)>,
}

impl AmColumn {
    /// Support function `number`'s C symbol (in the method's own library).
    pub fn support(&self, number: i16) -> Option<&str> {
        self.support.iter().find(|s| s.0 == number).map(|s| s.2.as_str())
    }
}

impl AmIndex {
    /// The index `ix` of table `t` (keys bound as `keys`), as its method sees it.
    /// (`cat`: the catalog the statement sees, a DDL statement's own.)
    pub fn of(cat: &crate::catalog::Catalog, t: &crate::catalog::Table, ix: &crate::catalog::Index, keys: &[crate::sql::expr::Expr], ctx: &crate::exec::ExecCtx) -> AmIndex {
        let method = cat.access_method_by_name(&ix.method).map_or(0, |a| a.oid);
        let columns = ix
            .columns
            .iter()
            .zip(keys)
            .map(|(c, k)| {
                let ty = k.ty();
                let opclass = if c.opclass != 0 { c.opclass } else { cat.default_opclass(method, &ty) };
                let support = cat
                    .opclasses
                    .get(&opclass)
                    .map(|oc| {
                        cat.opmembers
                            .values()
                            .filter(|m| m.func && m.family == oc.family && m.left == oc.intype)
                            .filter_map(|m| {
                                let f = cat.functions.get(&m.object)?;
                                let lib = crate::ext::native::library(f.probin.as_deref()?).to_string();
                                Some((m.number, lib, f.body.clone()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                AmColumn { ty, opclass, support }
            })
            .collect();
        let node = cat.relfilenode(ix.oid).unwrap_or(ix.oid);
        let mut prefix = crate::storage::keys::index_prefix(t.oid, ix.id);
        prefix.extend_from_slice(&node.to_be_bytes());
        AmIndex {
            oid: ix.oid,
            name: ix.name.clone(),
            table: t.oid,
            store: AmStore {
                store: ctx.sess.db.store.clone(),
                instance: ctx.sess.db.instance,
                prefix,
            },
            options: ix.options.clone(),
            columns,
        }
    }

    /// WITH option `name`'s integer value, if given.
    pub fn int_option(&self, name: &str) -> Option<i64> {
        self.options.iter().find_map(|o| {
            let (k, v) = o.split_once('=')?;
            if !k.trim().eq_ignore_ascii_case(name) {
                return None;
            }
            v.trim().trim_matches('\'').parse().ok()
        })
    }
}

/// Entries whose inserting transaction may still be running, by index (store id): data key
/// to the inserting (sub)transaction's id. VACUUM keeps them (HEAPTUPLE_INSERT_IN_PROGRESS):
/// an uncommitted row isn't in storage yet. In memory only: no transaction outlives the
/// process.
type Inflight = parking_lot::Mutex<std::collections::HashMap<(u64, Vec<u8>), std::collections::HashMap<Vec<u8>, u64>>>;

fn inflight() -> &'static Inflight {
    static C: std::sync::OnceLock<Inflight> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// Note an entry for row `dkey` made by transaction `xid` (entries of transactions before
/// the horizon `oldest` are forgotten as the set grows).
pub fn note_insert(store: &AmStore, dkey: &[u8], xid: u64, oldest: u64) {
    let mut m = inflight().lock();
    let set = m.entry(store.id()).or_default();
    if set.len() >= 4096 && set.len().is_power_of_two() {
        set.retain(|_, x| *x >= oldest);
    }
    set.insert(dkey.to_vec(), xid);
}

/// VACUUM's test of an entry (ambulkdelete's callback): its row (`dkey` of table `t`, its
/// index key `key`) is dead to everyone when no version of it since the oldest snapshot
/// holds `value` (any version will do for a row never updated) and no running transaction
/// made the entry. (`form`: the value an entry holds for a key value, `IndexAm::entry_value`.)
pub fn entry_dead(store: &AmStore, t: &crate::catalog::Table, key: &crate::sql::expr::Expr, form: &dyn Fn(&Value) -> Value, dkey: &[u8], value: &Value, ctx: &crate::exec::ExecCtx) -> Result<bool> {
    let xids = ctx.sess.db.xids();
    if let Some(&x) = inflight().lock().get(&store.id()).and_then(|m| m.get(dkey))
        && x >= xids.horizon()
        && xids.status(x).is_ok_and(|s| s == "in progress")
    {
        return Ok(false);
    }
    let updated = store.updated()?.read().contains(dkey);
    let image = |v: &Value| {
        let mut out = Vec::new();
        crate::storage::row::encode_value(&mut out, v);
        out
    };
    let want = image(value);
    let horizon = store.store.gc_horizon();
    let mut ts = store.store.visible_ts();
    while let Some((vts, bytes)) = store.store.get_version(dkey, ts)? {
        if let Some(b) = bytes {
            if !updated {
                return Ok(false);
            }
            let row = crate::exec::scan::decode_table_row(t, &b)?;
            if image(&form(&crate::sql::expr::eval(key, &row, ctx)?)) == want {
                return Ok(false);
            }
        }
        if vts <= horizon || vts == 0 {
            break;
        }
        ts = vts - 1;
    }
    Ok(true)
}

/// The first key byte of the updated rows' records (the method's own keys are below it).
const UPDATED: u8 = 0xFF;

type UpdatedSets = parking_lot::Mutex<std::collections::HashMap<(u64, Vec<u8>), std::sync::Arc<parking_lot::RwLock<std::collections::HashSet<Vec<u8>>>>>>;

fn updated_cache() -> &'static UpdatedSets {
    static C: std::sync::OnceLock<UpdatedSets> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// The caches native index methods keep of indexes whose records start with `prefix`
/// (one index's, or a table's every index's) let go: DROP INDEX, DROP TABLE and TRUNCATE
/// delete those records (a rollback reads them back from the records it restores). hnsw's
/// graphs, ivfflat's lists and bloom's pages hold every entry of their index.
pub fn evict(instance: u64, prefix: &[u8]) {
    crate::ext::vector::evict(instance, prefix);
    crate::ext::bloom::evict(instance, prefix);
}

/// An index's records: under the index's key prefix and relfilenode (so TRUNCATE and REINDEX,
/// which give it a new relfilenode, start an empty set, and their rollback finds the old one),
/// written at once rather than at commit, as PostgreSQL writes index pages.
#[derive(Clone)]
pub struct AmStore {
    pub store: std::sync::Arc<crate::storage::Store>,
    /// The database's instance (caches of one database's indexes).
    pub instance: u64,
    pub prefix: Vec<u8>,
}

impl AmStore {
    /// What identifies the index's records across sessions (a cache key).
    pub fn id(&self) -> (u64, Vec<u8>) {
        (self.instance, self.prefix.clone())
    }

    /// Whether the index has any record (its storage dropped, a cache of it can go).
    pub fn has_records(&self) -> bool {
        let end = crate::storage::keys::prefix_end(&self.prefix);
        self.store.scan_raw(&self.prefix, &end, self.store.visible_ts()).next().is_some()
    }

    /// Every record of the method's (keys not starting with 0xFF): (key after the prefix,
    /// value), in key order.
    pub fn load(&self) -> Result<Vec<(Vec<u8>, bytes::Bytes)>> {
        let mut end = self.prefix.clone();
        end.push(UPDATED);
        self.records(&self.prefix, &end)
    }

    fn records(&self, start: &[u8], end: &[u8]) -> Result<Vec<(Vec<u8>, bytes::Bytes)>> {
        let ts = self.store.visible_ts();
        let n = self.prefix.len();
        self.store
            .scan(start, end, ts)
            .map(|r| r.map(|(k, v)| (k[n..].to_vec(), v)))
            .collect()
    }

    /// Remove every record (the method's and the updated rows').
    pub fn clear(&self) -> Result<()> {
        let end = crate::storage::keys::prefix_end(&self.prefix);
        let all: Vec<(Vec<u8>, Option<Vec<u8>>)> = self.records(&self.prefix, &end)?.into_iter().map(|(k, _)| (k, None)).collect();
        self.write(all)?;
        updated_cache().lock().remove(&self.id());
        Ok(())
    }

    /// The rows given another entry by an UPDATE (whose older entries a scan must check).
    pub fn updated(&self) -> Result<std::sync::Arc<parking_lot::RwLock<std::collections::HashSet<Vec<u8>>>>> {
        let id = self.id();
        if let Some(u) = updated_cache().lock().get(&id) {
            return Ok(u.clone());
        }
        let mut start = self.prefix.clone();
        start.push(UPDATED);
        let end = crate::storage::keys::prefix_end(&start);
        let set = self.records(&start, &end)?.into_iter().map(|(k, _)| k[1..].to_vec()).collect();
        let u = std::sync::Arc::new(parking_lot::RwLock::new(set));
        Ok(updated_cache().lock().entry(id).or_insert(u).clone())
    }

    /// Note that row `dkey` got another entry (or lost its value) by an UPDATE.
    pub fn mark_updated(&self, dkey: &[u8]) -> Result<()> {
        let u = self.updated()?;
        if u.read().contains(dkey) {
            return Ok(());
        }
        let mut k = vec![UPDATED];
        k.extend_from_slice(dkey);
        self.write(vec![(k, Some(Vec::new()))])?;
        u.write().insert(dkey.to_vec());
        Ok(())
    }

    /// Write records (None deletes), by key after the prefix.
    pub fn write(&self, writes: Vec<(Vec<u8>, Option<Vec<u8>>)>) -> Result<()> {
        if writes.is_empty() {
            return Ok(());
        }
        let writes = writes
            .into_iter()
            .map(|(k, v)| {
                let mut key = self.prefix.clone();
                key.extend_from_slice(&k);
                (key, v.map(bytes::Bytes::from))
            })
            .collect();
        self.store.commit(writes, |_| Ok(()))?;
        Ok(())
    }
}
