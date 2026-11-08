//! The pages of a BRIN or GIN index of a table too big to model, measured in its build's pass
//! over the rows (`Ddl::measure_rows_with`) the way the access method builds it, rather than
//! by sorting every row's key as a btree's would be (`btree_pages_of`):
//! - BRIN: one summary tuple per page range (brinbuild's callback forms a range's tuple when
//!   the scan leaves it), packed after the metapage and the range map;
//! - GIN: per-key posting lists accumulated in memory and inserted into the entry tree each
//!   time they outgrow maintenance_work_mem, as ginbuild's BuildAccumulator does
//!   (`catalog::ginsim`).
//!
//! Rows' TIDs are where the measure's heap packing put them (`relsize::HeapPacker`).

use crate::catalog::ginsim::{GinIndex, GinKey, tid_code};
use crate::catalog::relsize;
use crate::sql::plan::BoundIndex;
use crate::types::{DataType, Value};
use crate::exec::fasthash::HashMap;

pub(super) enum AmMeasure {
    Brin(BrinMeasure),
    Gin(GinMeasure),
}

impl AmMeasure {
    /// The measure of index `bix` (its key columns' `types`), when its access method and
    /// operator classes have one; `mwm_kb` is maintenance_work_mem.
    pub(super) fn of(bix: &BoundIndex, types: &[DataType], mwm_kb: usize) -> Option<AmMeasure> {
        let opclass = |i: usize| bix.index.columns.get(i).map_or(0, |c| c.opclass);
        match bix.index.method.as_str() {
            "brin" => {
                let kinds = (0..bix.keys.len())
                    .map(|i| summary_kind(&types[i], opclass(i)))
                    .collect::<Option<Vec<_>>>()?;
                Some(AmMeasure::Brin(BrinMeasure::new(bix, types, kinds)))
            }
            "gin" if bix.keys.len() == 1 && crate::exec::sizing::gin_keys_known(&types[0], opclass(0)) => {
                Some(AmMeasure::Gin(GinMeasure::new(types[0].clone(), opclass(0), mwm_kb)))
            }
            _ => None,
        }
    }

    /// A row's index values (`vals`, its key columns first) at heap TID `tid`.
    pub(super) fn row(&mut self, vals: &[Value], tid: (u32, u16)) {
        match self {
            AmMeasure::Brin(b) => b.row(vals, tid.0),
            AmMeasure::Gin(g) => g.row(vals, tid),
        }
    }

    /// The index's pages, for a heap of `heap_pages`.
    pub(super) fn pages(self, heap_pages: u32) -> i32 {
        match self {
            AmMeasure::Brin(b) => b.pages(heap_pages),
            AmMeasure::Gin(g) => g.pages(),
        }
    }
}

/// What a BRIN operator class stores per range for a column.
#[derive(Clone, Copy)]
pub(super) enum Summary {
    /// minmax: the least and greatest values.
    MinMax,
    /// box_inclusion_ops: the union box and two flags (a fixed size).
    BoxInclusion,
}

fn summary_kind(ty: &DataType, opclass: u32) -> Option<Summary> {
    let name = match opclass {
        0 => None,
        o => Some(crate::catalog::system::opclass_name(o)?.0),
    };
    match (name, ty) {
        (Some(n), _) if n.ends_with("_minmax_ops") => Some(Summary::MinMax),
        (Some("box_inclusion_ops"), _) | (None, DataType::GBox) => Some(Summary::BoxInclusion),
        // (The other inclusion classes' unions, bloom's filters and minmax-multi's values
        // aren't sized here.)
        (Some(_), _) | (None, DataType::Inet | DataType::Cidr | DataType::Range(_) | DataType::Multirange(_)) => None,
        (None, _) => Some(Summary::MinMax),
    }
}

/// One column of the range being summarized.
#[derive(Clone, Default)]
struct Column {
    has_nulls: bool,
    has_values: bool,
    min: Option<Value>,
    max: Option<Value>,
}

pub(super) struct BrinMeasure {
    ppr: u32,
    kinds: Vec<Summary>,
    types: Vec<DataType>,
    /// The range the scan is in, and its columns.
    range: Option<u32>,
    cols: Vec<Column>,
    /// Summary tuples formed (with line pointers), in range order, and the ranges they cover.
    tuples: Vec<u16>,
    summarized: u32,
}

