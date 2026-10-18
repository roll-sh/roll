//! Query iterators (the module's `rqe_iterators`): pull iterators over the index that yield
//! `RSIndexResult`s in doc id order — inverted indexes (terms, tags, numeric and geo ranges,
//! missing and existing docs), wildcards, id lists, and the intersection, union, not and
//! optional combinators — with the module's exact child orders, which scoring and offsets see.

use super::doctable::DocTable;
use super::invidx::{InvertedIndex, Reader, Record, flags as iflags, offsets as offsets_iter};
use super::numtree::NumericFilter;
use redis_proto::Reply;
use std::rc::Rc;
use std::time::Instant;

pub type DocId = u64;

pub const K_UNION: u8 = 1;
pub const K_INTERSECT: u8 = 2;
pub const K_TERM: u8 = 4;
pub const K_VIRTUAL: u8 = 8;
pub const K_NUMERIC: u8 = 16;
pub const K_METRIC: u8 = 32;

pub const MASK_ALL: u128 = u128::MAX;

/// `RSQueryTerm`.
#[derive(Clone, Debug, Default)]
pub struct Term {
    pub s: Vec<u8>,
    pub idf: f64,
    pub bm25_idf: f64,
    /// The query's token id (`q->tokenId++` per reader opened), which the highlighter's
    /// fragments count distinct terms by.
    pub id: u32,
}

/// `RSIndexResult` (aggregates own copies of their children's results).
#[derive(Clone, Debug, Default)]
pub struct IxResult {
    pub doc_id: DocId,
    pub freq: u32,
    pub field_mask: u128,
    pub weight: f64,
    pub kind: u8,
    pub kind_mask: u8,
    pub term: Option<Rc<Term>>,
    pub offsets: Vec<u8>,
    pub num: f64,
    pub children: Vec<IxResult>,
}

impl IxResult {
    fn virt() -> Self {
        IxResult { kind: K_VIRTUAL, ..Default::default() }
    }

    fn aggregate(kind: u8) -> Self {
        IxResult { kind, ..Default::default() }
    }

    pub fn is_aggregate(&self) -> bool {
        self.kind & (K_UNION | K_INTERSECT) != 0
    }

    /// `reset_aggregate`.
    pub(super) fn reset_aggregate(&mut self) {
        self.doc_id = 0;
        self.freq = 0;
        self.field_mask = 0;
        self.kind_mask = 0;
        self.children.clear();
    }

    /// `push_borrowed`.
    pub(super) fn push(&mut self, child: &IxResult) {
        self.doc_id = child.doc_id;
        self.freq += child.freq;
        self.field_mask |= child.field_mask;
        self.kind_mask |= child.kind;
        self.children.push(child.clone());
    }

    /// `has_offsets`.
    pub fn has_offsets(&self) -> bool {
        match self.kind {
            K_TERM => !self.offsets.is_empty(),
            K_UNION | K_INTERSECT => self.kind_mask != K_VIRTUAL && self.kind_mask != (K_NUMERIC | K_METRIC),
            _ => false,
        }
    }

    /// `iterate_offsets`.
    pub fn offsets(&self) -> Offsets<'_> {
        match self.kind {
            K_TERM => Offsets::Term(offsets_iter(&self.offsets)),
            K_UNION | K_INTERSECT if self.children.len() == 1 => self.children[0].offsets(),
            K_UNION | K_INTERSECT => {
                let mut children: Vec<Offsets<'_>> = self.children.iter().map(|c| c.offsets()).collect();
                let positions = children.iter_mut().map(|c| c.next()).collect();
                Offsets::Merge { children, positions }
            }
            _ => Offsets::Empty,
        }
    }

    /// `IndexResult_IsWithinRange`.
    pub fn is_within_range(&self, max_slop: Option<u32>, in_order: bool) -> bool {
        if !self.is_aggregate() || self.children.len() <= 1 {
            return true;
        }
        let mut iters: Vec<Offsets<'_>> = self.children.iter().filter(|c| c.has_offsets()).map(|c| c.offsets()).collect();
        if iters.len() <= 1 {
            return true;
        }
        let max_slop = max_slop.unwrap_or(u32::MAX);
        if in_order { within_in_order(&mut iters, max_slop) } else { within_unordered(&mut iters, max_slop) }
    }

    /// `IndexResult_MinOffsetDelta`.
    pub fn min_offset_delta(&self) -> i32 {
        if !self.is_aggregate() || self.children.len() <= 1 {
            return 1;
        }
        let num = self.children.len();
        let eof = u32::MAX;
        let mut dist: i64 = 0;
        let mut i = 0;
        while i < num {
            // the next pair of children with offsets
            while i < num && !self.children[i].has_offsets() {
                i += 1;
            }
            if i == num {
                break;
            }
            let v1 = &self.children[i];
            i += 1;
            while i < num && !self.children[i].has_offsets() {
                i += 1;
            }
            if i == num {
                break;
            }
            let v2 = &self.children[i];
            let mut it1 = v1.offsets();
            let mut it2 = v2.offsets();
            let mut p1 = it1.next().unwrap_or(eof);
            let mut p2 = it2.next().unwrap_or(eof);
            let absdelta = |a: u32, b: u32| a.abs_diff(b);
            let mut cd = absdelta(p2, p1) as i32;
            while cd > 1 && p1 != eof && p2 != eof {
                cd = cd.min(absdelta(p2, p1) as i32);
                if p2 > p1 {
                    p1 = it1.next().unwrap_or(eof);
                } else {
                    p2 = it2.next().unwrap_or(eof);
                }
            }
            dist += cd as i64 * cd as i64;
        }
        if dist != 0 { (dist as f64).sqrt() as i32 } else { num as i32 - 1 }
    }
}

/// An offsets iterator (`OffsetIter`).
pub enum Offsets<'a> {
    Empty,
    Term(super::invidx::OffsetsIter<'a>),
    Merge { children: Vec<Offsets<'a>>, positions: Vec<Option<u32>> },
}

impl Iterator for Offsets<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        match self {
            Offsets::Empty => None,
            Offsets::Term(it) => it.next(),
            Offsets::Merge { children, positions } => {
                let (idx, v) = positions.iter().enumerate().filter_map(|(i, o)| o.map(|v| (i, v))).min_by_key(|&(_, v)| v)?;
                positions[idx] = children[idx].next();
                Some(v)
            }
        }
    }
}

fn within_in_order(iters: &mut [Offsets<'_>], max_slop: u32) -> bool {
    let n = iters.len();
    let mut positions = vec![0u32; n];
    loop {
        let mut span: i32 = 0;
        let mut over = false;
        for i in 0..n {
            let mut pos = if i == 0 {
                match iters[0].next() {
                    Some(p) => p,
                    None => return false,
                }
            } else {
                positions[i]
            };
            let last = if i == 0 { 0 } else { positions[i - 1] };
            while pos < last {
                match iters[i].next() {
                    Some(p) => pos = p,
                    None => return false,
                }
            }
            positions[i] = pos;
            if i > 0 {
                span += pos as i32 - last as i32 - 1;
                if span > 0 && span as u32 > max_slop {
                    over = true;
                    break;
                }
            }
        }
        if !over {
            return true;
        }
    }
}

fn within_unordered(iters: &mut [Offsets<'_>], max_slop: u32) -> bool {
    let n = iters.len();
    let Some(mut positions) = iters.iter_mut().map(|it| it.next()).collect::<Option<Vec<u32>>>() else { return false };
    let mut max_pos = positions.iter().fold(0u32, |m, &v| if v >= m { v } else { m });
    loop {
        let (min_idx, min_pos) = positions.iter().copied().enumerate().min_by_key(|&(_, v)| v).unwrap_or((0, u32::MAX));
        if min_pos != max_pos {
            let span = max_pos as i32 - min_pos as i32 - (n as i32 - 1);
            if span < 0 || span as u32 <= max_slop {
                return true;
            }
        }
        let Some(p) = iters[min_idx].next() else { break };
        positions[min_idx] = p;
        if p > max_pos {
            max_pos = p;
        }
    }
    false
}

/// `SkipToOutcome` (and EOF).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    Found,
    NotFound,
    Eof,
}

/// The iterator types the reducers look at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IKind {
    Empty,
    Wildcard,
    Union,
    Intersect,
    Other,
}

