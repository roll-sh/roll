//! Postgres's in-memory sorting, so that rows with equal sort keys come out in the order
//! Postgres puts them in: its quicksort (lib/sort_template.h: insertion sort below 7
//! elements, the presorted check, median-of-three / ninther pivots, Bentley-McIlroy
//! three-way partitioning) and tuplesort's bounded heap for a LIMIT above the sort
//! (make_bounded_heap / sort_bounded_heap).

use std::cmp::Ordering;

/// A comparison that may fail: the first error is kept and the sort carries on with
/// "equal".
pub struct Cmp<'a, T> {
    f: &'a mut dyn FnMut(&T, &T) -> crate::error::Result<Ordering>,
    pub err: Option<crate::error::Error>,
}

impl<'a, T> Cmp<'a, T> {
    pub fn new(f: &'a mut dyn FnMut(&T, &T) -> crate::error::Result<Ordering>) -> Self {
        Cmp { f, err: None }
    }

    fn cmp(&mut self, a: &T, b: &T) -> i32 {
        if self.err.is_some() {
            return 0;
        }
        match (self.f)(a, b) {
            Ok(Ordering::Less) => -1,
            Ok(Ordering::Equal) => 0,
            Ok(Ordering::Greater) => 1,
            Err(e) => {
                self.err = Some(e);
                0
            }
        }
    }
}

fn med3<T>(v: &[T], a: usize, b: usize, c: usize, cmp: &mut Cmp<T>) -> usize {
    if cmp.cmp(&v[a], &v[b]) < 0 {
        if cmp.cmp(&v[b], &v[c]) < 0 {
            b
        } else if cmp.cmp(&v[a], &v[c]) < 0 {
            c
        } else {
            a
        }
    } else if cmp.cmp(&v[b], &v[c]) > 0 {
        b
    } else if cmp.cmp(&v[a], &v[c]) < 0 {
        a
    } else {
        c
    }
}

fn swapn<T>(v: &mut [T], a: usize, b: usize, n: usize) {
    for i in 0..n {
        v.swap(a + i, b + i);
    }
}

/// sort_template.h's ST_SORT over `v[a..a + n]`.
fn qsort_range<T>(v: &mut [T], mut a: usize, mut n: usize, cmp: &mut Cmp<T>) {
    loop {
        if n < 7 {
            for pm in a + 1..a + n {
                let mut pl = pm;
                while pl > a && cmp.cmp(&v[pl - 1], &v[pl]) > 0 {
                    v.swap(pl, pl - 1);
                    pl -= 1;
                }
            }
            return;
        }
        let mut presorted = true;
        for pm in a + 1..a + n {
            if cmp.cmp(&v[pm - 1], &v[pm]) > 0 {
                presorted = false;
                break;
            }
        }
        if presorted {
            return;
        }
        let mut pm = a + n / 2;
        if n > 7 {
            let mut pl = a;
            let mut pn = a + n - 1;
            if n > 40 {
                let d = n / 8;
                pl = med3(v, pl, pl + d, pl + 2 * d, cmp);
                pm = med3(v, pm - d, pm, pm + d, cmp);
                pn = med3(v, pn - 2 * d, pn - d, pn, cmp);
            }
            pm = med3(v, pl, pm, pn, cmp);
        }
        v.swap(a, pm);
        let mut pa = a + 1;
        let mut pb = a + 1;
        // pc / pd may step below a + 1; keep them as signed positions.
        let mut pc = (a + n - 1) as isize;
        let mut pd = (a + n - 1) as isize;
        loop {
            while pb as isize <= pc {
                let r = cmp.cmp(&v[pb], &v[a]);
                if r > 0 {
                    break;
                }
                if r == 0 {
                    v.swap(pa, pb);
                    pa += 1;
                }
                pb += 1;
            }
            while pb as isize <= pc {
                let r = cmp.cmp(&v[pc as usize], &v[a]);
                if r < 0 {
                    break;
                }
                if r == 0 {
                    v.swap(pc as usize, pd as usize);
                    pd -= 1;
                }
                pc -= 1;
            }
            if pb as isize > pc {
                break;
            }
            v.swap(pb, pc as usize);
            pb += 1;
            pc -= 1;
        }
        let pn = a + n;
        let d1 = (pa - a).min(pb - pa);
        swapn(v, a, pb - d1, d1);
        let d1 = ((pd - pc) as usize).min(pn - pd as usize - 1);
        swapn(v, pb, pn - d1, d1);
        let d1 = pb - pa;
        let d2 = (pd - pc) as usize;
        if d1 <= d2 {
            if d1 > 1 {
                qsort_range(v, a, d1, cmp);
            }
            if d2 > 1 {
                a = pn - d2;
                n = d2;
                continue;
            }
        } else {
            if d2 > 1 {
                qsort_range(v, pn - d2, d2, cmp);
            }
            if d1 > 1 {
                n = d1;
                continue;
            }
        }
        return;
    }
}

