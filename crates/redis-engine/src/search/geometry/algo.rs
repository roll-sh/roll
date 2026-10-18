//! The Boost.Geometry algorithms the module's R-tree uses on its points and clockwise closed
//! polygons, in the FLAT (cartesian) and SPHERICAL (geographic, degrees) coordinate systems:
//! side tests, ring areas and `bg::correct`, envelopes, boxes (`within`, `covered_by`,
//! `disjoint`, `expand` with the spheroid's longitude wrapping) and the `within` / `intersects`
//! predicates between geometries.

use super::{Cs, Geom, Poly, Pt, Rect, Ring};

// ---------------------------------------------------------------------------------------------
// Sides and points

/// The side of `p` relative to the segment `a`→`b`: 1 left, -1 right, 0 collinear. On the
/// spheroid, relative to the great circle through `a` and `b`.
pub fn side(cs: Cs, a: Pt, b: Pt, p: Pt) -> i32 {
    match cs {
        Cs::Flat => side_by_triangle(a, b, p),
        Cs::Sphere => {
            // `strategy::side::geographic`: the azimuths from `a` (Andoyer, WGS84) compared.
            if equals(cs, p, a) || equals(cs, p, b) || equals(cs, a, b) {
                return 0;
            }
            azimuth_side_value(andoyer_azimuth(a, p), andoyer_azimuth(a, b))
        }
    }
}

/// WGS84's flattening, `(a - b) / a`.
pub fn flattening() -> f64 {
    (6378137.0 - 6_356_752.314_245_179) / 6378137.0
}

