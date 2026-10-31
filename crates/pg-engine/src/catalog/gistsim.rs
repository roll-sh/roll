//! GiST indexes' sizes (access/gist) for the sorted build CREATE INDEX does when the
//! operator class has sort support, as point_ops and range_ops do (gist_indexsortbuild):
//! the tuples sorted (points by z-order, gist_bbox_zorder_cmp; ranges as range_cmp; ties
//! by heap TID), packed into pages, and every four pages of a level split again with the
//! operator class's picksplit (gistSplit with gist_box_picksplit or range_gist_picksplit)
//! when the fourth fills.

use crate::exec::pgsort::{Cmp, pg_qsort};
use std::cmp::Ordering;

/// GiSTPageSize: a page's space for items and their line pointers.
const PAGE_SIZE: usize = 8192 - 24 - 16;
const ITEM_ID: usize = 4;
/// GIST_SORTED_BUILD_PAGE_NUM.
const PAGES_PER_FLUSH: usize = 4;
/// The size of an index tuple holding a box, and of one with a null key.
const BOX_TUPLE: usize = 40;
const NULL_TUPLE: usize = 16;
const LIMIT_RATIO: f64 = 0.3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GBox {
    pub hx: f64,
    pub hy: f64,
    pub lx: f64,
    pub ly: f64,
}

impl GBox {
    /// gist_point_compress.
    pub fn point(x: f64, y: f64) -> GBox {
        GBox { hx: x, hy: y, lx: x, ly: y }
    }
}

