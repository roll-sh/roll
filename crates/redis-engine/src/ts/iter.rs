//! Range queries (`series_iterator.c`, `filter_iterator.c`, `sample_iterator.c`, the multi-series
//! iterators): a chain of iterators passing batches of samples (the module's "enriched chunks")
//! from a series' chunks through the timestamp and value filters and the aggregation, ported
//! step for step so the buckets, their edges and the empty-bucket fills come out the same.

use super::Series;
use super::agg::{Agg, AggType};
use super::chunk::Sample;
use std::cell::Cell;

/// `BucketTimestamp`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BucketTs {
    #[default]
    Start,
    End,
    Mid,
}

/// `RangeAlignment`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Default,
    Start,
    End,
    Ts,
}

/// `RangeArgs`.
#[derive(Clone, Debug)]
pub struct Range {
    pub start: u64,
    pub end: u64,
    pub latest: bool,
    /// -1 for no COUNT.
    pub count: i64,
    pub aggs: Vec<AggType>,
    pub bucket: u64,
    pub empty: bool,
    pub bucket_ts: BucketTs,
    pub align: Align,
    pub align_ts: u64,
    pub by_value: Option<(f64, f64)>,
    pub by_ts: Option<Vec<u64>>,
}

impl Default for Range {
    fn default() -> Self {
        Range {
            start: 0,
            end: 0,
            latest: false,
            count: -1,
            aggs: Vec::new(),
            bucket: 0,
            empty: false,
            bucket_ts: BucketTs::Start,
            align: Align::Default,
            align_ts: 0,
            by_value: None,
            by_ts: None,
        }
    }
}

impl Range {
    /// The plain range [start, end] (what the module's internal lookups build).
    pub fn span(start: u64, end: u64) -> Range {
        Range { start, end, ..Range::default() }
    }

    fn filtered(&self, start: u64, end: u64) -> Range {
        Range { start, end, by_value: self.by_value, by_ts: self.by_ts.clone(), ..Range::default() }
    }
}

/// The unfinalized bucket of a compaction (`calculate_latest_sample`), worked out up front by
/// whoever can reach the source key; `used` records whether the query asked for it (the lookup
/// then counts).
#[derive(Debug, Default)]
pub struct Latest {
    pub sample: Option<Sample>,
    pub used: Cell<bool>,
}

/// An enriched chunk: timestamps and `vps` values per sample.
#[derive(Clone, Debug, Default)]
pub struct Batch {
    pub ts: Vec<u64>,
    pub v: Vec<f64>,
    pub vps: usize,
    pub rev: bool,
}

impl Batch {
    fn new(vps: usize, rev: bool) -> Self {
        Batch { ts: Vec::new(), v: Vec::new(), vps, rev }
    }

    pub fn len(&self) -> usize {
        self.ts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ts.is_empty()
    }

    pub fn row(&self, i: usize) -> &[f64] {
        &self.v[i * self.vps..(i + 1) * self.vps]
    }

    fn push(&mut self, ts: u64, vals: &[f64]) {
        self.ts.push(ts);
        self.v.extend_from_slice(vals);
    }

    fn reverse(&mut self) {
        self.ts.reverse();
        let vps = self.vps.max(1);
        let rows: Vec<Vec<f64>> = self.v.chunks(vps).rev().map(<[f64]>::to_vec).collect();
        self.v = rows.concat();
        self.rev = !self.rev;
    }
}

/// `AbstractIterator`.
pub trait Chain {
    fn next(&mut self) -> Option<Batch>;
}

/// `CalcBucketStart`.
pub fn bucket_start(ts: u64, bucket: u64, align: u64) -> u64 {
    let diff = ts.wrapping_sub(align) as i64;
    let n = bucket as i64;
    let m = diff.wrapping_rem(n).wrapping_add(n).wrapping_rem(n);
    ts.wrapping_sub(m as u64)
}

/// `BucketStartNormalize`.
pub fn normalize(ts: u64) -> u64 {
    (ts as i64).max(0) as u64
}

fn calc_bucket_ts(b: BucketTs, ts: u64, delta: u64) -> u64 {
    match b {
        BucketTs::Start => ts,
        BucketTs::Mid => ts.wrapping_add(((delta as i64) / 2) as u64),
        BucketTs::End => ts.wrapping_add(delta),
    }
}

