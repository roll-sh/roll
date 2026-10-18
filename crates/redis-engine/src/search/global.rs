//! The module's global state: the index registry (`specDict_g`), the schema prefixes trie
//! (`SchemaPrefixes_g`), aliases, and the keyspace subscription's scratch state (`hashFields`,
//! `global_RenameFromKey`).

use super::index::Index;
use super::rsdict::RsDict;
use super::error::{Code, QErr};
use super::triemap::TrieMap;
use bytes::Bytes;

/// `maxIndexes_g`.
pub const MAX_INDEXES: usize = 200_000;

/// The keyspace event classes the module subscribes to (`Initialize_KeyspaceNotifications`):
/// generic, hash, string, expired, evicted and module (`trimmed`, `key_trimmed` and `loaded`
/// have no class of their own here).
pub const EVENT_CLASSES: u32 =
    crate::notify::GENERIC | crate::notify::HASH | crate::notify::STRING | crate::notify::EXPIRED | crate::notify::EVICTED | crate::notify::MODULE;

/// A spelling dictionary's trie: terms (runes, in trie order) and their scores.
pub type Dict = std::collections::BTreeMap<Vec<u16>, f32>;

#[derive(Debug, Default)]
pub struct Global {
    /// `specDict_g`: indexes by name.
    pub indexes: RsDict<Index>,
    /// `SchemaPrefixes_g`: a prefix → the indexes it routes keys to, in creation order.
    pub prefixes: TrieMap<Vec<Bytes>>,
    /// `AliasTable_g`: alias → index name.
    pub aliases: RsDict<Bytes>,
    pub next_spec_id: u64,
    /// The module subscribed to keyspace events (its first index was created).
    pub subscribed: bool,
    /// `hashFields`: the fields the last H* command named (the command filter).
    pub hash_fields: Option<Vec<Bytes>>,
    /// `global_RenameFromKey`.
    pub rename_from: Option<Bytes>,
    /// `spellCheckDicts`: spelling dictionaries (FT.DICTADD), their terms as runes in trie order
    /// with their trie scores (1 per FT.DICTADD of the term).
    pub dicts: RsDict<Dict>,
    /// `maxIndexes_g` when FT.DEBUG SET_MAX_INDEXES changed it.
    pub max_indexes: Option<usize>,
    /// `g_CursorsList`: FT.AGGREGATE and FT.SEARCH WITHCURSOR's cursors (they outlive their index).
    pub cursors: super::cursor::Cursors,
    /// The shared SVS thread pool (`VecSimSVSThreadPoolImpl`) had an index attached: an SVS
    /// index was built (its first vector arrived) at some point. Process-wide and sticky.
    pub svs_pool: bool,
    /// The process-wide switches FT.DEBUG sets.
    pub debug: DebugState,
    /// What indexing keeps outside any index.
    pub indexing: super::index::Indexing,
    /// `RSGlobalStats.totalStats.queries`: the query counters INFO reports.
    pub stats: QueryStats,
    /// `RSGlobalStats.totalStats.used_dialects` is set with each index's bits; here the bits of
    /// the indexes dropped since the last flush (the live ones are or-ed in).
    pub dropped_dialects: u8,
    /// Dropped indexes' GCs still counting their documents not collected
    /// (`IndexsGlobalStats_*LogicallyDeleted`): a GC goes (`onTerminateCb`) at its next run,
    /// when it finds its index gone; (due, Unix ms; count).
    pub gc_orphans: Vec<(i64, u64)>,
}

/// A query error INFO counts (`QueryErrorsGlobalStats_UpdateError`'s codes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryError {
    Syntax,
    /// `QUERY_ERROR_CODE_PARSE_ARGS` and `QUERY_ERROR_CODE_ARG_UNRECOGNIZED`.
    Arguments,
    Timeout,
    Oom,
    UnavailableSlots,
}

impl QueryError {
    /// The counted code an error reply starts with (other codes aren't counted).
    pub fn of_reply(msg: &str) -> Option<QueryError> {
        let code = msg.split(' ').next().unwrap_or("");
        Some(match code {
            "SEARCH_SYNTAX" => QueryError::Syntax,
            "SEARCH_PARSE_ARGS" | "SEARCH_ARG_UNRECOGNIZED" => QueryError::Arguments,
            "SEARCH_TIMEOUT" => QueryError::Timeout,
            "SEARCH_OOM" => QueryError::Oom,
            "SEARCH_SLOTS_UNAVAIL" => QueryError::UnavailableSlots,
            _ => return None,
        })
    }
}

