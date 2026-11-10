//! A heap page's line pointers, packed (`Page::lps`): 8 bytes each. A model holds one per
//! row version, and nearly all of them are settled: inserted by a transaction every
//! snapshot sees as committed (or frozen), not deleted or updated since. Such a version
//! needs only its row, length, previous version, heap-only flag and command id; its xmin
//! is `SETTLED_XMIN` once settled (`Lps::settle`; nothing reads a settled version's xid,
//! only that it is committed for everyone), or the frozen one it was made with. Any other
//! version (deleted or updated, a field too wide) is kept whole in the page's `full` list
//! until it settles or pruning frees it. A version just written takes the small form too
//! when its xmin is the page's own (`Lps::own`: one normal xmin per page, the one a load or
//! a run of inserts puts on it), so a COPY's rows are 8 bytes each from the start and settle
//! by rewriting their xmin's two bits.
//! Readers see `Lp` values either way (`get`, `iter`); writers replace them (`set`,
//! `update`), and each slot takes the smallest form that holds its value.

use super::{Lp, Tup, Vis, Xs};
use crate::txn::xids::FIRST_NORMAL_XID;

/// The xmin a settled version reads back with (BootstrapTransactionId: committed, below
/// every horizon, and not one a modelled row is inserted with: those are frozen, 0, or
/// normal). Versions are matched to the xmax of the one they replaced only while it may
/// still matter (pruning's chain check accepts it: a settled version's inserter was the
/// updater).
pub const SETTLED_XMIN: u64 = 1;

/// A slot: the tag in the top two bits.
#[derive(Clone, Copy, Debug, Default)]
struct Slot(u64);

const TAG: u32 = 62;
const T_SMALL: u64 = 0;
const T_FULL: u64 = 1;
const T_REDIRECT: u64 = 2;
const T_EMPTY: u64 = 3;

// A small slot: row (32 bits), len (13), prev_off (9), heap_only (1), xmin (2: 0, 1 or 2,
// or `OWN_XMIN`), cid (5).
const LEN_AT: u32 = 32;
const PREV_AT: u32 = 45;
const HOT_AT: u32 = 54;
const XMIN_AT: u32 = 55;
const CID_AT: u32 = 57;
/// A small slot's xmin code for the page's own xmin (`Lps::own`; 3 is FirstNormalTransactionId,
/// no special xid's code).
const OWN_XMIN: u64 = 3;

fn bits(v: u64, at: u32, n: u32) -> u64 {
    (v >> at) & ((1 << n) - 1)
}

impl Slot {
    fn tag(self) -> u64 {
        self.0 >> TAG
    }

    /// The small form of `t` with xmin code `xmin` (its own xmin, or `OWN_XMIN`), if it has one.
    fn small(t: &Tup, xmin: u64) -> Option<Slot> {
        if t.xmax != 0 || t.next != 0 || t.hot_updated {
            return None;
        }
        if t.len >= 1 << 13 || t.prev_off >= 1 << 9 || t.cid >= 1 << 5 {
            return None;
        }
        Some(Slot(
            t.row as u64
                | (t.len as u64) << LEN_AT
                | (t.prev_off as u64) << PREV_AT
                | (t.heap_only as u64) << HOT_AT
                | xmin << XMIN_AT
                | (t.cid as u64) << CID_AT,
        ))
    }

    fn owns(self) -> bool {
        self.tag() == T_SMALL && bits(self.0, XMIN_AT, 2) == OWN_XMIN
    }

    /// The version a small slot holds (`own`: the page's own xmin).
    fn small_tup(self, own: u64) -> Tup {
        let v = self.0;
        let xmin = bits(v, XMIN_AT, 2);
        Tup {
            xmin: if xmin == OWN_XMIN { own } else { xmin },
            xmax: 0,
            cid: bits(v, CID_AT, 5) as u32,
            row: v as u32,
            prev_off: bits(v, PREV_AT, 9) as u16,
            next: 0,
            len: bits(v, LEN_AT, 13) as u16,
            heap_only: bits(v, HOT_AT, 1) != 0,
            hot_updated: false,
        }
    }

    fn of(tag: u64, low: u64) -> Slot {
        Slot(tag << TAG | low)
    }

    fn low(self) -> u64 {
        self.0 & ((1 << TAG) - 1)
    }
}