// float.h's comparisons: NaN sorts above everything and equals itself.
fn f_eq(a: f64, b: f64) -> bool {
    if a.is_nan() { b.is_nan() } else { !b.is_nan() && a == b }
}
fn f_lt(a: f64, b: f64) -> bool {
    !a.is_nan() && (b.is_nan() || a < b)
}
fn f_le(a: f64, b: f64) -> bool {
    b.is_nan() || (!a.is_nan() && a <= b)
}
fn f_gt(a: f64, b: f64) -> bool {
    !b.is_nan() && (a.is_nan() || a > b)
}
fn f_ge(a: f64, b: f64) -> bool {
    a.is_nan() || (!b.is_nan() && a >= b)
}
fn f_cmp(a: f64, b: f64) -> Ordering {
    if f_lt(a, b) {
        Ordering::Less
    } else if f_gt(a, b) {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}
fn f_max(a: f64, b: f64) -> f64 {
    if f_gt(a, b) { a } else { b }
}
fn f_min(a: f64, b: f64) -> f64 {
    if f_lt(a, b) { a } else { b }
}

fn union(a: &GBox, b: &GBox) -> GBox {
    GBox { hx: f_max(a.hx, b.hx), hy: f_max(a.hy, b.hy), lx: f_min(a.lx, b.lx), ly: f_min(a.ly, b.ly) }
}

fn size_box(b: &GBox) -> f64 {
    if f_le(b.hx, b.lx) || f_le(b.hy, b.ly) {
        return 0.0;
    }
    if b.hx.is_nan() || b.hy.is_nan() {
        return f64::INFINITY;
    }
    (b.hx - b.lx) * (b.hy - b.ly)
}

fn penalty(original: &GBox, new: &GBox) -> f64 {
    size_box(&union(original, new)) - size_box(original)
}

fn adjust(b: &mut GBox, addon: &GBox) {
    if f_lt(b.hx, addon.hx) {
        b.hx = addon.hx;
    }
    if f_gt(b.lx, addon.lx) {
        b.lx = addon.lx;
    }
    if f_lt(b.hy, addon.hy) {
        b.hy = addon.hy;
    }
    if f_gt(b.ly, addon.ly) {
        b.ly = addon.ly;
    }
}

/// point_zorder_internal over the box's low corner (as float4s).
fn zorder(b: &GBox) -> u64 {
    fn bits(f: f32) -> u32 {
        if f.is_nan() {
            return u32::MAX;
        }
        let i = f.to_bits();
        if i & 0x8000_0000 != 0 { i ^ u32::MAX } else { i | 0x8000_0000 }
    }
    fn spread(x: u32) -> u64 {
        let mut n = x as u64;
        n = (n | (n << 16)) & 0x0000_FFFF_0000_FFFF;
        n = (n | (n << 8)) & 0x00FF_00FF_00FF_00FF;
        n = (n | (n << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
        n = (n | (n << 2)) & 0x3333_3333_3333_3333;
        n = (n | (n << 1)) & 0x5555_5555_5555_5555;
        n
    }
    spread(bits(b.lx as f32)) | (spread(bits(b.ly as f32)) << 1)
}

/// A key: a point_ops box or a range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Key {
    Box(GBox),
    Range(RKey),
}

/// A tuple: its key (None for a null), size, the child block an inner tuple links, and
/// the row a leaf tuple points to (NO_ROW when not tracked).
#[derive(Clone, Copy, Debug)]
struct Tup {
    key: Option<Key>,
    size: usize,
    child: u32,
    row: u32,
}

/// A leaf tuple whose row isn't tracked.
pub const NO_ROW: u32 = u32::MAX;

impl Tup {
    fn of(key: Option<Key>) -> Tup {
        let size = match &key {
            None => NULL_TUPLE,
            Some(Key::Box(_)) => BOX_TUPLE,
            Some(Key::Range(r)) => r.tuple_size(),
        };
        Tup { key, size, child: 0, row: NO_ROW }
    }

    fn linking(key: Option<Key>, child: u32) -> Tup {
        Tup { child, ..Tup::of(key) }
    }
}

/// The opclass's `same` (gist_box_same, range_gist_same).
fn same(a: &Key, b: &Key) -> bool {
    match (a, b) {
        (Key::Box(x), Key::Box(y)) => {
            f_eq(x.lx, y.lx) && f_eq(x.ly, y.ly) && f_eq(x.hx, y.hx) && f_eq(x.hy, y.hy)
        }
        (Key::Range(x), Key::Range(y)) => {
            x.empty == y.empty
                && x.contain_empty == y.contain_empty
                && (x.empty || range_cmp(x, y) == Ordering::Equal)
                && x.lower_inc == y.lower_inc
                && x.upper_inc == y.upper_inc
        }
        _ => false,
    }
}

/// gistgetadjusted: the downlink key covering `add` too, when it changes.
fn adjusted(old: Option<Key>, add: Option<Key>) -> Option<Option<Key>> {
    let add = add?;
    let Some(old) = old else {
        return Some(Some(add));
    };
    let u = union_of(&[Tup::of(Some(old)), Tup::of(Some(add))])?;
    if same(&old, &u) { None } else { Some(Some(u)) }
}

/// gistpenalty with the opclass's penalty (gist_box_penalty, range_gist_penalty), as the
/// float4 it returns.
fn gist_penalty(orig: Option<&Key>, add: Option<&Key>) -> f32 {
    let p = match (orig, add) {
        (None, None) => return 0.0,
        (None, _) | (_, None) => return f32::INFINITY,
        (Some(Key::Box(o)), Some(Key::Box(n))) => penalty(o, n) as f32,
        (Some(Key::Range(o)), Some(Key::Range(n))) => range_penalty(o, n),
        _ => 0.0,
    };
    if p.is_nan() || p < 0.0 { 0.0 } else { p }
}

/// range_gist_penalty.
fn range_penalty(orig: &RKey, new: &RKey) -> f32 {
    const CONTAIN_EMPTY_PENALTY: f32 = 1.0;
    const INFINITE_BOUND_PENALTY: f32 = 2.0;
    let or_empty = orig.empty || orig.contain_empty;
    let (o_li, o_ui) = (!orig.empty && orig.lower.is_none(), !orig.empty && orig.upper.is_none());
    if new.empty {
        return if orig.empty {
            0.0
        } else if or_empty {
            CONTAIN_EMPTY_PENALTY
        } else if o_li && o_ui {
            2.0 * CONTAIN_EMPTY_PENALTY
        } else if o_li || o_ui {
            3.0 * CONTAIN_EMPTY_PENALTY
        } else {
            4.0 * CONTAIN_EMPTY_PENALTY
        };
    }
    let (n_li, n_ui) = (new.lower.is_none(), new.upper.is_none());
    if n_li && n_ui {
        let mut p = if o_li && o_ui {
            0.0
        } else if o_li || o_ui {
            INFINITE_BOUND_PENALTY
        } else {
            2.0 * INFINITE_BOUND_PENALTY
        };
        if or_empty {
            p += CONTAIN_EMPTY_PENALTY;
        }
        p
    } else if n_li {
        if !orig.empty && o_li {
            if o_ui {
                0.0
            } else if cmp_bounds(&new.hi(), &orig.hi()) == Ordering::Greater {
                orig.diff(new.upper, orig.upper) as f32
            } else {
                0.0
            }
        } else {
            f32::INFINITY
        }
    } else if n_ui {
        if !orig.empty && o_ui {
            if o_li {
                0.0
            } else if cmp_bounds(&new.lo(), &orig.lo()) == Ordering::Less {
                orig.diff(orig.lower, new.lower) as f32
            } else {
                0.0
            }
        } else {
            f32::INFINITY
        }
    } else if orig.empty || o_li || o_ui {
        f32::INFINITY
    } else {
        let mut diff = 0.0f64;
        if cmp_bounds(&new.lo(), &orig.lo()) == Ordering::Less {
            diff += orig.diff(orig.lower, new.lower);
        }
        if cmp_bounds(&new.hi(), &orig.hi()) == Ordering::Greater {
            diff += orig.diff(new.upper, orig.upper);
        }
        diff as f32
    }
}

fn union_of(tups: &[Tup]) -> Option<Key> {
    let mut out: Option<Key> = None;
    for k in tups.iter().filter_map(|t| t.key.as_ref()) {
        match (&mut out, k) {
            (Some(Key::Box(u)), Key::Box(b)) => adjust(u, b),
            (Some(Key::Range(u)), Key::Range(r)) => *u = super_union(u, r),
            (_, k) => out = Some(*k),
        }
    }
    out
}

/// A range of an integer-like subtype (int4, int8, date, timestamp[tz]) as range_gist
/// sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RKey {
    pub empty: bool,
    pub lower: Option<i64>,
    pub lower_inc: bool,
    pub upper: Option<i64>,
    pub upper_inc: bool,
    /// RANGE_CONTAIN_EMPTY: an inner key whose subtree holds empty ranges.
    pub contain_empty: bool,
    /// The subtype's size, and subtype_diff's divisor (1e6 for timestamps).
    pub width: u8,
    pub scale: f64,
}

#[derive(Clone, Copy)]
struct RBound {
    val: Option<i64>,
    inclusive: bool,
    lower: bool,
}

impl RKey {
    fn lo(&self) -> RBound {
        RBound { val: self.lower, inclusive: self.lower_inc, lower: true }
    }
    fn hi(&self) -> RBound {
        RBound { val: self.upper, inclusive: self.upper_inc, lower: false }
    }

    /// The index tuple: range_serialize's varlena (header, finite bounds, flags), with a
    /// 1-byte header in the tuple.
    fn tuple_size(&self) -> usize {
        let finite = if self.empty {
            0
        } else {
            self.lower.is_some() as usize + self.upper.is_some() as usize
        };
        let varlena = 8 + finite * self.width as usize + 1;
        (8 + varlena - 3).div_ceil(8) * 8
    }

    /// get_gist_range_class.
    fn class(&self) -> usize {
        if self.empty {
            return CLS_EMPTY;
        }
        ((self.lower.is_none() as usize) * CLS_LOWER_INF)
            | ((self.upper.is_none() as usize) * CLS_UPPER_INF)
            | ((self.contain_empty as usize) * CLS_CONTAIN_EMPTY)
    }

    /// subtype_diff through call_subtype_diff (negatives as 0).
    fn diff(&self, a: Option<i64>, b: Option<i64>) -> f64 {
        let (Some(a), Some(b)) = (a, b) else {
            return 0.0;
        };
        let v = (a as f64 - b as f64) / self.scale;
        if v >= 0.0 { v } else { 0.0 }
    }
}

const CLS_LOWER_INF: usize = 1;
const CLS_UPPER_INF: usize = 2;
const CLS_CONTAIN_EMPTY: usize = 4;
const CLS_EMPTY: usize = 8;
const CLS_COUNT: usize = 9;

/// range_cmp_bounds.
fn cmp_bounds(b1: &RBound, b2: &RBound) -> Ordering {
    let side = |lower: bool| if lower { Ordering::Less } else { Ordering::Greater };
    match (b1.val, b2.val) {
        (None, None) if b1.lower == b2.lower => Ordering::Equal,
        (None, _) => side(b1.lower),
        (_, None) => side(b2.lower).reverse(),
        (Some(x), Some(y)) => match x.cmp(&y) {
            Ordering::Equal => match (b1.inclusive, b2.inclusive) {
                (false, false) if b1.lower == b2.lower => Ordering::Equal,
                (false, false) | (false, true) => side(b1.lower).reverse(),
                (true, false) => side(b2.lower),
                (true, true) => Ordering::Equal,
            },
            o => o,
        },
    }
}

/// range_cmp: empty ranges first, then by lower and upper bound.
fn range_cmp(a: &RKey, b: &RKey) -> Ordering {
    match (a.empty, b.empty) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => cmp_bounds(&a.lo(), &b.lo()).then_with(|| cmp_bounds(&a.hi(), &b.hi())),
    }
}