/// `RQEIterator`.
pub trait Iter {
    /// The next result; false at EOF.
    fn read(&mut self) -> bool;
    fn skip_to(&mut self, id: DocId) -> Skip;
    fn current(&mut self) -> Option<&mut IxResult>;
    fn last_doc_id(&self) -> DocId;
    fn at_eof(&self) -> bool;
    fn num_estimated(&self) -> usize;
    /// `Rewind`: back to before the first read.
    fn rewind(&mut self);
    fn kind(&self) -> IKind {
        IKind::Other
    }
    fn sort_weight(&self, _prioritize_unions: bool) -> f64 {
        1.0
    }
    /// `set_result_weight` (unions).
    fn set_weight(&mut self, _w: f64) {}
    /// A union's query node type and string, which its profile shows (`NewUnionIterator`'s
    /// `type` and `q_str`).
    fn set_query(&mut self, _ty: &'static str, _s: Option<Vec<u8>>) {}
    /// `ProfileChildren`: wrap every child (recursively) in a [`Prof`].
    fn profile_children(&mut self) {}
    /// `PrintProfile`: the iterator's profile map; `c` holds the counters of the [`Prof`]
    /// wrapping it.
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs;
}

pub type BoxIt<'a> = Box<dyn Iter + 'a>;

// ---------------------------------------------------------------------------------------------
// Profiling (`Profile_AddIters`, `Profile_PrintIterators`)

/// A profile map's entries.
pub type Pairs = Vec<(Reply, Reply)>;

/// `ProfilePrintCtx`'s options: LIMITED collapses union children, and the clock prints times.
#[derive(Clone, Copy, Debug)]
pub struct PCtx {
    pub limited: bool,
    pub clock: bool,
}

/// `ProfileCounters` and the wrapper's wall time.
#[derive(Clone, Copy, Debug, Default)]
pub struct Counters {
    pub read: u64,
    pub skip: u64,
    pub eof: bool,
    pub ns: u64,
}

impl Counters {
    /// `num_reading_operations`: reads and skips, the final EOF one aside.
    fn ops(&self) -> i64 {
        (self.read + self.skip).saturating_sub(self.eof as u64) as i64
    }
}

pub(super) fn kv(m: &mut Pairs, k: &str, v: Reply) {
    m.push((Reply::Status(k.into()), v));
}

/// `print_optional_counters`.
pub(super) fn print_counters(m: &mut Pairs, pc: &PCtx, c: Option<&Counters>) {
    if pc.clock {
        kv(m, "Time", Reply::Double(c.map_or(0, |c| c.ns) as f64 / 1_000_000.0));
    }
    if let Some(c) = c {
        kv(m, "Number of reading operations", Reply::Int(c.ops()));
    }
}

/// `print_leaf`.
pub(super) fn print_leaf(ty: &str, pc: &PCtx, c: Option<&Counters>) -> Pairs {
    let mut m = Vec::new();
    kv(&mut m, "Type", Reply::Status(ty.into()));
    print_counters(&mut m, pc, c);
    m
}

/// `print_single_child`.
fn print_single(ty: &str, child: &BoxIt<'_>, pc: &PCtx, c: Option<&Counters>) -> Pairs {
    let mut m = print_leaf(ty, pc, c);
    kv(&mut m, "Child iterator", Reply::Map(child.print(pc, None)));
    m
}

/// `Profile`: counts its child's reads and skips and times them.
pub struct Prof<'a> {
    child: BoxIt<'a>,
    c: Counters,
}

impl Iter for Prof<'_> {
    fn read(&mut self) -> bool {
        let t = Instant::now();
        let r = self.child.read();
        self.c.ns += t.elapsed().as_nanos() as u64;
        self.c.read += 1;
        if !r {
            self.c.eof = true;
        }
        r
    }
    fn skip_to(&mut self, id: DocId) -> Skip {
        let t = Instant::now();
        let r = self.child.skip_to(id);
        self.c.ns += t.elapsed().as_nanos() as u64;
        self.c.skip += 1;
        if r == Skip::Eof {
            self.c.eof = true;
        }
        r
    }
    fn current(&mut self) -> Option<&mut IxResult> {
        self.child.current()
    }
    fn last_doc_id(&self) -> DocId {
        self.child.last_doc_id()
    }
    fn at_eof(&self) -> bool {
        self.child.at_eof()
    }
    fn num_estimated(&self) -> usize {
        self.child.num_estimated()
    }
    fn rewind(&mut self) {
        self.child.rewind();
    }
    fn kind(&self) -> IKind {
        self.child.kind()
    }
    fn sort_weight(&self, p: bool) -> f64 {
        self.child.sort_weight(p)
    }
    fn set_weight(&mut self, w: f64) {
        self.child.set_weight(w);
    }
    fn set_query(&mut self, ty: &'static str, q: Option<Vec<u8>>) {
        self.child.set_query(ty, q);
    }
    fn print(&self, pc: &PCtx, _c: Option<&Counters>) -> Pairs {
        self.child.print(pc, Some(&self.c))
    }
}

/// `format_g`: C's `%g`.
fn fmt_g(v: f64) -> String {
    crate::util::fmt_g_prec(v, 6)
}

/// A union's profile (`UnionOpaque::print_profile`): LIMITED collapses the children of all
/// but plain, geo and lexrange unions into their count.
pub(super) fn print_union<'c, 'a: 'c>(q: &(&'static str, Option<Vec<u8>>), children: impl ExactSizeIterator<Item = &'c BoxIt<'a>>, pc: &PCtx, c: Option<&Counters>) -> Pairs {
    let mut m = Vec::new();
    kv(&mut m, "Type", Reply::Status("UNION".into()));
    let ty = q.0;
    match &q.1 {
        None => kv(&mut m, "Query type", Reply::Status(ty.into())),
        Some(s) => kv(&mut m, "Query type", Reply::Bulk(format!("{ty} - {}", String::from_utf8_lossy(s)).into_bytes().into())),
    }
    print_counters(&mut m, pc, c);
    if !pc.limited || matches!(ty, "UNION" | "GEO" | "LEXRANGE") {
        kv(&mut m, "Child iterators", Reply::Array(children.map(|ch| Reply::Map(ch.print(pc, None))).collect()));
    } else {
        kv(&mut m, "Child iterators", Reply::Status(format!("The number of iterators in the union is {}", children.len())));
    }
    m
}

/// `into_profiled`: the subtree's iterators each wrapped in a [`Prof`], this one too.
pub fn profiled<'a>(mut it: BoxIt<'a>) -> BoxIt<'a> {
    it.profile_children();
    Box::new(Prof { child: it, c: Counters::default() })
}

pub(super) fn profile_child<'a>(c: &mut BoxIt<'a>) {
    let old = std::mem::replace(c, Box::new(Empty));
    *c = profiled(old);
}

/// What an inverted index iterator's profile shows it reads.
#[derive(Clone, Debug, Default)]
pub enum PKind {
    #[default]
    Text,
    Tag,
    /// The numeric range's min and max values.
    Numeric(f64, f64),
    /// The geo range's min and max (geohashes).
    Geo(f64, f64),
    /// The field's name.
    Missing(Vec<u8>),
    Wildcard,
}

/// What a query's iterators share: the index's doc table, the clock, and the text field
/// ids' schema positions (`fieldIdToIndex`).
pub struct Qcx<'a> {
    pub docs: &'a DocTable,
    pub now_ms: i64,
    pub ft_to_field: Vec<u16>,
}

/// `FieldExpirationPredicate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Predicate {
    Default,
    Missing,
}

/// `FieldExpirationChecker`'s field: a text field mask, or a schema position.
#[derive(Clone, Copy, Debug)]
pub enum Checker {
    Mask(u128, bool),
    Field(usize, Predicate),
}

fn did_expire(point: i64, now: i64) -> bool {
    point != 0 && point <= now
}

