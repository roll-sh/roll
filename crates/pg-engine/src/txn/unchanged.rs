//! Tables nothing changed since their last measure: VACUUM's skip (PostgreSQL's
//! visibility map lets it skip all-visible pages; COPY FREEZE leaves every page so).
//!
//! A table too big to model is measured by reading every row (`Ddl::measure_rows_with`):
//! its rows, the live pages they pack into and the oldest xmin past the frozen one. What a
//! measure found is kept here with the commit ts it holds as of: VACUUM's own full measure
//! (as of its snapshot), or a load into a table emptied in the same transaction (TRUNCATE,
//! then COPY: as of the commit, `Txn::note_load`). The store then watches the table
//! (`Store::watch_untouched`: no version committed in it between that ts and the watch's,
//! `Store::changed_since`, and every commit after drops the watch if it writes the table). A
//! later VACUUM of a table still watched takes the numbers instead of reading the rows: the
//! same pg_class and pg_stat numbers a full measure gives.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

thread_local! {
    /// VACUUMs on this thread that took a kept measure instead of reading the table.
    static SKIPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// VACUUMs on this thread that took a kept measure instead of reading the table, for tests
/// (other tests' VACUUMs, on threads of their own, don't count).
pub fn skips() -> usize {
    SKIPS.with(std::cell::Cell::get)
}

pub(crate) fn note_skip() {
    SKIPS.with(|c| c.set(c.get() + 1));
}

/// VACUUMs that took kept xmins instead of reading the table for its frozen xid, for tests.
pub static XMIN_SKIPS: AtomicUsize = AtomicUsize::new(0);

/// A table's measure, as of commit ts `ts`.
#[derive(Clone, Debug)]
pub struct Measured {
    pub ts: u64,
    /// The store's watch of the table (`Store::watch_untouched`).
    pub armed: u64,
    /// The column types it was measured with (a stored row's size depends on them).
    pub layout: u64,
    pub rows: usize,
    /// The live rows' heap pages (`relsize::heap_pages` of their stored sizes).
    pub live: i32,
    /// The smallest and largest xmin of a row (u64::MAX, 0: none).
    pub xmin: (u64, u64),
    /// A row may have values out of line (one past TOAST_TUPLE_THRESHOLD, or sized from
    /// its values): the TOAST table's oldest xmin isn't known without reading the rows.
    pub may_toast: bool,
}

impl Measured {
    /// The oldest xmin at or past `frozen` (u64::MAX: none), as the measure's pass finds it;
    /// None: not known from the smallest and largest.
    pub fn oldest_xmin(&self, frozen: u64) -> Option<u64> {
        if self.rows == 0 || frozen > self.xmin.1 {
            Some(u64::MAX)
        } else if frozen <= self.xmin.0 {
            Some(self.xmin.0)
        } else {
            None
        }
    }
}

#[derive(Default)]
pub struct Unchanged {
    tables: Mutex<HashMap<u32, Arc<Measured>>>,
}

impl Unchanged {
    /// Keep `m` (as of `m.ts`) while the store finds nothing written in `table` since.
    pub fn keep(&self, store: &crate::storage::Store, table: u32, mut m: Measured) {
        let start = crate::storage::keys::index_prefix(table, 0);
        let end = crate::storage::keys::prefix_end(&start);
        if let Some(armed) = store.watch_untouched(table, |_| !store.changed_since(&start, &end, m.ts)) {
            m.armed = armed;
            self.put(table, m);
        }
    }

    /// The measure kept of `table`, while the store's watch of it holds.
    pub fn valid(&self, store: &crate::storage::Store, table: u32) -> Option<Arc<Measured>> {
        let m = self.get(table)?;
        if store.untouched(table) == Some(m.armed) {
            return Some(m);
        }
        self.forget(table);
        None
    }

    fn put(&self, table: u32, m: Measured) {
        let mut t = self.tables.lock();
        // (An older measure doesn't replace a newer one.)
        if t.get(&table).is_none_or(|o| o.ts <= m.ts) {
            t.insert(table, Arc::new(m));
        }
    }

    fn get(&self, table: u32) -> Option<Arc<Measured>> {
        self.tables.lock().get(&table).cloned()
    }

    pub fn forget(&self, table: u32) {
        self.tables.lock().remove(&table);
    }
}

/// A table's column types as its measure sizes stored rows by them.
pub fn layout_of(types: &[crate::types::DataType], virt: &[bool]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{types:?}").hash(&mut h);
    virt.hash(&mut h);
    h.finish()
}

/// A load's measure as its rows go in (`Txn::note_load`), as `Ddl::measure_rows_with` would
/// find them: each row's stored size, in the order they went in, and its xmin.
pub struct Load {
    layout: u64,
    rows: usize,
    pages: crate::catalog::relsize::HeapPacker,
    xmin: (u64, u64),
    may_toast: bool,
    /// Every row's size was known from its stored bytes (else the measure sizes it from
    /// its values: none is kept).
    sized: bool,
}

impl Load {
    pub fn new(layout: u64) -> Load {
        Load { layout, rows: 0, pages: Default::default(), xmin: (u64::MAX, 0), may_toast: false, sized: true }
    }

    /// A row stored as `stored` (the stored tuple's length, None: not known from its bytes)
    /// with xmin `xmin`.
    pub fn row(&mut self, stored: Option<usize>, xmin: u64) {
        self.rows += 1;
        self.xmin = (self.xmin.0.min(xmin), self.xmin.1.max(xmin));
        match stored {
            Some(len) => {
                self.may_toast |= len > crate::exec::sizing::TOAST_THRESHOLD;
                self.pages.push(crate::catalog::relsize::heap_tuple_of_len(len));
            }
            None => self.sized = false,
        }
    }

    pub fn sized(&self) -> bool {
        self.sized
    }

    /// One of its rows has a value out of line.
    pub fn row_out_of_line(&mut self) {
        self.may_toast = true;
    }

    pub(super) fn measured(&self, ts: u64) -> Measured {
        Measured { ts, armed: 0, layout: self.layout, rows: self.rows, live: self.pages.pages(), xmin: self.xmin, may_toast: self.may_toast }
    }
}