/// range_super_union.
fn super_union(r1: &RKey, r2: &RKey) -> RKey {
    if r1.empty {
        let mut r = *r2;
        if !r.empty {
            r.contain_empty = true;
        }
        return r;
    }
    if r2.empty {
        let mut r = *r1;
        r.contain_empty = true;
        return r;
    }
    let mut r = *r1;
    if cmp_bounds(&r1.lo(), &r2.lo()) == Ordering::Greater {
        r.lower = r2.lower;
        r.lower_inc = r2.lower_inc;
    }
    if cmp_bounds(&r1.hi(), &r2.hi()) == Ordering::Less {
        r.upper = r2.upper;
        r.upper_inc = r2.upper_inc;
    }
    r.contain_empty = r1.contain_empty || r2.contain_empty;
    r
}

/// range_gist_picksplit: the 0-based positions going left and right.
fn range_picksplit(rs: &[RKey]) -> (Vec<usize>, Vec<usize>) {
    let n = rs.len();
    let mut counts = [0usize; CLS_COUNT];
    for r in rs {
        counts[r.class()] += 1;
    }
    let mut biggest = 0;
    let mut biggest_count = 0;
    let mut classes = 0;
    for (j, &c) in counts.iter().enumerate() {
        if c > 0 {
            if c > biggest_count {
                biggest_count = c;
                biggest = j;
            }
            classes += 1;
        }
    }
    if classes == 1 {
        return match biggest & !CLS_CONTAIN_EMPTY {
            0 => range_double_sorting_split(rs),
            CLS_LOWER_INF => range_single_sorting_split(rs, true),
            CLS_UPPER_INF => range_single_sorting_split(rs, false),
            _ => {
                // range_gist_fallback_split.
                let k = (n - 1) / 2;
                ((0..k).collect(), (k..n).collect())
            }
        };
    }
    let mut right = [false; CLS_COUNT];
    if counts[0] > 0 {
        right[0] = true;
    } else {
        let non_inf = counts[0] + counts[CLS_CONTAIN_EMPTY] + counts[CLS_EMPTY];
        let inf = n - non_inf;
        let non_empty =
            counts[0] + counts[CLS_LOWER_INF] + counts[CLS_UPPER_INF] + counts[CLS_LOWER_INF | CLS_UPPER_INF];
        let empty = n - non_empty;
        if inf > 0 && non_inf > 0 && inf.abs_diff(non_inf) <= empty.abs_diff(non_empty) {
            right[0] = true;
            right[CLS_CONTAIN_EMPTY] = true;
            right[CLS_EMPTY] = true;
        } else if empty > 0 && non_empty > 0 {
            right[0] = true;
            right[CLS_LOWER_INF] = true;
            right[CLS_UPPER_INF] = true;
            right[CLS_LOWER_INF | CLS_UPPER_INF] = true;
        } else {
            right[biggest] = true;
        }
    }
    (0..n).partition(|&i| !right[rs[i].class()])
}

/// range_gist_single_sorting_split.
fn range_single_sorting_split(rs: &[RKey], upper: bool) -> (Vec<usize>, Vec<usize>) {
    let n = rs.len();
    let mut items: Vec<(usize, RBound)> =
        rs.iter().enumerate().map(|(i, r)| (i, if upper { r.hi() } else { r.lo() })).collect();
    let mut f = |a: &(usize, RBound), b: &(usize, RBound)| Ok(cmp_bounds(&a.1, &b.1));
    pg_qsort(&mut items, &mut Cmp::new(&mut f));
    let k = n / 2;
    (items[..k].iter().map(|x| x.0).collect(), items[k..].iter().map(|x| x.0).collect())
}

