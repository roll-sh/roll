//! The pages of tables too big to model that updates changed, for what heap_update and an
//! on-access prune (heap_page_prune_opt) decide there: whether an update's new version stays
//! on its old one's page (a HOT update, unless an indexed column changed), goes to another
//! (n_tup_newpage_upd), and how many dead tuples a prune reclaims (n_dead_tup). One record for
//! every table of a database, of fixed size (`PAGE_SLOTS` pages, `ROW_SLOTS` rows, in sets of
//! `WAYS`): a page that isn't in it is taken as loaded (rows of the new version's size
//! up to the fillfactor), a row that isn't by where its estimate or its stored TID puts it.
//! In memory only (a restart starts it empty). Agent 331.

use super::{EstHeap, Tid};
use crate::catalog::{heapsim, relsize};
use crate::exec::fasthash::HashMap as FastMap;

/// Pages the record keeps, for all tables (48 bytes each: 768 KiB).
const PAGE_SLOTS: usize = 1 << 14;

/// Updated rows whose newest version's page the record keeps (8 bytes each: 256 KiB).
const ROW_SLOTS: usize = 1 << 15;

/// Slots a page or row may take (a set: the least recently updated page, or a row's by its
/// hash, put out for a new one).
const WAYS: usize = 4;

/// PD_PAGE_FULL, in `Page::lps`' top bit (a page has at most `MAX_HEAP_TUPLES` line pointers).
const FULL: u16 = 1 << 15;

/// A page an update changed.
#[derive(Clone, Copy, Default)]
struct Page {
    /// Its table (0: the slot is empty) and block.
    table: u32,
    page: u32,
    /// pd_upper - pd_lower.
    free: u16,
    /// Its line pointers (and PD_PAGE_FULL: `FULL`), the unused ones among them, and the
    /// LP_DEAD ones a prune left (a VACUUM frees them).
    lps: u16,
    unused: u16,
    lp_dead: u16,
    /// The versions updates left dead here before the last one, not pruned yet (`Dead`), and
    /// the last one's (a prune removes each group once its updaters are past every snapshot).
    older: Dead,
    last: Dead,
}

/// Dead versions of a page, by what a prune leaves of their line pointers (`Pruned`), their
/// bytes, and the newest of their updaters' xids.
#[derive(Clone, Copy, Default)]
struct Dead {
    redirect: u8,
    only: u8,
    dead: u8,
    chain: u8,
    bytes: u16,
    newest: u64,
}

impl Dead {
    fn any(&self) -> bool {
        (self.redirect | self.only | self.dead | self.chain) != 0
    }

    fn add(&mut self, d: &Dead) {
        self.redirect = self.redirect.saturating_add(d.redirect);
        self.only = self.only.saturating_add(d.only);
        self.dead = self.dead.saturating_add(d.dead);
        self.chain = self.chain.saturating_add(d.chain);
        self.bytes = self.bytes.saturating_add(d.bytes);
        self.newest = self.newest.max(d.newest);
    }
}

impl Page {
    fn items(&self) -> u16 {
        self.lps & !FULL
    }

    fn full(&self) -> bool {
        self.lps & FULL != 0
    }

    /// The dead versions a prune at `horizon` removes, taken out of the page.
    fn prunable(&mut self, horizon: u64) -> Dead {
        let mut out = Dead::default();
        for d in [&mut self.older, &mut self.last] {
            if d.any() && d.newest < horizon {
                out.add(d);
                *d = Dead::default();
            }
        }
        out
    }
}

/// What a prune leaves of a dead version's line pointer (`Page`), and what pg_stat counts
/// reclaimed (heap_page_prune_opt: deleted tuples less the new LP_DEAD items).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pruned {
    /// A HOT chain's root, the chain going on: a redirect (reclaimed).
    Redirect,
    /// A heap-only member, the chain going on: unused (reclaimed).
    Unused,
    /// A version moved off the page or whose indexed columns changed: LP_DEAD (not reclaimed:
    /// pg_stat counts it dead until a VACUUM).
    Dead,
    /// The heap-only newest version of a chain the row moved off the page from: unused, and
    /// the chain's root redirect LP_DEAD (one deleted, one new LP_DEAD: not reclaimed).
    Chain,
}

/// The database's record (`Models::pages`).
#[derive(Default)]
pub(in crate::exec) struct Record {
    /// In sets by (table, page) hashed; allocated by the first update past the model.
    pages: Vec<Page>,
    /// In sets by row (`key_hash`): its hash's high half (never 0), then the page of the row's newest
    /// version, the top bit set when that version is heap-only (a HOT update's).
    rows: Vec<u64>,
    /// By table: the kept pages its last VACUUM left room on, in page order, and the next one
    /// to look at (the free space map an update's new version finds a page in when its own
    /// page has no room).
    fsm: FastMap<u32, (Vec<u32>, usize)>,
}

