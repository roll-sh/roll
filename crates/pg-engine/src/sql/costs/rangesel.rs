//! rangesel and multirangesel (rangetypes_selfuncs.c, multirangetypes_selfuncs.c): a range
//! or multirange column against a constant, estimated from the column's bounds histogram and
//! its length histogram with the fraction of empty ranges (range_typanalyze's slots).

use super::{DEFAULT_INEQ_SEL, DEFAULT_RANGE_INEQ_SEL, Est, slot};
use crate::sql::expr::Expr;
use crate::types::range::{Bound, Range, RangeType, cmp_bounds};
use crate::types::{DataType, Value};
use std::cmp::Ordering;

/// STATISTIC_KIND_RANGE_LENGTH_HISTOGRAM and STATISTIC_KIND_BOUNDS_HISTOGRAM.
const KIND_RANGE_LENGTH: i16 = 6;
const KIND_BOUNDS: i16 = 7;

/// A range operator with the column on its left.
#[derive(Clone, Copy, PartialEq)]
enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Left,
    Right,
    OverLeft,
    OverRight,
    Overlap,
    Contains,
    ContainedBy,
    /// `@>` of an element.
    ContainsElem,
}

impl Op {
    fn of(name: &str) -> Option<Op> {
        Some(match name {
            "<" => Op::Lt,
            "<=" => Op::Le,
            ">" => Op::Gt,
            ">=" => Op::Ge,
            "<<" => Op::Left,
            ">>" => Op::Right,
            "&<" => Op::OverLeft,
            "&>" => Op::OverRight,
            "&&" => Op::Overlap,
            "@>" => Op::Contains,
            "<@" => Op::ContainedBy,
            _ => return None,
        })
    }

    /// The commutator (`&<` and `&>` have none).
    fn commute(self) -> Option<Op> {
        Some(match self {
            Op::Lt => Op::Gt,
            Op::Le => Op::Ge,
            Op::Gt => Op::Lt,
            Op::Ge => Op::Le,
            Op::Left => Op::Right,
            Op::Right => Op::Left,
            Op::Overlap => Op::Overlap,
            Op::Contains => Op::ContainedBy,
            Op::ContainedBy => Op::Contains,
            Op::OverLeft | Op::OverRight | Op::ContainsElem => return None,
        })
    }

    /// default_range_selectivity / default_multirange_selectivity.
    fn default_sel(self) -> f64 {
        match self {
            Op::Overlap => 0.01,
            Op::Contains | Op::ContainedBy => 0.005,
            Op::ContainsElem => DEFAULT_RANGE_INEQ_SEL,
            _ => DEFAULT_INEQ_SEL,
        }
    }
}

/// A range type's subtype_diff of two bound values, for the built-in ones (None: the type
/// has none, or one this doesn't evaluate).
fn subdiff(rt: &RangeType, a: &Value, b: &Value) -> Option<f64> {
    Some(match rt.subdiff.as_deref()? {
        "int4range_subdiff" | "int8range_subdiff" | "numrange_subdiff" | "float8mi" => a.as_f64()? - b.as_f64()?,
        "daterange_subdiff" => match (a, b) {
            (Value::Date(x), Value::Date(y)) => *x as f64 - *y as f64,
            _ => return None,
        },
        "tsrange_subdiff" | "tstzrange_subdiff" => match (a, b) {
            (Value::Timestamp(x), Value::Timestamp(y)) | (Value::Timestamptz(x), Value::Timestamptz(y)) => {
                (*x as f64 - *y as f64) / 1_000_000.0
            }
            _ => return None,
        },
        _ => return None,
    })
}

/// The histograms' search and interpolation over one range type.
struct Hist<'a> {
    rt: &'a RangeType,
}

