//! H3's coordinate systems and index arithmetic: `ijk` hex coordinates ([`Ijk`]), 2D hex-plane
//! vectors, face-centered coordinates on the icosahedron ([`FaceIjk`]), the 64-bit index layout,
//! and the projections between the sphere and a face — following Elasticsearch's port
//! (`CoordIJK`, `Vec2d`, `Vec3d`, `FaceIJK`, `H3Index`, `LatLng`) operation for operation.

// H3's constants, verbatim.
#![allow(clippy::excessive_precision, clippy::unreadable_literal)]

use super::base_cells::{BASE_CELL_DATA, FACE_IJK_BASE_CELLS};
use super::fast_math as fm;
use crate::format::java_round;

pub(super) const M_SQRT3_2: f64 = 0.8660254037844386467637231707529361834714;
pub(super) const M_2PI: f64 = 2.0 * std::f64::consts::PI;
pub(super) const NUM_BASE_CELLS: i32 = 122;
pub(super) const NUM_HEX_VERTS: usize = 6;
pub(super) const NUM_PENT_VERTS: usize = 5;
pub(super) const M_SQRT7: f64 = 2.6457513110645905905016157536392604257102;
pub(super) const M_RSQRT7: f64 = 1.0 / M_SQRT7;
pub(super) const RES0_U_GNOMONIC: f64 = 0.38196601125010500003;
pub(super) const M_AP7_ROT_RADS: f64 = 0.333473172251832115336090755351601070065900389;
pub(super) const EPSILON: f64 = 0.0000000000000001;
pub(super) const MAX_H3_RES: i32 = 15;
const M_ONESEVENTH: f64 = 1.0 / 7.0;
const M_RSIN60: f64 = 1.0 / M_SQRT3_2;
const M_ONETHIRD: f64 = 1.0 / 3.0;
const VEC2D_RESOLUTION: f64 = 1e-7;
const M_PI_2: f64 = std::f64::consts::FRAC_PI_2;
const MINIMUM_ANGULAR_RESOLUTION: f64 = std::f64::consts::PI * 1.0e-12;

// ---- digits ----

pub(super) const CENTER_DIGIT: i32 = 0;
pub(super) const K_AXES_DIGIT: i32 = 1;
const INVALID_DIGIT: i32 = 7;

/// `CoordIJK.rotate60cw(digit)`.
fn rotate_digit_cw(d: i32) -> i32 {
    match d {
        1 => 3,
        3 => 2,
        2 => 6,
        6 => 4,
        4 => 5,
        5 => 1,
        other => other,
    }
}

/// `CoordIJK.rotate60ccw(digit)`.
fn rotate_digit_ccw(d: i32) -> i32 {
    match d {
        1 => 5,
        5 => 4,
        4 => 6,
        6 => 2,
        2 => 3,
        3 => 1,
        other => other,
    }
}

const UNIT_VECS: [[i32; 3]; 7] = [[0, 0, 0], [0, 0, 1], [0, 1, 0], [0, 1, 1], [1, 0, 0], [1, 0, 1], [1, 1, 0]];

// ---- ijk ----

/// `CoordIJK`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ijk {
    pub i: i32,
    pub j: i32,
    pub k: i32,
}

impl Ijk {
    pub const fn new(i: i32, j: i32, k: i32) -> Ijk {
        Ijk { i, j, k }
    }

    fn add(&mut self, i: i32, j: i32, k: i32) {
        self.i += i;
        self.j += j;
        self.k += k;
    }

    fn sub(&mut self, i: i32, j: i32, k: i32) {
        self.i -= i;
        self.j -= j;
        self.k -= k;
    }

    pub fn normalize(&mut self) {
        let min = self.i.min(self.j.min(self.k));
        self.sub(min, min, min);
    }

    fn set_normalized(&mut self, i: i32, j: i32, k: i32) {
        *self = Ijk::new(i, j, k);
        self.normalize();
    }

    pub fn down_ap7(&mut self) {
        let (i, j, k) = (self.i * 3 + self.j, self.j * 3 + self.k, self.k * 3 + self.i);
        self.set_normalized(i, j, k);
    }

    pub fn down_ap7r(&mut self) {
        let (i, j, k) = (self.i * 3 + self.k, self.j * 3 + self.i, self.k * 3 + self.j);
        self.set_normalized(i, j, k);
    }

    fn down_ap3(&mut self) {
        let (i, j, k) = (self.i * 2 + self.j, self.j * 2 + self.k, self.k * 2 + self.i);
        self.set_normalized(i, j, k);
    }

    fn down_ap3r(&mut self) {
        let (i, j, k) = (self.i * 2 + self.k, self.j * 2 + self.i, self.k * 2 + self.j);
        self.set_normalized(i, j, k);
    }

    fn rotate60cw(&mut self) {
        let (i, j, k) = (self.i + self.j, self.j + self.k, self.i + self.k);
        self.set_normalized(i, j, k);
    }

    fn rotate60ccw(&mut self) {
        let (i, j, k) = (self.i + self.k, self.i + self.j, self.j + self.k);
        self.set_normalized(i, j, k);
    }

    fn neighbor(&mut self, digit: i32) {
        if digit > CENTER_DIGIT && digit < INVALID_DIGIT {
            let u = UNIT_VECS[digit as usize];
            self.add(u[0], u[1], u[2]);
            self.normalize();
        }
    }

