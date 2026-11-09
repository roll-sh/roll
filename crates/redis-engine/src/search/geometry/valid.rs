//! `bg::is_valid` with its failure messages for the module's points and clockwise closed
//! polygons: per ring the coordinate, size, topological dimension, closure, spike and
//! orientation checks, then the polygon's self turns (Boost's sectionalized segment pairs and
//! turn classification, stopping at the first unacceptable turn and reporting the first turn
//! found), holes outside or nested, and a disconnected interior.

use super::algo::{self, Loc, equals};
use super::turns;
use super::{Cs, Geom, Poly, Pt};
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

/// The polygon's self turns (`self_turns` with the null assign policy, adjacent segments
/// skipped), and whether an unacceptable one stopped the search.
pub fn self_turns(p: &Poly, cs: Cs) -> (Vec<Turn>, bool) {
    let rings: Vec<&[Pt]> = std::iter::once(p.outer.pts.as_slice()).chain(p.inners.iter().map(|r| r.pts.as_slice())).collect();
    let g = turns::Sectioned::new(cs, 0, rings);
    let mut found = Vec::new();
    let mut bad = false;
    // `stateless_predicate_based_interrupt_policy<is_acceptable_turn>`
    let mut interrupt = |new: &[turns::Turn]| -> bool {
        if new.iter().any(|t| !acceptable(t)) {
            bad = true;
        }
        bad
    };
    turns::self_turns(cs, &g, turns::NULL_POLICY, &mut found, &mut interrupt);
    let out = found
        .iter()
        .map(|t| Turn { pt: t.point, method: t.method, ops: [t.ops[0].op, t.ops[1].op], seg: [(t.ops[0].seg.ring, t.ops[0].seg.segment as usize), (t.ops[1].seg.ring, t.ops[1].seg.segment as usize)] })
        .collect();
    (out, bad)
}

/// `is_acceptable_turn<Polygon>`: a touch between different rings with both operations
/// intersection.
fn acceptable(t: &turns::Turn) -> bool {
    if t.ops[0].seg.ring == t.ops[1].seg.ring {
        return false;
    }
    (t.method == b'm' || t.method == b't') && t.ops[0].op == turns::INTER && t.ops[1].op == turns::INTER
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