/// A query warning INFO counts (`QueryWarningsGlobalStats_UpdateWarning`'s codes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryWarning {
    Timeout,
    /// The shard's and the coordinator's OOM warnings alike.
    Oom,
    MaxPrefixExpansions,
    AsmInaccurateResults,
}

/// One side's (`shard_*` or `coord_*`) error and warning counters.
#[derive(Debug, Default)]
pub struct ErrorCounters {
    pub syntax: AtomicU64,
    pub arguments: AtomicU64,
    pub timeout: AtomicU64,
    pub oom: AtomicU64,
    pub unavailable_slots: AtomicU64,
    pub warn_timeout: AtomicU64,
    pub warn_oom: AtomicU64,
    pub warn_max_prefix_expansions: AtomicU64,
    pub warn_asm_inaccurate: AtomicU64,
}

/// The process-wide query counters (`QueriesGlobalStats`); atomics, so query execution (which
/// sees the engine read-only) can count. A standalone server counts its own queries' errors and
/// warnings as the coordinator's (`COORD_ERR_WARN`); the shard side only gets what a shard
/// reply carries.
///
/// The hooks for query execution: [`QueryStats::count_query`] once per reply chunk sent (a
/// query, an FT.CURSOR READ), [`QueryStats::error`] / [`QueryStats::error_reply`] for an error
/// replied, [`QueryStats::warning`] for each warning a chunk carries (a timeout with the RETURN
/// policy, OOM, the prefix expansion limit).
#[derive(Debug, Default)]
pub struct QueryStats {
    pub queries_processed: AtomicU64,
    pub query_commands: AtomicU64,
    pub execution_ns: AtomicU64,
    pub shard: ErrorCounters,
    pub coord: ErrorCounters,
}

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

impl QueryStats {
    /// `TotalGlobalStats_CountQuery`: every chunk sent counts as a command; as a query unless
    /// it is a later read of a cursor (FT.CURSOR READ).
    pub fn count_query(&self, cursor_read: bool, elapsed: std::time::Duration) {
        self.query_commands.fetch_add(1, Relaxed);
        self.execution_ns.fetch_add(elapsed.as_nanos() as u64, Relaxed);
        if !cursor_read {
            self.queries_processed.fetch_add(1, Relaxed);
        }
    }

    fn side(&self, coord: bool) -> &ErrorCounters {
        if coord { &self.coord } else { &self.shard }
    }

    /// `QueryErrorsGlobalStats_UpdateError`.
    pub fn error(&self, e: QueryError, coord: bool) {
        let c = self.side(coord);
        match e {
            QueryError::Syntax => &c.syntax,
            QueryError::Arguments => &c.arguments,
            QueryError::Timeout => &c.timeout,
            QueryError::Oom => &c.oom,
            QueryError::UnavailableSlots => &c.unavailable_slots,
        }
        .fetch_add(1, Relaxed);
    }

    /// An error reply's code, when it is one INFO counts.
    pub fn error_reply(&self, msg: &str, coord: bool) {
        if let Some(e) = QueryError::of_reply(msg) {
            self.error(e, coord);
        }
    }

    /// `QueryWarningsGlobalStats_UpdateWarning`.
    pub fn warning(&self, w: QueryWarning, coord: bool) {
        let c = self.side(coord);
        match w {
            QueryWarning::Timeout => &c.warn_timeout,
            QueryWarning::Oom => &c.warn_oom,
            QueryWarning::MaxPrefixExpansions => &c.warn_max_prefix_expansions,
            QueryWarning::AsmInaccurateResults => &c.warn_asm_inaccurate,
        }
        .fetch_add(1, Relaxed);
    }

    /// A timeout where the query runs: ON_TIMEOUT FAIL's error or RETURN's warning, `internal`
    /// for the `_FT.*` (shard) requests. A user-facing error goes out as an error reply, which
    /// the command wrapper counts by its code, so only internal errors are counted here.
    pub fn timeout(&self, error: bool, internal: bool) {
        match (error, internal) {
            (true, false) => {}
            (true, true) => self.error(QueryError::Timeout, false),
            (false, internal) => self.warning(QueryWarning::Timeout, !internal),
        }
    }

    /// The memory guard's error (ON_OOM FAIL) or its empty reply's warning (RETURN), counted as
    /// [`QueryStats::timeout`] counts.
    pub fn oom(&self, error: bool, internal: bool) {
        match (error, internal) {
            (true, false) => {}
            (true, true) => self.error(QueryError::Oom, false),
            (false, internal) => self.warning(QueryWarning::Oom, !internal),
        }
    }
}