fn sub_max0(a: u64, b: u64) -> u64 {
    ((a as i64).wrapping_sub(b as i64)).max(0) as u64
}

// ---------------------------------------------------------------------------------------------
// The series
// ---------------------------------------------------------------------------------------------

/// `SeriesIterator`: one chunk per call, seeking from the chunk at or before the range's edge.
struct SeriesIter<'a> {
    s: &'a Series,
    /// The dict key of the chunk to process next.
    cur: Option<u64>,
    min: u64,
    max: u64,
    rev: bool,
    rev_chunk: bool,
    latest: bool,
    lat: Option<&'a Latest>,
    /// A reverse iterator that found nothing at or before its end seeks to the first chunk and
    /// then has nothing before it.
    single: bool,
}

impl<'a> SeriesIter<'a> {
    fn new(s: &'a Series, min: u64, max: u64, rev: bool, rev_chunk: bool, latest: bool, lat: Option<&'a Latest>) -> Self {
        let seek = if rev { max } else { min };
        let (cur, single) = match s.dict.range(..=seek).next_back() {
            Some((k, _)) => (Some(*k), false),
            None => (s.dict.keys().next().copied(), rev),
        };
        SeriesIter { s, cur, min, max, rev, rev_chunk, latest, lat, single }
    }

    fn should_finalize(&self) -> bool {
        self.latest && self.s.src_key.is_some() && self.max > self.s.last_ts
    }

    fn advance(&mut self) {
        use std::ops::Bound::{Excluded, Unbounded};
        let Some(k) = self.cur else { return };
        self.cur = if self.single {
            None
        } else if self.rev {
            self.s.dict.range(..k).next_back().map(|(k, _)| *k)
        } else {
            self.s.dict.range((Excluded(k), Unbounded)).next().map(|(k, _)| *k)
        };
    }

    fn handle_latest(&mut self, mut batch: Batch) -> Batch {
        if let Some(l) = self.lat {
            l.used.set(true);
            if let Some(smp) = l.sample
                && smp.ts <= self.max
                && smp.ts >= self.min
            {
                batch = Batch::new(1, self.rev_chunk);
                batch.push(smp.ts, &[smp.v]);
            }
        }
        self.latest = false;
        batch
    }
}

impl Chain for SeriesIter<'_> {
    fn next(&mut self) -> Option<Batch> {
        loop {
            if self.rev && self.should_finalize() {
                return Some(self.handle_latest(Batch::new(1, self.rev_chunk)));
            }
            let chunk = self.cur.and_then(|k| self.s.chunk_at(k));
            let Some(chunk) = chunk.filter(|c| c.num_samples() > 0) else {
                if self.should_finalize() {
                    return Some(self.handle_latest(Batch::new(1, self.rev_chunk)));
                }
                return None;
            };
            let mut samples = Vec::new();
            chunk.process(self.min, self.max, self.rev_chunk, &mut samples);
            let mut batch = Batch::new(1, self.rev_chunk);
            batch.ts = samples.iter().map(|s| s.ts).collect();
            batch.v = samples.iter().map(|s| s.v).collect();
            self.advance();
            if !self.rev && chunk.last_ts() < self.min {
                continue;
            }
            if !batch.is_empty() || !self.should_finalize() {
                return Some(batch);
            }
            return Some(self.handle_latest(batch));
        }
    }
}

/// `filterSamples`: keep the samples whose timestamps are in `filter[si..=ei]`.
fn filter_samples(b: &mut Batch, filter: &[u64], si: usize, ei: usize) -> usize {
    let n = b.len();
    let vps = b.vps;
    let mut count = 0;
    let mut stack = vec![(0usize, n - 1, si, ei)];
    while let Some((fsi, fei, ffs, ffe)) = stack.pop() {
        if fsi == fei {
            for f in filter.iter().take(ffe + 1).skip(ffs) {
                let t = b.ts[fsi];
                if t == *f {
                    b.ts[count] = t;
                    for a in 0..vps {
                        b.v[count * vps + a] = b.v[fsi * vps + a];
                    }
                    count += 1;
                    break;
                }
            }
            continue;
        }
        let mid = (fsi + fei) / 2;
        let mut found_left = false;
        let mut s = ffs;
        while s <= ffe && filter[s] < b.ts[fsi] {
            s += 1;
        }
        let mut e = s;
        while e <= ffe && filter[e] <= b.ts[mid] {
            found_left = true;
            e += 1;
        }
        let left = (fsi, mid, s, e.wrapping_sub(1));
        let mut found_right = false;
        let mut s = e;
        while s <= ffe && filter[s] < b.ts[mid + 1] {
            s += 1;
        }
        let mut e = s;
        while e <= ffe && filter[e] <= b.ts[fei] {
            found_right = true;
            e += 1;
        }
        if found_right {
            stack.push((mid + 1, fei, s, e - 1));
        }
        if found_left {
            stack.push(left);
        }
    }
    b.ts.truncate(count);
    b.v.truncate(count * vps);
    count
}

