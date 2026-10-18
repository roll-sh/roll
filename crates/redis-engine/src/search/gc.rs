//! The fork GC (`fork_gc/`, `gc.c`): a pass over an index's terms, numeric trees, tags,
//! missing-field and existing-docs indexes that rebuilds every block holding records of documents
//! gone from the doc table, drops the indexes left empty (with their terms, tag values and
//! suffixes), and trims numeric trees left sparse. The module scans in a forked child and applies
//! in the parent; nothing can change in between here, so the two halves run back to back with
//! the same statistics (`FGC_updateStats`, `gc_stats`).

use super::index::{Index, Suffix};
use super::invidx::GcInfo;
use super::spec;
use super::triemap::TrieMap;

/// `ForkGCStats` plus the context's scheduling state.
#[derive(Clone, Debug, Default)]
pub struct Gc {
    /// `totalCollected`: bytes freed minus bytes allocated.
    pub total_collected: i64,
    pub total_ms_run: u64,
    pub num_cycles: u64,
    pub last_run_ms: u64,
    pub numeric_nodes_missed: u64,
    pub blocks_denied: u64,
    /// `deletedOrUpdatedDocsFromLastRun`.
    pub pending: u64,
    /// `timerID`: when the next periodic run is due (Unix ms); 0 once stopped
    /// (FT.DEBUG GC_STOP_SCHEDULE).
    pub next_run: i64,
}

/// `getNextPeriod`: the run interval plus up to as many seconds again, at random.
pub fn next_period_ms(interval_s: i64) -> i64 {
    let interval = interval_s.max(1);
    interval * 1000 + (crate::util::now_ms().rem_euclid(interval)) * 1000
}

impl Gc {
    /// `GCContext_CreateGC` + `GCContext_Start`.
    pub fn new(interval_s: i64) -> Self {
        Gc { next_run: crate::util::now_ms() + next_period_ms(interval_s), ..Gc::default() }
    }
}

/// The GC knobs a cycle reads.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    /// `forkGcCleanThreshold`.
    pub clean_threshold: u64,
    /// `forkGCCleanNumericEmptyNodes`.
    pub clean_numeric_empty_nodes: bool,
}

impl Settings {
    pub fn from_cfg(cfg: &crate::config::Config) -> Self {
        Settings {
            clean_threshold: cfg.num("search-fork-gc-clean-threshold").max(0) as u64,
            clean_numeric_empty_nodes: !cfg.flag("ft:no-clean-numeric-empty-nodes"),
        }
    }
}

/// `FGC_RunCycle`: unless forced, nothing happens below the clean threshold; otherwise a pass
/// and the cycle's timing. False when the index has no GC.
pub fn run_cycle(ix: &mut Index, force: bool, set: Settings) -> bool {
    let Some(gc) = ix.gc.as_mut() else { return false };
    if !force && gc.pending < set.clean_threshold {
        return true;
    }
    let started = std::time::Instant::now();
    gc.pending = 0;
    collect(ix, set);
    let ms = started.elapsed().as_millis() as u64;
    let gc = ix.gc.as_mut().expect("checked above");
    gc.num_cycles += 1;
    gc.total_ms_run += ms;
    gc.last_run_ms = ms;
    true
}

/// `FGC_updateStats`.
fn update_stats(ix: &mut Index, records_removed: usize, freed: usize, allocated: usize) {
    ix.stats.num_records -= records_removed as i64;
    ix.stats.inverted_size += allocated as i64 - freed as i64;
    if let Some(gc) = ix.gc.as_mut() {
        gc.total_collected += freed as i64 - allocated as i64;
    }
}

/// `deleteSuffixTrieMap` (tags): the value leaves every suffix entry listing it; entries left
/// listing nothing go. Returns the entries deleted.
pub fn delete_suffix(trie: &mut TrieMap<Suffix>, s: &[u8]) -> usize {
    let mut deleted = 0;
    for j in 0..s.len() {
        let key = &s[j..];
        let Some(d) = trie.get_mut(key) else { continue };
        if j == 0 {
            d.term = None;
        }
        if let Some(i) = d.terms.iter().position(|t| t.as_ref() == s) {
            d.terms.swap_remove(i);
        }
        if d.terms.is_empty() {
            trie.remove(key);
            deleted += 1;
        }
    }
    deleted
}

/// One cycle's pass (`FGC_childScanIndexes` + `FGC_parentHandleFromChild`).
fn collect(ix: &mut Index, set: Settings) {
    collect_terms(ix);
    collect_numeric(ix, set);
    collect_tags(ix);
    collect_missing(ix);
    collect_existing(ix);
}