/// A line pointer as `Lps::shapes` gives it.
#[derive(Clone, Copy, Debug)]
pub enum Shape {
    Unused,
    Dead,
    Redirect,
    /// A version whose xmin is committed for everyone (settled or frozen), not deleted or
    /// updated: live.
    Settled { heap_only: bool },
    /// Any other version.
    Version { heap_only: bool },
}

const UNUSED: Slot = Slot(T_EMPTY << TAG | 1);
const DEAD: Slot = Slot(T_EMPTY << TAG);

#[derive(Clone, Debug, Default)]
pub(super) struct Lps {
    slots: Vec<Slot>,
    /// The versions a small slot can't hold, in no order.
    full: Vec<Tup>,
    /// The normal xmin small slots of code `OWN_XMIN` hold, while `own_n` (> 0) slots do.
    own: u64,
    own_n: u32,
    /// A page whose slots are all small, settled or frozen, and alike but for their rows,
    /// which follow each other (a load's, a layout's), keeps them as one run while `slots`
    /// is empty: line pointers 1..=`run_n` are `run` with its row, the next row, and so on
    /// (`freeze`; a change lays them out again first, `thaw`). Most of a big table's pages:
    /// no slot each.
    run_n: u16,
    run: Slot,
}

impl Lps {
    pub fn len(&self) -> usize {
        self.slots.len() + self.run_n as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Slot `i`, of the run or laid out.
    fn slot_at(&self, i: usize) -> Option<Slot> {
        if self.run_n > 0 {
            return (i < self.run_n as usize).then(|| Slot(self.run.0 + i as u64));
        }
        self.slots.get(i).copied()
    }

    fn all_slots(&self) -> impl Iterator<Item = Slot> + '_ {
        (0..self.run_n as u64).map(|i| Slot(self.run.0 + i)).chain(self.slots.iter().copied())
    }

    /// The run's slots laid out, for a change.
    fn thaw(&mut self) {
        if self.run_n > 0 {
            self.slots = self.all_slots().collect();
            self.run_n = 0;
        }
    }

    /// The slots as one run (`run`), when they can be.
    pub fn freeze(&mut self) {
        let Some(&first) = self.slots.first() else { return };
        if first.tag() != T_SMALL || first.owns() || self.own_n > 0 || !self.full.is_empty() {
            return;
        }
        // (Rows past u32::MAX - 1 would carry into the length's bits; NO_ROW is none's.)
        let n = self.slots.len();
        if n > u16::MAX as usize || (first.0 as u32) as u64 + n as u64 >= u32::MAX as u64 {
            return;
        }
        if self.slots.iter().enumerate().any(|(i, s)| s.0 != first.0 + i as u64) {
            return;
        }
        self.run = first;
        self.run_n = n as u16;
        self.slots = Vec::new();
    }

    fn view(&self, s: Slot) -> Lp {
        match s.tag() {
            T_SMALL => Lp::Normal(s.small_tup(self.own)),
            T_FULL => Lp::Normal(self.full[s.low() as usize]),
            T_REDIRECT => Lp::Redirect(s.low() as u16),
            _ if s.low() == 1 => Lp::Unused,
            _ => Lp::Dead,
        }
    }

    /// Line pointer `i` (0-based).
    pub fn get(&self, i: usize) -> Option<Lp> {
        self.slot_at(i).map(|s| self.view(s))
    }