/// A BRIN regular page's room for tuples: BLCKSZ less the page header and BrinSpecialSpace.
const BRIN_PAGE_ROOM: usize = 8192 - 24 - 8;
/// REVMAP_PAGE_MAXITEMS: TIDs per range map page.
const REVMAP_ITEMS: u32 = ((8192 - 24 - 8) / 6) as u32;

impl BrinMeasure {
    fn new(bix: &BoundIndex, types: &[DataType], kinds: Vec<Summary>) -> BrinMeasure {
        let n = kinds.len();
        BrinMeasure {
            ppr: crate::exec::sizing::brin_pages_per_range(&bix.index),
            kinds,
            types: types[..n].to_vec(),
            range: None,
            cols: vec![Column::default(); n],
            tuples: Vec::new(),
            summarized: 0,
        }
    }

    fn row(&mut self, vals: &[Value], block: u32) {
        let range = block / self.ppr;
        match self.range {
            Some(r) if r == range => {}
            // (A row the heap's free space map placed in a range already summarized: its
            // values could only widen that summary.)
            Some(r) if range < r => return,
            _ => {
                self.form();
                self.range = Some(range);
            }
        }
        for (((c, v), kind), ty) in self.cols.iter_mut().zip(vals).zip(&self.kinds).zip(&self.types) {
            if v.is_null() {
                c.has_nulls = true;
                continue;
            }
            c.has_values = true;
            // (A fixed-size summary's size doesn't depend on which values: any one stands
            // for the least and the greatest.)
            let fixed = matches!(kind, Summary::BoxInclusion) || relsize::type_layout(ty).0 > 0;
            if fixed {
                if c.min.is_none() {
                    c.min = Some(v.clone());
                    c.max = Some(v.clone());
                }
                continue;
            }
            if c.min.as_ref().is_none_or(|m| v.cmp_total(m).is_lt()) {
                c.min = Some(v.clone());
            }
            if c.max.as_ref().is_none_or(|m| v.cmp_total(m).is_gt()) {
                c.max = Some(v.clone());
            }
        }
    }

    /// brin_form_tuple of the range being summarized, if any.
    fn form(&mut self) {
        if self.range.is_none() {
            return;
        }
        let cols = std::mem::replace(&mut self.cols, vec![Column::default(); self.kinds.len()]);
        let natts = cols.len();
        let any_nulls = cols.iter().any(|c| c.has_nulls || !c.has_values);
        // SizeOfBrinTuple, the null bitmap (two bits a column), MAXALIGNed.
        let mut off = maxalign(5 + if any_nulls { (natts * 2).div_ceil(8) } else { 0 });
        let text = |_: &Value, _: &DataType| String::new();
        for ((c, kind), ty) in cols.iter().zip(&self.kinds).zip(&self.types) {
            if !c.has_values {
                continue;
            }
            match kind {
                Summary::MinMax => {
                    for v in [&c.min, &c.max].into_iter().flatten() {
                        off += relsize::datum_bytes(off, v, ty, &text);
                    }
                }
                Summary::BoxInclusion => {
                    // The union box (double-aligned) and two booleans.
                    off = off.next_multiple_of(8) + 32 + 2;
                }
            }
        }
        self.tuples.push((maxalign(off) + 4) as u16);
        self.summarized += 1;
    }

    fn pages(mut self, heap_pages: u32) -> i32 {
        self.form();
        // brin_fill_empty_ranges: a placeholder tuple (every column all-nulls) for each range
        // no row was in, the first one even of an empty table.
        let ranges = heap_pages.div_ceil(self.ppr).max(1);
        let empty = ranges.saturating_sub(self.summarized);
        let placeholder = (maxalign(5 + (self.kinds.len() * 2).div_ceil(8)) + 4) as u16;
        let mut regular = 0u32;
        let mut free = 0usize;
        for t in self.tuples.iter().copied().chain(std::iter::repeat_n(placeholder, empty as usize)) {
            if free < t as usize {
                regular += 1;
                free = BRIN_PAGE_ROOM;
            }
            free -= t as usize;
        }
        // The metapage, the range map's pages and the regular pages.
        (1 + ranges.div_ceil(REVMAP_ITEMS) + regular) as i32
    }
}