/// range_gist_double_sorting_split.
fn range_double_sorting_split(rs: &[RKey]) -> (Vec<usize>, Vec<usize>) {
    let n = rs.len();
    let mut by_lower: Vec<(RBound, RBound)> = rs.iter().map(|r| (r.lo(), r.hi())).collect();
    let mut by_upper = by_lower.clone();
    let mut f = |a: &(RBound, RBound), b: &(RBound, RBound)| Ok(cmp_bounds(&a.0, &b.0));
    pg_qsort(&mut by_lower, &mut Cmp::new(&mut f));
    let mut f = |a: &(RBound, RBound), b: &(RBound, RBound)| Ok(cmp_bounds(&a.1, &b.1));
    pg_qsort(&mut by_upper, &mut Cmp::new(&mut f));
    let r0 = rs[0];
    // The chosen split: right_lower, left_upper, ratio, overlap, common_left.
    let mut best: Option<(RBound, RBound, f32, f32, usize)> = None;
    let mut consider = |right_lower: RBound, min_left: usize, left_upper: RBound, max_left: usize| {
        let left = if min_left >= n.div_ceil(2) {
            min_left
        } else if max_left <= n / 2 {
            max_left
        } else {
            n / 2
        };
        let right = n - left;
        let ratio = left.min(right) as f32 / n as f32;
        if f64::from(ratio) <= LIMIT_RATIO {
            return;
        }
        let overlap = r0.diff(left_upper.val, right_lower.val) as f32;
        let select = match &best {
            None => true,
            Some(b) => overlap < b.3 || (overlap == b.3 && ratio > b.2),
        };
        if select {
            best = Some((right_lower, left_upper, ratio, overlap, max_left - left));
        }
    };
    let (mut i1, mut i2) = (0usize, 0usize);
    let mut right_lower = by_lower[0].0;
    let mut left_upper = by_upper[0].0;
    loop {
        while i1 < n && cmp_bounds(&right_lower, &by_lower[i1].0) == Ordering::Equal {
            if cmp_bounds(&by_lower[i1].1, &left_upper) == Ordering::Greater {
                left_upper = by_lower[i1].1;
            }
            i1 += 1;
        }
        if i1 >= n {
            break;
        }
        right_lower = by_lower[i1].0;
        while i2 < n && cmp_bounds(&by_upper[i2].1, &left_upper) != Ordering::Greater {
            i2 += 1;
        }
        consider(right_lower, i1, left_upper, i2);
    }
    let (mut i1, mut i2) = (n as isize - 1, n as isize - 1);
    let mut right_lower = by_lower[i1 as usize].1;
    let mut left_upper = by_upper[i2 as usize].1;
    loop {
        while i2 >= 0 && cmp_bounds(&left_upper, &by_upper[i2 as usize].1) == Ordering::Equal {
            if cmp_bounds(&by_upper[i2 as usize].0, &right_lower) == Ordering::Less {
                right_lower = by_upper[i2 as usize].0;
            }
            i2 -= 1;
        }
        if i2 < 0 {
            break;
        }
        left_upper = by_upper[i2 as usize].1;
        while i1 >= 0 && cmp_bounds(&by_lower[i1 as usize].0, &right_lower) != Ordering::Less {
            i1 -= 1;
        }
        consider(right_lower, (i1 + 1) as usize, left_upper, (i2 + 1) as usize);
    }
    let Some((right_lower, left_upper, _, _, common_left)) = best else {
        let k = (n - 1) / 2;
        return ((0..k).collect(), (k..n).collect());
    };
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let mut common: Vec<(usize, f64)> = Vec::new();
    for (i, r) in rs.iter().enumerate() {
        if cmp_bounds(&r.hi(), &left_upper) != Ordering::Greater {
            if cmp_bounds(&r.lo(), &right_lower) != Ordering::Less {
                common.push((i, r.diff(r.lower, right_lower.val) - r.diff(left_upper.val, r.upper)));
            } else {
                left.push(i);
            }
        } else {
            right.push(i);
        }
    }
    let mut f = |a: &(usize, f64), b: &(usize, f64)| {
        Ok(if a.1 < b.1 {
            Ordering::Less
        } else if a.1 > b.1 {
            Ordering::Greater
        } else {
            Ordering::Equal
        })
    };
    pg_qsort(&mut common, &mut Cmp::new(&mut f));
    for (k, &(i, _)) in common.iter().enumerate() {
        if k < common_left {
            left.push(i);
        } else {
            right.push(i);
        }
    }
    (left, right)
}

/// gistfitpage.
fn fits(tups: &[Tup]) -> bool {
    tups.iter().map(|t| t.size + ITEM_ID).sum::<usize>() <= PAGE_SIZE
}

struct Ctx {
    n: usize,
    bbox: GBox,
    first: bool,
    left_upper: f64,
    right_lower: f64,
    ratio: f32,
    overlap: f32,
    dim: usize,
    range: f64,
}

fn non_negative(v: f32) -> f32 {
    if v >= 0.0 { v } else { 0.0 }
}

/// g_box_consider_split.
fn consider(c: &mut Ctx, dim: usize, right_lower: f64, min_left: usize, left_upper: f64, max_left: usize) {
    let left = if min_left >= c.n.div_ceil(2) {
        min_left
    } else if max_left <= c.n / 2 {
        max_left
    } else {
        c.n / 2
    };
    let right = c.n - left;
    let ratio = left.min(right) as f32 / c.n as f32;
    if f64::from(ratio) <= LIMIT_RATIO {
        return;
    }
    let range = if dim == 0 { c.bbox.hx - c.bbox.lx } else { c.bbox.hy - c.bbox.ly };
    let overlap = ((left_upper - right_lower) / range) as f32;
    let select = if c.first {
        true
    } else if c.dim == dim {
        overlap < c.overlap || (overlap == c.overlap && ratio > c.ratio)
    } else {
        non_negative(overlap) < non_negative(c.overlap)
            || (range > c.range && non_negative(overlap) <= non_negative(c.overlap))
    };
    if select {
        c.first = false;
        c.ratio = ratio;
        c.range = range;
        c.overlap = overlap;
        c.right_lower = right_lower;
        c.left_upper = left_upper;
        c.dim = dim;
    }
}

