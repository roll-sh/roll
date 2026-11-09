//! Time series, as the RedisTimeSeries module built into Redis 8.10 keeps them (`tsdb.c`): a
//! dict of chunks keyed by their first timestamp, compaction rules feeding downsampled series,
//! labels in a global index. What happens to one series lives here; what reaches other keys
//! (compactions, references between series) is orchestrated by the commands.

pub mod agg;
pub mod chunk;
pub mod fmt;
pub mod index;
pub mod iter;
pub mod rdb;

use crate::client::jemalloc_size;
use crate::types::rax::{Tree, dict_size};
use agg::{Agg, AggType};
use chunk::{Chunk, Cr, Dp, Sample};
use std::collections::BTreeMap;

pub const TYPE_NAME: &str = "TSDB-TYPE";
pub const ENCVER: u32 = 9;
/// `sizeof(Series)`, `sizeof(CompactionRule)`, `sizeof(Label)`.
const SERIES_STRUCT: usize = 160;
const RULE_STRUCT: usize = 80;
const LABEL_STRUCT: usize = 16;
/// `SPLIT_FACTOR`.
const SPLIT_FACTOR: f64 = 1.2;
/// `SERIES_OPT_UNCOMPRESSED`, `SERIES_OPT_COMPRESSED_GORILLA`.
pub const OPT_UNCOMPRESSED: u64 = 1;
pub const OPT_COMPRESSED: u64 = 2;

/// How a module string was made, which decides what `RM_MallocSizeString` reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SKind {
    /// A command argument or a copy of one (embedded up to 44 bytes, else raw).
    Arg,
    /// Exactly sized raw (loaded from RDB).
    Raw,
    /// `RM_CreateStringPrintf`: an sds grown to fit by `sdscatvprintf`.
    Printf,
}

/// A `RedisModuleString` the module keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RStr {
    pub b: Vec<u8>,
    pub kind: SKind,
}

fn sds_hdr(len: usize) -> usize {
    match len {
        0..256 => 3,
        256..65536 => 5,
        _ => 9,
    }
}

impl RStr {
    pub fn arg(b: &[u8]) -> Self {
        RStr { b: b.to_vec(), kind: SKind::Arg }
    }

    pub fn raw(b: &[u8]) -> Self {
        RStr { b: b.to_vec(), kind: SKind::Raw }
    }

    pub fn printf(b: &[u8]) -> Self {
        RStr { b: b.to_vec(), kind: SKind::Printf }
    }

    /// `RM_MallocSizeString`.
    pub fn size(&self) -> usize {
        let len = self.b.len();
        match self.kind {
            SKind::Arg => crate::vset::string_size(len, false),
            SKind::Raw if len == 0 => 16 + jemalloc_size(3 + 1),
            SKind::Raw => crate::vset::string_size(len, true),
            SKind::Printf => {
                // `sdsempty()` has room for 4 bytes; past that `sdsMakeRoomFor` doubles.
                if len <= 4 {
                    16 + jemalloc_size(3 + 1)
                } else {
                    16 + jemalloc_size(sds_hdr(2 * len) + 2 * len + 1)
                }
            }
        }
    }
}

/// `CompactionRule`.
#[derive(Clone, Debug)]
pub struct Rule {
    pub dest: RStr,
    pub bucket: u64,
    pub align: u64,
    pub agg: Agg,
    /// `startCurrentTimeBucket` (`u64::MAX` is the module's -1: no bucket yet).
    pub start: u64,
    pub valid: bool,
}

impl Rule {
    /// `NewRule`.
    pub fn new(dest: RStr, kind: AggType, bucket: u64, align: u64) -> Rule {
        Rule { dest, bucket, align, agg: kind.create(false), start: u64::MAX, valid: false }
    }
}

/// What a new series is made with (`CreateCtx`).
#[derive(Clone, Debug)]
pub struct Create {
    pub retention: u64,
    pub chunk_size: u64,
    /// `None` is a NULL label array.
    pub labels: Option<Vec<(RStr, RStr)>>,
    pub options: u64,
    pub dup: Dp,
    pub ignore_time: i64,
    pub ignore_val: f64,
}

