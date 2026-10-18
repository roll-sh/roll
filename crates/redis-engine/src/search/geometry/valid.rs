//! `bg::is_valid` with its failure messages for the module's points and clockwise closed
//! polygons: per ring the coordinate, size, topological dimension, closure, spike and
//! orientation checks, then the polygon's self turns (Boost's sectionalized segment pairs and
//! turn classification, stopping at the first unacceptable turn and reporting the first turn
//! found), holes outside or nested, and a disconnected interior.

use super::algo::{self, distance_measure, equals, side, Loc};
use super::{Cs, Geom, Poly, Pt, Rect};
use crate::util::fmt_g_prec;

fn dsv(p: Pt) -> String {
    format!("({}, {})", fmt_g_prec(p.x, 6), fmt_g_prec(p.y, 6))
}

const INVALID_COORD: &str = "Geometry has point(s) with invalid coordinate(s)";

/// `Ok` when valid, else the failure message.
pub fn is_valid(g: &Geom, cs: Cs) -> Result<(), String> {
    match g {
        Geom::Point(p) => {
            if !p.x.is_finite() || !p.y.is_finite() {
                return Err(INVALID_COORD.into());
            }
            Ok(())
        }
        Geom::Poly(p) => valid_polygon(p, cs),
    }
}

fn valid_ring(r: &[Pt], cs: Cs, interior: bool) -> Result<(), String> {
    if r.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
        return Err(INVALID_COORD.into());
    }
    if r.len() < 4 {
        return Err("Geometry has too few points".into());
    }
    // `num_distinct_consecutive_points`, up to 4.
    let mut distinct = 1;
    for w in r.windows(2) {
        if !equals(cs, w[0], w[1]) {
            distinct += 1;
            if distinct >= 4 {
                break;
            }
        }
    }
    if distinct < 4 {
        return Err("Geometry has wrong topological dimension".into());
    }
    if !equals(cs, r[0], r[r.len() - 1]) {
        return Err("Geometry is defined as closed but is open".into());
    }
    if let Some(apex) = spike(r, cs) {
        return Err(format!("Geometry has spikes. A spike point was found with apex at {}", dsv(apex)));
    }
    let area = algo::ring_area(cs, r);
    if if interior { area < 0.0 } else { area > 0.0 } {
        Ok(())
    } else {
        Err("Geometry has wrong orientation".into())
    }
}

/// `has_spikes` on a closed ring: the apex of the first spike, the closing point included.
fn spike(r: &[Pt], cs: Cs) -> Option<Pt> {
    let diff = |from: usize| -> usize {
        let f = r[from];
        (from + 1..r.len()).find(|&i| !equals(cs, r[i], f)).unwrap_or(r.len())
    };
    let mut prev = 0;
    let mut cur = diff(prev);
    if cur == r.len() {
        return None;
    }
    let mut next = diff(cur);
    if next == r.len() {
        return None;
    }
    while next != r.len() {
        if algo::is_spike_or_equal(cs, r[next], r[cur], r[prev]) {
            return Some(r[cur]);
        }
        prev = cur;
        cur = next;
        next = diff(cur);
    }
    if equals(cs, r[0], r[r.len() - 1]) {
        // `apply_at_closure`: the last point differing from the front, from the back.
        let front = r[r.len() - 1];
        let p = (0..r.len() - 1).rev().find(|&i| !equals(cs, r[i], front)).map(|i| r[i]).unwrap_or(r[0]);
        let n = diff(0);
        let n = if n == r.len() { r[r.len() - 1] } else { r[n] };
        if algo::is_spike_or_equal(cs, n, r[0], p) {
            return Some(r[0]);
        }
    }
    None
}

fn valid_polygon(p: &Poly, cs: Cs) -> Result<(), String> {
    valid_ring(&p.outer.pts, cs, false)?;
    for h in &p.inners {
        valid_ring(&h.pts, cs, true)?;
    }
    let (turns, bad) = self_turns(p, cs);
    if bad {
        let t = &turns[0];
        return Err(format!(
            "Geometry has invalid self-intersections. A self-intersection point was found at {}; method: {}; operations: {}/{}; segment IDs {{source, multi, ring, segment}}: {{0, -1, {}, {}}}/{{0, -1, {}, {}}}",
            dsv(t.pt),
            t.method as char,
            t.ops[0] as char,
            t.ops[1] as char,
            t.seg[0].0,
            t.seg[0].1,
            t.seg[1].0,
            t.seg[1].1
        ));
    }
    holes_inside(p, cs, &turns)?;
    connected_interior(p, &turns)
}

// ---------------------------------------------------------------------------------------------
// Holes and connectivity

fn holes_inside(p: &Poly, cs: Cs, turns: &[Turn]) -> Result<(), String> {
    let mut with_ext = std::collections::BTreeSet::new();
    for t in turns {
        if t.seg[0].0 == -1 {
            with_ext.insert(t.seg[1].0);
        } else if t.seg[1].0 == -1 {
            with_ext.insert(t.seg[0].0);
        }
    }
    for (i, h) in p.inners.iter().enumerate() {
        if !with_ext.contains(&(i as i32)) && algo::ring_loc(cs, &p.outer.pts, h.pts[0]) == Loc::Outside {
            return Err("Geometry has interior rings defined outside the outer boundary".into());
        }
    }
    let mut touched = with_ext;
    for t in turns {
        touched.insert(t.seg[0].0);
        touched.insert(t.seg[1].0);
    }
    let free: Vec<&[Pt]> =
        p.inners.iter().enumerate().filter(|(i, _)| !touched.contains(&(*i as i32))).map(|(_, h)| h.pts.as_slice()).collect();
    // Rings without turns overlap when their interiors meet: one inside the other (or equal).
    let inside = |a: &[Pt], b: &[Pt]| -> bool {
        let mut all_boundary = true;
        for &v in a {
            match algo::ring_loc(cs, b, v) {
                Loc::Inside => return true,
                Loc::Outside => all_boundary = false,
                Loc::Boundary => {}
            }
        }
        all_boundary
    };
    for i in 0..free.len() {
        for j in i + 1..free.len() {
            if inside(free[i], free[j]) || inside(free[j], free[i]) {
                return Err("Geometry has nested interior rings".into());
            }
        }
    }
    Ok(())
}