impl Checker {
    /// `is_expired`.
    fn is_expired(&self, cx: &Qcx<'_>, res: &IxResult) -> bool {
        let Some(d) = cx.docs.get(res.doc_id) else { return false };
        let fe = &d.field_expirations;
        if fe.is_empty() {
            return false;
        }
        match *self {
            Checker::Field(index, pred) => {
                let valid = match fe.iter().find(|(i, _)| *i == index) {
                    Some(&(_, at)) => {
                        if did_expire(at, cx.now_ms) {
                            pred == Predicate::Missing
                        } else {
                            pred == Predicate::Default
                        }
                    }
                    None => pred == Predicate::Default,
                };
                !valid
            }
            Checker::Mask(mask, wide) => {
                let mut m = res.field_mask & mask;
                if !wide {
                    m &= u32::MAX as u128;
                }
                !verify_mask(fe, m, cx)
            }
        }
    }
}

/// `verify_mask` (the default predicate).
fn verify_mask(fe: &[(usize, i64)], mask: u128, cx: &Qcx<'_>) -> bool {
    let n = fe.len();
    let count = mask.count_ones() as usize;
    if n < count {
        return true;
    }
    let mut misses = 0;
    let mut cur = 0;
    for bit in 0..128 {
        if mask & (1u128 << bit) == 0 {
            continue;
        }
        let Some(&want) = cx.ft_to_field.get(bit) else { break };
        let want = want as usize;
        while cur < n && want > fe[cur].0 {
            cur += 1;
        }
        if cur >= n {
            break;
        }
        if want < fe[cur].0 {
            continue;
        }
        if !did_expire(fe[cur].1, cx.now_ms) {
            return true;
        }
        misses += 1;
    }
    misses < count
}

/// The reader-level filter (`FilterMaskReader`, `FilterNumericReader`, `FilterGeoReader`).
pub enum RFilter {
    None,
    Mask(u128),
    Numeric(NumericFilter),
    /// Center lon, lat and the radius in meters.
    Geo(f64, f64, f64),
}

impl RFilter {
    fn passes(&self, r: &mut Record<'_>) -> bool {
        match self {
            RFilter::None => true,
            RFilter::Mask(m) => r.field_mask & m != 0,
            RFilter::Numeric(f) => f.value_in_range(r.numeric),
            RFilter::Geo(lon, lat, radius) => {
                let (x, y) = super::geo::decode_geo(r.numeric);
                let d = super::geo::distance(*lon, *lat, x, y);
                r.numeric = d;
                d <= *radius
            }
        }
    }
}

/// `InvIndIterator` with its reader, filter and expiration checker.
pub struct InvIt<'a> {
    reader: Reader<'a>,
    filter: RFilter,
    store: u32,
    skip_multi: bool,
    checker: Option<Checker>,
    cx: &'a Qcx<'a>,
    at_eos: bool,
    last: DocId,
    res: IxResult,
    unique: usize,
    wildcard: bool,
    pk: PKind,
}

impl<'a> InvIt<'a> {
    pub fn new(ii: &'a InvertedIndex, res: IxResult, filter: RFilter, checker: Option<Checker>, cx: &'a Qcx<'a>) -> Self {
        InvIt {
            reader: ii.reader(),
            filter,
            store: ii.flags(),
            skip_multi: ii.flags() & iflags::HAS_MULTI_VALUE != 0,
            checker,
            cx,
            at_eos: false,
            last: 0,
            res,
            unique: ii.unique_docs() as usize,
            wildcard: false,
            pk: PKind::Text,
        }
    }

    /// What the iterator's profile shows it reads.
    pub fn profile_as(mut self, pk: PKind) -> Self {
        self.pk = pk;
        self
    }

    /// The existing docs' wildcard (`inverted_index::Wildcard`).
    pub fn wildcard(ii: &'a InvertedIndex, weight: f64, cx: &'a Qcx<'a>) -> Self {
        let res = IxResult { weight, field_mask: MASK_ALL, freq: 1, ..IxResult::virt() };
        let mut it = InvIt::new(ii, res, RFilter::None, None, cx);
        it.wildcard = true;
        it.pk = PKind::Wildcard;
        it
    }

    fn apply(&mut self, r: &Record<'_>) {
        self.res.doc_id = r.doc_id;
        if self.store & iflags::STORE_FREQS != 0 {
            self.res.freq = r.freq;
        }
        if self.store & iflags::STORE_FIELD_FLAGS != 0 {
            self.res.field_mask = r.field_mask;
        }
        if self.store & iflags::STORE_TERM_OFFSETS != 0 {
            self.res.offsets.clear();
            self.res.offsets.extend_from_slice(r.offsets);
        }
        if self.store & iflags::STORE_NUMERIC != 0 {
            self.res.num = r.numeric;
        }
    }

    fn next_record(&mut self) -> bool {
        loop {
            let Some(mut r) = self.reader.next_record() else { return false };
            if self.filter.passes(&mut r) {
                self.apply(&r);
                return true;
            }
        }
    }

    fn seek_record(&mut self, id: DocId) -> bool {
        let Some(mut r) = self.reader.seek(id) else { return false };
        if self.filter.passes(&mut r) {
            self.apply(&r);
            true
        } else {
            self.next_record()
        }
    }

    fn expired(&self) -> bool {
        self.checker.is_some_and(|c| c.is_expired(self.cx, &self.res))
    }
}

impl Iter for InvIt<'_> {
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        let mut m = Vec::new();
        let term = |m: &mut Pairs| {
            if let Some(t) = &self.res.term {
                kv(m, "Term", Reply::Bulk(t.s.clone().into()));
            }
        };
        match &self.pk {
            PKind::Wildcard => return print_leaf("WILDCARD", pc, c),
            PKind::Text => {
                kv(&mut m, "Type", Reply::Status("TEXT".into()));
                term(&mut m);
            }
            PKind::Tag => {
                kv(&mut m, "Type", Reply::Status("TAG".into()));
                term(&mut m);
            }
            PKind::Numeric(lo, hi) => {
                kv(&mut m, "Type", Reply::Status("NUMERIC".into()));
                kv(&mut m, "Term", Reply::Status(format!("{} - {}", fmt_g(*lo), fmt_g(*hi))));
            }
            PKind::Geo(lo, hi) => {
                let (se_lon, se_lat) = super::geo::decode_geo(*lo);
                let (nw_lon, nw_lat) = super::geo::decode_geo(*hi);
                kv(&mut m, "Type", Reply::Status("GEO".into()));
                kv(&mut m, "Term", Reply::Status(format!("{},{} - {},{}", fmt_g(se_lon), fmt_g(se_lat), fmt_g(nw_lon), fmt_g(nw_lat))));
            }
            PKind::Missing(f) => {
                kv(&mut m, "Type", Reply::Status("MISSING".into()));
                if !f.is_empty() {
                    kv(&mut m, "Field", Reply::Bulk(f.clone().into()));
                }
            }
        }
        print_counters(&mut m, pc, c);
        kv(&mut m, "Estimated number of matches", Reply::Int(self.num_estimated() as i64));
        m
    }

    fn read(&mut self) -> bool {
        if self.at_eos {
            return false;
        }
        while self.next_record() {
            if self.skip_multi && self.last == self.res.doc_id {
                continue;
            }
            if self.expired() {
                continue;
            }
            self.last = self.res.doc_id;
            return true;
        }
        self.at_eos = true;
        false
    }

    fn skip_to(&mut self, id: DocId) -> Skip {
        if self.at_eos {
            return Skip::Eof;
        }
        if !self.seek_record(id) {
            self.at_eos = true;
            return Skip::Eof;
        }
        if !self.expired() {
            self.last = self.res.doc_id;
            return if self.res.doc_id == id { Skip::Found } else { Skip::NotFound };
        }
        if self.read() { Skip::NotFound } else { Skip::Eof }
    }

    fn current(&mut self) -> Option<&mut IxResult> {
        Some(&mut self.res)
    }

    fn last_doc_id(&self) -> DocId {
        self.last
    }

    fn at_eof(&self) -> bool {
        self.at_eos
    }

    fn num_estimated(&self) -> usize {
        self.unique
    }

    fn rewind(&mut self) {
        self.at_eos = false;
        self.last = 0;
        self.res.doc_id = 0;
        self.reader.reset();
    }

    fn kind(&self) -> IKind {
        if self.wildcard { IKind::Wildcard } else { IKind::Other }
    }
}