    pub fn iter(&self) -> impl Iterator<Item = Lp> + '_ {
        self.all_slots().map(|s| self.view(s))
    }

    /// What each line pointer is, without making its version (`Heap::prune_with`).
    pub fn shapes(&self) -> impl Iterator<Item = Shape> + '_ {
        self.all_slots().map(|s| match s.tag() {
            T_SMALL => {
                let heap_only = bits(s.0, HOT_AT, 1) != 0;
                match bits(s.0, XMIN_AT, 2) {
                    OWN_XMIN => Shape::Version { heap_only },
                    _ => Shape::Settled { heap_only },
                }
            }
            T_FULL => Shape::Version { heap_only: self.full[s.low() as usize].heap_only },
            T_REDIRECT => Shape::Redirect,
            _ if s.low() == 1 => Shape::Unused,
            _ => Shape::Dead,
        })
    }

    /// Whether line pointer `i` is in use (not LP_UNUSED; past the end: not).
    pub fn used(&self, i: usize) -> bool {
        self.slot_at(i).is_some_and(|s| s.0 != UNUSED.0)
    }

    pub fn is_normal(&self, i: usize) -> bool {
        self.slot_at(i).is_some_and(|s| s.tag() <= T_FULL)
    }

    /// Normal line pointers.
    pub fn normal(&self) -> usize {
        self.run_n as usize + self.slots.iter().filter(|s| s.tag() <= T_FULL).count()
    }

    pub fn first_unused(&self) -> Option<usize> {
        self.slots.iter().position(|s| s.0 == UNUSED.0)
    }

    pub fn any_unused(&self) -> bool {
        self.first_unused().is_some()
    }

    fn slot(&mut self, lp: Lp) -> Slot {
        match lp {
            Lp::Normal(t) => self.small(&t).unwrap_or_else(|| {
                self.full.push(t);
                Slot::of(T_FULL, self.full.len() as u64 - 1)
            }),
            Lp::Redirect(o) => Slot::of(T_REDIRECT, o as u64),
            Lp::Dead => DEAD,
            Lp::Unused => UNUSED,
        }
    }

    /// The small form of `t`, if it has one: a normal xmin only as the page's own (which it
    /// becomes when no slot holds one).
    fn small(&mut self, t: &Tup) -> Option<Slot> {
        if t.xmin < FIRST_NORMAL_XID {
            return Slot::small(t, t.xmin);
        }
        if self.own_n > 0 && self.own != t.xmin {
            return None;
        }
        let s = Slot::small(t, OWN_XMIN)?;
        self.own = t.xmin;
        self.own_n += 1;
        Some(s)
    }

    /// Slot `s` no longer holds what it held: a whole version goes from `full` (the last
    /// one moves into its place).
    fn release(&mut self, s: Slot) {
        if s.owns() {
            self.own_n -= 1;
        }
        if s.tag() != T_FULL {
            return;
        }
        let (i, last) = (s.low(), self.full.len() as u64 - 1);
        self.full.swap_remove(i as usize);
        if i != last
            && let Some(m) = self.slots.iter_mut().find(|m| m.tag() == T_FULL && m.low() == last)
        {
            *m = Slot::of(T_FULL, i);
        }
        if self.full.is_empty() && self.full.capacity() > 8 {
            self.full = Vec::new();
        }
    }

    pub fn set(&mut self, i: usize, lp: Lp) {
        self.thaw();
        let old = std::mem::replace(&mut self.slots[i], UNUSED);
        self.release(old);
        self.slots[i] = self.slot(lp);
    }

    pub fn push(&mut self, lp: Lp) {
        self.thaw();
        let s = self.slot(lp);
        self.slots.push(s);
    }

    pub fn pop(&mut self) {
        self.thaw();
        if let Some(s) = self.slots.pop() {
            self.release(s);
        }
    }

    /// Change the version at line pointer `i` (a normal one: false otherwise).
    pub fn update(&mut self, i: usize, f: impl FnOnce(&mut Tup)) -> bool {
        self.thaw();
        match self.slots.get(i).copied() {
            Some(s) if s.tag() == T_FULL => {
                let j = s.low() as usize;
                f(&mut self.full[j]);
                let t = self.full[j];
                if let Some(m) = self.small(&t) {
                    self.slots[i] = m;
                    self.release(s);
                }
                true
            }
            Some(s) if s.tag() == T_SMALL => {
                let mut t = s.small_tup(self.own);
                f(&mut t);
                self.set(i, Lp::Normal(t));
                true
            }
            _ => false,
        }
    }

    /// The version at line pointer `i` (a normal one) is of row `row`.
    pub fn set_row(&mut self, i: usize, row: u32) {
        self.thaw();
        match self.slots.get_mut(i) {
            Some(s) if s.tag() == T_SMALL => s.0 = (s.0 & !(u32::MAX as u64)) | row as u64,
            Some(s) if s.tag() == T_FULL => self.full[s.low() as usize].row = row,
            _ => {}
        }
    }

    /// Versions whose inserter every snapshot now sees committed, not deleted or updated,
    /// take the small form (their xmin `SETTLED_XMIN`). Returns the oldest xmin of those
    /// left that may settle later (u64::MAX: none).
    pub fn settle(&mut self, v: &Vis) -> u64 {
        // Which whole versions settle (`full` by index), and the oldest xmin of those that
        // may later.
        let mut oldest = u64::MAX;
        // (The page's own xmin's versions: their two bits rewritten.)
        if self.own_n > 0 {
            if v.removable(self.own) && v.state(self.own) == Xs::Committed {
                for s in self.slots.iter_mut().filter(|s| s.owns()) {
                    s.0 = (s.0 & !(3 << XMIN_AT)) | (SETTLED_XMIN << XMIN_AT);
                }
                self.own_n = 0;
            } else {
                oldest = self.own;
            }
        }
        let mut any = false;
        let settles = |t: &Tup| t.xmax == 0 && t.xmin >= FIRST_NORMAL_XID;
        for t in self.full.iter().filter(|&t| settles(t)) {
            if !v.removable(t.xmin) || v.state(t.xmin) != Xs::Committed {
                oldest = oldest.min(t.xmin);
            } else {
                any = true;
            }
        }
        if !any {
            if oldest == u64::MAX {
                self.freeze();
            }
            return oldest;
        }
        // One pass over the slots: settled versions take their small form, the rest of
        // `full` is laid out again in slot order (a version at a time would move the last
        // whole one into each freed place, and look for its slot).
        let old = std::mem::take(&mut self.full);
        let mut keep = Vec::new();
        for s in &mut self.slots {
            if s.tag() != T_FULL {
                continue;
            }
            let mut t = old[s.low() as usize];
            if settles(&t) && v.removable(t.xmin) && v.state(t.xmin) == Xs::Committed {
                let x = std::mem::replace(&mut t.xmin, SETTLED_XMIN);
                if let Some(m) = Slot::small(&t, SETTLED_XMIN) {
                    *s = m;
                    continue;
                }
                t.xmin = x;
            }
            *s = Slot::of(T_FULL, keep.len() as u64);
            keep.push(t);
        }
        self.full = keep;
        if oldest == u64::MAX {
            self.freeze();
        }
        oldest
    }

    /// Every version's row changed by `f`.
    pub fn rows_mut(&mut self, mut f: impl FnMut(&mut u32)) {
        self.thaw();
        for s in &mut self.slots {
            if s.tag() == T_SMALL {
                let mut r = s.0 as u32;
                f(&mut r);
                s.0 = (s.0 & !(u32::MAX as u64)) | r as u64;
            }
        }
        self.full.iter_mut().for_each(|t| f(&mut t.row));
    }

    /// The bytes it holds (for memory accounting).
    pub fn bytes(&self) -> usize {
        self.slots.capacity() * size_of::<Slot>() + self.full.capacity() * size_of::<Tup>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tup(xmin: u64, xmax: u64, cid: u32, len: u16, prev_off: u16) -> Tup {
        Tup { xmin, xmax, cid, row: 7, prev_off, next: 0, len, heap_only: prev_off != 0, hot_updated: false }
    }

    #[test]
    fn settling_waits_for_snapshots_and_preserves_tuple_fields() {
        let xids = crate::txn::xids::Xids::local();
        let reader = xids.assign();
        // (Two writers: the first's versions small from the start, as the page's own xmin;
        // the second's whole.)
        let writers = [xids.assign(), xids.assign()];
        let mut l = Lps::default();
        for i in 0..226 {
            l.push(Lp::Normal(tup(writers[i % 2], 0, i as u32 % 32, 40, i as u16)));
        }
        assert_eq!(l.full.len(), 113);
        for w in writers {
            xids.finish(w, true);
        }
        let before = l.bytes();
        l.settle(&Vis::new(&xids));
        assert!(l.iter().enumerate().all(|(i, lp)| matches!(lp, Lp::Normal(t) if t.xmin == writers[i % 2])));
        xids.finish(reader, true);
        assert_eq!(l.settle(&Vis::new(&xids)), u64::MAX);
        assert!(l.bytes() < before / 2, "whole versions must be released");
        for (i, lp) in l.iter().enumerate() {
            let Lp::Normal(t) = lp else { panic!("tuple lost") };
            assert_eq!(t.xmin, SETTLED_XMIN);
            assert_eq!(t.cid, i as u32 % 32);
            assert_eq!(t.prev_off, i as u16);
            assert_eq!(t.len, 40);
            assert_eq!(t.row, 7);
            assert_eq!(t.heap_only, i != 0);
        }
        let aborted = xids.assign();
        l.push(Lp::Normal(tup(aborted, 0, 0, 40, 0)));
        xids.finish(aborted, false);
        l.settle(&Vis::new(&xids));
        assert!(matches!(l.get(226), Some(Lp::Normal(t)) if t.status(&Vis::new(&xids)) == super::super::Htsv::Dead));
    }

    /// Every line pointer reads back as it was set, whichever form its slot took, through
    /// sets, updates, pops and releases of whole versions.
    #[test]
    fn line_pointers_read_back() {
        let mut l = Lps::default();
        let mut want: Vec<Lp> = Vec::new();
        let vals = [
            Lp::Normal(tup(0, 0, 0, 120, 0)),
            Lp::Normal(tup(SETTLED_XMIN, 0, 31, 8160, 291)),
            Lp::Normal(tup(5000, 0, 0, 40, 0)),
            Lp::Normal(tup(1, 900, 3, 40, 2)),
            Lp::Normal(tup(1, 0, 40, 40, 0)),
            Lp::Normal(tup(1, 0, 0, u16::MAX, 0)),
            Lp::Redirect(17),
            Lp::Dead,
            Lp::Unused,
        ];
        let mut x = 1u64;
        for _ in 0..2000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let v = vals[(x >> 33) as usize % vals.len()];
            match (x >> 50) % 4 {
                0 if !want.is_empty() => {
                    let i = (x >> 20) as usize % want.len();
                    l.set(i, v);
                    want[i] = v;
                }
                1 if !want.is_empty() => {
                    let i = (x >> 20) as usize % want.len();
                    let done = l.update(i, |t| t.xmax = 0);
                    if let Lp::Normal(t) = &mut want[i] {
                        t.xmax = 0;
                        assert!(done);
                    }
                }
                2 if !want.is_empty() => {
                    l.pop();
                    want.pop();
                }
                _ => {
                    l.push(v);
                    want.push(v);
                }
            }
            assert_eq!(format!("{:?}", l.iter().collect::<Vec<_>>()), format!("{want:?}"));
        }
    }

    /// A page of settled versions of rows that follow each other is one run, read as its
    /// slots were, and laid out again by the first change; one whose versions differ (a
    /// length, a row out of turn, a version not settled) stays as it is.
    #[test]
    fn settled_pages_are_runs() {
        let xids = crate::txn::xids::Xids::local();
        let writer = xids.assign();
        let mut l = Lps::default();
        let version = |row: u32, xmin: u64| Lp::Normal(Tup { row, ..tup(xmin, 0, 3, 120, 0) });
        for r in 0..61 {
            l.push(version(1000 + r, writer));
        }
        let want = |l: &Lps| format!("{:?}", l.iter().collect::<Vec<_>>());
        let before = want(&l);
        l.freeze();
        assert_eq!(l.run_n, 0, "not settled yet");
        xids.finish(writer, true);
        assert_eq!(l.settle(&Vis::new(&xids)), u64::MAX);
        assert_eq!((l.run_n, l.slots.len(), l.bytes()), (61, 0, 0));
        let settled = before.replace(&format!("xmin: {writer},"), &format!("xmin: {SETTLED_XMIN},"));
        assert_eq!(want(&l), settled);
        assert_eq!((l.len(), l.normal(), l.used(60), l.used(61), l.is_normal(0)), (61, 61, true, false, true));
        assert!(matches!(l.get(5), Some(Lp::Normal(t)) if t.row == 1005 && t.len == 120 && t.cid == 3));
        l.update(7, |t| t.xmax = 9);
        assert_eq!((l.run_n, l.slots.len()), (0, 61));
        assert!(matches!(l.get(7), Some(Lp::Normal(t)) if t.xmax == 9 && t.row == 1007));
        assert!(matches!(l.get(8), Some(Lp::Normal(t)) if t.xmax == 0 && t.row == 1008));
        l.freeze();
        assert_eq!(l.run_n, 0, "a version deleted");
        let mut m = Lps::default();
        m.push(version(5, 0));
        m.push(version(7, 0));
        m.freeze();
        assert_eq!(m.run_n, 0, "rows out of turn");
    }
}
