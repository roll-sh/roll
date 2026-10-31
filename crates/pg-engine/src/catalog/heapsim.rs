//! A model of a table's heap file as Postgres lays it out, kept up as rows are written so
//! that pg_relation_size(), the free space map and visibility map forks, relpages after
//! VACUUM and the planner's page counts follow Postgres: 8 kB pages of line pointers and
//! MAXALIGNed tuples, new versions on the old version's page when they fit (HOT), dead
//! versions holding their space until pruning or VACUUM reclaims it, inserts placed by the
//! relation's cached target page and the free space map, bulk loads extending the file in
//! growing batches, and VACUUM truncating only empty trailing pages.
//!
//! Tuples are tracked by the row's storage key; transaction ids are the engine's.

use crate::txn::xids::Xids;
use crate::exec::fasthash::HashMap;

pub const BLCKSZ: usize = 8192;
pub const PAGE_HEADER: usize = 24;
pub const ITEM_ID: usize = 4;
pub const MAX_HEAP_TUPLES: usize = 291;
/// MaxHeapTupleSize: a page less its header and one line pointer.
pub const MAX_HEAP_TUPLE_SIZE: usize = BLCKSZ - 32;
const FSM_NODES: usize = BLCKSZ - PAGE_HEADER - 4;
const FSM_NONLEAF: usize = BLCKSZ / 2 - 1;
pub const FSM_SLOTS: usize = FSM_NODES - FSM_NONLEAF;
const FSM_DEPTH: u32 = 3;
const VM_HEAPBLOCKS: usize = (BLCKSZ - PAGE_HEADER) * 4;
const MAX_EXTEND_BY: usize = 64;

/// The running session, as a key: sessions run on threads of their own.
fn session_key() -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    h.finish()
}

pub fn maxalign(n: usize) -> usize {
    (n + 7) & !7
}

/// A heap tuple's address: (block, offset from 1).
pub type Tid = (u32, u16);