/// `SeriesFilterTSIterator`.
struct FilterTs<'a> {
    input: Box<dyn Chain + 'a>,
    values: Vec<u64>,
    count: usize,
    idx: usize,
    rev: bool,
}

impl Chain for FilterTs<'_> {
    fn next(&mut self) -> Option<Batch> {
        if self.idx == self.count {
            return None;
        }
        while let Some(mut b) = self.input.next() {
            if b.is_empty() {
                break;
            }
            let n = filter_samples(&mut b, &self.values, self.idx, self.count.wrapping_sub(1));
            if n > 0 {
                if self.rev {
                    b.reverse();
                    self.count -= n;
                } else {
                    self.idx += n;
                }
                return Some(b);
            }
        }
        None
    }
}

/// `SeriesFilterValIterator`.
struct FilterVal<'a> {
    input: Box<dyn Chain + 'a>,
    min: f64,
    max: f64,
}

impl Chain for FilterVal<'_> {
    fn next(&mut self) -> Option<Batch> {
        while let Some(b) = self.input.next() {
            let mut out = Batch::new(b.vps, b.rev);
            for i in 0..b.len() {
                let v = b.v[i * b.vps];
                if v >= self.min && v <= self.max {
                    out.push(b.ts[i], b.row(i));
                }
            }
            if !out.is_empty() {
                return Some(out);
            }
        }
        None
    }
}

/// `SeriesQuery`: the chain for a range over one series.
pub fn query<'a>(s: &'a Series, args: &Range, rev: bool, check_retention: bool, lat: Option<&'a Latest>) -> Box<dyn Chain + 'a> {
    let mut start = args.start;
    if check_retention && s.retention > 0 && s.last_ts > s.retention {
        start = start.max(s.last_ts - s.retention);
    }
    let rev_chunk = rev && args.by_ts.is_none();
    let mut chain: Box<dyn Chain + 'a> = Box::new(SeriesIter::new(s, start, args.end, rev, rev_chunk, args.latest, lat));
    if let Some(v) = &args.by_ts {
        chain = Box::new(FilterTs { input: chain, values: v.clone(), count: v.len(), idx: 0, rev });
    }
    if let Some((min, max)) = args.by_value {
        chain = Box::new(FilterVal { input: chain, min, max });
    }
    let align = match args.align {
        Align::Start => args.start,
        Align::End => args.end,
        Align::Ts => args.align_ts,
        Align::Default => 0,
    };
    if !args.aggs.is_empty() {
        chain = Box::new(AggIter::new(chain, s, args, align, rev));
    }
    chain
}

/// `SeriesSampleIterator`: the chain's samples one by one (first value), ending at the first
/// empty batch.
pub struct Samples<'a> {
    input: Box<dyn Chain + 'a>,
    chunk: Option<Batch>,
    i: usize,
}

impl<'a> Samples<'a> {
    pub fn new(s: &'a Series, args: &Range, rev: bool, check_retention: bool, lat: Option<&'a Latest>) -> Self {
        Samples { input: query(s, args, rev, check_retention, lat), chunk: None, i: 0 }
    }
}

impl Iterator for Samples<'_> {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        if self.chunk.as_ref().is_none_or(|c| self.i >= c.len()) {
            let c = self.input.next().filter(|c| !c.is_empty())?;
            self.chunk = Some(c);
            self.i = 0;
        }
        let c = self.chunk.as_ref().expect("filled");
        let s = Sample { ts: c.ts[self.i], v: c.v[self.i * c.vps] };
        self.i += 1;
        Some(s)
    }
}

// ---------------------------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------------------------