impl Hist<'_> {
    /// rbound_bsearch: the last histogram bound below `value` (or equal to it, `equal`).
    fn bsearch(&self, value: &Bound, hist: &[Bound], equal: bool) -> isize {
        let (mut lower, mut upper) = (-1isize, hist.len() as isize - 1);
        while lower < upper {
            let middle = (lower + upper + 1) / 2;
            let cmp = cmp_bounds(&hist[middle as usize], value);
            if cmp == Ordering::Less || (equal && cmp == Ordering::Equal) {
                lower = middle;
            } else {
                upper = middle - 1;
            }
        }
        lower
    }

    /// calc_hist_selectivity_scalar.
    fn scalar(&self, constbound: &Bound, hist: &[Bound], equal: bool) -> f64 {
        let n = hist.len();
        let index = self.bsearch(constbound, hist, equal);
        let mut selec = (index.max(0) as f64) / (n - 1) as f64;
        if index >= 0 && (index as usize) < n - 1 {
            selec += self.position(constbound, &hist[index as usize], &hist[index as usize + 1]) / (n - 1) as f64;
        }
        selec
    }

    /// get_position.
    fn position(&self, value: &Bound, hist1: &Bound, hist2: &Bound) -> f64 {
        match (&hist1.val, &hist2.val) {
            (Some(h1), Some(h2)) => {
                let Some(v) = &value.val else { return 0.5 };
                let Some(bin_width) = subdiff(self.rt, h2, h1) else { return 0.5 };
                if bin_width.is_nan() || bin_width <= 0.0 {
                    return 0.5;
                }
                let Some(d) = subdiff(self.rt, v, h1) else { return 0.5 };
                let position = d / bin_width;
                if position.is_nan() {
                    return 0.5;
                }
                position.clamp(0.0, 1.0)
            }
            (None, Some(_)) => {
                if value.infinite() && value.lower {
                    0.0
                } else {
                    1.0
                }
            }
            (Some(_), None) => {
                if value.infinite() && !value.lower {
                    1.0
                } else {
                    0.0
                }
            }
            (None, None) => 0.5,
        }
    }

    /// get_distance.
    fn distance(&self, b1: &Bound, b2: &Bound) -> f64 {
        match (&b1.val, &b2.val) {
            (Some(v1), Some(v2)) => match subdiff(self.rt, v2, v1) {
                Some(r) if !(r.is_nan() || r < 0.0) => r,
                _ => 1.0,
            },
            (None, None) if b1.lower == b2.lower => 0.0,
            _ => f64::INFINITY,
        }
    }

    /// calc_hist_selectivity_contained.
    fn contained(&self, lower: &Bound, upper: &Bound, hist_lower: &[Bound], lengths: &[f64]) -> f64 {
        let n = hist_lower.len();
        let upper = Bound { val: upper.val.clone(), inclusive: !upper.inclusive, lower: true };
        let upper_index = self.bsearch(&upper, hist_lower, false);
        if upper_index < 0 {
            return 0.0;
        }
        let upper_index = (upper_index as usize).min(n - 2);
        let mut bin_width = self.position(&upper, &hist_lower[upper_index], &hist_lower[upper_index + 1]);
        let mut prev_dist = 0.0;
        let mut sum_frac = 0.0;
        let mut i = upper_index as isize;
        while i >= 0 {
            let iu = i as usize;
            let mut final_bin = false;
            let dist = if cmp_bounds(&hist_lower[iu], lower) == Ordering::Less {
                let dist = self.distance(lower, &upper);
                bin_width -= self.position(lower, &hist_lower[iu], &hist_lower[iu + 1]);
                if bin_width < 0.0 {
                    bin_width = 0.0;
                }
                final_bin = true;
                dist
            } else {
                self.distance(&hist_lower[iu], &upper)
            };
            let frac = length_hist_frac(lengths, prev_dist, dist, true);
            sum_frac += frac * bin_width / (n - 1) as f64;
            if final_bin {
                break;
            }
            bin_width = 1.0;
            prev_dist = dist;
            i -= 1;
        }
        sum_frac
    }

    /// calc_hist_selectivity_contains.
    fn contains(&self, lower: &Bound, upper: &Bound, hist_lower: &[Bound], lengths: &[f64]) -> f64 {
        let n = hist_lower.len();
        let lower_index = self.bsearch(lower, hist_lower, true);
        if lower_index < 0 {
            return 0.0;
        }
        let lower_index = (lower_index as usize).min(n - 2);
        let mut bin_width = self.position(lower, &hist_lower[lower_index], &hist_lower[lower_index + 1]);
        let mut prev_dist = self.distance(lower, upper);
        let mut sum_frac = 0.0;
        for i in (0..=lower_index).rev() {
            let dist = self.distance(&hist_lower[i], upper);
            let frac = 1.0 - length_hist_frac(lengths, prev_dist, dist, false);
            sum_frac += frac * bin_width / (n - 1) as f64;
            bin_width = 1.0;
            prev_dist = dist;
        }
        sum_frac
    }
}

/// length_hist_bsearch.
fn length_bsearch(hist: &[f64], value: f64, equal: bool) -> isize {
    let (mut lower, mut upper) = (-1isize, hist.len() as isize - 1);
    while lower < upper {
        let middle = (lower + upper + 1) / 2;
        let m = hist[middle as usize];
        if m < value || (equal && m <= value) {
            lower = middle;
        } else {
            upper = middle - 1;
        }
    }
    lower
}

