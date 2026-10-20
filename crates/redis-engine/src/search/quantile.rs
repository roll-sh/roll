//! The streaming quantile estimator behind the `QUANTILE` reducer (`util/quantile.c`): values
//! are buffered, sorted and merged into a sample list (value, rank count `g` and rank delta
//! `d`, both single-precision as in the C), which is compressed after each full buffer.

const EPSILON: f64 = 0.01;

#[derive(Clone, Copy, Debug)]
struct Sample {
    v: f64,
    g: f32,
    d: f32,
    prev: Option<usize>,
    next: Option<usize>,
}

/// `QuantStream` (no target quantiles: the error bound is `2 * EPSILON * r`).
#[derive(Debug)]
pub struct QuantStream {
    buffer: Vec<f64>,
    cap: usize,
    samples: Vec<Sample>,
    first: Option<usize>,
    last: Option<usize>,
    n: usize,
    len: usize,
    pool: Vec<usize>,
}

fn max_val(r: f64) -> f64 {
    EPSILON * 2.0 * r
}

impl QuantStream {
    /// `NewQuantileStream(NULL, 0, cap)`.
    pub fn new(cap: usize) -> QuantStream {
        QuantStream { buffer: Vec::with_capacity(cap), cap, samples: Vec::new(), first: None, last: None, n: 0, len: 0, pool: Vec::new() }
    }

    fn new_sample(&mut self, v: f64) -> usize {
        let s = Sample { v, g: 1.0, d: 0.0, prev: None, next: None };
        match self.pool.pop() {
            Some(i) => {
                self.samples[i] = s;
                i
            }
            None => {
                self.samples.push(s);
                self.samples.len() - 1
            }
        }
    }

    /// `QS_InsertSampleAt`: before `pos` (the new sample's `prev` is only set when `pos` had
    /// one).
    fn insert_at(&mut self, pos: usize, s: usize) {
        self.samples[s].next = Some(pos);
        match self.samples[pos].prev {
            Some(p) => {
                self.samples[p].next = Some(s);
                self.samples[s].prev = Some(p);
            }
            None => self.first = Some(s),
        }
        self.samples[pos].prev = Some(s);
        self.len += 1;
    }

    fn append(&mut self, s: usize) {
        match self.last {
            None => {
                self.first = Some(s);
                self.last = Some(s);
            }
            Some(l) => {
                self.samples[s].prev = Some(l);
                self.samples[l].next = Some(s);
                self.last = Some(s);
            }
        }
        self.len += 1;
    }

    fn remove(&mut self, s: usize) {
        let Sample { prev, next, .. } = self.samples[s];
        if let Some(p) = prev {
            self.samples[p].next = next;
        }
        if let Some(n) = next {
            self.samples[n].prev = prev;
        }
        if self.last == Some(s) {
            self.last = prev;
        }
        if self.first == Some(s) {
            self.first = next;
        }
        self.pool.push(s);
        self.len -= 1;
    }

    /// `QS_Flush`.
    fn flush(&mut self) {
        let mut buf = std::mem::take(&mut self.buffer);
        buf.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut r = 0.0f64;
        let mut pos = self.first;
        for &cur in &buf {
            let s = self.new_sample(cur);
            let mut inserted = false;
            while let Some(p) = pos {
                if self.samples[p].v > cur {
                    self.samples[s].d = (max_val(r).floor() - 1.0) as f32;
                    self.insert_at(p, s);
                    inserted = true;
                    break;
                }
                r += self.samples[p].g as f64;
                pos = self.samples[p].next;
            }
            if !inserted {
                self.samples[s].d = 0.0;
                self.append(s);
            }
            self.n += 1;
        }
        buf.clear();
        self.buffer = buf;
    }

    /// `QS_Compress`.
    fn compress(&mut self) {
        if self.len < 2 {
            return;
        }
        let last = self.last.expect("samples");
        let mut cur = self.samples[last].prev;
        let mut r = self.n as f64 - 1.0 - self.samples[last].g as f64;
        while let Some(c) = cur {
            let next_cur = self.samples[c].prev;
            let parent = self.samples[c].next.expect("a parent");
            let g_cur = self.samples[c].g as f64;
            let (pg, pd) = (self.samples[parent].g, self.samples[parent].d);
            if ((self.samples[c].g + pg + pd) as f64) <= max_val(r) {
                self.samples[parent].g = (pg as f64 + g_cur) as f32;
                self.remove(c);
            }
            r -= g_cur;
            cur = next_cur;
        }
    }

    /// `QS_Insert`.
    pub fn insert(&mut self, v: f64) {
        self.buffer.push(v);
        if self.buffer.len() == self.cap {
            self.flush();
            self.compress();
        }
    }

    /// `QS_Query`.
    pub fn query(&mut self, q: f64) -> f64 {
        if !self.buffer.is_empty() {
            self.flush();
        }
        let mut t = (q * self.n as f64).ceil();
        t += (max_val(t) / 2.0).floor();
        let Some(mut prev) = self.first else { return f64::NAN };
        let mut r = 0.0f64;
        let mut cur = self.samples[prev].next;
        while let Some(c) = cur {
            let s = self.samples[c];
            if r + s.g as f64 + s.d as f64 >= t {
                break;
            }
            r += s.g as f64;
            prev = c;
            cur = s.next;
        }
        self.samples[prev].v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_for_small_streams() {
        let mut q = QuantStream::new(500);
        for v in [5.0, 1.0, 3.0, 2.0, 4.0] {
            q.insert(v);
        }
        assert_eq!(q.query(0.5), 3.0);
        assert_eq!(q.query(0.0), 1.0);
        assert_eq!(q.query(1.0), 5.0);
        assert!(QuantStream::new(10).query(0.5).is_nan());
    }

    #[test]
    fn compresses_long_streams() {
        let mut q = QuantStream::new(10);
        for i in 0..1000 {
            q.insert(i as f64);
        }
        let m = q.query(0.5);
        assert!((450.0..=550.0).contains(&m), "{m}");
    }
}