/// pg_qsort / qsort_tuple: Postgres's quicksort of `v`.
pub fn pg_qsort<T>(v: &mut [T], cmp: &mut Cmp<T>) {
    let n = v.len();
    if n > 1 {
        qsort_range(v, 0, n, cmp);
    }
}

/// tuplesort_heap_insert into the (reversed) heap `heap`.
fn heap_insert<T>(heap: &mut Vec<T>, t: T, cmp: &mut Cmp<T>) {
    heap.push(t);
    let mut j = heap.len() - 1;
    while j > 0 {
        let i = (j - 1) >> 1;
        // Reversed direction: the largest is at the root.
        if -cmp.cmp(&heap[j], &heap[i]) >= 0 {
            break;
        }
        heap.swap(i, j);
        j = i;
    }
}

/// tuplesort_heap_replace_top: `t` replaces the root and sifts down (reversed direction).
fn heap_replace_top<T>(heap: &mut [T], n: usize, t: T, cmp: &mut Cmp<T>) {
    // The hole walks down; `t` goes where it stops.
    let mut i = 0usize;
    heap[0] = t;
    loop {
        let mut j = 2 * i + 1;
        if j >= n {
            break;
        }
        if j + 1 < n && -cmp.cmp(&heap[j], &heap[j + 1]) > 0 {
            j += 1;
        }
        if -cmp.cmp(&heap[i], &heap[j]) <= 0 {
            break;
        }
        heap.swap(i, j);
        i = j;
    }
}

/// tuplesort's order of `items` (the first `bound` of them when bounded, as a LIMIT above
/// the sort asks): a quicksort, or the bounded heap once the input exceeds twice the
/// bound.
pub fn tuplesort<T>(items: Vec<T>, bound: Option<usize>, cmp: &mut Cmp<T>) -> Vec<T> {
    match bound {
        Some(b) if items.len() > b.saturating_mul(2) => {
            let mut s = Bounded::new(b);
            for t in items {
                s.put(t, cmp);
            }
            s.finish(cmp)
        }
        _ => {
            let mut items = items;
            pg_qsort(&mut items, cmp);
            if let Some(b) = bound {
                items.truncate(b);
            }
            items
        }
    }
}

/// A bounded tuplesort fed an item at a time (a LIMIT's sort reading its input), keeping
/// only what may still be among the first `bound`: `finish` returns what `tuplesort` of
/// all the items put returns.
pub struct Bounded<T> {
    bound: usize,
    /// TSS_INITIAL's items, then (`heap`) the bounded heap.
    items: Vec<T>,
    heap: bool,
    /// Items not put because the bounded heap would have discarded them (`skip`).
    skipped: usize,
}

impl<T> Bounded<T> {
    pub fn new(bound: usize) -> Bounded<T> {
        Bounded { bound, items: Vec::new(), heap: false, skipped: 0 }
    }

    /// `n` more items came, which the bounded heap would have discarded as they came (each
    /// not smaller than the bound's worth of items before it): only their number counts,
    /// for the heap being used at all (more than twice the bound in all). The heap's
    /// result is the same without them: a discarded item changes nothing.
    pub fn skip(&mut self, n: usize) {
        self.skipped += n;
    }

    /// tuplesort_puttuple. An item the bounded heap discards at once comes back (its
    /// caller may reuse it).
    pub fn put(&mut self, t: T, cmp: &mut Cmp<T>) -> Option<T> {
        if self.heap {
            return self.feed(t, cmp);
        }
        self.items.push(t);
        // TSS_INITIAL collects 2 * bound + 1 tuples, then make_bounded_heap.
        if self.items.len() > self.bound.saturating_mul(2) {
            self.heap = true;
            let items = std::mem::replace(&mut self.items, Vec::with_capacity(self.bound));
            for t in items {
                self.feed(t, cmp);
            }
        }
        None
    }

