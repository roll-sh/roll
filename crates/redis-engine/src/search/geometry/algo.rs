//! The Boost.Geometry algorithms the module's R-tree uses on its points and clockwise closed
//! polygons, in the FLAT (cartesian) and SPHERICAL (geographic, degrees) coordinate systems:
//! side tests, ring areas and `bg::correct`, envelopes, boxes (`within`, `covered_by`,
//! `disjoint`, `expand` with the spheroid's longitude wrapping) and the `within` / `intersects`
//! predicates between geometries.

use crate::fp::Mac;
use super::{Cs, Geom, Poly, Pt, Rect, Ring};

// ---------------------------------------------------------------------------------------------
// Sides and points
//
// The reference build contracts a product into the sum or difference of the same expression
// (`-ffp-contract=on`: the left product when both sides are products, never across statements
// or through calls): those spots use `mul_add` here, and the trigonometry is musl's (`libm`).

/// The side of `p` relative to the segment `a`→`b`: 1 left, -1 right, 0 collinear. On the
/// spheroid, relative to the geodesic through `a` and `b`.
pub fn side(cs: Cs, a: Pt, b: Pt, p: Pt) -> i32 {
    match cs {
        Cs::Flat => side_by_triangle(a, b, p),
        Cs::Sphere => {
            // `strategy::side::geographic`: the azimuths from `a` (Andoyer, WGS84) compared.
            if equals(cs, p, a) || equals(cs, p, b) || equals(cs, a, b) {
                return 0;
            }
            azimuth_side_value(super::geographic::azimuth(a, p), super::geographic::azimuth(a, b))
        }
    }
}

/// `azimuth_side_value`: the turn from `a12` to `a1p`, positive azimuths on the right.
pub fn azimuth_side_value(a1p: f64, a12: f64) -> i32 {
    use std::f64::consts::PI;
    let mut diff = a1p - a12;
    while diff > PI {
        diff -= 2.0 * PI;
    }
    while diff <= -PI {
        diff += 2.0 * PI;
    }
    if feq(diff, 0.0) || feq(diff, PI) || feq(diff, -PI) {
        0
    } else if diff > 0.0 {
        -1
    } else {
        1
    }
}

/// `math::equals` on doubles: equal, or within an epsilon scaled by the larger magnitude (at
/// least 1).
pub fn feq(a: f64, b: f64) -> bool {
    if a == b {
        return true;
    }
    if !a.is_finite() || !b.is_finite() {
        return false;
    }
    (a - b).abs() <= f64::EPSILON * a.abs().max(b.abs()).max(1.0)
}

/// `math::smaller`: below and not equal.
pub fn fsmaller(a: f64, b: f64) -> bool {
    a < b && !feq(a, b)
}

/// The `cartesian` less with epsilon equality: by x, then y.
fn less_eps(a: Pt, b: Pt) -> bool {
    if !feq(a.x, b.x) {
        return a.x < b.x;
    }
    if !feq(a.y, b.y) {
        return a.y < b.y;
    }
    false
}

/// `detail::determinant`: `ux * vy - uy * vx` (the first product fused).
pub fn determinant(ux: f64, uy: f64, vx: f64, vy: f64) -> f64 {
    ux.mac(vy, -(uy * vx))
}

/// `side_by_triangle` for doubles: the points ordered so the computation does not depend on
/// the segment's direction, zero within an epsilon scaled by the largest difference.
fn side_by_triangle(p1: Pt, p2: Pt, p: Pt) -> i32 {
    if equals(Cs::Flat, p1, p2) || equals(Cs::Flat, p1, p) || equals(Cs::Flat, p2, p) {
        return 0;
    }
    let value = |s1: Pt, s2: Pt, q: Pt| -> (f64, f64) {
        let dx = s2.x - s1.x;
        let dy = s2.y - s1.y;
        let dpx = q.x - s1.x;
        let dpy = q.y - s1.y;
        let factor = dx.abs().max(dy.abs()).max(dpx.abs()).max(dpy.abs()).max(1.0);
        (determinant(dx, dy, dpx, dpy), factor)
    };
    let (s, factor) = if less_eps(p, p1) {
        if less_eps(p, p2) { value(p, p1, p2) } else { value(p2, p, p1) }
    } else if less_eps(p1, p2) {
        value(p1, p2, p)
    } else {
        value(p2, p, p1)
    };
    let zero = s == 0.0 || (s.is_finite() && s.abs() <= f64::EPSILON * factor);
    if zero {
        0
    } else if s > 0.0 {
        1
    } else {
        -1
    }
}