/// Empty buckets one fill may add before we give up (the module would allocate them all).
const MAX_FILL: u64 = 1 << 24;

/// `AggregationIterator`.
struct AggIter<'a> {
    input: Box<dyn Chain + 'a>,
    s: &'a Series,
    aggs: Vec<Agg>,
    valid_per: Vec<bool>,
    align: u64,
    delta: u64,
    last_ts: u64,
    unfinalized: bool,
    reverse: bool,
    initialized: bool,
    empty: bool,
    bucket_ts: BucketTs,
    start: u64,
    end: u64,
    has_twa: bool,
    prefix_done: bool,
    suffix_done: bool,
    prev_ts: u64,
    valid_in_bucket: bool,
    filters: Range,
}

fn twa_ta(reverse: bool, bstart: u64, bend: u64, rstart: u64, rend: u64) -> u64 {
    if !reverse { bstart.max(rstart) } else { bend.min(rend) }
}

fn twa_tb(reverse: bool, bstart: u64, bend: u64, rstart: u64, rend: u64) -> u64 {
    if !reverse { bend.min(rend) } else { bstart.max(rstart) }
}

/// The (up to) two nearest valid samples on one side of a bucket: how many, nearest first.
type Neighbours = (usize, Sample, Sample);

/// `twa_calc_empty_bucket_val`.
fn twa_empty_val(ta: u64, tb: u64, (nb, b, bb): Neighbours, (na, a, aa): Neighbours) -> f64 {
    let mut empty = true;
    let mut both = false;
    if nb > 1 {
        let d = b.ts.wrapping_sub(bb.ts);
        if b.ts.wrapping_add(d) > ta {
            empty = false;
        }
    }
    if na > 1 {
        let d = aa.ts.wrapping_sub(a.ts);
        if tb.wrapping_add(d) > a.ts {
            empty = false;
        }
    }
    if na != 0 && nb != 0 {
        empty = false;
        both = true;
    }
    if empty {
        f64::NAN
    } else if both {
        let dv = a.v - b.v;
        let dts = a.ts.wrapping_sub(b.ts) as f64;
        let va = b.v + (ta.wrapping_sub(b.ts) as f64 * dv) / dts;
        let vb = b.v + (tb.wrapping_sub(b.ts) as f64 * dv) / dts;
        (va + vb) / 2.0
    } else if na > 1 {
        let d = aa.ts.wrapping_sub(a.ts);
        if tb.wrapping_add(d / 2) <= a.ts { f64::NAN } else { a.v }
    } else {
        let d = b.ts.wrapping_sub(bb.ts);
        if b.ts.wrapping_add(d / 2) <= ta { f64::NAN } else { b.v }
    }
}

impl<'a> AggIter<'a> {
    fn new(input: Box<dyn Chain + 'a>, s: &'a Series, args: &Range, align: u64, reverse: bool) -> Self {
        let aggs: Vec<Agg> = args.aggs.iter().map(|k| k.create(reverse)).collect();
        let n = aggs.len();
        AggIter {
            input,
            s,
            has_twa: args.aggs.iter().any(|a| a.twa()),
            aggs,
            valid_per: vec![false; n],
            align,
            delta: args.bucket,
            last_ts: 0,
            unfinalized: false,
            reverse,
            initialized: false,
            empty: args.empty,
            bucket_ts: args.bucket_ts,
            start: args.start,
            end: args.end,
            prefix_done: false,
            suffix_done: false,
            prev_ts: 0,
            valid_in_bucket: false,
            filters: args.filtered(0, 0),
        }
    }

    fn bstart(&self, ts: u64) -> u64 {
        bucket_start(ts, self.delta, self.align)
    }

    /// `twa_get_samples_from_left`: the two newest non-NaN samples before `cur`, through the
    /// query's filters.
    fn left(&self, cur: u64) -> Neighbours {
        let mut out = (0, Sample::default(), Sample::default());
        if cur > 0 {
            let args = self.filters.filtered(0, cur - 1);
            for smp in Samples::new(self.s, &args, true, true, None) {
                if !smp.v.is_nan() {
                    if out.0 == 0 {
                        out.1 = smp;
                        out.0 = 1;
                    } else {
                        out.2 = smp;
                        out.0 = 2;
                        break;
                    }
                }
            }
        }
        out
    }

