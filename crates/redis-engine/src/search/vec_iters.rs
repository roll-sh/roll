//! The vector query iterators: KNN's hybrid iterator (`HybridIterator`: a plain top-k, the
//! ad-hoc brute force over a filter's results, or batches of the closest vectors intersected
//! with the filter, switching to ad-hoc when the batches look too slow), and the range query's
//! lazy metric and id-list iterators — with VecSim's query parameter resolution.

use super::error::{Code, QErr};
use super::iters::{BoxIt, Checker, Counters, DocId, IKind, IdList, Iter, IxResult, K_HYBRID_METRIC, K_METRIC, K_VIRTUAL, MASK_ALL, PCtx, Pairs, Predicate, Qcx, Skip, kv, print_counters, profile_child};
use super::mmheap::MmHeap;
use super::spec::Algo;
use super::vecsim::{Batches, Mode, VecIndex, VecRes};
use redis_proto::Reply;

/// `VecsimQueryType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QType {
    Knn,
    Hybrid,
    Range,
}

/// `VecSimQueryParams`: what a query's attributes set.
#[derive(Clone, Copy, Debug, Default)]
pub struct QParams {
    pub ef_runtime: usize,
    pub epsilon: f64,
    pub batch_size: usize,
    pub mode: Option<Mode>,
    pub search_window: usize,
    pub search_buffer: usize,
    pub search_history: bool,
}

/// C `strtoll(s, &end, 0)` consuming all of `s`: `None` when it doesn't, or on overflow.
fn strtoll0(s: &[u8]) -> Option<i64> {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let neg = s.get(i) == Some(&b'-');
    if matches!(s.get(i), Some(b'+' | b'-')) {
        i += 1;
    }
    let mut base = 10;
    if s.get(i) == Some(&b'0') {
        if matches!(s.get(i + 1), Some(b'x' | b'X')) && s.get(i + 2).is_some_and(|c| c.is_ascii_hexdigit()) {
            base = 16;
            i += 2;
        } else {
            base = 8;
        }
    }
    let start = i;
    let mut v: i128 = 0;
    while let Some(d) = s.get(i).and_then(|&c| (c as char).to_digit(base)) {
        v = v * base as i128 + d as i128;
        if v > i64::MAX as i128 + 1 {
            return None;
        }
        i += 1;
    }
    if i == start || i != s.len() {
        return None;
    }
    let v = if neg { -v } else { v };
    if v > i64::MAX as i128 || v < i64::MIN as i128 {
        return None;
    }
    Some(v as i64)
}

fn positive_int(v: &[u8]) -> Option<usize> {
    let n = strtoll0(v)?;
    (n > 0 && n != i64::MAX).then_some(n as usize)
}

fn positive_double(v: &[u8]) -> Option<f64> {
    let (d, used) = super::strconv::c_strtod(v);
    // `errno`: ERANGE on overflow (not a written infinity) and on subnormal results; NaN passes
    let text = String::from_utf8_lossy(v).trim_start().trim_start_matches(['+', '-']).to_ascii_lowercase();
    let erange = (d.is_infinite() && !text.starts_with("inf")) || (d != 0.0 && d.abs() < f64::MIN_POSITIVE);
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    let positive = !(d <= 0.0);
    (used == v.len() && positive && d != f64::MAX && !erange).then_some(d)
}

