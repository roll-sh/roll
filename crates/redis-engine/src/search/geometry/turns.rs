//! Boost.Geometry's turn computation between polygon rings, as `bg::within`, `bg::intersects` and
//! `bg::is_valid` run it: the segments' intersection (`cartesian_segments`, and
//! `geographic_segments` with Andoyer's inverse and Sjöberg's intersection on the unit spheroid)
//! through the intersection-points and direction policies, `get_turn_info`'s classification of
//! each meeting (touch, touch-interior, crossing, collinear, equal and their opposite variants),
//! `sectionalize`'s monotonic sections, `partition` over their boxes and
//! `get_turns_in_sections` with its interrupt policy.

use super::algo::{self, feq, side};
use super::geographic::{self, Inverse};
use super::{Cs, Pt, Rect};

// ---------------------------------------------------------------------------------------------
// segment_ratio

/// `segment_ratio<double>`: a numerator over a positive denominator, compared through its
/// approximation (scaled by a million) unless close.
#[derive(Clone, Copy, Debug)]
pub struct Ratio {
    pub num: f64,
    pub den: f64,
    approx: f64,
}

impl Default for Ratio {
    fn default() -> Ratio {
        Ratio { num: 0.0, den: 1.0, approx: 0.0 }
    }
}

impl Ratio {
    pub fn new(num: f64, den: f64) -> Ratio {
        let (num, den) = if den < 0.0 { (-num, -den) } else { (num, den) };
        let approx = if den == 0.0 { 0.0 } else { num * 1_000_000.0 / den };
        Ratio { num, den, approx }
    }

    pub fn zero() -> Ratio {
        Ratio::new(0.0, 1.0)
    }

    pub fn one() -> Ratio {
        Ratio::new(1.0, 1.0)
    }

    pub fn is_zero(&self) -> bool {
        feq(self.num, 0.0)
    }

    pub fn on_segment(&self) -> bool {
        self.num >= 0.0 && self.num <= self.den
    }

    fn edge_value(&self) -> f64 {
        let r = 2.0 * (0.5 - self.approx / 1_000_000.0).abs();
        if r > 1.0 { 1.0 } else { r }
    }

    fn possibly_collinear(&self, threshold: f64) -> bool {
        self.den.abs() < threshold
    }

    fn close_to(&self, o: &Ratio) -> bool {
        (self.approx - o.approx).abs() < 50.0
    }

    pub fn lt(&self, o: &Ratio) -> bool {
        if self.close_to(o) {
            let a = self.num / self.den;
            let b = o.num / o.den;
            !feq(a, b) && a < b
        } else {
            self.approx < o.approx
        }
    }

    pub fn eq(&self, o: &Ratio) -> bool {
        self.close_to(o) && feq(self.num / self.den, o.num / o.den)
    }
}

// ---------------------------------------------------------------------------------------------
// The segments' intersection, through `segments_intersection_points` and `segments_direction`

/// The intersection points and direction policies' results for one pair of segments.
#[derive(Clone, Copy, Debug)]
pub struct Inter {
    pub count: usize,
    pub ips: [Pt; 2],
    /// (ratio along p, ratio along q) per intersection point.
    pub fr: [(Ratio, Ratio); 2],
    pub how: u8,
    pub opposite: bool,
    pub how_a: i32,
    pub how_b: i32,
    pub dir_a: i32,
    pub dir_b: i32,
    /// [p's endpoints against q, q's endpoints against p].
    pub sides: [[i32; 2]; 2],
    pub arrival: [i32; 2],
}

impl Inter {
    fn base(how: u8) -> Inter {
        Inter {
            count: 0,
            ips: [Pt::default(); 2],
            fr: [(Ratio::default(), Ratio::default()); 2],
            how,
            opposite: false,
            how_a: 0,
            how_b: 0,
            dir_a: 0,
            dir_b: 0,
            sides: [[0; 2]; 2],
            arrival: [0; 2],
        }
    }

    pub fn disjoint() -> Inter {
        Inter::base(b'd')
    }

    /// `degenerate`: both segments one point.
    fn degenerate(first: Pt) -> Inter {
        let mut r = Inter::base(b'0');
        r.count = 1;
        r.ips[0] = first;
        r
    }

    /// `one_degenerate`: the degenerate segment's point on the other.
    fn one_degenerate(first: Pt, ratio: Ratio, a_degenerate: bool) -> Inter {
        let mut r = Inter::degenerate(first);
        r.fr[0] = if a_degenerate { (Ratio::zero(), ratio) } else { (ratio, Ratio::zero()) };
        r
    }

    /// `segments_crosses` (and `segments_share_common_point`): one point.
    fn crosses(sides: [[i32; 2]; 2], ip: Pt, ra: Ratio, rb: Ratio) -> Inter {
        let mut r = Inter::base(b'i');
        r.count = 1;
        r.ips[0] = ip;
        r.fr[0] = (ra, rb);
        let ra0 = sides[0][0] == 0;
        let ra1 = sides[0][1] == 0;
        let rb0 = sides[1][0] == 0;
        let rb1 = sides[1][1] == 0;
        let d = |s: i32| if s == 1 { 1 } else { -1 };
        let (how, ha, hb, da, db) = if ra0 && rb0 {
            let dir = d(sides[1][1]);
            (b'f', -1, -1, -dir, dir)
        } else if ra1 && rb1 {
            let dir = d(sides[1][0]);
            (b't', 1, 1, -dir, dir)
        } else if ra1 && rb0 {
            let dir = d(sides[1][1]);
            (b'a', 1, -1, dir, dir)
        } else if ra0 && rb1 {
            let dir = d(sides[1][0]);
            (b'a', -1, 1, dir, dir)
        } else if rb0 {
            let dir = d(sides[1][1]);
            (b's', 0, -1, -dir, dir)
        } else if ra0 {
            let dir = -d(sides[1][1]);
            (b's', -1, 0, dir, -dir)
        } else if rb1 {
            let dir = d(sides[0][1]);
            (b'm', 0, 1, dir, dir)
        } else if ra1 {
            let dir = d(sides[1][1]);
            (b'm', 1, 0, dir, dir)
        } else {
            let dir = d(sides[1][1]);
            (b'i', -1, -1, -dir, dir)
        };
        r.how = how;
        r.how_a = ha;
        r.how_b = hb;
        r.dir_a = da;
        r.dir_b = db;
        r.sides = sides;
        r.arrival = [ha, hb];
        r
    }

    /// `segments_collinear` of segments `a` (a1→a2) and `b` (b1→b2) with the endpoints' positions
    /// on the other (0 before, 1 at the start, 2 inside, 3 at the end, 4 after).
    #[allow(clippy::too_many_arguments)]
    fn collinear(a: [Pt; 2], b: [Pt; 2], opposite: bool, pos: [i32; 4], ra_from: Ratio, ra_to: Ratio, rb_from: Ratio, rb_to: Ratio) -> Inter {
        let [a1b, a2b, b1a, b2a] = pos;
        let mut r = Inter::base(b'c');
        r.opposite = opposite;
        let mut on_a = [Ratio::default(); 2];
        let mut n = 0;
        if (1..=3).contains(&a1b) && n < 2 {
            r.ips[n] = a[0];
            r.fr[n] = (Ratio::zero(), ra_from);
            on_a[n] = Ratio::zero();
            n += 1;
        }
        if b1a == 2 && n < 2 {
            r.ips[n] = b[0];
            r.fr[n] = (rb_from, Ratio::zero());
            on_a[n] = rb_from;
            n += 1;
        }
        if (1..=3).contains(&a2b) && n < 2 {
            r.ips[n] = a[1];
            r.fr[n] = (Ratio::one(), ra_to);
            on_a[n] = Ratio::one();
            n += 1;
        }
        if b2a == 2 && n < 2 {
            r.ips[n] = b[1];
            r.fr[n] = (rb_to, Ratio::one());
            on_a[n] = rb_to;
            n += 1;
        }
        if n == 2 && on_a[1].lt(&on_a[0]) {
            r.fr.swap(0, 1);
            r.ips.swap(0, 1);
        }
        r.count = n;
        let arr = |to: i32| if to == 2 { 1 } else if to == 1 || to == 3 { 0 } else { -1 };
        r.arrival = [arr(a2b), arr(b2a)];
        let tally = |v: [i32; 2]| -> (i32, i32) {
            let (mut on_end, mut out) = (0, 0);
            for x in v {
                if x == 1 || x == 3 {
                    on_end += 1;
                } else if x != 2 {
                    out += 1;
                }
            }
            (on_end, out)
        };
        let (a_end, a_out) = tally([a1b, a2b]);
        let (b_end, b_out) = tally([b1a, b2a]);
        if a_end == 1 && b_end == 1 && a_out == 1 && b_out == 1 {
            r.how = if !opposite {
                b'a'
            } else if r.arrival[0] == 0 {
                b't'
            } else {
                b'f'
            };
        } else if a_end == 2 && b_end == 2 {
            r.how = b'e';
        }
        r
    }
}

