//! Indexes of native access methods (`crate::ext::index_am`): building them.

use super::Ddl;
use crate::Result;
use crate::catalog::{Index, Table};
use crate::exec::Row;
use crate::ext::index_am::{AmIndex, AmRoutine, AmRow};
use crate::sql::plan::BoundIndex;
use crate::storage::keys::{index_prefix, prefix_end};
use crate::types::Value;

impl Ddl<'_, '_> {
    /// Delete an index's stored entries in this transaction: the index dropped (`gone`), or
    /// cleared before it is built again (TRUNCATE, REINDEX, a rewrite).
    ///
    /// One range deletion where it can be (`storage::rdel`): it hides every version written
    /// before this transaction commits. Entries of a built-in method a rebuild writes go in
    /// through this transaction, after it. A native method's records are written outside
    /// transactions, under the index's relfilenode (`AmIndex::of`), a rebuild's before this
    /// commits: a range deletion keeps off the relfilenode the index is rebuilt under, whose
    /// records (if it is the old one) go a key at a time, including those written outside
    /// transactions since this transaction's snapshot.
    pub(super) fn delete_index_storage(&self, t: &Table, ix: &Index, gone: bool) -> Result<()> {
        if ix.id == 0 {
            return Ok(());
        }
        let start = index_prefix(t.oid, ix.id);
        let end = prefix_end(&start);
        let instance = self.ctx.sess.db.instance;
        if gone || crate::ext::index_am::builtin_method(&ix.method) {
            self.ctx.txn.borrow_mut().delete_range(start.clone(), end)?;
            crate::ext::index_am::evict(instance, &start);
            return Ok(());
        }
        let node = self.cat.relfilenode(ix.oid).unwrap_or(ix.oid);
        let mut cur = start.clone();
        cur.extend_from_slice(&node.to_be_bytes());
        let cur_end = prefix_end(&cur);
        {
            let mut txn = self.ctx.txn.borrow_mut();
            txn.delete_range(start.clone(), cur.clone())?;
            txn.delete_range(cur_end.clone(), end)?;
        }
        let mut keys: Vec<Vec<u8>> = self
            .ctx
            .txn
            .borrow()
            .scan(&cur, &cur_end)
            .map(|e| e.map(|(k, _)| k))
            .collect::<Result<_>>()?;
        let store = &self.ctx.sess.db.store;
        for e in store.scan(&cur, &cur_end, store.visible_ts()) {
            keys.push(e?.0);
        }
        keys.sort_unstable();
        keys.dedup();
        let mut txn = self.ctx.txn.borrow_mut();
        for k in keys {
            txn.delete(k)?;
        }
        crate::ext::index_am::evict(instance, &start);
        Ok(())
    }

    /// VACUUM of the indexes of native methods of table `t`: ambulkdelete over the entries
    /// whose rows are dead to everyone (`index_am::entry_dead`), then amvacuumcleanup.
    pub(super) fn vacuum_am_indexes(&mut self, t: &Table) -> Result<()> {
        if self.cat.access_methods.is_empty() {
            return Ok(());
        }
        let ta = std::sync::Arc::new(t.clone());
        for bix in self.binder().bound_indexes(&ta)? {
            let Some(r) = crate::ext::index_am::routine_by_name(&self.cat, &bix.index.method) else {
                continue;
            };
            r.load(self.ctx);
            let am = AmIndex::of(&self.cat, t, &bix.index, &bix.keys, self.ctx);
            let key = bix.keys[0].clone();
            let ctx = self.ctx;
            let form = |v: &Value| r.am.entry_value(&am, v);
            let mut dead = |dkey: &[u8], v: &Value| crate::ext::index_am::entry_dead(&am.store, t, &key, &form, dkey, v, ctx);
            let stats = r.am.vacuum(&am, &mut dead, ctx)?;
            // (ambulkdelete runs only over dead tuples: an index with none removed keeps its
            // statistics, as amvacuumcleanup without a bulk delete returns none.)
            if stats.removed > 0.0 {
                let pages = r.am.blocks(&am, ctx)? as i32;
                self.cat.relstats.insert(bix.index.oid, crate::catalog::RelStats { pages, tuples: stats.index_tuples, allvisible: 0, allfrozen: 0 });
            }
        }
        Ok(())
    }