/// gist_box_picksplit: the 0-based positions going left and right, and their unions.
fn picksplit(boxes: &[GBox]) -> (Vec<usize>, Vec<usize>) {
    let n = boxes.len();
    let mut bbox = boxes[0];
    for b in &boxes[1..] {
        adjust(&mut bbox, b);
    }
    let mut c = Ctx {
        n,
        bbox,
        first: true,
        left_upper: 0.0,
        right_lower: 0.0,
        ratio: 0.0,
        overlap: 0.0,
        dim: 0,
        range: 0.0,
    };
    for dim in 0..2 {
        let mut lower: Vec<(f64, f64)> =
            boxes.iter().map(|b| if dim == 0 { (b.lx, b.hx) } else { (b.ly, b.hy) }).collect();
        let mut upper = lower.clone();
        let mut f = |a: &(f64, f64), b: &(f64, f64)| Ok(f_cmp(a.0, b.0));
        pg_qsort(&mut lower, &mut Cmp::new(&mut f));
        let mut f = |a: &(f64, f64), b: &(f64, f64)| Ok(f_cmp(a.1, b.1));
        pg_qsort(&mut upper, &mut Cmp::new(&mut f));
        let (mut i1, mut i2) = (0usize, 0usize);
        let mut right_lower = lower[0].0;
        let mut left_upper = upper[0].0;
        loop {
            while i1 < n && f_eq(right_lower, lower[i1].0) {
                if f_lt(left_upper, lower[i1].1) {
                    left_upper = lower[i1].1;
                }
                i1 += 1;
            }
            if i1 >= n {
                break;
            }
            right_lower = lower[i1].0;
            while i2 < n && f_le(upper[i2].1, left_upper) {
                i2 += 1;
            }
            consider(&mut c, dim, right_lower, i1, left_upper, i2);
        }
        let (mut i1, mut i2) = (n as isize - 1, n as isize - 1);
        let mut right_lower = lower[i1 as usize].1;
        let mut left_upper = upper[i2 as usize].1;
        loop {
            while i2 >= 0 && f_eq(left_upper, upper[i2 as usize].1) {
                if f_gt(right_lower, upper[i2 as usize].0) {
                    right_lower = upper[i2 as usize].0;
                }
                i2 -= 1;
            }
            if i2 < 0 {
                break;
            }
            left_upper = upper[i2 as usize].1;
            while i1 >= 0 && f_ge(lower[i1 as usize].0, right_lower) {
                i1 -= 1;
            }
            consider(&mut c, dim, right_lower, (i1 + 1) as usize, left_upper, (i2 + 1) as usize);
        }
    }
    if c.first {
        // fallbackSplit: the first half left.
        let half = n / 2;
        return ((0..half).collect(), (half..n).collect());
    }
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let (mut lbox, mut rbox) = (GBox::point(0.0, 0.0), GBox::point(0.0, 0.0));
    let place = |side: &mut Vec<usize>, sbox: &mut GBox, i: usize| {
        if side.is_empty() {
            *sbox = boxes[i];
        } else {
            adjust(sbox, &boxes[i]);
        }
        side.push(i);
    };
    let mut common: Vec<(usize, f64)> = Vec::new();
    for (i, b) in boxes.iter().enumerate() {
        let (lo, up) = if c.dim == 0 { (b.lx, b.hx) } else { (b.ly, b.hy) };
        if f_le(up, c.left_upper) {
            if f_ge(lo, c.right_lower) {
                common.push((i, 0.0));
            } else {
                place(&mut left, &mut lbox, i);
            }
        } else {
            place(&mut right, &mut rbox, i);
        }
    }
    if !common.is_empty() {
        let m = (LIMIT_RATIO * n as f64).ceil() as usize;
        for e in &mut common {
            e.1 = (penalty(&lbox, &boxes[e.0]) - penalty(&rbox, &boxes[e.0])).abs();
        }
        let mut f = |a: &(usize, f64), b: &(usize, f64)| Ok(f_cmp(a.1, b.1));
        pg_qsort(&mut common, &mut Cmp::new(&mut f));
        let total = common.len();
        for (k, &(i, _)) in common.iter().enumerate() {
            let rest = total - k;
            if left.len() + rest <= m {
                place(&mut left, &mut lbox, i);
            } else if right.len() + rest <= m {
                place(&mut right, &mut rbox, i);
            } else if penalty(&lbox, &boxes[i]) < penalty(&rbox, &boxes[i]) {
                place(&mut left, &mut lbox, i);
            } else {
                place(&mut right, &mut rbox, i);
            }
        }
    }
    (left, right)
}

/// gistSplit: the pages the tuples split into, left parts first.
fn split(tups: &[Tup]) -> Vec<Vec<Tup>> {
    let len = tups.len();
    if len <= 1 {
        return vec![tups.to_vec()];
    }
    let nulls: Vec<usize> = (0..len).filter(|&i| tups[i].key.is_none()).collect();
    let (left, right): (Vec<usize>, Vec<usize>) = if nulls.len() == len {
        // gistSplitHalf: the first (len / 2 - 1) right.
        let r = (len / 2).saturating_sub(1);
        ((r..len).collect(), (0..r).collect())
    } else if !nulls.is_empty() {
        ((0..len).filter(|i| tups[*i].key.is_some()).collect(), nulls)
    } else if let Some(Key::Range(_)) = tups[0].key {
        let rs: Vec<RKey> = tups
            .iter()
            .map(|t| match t.key {
                Some(Key::Range(r)) => r,
                _ => unreachable!("mixed GiST keys"),
            })
            .collect();
        range_picksplit(&rs)
    } else {
        let boxes: Vec<GBox> = tups
            .iter()
            .map(|t| match t.key {
                Some(Key::Box(b)) => b,
                _ => unreachable!("mixed GiST keys"),
            })
            .collect();
        picksplit(&boxes)
    };
    // gistUserPicksplit: a picksplit that put everything on one side is replaced by
    // genericPickSplit's halves.
    let (left, right) = if left.is_empty() || right.is_empty() {
        let half = len / 2;
        ((0..half).collect(), (half..len).collect())
    } else {
        (left, right)
    };
    let side = |ix: &[usize]| -> Vec<Vec<Tup>> {
        let v: Vec<Tup> = ix.iter().map(|&i| tups[i]).collect();
        if fits(&v) { vec![v] } else { split(&v) }
    };
    let mut out = side(&left);
    out.extend(side(&right));
    out
}

struct Level {
    pages: Vec<Vec<Tup>>,
}

fn free_space(page: &[Tup]) -> usize {
    let used: usize = page.iter().map(|t| t.size + ITEM_ID).sum();
    (PAGE_SIZE - used).saturating_sub(ITEM_ID)
}

#[derive(Clone, Default)]
struct Page {
    leaf: bool,
    tups: Vec<Tup>,
    /// The clock at the page's last split (its NSN).
    nsn: u64,
}

/// A GiST index: its pages by block number (block 0 the root).
pub struct Gist {
    pages: Vec<Page>,
    levels: Vec<Level>,
    clock: u64,
    /// Leaf pages the last VACUUM deleted (recyclable after the next one), and those free
    /// to reuse (the free space map's: gistNewBuffer takes one before extending).
    deleted: Vec<u32>,
    free: std::collections::BTreeSet<u32>,
}