/// `Series`.
#[derive(Clone, Debug)]
pub struct Series {
    /// The chunks dict: key (a first timestamp) → slab slot.
    pub dict: BTreeMap<u64, usize>,
    slab: Vec<Option<Chunk>>,
    free: Vec<usize>,
    /// The dict's rax (`RM_MallocSizeDict`).
    tree: Tree,
    /// `lastChunk`.
    pub last: usize,
    pub retention: u64,
    pub chunk_size: u64,
    pub options: u64,
    pub rules: Vec<Rule>,
    pub last_ts: u64,
    pub last_value: f64,
    pub labels: Vec<(RStr, RStr)>,
    /// Whether the label array is allocated (calloc(0) after an RDB load still is).
    pub labels_alloc: bool,
    pub key: RStr,
    pub src_key: Option<RStr>,
    pub total: u64,
    pub dup: Dp,
    pub ignore_time: i64,
    pub ignore_val: f64,
}

/// The module's global state: the label index (keyed by series name, whatever the database)
/// and the key a `rename_from` event named.
#[derive(Default)]
pub struct Global {
    pub index: index::Index,
    pub rename_from: Option<Vec<u8>>,
}

/// The keyspace events the module's subscription acts on (`NotifyCallback`; `loaded` is handled
/// where keys are loaded).
pub fn hooked(event: &str) -> bool {
    matches!(event, "del" | "type_changed" | "expired" | "evict" | "evicted" | "key_trimmed" | "trimmed" | "restore" | "rename_from" | "rename_to")
}

/// `CalcBucketStart` and `BucketStartNormalize`.
pub use iter::{bucket_start, normalize};

impl Series {
    /// `NewSeries`: one empty chunk under key 0 unless `skip_chunk`.
    pub fn new(key: RStr, c: Create, skip_chunk: bool) -> Series {
        let options = if c.options & OPT_UNCOMPRESSED != 0 { c.options | OPT_UNCOMPRESSED } else { c.options | OPT_COMPRESSED };
        let mut s = Series {
            dict: BTreeMap::new(),
            slab: Vec::new(),
            free: Vec::new(),
            tree: Tree::new(),
            last: usize::MAX,
            retention: c.retention,
            chunk_size: c.chunk_size,
            options,
            rules: Vec::new(),
            last_ts: 0,
            last_value: 0.0,
            labels_alloc: c.labels.is_some(),
            labels: c.labels.unwrap_or_default(),
            key,
            src_key: None,
            total: 0,
            dup: c.dup,
            ignore_time: c.ignore_time,
            ignore_val: c.ignore_val,
        };
        if !skip_chunk {
            let id = s.alloc(s.new_chunk());
            s.dict_set(0, id);
            s.last = id;
        }
        s
    }

    pub fn compressed(&self) -> bool {
        self.options & OPT_UNCOMPRESSED == 0
    }

    pub fn new_chunk(&self) -> Chunk {
        Chunk::new(self.compressed(), self.chunk_size)
    }

    // ---- the chunk dict ----

    pub(crate) fn alloc(&mut self, c: Chunk) -> usize {
        match self.free.pop() {
            Some(id) => {
                self.slab[id] = Some(c);
                id
            }
            None => {
                self.slab.push(Some(c));
                self.slab.len() - 1
            }
        }
    }

    fn release(&mut self, id: usize) {
        if id != self.last && !self.dict.values().any(|&v| v == id) {
            self.slab[id] = None;
            self.free.push(id);
        }
    }

    pub fn chunk(&self, id: usize) -> &Chunk {
        self.slab[id].as_ref().expect("live chunk")
    }

    fn chunk_mut(&mut self, id: usize) -> &mut Chunk {
        self.slab[id].as_mut().expect("live chunk")
    }

    /// The chunk under a dict key.
    pub fn chunk_at(&self, key: u64) -> Option<&Chunk> {
        self.dict.get(&key).map(|&id| self.chunk(id))
    }

    pub fn last_chunk(&self) -> &Chunk {
        self.chunk(self.last)
    }

    /// `RM_DictSetC`: fails when the key is there.
    pub(crate) fn dict_set(&mut self, key: u64, id: usize) -> bool {
        if self.dict.contains_key(&key) {
            return false;
        }
        self.dict.insert(key, id);
        self.tree.insert(&key.to_be_bytes(), true);
        true
    }

    fn dict_del(&mut self, key: u64) -> Option<usize> {
        let id = self.dict.remove(&key)?;
        self.tree.remove(&key.to_be_bytes());
        Some(id)
    }

    /// `update_chunk_in_dict`.
    fn rekey(&mut self, id: usize, orig: u64, new: u64) {
        if self.dict_del(orig).is_none() {
            self.dict_del(0);
        }
        self.dict_set(new, id);
    }