    fn up_ap7r(&mut self) {
        let (i, j) = (self.i - self.k, self.j - self.k);
        let ni = java_round((2 * i + j) as f64 * M_ONESEVENTH) as i32;
        let nj = java_round((3 * j - i) as f64 * M_ONESEVENTH) as i32;
        self.set_normalized(ni, nj, 0);
    }

    fn up_ap7(&mut self) {
        let (i, j) = (self.i - self.k, self.j - self.k);
        let ni = java_round((3 * i - j) as f64 * M_ONESEVENTH) as i32;
        let nj = java_round((2 * j + i) as f64 * M_ONESEVENTH) as i32;
        self.set_normalized(ni, nj, 0);
    }

    fn unit_to_digit(&self) -> i32 {
        if self.i.min(self.j.min(self.k)) < 0 || self.i.max(self.j.max(self.k)) > 1 {
            return INVALID_DIGIT;
        }
        (self.i << 2) | (self.j << 1) | self.k
    }

    fn to_hex2d(self) -> (f64, f64) {
        let i = self.i - self.k;
        let j = self.j - self.k;
        (i as f64 - 0.5 * j as f64, j as f64 * M_SQRT3_2)
    }

    fn to_geo(self, face: usize, res: i32, substrate: bool) -> LatLng {
        let (x, y) = self.to_hex2d();
        hex2d_to_geo(x, y, face, res, substrate)
    }
}

// ---- lat/lng ----

/// `LatLng` (radians).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LatLng {
    pub lat: f64,
    pub lon: f64,
}

impl LatLng {
    /// `geoAzimuthRads(lat, lon)`.
    fn azimuth_to(&self, lat: f64, lon: f64) -> f64 {
        let cos_lat = fm::cos(lat);
        fm::atan2(cos_lat * fm::sin(lon - self.lon), fm::cos(self.lat) * fm::sin(lat) - fm::sin(self.lat) * cos_lat * fm::cos(lon - self.lon))
    }

    fn numerically_identical(&self, o: &LatLng) -> bool {
        (self.lat - o.lat).abs() < MINIMUM_ANGULAR_RESOLUTION && (self.lon - o.lon).abs() < MINIMUM_ANGULAR_RESOLUTION
    }

    /// `greatCircleMaxLatitude`: the highest latitude on the edge to `o`.
    pub fn great_circle_max_latitude(&self, o: &LatLng) -> f64 {
        if self.numerically_identical(o) {
            return o.lat;
        }
        let (a, b) = if o.lat > self.lat { (o, self) } else { (self, o) };
        let az = a.azimuth_to(b.lat, b.lon);
        if az.abs() < M_PI_2 { fm::acos((fm::sin(az) * fm::cos(a.lat)).abs()) } else { a.lat }
    }

    /// `greatCircleMinLatitude`: the lowest latitude on the edge to `o`.
    pub fn great_circle_min_latitude(&self, o: &LatLng) -> f64 {
        if self.numerically_identical(o) {
            return o.lat;
        }
        let (a, b) = if o.lat < self.lat { (o, self) } else { (self, o) };
        let az = a.azimuth_to(b.lat, b.lon);
        if az.abs() > M_PI_2 { -fm::acos((fm::sin(az) * fm::cos(a.lat)).abs()) } else { a.lat }
    }
}

// ---- hex plane ----

/// `Vec2d.faceAxesAzRadsCII`.
const FACE_AXES_AZ_RADS_CII: [[f64; 3]; 20] = [
    [5.619958268523939882, 3.525563166130744542, 1.431168063737548730],
    [5.760339081714187279, 3.665943979320991689, 1.571548876927796127],
    [0.780213654393430055, 4.969003859179821079, 2.874608756786625655],
    [0.430469363979999913, 4.619259568766391033, 2.524864466373195467],
    [6.130269123335111400, 4.035874020941915804, 1.941478918548720291],
    [2.692877706530642877, 0.598482604137447119, 4.787272808923838195],
    [2.982963003477243874, 0.888567901084048369, 5.077358105870439581],
    [3.532912002790141181, 1.438516900396945656, 5.627307105183336758],
    [3.494305004259568154, 1.399909901866372864, 5.588700106652763840],
    [3.003214169499538391, 0.908819067106342928, 5.097609271892733906],
    [5.930472956509811562, 3.836077854116615875, 1.741682751723420374],
    [0.138378484090254847, 4.327168688876645809, 2.232773586483450311],
    [0.448714947059150361, 4.637505151845541521, 2.543110049452346120],
    [0.158629650112549365, 4.347419854898940135, 2.253024752505744869],
    [5.891865957979238535, 3.797470855586042958, 1.703075753192847583],
    [2.711123289609793325, 0.616728187216597771, 4.805518392002988683],
    [3.294508837434268316, 1.200113735041072948, 5.388903939827463911],
    [3.804819692245439833, 1.710424589852244509, 5.899214794638635174],
    [3.664438879055192436, 1.570043776661997111, 5.758833981448388027],
    [2.361378999196363184, 0.266983896803167583, 4.455774101589558636],
];