/// A step of an insertion's path: the block, the clock when it was read, and the offset
/// of its downlink in the step above.
#[derive(Clone, Copy)]
struct Step {
    blk: u32,
    lsn: u64,
    off: Option<usize>,
}

impl Gist {
    fn alloc(&mut self, leaf: bool, tups: Vec<Tup>) -> u32 {
        if let Some(b) = self.free.pop_first() {
            self.pages[b as usize] = Page { leaf, tups, nsn: 0 };
            return b;
        }
        self.pages.push(Page { leaf, tups, nsn: 0 });
        (self.pages.len() - 1) as u32
    }

    /// gistbulkdelete: the leaf tuples whose rows `dead` names removed, and the leaf pages
    /// left empty deleted (gistvacuum_delete_empty_pages: their downlinks removed, each
    /// internal page keeping one at least, the root staying). A deleted page stays in the
    /// file; the VACUUM after next finds it recyclable and later splits reuse it.
    pub fn vacuum(&mut self, dead: &dyn Fn(u32) -> bool) {
        let recyclable = std::mem::take(&mut self.deleted);
        self.free.extend(recyclable);
        let gone: std::collections::HashSet<u32> = self.free.iter().copied().collect();
        let mut empty = std::collections::HashSet::new();
        for (b, p) in self.pages.iter_mut().enumerate() {
            if !p.leaf || gone.contains(&(b as u32)) {
                continue;
            }
            p.tups.retain(|t| t.row == NO_ROW || !dead(t.row));
            if b != 0 && p.tups.is_empty() {
                empty.insert(b as u32);
            }
        }
        for b in 0..self.pages.len() {
            if empty.is_empty() {
                break;
            }
            let p = &self.pages[b];
            if p.leaf || gone.contains(&(b as u32)) {
                continue;
            }
            let mut i = 0;
            while i < self.pages[b].tups.len() && self.pages[b].tups.len() > 1 {
                let child = self.pages[b].tups[i].child;
                if empty.remove(&child) {
                    self.pages[b].tups.remove(i);
                    self.deleted.push(child);
                } else {
                    i += 1;
                }
            }
        }
    }

    /// gist_indexsortbuild_levelstate_add.
    fn add(&mut self, l: usize, t: Tup) {
        if self.levels.len() <= l {
            self.levels.push(Level { pages: vec![Vec::new()] });
        }
        let cur = self.levels[l].pages.last().unwrap();
        if free_space(cur) < t.size + ITEM_ID {
            if self.levels[l].pages.len() == PAGES_PER_FLUSH {
                self.flush(l);
            } else {
                self.levels[l].pages.push(Vec::new());
            }
        }
        self.levels[l].pages.last_mut().unwrap().push(t);
    }

    /// gist_indexsortbuild_levelstate_flush.
    fn flush(&mut self, l: usize) {
        let pages = std::mem::replace(&mut self.levels[l].pages, vec![Vec::new()]);
        let dist = if pages.len() > 1 { split(&pages.concat()) } else { pages };
        for page in dist {
            let u = union_of(&page);
            let blk = self.alloc(l == 0, page);
            self.add(l + 1, Tup::linking(u, blk));
        }
    }

    /// The index blocks.
    pub fn nblocks(&self) -> u64 {
        self.pages.len() as u64
    }

    /// gistchoose, keeping the current best on ties (Postgres flips a coin).
    fn choose(&self, blk: u32, t: &Tup) -> usize {
        let mut best = -1.0f32;
        let mut result = 0;
        for (i, it) in self.pages[blk as usize].tups.iter().enumerate() {
            let p = gist_penalty(it.key.as_ref(), t.key.as_ref());
            if best < 0.0 || p < best {
                result = i;
                best = p;
            }
            if p == 0.0 {
                break;
            }
        }
        result
    }

    /// The path from the root to `blk`'s parent (gistFindPath), with downlink offsets.
    fn path_to(&self, blk: u32) -> Option<Vec<Step>> {
        let mut prev: std::collections::HashMap<u32, (u32, usize)> = Default::default();
        let mut queue = std::collections::VecDeque::from([0u32]);
        while let Some(b) = queue.pop_front() {
            let p = &self.pages[b as usize];
            if p.leaf {
                continue;
            }
            for (i, t) in p.tups.iter().enumerate() {
                prev.insert(t.child, (b, i));
                if t.child == blk {
                    let mut steps = vec![Step { blk, lsn: self.clock, off: Some(i) }];
                    let mut cur = b;
                    while let Some(&(pb, pi)) = prev.get(&cur) {
                        steps.push(Step { blk: cur, lsn: self.clock, off: Some(pi) });
                        cur = pb;
                    }
                    steps.push(Step { blk: cur, lsn: self.clock, off: None });
                    steps.reverse();
                    return Some(steps);
                }
                queue.push_back(t.child);
            }
        }
        None
    }

    /// gistplacetopage (and gistfinishsplit): put `tuples` on the last page of `stack`,
    /// replacing the tuple at `old`; whether the page split.
    fn place(&mut self, stack: &[Step], tuples: Vec<Tup>, old: Option<usize>) -> bool {
        let blk = stack.last().unwrap().blk as usize;
        let page = &self.pages[blk];
        let deleted = old.map_or(0, |o| page.tups[o].size + ITEM_ID);
        let need: usize = tuples.iter().map(|t| t.size + ITEM_ID).sum();
        if free_space(&page.tups) + deleted >= need {
            let page = &mut self.pages[blk];
            match old {
                Some(o) if tuples.len() == 1 => page.tups[o] = tuples[0],
                Some(o) => {
                    page.tups.remove(o);
                    page.tups.extend(tuples);
                }
                None => page.tups.extend(tuples),
            }
            self.clock += 1;
            return false;
        }
        let mut items = page.tups.clone();
        let leaf = page.leaf;
        if let Some(o) = old {
            items.remove(o);
        }
        items.extend(tuples);
        let dist = split(&items);
        self.clock += 1;
        if blk == 0 {
            let mut downs = Vec::new();
            for d in dist {
                let u = union_of(&d);
                let b = self.alloc(leaf, d);
                downs.push(Tup::linking(u, b));
            }
            self.pages[0] = Page { leaf: false, tups: downs, nsn: 0 };
            return true;
        }
        let mut info = Vec::new();
        for (i, d) in dist.into_iter().enumerate() {
            let u = union_of(&d);
            let b = if i == 0 {
                self.pages[blk].tups = d;
                blk as u32
            } else {
                self.alloc(leaf, d)
            };
            info.push(Tup::linking(u, b));
        }
        self.pages[blk].nsn = self.clock;
        // gistfinishsplit: the downlinks right to left, then the first two in place of
        // the old one.
        let child = blk as u32;
        for pos in (2..info.len()).rev() {
            let mut path = self.path_to(child).expect("GiST page without a parent");
            path.pop();
            self.place(&path, vec![info[pos]], None);
        }
        let mut path = self.path_to(child).expect("GiST page without a parent");
        let off = path.pop().unwrap().off;
        self.place(&path, vec![info[0], info[1]], off);
        true
    }