    /// Drop the chunk under key 0 a new series starts with (RDB load replaces it).
    pub(crate) fn drop_default_chunk(&mut self) {
        if let Some(id) = self.dict_del(0) {
            self.last = usize::MAX;
            self.release(id);
        }
    }

    /// The chunks in key order.
    pub fn chunks(&self) -> impl DoubleEndedIterator<Item = (u64, &Chunk)> + '_ {
        self.dict.iter().map(|(&k, &id)| (k, self.chunk(id)))
    }

    // ---- samples ----

    /// `SeriesTrim`: drop the chunks entirely past the retention.
    pub fn trim(&mut self) {
        if self.retention == 0 {
            return;
        }
        let min = self.last_ts.saturating_sub(self.retention);
        while let Some((&k, &id)) = self.dict.iter().next() {
            if self.chunk(id).last_ts() >= min {
                break;
            }
            self.dict_del(k);
            self.total = self.total.wrapping_sub(self.chunk(id).num_samples());
            self.release(id);
        }
    }

    /// `SeriesAddSample`: append past the last sample.
    pub fn add_sample(&mut self, ts: u64, v: f64) {
        let s = Sample { ts, v };
        let last = self.last;
        if self.chunk_mut(last).add(s) == Cr::End {
            self.trim();
            let mut c = self.new_chunk();
            c.add(s);
            let id = self.alloc(c);
            self.dict_set(ts, id);
            let old = self.last;
            self.last = id;
            self.release(old);
        }
        self.last_ts = ts;
        self.last_value = v;
        self.total += 1;
    }

    /// `SeriesUpsertSample` without its compaction step: `Err` when the policy refuses.
    pub fn upsert_raw(&mut self, ts: u64, v: f64, dp: Dp) -> Result<(), chunk::Refused> {
        let mut latest = true;
        let mut id = self.last;
        let mut first = self.chunk(id).first_ts();
        if ts < first && self.dict.len() > 1 {
            latest = false;
            id = match self.dict.range(..=ts).next_back() {
                Some((_, &i)) => i,
                None => *self.dict.values().next().ok_or(chunk::Refused)?,
            };
            first = self.chunk(id).first_ts();
        }
        if self.chunk(id).size(false) as f64 > self.chunk_size as f64 * SPLIT_FACTOR {
            let new = self.chunk_mut(id).split();
            let new_first = new.first_ts();
            let nid = self.alloc(new);
            self.dict_set(new_first, nid);
            if ts >= new_first {
                id = nid;
                first = new_first;
            }
            if latest {
                self.last = nid;
            }
        }
        let mut smp = Sample { ts, v };
        let n = self.chunk_mut(id).upsert(&mut smp, dp)?;
        self.total = self.total.wrapping_add(n as i64 as u64);
        if ts == self.last_ts {
            self.last_value = smp.v;
        }
        let after = self.chunk(id).first_ts();
        if after != first {
            self.rekey(id, first, after);
        }
        Ok(())
    }

    /// `SeriesDelRange` without its compaction step: the samples deleted and the last
    /// timestamp before.
    pub fn del_range_raw(&mut self, start: u64, end: u64) -> (u64, u64) {
        use std::ops::Bound::{Excluded, Unbounded};
        let mut deleted = 0u64;
        let mut cursor = self.dict.keys().next().copied();
        while let Some(k) = cursor {
            let id = self.dict[&k];
            let c = self.chunk(id);
            let next = |s: &Series, after: u64| s.dict.range((Excluded(after), Unbounded)).next().map(|(k, _)| *k);
            if c.num_samples() == 0 || c.first_ts() > end {
                break;
            }
            if c.last_ts() < start {
                cursor = next(self, k);
                continue;
            }
            let only = c.num_samples().wrapping_add(deleted) == self.total;
            let whole = c.first_ts() >= start && c.last_ts() <= end && !only;
            if !whole {
                let first = c.first_ts();
                deleted += self.chunk_mut(id).del_range(start, end);
                let after = self.chunk(id).first_ts();
                if after != first {
                    self.rekey(id, first, after);
                    cursor = next(self, after);
                } else {
                    cursor = next(self, k);
                }
                continue;
            }
            let was_last = id == self.last;
            self.dict_del(k);
            deleted += self.chunk(id).num_samples();
            if was_last {
                self.last = self.dict.values().next_back().copied().unwrap_or(usize::MAX);
            }
            self.release(id);
            cursor = next(self, k);
        }
        self.total = self.total.wrapping_sub(deleted);
        let before = self.last_ts;
        if end >= self.last_ts && start <= self.last_ts {
            let last = self.dict.values().next_back().map(|&id| self.chunk(id)).filter(|c| c.num_samples() > 0).map(|c| (c.last_ts(), c.last_value()));
            (self.last_ts, self.last_value) = last.unwrap_or((0, 0.0));
        }
        (deleted, before)
    }

    /// `SeriesCalcRange`: aggregate [start, end] (every sample, NaNs included) into a fresh
    /// context; whether it saw none.
    pub fn calc_range(&self, start: u64, end: u64, kind: AggType) -> (Agg, bool) {
        let mut agg = kind.create(false);
        if kind.twa() {
            agg.bucket_params(start, end.wrapping_add(1));
            if start > 0
                && let Some(s) = iter::Samples::new(self, &iter::Range::span(0, start - 1), true, true, None).next()
            {
                agg.prev_bucket_last(s.v, s.ts);
            }
        }
        let mut empty = true;
        for s in iter::Samples::new(self, &iter::Range::span(start, end), false, true, None) {
            agg.append(s.v, s.ts);
            empty = false;
        }
        if kind.twa()
            && let Some(s) = iter::Samples::new(self, &iter::Range::span(end.wrapping_add(1), u64::MAX), false, true, None).next()
        {
            agg.next_bucket_first(s.v, s.ts);
        }
        (agg, empty)
    }

    /// `getFirstValidTimestamp`: the first timestamp within the retention and how many older
    /// samples are still kept.
    pub fn first_valid(&self) -> (u64, u64) {
        if self.total == 0 {
            return (0, 0);
        }
        let min = if self.retention != 0 && self.retention < self.last_ts { self.last_ts - self.retention } else { 0 };
        let mut count = 0;
        let mut last = Sample::default();
        for s in iter::Samples::new(self, &iter::Range::span(0, self.last_ts), false, false, None) {
            last = s;
            if s.ts >= min {
                break;
            }
            count += 1;
        }
        (last.ts, count)
    }

    // ---- rules ----

    pub fn rule(&self, dest: &[u8]) -> Option<&Rule> {
        self.rules.iter().find(|r| r.dest.b == dest)
    }

    pub fn rule_mut(&mut self, dest: &[u8]) -> Option<&mut Rule> {
        self.rules.iter_mut().find(|r| r.dest.b == dest)
    }

    /// `SeriesDeleteRule`.
    pub fn delete_rule(&mut self, dest: &[u8]) -> bool {
        match self.rules.iter().position(|r| r.dest.b == dest) {
            Some(i) => {
                self.rules.remove(i);
                true
            }
            None => false,
        }
    }

    /// `SeriesDeleteSrcRule`.
    pub fn delete_src(&mut self, src: &[u8]) -> bool {
        if self.src_key.as_ref().is_some_and(|s| s.b == src) {
            self.src_key = None;
            return true;
        }
        false
    }

    // ---- copies and sizes ----

    /// `CopySeries`: no source, no rules; labels copied as they are.
    pub fn copy_as(&self, to: RStr) -> Series {
        let mut s = self.clone();
        s.key = to;
        s.src_key = None;
        s.rules.clear();
        s
    }

    /// `SeriesMemUsage` (with the index's share for this series' name, `index_part`).
    pub fn mem_usage(&self, index_part: usize) -> usize {
        let mut n = jemalloc_size(SERIES_STRUCT) + self.key.size();
        for r in &self.rules {
            n += jemalloc_size(RULE_STRUCT) + r.agg.kind.context_size();
        }
        if self.labels_alloc {
            n += jemalloc_size(self.labels.len() * LABEL_STRUCT);
        }
        for (k, v) in &self.labels {
            n += k.size() + v.size();
        }
        n += dict_size(&self.tree);
        for (_, c) in self.chunks() {
            n += c.size(true) as usize;
        }
        n + index_part
    }

    /// A cheap stand-in for the bytes held, for the server's own accounting.
    pub fn approx_mem(&self) -> usize {
        SERIES_STRUCT + self.dict.len() * (self.chunk_size as usize + 112) + self.rules.len() * 128 + self.labels.len() * 64 + self.key.b.len()
    }

    pub fn chunk_count(&self) -> usize {
        self.dict.len()
    }
}