    /// `twa_get_samples_from_right`.
    fn right(&self, cur: u64) -> Neighbours {
        let mut out = (0, Sample::default(), Sample::default());
        if cur < u64::MAX {
            let args = self.filters.filtered(cur, u64::MAX);
            for smp in Samples::new(self.s, &args, false, true, None) {
                if !smp.v.is_nan() {
                    if out.0 == 0 {
                        out.1 = smp;
                        out.0 = 1;
                    } else {
                        out.2 = smp;
                        out.0 = 2;
                        break;
                    }
                }
            }
        }
        out
    }

    /// `twa_compute_empty_bucket_value`.
    fn twa_empty(&self, bucket: u64) -> f64 {
        let d = self.delta;
        let ta = twa_ta(false, bucket, bucket.wrapping_add(d), self.start, self.end);
        let tb = twa_tb(false, bucket, bucket.wrapping_add(d), self.start, self.end);
        twa_empty_val(ta, tb, self.left(ta), self.right(tb))
    }

    fn reset_all(&mut self) {
        for (a, v) in self.aggs.iter_mut().zip(self.valid_per.iter_mut()) {
            a.reset();
            *v = false;
        }
        self.valid_in_bucket = false;
    }

    /// The values of the bucket in the contexts (`finalizeBucket`'s and `agg_iter_finalize`'s
    /// common part).
    fn bucket_values(&self) -> Vec<f64> {
        let mut vals = vec![0.0; self.aggs.len()];
        if !self.valid_in_bucket {
            let twa = if self.has_twa { self.twa_empty(self.last_ts) } else { 0.0 };
            for (a, v) in self.aggs.iter().zip(vals.iter_mut()) {
                *v = if a.kind.twa() { twa } else { a.finalize_empty() };
            }
        } else {
            let mut twa = None;
            for (i, a) in self.aggs.iter().enumerate() {
                vals[i] = if self.valid_per[i] {
                    a.value()
                } else if a.kind.twa() {
                    *twa.get_or_insert_with(|| self.twa_empty(self.last_ts))
                } else {
                    a.finalize_empty()
                };
            }
        }
        vals
    }

    /// `finalizeBucket`: true when a bucket was written.
    fn finalize_bucket(&mut self, out: &mut Batch) -> bool {
        if !self.valid_in_bucket && !self.empty {
            self.reset_all();
            return false;
        }
        let vals = self.bucket_values();
        out.push(calc_bucket_ts(self.bucket_ts, self.last_ts, self.delta), &vals);
        self.reset_all();
        true
    }

    /// `seed_locf_for_empty_gap`.
    fn seed_locf(&mut self, lowest: u64, reversed: bool) {
        for i in 0..self.aggs.len() {
            if self.aggs[i].kind != AggType::Last || (!reversed && !self.aggs[i].unseeded()) {
                continue;
            }
            let (n, older, _) = self.left(lowest);
            if n > 0 {
                self.aggs[i].seed_locf(older.v, older.ts);
            } else {
                self.aggs[i].seed_locf(f64::NAN, 0);
            }
        }
    }

    /// `fillEmptyBuckets`.
    fn fill_empty(&mut self, out: &mut Batch, first: u64, end: u64, reversed: bool, edges: bool) -> Result<(), ()> {
        let d = self.delta;
        let (first, end) = if reversed { (end, first) } else { (first, end) };
        if end < first || (end - first) % d != 0 {
            return Err(());
        }
        let n = ((end - first) / d).wrapping_add(1);
        if n == 0 {
            return Err(());
        }
        let mut cur = if reversed { end } else { first };
        if edges {
            let ta = twa_ta(false, cur, cur.wrapping_add(d), self.start, self.end);
            if self.left(ta).0 == 0 || self.right(ta).0 == 0 {
                return Ok(());
            }
        }
        if n > MAX_FILL {
            return Err(());
        }
        self.seed_locf(first, reversed);
        if self.has_twa {
            let ta = twa_ta(false, cur, cur.wrapping_add(d), self.start, self.end);
            let (before, after) = (self.left(ta), self.right(ta));
            for _ in 0..n {
                let ta = twa_ta(false, cur, cur.wrapping_add(d), self.start, self.end);
                let tb = twa_tb(false, cur, cur.wrapping_add(d), self.start, self.end);
                let twa = twa_empty_val(ta, tb, before, after);
                let vals: Vec<f64> = self.aggs.iter().map(|g| if g.kind.twa() { twa } else { g.finalize_empty() }).collect();
                out.push(calc_bucket_ts(self.bucket_ts, cur, d), &vals);
                cur = if reversed { cur.wrapping_sub(d) } else { cur.wrapping_add(d) };
            }
        } else {
            for _ in 0..n {
                let vals: Vec<f64> = self.aggs.iter().map(Agg::finalize_empty).collect();
                out.push(calc_bucket_ts(self.bucket_ts, cur, d), &vals);
                cur = if reversed { cur.wrapping_sub(d) } else { cur.wrapping_add(d) };
            }
        }
        Ok(())
    }