/// `arithmetic::side_value` of `p` against the line `a x + b y + c = 0`.
fn line_side_value(a: f64, b: f64, c: f64, p: Pt) -> f64 {
    a.mac(p.x, b * p.y) + c
}

/// `get_distance_measure` in the plane: the side value of `p` against the infinite line through
/// `p1` and `p2` (`make_infinite_line`; zero on the spheroid).
pub fn distance_measure(cs: Cs, p1: Pt, p2: Pt, p: Pt) -> f64 {
    match cs {
        Cs::Flat => {
            let a = p1.y - p2.y;
            let b = p2.x - p1.x;
            let c = (-a).mac(p1.x, -(b * p1.y));
            line_side_value(a, b, c, p)
        }
        Cs::Sphere => 0.0,
    }
}

/// `strategy::normalize::spherical_point` with `exact = false`: the longitude into
/// (-180, 180] (within a float epsilon of ±180 it is 180), 0 at the poles.
fn normalize_inexact(p: Pt) -> Pt {
    let eps = f32::EPSILON as f64;
    let mut lon = p.x;
    if (lon.abs() - 180.0).abs() <= eps {
        lon = 180.0;
    } else if lon > 180.0 {
        lon = (lon + 180.0) % 360.0 - 180.0;
        if (lon + 180.0).abs() <= eps {
            lon = 180.0;
        }
    } else if lon < -180.0 {
        lon = (lon - 180.0) % 360.0 + 180.0;
    }
    if feq(p.y.abs(), 90.0) {
        lon = 0.0;
    }
    Pt { x: lon, y: p.y }
}

/// `equals_point_point` (`cartesian_point_point`, `spherical_point_point`): the coordinates
/// equal within the epsilon, on the spheroid after normalizing both points.
pub fn equals(cs: Cs, a: Pt, b: Pt) -> bool {
    match cs {
        Cs::Flat => feq(a.x, b.x) && feq(a.y, b.y),
        Cs::Sphere => {
            let (a, b) = (normalize_inexact(a), normalize_inexact(b));
            feq(a.x, b.x) && feq(a.y, b.y)
        }
    }
}

/// `direction_code_impl<cartesian_tag>` for collinear `a`, `b`, `p`: `p`'s side of the line
/// through `b` perpendicular to `a`→`b` (1 past `b`, -1 back, 0 at it).
fn direction_code_flat(a: Pt, b: Pt, p: Pt) -> i32 {
    // `make_perpendicular_line(a, b, b)`
    let la = a.y - b.y;
    let lb = b.x - a.x;
    let (pa, pb) = (lb, -la);
    if feq(pa, 0.0) && feq(pb, 0.0) {
        return 0;
    }
    let c = (-pa).mac(b.x, -(pb * b.y));
    let v = line_side_value(pa, pb, c, p);
    if v == 0.0 {
        0
    } else if v > 0.0 {
        1
    } else {
        -1
    }
}

/// `point_is_spike_or_equal(last, a, b)`: `last` back on the segment `a`→`b`'s line, not past `b`.
pub fn is_spike_or_equal(cs: Cs, next: Pt, cur: Pt, prev: Pt) -> bool {
    if side(cs, next, cur, prev) != 0 {
        return false;
    }
    match cs {
        Cs::Flat => direction_code_flat(next, cur, prev) < 1,
        Cs::Sphere => super::geographic::direction_code(next, cur, prev) < 1,
    }
}

// ---------------------------------------------------------------------------------------------
// Areas and `correct`

