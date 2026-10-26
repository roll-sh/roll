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
        let st = self.xids.status(x);
        let s = match st {
            Ok("in progress") => Xs::Running,
            Ok("aborted") => Xs::Aborted,
            _ => Xs::Committed,
        };
        if st.is_ok() && s != Xs::Running {
            self.settled.set(Some((x, s)));
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

#[derive(Clone, Debug)]
pub struct Tup {
    /// t_len (the tuple takes MAXALIGN(len) on the page).
    pub len: u32,
    pub xmin: u64,
    /// The deleting or updating transaction (0: none).
    pub xmax: u64,
    heap_only: bool,
    hot_updated: bool,
    /// The next version's offset on this page (HOT chains).
    next: u16,
    /// The version this one replaced (followed back when this one's insert aborted).
    prev: Option<Tid>,
    /// t_cid: the inserting command's id, the deleting one's, or a combo command id for
    /// both (what the cmin and cmax system columns show).
    pub cid: u32,
}

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
        let upper = BLCKSZ - self.used;
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
        match self.lps.get(off as usize - 1) {
            Some(Lp::Normal(t)) => Some(t),
            _ => None,
        }
    }

    fn tup_mut(&mut self, off: u16) -> Option<&mut Tup> {
        match self.lps.get_mut(off as usize - 1) {
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
    keys: HashMap<Vec<u8>, Tid>,
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
    pending: HashMap<u64, Vec<(Vec<u8>, Tid, Option<Vec<u8>>)>>,
    /// Pages pinned by something else while a scan reads them (an AFTER ROW trigger's
    /// tuple slot): pruning them needs a cleanup lock it can't get.
    pins: HashMap<u32, usize>,
    /// Laid out afresh from rows whose stored TIDs it doesn't keep: where it has a row, not
    /// the TID stored in the row, is its place (a scan's order).
    pub unstamped: bool,
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

    pub fn nblocks(&self) -> u32 {
        self.pages.len() as u32
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
        let prev = self.pages.get(tid.0 as usize)?.tup(tid.1)?.prev;
        Some((tid, prev))
    }

    /// The version of the row a reader sees: past versions still being written by other
    /// transactions (`mine`: the reader's own transaction ids) to the one they replace.
    pub fn locate_for(&self, key: &[u8], v: &Vis, mine: &dyn Fn(u64) -> bool) -> Option<Tid> {
        let mut tid = *self.keys.get(key)?;
        loop {
            let t = self.pages.get(tid.0 as usize)?.tup(tid.1)?;
            match v.state(t.xmin) {
                Xs::Aborted => {}
                Xs::Running if !mine(t.xmin) => {}
                _ => return Some(tid),
            }
            match t.prev {
                Some(p) => tid = p,
                // (A row another transaction is inserting: where it is.)
                None => return Some(tid),
            }
        }
    }

    pub fn locate(&self, key: &[u8], v: &Vis) -> Option<Tid> {
        let mut tid = *self.keys.get(key)?;
        loop {
            let t = self.pages.get(tid.0 as usize)?.tup(tid.1)?;
            if v.state(t.xmin) != Xs::Aborted {
                return Some(tid);
            }
            tid = t.prev?;
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
            match t.prev {
                Some(p) if p.0 == cur.0 && p != cur => cur = p,
                _ => return cur,
            }
        }
        cur
    }

    pub fn tid_of(&self, key: &[u8]) -> Option<Tid> {
        self.keys.get(key).copied()
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
            len: len as u32,
            xmin,
            xmax: 0,
            heap_only: false,
            hot_updated: false,
            next: 0,
            prev: None,
            cid: 0,
        }
    }

    /// heap_insert.
    pub fn insert(&mut self, key: Vec<u8>, len: usize, xmin: u64, mode: &mut InsertMode) -> Tid {
        let blk = self.get_buffer(len, None, mode.use_fsm, mode.bistate.as_mut(), 1);
        let xmin = if mode.frozen { 0 } else { xmin };
        let tid = self.put(blk, Self::new_tup(len, xmin), mode.frozen);
        self.keys.insert(key, tid);
        tid
    }

    /// heap_multi_insert (a COPY batch).
    pub fn multi_insert(
        &mut self,
        rows: Vec<(Vec<u8>, usize)>,
        xmin: u64,
        mode: &mut InsertMode,
    ) -> Vec<Tid> {
        let save = self.save_free();
        let pages_needed = |from: usize| {
            let mut avail = BLCKSZ - PAGE_HEADER - save;
            let mut n = 1;
            for r in &rows[from..] {
                let sz = ITEM_ID + maxalign(r.1);
                if avail < sz {
                    n += 1;
                    avail = BLCKSZ - PAGE_HEADER - save;
                }
                avail = avail.saturating_sub(sz);
            }
            n
        };
        let mut out = Vec::with_capacity(rows.len());
        let (mut done, mut npages, mut used) = (0, 0, 0);
        let mut empty_start = false;
        while done < rows.len() {
            if done == 0 || !empty_start {
                npages = pages_needed(done);
                used = 0;
            } else {
                used += 1;
            }
            let blk = self.get_buffer(
                rows[done].1,
                None,
                mode.use_fsm,
                mode.bistate.as_mut(),
                npages - used,
            );
            empty_start = self.pages[blk as usize].lps.is_empty();
            let mut n = 0;
            while done + n < rows.len() {
                let len = rows[done + n].1;
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
        for ((k, _), tid) in rows.into_iter().zip(&out) {
            self.keys.insert(k, *tid);
        }
        out
    }

    /// heap_delete.
    pub fn delete(&mut self, key: &[u8], xid: u64, v: &Vis) {
        let Some((b, off)) = self.locate(key, v) else {
            return;
        };
        let p = &mut self.pages[b as usize];
        if let Some(t) = p.tup_mut(off) {
            t.xmax = xid;
            t.hot_updated = false;
        }
        p.set_prunable(xid);
        p.all_visible = false;
    }

    /// heap_update: the new version goes on the old one's page if it fits (a HOT update
    /// when `hot` allows: no indexed column changed), else where an insert would go.
    /// Returns the new version's TID and whether it was HOT.
    pub fn update(
        &mut self,
        old_key: &[u8],
        new_key: Vec<u8>,
        len: usize,
        xid: u64,
        hot: bool,
        v: &Vis,
    ) -> (Tid, bool) {
        let Some((ob, ooff)) = self.locate(old_key, v) else {
            let tid = self.insert(new_key, len, xid, &mut InsertMode::single());
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
        t.prev = Some((ob, ooff));
        let (_, noff) = self.put(nb, t, false);
        let op = &mut self.pages[ob as usize];
        if let Some(o) = op.tup_mut(ooff) {
            o.xmax = xid;
            o.hot_updated = hot;
            o.next = if hot { noff } else { 0 };
        }
        if !same {
            op.full = true;
        }
        op.set_prunable(xid);
        op.all_visible = false;
        let moved = (old_key != new_key.as_slice()).then(|| old_key.to_vec());
        if moved.is_some() {
            self.keys.remove(old_key);
        }
        if xid >= crate::txn::xids::FIRST_NORMAL_XID {
            self.pending
                .entry(xid)
                .or_default()
                .push((new_key.clone(), (nb, noff), moved));
        }
        self.keys.insert(new_key, (nb, noff));
        ((nb, noff), hot)
    }

    /// Rows whose last update's transaction aborted point at the version it replaced
    /// again (see `pending`); those of transactions that ended otherwise are forgotten.
    fn settle(&mut self, v: &Vis) {
        if self.pending.is_empty() {
            return;
        }
        let ended: Vec<u64> = (self.pending.keys().copied())
            .filter(|x| v.state(*x) != Xs::Running)
            .collect();
        for x in ended {
            let list = self.pending.remove(&x).unwrap_or_default();
            if v.state(x) != Xs::Aborted {
                continue;
            }
            for (key, tid, moved) in list.into_iter().rev() {
                if self.keys.get(&key) != Some(&tid) {
                    continue;
                }
                let back = self.locate(&key, v);
                match moved {
                    Some(old) => {
                        self.keys.remove(&key);
                        if let Some(b) = back {
                            self.keys.insert(old, b);
                        }
                    }
                    None => {
                        if let Some(b) = back {
                            self.keys.insert(key, b);
                        }
                    }
                }
            }
        }
    }

    /// heap_page_prune_opt: what a scan reading the page does.
    pub fn prune_opt(&mut self, blk: u32, v: &Vis) {
        let Some(p) = self.pages.get(blk as usize) else {
            return;
        };
        if p.prune_xid == 0 || !v.removable(p.prune_xid) {
            return;
        }
        // (ConditionalLockBufferForCleanup fails while another pin is held.)
        if self.pins.contains_key(&blk) {
            return;
        }
        let minfree = self.save_free().max(BLCKSZ / 10);
        if p.full || p.heap_free() < minfree {
            self.prune(blk, v, false);
        }
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
        if do_prune {
            for (off, st) in new.into_iter().enumerate().skip(1) {
                if let Some(st) = st {
                    if let Lp::Normal(t) = &p.lps[off - 1] {
                        p.used -= maxalign(t.len as usize);
                        self.versions -= 1;
                    }
                    p.lps[off - 1] = st;
                }
            }
            p.repair();
        }
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
        res.pages = pages;
        res.all_visible = self.all_visible_pages();
        res
    }

    fn truncate_to(&mut self, pages: u32) {
        for p in &self.pages[pages as usize..] {
            self.versions -= p.lps.iter().filter(|l| matches!(l, Lp::Normal(_))).count();
        }
        self.pages.truncate(pages as usize);
        self.keys.retain(|_, t| t.0 < pages);
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
        let mut keys = HashMap::with_capacity_and_hasher(pairs.len(), Default::default());
        for (old, new) in pairs {
            if let Some(tid) = self.keys.get(old) {
                keys.insert(new.clone(), *tid);
            }
        }
        self.keys = keys;
        // (A row that moved back from an aborted update is under its old key: forgotten.)
        let renamed: HashMap<&[u8], &Vec<u8>> =
            pairs.iter().map(|(o, n)| (o.as_slice(), n)).collect();
        for list in self.pending.values_mut() {
            list.retain_mut(|(k, _, moved)| match renamed.get(k.as_slice()) {
                Some(n) => {
                    *k = (*n).clone();
                    *moved = None;
                    true
                }
                None => false,
            });
        }
    }

    /// The rows whose current version is live, in physical order: (key, t_len).
    pub fn live_rows(&self, v: &Vis) -> Vec<(Vec<u8>, usize)> {
        let mut out: Vec<(Tid, &Vec<u8>, usize)> = Vec::new();
        for (k, &tid) in &self.keys {
            if let Some(t) = self.pages.get(tid.0 as usize).and_then(|p| p.tup(tid.1))
                && matches!(t.status(v), Htsv::Live | Htsv::DeleteInProgress | Htsv::InsertInProgress)
            {
                out.push((tid, k, t.len as usize));
            }
        }
        out.sort_by_key(|x| x.0);
        out.into_iter().map(|(_, k, l)| (k.clone(), l)).collect()
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
        h.keys.insert(k, tid);
        tids.push(tid);
    }
    (h, tids)
}