    /// `agg_iter_on_empty_chunk` (when there's no open bucket).
    fn on_empty_chunk(&mut self) -> Option<Batch> {
        if !self.empty {
            return None;
        }
        let d = self.delta;
        let rev = self.reverse;
        let mut out = Batch::new(self.aggs.len(), rev);
        if !self.prefix_done {
            self.prefix_done = true;
            self.suffix_done = true;
            let mut first = self.bstart(self.start);
            let mut last = self.bstart(self.end);
            if rev {
                std::mem::swap(&mut first, &mut last);
            }
            self.fill_empty(&mut out, first, last, rev, true).ok()?;
            return Some(out);
        }
        if !self.suffix_done {
            self.suffix_done = true;
            let last = self.bstart(if rev { self.start } else { self.end });
            let mut first = self.bstart(self.prev_ts);
            if !rev {
                first = first.wrapping_add(d);
                if first > last {
                    return None;
                }
            } else {
                if first <= last {
                    return None;
                }
                first = sub_max0(first, d);
            }
            self.fill_empty(&mut out, first, last, rev, true).ok()?;
            return Some(out);
        }
        None
    }

    /// `agg_iter_apply_empty_prefix`.
    fn apply_empty_prefix(&mut self, first_sample: u64, out: &mut Batch) -> Result<(), ()> {
        if !self.empty || self.prefix_done {
            return Ok(());
        }
        self.prefix_done = true;
        let d = self.delta;
        let rev = self.reverse;
        let first = self.bstart(if rev { self.end } else { self.start });
        let mut last = self.bstart(first_sample);
        let mut has = true;
        if !rev {
            if first >= last {
                has = false;
            }
            last = sub_max0(last, d);
        } else {
            last = last.wrapping_add(d);
            if first < last {
                has = false;
            }
        }
        if !has {
            return Ok(());
        }
        self.fill_empty(out, first, last, rev, true)
    }

    /// `agg_iter_init_if_needed`.
    fn init_if_needed(&mut self, init_ts: u64) {
        if self.initialized {
            return;
        }
        let d = self.delta;
        self.last_ts = self.bstart(init_ts);
        self.initialized = true;
        if self.has_twa {
            let bs = normalize(self.last_ts);
            let be = self.last_ts.wrapping_add(d);
            let ta = twa_ta(self.reverse, bs, be, self.start, self.end);
            let tb = twa_tb(self.reverse, bs, be, self.start, self.end);
            let rev = self.reverse;
            for a in self.aggs.iter_mut().filter(|a| a.kind.twa()) {
                a.bucket_params(if !rev { ta } else { tb }, if !rev { tb } else { ta });
            }
        }
        if self.has_twa && !(!self.reverse && init_ts == 0) {
            let rev = self.reverse;
            let args = if rev { self.filters.filtered(init_ts.wrapping_add(1), u64::MAX) } else { self.filters.filtered(0, init_ts - 1) };
            let found = Samples::new(self.s, &args, !rev, true, None).find(|smp| !smp.v.is_nan());
            if let Some(smp) = found {
                for a in self.aggs.iter_mut().filter(|a| a.kind.twa()) {
                    a.prev_bucket_last(smp.v, smp.ts);
                }
            }
        }
    }

    /// `agg_iter_empty_gap_after_finalize` (the general path's flavour).
    fn gap_after_finalize(&self, scope: u64) -> Option<(u64, u64)> {
        let d = self.delta;
        if self.reverse {
            let first = sub_max0(scope, d.wrapping_mul(2));
            let last = self.last_ts.wrapping_add(d);
            if scope < last.wrapping_add(d.wrapping_mul(2)) {
                return None;
            }
            Some((first, last))
        } else {
            let first = scope;
            if first >= self.last_ts {
                return None;
            }
            Some((first, sub_max0(self.last_ts, d)))
        }
    }