/// `Empty`.
pub struct Empty;

impl Iter for Empty {
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        print_leaf("EMPTY", pc, c)
    }
    fn read(&mut self) -> bool {
        false
    }
    fn skip_to(&mut self, _id: DocId) -> Skip {
        Skip::Eof
    }
    fn current(&mut self) -> Option<&mut IxResult> {
        None
    }
    fn last_doc_id(&self) -> DocId {
        0
    }
    fn at_eof(&self) -> bool {
        true
    }
    fn num_estimated(&self) -> usize {
        0
    }
    fn rewind(&mut self) {}
    fn kind(&self) -> IKind {
        IKind::Empty
    }
}

/// `Wildcard`: every id up to the table's max.
pub struct Wildcard {
    top: DocId,
    res: IxResult,
}

impl Wildcard {
    pub fn new(top: DocId, weight: f64) -> Self {
        Wildcard { top, res: IxResult { freq: 1, weight, field_mask: MASK_ALL, ..IxResult::virt() } }
    }
}

impl Iter for Wildcard {
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        print_leaf("WILDCARD", pc, c)
    }
    fn read(&mut self) -> bool {
        if self.at_eof() {
            return false;
        }
        self.res.doc_id += 1;
        true
    }
    fn skip_to(&mut self, id: DocId) -> Skip {
        if self.at_eof() {
            return Skip::Eof;
        }
        if id > self.top {
            self.res.doc_id = self.top;
            return Skip::Eof;
        }
        self.res.doc_id = id;
        Skip::Found
    }
    fn current(&mut self) -> Option<&mut IxResult> {
        Some(&mut self.res)
    }
    fn last_doc_id(&self) -> DocId {
        self.res.doc_id
    }
    fn at_eof(&self) -> bool {
        self.res.doc_id >= self.top
    }
    fn num_estimated(&self) -> usize {
        self.top as usize
    }
    fn rewind(&mut self) {
        self.res.doc_id = 0;
    }
    fn kind(&self) -> IKind {
        IKind::Wildcard
    }
}

/// `IdListSorted`.
pub struct IdList {
    ids: Vec<DocId>,
    offset: usize,
    res: IxResult,
}

impl IdList {
    pub fn new(ids: Vec<DocId>, res: IxResult) -> Self {
        IdList { ids, offset: 0, res }
    }
}

impl Iter for IdList {
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        print_leaf("ID-LIST-SORTED", pc, c)
    }
    fn read(&mut self) -> bool {
        let Some(&id) = self.ids.get(self.offset) else { return false };
        self.offset += 1;
        self.res.doc_id = id;
        true
    }
    fn skip_to(&mut self, target: DocId) -> Skip {
        let len = self.ids.len();
        if self.at_eof() || *self.ids.last().unwrap() < target {
            self.offset = len;
            if len > 0 {
                self.res.doc_id = self.ids[len - 1];
            }
            return Skip::Eof;
        }
        let mut bottom = self.offset;
        let delta = target - self.res.doc_id;
        let mut top = (self.offset + delta as usize).min(len);
        let mut i = 0;
        let mut undershot = false;
        let mut cur = 0;
        while bottom < top {
            i = (bottom + top) >> 1;
            cur = self.ids[i];
            match cur.cmp(&target) {
                std::cmp::Ordering::Equal => {
                    undershot = false;
                    break;
                }
                std::cmp::Ordering::Less => {
                    bottom = i + 1;
                    undershot = true;
                }
                std::cmp::Ordering::Greater => {
                    top = i;
                    undershot = false;
                }
            }
        }
        if undershot {
            i += 1;
            cur = self.ids[i];
        }
        self.res.doc_id = cur;
        self.offset = i + 1;
        if cur == target { Skip::Found } else { Skip::NotFound }
    }
    fn current(&mut self) -> Option<&mut IxResult> {
        Some(&mut self.res)
    }
    fn last_doc_id(&self) -> DocId {
        self.res.doc_id
    }
    fn at_eof(&self) -> bool {
        self.offset >= self.ids.len()
    }
    fn num_estimated(&self) -> usize {
        self.ids.len()
    }
    fn rewind(&mut self) {
        self.offset = 0;
        self.res.doc_id = 0;
    }
}

/// `Intersection`.
pub struct Intersection<'a> {
    children: Vec<BoxIt<'a>>,
    last: DocId,
    num_expected: usize,
    eof: bool,
    max_slop: Option<u32>,
    in_order: bool,
    res: IxResult,
}

enum Agree {
    Agreed,
    Ahead(DocId),
    Eof,
}

impl<'a> Intersection<'a> {
    pub fn new(mut children: Vec<BoxIt<'a>>, weight: f64, prioritize: bool, max_slop: Option<u32>, in_order: bool) -> Self {
        if !in_order {
            children.sort_by(|a, b| {
                let wa = a.num_estimated() as f64 * a.sort_weight(prioritize);
                let wb = b.num_estimated() as f64 * b.sort_weight(prioritize);
                wa.total_cmp(&wb)
            });
        }
        let num_expected = children.iter().map(|c| c.num_estimated()).min();
        let res = IxResult { weight, ..IxResult::aggregate(K_INTERSECT) };
        Intersection { eof: num_expected.is_none(), num_expected: num_expected.unwrap_or(0), children, last: 0, max_slop, in_order, res }
    }

    fn relevancy(&self) -> bool {
        self.max_slop.is_some() || self.in_order
    }

    fn relevant(&self) -> bool {
        self.res.is_within_range(self.max_slop, self.in_order)
    }

    fn read_first(&mut self) -> Option<DocId> {
        if self.children[0].read() {
            Some(self.children[0].current().map_or(0, |r| r.doc_id))
        } else {
            self.eof = true;
            None
        }
    }

    fn agree(&mut self, target: DocId) -> Agree {
        for c in &mut self.children {
            if c.last_doc_id() == target {
                continue;
            }
            match c.skip_to(target) {
                Skip::Eof => {
                    self.eof = true;
                    return Agree::Eof;
                }
                Skip::Found => {}
                Skip::NotFound => return Agree::Ahead(c.current().map_or(0, |r| r.doc_id)),
            }
        }
        Agree::Agreed
    }

    fn consensus(&mut self, mut target: DocId) -> Option<DocId> {
        loop {
            match self.agree(target) {
                Agree::Agreed => return Some(target),
                Agree::Ahead(t) => target = t,
                Agree::Eof => return None,
            }
        }
    }

    fn consensus_relevant(&mut self, mut target: DocId) -> Option<DocId> {
        loop {
            let id = self.consensus(target)?;
            self.build(id);
            if self.relevant() {
                self.last = id;
                return Some(id);
            }
            target = self.read_first()?;
        }
    }

    fn build(&mut self, id: DocId) {
        self.res.freq = 0;
        self.res.field_mask = 0;
        self.res.kind_mask = 0;
        self.res.children.clear();
        self.res.doc_id = id;
        for c in &mut self.children {
            if let Some(r) = c.current() {
                let r = r.clone();
                self.res.push(&r);
            }
        }
    }
}

