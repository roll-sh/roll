//! Writes to watched tables, logged as they commit: under the commit lock, before they are
//! visible to anyone. For structures derived from a table's rows that have to see every
//! change to them (`exec::search`'s search structures of the index access methods without
//! stored entries). A structure registers under the commit lock (`Watch::register`), reads
//! the table as of a time every commit before that is visible at, and then takes the writes
//! committed since from the log (`Watch::pending`): no commit falls between the two.

use bytes::Bytes;
use parking_lot::Mutex;
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A derived structure's state (its owner's type).
pub type State = Arc<Mutex<Box<dyn Any + Send>>>;

/// Writes a table's log keeps for structures that haven't read them: past that, the table's
/// structures are dropped (and rebuilt when next needed) rather than the log growing.
const MAX_LOG: usize = 1 << 18;

#[derive(Default)]
pub struct Watch {
    /// Whether any table is watched: commits check nothing until one is.
    any: AtomicBool,
    tables: Mutex<HashMap<u32, Watched>>,
}

#[derive(Default)]
struct Watched {
    /// The writes since sequence number `base`, oldest first (a key and its new value,
    /// None: deleted).
    log: VecDeque<(Vec<u8>, Option<Bytes>)>,
    base: u64,
    /// Each structure (by index oid): its state, and the log position it has read up to.
    states: HashMap<u32, (State, u64)>,
}

/// The table whose row `key` is (a data key: the table's oid, then index 0).
fn table_of(key: &[u8]) -> Option<u32> {
    (key.len() > 8 && key[4..8] == [0; 4]).then(|| u32::from_be_bytes([key[0], key[1], key[2], key[3]]))
}

impl Watch {
    /// A transaction commits `writes` (under the commit lock, before they're visible).
    pub fn committing(&self, writes: &[(Vec<u8>, Option<Bytes>)]) {
        if !self.any.load(Ordering::Acquire) {
            return;
        }
        let mut tables = self.tables.lock();
        let mut over = Vec::new();
        for (k, v) in writes {
            if let Some(t) = table_of(k)
                && let Some(w) = tables.get_mut(&t)
            {
                w.log.push_back((k.clone(), v.clone()));
                if w.log.len() > MAX_LOG {
                    over.push(t);
                }
            }
        }
        for t in over {
            tables.remove(&t);
        }
    }

    /// A commit too big to log (a spilled transaction's) writes `key`: its table's
    /// structures are dropped.
    pub fn committing_unlogged(&self, key: &[u8]) {
        if self.any.load(Ordering::Acquire)
            && let Some(t) = table_of(key)
        {
            let mut tables = self.tables.lock();
            if tables.contains_key(&t) {
                tables.remove(&t);
            }
        }
    }

    /// Registers structure `state` of `table` (by `index`), replacing any it had: it reads
    /// the log from here on. Called under the store's commit lock.
    pub fn register(&self, table: u32, index: u32, state: State) {
        let mut tables = self.tables.lock();
        let w = tables.entry(table).or_default();
        let at = w.base + w.log.len() as u64;
        w.states.insert(index, (state, at));
        self.any.store(true, Ordering::Release);
    }

    /// The structure registered for `index` of `table`, if any.
    pub fn state(&self, table: u32, index: u32) -> Option<State> {
        Some(self.tables.lock().get(&table)?.states.get(&index)?.0.clone())
    }

    /// The writes `state` (`index`'s structure) hasn't read yet, now read. None: it isn't
    /// registered any more (dropped, or replaced by another), so it may have missed some.
    pub fn pending(&self, table: u32, index: u32, state: &State) -> Option<Vec<(Vec<u8>, Option<Bytes>)>> {
        let mut tables = self.tables.lock();
        let w = tables.get_mut(&table)?;
        let end = w.base + w.log.len() as u64;
        let (s, at) = w.states.get_mut(&index)?;
        if !Arc::ptr_eq(s, state) {
            return None;
        }
        let from = (*at - w.base) as usize;
        let out: Vec<_> = w.log.range(from..).cloned().collect();
        *at = end;
        // The log's prefix every structure has read goes.
        let low = w.states.values().map(|(_, a)| *a).min().unwrap_or(end);
        let drop = (low - w.base) as usize;
        w.log.drain(..drop);
        w.base = low;
        Some(out)
    }

    /// Drops `table`'s structures (DDL changed what they derive from).
    pub fn forget(&self, table: u32) {
        if self.any.load(Ordering::Acquire) {
            self.tables.lock().remove(&table);
        }
    }

    /// Drops the structure of `index` of `table`.
    pub fn forget_index(&self, table: u32, index: u32) {
        if self.any.load(Ordering::Acquire)
            && let Some(w) = self.tables.lock().get_mut(&table)
        {
            w.states.remove(&index);
        }
    }

    /// Memory the logs hold (bytes of keys and values).
    pub fn log_bytes(&self) -> usize {
        self.tables
            .lock()
            .values()
            .flat_map(|w| w.log.iter())
            .map(|(k, v)| k.len() + v.as_ref().map_or(0, |v| v.len()))
            .sum()
    }
}