/// `andoyer_inverse`'s forward azimuth between two points in degrees.
pub fn andoyer_azimuth(p1: Pt, p2: Pt) -> f64 {
    use std::f64::consts::PI;
    let (lon1, lat1, lon2, lat2) = (p1.x.to_radians(), p1.y.to_radians(), p2.x.to_radians(), p2.y.to_radians());
    if feq(lon1, lon2) && feq(lat1, lat2) {
        return 0.0;
    }
    let f = flattening();
    let dlon = lon2 - lon1;
    let (sin_dlon, cos_dlon) = (dlon.sin(), dlon.cos());
    let (sin_lat1, cos_lat1) = (lat1.sin(), lat1.cos());
    let (sin_lat2, cos_lat2) = (lat2.sin(), lat2.cos());
    let cos_d = (sin_lat1 * sin_lat2 + cos_lat1 * cos_lat2 * cos_dlon).clamp(-1.0, 1.0);
    let d = cos_d.acos();
    let sin_d = d.sin();
    if feq(sin_d, 0.0) {
        if cos_d >= 0.0 {
            return 0.0;
        }
        return if !feq(sin_lat1, 1.0) { 0.0 } else { PI };
    }
    let mut a = 0.0;
    let mut u = 0.0;
    if feq(cos_lat2, 0.0) {
        if sin_lat2 < 0.0 {
            a = PI;
        }
    } else {
        let tan_lat2 = sin_lat2 / cos_lat2;
        let m = cos_lat1 * tan_lat2 - sin_lat1 * cos_dlon;
        a = sin_dlon.atan2(m);
        u = (f / 2.0) * (cos_lat1 * cos_lat1) * (2.0 * a).sin();
    }
    let mut v = 0.0;
    if !feq(cos_lat1, 0.0) {
        let tan_lat1 = sin_lat1 / cos_lat1;
        let n = cos_lat2 * tan_lat1 - sin_lat2 * cos_dlon;
        let b = sin_dlon.atan2(n);
        v = (f / 2.0) * (cos_lat2 * cos_lat2) * (2.0 * b).sin();
    }
    let t = d / sin_d;
    let da = v * t - u;
    let mut az = a - da;
    // `normalize_azimuth`.
    if a >= 0.0 {
        if da >= 0.0 {
            if az < 0.0 {
                az = 0.0;
            }
        } else if az > PI {
            az = PI;
        }
    } else if da <= 0.0 {
        if az > 0.0 {
            az = 0.0;
        }
    } else if az < -PI {
        az = -PI;
    }
    az
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
        (dx * dpy - dy * dpx, factor)
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

/// `get_distance_measure` in the plane: the side value of `p` against the infinite line through
/// `p1` and `p2` (zero on the spheroid).
pub fn distance_measure(cs: Cs, p1: Pt, p2: Pt, p: Pt) -> f64 {
    match cs {
        Cs::Flat => {
            let a = p1.y - p2.y;
            let b = p2.x - p1.x;
            let c = -a * p1.x - b * p1.y;
            a * p.x + b * p.y + c
        }
        Cs::Sphere => 0.0,
    }
}

type V3 = [f64; 3];

fn unit(p: Pt) -> V3 {
    let (lon, lat) = (p.x.to_radians(), p.y.to_radians());
    [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
}

fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// `normalize_longitude`: into (-180, 180].
pub fn norm_lon(l: f64) -> f64 {
    if l > -180.0 && l <= 180.0 {
        return l;
    }
    let r = (l + 180.0).rem_euclid(360.0) - 180.0;
    if r == -180.0 { 180.0 } else { r }
}

/// `equals_point_point`: on the spheroid, longitudes compare normalized and every point of a
/// pole is the pole.
pub fn equals(cs: Cs, a: Pt, b: Pt) -> bool {
    match cs {
        Cs::Flat => feq(a.x, b.x) && feq(a.y, b.y),
        Cs::Sphere => {
            if a.y != b.y {
                return false;
            }
            if a.y.abs() == 90.0 {
                return true;
            }
            norm_lon(a.x) == norm_lon(b.x)
        }
    }
}

/// `direction_code` for collinear `a`, `b`, `p`: 1 when `p` continues past `b` in the direction
/// `a`→`b`, -1 when it goes back, 0 at `b`.
pub fn direction_code(cs: Cs, a: Pt, b: Pt, p: Pt) -> i32 {
    let v = match cs {
        Cs::Flat => {
            let (dx, dy) = (b.x - a.x, b.y - a.y);
            if dx == 0.0 && dy == 0.0 {
                return 0;
            }
            // `make_perpendicular_line(a, b, b)`'s side value.
            let c = -dx * b.x - dy * b.y;
            dx * p.x + dy * p.y + c
        }
        Cs::Sphere => {
            if equals(cs, a, b) || equals(cs, b, p) {
                return 0;
            }
            let (a3, b3, p3) = (unit(a), unit(b), unit(p));
            // Along the great circle: the tangent at b pointing away from a.
            let n = cross(a3, b3);
            let t = cross(n, b3);
            dot(t, p3)
        }
    };
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
    side(cs, next, cur, prev) == 0 && direction_code(cs, next, cur, prev) < 1
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
            let mut sum = 0.0;
            for w in r.windows(2) {
                sum += (w[0].x + w[1].x) * (w[0].y - w[1].y);
            }
            sum / 2.0
        }
        Cs::Sphere => {
            // The signed spherical excess, by the segments' trapezoids towards the equator.
            let mut sum = 0.0;
            for w in r.windows(2) {
                let (l1, l2) = (w[0].x.to_radians(), w[1].x.to_radians());
                let (p1, p2) = (w[0].y.to_radians(), w[1].y.to_radians());
                let mut dl = l2 - l1;
                if dl > std::f64::consts::PI {
                    dl -= 2.0 * std::f64::consts::PI;
                } else if dl < -std::f64::consts::PI {
                    dl += 2.0 * std::f64::consts::PI;
                }
                // The trapezoid between the segment and the equator (its spherical excess).
                let (t1, t2) = ((p1 / 2.0).tan(), (p2 / 2.0).tan());
                sum += 2.0 * ((dl / 2.0).tan() * (t1 + t2) / (1.0 + t1 * t2)).atan();
            }
            // Clockwise rings (eastward on top) come out positive, like boost's.
            sum
        }
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

fn longitude_distance_unsigned(a: f64, b: f64) -> f64 {
    let d = norm_lon(b - a);
    if d < 0.0 { d + 360.0 } else { d }
}

/// `spherical_box` normalization: longitudes into [-180, 180) with max ≥ min, whole-circle boxes
/// [-180, 180].
pub fn normalize_box(b: &Rect) -> Rect {
    let mut r = *b;
    let diff = b.max.x - b.min.x;
    if diff >= 360.0 {
        r.min.x = -180.0;
        r.max.x = 180.0;
        return r;
    }
    let mut lo = norm_lon(b.min.x);
    let mut hi = norm_lon(b.max.x);
    if lo == 180.0 {
        lo = -180.0;
    }
    if hi < lo || (hi == lo && diff > 0.0) {
        hi += 360.0;
    }
    if b.min.y.abs() == 90.0 && b.max.y == b.min.y {
        lo = 0.0;
        hi = 0.0;
    }
    r.min.x = lo;
    r.max.x = hi;
    r
}

/// `envelope_range_of_boxes` (two boxes): the smallest longitude interval covering both.
fn range_of_boxes(a: &Rect, b: &Rect) -> Rect {
    let boxes = [*a, *b];
    let mut intervals: Vec<(f64, f64)> = Vec::new();
    for bx in &boxes {
        if bx.min.y == 90.0 || bx.max.y == -90.0 {
            continue;
        }
        if bx.max.x > 180.0 {
            intervals.push((bx.min.x, 180.0));
            intervals.push((-180.0, bx.max.x - 360.0));
        } else {
            intervals.push((bx.min.x, bx.max.x));
        }
    }
    let (mut lo, mut hi) = (0.0, 0.0);
    if !intervals.is_empty() {
        lo = intervals.iter().map(|i| i.0).fold(f64::INFINITY, f64::min);
        hi = intervals.iter().map(|i| i.1).fold(f64::NEG_INFINITY, f64::max);
        if hi - lo > 180.0 {
            // `maximum_gap` between the sorted intervals.
            let mut iv = intervals.clone();
            iv.sort_by(|x, y| x.0.total_cmp(&y.0));
            let (mut gap, mut gl, mut gr) = (0.0, 0.0, 0.0);
            let mut reach = iv[0].1;
            for w in iv.iter().skip(1) {
                if w.0 > reach && w.0 - reach > gap {
                    gap = w.0 - reach;
                    gl = reach;
                    gr = w.0;
                }
                reach = reach.max(w.1);
            }
            if gap > 0.0 {
                let wrapped = 360.0 + lo - hi;
                if gap > wrapped {
                    lo = gr;
                    hi = gl + 360.0;
                }
            }
        }
    }
    let min_lat = a.min.y.min(b.min.y);
    let max_lat = a.max.y.max(b.max.y);
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
            let (n1, n2) = (normalize_box(o), normalize_box(b));
            *b = range_of_boxes(&n1, &n2);
        }
    }
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
        Cs::Sphere => {
            let mut acc: Option<Rect> = None;
            for w in r.windows(2) {
                let sb = segment_envelope(w[0], w[1]);
                match &mut acc {
                    None => acc = Some(sb),
                    Some(a) => expand(cs, a, &sb),
                }
            }
            acc.unwrap_or(Rect { min: r[0], max: r[0] })
        }
    }
}