    /// gistdoinsert of a tuple with this key.
    pub fn insert(&mut self, key: Option<Key>) {
        self.insert_row(key, NO_ROW);
    }

    /// gistdoinsert of a tuple with this key, pointing to row `row`.
    pub fn insert_row(&mut self, key: Option<Key>, row: u32) {
        let t = Tup { row, ..Tup::of(key) };
        let mut stack = vec![Step { blk: 0, lsn: self.clock, off: None }];
        loop {
            // Back up past pages split since their parent was read.
            while stack.len() > 1 {
                let s = stack[stack.len() - 1];
                let parent = stack[stack.len() - 2];
                if self.pages[s.blk as usize].nsn > parent.lsn {
                    stack.pop();
                } else {
                    break;
                }
            }
            let top = stack.len() - 1;
            stack[top].lsn = self.clock;
            let blk = stack[top].blk;
            if self.pages[blk as usize].leaf {
                self.place(&stack, vec![t], None);
                return;
            }
            let off = self.choose(blk, &t);
            let down = self.pages[blk as usize].tups[off];
            if let Some(newkey) = adjusted(down.key, t.key)
                && self.place(&stack, vec![Tup::linking(newkey, down.child)], Some(off))
            {
                if blk != 0 {
                    stack.pop();
                }
                continue;
            }
            stack.push(Step { blk: down.child, lsn: self.clock, off: Some(off) });
        }
    }
}

/// A sorted build of these keys (in heap TID order).
pub fn sorted_build(keys: Vec<Option<Key>>) -> Gist {
    sorted_build_rows(keys.into_iter().map(|k| (k, NO_ROW)).collect())
}

/// A sorted build of these keys (in heap TID order), each leaf tuple pointing to its row.
pub fn sorted_build_rows(keys: Vec<(Option<Key>, u32)>) -> Gist {
    // The sort: points by z-order, ranges by range_cmp, nulls last; ties in the given
    // (TID) order.
    let mut keyed: Vec<(usize, Option<Key>, u64, u32)> = keys
        .into_iter()
        .enumerate()
        .map(|(i, (k, row))| {
            let z = match &k {
                Some(Key::Box(b)) => zorder(b),
                _ => 0,
            };
            (i, k, z, row)
        })
        .collect();
    keyed.sort_by(|a, b| {
        match (&a.1, &b.1) {
            (Some(Key::Box(_)), Some(Key::Box(_))) => a.2.cmp(&b.2),
            (Some(Key::Range(x)), Some(Key::Range(y))) => range_cmp(x, y),
            (None, None) => Ordering::Equal,
            (None, _) => Ordering::Greater,
            (_, None) => Ordering::Less,
            _ => Ordering::Equal,
        }
        .then(a.0.cmp(&b.0))
    });
    // Block 0 is the root's.
    let mut g = Gist {
        pages: vec![Page::default()],
        levels: vec![Level { pages: vec![Vec::new()] }],
        clock: 0,
        deleted: Vec::new(),
        free: Default::default(),
    };
    for (_, k, _, row) in keyed {
        g.add(0, Tup { row, ..Tup::of(k) });
    }
    let mut l = 0;
    while l + 1 < g.levels.len() || g.levels[l].pages.len() > 1 {
        g.flush(l);
        l += 1;
    }
    let root = g.levels[l].pages.pop().unwrap_or_default();
    g.pages[0] = Page { leaf: l == 0, tups: root, nsn: 0 };
    g.levels.clear();
    g
}

/// A queue entry of a GiST scan (GISTSearchItem): a heap tuple's row, or an index page to
/// visit, with its ordering distances (None: null).
#[derive(Clone)]
struct SearchItem {
    dist: Vec<Option<f64>>,
    /// The row (a heap item), else the block of an index page.
    row: Option<u32>,
    blk: u32,
}