    /// The storage of each index of a native access method: (index, (storage id, relfilenode)).
    pub(super) fn am_index_storage(&self) -> Vec<(u32, (u32, u32))> {
        if self.cat.access_methods.is_empty() {
            return Vec::new();
        }
        self.cat
            .indexes
            .values()
            .filter(|ix| crate::ext::index_am::is_native(&self.cat, &ix.method))
            .map(|ix| (ix.oid, (ix.id, self.cat.relfilenode(ix.oid).unwrap_or(ix.oid))))
            .collect()
    }

    /// After a statement: the indexes of native methods it gave new storage (TRUNCATE, a
    /// table rewrite, VACUUM FULL, CLUSTER, REINDEX) are built again there, as
    /// PostgreSQL rebuilds an index in its new relfilenode.
    pub(super) fn rebuild_renewed_am_indexes(&mut self, before: &[(u32, (u32, u32))]) -> Result<()> {
        if before.is_empty() {
            return Ok(());
        }
        let after = self.am_index_storage();
        let mut renewed = false;
        for (oid, storage) in after {
            if before.iter().any(|(o, s)| *o == oid && *s != storage) {
                self.rebuild_am_index(oid)?;
                renewed = true;
            }
        }
        if renewed {
            self.save()?;
        }
        Ok(())
    }

    /// An index of a native method given new storage: built again there. (Its old records go
    /// with the transaction, so a rollback finds them.)
    pub(super) fn rebuild_am_index(&mut self, ix: u32) -> Result<()> {
        let Some(index) = self.cat.indexes.get(&ix).map(|i| (**i).clone()) else {
            return Ok(());
        };
        if crate::ext::index_am::routine_by_name(&self.cat, &index.method).is_none() {
            return Ok(());
        }
        let Some(t) = self.cat.index_table(index.table).map(|t| (*t).clone()) else {
            return Ok(());
        };
        self.delete_index_storage(&t, &index, false)?;
        self.build_index(&t, &index)
    }

    /// ambuild of index `ix` of table `t` (bound as `bix`): the method builds it over the
    /// table's rows in heap order, as PostgreSQL's build scan reads them.
    pub(super) fn build_am_index(&mut self, r: &'static AmRoutine, t: &Table, ix: &Index, bix: &BoundIndex) -> Result<()> {
        r.load(self.ctx);
        r.am.options(&ix.options)?;
        let am = AmIndex::of(&self.cat, t, ix, &bix.keys, self.ctx);
        // (A build starts from nothing: records a rolled-back build left under the same
        // relfilenode go.)
        let old: Vec<(Vec<u8>, Option<Vec<u8>>)> = am.store.load()?.into_iter().map(|(k, _)| (k, None)).collect();
        am.store.write(old)?;
        let keys = self.heap_ordered_keys(t)?;
        let blocks = crate::exec::sizing::fork_blocks(t.oid, crate::exec::sizing::Fork::Main, self.ctx)?.unwrap_or(0) as u32;
        // (Only the columns its keys, predicate and INCLUDE columns read are decoded, as the
        // built-in methods' build pass decodes them.)
        let mut cols = std::collections::HashSet::new();
        crate::exec::sizing::index_columns(bix, &mut cols);
        let need = (0..t.columns.len()).map(|i| cols.contains(&i)).collect();
        let mut heap = DdlHeap { d: self, t, bix, keys, blocks, need };
        let stats = r.am.build(&am, &mut heap, self.ctx)?;
        self.index_built(t, ix)?;
        // index_update_stats: the index's pages and the tuples ambuild made.
        let pages = r.am.blocks(&am, self.ctx)? as i32;
        self.cat.relstats.insert(ix.oid, crate::catalog::RelStats { pages, tuples: stats.index_tuples, allvisible: 0, allfrozen: 0 });
        Ok(())
    }

