//! What one ES|QL query holds in memory, charged to the circuit breakers as it is built.
//!
//! Elasticsearch charges every block of a query's pages to the `request` breaker through its
//! `BlockFactory` (aggregation hashes and states through `BigArrays`, as `<reused_arrays>`), and
//! releases each when the block is closed; past the limit the query fails with a
//! `circuit_breaking_exception` (429). roll does the same per structure: a [`Memory`] is the
//! query's account (one per request, [`Memory::for_query`]), and each page of rows, group table,
//! aggregate state, sort buffer, join table and result table holds a [`Held`] sized from what it
//! allocated (vector capacities times element sizes, string capacities, never a factor of the
//! bytes read), grown as the structure grows and released when it drops. Whatever is still held
//! when the account drops (the result table, kept until the response is built) is released then,
//! so a query that fails or is cancelled gives everything back.
//!
//! Each charge goes to the cluster's `request` breaker, whose limit a tenant sets
//! (`indices.breaker.request.limit`), and to the node's budget ([`crate::ClusterConfig::request_memory`]),
//! which only the operator sets: either one refuses a charge past its limit.

use super::value::{Acc, V};
use crate::error::EsResult;
use crate::json::Value;
use search_aggs::memory::MemoryBudget;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Elasticsearch's label for what ES|QL's operators allocate (`BigArrays`).
pub const REUSED_ARRAYS: &str = "<reused_arrays>";
/// Elasticsearch's label for pages of values read from the source (`ValuesSourceReaderOperator`).
pub const LOAD_BLOCKS: &str = "load blocks";

/// How often a structure that grows row by row settles its charge: every this many bytes.
const SETTLE_BYTES: usize = 64 * 1024;

/// One query's memory account (cheap to clone: every clone is the same account).
#[derive(Clone, Default)]
pub struct Memory(Option<Arc<Account>>);

struct Account {
    request: Arc<MemoryBudget>,
    node: Option<Arc<MemoryBudget>>,
    /// Everything charged through this account and not yet released.
    held: AtomicU64,
}

impl Drop for Account {
    fn drop(&mut self) {
        let left = *self.held.get_mut();
        self.request.release(left);
        if let Some(n) = &self.node {
            n.release(left);
        }
    }
}

impl Memory {
    /// An account charging `request` and, when there is one, the node's budget.
    pub fn new(request: Arc<MemoryBudget>, node: Option<Arc<MemoryBudget>>) -> Memory {
        Memory(Some(Arc::new(Account { request, node, held: AtomicU64::new(0) })))
    }

    /// The account of a query on `cluster`: its `request` breaker (limit brought up to date)
    /// and the node's budget.
    pub fn for_query(cluster: &crate::cluster::Cluster) -> Memory {
        let st = cluster.state();
        Memory::new(cluster.breakers.request(cluster, &st), cluster.breakers.node().cloned())
    }

    /// Bytes this account holds now.
    pub fn used(&self) -> u64 {
        self.0.as_ref().map_or(0, |a| a.held.load(Ordering::Relaxed))
    }

    fn reserve(&self, bytes: u64, label: &str) -> EsResult<()> {
        let Some(a) = &self.0 else { return Ok(()) };
        if bytes == 0 {
            return Ok(());
        }
        a.request.reserve(bytes, label).map_err(crate::search::aggs::agg_error)?;
        if let Some(n) = &a.node
            && let Err(e) = n.reserve(bytes, label)
        {
            a.request.release(bytes);
            return Err(crate::search::aggs::agg_error(e));
        }
        a.held.fetch_add(bytes, Ordering::Relaxed);
        Ok(())
    }

    fn release(&self, bytes: u64) {
        let Some(a) = &self.0 else { return };
        if bytes == 0 {
            return;
        }
        a.held.fetch_sub(bytes, Ordering::Relaxed);
        a.request.release(bytes);
        if let Some(n) = &a.node {
            n.release(bytes);
        }
    }

    /// A structure of `bytes`, charged under `label` until the [`Held`] drops.
    pub fn hold(&self, bytes: usize, label: &'static str) -> EsResult<Held> {
        let mut h = Held { mem: self.clone(), bytes: 0, label };
        h.set(bytes)?;
        Ok(h)
    }
}

/// One structure's charge, released when dropped.
pub struct Held {
    mem: Memory,
    bytes: u64,
    label: &'static str,
}

impl Held {
    /// Brings the charge to `total` bytes: more is charged (and may break), less is released.
    pub fn set(&mut self, total: usize) -> EsResult<()> {
        let total = total as u64;
        if total > self.bytes {
            self.mem.reserve(total - self.bytes, self.label)?;
        } else {
            self.mem.release(self.bytes - total);
        }
        self.bytes = total;
        Ok(())
    }

    /// Charges `more` bytes.
    pub fn grow(&mut self, more: usize) -> EsResult<()> {
        self.set(self.bytes as usize + more)
    }

    /// Leaves the charge on the account until the query ends (a result handed up and out).
    pub fn keep(mut self) {
        self.bytes = 0;
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.mem.release(self.bytes);
    }
}

/// A charge that grows in small steps (a group table, a join's lookup map), settled with the
/// budgets every [`SETTLE_BYTES`] so the hot loop only adds.
pub struct Tally {
    held: Held,
    pending: usize,
}

