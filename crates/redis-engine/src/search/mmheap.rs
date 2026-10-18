//! The min-max heap the result sorter keeps its top N results in (`util/minmax_heap.c`). The
//! element order on ties follows the C algorithm step by step, since the sorter's comparators
//! need not be a total order (rows without a doc id compare as "greater" both ways).

/// The comparator: negative, zero or positive, as the C `cmp`.
pub type Cmp<'c, T> = dyn FnMut(&T, &T) -> i32 + 'c;

/// `mm_heap_t`, 1-based.
pub struct MmHeap<T> {
    data: Vec<Option<T>>,
    pub size: usize,
}

fn is_min(n: usize) -> bool {
    (n as u32).leading_zeros() & 1 == 1
}

impl<T> MmHeap<T> {
    /// `mmh_init_with_size` (a size of 0 is 1).
    pub fn new(size: usize) -> MmHeap<T> {
        MmHeap { data: vec![None], size: size.max(1) }
    }

    pub fn count(&self) -> usize {
        self.data.len() - 1
    }

    fn at(&self, i: usize) -> &T {
        self.data[i].as_ref().expect("a heap slot")
    }

    fn gt(&self, c: &mut Cmp<'_, T>, x: usize, y: usize) -> bool {
        c(self.at(x), self.at(y)) > 0
    }

    fn lt(&self, c: &mut Cmp<'_, T>, x: usize, y: usize) -> bool {
        c(self.at(x), self.at(y)) < 0
    }

    fn bubbleup_min(&mut self, c: &mut Cmp<'_, T>, mut i: usize) {
        loop {
            let pp = i / 2 / 2;
            if pp == 0 || !self.lt(c, i, pp) {
                return;
            }
            self.data.swap(i, pp);
            i = pp;
        }
    }

    fn bubbleup_max(&mut self, c: &mut Cmp<'_, T>, mut i: usize) {
        loop {
            let pp = i / 2 / 2;
            if pp == 0 || !self.gt(c, i, pp) {
                return;
            }
            self.data.swap(i, pp);
            i = pp;
        }
    }

    fn bubbleup(&mut self, c: &mut Cmp<'_, T>, i: usize) {
        let p = i / 2;
        if p == 0 {
            return;
        }
        if is_min(i) {
            if self.gt(c, i, p) {
                self.data.swap(i, p);
                self.bubbleup_max(c, p);
            } else {
                self.bubbleup_min(c, i);
            }
        } else if self.lt(c, i, p) {
            self.data.swap(i, p);
            self.bubbleup_min(c, p);
        } else {
            self.bubbleup_max(c, i);
        }
    }

    fn choose3(&self, c: &mut Cmp<'_, T>, max: bool, a: usize, b: usize, d: usize) -> usize {
        let f = |s: &Self, c: &mut Cmp<'_, T>, x: usize, y: usize| if max { s.gt(c, x, y) } else { s.lt(c, x, y) };
        if f(self, c, a, b) {
            if f(self, c, a, d) { a } else { d }
        } else if f(self, c, b, d) {
            b
        } else {
            d
        }
    }

    fn best_descendant(&self, c: &mut Cmp<'_, T>, max: bool, i: usize) -> Option<usize> {
        let f = |s: &Self, c: &mut Cmp<'_, T>, x: usize, y: usize| if max { s.gt(c, x, y) } else { s.lt(c, x, y) };
        let a = i * 2;
        let b = a + 1;
        let (c1, d1, e1, f1) = (a * 2, a * 2 + 1, b * 2, b * 2 + 1);
        let n = self.count();
        if f1 <= n {
            return Some(if f(self, c, c1, d1) { self.choose3(c, max, c1, e1, f1) } else { self.choose3(c, max, d1, e1, f1) });
        }
        if e1 <= n {
            return Some(self.choose3(c, max, c1, d1, e1));
        }
        if d1 <= n {
            return Some(self.choose3(c, max, b, c1, d1));
        }
        if c1 <= n {
            return Some(if f(self, c, b, c1) { b } else { c1 });
        }
        if b <= n {
            return Some(if f(self, c, a, b) { a } else { b });
        }
        if a <= n {
            return Some(a);
        }
        None
    }