/// `Vec2d.faceCenterGeo`.
const FACE_CENTER_GEO: [LatLng; 20] = [
    LatLng { lat: 0.803582649718989942, lon: 1.248397419617396099 },
    LatLng { lat: 1.307747883455638156, lon: 2.536945009877921159 },
    LatLng { lat: 1.054751253523952054, lon: -1.347517358900396623 },
    LatLng { lat: 0.600191595538186799, lon: -0.450603909469755746 },
    LatLng { lat: 0.491715428198773866, lon: 0.401988202911306943 },
    LatLng { lat: 0.172745327415618701, lon: 1.678146885280433686 },
    LatLng { lat: 0.605929321571350690, lon: 2.953923329812411617 },
    LatLng { lat: 0.427370518328979641, lon: -1.888876200336285401 },
    LatLng { lat: -0.079066118549212831, lon: -0.733429513380867741 },
    LatLng { lat: -0.230961644455383637, lon: 0.506495587332349035 },
    LatLng { lat: 0.079066118549212831, lon: 2.408163140208925497 },
    LatLng { lat: 0.230961644455383637, lon: -2.635097066257444203 },
    LatLng { lat: -0.172745327415618701, lon: -1.463445768309359553 },
    LatLng { lat: -0.605929321571350690, lon: -0.187669323777381622 },
    LatLng { lat: -0.427370518328979641, lon: 1.252716453253507838 },
    LatLng { lat: -0.600191595538186799, lon: 2.690988744120037492 },
    LatLng { lat: -0.491715428198773866, lon: -2.739604450678486295 },
    LatLng { lat: -0.803582649718989942, lon: -1.893195233972397139 },
    LatLng { lat: -1.307747883455638156, lon: -0.604647643711872080 },
    LatLng { lat: -1.054751253523952054, lon: 1.794075294689396615 },
];

/// `Vec3d.faceCenterPoint`.
const FACE_CENTER_POINT: [[f64; 3]; 20] = [
    [0.2199307791404606, 0.6583691780274996, 0.7198475378926182],
    [-0.2139234834501421, 0.1478171829550703, 0.9656017935214205],
    [0.1092625278784797, -0.4811951572873210, 0.8697775121287253],
    [0.7428567301586791, -0.3593941678278028, 0.5648005936517033],
    [0.8112534709140969, 0.3448953237639384, 0.4721387736413930],
    [-0.1055498149613921, 0.9794457296411413, 0.1718874610009365],
    [-0.8075407579970092, 0.1533552485898818, 0.5695261994882688],
    [-0.2846148069787907, -0.8644080972654206, 0.4144792552473539],
    [0.7405621473854482, -0.6673299564565524, -0.0789837646326737],
    [0.8512303986474293, 0.4722343788582681, -0.2289137388687808],
    [-0.7405621473854481, 0.6673299564565524, 0.0789837646326737],
    [-0.8512303986474292, -0.4722343788582682, 0.2289137388687808],
    [0.1055498149613919, -0.9794457296411413, -0.1718874610009365],
    [0.8075407579970092, -0.1533552485898819, -0.5695261994882688],
    [0.2846148069787908, 0.8644080972654204, -0.4144792552473539],
    [-0.7428567301586791, 0.3593941678278027, -0.5648005936517033],
    [-0.8112534709140971, -0.3448953237639382, -0.4721387736413930],
    [-0.2199307791404607, -0.6583691780274996, -0.7198475378926182],
    [0.2139234834501420, -0.1478171829550704, -0.9656017935214205],
    [-0.1092625278784796, 0.4811951572873210, -0.8697775121287253],
];

/// `Vec2d.posAngleRads`.
fn pos_angle_rads(rads: f64) -> f64 {
    if rads < 0.0 {
        rads + M_2PI
    } else if rads >= M_2PI {
        rads - M_2PI
    } else {
        rads
    }
}

#[inline]
fn is_class_iii(res: i32) -> bool {
    (res & 1) == 1
}

/// `Vec2d.hex2dToGeo`.
fn hex2d_to_geo(x: f64, y: f64, face: usize, res: i32, substrate: bool) -> LatLng {
    let mut r = (x * x + y * y).sqrt();
    if r < EPSILON {
        return FACE_CENTER_GEO[face];
    }
    let mut theta = fm::atan2(y, x);
    for _ in 0..res {
        r *= M_RSQRT7;
    }
    if substrate {
        r *= M_ONETHIRD;
        if is_class_iii(res) {
            r *= M_RSQRT7;
        }
    }
    r *= RES0_U_GNOMONIC;
    r = fm::atan(r);
    if !substrate && is_class_iii(res) {
        theta = pos_angle_rads(theta + M_AP7_ROT_RADS);
    }
    theta = pos_angle_rads(FACE_AXES_AZ_RADS_CII[face][0] - theta);
    geo_az_distance_rads(&FACE_CENTER_POINT[face], theta, r)
}

/// `Vec3d.geoAzDistanceRads`: the point `distance` radians from `p` along azimuth `az`.
fn geo_az_distance_rads(p: &[f64; 3], az: f64, distance: f64) -> LatLng {
    let az = pos_angle_rads(az);
    let magnitude = (p[0] * p[0] + p[1] * p[1] + 0.0 * 0.0).sqrt();
    let de_x = -p[1] / magnitude;
    let de_y = p[0] / magnitude;
    let dn_x = -p[2] * de_y;
    let dn_y = p[2] * de_x;
    let dn_z = p[0] * de_y - p[1] * de_x;
    let sin_az = fm::sin(az);
    let cos_az = fm::cos(az);
    let sin_d = fm::sin(distance);
    let cos_d = fm::cos(distance);
    let d_x = dn_x * cos_az + de_x * sin_az;
    let d_y = dn_y * cos_az + de_y * sin_az;
    let d_z = dn_z * cos_az;
    let n2_x = p[0] * cos_d + d_x * sin_d;
    let n2_y = p[1] * cos_d + d_y * sin_d;
    let n2_z = p[2] * cos_d + d_z * sin_d;
    LatLng { lat: fm::asin(n2_z), lon: fm::atan2(n2_y, n2_x) }
}