/// What `_FT.DEBUG` toggles outside any index: the thread pools' pause states, the test
/// scorers, the yield sleep and the VecSim timeout mock.
#[derive(Debug, Default)]
pub struct DebugState {
    /// `workersThreadPool_pause` (search-workers' pool).
    pub workers_paused: bool,
    /// `ConcurrentSearch_pause` (the coordinator's pool).
    pub coord_paused: bool,
    /// `Ext_RegisterTestScorers` ran.
    pub test_scorers: bool,
    /// `indexerSleepBeforeYieldMicros`.
    pub sleep_before_yield_us: u32,
    /// VECSIM_MOCK_TIMEOUT enable.
    pub vecsim_timeout: bool,
}

/// An index whose SVS-VAMANA vector index has been built.
fn has_svs(ix: &Index) -> bool {
    ix.fields.iter().any(|f| f.vectors.as_ref().is_some_and(|v| v.algo == super::spec::Algo::Svs))
}

impl Global {
    /// `VecSim_UpdateThreadPoolSize`: no worker threads writes vectors in place, workers
    /// ingest them in the background.
    pub fn set_vec_mode(&mut self, workers: i128) {
        let mode = if workers > 0 { super::hnsw::WriteMode::Async } else { super::hnsw::WriteMode::InPlace };
        for ix in self.indexes.values_mut() {
            ix.vec_mode = mode;
        }
    }

    pub fn get(&self, name: &[u8]) -> Option<&Index> {
        self.indexes.get(name)
    }

    pub fn get_mut(&mut self, name: &[u8]) -> Option<&mut Index> {
        self.indexes.get_mut(name)
    }

    /// `IndexSpec_LoadUnsafe`: an index by name, or by alias.
    pub fn resolve(&self, name: &[u8]) -> Option<&Bytes> {
        if let Some((k, _)) = self.indexes.get_key_value(name) {
            return Some(k);
        }
        self.aliases.get(name)
    }

    /// `VecSim_GetSharedMemory`: what the shared SVS thread pool's allocator holds once an index
    /// attached, its slot vector and one `ThreadSlot` per worker (`search-workers`, at least 1).
    pub fn vecsim_shared_memory(&self, workers: usize) -> usize {
        if self.svs_pool || self.indexes.values().any(has_svs) { 192 + 80 * workers.max(1) } else { 0 }
    }

    /// `RSGlobalStats.totalStats.used_dialects`: bit `d - 1` for each dialect a query used since
    /// the last flush.
    pub fn used_dialects(&self) -> u8 {
        self.indexes.values().fold(self.dropped_dialects, |a, ix| a | ix.used_dialects)
    }

    /// `IndexesGlobalStats_GetLogicallyDeletedDocs`: the live GCs' documents not collected yet,
    /// and those of the dropped indexes' GCs that haven't run since.
    pub fn logically_deleted(&self, now_ms: i64) -> u64 {
        let live: u64 = self.indexes.values().filter_map(|ix| ix.gc.as_ref()).map(|g| g.pending).sum();
        live + self.gc_orphans.iter().filter(|(due, _)| *due > now_ms).map(|(_, n)| n).sum::<u64>()
    }

    /// `maxIndexes_g`.
    pub fn max_indexes(&self) -> usize {
        self.max_indexes.unwrap_or(MAX_INDEXES)
    }

    /// `Indexes_LoadIndexSpecUnsafeEx`: by name (or alias, unless `no_alias`), counting a use
    /// and restarting a TEMPORARY index's timer (`IndexSpec_OnAcquire`).
    pub fn load(&mut self, name: &[u8], no_alias: bool) -> Option<Bytes> {
        let n = if no_alias { self.indexes.get_key_value(name).map(|(k, _)| k) } else { self.resolve(name) }.cloned()?;
        if let Some(ix) = self.indexes.get_mut(&n) {
            ix.counter += 1;
            ix.restart_timer();
        }
        Some(n)
    }

    /// The same lookup with `INDEXSPEC_LOAD_NOTIMERUPDATE` (FT.DEBUG TTL and friends).
    pub fn load_no_timer(&mut self, name: &[u8]) -> Option<Bytes> {
        let n = self.resolve(name).cloned()?;
        if let Some(ix) = self.indexes.get_mut(&n) {
            ix.counter += 1;
        }
        Some(n)
    }

    /// `IndexSpec_OnAcquire`: a lookup by name or alias counts a use (FT.INFO's `number_of_uses`).
    pub fn count_use(&mut self, name: &[u8]) {
        if let Some(n) = self.resolve(name).cloned()
            && let Some(ix) = self.indexes.get_mut(&n)
        {
            ix.counter += 1;
            ix.restart_timer();
        }
    }