/// A global compaction policy rule (`SimpleCompactionRule`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicyRule {
    pub agg: AggType,
    pub bucket: u64,
    pub retention: u64,
    pub align: u64,
}

/// `parse_string_to_millisecs`.
fn policy_ms(s: &[u8], zero_ok: bool) -> Option<u64> {
    if !s.first().is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let digits = s.iter().take_while(|c| c.is_ascii_digit()).count();
    let n: u64 = std::str::from_utf8(&s[..digits]).ok()?.parse().unwrap_or(u64::MAX);
    let unit = match &s[digits..] {
        [] if n == 0 => b'\0',
        [u] => *u,
        _ => return None,
    };
    let mult: u64 = match unit {
        b'\0' | b'm' => 1,
        b's' => 1000,
        b'M' => 60_000,
        b'h' => 3_600_000,
        b'd' => 86_400_000,
        _ => return None,
    };
    if n == 0 {
        return zero_ok.then_some(0);
    }
    Some(mult.wrapping_mul(n))
}

/// `ParseCompactionPolicy`: `agg:bucket:retention[:align]` rules separated by `;`.
pub fn parse_policy(s: &[u8]) -> Option<Vec<PolicyRule>> {
    let s = index::c_str(s);
    let mut out = Vec::new();
    for rule in s.split(|&c| c == b';').filter(|r| !r.is_empty()) {
        let toks: Vec<&[u8]> = rule.split(|&c| c == b':').filter(|t| !t.is_empty()).collect();
        if toks.len() != 3 && toks.len() != 4 {
            return None;
        }
        out.push(PolicyRule {
            agg: AggType::parse(toks[0])?,
            bucket: policy_ms(toks[1], false)?,
            retention: policy_ms(toks[2], true)?,
            align: match toks.get(3) {
                Some(t) => policy_ms(t, true)?,
                None => 0,
            },
        });
    }
    Some(out)
}