/// A great-circle segment's box: the endpoints' longitudes (the short way round) and the
/// latitudes, extended to the circle's vertex when it lies inside the segment.
pub fn segment_envelope(a: Pt, b: Pt) -> Rect {
    let (mut lon1, mut lon2) = (norm_lon(a.x), norm_lon(b.x));
    let (lat1, lat2) = (a.y, b.y);
    let mut lo = lat1.min(lat2);
    let mut hi = lat1.max(lat2);
    if lon2 < lon1 {
        std::mem::swap(&mut lon1, &mut lon2);
    }
    if lon2 - lon1 > 180.0 {
        let t = lon1 + 360.0;
        lon1 = lon2;
        lon2 = t;
    }
    if lat1.abs() != 90.0 && lat2.abs() != 90.0 && a.x != b.x {
        let (a3, b3) = (unit(a), unit(b));
        let n = cross(a3, b3);
        let nn = dot(n, n).sqrt();
        if nn > 0.0 {
            // The circle's vertex latitude: 90° less the normal's angle from the axis.
            let vlat = (n[2].abs() / nn).acos().to_degrees();
            for (sgn, v) in [(1.0, [-n[0] * n[2], -n[1] * n[2], n[0] * n[0] + n[1] * n[1]]), (-1.0, [n[0] * n[2], n[1] * n[2], -(n[0] * n[0] + n[1] * n[1])])] {
                // The vertex is inside the segment when it is between a and b along the circle.
                let s1 = dot(cross(a3, v), n);
                let s2 = dot(cross(v, b3), n);
                if s1 > 0.0 && s2 > 0.0 {
                    if sgn > 0.0 {
                        hi = hi.max(vlat);
                    } else {
                        lo = lo.min(-vlat);
                    }
                }
            }
        }
    }
    Rect { min: Pt { x: lon1, y: lo }, max: Pt { x: lon2, y: hi } }
}