/// `Vec2d.hex2dToCoordIJK`: the hex containing a hex-plane point.
fn hex2d_to_ijk(x: f64, y: f64) -> Ijk {
    let a1 = x.abs();
    let a2 = y.abs();
    let x2 = a2 * M_RSIN60;
    let x1 = a1 + x2 * 0.5;
    let m1 = x1 as i32;
    let m2 = x2 as i32;
    let r1 = x1 - m1 as f64;
    let r2 = x2 - m2 as f64;
    let (mut i, mut j);
    if r1 < 0.5 {
        if r1 < M_ONETHIRD {
            i = m1;
            j = if r2 < (1.0 + r1) * 0.5 { m2 } else { m2 + 1 };
        } else {
            j = if r2 < (1.0 - r1) { m2 } else { m2 + 1 };
            i = if (1.0 - r1) <= r2 && r2 < (2.0 * r1) { m1 + 1 } else { m1 };
        }
    } else if r1 < 2.0 * M_ONETHIRD {
        j = if r2 < (1.0 - r1) { m2 } else { m2 + 1 };
        i = if (2.0 * r1 - 1.0) < r2 && r2 < (1.0 - r1) { m1 } else { m1 + 1 };
    } else {
        i = m1 + 1;
        j = if r2 < (r1 * 0.5) { m2 } else { m2 + 1 };
    }
    if x < 0.0 {
        if j % 2 == 0 {
            let axisi = j / 2;
            let diff = i - axisi;
            i -= 2 * diff;
        } else {
            let axisi = (j + 1) / 2;
            let diff = i - axisi;
            i -= 2 * diff + 1;
        }
    }
    if y < 0.0 {
        i -= (2 * j + 1) / 2;
        j = -j;
    }
    let mut c = Ijk::new(i, j, 0);
    c.normalize();
    c
}

// ---- the index ----

pub(super) const H3_INIT: u64 = 35184372088831;
const H3_MODE_OFFSET: u32 = 59;
const H3_BC_OFFSET: u32 = 45;
const H3_RES_OFFSET: u32 = 52;
const H3_DIGIT_MASK: u64 = 7;

#[inline]
pub(super) fn get_base_cell(h: u64) -> i32 {
    ((h >> H3_BC_OFFSET) & 127) as i32
}

#[inline]
pub(super) fn get_resolution(h: u64) -> i32 {
    ((h >> H3_RES_OFFSET) & 15) as i32
}

#[inline]
pub(super) fn get_digit(h: u64, res: i32) -> i32 {
    ((h >> ((MAX_H3_RES - res) * 3)) & H3_DIGIT_MASK) as i32
}

#[inline]
pub(super) fn set_digit(h: u64, res: i32, digit: i32) -> u64 {
    let x = (MAX_H3_RES - res) * 3;
    (h & !(H3_DIGIT_MASK << x)) | ((digit as u64) << x)
}

pub(super) fn is_base_cell_pentagon(bc: i32) -> bool {
    (0..NUM_BASE_CELLS).contains(&bc) && BASE_CELL_DATA[bc as usize].2
}

pub(super) fn base_cell_is_cw_offset(bc: i32, face: usize) -> bool {
    let o = BASE_CELL_DATA[bc as usize].3;
    o[0] as i32 == face as i32 || o[1] as i32 == face as i32
}

/// `h3LeadingNonZeroDigit`.
pub(super) fn leading_non_zero_digit(h: u64) -> i32 {
    for r in 1..=get_resolution(h) {
        let d = get_digit(h, r);
        if d != CENTER_DIGIT {
            return d;
        }
    }
    CENTER_DIGIT
}

pub(super) fn is_pentagon(h: u64) -> bool {
    is_base_cell_pentagon(get_base_cell(h)) && leading_non_zero_digit(h) == 0
}

pub(super) fn rotate60cw(mut h: u64) -> u64 {
    for r in 1..=get_resolution(h) {
        h = set_digit(h, r, rotate_digit_cw(get_digit(h, r)));
    }
    h
}

pub(super) fn rotate60ccw(mut h: u64) -> u64 {
    for r in 1..=get_resolution(h) {
        h = set_digit(h, r, rotate_digit_ccw(get_digit(h, r)));
    }
    h
}

pub(super) fn rotate_pent60ccw(mut h: u64) -> u64 {
    let mut found = false;
    for r in 1..=get_resolution(h) {
        h = set_digit(h, r, rotate_digit_ccw(get_digit(h, r)));
        if !found && get_digit(h, r) != 0 {
            found = true;
            if leading_non_zero_digit(h) == K_AXES_DIGIT {
                h = rotate60ccw(h);
            }
        }
    }
    h
}