/// `MillisecondsToTimeString`.
fn ms_string(ms: u64) -> String {
    const D: u64 = 86_400_000;
    if ms.is_multiple_of(D) {
        format!("{}d", ms / D)
    } else if ms.is_multiple_of(3_600_000) {
        format!("{}h", ms / 3_600_000)
    } else if ms.is_multiple_of(60_000) {
        format!("{}M", ms / 60_000)
    } else if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}m")
    }
}

/// `CompactionRulesToString`.
pub fn policy_string(rules: &[PolicyRule]) -> String {
    rules
        .iter()
        .map(|r| {
            let mut s = format!("{}:{}:{}", r.agg.lower(), ms_string(r.bucket), ms_string(r.retention));
            if r.align > 0 {
                s.push(':');
                s.push_str(&ms_string(r.align));
            }
            s
        })
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(compressed: bool) -> Create {
        Create {
            retention: 0,
            chunk_size: 128,
            labels: None,
            options: if compressed { 0 } else { OPT_UNCOMPRESSED },
            dup: Dp::Block,
            ignore_time: 0,
            ignore_val: 0.0,
        }
    }

    #[test]
    fn adds_upserts_and_deletes() {
        for compressed in [true, false] {
            let mut s = Series::new(RStr::arg(b"k"), create(compressed), false);
            for t in 1..=200u64 {
                s.add_sample(t * 10, t as f64);
            }
            assert_eq!(s.total, 200);
            assert!(s.chunk_count() > 1);
            assert!(s.upsert_raw(15, 1.5, Dp::Block).is_ok());
            assert!(s.upsert_raw(15, 2.0, Dp::Block).is_err());
            assert_eq!(s.total, 201);
            let all: Vec<u64> = iter::Samples::new(&s, &iter::Range::span(0, u64::MAX), false, false, None).map(|x| x.ts).collect();
            assert_eq!(all.len(), 201);
            assert!(all.windows(2).all(|w| w[0] < w[1]));
            let (n, before) = s.del_range_raw(0, 1000);
            assert_eq!((n, before), (101, 2000));
            assert_eq!(s.total, 100);
            let (n, _) = s.del_range_raw(0, u64::MAX);
            assert_eq!(n, 100);
            assert_eq!((s.total, s.last_ts), (0, 0));
            assert_eq!(s.chunk_count(), 1);
        }
    }

    #[test]
    fn policies() {
        let p = parse_policy(b"max:1M:1h;avg:10s:0;min:1d:0m:5s").unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0], PolicyRule { agg: AggType::Max, bucket: 60_000, retention: 3_600_000, align: 0 });
        assert_eq!(policy_string(&p), "max:1M:1h;avg:10s:0d;min:1d:0d:5s");
        assert!(parse_policy(b"max:1000:1h").is_none());
        assert!(parse_policy(b"max:0m:1h").is_none());
        assert!(parse_policy(b"nope:1m:1h").is_none());
        assert_eq!(parse_policy(b"").unwrap(), vec![]);
    }
}