/// get_len_position.
fn len_position(value: f64, hist1: f64, hist2: f64) -> f64 {
    match (hist1.is_infinite(), hist2.is_infinite()) {
        (false, false) => {
            if value.is_infinite() {
                0.5
            } else {
                1.0 - (hist2 - value) / (hist2 - hist1)
            }
        }
        (true, false) => 1.0,
        (true, true) => 0.0,
        (false, true) => 0.5,
    }
}

/// calc_length_hist_frac: the fraction of ranges with lengths between `length1` and
/// `length2` (the average of the length histogram's cumulative fraction over them).
fn length_hist_frac(hist: &[f64], length1: f64, length2: f64, equal: bool) -> f64 {
    let n = hist.len();
    if length2 < 0.0 {
        return 0.0;
    }
    if length2.is_infinite() && equal {
        return 1.0;
    }
    let mut i = length_bsearch(hist, length1, equal);
    if i >= n as isize - 1 {
        return 1.0;
    }
    let pos = if i < 0 {
        i = 0;
        0.0
    } else {
        len_position(length1, hist[i as usize], hist[i as usize + 1])
    };
    let mut i = i as usize;
    let mut pb = (i as f64 + pos) / (n - 1) as f64;
    let mut b = length1;
    if length2 == length1 {
        return pb;
    }
    let mut area = 0.0;
    while i < n - 1 {
        let bin_upper = hist[i + 1];
        if !(bin_upper < length2 || (equal && bin_upper <= length2)) {
            break;
        }
        let a = b;
        let pa = pb;
        b = bin_upper;
        pb = i as f64 / (n - 1) as f64;
        if pa > 0.0 || pb > 0.0 {
            area += 0.5 * (pb + pa) * (b - a);
        }
        i += 1;
    }
    let a = b;
    let pa = pb;
    b = length2;
    let pos = if i >= n - 1 || hist[i] == hist[i + 1] {
        0.0
    } else {
        len_position(length2, hist[i], hist[i + 1])
    };
    pb = (i as f64 + pos) / (n - 1) as f64;
    if pa > 0.0 || pb > 0.0 {
        area += 0.5 * (pb + pa) * (b - a);
    }
    if area.is_infinite() && length2.is_infinite() { 0.5 } else { area / (length2 - length1) }
}