/// float8_cmp_internal: NaN above everything, equal to itself.
fn float8_cmp(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

/// pairingheap_GISTSearchItem_cmp: the nearer item is the greater; heap items before
/// index pages at the same distance.
fn item_cmp(a: &SearchItem, b: &SearchItem) -> i32 {
    for (x, y) in a.dist.iter().zip(&b.dist) {
        match (x, y) {
            (None, Some(_)) => return -1,
            (Some(_), None) => return 1,
            (Some(x), Some(y)) => match float8_cmp(*x, *y) {
                Ordering::Less => return 1,
                Ordering::Greater => return -1,
                Ordering::Equal => {}
            },
            (None, None) => {}
        }
    }
    match (a.row.is_some(), b.row.is_some()) {
        (true, false) => 1,
        (false, true) => -1,
        _ => 0,
    }
}

/// lib/pairingheap.c over search items (a max-heap by `item_cmp`).
#[derive(Default)]
struct PairingHeap {
    items: Vec<SearchItem>,
    first_child: Vec<Option<usize>>,
    next_sibling: Vec<Option<usize>>,
    root: Option<usize>,
}

impl PairingHeap {
    fn merge(&mut self, a: Option<usize>, b: Option<usize>) -> Option<usize> {
        let (Some(mut a), Some(mut b)) = (a, b) else {
            return a.or(b);
        };
        if item_cmp(&self.items[a], &self.items[b]) < 0 {
            std::mem::swap(&mut a, &mut b);
        }
        self.next_sibling[b] = self.first_child[a];
        self.first_child[a] = Some(b);
        Some(a)
    }

    fn add(&mut self, item: SearchItem) {
        let n = self.items.len();
        self.items.push(item);
        self.first_child.push(None);
        self.next_sibling.push(None);
        self.root = self.merge(self.root, Some(n));
        if let Some(r) = self.root {
            self.next_sibling[r] = None;
        }
    }

    fn merge_children(&mut self, children: Option<usize>) -> Option<usize> {
        let c = children?;
        if self.next_sibling[c].is_none() {
            return Some(c);
        }
        let mut next = Some(c);
        let mut pairs: Option<usize> = None;
        while let Some(curr) = next {
            let Some(sib) = self.next_sibling[curr] else {
                self.next_sibling[curr] = pairs;
                pairs = Some(curr);
                break;
            };
            next = self.next_sibling[sib];
            let m = self.merge(Some(curr), Some(sib)).unwrap();
            self.next_sibling[m] = pairs;
            pairs = Some(m);
        }
        let mut newroot = pairs;
        let mut next = pairs.and_then(|p| self.next_sibling[p]);
        while let Some(curr) = next {
            next = self.next_sibling[curr];
            newroot = self.merge(newroot, Some(curr));
        }
        newroot
    }

    fn remove_first(&mut self) -> Option<SearchItem> {
        let r = self.root?;
        let children = self.first_child[r];
        self.root = self.merge_children(children);
        if let Some(nr) = self.root {
            self.next_sibling[nr] = None;
        }
        Some(self.items[r].clone())
    }
}

/// A GiST index scan in progress (gistgettuple's queue): rows come out as they're asked
/// for, so a scan wanting k rows (`ORDER BY p <-> q LIMIT k`) reads about the pages and
/// entries nearest the query, not the whole index.
pub struct Search {
    orderbys: usize,
    queue: PairingHeap,
    /// An unordered scan's matching rows of the leaf page read last.
    page: std::collections::VecDeque<u32>,
    started: bool,
}

impl Search {
    pub fn new(orderbys: usize) -> Search {
        Search { orderbys, queue: PairingHeap::default(), page: Default::default(), started: false }
    }

    /// The scan's next row over index `g` (`test`: see `Gist::search`; it must be the
    /// same function every call).
    pub fn next(
        &mut self,
        g: &Gist,
        test: &mut dyn FnMut(Option<&Key>, bool) -> Option<Vec<Option<f64>>>,
    ) -> Option<u32> {
        if !self.started {
            self.started = true;
            g.scan_page(0, self.orderbys, &mut self.queue, &mut self.page, test);
        }
        loop {
            if let Some(r) = self.page.pop_front() {
                return Some(r);
            }
            let item = self.queue.remove_first()?;
            match item.row {
                Some(r) => return Some(r),
                None => g.scan_page(item.blk, self.orderbys, &mut self.queue, &mut self.page, test),
            }
        }
    }
}

impl Gist {
    /// A GiST index scan (gistgettuple): the rows of the leaf tuples `test` accepts, in the
    /// order the scan returns them. `test(key, leaf)` is the consistent function: None to
    /// skip the tuple, else its ordering distances (`orderbys` of them; none for an
    /// unordered scan, which returns each leaf page's matches as it reads the page).
    pub fn search(
        &self,
        orderbys: usize,
        test: &mut dyn FnMut(Option<&Key>, bool) -> Option<Vec<Option<f64>>>,
    ) -> Vec<u32> {
        let mut s = Search::new(orderbys);
        let mut out = Vec::new();
        while let Some(r) = s.next(self, test) {
            out.push(r);
        }
        out
    }

    /// One page of a scan (gistScanPage): an unordered scan's matching leaf rows go to
    /// `out`, everything else into the queue.
    fn scan_page(
        &self,
        blk: u32,
        orderbys: usize,
        queue: &mut PairingHeap,
        out: &mut std::collections::VecDeque<u32>,
        test: &mut dyn FnMut(Option<&Key>, bool) -> Option<Vec<Option<f64>>>,
    ) {
        let page = &self.pages[blk as usize];
        for t in &page.tups {
            let Some(mut dist) = test(t.key.as_ref(), page.leaf) else {
                continue;
            };
            dist.resize(orderbys, None);
            if page.leaf && orderbys == 0 {
                out.push_back(t.row);
            } else if page.leaf {
                queue.add(SearchItem { dist, row: Some(t.row), blk: 0 });
            } else {
                queue.add(SearchItem { dist, row: None, blk: t.child });
            }
        }
    }

    /// The pages a scan reads (gistScanPage locks each one): the root, then every page a
    /// downlink `test` accepts leads to.
    pub fn search_pages(
        &self,
        test: &mut dyn FnMut(Option<&Key>, bool) -> Option<Vec<Option<f64>>>,
    ) -> Vec<u32> {
        let mut out = Vec::new();
        let mut todo = vec![0u32];
        while let Some(blk) = todo.pop() {
            let Some(page) = self.pages.get(blk as usize) else { continue };
            out.push(blk);
            if page.leaf {
                continue;
            }
            for t in &page.tups {
                if test(t.key.as_ref(), false).is_some() {
                    todo.push(t.child);
                }
            }
        }
        out
    }

    /// The leaf page an insertion of `key` goes to (gistdoinsert's descent, gistchoose at
    /// each level): what its SSI check names.
    pub fn leaf_for(&self, key: Option<Key>) -> u32 {
        let t = Tup::of(key);
        let mut blk = 0u32;
        while let Some(page) = self.pages.get(blk as usize) {
            if page.leaf || page.tups.is_empty() {
                break;
            }
            let off = self.choose(blk, &t);
            blk = page.tups[off].child;
        }
        blk
    }

    /// A key's box (a point_ops entry).
    pub fn key_box(key: &Key) -> Option<GBox> {
        match key {
            Key::Box(b) => Some(*b),
            Key::Range(_) => None,
        }
    }
}