    /// `INDEXSPEC_LOAD_NOCOUNTERINC` (`VERIFY_ACL`): the timer restarts, no use is counted.
    pub fn touch(&mut self, name: &[u8]) {
        if let Some(n) = self.resolve(name).cloned()
            && let Some(ix) = self.indexes.get_mut(&n)
        {
            ix.restart_timer();
        }
    }

    /// `onFlush` (FLUSHDB/FLUSHALL): every index, alias, dictionary and cursor goes, and the
    /// dialects used are forgotten.
    pub fn flush(&mut self) {
        // `Indexes_Free`: each spec is deleted from the registry, whose tables never shrink.
        for name in self.indexes.keys().cloned().collect::<Vec<_>>() {
            self.remove(&name);
        }
        self.dropped_dialects = 0;
        self.prefixes = TrieMap::new();
        self.cursors.clear();
        self.aliases.clear();
        self.dicts.clear();
    }

    /// `IndexAlias_Add`.
    pub fn add_alias(&mut self, alias: Bytes, target: &Bytes) -> Result<(), QErr> {
        if !self.aliases.add(alias.clone(), target.clone()) {
            return Err(QErr::new(Code::IndexExists, "Alias already exists"));
        }
        if let Some(ix) = self.indexes.get_mut(target) {
            ix.aliases.push(alias);
        }
        Ok(())
    }

    /// `IndexAlias_Del`: the alias must be one of the index's (compared without case).
    pub fn del_alias(&mut self, alias: &[u8], target: &Bytes) -> Result<(), QErr> {
        let Some(ix) = self.indexes.get_mut(target) else { return Ok(()) };
        let Some(i) = ix.aliases.iter().position(|a| a.eq_ignore_ascii_case(alias)) else {
            return Err(QErr::new(Code::NoIndex, "Alias does not belong to provided spec"));
        };
        ix.aliases.swap_remove(i);
        self.aliases.delete(alias);
        Ok(())
    }

    /// Registers a new index and its prefixes (`SchemaPrefixes_Add`).
    pub fn add(&mut self, index: Index) {
        let name = index.spec.name.clone();
        for p in &index.spec.rule.prefixes {
            match self.prefixes.get_mut(p) {
                Some(v) => v.push(name.clone()),
                None => {
                    self.prefixes.insert(p, vec![name.clone()]);
                }
            }
        }
        self.indexes.add(name, index);
    }

    /// `Indexes_RemoveSpecFromGlobals`: the index, its prefixes and aliases go.
    pub fn remove(&mut self, name: &[u8]) -> Option<Index> {
        let index = self.indexes.delete(name)?;
        self.svs_pool |= has_svs(&index);
        self.dropped_dialects |= index.used_dialects;
        let now = crate::util::now_ms();
        self.gc_orphans.retain(|(due, _)| *due > now);
        if let Some(gc) = &index.gc
            && gc.pending > 0
        {
            // A stopped GC (FT.DEBUG GC_STOP_SCHEDULE) never runs again.
            self.gc_orphans.push((if gc.next_run == 0 { i64::MAX } else { gc.next_run }, gc.pending));
        }
        for p in &index.spec.rule.prefixes {
            if let Some(v) = self.prefixes.get_mut(p)
                && let Some(i) = v.iter().position(|n| n.as_ref() == name)
            {
                v.swap_remove(i);
                if v.is_empty() {
                    self.prefixes.remove(p);
                }
            }
        }
        // `IndexSpec_ClearAliases`.
        for a in &index.aliases {
            self.aliases.delete(a);
        }
        // An alias the index's list lost track of (FT.ALIASDEL with another case removes the list
        // entry but not the table's) would dangle in the module, a use-after-free; here it goes.
        let stale: Vec<Bytes> = self.aliases.iter().filter(|(_, t)| t.as_ref() == name).map(|(a, _)| a.clone()).collect();
        for a in stale {
            self.aliases.delete(&a);
        }
        Some(index)
    }

    /// The indexes whose prefixes match a key, each once, in prefix order (`TrieMap_FindPrefixes`).
    pub fn matching(&self, key: &[u8]) -> Vec<Bytes> {
        let mut out: Vec<Bytes> = Vec::new();
        for names in self.prefixes.find_prefixes(key) {
            for n in names {
                if !out.contains(n) {
                    out.push(n.clone());
                }
            }
        }
        out
    }
}