/// `Vec3d.geoToH3`: the cell of resolution `res` containing `(lat, lon)` (radians).
pub(super) fn geo_to_h3(res: i32, lat: f64, lon: f64) -> u64 {
    let cos_lat = fm::cos(lat);
    let z = fm::sin(lat);
    let x = fm::cos(lon) * cos_lat;
    let y = fm::sin(lon) * cos_lat;
    let sq = |a: f64| a * a;
    let dist = |c: &[f64; 3]| sq(x - c[0]) + sq(y - c[1]) + sq(z - c[2]);
    let mut face = 0;
    let mut sqd = dist(&FACE_CENTER_POINT[0]);
    for (i, c) in FACE_CENTER_POINT.iter().enumerate().skip(1) {
        let d = dist(c);
        if d < sqd {
            face = i;
            sqd = d;
        }
    }
    let mut r = fm::acos(1.0 - sqd * 0.5);
    if r < EPSILON {
        return face_ijk_to_h3(res, face, Ijk::new(0, 0, 0));
    }
    let mut theta = pos_angle_rads(FACE_AXES_AZ_RADS_CII[face][0] - pos_angle_rads(geo_azimuth_rads(&FACE_CENTER_POINT[face], x, y, z)));
    if is_class_iii(res) {
        theta = pos_angle_rads(theta - M_AP7_ROT_RADS);
    }
    r = fm::tan(r);
    r /= RES0_U_GNOMONIC;
    for _ in 0..res {
        r *= M_SQRT7;
    }
    face_ijk_to_h3(res, face, hex2d_to_ijk(r * fm::cos(theta), r * fm::sin(theta)))
}

/// `Vec3d.geoAzimuthRads`: azimuth from face center `c` to the unit vector `(x, y, z)`.
fn geo_azimuth_rads(c: &[f64; 3], x: f64, y: f64, z: f64) -> f64 {
    let c1x = c[1] * z - c[2] * y;
    let c1y = c[2] * x - c[0] * z;
    let c1z = c[0] * y - c[1] * x;
    let c2x = c[1];
    let c2y = -c[0];
    let c2z = 0.0;
    let c1c2x = c1y * c2z - c1z * c2y;
    let c1c2y = c1z * c2x - c1x * c2z;
    let c1c2z = c1x * c2y - c1y * c2x;
    let sign = java_signum(c[0] * c1c2x + c[1] * c1c2y + c[2] * c1c2z);
    let magnitude = (c1c2x * c1c2x + c1c2y * c1c2y + c1c2z * c1c2z).sqrt();
    fm::atan2(sign * magnitude, c1x * c2x + c1y * c2y + c1z * c2z)
}

/// `Math.signum`.
fn java_signum(d: f64) -> f64 {
    if d == 0.0 || d.is_nan() { d } else { 1.0f64.copysign(d) }
}

/// `FaceIJK.faceIjkToH3`.
fn face_ijk_to_h3(res: i32, face: usize, mut coord: Ijk) -> u64 {
    let mut h = H3_INIT;
    h = (h & !(15u64 << H3_MODE_OFFSET)) | (1u64 << H3_MODE_OFFSET);
    h = (h & !(15u64 << H3_RES_OFFSET)) | ((res as u64) << H3_RES_OFFSET);
    let set_bc = |h: u64, bc: u64| (h & !(127u64 << H3_BC_OFFSET)) | (bc << H3_BC_OFFSET);
    let lookup = |c: &Ijk| FACE_IJK_BASE_CELLS[face][c.i as usize][c.j as usize][c.k as usize];
    if res == 0 {
        return set_bc(h, lookup(&coord).0 as u64);
    }
    for r in (1..=res).rev() {
        let last = coord;
        let mut scratch;
        if is_class_iii(r) {
            coord.up_ap7();
            scratch = coord;
            scratch.down_ap7();
        } else {
            coord.up_ap7r();
            scratch = coord;
            scratch.down_ap7r();
        }
        scratch.set_normalized(last.i - scratch.i, last.j - scratch.j, last.k - scratch.k);
        h = set_digit(h, r, scratch.unit_to_digit());
    }
    let (bc, rots) = lookup(&coord);
    let bc = bc as i32;
    h = set_bc(h, bc as u64);
    if is_base_cell_pentagon(bc) {
        if leading_non_zero_digit(h) == K_AXES_DIGIT {
            h = if base_cell_is_cw_offset(bc, face) { rotate60cw(h) } else { rotate60ccw(h) };
        }
        for _ in 0..rots {
            h = rotate_pent60ccw(h);
        }
    } else {
        for _ in 0..rots {
            h = rotate60ccw(h);
        }
    }
    h
}

// ---- face coordinates and cell boundaries ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Overage {
    None,
    FaceEdge,
    NewFace,
}

const IJ: usize = 1;
const KI: usize = 2;
const JK: usize = 3;

const MAX_DIM_BY_CII_RES: [i32; 17] = [2, -1, 14, -1, 98, -1, 686, -1, 4802, -1, 33614, -1, 235298, -1, 1647086, -1, 11529602];
const UNIT_SCALE_BY_CII_RES: [i32; 17] = [1, -1, 7, -1, 49, -1, 343, -1, 2401, -1, 16807, -1, 117649, -1, 823543, -1, 5764801];