    fn feed(&mut self, t: T, cmp: &mut Cmp<T>) -> Option<T> {
        let heap = &mut self.items;
        if heap.len() < self.bound {
            heap_insert(heap, t, cmp);
        } else if self.bound == 0 || -cmp.cmp(&t, &heap[0]) <= 0 {
            // Not smaller than everything kept: discarded.
            return Some(t);
        } else {
            let n = heap.len();
            heap_replace_top(heap, n, t, cmp);
        }
        None
    }

    /// tuplesort_performsort and the items in order.
    pub fn finish(mut self, cmp: &mut Cmp<T>) -> Vec<T> {
        if !self.heap && self.items.len() + self.skipped > self.bound.saturating_mul(2) {
            // (The items skipped made it more than twice the bound: the heap's, fed the
            // items put in their order.)
            self.heap = true;
            let items = std::mem::replace(&mut self.items, Vec::with_capacity(self.bound));
            for t in items {
                self.feed(t, cmp);
            }
        }
        if !self.heap {
            return tuplesort(self.items, Some(self.bound), cmp);
        }
        let mut heap = self.items;
        // sort_bounded_heap: delete-top repeatedly, each freed slot taking the old top.
        let mut n = heap.len();
        while n > 1 {
            heap.swap(0, n - 1);
            n -= 1;
            // The element now at 0 (the former last) sifts down within the first n.
            let mut i = 0usize;
            loop {
                let mut j = 2 * i + 1;
                if j >= n {
                    break;
                }
                if j + 1 < n && -cmp.cmp(&heap[j], &heap[j + 1]) > 0 {
                    j += 1;
                }
                if -cmp.cmp(&heap[i], &heap[j]) <= 0 {
                    break;
                }
                heap.swap(i, j);
                i = j;
            }
        }
        heap
    }
}

/// An incremental sort's sorts, full ones (0) and of presorted prefix groups (1): how many,
/// the methods used (1 top-N heapsort, 2 quicksort), and the rows each kept.
#[derive(Clone, Copy, Debug, Default)]
pub struct IncrStats {
    pub groups: [u64; 2],
    pub methods: [u8; 2],
    pub kept_total: [u64; 2],
    pub kept_max: [u64; 2],
}

/// DEFAULT_MIN_GROUP_SIZE and DEFAULT_MAX_FULL_SORT_GROUP_SIZE.
const MIN_GROUP: usize = 32;
const MAX_FULL_SORT_GROUP: usize = 2 * MIN_GROUP;

/// nodeIncrementalSort.c's order of `input`, already sorted on a prefix of the keys:
/// batches of at least 32 rows (to the end of the prefix group then reached) sorted on all
/// keys (`full`), and once such a batch grows past 64 rows, each prefix group (`same`)
/// sorted on the rest (`suffix`) on its own. `bound`: a LIMIT above wants only that many.
/// `input` is read only as far as that takes; `stats` counts the sorts done
/// (instrumentSortedGroup).
pub fn incremental_sort<T: Clone>(
    input: impl IntoIterator<Item = T>,
    bound: Option<usize>,
    full: &mut Cmp<T>,
    suffix: &mut Cmp<T>,
    same: &mut dyn FnMut(&T, &T) -> bool,
    stats: &mut IncrStats,
) -> Vec<T> {
    let mut input = input.into_iter();
    let mut sorter = IncrSort::new(bound);
    let mut out: Vec<T> = Vec::new();
    while let Some(batch) = sorter.next_batch(&mut input, full, suffix, same, stats) {
        out.extend(batch);
    }
    out
}

#[derive(PartialEq)]
enum IncrSt {
    LoadFull,
    ReadFull,
    LoadPrefix,
    ReadPrefix,
}

/// `incremental_sort`'s state between its sorted batches (ExecIncrementalSort's), so that
/// an Incremental Sort node gives out one batch at a time.
pub struct IncrSort<T> {
    bound: Option<usize>,
    st: IncrSt,
    pivot: Option<T>,
    transfer: Option<T>,
    remaining: usize,
    outer_done: bool,
    bound_done: usize,
    fullsorted: std::vec::IntoIter<T>,
    prefix: Vec<T>,
    prefixsorted: std::vec::IntoIter<T>,
    /// Rows given out, and whether the last batch was.
    emitted: usize,
    done: bool,
}