impl Tally {
    pub fn new(mem: &Memory) -> EsResult<Tally> {
        Ok(Tally { held: mem.hold(0, REUSED_ARRAYS)?, pending: 0 })
    }

    /// Adds `bytes`, charged at the next [`Tally::check`] or [`Tally::settle`].
    #[inline]
    pub fn note(&mut self, bytes: usize) {
        self.pending += bytes;
    }

    /// Charges what was added once enough has piled up.
    #[inline]
    pub fn check(&mut self) -> EsResult<()> {
        if self.pending >= SETTLE_BYTES {
            self.settle()?;
        }
        Ok(())
    }

    #[inline]
    pub fn add(&mut self, bytes: usize) -> EsResult<()> {
        self.note(bytes);
        self.check()
    }

    /// Charges what was added since the last settling.
    pub fn settle(&mut self) -> EsResult<()> {
        self.held.grow(std::mem::take(&mut self.pending))
    }

    /// Settles, then leaves the charge on the account until the query ends.
    pub fn keep(mut self) -> EsResult<()> {
        self.settle()?;
        self.held.keep();
        Ok(())
    }
}

/// Rows being built one at a time (a join's or an expansion's output, an `INLINE STATS`
/// table): [`Growth::track`] after each push charges what the new rows hold.
pub struct Growth {
    tally: Tally,
    counted: usize,
}

impl Growth {
    pub fn new(mem: &Memory) -> EsResult<Growth> {
        Ok(Growth { tally: Tally::new(mem)?, counted: 0 })
    }

    /// Counts the rows of `out` added since the last call.
    #[inline]
    pub fn track(&mut self, out: &[Vec<V>]) -> EsResult<()> {
        if out.len() > self.counted {
            let bytes = out[self.counted..].iter().map(row_bytes).sum::<usize>();
            self.counted = out.len();
            self.tally.add(bytes)?;
        }
        Ok(())
    }

    /// Leaves what the rows hold on the account until the query ends.
    pub fn keep(self) -> EsResult<()> {
        self.tally.keep()
    }
}

/// What a value holds outside the row's slot for it.
#[inline]
pub fn value_heap(v: &V) -> usize {
    match v {
        V::Str(s) | V::Ip(s) => s.capacity(),
        V::Multi(vs) => vs.capacity() * size_of::<V>() + vs.iter().map(value_heap).sum::<usize>(),
        _ => 0,
    }
}

/// What a row holds: its vector and its values.
#[inline]
pub fn row_bytes(r: &Vec<V>) -> usize {
    size_of::<Vec<V>>() + r.capacity() * size_of::<V>() + r.iter().map(value_heap).sum::<usize>()
}

/// What a page or table of rows holds.
pub fn rows_bytes(rows: &Vec<Vec<V>>) -> usize {
    (rows.capacity() - rows.len()) * size_of::<Vec<V>>() + rows.iter().map(row_bytes).sum::<usize>()
}

/// What an aggregate state holds outside its slot; O(1), so it can be read around every value
/// added (a collecting state's values are counted as they are added, [`Acc`] by [`value_heap`]).
#[inline]
pub fn acc_heap(a: &Acc) -> usize {
    match a {
        Acc::Collect(all) => all.capacity() * size_of::<V>(),
        Acc::Points(points) => points.capacity() * size_of::<(i64, V)>(),
        Acc::Distinct { hll, .. } => size_of::<search_aggs::metrics::cardinality::Hll>() + hll.heap_bytes(),
        Acc::MinMax(Some(v)) => value_heap(v),
        _ => 0,
    }
}

/// Whether adding to `a` can grow it.
#[inline]
pub fn acc_grows(a: &Acc) -> bool {
    matches!(a, Acc::Collect(_) | Acc::Points(_) | Acc::Distinct { .. } | Acc::MinMax(_))
}

/// What a JSON tree holds (the response's values).
pub fn json_bytes(v: &Value) -> usize {
    size_of::<Value>()
        + match v {
            Value::Str(s) => s.capacity(),
            Value::Arr(a) => (a.capacity().saturating_sub(a.len())) * size_of::<Value>() + a.iter().map(json_bytes).sum::<usize>(),
            // An index map: its entries (key, value, hash) and its index table.
            Value::Obj(m) => m.capacity() * (size_of::<String>() + size_of::<u64>() + size_of::<usize>()) + m.iter().map(|(k, v)| k.capacity() + json_bytes(v)).sum::<usize>(),
            _ => 0,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_budgets_and_release() {
        let request = MemoryBudget::new(1 << 20);
        let node = MemoryBudget::new(1000);
        let mem = Memory::new(request.clone(), Some(node.clone()));
        let mut h = mem.hold(600, REUSED_ARRAYS).unwrap();
        let e = h.grow(600).unwrap_err();
        assert_eq!((e.status, &*e.kind), (429, "circuit_breaking_exception"));
        assert_eq!((request.used(), node.used()), (600, 600), "a refused charge leaves neither budget holding it");
        mem.hold(100, REUSED_ARRAYS).unwrap().keep();
        drop(h);
        assert_eq!((request.used(), node.used()), (100, 100));
        drop(mem);
        assert_eq!((request.used(), node.used()), (0, 0), "what was kept goes back when the query ends");
    }
}
