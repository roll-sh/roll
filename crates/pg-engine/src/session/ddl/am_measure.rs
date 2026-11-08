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
enum Summary {
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
            match self.acc.get_mut(&k) {
                Some(list) => {
                    // (A list grows by doubling from DEF_NPTR (5) TIDs, 6 bytes each.)
                    if list.len() >= 5 && list.len().is_power_of_two() {
                        self.used += list.len() * 6;
                    }
                    list.push(code);
                }
                None => {
                    // (A GinEntryAccumulator, a copy of a by-reference key, a first list.)
                    self.used += 64 + if k.datum > 8 { chunk(k.datum) } else { 0 } + chunk(30);
                    self.acc.insert(k, vec![code]);
                }
            }
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

/// An AllocSet chunk's size for a request of `n` bytes (powers of two, a header).
fn chunk(n: usize) -> usize {
    n.max(8).next_power_of_two() + 16
}