/// `VecSim_ResolveQueryParams`: VecSim's `VecSimIndex_ResolveParams` with its errors as the
/// module reports them.
pub fn resolve_params(algo: Algo, params: &[(Vec<u8>, Vec<u8>)], qt: QType) -> Result<QParams, QErr> {
    let fail = |code: Code| Err(QErr::user(code, code.strings().1, " (Error parsing vector similarity parameters)"));
    let mut q = QParams::default();
    for (name, value) in params {
        let name = crate::util::cstr(name);
        let is = |s: &str| name.eq_ignore_ascii_case(s.as_bytes());
        if is("EF_RUNTIME") {
            if algo != Algo::Hnsw || qt == QType::Range {
                return fail(Code::NoOption);
            }
            if q.ef_runtime != 0 {
                return fail(Code::DupParam);
            }
            match positive_int(value) {
                Some(n) => q.ef_runtime = n,
                None => return fail(Code::BadVal),
            }
        } else if is("EPSILON") {
            if algo == Algo::Flat {
                return fail(Code::NoOption);
            }
            if qt != QType::Range {
                return fail(Code::NonRange);
            }
            if q.epsilon != 0.0 {
                return fail(Code::DupParam);
            }
            match positive_double(value) {
                Some(d) => q.epsilon = d,
                None => return fail(Code::BadVal),
            }
        } else if is("RERANK") {
            // disk indexes only
            return fail(Code::NoOption);
        } else if is("BATCH_SIZE") {
            if qt != QType::Hybrid {
                return fail(Code::NonHybrid);
            }
            if q.batch_size != 0 {
                return fail(Code::DupParam);
            }
            match positive_int(value) {
                Some(n) => q.batch_size = n,
                None => return fail(Code::BadVal),
            }
        } else if is("HYBRID_POLICY") {
            if qt != QType::Hybrid {
                return fail(Code::NonHybrid);
            }
            if q.mode.is_some() {
                return fail(Code::DupParam);
            }
            let v = crate::util::cstr(value);
            if v.eq_ignore_ascii_case(b"BATCHES") {
                q.mode = Some(Mode::HybridBatches);
            } else if v.eq_ignore_ascii_case(b"ADHOC_BF") {
                q.mode = Some(Mode::HybridAdhocBf);
            } else {
                return fail(Code::HybridNonExist);
            }
        } else if is("SEARCH_WINDOW_SIZE") || is("SEARCH_BUFFER_CAPACITY") {
            if algo != Algo::Svs {
                return fail(Code::NoOption);
            }
            let slot = if is("SEARCH_WINDOW_SIZE") { &mut q.search_window } else { &mut q.search_buffer };
            if *slot != 0 {
                return fail(Code::DupParam);
            }
            match positive_int(value) {
                Some(n) => *slot = n,
                None => return fail(Code::BadVal),
            }
        } else if is("USE_SEARCH_HISTORY") {
            if algo != Algo::Svs {
                return fail(Code::NoOption);
            }
            if q.search_history {
                return fail(Code::DupParam);
            }
            let v = crate::util::cstr(value).to_ascii_uppercase();
            if !matches!(&v[..], b"ON" | b"OFF" | b"AUTO") {
                return fail(Code::BadVal);
            }
            q.search_history = true;
        } else {
            return fail(Code::NoOption);
        }
    }
    if q.mode == Some(Mode::HybridAdhocBf) && q.batch_size > 0 {
        return fail(Code::AdhocWithBatchSize);
    }
    if q.mode == Some(Mode::HybridAdhocBf) && algo == Algo::Hnsw && q.ef_runtime > 0 {
        return fail(Code::AdhocWithEfRuntime);
    }
    Ok(q)
}

/// A heap entry's vector score (`VECTOR_SCORE`).
fn vscore(r: &IxResult) -> f64 {
    if r.kind == K_METRIC { r.num } else { r.children.first().map_or(0.0, |c| c.num) }
}

/// `cmpVecSimResByScore`: by score, ties "greater" when the first has the lower id.
fn cmp_res(a: &IxResult, b: &IxResult) -> i32 {
    let (s1, s2) = (vscore(a), vscore(b));
    if s1 < s2 {
        -1
    } else if s1 > s2 {
        1
    } else {
        (a.doc_id < b.doc_id) as i32
    }
}

fn metric_result(doc_id: DocId, score: f64) -> IxResult {
    IxResult { doc_id, kind: K_METRIC, num: score, field_mask: MASK_ALL, weight: 1.0, ..Default::default() }
}