/// `ring_area`: zero for rings under 4 points; positive for clockwise rings.
pub fn ring_area(cs: Cs, r: &[Pt]) -> f64 {
    if r.len() < 4 {
        return 0.0;
    }
    match cs {
        Cs::Flat => {
            // `strategy::area::cartesian`: the surveyor's sum `(x1 + x2) * (y1 - y2)`.
            // The reference's loop is vectorized four segments at a time (the products
            // rounded, then added in order); the remaining segments run scalar, each product
            // fused into the sum.
            let mut sum = 0.0;
            let n = r.len() - 1;
            let vectorized = n - n % 4;
            for (i, w) in r.windows(2).enumerate() {
                let (a, b) = (w[0].x + w[1].x, w[0].y - w[1].y);
                sum = if i < vectorized { sum + a * b } else { a.mac(b, sum) };
            }
            sum / 2.0
        }
        Cs::Sphere => super::geographic::ring_area(r),
    }
}

/// `close_or_open_ring` for closed rings: the first point appended when it differs from the last.
fn close_ring(cs: Cs, r: &mut Ring) {
    if r.pts.len() <= 2 {
        return;
    }
    let (f, l) = (r.pts[0], r.pts[r.pts.len() - 1]);
    if !equals(cs, f, l) {
        r.push(f);
    }
}