/// The first slot of the set of `table`'s page `page`.
fn page_set(table: u32, page: u32) -> usize {
    let h = (u64::from(table) << 32 | u64::from(page)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (h >> 40) as usize & (PAGE_SLOTS - WAYS)
}

fn row_set(row: u64) -> usize {
    row as usize & (ROW_SLOTS - WAYS)
}

fn row_tag(row: u64) -> u64 {
    (row >> 32 | 1) << 32
}

impl Record {
    fn alloc(&mut self) {
        if self.pages.is_empty() {
            self.pages = vec![Page::default(); PAGE_SLOTS];
            self.rows = vec![0; ROW_SLOTS];
        }
    }

    /// The slot of `table`'s page `page`, when kept.
    fn page_at(&self, table: u32, page: u32) -> Option<usize> {
        let set = page_set(table, page);
        (set..set + WAYS).find(|&i| self.pages.get(i).is_some_and(|p| p.table == table && p.page == page))
    }

    /// The slot for `table`'s page `page`: its own, else an empty one of its set, else the
    /// one of the set updated least recently (its newest updater's xid the lowest), emptied.
    fn page_slot(&mut self, table: u32, page: u32) -> (usize, bool) {
        if let Some(i) = self.page_at(table, page) {
            return (i, true);
        }
        let set = page_set(table, page);
        let i = (set..set + WAYS).min_by_key(|&i| (self.pages[i].table != 0, self.pages[i].last.newest)).unwrap_or(set);
        (i, false)
    }

    /// The page and heap-only state of updated row `row`'s newest version, when kept.
    pub(super) fn row(&self, row: u64) -> Option<(u32, bool)> {
        let set = row_set(row);
        let e = *self.rows.get(set..set + WAYS)?.iter().find(|&&e| e & !0xffff_ffff == row_tag(row))?;
        Some(((e as u32) & 0x7fff_ffff, e as u32 & 0x8000_0000 != 0))
    }

    fn set_row(&mut self, row: u64, at: Option<(u32, bool)>) {
        let set = row_set(row);
        let Some(slots) = self.rows.get_mut(set..set + WAYS) else { return };
        let own = slots.iter().position(|&e| e & !0xffff_ffff == row_tag(row));
        match at {
            Some((page, only)) if page < 0x8000_0000 => {
                // (Its own slot, else an empty one, else one by its hash.)
                let i = own.or_else(|| slots.iter().position(|&e| e == 0)).unwrap_or((row >> 24) as usize % WAYS);
                slots[i] = row_tag(row) | u64::from(page) | if only { 0x8000_0000 } else { 0 };
            }
            // (Its place not known: its entry goes.)
            _ => {
                if let Some(i) = own {
                    slots[i] = 0;
                }
            }
        }
    }

    /// Updated row `row`'s new version went in at `at` (None: its place isn't known),
    /// heap-only or not.
    pub(super) fn row_moved(&mut self, row: u64, at: Option<Tid>, only: bool) {
        self.set_row(row, at.map(|t| (t.0, only)));
    }

    /// heap_update of a row of `table` whose newest version is on `page` (`row`: its key's
    /// hash), its new version `len` bytes, by transaction `xid`; `hot`: no indexed column
    /// changes; versions dead to every snapshot are those of xids below `horizon`. First
    /// heap_page_prune_opt, as the update's read of the page runs it (a page marked full, or
    /// with less than its fillfactor's room or a tenth of a page free), then the new version
    /// stays on the page when it fits. (The new version's TID when it stays, the tuples the
    /// prune reclaimed.) The page being filled and the tail keep their room and line pointers
    /// in `e`, the rest here.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn update_on(&mut self, e: &mut EstHeap, table: u32, page: u32, row: u64, len: usize, fillfactor: usize, xid: u64, horizon: u64, hot: bool) -> (Option<Tid>, u64) {
        self.alloc();
        let size = heapsim::maxalign(len);
        let need = size + heapsim::ITEM_ID;
        let reserve = heapsim::BLCKSZ * (100 - fillfactor.min(100)) / 100;
        let minfree = reserve.max(heapsim::BLCKSZ / 10);
        let fill = e.pages > 0 && page == e.fill;
        let tail = !fill && e.tail.as_ref().is_some_and(|t| t.page == page);
        let only = self.row(row).is_some_and(|(p, only)| p == page && only);
        let (i, kept) = self.page_slot(table, page);
        let p = &mut self.pages[i];
        if !kept {
            // (Not kept, or put out by another page: as loaded.)
            let n = ((heapsim::BLCKSZ - heapsim::PAGE_HEADER - reserve) / need).min(relsize::MAX_HEAP_TUPLES);
            *p = Page { table, page, free: (heapsim::BLCKSZ - heapsim::PAGE_HEADER - n * need) as u16, lps: n as u16, ..Default::default() };
        }
        let (mut free, mut lps) = if fill {
            (e.room, e.items)
        } else if let Some(t) = e.tail.as_ref().filter(|_| tail) {
            (t.room, t.items)
        } else {
            (p.free as usize, p.items())
        };
        let mut reclaimed = 0;
        let mut full = p.full();
        if full || free.saturating_sub(heapsim::ITEM_ID) < minfree {
            let d = p.prunable(horizon);
            if d.any() {
                free += d.bytes as usize;
                reclaimed = u64::from(d.redirect) + u64::from(d.only);
                p.unused += u16::from(d.only) + u16::from(d.chain);
                p.lp_dead += u16::from(d.dead) + u16::from(d.chain);
                full = false;
            }
        }
        let fits = need <= free && (p.unused > 0 || (lps as usize) < relsize::MAX_HEAP_TUPLES);
        let stays = fits && hot;
        let kind = match (only, stays) {
            (true, true) => Pruned::Unused,
            (true, false) => Pruned::Chain,
            (false, true) => Pruned::Redirect,
            (false, false) => Pruned::Dead,
        };
        let last = std::mem::take(&mut p.last);
        p.older.add(&last);
        p.last = Dead {
            redirect: u8::from(kind == Pruned::Redirect),
            only: u8::from(kind == Pruned::Unused),
            dead: u8::from(kind == Pruned::Dead),
            chain: u8::from(kind == Pruned::Chain),
            bytes: size as u16,
            newest: xid,
        };
        let tid = if fits {
            if p.unused > 0 {
                p.unused -= 1;
                free -= size;
            } else {
                free -= need;
                lps += 1;
            }
            Some((page, lps))
        } else {
            full = true;
            None
        };
        if fill {
            (e.room, e.items) = (free, lps);
        } else if let Some(t) = e.tail.as_mut().filter(|_| tail) {
            (t.room, t.items) = (free, lps);
        }
        p.free = free.min(u16::MAX as usize) as u16;
        p.lps = lps | if full { FULL } else { 0 };
        (tid, reclaimed)
    }

    /// A page the free space map gives a new version of `len` bytes (fillfactor
    /// `fillfactor`) of `table` that its old one's page (`not`) had no room for: the next
    /// kept page its last VACUUM left room on that has room for it, as
    /// RelationGetBufferForTuple finds one (GetPageWithFreeSpace). Its TID there.
    pub(super) fn fsm_page(&mut self, e: &EstHeap, table: u32, len: usize, fillfactor: usize, not: u32) -> Option<Tid> {
        let (pages, at) = self.fsm.get_mut(&table)?;
        let size = heapsim::maxalign(len);
        let need = size + heapsim::ITEM_ID;
        let reserve = heapsim::BLCKSZ * (100 - fillfactor.min(100)) / 100;
        while let Some(&page) = pages.get(*at) {
            let set = page_set(table, page);
            let Some(p) = self.pages[set..set + WAYS].iter_mut().find(|p| p.table == table && p.page == page) else {
                *at += 1;
                continue;
            };
            if page != not
                && (page != e.fill || e.pages == 0)
                && e.tail.as_ref().is_none_or(|t| t.page != page)
                && need + reserve <= p.free as usize
                && (p.unused > 0 || (p.items() as usize) < relsize::MAX_HEAP_TUPLES)
            {
                if p.unused > 0 {
                    p.unused -= 1;
                    p.free -= size as u16;
                } else {
                    p.free -= need as u16;
                    p.lps += 1;
                }
                return Some((page, p.items()));
            }
            *at += 1;
        }
        None
    }

    /// A lazy VACUUM of `table`: the dead versions and line pointers it removes on its kept
    /// pages (versions of xids below `horizon`; their line pointers unused, a HOT chain root's
    /// left a redirect, their room free), and those pages with room as its free space map.
    pub(super) fn vacuumed(&mut self, table: u32, horizon: u64) {
        if self.pages.is_empty() {
            return;
        }
        let mut fsm = Vec::new();
        for p in self.pages.iter_mut().filter(|p| p.table == table) {
            let d = p.prunable(horizon);
            p.free += d.bytes;
            p.unused += u16::from(d.only) + u16::from(d.dead) + 2 * u16::from(d.chain);
            p.unused += std::mem::take(&mut p.lp_dead);
            p.lps &= !FULL;
            if p.free as usize > heapsim::ITEM_ID {
                fsm.push(p.page);
            }
        }
        fsm.sort_unstable();
        if fsm.is_empty() {
            self.fsm.remove(&table);
        } else {
            self.fsm.insert(table, (fsm, 0));
        }
    }

    /// `table`'s pages are new files (a rewrite, TRUNCATE, DROP) or modelled again: nothing
    /// kept of them, nor of any row (the rows' entries don't say their table).
    pub(super) fn forget(&mut self, table: u32) {
        self.fsm.remove(&table);
        let mut had = false;
        for p in self.pages.iter_mut().filter(|p| p.table == table) {
            *p = Page::default();
            had = true;
        }
        if had {
            self.rows.fill(0);
        }
    }
}