/// tuplesort with an optional bound: (rows, whether the bounded heap was used), counted as a
/// full sort's group (0) or a prefix sort's (1).
fn incr_group_sort<T: Clone>(rows: Vec<T>, b: Option<usize>, c: &mut Cmp<T>, st: &mut IncrStats, g: usize) -> (Vec<T>, bool) {
    let used = b.is_some_and(|b| rows.len() > b.saturating_mul(2));
    let kept = (if used { b.unwrap_or(0).min(rows.len()) } else { rows.len() }) as u64;
    st.groups[g] += 1;
    st.methods[g] |= if used { 1 } else { 2 };
    st.kept_total[g] += kept;
    st.kept_max[g] = st.kept_max[g].max(kept);
    (tuplesort(rows, b, c), used)
}

impl<T: Clone> IncrSort<T> {
    pub fn new(bound: Option<usize>) -> Self {
        IncrSort {
            bound,
            st: IncrSt::LoadFull,
            pivot: None,
            transfer: None,
            remaining: 0,
            outer_done: false,
            bound_done: 0,
            fullsorted: Vec::new().into_iter(),
            prefix: Vec::new(),
            prefixsorted: Vec::new().into_iter(),
            emitted: 0,
            done: false,
        }
    }

    /// switchToPresortedPrefixMode.
    fn switch(&mut self, suffix: &mut Cmp<T>, same: &mut dyn FnMut(&T, &T) -> bool, stats: &mut IncrStats) -> IncrSt {
        self.prefix.clear();
        let mut n = 0;
        while n < self.remaining {
            if n == 0 && self.transfer.is_some() {
                let t = self.transfer.clone().unwrap();
                self.prefix.push(t.clone());
                self.pivot = Some(t);
            } else {
                let Some(t) = self.fullsorted.next() else { break };
                self.transfer = Some(t.clone());
                if self.pivot.is_none() {
                    self.pivot = Some(t.clone());
                }
                if same(self.pivot.as_ref().unwrap(), &t) {
                    self.prefix.push(t);
                } else {
                    self.pivot = None;
                    break;
                }
            }
            n += 1;
        }
        self.remaining -= n;
        if self.remaining == 0 {
            self.pivot = self.transfer.take();
            IncrSt::LoadPrefix
        } else {
            let b = self.bound.map(|b| b.saturating_sub(self.bound_done));
            let (rows, _) = incr_group_sort(std::mem::take(&mut self.prefix), b, suffix, stats, 1);
            self.prefixsorted = rows.into_iter();
            if let Some(bd) = self.bound {
                self.bound_done = bd.min(self.bound_done + n);
            }
            IncrSt::ReadPrefix
        }
    }