    /// `agg_iter_general_on_bucket_boundary`.
    fn on_boundary(&mut self, smp: Sample, scope: &mut u64, out: &mut Batch) -> Result<(), ()> {
        for a in self.aggs.iter_mut() {
            if a.kind.twa() && a.kind.valid(smp.v) {
                a.next_bucket_first(smp.v, smp.ts);
            }
        }
        let mut twa_last = vec![None; self.aggs.len()];
        if self.has_twa {
            for (i, a) in self.aggs.iter().enumerate() {
                if a.kind.twa() {
                    twa_last[i] = Some((self.valid_per[i], a.last_sample().unwrap_or_default()));
                }
            }
        }
        self.finalize_bucket(out);
        self.last_ts = self.bstart(smp.ts);
        if self.empty
            && let Some((first, last)) = self.gap_after_finalize(*scope)
        {
            let rev = self.reverse;
            self.fill_empty(out, first, last, rev, false)?;
        }
        *scope = self.last_ts.wrapping_add(self.delta);
        self.last_ts = normalize(self.last_ts);
        if self.has_twa {
            let rev = self.reverse;
            let tb = twa_tb(rev, self.last_ts, *scope, self.start, self.end);
            let (lt, sc) = (self.last_ts, *scope);
            for (i, a) in self.aggs.iter_mut().enumerate() {
                if let Some((had_valid, (ts, v))) = twa_last[i] {
                    if had_valid && a.kind.valid(v) {
                        a.prev_bucket_last(v, ts);
                    }
                    a.bucket_params(if !rev { lt } else { tb }, if !rev { tb } else { sc });
                }
            }
        }
        Ok(())
    }

    fn append_all(&mut self, smp: Sample) {
        for (i, a) in self.aggs.iter_mut().enumerate() {
            if a.kind.valid(smp.v) {
                a.append(smp.v, smp.ts);
                self.valid_in_bucket = true;
                self.valid_per[i] = true;
            }
        }
    }

    /// `agg_iter_finalize`.
    fn finalize(&mut self) -> Option<Batch> {
        self.unfinalized = false;
        let rev = self.reverse;
        for i in 0..self.aggs.len() {
            if !self.aggs[i].kind.twa() {
                continue;
            }
            let (lts, _) = self.aggs[i].last_sample().unwrap_or_default();
            if rev && lts == 0 {
                continue;
            }
            let args = if rev { self.filters.filtered(0, lts.wrapping_sub(1)) } else { self.filters.filtered(lts.wrapping_add(1), u64::MAX) };
            let kind = self.aggs[i].kind;
            let found = Samples::new(self.s, &args, rev, true, None).find(|smp| kind.valid(smp.v));
            if let Some(smp) = found {
                self.aggs[i].next_bucket_first(smp.v, smp.ts);
            }
        }
        let mut out = Batch::new(self.aggs.len(), rev);
        if !self.valid_in_bucket && !self.empty {
            self.reset_all();
            return Some(out);
        }
        let vals = self.bucket_values();
        for v in self.valid_per.iter_mut() {
            *v = false;
        }
        self.valid_in_bucket = false;
        out.push(calc_bucket_ts(self.bucket_ts, self.last_ts, self.delta), &vals);
        if self.empty && !self.suffix_done {
            self.suffix_done = true;
            let d = self.delta;
            let last = self.bstart(if rev { self.start } else { self.end });
            let mut first = self.bstart(self.last_ts);
            let mut has = true;
            if rev {
                if first <= last {
                    has = false;
                }
                first = sub_max0(first, d);
            } else {
                if first >= last {
                    has = false;
                }
                first = first.wrapping_add(d);
            }
            if has {
                self.fill_empty(&mut out, first, last, rev, true).ok()?;
            }
        }
        Some(out)
    }
}