/// The hybrid iterator (`HybridIterator`).
pub struct HybridIt<'a> {
    vi: &'a VecIndex,
    q: &'a Qcx<'a>,
    child: Option<BoxIt<'a>>,
    blob: Vec<u8>,
    k: usize,
    params: QParams,
    mode: Mode,
    can_trim: bool,
    key: Option<usize>,
    checker: Checker,
    prepared: bool,
    reply: Vec<VecRes>,
    pos: usize,
    heap: MmHeap<IxResult>,
    current: Option<IxResult>,
    last: DocId,
    eof: bool,
    iterations: usize,
    max_batch: usize,
    max_batch_iter: usize,
}

impl<'a> HybridIt<'a> {
    /// `NewHybridVectorIterator` (after the reducer).
    #[allow(clippy::too_many_arguments)]
    pub fn new(vi: &'a VecIndex, q: &'a Qcx<'a>, fi: usize, blob: &[u8], k: usize, params: QParams, child: Option<BoxIt<'a>>, can_trim: bool, key: Option<usize>) -> Self {
        let mode = match &child {
            None => Mode::StandardKnn,
            Some(_) if k == 0 => Mode::StandardKnn,
            Some(c) => {
                let subset = c.num_estimated().min(vi.size());
                match params.mode {
                    Some(m) => m,
                    None if vi.prefer_adhoc(subset, k) => Mode::HybridAdhocBf,
                    None => Mode::HybridBatches,
                }
            }
        };
        HybridIt {
            vi,
            q,
            child,
            blob: blob.to_vec(),
            k,
            params,
            mode,
            can_trim,
            key,
            checker: Checker::Field(fi, Predicate::Default),
            prepared: false,
            reply: Vec::new(),
            pos: 0,
            heap: MmHeap::new(k.max(1)),
            current: None,
            last: 0,
            eof: false,
            iterations: 0,
            max_batch: 0,
            max_batch_iter: 0,
        }
    }

    /// `insertResultToHeap`: a metric result (with the child's metrics) when the scorer
    /// needs nothing deeper, else the vector result and a copy of the child's.
    fn insert(&mut self, child: &IxResult, doc: DocId, score: f64, upper: &mut f64) {
        let mut vec = metric_result(doc, score);
        let res = if self.can_trim {
            vec.metrics = child.metrics.clone();
            vec
        } else {
            let mut r = IxResult { kind: K_HYBRID_METRIC, weight: 1.0, ..Default::default() };
            r.push(&vec);
            r.push(child);
            r
        };
        let mut res = res;
        if let Some(k) = self.key {
            res.metrics.push((k, score));
        }
        if self.heap.count() < self.k {
            self.heap.insert(&mut cmp_res, res);
        } else {
            self.heap.exchange_max(&mut cmp_res, res);
        }
        *upper = self.heap.peek_max(&mut cmp_res).map_or(f64::INFINITY, vscore);
    }

    /// `computeDistances`: every filtered document's distance, the best `k` kept.
    fn compute_distances(&mut self) {
        let mut upper = f64::INFINITY;
        let q = self.vi.preprocess(&self.blob);
        let Some(mut child) = self.child.take() else { return };
        while child.read() {
            let id = child.last_doc_id();
            let metric = self.vi.distance_from(id, &q);
            if metric.is_nan() {
                continue;
            }
            if self.heap.count() < self.k || metric < upper {
                let cur = child.current().cloned().unwrap_or_default();
                self.insert(&cur, id, metric, &mut upper);
            }
        }
        self.child = Some(child);
    }

