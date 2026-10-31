//! GiST point_ops / range_ops indexes' history (`RelFiles::gist_logs`): the entries the
//! index's build wrote, then every entry inserted since and every VACUUM's removals, in
//! the order they happened. The index's model replays it (`replay`), so the dead entries
//! PostgreSQL's tree keeps until VACUUM split its pages as they did there, and VACUUM
//! empties and deletes pages as gistbulkdelete does: the model's page count and its scans'
//! order follow updates and deletes.
//!
//! A history is kept only while it accounts for the table: every row a snapshot sees must
//! have its latest entry there, with the key it has now, or the model is built from the
//! rows instead (and the history started again from that build).

use crate::catalog::gistsim::{Gist, Key, sorted_build_rows};
use crate::catalog::heapsim::Tid;
use std::collections::{HashMap, HashSet};

/// Entries a history may hold (about 50 bytes each); past it, the index's model is built
/// from its rows.
pub const MAX_ENTRIES: usize = 1 << 20;

#[derive(Clone, Default)]
pub struct GistLog {
    /// The build's entries, in heap (TID) order.
    build: Vec<(Tid, Option<Key>)>,
    ops: Vec<Op>,
    entries: usize,
    vacuums: usize,
}

#[derive(Clone)]
enum Op {
    /// gistdoinsert of an entry for the heap tuple at this TID.
    Insert(Tid, Option<Key>),
    /// gistbulkdelete of the entries of these heap tuples.
    Vacuum(Vec<Tid>),
}

impl GistLog {
    /// A history starting with a build of these entries (in TID order), then these
    /// insertions.
    pub fn new(build: Vec<(Tid, Option<Key>)>, inserted: Vec<(Tid, Option<Key>)>) -> GistLog {
        let entries = build.len() + inserted.len();
        GistLog { build, ops: inserted.into_iter().map(|(t, k)| Op::Insert(t, k)).collect(), entries, vacuums: 0 }
    }

    /// An entry was inserted for the heap tuple at `tid` (false: the history is full).
    pub fn insert(&mut self, tid: Tid, key: Option<Key>) -> bool {
        self.entries += 1;
        self.ops.push(Op::Insert(tid, key));
        self.entries <= MAX_ENTRIES
    }

    /// VACUUM removed the entries of these heap tuples.
    pub fn vacuum(&mut self, dead: &[Tid]) {
        self.vacuums += 1;
        self.ops.push(Op::Vacuum(dead.to_vec()));
    }

    /// Its entries and VACUUMs: a model built before it grew is stale (an aborted
    /// transaction's entries, or a VACUUM, change the index without a commit).
    pub fn version(&self) -> usize {
        self.entries + self.vacuums
    }

    /// The index as the history built it, with the data key of each live entry's row (by
    /// entry, empty for one whose tuple is dead or another version than the one `rows`
    /// has), when every row of `rows` ((the TID index entries name, the HOT chain's root),
    /// data key, key) has its latest entry here with the same key. None: it doesn't.
    pub fn replay(&self, rows: &[(Tid, Vec<u8>, Option<Key>)]) -> Option<(Gist, Vec<Vec<u8>>)> {
        // Each TID's latest entry not removed by a VACUUM (a TID is reused only after one).
        let mut tids: Vec<Tid> = Vec::with_capacity(self.entries);
        let mut keys: Vec<Option<Key>> = Vec::with_capacity(self.entries);
        let mut latest: HashMap<Tid, u32> = HashMap::with_capacity(rows.len());
        for (t, k) in self.build.iter().map(|(t, k)| (t, k)).chain(self.ops.iter().filter_map(|o| match o {
            Op::Insert(t, k) => Some((t, k)),
            Op::Vacuum(_) => None,
        })) {
            tids.push(*t);
            keys.push(*k);
        }
        let mut e = 0u32;
        for t in &tids[..self.build.len()] {
            latest.insert(*t, e);
            e += 1;
        }
        for o in &self.ops {
            match o {
                Op::Insert(t, _) => {
                    latest.insert(*t, e);
                    e += 1;
                }
                Op::Vacuum(dead) => dead.iter().for_each(|t| {
                    latest.remove(t);
                }),
            }
        }
        let mut data = vec![Vec::new(); tids.len()];
        for (t, dkey, key) in rows {
            let e = *latest.get(t)? as usize;
            if keys[e] != *key {
                return None;
            }
            data[e] = dkey.clone();
        }
        let mut g = sorted_build_rows(self.build.iter().enumerate().map(|(i, (_, k))| (*k, i as u32)).collect());
        let mut e = self.build.len();
        for o in &self.ops {
            match o {
                Op::Insert(_, k) => {
                    g.insert_row(*k, e as u32);
                    e += 1;
                }
                Op::Vacuum(dead) => {
                    let dead: HashSet<Tid> = dead.iter().copied().collect();
                    let n = e;
                    g.vacuum(&|r| (r as usize) < n && dead.contains(&tids[r as usize]));
                }
            }
        }
        Some((g, data))
    }
}