impl Est<'_> {
    /// rangesel / multirangesel of operator `name` between `l` and `r`: None when neither
    /// side is a range or multirange column (the caller's default).
    pub(super) fn rangesel(&self, name: &str, l: &Expr, r: &Expr) -> Option<f64> {
        let op = Op::of(name)?;
        let (v, c, varonleft) = match (self.var(l), self.constant(r)) {
            (Some(v), Some(c)) => (v, c, true),
            _ => match (self.constant(l), self.var(r)) {
                (Some(c), Some(v)) => (v, c, false),
                _ => return None,
            },
        };
        let (rt, multi) = match v.ty() {
            DataType::Range(rt) => (rt.clone(), false),
            DataType::Multirange(rt) => (rt.clone(), true),
            // (`elem <@ range`: the element is the column; the default.)
            _ => return Some(if op == Op::ContainedBy || op == Op::Contains { DEFAULT_RANGE_INEQ_SEL } else { op.default_sel() }),
        };
        // (A value known only when run.)
        let Some(c) = c else { return Some(op.default_sel()) };
        if c.is_null() {
            return Some(0.0);
        }
        let op = if varonleft {
            op
        } else {
            // (No commutator: default_range_selectivity of operator 0.)
            match op.commute() {
                Some(o) => o,
                None => return Some(0.01),
            }
        };
        // The constant's span: a range of the column's type (a multirange column's: a
        // range or multirange of its range type), or an element `@>` looks for.
        let (op, span): (Op, Option<Range>) = match &c {
            Value::Range(cr) => (op, Some((**cr).clone())),
            Value::Multirange(m) if multi => (
                op,
                Some(match (m.first(), m.last()) {
                    (Some(a), Some(b)) => Range { empty: false, lower: a.lower.clone(), lower_inc: a.lower_inc, upper: b.upper.clone(), upper_inc: b.upper_inc },
                    _ => Range::empty(),
                }),
            ),
            Value::Multirange(_) => (op, None),
            e if op == Op::Contains => (
                Op::ContainsElem,
                Some(Range { empty: false, lower: Some(e.clone()), lower_inc: true, upper: Some(e.clone()), upper_inc: true }),
            ),
            _ => (op, None),
        };
        let _g = crate::types::range::under(rt.coll());
        let Some(span) = span else { return Some(op.default_sel().clamp(0.0, 1.0)) };
        // (multirangesel reads the length histogram for `@>` and for `<@` of a multirange,
        // not for a multirange `<@` a range (OID_RANGE_MULTIRANGE_CONTAINED_OP isn't in its
        // list): with none, every length fraction is 1.)
        let lengths = !(multi && op == Op::ContainedBy && matches!(c, Value::Range(_)));
        let st = v.stats(self.cat);
        let null_frac = st.map_or(0.0, |s| s.nullfrac as f64);
        let empty_frac = st
            .and_then(|s| slot(s, KIND_RANGE_LENGTH, self.fmt))
            .and_then(|(_, nums)| nums.first().copied())
            .unwrap_or(0.0);
        let selec = if span.empty {
            match op {
                Op::Overlap | Op::OverLeft | Op::OverRight | Op::Left | Op::Right | Op::Lt => 0.0,
                Op::ContainedBy | Op::Le => empty_frac,
                Op::Contains | Op::Ge => 1.0,
                Op::Gt => 1.0 - empty_frac,
                Op::ContainsElem => 0.0,
            }
        } else {
            let mut hist_selec = st.map_or(-1.0, |s| self.hist_selectivity(s, &rt, &span, op, lengths));
            if hist_selec < 0.0 {
                hist_selec = op.default_sel();
            }
            if op == Op::ContainedBy {
                (1.0 - empty_frac) * hist_selec + empty_frac
            } else {
                (1.0 - empty_frac) * hist_selec
            }
        };
        Some((selec * (1.0 - null_frac)).clamp(0.0, 1.0))
    }

    /// calc_hist_selectivity: -1 without the histograms it needs.
    fn hist_selectivity(&self, st: &crate::catalog::ColStats, rt: &RangeType, c: &Range, op: Op, with_lengths: bool) -> f64 {
        let Some((vals, _)) = slot(st, KIND_BOUNDS, self.fmt) else { return -1.0 };
        if vals.len() < 2 {
            return -1.0;
        }
        let mut hist_lower = Vec::with_capacity(vals.len());
        let mut hist_upper = Vec::with_capacity(vals.len());
        for v in vals.iter() {
            let Value::Range(r) = v else { return -1.0 };
            if r.empty {
                return -1.0;
            }
            hist_lower.push(r.lower_bound());
            hist_upper.push(r.upper_bound());
        }
        let lengths: Vec<f64> = if with_lengths && matches!(op, Op::Contains | Op::ContainedBy) {
            let Some((lv, _)) = slot(st, KIND_RANGE_LENGTH, self.fmt) else { return -1.0 };
            if lv.len() < 2 {
                return -1.0;
            }
            lv.iter().map(|x| x.as_f64().unwrap_or(f64::INFINITY)).collect()
        } else {
            Vec::new()
        };
        let h = Hist { rt };
        let (cl, cu) = (c.lower_bound(), c.upper_bound());
        match op {
            Op::Lt => h.scalar(&cl, &hist_lower, false),
            Op::Le => h.scalar(&cl, &hist_lower, true),
            Op::Gt => 1.0 - h.scalar(&cl, &hist_lower, false),
            Op::Ge => 1.0 - h.scalar(&cl, &hist_lower, true),
            Op::Left => h.scalar(&cl, &hist_upper, false),
            Op::Right => 1.0 - h.scalar(&cu, &hist_lower, true),
            Op::OverRight => 1.0 - h.scalar(&cl, &hist_lower, false),
            Op::OverLeft => h.scalar(&cu, &hist_upper, true),
            Op::Overlap | Op::ContainsElem => {
                let s = h.scalar(&cl, &hist_upper, false) + (1.0 - h.scalar(&cu, &hist_lower, true));
                1.0 - s
            }
            Op::Contains => h.contains(&cl, &cu, &hist_lower, &lengths),
            Op::ContainedBy => {
                if cl.infinite() {
                    h.scalar(&cu, &hist_upper, true)
                } else if cu.infinite() {
                    1.0 - h.scalar(&cl, &hist_lower, false)
                } else {
                    h.contained(&cl, &cu, &hist_lower, &lengths)
                }
            }
        }
    }
}
