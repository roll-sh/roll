//! Boost.Geometry's geographic strategies (degrees on the WGS84 spheroid, Andoyer's inverse
//! formula), as the module's SPHERICAL coordinate system uses them: segment envelopes with the
//! geodesic's vertex latitude, ring envelopes reaching a pole inside, the ellipsoidal ring area
//! (`bg::correct`'s orientation), the direction code (spikes) and the segments' relation with
//! Sjöberg's geodesic intersection point.

use super::algo::{expand, feq};
use super::{Cs, Pt, Rect};
use std::f64::consts::PI;

/// `srs::spheroid<double>`'s radii.
const A: f64 = 6378137.0;
const B: f64 = 6_356_752.314_245_179;

/// `formula::flattening`: `(a - b) / a`.
fn flattening(a: f64, b: f64) -> f64 {
    (a - b) / a
}

fn d2r() -> f64 {
    PI / 180.0
}

fn r2d() -> f64 {
    180.0 / PI
}

/// `math::sign`.
fn sign(v: f64) -> i32 {
    if v > 0.0 {
        1
    } else if v < 0.0 {
        -1
    } else {
        0
    }
}

/// `normalize_spheroidal_coordinates::apply(longitude)` (exact) for a half period.
fn normalize_lon_with(l: &mut f64, half: f64) {
    let period = 2.0 * half;
    if feq(l.abs(), half) {
        *l = half;
    } else if *l > half {
        *l = (*l + half) % period - half;
        if feq(*l, -half) {
            *l = half;
        }
    } else if *l < -half {
        *l = (*l - half) % period + half;
    }
}

fn normalize_lon_rad(l: &mut f64) {
    normalize_lon_with(l, PI);
}

pub fn normalize_lon_deg(l: &mut f64) {
    normalize_lon_with(l, 180.0);
}

/// `longitude_distance_signed<radian>`.
fn lon_dist_rad(a: f64, b: f64) -> f64 {
    let mut d = b - a;
    normalize_lon_rad(&mut d);
    d
}

/// `longitude_distance_signed<degree>`.
fn lon_dist_deg(a: f64, b: f64) -> f64 {
    let mut d = b - a;
    normalize_lon_deg(&mut d);
    d
}

/// `strategy::normalize::spherical_point`: the longitude into (-180, 180], 0 at the poles.
fn normalize_point(p: Pt) -> Pt {
    let mut lon = p.x;
    normalize_lon_deg(&mut lon);
    if feq(p.y.abs(), 90.0) {
        lon = 0.0;
    }
    Pt { x: lon, y: p.y }
}

// ---------------------------------------------------------------------------------------------
// Andoyer's inverse formula

#[derive(Clone, Copy, Debug, Default)]
pub struct Inverse {
    pub distance: f64,
    pub azimuth: f64,
    pub reverse_azimuth: f64,
}

fn normalize_azimuth(az: &mut f64, a: f64, da: f64) {
    if a >= 0.0 {
        if da >= 0.0 {
            if *az < 0.0 {
                *az = 0.0;
            }
        } else if *az > PI {
            *az = PI;
        }
    } else if da <= 0.0 {
        if *az > 0.0 {
            *az = 0.0;
        }
    } else if *az < -PI {
        *az = -PI;
    }
}

/// `andoyer_inverse::apply` (radians) on a spheroid of radii `a` and `b`.
pub fn andoyer(lon1: f64, lat1: f64, lon2: f64, lat2: f64, a: f64, b: f64) -> Inverse {
    let mut r = Inverse::default();
    if feq(lon1, lon2) && feq(lat1, lat2) {
        return r;
    }
    let f = flattening(a, b);
    let dlon = lon2 - lon1;
    let (sin_dlon, cos_dlon) = (dlon.sin(), dlon.cos());
    let (sin_lat1, cos_lat1) = (lat1.sin(), lat1.cos());
    let (sin_lat2, cos_lat2) = (lat2.sin(), lat2.cos());
    let cos_d = (sin_lat1 * sin_lat2 + cos_lat1 * cos_lat2 * cos_dlon).clamp(-1.0, 1.0);
    let d = cos_d.acos();
    let sin_d = d.sin();
    {
        let k = (sin_lat1 - sin_lat2) * (sin_lat1 - sin_lat2);
        let l = (sin_lat1 + sin_lat2) * (sin_lat1 + sin_lat2);
        let three_sin_d = 3.0 * sin_d;
        let one_minus_cos_d = 1.0 - cos_d;
        let one_plus_cos_d = 1.0 + cos_d;
        let h = if feq(one_minus_cos_d, 0.0) { 0.0 } else { (d + three_sin_d) / one_minus_cos_d };
        let g = if feq(one_plus_cos_d, 0.0) { 0.0 } else { (d - three_sin_d) / one_plus_cos_d };
        let dd = -(f / 4.0) * (h * k + g * l);
        r.distance = a * (d + dd);
    }
    if feq(sin_d, 0.0) {
        if cos_d >= 0.0 {
            r.azimuth = 0.0;
            r.reverse_azimuth = 0.0;
        } else if !feq(sin_lat1, 1.0) {
            r.azimuth = 0.0;
            r.reverse_azimuth = PI;
        } else {
            r.azimuth = PI;
            r.reverse_azimuth = 0.0;
        }
        return r;
    }
    let (mut aa, mut u) = (0.0, 0.0);
    if feq(cos_lat2, 0.0) {
        if sin_lat2 < 0.0 {
            aa = PI;
        }
    } else {
        let tan_lat2 = sin_lat2 / cos_lat2;
        let m = cos_lat1 * tan_lat2 - sin_lat1 * cos_dlon;
        aa = sin_dlon.atan2(m);
        let sin_2a = (2.0 * aa).sin();
        u = (f / 2.0) * (cos_lat1 * cos_lat1) * sin_2a;
    }
    let (mut bb, mut v) = (0.0, 0.0);
    if feq(cos_lat1, 0.0) {
        if sin_lat1 < 0.0 {
            bb = PI;
        }
    } else {
        let tan_lat1 = sin_lat1 / cos_lat1;
        let n = cos_lat2 * tan_lat1 - sin_lat2 * cos_dlon;
        bb = sin_dlon.atan2(n);
        let sin_2b = (2.0 * bb).sin();
        v = (f / 2.0) * (cos_lat2 * cos_lat2) * sin_2b;
    }
    let t = d / sin_d;
    let da = v * t - u;
    r.azimuth = aa - da;
    normalize_azimuth(&mut r.azimuth, aa, da);
    let db = -u * t + v;
    r.reverse_azimuth = if bb >= 0.0 { PI - bb - db } else { -PI - bb - db };
    normalize_azimuth(&mut r.reverse_azimuth, bb, db);
    r
}

