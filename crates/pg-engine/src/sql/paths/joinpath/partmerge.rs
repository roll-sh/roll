//! partition_bounds_merge (partbounds.c): two partitioned join inputs whose partition
//! bounds differ can still be joined partition by partition when each partition of one
//! side overlaps at most one of the other's (list values matching, ranges overlapping, the
//! default and NULL partitions taking what the others don't). The pairs of partitions to
//! join, in the merged partitions' order (a side's partition None: an empty one there,
//! for an outer join), and the merged partitions' bounds.

use crate::catalog::PartStrategy;
use crate::catalog::partition::{Datum, PartBound, RangeDatum, key_cmp};
use crate::sql::joins::Jt;
use crate::types::Value;
use std::cmp::Ordering;

/// One side: its partitions' bounds (in bound order) and which are not empty.
pub(super) struct Side<'a> {
    pub bounds: &'a [PartBound],
    pub live: Vec<bool>,
}

/// The result: (outer partition, inner partition) per merged partition, and its bounds.
pub(super) type Merged = (Vec<(Option<usize>, Option<usize>)>, Vec<PartBound>);

struct Map {
    merged_indexes: Vec<i64>,
    merged: Vec<bool>,
    did_remapping: bool,
    old_indexes: Vec<i64>,
}

impl Map {
    fn new(n: usize) -> Map {
        Map {
            merged_indexes: vec![-1; n],
            merged: vec![false; n],
            did_remapping: false,
            old_indexes: vec![-1; n],
        }
    }
}

fn is_outer(jt: Jt) -> bool {
    matches!(jt, Jt::Left | Jt::Full | Jt::Anti)
}

/// partition_bounds_merge for `jt` (Inner, Left, Full, Semi or Anti; outer first).
pub(super) fn merge(strategy: PartStrategy, outer: &Side, inner: &Side, jt: Jt) -> Option<Merged> {
    match strategy {
        PartStrategy::List => merge_list(outer, inner, jt),
        PartStrategy::Range => merge_range(outer, inner, jt),
        _ => None,
    }
}

fn merge_matching(om: &mut Map, im: &mut Map, oi: usize, ii: usize, next: &mut i64) -> i64 {
    let (omi, imi) = (om.merged_indexes[oi], im.merged_indexes[ii]);
    let (omg, img) = (om.merged[oi], im.merged[ii]);
    if omi >= 0 && imi >= 0 {
        if omi == imi {
            return omi;
        }
        if !omg && !img {
            if omi < imi {
                om.merged[oi] = true;
                im.merged_indexes[ii] = omi;
                im.merged[ii] = true;
                im.did_remapping = true;
                im.old_indexes[ii] = imi;
                return omi;
            }
            im.merged[ii] = true;
            om.merged_indexes[oi] = imi;
            om.merged[oi] = true;
            om.did_remapping = true;
            om.old_indexes[oi] = omi;
            return imi;
        }
        return -1;
    }
    if omi == -1 && imi == -1 {
        let m = *next;
        om.merged_indexes[oi] = m;
        om.merged[oi] = true;
        im.merged_indexes[ii] = m;
        im.merged[ii] = true;
        *next += 1;
        return m;
    }
    if omi >= 0 && !om.merged[oi] {
        im.merged_indexes[ii] = omi;
        im.merged[ii] = true;
        om.merged[oi] = true;
        return omi;
    }
    if imi >= 0 && !im.merged[ii] {
        om.merged_indexes[oi] = imi;
        om.merged[oi] = true;
        im.merged[ii] = true;
        return imi;
    }
    -1
}

fn with_dummy(map: &mut Map, index: usize, next: &mut i64) -> i64 {
    let m = *next;
    map.merged_indexes[index] = m;
    *next += 1;
    m
}