    /// The next sorted batch of the output (None after the last), reading `input` only as
    /// far as it takes.
    pub fn next_batch(
        &mut self,
        input: &mut dyn Iterator<Item = T>,
        full: &mut Cmp<T>,
        suffix: &mut Cmp<T>,
        same: &mut dyn FnMut(&T, &T) -> bool,
        stats: &mut IncrStats,
    ) -> Option<Vec<T>> {
        let bound = self.bound;
        loop {
            if self.done || bound.is_some_and(|b| self.emitted >= b) {
                self.done = true;
                return None;
            }
            match self.st {
                IncrSt::LoadFull => {
                    let cur_bound = bound.map(|b| b.saturating_sub(self.bound_done));
                    let min_group = cur_bound.map_or(MIN_GROUP, |c| c.min(MIN_GROUP));
                    let set_bound = cur_bound.filter(|c| *c < MIN_GROUP);
                    let mut batch: Vec<T> = Vec::new();
                    if let Some(p) = self.pivot.take() {
                        batch.push(p.clone());
                        if batch.len() == min_group {
                            self.pivot = Some(p);
                        }
                    }
                    loop {
                        let Some(t) = input.next() else {
                            self.outer_done = true;
                            let (rows, _) = incr_group_sort(std::mem::take(&mut batch), set_bound, full, stats, 0);
                            self.fullsorted = rows.into_iter();
                            self.st = IncrSt::ReadFull;
                            break;
                        };
                        if batch.len() < min_group {
                            batch.push(t.clone());
                            if batch.len() == min_group {
                                self.pivot = Some(t);
                            }
                        } else if self.pivot.as_ref().is_some_and(|p| same(p, &t)) {
                            batch.push(t);
                        } else {
                            self.pivot = Some(t);
                            if let Some(b) = bound {
                                self.bound_done = b.min(self.bound_done + batch.len());
                            }
                            let (rows, _) = incr_group_sort(std::mem::take(&mut batch), set_bound, full, stats, 0);
                            self.fullsorted = rows.into_iter();
                            self.st = IncrSt::ReadFull;
                            break;
                        }
                        if batch.len() > MAX_FULL_SORT_GROUP {
                            self.pivot = None;
                            let n = batch.len();
                            let (rows, used) = incr_group_sort(std::mem::take(&mut batch), set_bound, full, stats, 0);
                            self.fullsorted = rows.into_iter();
                            self.remaining = if used { cur_bound.map_or(n, |c| c.min(n)) } else { n };
                            self.st = self.switch(suffix, same, stats);
                            break;
                        }
                    }
                }
                IncrSt::ReadFull | IncrSt::ReadPrefix => {
                    let src = if self.st == IncrSt::ReadFull { &mut self.fullsorted } else { &mut self.prefixsorted };
                    let mut out: Vec<T> = src.by_ref().collect();
                    if self.outer_done {
                        self.done = true;
                    } else {
                        self.st = if self.remaining > 0 { self.switch(suffix, same, stats) } else { IncrSt::LoadFull };
                    }
                    if let Some(b) = bound {
                        out.truncate(b - self.emitted);
                    }
                    self.emitted += out.len();
                    if !out.is_empty() {
                        return Some(out);
                    }
                }
                IncrSt::LoadPrefix => {
                    let mut n = 0;
                    loop {
                        let Some(t) = input.next() else {
                            self.outer_done = true;
                            break;
                        };
                        if self.pivot.as_ref().is_some_and(|p| same(p, &t)) {
                            self.prefix.push(t);
                            n += 1;
                        } else {
                            self.pivot = Some(t);
                            break;
                        }
                    }
                    let b = bound.map(|b| b.saturating_sub(self.bound_done));
                    let (rows, _) = incr_group_sort(std::mem::take(&mut self.prefix), b, suffix, stats, 1);
                    self.prefixsorted = rows.into_iter();
                    self.st = IncrSt::ReadPrefix;
                    if let Some(bd) = bound {
                        self.bound_done = bd.min(self.bound_done + n);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Items a chunk's own bounded heap would discard (as a parallel scan's workers drop
    /// them for a bounded sort above, once the chunk had more than twice the bound) may be
    /// skipped: the sort comes out the same, ties and all.
    #[test]
    fn skipped_items_change_nothing() {
        let mut x = 12345u64;
        let mut next = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        for round in 0..400 {
            let len = 1 + next(300) as usize;
            let keys = 1 + next(if round % 2 == 0 { 6 } else { 1000 });
            let items: Vec<(u64, usize)> = (0..len).map(|i| (next(keys), i)).collect();
            let bound = 1 + next(20) as usize;
            let chunk = 1 + next(80) as usize;
            let mut f = |a: &(u64, usize), b: &(u64, usize)| Ok(a.0.cmp(&b.0));
            let want = tuplesort(items.clone(), Some(bound), &mut Cmp::new(&mut f));
            let mut s = Bounded::new(bound);
            let mut c = Cmp::new(&mut f);
            for part in items.chunks(chunk) {
                let (mut seen, mut heap) = (0, std::collections::BinaryHeap::new());
                for &t in part {
                    seen += 1;
                    if seen > 2 * bound && heap.len() == bound && t.0 >= *heap.peek().unwrap() {
                        s.skip(1);
                        continue;
                    }
                    heap.push(t.0);
                    if heap.len() > bound {
                        heap.pop();
                    }
                    s.put(t, &mut c);
                }
            }
            assert_eq!(s.finish(&mut c), want, "round {round}: bound {bound}, chunks of {chunk}");
        }
    }

    fn sorted(keys: &[(i32, &'static str)], bound: Option<usize>) -> Vec<&'static str> {
        let mut f = |a: &(i32, &str), b: &(i32, &str)| Ok(a.0.cmp(&b.0));
        let mut c = Cmp::new(&mut f);
        tuplesort(keys.to_vec(), bound, &mut c)
            .into_iter()
            .map(|x| x.1)
            .collect()
    }

    #[test]
    fn ties_like_postgres() {
        let v = [
            (1, "b1"),
            (2, "b2"),
            (1, "b1x"),
            (3, "b3"),
            (1, "b1y"),
            (7, "b7"),
            (8, "b8"),
        ];
        assert_eq!(
            sorted(&v, None),
            ["b1y", "b1x", "b1", "b2", "b3", "b7", "b8"]
        );
    }
}