// ---------------------------------------------------------------------------------------------
// Point and segment relations

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loc {
    Outside,
    Boundary,
    Inside,
}

/// `p` on the segment `a`–`b` (endpoints included).
fn on_segment(cs: Cs, a: Pt, b: Pt, p: Pt) -> bool {
    if equals(cs, a, p) || equals(cs, b, p) {
        return true;
    }
    if side(cs, a, b, p) != 0 {
        return false;
    }
    match cs {
        Cs::Flat => p.x >= a.x.min(b.x) && p.x <= a.x.max(b.x) && p.y >= a.y.min(b.y) && p.y <= a.y.max(b.y),
        Cs::Sphere => direction_code(cs, a, p, b) > 0 && direction_code(cs, b, p, a) > 0,
    }
}

/// The winding test of `p` against a closed ring: inside, on its boundary or outside. On the
/// spheroid the ray goes north along `p`'s meridian (great-circle segments are monotone in
/// longitude), in the plane along +x.
pub fn ring_loc(cs: Cs, r: &[Pt], p: Pt) -> Loc {
    if r.len() < 2 {
        return Loc::Outside;
    }
    let mut wn = 0i32;
    for w in r.windows(2) {
        let (a, b) = (w[0], w[1]);
        if on_segment(cs, a, b, p) {
            return Loc::Boundary;
        }
        match cs {
            Cs::Flat => {
                if a.y <= p.y {
                    if b.y > p.y && side(cs, a, b, p) > 0 {
                        wn += 1;
                    }
                } else if b.y <= p.y && side(cs, a, b, p) < 0 {
                    wn -= 1;
                }
            }
            Cs::Sphere => {
                let ax = p.x + norm_lon(a.x - p.x);
                let bx = p.x + norm_lon(b.x - p.x);
                if (bx - ax).abs() > 180.0 {
                    continue;
                }
                if ax <= p.x && bx > p.x {
                    if side(cs, a, b, p) < 0 {
                        wn += 1;
                    }
                } else if bx <= p.x && ax > p.x && side(cs, a, b, p) > 0 {
                    wn -= 1;
                }
            }
        }
    }
    if wn != 0 { Loc::Inside } else { Loc::Outside }
}

/// A point against a polygon: inside the exterior and outside every hole is inside, on any
/// ring is the boundary.
pub fn poly_loc(cs: Cs, poly: &Poly, p: Pt) -> Loc {
    match ring_loc(cs, &poly.outer.pts, p) {
        Loc::Outside => return Loc::Outside,
        Loc::Boundary => return Loc::Boundary,
        Loc::Inside => {}
    }
    for h in &poly.inners {
        match ring_loc(cs, &h.pts, p) {
            Loc::Inside => return Loc::Outside,
            Loc::Boundary => return Loc::Boundary,
            Loc::Outside => {}
        }
    }
    Loc::Inside
}

/// Whether the closed segments `a`–`b` and `c`–`d` share a point.
pub fn segments_intersect(cs: Cs, a: Pt, b: Pt, c: Pt, d: Pt) -> bool {
    let d1 = side(cs, a, b, c);
    let d2 = side(cs, a, b, d);
    let d3 = side(cs, c, d, a);
    let d4 = side(cs, c, d, b);
    if d1 * d2 < 0 && d3 * d4 < 0 {
        return true;
    }
    (d1 == 0 && on_segment(cs, a, b, c)) || (d2 == 0 && on_segment(cs, a, b, d)) || (d3 == 0 && on_segment(cs, c, d, a)) || (d4 == 0 && on_segment(cs, c, d, b))
}