fn maxalign(n: usize) -> usize {
    n.next_multiple_of(8)
}

pub(super) struct GinMeasure {
    ty: DataType,
    opclass: u32,
    /// Posting lists not inserted yet, and the memory BuildAccumulator would count for them.
    acc: HashMap<GinKey, Vec<u64>>,
    used: usize,
    limit: usize,
    index: GinIndex,
}

impl GinMeasure {
    fn new(ty: DataType, opclass: u32, mwm_kb: usize) -> GinMeasure {
        GinMeasure { ty, opclass, acc: HashMap::default(), used: 0, limit: mwm_kb.max(64) * 1024, index: GinIndex::new() }
    }

    fn row(&mut self, vals: &[Value], (block, offset): (u32, u16)) {
        let Some(keys) = vals.first().and_then(|v| crate::exec::sizing::gin_keys(v, &self.ty, self.opclass)) else {
            return;
        };
        let code = tid_code(block, offset);
        for k in keys {
            self.used += accumulate(&mut self.acc, k, code);
        }
        // ginBuildCallback: everything accumulated goes into the index past
        // maintenance_work_mem.
        if self.used >= self.limit {
            self.flush();
        }
    }

    /// ginbuild's dump of the accumulator: keys in order, each with its TIDs sorted.
    fn flush(&mut self) {
        let mut entries: Vec<(GinKey, Vec<u64>)> = self.acc.drain().collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (k, mut list) in entries {
            if !list.is_sorted() {
                list.sort_unstable();
            }
            self.index.insert(k, list);
        }
        self.used = 0;
    }

    fn pages(mut self) -> i32 {
        self.flush();
        self.index.nblocks() as i32
    }
}

/// Adds `code` to key `k`'s list in `acc`: the memory BuildAccumulator counts for it.
fn accumulate(acc: &mut HashMap<GinKey, Vec<u64>>, k: GinKey, code: u64) -> usize {
    match acc.get_mut(&k) {
        Some(list) => {
            // (A list grows by doubling from DEF_NPTR (5) TIDs, 6 bytes each.)
            let grown = if list.len() >= 5 && list.len().is_power_of_two() { list.len() * 6 } else { 0 };
            list.push(code);
            grown
        }
        None => {
            // (A GinEntryAccumulator, a copy of a by-reference key, a first list.)
            let n = 64 + if k.datum > 8 { chunk(k.datum) } else { 0 } + chunk(30);
            acc.insert(k, vec![code]);
            n
        }
    }
}

/// An AllocSet chunk's size for a request of `n` bytes (powers of two, a header).
fn chunk(n: usize) -> usize {
    n.max(8).next_power_of_two() + 16
}

/// What the participants of a parallel BRIN or GIN build measure (`Ddl::measure_am_parallel`),
/// rows known by their chunk and place in it (`row_code`) until every chunk's rows are
/// counted and so their TIDs known:
/// - BRIN with fixed-size summaries only (a summary's size doesn't depend on which values):
///   whether each row's key columns are null;
/// - GIN of one column: its keys' posting lists, each participant's accumulated up to its share
///   of maintenance_work_mem and kept as compressed sorted runs past it (ginBuildCallbackParallel's
///   flushes into its tuplesort), merged by key at the end and each key's list inserted once,
///   as `_gin_parallel_merge` combines the workers' tuples.
pub(super) enum AmPlan {
    Brin { cols: Vec<usize>, kinds: Vec<Summary> },
    Gin { col: usize, ty: DataType, opclass: u32, limit: usize },
}

/// A row by its chunk and its place in it.
pub(super) fn row_code(chunk: usize, n: u32) -> u64 {
    ((chunk as u64) << 32) | n as u64
}