/// `adjacentFaceDir[from][to]`: which quadrant of `from` faces `to` (`-1`: not adjacent).
const ADJACENT_FACE_DIR: [[i8; 20]; 20] = {
    const N: i8 = -1;
    const A: i8 = IJ as i8;
    const B: i8 = KI as i8;
    const C: i8 = JK as i8;
    [
        [0, B, N, N, A, C, N, N, N, N, N, N, N, N, N, N, N, N, N, N],
        [A, 0, B, N, N, N, C, N, N, N, N, N, N, N, N, N, N, N, N, N],
        [N, A, 0, B, N, N, N, C, N, N, N, N, N, N, N, N, N, N, N, N],
        [N, N, A, 0, B, N, N, N, C, N, N, N, N, N, N, N, N, N, N, N],
        [B, N, N, A, 0, N, N, N, N, C, N, N, N, N, N, N, N, N, N, N],
        [C, N, N, N, N, 0, N, N, N, N, A, N, N, N, B, N, N, N, N, N],
        [N, C, N, N, N, N, 0, N, N, N, B, A, N, N, N, N, N, N, N, N],
        [N, N, C, N, N, N, N, 0, N, N, N, B, A, N, N, N, N, N, N, N],
        [N, N, N, C, N, N, N, N, 0, N, N, N, B, A, N, N, N, N, N, N],
        [N, N, N, N, C, N, N, N, N, 0, N, N, N, B, A, N, N, N, N, N],
        [N, N, N, N, N, A, B, N, N, N, 0, N, N, N, N, C, N, N, N, N],
        [N, N, N, N, N, N, A, B, N, N, N, 0, N, N, N, N, C, N, N, N],
        [N, N, N, N, N, N, N, A, B, N, N, N, 0, N, N, N, N, C, N, N],
        [N, N, N, N, N, N, N, N, A, B, N, N, N, 0, N, N, N, N, C, N],
        [N, N, N, N, N, B, N, N, N, A, N, N, N, N, 0, N, N, N, N, C],
        [N, N, N, N, N, N, N, N, N, N, C, N, N, N, N, 0, A, N, N, B],
        [N, N, N, N, N, N, N, N, N, N, N, C, N, N, N, B, 0, A, N, N],
        [N, N, N, N, N, N, N, N, N, N, N, N, C, N, N, N, B, 0, A, N],
        [N, N, N, N, N, N, N, N, N, N, N, N, N, C, N, N, N, B, 0, A],
        [N, N, N, N, N, N, N, N, N, N, N, N, N, N, C, A, N, N, B, 0],
    ]
};

/// `faceNeighbors[face][quadrant]`: `(face, translate ijk, ccw 60° rotations)`.
const FACE_NEIGHBORS: [[(u8, [i32; 3], u8); 4]; 20] = [
    [(0, [0, 0, 0], 0), (4, [2, 0, 2], 1), (1, [2, 2, 0], 5), (5, [0, 2, 2], 3)],
    [(1, [0, 0, 0], 0), (0, [2, 0, 2], 1), (2, [2, 2, 0], 5), (6, [0, 2, 2], 3)],
    [(2, [0, 0, 0], 0), (1, [2, 0, 2], 1), (3, [2, 2, 0], 5), (7, [0, 2, 2], 3)],
    [(3, [0, 0, 0], 0), (2, [2, 0, 2], 1), (4, [2, 2, 0], 5), (8, [0, 2, 2], 3)],
    [(4, [0, 0, 0], 0), (3, [2, 0, 2], 1), (0, [2, 2, 0], 5), (9, [0, 2, 2], 3)],
    [(5, [0, 0, 0], 0), (10, [2, 2, 0], 3), (14, [2, 0, 2], 3), (0, [0, 2, 2], 3)],
    [(6, [0, 0, 0], 0), (11, [2, 2, 0], 3), (10, [2, 0, 2], 3), (1, [0, 2, 2], 3)],
    [(7, [0, 0, 0], 0), (12, [2, 2, 0], 3), (11, [2, 0, 2], 3), (2, [0, 2, 2], 3)],
    [(8, [0, 0, 0], 0), (13, [2, 2, 0], 3), (12, [2, 0, 2], 3), (3, [0, 2, 2], 3)],
    [(9, [0, 0, 0], 0), (14, [2, 2, 0], 3), (13, [2, 0, 2], 3), (4, [0, 2, 2], 3)],
    [(10, [0, 0, 0], 0), (5, [2, 2, 0], 3), (6, [2, 0, 2], 3), (15, [0, 2, 2], 3)],
    [(11, [0, 0, 0], 0), (6, [2, 2, 0], 3), (7, [2, 0, 2], 3), (16, [0, 2, 2], 3)],
    [(12, [0, 0, 0], 0), (7, [2, 2, 0], 3), (8, [2, 0, 2], 3), (17, [0, 2, 2], 3)],
    [(13, [0, 0, 0], 0), (8, [2, 2, 0], 3), (9, [2, 0, 2], 3), (18, [0, 2, 2], 3)],
    [(14, [0, 0, 0], 0), (9, [2, 2, 0], 3), (5, [2, 0, 2], 3), (19, [0, 2, 2], 3)],
    [(15, [0, 0, 0], 0), (16, [2, 0, 2], 1), (19, [2, 2, 0], 5), (10, [0, 2, 2], 3)],
    [(16, [0, 0, 0], 0), (17, [2, 0, 2], 1), (15, [2, 2, 0], 5), (11, [0, 2, 2], 3)],
    [(17, [0, 0, 0], 0), (18, [2, 0, 2], 1), (16, [2, 2, 0], 5), (12, [0, 2, 2], 3)],
    [(18, [0, 0, 0], 0), (19, [2, 0, 2], 1), (17, [2, 2, 0], 5), (13, [0, 2, 2], 3)],
    [(19, [0, 0, 0], 0), (15, [2, 0, 2], 1), (18, [2, 2, 0], 5), (14, [0, 2, 2], 3)],
];