/// The parameters (along `a`→`b`, in (0, 1)) where `c`–`d` meets the segment: the crossing, or
/// the ends of a collinear overlap.
fn split_params(cs: Cs, a: Pt, b: Pt, c: Pt, d: Pt, out: &mut Vec<f64>) {
    let d1 = side(cs, a, b, c);
    let d2 = side(cs, a, b, d);
    let param = |p: Pt| -> f64 {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        if dx.abs() >= dy.abs() { (p.x - a.x) / dx } else { (p.y - a.y) / dy }
    };
    if d1 == 0 && d2 == 0 {
        for p in [c, d] {
            if on_segment(cs, a, b, p) {
                out.push(param(p));
            }
        }
        return;
    }
    if !segments_intersect(cs, a, b, c, d) {
        return;
    }
    if d1 == 0 {
        out.push(param(c));
        return;
    }
    if d2 == 0 {
        out.push(param(d));
        return;
    }
    let d3 = side(cs, c, d, a);
    let d4 = side(cs, c, d, b);
    if d3 == 0 || d4 == 0 {
        return;
    }
    // A proper crossing: where the line through c and d cuts a→b.
    let den = (b.x - a.x) * (d.y - c.y) - (b.y - a.y) * (d.x - c.x);
    if den != 0.0 {
        out.push(((c.x - a.x) * (d.y - c.y) - (c.y - a.y) * (d.x - c.x)) / den);
    }
}

fn rings(p: &Poly) -> impl Iterator<Item = &Ring> {
    std::iter::once(&p.outer).chain(p.inners.iter())
}

fn edges(p: &Poly) -> impl Iterator<Item = (Pt, Pt)> + '_ {
    rings(p).flat_map(|r| r.pts.windows(2).map(|w| (w[0], w[1])))
}

/// The pieces of the segment `a`→`b` between its meetings with `other`'s boundary: each piece's
/// midpoint, in order.
fn piece_midpoints(cs: Cs, a: Pt, b: Pt, other: &Poly) -> Vec<Pt> {
    let mut ts = vec![0.0, 1.0];
    for (c, d) in edges(other) {
        split_params(cs, a, b, c, d, &mut ts);
    }
    ts.retain(|t| (0.0..=1.0).contains(t));
    ts.sort_by(f64::total_cmp);
    ts.dedup();
    let mut out = Vec::new();
    for w in ts.windows(2) {
        let t = (w[0] + w[1]) / 2.0;
        out.push(Pt { x: a.x + (b.x - a.x) * t, y: a.y + (b.y - a.y) * t });
    }
    out
}

/// `covered_by(a, b)` for polygons: `a`'s boundary in `b`, and no hole of `b` inside `a`.
fn poly_covered_by(cs: Cs, a: &Poly, b: &Poly) -> bool {
    for (p, q) in edges(a) {
        for m in piece_midpoints(cs, p, q, b) {
            if poly_loc(cs, b, m) == Loc::Outside {
                return false;
            }
        }
    }
    for h in &b.inners {
        for w in h.pts.windows(2) {
            for m in piece_midpoints(cs, w[0], w[1], a) {
                if poly_loc(cs, a, m) == Loc::Inside {
                    return false;
                }
            }
        }
    }
    true
}

/// `bg::intersects`.
pub fn intersects(cs: Cs, a: &Geom, b: &Geom) -> bool {
    match (a, b) {
        (Geom::Point(p), Geom::Point(q)) => equals(cs, *p, *q),
        (Geom::Point(p), Geom::Poly(q)) | (Geom::Poly(q), Geom::Point(p)) => poly_loc(cs, q, *p) != Loc::Outside,
        (Geom::Poly(p), Geom::Poly(q)) => {
            for (a1, a2) in edges(p) {
                for (b1, b2) in edges(q) {
                    if segments_intersect(cs, a1, a2, b1, b2) {
                        return true;
                    }
                }
            }
            let inside = |x: &Poly, y: &Poly| x.outer.pts.first().is_some_and(|&f| poly_loc(cs, y, f) != Loc::Outside);
            inside(p, q) || inside(q, p)
        }
    }
}

/// `bg::within(a, b)` with `b` a polygon: no point of `a` outside `b`, interiors meeting.
pub fn within(cs: Cs, a: &Geom, b: &Poly) -> bool {
    match a {
        Geom::Point(p) => poly_loc(cs, b, *p) == Loc::Inside,
        Geom::Poly(a) => poly_covered_by(cs, a, b),
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