    fn trickledown(&mut self, c: &mut Cmp<'_, T>, max: bool, mut i: usize) {
        loop {
            let Some(m) = self.best_descendant(c, max, i) else { return };
            let better = |s: &Self, c: &mut Cmp<'_, T>, x: usize, y: usize| if max { s.gt(c, x, y) } else { s.lt(c, x, y) };
            if m > i * 2 + 1 {
                if better(self, c, m, i) {
                    self.data.swap(i, m);
                    let worse = if max { self.lt(c, m, m / 2) } else { self.gt(c, m, m / 2) };
                    if worse {
                        self.data.swap(m, m / 2);
                    }
                    i = m;
                    continue;
                }
                return;
            }
            if better(self, c, m, i) {
                self.data.swap(i, m);
            }
            return;
        }
    }

    /// `mmh_insert`.
    pub fn insert(&mut self, c: &mut Cmp<'_, T>, v: T) {
        self.data.push(Some(v));
        if self.count() > self.size {
            self.size *= 2;
        }
        let n = self.count();
        self.bubbleup(c, n);
    }

    /// `mmh_peek_min`.
    pub fn peek_min(&self) -> Option<&T> {
        self.data.get(1).and_then(|v| v.as_ref())
    }

    /// `mmh_exchange_min`: the old minimum, replaced by `v`.
    pub fn exchange_min(&mut self, c: &mut Cmp<'_, T>, v: T) -> T {
        let old = self.data[1].replace(v).expect("a minimum");
        self.trickledown(c, false, 1);
        old
    }

    /// `mmh_peek_max`.
    pub fn peek_max(&self, c: &mut Cmp<'_, T>) -> Option<&T> {
        match self.count() {
            0 => None,
            1 => Some(self.at(1)),
            2 => Some(self.at(2)),
            _ => Some(if self.gt(c, 2, 3) { self.at(2) } else { self.at(3) }),
        }
    }

    /// `mmh_exchange_max`: the old maximum, replaced by `v` (none when empty).
    pub fn exchange_max(&mut self, c: &mut Cmp<'_, T>, v: T) -> Option<T> {
        match self.count() {
            0 => None,
            1 => self.data[1].replace(v),
            2 => {
                let old = self.data[2].replace(v);
                if self.lt(c, 2, 1) {
                    self.data.swap(2, 1);
                }
                old
            }
            _ => {
                let idx = if self.lt(c, 2, 3) { 3 } else { 2 };
                let old = self.data[idx].replace(v);
                if self.lt(c, idx, 1) {
                    self.data.swap(idx, 1);
                }
                self.trickledown(c, true, idx);
                old
            }
        }
    }

    /// `mmh_pop_min`.
    pub fn pop_min(&mut self, c: &mut Cmp<'_, T>) -> Option<T> {
        match self.count() {
            0 => None,
            1 => self.data.pop().flatten(),
            _ => {
                let last = self.data.pop().flatten();
                let d = std::mem::replace(&mut self.data[1], last);
                self.trickledown(c, false, 1);
                d
            }
        }
    }

    /// `mmh_clear`.
    pub fn clear(&mut self) {
        self.data.truncate(1);
    }

    /// `mmh_pop_max`.
    pub fn pop_max(&mut self, c: &mut Cmp<'_, T>) -> Option<T> {
        let n = self.count();
        match n {
            0 => None,
            1 | 2 => self.data.pop().flatten(),
            _ => {
                let idx = if self.lt(c, 2, 3) { 3 } else { 2 };
                let last = self.data.pop().flatten();
                let d = if idx == n { last } else { std::mem::replace(&mut self.data[idx], last) };
                if idx < self.count() + 1 {
                    self.trickledown(c, true, idx);
                }
                d
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_n_descending() {
        let mut h: MmHeap<i32> = MmHeap::new(5);
        let mut cmp = |a: &i32, b: &i32| a.cmp(b) as i32;
        for v in [5, 1, 9, 3, 7, 2, 8, 6, 4, 0] {
            if h.count() < h.size {
                h.insert(&mut cmp, v);
            } else if cmp(&v, h.peek_min().unwrap()) > 0 {
                h.exchange_min(&mut cmp, v);
            }
        }
        let mut out = Vec::new();
        while let Some(v) = h.pop_max(&mut cmp) {
            out.push(v);
        }
        assert_eq!(out, vec![9, 8, 7, 6, 5]);
    }

    #[test]
    fn pops_everything_in_order() {
        let mut h: MmHeap<u32> = MmHeap::new(64);
        let mut cmp = |a: &u32, b: &u32| a.cmp(b) as i32;
        let vals: Vec<u32> = (0..50).map(|i| (i * 37) % 50).collect();
        for &v in &vals {
            h.insert(&mut cmp, v);
        }
        let mut out = Vec::new();
        while let Some(v) = h.pop_max(&mut cmp) {
            out.push(v);
        }
        assert_eq!(out, (0..50).rev().collect::<Vec<_>>());
    }
}