const VERTEX_CLASSIII: [[i32; 3]; 6] = [[5, 4, 0], [1, 5, 0], [0, 5, 4], [0, 1, 5], [4, 0, 5], [5, 0, 1]];
const VERTEX_CLASSII: [[i32; 3]; 6] = [[2, 1, 0], [1, 2, 0], [0, 2, 1], [0, 1, 2], [1, 0, 2], [2, 0, 1]];

/// The vertices of a cell (at most 10: a class III cell crossing icosahedron edges).
#[derive(Clone, Debug, Default)]
pub struct CellBoundary {
    pub points: Vec<LatLng>,
}

/// `FaceIJK`.
#[derive(Clone, Copy, Debug)]
pub(super) struct FaceIjk {
    pub face: usize,
    pub coord: Ijk,
}

impl FaceIjk {
    /// `adjustOverageClassII`.
    fn adjust_overage_class_ii(&mut self, res: i32, pent_leading4: bool, substrate: bool) -> Overage {
        let mut overage = Overage::None;
        let mut max_dim = MAX_DIM_BY_CII_RES[res as usize];
        if substrate {
            max_dim *= 3;
        }
        let sum = self.coord.i + self.coord.j + self.coord.k;
        if substrate && sum == max_dim {
            overage = Overage::FaceEdge;
        } else if sum > max_dim {
            overage = Overage::NewFace;
            let orient = if self.coord.k > 0 {
                if self.coord.j > 0 {
                    FACE_NEIGHBORS[self.face][JK]
                } else {
                    if pent_leading4 {
                        self.coord.sub(max_dim, 0, 0);
                        self.coord.rotate60cw();
                        self.coord.add(max_dim, 0, 0);
                    }
                    FACE_NEIGHBORS[self.face][KI]
                }
            } else {
                FACE_NEIGHBORS[self.face][IJ]
            };
            self.face = orient.0 as usize;
            for _ in 0..orient.2 {
                self.coord.rotate60ccw();
            }
            let mut unit_scale = UNIT_SCALE_BY_CII_RES[res as usize];
            if substrate {
                unit_scale *= 3;
            }
            self.coord.add(orient.1[0] * unit_scale, orient.1[1] * unit_scale, orient.1[2] * unit_scale);
            self.coord.normalize();
            if substrate && self.coord.i + self.coord.j + self.coord.k == max_dim {
                overage = Overage::FaceEdge;
            }
        }
        overage
    }

    fn adjust_pent_vert_overage(&mut self, res: i32) {
        while self.adjust_overage_class_ii(res, false, true) == Overage::NewFace {}
    }

    /// `faceIjkToCellBoundary` / `faceIjkPentToCellBoundary`.
    fn to_cell_boundary(mut self, res: i32, pentagon: bool) -> CellBoundary {
        self.coord.down_ap3();
        self.coord.down_ap3r();
        let mut adj_res = res;
        if is_class_iii(res) {
            self.coord.down_ap7r();
            adj_res += 1;
        }
        match (pentagon, is_class_iii(res)) {
            (false, false) => self.boundary_class_ii(adj_res, NUM_HEX_VERTS, false),
            (true, false) => self.boundary_class_ii(adj_res, NUM_PENT_VERTS, true),
            (false, true) => self.hex_boundary_class_iii(adj_res),
            (true, true) => self.pent_boundary_class_iii(adj_res),
        }
    }

    fn vertex(&self, v: [i32; 3]) -> FaceIjk {
        let mut c = Ijk::new(v[0] + self.coord.i, v[1] + self.coord.j, v[2] + self.coord.k);
        c.normalize();
        FaceIjk { face: self.face, coord: c }
    }

    fn boundary_class_ii(&self, adj_res: i32, verts: usize, pentagon: bool) -> CellBoundary {
        let points = (0..verts)
            .map(|v| {
                let mut f = self.vertex(VERTEX_CLASSII[v]);
                if pentagon {
                    f.adjust_pent_vert_overage(adj_res);
                } else {
                    f.adjust_overage_class_ii(adj_res, false, true);
                }
                f.coord.to_geo(f.face, adj_res, true)
            })
            .collect();
        CellBoundary { points }
    }

    fn pent_boundary_class_iii(&self, adj_res: i32) -> CellBoundary {
        let mut points = Vec::with_capacity(10);
        let mut last_coord = Ijk::new(0, 0, 0);
        let mut last_face = self.face;
        for vert in 0..=NUM_PENT_VERTS {
            let v = vert % NUM_PENT_VERTS;
            let mut f = self.vertex(VERTEX_CLASSIII[v]);
            f.adjust_pent_vert_overage(adj_res);
            if vert > 0 {
                let orig0 = last_coord.to_hex2d();
                let dir = ADJACENT_FACE_DIR[f.face][last_face] as usize;
                let orient = FACE_NEIGHBORS[f.face][dir];
                last_coord = f.coord;
                for _ in 0..orient.2 {
                    last_coord.rotate60ccw();
                }
                let unit_scale = UNIT_SCALE_BY_CII_RES[adj_res as usize] * 3;
                last_coord.add(orient.1[0] * unit_scale, orient.1[1] * unit_scale, orient.1[2] * unit_scale);
                last_coord.normalize();
                let orig1 = last_coord.to_hex2d();
                let o_face = orient.0 as usize;
                if let Some((x, y)) = find_intersection(orig0, orig1, adj_res, ADJACENT_FACE_DIR[o_face][f.face] as usize) {
                    points.push(hex2d_to_geo(x, y, o_face, adj_res, true));
                }
            }
            if vert < NUM_PENT_VERTS {
                points.push(f.coord.to_geo(f.face, adj_res, true));
            }
            last_face = f.face;
            last_coord = f.coord;
        }
        CellBoundary { points }
    }