/// The segments' intersection in the coordinate system.
pub fn intersect(cs: Cs, p1: Pt, p2: Pt, q1: Pt, q2: Pt) -> Inter {
    match cs {
        Cs::Flat => cartesian(p1, p2, q1, q2),
        Cs::Sphere => geographic_segments(p1, p2, q1, q2),
    }
}

fn position_value(ca: f64, cb1: f64, cb2: f64) -> i32 {
    if feq(ca, cb1) {
        1
    } else if feq(ca, cb2) {
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

fn sign(v: f64) -> i32 {
    if v > 0.0 {
        1
    } else if v < 0.0 {
        -1
    } else {
        0
    }
}

/// `cartesian_segments::apply` (`unified`).
fn cartesian(p1: Pt, p2: Pt, q1: Pt, q2: Pt) -> Inter {
    let eq = |a: Pt, b: Pt| algo::equals(Cs::Flat, a, b);
    let p_is_point = eq(p1, p2);
    let q_is_point = eq(q1, q2);
    if p_is_point && q_is_point {
        return if eq(p1, q2) { Inter::degenerate(p1) } else { Inter::disjoint() };
    }
    let disjoint_by_range = |a1: f64, a2: f64, b1: f64, b2: f64| {
        let (mina, maxa) = if a1 > a2 { (a2, a1) } else { (a1, a2) };
        let (minb, maxb) = if b1 > b2 { (b2, b1) } else { (b1, b2) };
        algo::fsmaller(maxa, minb) || algo::fsmaller(maxb, mina)
    };
    if disjoint_by_range(p1.x, p2.x, q1.x, q2.x) || disjoint_by_range(p1.y, p2.y, q1.y, q2.y) {
        return Inter::disjoint();
    }
    let mut sides = [[side(Cs::Flat, q1, q2, p1), side(Cs::Flat, q1, q2, p2)], [0, 0]];
    if sides[0][0] * sides[0][1] == 1 {
        return Inter::disjoint();
    }
    sides[1] = [side(Cs::Flat, p1, p2, q1), side(Cs::Flat, p1, p2, q2)];
    if sides[1][0] * sides[1][1] == 1 {
        return Inter::disjoint();
    }
    let (dx_a, dy_a, dx_b, dy_b) = (p2.x - p1.x, p2.y - p1.y, q2.x - q1.x, q2.y - q1.y);
    let mut collinear = sides == [[0, 0], [0, 0]];
    let (mut ra, mut rb) = (Ratio::default(), Ratio::default());
    if !collinear {
        let det = algo::determinant;
        let (wx, wy) = (p1.x - q1.x, p1.y - q1.y);
        let nom_a = det(dx_b, dy_b, wx, wy);
        let den_a = det(dx_a, dy_a, dx_b, dy_b);
        let (wx2, wy2) = (q1.x - p1.x, q1.y - p1.y);
        let nom_b = det(dx_a, dy_a, wx2, wy2);
        let den_b = det(dx_b, dy_b, dx_a, dy_a);
        let factor = dx_a.abs().max(dy_a.abs()).max(dx_b.abs()).max(dy_b.abs()).max(1.0);
        let zero = |v: f64| v == 0.0 || (v.is_finite() && v.abs() <= f64::EPSILON * factor);
        if zero(den_a) || zero(den_b) {
            sides = [[0, 0], [0, 0]];
            collinear = true;
        } else {
            ra = Ratio::new(nom_a, den_a);
            rb = Ratio::new(nom_b, den_b);
        }
    }
    if collinear {
        let (adx_a, ady_a, adx_b, ady_b) = (dx_a.abs(), dy_a.abs(), dx_b.abs(), dy_b.abs());
        let (use_x, ok) = if p_is_point {
            (adx_b >= ady_b, true)
        } else if q_is_point {
            (adx_a >= ady_a, true)
        } else {
            let (min_dx, min_dy) = (adx_a.min(adx_b), ady_a.min(ady_b));
            if min_dx == min_dy { (true, min_dx > 0.0) } else { (min_dx > min_dy, true) }
        };
        if ok {
            let c = |p: Pt| if use_x { p.x } else { p.y };
            if p_is_point {
                let r = Ratio::new(c(p1) - c(q1), c(q2) - c(q1));
                return if r.on_segment() { Inter::one_degenerate(p1, r, true) } else { Inter::disjoint() };
            }
            if q_is_point {
                let r = Ratio::new(c(q1) - c(p1), c(p2) - c(p1));
                return if r.on_segment() { Inter::one_degenerate(q1, r, false) } else { Inter::disjoint() };
            }
            let (oa1, oa2, ob1, ob2) = (c(p1), c(p2), c(q1), c(q2));
            let len_a = oa2 - oa1;
            let len_b = ob2 - ob1;
            let mut ra_from = Ratio::new(oa1 - ob1, len_b);
            let mut ra_to = Ratio::new(oa2 - ob1, len_b);
            let mut rb_from = Ratio::new(ob1 - oa1, len_a);
            let mut rb_to = Ratio::new(ob2 - oa1, len_a);
            let a1b = position_value(oa1, ob1, ob2);
            let a2b = position_value(oa2, ob1, ob2);
            let b1a = position_value(ob1, oa1, oa2);
            let b2a = position_value(ob2, oa1, oa2);
            if a1b == 1 {
                ra_from = Ratio::zero();
                rb_from = Ratio::zero();
            } else if a1b == 3 {
                ra_from = Ratio::one();
                rb_to = Ratio::zero();
            }
            if a2b == 1 {
                ra_to = Ratio::zero();
                rb_from = Ratio::one();
            } else if a2b == 3 {
                ra_to = Ratio::one();
                rb_to = Ratio::one();
            }
            if (a1b < 1 && a2b < 1) || (a1b > 3 && a2b > 3) {
                return Inter::disjoint();
            }
            let opposite = sign(len_a) != sign(len_b);
            return Inter::collinear([p1, p2], [q1, q2], opposite, [a1b, a2b, b1a, b2a], ra_from, ra_to, rb_from, rb_to);
        }
    }
    if eq(p1, q1) || eq(p1, q2) {
        return Inter::crosses(sides, p1, ra, rb);
    }
    if eq(p2, q1) || eq(p2, q2) {
        return Inter::crosses(sides, p2, ra, rb);
    }
    // `segment_intersection_info::calculate`
    let (cla, clb) = (dx_a.mul_add(dx_a, dy_a * dy_a), dx_b.mul_add(dx_b, dy_b * dy_b));
    let clm = cla.max(clb);
    let use_a = if clm <= 0.0 {
        true
    } else {
        let (rcla, rclb) = (1.0 - cla / clm, 1.0 - clb / clm);
        5.0f64.mul_add(ra.edge_value(), rcla) > 5.0f64.mul_add(rb.edge_value(), rclb)
    };
    let mut ip = if use_a {
        Pt { x: p1.x + (ra.num * dx_a) / ra.den, y: p1.y + (ra.num * dy_a) / ra.den }
    } else {
        Pt { x: q1.x + (rb.num * dx_b) / rb.den, y: q1.y + (rb.num * dy_b) / rb.den }
    };
    if ra.possibly_collinear(1.0e-3) && rb.possibly_collinear(1.0e-3) {
        assign_if_exceeds(&mut ip, p1, p2);
        assign_if_exceeds(&mut ip, q1, q2);
    }
    Inter::crosses(sides, ip, ra, rb)
}

fn assign_if_exceeds(pt: &mut Pt, s0: Pt, s1: Pt) {
    let exceeds = |p: Pt, a: Pt, b: Pt| -> bool {
        let dim = |c: f64, c0: f64, c1: f64| {
            if c0 < c1 {
                algo::fsmaller(c, c0)
            } else if c0 > c1 {
                c > c0 && !feq(c, c0)
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

fn sph_eq(a: Pt, b: Pt) -> bool {
    algo::equals(Cs::Sphere, a, b)
}

fn same_direction(az1: f64, az2: f64) -> bool {
    geographic::lon_dist_rad(az1, az2).abs() <= std::f64::consts::FRAC_PI_2
}

fn is_endpoint_equal(dist: f64, ai: Pt, b1: Pt) -> bool {
    dist.abs() <= 0.00000001 && (feq(dist, 0.0) || sph_eq(ai, b1))
}

fn inv(a: Pt, b: Pt) -> Inverse {
    geographic::inverse_unit(a.x.to_radians(), a.y.to_radians(), b.x.to_radians(), b.y.to_radians())
}

/// `geographic_segments::calculate_collinear_data`: whether `b1` is on `a`, with a's length and
/// b1's signed distance along it.
#[allow(clippy::too_many_arguments)]
fn collinear_data(a1: Pt, a2: Pt, b1: Pt, r12: &Inverse, r1b1: &Inverse, r1b2: &Inverse, degen_neq_coords: bool) -> (bool, f64, f64) {
    let d12 = r12.distance;
    let mut d1b1 = r1b1.distance;
    if !same_direction(r1b1.azimuth, r12.azimuth) {
        d1b1 = -d1b1;
    }
    if is_endpoint_equal(d1b1, a1, b1) {
        return (true, d12, 0.0);
    }
    if is_endpoint_equal(d12 - d1b1, a2, b1) {
        return (true, d12, d12);
    }
    if degen_neq_coords {
        if feq(r1b2.distance, 0.0) {
            return (true, d12, 0.0);
        }
        if feq(d12 - r1b2.distance, 0.0) {
            return (true, d12, d12);
        }
    }
    (Ratio::new(d1b1, d12).on_segment(), d12, d1b1)
}

/// `geographic_segments::apply` for segments `p0`→`p1` and `q0`→`q1` (degrees): each ordered
/// south to north for the computation, the results mapped back to the original directions.
fn geographic_segments(p0: Pt, p1: Pt, q0: Pt, q1: Pt) -> Inter {
    let a_rev = p0.y > p1.y;
    let b_rev = q0.y > q1.y;
    let (a1, a2) = if a_rev { (p1, p0) } else { (p0, p1) };
    let (b1, b2) = if b_rev { (q1, q0) } else { (q0, q1) };
    let mut a_is_point = sph_eq(a1, a2);
    let mut b_is_point = sph_eq(b1, b2);
    if a_is_point && b_is_point {
        return if sph_eq(a1, b2) { Inter::degenerate(p0) } else { Inter::disjoint() };
    }
    let mut sides = [[0i32; 2]; 2];
    let mut is_equal_a1_b1 = sph_eq(a1, b1);
    let mut is_equal_a2_b1 = sph_eq(a2, b1);
    let mut degen_neq_coords = false;
    let (mut res_b1_b2, mut res_b1_a1, mut res_b1_a2) = (Inverse::default(), Inverse::default(), Inverse::default());
    if !b_is_point {
        res_b1_b2 = inv(b1, b2);
        if feq(res_b1_b2.distance, 0.0) {
            b_is_point = true;
            degen_neq_coords = true;
        } else {
            res_b1_a1 = inv(b1, a1);
            if feq(res_b1_a1.distance, 0.0) {
                is_equal_a1_b1 = true;
            }
            res_b1_a2 = inv(b1, a2);
            if feq(res_b1_a2.distance, 0.0) {
                is_equal_a2_b1 = true;
            }
            sides[0] = [
                if is_equal_a1_b1 { 0 } else { algo::azimuth_side_value(res_b1_a1.azimuth, res_b1_b2.azimuth) },
                if is_equal_a2_b1 { 0 } else { algo::azimuth_side_value(res_b1_a2.azimuth, res_b1_b2.azimuth) },
            ];
            if sides[0][0] * sides[0][1] == 1 {
                return Inter::disjoint();
            }
        }
    }
    let mut is_equal_a1_b2 = sph_eq(a1, b2);
    let (mut res_a1_a2, mut res_a1_b1, mut res_a1_b2) = (Inverse::default(), Inverse::default(), Inverse::default());
    if !a_is_point {
        res_a1_a2 = inv(a1, a2);
        if feq(res_a1_a2.distance, 0.0) {
            a_is_point = true;
            degen_neq_coords = true;
        } else {
            res_a1_b1 = inv(a1, b1);
            if feq(res_a1_b1.distance, 0.0) {
                is_equal_a1_b1 = true;
            }
            res_a1_b2 = inv(a1, b2);
            if feq(res_a1_b2.distance, 0.0) {
                is_equal_a1_b2 = true;
            }
            sides[1] = [
                if is_equal_a1_b1 { 0 } else { algo::azimuth_side_value(res_a1_b1.azimuth, res_a1_a2.azimuth) },
                if is_equal_a1_b2 { 0 } else { algo::azimuth_side_value(res_a1_b2.azimuth, res_a1_a2.azimuth) },
            ];
            if sides[1][0] * sides[1][1] == 1 {
                return Inter::disjoint();
            }
        }
    }
    if a_is_point && b_is_point {
        return if is_equal_a1_b2 { Inter::degenerate(p0) } else { Inter::disjoint() };
    }
    let mut collinear = sides == [[0, 0], [0, 0]];
    if !collinear {
        if sides[0] == [0, 0] {
            collinear = true;
            sides[1] = [0, 0];
        } else if sides[1] == [0, 0] {
            collinear = true;
            sides[0] = [0, 0];
        }
    }
    if collinear {
        if a_is_point {
            // `collinear_one_degenerated(a, true, b1, b2, a1, a2, …)`
            let (ok, d12, mut d1o) = collinear_data(b1, b2, a1, &res_b1_b2, &res_b1_a1, &res_b1_a2, degen_neq_coords);
            if !ok {
                return Inter::disjoint();
            }
            if b_rev {
                d1o = d12 - d1o;
            }
            return Inter::one_degenerate(p0, Ratio::new(d1o, d12), true);
        }
        if b_is_point {
            let (ok, d12, mut d1o) = collinear_data(a1, a2, b1, &res_a1_a2, &res_a1_b1, &res_a1_b2, degen_neq_coords);
            if !ok {
                return Inter::disjoint();
            }
            if a_rev {
                d1o = d12 - d1o;
            }
            return Inter::one_degenerate(q0, Ratio::new(d1o, d12), false);
        }
        let (dist_a1_a2, mut dist_a1_b1, mut dist_a1_b2, dist_b1_b2, mut dist_b1_a1, mut dist_b1_a2);
        if res_a1_a2.distance <= res_b1_b2.distance {
            let (_, d12, d1b1) = collinear_data(a1, a2, b1, &res_a1_a2, &res_a1_b1, &res_a1_b2, false);
            let (_, _, d1b2) = collinear_data(a1, a2, b2, &res_a1_a2, &res_a1_b2, &res_a1_b1, false);
            dist_a1_a2 = d12;
            dist_a1_b1 = d1b1;
            dist_a1_b2 = d1b2;
            dist_b1_b2 = dist_a1_b2 - dist_a1_b1;
            dist_b1_a1 = -dist_a1_b1;
            dist_b1_a2 = dist_a1_a2 - dist_a1_b1;
        } else {
            let (_, d12, d1a1) = collinear_data(b1, b2, a1, &res_b1_b2, &res_b1_a1, &res_b1_a2, false);
            let (_, _, d1a2) = collinear_data(b1, b2, a2, &res_b1_b2, &res_b1_a2, &res_b1_a1, false);
            dist_b1_b2 = d12;
            dist_b1_a1 = d1a1;
            dist_b1_a2 = d1a2;
            dist_a1_a2 = dist_b1_a2 - dist_b1_a1;
            dist_a1_b1 = -dist_b1_a1;
            dist_a1_b2 = dist_b1_b2 - dist_b1_a1;
        }
        let mut a1_on_b = position_value(0.0, dist_a1_b1, dist_a1_b2);
        let mut a2_on_b = position_value(dist_a1_a2, dist_a1_b1, dist_a1_b2);
        let mut b1_on_a = position_value(0.0, dist_b1_a1, dist_b1_a2);
        let mut b2_on_a = position_value(dist_b1_b2, dist_b1_a1, dist_b1_a2);
        if (a1_on_b < 1 && a2_on_b < 1) || (a1_on_b > 3 && a2_on_b > 3) {
            return Inter::disjoint();
        }
        if a1_on_b == 1 {
            dist_b1_a1 = 0.0;
            dist_a1_b1 = 0.0;
        } else if a1_on_b == 3 {
            dist_b1_a1 = dist_b1_b2;
            dist_a1_b2 = 0.0;
        }
        if a2_on_b == 1 {
            dist_b1_a2 = 0.0;
            dist_a1_b1 = dist_a1_a2;
        } else if a2_on_b == 3 {
            dist_b1_a2 = dist_b1_b2;
            dist_a1_b2 = dist_a1_a2;
        }
        let mut opposite = !same_direction(res_a1_a2.azimuth, res_b1_b2.azimuth);
        if a_rev {
            opposite = !opposite;
            std::mem::swap(&mut a1_on_b, &mut a2_on_b);
            b1_on_a = 4 - b1_on_a;
            b2_on_a = 4 - b2_on_a;
            std::mem::swap(&mut dist_b1_a1, &mut dist_b1_a2);
            dist_a1_b1 = dist_a1_a2 - dist_a1_b1;
            dist_a1_b2 = dist_a1_a2 - dist_a1_b2;
        }
        if b_rev {
            opposite = !opposite;
            a1_on_b = 4 - a1_on_b;
            a2_on_b = 4 - a2_on_b;
            std::mem::swap(&mut b1_on_a, &mut b2_on_a);
            dist_b1_a1 = dist_b1_b2 - dist_b1_a1;
            dist_b1_a2 = dist_b1_b2 - dist_b1_a2;
            std::mem::swap(&mut dist_a1_b1, &mut dist_a1_b2);
        }
        return Inter::collinear(
            [p0, p1],
            [q0, q1],
            opposite,
            [a1_on_b, a2_on_b, b1_on_a, b2_on_a],
            Ratio::new(dist_b1_a1, dist_b1_b2),
            Ratio::new(dist_b1_a2, dist_b1_b2),
            Ratio::new(dist_a1_b1, dist_a1_a2),
            Ratio::new(dist_a1_b2, dist_a1_a2),
        );
    }
    if a_is_point || b_is_point {
        return Inter::disjoint();
    }
    let Some(mut ipd) = ip_data(a1, a2, b1, b2, &res_a1_a2, &res_a1_b1, &res_a1_b2, &res_b1_b2, &res_b1_a1, &res_b1_a2, &sides) else {
        return Inter::disjoint();
    };
    if a_rev {
        sides[0].swap(0, 1);
        sides[1] = [-sides[1][0], -sides[1][1]];
        ipd.dist_a1_ip = ipd.dist_a1_a2 - ipd.dist_a1_ip;
        ipd.flag = match ipd.flag {
            IpFlag::AtA1 => IpFlag::AtA2,
            IpFlag::AtA2 => IpFlag::AtA1,
            f => f,
        };
    }
    if b_rev {
        sides[1].swap(0, 1);
        sides[0] = [-sides[0][0], -sides[0][1]];
        ipd.dist_b1_ip = ipd.dist_b1_b2 - ipd.dist_b1_ip;
        ipd.flag = match ipd.flag {
            IpFlag::AtB1 => IpFlag::AtB2,
            IpFlag::AtB2 => IpFlag::AtB1,
            f => f,
        };
    }
    let ip = match ipd.flag {
        IpFlag::Inters => Pt { x: ipd.lon.to_degrees(), y: ipd.lat.to_degrees() },
        IpFlag::AtA1 => p0,
        IpFlag::AtA2 => p1,
        IpFlag::AtB1 => q0,
        IpFlag::AtB2 => q1,
    };
    Inter::crosses(sides, ip, Ratio::new(ipd.dist_a1_ip, ipd.dist_a1_a2), Ratio::new(ipd.dist_b1_ip, ipd.dist_b1_b2))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IpFlag {
    Inters,
    AtA1,
    AtA2,
    AtB1,
    AtB2,
}

struct IpData {
    lon: f64,
    lat: f64,
    dist_a1_a2: f64,
    dist_a1_ip: f64,
    dist_b1_b2: f64,
    dist_b1_ip: f64,
    flag: IpFlag,
}

/// `geographic_segments::calculate_ip_data`.
#[allow(clippy::too_many_arguments)]
fn ip_data(
    a1: Pt,
    a2: Pt,
    b1: Pt,
    b2: Pt,
    res_a1_a2: &Inverse,
    res_a1_b1: &Inverse,
    res_a1_b2: &Inverse,
    res_b1_b2: &Inverse,
    res_b1_a1: &Inverse,
    res_b1_a2: &Inverse,
    sides: &[[i32; 2]; 2],
) -> Option<IpData> {
    let r = |p: Pt| (p.x.to_radians(), p.y.to_radians());
    let ((a1_lon, a1_lat), (a2_lon, a2_lat), (b1_lon, b1_lat), (b2_lon, b2_lat)) = (r(a1), r(a2), r(b1), r(b2));
    let dist_a1_a2 = res_a1_a2.distance;
    let dist_b1_b2 = res_b1_b2.distance;
    let mk = |lon: f64, lat: f64, dist_a1_ip: f64, dist_b1_ip: f64, flag: IpFlag| Some(IpData { lon, lat, dist_a1_a2, dist_a1_ip, dist_b1_b2, dist_b1_ip, flag });
    if sph_eq(a1, b1) {
        return mk(a1_lon, a1_lat, 0.0, 0.0, IpFlag::AtA1);
    } else if sph_eq(a1, b2) {
        return mk(a1_lon, a1_lat, 0.0, dist_b1_b2, IpFlag::AtA1);
    } else if sph_eq(a2, b1) {
        return mk(a2_lon, a2_lat, dist_a1_a2, 0.0, IpFlag::AtA2);
    } else if sph_eq(a2, b2) {
        return mk(a2_lon, a2_lat, dist_a1_a2, dist_b1_b2, IpFlag::AtA2);
    }
    if sides[0][0] == 0 {
        if res_b1_a1.distance <= res_b1_b2.distance && same_direction(res_b1_a1.azimuth, res_b1_b2.azimuth) {
            return mk(a1_lon, a1_lat, 0.0, res_b1_a1.distance, IpFlag::AtA1);
        }
        return None;
    } else if sides[0][1] == 0 {
        if res_b1_a2.distance <= res_b1_b2.distance && same_direction(res_b1_a2.azimuth, res_b1_b2.azimuth) {
            return mk(a2_lon, a2_lat, res_a1_a2.distance, res_b1_a2.distance, IpFlag::AtA2);
        }
        return None;
    } else if sides[1][0] == 0 {
        if res_a1_b1.distance <= res_a1_a2.distance && same_direction(res_a1_b1.azimuth, res_a1_a2.azimuth) {
            return mk(b1_lon, b1_lat, res_a1_b1.distance, 0.0, IpFlag::AtB1);
        }
        return None;
    } else if sides[1][1] == 0 {
        if res_a1_b2.distance <= res_a1_a2.distance && same_direction(res_a1_b2.azimuth, res_a1_a2.azimuth) {
            return mk(b2_lon, b2_lat, res_a1_b2.distance, res_b1_b2.distance, IpFlag::AtB2);
        }
        return None;
    }
    let (mut lon, mut lat) = geographic::sjoberg_intersection(a1_lon, a1_lat, a2_lon, a2_lat, res_a1_a2.azimuth, b1_lon, b1_lat, b2_lon, b2_lat, res_b1_b2.azimuth)?;
    let res_a1_ip = geographic::inverse_unit(a1_lon, a1_lat, lon, lat);
    let mut dist_a1_ip = res_a1_ip.distance;
    if !same_direction(res_a1_ip.azimuth, res_a1_a2.azimuth) {
        dist_a1_ip = -dist_a1_ip;
    }
    let is_on_a = Ratio::new(dist_a1_ip, dist_a1_a2).on_segment();
    let is_on_a1 = feq(lon, a1_lon) && feq(lat, a1_lat);
    let is_on_a2 = feq(lon, a2_lon) && feq(lat, a2_lat);
    if !(is_on_a || is_on_a1 || is_on_a2) {
        return None;
    }
    let res_b1_ip = geographic::inverse_unit(b1_lon, b1_lat, lon, lat);
    let mut dist_b1_ip = res_b1_ip.distance;
    if !same_direction(res_b1_ip.azimuth, res_b1_b2.azimuth) {
        dist_b1_ip = -dist_b1_ip;
    }
    let is_on_b = Ratio::new(dist_b1_ip, dist_b1_b2).on_segment();
    let is_on_b1 = feq(lon, b1_lon) && feq(lat, b1_lat);
    let is_on_b2 = feq(lon, b2_lon) && feq(lat, b2_lat);
    if !(is_on_b || is_on_b1 || is_on_b2) {
        return None;
    }
    let mut flag = IpFlag::Inters;
    if is_on_b1 {
        (lon, lat) = (b1_lon, b1_lat);
        dist_a1_ip = geographic::inverse_unit(a1_lon, a1_lat, lon, lat).distance;
        dist_b1_ip = 0.0;
        flag = IpFlag::AtB1;
    } else if is_on_b2 {
        (lon, lat) = (b2_lon, b2_lat);
        dist_a1_ip = geographic::inverse_unit(a1_lon, a1_lat, lon, lat).distance;
        dist_b1_ip = res_b1_b2.distance;
        flag = IpFlag::AtB2;
    }
    if is_on_a1 {
        (lon, lat) = (a1_lon, a1_lat);
        dist_a1_ip = 0.0;
        dist_b1_ip = geographic::inverse_unit(b1_lon, b1_lat, lon, lat).distance;
        flag = IpFlag::AtA1;
    } else if is_on_a2 {
        (lon, lat) = (a2_lon, a2_lat);
        dist_a1_ip = res_a1_a2.distance;
        dist_b1_ip = geographic::inverse_unit(b1_lon, b1_lat, lon, lat).distance;
        flag = IpFlag::AtA2;
    }
    mk(lon, lat, dist_a1_ip, dist_b1_ip, flag)
}

// ---------------------------------------------------------------------------------------------
// get_turn_info

pub const UNION: u8 = b'u';
pub const INTER: u8 = b'i';
pub const BLOCKED: u8 = b'x';
pub const CONTINUE: u8 = b'c';
pub const OPPOSITE: u8 = b'o';
pub const NONE: u8 = b'-';

/// `segment_identifier` (the piece index is always -1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct SegId {
    pub source: i32,
    pub multi: i32,
    pub ring: i32,
    pub segment: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct Op {
    pub seg: SegId,
    pub op: u8,
    pub fraction: Ratio,
}

#[derive(Clone, Copy, Debug)]
pub struct Turn {
    pub point: Pt,
    pub method: u8,
    pub ops: [Op; 2],
}

/// Which extra turns `get_turn_info` emits (the assign policy).
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub no_turn: bool,
    pub degenerate: bool,
    pub opposite: bool,
}

/// `assign_null_policy` (and relate's `assign_policy<false>`).
pub const NULL_POLICY: Policy = Policy { no_turn: false, degenerate: false, opposite: false };
/// `assign_disjoint_policy`.
pub const DISJOINT_POLICY: Policy = Policy { no_turn: true, degenerate: true, opposite: true };

/// A segment with the next distinct point after its end (an areal unique sub-range).
#[derive(Clone, Copy, Debug)]
pub struct Sub {
    pub i: Pt,
    pub j: Pt,
    pub k: Pt,
}

impl Sub {
    fn at(&self, n: usize) -> Pt {
        match n {
            0 => self.i,
            1 => self.j,
            _ => self.k,
        }
    }
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

fn opposite(a: i32, b: i32) -> bool {
    a * b == -1
}

fn same(a: i32, b: i32) -> bool {
    a * b == 1
}

fn both(ti: &mut Turn, op: u8) {
    ti.ops[0].op = op;
    ti.ops[1].op = op;
}

fn ui_else_iu(cond: bool, ti: &mut Turn) {
    ti.ops[0].op = if cond { UNION } else { INTER };
    ti.ops[1].op = if cond { INTER } else { UNION };
}

fn uu_else_ii(cond: bool, ti: &mut Turn) {
    both(ti, if cond { UNION } else { INTER });
}

fn assign_point(ti: &mut Turn, method: u8, inf: &Inter, index: usize) {
    ti.method = method;
    ti.point = inf.ips[index];
    ti.ops[0].fraction = inf.fr[index].0;
    ti.ops[1].fraction = inf.fr[index].1;
}

fn assign_point_and_correct(ti: &mut Turn, method: u8, inf: &Inter) {
    ti.method = method;
    ti.point = inf.ips[0];
    for i in 0..2 {
        ti.ops[i].fraction = match inf.arrival[i] {
            1 => Ratio::new(1.0, 1.0),
            -1 => Ratio::new(0.0, 1.0),
            _ => {
                if i == 0 {
                    inf.fr[0].0
                } else {
                    inf.fr[0].1
                }
            }
        };
    }
}

fn non_opposite_to_index(inf: &Inter) -> usize {
    if inf.fr[0].1.lt(&inf.fr[1].1) { 1 } else { 0 }
}

/// `turn_info_verification_functions::distance_measure`: the squared distance.
fn dist2(a: Pt, b: Pt) -> f64 {
    let (dx, dy) = (a.x - b.x, a.y - b.y);
    dx.mul_add(dx, dy * dy)
}

/// `get_distance_measure`: `p`'s side value against the infinite line through `p1` and `p2`
/// (zero when `p` is one of them, or off the plane).
fn distance_measure(cs: Cs, p1: Pt, p2: Pt, p: Pt) -> f64 {
    let identical = |a: Pt, b: Pt| a.x == b.x && a.y == b.y;
    if identical(p1, p) || identical(p2, p) {
        return 0.0;
    }
    algo::distance_measure(cs, p1, p2, p)
}

/// `set_both_verified` (`both_collinear` with side verification).
fn both_collinear(cs: Cs, rp: &Sub, rq: &Sub, index_p: usize, ip: usize, iq: usize, ti: &mut Turn) {
    let index_q = 1 - index_p;
    let dp = dist2(ti.point, rp.at(ip));
    let dq = dist2(ti.point, rq.at(iq));
    let p_closer = dp < dq;
    let dm = if p_closer { distance_measure(cs, rq.at(iq - 1), rq.at(iq), rp.at(ip)) } else { distance_measure(cs, rp.at(ip - 1), rp.at(ip), rq.at(iq)) };
    if dm != 0.0 {
        let p_left = if p_closer { dm > 0.0 } else { dm < 0.0 };
        ti.ops[index_p].op = if p_left { UNION } else { INTER };
        ti.ops[index_q].op = if p_left { INTER } else { UNION };
        return;
    }
    both(ti, CONTINUE);
}

fn verified_side(cs: Cs, s: i32, rp: &Sub, rq: &Sub, ip: usize, iq: usize) -> i32 {
    if s == 0 {
        let dm = distance_measure(cs, rp.at(ip), rp.at(ip + 1), rq.at(iq));
        return sign(dm);
    }
    s
}

/// `touch_interior::handle_as_touch`.
fn as_touch(inf: &Inter, side: &Sides, non_touching: &Sub) -> bool {
    if same(side.pj_wrt_q1(), side.qj_wrt_p2()) || same(side.pj_wrt_q2(), side.qj_wrt_p1()) {
        return false;
    }
    feq(dist2(inf.ips[0], non_touching.j), 0.0)
}

/// `touch_interior::apply<Index>`: `side.p` is the segment touched in its interior.
fn touch_interior(cs: Cs, ti: &mut Turn, inf: &Inter, side: &Sides, index: usize) {
    assign_point_and_correct(ti, b'm', inf);
    let index_p = index;
    let index_q = 1 - index;
    let side_qi_p = inf.sides[index_q][0];
    let side_qk_p = side.qk_wrt_p1();
    if side_qi_p == -side_qk_p {
        let i = if side_qk_p == -1 { index_p } else { index_q };
        ti.ops[i].op = UNION;
        ti.ops[1 - i].op = INTER;
        return;
    }
    let side_qk_q = side.qk_wrt_q1();
    let side_pj_q2 = side.pj_wrt_q2();
    if side_qi_p == -1 && side_qk_p == -1 && side_qk_q == 1 {
        both(ti, INTER);
    } else if side_qi_p == 1 && side_qk_p == 1 && side_qk_q == -1 {
        if side_pj_q2 == -1 {
            both(ti, UNION);
        } else {
            ti.ops[index_p].op = UNION;
            ti.ops[index_q].op = BLOCKED;
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
        ti.ops[i].op = UNION;
        ti.ops[1 - i].op = INTER;
    } else if side_qk_p == 0 {
        if side_qk_q == side_qi_p {
            both_collinear(cs, side.p, side.q, index_p, 1, 2, ti);
        } else {
            ti.ops[index_p].op = if side_qk_q == 1 { INTER } else { UNION };
            ti.ops[index_q].op = BLOCKED;
        }
    } else {
        ti.method = b'!';
    }
}

/// `touch::apply`.
fn touch(cs: Cs, ti: &mut Turn, inf: &Inter, side: &Sides) {
    assign_point_and_correct(ti, b't', inf);
    let (p, q) = (side.p, side.q);
    let side_pk_q1 = side.pk_wrt_q1();
    let side_qi_p1 = verified_side(cs, inf.sides[1][0], p, q, 0, 0);
    let side_qk_p1 = verified_side(cs, side.qk_wrt_p1(), p, q, 0, 2);
    if !opposite(side_qi_p1, side_qk_p1) {
        let side_pk_q2 = side.pk_wrt_q2();
        let side_pk_p = side.pk_wrt_p1();
        let side_qk_q = side.qk_wrt_q1();
        let q_turns_left = side_qk_q == 1;
        let block_q = side_qk_p1 == 0 && !same(side_qi_p1, side_qk_q);
        if side_pk_p == side_qi_p1 || side_pk_p == side_qk_p1 || (side_qi_p1 == 0 && side_qk_p1 == 0 && side_pk_p != -1) {
            if side_qk_p1 == 0 && side_pk_q1 == 0 && opposite(side.pi_wrt_q1(), side.qk_wrt_p2()) {
                // `handle_imperfect_touch`
                let has_distance = |r1: &Sub, r2: &Sub| distance_measure(cs, r1.i, r1.j, r2.j) > 0.0 && distance_measure(cs, r2.j, r2.k, r1.i) > 0.0;
                if side_pk_q2 == -1 && has_distance(p, q) {
                    ti.ops[0].op = BLOCKED;
                    ti.ops[1].op = UNION;
                    return;
                }
                if side_pk_q2 == 1 && has_distance(q, p) {
                    ti.ops[0].op = UNION;
                    ti.ops[1].op = BLOCKED;
                    return;
                }
            }
            if side_pk_q2 == 0 && !block_q {
                both_collinear(cs, p, q, 0, 2, 2, ti);
                return;
            }
            if side_pk_q1 == 0 {
                ti.ops[0].op = BLOCKED;
                ti.ops[1].op = if block_q {
                    BLOCKED
                } else if q_turns_left {
                    INTER
                } else {
                    UNION
                };
                return;
            }
            if side_pk_q1 == side_pk_q2 && !opposite(side_pk_q1, side_qk_q) {
                ui_else_iu(q_turns_left, ti);
                if block_q {
                    ti.ops[1].op = BLOCKED;
                }
                return;
            }
            if side_pk_q2 == -side_qk_q {
                ui_else_iu(!q_turns_left, ti);
                return;
            }
            if side_pk_q1 == -side_qk_q {
                uu_else_ii(!q_turns_left, ti);
                if block_q {
                    ti.ops[1].op = BLOCKED;
                }
            }
        } else {
            ti.ops[0].op = if q_turns_left { INTER } else { UNION };
            ti.ops[1].op = if block_q {
                BLOCKED
            } else if side_qi_p1 == 1 || side_qk_p1 == 1 {
                UNION
            } else {
                INTER
            };
        }
    } else {
        let side_pk_p = verified_side(cs, side.pk_wrt_p1(), p, p, 0, 2);
        let right_to_left = side_qk_p1 == 1;
        if side_pk_p == side_qi_p1 {
            if side_pk_q1 == 0 {
                ti.ops[0].op = BLOCKED;
                ti.ops[1].op = if right_to_left { UNION } else { INTER };
                return;
            }
            if side_pk_q1 == side_qk_p1 {
                uu_else_ii(right_to_left, ti);
                return;
            }
        }
        if side_pk_p == side_qk_p1 {
            let side_pk_q2 = side.pk_wrt_q2();
            if side_pk_q2 == 0 {
                both(ti, CONTINUE);
                return;
            }
            if side_pk_q2 == side_qk_p1 {
                ui_else_iu(right_to_left, ti);
                return;
            }
        }
        ui_else_iu(!right_to_left, ti);
    }
}

/// `equal::apply`.
fn equal(cs: Cs, ti: &mut Turn, inf: &Inter, side: &Sides) {
    assign_point(ti, b'e', inf, non_opposite_to_index(inf));
    let side_pk_q2 = side.pk_wrt_q2();
    let side_pk_p = side.pk_wrt_p1();
    let side_qk_p = side.qk_wrt_p1();
    if side_pk_p == side_qk_p {
        let side_qk_p2 = side.qk_wrt_p2();
        if opposite(side_qk_p2, side_pk_q2) {
            ui_else_iu(side_pk_q2 == 1, ti);
            return;
        }
    }
    if side_pk_q2 == 0 && side_pk_p == side_qk_p {
        both_collinear(cs, side.p, side.q, 0, 2, 2, ti);
        return;
    }
    if !opposite(side_pk_p, side_qk_p) {
        ui_else_iu(side_pk_q2 != -1, ti);
    } else {
        ui_else_iu(side_pk_p != -1, ti);
    }
}

/// `collinear::handle_as_equal`.
fn collinear_as_equal(inf: &Inter, p: &Sub, q: &Sub) -> bool {
    let (ap, aq) = (inf.arrival[0], inf.arrival[1]);
    if ap * aq != -1 || inf.count != 2 {
        return false;
    }
    let dm = if ap == 1 { dist2(inf.ips[1], q.j) } else { dist2(inf.ips[1], p.j) };
    feq(dm, 0.0)
}

/// `collinear::apply`.
fn collinear(ti: &mut Turn, inf: &Inter, side: &Sides) {
    assign_point(ti, b'c', inf, non_opposite_to_index(inf));
    let arrival_p = inf.arrival[0];
    let side_p = side.pk_wrt_p1();
    let side_q = side.qk_wrt_q1();
    let product = arrival_p * if arrival_p == 1 { side_p } else { side_q };
    if product == 0 {
        both(ti, CONTINUE);
    } else {
        ui_else_iu(product == 1, ti);
    }
}

/// `collinear_opposite::apply`.
fn collinear_opposite(model: &Turn, inf: &Inter, side: &Sides, pol: Policy, out: &mut Vec<Turn>) {
    let mut tp = *model;
    let set_tp = |index: usize, side_rk_r: i32, tp: &mut Turn| -> bool {
        let mut blocked = BLOCKED;
        match side_rk_r {
            1 => tp.ops[index].op = INTER,
            -1 => tp.ops[index].op = UNION,
            0 => {
                if pol.opposite {
                    tp.ops[index].op = OPPOSITE;
                    blocked = OPPOSITE;
                } else {
                    return false;
                }
            }
            _ => {}
        }
        tp.ops[1 - index].op = blocked;
        assign_point(tp, b'c', inf, 1 - index);
        true
    };
    if inf.arrival[0] == 1 && set_tp(0, side.pk_wrt_p1(), &mut tp) {
        out.push(tp);
    }
    if inf.arrival[1] == 1 && set_tp(1, side.qk_wrt_q1(), &mut tp) {
        out.push(tp);
    }
    if pol.opposite && ((inf.arrival[1] == -1 && inf.arrival[0] == 0) || (inf.arrival[0] == -1 && inf.arrival[1] == 0)) {
        both(&mut tp, OPPOSITE);
        for i in 0..inf.count {
            assign_point(&mut tp, b'c', inf, i);
            out.push(tp);
        }
    }
}

/// `get_turn_info::apply` for areal sub-ranges `p` and `q`.
pub fn turn_info(cs: Cs, p: &Sub, q: &Sub, model: &Turn, pol: Policy, out: &mut Vec<Turn>) {
    let inf = intersect(cs, p.i, p.j, q.i, q.j);
    let method = inf.how;
    if method == b'd' {
        return;
    }
    let mut tp = *model;
    let sides = Sides { cs, p, q };
    let swapped = Sides { cs, p: q, q: p };
    let mut handle_as_touch = method == b't';
    let mut handle_as_equal = method == b'e';
    let handle_as_collinear = method == b'c';
    let mut do_only_convert = method == b'a' || method == b'f';
    if method == b's' {
        do_only_convert = true;
    }
    if method == b'm' {
        if inf.arrival[1] == 1 {
            if as_touch(&inf, &sides, p) {
                handle_as_touch = true;
            } else {
                touch_interior(cs, &mut tp, &inf, &sides, 0);
                out.push(tp);
            }
        } else if as_touch(&inf, &swapped, q) {
            handle_as_touch = true;
        } else {
            touch_interior(cs, &mut tp, &inf, &swapped, 1);
            out.push(tp);
        }
    }
    if method == b'i' {
        assign_point(&mut tp, b'i', &inf, 0);
        let index = if inf.sides[1][0] == 1 { 0 } else { 1 };
        tp.ops[index].op = UNION;
        tp.ops[1 - index].op = INTER;
        out.push(tp);
    }
    if handle_as_touch {
        touch(cs, &mut tp, &inf, &sides);
        out.push(tp);
    }
    if handle_as_collinear {
        if !inf.opposite {
            if inf.arrival[0] == 0 || collinear_as_equal(&inf, p, q) {
                handle_as_equal = true;
            } else {
                collinear(&mut tp, &inf, &sides);
                out.push(tp);
            }
        } else {
            collinear_opposite(&tp, &inf, &sides, pol, out);
        }
    }
    if handle_as_equal {
        if !inf.opposite {
            equal(cs, &mut tp, &inf, &sides);
            if handle_as_collinear {
                tp.method = b'c';
            }
            out.push(tp);
        } else if pol.opposite {
            // `equal_opposite`
            let mut t = tp;
            both(&mut t, OPPOSITE);
            for i in 0..inf.count {
                assign_point(&mut t, b'-', &inf, i);
                out.push(t);
            }
        }
    }
    if ((method == b'0' && pol.degenerate) || (do_only_convert && pol.no_turn)) && inf.count > 0 {
        assign_point(&mut tp, b'-', &inf, 0);
        both(&mut tp, CONTINUE);
        out.push(tp);
    }
}

// ---------------------------------------------------------------------------------------------
// Sections

#[derive(Clone, Debug)]
pub struct Section {
    /// The ring index (-1 for the exterior).
    pub ring: i32,
    pub begin: usize,
    pub end: usize,
    pub dirs: [i32; 2],
    pub duplicate: bool,
    pub ndi: usize,
    pub range_count: usize,
    pub bbox: Rect,
    pub count: usize,
}

pub fn inverse_box() -> Rect {
    Rect { min: Pt { x: f64::MAX, y: f64::MAX }, max: Pt { x: f64::MIN, y: f64::MIN } }
}

fn expand_pt(b: &mut Rect, p: Pt) {
    b.min.x = b.min.x.min(p.x);
    b.min.y = b.min.y.min(p.y);
    b.max.x = b.max.x.max(p.x);
    b.max.y = b.max.y.max(p.y);
}

/// `sectionalize` in both dimensions over the rings (the exterior first): monotonic runs of at
/// most 11 segments, duplicate segments in their own sections; boxes enlarged by a scaled
/// epsilon.
pub fn sectionalize(rings: &[&[Pt]], cs: Cs) -> Vec<Section> {
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
                Cs::Sphere => geographic::lon_dist_deg(a.x, b.x),
            };
            let mut dirs = [sign(dx), if b.y > a.y { 1 } else if b.y < a.y { -1 } else { 0 }];
            let mut duplicate = false;
            if dirs[0] == 0 && feq(a.x, b.x) && feq(a.y, b.y) {
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
                        Cs::Sphere => algo::expand_segment(&mut s.bbox, a, b),
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
    // `enlarge_sections`: `expand_by_epsilon` with `scaled_epsilon(1000)`, which only the
    // first dimension gets: `corner_by_epsilon::apply(point, eps)` recurses into the next
    // dimension through the overload without `eps`, so y moves by the plain scaled epsilon
    let eps = f64::EPSILON * 1000.0;
    for s in &mut out {
        let b = &mut s.bbox;
        b.min.x -= b.min.x.abs().max(1.0) * eps;
        b.min.y -= b.min.y.abs().max(1.0) * f64::EPSILON;
        b.max.x += b.max.x.abs().max(1.0) * eps;
        b.max.y += b.max.y.abs().max(1.0) * f64::EPSILON;
    }
    out
}

/// `preceding<0>`: the point before the other box in the section's x direction.
fn preceding(cs: Cs, dir: i32, p: Pt, point_box: &Rect, other: &Rect) -> bool {
    match cs {
        Cs::Flat => (dir == 1 && p.x < other.min.x) || (dir == -1 && p.x > other.max.x),
        Cs::Sphere => {
            let (value, other_min, other_max) = (p.x, other.min.x, other.max.x);
            // `covered_by_range<0, spherical_tag>`
            let covered = (value >= other_min && value <= other_max)
                || other_max - other_min >= 360.0
                || other_min + algo::longitude_distance_unsigned(other_min, value) <= other_max;
            if covered {
                return false;
            }
            if dir == 1 {
                let diff_min = geographic::lon_dist_deg(other_min, value);
                let diff_min_min = geographic::lon_dist_deg(other_min, point_box.min.x);
                diff_min < 0.0 && diff_min_min <= 0.0 && diff_min_min <= diff_min
            } else if dir == -1 {
                let diff_max = geographic::lon_dist_deg(other_max, value);
                let diff_max_max = geographic::lon_dist_deg(other_max, point_box.max.x);
                diff_max > 0.0 && diff_max_max >= 0.0 && diff_max <= diff_max_max
            } else {
                false
            }
        }
    }
}

fn exceeding(cs: Cs, dir: i32, p: Pt, point_box: &Rect, other: &Rect) -> bool {
    preceding(cs, -dir, p, point_box, other)
}

/// The next point after `r[j]` different from it, around the closed ring (wrapping to the
/// first point, or past it when `skip_first`).
fn next_distinct(cs: Cs, r: &[Pt], j: usize, skip_first: bool, range_count: usize) -> Pt {
    let n = r.len();
    let wrap = |k: usize, skip: bool| if k >= n { if skip { 1.min(n - 1) } else { 0 } } else { k };
    // `next` is the circling iterator one past `j` (it skips the first point when wrapping)
    let mut k = wrap(j + 1, true);
    let mut check = 0;
    while algo::equals(cs, r[j], r[k]) && check < range_count {
        check += 1;
        k = wrap(k + 1, skip_first);
    }
    r[k]
}

/// One geometry's rings and sections for `get_turns`.
pub struct Sectioned<'a> {
    pub source: i32,
    pub rings: Vec<&'a [Pt]>,
    pub sections: Vec<Section>,
}

impl<'a> Sectioned<'a> {
    pub fn new(cs: Cs, source: i32, rings: Vec<&'a [Pt]>) -> Sectioned<'a> {
        let sections = sectionalize(&rings, cs);
        Sectioned { source, rings, sections }
    }
}

/// `get_start_point_iterator`: the first segment index and its non-duplicate index, skipping
/// the points before the other box.
fn start_of(cs: Cs, sec: &Section, r: &[Pt], dir: i32, other: &Rect) -> (usize, usize) {
    let mut skipped = 0;
    while sec.begin + 1 + skipped <= sec.end && preceding(cs, dir, r[sec.begin + 1 + skipped], &sec.bbox, other) {
        skipped += 1;
    }
    (sec.begin + skipped, sec.ndi + skipped)
}

/// `get_turns_in_sections`: false when the interrupt policy stopped it.
#[allow(clippy::too_many_arguments)]
pub fn turns_in_sections(
    cs: Cs,
    g1: &Sectioned,
    s1: &Section,
    g2: &Sectioned,
    s2: &Section,
    skip_adjacent: bool,
    pol: Policy,
    turns: &mut Vec<Turn>,
    interrupt: &mut dyn FnMut(&[Turn]) -> bool,
) -> bool {
    if (s1.duplicate && s1.count + 1 < s1.range_count) || (s2.duplicate && s2.count + 1 < s2.range_count) {
        return true;
    }
    let r1 = g1.rings[(s1.ring + 1) as usize];
    let r2 = g2.rings[(s2.ring + 1) as usize];
    let dir1 = s1.dirs[0];
    let dir2 = s2.dirs[0];
    let (mut i1, mut ndi1) = start_of(cs, s1, r1, dir1, &s2.bbox);
    while i1 < s1.end && !exceeding(cs, dir1, r1[i1], &s1.bbox, &s2.bbox) {
        let sub1 = Sub { i: r1[i1], j: r1[i1 + 1], k: next_distinct(cs, r1, i1 + 1, true, s1.range_count) };
        let (mut i2, mut ndi2) = start_of(cs, s2, r2, dir2, &s1.bbox);
        while i2 < s2.end && !exceeding(cs, dir2, r2[i2], &s2.bbox, &s1.bbox) {
            let mut skip = false;
            if g1.source == g2.source && s1.ring == s2.ring && skip_adjacent {
                let n = s1.range_count;
                skip = ndi2 == ndi1 + 1 || (i1 == 0 && i2 + 2 >= n);
            }
            if !skip {
                let sub2 = Sub { i: r2[i2], j: r2[i2 + 1], k: next_distinct(cs, r2, i2 + 1, false, s2.range_count) };
                let model = Turn {
                    point: Pt::default(),
                    method: b'-',
                    ops: [
                        Op { seg: SegId { source: g1.source, multi: -1, ring: s1.ring, segment: i1 as i32 }, op: NONE, fraction: Ratio::default() },
                        Op { seg: SegId { source: g2.source, multi: -1, ring: s2.ring, segment: i2 as i32 }, op: NONE, fraction: Ratio::default() },
                    ],
                };
                let before = turns.len();
                turn_info(cs, &sub1, &sub2, &model, pol, turns);
                if interrupt(&turns[before..]) {
                    return false;
                }
            }
            i2 += 1;
            ndi2 += 1;
        }
        i1 += 1;
        ndi1 += 1;
    }
    true
}

/// `get_turns` between two polygons: the sections of each, the pairs of overlapping boxes found
/// by `partition`.
pub fn get_turns(cs: Cs, g1: &Sectioned, g2: &Sectioned, pol: Policy, turns: &mut Vec<Turn>, interrupt: &mut dyn FnMut(&[Turn]) -> bool) {
    let boxes1: Vec<Rect> = g1.sections.iter().map(|s| s.bbox).collect();
    let boxes2: Vec<Rect> = g2.sections.iter().map(|s| s.bbox).collect();
    let mut visit = |a: usize, b: usize| -> bool {
        let (s1, s2) = (&g1.sections[a], &g2.sections[b]);
        if algo::box_disjoint(cs, &s1.bbox, &s2.bbox) {
            return true;
        }
        turns_in_sections(cs, g1, s1, g2, s2, false, pol, turns, interrupt)
    };
    partition_two(cs, &boxes1, &boxes2, &mut visit);
}

/// The self turns of a polygon (`self_get_turn_points` with adjacent segments skipped).
pub fn self_turns(cs: Cs, g: &Sectioned, pol: Policy, turns: &mut Vec<Turn>, interrupt: &mut dyn FnMut(&[Turn]) -> bool) {
    let boxes: Vec<Rect> = g.sections.iter().map(|s| s.bbox).collect();
    let mut visit = |a: usize, b: usize| -> bool {
        let (s1, s2) = (&g.sections[a], &g.sections[b]);
        if algo::box_disjoint(cs, &s1.bbox, &s2.bbox) || s1.duplicate || s2.duplicate {
            return true;
        }
        turns_in_sections(cs, g, s1, g, s2, true, pol, turns, interrupt)
    };
    partition_one(cs, &boxes, &mut visit);
}

// ---------------------------------------------------------------------------------------------
// Partition

type Visit<'a> = dyn FnMut(usize, usize) -> bool + 'a;

const MIN_ELEMENTS: usize = 16;

fn boxes_total(cs: Cs, items: &[usize], boxes: &[Rect], total: &mut Rect) {
    for &i in items {
        algo::expand(cs, total, &boxes[i]);
    }
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

fn ok(a: &[usize], level: usize) -> bool {
    a.len() >= MIN_ELEMENTS && level < 100
}

fn handle_one(items: &[usize], visit: &mut Visit) -> bool {
    for (i, &a) in items.iter().enumerate() {
        for &b in &items[i + 1..] {
            if !visit(a, b) {
                return false;
            }
        }
    }
    true
}

fn handle_two(a: &[usize], b: &[usize], visit: &mut Visit) -> bool {
    for &x in a {
        for &y in b {
            if !visit(x, y) {
                return false;
            }
        }
    }
    true
}

/// `partition<box>::apply` over one range: every pair of items whose boxes may overlap.
pub fn partition_one(cs: Cs, boxes: &[Rect], visit: &mut Visit) -> bool {
    let items: Vec<usize> = (0..boxes.len()).collect();
    if items.len() > MIN_ELEMENTS {
        let mut total = inverse_box();
        boxes_total(cs, &items, boxes, &mut total);
        one_range(cs, 0, &total, &items, 0, boxes, visit)
    } else {
        handle_one(&items, visit)
    }
}

/// `partition<box>::apply` over two ranges.
pub fn partition_two(cs: Cs, boxes1: &[Rect], boxes2: &[Rect], visit: &mut Visit) -> bool {
    let items1: Vec<usize> = (0..boxes1.len()).collect();
    let items2: Vec<usize> = (0..boxes2.len()).collect();
    if items1.len() > MIN_ELEMENTS && items2.len() > MIN_ELEMENTS {
        let mut total = inverse_box();
        boxes_total(cs, &items1, boxes1, &mut total);
        boxes_total(cs, &items2, boxes2, &mut total);
        two_ranges(cs, 0, &total, &items1, &items2, 0, boxes1, boxes2, visit)
    } else {
        handle_two(&items1, &items2, visit)
    }
}

fn one_range(cs: Cs, dim: usize, b: &Rect, items: &[usize], level: usize, boxes: &[Rect], visit: &mut Visit) -> bool {
    let (lo, hi) = divide(dim, b);
    let (lower, upper, exceeding) = subsets(cs, &lo, &hi, items, boxes);
    let next = |bx: &Rect, inp: &[usize], visit: &mut Visit| -> bool {
        if ok(inp, level) { one_range(cs, 1 - dim, bx, inp, level + 1, boxes, visit) } else { handle_one(inp, visit) }
    };
    let next2 = |bx: &Rect, a: &[usize], c: &[usize], visit: &mut Visit| -> bool {
        if ok(a, level) && ok(c, level) {
            two_ranges(cs, 1 - dim, bx, a, c, level + 1, boxes, boxes, visit)
        } else {
            handle_two(a, c, visit)
        }
    };
    if !exceeding.is_empty() {
        let mut eb = inverse_box();
        boxes_total(cs, &exceeding, boxes, &mut eb);
        if !(next(&eb, &exceeding, visit) && next2(&eb, &exceeding, &lower, visit) && next2(&eb, &exceeding, &upper, visit)) {
            return false;
        }
    }
    next(&lo, &lower, visit) && next(&hi, &upper, visit)
}

#[allow(clippy::too_many_arguments)]
fn two_ranges(cs: Cs, dim: usize, b: &Rect, in1: &[usize], in2: &[usize], level: usize, boxes1: &[Rect], boxes2: &[Rect], visit: &mut Visit) -> bool {
    let (lo, hi) = divide(dim, b);
    let (lower1, upper1, exc1) = subsets(cs, &lo, &hi, in1, boxes1);
    let (lower2, upper2, exc2) = subsets(cs, &lo, &hi, in2, boxes2);
    let next = |bx: &Rect, a: &[usize], c: &[usize], visit: &mut Visit| -> bool { two_ranges(cs, 1 - dim, bx, a, c, level + 1, boxes1, boxes2, visit) };
    if !exc1.is_empty() {
        if ok(&exc1, level) && ok(&exc2, level) {
            let mut eb = inverse_box();
            boxes_total(cs, &exc1, boxes1, &mut eb);
            boxes_total(cs, &exc2, boxes2, &mut eb);
            if !next(&eb, &exc1, &exc2, visit) {
                return false;
            }
        } else if !handle_two(&exc1, &exc2, visit) {
            return false;
        }
        if ok(&lower2, level) && ok(&upper2, level) && ok(&exc1, level) {
            let mut eb = inverse_box();
            boxes_total(cs, &exc1, boxes1, &mut eb);
            if !(next(&eb, &exc1, &lower2, visit) && next(&eb, &exc1, &upper2, visit)) {
                return false;
            }
        } else if !(handle_two(&exc1, &lower2, visit) && handle_two(&exc1, &upper2, visit)) {
            return false;
        }
    }
    if !exc2.is_empty() {
        if ok(&lower1, level) && ok(&upper1, level) && ok(&exc2, level) {
            let mut eb = inverse_box();
            boxes_total(cs, &exc2, boxes2, &mut eb);
            if !(next(&eb, &lower1, &exc2, visit) && next(&eb, &upper1, &exc2, visit)) {
                return false;
            }
        } else if !(handle_two(&lower1, &exc2, visit) && handle_two(&upper1, &exc2, visit)) {
            return false;
        }
    }
    if ok(&lower1, level) && ok(&lower2, level) {
        if !next(&lo, &lower1, &lower2, visit) {
            return false;
        }
    } else if !handle_two(&lower1, &lower2, visit) {
        return false;
    }
    if ok(&upper1, level) && ok(&upper2, level) {
        if !next(&hi, &upper1, &upper2, visit) {
            return false;
        }
    } else if !handle_two(&upper1, &upper2, visit) {
        return false;
    }
    true
}