/// `FGC_childCollectTerms` / `FGC_parentHandleTerms`, in the terms trie's order.
fn collect_terms(ix: &mut Index) {
    let terms: Vec<Vec<u8>> = ix.term_trie.iterate_all().collect_all().into_iter().filter_map(|e| super::trie::runes_to_str(&e.0)).collect();
    for term in terms {
        let docs = &ix.docs;
        let Some(ii) = ix.terms.get_mut(term.as_slice()) else { continue };
        let Some(mut info) = ii.gc(&|id| docs.exists(id), &mut |_| {}) else { continue };
        ix.stats.total_blocks += info.block_count_delta;
        if ii.unique_docs() == 0 {
            info.bytes_freed += ii.memory_usage();
            ix.stats.total_blocks -= ii.number_of_blocks() as i64;
            ix.terms.remove(term.as_slice());
            ix.term_trie.delete(&term);
            ix.stats.num_terms = ix.stats.num_terms.wrapping_sub(1);
            ix.stats.terms_size = ix.stats.terms_size.wrapping_sub(term.len() as u64);
            if !term.is_empty()
                && let Some(s) = ix.suffix.as_mut()
            {
                ix.suffix_size = ix.suffix_size.wrapping_sub(s.delete(&term));
            }
        }
        update_stats(ix, info.entries_removed, info.bytes_freed, info.bytes_allocated);
    }
}

/// `FGC_childCollectNumeric` / `FGC_parentHandleNumeric`: every numeric and geo field's tree,
/// then (when a node had something collected) `NumericRangeTree_CompactIfSparse`.
fn collect_numeric(ix: &mut Index, set: Settings) {
    for fi in 0..ix.spec.fields.len() {
        if !ix.spec.fields[fi].is(spec::T_NUMERIC | spec::T_GEO) {
            continue;
        }
        let docs = &ix.docs;
        let Some(tree) = ix.fields[fi].numeric.as_mut() else { continue };
        let r = tree.gc(&|id| docs.exists(id));
        if r.nodes == 0 {
            continue;
        }
        let compact = set.clean_numeric_empty_nodes.then(|| tree.compact_if_sparse().0);
        ix.stats.total_blocks += r.block_count_delta;
        update_stats(ix, r.entries_removed, r.bytes_freed, r.bytes_allocated);
        if let Some(t) = compact {
            if t.size_delta < 0 {
                update_stats(ix, 0, (-t.size_delta) as usize, 0);
            }
            ix.stats.total_blocks += t.block_count_delta as i64;
        }
    }
}

/// `FGC_childCollectTags` / `FGC_parentHandleTags`: each tag field's values in order.
fn collect_tags(ix: &mut Index) {
    for fi in 0..ix.spec.fields.len() {
        if !ix.spec.fields[fi].is(spec::T_TAG) {
            continue;
        }
        let Some(values) = ix.fields[fi].tag.as_ref().map(|t| t.values.keys()) else { continue };
        for v in values {
            let docs = &ix.docs;
            let Some(tag) = ix.fields[fi].tag.as_mut() else { break };
            let Some(ii) = tag.values.get_mut(&v) else { continue };
            let Some(mut info) = ii.gc(&|id| docs.exists(id), &mut |_| {}) else { continue };
            ix.stats.total_blocks += info.block_count_delta;
            if ii.unique_docs() == 0 {
                info.bytes_freed += ii.memory_usage();
                ix.stats.total_blocks -= ii.number_of_blocks() as i64;
                tag.values.remove(&v);
                if !v.is_empty()
                    && let Some(s) = tag.suffix.as_mut()
                {
                    delete_suffix(s, &v);
                }
            }
            update_stats(ix, info.entries_removed, info.bytes_freed, info.bytes_allocated);
        }
    }
}

/// `FGC_childCollectMissingDocs` / `FGC_parentHandleMissingDocs`: their records were never
/// counted, so none are taken off `num_records`.
fn collect_missing(ix: &mut Index) {
    let names: Vec<bytes::Bytes> = ix.missing.keys().cloned().collect();
    for name in names {
        let docs = &ix.docs;
        let Some(ii) = ix.missing.get_mut(&name) else { continue };
        let Some(mut info) = ii.gc(&|id| docs.exists(id), &mut |_| {}) else { continue };
        ix.stats.total_blocks += info.block_count_delta;
        if ii.unique_docs() == 0 {
            info.bytes_freed += ii.memory_usage();
            ix.stats.total_blocks -= ii.number_of_blocks() as i64;
            ix.missing.remove(&name);
        }
        update_stats(ix, 0, info.bytes_freed, info.bytes_allocated);
    }
}

/// `collect_existing_docs` / `apply_existing_docs` (INDEXALL).
fn collect_existing(ix: &mut Index) {
    let docs = &ix.docs;
    let Some(ii) = ix.existing.as_mut() else { return };
    let Some(GcInfo { mut bytes_freed, bytes_allocated, block_count_delta, .. }) = ii.gc(&|id| docs.exists(id), &mut |_| {}) else { return };
    let mut blocks = block_count_delta;
    if ii.unique_docs() == 0 {
        bytes_freed += ii.memory_usage();
        blocks -= ii.number_of_blocks() as i64;
        ix.existing = None;
    }
    ix.stats.total_blocks += blocks;
    update_stats(ix, 0, bytes_freed, bytes_allocated);
}