    fn hex_boundary_class_iii(&self, adj_res: i32) -> CellBoundary {
        let mut points = Vec::with_capacity(10);
        let mut last_face: Option<usize> = None;
        let mut last_overage = Overage::None;
        for vert in 0..=NUM_HEX_VERTS {
            let v = vert % NUM_HEX_VERTS;
            let mut f = self.vertex(VERTEX_CLASSIII[v]);
            let overage = f.adjust_overage_class_ii(adj_res, false, true);
            if vert > 0 && Some(f.face) != last_face && last_overage != Overage::FaceEdge {
                let last_v = (v + 5) % NUM_HEX_VERTS;
                let orig0 = self.vertex(VERTEX_CLASSIII[last_v]).coord.to_hex2d();
                let orig1 = self.vertex(VERTEX_CLASSIII[v]).coord.to_hex2d();
                let lf = last_face.expect("vert > 0");
                let face2 = if lf == self.face { f.face } else { lf };
                if let Some((x, y)) = find_intersection(orig0, orig1, adj_res, ADJACENT_FACE_DIR[self.face][face2] as usize) {
                    points.push(hex2d_to_geo(x, y, self.face, adj_res, true));
                }
            }
            if vert < NUM_HEX_VERTS {
                points.push(f.coord.to_geo(f.face, adj_res, true));
            }
            last_face = Some(f.face);
            last_overage = overage;
        }
        CellBoundary { points }
    }
}

/// `FaceIJK.findIntersectionPoint`: where a cell edge crosses the icosahedron edge `face_dir`
/// (`None` at a hexagon vertex).
fn find_intersection(o0: (f64, f64), o1: (f64, f64), adj_res: i32, face_dir: usize) -> Option<(f64, f64)> {
    let m = MAX_DIM_BY_CII_RES[adj_res as usize] as f64;
    let corners = [(3.0 * m, 0.0), (-1.5 * m, 3.0 * M_SQRT3_2 * m), (-1.5 * m, -3.0 * M_SQRT3_2 * m)];
    let (e0, e1) = match face_dir {
        IJ => (corners[0], corners[1]),
        JK => (corners[1], corners[2]),
        _ => (corners[2], corners[0]),
    };
    // `Vec2d.v2dIntersect`.
    let s1x = o1.0 - o0.0;
    let s1y = o1.1 - o0.1;
    let s2x = e1.0 - e0.0;
    let s2y = e1.1 - e0.1;
    let t = (s2x * (o0.1 - e0.1) - s2y * (o0.0 - e0.0)) / (-s2x * s1y + s1x * s2y);
    let inter = (o0.0 + (t * s1x), o0.1 + (t * s1y));
    let same = |a: (f64, f64)| (inter.0 - a.0).abs() < VEC2D_RESOLUTION && (inter.1 - a.1).abs() < VEC2D_RESOLUTION;
    if same(o0) || same(o1) { None } else { Some(inter) }
}

/// `H3Index.h3ToFaceIjk`.
pub(super) fn h3_to_face_ijk(mut h: u64) -> FaceIjk {
    let bc = get_base_cell(h);
    if is_base_cell_pentagon(bc) && leading_non_zero_digit(h) == 5 {
        h = rotate60cw(h);
    }
    let data = BASE_CELL_DATA[bc as usize];
    let mut fijk = FaceIjk { face: data.0 as usize, coord: Ijk::new(data.1[0], data.1[1], data.1[2]) };
    // `h3ToFaceIjkWithInitializedFijk`.
    let res = get_resolution(h);
    let possible_overage = is_base_cell_pentagon(bc) || (res != 0 && fijk.coord != Ijk::new(0, 0, 0));
    for r in 1..=res {
        if is_class_iii(r) {
            fijk.coord.down_ap7();
        } else {
            fijk.coord.down_ap7r();
        }
        fijk.coord.neighbor(get_digit(h, r));
    }
    if !possible_overage {
        return fijk;
    }
    let orig = fijk.coord;
    let mut res = res;
    if is_class_iii(res) {
        fijk.coord.down_ap7r();
        res += 1;
    }
    let pent_leading4 = is_base_cell_pentagon(bc) && leading_non_zero_digit(h) == 4;
    if fijk.adjust_overage_class_ii(res, pent_leading4, false) != Overage::None {
        if is_base_cell_pentagon(bc) {
            while fijk.adjust_overage_class_ii(res, false, false) != Overage::None {}
        }
        if res != get_resolution(h) {
            fijk.coord.up_ap7r();
        }
    } else if res != get_resolution(h) {
        fijk.coord = orig;
    }
    fijk
}

/// `H3.h3ToGeoBoundary`.
pub(super) fn h3_to_geo_boundary(h: u64) -> CellBoundary {
    h3_to_face_ijk(h).to_cell_boundary(get_resolution(h), is_pentagon(h))
}

/// `H3.h3ToLatLng`: the cell's center.
pub(super) fn h3_to_lat_lng(h: u64) -> LatLng {
    let f = h3_to_face_ijk(h);
    f.coord.to_geo(f.face, get_resolution(h), false)
}
