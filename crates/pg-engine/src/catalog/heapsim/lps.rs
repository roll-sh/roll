//! A heap page's line pointers, packed (`Page::lps`): 8 bytes each. A model holds one per
//! row version, and nearly all of them are settled: inserted by a transaction every
//! snapshot sees as committed (or frozen), not deleted or updated since. Such a version
//! needs only its row, length, previous version, heap-only flag and command id; its xmin
//! is `SETTLED_XMIN` once settled (`Lps::settle`; nothing reads a settled version's xid,
//! only that it is committed for everyone), or the frozen one it was made with. Any other
//! version (just written, deleted or updated, a field too wide) is kept whole in the page's
//! `full` list until it settles or pruning frees it.
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
#[derive(Clone, Copy, Debug)]
struct Slot(u64);

const TAG: u32 = 62;
const T_SMALL: u64 = 0;
const T_FULL: u64 = 1;
const T_REDIRECT: u64 = 2;
const T_EMPTY: u64 = 3;

// A small slot: row (32 bits), len (13), prev_off (9), heap_only (1), xmin (2: 0, 1 or 2),
// cid (5).
const LEN_AT: u32 = 32;
const PREV_AT: u32 = 45;
const HOT_AT: u32 = 54;
const XMIN_AT: u32 = 55;
const CID_AT: u32 = 57;

fn bits(v: u64, at: u32, n: u32) -> u64 {
    (v >> at) & ((1 << n) - 1)
}

impl Slot {
    fn tag(self) -> u64 {
        self.0 >> TAG
    }

    /// The small form of `t`, if it has one.
    fn small(t: &Tup) -> Option<Slot> {
        if t.xmax != 0 || t.next != 0 || t.hot_updated || t.xmin >= FIRST_NORMAL_XID {
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
                | t.xmin << XMIN_AT
                | (t.cid as u64) << CID_AT,
        ))
    }

    fn small_tup(self) -> Tup {
        let v = self.0;
        Tup {
            xmin: bits(v, XMIN_AT, 2),
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

const UNUSED: Slot = Slot(T_EMPTY << TAG | 1);
const DEAD: Slot = Slot(T_EMPTY << TAG);

#[derive(Clone, Debug, Default)]
pub(super) struct Lps {
    slots: Vec<Slot>,
    /// The versions a small slot can't hold, in no order.
    full: Vec<Tup>,
}

impl Lps {
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    fn view(&self, s: Slot) -> Lp {
        match s.tag() {
            T_SMALL => Lp::Normal(s.small_tup()),
            T_FULL => Lp::Normal(self.full[s.low() as usize]),
            T_REDIRECT => Lp::Redirect(s.low() as u16),
            _ if s.low() == 1 => Lp::Unused,
            _ => Lp::Dead,
        }
    }

    /// Line pointer `i` (0-based).
    pub fn get(&self, i: usize) -> Option<Lp> {
        self.slots.get(i).map(|&s| self.view(s))
    }

    pub fn iter(&self) -> impl Iterator<Item = Lp> + '_ {
        self.slots.iter().map(|&s| self.view(s))
    }

    /// Whether line pointer `i` is in use (not LP_UNUSED; past the end: not).
    pub fn used(&self, i: usize) -> bool {
        self.slots.get(i).is_some_and(|s| s.0 != UNUSED.0)
    }

    pub fn is_normal(&self, i: usize) -> bool {
        self.slots.get(i).is_some_and(|s| s.tag() <= T_FULL)
    }

    /// Normal line pointers.
    pub fn normal(&self) -> usize {
        self.slots.iter().filter(|s| s.tag() <= T_FULL).count()
    }

    pub fn first_unused(&self) -> Option<usize> {
        self.slots.iter().position(|s| s.0 == UNUSED.0)
    }

    pub fn any_unused(&self) -> bool {
        self.first_unused().is_some()
    }

    fn slot(&mut self, lp: Lp) -> Slot {
        match lp {
            Lp::Normal(t) => Slot::small(&t).unwrap_or_else(|| {
                self.full.push(t);
                Slot::of(T_FULL, self.full.len() as u64 - 1)
            }),
            Lp::Redirect(o) => Slot::of(T_REDIRECT, o as u64),
            Lp::Dead => DEAD,
            Lp::Unused => UNUSED,
        }
    }

    /// Slot `s` no longer holds what it held: a whole version goes from `full` (the last
    /// one moves into its place).
    fn release(&mut self, s: Slot) {
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
        let old = std::mem::replace(&mut self.slots[i], UNUSED);
        self.release(old);
        self.slots[i] = self.slot(lp);
    }

    pub fn push(&mut self, lp: Lp) {
        let s = self.slot(lp);
        self.slots.push(s);
    }

    pub fn pop(&mut self) {
        if let Some(s) = self.slots.pop() {
            self.release(s);
        }
    }

    /// Change the version at line pointer `i` (a normal one: false otherwise).
    pub fn update(&mut self, i: usize, f: impl FnOnce(&mut Tup)) -> bool {
        match self.slots.get(i).copied() {
            Some(s) if s.tag() == T_FULL => {
                let j = s.low() as usize;
                f(&mut self.full[j]);
                if let Some(m) = Slot::small(&self.full[j]) {
                    self.slots[i] = m;
                    self.release(s);
                }
                true
            }
            Some(s) if s.tag() == T_SMALL => {
                let mut t = s.small_tup();
                f(&mut t);
                self.set(i, Lp::Normal(t));
                true
            }
            _ => false,
        }
    }

    /// Versions whose inserter every snapshot now sees committed, not deleted or updated,
    /// take the small form (their xmin `SETTLED_XMIN`). Returns the oldest xmin of those
    /// left that may settle later (u64::MAX: none).
    pub fn settle(&mut self, v: &Vis) -> u64 {
        let mut oldest = u64::MAX;
        let mut i = 0;
        while i < self.slots.len() {
            let s = self.slots[i];
            i += 1;
            if s.tag() != T_FULL {
                continue;
            }
            let t = &mut self.full[s.low() as usize];
            if t.xmax != 0 || t.xmin < FIRST_NORMAL_XID {
                continue;
            }
            if !v.removable(t.xmin) || v.state(t.xmin) != Xs::Committed {
                oldest = oldest.min(t.xmin);
                continue;
            }
            let x = std::mem::replace(&mut t.xmin, SETTLED_XMIN);
            match Slot::small(t) {
                Some(m) => {
                    self.slots[i - 1] = m;
                    self.release(s);
                }
                None => t.xmin = x,
            }
        }
        oldest
    }

    /// Every version's row changed by `f`.
    pub fn rows_mut(&mut self, mut f: impl FnMut(&mut u32)) {
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
        let writer = xids.assign();
        let mut l = Lps::default();
        for i in 0..226 {
            l.push(Lp::Normal(tup(writer, 0, i % 32, 40, i as u16)));
        }
        xids.finish(writer, true);
        let before = l.bytes();
        l.settle(&Vis::new(&xids));
        assert!(l.iter().all(|lp| matches!(lp, Lp::Normal(t) if t.xmin == writer)));
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
}