/// Andoyer on WGS84 (the side, azimuth, envelope and area strategies).
fn inverse_wgs(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> Inverse {
    andoyer(lon1, lat1, lon2, lat2, A, B)
}

/// The unit spheroid's polar radius (`formula::unit_spheroid`, the intersection strategy's).
fn unit_b() -> f64 {
    B / A
}

fn inverse_unit(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> Inverse {
    andoyer(lon1, lat1, lon2, lat2, 1.0, unit_b())
}

use super::algo::azimuth_side_value;

// ---------------------------------------------------------------------------------------------
// Envelopes

/// `vertex_latitude_on_spheroid`.
fn vertex_latitude(lat1: f64, alp1: f64) -> f64 {
    let f = flattening(A, B);
    let one_minus_f = 1.0 - f;
    let bet1 = (one_minus_f * lat1.tan()).atan();
    let betv = (bet1.cos() * alp1.sin()).abs().acos();
    (betv.tan() / one_minus_f).atan()
}

/// `envelope::geographic_segment` (degrees): the endpoints' box, the short way round the
/// longitudes, extended to the geodesic's vertex when its azimuth passes the vertical.
pub fn segment_envelope(p1: Pt, p2: Pt) -> Rect {
    let (n1, n2) = (normalize_point(p1), normalize_point(p2));
    let (mut lon1, mut lat1, mut lon2, mut lat2) = (n1.x, n1.y, n2.x, n2.y);
    // `special_cases`
    let is_pole1 = feq(lat1.abs(), 90.0);
    let is_pole2 = feq(lat2.abs(), 90.0);
    if is_pole1 && is_pole2 {
        lon1 = 0.0;
        lon2 = 0.0;
    } else if is_pole1 {
        lon1 = lon2;
    } else if is_pole2 {
        lon2 = lon1;
    }
    if lon1 == lon2 {
        if lat1 > lat2 {
            std::mem::swap(&mut lat1, &mut lat2);
        }
    } else {
        if lon1 > lon2 {
            std::mem::swap(&mut lon1, &mut lon2);
            std::mem::swap(&mut lat1, &mut lat2);
        }
        if (lon1 - lon2).abs() > 180.0 {
            lon1 += 360.0;
            std::mem::swap(&mut lon1, &mut lon2);
            std::mem::swap(&mut lat1, &mut lat2);
        }
    }
    let inv = inverse_wgs(lon1 * d2r(), lat1 * d2r(), lon2 * d2r(), lat2 * d2r());
    let (mut a1, mut a2) = (inv.azimuth, inv.reverse_azimuth);
    // `compute_box_corners`
    let mut lat1_rad = lat1 * d2r();
    if lat1 > lat2 {
        std::mem::swap(&mut lat1, &mut lat2);
        lat1_rad = lat1 * d2r();
        std::mem::swap(&mut a1, &mut a2);
    }
    let half = PI / 2.0;
    let contains = if a1 < a2 { a1 < half && half < a2 } else { a1 > half && half > a2 };
    if contains {
        let p_max = vertex_latitude(lat1_rad, a1);
        if lat1 + lat2 < 0.0 {
            let lat_min = -p_max * r2d();
            if lat1 > lat_min {
                lat1 = lat_min;
            }
        } else {
            let lat_max = p_max * r2d();
            if lat2 < lat_max {
                lat2 = lat_max;
            }
        }
    }
    Rect { min: Pt { x: lon1, y: lat1 }, max: Pt { x: lon2, y: lat2 } }
}

/// `side_of_pole`.
fn side_of_pole(p1: Pt, p2: Pt, pole: Pt) -> i32 {
    let diff = lon_dist_deg(p1.x, p2.x);
    if feq(diff, 0.0) || feq(diff, 180.0) {
        return 0;
    }
    if feq(pole.y, p1.y) || feq(pole.y, p2.y) {
        return 0;
    }
    if pole.y >= 0.0 {
        if diff < 0.0 { -1 } else { 1 }
    } else if diff > 0.0 {
        -1
    } else {
        1
    }
}

/// `spherical_winding_base`'s counter, with its side strategy (`side_of_pole` for the
/// envelope's pole test, the geographic side for `within`).
struct Winding {
    count: i32,
    count_s: i32,
    raw: i32,
    raw_anti: i32,
    touches: bool,
    side: fn(Pt, Pt, Pt) -> i32,
}

fn lons_equal(a: f64, b: f64) -> bool {
    feq(lon_dist_deg(a, b), 0.0)
}

impl Winding {
    fn code(&self) -> i32 {
        if self.touches {
            return 0;
        }
        if self.raw != 0 && self.raw_anti != 0 {
            if self.raw > 0 {
                return if self.count + self.count_s == 0 { -1 } else { 1 };
            }
            return if self.count == 0 { -1 } else { 1 };
        }
        if self.count == 0 { -1 } else { 1 }
    }

    /// `check_touch`: the eq flags and antipodality; true when decided (touch or skip).
    fn check_touch(&mut self, p: Pt, s1: Pt, s2: Pt, eq1: &mut bool, eq2: &mut bool, anti: &mut bool) -> bool {
        let pi = 180.0;
        let half_pi = 90.0;
        let eq1_strict = lons_equal(s1.x, p.x);
        let eq2_strict = lons_equal(s2.x, p.x);
        let anti_p_lon = p.x + if p.x <= 0.0 { pi } else { -pi };
        let eq1_anti = !eq1_strict && lons_equal(s1.x, anti_p_lon);
        let eq2_anti = !eq2_strict && lons_equal(s2.x, anti_p_lon);
        *eq1 = eq1_strict || eq1_anti;
        *eq2 = eq2_strict || eq2_anti;
        let s_lon_diff = lon_dist_deg(s1.x, s2.x);
        *anti = feq(s_lon_diff, pi);
        if *anti {
            let e = *eq1 || *eq2;
            *eq1 = e;
            *eq2 = e;
            if feq(p.y.abs(), half_pi) {
                *eq1 = true;
                *eq2 = true;
            }
        }
        if lon_dist_deg(s2.x, p.x) == 0.0 {
            let north = feq(s1.y, half_pi);
            let south = feq(s1.y, -half_pi);
            if north || south {
                self.touches = if south { s2.y > p.y } else { s2.y < p.y };
                return self.touches;
            }
        }
        if lon_dist_deg(s1.x, p.x) == 0.0 {
            let north = feq(s2.y, half_pi);
            let south = feq(s2.y, -half_pi);
            if north || south {
                self.touches = if south { s1.y > p.y } else { s1.y < p.y };
                return self.touches;
            }
        }
        if *eq1 && *eq2 {
            if !*anti {
                if ((s1.y <= p.y && s2.y >= p.y) || (s2.y <= p.y && s1.y >= p.y)) && (!eq1_anti || !eq2_anti) {
                    self.touches = true;
                }
            } else if pi - s1.y - s2.y <= pi {
                if (eq1_strict && s1.y <= p.y) || (eq2_strict && s2.y <= p.y) || feq(p.y, half_pi) {
                    self.touches = true;
                } else if !eq1_strict && !eq2_strict && feq(p.y, -half_pi) {
                    return false;
                }
            } else if (eq1_strict && s1.y >= p.y) || (eq2_strict && s2.y >= p.y) || feq(p.y, -half_pi) {
                self.touches = true;
            } else if !eq1_strict && !eq2_strict && feq(p.y, half_pi) {
                return false;
            }
            return true;
        }
        false
    }

    /// `calculate_count`: (count, is_anti).
    fn count_of(p: Pt, s1p: Pt, s2p: Pt, eq1: bool, eq2: bool, anti: bool) -> (i32, bool) {
        let pi = 180.0;
        let half_pi = 90.0;
        let s1_pole = feq(s1p.y.abs(), half_pi);
        let s2_pole = feq(s2p.y.abs(), half_pi);
        if s1_pole && s2_pole {
            return (0, false);
        }
        let mut s1 = s1p.x;
        let mut s2 = s2p.x;
        if s1_pole {
            s1 = if p.x != 0.0 { 0.0 } else { 1.0 };
        }
        if s2_pole {
            s2 = if p.x != 0.0 { 0.0 } else { 1.0 };
        }
        let s1_p = lon_dist_deg(s1, p.x);
        if anti {
            return (if s1_p < 0.0 { -2 } else { 2 }, false);
        }
        let s1_s2 = lon_dist_deg(s1, s2);
        if eq1 || eq2 {
            return (if s1_s2 < 0.0 { -1 } else { 1 }, lons_equal(p.x + pi, if eq1 { s1 } else { s2 }));
        }
        if sign(s1_p) == sign(s1_s2) && s1_p.abs() < s1_s2.abs() {
            return (if s1_s2 < 0.0 { -2 } else { 2 }, false);
        }
        let s1_p_anti = lon_dist_deg(s1, p.x + pi);
        if sign(s1_p_anti) == sign(s1_s2) && s1_p_anti.abs() < s1_s2.abs() {
            return (if s1_s2 < 0.0 { -2 } else { 2 }, true);
        }
        (0, false)
    }

    fn side_equal(&self, p: Pt, se: Pt, count: i32) -> i32 {
        if feq(p.y, se.y) {
            return 0;
        }
        let ss1 = se;
        let mut x = se.x + if count > 0 { 1.0 } else { -1.0 };
        normalize_lon_deg(&mut x);
        (self.side)(ss1, Pt { x, y: se.y }, p)
    }

    fn apply(&mut self, p: Pt, s1: Pt, s2: Pt) -> bool {
        let (mut eq1, mut eq2, mut anti) = (false, false, false);
        let (count, is_anti) = if self.check_touch(p, s1, s2, &mut eq1, &mut eq2, &mut anti) { (0, false) } else { Self::count_of(p, s1, s2, eq1, eq2, anti) };
        if count != 0 {
            if !is_anti {
                let side = if count == 1 || count == -1 {
                    self.side_equal(p, if eq1 { s1 } else { s2 }, count)
                } else if !anti {
                    (self.side)(s1, s2, p)
                } else {
                    sign(count as f64) * if 180.0 - s1.y - s2.y <= 180.0 { -1 } else { 1 }
                };
                if side == 0 {
                    self.touches = true;
                    self.count = 0;
                    return false;
                }
                if side * count > 0 {
                    self.count += count;
                }
                self.raw += count;
            } else {
                self.count_s -= count;
                self.raw_anti -= count;
            }
        }
        !self.touches
    }
}

/// `pole_within`: the pole inside the (closed, clockwise) ring.
fn pole_within(lat_pole: f64, r: &[Pt]) -> bool {
    if r.len() < 4 {
        return false;
    }
    winding(r, Pt { x: 0.0, y: lat_pole }, side_of_pole) > 0
}

/// `point_in_range` with the spherical winding: 1 inside, 0 on the boundary, -1 outside.
fn winding(r: &[Pt], p: Pt, side: fn(Pt, Pt, Pt) -> i32) -> i32 {
    let mut w = Winding { count: 0, count_s: 0, raw: 0, raw_anti: 0, touches: false, side };
    for s in r.windows(2) {
        if !w.apply(p, s[0], s[1]) {
            break;
        }
    }
    w.code()
}

/// `point_in_geometry` of a point and a (closed, clockwise) ring with
/// `strategy::within::geographic_winding`: 1 inside, 0 on the boundary, -1 outside.
pub fn point_in_ring(r: &[Pt], p: Pt) -> i32 {
    if r.len() < 4 {
        return -1;
    }
    winding(r, p, |a, b, c| super::algo::side(Cs::Sphere, a, b, c))
}

/// `envelope::geographic_ring` of a closed ring.
pub fn ring_envelope(r: &[Pt]) -> Rect {
    if r.len() == 1 {
        return Rect { min: r[0], max: r[0] };
    }
    let mut mbr = segment_envelope(r[0], r[1]);
    for s in r.windows(2).skip(1) {
        let e = segment_envelope(s[0], s[1]);
        expand(Cs::Sphere, &mut mbr, &e);
    }
    if mbr.max.x - mbr.min.x >= 360.0 {
        let (mut lat_min, mut lat_max) = (mbr.min.y, mbr.max.y);
        if feq(lat_min, -90.0) {
            lat_min = -90.0;
        }
        if feq(lat_max, 90.0) {
            lat_max = 90.0;
        }
        if lat_max < 90.0 && pole_within(90.0, r) {
            lat_max = 90.0;
        }
        if lat_min > -90.0 && pole_within(-90.0, r) {
            lat_min = -90.0;
        }
        mbr.min.y = lat_min;
        mbr.max.y = lat_max;
    }
    mbr
}

// ---------------------------------------------------------------------------------------------
// Areas

/// `boost::math::hypot`.
fn hypot(x: f64, y: f64) -> f64 {
    let (mut x, mut y) = (x.abs(), y.abs());
    if y > x {
        std::mem::swap(&mut x, &mut y);
    }
    if x * f64::EPSILON >= y {
        return x;
    }
    let rat = y / x;
    x * (1.0 + rat * rat).sqrt()
}

fn trapezoidal(lat1: f64, lat2: f64, lon21: f64) -> f64 {
    let t1 = (lat1 / 2.0).tan();
    let t2 = (lat2 / 2.0).tan();
    2.0 * (((t1 + t2) / (1.0 + t1 * t2)) * (lon21 / 2.0).tan()).atan()
}

/// `area_formulas::crosses_prime_meridian` (radians).
fn crosses_prime_meridian(lon1: f64, lon2: f64) -> bool {
    let two_pi = 2.0 * PI;
    let mut lon12 = lon2 - lon1;
    normalize_lon_rad(&mut lon12);
    if lon12 == PI || lon12 == -PI {
        return true;
    }
    let p1 = lon1 - (lon1 / two_pi).floor() * two_pi;
    let p2 = lon2 - (lon2 / two_pi).floor() * two_pi;
    let (mx, mn) = (p1.max(p2), p1.min(p2));
    mx > PI && mn < PI && mx - mn > PI
}

/// `strategy::area::geographic<andoyer, 1>` over a closed ring (degrees): positive for clockwise
/// rings.
pub fn ring_area(r: &[Pt]) -> f64 {
    if r.len() < 4 {
        return 0.0;
    }
    let a2 = A * A;
    let e2 = 1.0 - (B / A) * (B / A);
    let ep2 = e2 / (1.0 - e2);
    let ep = ep2.sqrt();
    let c2 = if feq(e2, 0.0) {
        a2
    } else {
        let e = e2.sqrt();
        (a2 / 2.0) * (1.0 + (1.0 - e2) * e.atanh() / e)
    };
    let f = flattening(A, B);
    let n = f / (2.0 - f);
    let coeffs_var = [(10.0 - 4.0 * n) / 15.0, -1.0 / 5.0, 1.0 / 45.0];
    let mean_radius = (2.0 * A + B) / 3.0;
    let (mut excess_sum, mut correction_sum, mut crosses) = (0.0, 0.0, 0usize);
    for s in r.windows(2) {
        let (p1, p2) = (s[0], s[1]);
        if feq(p1.x, p2.x) {
            continue;
        }
        let (lon1, lat1, lon2, lat2) = (p1.x * d2r(), p1.y * d2r(), p2.x * d2r(), p2.y * d2r());
        if crosses_prime_meridian(lon1, lon2) {
            crosses += 1;
        }
        if feq(p1.y, 0.0) && feq(p2.y, 0.0) {
            continue;
        }
        // `area_formulas::ellipsoidal`
        let inv = inverse_wgs(lon1, lat1, lon2, lat2);
        let (alp1, alp2) = (inv.azimuth, inv.reverse_azimuth);
        let half_pi = PI / 2.0;
        let one_minus_f = 1.0 - f;
        let tan_bet1 = lat1.tan() * one_minus_f;
        let tan_bet2 = lat2.tan() * one_minus_f;
        let cos_bet1 = tan_bet1.atan().cos();
        let cos_bet2 = tan_bet2.atan().cos();
        let sin_bet1 = tan_bet1 * cos_bet1;
        let sin_bet2 = tan_bet2 * cos_bet2;
        let sin_alp1 = alp1.sin();
        let cos_alp1 = alp1.cos();
        let cos_alp2 = alp2.cos();
        let sin_alp0 = sin_alp1 * cos_bet1;
        let mut lon12 = lon2 - lon1;
        normalize_lon_rad(&mut lon12);
        let spherical = if lon12 == PI || lon12 == -PI {
            PI
        } else {
            let meridian = lon12 == 0.0 || lat1 == half_pi || lat1 == -half_pi || lat2 == half_pi || lat2 == -half_pi;
            if !meridian && inv.distance < mean_radius / 638.0 { trapezoidal(lat1, lat2, lon12) } else { alp2 - alp1 }
        };
        let cos_alp0 = (1.0 - sin_alp0 * sin_alp0).sqrt();
        let (mut cos_sig1, mut cos_sig2) = (cos_alp1 * cos_bet1, cos_alp2 * cos_bet2);
        let (mut sin_sig1, mut sin_sig2) = (sin_bet1, sin_bet2);
        let h1 = hypot(sin_sig1, cos_sig1);
        sin_sig1 /= h1;
        cos_sig1 /= h1;
        let h2 = hypot(sin_sig2, cos_sig2);
        sin_sig2 /= h2;
        cos_sig2 /= h2;
        let _ = (sin_sig1, sin_sig2);
        let k2 = (ep * cos_alp0) * (ep * cos_alp0);
        let s = (1.0 + k2).sqrt();
        let eps = (s - 1.0) / (s + 1.0);
        // `evaluate_coeffs_var2` (order 1)
        let coeffs = [coeffs_var[1] * eps + coeffs_var[0], eps * coeffs_var[2]];
        let clenshaw = |cosx: f64| -> f64 {
            // two coefficients: the odd one, a zero step, the even one
            let (mut bk1, mut bk2) = (0.0, 0.0);
            let steps = [coeffs[1], 0.0, coeffs[0]];
            for c in steps {
                let bk = c + 2.0 * cosx * bk1 - bk2;
                bk2 = bk1;
                bk1 = bk;
            }
            coeffs[0] + bk1 * cosx - bk2
        };
        let i12 = clenshaw(cos_sig2) - clenshaw(cos_sig1);
        excess_sum += spherical;
        correction_sum += cos_alp0 * sin_alp0 * i12;
    }
    let spherical_term = c2 * excess_sum;
    let ellipsoidal_term = e2 * a2 * correction_sum;
    let sum = if (ellipsoidal_term / spherical_term).abs() > 0.01 { spherical_term } else { spherical_term + ellipsoidal_term };
    if crosses % 2 == 1 {
        let times = 1 + crosses / 2;
        let mut result = 2.0 * PI * c2 * times as f64 - sum.abs();
        if sign(sum) == 1 {
            result = -result;
        }
        result
    } else {
        sum
    }
}

// ---------------------------------------------------------------------------------------------
// Direction code

/// `direction_code_impl<spherical_equatorial_tag>` (degrees): 1 when `p` continues past `b`
/// in the direction `a`→`b`, -1 when it goes back, 0 at `b`.
pub fn direction_code(a: Pt, b: Pt, p: Pt) -> i32 {
    if (feq(b.x, a.x) && feq(b.y, a.y)) || (feq(b.x, p.x) && feq(b.y, p.y)) {
        return 0;
    }
    let is_a_pole = feq(90.0, a.y.abs());
    let is_b_pole = feq(90.0, b.y.abs());
    let is_p_pole = feq(90.0, p.y.abs());
    if is_b_pole && ((is_a_pole && sign(b.y) == sign(a.y)) || (is_p_pole && sign(b.y) == sign(p.y))) {
        return 0;
    }
    let dlon1 = lon_dist_deg(b.x, a.x);
    let dlon2 = lon_dist_deg(b.x, p.x);
    let lat_dist = |lat1: f64, lat2: f64, lon_ds: f64| -> (f64, bool) {
        let anti = feq(lon_ds.abs(), 180.0);
        if anti {
            let r = lat2 + lat1;
            (if r >= 0.0 { 180.0 - r } else { -180.0 - r }, true)
        } else {
            (lat2 - lat1, false)
        }
    };
    let (dlat1, anti1) = lat_dist(b.y, a.y, dlon1);
    let (dlat2, anti2) = lat_dist(b.y, p.y, dlon2);
    let mx = if is_a_pole || is_b_pole || is_p_pole { 0.0 } else { (if anti1 { 0.0 } else { dlon1.abs() }).min(if anti2 { 0.0 } else { dlon2.abs() }) };
    let my = dlat1.abs().min(dlat2.abs());
    let (s1, s2) = if mx >= my {
        (if dlon1 > 0.0 { 1 } else { -1 }, if dlon2 > 0.0 { 1 } else { -1 })
    } else {
        (if dlat1 > 0.0 { 1 } else { -1 }, if dlat2 > 0.0 { 1 } else { -1 })
    };
    if s1 == s2 { -1 } else { 1 }
}

// ---------------------------------------------------------------------------------------------
// Segment relations

/// What `geographic_segments::apply` decided.
#[derive(Clone, Copy, Debug)]
pub enum Relation {
    Disjoint,
    /// Endpoint degenerate cases (treated as disjoint by the caller).
    Degenerate,
    /// `segments_crosses`: the sides, the point (degrees) and the ratios along a and b.
    Crosses { sides: [[i32; 2]; 2], ip: Pt, ra: (f64, f64), rb: (f64, f64) },
    /// `segments_collinear`: the opposite flag, the positions and the ratios.
    Collinear { opposite: bool, pos: [i32; 4], ra_from: (f64, f64), ra_to: (f64, f64), rb_from: (f64, f64), rb_to: (f64, f64) },
}

/// `strategy::within::spherical_point_point`.
fn equals_pp(a: Pt, b: Pt) -> bool {
    super::algo::equals(Cs::Sphere, a, b)
}

fn same_direction(az1: f64, az2: f64) -> bool {
    lon_dist_rad(az1, az2).abs() <= PI / 2.0
}

/// `segment_ratio::on_segment` of `num / den`.
fn on_segment(num: f64, den: f64) -> bool {
    let (n, d) = if den < 0.0 { (-num, -den) } else { (num, den) };
    n >= 0.0 && n <= d
}

fn is_near(d: f64) -> bool {
    d.abs() <= 0.00000001
}

fn is_endpoint_equal(dist: f64, ai: Pt, b1: Pt) -> bool {
    is_near(dist) && (feq(dist, 0.0) || equals_pp(ai, b1))
}

/// `calculate_collinear_data`: the distances of a's end and b1 along a.
fn collinear_data(a1: Pt, a2: Pt, b1: Pt, r12: &Inverse, r1b1: &Inverse) -> (f64, f64, bool) {
    let d12 = r12.distance;
    let mut d1b1 = r1b1.distance;
    if !same_direction(r1b1.azimuth, r12.azimuth) {
        d1b1 = -d1b1;
    }
    if is_endpoint_equal(d1b1, a1, b1) {
        return (d12, 0.0, true);
    }
    if is_endpoint_equal(d12 - d1b1, a2, b1) {
        return (d12, d12, true);
    }
    let ok = on_segment(d1b1, d12);
    (d12, d1b1, ok)
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

/// Where the intersection point is (`intersection_point_flag`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IpFlag {
    Inters,
    AtA1,
    AtA2,
    AtB1,
    AtB2,
}

/// `geographic_segments::apply` for segments `p` and `q` (degrees): each ordered south to
/// north for the computation, the results mapped back to the original directions.
pub fn relate(p0: Pt, p1: Pt, q0: Pt, q1: Pt) -> Relation {
    let a_rev = p0.y > p1.y;
    let b_rev = q0.y > q1.y;
    let (a1, a2) = if a_rev { (p1, p0) } else { (p0, p1) };
    let (b1, b2) = if b_rev { (q1, q0) } else { (q0, q1) };
    let mut a_is_point = equals_pp(a1, a2);
    let mut b_is_point = equals_pp(b1, b2);
    if a_is_point && b_is_point {
        return if equals_pp(a1, b2) { Relation::Degenerate } else { Relation::Disjoint };
    }
    let r = d2r();
    let (a1_lon, a1_lat, a2_lon, a2_lat) = (a1.x * r, a1.y * r, a2.x * r, a2.y * r);
    let (b1_lon, b1_lat, b2_lon, b2_lat) = (b1.x * r, b1.y * r, b2.x * r, b2.y * r);
    let mut sides = [[0i32; 2]; 2];
    let mut is_equal_a1_b1 = equals_pp(a1, b1);
    let mut is_equal_a2_b1 = equals_pp(a2, b1);
    let mut res_b1_b2 = Inverse::default();
    let mut res_b1_a1 = Inverse::default();
    let mut res_b1_a2 = Inverse::default();
    if !b_is_point {
        res_b1_b2 = inverse_unit(b1_lon, b1_lat, b2_lon, b2_lat);
        if feq(res_b1_b2.distance, 0.0) {
            b_is_point = true;
        } else {
            res_b1_a1 = inverse_unit(b1_lon, b1_lat, a1_lon, a1_lat);
            if feq(res_b1_a1.distance, 0.0) {
                is_equal_a1_b1 = true;
            }
            res_b1_a2 = inverse_unit(b1_lon, b1_lat, a2_lon, a2_lat);
            if feq(res_b1_a2.distance, 0.0) {
                is_equal_a2_b1 = true;
            }
            sides[0] = [
                if is_equal_a1_b1 { 0 } else { azimuth_side_value(res_b1_a1.azimuth, res_b1_b2.azimuth) },
                if is_equal_a2_b1 { 0 } else { azimuth_side_value(res_b1_a2.azimuth, res_b1_b2.azimuth) },
            ];
            if sides[0][0] * sides[0][1] == 1 {
                return Relation::Disjoint;
            }
        }
    }
    let mut is_equal_a1_b2 = equals_pp(a1, b2);
    let mut res_a1_a2 = Inverse::default();
    let mut res_a1_b1 = Inverse::default();
    let mut res_a1_b2 = Inverse::default();
    if !a_is_point {
        res_a1_a2 = inverse_unit(a1_lon, a1_lat, a2_lon, a2_lat);
        if feq(res_a1_a2.distance, 0.0) {
            a_is_point = true;
        } else {
            res_a1_b1 = inverse_unit(a1_lon, a1_lat, b1_lon, b1_lat);
            if feq(res_a1_b1.distance, 0.0) {
                is_equal_a1_b1 = true;
            }
            res_a1_b2 = inverse_unit(a1_lon, a1_lat, b2_lon, b2_lat);
            if feq(res_a1_b2.distance, 0.0) {
                is_equal_a1_b2 = true;
            }
            sides[1] = [
                if is_equal_a1_b1 { 0 } else { azimuth_side_value(res_a1_b1.azimuth, res_a1_a2.azimuth) },
                if is_equal_a1_b2 { 0 } else { azimuth_side_value(res_a1_b2.azimuth, res_a1_a2.azimuth) },
            ];
            if sides[1][0] * sides[1][1] == 1 {
                return Relation::Disjoint;
            }
        }
    }
    if a_is_point && b_is_point {
        return if is_equal_a1_b2 { Relation::Degenerate } else { Relation::Disjoint };
    }
    let mut collinear = sides[0] == [0, 0] && sides[1] == [0, 0];
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
        if a_is_point || b_is_point {
            return Relation::Degenerate;
        }
        let (mut dist_a1_a2, mut dist_a1_b1, mut dist_a1_b2, dist_b1_b2, mut dist_b1_a1, mut dist_b1_a2);
        if res_a1_a2.distance <= res_b1_b2.distance {
            let (d12, d1b1, _) = collinear_data(a1, a2, b1, &res_a1_a2, &res_a1_b1);
            let (_, d1b2, _) = collinear_data(a1, a2, b2, &res_a1_a2, &res_a1_b2);
            dist_a1_a2 = d12;
            dist_a1_b1 = d1b1;
            dist_a1_b2 = d1b2;
            dist_b1_b2 = dist_a1_b2 - dist_a1_b1;
            dist_b1_a1 = -dist_a1_b1;
            dist_b1_a2 = dist_a1_a2 - dist_a1_b1;
        } else {
            let (d12, d1a1, _) = collinear_data(b1, b2, a1, &res_b1_b2, &res_b1_a1);
            let (_, d1a2, _) = collinear_data(b1, b2, a2, &res_b1_b2, &res_b1_a2);
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
            return Relation::Disjoint;
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
        let _ = &mut dist_a1_a2;
        return Relation::Collinear {
            opposite,
            pos: [a1_on_b, a2_on_b, b1_on_a, b2_on_a],
            ra_from: (dist_b1_a1, dist_b1_b2),
            ra_to: (dist_b1_a2, dist_b1_b2),
            rb_from: (dist_a1_b1, dist_a1_a2),
            rb_to: (dist_a1_b2, dist_a1_a2),
        };
    }
    if a_is_point || b_is_point {
        return Relation::Disjoint;
    }
    // `calculate_ip_data`
    let dist_a1_a2 = res_a1_a2.distance;
    let dist_b1_b2 = res_b1_b2.distance;
    let (mut lon, mut lat) = (0.0, 0.0);
    let (mut dist_a1_ip, mut dist_b1_ip);
    let mut flag;
    if equals_pp(a1, b1) {
        (dist_a1_ip, dist_b1_ip, flag) = (0.0, 0.0, IpFlag::AtA1);
    } else if equals_pp(a1, b2) {
        (dist_a1_ip, dist_b1_ip, flag) = (0.0, dist_b1_b2, IpFlag::AtA1);
    } else if equals_pp(a2, b1) {
        (dist_a1_ip, dist_b1_ip, flag) = (dist_a1_a2, 0.0, IpFlag::AtA2);
    } else if equals_pp(a2, b2) {
        (dist_a1_ip, dist_b1_ip, flag) = (dist_a1_a2, dist_b1_b2, IpFlag::AtA2);
    } else if sides[0][0] == 0 {
        if !(res_b1_a1.distance <= res_b1_b2.distance && same_direction(res_b1_a1.azimuth, res_b1_b2.azimuth)) {
            return Relation::Disjoint;
        }
        (dist_a1_ip, dist_b1_ip, flag) = (0.0, res_b1_a1.distance, IpFlag::AtA1);
    } else if sides[0][1] == 0 {
        if !(res_b1_a2.distance <= res_b1_b2.distance && same_direction(res_b1_a2.azimuth, res_b1_b2.azimuth)) {
            return Relation::Disjoint;
        }
        (dist_a1_ip, dist_b1_ip, flag) = (res_a1_a2.distance, res_b1_a2.distance, IpFlag::AtA2);
    } else if sides[1][0] == 0 {
        if !(res_a1_b1.distance <= res_a1_a2.distance && same_direction(res_a1_b1.azimuth, res_a1_a2.azimuth)) {
            return Relation::Disjoint;
        }
        (dist_a1_ip, dist_b1_ip, flag) = (res_a1_b1.distance, 0.0, IpFlag::AtB1);
    } else if sides[1][1] == 0 {
        if !(res_a1_b2.distance <= res_a1_a2.distance && same_direction(res_a1_b2.azimuth, res_a1_a2.azimuth)) {
            return Relation::Disjoint;
        }
        (dist_a1_ip, dist_b1_ip, flag) = (res_a1_b2.distance, res_b1_b2.distance, IpFlag::AtB2);
    } else {
        let Some((ilon, ilat)) = sjoberg_intersection(a1_lon, a1_lat, a2_lon, a2_lat, res_a1_a2.azimuth, b1_lon, b1_lat, b2_lon, b2_lat, res_b1_b2.azimuth) else {
            return Relation::Disjoint;
        };
        (lon, lat) = (ilon, ilat);
        let res_a1_ip = inverse_unit(a1_lon, a1_lat, lon, lat);
        dist_a1_ip = res_a1_ip.distance;
        if !same_direction(res_a1_ip.azimuth, res_a1_a2.azimuth) {
            dist_a1_ip = -dist_a1_ip;
        }
        let is_on_a = on_segment(dist_a1_ip, dist_a1_a2);
        let is_on_a1 = feq(lon, a1_lon) && feq(lat, a1_lat);
        let is_on_a2 = feq(lon, a2_lon) && feq(lat, a2_lat);
        if !(is_on_a || is_on_a1 || is_on_a2) {
            return Relation::Disjoint;
        }
        let res_b1_ip = inverse_unit(b1_lon, b1_lat, lon, lat);
        dist_b1_ip = res_b1_ip.distance;
        if !same_direction(res_b1_ip.azimuth, res_b1_b2.azimuth) {
            dist_b1_ip = -dist_b1_ip;
        }
        let is_on_b = on_segment(dist_b1_ip, dist_b1_b2);
        let is_on_b1 = feq(lon, b1_lon) && feq(lat, b1_lat);
        let is_on_b2 = feq(lon, b2_lon) && feq(lat, b2_lat);
        if !(is_on_b || is_on_b1 || is_on_b2) {
            return Relation::Disjoint;
        }
        flag = IpFlag::Inters;
        if is_on_b1 {
            (lon, lat) = (b1_lon, b1_lat);
            dist_a1_ip = inverse_unit(a1_lon, a1_lat, lon, lat).distance;
            dist_b1_ip = 0.0;
            flag = IpFlag::AtB1;
        } else if is_on_b2 {
            (lon, lat) = (b2_lon, b2_lat);
            dist_a1_ip = inverse_unit(a1_lon, a1_lat, lon, lat).distance;
            dist_b1_ip = res_b1_b2.distance;
            flag = IpFlag::AtB2;
        }
        if is_on_a1 {
            (lon, lat) = (a1_lon, a1_lat);
            dist_a1_ip = 0.0;
            dist_b1_ip = inverse_unit(b1_lon, b1_lat, lon, lat).distance;
            flag = IpFlag::AtA1;
        } else if is_on_a2 {
            (lon, lat) = (a2_lon, a2_lat);
            dist_a1_ip = res_a1_a2.distance;
            dist_b1_ip = inverse_unit(b1_lon, b1_lat, lon, lat).distance;
            flag = IpFlag::AtA2;
        }
    }
    // the segments' original directions
    let rev_sides = |sides: &mut [[i32; 2]; 2], w: usize| {
        sides[w].swap(0, 1);
        sides[1 - w] = [-sides[1 - w][0], -sides[1 - w][1]];
    };
    if a_rev {
        rev_sides(&mut sides, 0);
        dist_a1_ip = dist_a1_a2 - dist_a1_ip;
        flag = match flag {
            IpFlag::AtA1 => IpFlag::AtA2,
            IpFlag::AtA2 => IpFlag::AtA1,
            f => f,
        };
    }
    if b_rev {
        rev_sides(&mut sides, 1);
        dist_b1_ip = dist_b1_b2 - dist_b1_ip;
        flag = match flag {
            IpFlag::AtB1 => IpFlag::AtB2,
            IpFlag::AtB2 => IpFlag::AtB1,
            f => f,
        };
    }
    let ip = match flag {
        IpFlag::Inters => Pt { x: lon * r2d(), y: lat * r2d() },
        IpFlag::AtA1 => p0,
        IpFlag::AtA2 => p1,
        IpFlag::AtB1 => q0,
        IpFlag::AtB2 => q1,
    };
    Relation::Crosses { sides, ip, ra: (dist_a1_ip, dist_a1_a2), rb: (dist_b1_ip, dist_b1_b2) }
}

// ---------------------------------------------------------------------------------------------
// Sjöberg's intersection (order 1, Andoyer's inverse, the unit spheroid)

/// `sjoberg_intersection_spherical_02::apply_alt`: the longitude and tan(latitude).
#[allow(clippy::too_many_arguments)]
fn sjoberg_spherical(lon1: f64, lat1: f64, lon_a2: f64, lat_a2: f64, lon2: f64, lat2: f64, lon_b2: f64, lat_b2: f64) -> Option<(f64, f64)> {
    let (cos_lon1, sin_lon1) = (lon1.cos(), lon1.sin());
    let (cos_lon2, sin_lon2) = (lon2.cos(), lon2.sin());
    let (sin_lat1, sin_lat2) = (lat1.sin(), lat2.sin());
    let (cos_lat1, cos_lat2) = (lat1.cos(), lat2.cos());
    let tan_lat_a2 = lat_a2.tan();
    let tan_lat_b2 = lat_b2.tan();
    let tan_lat1 = sin_lat1 / cos_lat1;
    let tan_lat2 = sin_lat2 / cos_lat2;
    let dlon1 = lon_a2 - lon1;
    let sin_dlon1 = dlon1.sin();
    let dlon2 = lon_b2 - lon2;
    let sin_dlon2 = dlon2.sin();
    let cos_dlon1 = dlon1.cos();
    let cos_dlon2 = dlon2.cos();
    let tan_alpha1_x = cos_lat1 * tan_lat_a2 - sin_lat1 * cos_dlon1;
    let tan_alpha2_x = cos_lat2 * tan_lat_b2 - sin_lat2 * cos_dlon2;
    let vertical1 = feq(sin_dlon1, 0.0) || feq(tan_alpha1_x, 0.0);
    let vertical2 = feq(sin_dlon2, 0.0) || feq(tan_alpha2_x, 0.0);
    let (mut tan_alpha1, mut tan_alpha2) = (0.0, 0.0);
    let mut lon;
    if vertical1 && vertical2 {
        return None;
    } else if vertical1 {
        tan_alpha2 = sin_dlon2 / tan_alpha2_x;
        lon = lon1;
    } else if vertical2 {
        tan_alpha1 = sin_dlon1 / tan_alpha1_x;
        lon = lon2;
    } else {
        tan_alpha1 = sin_dlon1 / tan_alpha1_x;
        tan_alpha2 = sin_dlon2 / tan_alpha2_x;
        let t1 = tan_alpha1 * cos_lat1;
        let t2 = tan_alpha2 * cos_lat2;
        let t1t2 = t1 * t2;
        let tan_lon_y = t1 * sin_lon2 - t2 * sin_lon1 + t1t2 * (tan_lat1 * cos_lon1 - tan_lat2 * cos_lon2);
        let tan_lon_x = t1 * cos_lon2 - t2 * cos_lon1 - t1t2 * (tan_lat1 * sin_lon1 - tan_lat2 * sin_lon2);
        lon = tan_lon_y.atan2(tan_lon_x);
    }
    let lon_2 = if lon > 0.0 { lon - PI } else { lon + PI };
    let ld = |a: f64, b: f64| lon_dist_rad(a, b).abs();
    let lon_dist1 = ld(lon1, lon).min(ld(lon_a2, lon)).max(ld(lon2, lon).min(ld(lon_b2, lon)));
    let lon_dist2 = ld(lon1, lon_2).min(ld(lon_a2, lon_2)).max(ld(lon2, lon_2).min(ld(lon_b2, lon_2)));
    if lon_dist2 < lon_dist1 {
        lon = lon_2;
    }
    let (sin_lon, cos_lon) = (lon.sin(), lon.cos());
    let tan_lat = if tan_alpha1.abs() >= tan_alpha2.abs() {
        let sin_dlon_1 = sin_lon * cos_lon1 - cos_lon * sin_lon1;
        let cos_dlon_1 = cos_lon * cos_lon1 + sin_lon * sin_lon1;
        let lat_y_1 = sin_dlon_1 + tan_alpha1 * sin_lat1 * cos_dlon_1;
        let lat_x_1 = tan_alpha1 * cos_lat1;
        lat_y_1 / lat_x_1
    } else {
        let sin_dlon_2 = sin_lon * cos_lon2 - cos_lon * sin_lon2;
        let cos_dlon_2 = cos_lon * cos_lon2 + sin_lon * sin_lon2;
        let lat_y_2 = sin_dlon_2 + tan_alpha2 * sin_lat2 * cos_dlon_2;
        let lat_x_2 = tan_alpha2 * cos_lat2;
        lat_y_2 / lat_x_2
    };
    Some((lon, tan_lat))
}

/// `sjoberg_geodesic<CT, 1>`.
struct Geodesic {
    lonj: f64,
    one_minus_f: f64,
    e_sqr: f64,
    sin_betaj: f64,
    cj: f64,
    cj_sqr: f64,
    sqrt_1_cj_sqr: f64,
    sign_lon_diff: f64,
    is_on_equator: bool,
    is_cj_zero: bool,
    t0j: f64,
    asin_tj_t0j: f64,
}

struct Vertex {
    dl0j: f64,
}

impl Geodesic {
    fn new(lon: f64, lat: f64, alpha: f64, f: f64) -> Self {
        let one_minus_f = 1.0 - f;
        let e_sqr = f * (2.0 - f);
        let tan_latj = lat.tan();
        let tan_betaj = one_minus_f * tan_latj;
        let betaj = tan_betaj.atan();
        let sin_betaj = betaj.sin();
        let cos_betaj = betaj.cos();
        let sin_alphaj = alpha.sin();
        let half = PI / 2.0;
        let sign_c = if (half < alpha && alpha < PI) || (-half < alpha && alpha < 0.0) { -1.0 } else { 1.0 };
        let cj = sign_c * cos_betaj * sin_alphaj;
        let cj_sqr = cj * cj;
        let sqrt_1_cj_sqr = (1.0 - cj_sqr).sqrt();
        let is_on_equator = feq(sqrt_1_cj_sqr, 0.0);
        let is_cj_zero = feq(cj, 0.0);
        let t0j = if !is_cj_zero { sqrt_1_cj_sqr / cj } else { 0.0 };
        let asin_tj_t0j = if !is_on_equator { (tan_betaj * cj / sqrt_1_cj_sqr).asin() } else { 0.0 };
        Geodesic {
            lonj: lon,
            one_minus_f,
            e_sqr,
            sin_betaj,
            cj,
            cj_sqr,
            sqrt_1_cj_sqr,
            sign_lon_diff: if alpha >= 0.0 { 1.0 } else { -1.0 },
            is_on_equator,
            is_cj_zero,
            t0j,
            asin_tj_t0j,
        }
    }

    /// `sjoberg_d_lambda_e_sqr<1>`.
    fn d_lambda(&self, sin_beta: f64) -> f64 {
        let asin_b = (sin_beta / self.sqrt_1_cj_sqr).clamp(-1.0, 1.0).asin();
        let asin_bj = (self.sin_betaj / self.sqrt_1_cj_sqr).asin();
        let l0 = (asin_b - asin_bj) / 2.0;
        -self.cj * self.e_sqr * l0
    }

    fn vertex(&self) -> Vertex {
        if !self.is_cj_zero {
            let sin_beta0j = sign(self.t0j) as f64 * self.sqrt_1_cj_sqr;
            Vertex { dl0j: self.d_lambda(sin_beta0j) }
        } else {
            Vertex { dl0j: 0.0 }
        }
    }

    fn sin_beta_ok(&self, sin_beta: f64) -> bool {
        (sin_beta / self.sqrt_1_cj_sqr).abs() <= 1.0 || feq(sin_beta.abs(), self.sqrt_1_cj_sqr)
    }

    fn lon_diff(&self, sin_beta: f64, t: f64) -> Option<f64> {
        if self.is_cj_zero {
            return Some(0.0);
        }
        if !self.sin_beta_ok(sin_beta) {
            return None;
        }
        let dlj = self.d_lambda(sin_beta);
        let delta_k = self.sign_lon_diff * (-self.asin_tj_t0j + dlj);
        let asin_t_t0j = (t / self.t0j).clamp(-1.0, 1.0).asin();
        Some(self.sign_lon_diff * asin_t_t0j + delta_k)
    }

    fn k_diffs(&self, sin_beta: f64, vd: &Vertex, check: bool) -> Option<(f64, f64)> {
        if self.is_cj_zero {
            return Some((0.0, self.sign_lon_diff * PI));
        }
        if check && !self.sin_beta_ok(sin_beta) {
            return None;
        }
        let dlj = self.d_lambda(sin_beta);
        let before = self.sign_lon_diff * (-self.asin_tj_t0j + dlj);
        let behind = self.sign_lon_diff * (PI - self.asin_tj_t0j + vd.dl0j + (vd.dl0j - dlj));
        Some((before, behind))
    }

    fn lat(&self, t: f64) -> f64 {
        (t / self.one_minus_f).atan()
    }

    fn lon_of_equator_intersection(&self) -> f64 {
        let dlj = self.d_lambda(0.0);
        self.lonj - self.asin_tj_t0j + dlj
    }
}

/// `sjoberg_intersection::apply` (radians): the geodesics' intersection.
#[allow(clippy::too_many_arguments)]
fn sjoberg_intersection(lon_a1: f64, lat_a1: f64, lon_a2: f64, lat_a2: f64, alpha_a1: f64, lon_b1: f64, lat_b1: f64, lon_b2: f64, lat_b2: f64, alpha_b1: f64) -> Option<(f64, f64)> {
    let f = flattening(1.0, unit_b());
    let one_minus_f = 1.0 - f;
    let g1 = Geodesic::new(lon_a1, lat_a1, alpha_a1, f);
    let g2 = Geodesic::new(lon_b1, lat_b1, alpha_b1, f);
    if g1.is_on_equator && g2.is_on_equator {
        return None;
    } else if g1.is_on_equator {
        return Some((g2.lon_of_equator_intersection(), 0.0));
    } else if g2.is_on_equator {
        return Some((g1.lon_of_equator_intersection(), 0.0));
    }
    let lon1_minus_lon2 = lon_dist_rad(g2.lonj, g1.lonj);
    if g1.is_cj_zero && g2.is_cj_zero {
        if feq(lon1_minus_lon2, 0.0) || feq(lon1_minus_lon2 + if lon1_minus_lon2 < 0.0 { PI } else { -PI }, 0.0) {
            return None;
        }
        let half = PI / 2.0;
        let (abs1, abs2) = (lat_a1.abs(), lat_a2.abs());
        let lat = if feq(abs1, abs2) {
            half
        } else {
            let closer = if abs1 > abs2 { lat_a1 } else { lat_a2 };
            if closer >= 0.0 { half } else { -half }
        };
        return Some((0.0, lat));
    }
    let (lon_sph, tan_lat_sph) = sjoberg_spherical(lon_a1, lat_a1, lon_a2, lat_a2, lon_b1, lat_b1, lon_b2, lat_b2).unwrap_or((0.0, 0.0));
    if feq(f, 0.0) {
        return Some((lon_sph, tan_lat_sph.atan()));
    }
    let t = one_minus_f * tan_lat_sph;
    let beta = t.atan();
    if let Some((lon, lat)) = newton(&g1, &g2, beta, t, lon1_minus_lon2, lon_sph)
        && lon_ok(&g1, lon_a1, lon_a2, lon)
        && lon_ok(&g2, lon_b1, lon_b2, lon)
    {
        return Some((lon, lat));
    }
    converge_07(&g1, &g2, beta, t, lon1_minus_lon2, lon_sph)
}

fn newton(g1: &Geodesic, g2: &Geodesic, mut beta: f64, mut t: f64, lon1_minus_lon2: f64, lon_sph: f64) -> Option<(f64, f64)> {
    let e_sqr = g1.e_sqr;
    let (mut lon1_diff, mut lon2_diff) = (0.0, 0.0);
    let half = PI / 2.0;
    if g1.is_cj_zero && (g1.lonj - lon_sph).abs() > half {
        return None;
    }
    if g2.is_cj_zero && (g2.lonj - lon_sph).abs() > half {
        return None;
    }
    let mut abs_dbeta_last = 0.0;
    for i in 0..10 {
        let cos_beta = beta.cos();
        if feq(cos_beta, 0.0) {
            return None;
        }
        let sin_beta = beta.sin();
        let cos_beta_sqr = cos_beta * cos_beta;
        let g = 1.0 - e_sqr * cos_beta_sqr;
        let (mut f1, mut f2) = (0.0, 0.0);
        if !g1.is_cj_zero {
            lon1_diff = g1.lon_diff(sin_beta, t)?;
            let h = cos_beta_sqr - g1.cj_sqr;
            if feq(h, 0.0) {
                return None;
            }
            f1 = g1.cj / cos_beta * (g / h).sqrt();
        }
        if !g2.is_cj_zero {
            lon2_diff = g2.lon_diff(sin_beta, t)?;
            let h = cos_beta_sqr - g2.cj_sqr;
            if feq(h, 0.0) {
                return None;
            }
            f2 = g2.cj / cos_beta * (g / h).sqrt();
        }
        let denom = f1 - f2;
        if feq(denom, 0.0) {
            return None;
        }
        let dbeta = (lon1_minus_lon2 + lon1_diff - lon2_diff) / denom;
        let abs_dbeta = dbeta.abs();
        if i > 0 && abs_dbeta > abs_dbeta_last {
            return None;
        }
        abs_dbeta_last = abs_dbeta;
        if feq(dbeta, 0.0) {
            break;
        }
        beta -= dbeta;
        t = beta.tan();
    }
    let lat = g1.lat(t);
    let lon = if !g1.is_cj_zero { g1.lonj + lon1_diff } else { g2.lonj + lon2_diff };
    Some((lon, lat))
}

fn lon_ok(g: &Geodesic, lon1: f64, lon2: f64, lon: f64) -> bool {
    if g.is_cj_zero {
        return true;
    }
    let mut d1p = lon_dist_rad(lon1, lon);
    let mut d12 = lon_dist_rad(lon1, lon2);
    if d12 < 0.0 {
        d1p = -d1p;
        d12 = -d12;
    }
    (0.0 <= d1p && d1p <= d12) || feq(d1p, 0.0) || feq(d1p, d12)
}

#[derive(Clone, Copy, Default)]
struct C07 {
    lon1: f64,
    lon2: f64,
    k1: f64,
    k2: f64,
    t1: f64,
    t2: f64,
}

fn c07_one(sin_beta: f64, t: f64, g: &Geodesic, v: &Vertex, lon_sph: f64, check: bool) -> Option<(f64, f64)> {
    let (before, behind) = g.k_diffs(sin_beta, v, check)?;
    let asin_t_t0j = if !g.is_cj_zero { (t / g.t0j).clamp(-1.0, 1.0).asin() } else { 0.0 };
    let s = g.sign_lon_diff * asin_t_t0j;
    let lon_before = g.lonj + s + before;
    let lon_behind = g.lonj - s + behind;
    let db = lon_dist_rad(lon_before, lon_sph);
    let dh = lon_dist_rad(lon_behind, lon_sph);
    if db.abs() <= dh.abs() { Some((lon_before, before)) } else { Some((lon_behind, behind)) }
}

fn calc_ts(t: f64, k: f64, g1: &Geodesic, g2: &Geodesic) -> (f64, f64) {
    let kk = k.sin();
    let (mut t1, mut t2);
    if g1.is_cj_zero {
        t1 = kk * g2.t0j;
        t2 = -t1;
    } else if g2.is_cj_zero {
        t1 = -kk * g1.t0j;
        t2 = -t1;
    } else {
        let a = g1.t0j * g1.t0j + g2.t0j * g2.t0j;
        let b = 2.0 * g1.t0j * g2.t0j * (1.0 - kk * kk).sqrt();
        let k_t = kk * g1.t0j * g2.t0j;
        let d1 = (a + b).sqrt();
        let d2 = (a - b).sqrt();
        let n1 = if feq(d1, 0.0) { 0.0 } else { k_t / d1 };
        let n2 = if feq(d2, 0.0) { 0.0 } else { k_t / d2 };
        let (n3, n4) = (-n1, -n2);
        let abs_max = n1.abs().max(n2.abs());
        t1 = -abs_max;
        t2 = abs_max;
        if t1 < t {
            for n in [n1, n2, n3, n4] {
                if n < t && n > t1 {
                    t1 = n;
                }
            }
        }
        if t2 > t {
            for n in [n1, n2, n3, n4] {
                if n > t && n < t2 {
                    t2 = n;
                }
            }
        }
    }
    if (t - t2).abs() < (t - t1).abs() {
        std::mem::swap(&mut t1, &mut t2);
    }
    (t1, t2)
}

#[allow(clippy::too_many_arguments)]
fn c07_step(sin_beta: f64, t: f64, l12: f64, g1: &Geodesic, v1: &Vertex, g2: &Geodesic, v2: &Vertex, lon_sph: f64, check: bool) -> Option<C07> {
    let (lon1, k1) = c07_one(sin_beta, t, g1, v1, lon_sph, check)?;
    let (lon2, k2) = c07_one(sin_beta, t, g2, v2, lon_sph, check)?;
    let k = l12 + k1 - k2;
    let (t1, t2) = calc_ts(t, k, g1, g2);
    Some(C07 { lon1, lon2, k1, k2, t1, t2 })
}

fn c07_update(t: &mut f64, beta: &mut f64, t_new: f64) -> bool {
    let beta_new = t_new.atan();
    let dbeta = beta_new - *beta;
    *beta = beta_new;
    *t = t_new;
    feq(dbeta, 0.0)
}

fn converge_07(g1: &Geodesic, g2: &Geodesic, mut beta: f64, mut t: f64, l12: f64, lon_sph: f64) -> Option<(f64, f64)> {
    let (v1, v2) = (g1.vertex(), g2.vertex());
    let mut result = c07_step(beta.sin(), t, l12, g1, &v1, g2, &v2, lon_sph, false)?;
    let mut t_direction = 0;
    let mut lon_diff_prev = lon_dist_rad(result.lon1, result.lon2).abs();
    for _ in 2..20 {
        let (mut c1, mut c2) = (result.t1, result.t2);
        if t_direction < 0 {
            c1 = result.t1.min(result.t2);
            c2 = result.t1.max(result.t2);
        } else if t_direction > 0 {
            c1 = result.t1.max(result.t2);
            c2 = result.t1.min(result.t2);
        } else {
            t_direction = if c1 < c2 { -1 } else { 1 };
        }
        let (mut t1, mut beta1) = (t, beta);
        if c07_update(&mut t1, &mut beta1, c1) {
            break;
        }
        let mut try_t2 = false;
        match c07_step(beta1.sin(), t1, l12, g1, &v1, g2, &v2, lon_sph, true) {
            Some(cur) => {
                let d1 = lon_dist_rad(cur.lon1, cur.lon2).abs();
                if lon_diff_prev > d1 {
                    t = t1;
                    beta = beta1;
                    lon_diff_prev = d1;
                    result = cur;
                } else if c1 != c2 {
                    try_t2 = true;
                } else {
                    break;
                }
            }
            None => {
                if c1 != c2 {
                    try_t2 = true;
                } else {
                    return None;
                }
            }
        }
        if try_t2 {
            let (mut t2, mut beta2) = (t, beta);
            if c07_update(&mut t2, &mut beta2, c2) {
                break;
            }
            let cur = c07_step(beta2.sin(), t2, l12, g1, &v1, g2, &v2, lon_sph, true)?;
            let d2 = lon_dist_rad(cur.lon1, cur.lon2).abs();
            if lon_diff_prev > d2 {
                t_direction *= -1;
                t = t2;
                beta = beta2;
                lon_diff_prev = d2;
                result = cur;
            } else {
                break;
            }
        }
    }
    let lat = g1.lat(t);
    let mut lon = if !g1.is_cj_zero { result.lon1 } else { result.lon2 };
    normalize_lon_rad(&mut lon);
    let _ = (result.k1, result.k2);
    Some((lon, lat))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes() {
        // the geodesic's vertex on the spheroid (the sphere's would be 52.7476)
        let b = segment_envelope(Pt { x: 0.0, y: 50.0 }, Pt { x: 50.0, y: 50.0 });
        assert_eq!(crate::util::fmt_g_prec(b.max.y, 6), "52.7549");
    }
}
