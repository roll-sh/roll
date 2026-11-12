//! Compaction strategies: which SSTables to compact together next.
//!
//! The strategies decide only *what* to compact; the table's executor merges, purges and swaps
//! (see `table.rs`). The choices follow the references closely enough that their visible
//! effects match: tombstones are purged after `gc_grace_seconds` (or the `tombstone_gc` mode) by
//! whatever compaction reaches them, fully expired SSTables are dropped whole, single-SSTable
//! tombstone compactions run past `tombstone_threshold`, and TWCS keeps one SSTable per window.
//!
//! - `SizeTieredCompactionStrategy`: Cassandra's `SizeTieredCompactionStrategy.getBuckets`
//!   (Apache-2.0), most populous bucket first.
//! - `LeveledCompactionStrategy`: L0 → L1, then the level most over its size target into the
//!   next, with `sstable_size_in_mb` outputs (`LeveledManifest`).
//! - `TimeWindowCompactionStrategy`: windows by max timestamp; size-tiered in the newest window,
//!   one SSTable per older window; expired SSTables dropped (`TimeWindowCompactionStrategy`).
//! - `IncrementalCompactionStrategy` (ScyllaDB): size tiers of *runs* (the fragments one
//!   compaction wrote), compacting 2+ runs, outputs in `sstable_size_in_mb` fragments, plus the
//!   `space_amplification_goal`.
//! - `UnifiedCompactionStrategy` (Cassandra 5): levels by size with fanout and threshold from
//!   `scaling_parameters` (`T4` tiered, `L10` leveled, `N`).

use crate::model::PartitionKey;
use crate::sstable::SsTable;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// The table's `compaction` option as declared (class and options, verbatim).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionParams {
    pub class: String,
    pub options: BTreeMap<String, String>,
}

impl Default for CompactionParams {
    fn default() -> Self {
        Self { class: "SizeTieredCompactionStrategy".into(), options: BTreeMap::new() }
    }
}