    /// `alternatingIterate`: a batch (by id) intersected with the filter.
    fn alternating(&mut self, child: &mut BoxIt<'a>, batch: &[VecRes], upper: &mut f64) {
        let mut pos = 0;
        let mut cur: VecRes = (0, 0.0);
        let mut child_ok = child.read();
        let mut vec_ok = if pos < batch.len() {
            cur = batch[pos];
            pos += 1;
            true
        } else {
            false
        };
        while child_ok && vec_ok {
            let cl = child.last_doc_id();
            if cur.0 == cl {
                if self.heap.count() < self.k || cur.1 < *upper {
                    let c = child.current().cloned().unwrap_or_default();
                    self.insert(&c, cur.0, cur.1, upper);
                }
                child_ok = child.read();
                vec_ok = if pos < batch.len() {
                    cur = batch[pos];
                    pos += 1;
                    true
                } else {
                    false
                };
            } else if cur.0 > cl {
                child_ok = child.skip_to(cur.0) != Skip::Eof;
            } else if pos < batch.len() {
                vec_ok = false;
                while pos < batch.len() {
                    let r = batch[pos];
                    pos += 1;
                    if cl > r.0 {
                        continue;
                    }
                    cur = r;
                    vec_ok = true;
                    break;
                }
            } else {
                break;
            }
        }
    }

    /// `reviewHybridSearchPolicy`.
    fn review(&self, n_left: usize, child_upper: usize, child_est: &mut usize) -> bool {
        if self.params.mode == Some(Mode::HybridBatches) && self.params.batch_size != 0 {
            return false;
        }
        let new_results = self.heap.count() - (self.k - n_left);
        let ratio = new_results as f32 / n_left as f32;
        let cur_est = (ratio * self.vi.size() as f32) as usize;
        *child_est = (*child_est + cur_est) / 2;
        if *child_est > child_upper {
            *child_est = child_upper;
        }
        if self.params.mode == Some(Mode::HybridBatches) {
            return false;
        }
        self.vi.prefer_adhoc(*child_est, self.k)
    }

    /// `prepareResults`.
    fn prepare(&mut self) {
        match self.mode {
            Mode::StandardKnn => {
                self.reply = self.vi.top_k(&self.blob, self.k);
                self.pos = 0;
                return;
            }
            Mode::HybridAdhocBf | Mode::HybridBatchesToAdhocBf => {
                self.compute_distances();
                return;
            }
            _ => {}
        }
        let Some(mut child) = self.child.take() else { return };
        if child.num_estimated() == 0 {
            self.child = Some(child);
            return;
        }
        let mut batches: Batches = self.vi.batches(&self.blob);
        let mut upper = f64::INFINITY;
        let mut child_est = child.num_estimated().min(self.vi.size());
        let child_upper = child_est;
        self.max_batch = self.params.batch_size;
        let mut to_adhoc = false;
        while batches.has_next() {
            self.iterations += 1;
            let size = self.vi.size();
            let n_left = self.k - self.heap.count();
            let mut batch_size = self.params.batch_size;
            if batch_size == 0 {
                batch_size = (n_left as f32 * (size as f32 / child_est as f32)) as usize + 1;
                if batch_size > self.max_batch {
                    self.max_batch = batch_size;
                    self.max_batch_iter = self.iterations - 1;
                }
            }
            let batch = batches.next(self.vi, batch_size);
            child.rewind();
            self.alternating(&mut child, &batch, &mut upper);
            if self.heap.count() == self.k {
                break;
            }
            if self.review(n_left, child_upper, &mut child_est) {
                to_adhoc = true;
                break;
            }
        }
        if to_adhoc {
            self.mode = Mode::HybridBatchesToAdhocBf;
            self.heap.clear();
            child.rewind();
            self.child = Some(child);
            self.compute_distances();
            return;
        }
        self.child = Some(child);
    }

    fn expired(&self, r: &IxResult) -> bool {
        self.checker.is_expired(self.q, r)
    }
}