impl AmPlan {
    /// For index `bix` (its key columns' `types`) of plain key columns, `participants` sharing
    /// maintenance_work_mem (`mwm_kb`); None: measured serially.
    pub(super) fn of(bix: &BoundIndex, types: &[DataType], mwm_kb: usize, participants: usize) -> Option<AmPlan> {
        let cols = bix
            .keys
            .iter()
            .map(|k| if let crate::sql::expr::Expr::Col(i, _) = k { Some(*i) } else { None })
            .collect::<Option<Vec<usize>>>()?;
        if bix.predicate.is_some() {
            return None;
        }
        match AmMeasure::of(bix, types, mwm_kb)? {
            AmMeasure::Brin(b) => {
                let fixed = b.kinds.iter().zip(&b.types).all(|(k, ty)| matches!(k, Summary::BoxInclusion) || relsize::type_layout(ty).0 > 0);
                (fixed && cols.len() <= 8).then_some(AmPlan::Brin { cols, kinds: b.kinds })
            }
            AmMeasure::Gin(g) => Some(AmPlan::Gin { col: cols[0], ty: g.ty, opclass: g.opclass, limit: g.limit / participants.max(1) }),
        }
    }

    /// The table columns a participant decodes.
    pub(super) fn columns(&self) -> Vec<usize> {
        match self {
            AmPlan::Brin { cols, .. } => cols.clone(),
            AmPlan::Gin { col, .. } => vec![*col],
        }
    }
}

/// One participant's part of a parallel measure.
#[derive(Default)]
pub(super) struct AmPart {
    /// BRIN: each chunk's rows' null masks (bit i: key column i is null), and a value of each
    /// column (any one: its summaries are fixed-size).
    masks: Vec<(usize, Vec<u8>)>,
    values: Vec<Option<Value>>,
    /// GIN: posting lists of row codes not spilled yet, the memory BuildAccumulator would
    /// count for them, and the runs spilled.
    acc: HashMap<GinKey, Vec<u64>>,
    used: usize,
    runs: Vec<Run>,
}

impl AmPart {
    /// Row `n` of chunk `chunk` (decoded: its key columns at least).
    pub(super) fn row(&mut self, plan: &AmPlan, chunk: usize, n: u32, row: &[Value]) {
        match plan {
            AmPlan::Brin { cols, .. } => {
                if self.masks.last().is_none_or(|m| m.0 != chunk) {
                    self.masks.push((chunk, Vec::new()));
                    self.values.resize(cols.len(), None);
                }
                let mut mask = 0u8;
                for (i, &c) in cols.iter().enumerate() {
                    match &row[c] {
                        Value::Null => mask |= 1 << i,
                        v if self.values[i].is_none() => self.values[i] = Some(v.clone()),
                        _ => {}
                    }
                }
                self.masks.last_mut().expect("a chunk").1.push(mask);
            }
            AmPlan::Gin { col, ty, opclass, limit } => {
                let Some(keys) = crate::exec::sizing::gin_keys(&row[*col], ty, *opclass) else { return };
                let code = row_code(chunk, n);
                for k in keys {
                    self.used += accumulate(&mut self.acc, k, code);
                }
                if self.used >= *limit {
                    self.spill();
                }
            }
        }
    }

    /// The accumulated posting lists as a run of their own.
    pub(super) fn spill(&mut self) {
        if self.acc.is_empty() {
            return;
        }
        let mut entries: Vec<(GinKey, Vec<u64>)> = self.acc.drain().collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut run = Run::default();
        for (k, list) in entries {
            run.push(k, &list);
        }
        self.runs.push(run);
        self.used = 0;
    }
}

/// Posting lists of row codes in key order: each key with its list's length, the lists
/// delta-encoded (LEB128) one after another in `data`.
#[derive(Default)]
struct Run {
    keys: Vec<(GinKey, u32)>,
    data: Vec<u8>,
}

impl Run {
    fn push(&mut self, k: GinKey, list: &[u64]) {
        let mut prev = 0;
        for &c in list {
            // (A participant's rows come in their order: each list ascends.)
            let mut d = c - prev;
            prev = c;
            while d >= 0x80 {
                self.data.push(d as u8 | 0x80);
                d >>= 7;
            }
            self.data.push(d as u8);
        }
        self.keys.push((k, list.len() as u32));
    }
}

/// A run being read: the next key and where its list starts.
struct RunCursor {
    run: Run,
    key: usize,
    at: usize,
}

impl RunCursor {
    fn key(&self) -> Option<&GinKey> {
        self.run.keys.get(self.key).map(|k| &k.0)
    }