impl Iter for Intersection<'_> {
    fn profile_children(&mut self) {
        self.children.iter_mut().for_each(profile_child);
    }
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        let mut m = print_leaf("INTERSECT", pc, c);
        kv(&mut m, "Child iterators", Reply::Array(self.children.iter().map(|ch| Reply::Map(ch.print(pc, None))).collect()));
        m
    }
    fn read(&mut self) -> bool {
        if self.eof {
            return false;
        }
        let Some(target) = self.read_first() else { return false };
        if self.relevancy() {
            self.consensus_relevant(target).is_some()
        } else {
            match self.consensus(target) {
                Some(id) => {
                    self.build(id);
                    self.last = id;
                    true
                }
                None => false,
            }
        }
    }

    fn skip_to(&mut self, id: DocId) -> Skip {
        if self.eof {
            return Skip::Eof;
        }
        let Some(found) = self.consensus(id) else { return Skip::Eof };
        self.build(found);
        if self.relevancy() {
            let relevant = self.relevant();
            if found == id && relevant {
                self.last = found;
                return Skip::Found;
            }
            if relevant {
                self.last = found;
                return Skip::NotFound;
            }
            let Some(next) = self.read_first() else { return Skip::Eof };
            return if self.consensus_relevant(next).is_some() { Skip::NotFound } else { Skip::Eof };
        }
        self.last = found;
        if found == id { Skip::Found } else { Skip::NotFound }
    }

    fn current(&mut self) -> Option<&mut IxResult> {
        (!self.eof).then_some(&mut self.res)
    }
    fn last_doc_id(&self) -> DocId {
        self.last
    }
    fn at_eof(&self) -> bool {
        self.eof
    }
    fn num_estimated(&self) -> usize {
        self.num_expected
    }
    fn rewind(&mut self) {
        self.last = 0;
        self.eof = self.children.is_empty();
        self.res.reset_aggregate();
        self.children.iter_mut().for_each(|c| c.rewind());
    }
    fn kind(&self) -> IKind {
        IKind::Intersect
    }
    fn sort_weight(&self, _p: bool) -> f64 {
        1.0 / self.children.len().max(1) as f64
    }
}

/// `new_intersection_iterator`'s reductions: wildcards dropped next to other children, any
/// empty child empties it, a single child stands alone.
pub fn intersect<'a>(children: Vec<BoxIt<'a>>, weight: f64, prioritize: bool, max_slop: Option<u32>, in_order: bool) -> BoxIt<'a> {
    if children.is_empty() {
        return Box::new(Empty);
    }
    let mut last_wildcard: Option<BoxIt<'a>> = None;
    let mut kept: Vec<BoxIt<'a>> = Vec::with_capacity(children.len());
    for c in children {
        if c.kind() == IKind::Wildcard {
            if kept.is_empty() {
                last_wildcard = Some(c);
            }
        } else {
            last_wildcard = None;
            kept.push(c);
        }
    }
    if kept.is_empty() {
        return last_wildcard.map_or_else(|| Box::new(Empty) as BoxIt<'a>, |w| w);
    }
    if kept.iter().any(|c| c.kind() == IKind::Empty) {
        return Box::new(Empty);
    }
    if kept.len() == 1 {
        return kept.pop().unwrap();
    }
    Box::new(Intersection::new(kept, weight, prioritize, max_slop, in_order))
}

/// `UnionFlat` (`QUICK_EXIT` as a flag).
pub struct UnionFlat<'a> {
    children: Vec<BoxIt<'a>>,
    num_active: usize,
    num_estimated: usize,
    eof: bool,
    quick: bool,
    res: IxResult,
    q: (&'static str, Option<Vec<u8>>),
    /// Each child's insertion index (swap-removes reorder them; profiles print them in it).
    orig: Vec<usize>,
}

impl<'a> UnionFlat<'a> {
    pub fn new(children: Vec<BoxIt<'a>>, quick: bool) -> Self {
        let num_estimated = children.iter().map(|c| c.num_estimated()).sum();
        let n = children.len();
        UnionFlat { num_active: children.len(), eof: children.is_empty(), children, num_estimated, quick, res: IxResult::aggregate(K_UNION), q: ("UNION", None), orig: (0..n).collect() }
    }

    fn swap_remove(&mut self, i: usize) {
        self.num_active -= 1;
        if i < self.num_active {
            self.children.swap(i, self.num_active);
            self.orig.swap(i, self.num_active);
        }
    }

    fn add_child(&mut self, i: usize) {
        if let Some(r) = self.children[i].current() {
            let r = r.clone();
            self.res.push(&r);
        }
    }

    fn build(&mut self, min: DocId) {
        self.res.reset_aggregate();
        self.res.doc_id = min;
        for i in 0..self.num_active {
            if self.children[i].last_doc_id() == min {
                self.add_child(i);
            }
        }
    }

    fn quick_set(&mut self, i: usize) {
        self.res.reset_aggregate();
        self.res.doc_id = self.children[i].last_doc_id();
        self.add_child(i);
    }

    fn read_full(&mut self) -> bool {
        let last = self.res.doc_id;
        let mut min = DocId::MAX;
        let mut i = 0;
        while i < self.num_active {
            let c = &mut self.children[i];
            if last == 0 {
                if c.last_doc_id() == 0 && (c.at_eof() || !c.read()) {
                    self.swap_remove(i);
                    continue;
                }
            } else if c.last_doc_id() == last && !c.read() {
                self.swap_remove(i);
                continue;
            }
            min = min.min(self.children[i].last_doc_id());
            i += 1;
        }
        if min == DocId::MAX {
            self.eof = true;
            return false;
        }
        self.build(min);
        true
    }

    fn skip_full(&mut self, id: DocId) -> Skip {
        let mut min = DocId::MAX;
        let mut i = 0;
        self.res.reset_aggregate();
        while i < self.num_active {
            let cl = self.children[i].last_doc_id();
            if cl >= id {
                min = min.min(cl);
                if cl == id {
                    self.add_child(i);
                }
                i += 1;
                continue;
            }
            match self.children[i].skip_to(id) {
                Skip::Found => {
                    min = min.min(self.children[i].last_doc_id_of_current());
                    self.add_child(i);
                }
                Skip::NotFound => min = min.min(self.children[i].last_doc_id_of_current()),
                Skip::Eof => {
                    self.swap_remove(i);
                    continue;
                }
            }
            i += 1;
        }
        if min == DocId::MAX {
            self.eof = true;
            return Skip::Eof;
        }
        if min == id {
            self.res.doc_id = min;
            Skip::Found
        } else {
            self.build(min);
            Skip::NotFound
        }
    }

    fn skip_quick(&mut self, id: DocId) -> Skip {
        let mut min = DocId::MAX;
        let mut min_i = 0;
        let mut i = 0;
        while i < self.num_active {
            let cl = self.children[i].last_doc_id();
            if cl < id {
                match self.children[i].skip_to(id) {
                    Skip::Found => {
                        self.quick_set(i);
                        return Skip::Found;
                    }
                    Skip::NotFound => {
                        let d = self.children[i].last_doc_id_of_current();
                        if d < min {
                            min = d;
                            min_i = i;
                        }
                    }
                    Skip::Eof => {
                        self.swap_remove(i);
                        continue;
                    }
                }
            } else if cl == id {
                self.quick_set(i);
                return Skip::Found;
            } else if cl < min {
                min = cl;
                min_i = i;
            }
            i += 1;
        }
        if min != DocId::MAX {
            self.quick_set(min_i);
            Skip::NotFound
        } else {
            self.eof = true;
            Skip::Eof
        }
    }
}

trait CurrentId {
    fn last_doc_id_of_current(&mut self) -> DocId;
}

impl CurrentId for BoxIt<'_> {
    /// The doc id of a `skip_to`'s returned result.
    fn last_doc_id_of_current(&mut self) -> DocId {
        self.current().map_or(0, |r| r.doc_id)
    }
}

impl Iter for UnionFlat<'_> {
    fn set_query(&mut self, ty: &'static str, s: Option<Vec<u8>>) {
        self.q = (ty, s);
    }
    fn profile_children(&mut self) {
        self.children.iter_mut().for_each(profile_child);
    }
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        let mut by_orig: Vec<(usize, &BoxIt<'_>)> = self.orig.iter().copied().zip(&self.children).collect();
        by_orig.sort_by_key(|&(o, _)| o);
        print_union(&self.q, by_orig.into_iter().map(|(_, c)| c), pc, c)
    }
    fn read(&mut self) -> bool {
        if self.eof {
            return false;
        }
        if self.quick {
            let next = self.res.doc_id.saturating_add(1);
            self.skip_to(next) != Skip::Eof
        } else {
            self.read_full()
        }
    }
    fn skip_to(&mut self, id: DocId) -> Skip {
        if self.eof {
            return Skip::Eof;
        }
        if self.quick { self.skip_quick(id) } else { self.skip_full(id) }
    }
    fn current(&mut self) -> Option<&mut IxResult> {
        (!self.eof).then_some(&mut self.res)
    }
    fn last_doc_id(&self) -> DocId {
        self.res.doc_id
    }
    fn at_eof(&self) -> bool {
        self.eof
    }
    fn num_estimated(&self) -> usize {
        self.num_estimated
    }
    fn rewind(&mut self) {
        // The children back in their insertion order.
        let mut all: Vec<(usize, BoxIt<'_>)> = self.orig.drain(..).zip(self.children.drain(..)).collect();
        all.sort_by_key(|&(o, _)| o);
        for (o, c) in all {
            self.orig.push(o);
            self.children.push(c);
        }
        self.num_active = self.children.len();
        self.eof = self.children.is_empty();
        self.res.reset_aggregate();
        self.children.iter_mut().for_each(|c| c.rewind());
    }
    fn kind(&self) -> IKind {
        IKind::Union
    }
    fn sort_weight(&self, p: bool) -> f64 {
        if p { self.children.len().max(1) as f64 } else { 1.0 }
    }
    fn set_weight(&mut self, w: f64) {
        self.res.weight = w;
    }
}