impl CompactionParams {
    pub fn new(class: &str, options: &[(&str, &str)]) -> Self {
        Self { class: class.into(), options: options.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }

    fn get<T: std::str::FromStr>(&self, k: &str, default: T) -> T {
        self.options.get(k).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
    }

    fn get_bool(&self, k: &str, default: bool) -> bool {
        self.options.get(k).map_or(default, |v| v.trim().eq_ignore_ascii_case("true"))
    }

    /// The strategy, with its options parsed (defaults as in the references).
    pub fn strategy(&self) -> Strategy {
        let class = self.class.rsplit('.').next().unwrap_or(&self.class);
        let common = Common {
            // ScyllaDB's `NullCompactionStrategy` never compacts in the background.
            enabled: class != "NullCompactionStrategy" && self.get_bool("enabled", true),
            min_threshold: self.get("min_threshold", 4usize).max(2),
            max_threshold: self.get("max_threshold", 32usize).max(2),
            tombstone_threshold: self.get("tombstone_threshold", 0.2f64),
            tombstone_compaction_interval_s: self.get("tombstone_compaction_interval", 86_400i64),
            unchecked_tombstone_compaction: self.get_bool("unchecked_tombstone_compaction", false),
        };
        let mb = |k: &str, d: u64| self.get(k, d).saturating_mul(1 << 20);
        match class {
            "LeveledCompactionStrategy" => {
                Strategy::Leveled(Lcs { common, sstable_size: mb("sstable_size_in_mb", 160).max(1), fanout: self.get("fanout_size", 10u64).max(2) })
            }
            "TimeWindowCompactionStrategy" => {
                let unit = match self.options.get("compaction_window_unit").map(|s| s.to_ascii_uppercase()).as_deref() {
                    Some("MINUTES") => 60,
                    Some("HOURS") => 3600,
                    _ => 86_400,
                };
                let resolution = match self.options.get("timestamp_resolution").map(|s| s.to_ascii_uppercase()).as_deref() {
                    Some("MILLISECONDS") => 1_000,
                    Some("SECONDS") => 1,
                    Some("NANOSECONDS") => 1_000_000_000,
                    _ => 1_000_000,
                };
                Strategy::TimeWindow(Twcs {
                    common,
                    window_s: unit * self.get("compaction_window_size", 1i64).max(1),
                    per_second: resolution,
                    stcs: Stcs::from(self, common),
                })
            }
            "IncrementalCompactionStrategy" => Strategy::Incremental(Ics {
                common,
                bucket_low: self.get("bucket_low", std::f64::consts::FRAC_1_SQRT_2),
                bucket_high: self.get("bucket_high", std::f64::consts::SQRT_2),
                min_sstable_size: mb("min_sstable_size", 50),
                fragment_size: mb("sstable_size_in_mb", 1000).max(1),
                space_amplification_goal: self.options.get("space_amplification_goal").and_then(|v| v.parse().ok()).filter(|g: &f64| *g > 1.0),
            }),
            "UnifiedCompactionStrategy" => {
                let sp = self.options.get("scaling_parameters").cloned().unwrap_or_else(|| "T4".into());
                let w = parse_scaling(sp.split(',').next().unwrap_or("T4"));
                Strategy::Unified(Ucs {
                    common,
                    w,
                    target_size: parse_size(self.options.get("target_sstable_size").map(String::as_str)).unwrap_or(1 << 30),
                    min_size: parse_size(self.options.get("min_sstable_size").map(String::as_str)).unwrap_or(100 << 20).max(1),
                })
            }
            _ => Strategy::SizeTiered(Stcs::from(self, common)),
        }
    }
}

/// `T4` → 2, `L10` → -8, `N` → 0, or a plain integer (`UnifiedCompactionStrategy` notation).
fn parse_scaling(s: &str) -> i64 {
    let s = s.trim();
    let n = |x: &str| x.parse::<i64>().unwrap_or(4).max(2);
    match s.chars().next().map(|c| c.to_ascii_uppercase()) {
        Some('T') => n(&s[1..]) - 2,
        Some('L') => 2 - n(&s[1..]),
        Some('N') => 0,
        _ => s.parse().unwrap_or(2),
    }
}

/// `1GiB`, `100MiB`, `512KiB`, `1000` (bytes).
fn parse_size(s: Option<&str>) -> Option<u64> {
    let s = s?.trim();
    let (num, mult) = [("GiB", 1u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10), ("B", 1)]
        .iter()
        .find_map(|(suf, m)| s.strip_suffix(suf).map(|n| (n.trim(), *m)))
        .unwrap_or((s, 1));
    num.parse::<u64>().ok().map(|n| n.saturating_mul(mult))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Common {
    pub enabled: bool,
    pub min_threshold: usize,
    pub max_threshold: usize,
    pub tombstone_threshold: f64,
    pub tombstone_compaction_interval_s: i64,
    pub unchecked_tombstone_compaction: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stcs {
    pub common: Common,
    pub bucket_low: f64,
    pub bucket_high: f64,
    pub min_sstable_size: u64,
}

impl Stcs {
    fn from(p: &CompactionParams, common: Common) -> Self {
        Self {
            common,
            bucket_low: p.get("bucket_low", 0.5),
            bucket_high: p.get("bucket_high", 1.5),
            min_sstable_size: p.get("min_sstable_size", 50u64 << 20),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lcs {
    pub common: Common,
    pub sstable_size: u64,
    pub fanout: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Twcs {
    pub common: Common,
    pub window_s: i64,
    /// Timestamp units per second.
    pub per_second: i64,
    pub stcs: Stcs,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ics {
    pub common: Common,
    pub bucket_low: f64,
    pub bucket_high: f64,
    pub min_sstable_size: u64,
    pub fragment_size: u64,
    pub space_amplification_goal: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ucs {
    pub common: Common,
    /// The scaling parameter: > 0 tiered, < 0 leveled.
    pub w: i64,
    pub target_size: u64,
    pub min_size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Strategy {
    SizeTiered(Stcs),
    Leveled(Lcs),
    TimeWindow(Twcs),
    Incremental(Ics),
    Unified(Ucs),
}

/// Why a task runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Minor,
    Major,
    /// A single SSTable rewritten for its droppable tombstones.
    Tombstone,
    /// Fully expired SSTables dropped without reading them.
    Expired,
}

/// SSTables to compact into `level`, in outputs of at most `max_output_bytes`.
#[derive(Clone, Debug)]
pub struct Task {
    pub inputs: Vec<Arc<SsTable>>,
    pub level: u32,
    pub max_output_bytes: u64,
    pub kind: TaskKind,
}

/// What the strategies know about the table at planning time.
pub struct Plan<'a> {
    pub sstables: &'a [Arc<SsTable>],
    /// Unix seconds.
    pub now: i64,
    pub gc_before: i64,
    /// The smallest timestamp in the memtables (`i64::MAX` when empty).
    pub memtable_min_timestamp: i64,
}

fn size(s: &SsTable) -> u64 {
    s.data_size()
}

pub(crate) fn overlaps(a: &SsTable, b: &SsTable) -> bool {
    let (sa, sb) = (a.stats(), b.stats());
    sa.first_key <= sb.last_key && sb.first_key <= sa.last_key
}

pub(crate) fn overlaps_range(s: &SsTable, first: &PartitionKey, last: &PartitionKey) -> bool {
    s.stats().first_key <= *last && *first <= s.stats().last_key
}

/// `CompactionController.getFullyExpiredSSTables`: candidates with nothing live past
/// `gc_before` whose newest write is older than any live data elsewhere that may overlap.
pub fn fully_expired(candidates: &[Arc<SsTable>], others: &[Arc<SsTable>], gc_before: i64, memtable_min_ts: i64) -> Vec<Arc<SsTable>> {
    if gc_before == i64::MIN {
        return Vec::new();
    }
    let mut min_ts = memtable_min_ts;
    for o in others {
        if candidates.iter().any(|c| overlaps(c, o)) && o.stats().max_local_deletion_time >= gc_before {
            min_ts = min_ts.min(o.stats().min_timestamp);
        }
    }
    let mut out = Vec::new();
    for c in candidates {
        if c.stats().max_local_deletion_time < gc_before {
            out.push(c.clone());
        } else {
            min_ts = min_ts.min(c.stats().min_timestamp);
        }
    }
    out.retain(|c| c.stats().max_timestamp < min_ts);
    out
}

/// `SizeTieredCompactionStrategy.getBuckets`.
pub fn buckets<T: Clone>(files: &[(T, u64)], bucket_high: f64, bucket_low: f64, min_size: u64) -> Vec<Vec<T>> {
    let mut sorted: Vec<(T, u64)> = files.to_vec();
    sorted.sort_by_key(|(_, s)| *s);
    // (average size, members) in creation order: deterministic, unlike Cassandra's HashMap.
    let mut buckets: Vec<(u64, Vec<T>)> = Vec::new();
    'outer: for (f, size) in sorted {
        for (avg, members) in buckets.iter_mut() {
            let old = *avg as f64;
            if (size as f64 > old * bucket_low && (size as f64) < old * bucket_high) || (size < min_size && *avg < min_size) {
                let total = members.len() as u64 * *avg;
                *avg = (total + size) / (members.len() as u64 + 1);
                members.push(f);
                continue 'outer;
            }
        }
        buckets.push((size, vec![f]));
    }
    buckets.into_iter().map(|(_, m)| m).collect()
}

/// `AbstractCompactionStrategy.worthDroppingTombstones` (without the per-range key estimate:
/// overlapping SSTables need `unchecked_tombstone_compaction` or a fully expired check).
fn worth_dropping_tombstones(c: &Common, s: &Arc<SsTable>, all: &[Arc<SsTable>], plan: &Plan) -> bool {
    if plan.gc_before == i64::MIN {
        return false;
    }
    if plan.now * 1000 < s.created_ms + c.tombstone_compaction_interval_s * 1000 {
        return false;
    }
    if s.stats().droppable_ratio(plan.gc_before) <= c.tombstone_threshold {
        return false;
    }
    if c.unchecked_tombstone_compaction {
        return true;
    }
    let others: Vec<Arc<SsTable>> = all.iter().filter(|o| o.uid != s.uid && overlaps(o, s)).cloned().collect();
    others.is_empty() || !fully_expired(std::slice::from_ref(s), &others, plan.gc_before, plan.memtable_min_timestamp).is_empty()
}

fn tombstone_task(c: &Common, plan: &Plan, level_of: impl Fn(&SsTable) -> u32) -> Option<Task> {
    let mut by_ratio: Vec<&Arc<SsTable>> = plan.sstables.iter().collect();
    by_ratio.sort_by(|a, b| b.stats().droppable_ratio(plan.gc_before).total_cmp(&a.stats().droppable_ratio(plan.gc_before)));
    by_ratio
        .into_iter()
        .find(|s| worth_dropping_tombstones(c, s, plan.sstables, plan))
        .map(|s| Task { inputs: vec![s.clone()], level: level_of(s), max_output_bytes: u64::MAX, kind: TaskKind::Tombstone })
}

fn expired_task(plan: &Plan) -> Option<Task> {
    let exp = fully_expired(plan.sstables, &[], plan.gc_before, plan.memtable_min_timestamp);
    (!exp.is_empty()).then(|| Task { inputs: exp, level: 0, max_output_bytes: u64::MAX, kind: TaskKind::Expired })
}

impl Strategy {
    pub fn enabled(&self) -> bool {
        self.common().enabled
    }

    pub fn common(&self) -> &Common {
        match self {
            Strategy::SizeTiered(s) => &s.common,
            Strategy::Leveled(s) => &s.common,
            Strategy::TimeWindow(s) => &s.common,
            Strategy::Incremental(s) => &s.common,
            Strategy::Unified(s) => &s.common,
        }
    }

    /// The level flushed SSTables go to.
    pub fn flush_level(&self) -> u32 {
        0
    }

    /// The next background task, if any.
    pub fn next_task(&self, plan: &Plan) -> Option<Task> {
        if plan.sstables.is_empty() {
            return None;
        }
        match self {
            Strategy::SizeTiered(s) => stcs_task(s, plan).or_else(|| tombstone_task(&s.common, plan, |_| 0)),
            Strategy::Leveled(l) => lcs_task(l, plan).or_else(|| tombstone_task(&l.common, plan, |s| s.level)),
            Strategy::TimeWindow(t) => expired_task(plan).or_else(|| twcs_task(t, plan)).or_else(|| tombstone_task(&t.common, plan, |_| 0)),
            Strategy::Incremental(i) => ics_task(i, plan).or_else(|| tombstone_task(&i.common, plan, |_| 0)),
            Strategy::Unified(u) => ucs_task(u, plan).or_else(|| tombstone_task(&u.common, plan, |s| s.level)),
        }
    }

    /// Major compaction: everything into one SSTable (or `sstable_size_in_mb` pieces for LCS,
    /// fragments for ICS, target-size pieces for UCS).
    pub fn major_task(&self, plan: &Plan) -> Option<Task> {
        if plan.sstables.is_empty() {
            return None;
        }
        let (level, max) = match self {
            Strategy::Leveled(l) => (1, l.sstable_size),
            Strategy::Incremental(i) => (0, i.fragment_size),
            Strategy::Unified(u) => (0, u.target_size),
            _ => (0, u64::MAX),
        };
        Some(Task { inputs: plan.sstables.to_vec(), level, max_output_bytes: max, kind: TaskKind::Major })
    }
}

fn trim(mut bucket: Vec<Arc<SsTable>>, max: usize) -> Vec<Arc<SsTable>> {
    // Cassandra trims by read hotness; without per-SSTable read rates, keep the smallest.
    bucket.sort_by_key(|s| size(s));
    bucket.truncate(max);
    bucket
}

fn stcs_bucket(s: &Stcs, sstables: &[Arc<SsTable>], min_threshold: usize) -> Option<Vec<Arc<SsTable>>> {
    let files: Vec<(Arc<SsTable>, u64)> = sstables.iter().map(|t| (t.clone(), size(t))).collect();
    buckets(&files, s.bucket_high, s.bucket_low, s.min_sstable_size)
        .into_iter()
        .filter(|b| b.len() >= min_threshold)
        .map(|b| trim(b, s.common.max_threshold))
        .max_by(|a, b| a.len().cmp(&b.len()).then_with(|| avg(b).cmp(&avg(a))))
}

fn avg(b: &[Arc<SsTable>]) -> u64 {
    b.iter().map(|s| size(s)).sum::<u64>() / b.len().max(1) as u64
}

fn stcs_task(s: &Stcs, plan: &Plan) -> Option<Task> {
    stcs_bucket(s, plan.sstables, s.common.min_threshold).map(|inputs| Task { inputs, level: 0, max_output_bytes: u64::MAX, kind: TaskKind::Minor })
}

fn lcs_task(l: &Lcs, plan: &Plan) -> Option<Task> {
    let mut levels: BTreeMap<u32, Vec<Arc<SsTable>>> = BTreeMap::new();
    for s in plan.sstables {
        levels.entry(s.level).or_default().push(s.clone());
    }
    let l0 = levels.get(&0).cloned().unwrap_or_default();
    // Too far behind in L0: size-tier it first (`LeveledManifest.getSTCSInL0CompactionCandidate`).
    if l0.len() > 32 {
        let stcs = Stcs { common: l.common, bucket_low: 0.5, bucket_high: 1.5, min_sstable_size: 50 << 20 };
        if let Some(inputs) = stcs_bucket(&stcs, &l0, l.common.min_threshold) {
            return Some(Task { inputs, level: 0, max_output_bytes: u64::MAX, kind: TaskKind::Minor });
        }
    }
    // Higher levels over their target, highest score first.
    let mut best: Option<(f64, u32)> = None;
    for (&lvl, ssts) in &levels {
        if lvl == 0 {
            continue;
        }
        let target = l.sstable_size as f64 * (l.fanout as f64).powi(lvl as i32);
        let score = ssts.iter().map(|s| size(s)).sum::<u64>() as f64 / target;
        if score > 1.001 && best.is_none_or(|(b, _)| score > b) {
            best = Some((score, lvl));
        }
    }
    if l0.len() >= l.common.min_threshold {
        let mut inputs = trim(l0, l.common.max_threshold);
        let (first, last) = key_range(&inputs);
        if let Some(l1) = levels.get(&1) {
            inputs.extend(l1.iter().filter(|s| overlaps_range(s, &first, &last)).cloned());
        }
        return Some(Task { inputs, level: 1, max_output_bytes: l.sstable_size, kind: TaskKind::Minor });
    }
    let (_, lvl) = best?;
    // The oldest SSTable of the level, with what it overlaps in the next.
    let pick = levels[&lvl].iter().min_by_key(|s| (s.created_ms, s.uid))?.clone();
    let (first, last) = (pick.stats().first_key.clone(), pick.stats().last_key.clone());
    let mut inputs = vec![pick];
    if let Some(next) = levels.get(&(lvl + 1)) {
        inputs.extend(next.iter().filter(|s| overlaps_range(s, &first, &last)).cloned());
    }
    Some(Task { inputs, level: lvl + 1, max_output_bytes: l.sstable_size, kind: TaskKind::Minor })
}

fn key_range(ssts: &[Arc<SsTable>]) -> (PartitionKey, PartitionKey) {
    let first = ssts.iter().map(|s| s.stats().first_key.clone()).min().unwrap_or_default();
    let last = ssts.iter().map(|s| s.stats().last_key.clone()).max().unwrap_or_default();
    (first, last)
}

impl Twcs {
    /// The window (start, in seconds) an SSTable belongs to, by its newest timestamp.
    pub fn window_of(&self, max_timestamp: i64) -> i64 {
        let secs = max_timestamp.div_euclid(self.per_second.max(1));
        secs.div_euclid(self.window_s) * self.window_s
    }
}

fn twcs_task(t: &Twcs, plan: &Plan) -> Option<Task> {
    let mut windows: BTreeMap<i64, Vec<Arc<SsTable>>> = BTreeMap::new();
    for s in plan.sstables {
        windows.entry(t.window_of(s.stats().max_timestamp)).or_default().push(s.clone());
    }
    let now_window = t.window_of(plan.now.saturating_mul(t.per_second));
    // Newest window first (`newestBucket`).
    for (w, ssts) in windows.iter().rev() {
        if *w >= now_window {
            if let Some(inputs) = stcs_bucket(&t.stcs, ssts, t.common.min_threshold) {
                return Some(Task { inputs, level: 0, max_output_bytes: u64::MAX, kind: TaskKind::Minor });
            }
        } else if ssts.len() >= 2 {
            return Some(Task { inputs: trim(ssts.clone(), t.common.max_threshold), level: 0, max_output_bytes: u64::MAX, kind: TaskKind::Minor });
        }
    }
    None
}

/// Runs: the fragments one compaction wrote share `run` (see [`SsTable::run`]).
fn runs(sstables: &[Arc<SsTable>]) -> Vec<Vec<Arc<SsTable>>> {
    let mut by: HashMap<u64, Vec<Arc<SsTable>>> = HashMap::new();
    for s in sstables {
        by.entry(s.run).or_default().push(s.clone());
    }
    let mut v: Vec<Vec<Arc<SsTable>>> = by.into_values().collect();
    v.sort_by_key(|r| r.iter().map(|s| s.uid).min());
    v
}

fn ics_task(i: &Ics, plan: &Plan) -> Option<Task> {
    let runs = runs(plan.sstables);
    let files: Vec<(usize, u64)> = runs.iter().enumerate().map(|(n, r)| (n, r.iter().map(|s| s.stats().uncompressed_bytes.max(size(s))).sum())).collect();
    let tiers = buckets(&files, i.bucket_high, i.bucket_low, i.min_sstable_size);
    // Without `compaction_enforce_min_threshold`, two runs in a tier are enough.
    let best = tiers.iter().filter(|b| b.len() >= 2).max_by_key(|b| b.len());
    if let Some(b) = best {
        let mut chosen: Vec<usize> = b.clone();
        chosen.sort_by_key(|n| files[*n].1);
        chosen.truncate(i.common.max_threshold);
        let inputs = chosen.into_iter().flat_map(|n| runs[n].clone()).collect();
        return Some(Task { inputs, level: 0, max_output_bytes: i.fragment_size, kind: TaskKind::Minor });
    }
    // Space amplification: the two largest tiers together when the second is big enough.
    if let Some(goal) = i.space_amplification_goal
        && tiers.len() >= 2
    {
        let mut by_size: Vec<(u64, &Vec<usize>)> = tiers.iter().map(|t| (t.iter().map(|n| files[*n].1).sum(), t)).collect();
        by_size.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
        let (largest, second) = (by_size[0].0 as f64, by_size[1].0 as f64);
        if largest > 0.0 && (largest + second) / largest > goal {
            let inputs = by_size[0].1.iter().chain(by_size[1].1.iter()).flat_map(|n| runs[*n].clone()).collect();
            return Some(Task { inputs, level: 0, max_output_bytes: i.fragment_size, kind: TaskKind::Minor });
        }
    }
    None
}

fn ucs_task(u: &Ucs, plan: &Plan) -> Option<Task> {
    let fanout = (2 + u.w.unsigned_abs()) as f64;
    let threshold = if u.w > 0 { fanout as usize } else { 2 };
    let mut levels: BTreeMap<u32, Vec<Arc<SsTable>>> = BTreeMap::new();
    for s in plan.sstables {
        let ratio = (size(s) as f64 / u.min_size as f64).max(1.0);
        let level = (ratio.ln() / fanout.ln()).floor().max(0.0) as u32;
        levels.entry(level).or_default().push(s.clone());
    }
    for (lvl, ssts) in levels {
        if ssts.len() >= threshold {
            let inputs = trim(ssts, u.common.max_threshold.max(threshold));
            return Some(Task { inputs, level: lvl + 1, max_output_bytes: u.target_size, kind: TaskKind::Minor });
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn stcs_buckets_like_cassandra() {
        let files = vec![("a", 100u64), ("b", 110), ("c", 1000), ("d", 105), ("e", 2000), ("f", 1100)];
        let b = buckets(&files, 1.5, 0.5, 50);
        assert_eq!(b, vec![vec!["a", "d", "b"], vec!["c", "f"], vec!["e"]]);
        // Small files share a bucket whatever their sizes.
        let files = vec![("a", 1u64), ("b", 30), ("c", 49)];
        assert_eq!(buckets(&files, 1.5, 0.5, 50).len(), 1);
    }

    #[test]
    fn params_parse_with_defaults() {
        let p = CompactionParams::new("org.apache.cassandra.db.compaction.LeveledCompactionStrategy", &[("sstable_size_in_mb", "1")]);
        assert_eq!(p.strategy(), Strategy::Leveled(Lcs { common: p.strategy().common().to_owned(), sstable_size: 1 << 20, fanout: 10 }));
        let t = CompactionParams::new("TimeWindowCompactionStrategy", &[("compaction_window_unit", "HOURS"), ("compaction_window_size", "2")]);
        let Strategy::TimeWindow(t) = t.strategy() else { panic!() };
        assert_eq!(t.window_s, 7200);
        assert_eq!(t.window_of(7_199 * 1_000_000), 0);
        assert_eq!(t.window_of(7_200 * 1_000_000), 7200);
        assert_eq!(t.window_of(-1), -7200);
        let u = CompactionParams::new("UnifiedCompactionStrategy", &[("scaling_parameters", "L10"), ("target_sstable_size", "256MiB")]);
        let Strategy::Unified(u) = u.strategy() else { panic!() };
        assert_eq!((u.w, u.target_size), (-8, 256 << 20));
        assert_eq!(parse_scaling("T4"), 2);
        assert_eq!(parse_scaling("N"), 0);
        let s = CompactionParams::new("SizeTieredCompactionStrategy", &[("enabled", "false"), ("min_threshold", "6")]).strategy();
        assert!(!s.enabled());
        assert_eq!(s.common().min_threshold, 6);
        let i = CompactionParams::new("IncrementalCompactionStrategy", &[("space_amplification_goal", "1.25")]).strategy();
        let Strategy::Incremental(i) = i else { panic!() };
        assert_eq!((i.fragment_size, i.space_amplification_goal), (1000 << 20, Some(1.25)));
    }
}