    /// The table's rows' data keys in heap (ctid) order with their blocks where the heap is
    /// modelled, else in storage order (blocks then spread evenly: `AmRow::block`).
    fn heap_ordered_keys(&self, t: &Table) -> Result<Vec<(Vec<u8>, Option<u32>)>> {
        let start = index_prefix(t.oid, 0);
        let end = prefix_end(&start);
        let mut keys: Vec<Vec<u8>> = Vec::new();
        let mut tids: Vec<Option<crate::catalog::heapsim::Tid>> = Vec::new();
        let mut batch: Vec<(Vec<u8>, bytes::Bytes)> = Vec::new();
        let mut modelled = true;
        let flush = |batch: &mut Vec<(Vec<u8>, bytes::Bytes)>, keys: &mut Vec<Vec<u8>>, tids: &mut Vec<_>, modelled: &mut bool| {
            if *modelled {
                match crate::exec::sizing::row_tids(t, batch, self.ctx) {
                    Some(v) => tids.extend(v),
                    None => *modelled = false,
                }
            }
            keys.extend(batch.drain(..).map(|(k, _)| k));
        };
        for e in self.ctx.txn.borrow().scan(&start, &end) {
            batch.push(e?);
            if batch.len() == 1024 {
                flush(&mut batch, &mut keys, &mut tids, &mut modelled);
            }
        }
        flush(&mut batch, &mut keys, &mut tids, &mut modelled);
        if !modelled || tids.len() != keys.len() {
            return Ok(keys.into_iter().map(|k| (k, None)).collect());
        }
        // (Rows the model doesn't place go last, in storage order.)
        let rank = |i: usize| (tids[i].is_none(), tids[i], i);
        let mut order: Vec<usize> = (0..keys.len()).collect();
        if !(1..keys.len()).all(|i| rank(i - 1) < rank(i)) {
            order.sort_unstable_by_key(|&i| rank(i));
        }
        let mut slots: Vec<Option<Vec<u8>>> = keys.into_iter().map(Some).collect();
        Ok(order.into_iter().filter_map(|i| Some((slots[i].take()?, tids[i].map(|t| t.0)))).collect())
    }
}

/// The table's rows for a build of index `bix` (`BuildHeap`): fetched by data key, in heap
/// order, the index's values computed as the table's owner (`table::index_values`).
struct DdlHeap<'a, 'b, 'c> {
    d: &'a Ddl<'b, 'c>,
    t: &'a Table,
    bix: &'a BoundIndex,
    keys: Vec<(Vec<u8>, Option<u32>)>,
    blocks: u32,
    /// The table's columns the index reads (`sizing::index_columns`), the rest left NULL.
    need: Vec<bool>,
}

impl crate::ext::index_am::BuildHeap for DdlHeap<'_, '_, '_> {
    fn blocks(&self) -> u32 {
        self.blocks
    }

    fn scan(&mut self) -> Box<dyn Iterator<Item = Result<AmRow>> + '_> {
        let n = self.keys.len().max(1) as u64;
        let blocks = self.blocks as u64;
        let (d, t, bix, need) = (self.d, self.t, self.bix, &self.need);
        Box::new(self.keys.iter().enumerate().filter_map(move |(i, (k, block))| {
            let r = (|| -> Result<Option<AmRow>> {
                d.ctx.check()?;
                let Some(bytes) = d.ctx.txn.borrow().get(k)? else {
                    return Ok(None);
                };
                let row: Row = crate::exec::scan::decode_table_row_need(t, &bytes, need)?;
                let Some(values) = super::table::index_values(bix, &row, d)? else {
                    return Ok(None);
                };
                let block = block.unwrap_or((i as u64 * blocks / n) as u32);
                Ok(Some(AmRow { dkey: k.clone(), block, values }))
            })();
            r.transpose()
        }))
    }
}