impl Iter for HybridIt<'_> {
    fn read(&mut self) -> bool {
        if !self.prepared {
            self.prepared = true;
            self.prepare();
        }
        loop {
            if self.eof {
                return false;
            }
            let next = if self.mode == Mode::StandardKnn {
                match self.reply.get(self.pos).copied() {
                    Some((id, score)) => {
                        self.pos += 1;
                        let mut r = metric_result(id, score);
                        if let Some(k) = self.key {
                            r.metrics.push((k, score));
                        }
                        Some(r)
                    }
                    None => None,
                }
            } else {
                self.heap.pop_min(&mut cmp_res)
            };
            let Some(r) = next else {
                self.eof = true;
                return false;
            };
            let expired = self.expired(&r);
            self.current = Some(r);
            if expired {
                continue;
            }
            self.last = self.current.as_ref().map_or(0, |r| r.doc_id);
            return true;
        }
    }
    fn skip_to(&mut self, id: DocId) -> Skip {
        // The iterator is always the root, which nothing skips; read on to the target.
        while self.last < id {
            if !self.read() {
                return Skip::Eof;
            }
        }
        if self.last == id { Skip::Found } else { Skip::NotFound }
    }
    fn current(&mut self) -> Option<&mut IxResult> {
        self.current.as_mut()
    }
    fn last_doc_id(&self) -> DocId {
        self.last
    }
    fn at_eof(&self) -> bool {
        self.eof
    }
    fn num_estimated(&self) -> usize {
        let n = self.k.min(self.vi.size());
        match &self.child {
            None => n,
            Some(c) => n.min(c.num_estimated()),
        }
    }
    fn rewind(&mut self) {
        self.prepared = false;
        self.iterations = 0;
        self.max_batch = 0;
        self.max_batch_iter = 0;
        self.reply.clear();
        self.pos = 0;
        self.last = 0;
        self.eof = false;
        self.current = None;
        if matches!(self.mode, Mode::HybridAdhocBf | Mode::HybridBatches) {
            self.heap.clear();
            if let Some(c) = &mut self.child {
                c.rewind();
            }
        }
    }
    fn kind(&self) -> IKind {
        IKind::Other
    }
    fn profile_children(&mut self) {
        if let Some(c) = &mut self.child {
            profile_child(c);
        }
    }
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        let mut m = Vec::new();
        kv(&mut m, "Type", Reply::Status("VECTOR".into()));
        print_counters(&mut m, pc, c);
        kv(&mut m, "Vector search mode", Reply::Status(self.mode.name().into()));
        if matches!(self.mode, Mode::HybridBatches | Mode::HybridBatchesToAdhocBf) {
            kv(&mut m, "Batches number", Reply::Int(self.iterations as i64));
            kv(&mut m, "Largest batch size", Reply::Int(self.max_batch as i64));
            kv(&mut m, "Largest batch iteration (zero based)", Reply::Int(self.max_batch_iter as i64));
        }
        if let Some(ch) = &self.child {
            kv(&mut m, "Child iterator", Reply::Map(ch.print(pc, None)));
        }
        m
    }
}

/// A range query's lazy iterator: the ids within the radius (by id, or by score for
/// FT.HYBRID's unfiltered RANGE), with their distances as metrics when the query yields them
/// (`MetricLazy` / `IdListLazy`).
pub struct RangeIt<'a> {
    vi: &'a VecIndex,
    blob: Vec<u8>,
    radius: f64,
    key: Option<usize>,
    yields: bool,
    by_score: bool,
    produced: bool,
    list: IdList,
    dists: Vec<f64>,
}

impl<'a> RangeIt<'a> {
    pub fn new(vi: &'a VecIndex, blob: &[u8], radius: f64, yields: bool, key: Option<usize>, by_score: bool) -> Self {
        let res = if yields { metric_result(0, 0.0) } else { IxResult { kind: K_VIRTUAL, weight: 1.0, ..Default::default() } };
        RangeIt { vi, blob: blob.to_vec(), radius, key, yields, by_score, produced: false, list: IdList::new(Vec::new(), res), dists: Vec::new() }
    }

    fn ensure(&mut self) {
        if !self.produced {
            self.produced = true;
            let r = if self.by_score {
                // `BY_SCORE`: the scan's results through `std::sort` on the score alone.
                let mut r = self.vi.range_unsorted(&self.blob, self.radius);
                std_sort(&mut r, |a, b| a.1 < b.1);
                r
            } else {
                self.vi.range(&self.blob, self.radius)
            };
            self.list.ids = r.iter().map(|x| x.0).collect();
            self.dists = r.iter().map(|x| x.1).collect();
        }
    }