#[allow(clippy::too_many_arguments)]
fn process_outer(
    om: &mut Map,
    im: &mut Map,
    outer_has_default: bool,
    inner_default: Option<usize>,
    oi: usize,
    jt: Jt,
    next: &mut i64,
    default_index: &mut i64,
) -> i64 {
    if let Some(idf) = inner_default {
        if outer_has_default {
            return -1;
        }
        let m = merge_matching(om, im, oi, idf, next);
        if m == -1 {
            return -1;
        }
        if jt == Jt::Full && *default_index == -1 {
            *default_index = m;
        }
        m
    } else {
        let m = om.merged_indexes[oi];
        if m == -1 { with_dummy(om, oi, next) } else { m }
    }
}

#[allow(clippy::too_many_arguments)]
fn process_inner(
    om: &mut Map,
    im: &mut Map,
    inner_has_default: bool,
    outer_default: Option<usize>,
    ii: usize,
    jt: Jt,
    next: &mut i64,
    default_index: &mut i64,
) -> i64 {
    if let Some(odf) = outer_default {
        if inner_has_default {
            return -1;
        }
        let m = merge_matching(om, im, odf, ii, next);
        if m == -1 {
            return -1;
        }
        if is_outer(jt) && *default_index == -1 {
            *default_index = m;
        }
        m
    } else {
        let m = im.merged_indexes[ii];
        if m == -1 { with_dummy(im, ii, next) } else { m }
    }
}

#[allow(clippy::too_many_arguments)]
fn merge_defaults(
    om: &mut Map,
    im: &mut Map,
    outer_default: Option<usize>,
    inner_default: Option<usize>,
    jt: Jt,
    next: &mut i64,
    default_index: &mut i64,
) {
    match (outer_default, inner_default) {
        (Some(od), None) => {
            if is_outer(jt) && om.merged_indexes[od] == -1 {
                *default_index = with_dummy(om, od, next);
            }
        }
        (None, Some(idf)) => {
            if jt == Jt::Full && im.merged_indexes[idf] == -1 {
                *default_index = with_dummy(im, idf, next);
            }
        }
        (Some(od), Some(idf)) => {
            *default_index = merge_matching(om, im, od, idf, next);
        }
        (None, None) => {}
    }
}

/// The pairs, by merged index (generate_matching_part_pairs).
fn pairs(om: &Map, im: &Map, nmerged: i64) -> Vec<(Option<usize>, Option<usize>)> {
    let n = nmerged as usize;
    let mut oidx = vec![None; n];
    let mut iidx = vec![None; n];
    for (i, &m) in om.merged_indexes.iter().enumerate() {
        if m >= 0 {
            oidx[m as usize] = Some(i);
        }
    }
    for (i, &m) in im.merged_indexes.iter().enumerate() {
        if m >= 0 {
            iidx[m as usize] = Some(i);
        }
    }
    (0..n)
        .map(|m| (oidx[m], iidx[m]))
        .collect()
}

fn fix_merged_indexes(om: &Map, im: &Map, nmerged: i64, merged_indexes: &mut [i64]) {
    let mut new = vec![-1i64; nmerged as usize];
    for map in [om, im] {
        if map.did_remapping {
            for (i, &old) in map.old_indexes.iter().enumerate() {
                if old >= 0 {
                    new[old as usize] = map.merged_indexes[i];
                }
            }
        }
    }
    for m in merged_indexes.iter_mut() {
        if new[*m as usize] >= 0 {
            *m = new[*m as usize];
        }
    }
}