/// `DocIdMinHeap`.
#[derive(Default)]
struct MinHeap {
    data: Vec<(DocId, usize)>,
}

impl MinHeap {
    fn push(&mut self, id: DocId, child: usize) {
        self.data.push((id, child));
        let mut i = self.data.len() - 1;
        while i > 0 {
            let p = (i - 1) / 2;
            if self.data[i].0 >= self.data[p].0 {
                break;
            }
            self.data.swap(i, p);
            i = p;
        }
    }

    fn pop(&mut self) {
        let last = self.data.len() - 1;
        if last > 0 {
            self.data.swap(0, last);
            self.data.pop();
            self.sift_down(0);
        } else {
            self.data.pop();
        }
    }

    fn replace_root(&mut self, id: DocId, child: usize) {
        self.data[0] = (id, child);
        self.sift_down(0);
    }

    fn sift_down(&mut self, mut i: usize) {
        let len = self.data.len();
        if len <= 1 {
            return;
        }
        let e = self.data[i];
        loop {
            let l = 2 * i + 1;
            if l >= len {
                break;
            }
            let r = l + 1;
            let s = if r < len && self.data[r].0 < self.data[l].0 { r } else { l };
            if e.0 <= self.data[s].0 {
                break;
            }
            self.data[i] = self.data[s];
            i = s;
        }
        self.data[i] = e;
    }
}

/// `UnionHeap`.
pub struct UnionHeap<'a> {
    children: Vec<BoxIt<'a>>,
    num_estimated: usize,
    eof: bool,
    quick: bool,
    res: IxResult,
    heap: MinHeap,
    q: (&'static str, Option<Vec<u8>>),
}

impl<'a> UnionHeap<'a> {
    pub fn new(children: Vec<BoxIt<'a>>, quick: bool) -> Self {
        let num_estimated = children.iter().map(|c| c.num_estimated()).sum();
        UnionHeap { eof: children.is_empty(), children, num_estimated, quick, res: IxResult::aggregate(K_UNION), heap: MinHeap::default(), q: ("UNION", None) }
    }

    fn add_child(&mut self, i: usize) {
        if let Some(r) = self.children[i].current() {
            let r = r.clone();
            self.res.push(&r);
        }
    }

    fn quick_set(&mut self, i: usize) {
        self.res.reset_aggregate();
        self.res.doc_id = self.children[i].last_doc_id();
        self.add_child(i);
    }

    fn build(&mut self, min: DocId) {
        self.res.reset_aggregate();
        self.res.doc_id = min;
        if self.heap.data.is_empty() {
            return;
        }
        let mut stack = vec![0usize];
        while let Some(hi) = stack.pop() {
            if hi >= self.heap.data.len() {
                continue;
            }
            let (id, ci) = self.heap.data[hi];
            if id != min {
                continue;
            }
            self.add_child(ci);
            let (l, r) = (2 * hi + 1, 2 * hi + 2);
            if l < self.heap.data.len() && stack.len() < 64 {
                stack.push(l);
            }
            if r < self.heap.data.len() && stack.len() < 64 {
                stack.push(r);
            }
        }
    }

    fn advance_matching(&mut self, cur: DocId) {
        while let Some(&(id, ci)) = self.heap.data.first() {
            if id != cur {
                break;
            }
            if self.children[ci].read() {
                let d = self.children[ci].last_doc_id();
                self.heap.replace_root(d, ci);
            } else {
                self.heap.pop();
            }
        }
    }

    fn init(&mut self) {
        for i in 0..self.children.len() {
            let c = &mut self.children[i];
            if c.last_doc_id() == 0 && !c.at_eof() {
                if c.read() {
                    let d = c.last_doc_id();
                    self.heap.push(d, i);
                }
            } else if c.last_doc_id() > 0 {
                let d = c.last_doc_id();
                self.heap.push(d, i);
            }
        }
    }

    fn advance_lagging(&mut self, id: DocId) -> Option<usize> {
        while let Some(&(root, ci)) = self.heap.data.first() {
            if root >= id {
                break;
            }
            match self.children[ci].skip_to(id) {
                Skip::Found => {
                    let d = self.children[ci].last_doc_id_of_current();
                    self.heap.replace_root(d, ci);
                    if self.quick {
                        return Some(ci);
                    }
                }
                Skip::NotFound => {
                    let d = self.children[ci].last_doc_id_of_current();
                    self.heap.replace_root(d, ci);
                }
                Skip::Eof => self.heap.pop(),
            }
        }
        None
    }

    fn advance_to(&mut self, id: DocId) -> Option<usize> {
        if self.heap.data.is_empty() && self.res.doc_id == 0 {
            for i in 0..self.children.len() {
                if self.children[i].at_eof() {
                    continue;
                }
                match self.children[i].skip_to(id) {
                    Skip::Found | Skip::NotFound => {
                        let d = self.children[i].last_doc_id_of_current();
                        self.heap.push(d, i);
                    }
                    Skip::Eof => {}
                }
            }
            None
        } else {
            self.advance_lagging(id)
        }
    }
}

impl Iter for UnionHeap<'_> {
    fn set_query(&mut self, ty: &'static str, s: Option<Vec<u8>>) {
        self.q = (ty, s);
    }
    fn profile_children(&mut self) {
        self.children.iter_mut().for_each(profile_child);
    }
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        print_union(&self.q, self.children.iter(), pc, c)
    }
    fn read(&mut self) -> bool {
        if self.eof {
            return false;
        }
        if self.quick {
            let next = self.res.doc_id.saturating_add(1);
            return self.skip_to(next) != Skip::Eof;
        }
        if self.res.doc_id == 0 {
            self.init();
        } else {
            let last = self.res.doc_id;
            self.advance_matching(last);
        }
        let Some(&(min, _)) = self.heap.data.first() else {
            self.eof = true;
            return false;
        };
        self.build(min);
        true
    }
    fn skip_to(&mut self, id: DocId) -> Skip {
        if self.eof {
            return Skip::Eof;
        }
        if let Some(i) = self.advance_to(id)
            && self.quick
        {
            self.quick_set(i);
            return Skip::Found;
        }
        let Some(&(min, ci)) = self.heap.data.first() else {
            self.eof = true;
            return Skip::Eof;
        };
        if self.quick {
            self.quick_set(ci);
        } else {
            self.build(min);
        }
        if min == id { Skip::Found } else { Skip::NotFound }
    }
    fn current(&mut self) -> Option<&mut IxResult> {
        (!self.eof).then_some(&mut self.res)
    }
    fn last_doc_id(&self) -> DocId {
        self.res.doc_id
    }
    fn at_eof(&self) -> bool {
        self.eof
    }
    fn num_estimated(&self) -> usize {
        self.num_estimated
    }
    fn rewind(&mut self) {
        self.eof = self.children.is_empty();
        self.res.reset_aggregate();
        self.children.iter_mut().for_each(|c| c.rewind());
        self.heap.data.clear();
    }
    fn kind(&self) -> IKind {
        IKind::Union
    }
    fn sort_weight(&self, p: bool) -> f64 {
        if p { self.children.len().max(1) as f64 } else { 1.0 }
    }
    fn set_weight(&mut self, w: f64) {
        self.res.weight = w;
    }
}