    fn set_metric(&mut self) {
        if !self.yields || self.list.offset == 0 {
            return;
        }
        let v = self.dists[self.list.offset - 1];
        self.list.res.num = v;
        self.list.res.metrics.clear();
        if let Some(k) = self.key {
            self.list.res.metrics.push((k, v));
        }
    }
}

impl Iter for RangeIt<'_> {
    fn read(&mut self) -> bool {
        self.ensure();
        if self.list.at_eof() {
            return false;
        }
        let r = self.list.read();
        if r {
            self.set_metric();
        }
        r
    }
    fn skip_to(&mut self, id: DocId) -> Skip {
        self.ensure();
        let r = self.list.skip_to(id);
        if r != Skip::Eof {
            self.set_metric();
        }
        r
    }
    fn current(&mut self) -> Option<&mut IxResult> {
        self.list.current()
    }
    fn last_doc_id(&self) -> DocId {
        self.list.last_doc_id()
    }
    fn at_eof(&self) -> bool {
        self.produced && self.list.at_eof()
    }
    fn num_estimated(&self) -> usize {
        if self.produced { self.list.num_estimated() } else { self.vi.size() }
    }
    fn rewind(&mut self) {
        self.list.rewind();
    }
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        if !self.yields {
            return self.list.print(pc, c);
        }
        let mut m = Vec::new();
        let ty = if self.by_score { "METRIC SORTED BY SCORE - VECTOR DISTANCE" } else { "METRIC SORTED BY ID - VECTOR DISTANCE" };
        kv(&mut m, "Type", Reply::Status(ty.into()));
        print_counters(&mut m, pc, c);
        kv(&mut m, "Vector search mode", Reply::Status("RANGE_QUERY".into()));
        m
    }
}

/// libstdc++'s `std::sort` (introsort: median-of-three unguarded partitions down to runs of
/// 16, heapsort past `2 * lg n` levels, then a final insertion sort), so equal elements end up
/// where the module's do.
pub fn std_sort<T>(v: &mut [T], less: impl Fn(&T, &T) -> bool + Copy) {
    let n = v.len();
    if n < 2 {
        return;
    }
    let depth = 2 * (usize::BITS - 1 - n.leading_zeros()) as usize;
    introsort_loop(v, depth, less);
    // `__final_insertion_sort`
    if n > 16 {
        insertion_sort(&mut v[..16], less);
        for i in 16..n {
            unguarded_linear_insert(v, i, less);
        }
    } else {
        insertion_sort(v, less);
    }
}

fn introsort_loop<T>(v: &mut [T], mut depth: usize, less: impl Fn(&T, &T) -> bool + Copy) {
    let mut last = v.len();
    while last > 16 {
        if depth == 0 {
            heap_sort(&mut v[..last], less);
            return;
        }
        depth -= 1;
        // `__unguarded_partition_pivot`
        let mid = last / 2;
        move_median_to_first(v, 0, 1, mid, last - 1, less);
        let (mut lo, mut hi) = (1, last);
        let cut = loop {
            while less(&v[lo], &v[0]) {
                lo += 1;
            }
            hi -= 1;
            while less(&v[0], &v[hi]) {
                hi -= 1;
            }
            if lo >= hi {
                break lo;
            }
            v.swap(lo, hi);
            lo += 1;
        };
        introsort_loop(&mut v[cut..last], depth, less);
        last = cut;
    }
}

fn move_median_to_first<T>(v: &mut [T], result: usize, a: usize, b: usize, c: usize, less: impl Fn(&T, &T) -> bool) {
    let m = if less(&v[a], &v[b]) {
        if less(&v[b], &v[c]) {
            b
        } else if less(&v[a], &v[c]) {
            c
        } else {
            a
        }
    } else if less(&v[a], &v[c]) {
        a
    } else if less(&v[b], &v[c]) {
        c
    } else {
        b
    };
    v.swap(result, m);
}