fn merge_list(outer: &Side, inner: &Side, jt: Jt) -> Option<Merged> {
    // The non-null values (sorted) with their partitions; the NULL and default ones.
    let flatten = |s: &Side| {
        let mut vals: Vec<(Value, usize)> = Vec::new();
        let mut null = None;
        let mut default = None;
        for (i, b) in s.bounds.iter().enumerate() {
            match b {
                PartBound::List(v) => {
                    for d in v {
                        if d.0.is_null() {
                            null = Some(i);
                        } else {
                            vals.push((d.0.clone(), i));
                        }
                    }
                }
                PartBound::Default => default = Some(i),
                _ => return None,
            }
        }
        vals.sort_by(|a, b| key_cmp(0, &a.0, &b.0));
        Some((vals, null, default))
    };
    let (ovals, onull, odef) = flatten(outer)?;
    let (ivals, inull, idef) = flatten(inner)?;
    let odef = odef.filter(|&d| outer.live[d]);
    let idef = idef.filter(|&d| inner.live[d]);
    let mut om = Map::new(outer.bounds.len());
    let mut im = Map::new(inner.bounds.len());
    let mut next = 0i64;
    let mut default_index = -1i64;
    let mut null_index = -1i64;
    let mut merged_vals: Vec<Value> = Vec::new();
    let mut merged_indexes: Vec<i64> = Vec::new();
    let (mut op, mut ip) = (0usize, 0usize);
    while op < ovals.len() || ip < ivals.len() {
        if op < ovals.len() && !outer.live[ovals[op].1] {
            op += 1;
            continue;
        }
        if ip < ivals.len() && !inner.live[ivals[ip].1] {
            ip += 1;
            continue;
        }
        let cmp = if op >= ovals.len() {
            Ordering::Greater
        } else if ip >= ivals.len() {
            Ordering::Less
        } else {
            key_cmp(0, &ovals[op].0, &ivals[ip].0)
        };
        let mut merged = -1i64;
        let mut val = None;
        match cmp {
            Ordering::Equal => {
                merged = merge_matching(&mut om, &mut im, ovals[op].1, ivals[ip].1, &mut next);
                if merged == -1 {
                    return None;
                }
                val = Some(ovals[op].0.clone());
                op += 1;
                ip += 1;
            }
            Ordering::Less => {
                if idef.is_some() || is_outer(jt) {
                    merged = process_outer(
                        &mut om,
                        &mut im,
                        odef.is_some(),
                        idef,
                        ovals[op].1,
                        jt,
                        &mut next,
                        &mut default_index,
                    );
                    if merged == -1 {
                        return None;
                    }
                    val = Some(ovals[op].0.clone());
                }
                op += 1;
            }
            Ordering::Greater => {
                if odef.is_some() || jt == Jt::Full {
                    merged = process_inner(
                        &mut om,
                        &mut im,
                        idef.is_some(),
                        odef,
                        ivals[ip].1,
                        jt,
                        &mut next,
                        &mut default_index,
                    );
                    if merged == -1 {
                        return None;
                    }
                    val = Some(ivals[ip].0.clone());
                }
                ip += 1;
            }
        }
        if merged >= 0 && merged != default_index {
            merged_vals.push(val.unwrap());
            merged_indexes.push(merged);
        }
    }
    // The NULL partitions.
    let onull = onull.filter(|&n| outer.live[n]);
    let inull = inull.filter(|&n| inner.live[n]);
    if onull.is_some() || inull.is_some() {
        let co = onull.is_some_and(|n| om.merged_indexes[n] == -1);
        let ci = inull.is_some_and(|n| im.merged_indexes[n] == -1);
        match (co, ci) {
            (true, false) => {
                if is_outer(jt) {
                    null_index = with_dummy(&mut om, onull.unwrap(), &mut next);
                }
            }
            (false, true) => {
                if jt == Jt::Full {
                    null_index = with_dummy(&mut im, inull.unwrap(), &mut next);
                }
            }
            (true, true) => {
                if is_outer(jt) {
                    null_index =
                        merge_matching(&mut om, &mut im, onull.unwrap(), inull.unwrap(), &mut next);
                }
            }
            (false, false) => {}
        }
    }
    if odef.is_some() || idef.is_some() {
        merge_defaults(&mut om, &mut im, odef, idef, jt, &mut next, &mut default_index);
    }
    if next == 0 {
        return None;
    }
    if om.did_remapping || im.did_remapping {
        fix_merged_indexes(&om, &im, next, &mut merged_indexes);
    }
    let ps = pairs(&om, &im, next);
    let mut bounds: Vec<Vec<Datum>> = vec![Vec::new(); next as usize];
    for (v, m) in merged_vals.into_iter().zip(&merged_indexes) {
        bounds[*m as usize].push(Datum(v));
    }
    if null_index >= 0 {
        bounds[null_index as usize].push(Datum(Value::Null));
    }
    let mut out = Vec::new();
    let mut out_bounds = Vec::new();
    for (m, p) in ps.into_iter().enumerate() {
        if p.0.is_none() && p.1.is_none() {
            continue;
        }
        out.push(p);
        out_bounds.push(if m as i64 == default_index {
            PartBound::Default
        } else {
            PartBound::List(std::mem::take(&mut bounds[m]))
        });
    }
    Some((out, out_bounds))
}