/// `new_union_iterator` + `set_result_weight`: empties dropped, a quick-exit union with a
/// wildcard is that wildcard, one child stands alone, and the heap past `min_heap` children.
pub fn union<'a>(children: Vec<BoxIt<'a>>, quick: bool, min_heap: usize, weight: f64) -> BoxIt<'a> {
    union_typed(children, quick, min_heap, weight, ("UNION", None))
}

/// [`union`] with the query node type and string its profile shows.
pub fn union_typed<'a>(children: Vec<BoxIt<'a>>, quick: bool, min_heap: usize, weight: f64, q: (&'static str, Option<Vec<u8>>)) -> BoxIt<'a> {
    let mut children: Vec<BoxIt<'a>> = children.into_iter().filter(|c| c.kind() != IKind::Empty).collect();
    if quick && let Some(pos) = children.iter().position(|c| c.kind() == IKind::Wildcard) {
        return children.swap_remove(pos);
    }
    match children.len() {
        0 => Box::new(Empty),
        1 => children.pop().unwrap(),
        n => {
            let mut u: BoxIt<'a> = if n > min_heap { Box::new(UnionHeap::new(children, quick)) } else { Box::new(UnionFlat::new(children, quick)) };
            u.set_weight(weight);
            u.set_query(q.0, q.1);
            u
        }
    }
}

/// `Not`.
pub struct Not<'a> {
    child: BoxIt<'a>,
    max: DocId,
    res: IxResult,
}

impl Iter for Not<'_> {
    fn profile_children(&mut self) {
        profile_child(&mut self.child);
    }
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        print_single("NOT", &self.child, pc, c)
    }
    fn read(&mut self) -> bool {
        while !self.at_eof() {
            self.res.doc_id += 1;
            let child_eof = self.res.doc_id > self.child.last_doc_id() && !self.child.read();
            if child_eof || self.res.doc_id != self.child.last_doc_id() {
                return true;
            }
        }
        false
    }
    fn skip_to(&mut self, id: DocId) -> Skip {
        if self.at_eof() {
            return Skip::Eof;
        }
        if id > self.max {
            self.res.doc_id = self.max;
            return Skip::Eof;
        }
        if self.child.last_doc_id() > id || (self.child.at_eof() && id > self.child.last_doc_id()) {
            self.res.doc_id = id;
            return Skip::Found;
        }
        if self.child.last_doc_id() < id && self.child.skip_to(id) != Skip::Found {
            self.res.doc_id = id;
            return Skip::Found;
        }
        self.res.doc_id = id;
        if self.read() { Skip::NotFound } else { Skip::Eof }
    }
    fn current(&mut self) -> Option<&mut IxResult> {
        Some(&mut self.res)
    }
    fn last_doc_id(&self) -> DocId {
        self.res.doc_id
    }
    fn at_eof(&self) -> bool {
        self.res.doc_id >= self.max
    }
    fn num_estimated(&self) -> usize {
        self.max as usize
    }
    fn rewind(&mut self) {
        self.res.doc_id = 0;
        self.child.rewind();
    }
}

/// `NotOptimized`: the complement within the existing docs.
pub struct NotOptimized<'a> {
    wcii: BoxIt<'a>,
    child: BoxIt<'a>,
    max: DocId,
    forced_eof: bool,
    res: IxResult,
}

impl NotOptimized<'_> {
    fn advance_wcii(&mut self) -> bool {
        if !self.wcii.read() {
            self.forced_eof = true;
            return false;
        }
        true
    }

    fn ahead_or_depleted(&self, id: DocId) -> bool {
        id < self.child.last_doc_id() || (self.child.at_eof() && id > self.child.last_doc_id())
    }

    fn read_inner(&mut self) -> bool {
        if self.at_eof() {
            self.forced_eof = true;
            return false;
        }
        if !self.advance_wcii() {
            return false;
        }
        loop {
            let w = self.wcii.last_doc_id();
            if self.ahead_or_depleted(w) {
                self.res.doc_id = w;
                return true;
            } else if w == self.child.last_doc_id() {
                self.child.read();
                if !self.advance_wcii() {
                    return false;
                }
            } else {
                while !self.child.at_eof() && self.child.last_doc_id() < w {
                    self.child.read();
                }
            }
        }
    }
}

impl Iter for NotOptimized<'_> {
    fn profile_children(&mut self) {
        profile_child(&mut self.child);
    }
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        print_single("NOT", &self.child, pc, c)
    }
    fn read(&mut self) -> bool {
        self.read_inner()
    }
    fn skip_to(&mut self, id: DocId) -> Skip {
        if self.at_eof() {
            return Skip::Eof;
        }
        if id > self.max {
            self.forced_eof = true;
            return Skip::Eof;
        }
        if self.wcii.skip_to(id) == Skip::Eof {
            self.forced_eof = true;
            return Skip::Eof;
        }
        let w = self.wcii.last_doc_id();
        if !self.child.at_eof() && self.child.last_doc_id() < w {
            self.child.skip_to(w);
        }
        if self.child.last_doc_id() == w {
            return if self.read_inner() { Skip::NotFound } else { Skip::Eof };
        }
        self.res.doc_id = w;
        if w == id { Skip::Found } else { Skip::NotFound }
    }
    fn current(&mut self) -> Option<&mut IxResult> {
        Some(&mut self.res)
    }
    fn last_doc_id(&self) -> DocId {
        self.res.doc_id
    }
    fn at_eof(&self) -> bool {
        self.forced_eof || self.res.doc_id >= self.max
    }
    fn num_estimated(&self) -> usize {
        self.wcii.num_estimated()
    }
    fn rewind(&mut self) {
        self.forced_eof = false;
        self.res.doc_id = 0;
        self.wcii.rewind();
        self.child.rewind();
    }
}

/// `Optional`.
pub struct Optional<'a> {
    child: BoxIt<'a>,
    max: DocId,
    weight: f64,
    res: IxResult,
}

impl Iter for Optional<'_> {
    fn profile_children(&mut self) {
        profile_child(&mut self.child);
    }
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        print_single("OPTIONAL", &self.child, pc, c)
    }
    fn read(&mut self) -> bool {
        if self.at_eof() {
            return false;
        }
        let next = self.res.doc_id + 1;
        let cl = self.child.last_doc_id();
        let real = match cl.cmp(&next) {
            std::cmp::Ordering::Less => self.child.read(),
            std::cmp::Ordering::Equal => self.child.current().is_some(),
            std::cmp::Ordering::Greater => false,
        };
        self.res.doc_id = next;
        if real {
            let w = self.weight;
            if let Some(r) = self.child.current()
                && r.doc_id == next
            {
                r.weight = w;
            }
        }
        true
    }
    fn skip_to(&mut self, id: DocId) -> Skip {
        if id > self.max || self.at_eof() {
            self.res.doc_id = self.max;
            return Skip::Eof;
        }
        if id > self.child.last_doc_id() {
            self.child.skip_to(id);
        }
        let w = self.weight;
        if let Some(r) = self.child.current()
            && r.doc_id == id
        {
            r.weight = w;
        }
        self.res.doc_id = id;
        Skip::Found
    }
    fn current(&mut self) -> Option<&mut IxResult> {
        let id = self.res.doc_id;
        if self.child.last_doc_id() == id && self.child.current().is_some() {
            return self.child.current();
        }
        Some(&mut self.res)
    }
    fn last_doc_id(&self) -> DocId {
        self.res.doc_id
    }
    fn at_eof(&self) -> bool {
        self.res.doc_id >= self.max
    }
    fn num_estimated(&self) -> usize {
        self.max as usize
    }
    fn rewind(&mut self) {
        self.res.doc_id = 0;
        self.child.rewind();
    }
}

/// `OptionalOptimized`.
pub struct OptionalOptimized<'a> {
    wcii: BoxIt<'a>,
    child: BoxIt<'a>,
    virt: IxResult,
    max: DocId,
    weight: f64,
    last: DocId,
    eof: bool,
}

impl OptionalOptimized<'_> {
    fn land(&mut self, id: DocId) {
        if id > self.child.last_doc_id() {
            self.child.skip_to(id);
        }
        self.last = id;
        self.eof = id >= self.max;
        if self.child.last_doc_id() == id {
            let w = self.weight;
            if let Some(r) = self.child.current() {
                r.weight = w;
            }
        } else {
            self.virt.doc_id = id;
        }
    }
}

