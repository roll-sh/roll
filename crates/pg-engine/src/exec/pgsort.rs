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
    #[derive(PartialEq)]
    enum St {
        LoadFull,
        ReadFull,
        LoadPrefix,
        ReadPrefix,
    }
    let mut input = input.into_iter();
    let mut out: Vec<T> = Vec::new();
    let mut st = St::LoadFull;
    let mut pivot: Option<T> = None;
    let mut transfer: Option<T> = None;
    let mut remaining = 0usize;
    let mut outer_done = false;
    let mut bound_done = 0usize;
    let mut fullsorted: std::vec::IntoIter<T> = Vec::new().into_iter();
    let mut prefix: Vec<T> = Vec::new();
    let mut prefixsorted: std::vec::IntoIter<T> = Vec::new().into_iter();
    // tuplesort with an optional bound: (rows, whether the bounded heap was used), counted
    // as a full sort's group (0) or a prefix sort's (1).
    let sort = |rows: Vec<T>, b: Option<usize>, c: &mut Cmp<T>, st: &mut IncrStats, g: usize| -> (Vec<T>, bool) {
        let used = b.is_some_and(|b| rows.len() > b.saturating_mul(2));
        let kept = (if used { b.unwrap_or(0).min(rows.len()) } else { rows.len() }) as u64;
        st.groups[g] += 1;
        st.methods[g] |= if used { 1 } else { 2 };
        st.kept_total[g] += kept;
        st.kept_max[g] = st.kept_max[g].max(kept);
        (tuplesort(rows, b, c), used)
    };
    // switchToPresortedPrefixMode.
    let switch = |fullsorted: &mut std::vec::IntoIter<T>,
                  pivot: &mut Option<T>,
                  transfer: &mut Option<T>,
                  remaining: &mut usize,
                  prefix: &mut Vec<T>,
                  prefixsorted: &mut std::vec::IntoIter<T>,
                  bound_done: &mut usize,
                  suffix: &mut Cmp<T>,
                  same: &mut dyn FnMut(&T, &T) -> bool,
                  stats: &mut IncrStats|
     -> St {
        prefix.clear();
        let mut n = 0;
        while n < *remaining {
            if n == 0 && transfer.is_some() {
                let t = transfer.clone().unwrap();
                prefix.push(t.clone());
                *pivot = Some(t);
            } else {
                let Some(t) = fullsorted.next() else { break };
                *transfer = Some(t.clone());
                if pivot.is_none() {
                    *pivot = Some(t.clone());
                }
                if same(pivot.as_ref().unwrap(), &t) {
                    prefix.push(t);
                } else {
                    *pivot = None;
                    break;
                }
            }
            n += 1;
        }
        *remaining -= n;
        if *remaining == 0 {
            *pivot = transfer.take();
            St::LoadPrefix
        } else {
            let b = bound.map(|b| b.saturating_sub(*bound_done));
            let (rows, _) = sort(std::mem::take(prefix), b, suffix, stats, 1);
            *prefixsorted = rows.into_iter();
            if let Some(bd) = bound {
                *bound_done = bd.min(*bound_done + n);
            }
            St::ReadPrefix
        }
    };
    loop {
        if bound.is_some_and(|b| out.len() >= b) {
            out.truncate(bound.unwrap());
            return out;
        }
        match st {
            St::LoadFull => {
                let cur_bound = bound.map(|b| b.saturating_sub(bound_done));
                let min_group = cur_bound.map_or(MIN_GROUP, |c| c.min(MIN_GROUP));
                let set_bound = cur_bound.filter(|c| *c < MIN_GROUP);
                let mut batch: Vec<T> = Vec::new();
                if let Some(p) = pivot.take() {
                    batch.push(p.clone());
                    if batch.len() == min_group {
                        pivot = Some(p);
                    }
                }
                loop {
                    let Some(t) = input.next() else {
                        outer_done = true;
                        let (rows, _) = sort(std::mem::take(&mut batch), set_bound, full, stats, 0);
                        fullsorted = rows.into_iter();
                        st = St::ReadFull;
                        break;
                    };
                    if batch.len() < min_group {
                        batch.push(t.clone());
                        if batch.len() == min_group {
                            pivot = Some(t);
                        }
                    } else if pivot.as_ref().is_some_and(|p| same(p, &t)) {
                        batch.push(t);
                    } else {
                        pivot = Some(t);
                        if let Some(b) = bound {
                            bound_done = b.min(bound_done + batch.len());
                        }
                        let (rows, _) = sort(std::mem::take(&mut batch), set_bound, full, stats, 0);
                        fullsorted = rows.into_iter();
                        st = St::ReadFull;
                        break;
                    }
                    if batch.len() > MAX_FULL_SORT_GROUP {
                        pivot = None;
                        let n = batch.len();
                        let (rows, used) = sort(std::mem::take(&mut batch), set_bound, full, stats, 0);
                        fullsorted = rows.into_iter();
                        remaining = if used {
                            cur_bound.map_or(n, |c| c.min(n))
                        } else {
                            n
                        };
                        st = switch(
                            &mut fullsorted,
                            &mut pivot,
                            &mut transfer,
                            &mut remaining,
                            &mut prefix,
                            &mut prefixsorted,
                            &mut bound_done,
                            suffix,
                            same,
                            stats,
                        );
                        break;
                    }
                }
            }
            St::ReadFull | St::ReadPrefix => {
                let src = if st == St::ReadFull {
                    &mut fullsorted
                } else {
                    &mut prefixsorted
                };
                out.extend(src.by_ref());
                if outer_done {
                    if let Some(b) = bound {
                        out.truncate(b);
                    }
                    return out;
                }
                st = if remaining > 0 {
                    switch(
                        &mut fullsorted,
                        &mut pivot,
                        &mut transfer,
                        &mut remaining,
                        &mut prefix,
                        &mut prefixsorted,
                        &mut bound_done,
                        suffix,
                        same,
                        stats,
                    )
                } else {
                    St::LoadFull
                };
            }
            St::LoadPrefix => {
                let mut n = 0;
                loop {
                    let Some(t) = input.next() else {
                        outer_done = true;
                        break;
                    };
                    if pivot.as_ref().is_some_and(|p| same(p, &t)) {
                        prefix.push(t);
                        n += 1;
                    } else {
                        pivot = Some(t);
                        break;
                    }
                }
                let b = bound.map(|b| b.saturating_sub(bound_done));
                let (rows, _) = sort(std::mem::take(&mut prefix), b, suffix, stats, 1);
                prefixsorted = rows.into_iter();
                st = St::ReadPrefix;
                if let Some(bd) = bound {
                    bound_done = bd.min(bound_done + n);
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