/// partition_rbound_cmp of two range bounds (the bound of a partition, and whether it is
/// its lower one): an exclusive upper bound is below an inclusive lower one of the same
/// values.
fn rbound_cmp(a: &[RangeDatum], alower: bool, b: &[RangeDatum], blower: bool) -> Ordering {
    let kind = |d: &RangeDatum| match d {
        RangeDatum::MinValue => -1,
        RangeDatum::Value(_) => 0,
        RangeDatum::MaxValue => 1,
    };
    let mut c = Ordering::Equal;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let (kx, ky) = (kind(x), kind(y));
        if kx != ky {
            c = kx.cmp(&ky);
            break;
        }
        if kx != 0 {
            break;
        }
        if let (RangeDatum::Value(p), RangeDatum::Value(q)) = (x, y) {
            c = key_cmp(i, &p.0, &q.0);
            if c != Ordering::Equal {
                break;
            }
        }
    }
    if c == Ordering::Equal && alower != blower {
        c = if alower { Ordering::Greater } else { Ordering::Less };
    }
    c
}

/// A range-partitioned side's live partitions in order (index, lower and upper bound), and
/// its default partition.
type Ranges<'b> = (Vec<(usize, &'b [RangeDatum], &'b [RangeDatum])>, Option<usize>);

fn ranges<'b>(s: &Side<'b>) -> Option<Ranges<'b>> {
    let mut v = Vec::new();
    let mut def = None;
    for (i, b) in s.bounds.iter().enumerate() {
        match b {
            PartBound::Range { from, to } => {
                if s.live[i] {
                    v.push((i, from.as_slice(), to.as_slice()));
                }
            }
            PartBound::Default => def = Some(i),
            _ => return None,
        }
    }
    Some((v, def))
}