impl Iter for OptionalOptimized<'_> {
    fn profile_children(&mut self) {
        profile_child(&mut self.child);
    }
    fn print(&self, pc: &PCtx, c: Option<&Counters>) -> Pairs {
        print_single("OPTIONAL", &self.child, pc, c)
    }
    fn read(&mut self) -> bool {
        if self.eof {
            return false;
        }
        if !self.wcii.read() {
            self.eof = true;
            return false;
        }
        let id = self.wcii.last_doc_id_of_current();
        if id > self.max {
            self.eof = true;
            return false;
        }
        self.land(id);
        true
    }
    fn skip_to(&mut self, id: DocId) -> Skip {
        if id > self.max || self.eof {
            self.eof = true;
            return Skip::Eof;
        }
        let found = match self.wcii.skip_to(id) {
            Skip::Eof => {
                self.eof = true;
                return Skip::Eof;
            }
            s => s == Skip::Found,
        };
        let eff = self.wcii.last_doc_id_of_current();
        if eff > self.max {
            self.eof = true;
            return Skip::Eof;
        }
        self.land(eff);
        if found { Skip::Found } else { Skip::NotFound }
    }
    fn current(&mut self) -> Option<&mut IxResult> {
        if self.last != 0 && self.child.last_doc_id() == self.last && self.child.current().is_some() {
            return self.child.current();
        }
        Some(&mut self.virt)
    }
    fn last_doc_id(&self) -> DocId {
        self.last
    }
    fn at_eof(&self) -> bool {
        self.eof
    }
    fn num_estimated(&self) -> usize {
        self.wcii.num_estimated()
    }
    fn rewind(&mut self) {
        self.last = 0;
        self.eof = false;
        self.virt.doc_id = 0;
        self.wcii.rewind();
        self.child.rewind();
    }
}

/// The wildcard a query gets (`new_wildcard_iterator`): the existing docs with INDEXALL,
/// else every id.
pub fn wildcard<'a>(existing: Option<&'a InvertedIndex>, index_all: bool, max: DocId, weight: f64, cx: &'a Qcx<'a>) -> BoxIt<'a> {
    if index_all {
        return match existing {
            Some(ii) => Box::new(InvIt::wildcard(ii, weight, cx)),
            None => Box::new(Empty),
        };
    }
    Box::new(Wildcard::new(max, weight))
}

/// `new_not_iterator`.
pub fn not<'a>(child: BoxIt<'a>, existing: Option<&'a InvertedIndex>, index_all: bool, max: DocId, weight: f64, cx: &'a Qcx<'a>) -> BoxIt<'a> {
    match child.kind() {
        IKind::Empty => {
            let mut wc = wildcard(existing, index_all, max, weight, cx);
            if let Some(r) = wc.current() {
                r.freq = 0;
            }
            return wc;
        }
        IKind::Wildcard => return Box::new(Empty),
        _ => {}
    }
    let res = IxResult { weight, field_mask: MASK_ALL, ..IxResult::virt() };
    if index_all {
        let wcii = wildcard(existing, true, max, weight, cx);
        Box::new(NotOptimized { wcii, child, max, forced_eof: false, res })
    } else {
        Box::new(Not { child, max, res })
    }
}

/// `new_optional_iterator`.
pub fn optional<'a>(mut child: BoxIt<'a>, existing: Option<&'a InvertedIndex>, index_all: bool, max: DocId, weight: f64, cx: &'a Qcx<'a>) -> BoxIt<'a> {
    match child.kind() {
        IKind::Empty => return wildcard(existing, index_all, max, 0.0, cx),
        IKind::Wildcard => {
            if let Some(r) = child.current() {
                r.weight = weight;
            }
            return child;
        }
        _ => {}
    }
    let virt = IxResult { freq: 1, field_mask: MASK_ALL, ..IxResult::virt() };
    if index_all {
        let wcii = wildcard(existing, true, max, weight, cx);
        Box::new(OptionalOptimized { wcii, child, virt, max, weight, last: 0, eof: false })
    } else {
        Box::new(Optional { child, max, weight, res: virt })
    }
}

/// `calculate_idf`.
pub fn idf(total: u64, term: u64) -> f64 {
    let term = term.max(1) as f64;
    let v = 1.0 + (total + 1) as f64 / term;
    // `ilogb` (v >= 1, always normal)
    (((v.to_bits() >> 52) & 0x7ff) as i64 - 1023) as f64
}

/// `calculate_idf_bm25`.
pub fn bm25_idf(total: u64, term: u64) -> f64 {
    let total = total.max(term) as f64;
    let term = term as f64;
    (1.0 + (total - term + 0.5) / (term + 0.5)).ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[u64]) -> BoxIt<'static> {
        Box::new(IdList::new(v.to_vec(), IxResult { weight: 1.0, ..IxResult::virt() }))
    }

    fn drain(mut it: BoxIt<'_>) -> Vec<u64> {
        let mut out = Vec::new();
        while it.read() {
            out.push(it.current().unwrap().doc_id);
        }
        out
    }

    #[test]
    fn combinators() {
        assert_eq!(drain(intersect(vec![ids(&[1, 3, 5, 7]), ids(&[3, 4, 7, 9])], 1.0, false, None, false)), [3, 7]);
        for min_heap in [20, 0] {
            for quick in [false, true] {
                assert_eq!(drain(union(vec![ids(&[1, 5]), ids(&[2, 5, 8]), ids(&[9])], quick, min_heap, 1.0)), [1, 2, 5, 8, 9]);
            }
        }
        let cx = Qcx { docs: Box::leak(Box::new(DocTable::new(0))), now_ms: 0, ft_to_field: vec![] };
        let cx: &'static Qcx<'static> = Box::leak(Box::new(cx));
        assert_eq!(drain(not(ids(&[2, 3]), None, false, 5, 1.0, cx)), [1, 4, 5]);
        assert_eq!(drain(optional(ids(&[2]), None, false, 3, 1.0, cx)), [1, 2, 3]);
        let mut u = union(vec![ids(&[1, 5]), ids(&[5])], false, 20, 1.0);
        assert_eq!(u.skip_to(4), Skip::NotFound);
        assert_eq!(u.current().unwrap().children.len(), 2);
    }

    #[test]
    fn profiles() {
        let u = union_typed(vec![ids(&[1, 5]), ids(&[2, 5, 8])], false, 20, 1.0, ("TAG", None));
        let it = profiled(intersect(vec![u, ids(&[5, 8])], 1.0, false, None, false));
        let mut it = it;
        while it.read() {}
        let pc = PCtx { limited: false, clock: false };
        let m = it.print(&pc, None);
        let get = |m: &Pairs, k: &str| m.iter().find(|(a, _)| *a == Reply::Status(k.into())).map(|(_, v)| v.clone());
        assert_eq!(get(&m, "Type"), Some(Reply::Status("INTERSECT".into())));
        assert_eq!(get(&m, "Number of reading operations"), Some(Reply::Int(2)));
        let Some(Reply::Array(kids)) = get(&m, "Child iterators") else { panic!("children") };
        let Reply::Map(first) = &kids[0] else { panic!("a map") };
        assert_eq!(get(first, "Type"), Some(Reply::Status("ID-LIST-SORTED".into())));
        let Reply::Map(union) = &kids[1] else { panic!("a map") };
        assert_eq!(get(union, "Query type"), Some(Reply::Status("TAG".into())));
        let limited = PCtx { limited: true, clock: false };
        let Reply::Map(union) = (match get(&it.print(&limited, None), "Child iterators") {
            Some(Reply::Array(k)) => k[1].clone(),
            _ => panic!("children"),
        }) else {
            panic!("a map")
        };
        assert_eq!(get(&union, "Child iterators"), Some(Reply::Status("The number of iterators in the union is 2".into())));
    }

    #[test]
    fn idfs() {
        assert_eq!(idf(10, 2), 2.0);
        assert!((bm25_idf(10, 2) - (1.0f64 + 8.5 / 2.5).ln()).abs() < 1e-12);
    }
}