    /// The next key's codes, appended to `out`.
    fn take(&mut self, out: &mut Vec<u64>) {
        let n = self.run.keys[self.key].1;
        let mut prev = 0u64;
        for _ in 0..n {
            let (mut d, mut shift) = (0u64, 0);
            loop {
                let b = self.run.data[self.at];
                self.at += 1;
                d |= ((b & 0x7f) as u64) << shift;
                if b < 0x80 {
                    break;
                }
                shift += 7;
            }
            prev += d;
            out.push(prev);
        }
        self.key += 1;
    }
}

/// The pages of the index the participants' `parts` measured, chunk `c`'s rows starting at row
/// `starts[c]` of the table, row `i`'s TID `tid(i)`, the heap `heap_pages` long; the index
/// bound `bix` with key column types `types`.
pub(super) fn merged_pages(
    plan: AmPlan,
    bix: &BoundIndex,
    types: &[DataType],
    parts: Vec<AmPart>,
    starts: &[u64],
    tid: &dyn Fn(u64) -> (u32, u16),
    heap_pages: u32,
) -> i32 {
    let ordinal = |code: u64| starts[(code >> 32) as usize] + (code & 0xffff_ffff);
    match plan {
        AmPlan::Brin { kinds, .. } => {
            let mut b = BrinMeasure::new(bix, types, kinds);
            let n = b.kinds.len();
            let mut values: Vec<Option<Value>> = vec![None; n];
            let mut masks = Vec::new();
            for p in parts {
                for (v, s) in values.iter_mut().zip(p.values) {
                    if v.is_none() {
                        *v = s;
                    }
                }
                masks.extend(p.masks);
            }
            masks.sort_unstable_by_key(|m| m.0);
            // (A range's summary is fixed-size: it takes only which columns were seen null and
            // which with a value, given to the measure as at most two rows of the range: one
            // with a value where one was seen, one with a null where one was.)
            let all = ((1u16 << n) - 1) as u8;
            let row = |pick: &dyn Fn(usize) -> bool| -> Vec<Value> {
                (0..n).map(|i| if pick(i) { values[i].clone().unwrap_or(Value::Null) } else { Value::Null }).collect()
            };
            let flush = |b: &mut BrinMeasure, block: u32, nulls: u8, seen: u8| {
                b.row(&row(&|i| seen >> i & 1 == 1), block);
                if nulls & seen != 0 {
                    b.row(&row(&|i| nulls >> i & 1 == 0), block);
                }
            };
            // (The range being read: its number, its first row's block, and its columns seen
            // null and seen with a value.)
            let mut cur: Option<(u32, u32)> = None;
            let (mut nulls, mut seen) = (0u8, 0u8);
            for (c, rows) in masks {
                for (j, mask) in rows.into_iter().enumerate() {
                    let block = tid(starts[c] + j as u64).0;
                    let range = block / b.ppr;
                    match cur {
                        Some((r, _)) if r == range => {}
                        Some((_, first)) => {
                            flush(&mut b, first, nulls, seen);
                            (nulls, seen) = (0, 0);
                            cur = Some((range, block));
                        }
                        None => cur = Some((range, block)),
                    }
                    nulls |= mask;
                    seen |= !mask & all;
                }
            }
            if let Some((_, first)) = cur {
                flush(&mut b, first, nulls, seen);
            }
            b.pages(heap_pages)
        }
        AmPlan::Gin { .. } => {
            let mut cursors: Vec<RunCursor> = parts
                .into_iter()
                .flat_map(|mut p| {
                    p.spill();
                    p.runs
                })
                .map(|run| RunCursor { run, key: 0, at: 0 })
                .collect();
            let mut index = GinIndex::new();
            let mut codes = Vec::new();
            while let Some(min) = cursors.iter().filter_map(RunCursor::key).min().cloned() {
                codes.clear();
                for c in &mut cursors {
                    if c.key() == Some(&min) {
                        c.take(&mut codes);
                    }
                }
                codes.sort_unstable();
                let mut list: Vec<u64> = codes
                    .iter()
                    .map(|&c| {
                        let (b, o) = tid(ordinal(c));
                        tid_code(b, o)
                    })
                    .collect();
                if !list.is_sorted() {
                    list.sort_unstable();
                }
                index.insert(min, list);
            }
            index.nblocks() as i32
        }
    }
}