/// The complement graph of rings and turn points: a cycle disconnects the interior.
fn connected_interior(p: &Poly, turns: &[Turn]) -> Result<(), String> {
    let nrings = p.inners.len() + 1;
    let mut ips: Vec<Pt> = Vec::new();
    let mut nb: Vec<std::collections::BTreeSet<usize>> = vec![Default::default(); nrings];
    for t in turns {
        let v1 = (t.seg[0].0 + 1) as usize;
        let v2 = (t.seg[1].0 + 1) as usize;
        let vip = match ips.iter().position(|q| !less(*q, t.pt) && !less(t.pt, *q)) {
            Some(i) => nrings + i,
            None => {
                ips.push(t.pt);
                nb.push(Default::default());
                nrings + ips.len() - 1
            }
        };
        for v in [v1, v2] {
            nb[v].insert(vip);
            nb[vip].insert(v);
        }
    }
    // Vertices in the set's order: rings by id, then turn points by coordinates.
    let mut order: Vec<usize> = (0..nrings).collect();
    let mut ipo: Vec<usize> = (0..ips.len()).collect();
    ipo.sort_by(|&a, &b| {
        if less(ips[a], ips[b]) {
            std::cmp::Ordering::Less
        } else if less(ips[b], ips[a]) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    order.extend(ipo.into_iter().map(|i| nrings + i));
    let n = nb.len();
    let mut visited = vec![false; n];
    let mut parent = vec![usize::MAX; n];
    for &start in &order {
        if visited[start] {
            continue;
        }
        let mut stack = vec![start];
        while let Some(v) = stack.pop() {
            visited[v] = true;
            for &w in &nb[v] {
                if w != parent[v] {
                    if visited[w] {
                        return Err("Geometry has disconnected interior".into());
                    }
                    parent[w] = v;
                    stack.push(w);
                }
            }
        }
    }
    Ok(())
}

fn less(a: Pt, b: Pt) -> bool {
    if !algo::feq(a.x, b.x) {
        return a.x < b.x;
    }
    if !algo::feq(a.y, b.y) {
        return a.y < b.y;
    }
    false
}

// ---------------------------------------------------------------------------------------------
// Self turns

pub struct Turn {
    pub pt: Pt,
    pub method: u8,
    pub ops: [u8; 2],
    /// (ring index, -1 for the exterior; segment index).
    pub seg: [(i32, usize); 2],
}

struct Section {
    ring: i32,
    begin: usize,
    end: usize,
    dirs: [i32; 2],
    duplicate: bool,
    ndi: usize,
    range_count: usize,
    bbox: Rect,
    count: usize,
}

fn inverse_box() -> Rect {
    Rect { min: Pt { x: f64::MAX, y: f64::MAX }, max: Pt { x: f64::MIN, y: f64::MIN } }
}

fn expand_pt(b: &mut Rect, p: Pt) {
    b.min.x = b.min.x.min(p.x);
    b.min.y = b.min.y.min(p.y);
    b.max.x = b.max.x.max(p.x);
    b.max.y = b.max.y.max(p.y);
}

/// `sectionalize` in both dimensions: monotonic runs of at most 11 segments, duplicate
/// segments in their own sections; boxes enlarged by a scaled epsilon.
fn sectionalize(rings: &[&[Pt]], cs: Cs) -> Vec<Section> {
    let mut out = Vec::new();
    for (ri, r) in rings.iter().enumerate() {
        let ring = ri as i32 - 1;
        if r.len() < 2 {
            continue;
        }
        let mut cur: Option<Section> = None;
        let mut ndi = 0;
        for i in 0..r.len() - 1 {
            let (a, b) = (r[i], r[i + 1]);
            let dx = match cs {
                Cs::Flat => b.x - a.x,
                Cs::Sphere => algo::norm_lon(b.x - a.x),
            };
            let mut dirs = [sign(dx), if b.y > a.y { 1 } else if b.y < a.y { -1 } else { 0 }];
            let mut duplicate = false;
            if dirs[0] == 0 && algo::feq(a.x, b.x) && algo::feq(a.y, b.y) {
                duplicate = true;
                dirs = [-99, -99];
            }
            if let Some(s) = &cur
                && (s.dirs != dirs || s.count > 10)
            {
                out.push(cur.take().expect("section"));
            }
            match &mut cur {
                None => {
                    let mut bbox = inverse_box();
                    match cs {
                        Cs::Flat => {
                            expand_pt(&mut bbox, a);
                            expand_pt(&mut bbox, b);
                        }
                        Cs::Sphere => bbox = algo::segment_envelope(a, b),
                    }
                    cur = Some(Section { ring, begin: i, end: i + 1, dirs, duplicate, ndi, range_count: r.len(), bbox, count: 1 });
                }
                Some(s) => {
                    match cs {
                        Cs::Flat => expand_pt(&mut s.bbox, b),
                        Cs::Sphere => algo::expand(cs, &mut s.bbox, &algo::segment_envelope(a, b)),
                    }
                    s.end = i + 1;
                    s.count += 1;
                }
            }
            if !duplicate {
                ndi += 1;
            }
        }
        if let Some(s) = cur {
            out.push(s);
        }
    }
    let eps = f64::EPSILON * 1000.0;
    for s in &mut out {
        let b = &mut s.bbox;
        b.min.x -= b.min.x.abs().max(1.0) * eps;
        b.min.y -= b.min.y.abs().max(1.0) * eps;
        b.max.x += b.max.x.abs().max(1.0) * eps;
        b.max.y += b.max.y.abs().max(1.0) * eps;
    }
    out
}

fn sign(v: f64) -> i32 {
    if v > 0.0 {
        1
    } else if v < 0.0 {
        -1
    } else {
        0
    }
}

/// The polygon's self turns, and whether an unacceptable one stopped the search.
pub fn self_turns(p: &Poly, cs: Cs) -> (Vec<Turn>, bool) {
    let rings: Vec<&[Pt]> = std::iter::once(p.outer.pts.as_slice()).chain(p.inners.iter().map(|r| r.pts.as_slice())).collect();
    let secs = sectionalize(&rings, cs);
    let mut turns = Vec::new();
    let mut bad = false;
    let boxes: Vec<Rect> = secs.iter().map(|s| s.bbox).collect();
    let items: Vec<usize> = (0..secs.len()).collect();
    let mut visit = |a: usize, b: usize| -> bool {
        let (s1, s2) = (&secs[a], &secs[b]);
        if algo::box_disjoint(cs, &s1.bbox, &s2.bbox) || s1.duplicate || s2.duplicate {
            return true;
        }
        section_pair(cs, &rings, s1, s2, &mut turns, &mut bad)
    };
    partition(cs, &items, &boxes, &mut visit);
    (turns, bad)
}

/// `get_turns_in_sections`: false when interrupted.
fn section_pair(cs: Cs, rings: &[&[Pt]], s1: &Section, s2: &Section, turns: &mut Vec<Turn>, bad: &mut bool) -> bool {
    let r1 = rings[(s1.ring + 1) as usize];
    let r2 = rings[(s2.ring + 1) as usize];
    for (k1, i1) in (s1.begin..s1.end).enumerate() {
        let ndi1 = s1.ndi + k1;
        for (k2, i2) in (s2.begin..s2.end).enumerate() {
            let ndi2 = s2.ndi + k2;
            if s1.ring == s2.ring {
                let n = s1.range_count;
                if ndi2 == ndi1 + 1 || (i1 == 0 && i2 + 2 >= n) {
                    continue;
                }
            }
            let p = sub(cs, r1, i1);
            let q = sub(cs, r2, i2);
            let before = turns.len();
            turn_info(cs, &p, &q, [(s1.ring, i1), (s2.ring, i2)], turns);
            if turns[before..].iter().any(|t| !acceptable(t)) {
                *bad = true;
                return false;
            }
        }
    }
    true
}

fn acceptable(t: &Turn) -> bool {
    if t.seg[0].0 == t.seg[1].0 {
        return false;
    }
    (t.method == b'm' || t.method == b't') && t.ops[0] == b'i' && t.ops[1] == b'i'
}

/// A segment with the next distinct point after its end (around the ring).
#[derive(Clone, Copy)]
struct Sub {
    i: Pt,
    j: Pt,
    k: Pt,
}

fn sub(cs: Cs, r: &[Pt], i: usize) -> Sub {
    let (a, b) = (r[i], r[i + 1]);
    let n = r.len();
    let mut idx = (i + 2) % n;
    let mut check = 0;
    while equals(cs, b, r[idx]) && check < n {
        idx = (idx + 1) % n;
        check += 1;
    }
    Sub { i: a, j: b, k: r[idx] }
}

// ---------------------------------------------------------------------------------------------
// Partition

fn partition(cs: Cs, items: &[usize], boxes: &[Rect], visit: &mut dyn FnMut(usize, usize) -> bool) -> bool {
    if items.len() > 16 {
        let total = boxes_of(cs, items, boxes);
        one_range(cs, 0, &total, items, 0, boxes, visit)
    } else {
        handle_one(items, visit)
    }
}

fn boxes_of(cs: Cs, items: &[usize], boxes: &[Rect]) -> Rect {
    let mut b = inverse_box();
    let mut first = true;
    for &i in items {
        if first {
            b = boxes[i];
            first = false;
        } else {
            algo::expand(cs, &mut b, &boxes[i]);
        }
    }
    b
}

fn handle_one(items: &[usize], visit: &mut dyn FnMut(usize, usize) -> bool) -> bool {
    for (i, &a) in items.iter().enumerate() {
        for &b in &items[i + 1..] {
            if !visit(a, b) {
                return false;
            }
        }
    }
    true
}

fn handle_two(a: &[usize], b: &[usize], visit: &mut dyn FnMut(usize, usize) -> bool) -> bool {
    for &x in a {
        for &y in b {
            if !visit(x, y) {
                return false;
            }
        }
    }
    true
}

fn divide(dim: usize, b: &Rect) -> (Rect, Rect) {
    let (mut lo, mut hi) = (*b, *b);
    if dim == 0 {
        let mid = (b.min.x + b.max.x) / 2.0;
        lo.max.x = mid;
        hi.min.x = mid;
    } else {
        let mid = (b.min.y + b.max.y) / 2.0;
        lo.max.y = mid;
        hi.min.y = mid;
    }
    (lo, hi)
}

type Subsets = (Vec<usize>, Vec<usize>, Vec<usize>);

fn subsets(cs: Cs, lo: &Rect, hi: &Rect, items: &[usize], boxes: &[Rect]) -> Subsets {
    let (mut l, mut u, mut e) = (Vec::new(), Vec::new(), Vec::new());
    for &i in items {
        let lo_ov = !algo::box_disjoint(cs, lo, &boxes[i]);
        let hi_ov = !algo::box_disjoint(cs, hi, &boxes[i]);
        if lo_ov && hi_ov {
            e.push(i);
        } else if lo_ov {
            l.push(i);
        } else if hi_ov {
            u.push(i);
        }
    }
    (l, u, e)
}

fn ok1(a: &[usize], level: usize) -> bool {
    a.len() >= 16 && level < 100
}

fn one_range(cs: Cs, dim: usize, b: &Rect, items: &[usize], level: usize, boxes: &[Rect], visit: &mut dyn FnMut(usize, usize) -> bool) -> bool {
    let (lo, hi) = divide(dim, b);
    let (lower, upper, exceeding) = subsets(cs, &lo, &hi, items, boxes);
    let next = |bx: &Rect, inp: &[usize], visit: &mut dyn FnMut(usize, usize) -> bool| -> bool {
        if ok1(inp, level) { one_range(cs, 1 - dim, bx, inp, level + 1, boxes, visit) } else { handle_one(inp, visit) }
    };
    let next2 = |bx: &Rect, a: &[usize], c: &[usize], visit: &mut dyn FnMut(usize, usize) -> bool| -> bool {
        if ok1(a, level) && ok1(c, level) {
            two_ranges(cs, 1 - dim, bx, a, c, level + 1, boxes, visit)
        } else {
            handle_two(a, c, visit)
        }
    };
    if !exceeding.is_empty() {
        let eb = boxes_of(cs, &exceeding, boxes);
        if !(next(&eb, &exceeding, visit) && next2(&eb, &exceeding, &lower, visit) && next2(&eb, &exceeding, &upper, visit)) {
            return false;
        }
    }
    next(&lo, &lower, visit) && next(&hi, &upper, visit)
}

#[allow(clippy::too_many_arguments)]
fn two_ranges(
    cs: Cs,
    dim: usize,
    b: &Rect,
    in1: &[usize],
    in2: &[usize],
    level: usize,
    boxes: &[Rect],
    visit: &mut dyn FnMut(usize, usize) -> bool,
) -> bool {
    let (lo, hi) = divide(dim, b);
    let (lower1, upper1, exc1) = subsets(cs, &lo, &hi, in1, boxes);
    let (lower2, upper2, exc2) = subsets(cs, &lo, &hi, in2, boxes);
    let next = |bx: &Rect, a: &[usize], c: &[usize], visit: &mut dyn FnMut(usize, usize) -> bool| -> bool {
        two_ranges(cs, 1 - dim, bx, a, c, level + 1, boxes, visit)
    };
    if !exc1.is_empty() {
        if ok1(&exc1, level) && ok1(&exc2, level) {
            let mut eb = boxes_of(cs, &exc1, boxes);
            for &i in &exc2 {
                algo::expand(cs, &mut eb, &boxes[i]);
            }
            if !next(&eb, &exc1, &exc2, visit) {
                return false;
            }
        } else if !handle_two(&exc1, &exc2, visit) {
            return false;
        }
        if ok1(&lower2, level) && ok1(&upper2, level) && ok1(&exc1, level) {
            let eb = boxes_of(cs, &exc1, boxes);
            if !(next(&eb, &exc1, &lower2, visit) && next(&eb, &exc1, &upper2, visit)) {
                return false;
            }
        } else if !(handle_two(&exc1, &lower2, visit) && handle_two(&exc1, &upper2, visit)) {
            return false;
        }
    }
    if !exc2.is_empty() {
        if ok1(&lower1, level) && ok1(&upper1, level) && ok1(&exc2, level) {
            let eb = boxes_of(cs, &exc2, boxes);
            if !(next(&eb, &lower1, &exc2, visit) && next(&eb, &upper1, &exc2, visit)) {
                return false;
            }
        } else if !(handle_two(&lower1, &exc2, visit) && handle_two(&upper1, &exc2, visit)) {
            return false;
        }
    }
    if ok1(&lower1, level) && ok1(&lower2, level) {
        if !next(&lo, &lower1, &lower2, visit) {
            return false;
        }
    } else if !handle_two(&lower1, &lower2, visit) {
        return false;
    }
    if ok1(&upper1, level) && ok1(&upper2, level) {
        if !next(&hi, &upper1, &upper2, visit) {
            return false;
        }
    } else if !handle_two(&upper1, &upper2, visit) {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------------------------
// Segment intersection and turn classification

struct Inter {
    how: u8,
    opposite: bool,
    arrival: [i32; 2],
    /// [p's points against q, q's points against p].
    sides: [[i32; 2]; 2],
    count: usize,
    ips: [Pt; 2],
    /// (ratio along p, ratio along q) per intersection point.
    fr: [(f64, f64); 2],
}

fn intersect(cs: Cs, p: &Sub, q: &Sub) -> Option<Inter> {
    if cs == Cs::Sphere {
        return intersect_geographic(p, q);
    }
    let (p1, p2, q1, q2) = (p.i, p.j, q.i, q.j);
    let p_point = equals(cs, p1, p2);
    let q_point = equals(cs, q1, q2);
    if p_point || q_point {
        // Duplicate segments are in skipped sections.
        return None;
    }
    if cs == Cs::Flat {
        let disjoint_range = |a1: f64, a2: f64, b1: f64, b2: f64| {
            let (mina, maxa) = if a1 > a2 { (a2, a1) } else { (a1, a2) };
            let (minb, maxb) = if b1 > b2 { (b2, b1) } else { (b1, b2) };
            algo::fsmaller(maxa, minb) || algo::fsmaller(maxb, mina)
        };
        if disjoint_range(p1.x, p2.x, q1.x, q2.x) || disjoint_range(p1.y, p2.y, q1.y, q2.y) {
            return None;
        }
    }
    let mut sides = [[side(cs, q1, q2, p1), side(cs, q1, q2, p2)], [0, 0]];
    if sides[0][0] * sides[0][1] == 1 {
        return None;
    }
    sides[1] = [side(cs, p1, p2, q1), side(cs, p1, p2, q2)];
    if sides[1][0] * sides[1][1] == 1 {
        return None;
    }
    let (dxa, dya, dxb, dyb) = (p2.x - p1.x, p2.y - p1.y, q2.x - q1.x, q2.y - q1.y);
    let mut collinear = sides[0] == [0, 0] && sides[1] == [0, 0];
    let (mut ra, mut rb) = ((0.0, 1.0), (0.0, 1.0));
    if !collinear {
        let det = |a: f64, b: f64, c: f64, d: f64| a * d - b * c;
        let (wx, wy) = (p1.x - q1.x, p1.y - q1.y);
        let na = det(dxb, dyb, wx, wy);
        let da = det(dxa, dya, dxb, dyb);
        let nb = det(dxa, dya, -wx, -wy);
        let db = det(dxb, dyb, dxa, dya);
        let factor = dxa.abs().max(dya.abs()).max(dxb.abs()).max(dyb.abs()).max(1.0);
        let zero = |v: f64| v == 0.0 || (v.is_finite() && v.abs() <= f64::EPSILON * factor);
        if zero(da) || zero(db) {
            sides = [[0, 0], [0, 0]];
            collinear = true;
        } else {
            ra = if da < 0.0 { (-na, -da) } else { (na, da) };
            rb = if db < 0.0 { (-nb, -db) } else { (nb, db) };
        }
    }
    if collinear {
        let (adxa, adya, adxb, adyb) = (dxa.abs(), dya.abs(), dxb.abs(), dyb.abs());
        let (min_dx, min_dy) = (adxa.min(adxb), adya.min(adyb));
        let (use_x, nonzero) = if min_dx == min_dy { (true, min_dx > 0.0) } else { (min_dx > min_dy, true) };
        if nonzero {
            let c = |pt: Pt| if use_x { pt.x } else { pt.y };
            return collinear_relate(p1, p2, q1, q2, c(p1), c(p2), c(q1), c(q2), sides);
        }
    }
    let frac = (ra.0 / ra.1, rb.0 / rb.1);
    let ip = if equals(cs, p1, q1) || equals(cs, p1, q2) {
        p1
    } else if equals(cs, p2, q1) || equals(cs, p2, q2) {
        p2
    } else {
        // `calculate`: from the segment nearer an end, or the longer one.
        let (cla, clb) = (dxa * dxa + dya * dya, dxb * dxb + dyb * dyb);
        let clm = cla.max(clb);
        let edge = |r: (f64, f64)| (2.0 * (0.5 - r.0 / r.1).abs()).min(1.0);
        let use_a = if clm <= 0.0 { true } else { 5.0 * edge(ra) + (1.0 - cla / clm) > 5.0 * edge(rb) + (1.0 - clb / clm) };
        let mut pt = if use_a {
            Pt { x: p1.x + (ra.0 * dxa) / ra.1, y: p1.y + (ra.0 * dya) / ra.1 }
        } else {
            Pt { x: q1.x + (rb.0 * dxb) / rb.1, y: q1.y + (rb.0 * dyb) / rb.1 }
        };
        if ra.1.abs() < 1.0e-3 && rb.1.abs() < 1.0e-3 {
            assign_if_exceeds(&mut pt, p1, p2);
            assign_if_exceeds(&mut pt, q1, q2);
        }
        pt
    };
    let (how, arrival) = crosses_how(&sides);
    Some(Inter { how, opposite: false, arrival, sides, count: 1, ips: [ip, ip], fr: [frac, frac] })
}

fn assign_if_exceeds(pt: &mut Pt, s0: Pt, s1: Pt) {
    let exceeds = |p: Pt, a: Pt, b: Pt| -> bool {
        let dim = |c: f64, c0: f64, c1: f64| {
            if c0 < c1 {
                algo::fsmaller(c, c0)
            } else if c0 > c1 {
                c > c0 && !algo::feq(c, c0)
            } else {
                false
            }
        };
        dim(p.x, a.x, b.x) || dim(p.y, a.y, b.y)
    };
    if exceeds(*pt, s0, s1) {
        *pt = s0;
    } else if exceeds(*pt, s1, s0) {
        *pt = s1;
    }
}

/// `segments_direction::segments_crosses`: the "how" and the arrivals.
fn crosses_how(sides: &[[i32; 2]; 2]) -> (u8, [i32; 2]) {
    let ra0 = sides[0][0] == 0;
    let ra1 = sides[0][1] == 0;
    let rb0 = sides[1][0] == 0;
    let rb1 = sides[1][1] == 0;
    if ra0 && rb0 {
        (b'f', [-1, -1])
    } else if ra1 && rb1 {
        (b't', [1, 1])
    } else if ra1 && rb0 {
        (b'a', [1, -1])
    } else if ra0 && rb1 {
        (b'a', [-1, 1])
    } else if rb0 {
        (b's', [0, -1])
    } else if ra0 {
        (b's', [-1, 0])
    } else if rb1 {
        (b'm', [0, 1])
    } else if ra1 {
        (b'm', [1, 0])
    } else {
        (b'i', [-1, -1])
    }
}

fn position_value(ca: f64, cb1: f64, cb2: f64) -> i32 {
    if algo::feq(ca, cb1) {
        1
    } else if algo::feq(ca, cb2) {
        3
    } else if cb1 < cb2 {
        if ca < cb1 {
            0
        } else if ca > cb2 {
            4
        } else {
            2
        }
    } else if ca > cb1 {
        0
    } else if ca < cb2 {
        4
    } else {
        2
    }
}

/// `geographic_segments` (the SPHERICAL coordinate system's intersection strategy): the
/// relation of `p` and `q` with Sjöberg's intersection point.
fn intersect_geographic(p: &Sub, q: &Sub) -> Option<Inter> {
    use super::geographic::{Relation, relate};
    let ratio = |r: (f64, f64)| -> f64 { if r.1 == 0.0 { 0.0 } else { r.0 / r.1 } };
    match relate(p.i, p.j, q.i, q.j) {
        Relation::Disjoint | Relation::Degenerate => None,
        Relation::Crosses { sides, ip, ra, rb } => {
            let (how, arrival) = crosses_how(&sides);
            let fr = (ratio(ra), ratio(rb));
            Some(Inter { how, opposite: false, arrival, sides, count: 1, ips: [ip, ip], fr: [fr, fr] })
        }
        Relation::Collinear { opposite, pos, ra_from, ra_to, rb_from, rb_to } => {
            collinear_tail(p.i, p.j, q.i, q.j, opposite, pos, [ratio(ra_from), ratio(ra_to), ratio(rb_from), ratio(rb_to)], [[0, 0], [0, 0]])
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collinear_relate(a1: Pt, a2: Pt, b1: Pt, b2: Pt, oa1: f64, oa2: f64, ob1: f64, ob2: f64, sides: [[i32; 2]; 2]) -> Option<Inter> {
    let len_a = oa2 - oa1;
    let len_b = ob2 - ob1;
    let mut ra_from = (oa1 - ob1) / len_b;
    let mut ra_to = (oa2 - ob1) / len_b;
    let mut rb_from = (ob1 - oa1) / len_a;
    let mut rb_to = (ob2 - oa1) / len_a;
    let a1b = position_value(oa1, ob1, ob2);
    let a2b = position_value(oa2, ob1, ob2);
    let b1a = position_value(ob1, oa1, oa2);
    let b2a = position_value(ob2, oa1, oa2);
    if a1b == 1 {
        ra_from = 0.0;
        rb_from = 0.0;
    } else if a1b == 3 {
        ra_from = 1.0;
        rb_to = 0.0;
    }
    if a2b == 1 {
        ra_to = 0.0;
        rb_from = 1.0;
    } else if a2b == 3 {
        ra_to = 1.0;
        rb_to = 1.0;
    }
    if (a1b < 1 && a2b < 1) || (a1b > 3 && a2b > 3) {
        return None;
    }
    let opposite = sign(len_a) != sign(len_b);
    collinear_tail(a1, a2, b1, b2, opposite, [a1b, a2b, b1a, b2a], [ra_from, ra_to, rb_from, rb_to], sides)
}

/// `segments_collinear` of the intersection points and direction policies: the overlap's
/// points (along `a`) and the turn's "how".
#[allow(clippy::too_many_arguments)]
fn collinear_tail(a1: Pt, a2: Pt, b1: Pt, b2: Pt, opposite: bool, pos: [i32; 4], ratios: [f64; 4], sides: [[i32; 2]; 2]) -> Option<Inter> {
    let [a1b, a2b, b1a, b2a] = pos;
    let [ra_from, ra_to, rb_from, rb_to] = ratios;
    // The intersection points (`segments_intersection_points::segments_collinear`).
    let mut ips = [a1, a1];
    let mut fr = [(0.0, 0.0); 2];
    let mut on_a = [0.0; 2];
    let mut n = 0;
    if (1..=3).contains(&a1b) && n < 2 {
        ips[n] = a1;
        fr[n] = (0.0, ra_from);
        on_a[n] = 0.0;
        n += 1;
    }
    if b1a == 2 && n < 2 {
        ips[n] = b1;
        fr[n] = (rb_from, 0.0);
        on_a[n] = rb_from;
        n += 1;
    }
    if (1..=3).contains(&a2b) && n < 2 {
        ips[n] = a2;
        fr[n] = (1.0, ra_to);
        on_a[n] = 1.0;
        n += 1;
    }
    if b2a == 2 && n < 2 {
        ips[n] = b2;
        fr[n] = (rb_to, 1.0);
        on_a[n] = rb_to;
        n += 1;
    }
    if n == 2 && on_a[1] < on_a[0] {
        ips.swap(0, 1);
        fr.swap(0, 1);
    }
    // The direction (`segments_direction::segments_collinear`).
    let arr = |to: i32| if to == 2 { 1 } else if to == 1 || to == 3 { 0 } else { -1 };
    let arrival = [arr(a2b), arr(b2a)];
    let count = |v: [i32; 2]| -> (i32, i32, i32) {
        let (mut ins, mut on_end, mut out) = (0, 0, 0);
        for x in v {
            if x == 1 || x == 3 {
                on_end += 1;
            } else if x == 2 {
                ins += 1;
            } else {
                out += 1;
            }
        }
        (ins, on_end, out)
    };
    let (_, a_end, a_out) = count([a1b, a2b]);
    let (_, b_end, b_out) = count([b1a, b2a]);
    let mut how = b'c';
    if a_end == 1 && b_end == 1 && a_out == 1 && b_out == 1 {
        how = if !opposite {
            b'a'
        } else if arrival[0] == 0 {
            b't'
        } else {
            b'f'
        };
    } else if a_end == 2 && b_end == 2 {
        how = b'e';
    }
    Some(Inter { how, opposite, arrival, sides, count: n, ips, fr })
}

/// The side calculator over a pair of sub-ranges.
struct Sides<'a> {
    cs: Cs,
    p: &'a Sub,
    q: &'a Sub,
}

impl Sides<'_> {
    fn s(&self, a: Pt, b: Pt, c: Pt) -> i32 {
        side(self.cs, a, b, c)
    }
    fn pi_wrt_q1(&self) -> i32 {
        self.s(self.q.i, self.q.j, self.p.i)
    }
    fn pj_wrt_q1(&self) -> i32 {
        self.s(self.q.i, self.q.j, self.p.j)
    }
    fn pj_wrt_q2(&self) -> i32 {
        self.s(self.q.j, self.q.k, self.p.j)
    }
    fn qj_wrt_p1(&self) -> i32 {
        self.s(self.p.i, self.p.j, self.q.j)
    }
    fn qj_wrt_p2(&self) -> i32 {
        self.s(self.p.j, self.p.k, self.q.j)
    }
    fn pk_wrt_p1(&self) -> i32 {
        self.s(self.p.i, self.p.j, self.p.k)
    }
    fn pk_wrt_q1(&self) -> i32 {
        self.s(self.q.i, self.q.j, self.p.k)
    }
    fn qk_wrt_p1(&self) -> i32 {
        self.s(self.p.i, self.p.j, self.q.k)
    }
    fn qk_wrt_q1(&self) -> i32 {
        self.s(self.q.i, self.q.j, self.q.k)
    }
    fn pk_wrt_q2(&self) -> i32 {
        self.s(self.q.j, self.q.k, self.p.k)
    }
    fn qk_wrt_p2(&self) -> i32 {
        self.s(self.p.j, self.p.k, self.q.k)
    }
}

const UNION: u8 = b'u';
const INTER: u8 = b'i';
const BLOCKED: u8 = b'x';
const CONTINUE: u8 = b'c';
const NONE: u8 = b'-';

fn opposite(a: i32, b: i32) -> bool {
    a * b == -1
}

fn same(a: i32, b: i32) -> bool {
    a * b == 1
}

fn ui_else_iu(cond: bool, ops: &mut [u8; 2]) {
    *ops = if cond { [UNION, INTER] } else { [INTER, UNION] };
}

fn uu_else_ii(cond: bool, ops: &mut [u8; 2]) {
    *ops = if cond { [UNION, UNION] } else { [INTER, INTER] };
}

fn dist2(a: Pt, b: Pt) -> f64 {
    let (dx, dy) = (a.x - b.x, a.y - b.y);
    dx * dx + dy * dy
}

/// `set_both_verified` (`both_collinear`): continue, unless a measurable side difference picks
/// union and intersection.
#[allow(clippy::too_many_arguments)]
fn both_collinear(cs: Cs, rp: [Pt; 3], rq: [Pt; 3], ip: usize, iq: usize, point: Pt, ops: &mut [u8; 2], index_p: usize) {
    let index_q = 1 - index_p;
    let dp = dist2(point, rp[ip]);
    let dq = dist2(point, rq[iq]);
    let p_closer = dp < dq;
    let dm = if p_closer { distance_measure(cs, rq[iq - 1], rq[iq], rp[ip]) } else { distance_measure(cs, rp[ip - 1], rp[ip], rq[iq]) };
    if dm != 0.0 {
        let p_left = if p_closer { dm > 0.0 } else { dm < 0.0 };
        ops[index_p] = if p_left { UNION } else { INTER };
        ops[index_q] = if p_left { INTER } else { UNION };
        return;
    }
    *ops = [CONTINUE, CONTINUE];
}

fn arr3(s: &Sub) -> [Pt; 3] {
    [s.i, s.j, s.k]
}

/// `get_turn_info` with the null assign policy: the turns of `p` against `q`.
fn turn_info(cs: Cs, p: &Sub, q: &Sub, seg: [(i32, usize); 2], out: &mut Vec<Turn>) {
    let Some(inf) = intersect(cs, p, q) else { return };
    let sides = Sides { cs, p, q };
    let mut handle_as_touch = inf.how == b't';
    let mut handle_as_equal = inf.how == b'e';
    let mk = |pt: Pt, method: u8, ops: [u8; 2]| Turn { pt, method, ops, seg };
    if inf.how == b'm' {
        if inf.arrival[1] == 1 {
            if touch_interior_as_touch(cs, &inf, &sides, p) {
                handle_as_touch = true;
            } else {
                let ops = touch_interior(cs, &inf, &sides, 0);
                out.push(mk(inf.ips[0], b'm', ops));
            }
        } else {
            let swapped = Sides { cs, p: q, q: p };
            if touch_interior_as_touch(cs, &inf, &swapped, q) {
                handle_as_touch = true;
            } else {
                let ops = touch_interior(cs, &inf, &swapped, 1);
                out.push(mk(inf.ips[0], b'm', ops));
            }
        }
    }
    if inf.how == b'i' {
        let index = if inf.sides[1][0] == 1 { 0 } else { 1 };
        let mut ops = [NONE; 2];
        ops[index] = UNION;
        ops[1 - index] = INTER;
        out.push(mk(inf.ips[0], b'i', ops));
    }
    if handle_as_touch {
        let ops = touch(cs, &inf, &sides);
        out.push(mk(inf.ips[0], b't', ops));
    }
    let collinear = inf.how == b'c';
    if collinear {
        if !inf.opposite {
            if inf.arrival[0] == 0 || collinear_as_equal(&inf, p, q) {
                handle_as_equal = true;
            } else {
                let idx = non_opposite_to_index(&inf);
                let side_p = sides.pk_wrt_p1();
                let side_q = sides.qk_wrt_q1();
                let product = inf.arrival[0] * if inf.arrival[0] == 1 { side_p } else { side_q };
                let mut ops = [CONTINUE; 2];
                if product != 0 {
                    ui_else_iu(product == 1, &mut ops);
                }
                out.push(mk(inf.ips[idx], b'c', ops));
            }
        } else {
            // `collinear_opposite`: a turn where either arrives within the other and turns.
            if inf.arrival[0] == 1 {
                let s = sides.pk_wrt_p1();
                if s != 0 {
                    let mut ops = [BLOCKED; 2];
                    ops[0] = if s == 1 { INTER } else { UNION };
                    out.push(mk(inf.ips[1], b'c', ops));
                }
            }
            if inf.arrival[1] == 1 {
                let s = sides.qk_wrt_q1();
                if s != 0 {
                    let mut ops = [BLOCKED; 2];
                    ops[1] = if s == 1 { INTER } else { UNION };
                    out.push(mk(inf.ips[0], b'c', ops));
                }
            }
        }
    }
    if handle_as_equal && !inf.opposite {
        let idx = non_opposite_to_index(&inf);
        let point = inf.ips[idx];
        let mut ops = [NONE; 2];
        equal(cs, &sides, point, &mut ops);
        out.push(mk(point, if collinear { b'c' } else { b'e' }, ops));
    }
}

fn non_opposite_to_index(inf: &Inter) -> usize {
    if inf.fr[0].1 < inf.fr[1].1 { 1 } else { 0 }
}

fn collinear_as_equal(inf: &Inter, p: &Sub, q: &Sub) -> bool {
    let (ap, aq) = (inf.arrival[0], inf.arrival[1]);
    if ap * aq != -1 || inf.count != 2 {
        return false;
    }
    let dm = if ap == 1 { dist2(inf.ips[1], q.j) } else { dist2(inf.ips[1], p.j) };
    algo::feq(dm, 0.0)
}

fn touch_interior_as_touch(_cs: Cs, inf: &Inter, side: &Sides, non_touching: &Sub) -> bool {
    if same(side.pj_wrt_q1(), side.qj_wrt_p2()) || same(side.pj_wrt_q2(), side.qj_wrt_p1()) {
        return false;
    }
    algo::feq(dist2(inf.ips[0], non_touching.j), 0.0)
}

/// `touch_interior::apply<Index>`: `side.p` is the segment touched in its interior.
fn touch_interior(cs: Cs, inf: &Inter, side: &Sides, index: usize) -> [u8; 2] {
    let index_p = index;
    let index_q = 1 - index;
    let mut ops = [NONE; 2];
    let side_qi_p = inf.sides[index_q][0];
    let side_qk_p = side.qk_wrt_p1();
    if side_qi_p == -side_qk_p {
        let i = if side_qk_p == -1 { index_p } else { index_q };
        ops[i] = UNION;
        ops[1 - i] = INTER;
        return ops;
    }
    let side_qk_q = side.qk_wrt_q1();
    let side_pj_q2 = side.pj_wrt_q2();
    if side_qi_p == -1 && side_qk_p == -1 && side_qk_q == 1 {
        ops = [INTER, INTER];
    } else if side_qi_p == 1 && side_qk_p == 1 && side_qk_q == -1 {
        if side_pj_q2 == -1 {
            ops = [UNION, UNION];
        } else {
            ops[index_p] = UNION;
            ops[index_q] = BLOCKED;
        }
    } else if side_qi_p == side_qk_p && side_qi_p == side_qk_q {
        let mut i = if side_qk_q == 1 { index_q } else { index_p };
        if side_pj_q2 == 0 {
            i = 1 - i;
        }
        if opposite(side_pj_q2, side_qi_p) {
            let (a, b) = (side.qj_wrt_p1(), side.qj_wrt_p2());
            if same(a, b) && opposite(side.pj_wrt_q1(), side_pj_q2) {
                i = 1 - i;
            }
        }
        ops[i] = UNION;
        ops[1 - i] = INTER;
    } else if side_qk_p == 0 {
        if side_qk_q == side_qi_p {
            // `both_collinear<index_p, index_q>(range_p, range_q, 1, 2)`.
            let mut o = [NONE; 2];
            both_collinear(cs, arr3(side.p), arr3(side.q), 1, 2, inf.ips[0], &mut o, index_p);
            ops = o;
        } else {
            ops[index_p] = if side_qk_q == 1 { INTER } else { UNION };
            ops[index_q] = BLOCKED;
        }
    }
    ops
}

fn verified_side(cs: Cs, s: i32, a: Pt, b: Pt, c: Pt) -> i32 {
    if s == 0 {
        let dm = distance_measure(cs, a, b, c);
        return sign(dm);
    }
    s
}

fn touch(cs: Cs, inf: &Inter, side: &Sides) -> [u8; 2] {
    let (p, q) = (side.p, side.q);
    let mut ops = [NONE; 2];
    let side_pk_q1 = side.pk_wrt_q1();
    let side_qi_p1 = verified_side(cs, inf.sides[1][0], p.i, p.j, q.i);
    let side_qk_p1 = verified_side(cs, side.qk_wrt_p1(), p.i, p.j, q.k);
    if !opposite(side_qi_p1, side_qk_p1) {
        let side_pk_q2 = side.pk_wrt_q2();
        let side_pk_p = side.pk_wrt_p1();
        let side_qk_q = side.qk_wrt_q1();
        let q_turns_left = side_qk_q == 1;
        let block_q = side_qk_p1 == 0 && !same(side_qi_p1, side_qk_q);
        if side_pk_p == side_qi_p1 || side_pk_p == side_qk_p1 || (side_qi_p1 == 0 && side_qk_p1 == 0 && side_pk_p != -1) {
            if side_qk_p1 == 0 && side_pk_q1 == 0 && opposite(side.pi_wrt_q1(), side.qk_wrt_p2()) {
                // `handle_imperfect_touch`.
                let has_distance = |r1: &Sub, r2: &Sub| {
                    distance_measure(cs, r1.i, r1.j, r2.j) > 0.0 && distance_measure(cs, r2.j, r2.k, r1.i) > 0.0
                };
                if side_pk_q2 == -1 && has_distance(p, q) {
                    return [BLOCKED, UNION];
                }
                if side_pk_q2 == 1 && has_distance(q, p) {
                    return [UNION, BLOCKED];
                }
            }
            if side_pk_q2 == 0 && !block_q {
                both_collinear(cs, arr3(p), arr3(q), 2, 2, inf.ips[0], &mut ops, 0);
                return ops;
            }
            if side_pk_q1 == 0 {
                ops[0] = BLOCKED;
                ops[1] = if block_q {
                    BLOCKED
                } else if q_turns_left {
                    INTER
                } else {
                    UNION
                };
                return ops;
            }
            if side_pk_q1 == side_pk_q2 && !opposite(side_pk_q1, side_qk_q) {
                ui_else_iu(q_turns_left, &mut ops);
                if block_q {
                    ops[1] = BLOCKED;
                }
                return ops;
            }
            if side_pk_q2 == -side_qk_q {
                ui_else_iu(!q_turns_left, &mut ops);
                return ops;
            }
            if side_pk_q1 == -side_qk_q {
                uu_else_ii(!q_turns_left, &mut ops);
                if block_q {
                    ops[1] = BLOCKED;
                }
                return ops;
            }
        } else {
            ops[0] = if q_turns_left { INTER } else { UNION };
            ops[1] = if block_q {
                BLOCKED
            } else if side_qi_p1 == 1 || side_qk_p1 == 1 {
                UNION
            } else {
                INTER
            };
            return ops;
        }
        ops
    } else {
        let side_pk_p = verified_side(cs, side.pk_wrt_p1(), p.i, p.j, p.k);
        let right_to_left = side_qk_p1 == 1;
        if side_pk_p == side_qi_p1 {
            if side_pk_q1 == 0 {
                return [BLOCKED, if right_to_left { UNION } else { INTER }];
            }
            if side_pk_q1 == side_qk_p1 {
                uu_else_ii(right_to_left, &mut ops);
                return ops;
            }
        }
        if side_pk_p == side_qk_p1 {
            let side_pk_q2 = side.pk_wrt_q2();
            if side_pk_q2 == 0 {
                return [CONTINUE, CONTINUE];
            }
            if side_pk_q2 == side_qk_p1 {
                ui_else_iu(right_to_left, &mut ops);
                return ops;
            }
        }
        ui_else_iu(!right_to_left, &mut ops);
        ops
    }
}

fn equal(cs: Cs, side: &Sides, point: Pt, ops: &mut [u8; 2]) {
    let side_pk_q2 = side.pk_wrt_q2();
    let side_pk_p = side.pk_wrt_p1();
    let side_qk_p = side.qk_wrt_p1();
    if side_pk_p == side_qk_p {
        let side_qk_p2 = side.qk_wrt_p2();
        if opposite(side_qk_p2, side_pk_q2) {
            ui_else_iu(side_pk_q2 == 1, ops);
            return;
        }
    }
    if side_pk_q2 == 0 && side_pk_p == side_qk_p {
        both_collinear(cs, arr3(side.p), arr3(side.q), 2, 2, point, ops, 0);
        return;
    }
    if !opposite(side_pk_p, side_qk_p) {
        ui_else_iu(side_pk_q2 != -1, ops);
    } else {
        ui_else_iu(side_pk_p != -1, ops);
    }
}

#[cfg(test)]
mod tests {
    use super::super::wkt::from_wkt;
    use super::*;

    fn err(w: &str) -> String {
        match from_wkt(w.as_bytes(), Cs::Flat) {
            Ok(_) => "OK".into(),
            Err(e) => String::from_utf8(e).unwrap(),
        }
    }

    #[test]
    fn failures() {
        assert_eq!(err("POLYGON((0 0,1 1,2 2,0 0))"), "invalid geometry: Geometry has spikes. A spike point was found with apex at (2, 2)");
        assert_eq!(err("POLYGON((0 0 0, 1 0 0, 1 1 0, 0 0 0))"), "invalid geometry: Geometry has spikes. A spike point was found with apex at (1, 0)");
        assert_eq!(err("POLYGON EMPTY"), "invalid geometry: Geometry has too few points");
        assert_eq!(
            err("POLYGON((0 0,0 4,4 4,4 2,-2 2,-2 0,0 0))"),
            "invalid geometry: Geometry has invalid self-intersections. A self-intersection point was found at (0, 2); method: i; operations: i/u; segment IDs {source, multi, ring, segment}: {0, -1, -1, 0}/{0, -1, -1, 3}"
        );
        assert_eq!(
            err("POLYGON((0 0,0 4,2 4,2 -2,4 -2,4 0,0 0))"),
            "invalid geometry: Geometry has invalid self-intersections. A self-intersection point was found at (2, 0); method: i; operations: u/i; segment IDs {source, multi, ring, segment}: {0, -1, -1, 2}/{0, -1, -1, 5}"
        );
        assert_eq!(err("POLYGON((0 0,0 10,10 10,10 0,0 0),(2 2,4 2,4 4,2 4,2 2))"), "OK");
        assert_eq!(err("POLYGON((0 0,0 10,10 10,10 0,0 0),(20 20,24 20,24 24,20 24,20 20))"), "invalid geometry: Geometry has interior rings defined outside the outer boundary");
        assert_eq!(err("POLYGON((0 0,0 10,10 10,10 0,0 0),(1 1,9 1,9 9,1 9,1 1),(2 2,4 2,4 4,2 4,2 2))"), "invalid geometry: Geometry has nested interior rings");
    }
}
