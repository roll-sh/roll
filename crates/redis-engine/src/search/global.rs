//! The module's global state: the index registry (`specDict_g`), the schema prefixes trie
//! (`SchemaPrefixes_g`), aliases, and the keyspace subscription's scratch state (`hashFields`,
//! `global_RenameFromKey`).

use super::index::Index;
use super::rsdict::RsDict;
use super::error::{Code, QErr};
use super::triemap::TrieMap;
use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// `g_CursorsList`: FT.AGGREGATE WITHCURSOR's cursors (they outlive their index).
    pub cursors: super::cursor::Cursors,
    /// The shared SVS thread pool (`VecSimSVSThreadPoolImpl`) had an index attached: an SVS
    /// index was built (its first vector arrived) at some point. Process-wide and sticky.
    pub svs_pool: bool,
    /// The process-wide switches FT.DEBUG sets.
    pub debug: DebugState,
    /// The queries' timeout counters.
    pub query_stats: QueryStats,
}

/// `RSGlobalStats.totalStats.queries`' timeout counters, INFO search's
/// `search_{coord,shard}_total_query_{errors,warnings}_timeout`: a reply's timeout warning
/// (every chunk's) and ON_TIMEOUT FAIL's timeout error, by user-facing (`coord`) and internal
/// (`_FT.*`, shard) requests. Counted through a shared engine reference, hence atomics.
#[derive(Debug, Default)]
pub struct QueryStats {
    pub coord_errors_timeout: AtomicU64,
    pub coord_warnings_timeout: AtomicU64,
    pub shard_errors_timeout: AtomicU64,
    pub shard_warnings_timeout: AtomicU64,
}

impl QueryStats {
    /// `QueryErrorsGlobalStats_UpdateError` / `QueryWarningsGlobalStats_UpdateWarning` with
    /// a timeout.
    pub fn timeout(&self, error: bool, internal: bool) {
        let c = match (error, internal) {
            (true, false) => &self.coord_errors_timeout,
            (false, false) => &self.coord_warnings_timeout,
            (true, true) => &self.shard_errors_timeout,
            (false, true) => &self.shard_warnings_timeout,
        };
        c.fetch_add(1, Ordering::Relaxed);
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

    /// `onFlush` (FLUSHDB/FLUSHALL): every index, alias, dictionary and cursor goes.
    pub fn flush(&mut self) {
        // `Indexes_Free`: each spec is deleted from the registry, whose tables never shrink.
        for name in self.indexes.keys().cloned().collect::<Vec<_>>() {
            self.remove(&name);
        }
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