/// `bg::correct`: rings closed, the exterior made clockwise, the interiors counter-clockwise.
pub fn correct(p: &mut Poly, cs: Cs) {
    close_ring(cs, &mut p.outer);
    if ring_area(cs, &p.outer.pts) < 0.0 {
        p.outer.pts.reverse();
    }
    for r in &mut p.inners {
        close_ring(cs, r);
        if ring_area(cs, &r.pts) > 0.0 {
            r.pts.reverse();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Boxes

/// `content`: the product of the sides.
pub fn content(b: &Rect) -> f64 {
    (b.max.x - b.min.x) * (b.max.y - b.min.y)
}

/// `longitude_distance_unsigned<degree>`: from `a` east to `b`, in [0, 360).
pub fn longitude_distance_unsigned(a: f64, b: f64) -> f64 {
    let mut d = b - a;
    super::geographic::normalize_lon_deg(&mut d);
    if d < 0.0 { d + 360.0 } else { d }
}

/// `is_inverse_spheroidal_coordinates`: a box `assign_inverse` made.
fn is_inverse(b: &Rect) -> bool {
    b.min.x == f64::MAX && b.min.y == f64::MAX && b.max.x == f64::MIN && b.max.y == f64::MIN
}

/// `strategy::normalize::spherical_box` (`normalize_spheroidal_box_coordinates`): the corners'
/// longitudes into (-180, 180] (poles kept), a box on a pole collapsed to longitude 0, a band
/// (a full turn) made [-180, 180], else the max moved a turn up when below the min.
pub fn normalize_box(b: &Rect) -> Rect {
    let span = (b.min.x - b.max.x).abs();
    let band = span > 360.0 || feq(span, 360.0);
    let (mut lo, mut hi) = (b.min.x, b.max.x);
    super::geographic::normalize_lon_deg(&mut lo);
    super::geographic::normalize_lon_deg(&mut hi);
    if (feq(b.min.y, -90.0) && feq(b.max.y, -90.0)) || (feq(b.min.y, 90.0) && feq(b.max.y, 90.0)) {
        lo = 0.0;
        hi = 0.0;
    } else if band {
        lo = -180.0;
        hi = 180.0;
    } else if lo > hi {
        hi += 360.0;
    }
    Rect { min: Pt { x: lo, y: b.min.y }, max: Pt { x: hi, y: b.max.y } }
}

/// `sweep_event::operator<`: by time (equal within the epsilon: starts first).
fn event_less(a: &(f64, bool, usize), b: &(f64, bool, usize)) -> bool {
    if !feq(a.0, b.0) { a.0 < b.0 } else { a.1 && !b.1 }
}

fn event_after(a: &(f64, bool, usize), b: &(f64, bool, usize)) -> bool {
    event_less(b, a)
}

/// `maximum_gap`: the sweep over the intervals' start and end events (times equal within the
/// epsilon ordered starts first), the widest gap where nothing overlaps.
fn maximum_gap(intervals: &[(f64, f64)]) -> (f64, f64, f64) {
    // `event_greater` over (time, is_start, interval): the earliest event on top
    let starts = intervals.iter().enumerate().map(|(i, iv)| (iv.0, true, i)).collect();
    let mut queue = super::super::hnsw::StdHeap::from_vec(starts, event_after);
    let (mut count, mut gl, mut gr) = (0usize, 0.0f64, 0.0f64);
    while !queue.is_empty() {
        let ev = queue.pop();
        if ev.1 {
            count += 1;
            queue.push((intervals[ev.2].1, false, ev.2));
        } else {
            count -= 1;
            if count == 0 && !queue.is_empty() {
                let next = intervals[queue.top().2].0;
                let gap = next - ev.0;
                if gap > gr - gl {
                    gl = ev.0;
                    gr = next;
                }
            }
        }
    }
    (gr - gl, gl, gr)
}

fn flarger(a: f64, b: f64) -> bool {
    a > b && !feq(a, b)
}

/// `envelope_range_of_boxes` (two boxes): the smallest longitude interval covering both.
fn range_of_boxes(a: &Rect, b: &Rect) -> Rect {
    let boxes = [*a, *b];
    let mut intervals: Vec<(f64, f64)> = Vec::new();
    for bx in &boxes {
        if is_inverse(bx) {
            continue;
        }
        if feq(bx.min.y, 90.0) || feq(bx.max.y, -90.0) {
            continue;
        }
        if flarger(bx.max.x, 180.0) {
            intervals.push((bx.min.x, 180.0));
            intervals.push((-180.0, bx.max.x - 360.0));
        } else {
            intervals.push((bx.min.x, bx.max.x));
        }
    }
    let (mut lo, mut hi) = (0.0, 0.0);
    if !intervals.is_empty() {
        // `min_element` / `max_element` with `math::smaller`
        lo = intervals[0].0;
        for iv in &intervals[1..] {
            if fsmaller(iv.0, lo) {
                lo = iv.0;
            }
        }
        hi = intervals[0].1;
        for iv in &intervals[1..] {
            if fsmaller(hi, iv.1) {
                hi = iv.1;
            }
        }
        if flarger(hi - lo, 180.0) {
            let (gap, gl, gr) = maximum_gap(&intervals);
            if flarger(gap, 0.0) {
                let wrapped = 360.0 + lo - hi;
                if flarger(gap, wrapped) {
                    lo = gr;
                    hi = gl + 360.0;
                }
            }
        }
    }
    let min_lat = if fsmaller(b.min.y, a.min.y) { b.min.y } else { a.min.y };
    let max_lat = if fsmaller(a.max.y, b.max.y) { b.max.y } else { a.max.y };
    Rect { min: Pt { x: lo, y: min_lat }, max: Pt { x: hi, y: max_lat } }
}

/// `expand(box, box)`.
pub fn expand(cs: Cs, b: &mut Rect, o: &Rect) {
    match cs {
        Cs::Flat => {
            b.min.x = b.min.x.min(o.min.x);
            b.min.y = b.min.y.min(o.min.y);
            b.max.x = b.max.x.max(o.max.x);
            b.max.y = b.max.y.max(o.max.y);
        }
        Cs::Sphere => {
            // `envelope_box_on_spheroid`: inverse boxes are not normalized
            let norm = |x: &Rect| if is_inverse(x) { *x } else { normalize_box(x) };
            let (n1, n2) = (norm(o), norm(b));
            *b = range_of_boxes(&n1, &n2);
        }
    }
}

/// `expand(box, segment)` on the spheroid (`segment_on_spheroid`): the segment's envelope as it
/// is (not normalized) with the normalized box.
pub fn expand_segment(b: &mut Rect, p: Pt, q: Pt) {
    let e = segment_envelope(p, q);
    let n = if is_inverse(b) { *b } else { normalize_box(b) };
    *b = range_of_boxes(&e, &n);
}

/// `within(box, box)`: inside the closed box with interiors (each side's extent) overlapping.
pub fn box_within(cs: Cs, ed: &Rect, ing: &Rect) -> bool {
    let lat = ing.min.y <= ed.min.y && ed.max.y <= ing.max.y && ed.min.y < ed.max.y;
    lat && lon_range(cs, ed, ing, true)
}

/// `covered_by(box, box)`.
pub fn box_covered_by(cs: Cs, ed: &Rect, ing: &Rect) -> bool {
    let lat = ed.min.y >= ing.min.y && ed.max.y <= ing.max.y;
    lat && lon_range(cs, ed, ing, false)
}

fn lon_range(cs: Cs, ed: &Rect, ing: &Rect, within: bool) -> bool {
    let coord = if within {
        ing.min.x <= ed.min.x && ed.max.x <= ing.max.x && ed.min.x < ed.max.x
    } else {
        ed.min.x >= ing.min.x && ed.max.x <= ing.max.x
    };
    if coord || cs == Cs::Flat {
        return coord;
    }
    // `box_longitude_range`.
    let diff_ed = ed.max.x - ed.min.x;
    let diff_ing = ing.max.x - ing.min.x;
    if diff_ing >= 360.0 {
        return true;
    }
    if diff_ing < diff_ed || (within && diff_ed <= 0.0) {
        return false;
    }
    let diff_min = longitude_distance_unsigned(ing.min.x, ed.min.x);
    ing.min.x + diff_min + diff_ed <= ing.max.x
}

/// `disjoint(box, box)`.
pub fn box_disjoint(cs: Cs, a: &Rect, b: &Rect) -> bool {
    if cs == Cs::Sphere {
        let (d1, d2) = (a.max.x - a.min.x, b.max.x - b.min.x);
        if d1 < 360.0 && d2 < 360.0 {
            let diff_min = longitude_distance_unsigned(a.min.x, b.min.x);
            let b2_min = a.min.x + diff_min;
            let mut b2_max = b2_min - 360.0 + d2;
            if (b2_max - b.max.x).abs() < 180.0 {
                b2_max = b.max.x;
            }
            if b2_min > a.max.x && b2_max < a.min.x {
                return true;
            }
        }
        return a.max.y < b.min.y || a.min.y > b.max.y;
    }
    a.max.x < b.min.x || a.min.x > b.max.x || a.max.y < b.min.y || a.min.y > b.max.y
}

pub fn box_intersects(cs: Cs, a: &Rect, b: &Rect) -> bool {
    !box_disjoint(cs, a, b)
}

/// `nexttoward` a double towards ±infinity.
fn next_toward(x: f64, up: bool) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x == 0.0 {
        let tiny = f64::from_bits(1);
        return if up { tiny } else { -tiny };
    }
    let bits = x.to_bits();
    let away = (x > 0.0) == up;
    f64::from_bits(if away { bits + 1 } else { bits - 1 })
}

/// The module's `make_mbr`: a point's box one ulp around it, a polygon's envelope.
pub fn mbr(cs: Cs, g: &Geom) -> Rect {
    match g {
        Geom::Point(p) => Rect {
            min: Pt { x: next_toward(p.x, false), y: next_toward(p.y, false) },
            max: Pt { x: next_toward(p.x, true), y: next_toward(p.y, true) },
        },
        Geom::Poly(p) => envelope(cs, &p.outer.pts),
    }
}

/// `return_envelope` of a polygon (its exterior ring): on the spheroid, the segments' latitude
/// extremes and the shortest longitude interval.
fn envelope(cs: Cs, r: &[Pt]) -> Rect {
    if r.is_empty() {
        return Rect { min: Pt { x: f64::MAX, y: f64::MAX }, max: Pt { x: f64::MIN, y: f64::MIN } };
    }
    match cs {
        Cs::Flat => {
            let mut b = Rect { min: r[0], max: r[0] };
            for p in r {
                b.min.x = b.min.x.min(p.x);
                b.min.y = b.min.y.min(p.y);
                b.max.x = b.max.x.max(p.x);
                b.max.y = b.max.y.max(p.y);
            }
            b
        }
        Cs::Sphere => super::geographic::ring_envelope(r),
    }
}

/// A great-circle segment's box: the endpoints' longitudes (the short way round) and the
/// latitudes, extended to the circle's vertex when it lies inside the segment.
pub fn segment_envelope(a: Pt, b: Pt) -> Rect {
    super::geographic::segment_envelope(a, b)
}

// ---------------------------------------------------------------------------------------------
// Point and polygon relations

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loc {
    Outside,
    Boundary,
    Inside,
}

/// The winding test of `p` against a closed ring: inside, on its boundary or outside.
pub fn ring_loc(cs: Cs, r: &[Pt], p: Pt) -> Loc {
    super::relate::loc(super::relate::point_in_ring(cs, r, p))
}

/// A point against a polygon: inside the exterior and outside every hole is inside, on any
/// ring is the boundary.
pub fn poly_loc(cs: Cs, poly: &Poly, p: Pt) -> Loc {
    super::relate::loc(super::relate::point_in_poly(cs, poly, p))
}

/// `bg::intersects`.
pub fn intersects(cs: Cs, a: &Geom, b: &Geom) -> bool {
    match (a, b) {
        (Geom::Point(p), Geom::Point(q)) => equals(cs, *p, *q),
        (Geom::Point(p), Geom::Poly(q)) | (Geom::Poly(q), Geom::Point(p)) => poly_loc(cs, q, *p) != Loc::Outside,
        (Geom::Poly(p), Geom::Poly(q)) => super::relate::poly_intersects(cs, p, q),
    }
}

/// `bg::within(a, b)` with `b` a polygon.
pub fn within(cs: Cs, a: &Geom, b: &Poly) -> bool {
    match a {
        Geom::Point(p) => poly_loc(cs, b, *p) == Loc::Inside,
        Geom::Poly(a) => super::relate::poly_within(cs, a, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poly(pts: &[(f64, f64)]) -> Poly {
        let mut p = Poly::default();
        for &(x, y) in pts {
            p.outer.push(Pt { x, y });
        }
        correct(&mut p, Cs::Flat);
        p
    }

    #[test]
    fn predicates() {
        let sq = poly(&[(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0)]);
        assert_eq!(poly_loc(Cs::Flat, &sq, Pt { x: 5.0, y: 5.0 }), Loc::Inside);
        assert_eq!(poly_loc(Cs::Flat, &sq, Pt { x: 10.0, y: 5.0 }), Loc::Boundary);
        assert_eq!(poly_loc(Cs::Flat, &sq, Pt { x: 11.0, y: 5.0 }), Loc::Outside);
        let inner = poly(&[(0.0, 0.0), (0.0, 5.0), (5.0, 5.0), (5.0, 0.0)]);
        assert!(within(Cs::Flat, &Geom::Poly(inner.clone()), &sq));
        assert!(!within(Cs::Flat, &Geom::Poly(sq.clone()), &inner));
        let far = poly(&[(20.0, 20.0), (20.0, 25.0), (25.0, 25.0), (25.0, 20.0)]);
        assert!(!intersects(Cs::Flat, &Geom::Poly(far), &Geom::Poly(sq.clone())));
        let touch = poly(&[(10.0, 0.0), (10.0, 5.0), (15.0, 5.0), (15.0, 0.0)]);
        assert!(intersects(Cs::Flat, &Geom::Poly(touch.clone()), &Geom::Poly(sq.clone())));
        assert!(!within(Cs::Flat, &Geom::Poly(touch), &sq));
        assert_eq!(ring_area(Cs::Flat, &sq.outer.pts), 100.0);
    }

    #[test]
    fn spherical() {
        let p = Pt { x: 10.0, y: 10.1 };
        let mut sq = Poly::default();
        for (x, y) in [(0.0, 0.0), (0.0, 10.0), (20.0, 10.0), (20.0, 0.0), (0.0, 0.0)] {
            sq.outer.push(Pt { x, y });
        }
        assert!(ring_area(Cs::Sphere, &sq.outer.pts) > 0.0);
        assert_eq!(poly_loc(Cs::Sphere, &sq, Pt { x: 10.0, y: 5.0 }), Loc::Inside);
        assert_eq!(poly_loc(Cs::Sphere, &sq, p), Loc::Inside);
        assert_eq!(poly_loc(Cs::Sphere, &sq, Pt { x: 10.0, y: -5.0 }), Loc::Outside);
        assert_eq!(poly_loc(Cs::Sphere, &sq, Pt { x: 30.0, y: 5.0 }), Loc::Outside);
        let e = envelope(Cs::Sphere, &sq.outer.pts);
        assert!(e.max.y > 10.1 && e.max.y < 10.2, "{e:?}");
        let mut b = Rect { min: Pt { x: 170.0, y: 0.0 }, max: Pt { x: 175.0, y: 1.0 } };
        expand(Cs::Sphere, &mut b, &Rect { min: Pt { x: -175.0, y: 0.0 }, max: Pt { x: -170.0, y: 1.0 } });
        assert_eq!((b.min.x, b.max.x), (170.0, 190.0));
    }
}