impl Chain for AggIter<'_> {
    fn next(&mut self) -> Option<Batch> {
        let rev = self.reverse;
        let Some(mut chunk) = self.input.next().filter(|c| !c.is_empty()) else {
            if self.unfinalized {
                return self.finalize();
            }
            return self.on_empty_chunk();
        };
        let mut out = Batch::new(self.aggs.len(), rev);
        self.apply_empty_prefix(chunk.ts[0], &mut out).ok()?;
        self.unfinalized = true;
        self.init_if_needed(chunk.ts[0]);
        let mut scope = self.last_ts.wrapping_add(self.delta);
        self.last_ts = normalize(self.last_ts);
        loop {
            for i in 0..chunk.len() {
                let smp = Sample { ts: chunk.ts[i], v: chunk.v[i * chunk.vps] };
                if (!rev && smp.ts >= scope) || (rev && smp.ts < self.last_ts) {
                    self.on_boundary(smp, &mut scope, &mut out).ok()?;
                }
                self.append_all(smp);
            }
            if let Some(&t) = out.ts.last() {
                self.prev_ts = t;
                return Some(out);
            }
            match self.input.next() {
                Some(c) => chunk = c,
                None => return self.finalize(),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Several series
// ---------------------------------------------------------------------------------------------

/// `MultiSeriesSampleIterator`: the samples of several series merged by timestamp with the
/// module's binary heap (its tie order included).
pub struct Merge<'a> {
    heap: Vec<(Sample, usize)>,
    iters: Vec<Samples<'a>>,
    reverse: bool,
}

impl<'a> Merge<'a> {
    pub fn new(iters: Vec<Samples<'a>>, reverse: bool) -> Self {
        let mut m = Merge { heap: Vec::new(), iters, reverse };
        for i in 0..m.iters.len() {
            if let Some(s) = m.iters[i].next() {
                m.offer((s, i));
            }
        }
        m
    }

    fn heap_cmp(&self, a: usize, b: usize) -> i32 {
        let (x, y) = (self.heap[a].0.ts, self.heap[b].0.ts);
        let before = if self.reverse { x > y } else { x < y };
        if before { 1 } else { -1 }
    }

    fn offer(&mut self, item: (Sample, usize)) {
        self.heap.push(item);
        let mut idx = self.heap.len() - 1;
        while idx != 0 {
            let parent = (idx - 1) / 2;
            if self.heap_cmp(idx, parent) < 0 {
                return;
            }
            self.heap.swap(idx, parent);
            idx = parent;
        }
    }

    fn pushdown(&mut self, mut idx: usize) {
        let count = self.heap.len();
        loop {
            let (l, r) = (idx * 2 + 1, idx * 2 + 2);
            let child = if r >= count {
                if l >= count {
                    return;
                }
                l
            } else if self.heap_cmp(l, r) < 0 {
                r
            } else {
                l
            };
            if self.heap_cmp(idx, child) < 0 {
                self.heap.swap(idx, child);
                idx = child;
            } else {
                return;
            }
        }
    }

    fn poll(&mut self) -> Option<(Sample, usize)> {
        if self.heap.is_empty() {
            return None;
        }
        let item = self.heap.swap_remove(0);
        if self.heap.len() > 1 {
            self.pushdown(0);
        }
        Some(item)
    }
}

impl Iterator for Merge<'_> {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        let (s, i) = self.poll()?;
        if let Some(n) = self.iters[i].next() {
            self.offer((n, i));
        }
        Some(s)
    }
}

/// `MultiSeriesAggDupSampleIterator`: samples sharing a timestamp reduced to one.
pub struct Reduce<'a> {
    input: Merge<'a>,
    agg: Agg,
    next: Option<Sample>,
}

impl<'a> Reduce<'a> {
    pub fn new(mut input: Merge<'a>, kind: AggType) -> Self {
        let next = input.next();
        Reduce { input, agg: kind.create(false), next }
    }
}

impl Iterator for Reduce<'_> {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        let first = self.next?;
        let kind = self.agg.kind;
        let mut any = kind.valid(first.v);
        if any {
            self.agg.append(first.v, first.ts);
        }
        let mut after = None;
        for s in self.input.by_ref() {
            if s.ts != first.ts {
                after = Some(s);
                break;
            }
            if kind.valid(s.v) {
                self.agg.append(s.v, s.ts);
                any = true;
            }
        }
        let v = if any {
            self.agg.value()
        } else if matches!(kind, AggType::Count | AggType::CountNan | AggType::CountAll) {
            self.agg.finalize_empty()
        } else {
            f64::NAN
        };
        self.agg.reset();
        self.next = after;
        Some(Sample { ts: first.ts, v })
    }
}