/// A model's planner-visible size generation (`sizing::size_watch`): bumped whenever the
/// page counts the planner reads change (a heap's main, fsm and vm forks; a btree's pages
/// and height), retired when the model goes. A clone is a model of its own: a fresh one.
#[derive(Debug, Default)]
pub struct SizeGen(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl Clone for SizeGen {
    fn clone(&self) -> SizeGen {
        SizeGen::default()
    }
}

impl Drop for SizeGen {
    fn drop(&mut self) {
        self.0.store(u64::MAX, std::sync::atomic::Ordering::Release);
    }
}

impl SizeGen {
    pub fn bump(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    pub fn watch(&self) -> SizeWatch {
        SizeWatch {
            seen: self.0.load(std::sync::atomic::Ordering::Acquire),
            cell: self.0.clone(),
        }
    }
}

/// What a model's sizes were when it was taken (`SizeGen::watch`).
#[derive(Clone, Debug)]
pub struct SizeWatch {
    cell: std::sync::Arc<std::sync::atomic::AtomicU64>,
    seen: u64,
}

impl SizeWatch {
    /// Whether the relation's planner-visible sizes are still what they were (one atomic
    /// load, no lock). False after any change, and once the model is gone.
    pub fn holds(&self) -> bool {
        self.cell.load(std::sync::atomic::Ordering::Acquire) == self.seen
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Xs {
    Committed,
    Aborted,
    Running,
}

/// Transaction states as pruning and VACUUM see them: `horizon` is the oldest
/// transaction a running transaction or a snapshot still held may see as running
/// (anything that ended before it is settled for everyone: GlobalVisState).
pub struct Vis<'a> {
    xids: &'a Xids,
    pub horizon: u64,
    /// The last transaction looked up whose fate is settled (rows come in runs of one
    /// writer's).
    settled: std::cell::Cell<Option<(u64, Xs)>>,
}

impl<'a> Vis<'a> {
    pub fn new(xids: &'a Xids) -> Vis<'a> {
        Self::with_horizon(xids, xids.horizon())
    }

    /// A session's temporary relation's (GlobalVisTempRels): only its own transaction
    /// (`own`, its id if it has one) holds deletions back.
    pub fn temp(xids: &'a Xids, own: Option<u64>) -> Vis<'a> {
        Self::with_horizon(xids, xids.temp_horizon(own))
    }

    fn with_horizon(xids: &'a Xids, horizon: u64) -> Vis<'a> {
        Vis {
            xids,
            horizon,
            settled: std::cell::Cell::new(None),
        }
    }

    fn state(&self, x: u64) -> Xs {
        if x < crate::txn::xids::FIRST_NORMAL_XID {
            return Xs::Committed;
        }
        if let Some((y, s)) = self.settled.get()
            && y == x
        {
            return s;
        }
        // Ended transactions stay ended: a thread's last answers are kept (direct mapped by
        // xid), so concurrent scans don't all take the xid table's mutex for every row.
        thread_local! {
            static ENDED: std::cell::Cell<[(u64, u64, bool); 64]> = const { std::cell::Cell::new([(0, 0, false); 64]) };
        }
        let table = self.xids.uid();
        let slot = (x % 64) as usize;
        if let Some(s) = ENDED.with(|c| {
            let (t, y, aborted) = c.get()[slot];
            (t == table && y == x).then_some(if aborted { Xs::Aborted } else { Xs::Committed })
        }) {
            self.settled.set(Some((x, s)));
            return s;
        }
        let st = self.xids.status(x);
        let s = match st {
            Ok("in progress") => Xs::Running,
            Ok("aborted") => Xs::Aborted,
            _ => Xs::Committed,
        };
        if st.is_ok() && s != Xs::Running {
            self.settled.set(Some((x, s)));
            ENDED.with(|c| {
                let mut a = c.get();
                a[slot] = (table, x, s == Xs::Aborted);
                c.set(a);
            });
        }
        s
    }

    /// GlobalVisTestIsRemovableXid.
    fn removable(&self, x: u64) -> bool {
        x < self.horizon
    }
}

/// HeapTupleSatisfiesVacuum's verdict.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Htsv {
    Live,
    Dead,
    RecentlyDead,
    InsertInProgress,
    DeleteInProgress,
}

/// (32 bytes: a model holds one per row version.)
#[derive(Clone, Debug)]
pub struct Tup {
    pub xmin: u64,
    /// The deleting or updating transaction (0: none).
    pub xmax: u64,
    /// t_cid: the inserting command's id, the deleting one's, or a combo command id for
    /// both (what the cmin and cmax system columns show).
    pub cid: u32,
    /// The row it is a version of (`KeyMap`; `NO_ROW`: none).
    row: u32,
    /// The version this one replaced (followed back when this one's insert aborted): its
    /// offset on this page, 0 for none, or `FAR_PREV` for one on another page, kept in
    /// `Heap::far_prev` while this version's transaction may still abort.
    prev_off: u16,
    /// The next version's offset on this page (HOT chains).
    next: u16,
    /// t_len (the tuple takes MAXALIGN(len) on the page; at most MaxHeapTupleSize, a
    /// longer one held as u16::MAX, which no page has room for either).
    pub len: u16,
    heap_only: bool,
    hot_updated: bool,
}

/// `Tup::prev_off` of a version whose previous one is on another page.
const FAR_PREV: u16 = u16::MAX;

impl Tup {

    fn status(&self, v: &Vis) -> Htsv {
        match v.state(self.xmin) {
            Xs::Aborted => Htsv::Dead,
            Xs::Running => Htsv::InsertInProgress,
            Xs::Committed => {
                if self.xmax == 0 {
                    return Htsv::Live;
                }
                match v.state(self.xmax) {
                    Xs::Aborted => Htsv::Live,
                    Xs::Running => Htsv::DeleteInProgress,
                    Xs::Committed if v.removable(self.xmax) => Htsv::Dead,
                    Xs::Committed => Htsv::RecentlyDead,
                }
            }
        }
    }
}

/// A live version whose inserter is settled for everyone.
fn visible_to_all(t: &Tup, v: &Vis) -> bool {
    t.xmin < crate::txn::xids::FIRST_NORMAL_XID || v.removable(t.xmin)
}

#[derive(Clone, Debug)]
enum Lp {
    Unused,
    Dead,
    Redirect(u16),
    Normal(Tup),
}

impl Lp {
    fn used(&self) -> bool {
        !matches!(self, Lp::Unused)
    }
}

#[derive(Clone, Debug, Default)]
struct Page {
    lps: Vec<Lp>,
    /// Bytes of tuple storage (MAXALIGNed).
    used: usize,
    /// PD_PAGE_FULL: an UPDATE found no room here.
    full: bool,
    /// pd_prune_xid (0: nothing prunable).
    prune_xid: u64,
    all_visible: bool,
    /// PD_HAS_FREE_LINES.
    free_lps: bool,
    /// Never initialized (an all-zeroes page from a bulk extension).
    new: bool,
}

impl Page {
    fn fresh() -> Page {
        Page::default()
    }

    /// PageGetFreeSpace.
    fn free_space(&self) -> usize {
        if self.new {
            return 0;
        }
        let lower = PAGE_HEADER + ITEM_ID * self.lps.len();
        // (A page restored at its rows' stored TIDs can hold more than their re-measured sizes allow.)
        let upper = BLCKSZ.saturating_sub(self.used);
        upper.saturating_sub(lower).saturating_sub(ITEM_ID)
    }

    /// PageGetHeapFreeSpace.
    fn heap_free(&self) -> usize {
        let space = self.free_space();
        if space > 0
            && self.lps.len() >= MAX_HEAP_TUPLES
            && !(self.free_lps && self.lps.iter().any(|l| !l.used()))
        {
            return 0;
        }
        space
    }

    /// PageAddItem: the first unused line pointer if the page says it has one, else a
    /// new one at the end.
    fn add(&mut self, t: Tup) -> u16 {
        self.used += maxalign(t.len as usize);
        if self.free_lps {
            if let Some(i) = self.lps.iter().position(|l| !l.used()) {
                self.lps[i] = Lp::Normal(t);
                return i as u16 + 1;
            }
            self.free_lps = false;
        }
        self.lps.push(Lp::Normal(t));
        self.lps.len() as u16
    }

    fn tup(&self, off: u16) -> Option<&Tup> {
        // (Offset 0 is no line pointer's: a TID asked for can have it.)
        match self.lps.get((off as usize).wrapping_sub(1)) {
            Some(Lp::Normal(t)) => Some(t),
            _ => None,
        }
    }

    fn tup_mut(&mut self, off: u16) -> Option<&mut Tup> {
        match self.lps.get_mut((off as usize).wrapping_sub(1)) {
            Some(Lp::Normal(t)) => Some(t),
            _ => None,
        }
    }

    /// PageSetPrunable.
    fn set_prunable(&mut self, xid: u64) {
        if self.prune_xid == 0 || xid < self.prune_xid {
            self.prune_xid = xid;
        }
    }

    /// PageRepairFragmentation: storage is already accounted; trailing unused line
    /// pointers go and the free-line-pointer hint is refreshed.
    fn repair(&mut self) {
        while self.lps.last().is_some_and(|l| !l.used()) {
            self.lps.pop();
        }
        self.free_lps = self.lps.iter().any(|l| !l.used());
    }

    /// PageTruncateLinePointerArray (keeps at least the first line pointer).
    fn truncate_lps(&mut self) {
        while self.lps.len() > 1 && self.lps.last().is_some_and(|l| !l.used()) {
            self.lps.pop();
        }
        self.free_lps = self.lps.iter().any(|l| !l.used());
    }
}

// ── free space map ──

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FsmAddr {
    level: u32,
    page: u32,
}

const FSM_ROOT: FsmAddr = FsmAddr {
    level: FSM_DEPTH - 1,
    page: 0,
};

fn fsm_location(blk: u32) -> (FsmAddr, usize) {
    (
        FsmAddr {
            level: 0,
            page: blk / FSM_SLOTS as u32,
        },
        blk as usize % FSM_SLOTS,
    )
}

fn fsm_parent(a: FsmAddr) -> (FsmAddr, usize) {
    (
        FsmAddr {
            level: a.level + 1,
            page: a.page / FSM_SLOTS as u32,
        },
        a.page as usize % FSM_SLOTS,
    )
}

fn fsm_child(a: FsmAddr, slot: usize) -> FsmAddr {
    FsmAddr {
        level: a.level - 1,
        page: a.page * FSM_SLOTS as u32 + slot as u32,
    }
}

fn fsm_physical(a: FsmAddr) -> u32 {
    let mut leaf = a.page as u64;
    for _ in 0..a.level {
        leaf *= FSM_SLOTS as u64;
    }
    let mut pages = 0u64;
    for _ in 0..FSM_DEPTH {
        pages += leaf + 1;
        leaf /= FSM_SLOTS as u64;
    }
    (pages - a.level as u64 - 1) as u32
}

fn avail_cat(avail: usize) -> u8 {
    if avail >= MAX_HEAP_TUPLE_SIZE {
        255
    } else {
        (avail / 32).min(254) as u8
    }
}

fn needed_cat(needed: usize) -> u8 {
    if needed == 0 {
        1
    } else {
        needed.div_ceil(32).min(255) as u8
    }
}

/// One FSM page: a max-tree over its slots' free space categories.
#[derive(Clone, Debug)]
struct FsmPage {
    nodes: Vec<u8>,
    next: usize,
}

impl FsmPage {
    fn new() -> FsmPage {
        FsmPage {
            nodes: vec![0; FSM_NODES],
            next: 0,
        }
    }

    fn get(&self, slot: usize) -> u8 {
        self.nodes[FSM_NONLEAF + slot]
    }

    /// fsm_set_avail.
    fn set(&mut self, slot: usize, v: u8) -> bool {
        let mut n = FSM_NONLEAF + slot;
        let old = self.nodes[n];
        if old == v && v <= self.nodes[0] {
            return false;
        }
        self.nodes[n] = v;
        while n > 0 {
            n = (n - 1) / 2;
            let l = 2 * n + 1;
            let mut m = self.nodes[l];
            if l + 1 < FSM_NODES {
                m = m.max(self.nodes[l + 1]);
            }
            if self.nodes[n] == m {
                break;
            }
            self.nodes[n] = m;
        }
        if v > self.nodes[0] {
            self.rebuild();
        }
        true
    }

    fn rebuild(&mut self) {
        for n in (0..FSM_NONLEAF).rev() {
            let l = 2 * n + 1;
            let mut m = 0;
            if l < FSM_NODES {
                m = self.nodes[l];
            }
            if l + 1 < FSM_NODES {
                m = m.max(self.nodes[l + 1]);
            }
            self.nodes[n] = m;
        }
    }

    /// fsm_search_avail: from the next-slot hint, step right and climb until a subtree
    /// has room, then descend preferring the left.
    fn search(&mut self, min: u8, advance: bool) -> Option<usize> {
        if self.nodes[0] < min {
            return None;
        }
        let target = if self.next < FSM_SLOTS { self.next } else { 0 };
        let mut n = FSM_NONLEAF + target;
        while n > 0 {
            if self.nodes[n] >= min {
                break;
            }
            let mut r = n + 1;
            if (r + 1) & r == 0 {
                r = (r - 1) / 2;
            }
            n = (r - 1) / 2;
        }
        while n < FSM_NONLEAF {
            let l = 2 * n + 1;
            if l < FSM_NODES && self.nodes[l] >= min {
                n = l;
            } else if l + 1 < FSM_NODES && self.nodes[l + 1] >= min {
                n = l + 1;
            } else {
                self.rebuild();
                return None;
            }
        }
        let slot = n - FSM_NONLEAF;
        self.next = slot + advance as usize;
        Some(slot)
    }

    fn truncate(&mut self, nslots: usize) {
        for s in nslots..FSM_SLOTS {
            self.nodes[FSM_NONLEAF + s] = 0;
        }
        self.rebuild();
    }
}

#[derive(Clone, Debug, Default)]
struct Fsm {
    /// The fork's length in pages.
    blocks: u32,
    pages: HashMap<u32, FsmPage>,
}

impl Fsm {
    fn page(&mut self, a: FsmAddr, extend: bool) -> Option<&mut FsmPage> {
        let blk = fsm_physical(a);
        if blk >= self.blocks {
            if !extend {
                return None;
            }
            self.blocks = blk + 1;
        }
        Some(self.pages.entry(blk).or_insert_with(FsmPage::new))
    }

    fn set_and_search(&mut self, a: FsmAddr, slot: usize, v: u8, min: u8) -> Option<usize> {
        let p = self.page(a, true)?;
        p.set(slot, v);
        if min == 0 {
            return None;
        }
        p.search(min, a.level == 0)
    }

    /// fsm_search: from the root down, fixing out-of-date upper levels on the way.
    fn search(&mut self, min: u8) -> Option<u32> {
        let mut a = FSM_ROOT;
        let mut restarts = 0;
        loop {
            let (slot, max) = match self.page(a, false) {
                Some(p) => {
                    let s = p.search(min, a.level == 0);
                    (s, p.nodes[0])
                }
                None => (None, 0),
            };
            match slot {
                Some(s) if a.level == 0 => return Some(a.page * FSM_SLOTS as u32 + s as u32),
                Some(s) => a = fsm_child(a, s),
                None if a.level == FSM_ROOT.level => return None,
                None => {
                    let (parent, pslot) = fsm_parent(a);
                    self.set_and_search(parent, pslot, max, 0);
                    restarts += 1;
                    if restarts > 10000 {
                        return None;
                    }
                    a = FSM_ROOT;
                }
            }
        }
    }

    /// RecordPageWithFreeSpace.
    fn record(&mut self, blk: u32, avail: usize) {
        let (a, slot) = fsm_location(blk);
        self.set_and_search(a, slot, avail_cat(avail), 0);
    }

    /// GetRecordedFreeSpace (as a category).
    fn recorded(&mut self, blk: u32) -> u8 {
        let (a, slot) = fsm_location(blk);
        self.page(a, false).map_or(0, |p| p.get(slot))
    }

    /// FreeSpaceMapVacuumRange.
    fn vacuum_range(&mut self, start: u32, end: u32) {
        if end > start {
            let mut eof = false;
            self.vacuum_page(FSM_ROOT, start, end, &mut eof);
        }
    }

    fn vacuum_page(&mut self, a: FsmAddr, start: u32, end: u32, eof: &mut bool) -> u8 {
        if self.page(a, false).is_none() {
            *eof = true;
            return 0;
        }
        *eof = false;
        if a.level > 0 {
            let (mut fs, mut fss) = fsm_location(start);
            let (mut fe, mut fes) = fsm_location(end - 1);
            while fs.level < a.level {
                (fs, fss) = fsm_parent(fs);
                (fe, fes) = fsm_parent(fe);
            }
            let start_slot = if fs.page == a.page {
                fss as isize
            } else if fs.page > a.page {
                FSM_SLOTS as isize
            } else {
                0
            };
            let end_slot = if fe.page == a.page {
                fes as isize
            } else if fe.page > a.page {
                FSM_SLOTS as isize - 1
            } else {
                -1
            };
            let mut child_eof = false;
            let mut slot = start_slot;
            while slot <= end_slot {
                let avail = if !child_eof {
                    self.vacuum_page(fsm_child(a, slot as usize), start, end, &mut child_eof)
                } else {
                    0
                };
                let p = self.page(a, false).expect("fsm page");
                if p.get(slot as usize) != avail {
                    p.set(slot as usize, avail);
                }
                slot += 1;
            }
        }
        let p = self.page(a, false).expect("fsm page");
        p.next = 0;
        p.nodes[0]
    }

    /// FreeSpaceMapPrepareTruncateRel plus the fork's truncation.
    fn truncate(&mut self, nblocks: u32) {
        if self.blocks == 0 {
            return;
        }
        let (a, slot) = fsm_location(nblocks);
        let new_blocks = if slot > 0 {
            match self.page(a, false) {
                Some(p) => p.truncate(slot),
                None => return,
            }
            fsm_physical(a) + 1
        } else {
            let n = fsm_physical(a);
            if self.blocks <= n {
                return;
            }
            n
        };
        self.blocks = new_blocks;
        self.pages.retain(|b, _| *b < new_blocks);
    }
}

// ── the heap ──

/// A bulk insert's state (BulkInsertState): its current page, the pages its last
/// extension added that it hasn't used yet, and how far it has extended so far.
#[derive(Default, Debug)]
pub struct Bistate {
    current: Option<u32>,
    next_free: Option<u32>,
    last_free: u32,
    extended: usize,
}

/// How rows are being inserted.
#[derive(Debug)]
pub struct InsertMode {
    /// HEAP_INSERT_SKIP_FSM unset.
    pub use_fsm: bool,
    pub bistate: Option<Bistate>,
    /// HEAP_INSERT_FROZEN (REFRESH MATERIALIZED VIEW).
    pub frozen: bool,
}

impl InsertMode {
    /// A plain INSERT.
    pub fn single() -> InsertMode {
        InsertMode {
            use_fsm: true,
            bistate: None,
            frozen: false,
        }
    }

    /// CREATE TABLE AS, a table rewrite: a bulk insert that skips the free space map.
    pub fn bulk() -> InsertMode {
        InsertMode {
            use_fsm: false,
            bistate: Some(Bistate::default()),
            frozen: false,
        }
    }
}

/// What VACUUM found.
#[derive(Debug, Default, Clone)]
pub struct VacuumResult {
    pub pages: u32,
    pub live: f64,
    pub all_visible: u32,
    /// Heap tuples whose index entries VACUUM removed (their line pointers are reusable).
    pub dead: Vec<Tid>,
    /// Dead tuples it had to keep, still visible to a running transaction or a snapshot
    /// (vacrel->recently_dead_tuples).
    pub recently_dead: u32,
}

#[derive(Debug, Default)]
pub struct Heap {
    pages: Vec<Page>,
    keys: super::keymap::KeyMap,
    /// The relation's cached insertion target (smgr_targblock), per session: each
    /// backend keeps its own (sessions are told apart by their threads, a session
    /// running on one thread of its own).
    targets: HashMap<u64, u32>,
    fsm: Fsm,
    vm_blocks: u32,
    /// Heap fillfactor (percent).
    pub fillfactor: usize,
    /// Tuple versions held (normal line pointers).
    versions: usize,
    /// Versions made by updates whose transaction may still abort, by its xid: (the row's
    /// key, the new version, the key the row had before when that differs). An aborted
    /// one's row goes back to the version it replaced (`settle`) before pruning can
    /// remove the way back.
    pending: HashMap<u64, Vec<(u32, Tid, Option<u32>)>>,
    /// Pages pinned by something else while a scan reads them (an AFTER ROW trigger's
    /// tuple slot): pruning them needs a cleanup lock it can't get.
    pins: HashMap<u32, usize>,
    /// Laid out afresh from rows whose stored TIDs it doesn't keep: where it has a row, not
    /// the TID stored in the row, is its place (a scan's order).
    pub unstamped: bool,
    /// Some row's place doesn't follow its key's order: an update, an insert into free
    /// space before the last row's, rows rekeyed or laid out out of key order. Until then
    /// keys and TIDs increase together, so a sequential scan reading rows in storage order
    /// reads them in heap order (`in_key_order`).
    pub disordered: bool,
    /// The last row placed in key order: its key and TID.
    last_placed: Option<(Vec<u8>, Tid)>,
    /// Deleted rows' last versions, by TID, with their keys: once pruning or VACUUM makes
    /// the line pointer unused (reusable by another row), the key no longer names it.
    deleted: HashMap<Tid, u32>,
    /// The previous versions on other pages of versions whose transactions may still abort
    /// (`Tup::prev_off` `FAR_PREV`), until `settle` finds them ended.
    far_prev: HashMap<Tid, Tid>,
    /// Bumped when (main, fsm, vm) pages change (`size_sig`, `note_size`).
    pub sizes: SizeGen,
    size_sig: (u32, u32, u32),
}

impl Heap {
    /// A buffer pin on page `blk` taken (`true`) or dropped.
    pub fn pin(&mut self, blk: u32, on: bool) {
        let n = self.pins.entry(blk).or_default();
        if on {
            *n += 1;
        } else {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.pins.remove(&blk);
            }
        }
    }

    pub fn new(fillfactor: usize) -> Heap {
        Heap {
            fillfactor,
            ..Default::default()
        }
    }

    /// Whether rows in storage (key) order are in heap order: every row was placed after
    /// the rows of smaller keys, and the TIDs rows carry are the model's.
    pub fn in_key_order(&self) -> bool {
        !self.disordered && !self.unstamped
    }

    /// Whether its rows are in key order, whatever TIDs they were stored with: a scan that
    /// doesn't read their TIDs may read them in storage order (`in_key_order` without
    /// `unstamped`).
    pub fn keys_in_heap_order(&self) -> bool {
        !self.disordered
    }

    /// Whether some page has a prunable xid (a delete or an update set pd_prune_xid).
    pub fn any_prunable(&self) -> bool {
        self.pages.iter().any(|p| p.prune_xid != 0)
    }

    /// A row stored under `key` was placed at `tid`.
    fn placed(&mut self, key: &[u8], tid: Tid) {
        if self.disordered {
            return;
        }
        match &mut self.last_placed {
            Some((k, t)) if key > k.as_slice() && tid > *t => {
                k.clear();
                k.extend_from_slice(key);
                *t = tid;
            }
            Some(_) => {
                self.disordered = true;
                self.last_placed = None;
            }
            None => self.last_placed = Some((key.to_vec(), tid)),
        }
    }

    pub fn nblocks(&self) -> u32 {
        self.pages.len() as u32
    }

    /// Bump `sizes` if a fork's length changed since the last call.
    fn note_size(&mut self) {
        let sig = (self.pages.len() as u32, self.fsm.blocks, self.vm_blocks);
        if sig != self.size_sig {
            self.size_sig = sig;
            self.sizes.bump();
        }
    }

    pub fn fsm_blocks(&self) -> u32 {
        self.fsm.blocks
    }

    /// GetRecordedFreeSpace: the free space the FSM records for page `blk`, in bytes (its
    /// category's lower bound; 0 where it has no page).
    pub fn recorded_free(&mut self, blk: u32) -> usize {
        self.fsm.recorded(blk) as usize * 32
    }

    /// The free space of the last page (PageGetHeapFreeSpace; 0 without pages).
    pub fn last_page_free(&self) -> usize {
        self.pages.last().map_or(0, Page::heap_free)
    }

    pub fn vm_blocks(&self) -> u32 {
        self.vm_blocks
    }

    /// About the bytes the model holds (pages and their line pointers, the rows, the free
    /// space map's pages, the bookkeeping maps), for memory accounting.
    pub fn bytes(&self) -> usize {
        let lps: usize = self.pages.iter().map(|p| p.lps.capacity()).sum();
        self.pages.capacity() * size_of::<Page>()
            + lps * size_of::<Lp>()
            + self.keys.bytes()
            + self.fsm.pages.len() * (size_of::<FsmPage>() + 16)
            + (self.deleted.len() + self.far_prev.len()) * 24
            + self.pending.values().map(|l| l.capacity() * 20 + 16).sum::<usize>()
    }

    pub fn versions(&self) -> usize {
        self.versions
    }

    pub fn all_visible_pages(&self) -> u32 {
        self.pages.iter().filter(|p| p.all_visible).count() as u32
    }

    /// The visibility map as a database opened afresh finds it: its first `n` pages
    /// all-visible (the catalog's relallvisible; the map isn't kept with the model).
    pub fn seed_all_visible(&mut self, n: u32) {
        for p in self.pages.iter_mut().take(n as usize) {
            p.all_visible = true;
        }
    }

    /// Whether the visibility map marks page `blk` all-visible.
    pub fn page_all_visible(&self, blk: u32) -> bool {
        self.pages.get(blk as usize).is_some_and(|p| p.all_visible)
    }

    /// Whether the visibility map marks page `blk` all-frozen: all-visible with every
    /// tuple frozen (its inserter older than any transaction: COPY FREEZE's, frozen ones').
    pub fn page_all_frozen(&self, blk: u32) -> bool {
        self.pages.get(blk as usize).is_some_and(|p| {
            p.all_visible
                && p.lps.iter().all(|l| match l {
                    Lp::Normal(t) => t.xmin < crate::txn::xids::FIRST_NORMAL_XID,
                    _ => true,
                })
        })
    }

    /// pg_truncate_visibility_map: the map's file goes; no page is marked any more.
    pub fn truncate_vm(&mut self) {
        for p in &mut self.pages {
            p.all_visible = false;
        }
        self.vm_blocks = 0;
        self.note_size();
    }

    /// pgstat_heap's counts over every page: (live tuples, their bytes, dead tuples, their
    /// bytes, free bytes). A tuple is live as SnapshotDirty sees it (its insert committed or
    /// in progress, its delete not committed), dead otherwise; free space is
    /// PageGetExactFreeSpace's.
    pub fn tuple_stats(&self, v: &Vis) -> (u64, u64, u64, u64, u64) {
        let mut s = (0, 0, 0, 0, 0);
        for p in &self.pages {
            for l in &p.lps {
                if let Lp::Normal(t) = l {
                    match t.status(v) {
                        Htsv::Live | Htsv::InsertInProgress | Htsv::DeleteInProgress => {
                            s.0 += 1;
                            s.1 += t.len as u64;
                        }
                        Htsv::Dead | Htsv::RecentlyDead => {
                            s.2 += 1;
                            s.3 += t.len as u64;
                        }
                    }
                }
            }
            if !p.new {
                let lower = PAGE_HEADER + ITEM_ID * p.lps.len();
                s.4 += (BLCKSZ - p.used).saturating_sub(lower) as u64;
            }
        }
        s
    }

    /// A relcache invalidation: the cached insertion targets are forgotten.
    pub fn invalidate(&mut self) {
        self.targets.clear();
    }

    /// The running session's insertion target.
    fn target(&self) -> Option<u32> {
        self.targets.get(&session_key()).copied()
    }

    fn set_target(&mut self, b: u32) {
        self.targets.insert(session_key(), b);
    }

    /// RelationGetTargetPageFreeSpace.
    fn save_free(&self) -> usize {
        BLCKSZ * (100 - self.fillfactor.clamp(10, 100)) / 100
    }

    /// The current version of the row stored under `key` (one whose insert aborted
    /// leads back to the version it replaced).
    /// The version `locate` finds, and the one it replaced (an update's old version).
    pub fn locate_with_prev(&self, key: &[u8], v: &Vis) -> Option<(Tid, Option<Tid>)> {
        let tid = self.locate(key, v)?;
        let prev = self.prev_of(tid);
        Some((tid, prev))
    }

    /// The version of the row a reader sees: past versions still being written by other
    /// transactions (`mine`: the reader's own transaction ids) to the one they replace.
    pub fn locate_for(&self, key: &[u8], v: &Vis, mine: &dyn Fn(u64) -> bool) -> Option<Tid> {
        let mut tid = self.keys.get(key)?;
        loop {
            let t = self.pages.get(tid.0 as usize)?.tup(tid.1)?;
            match v.state(t.xmin) {
                Xs::Aborted => {}
                Xs::Running if !mine(t.xmin) => {}
                _ => return Some(tid),
            }
            match self.prev_of(tid) {
                Some(p) => tid = p,
                // (A row another transaction is inserting: where it is.)
                None => return Some(tid),
            }
        }
    }

    /// The version the one at `tid` replaced, if known (`Tup::prev_off`).
    fn prev_of(&self, tid: Tid) -> Option<Tid> {
        match self.tuple(tid)?.prev_off {
            0 => None,
            FAR_PREV => self.far_prev.get(&tid).copied(),
            off => Some((tid.0, off)),
        }
    }

    /// The version at `tid` replaced the one at `prev`.
    fn set_prev(&mut self, tid: Tid, prev: Option<Tid>) {
        let off = match prev {
            None => 0,
            Some(p) if p.0 == tid.0 => p.1,
            Some(p) => {
                self.far_prev.insert(tid, p);
                FAR_PREV
            }
        };
        if let Some(t) = self.pages.get_mut(tid.0 as usize).and_then(|p| p.tup_mut(tid.1)) {
            t.prev_off = off;
        }
    }

    /// The row under `key` has its current version at `tid` (`KeyMap::insert`), and that
    /// version is the row's.
    fn keyed(&mut self, key: &[u8], tid: Tid) -> u32 {
        let row = self.keys.insert(key, tid);
        if let Some(t) = self.pages.get_mut(tid.0 as usize).and_then(|p| p.tup_mut(tid.1)) {
            t.row = row;
        }
        row
    }

    /// The key of the row whose version is at `tid` (any version: an older one's row is the
    /// key it had then).
    pub fn key_at(&self, tid: Tid) -> Option<&[u8]> {
        self.keys.key(self.tuple(tid)?.row)
    }

    pub fn locate(&self, key: &[u8], v: &Vis) -> Option<Tid> {
        let mut tid = self.keys.get(key)?;
        loop {
            let t = self.pages.get(tid.0 as usize)?.tup(tid.1)?;
            if v.state(t.xmin) != Xs::Aborted {
                return Some(tid);
            }
            tid = self.prev_of(tid)?;
        }
    }

    /// The root of the HOT chain `tid` is in: the line pointer the indexes point to (a
    /// bitmap heap scan reads the TIDs they give, in that order).
    pub fn hot_root(&self, tid: Tid) -> Tid {
        let mut cur = tid;
        for _ in 0..u16::MAX {
            let Some(t) = self.tuple(cur) else { return cur };
            if !t.heap_only {
                return cur;
            }
            match self.prev_of(cur) {
                Some(p) if p.0 == cur.0 && p != cur => cur = p,
                _ => return cur,
            }
        }
        cur
    }

    /// The rows a scan of the stored data finds (inserted by a committed transaction, not
    /// deleted by one): (TID, key, tuple length), in TID order.
    pub fn committed_rows(&self, v: &Vis) -> Vec<(Tid, &[u8], u32)> {
        let mut out = Vec::with_capacity(self.keys.len());
        for (k, _, latest) in self.keys.iter() {
            // (`locate` from the latest version: past versions aborted to the one they replaced.)
            let mut tid = Some(latest);
            while let Some(at) = tid
                && let Some(t) = self.tuple(at)
                && v.state(t.xmin) == Xs::Aborted
            {
                tid = self.prev_of(at);
            }
            if let Some(tid) = tid
                && let Some(t) = self.tuple(tid)
                && matches!(t.status(v), Htsv::Live | Htsv::DeleteInProgress)
            {
                out.push((tid, k, t.len as u32));
            }
        }
        out.sort_unstable_by_key(|x| x.0);
        out
    }

    pub fn tid_of(&self, key: &[u8]) -> Option<Tid> {
        self.keys.get(key)
    }

    /// The rows whose versions are at `tids` (each version's row: `key_at`): (TID, key), in
    /// the order given; a TID of no normal tuple, or of one with no row, has none.
    pub fn keys_at(&self, tids: &[Tid]) -> Vec<(Tid, Vec<u8>)> {
        tids.iter().filter_map(|&t| Some((t, self.key_at(t)?.to_vec()))).collect()
    }

    /// The tuple at `tid`, if it is a normal one.
    pub fn tuple(&self, tid: Tid) -> Option<&Tup> {
        self.pages.get(tid.0 as usize)?.tup(tid.1)
    }

    /// Set the raw command id (t_cid) of the tuple at `tid`.
    pub fn set_raw_cid(&mut self, tid: Tid, cid: u32) {
        if let Some(t) = self.pages.get_mut(tid.0 as usize).and_then(|p| p.tup_mut(tid.1)) {
            t.cid = cid;
        }
    }

    fn vm_pin(&mut self, blk: u32) {
        let need = blk / VM_HEAPBLOCKS as u32 + 1;
        self.vm_blocks = self.vm_blocks.max(need);
        self.note_size();
    }

    /// Read a page for writing: a never-initialized one is initialized.
    fn init_page(&mut self, blk: u32) {
        let p = &mut self.pages[blk as usize];
        if p.new {
            *p = Page::fresh();
        }
    }

    /// RelationAddBlocks.
    fn add_blocks(&mut self, bi: Option<&mut Bistate>, num_pages: usize, use_fsm: bool) -> u32 {
        let extend_by = if bi.is_none() && !use_fsm {
            1
        } else {
            let mut e = num_pages;
            if let Some(b) = &bi {
                e = e.max(b.extended);
            }
            e.min(MAX_EXTEND_BY)
        };
        let not_in_fsm = if num_pages > 1 && bi.is_none() {
            1
        } else {
            num_pages
        };
        let first = self.pages.len() as u32;
        self.pages.push(Page::fresh());
        for _ in 1..extend_by {
            self.pages.push(Page {
                new: true,
                ..Page::fresh()
            });
        }
        let last = first + extend_by as u32 - 1;
        if use_fsm {
            for i in not_in_fsm.max(1)..extend_by {
                self.fsm
                    .record(first + i as u32, BLCKSZ - PAGE_HEADER);
            }
            if not_in_fsm < extend_by {
                self.fsm.vacuum_range(first + not_in_fsm as u32, last + 1);
            }
        }
        if let Some(b) = bi {
            if extend_by > 1 {
                b.next_free = Some(first + 1);
                b.last_free = last;
            } else {
                b.next_free = None;
            }
            b.current = Some(first);
            b.extended += extend_by;
        }
        first
    }

    /// RelationGetBufferForTuple: the page a tuple of `len` bytes goes on.
    fn get_buffer(
        &mut self,
        len: usize,
        other: Option<u32>,
        use_fsm: bool,
        mut bi: Option<&mut Bistate>,
        num_pages: usize,
    ) -> u32 {
        let len = maxalign(len);
        let num_pages = num_pages.max(1);
        let save = self.save_free();
        let nearly_empty = MAX_HEAP_TUPLE_SIZE - (MAX_HEAP_TUPLES / 8 * ITEM_ID);
        let target_free = if len + save > nearly_empty {
            len.max(nearly_empty)
        } else {
            len + save
        };
        let _ = other;
        let mut target = match bi.as_deref() {
            Some(b) if b.current.is_some() => b.current,
            _ => self.target(),
        };
        if target.is_none() && use_fsm {
            target = self.fsm.search(needed_cat(target_free));
            target = target.filter(|&b| (b as usize) < self.pages.len());
        }
        if target.is_none() && !self.pages.is_empty() {
            target = Some(self.pages.len() as u32 - 1);
        }
        while let Some(b) = target {
            if b as usize >= self.pages.len() {
                break;
            }
            self.init_page(b);
            if let Some(bs) = bi.as_deref_mut() {
                bs.current = Some(b);
            }
            let free = self.pages[b as usize].heap_free();
            if target_free <= free {
                self.set_target(b);
                self.note_size();
                return b;
            }
            if let Some(bs) = bi.as_deref_mut().filter(|bs| bs.next_free.is_some()) {
                if use_fsm {
                    self.fsm.record(b, free);
                }
                let nf = bs.next_free.unwrap();
                target = Some(nf);
                if nf >= bs.last_free {
                    bs.next_free = None;
                } else {
                    bs.next_free = Some(nf + 1);
                }
            } else if !use_fsm {
                break;
            } else {
                // RecordAndGetPageWithFreeSpace.
                let (a, slot) = fsm_location(b);
                let found = self
                    .fsm
                    .set_and_search(a, slot, avail_cat(free), needed_cat(target_free))
                    .map(|s| a.page * FSM_SLOTS as u32 + s as u32)
                    .filter(|&n| (n as usize) < self.pages.len());
                target = match found {
                    Some(n) => Some(n),
                    None => self
                        .fsm
                        .search(needed_cat(target_free))
                        .filter(|&n| (n as usize) < self.pages.len()),
                };
            }
        }
        let b = self.add_blocks(bi, num_pages, use_fsm);
        self.set_target(b);
        self.note_size();
        b
    }

    /// Place a new tuple version on page `blk`.
    fn put(&mut self, blk: u32, t: Tup, frozen: bool) -> Tid {
        let p = &mut self.pages[blk as usize];
        if frozen && p.lps.is_empty() {
            p.all_visible = true;
        } else if p.all_visible && !frozen {
            p.all_visible = false;
        }
        let off = p.add(t);
        if frozen {
            self.vm_pin(blk);
        }
        self.versions += 1;
        (blk, off)
    }

    fn new_tup(len: usize, xmin: u64) -> Tup {
        Tup {
            len: len.min(u16::MAX as usize) as u16,
            xmin,
            xmax: 0,
            heap_only: false,
            hot_updated: false,
            next: 0,
            row: super::keymap::NO_ROW,
            prev_off: 0,
            cid: 0,
        }
    }

    /// heap_insert.
    pub fn insert(&mut self, key: Vec<u8>, len: usize, xmin: u64, mode: &mut InsertMode) -> Tid {
        let blk = self.get_buffer(len, None, mode.use_fsm, mode.bistate.as_mut(), 1);
        let xmin = if mode.frozen { 0 } else { xmin };
        let tid = self.put(blk, Self::new_tup(len, xmin), mode.frozen);
        self.placed(&key, tid);
        self.keyed(&key, tid);
        tid
    }

    /// heap_multi_insert (a COPY batch).
    pub fn multi_insert(
        &mut self,
        rows: Vec<(Vec<u8>, usize)>,
        xmin: u64,
        mode: &mut InsertMode,
    ) -> Vec<Tid> {
        let lens: Vec<usize> = rows.iter().map(|r| r.1).collect();
        let out = self.multi_place(&lens, xmin, mode);
        for ((k, _), tid) in rows.into_iter().zip(&out) {
            self.placed(&k, *tid);
            self.keyed(&k, *tid);
        }
        out
    }

    /// `insert` of a row only its page needs (a model dropped next, whose pages are
    /// estimated from it: `sizing::past_model`): no key kept.
    pub fn insert_unkeyed(&mut self, len: usize, xmin: u64, mode: &mut InsertMode) {
        let blk = self.get_buffer(len, None, mode.use_fsm, mode.bistate.as_mut(), 1);
        let xmin = if mode.frozen { 0 } else { xmin };
        self.put(blk, Self::new_tup(len, xmin), mode.frozen);
    }

    /// `multi_insert` of rows of these lengths onto their pages, without their keys (as
    /// `insert_unkeyed`; the pages are the same).
    pub fn multi_place(&mut self, lens: &[usize], xmin: u64, mode: &mut InsertMode) -> Vec<Tid> {
        let save = self.save_free();
        let pages_needed = |from: usize| {
            let mut avail = BLCKSZ - PAGE_HEADER - save;
            let mut n = 1;
            for &len in &lens[from..] {
                let sz = ITEM_ID + maxalign(len);
                if avail < sz {
                    n += 1;
                    avail = BLCKSZ - PAGE_HEADER - save;
                }
                avail = avail.saturating_sub(sz);
            }
            n
        };
        let mut out = Vec::with_capacity(lens.len());
        let (mut done, mut npages, mut used) = (0, 0, 0);
        let mut empty_start = false;
        while done < lens.len() {
            if done == 0 || !empty_start {
                npages = pages_needed(done);
                used = 0;
            } else {
                used += 1;
            }
            let blk = self.get_buffer(
                lens[done],
                None,
                mode.use_fsm,
                mode.bistate.as_mut(),
                npages - used,
            );
            empty_start = self.pages[blk as usize].lps.is_empty();
            let mut n = 0;
            while done + n < lens.len() {
                let len = lens[done + n];
                if n > 0 && self.pages[blk as usize].heap_free() < maxalign(len) + save {
                    break;
                }
                // (HEAP_INSERT_FROZEN: the tuple is frozen as it goes in.)
                let x = if mode.frozen { 0 } else { xmin };
                out.push(self.put(blk, Self::new_tup(len, x), mode.frozen));
                n += 1;
            }
            done += n;
        }
        out
    }

    /// heap_delete.
    pub fn delete(&mut self, key: &[u8], xid: u64, v: &Vis) {
        let Some((b, off)) = self.locate(key, v) else {
            return;
        };
        let p = &mut self.pages[b as usize];
        let mut row = super::keymap::NO_ROW;
        if let Some(t) = p.tup_mut(off) {
            t.xmax = xid;
            t.hot_updated = false;
            row = t.row;
        }
        p.set_prunable(xid);
        p.all_visible = false;
        if row != super::keymap::NO_ROW {
            self.deleted.insert((b, off), row);
        }
    }

    /// Line pointers `freed` are unused now: a deleted row's key that still names one of
    /// them (its last version) goes, as nothing is left of the row. (Before, a later row
    /// given the line pointer was found under the deleted row's key too: a VACUUM FULL
    /// copied it twice.)
    fn forget_deleted(&mut self, freed: &[Tid]) {
        if self.deleted.is_empty() {
            return;
        }
        for tid in freed {
            if let Some(row) = self.deleted.remove(tid) {
                self.keys.remove_row_at(row, *tid);
            }
        }
        self.compact_rows();
    }

    /// heap_update: the new version goes on the old one's page if it fits (a HOT update
    /// when `hot` allows: no indexed column changed), else where an insert would go.
    /// Returns the new version's TID and whether it was HOT.
    pub fn update(
        &mut self,
        old_key: &[u8],
        new_key: &[u8],
        len: usize,
        xid: u64,
        hot: bool,
        v: &Vis,
    ) -> (Tid, bool) {
        let Some((ob, ooff)) = self.locate(old_key, v) else {
            let tid = self.insert(new_key.to_vec(), len, xid, &mut InsertMode::single());
            return (tid, false);
        };
        // (No pruning here: heap_update leaves that to the scans that read the page.)
        let free = self.pages[ob as usize].heap_free();
        let (nb, same) = if maxalign(len) <= free {
            (ob, true)
        } else {
            let nb = self.get_buffer(len, Some(ob), true, None, 1);
            (nb, nb == ob)
        };
        let hot = same && hot;
        let mut t = Self::new_tup(len, xid);
        t.heap_only = hot;
        let (_, noff) = self.put(nb, t, false);
        self.set_prev((nb, noff), Some((ob, ooff)));
        let op = &mut self.pages[ob as usize];
        let mut old_row = super::keymap::NO_ROW;
        if let Some(o) = op.tup_mut(ooff) {
            o.xmax = xid;
            o.hot_updated = hot;
            o.next = if hot { noff } else { 0 };
            old_row = o.row;
        }
        if !same {
            op.full = true;
        }
        op.set_prunable(xid);
        op.all_visible = false;
        let moved = old_key != new_key;
        if moved {
            self.keys.remove(old_key);
        }
        self.disordered = true;
        let row = self.keyed(new_key, (nb, noff));
        if xid >= crate::txn::xids::FIRST_NORMAL_XID {
            self.pending
                .entry(xid)
                .or_default()
                .push((row, (nb, noff), moved.then_some(old_row)));
        }
        ((nb, noff), hot)
    }

    /// Rows whose last update's transaction aborted point at the version it replaced
    /// again (see `pending`); those of transactions that ended otherwise are forgotten,
    /// with the links back to other pages their new versions kept (`far_prev`).
    fn settle(&mut self, v: &Vis) {
        if self.pending.is_empty() {
            return;
        }
        let ended: Vec<u64> = (self.pending.keys().copied())
            .filter(|x| v.state(*x) != Xs::Running)
            .collect();
        for x in ended {
            let list = self.pending.remove(&x).unwrap_or_default();
            if v.state(x) == Xs::Aborted {
                for &(row, tid, moved) in list.iter().rev() {
                    // (Rare: the keys are copied out of the map they go back into.)
                    let Some(key) = self.keys.key(row).map(<[u8]>::to_vec) else { continue };
                    if self.keys.get(&key) != Some(tid) {
                        continue;
                    }
                    let back = self.locate(&key, v);
                    if let Some(old) = moved {
                        self.keys.remove(&key);
                        if let (Some(b), Some(k)) = (back, self.keys.key(old).map(<[u8]>::to_vec)) {
                            self.keyed(&k, b);
                        }
                    } else if let Some(b) = back {
                        self.keyed(&key, b);
                    }
                }
            }
            if !self.far_prev.is_empty() {
                for (_, tid, _) in &list {
                    self.far_prev.remove(tid);
                }
            }
        }
    }

    /// heap_page_prune_opt: what a scan reading the page does.
    pub fn prune_opt(&mut self, blk: u32, v: &Vis) {
        if self.prune_wanted(blk, v) {
            self.prune(blk, v, false);
        }
    }

    /// Whether `prune_opt` would prune page `blk` (heap_page_prune_opt's tests).
    pub fn prune_wanted(&self, blk: u32, v: &Vis) -> bool {
        let Some(p) = self.pages.get(blk as usize) else {
            return false;
        };
        if p.prune_xid == 0 || !v.removable(p.prune_xid) {
            return false;
        }
        // (ConditionalLockBufferForCleanup fails while another pin is held.)
        if self.pins.contains_key(&blk) {
            return false;
        }
        let minfree = self.save_free().max(BLCKSZ / 10);
        p.full || p.heap_free() < minfree
    }

    /// A sequential scan reads (and may prune) every page.
    pub fn prune_all(&mut self, v: &Vis) {
        for b in 0..self.pages.len() as u32 {
            self.prune_opt(b, v);
        }
    }

    /// heap_page_prune_and_freeze: dead versions lose their storage; a dead HOT chain's
    /// root becomes a dead (or with `unused_now`, unused) line pointer or redirects to
    /// the chain's first live member. Returns (normal-or-redirect items kept, dead line
    /// pointers left, live tuples, all visible).
    fn prune(&mut self, blk: u32, v: &Vis, unused_now: bool) -> (bool, Vec<u16>, usize, bool) {
        self.settle(v);
        let p = &self.pages[blk as usize];
        let n = p.lps.len();
        let mut htsv: Vec<Option<Htsv>> = vec![None; n + 1];
        let mut processed = vec![false; n + 1];
        // New states: None = unchanged.
        let mut new: Vec<Option<Lp>> = vec![None; n + 1];
        let mut roots = Vec::new();
        let mut heaponly = Vec::new();
        for off in 1..=n {
            match &p.lps[off - 1] {
                Lp::Unused => processed[off] = true,
                Lp::Dead => {
                    processed[off] = true;
                    if unused_now {
                        new[off] = Some(Lp::Unused);
                    }
                }
                Lp::Redirect(_) => roots.push(off),
                Lp::Normal(t) => {
                    htsv[off] = Some(t.status(v));
                    if t.heap_only {
                        heaponly.push(off);
                    } else {
                        roots.push(off);
                    }
                }
            }
        }
        let mut prune_xid = 0u64;
        let mut prunable = |x: u64| {
            if x != 0 && (prune_xid == 0 || x < prune_xid) {
                prune_xid = x;
            }
        };
        let dead_or_unused = if unused_now { Lp::Unused } else { Lp::Dead };
        for &root in &roots {
            if processed[root] {
                continue;
            }
            let mut chain: Vec<usize> = Vec::new();
            let mut ndead = 0;
            let mut off = root;
            let mut prior_xmax = 0u64;
            let is_redirect = matches!(p.lps[root - 1], Lp::Redirect(_));
            loop {
                if off < 1 || off > n || processed[off] {
                    break;
                }
                match &p.lps[off - 1] {
                    Lp::Redirect(to) => {
                        if !chain.is_empty() {
                            break;
                        }
                        chain.push(off);
                        off = *to as usize;
                        continue;
                    }
                    Lp::Normal(t) => {
                        if prior_xmax != 0 && t.xmin != prior_xmax {
                            break;
                        }
                        chain.push(off);
                        match htsv[off] {
                            Some(Htsv::Dead) => ndead = chain.len(),
                            Some(Htsv::RecentlyDead) => {}
                            _ => break,
                        }
                        if !t.hot_updated {
                            break;
                        }
                        off = t.next as usize;
                        prior_xmax = t.xmax;
                    }
                    _ => break,
                }
            }
            for &c in &chain {
                processed[c] = true;
            }
            if is_redirect && chain.len() < 2 {
                new[root] = Some(dead_or_unused.clone());
                continue;
            }
            let unchanged = |c: usize, prunable: &mut dyn FnMut(u64)| {
                if let (Some(h), Lp::Normal(t)) = (htsv[c], &p.lps[c - 1])
                    && matches!(h, Htsv::RecentlyDead | Htsv::DeleteInProgress)
                {
                    prunable(t.xmax);
                }
            };
            if ndead == 0 {
                for &c in &chain {
                    unchanged(c, &mut prunable);
                }
            } else if ndead == chain.len() {
                new[root] = Some(dead_or_unused.clone());
                for &c in &chain[1..] {
                    new[c] = Some(Lp::Unused);
                }
            } else {
                new[root] = Some(Lp::Redirect(chain[ndead] as u16));
                for &c in &chain[1..ndead] {
                    new[c] = Some(Lp::Unused);
                }
                for &c in &chain[ndead..] {
                    unchanged(c, &mut prunable);
                }
            }
        }
        for &off in &heaponly {
            if processed[off] {
                continue;
            }
            processed[off] = true;
            if htsv[off] == Some(Htsv::Dead) {
                new[off] = Some(Lp::Unused);
            } else if let (Some(h), Lp::Normal(t)) = (htsv[off], &p.lps[off - 1])
                && matches!(h, Htsv::RecentlyDead | Htsv::DeleteInProgress)
            {
                prunable(t.xmax);
            }
        }
        let do_prune = new.iter().any(Option::is_some);
        let p = &mut self.pages[blk as usize];
        if p.prune_xid != prune_xid || p.full {
            p.prune_xid = prune_xid;
            p.full = false;
        }
        let mut freed = Vec::new();
        if do_prune {
            for (off, st) in new.into_iter().enumerate().skip(1) {
                if let Some(st) = st {
                    if let Lp::Normal(t) = &p.lps[off - 1] {
                        p.used -= maxalign(t.len as usize);
                        self.versions -= 1;
                    }
                    if matches!(st, Lp::Unused) {
                        freed.push((blk, off as u16));
                    }
                    p.lps[off - 1] = st;
                }
            }
            p.repair();
        }
        self.forget_deleted(&freed);
        let p = &mut self.pages[blk as usize];
        // What VACUUM needs to know about the page now.
        let mut hastup = false;
        let mut dead = Vec::new();
        let mut live = 0;
        let mut all_visible = true;
        for (i, lp) in p.lps.iter().enumerate() {
            match lp {
                Lp::Unused => {}
                Lp::Dead => dead.push(i as u16 + 1),
                Lp::Redirect(_) => hastup = true,
                Lp::Normal(t) => {
                    hastup = true;
                    match t.status(v) {
                        Htsv::Live => {
                            live += 1;
                            all_visible &= visible_to_all(t, v);
                        }
                        Htsv::DeleteInProgress => {
                            live += 1;
                            all_visible = false;
                        }
                        _ => all_visible = false,
                    }
                }
            }
        }
        let all_visible = all_visible && dead.is_empty();
        (hastup, dead, live, all_visible)
    }

    /// Lazy VACUUM: prune every page, then (when there were dead tuples and `nindexes`
    /// indexes to clean, unless that is bypassed) free the dead line pointers, record
    /// free space, mark pages all-visible and truncate empty trailing pages.
    pub fn vacuum(&mut self, v: &Vis, nindexes: usize, truncate: bool) -> VacuumResult {
        self.settle(v);
        let rel_pages = self.pages.len() as u32;
        let mut res = VacuumResult::default();
        if rel_pages > 0 {
            self.vm_pin(rel_pages - 1);
        }
        let mut nonempty = 0u32;
        let mut lpdead_pages = 0u32;
        let mut dead_by_page: Vec<(u32, Vec<u16>)> = Vec::new();
        for b in 0..rel_pages {
            let p = &self.pages[b as usize];
            if p.new {
                if self.fsm.recorded(b) == 0 {
                    self.fsm.record(b, BLCKSZ - PAGE_HEADER);
                }
                continue;
            }
            if p.lps.is_empty() {
                let p = &mut self.pages[b as usize];
                p.all_visible = true;
                let free = p.heap_free();
                self.fsm.record(b, free);
                continue;
            }
            let (hastup, dead, live, all_visible) = self.prune(b, v, nindexes == 0);
            res.live += live as f64;
            // HEAPTUPLE_RECENTLY_DEAD: dead, but a transaction or snapshot may still see it.
            res.recently_dead += self.pages[b as usize]
                .lps
                .iter()
                .filter(|l| matches!(l, Lp::Normal(t) if t.status(v) == Htsv::RecentlyDead))
                .count() as u32;
            if hastup {
                nonempty = b + 1;
            }
            self.pages[b as usize].all_visible = all_visible;
            if !dead.is_empty() {
                lpdead_pages += 1;
                dead_by_page.push((b, dead));
            } else {
                let free = self.pages[b as usize].heap_free();
                self.fsm.record(b, free);
            }
        }
        if !dead_by_page.is_empty() {
            let threshold = (rel_pages as f64 * 0.02) as u32;
            let bypass = lpdead_pages < threshold;
            for (b, dead) in dead_by_page {
                if !bypass {
                    let p = &mut self.pages[b as usize];
                    for &off in &dead {
                        p.lps[off as usize - 1] = Lp::Unused;
                        res.dead.push((b, off));
                    }
                    p.truncate_lps();
                    let vis = p.lps.iter().all(|l| match l {
                        Lp::Unused | Lp::Redirect(_) => true,
                        Lp::Dead => false,
                        Lp::Normal(t) => t.status(v) == Htsv::Live && visible_to_all(t, v),
                    });
                    p.all_visible = vis;
                    let free = p.heap_free();
                    self.fsm.record(b, free);
                }
            }
        }
        self.fsm.vacuum_range(0, rel_pages);
        let mut pages = rel_pages;
        let freeable = rel_pages - nonempty;
        if truncate && freeable > 0 && (freeable >= 1000 || freeable >= rel_pages / 16) {
            // count_nondeletable_pages
            while pages > nonempty {
                let p = &self.pages[pages as usize - 1];
                if !p.new && p.lps.iter().any(Lp::used) {
                    break;
                }
                pages -= 1;
            }
            if pages < rel_pages {
                self.truncate_to(pages);
            }
        }
        self.targets.clear();
        let freed = std::mem::take(&mut res.dead);
        self.forget_deleted(&freed);
        res.dead = freed;
        res.pages = pages;
        res.all_visible = self.all_visible_pages();
        self.note_size();
        res
    }

    fn truncate_to(&mut self, pages: u32) {
        for p in &self.pages[pages as usize..] {
            self.versions -= p.lps.iter().filter(|l| matches!(l, Lp::Normal(_))).count();
        }
        self.pages.truncate(pages as usize);
        self.keys.retain(|t| t.0 < pages);
        self.fsm.truncate(pages);
        if self.vm_blocks > 0 {
            let blk = pages as usize / VM_HEAPBLOCKS;
            let within = pages as usize % VM_HEAPBLOCKS;
            self.vm_blocks = self.vm_blocks.min(blk as u32 + (within > 0) as u32);
        }
    }

    /// Live tuples, as ANALYZE counts them.
    pub fn live_tuples(&self, v: &Vis) -> f64 {
        let mut n = 0;
        for p in &self.pages {
            for lp in &p.lps {
                if let Lp::Normal(t) = lp
                    && matches!(t.status(v), Htsv::Live | Htsv::DeleteInProgress)
                {
                    n += 1;
                }
            }
        }
        n as f64
    }

    /// Rows stored under new keys (old, new); rows not listed are forgotten.
    pub fn rekey(&mut self, pairs: &[(Vec<u8>, Vec<u8>)]) {
        let mut keys = super::keymap::KeyMap::default();
        keys.reserve(pairs.len(), pairs.iter().map(|(_, n)| n.len()).sum());
        let mut map = vec![super::keymap::NO_ROW; self.keys.rows()];
        for (old, new) in pairs {
            if let Some(row) = self.keys.find(old) {
                map[row as usize] = keys.insert(new, self.keys.tid(row));
            }
        }
        self.keys = keys;
        self.disordered = true;
        // (A row that moved back from an aborted update is under its old key: forgotten.)
        self.renumber_rows(&map);
        for list in self.pending.values_mut() {
            list.retain_mut(|(row, _, moved)| {
                *moved = None;
                *row != super::keymap::NO_ROW
            });
        }
    }

    /// Rows renumbered (`map`: each old row's new number, or `NO_ROW`): every version's,
    /// and those `pending` and `deleted` name.
    fn renumber_rows(&mut self, map: &[u32]) {
        let to = |r: u32| map.get(r as usize).copied().unwrap_or(super::keymap::NO_ROW);
        for p in &mut self.pages {
            for l in &mut p.lps {
                if let Lp::Normal(t) = l {
                    t.row = to(t.row);
                }
            }
        }
        for list in self.pending.values_mut() {
            for (row, _, moved) in list.iter_mut() {
                *row = to(*row);
                *moved = moved.map(to);
            }
        }
        self.deleted.retain(|_, r| {
            *r = to(*r);
            *r != super::keymap::NO_ROW
        });
    }

    /// Drop the rows of removed keys no version names any more (`KeyMap::wasteful`).
    fn compact_rows(&mut self) {
        if !self.keys.wasteful() {
            return;
        }
        let mut keep = vec![false; self.keys.rows()];
        for p in &self.pages {
            for l in &p.lps {
                if let Lp::Normal(t) = l
                    && let Some(k) = keep.get_mut(t.row as usize)
                {
                    *k = true;
                }
            }
        }
        let map = self.keys.compact(&keep);
        self.renumber_rows(&map);
    }

    /// The rows whose current version is live, in physical order: (key, t_len).
    /// Rows whose last update aborted go back to the versions it replaced first (`settle`):
    /// until then their keys point at the aborted versions, and a moved row isn't under its
    /// old key.
    pub fn live_rows(&mut self, v: &Vis) -> Vec<(Vec<u8>, usize)> {
        self.settle(v);
        let mut out: Vec<(Tid, &[u8], usize)> = Vec::new();
        for (k, _, _) in self.keys.iter() {
            if let Some(tid) = self.locate(k, v)
                && let Some(t) = self.tuple(tid)
                && matches!(t.status(v), Htsv::Live | Htsv::DeleteInProgress | Htsv::InsertInProgress)
            {
                out.push((tid, k, t.len as usize));
            }
        }
        out.sort_by_key(|x| x.0);
        out.into_iter().map(|(_, k, l)| (k.to_vec(), l)).collect()
    }

    /// Whether the version at `tid` is gone for everyone (bottom-up index deletion).
    /// heap_hot_search_buffer's all_dead: every version of the HOT chain at `tid` is dead
    /// to everyone.
    pub fn chain_dead(&self, tid: Tid, v: &Vis) -> bool {
        let Some(page) = self.pages.get(tid.0 as usize) else {
            return true;
        };
        let mut off = tid.1;
        for _ in 0..=page.lps.len() {
            match page.lps.get(off as usize - 1) {
                Some(Lp::Redirect(o)) => off = *o,
                Some(Lp::Normal(t)) => {
                    if t.status(v) != Htsv::Dead {
                        return false;
                    }
                    if !t.hot_updated || t.next == 0 {
                        return true;
                    }
                    off = t.next;
                }
                Some(Lp::Dead) | Some(Lp::Unused) | None => return true,
            }
        }
        false
    }

    pub fn is_dead(&self, tid: Tid, v: &Vis) -> bool {
        match self.pages.get(tid.0 as usize).and_then(|p| p.lps.get(tid.1 as usize - 1)) {
            Some(Lp::Normal(t)) => t.status(v) == Htsv::Dead,
            Some(Lp::Dead) | Some(Lp::Unused) | None => true,
            Some(Lp::Redirect(_)) => false,
        }
    }
}

/// A varint (the kept models' records: `exec::sizing::kept`).
pub(crate) fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// A varint `put_varint` wrote, advancing `b` past it.
pub(crate) fn get_varint(b: &mut &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let (&x, rest) = b.split_first()?;
        *b = rest;
        v |= ((x & 0x7f) as u64) << shift;
        if x < 0x80 {
            return Some(v);
        }
    }
    None
}

impl Heap {
    /// What `restored` can't tell from the rows, for a heap kept across a clean restart
    /// (`exec::sizing::kept`; `read_shape`): every page's line pointers as they are (unused,
    /// dead, redirects, the dead tuples still taking space, HOT chain links: `is_row` names
    /// the rows `restored` puts back), its visibility map bit, PD_PAGE_FULL and whether it
    /// has a prunable xid, and the free space map's pages and the visibility map's length.
    /// PostgreSQL's files keep all of it.
    pub fn write_shape(&self, out: &mut Vec<u8>, is_row: &dyn Fn(Tid) -> bool) {
        put_varint(out, self.pages.len() as u64);
        for (b, p) in self.pages.iter().enumerate() {
            put_varint(out, p.lps.len() as u64);
            let flags = p.all_visible as u64 | (p.full as u64) << 1 | (p.free_lps as u64) << 2 | ((p.prune_xid != 0) as u64) << 3;
            put_varint(out, flags);
            for (i, lp) in p.lps.iter().enumerate() {
                match lp {
                    Lp::Unused => put_varint(out, 0),
                    Lp::Dead => put_varint(out, 1),
                    Lp::Redirect(o) => {
                        put_varint(out, 2);
                        put_varint(out, *o as u64);
                    }
                    Lp::Normal(t) => {
                        let row = is_row((b as u32, i as u16 + 1));
                        let links = t.heap_only || t.hot_updated;
                        put_varint(out, 3 + row as u64 * 2 + links as u64);
                        if !row {
                            put_varint(out, t.len as u64);
                        }
                        if links {
                            put_varint(out, t.heap_only as u64 | (t.hot_updated as u64) << 1);
                            put_varint(out, t.next as u64);
                        }
                    }
                }
            }
        }
        put_varint(out, self.vm_blocks as u64);
        put_varint(out, self.fsm.blocks as u64);
        let mut fsm: Vec<(&u32, &FsmPage)> = self.fsm.pages.iter().collect();
        fsm.sort_unstable_by_key(|(b, _)| **b);
        put_varint(out, fsm.len() as u64);
        for (b, p) in fsm {
            put_varint(out, *b as u64);
            put_varint(out, p.next as u64);
            let set: Vec<(usize, u8)> =
                p.nodes.iter().enumerate().filter(|(_, v)| **v != 0).map(|(i, v)| (i, *v)).collect();
            put_varint(out, set.len() as u64);
            let mut prev = 0;
            for (i, v) in set {
                put_varint(out, (i - prev) as u64);
                out.push(v);
                prev = i;
            }
        }
    }

    /// Give a heap `restored` from its kept rows the shape `write_shape` kept: its rows
    /// stay where they are, everything else on the pages comes back (dead tuples dead to
    /// everyone, as after a restart). None (the heap as it was) when the bytes don't hold a
    /// shape, or hold one the rows don't fit.
    pub fn read_shape(&mut self, mut b: &[u8]) -> Option<()> {
        let b = &mut b;
        let n = get_varint(b)? as usize;
        if n < self.pages.len() || n > 1 << 22 {
            return None;
        }
        // (Read whole and checked against the rows, then applied.)
        enum L {
            Unused,
            Dead,
            Redirect(u16),
            Row(u8, u16),
            Gone(u32, u8, u16),
        }
        let mut pages = Vec::with_capacity(n);
        for i in 0..n {
            let nlps = get_varint(b)? as usize;
            let flags = get_varint(b)?;
            let have = self.pages.get(i).map_or(&[][..], |p| &p.lps[..]);
            if nlps < have.len() || nlps > MAX_HEAP_TUPLES {
                return None;
            }
            let mut lps = Vec::with_capacity(nlps);
            for j in 0..nlps {
                let code = get_varint(b)?;
                let placed = matches!(have.get(j), Some(Lp::Normal(_)));
                let links = |b: &mut &[u8]| -> Option<(u8, u16)> { Some((get_varint(b)? as u8 & 3, u16::try_from(get_varint(b)?).ok()?)) };
                let lp = match code {
                    0 => L::Unused,
                    1 => L::Dead,
                    2 => L::Redirect(u16::try_from(get_varint(b)?).ok().filter(|&o| o >= 1 && o as usize <= nlps)?),
                    3 | 4 => {
                        let len = u32::try_from(get_varint(b)?).ok()?;
                        let (f, next) = if code == 4 { links(b)? } else { (0, 0) };
                        L::Gone(len, f, next)
                    }
                    5 => L::Row(0, 0),
                    6 => {
                        let (f, next) = links(b)?;
                        L::Row(f, next)
                    }
                    _ => return None,
                };
                if matches!(lp, L::Row(..)) != placed {
                    return None;
                }
                lps.push(lp);
            }
            pages.push((flags, lps));
        }
        let vm_blocks = u32::try_from(get_varint(b)?).ok()?;
        let fsm_blocks = u32::try_from(get_varint(b)?).ok()?;
        let mut fsm = HashMap::default();
        for _ in 0..get_varint(b)? {
            let blk = u32::try_from(get_varint(b)?).ok()?;
            let mut p = FsmPage::new();
            p.next = get_varint(b)? as usize;
            let mut at = 0;
            for _ in 0..get_varint(b)? {
                at += get_varint(b)? as usize;
                let (&v, rest) = b.split_first()?;
                *b = rest;
                *p.nodes.get_mut(at)? = v;
            }
            fsm.insert(blk, p);
        }
        if !b.is_empty() {
            return None;
        }
        self.pages.resize_with(n, Page::fresh);
        for (p, (flags, lps)) in self.pages.iter_mut().zip(pages) {
            p.lps.resize(lps.len(), Lp::Dead);
            let mut prev = Vec::new();
            for (j, lp) in lps.into_iter().enumerate() {
                let mut chain = |t: &mut Tup, f: u8, next: u16| {
                    (t.heap_only, t.hot_updated, t.next) = (f & 1 != 0, f & 2 != 0, next);
                    if t.hot_updated && next != 0 {
                        prev.push((next, j as u16 + 1));
                    }
                };
                match lp {
                    L::Unused => p.lps[j] = Lp::Unused,
                    L::Dead => p.lps[j] = Lp::Dead,
                    L::Redirect(o) => {
                        p.lps[j] = Lp::Redirect(o);
                        prev.push((o, j as u16 + 1));
                    }
                    L::Row(f, next) => {
                        if let Lp::Normal(t) = &mut p.lps[j] {
                            chain(t, f, next);
                        }
                    }
                    L::Gone(len, f, next) => {
                        // (Dead to everyone: FrozenTransactionId deleted it.)
                        let mut t = Heap::new_tup(len as usize, 0);
                        t.xmax = crate::txn::xids::FIRST_NORMAL_XID - 1;
                        chain(&mut t, f, next);
                        p.used += maxalign(len as usize);
                        self.versions += 1;
                        p.lps[j] = Lp::Normal(t);
                    }
                }
            }
            // (Each chain member's way back, as `update` and pruning leave it.)
            for (to, from) in prev {
                if let Some(Lp::Normal(t)) = p.lps.get_mut(to as usize - 1) {
                    t.prev_off = from;
                }
            }
            p.all_visible = flags & 1 != 0;
            p.full = flags & 2 != 0;
            p.free_lps = flags & 4 != 0;
            p.prune_xid = if flags & 8 != 0 { crate::txn::xids::FIRST_NORMAL_XID - 1 } else { 0 };
            p.new = false;
        }
        self.vm_blocks = vm_blocks;
        self.fsm = Fsm { blocks: fsm_blocks, pages: fsm };
        self.note_size();
        Some(())
    }
}

/// The heap a reopened database (after a clean shutdown or a crash) finds: every row at the
/// TID it was written with, since PostgreSQL never moves a committed row on restart or
/// recovery. The line pointers between them are dead (see `place_at`). Rows come in TID
/// order, and rows without a TID come last. A row
/// without a TID, or whose TID an earlier row already holds (a database written before
/// restored models existed), goes where a rewrite would put it. Returns whether any row's
/// place differs from its stored TID, and each row's place (in the order given).
pub fn restored(fillfactor: usize, rows: Vec<(Vec<u8>, usize, Option<Tid>)>) -> (Heap, bool, Vec<Tid>) {
    let mut h = Heap::new(fillfactor);
    h.keys.reserve(rows.len(), rows.iter().map(|r| r.0.len()).sum());
    let mut moved = false;
    let mut tids = vec![(0, 0); rows.len()];
    let mut rest = Vec::new();
    for (i, (k, len, stored)) in rows.into_iter().enumerate() {
        match stored.filter(|&tid| h.place_at(tid, Heap::new_tup(len, 0))) {
            Some(tid) => {
                h.placed(&k, tid);
                h.keyed(&k, tid);
                tids[i] = tid;
            }
            None => {
                moved |= stored.is_some();
                rest.push((i, k, len));
            }
        }
    }
    for (i, k, len) in rest {
        let tid = h.append(Heap::new_tup(len, 0));
        h.placed(&k, tid);
        h.keyed(&k, tid);
        tids[i] = tid;
    }
    (h, moved, tids)
}

impl Heap {
    /// Put `t` at `tid` unless a tuple (or a redirect) holds that line pointer already. Pages
    /// and the line pointers before it are made as needed. Line pointers made as gaps are
    /// dead: in PostgreSQL they hold versions pruned away or still dead, which only VACUUM
    /// frees for reuse. Returns false when the slot is held, or the TID can't be one a
    /// modelled heap gave out.
    fn place_at(&mut self, (b, off): Tid, t: Tup) -> bool {
        if off == 0 || off as usize > MAX_HEAP_TUPLES || b >= 1 << 20 {
            return false;
        }
        if self.pages.len() <= b as usize {
            self.pages.resize_with(b as usize + 1, Page::fresh);
        }
        let p = &mut self.pages[b as usize];
        if matches!(p.lps.get(off as usize - 1), Some(Lp::Normal(_) | Lp::Redirect(_))) {
            return false;
        }
        if p.lps.len() < off as usize {
            p.lps.resize(off as usize, Lp::Dead);
        }
        p.used += maxalign(t.len as usize);
        p.lps[off as usize - 1] = Lp::Normal(t);
        p.new = false;
        self.versions += 1;
        true
    }

    /// Put `t` where a rewrite would: on the last page if it fits there, else a new one.
    fn append(&mut self, t: Tup) -> Tid {
        let need = maxalign(t.len as usize) + self.save_free();
        if !self.pages.last().is_some_and(|p| need <= p.heap_free()) {
            self.pages.push(Page::fresh());
        }
        let b = self.pages.len() as u32 - 1;
        self.put(b, t, false)
    }

    /// A prepared transaction's write to the row stored under `key`, found when the heap is
    /// laid out afresh (`restored`): the version it replaced or deleted gets its xid as
    /// xmax, and the version it made (`new`: its length and stored TID) goes at that TID.
    /// This is what `update` / `delete` would have left, so COMMIT PREPARED and ROLLBACK
    /// PREPARED settle it the same way. Returns whether the new version had to go elsewhere.
    pub fn restore_pending(&mut self, xid: u64, key: Vec<u8>, new: Option<(usize, Option<Tid>)>) -> bool {
        let old = self.keys.get(&key);
        if let Some((b, off)) = old {
            let p = &mut self.pages[b as usize];
            if let Some(t) = p.tup_mut(off) {
                t.xmax = xid;
            }
            p.set_prunable(xid);
            p.all_visible = false;
        }
        let Some((len, stored)) = new else {
            return false;
        };
        let t = Self::new_tup(len, xid);
        let tid = match stored.filter(|&tid| self.place_at(tid, t.clone())) {
            Some(tid) => tid,
            None => self.append(t),
        };
        self.set_prev(tid, old);
        self.disordered = true;
        let row = self.keyed(&key, tid);
        self.pending.entry(xid).or_default().push((row, tid, None));
        stored.is_some_and(|s| s != tid)
    }
}

/// A heap built the way a rewrite (VACUUM FULL, CLUSTER) writes it: pages filled in turn
/// up to the fillfactor, no free space map or visibility map yet.
pub fn rewritten(fillfactor: usize, rows: Vec<(Vec<u8>, usize)>) -> (Heap, Vec<Tid>) {
    let mut h = Heap::new(fillfactor);
    let save = h.save_free();
    let mut tids = Vec::with_capacity(rows.len());
    for (k, len) in rows {
        let fits = h
            .pages
            .last()
            .is_some_and(|p| maxalign(len) + save <= p.heap_free());
        if !fits {
            h.pages.push(Page::fresh());
        }
        let b = h.pages.len() as u32 - 1;
        let tid = h.put(b, Heap::new_tup(len, 0), false);
        h.placed(&k, tid);
        h.keyed(&k, tid);
        tids.push(tid);
    }
    (h, tids)
}

// (A model holds one per entry / version.)
const _: () = assert!(size_of::<Lp>() == 32);

#[cfg(test)]
mod tests {
    use super::*;

    /// A TID gives the key of the row its version belongs to: the current version's, an
    /// older version's (its key as it was when the row moved to another key), a deleted
    /// row's; and none for a TID with no tuple.
    #[test]
    fn key_at_answers_every_version() {
        let xids = Xids::new(10, |_| {});
        let v = Vis::new(&xids);
        let mut h = Heap::new(100);
        let mut mode = InsertMode::single();
        let a = h.insert(b"a".to_vec(), 100, 2, &mut mode);
        let b = h.insert(b"b".to_vec(), 100, 2, &mut mode);
        let (a2, hot) = h.update(b"a", b"a", 100, 2, true, &v);
        assert!(hot);
        let (c, _) = h.update(b"b", b"c", 100, 2, false, &v);
        h.delete(b"a", 2, &v);
        assert_eq!(h.key_at(a), Some(&b"a"[..]));
        assert_eq!(h.key_at(a2), Some(&b"a"[..]));
        assert_eq!(h.key_at(b), Some(&b"b"[..]));
        assert_eq!(h.key_at(c), Some(&b"c"[..]));
        assert_eq!(h.key_at((7, 1)), None);
        assert_eq!(h.key_at((0, 0)), None, "offset 0 is no line pointer's");
        assert_eq!(h.tid_of(b"b"), None);
        assert_eq!(h.tid_of(b"c"), Some(c));
        let got = h.keys_at(&[a, (7, 1), c]);
        assert_eq!(got, vec![(a, b"a".to_vec()), (c, b"c".to_vec())]);
        // Rows stored under new keys: every version follows its row.
        h.rekey(&[(b"a".to_vec(), b"x".to_vec()), (b"c".to_vec(), b"y".to_vec())]);
        assert_eq!(h.key_at(a), Some(&b"x"[..]));
        assert_eq!(h.key_at(c), Some(&b"y"[..]));
        assert_eq!(h.key_at(b), None, "a row not listed is forgotten");
    }

    /// A heap's size watch holds while inserts fit its pages and fails once one extends the
    /// heap, VACUUM truncates it, or the model goes.
    #[test]
    fn size_watch_follows_forks() {
        let mut h = Heap::new(100);
        // (Frozen inserts: the vm fork exists.)
        let mut mode = InsertMode { frozen: true, ..InsertMode::single() };
        h.insert(vec![0], 100, 2, &mut mode);
        let w = h.sizes.watch();
        let mut i = 1u32;
        while h.nblocks() == 1 {
            assert!(w.holds(), "no page added yet ({i} rows)");
            h.insert(i.to_be_bytes().to_vec(), 100, 2, &mut mode);
            i += 1;
        }
        assert!(!w.holds());
        let w = h.sizes.watch();
        h.insert(vec![9; 9], 100, 2, &mut mode);
        assert!(w.holds());
        h.truncate_vm();
        assert!(!w.holds(), "the vm fork went");
        let w = h.sizes.watch();
        drop(h);
        assert!(!w.holds());
    }
}