fn insertion_sort<T>(v: &mut [T], less: impl Fn(&T, &T) -> bool + Copy) {
    for i in 1..v.len() {
        if less(&v[i], &v[0]) {
            v[..=i].rotate_right(1);
        } else {
            unguarded_linear_insert(v, i, less);
        }
    }
}

fn unguarded_linear_insert<T>(v: &mut [T], mut i: usize, less: impl Fn(&T, &T) -> bool) {
    while less(&v[i], &v[i - 1]) {
        v.swap(i, i - 1);
        i -= 1;
    }
}

/// `__partial_sort(first, last, last)`: `__make_heap`, then `__sort_heap`.
fn heap_sort<T>(v: &mut [T], less: impl Fn(&T, &T) -> bool + Copy) {
    let len = v.len();
    if len < 2 {
        return;
    }
    let mut parent = (len - 2) / 2;
    loop {
        adjust_heap(v, parent, len, less);
        if parent == 0 {
            break;
        }
        parent -= 1;
    }
    let mut last = len;
    while last > 1 {
        last -= 1;
        // `__pop_heap(first, last, last)`: the top goes to `last`, its value re-sifted.
        v.swap(0, last);
        adjust_heap(v, 0, last, less);
    }
}

/// `__adjust_heap` with the value at `hole` (moves are swaps along the hole's path, so the
/// value travels with it).
fn adjust_heap<T>(v: &mut [T], mut hole: usize, len: usize, less: impl Fn(&T, &T) -> bool) {
    let top = hole;
    let mut child = hole;
    while child < (len - 1) / 2 {
        child = 2 * (child + 1);
        if less(&v[child], &v[child - 1]) {
            child -= 1;
        }
        v.swap(hole, child);
        hole = child;
    }
    if len & 1 == 0 && child == (len - 2) / 2 {
        child = 2 * (child + 1);
        v.swap(hole, child - 1);
        hole = child - 1;
    }
    // `__push_heap`
    while hole > top {
        let parent = (hole - 1) / 2;
        if !less(&v[parent], &v[hole]) {
            break;
        }
        v.swap(hole, parent);
        hole = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn std_sort_sorts() {
        let mut seed = 12345u64;
        for n in [0usize, 1, 2, 5, 16, 17, 40, 300, 2000] {
            let mut v: Vec<(u32, usize)> = (0..n)
                .map(|i| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    (((seed >> 33) % 7) as u32, i)
                })
                .collect();
            std_sort(&mut v, |a, b| a.0 < b.0);
            assert!(v.windows(2).all(|w| w[0].0 <= w[1].0));
            let mut ids: Vec<usize> = v.iter().map(|x| x.1).collect();
            ids.sort();
            assert_eq!(ids, (0..n).collect::<Vec<_>>());
        }
    }

    #[test]
    fn strtoll_base0() {
        assert_eq!(strtoll0(b"10"), Some(10));
        assert_eq!(strtoll0(b"0x10"), Some(16));
        assert_eq!(strtoll0(b"010"), Some(8));
        assert_eq!(strtoll0(b" +7"), Some(7));
        assert_eq!(strtoll0(b"3.5"), None);
        assert_eq!(strtoll0(b""), None);
        assert_eq!(strtoll0(b"0"), Some(0));
    }

    #[test]
    fn params() {
        let p = |n: &str, v: &str| vec![(n.as_bytes().to_vec(), v.as_bytes().to_vec())];
        assert!(resolve_params(Algo::Flat, &p("EF_RUNTIME", "5"), QType::Knn).is_err());
        assert_eq!(resolve_params(Algo::Hnsw, &p("EF_RUNTIME", "5"), QType::Knn).unwrap().ef_runtime, 5);
        let e = resolve_params(Algo::Flat, &p("BATCH_SIZE", "5"), QType::Knn).unwrap_err();
        assert_eq!(e.text(), "SEARCH_HYBRID_ATTR_NON_HYBRID hybrid query attributes were sent for a non-hybrid query (Error parsing vector similarity parameters)");
    }
}