fn merge_range(outer: &Side, inner: &Side, jt: Jt) -> Option<Merged> {
    let (orng, odef) = ranges(outer)?;
    let (irng, idef) = ranges(inner)?;
    let odef = odef.filter(|&d| outer.live[d]);
    let idef = idef.filter(|&d| inner.live[d]);
    let mut om = Map::new(outer.bounds.len());
    let mut im = Map::new(inner.bounds.len());
    let mut next = 0i64;
    let mut default_index = -1i64;
    let mut merged_bounds: Vec<(i64, Vec<RangeDatum>, Vec<RangeDatum>)> = Vec::new();
    let (mut op, mut ip) = (0usize, 0usize);
    while op < orng.len() || ip < irng.len() {
        let (overlap, lb_cmp, ub_cmp) = if op >= orng.len() {
            (false, Ordering::Greater, Ordering::Greater)
        } else if ip >= irng.len() {
            (false, Ordering::Less, Ordering::Less)
        } else {
            let (_, olb, oub) = orng[op];
            let (_, ilb, iub) = irng[ip];
            if rbound_cmp(oub, false, ilb, true) == Ordering::Less {
                (false, Ordering::Less, Ordering::Less)
            } else if rbound_cmp(olb, true, iub, false) == Ordering::Greater {
                (false, Ordering::Greater, Ordering::Greater)
            } else {
                (
                    true,
                    rbound_cmp(olb, true, ilb, true),
                    rbound_cmp(oub, false, iub, false),
                )
            }
        };
        let mut merged = -1i64;
        let mut mb: Option<(Vec<RangeDatum>, Vec<RangeDatum>)> = None;
        if overlap {
            let (oi, olb, oub) = orng[op];
            let (ii, ilb, iub) = irng[ip];
            merged = merge_matching(&mut om, &mut im, oi, ii, &mut next);
            if merged == -1 {
                return None;
            }
            let (lb, ub) = match jt {
                Jt::Inner | Jt::Semi => (
                    if lb_cmp == Ordering::Greater { olb } else { ilb },
                    if ub_cmp == Ordering::Less { oub } else { iub },
                ),
                Jt::Left | Jt::Anti => (olb, oub),
                _ => (
                    if lb_cmp == Ordering::Less { olb } else { ilb },
                    if ub_cmp == Ordering::Greater { oub } else { iub },
                ),
            };
            mb = Some((lb.to_vec(), ub.to_vec()));
            op += 1;
            ip += 1;
            // (A partition overlapping the next one of the other side: two matches.)
            if ub_cmp == Ordering::Greater
                && ip < irng.len()
                && rbound_cmp(oub, false, irng[ip].1, true) == Ordering::Greater
            {
                return None;
            }
            if ub_cmp == Ordering::Less
                && op < orng.len()
                && rbound_cmp(orng[op].1, true, iub, false) == Ordering::Less
            {
                return None;
            }
            if (odef.is_some() && (lb_cmp == Ordering::Greater || ub_cmp == Ordering::Less))
                || (idef.is_some() && (lb_cmp == Ordering::Less || ub_cmp == Ordering::Greater))
            {
                return None;
            }
        } else if ub_cmp == Ordering::Less {
            let (oi, olb, oub) = orng[op];
            if idef.is_some() || is_outer(jt) {
                merged = process_outer(
                    &mut om,
                    &mut im,
                    odef.is_some(),
                    idef,
                    oi,
                    jt,
                    &mut next,
                    &mut default_index,
                );
                if merged == -1 {
                    return None;
                }
                mb = Some((olb.to_vec(), oub.to_vec()));
            }
            op += 1;
        } else {
            let (ii, ilb, iub) = irng[ip];
            if odef.is_some() || jt == Jt::Full {
                merged = process_inner(
                    &mut om,
                    &mut im,
                    idef.is_some(),
                    odef,
                    ii,
                    jt,
                    &mut next,
                    &mut default_index,
                );
                if merged == -1 {
                    return None;
                }
                mb = Some((ilb.to_vec(), iub.to_vec()));
            }
            ip += 1;
        }
        if merged >= 0
            && merged != default_index
            && let Some((lb, ub)) = mb
        {
            merged_bounds.push((merged, lb, ub));
        }
    }
    if odef.is_some() || idef.is_some() {
        merge_defaults(&mut om, &mut im, odef, idef, jt, &mut next, &mut default_index);
    }
    if next == 0 {
        return None;
    }
    let ps = pairs(&om, &im, next);
    let mut out = Vec::new();
    let mut out_bounds = Vec::new();
    for (m, p) in ps.into_iter().enumerate() {
        if p.0.is_none() && p.1.is_none() {
            continue;
        }
        out.push(p);
        out_bounds.push(if m as i64 == default_index {
            PartBound::Default
        } else {
            match merged_bounds.iter().find(|(i, _, _)| *i == m as i64) {
                Some((_, lb, ub)) => PartBound::Range {
                    from: lb.clone(),
                    to: ub.clone(),
                },
                None => PartBound::Default,
            }
        });
    }
    Some((out, out_bounds))
}
