//! Geo commands (`geo.c`, `geohash.c`, `geohash_helper.c`): points are stored in a sorted set
//! with a 52-bit interleaved geohash as the score.
//!
//! The arithmetic is ported operation for operation so coordinates and distances come out
//! bit-identical. The reference build fuses multiply-adds (`-ffp-contract=fast`), so the places
//! where it does use `mul_add` here too, and the trigonometry goes through `libm` (a port of the
//! reference's musl libm) rather than the platform's.

use super::zsets::{limits, refsort, store_result, zadd_generic, zfloat, zset_from_pairs, zset_ref};
use super::*;
use crate::cmd;
use crate::notify;
use crate::types::ZSet;
use crate::value::Str;
use std::cmp::Ordering;

pub static COMMANDS: &[Command] = &[
    cmd!("geoadd", -5, WRITE | DENYOOM, 1, 1, 1, geoadd, Geo),
    cmd!("geodist", -4, READONLY, 1, 1, 1, geodist, Geo),
    cmd!("geohash", -2, READONLY, 1, 1, 1, geohash, Geo),
    cmd!("geopos", -2, READONLY, 1, 1, 1, geopos, Geo),
    cmd!("georadius", -6, WRITE | DENYOOM, 1, 1, 1, keys = georadius_keys, georadius, Geo),
    cmd!("georadius_ro", -6, READONLY, 1, 1, 1, georadius_ro, Geo),
    cmd!("georadiusbymember", -5, WRITE | DENYOOM, 1, 1, 1, keys = georadius_keys, georadiusbymember, Geo),
    cmd!("georadiusbymember_ro", -5, READONLY, 1, 1, 1, georadiusbymember_ro, Geo),
    cmd!("geosearch", -7, READONLY, 1, 1, 1, geosearch, Geo),
    cmd!("geosearchstore", -8, WRITE | DENYOOM, 1, 2, 1, geosearchstore, Geo),
];

/// The source key plus the last `STORE`/`STOREDIST` target (`georadiusGetKeys`).
fn georadius_keys(args: &[Bytes]) -> Vec<usize> {
    let mut stored = None;
    let mut i = 5;
    while i < args.len() {
        if (eq(&args[i], "store") || eq(&args[i], "storedist")) && i + 1 < args.len() {
            stored = Some(i + 1);
            i += 1;
        }
        i += 1;
    }
    let mut v = vec![1];
    v.extend(stored);
    v
}

// ---- geohash ----

const STEP_MAX: u8 = 26;
const LAT_MIN: f64 = -85.05112878;
const LAT_MAX: f64 = 85.05112878;
const LONG_MIN: f64 = -180.0;
const LONG_MAX: f64 = 180.0;
const D_R: f64 = std::f64::consts::PI / 180.0;
const EARTH_RADIUS_IN_METERS: f64 = 6372797.560856;
const MERCATOR_MAX: f64 = 20037726.37;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Bits {
    bits: u64,
    step: u8,
}

impl Bits {
    fn is_zero(self) -> bool {
        self.bits == 0 && self.step == 0
    }
}

#[derive(Clone, Copy)]
struct Range {
    min: f64,
    max: f64,
}

const LONG_RANGE: Range = Range { min: LONG_MIN, max: LONG_MAX };
const LAT_RANGE: Range = Range { min: LAT_MIN, max: LAT_MAX };

#[derive(Clone, Copy, Default)]
struct Area {
    lon_min: f64,
    lon_max: f64,
    lat_min: f64,
    lat_max: f64,
}

#[derive(Clone, Copy, Default)]
struct Neighbors {
    north: Bits,
    east: Bits,
    west: Bits,
    south: Bits,
    north_east: Bits,
    south_east: Bits,
    north_west: Bits,
    south_west: Bits,
}

fn interleave64(xlo: u32, ylo: u32) -> u64 {
    const B: [u64; 5] = [0x5555555555555555, 0x3333333333333333, 0x0F0F0F0F0F0F0F0F, 0x00FF00FF00FF00FF, 0x0000FFFF0000FFFF];
    const S: [u32; 5] = [1, 2, 4, 8, 16];
    let mut x = xlo as u64;
    let mut y = ylo as u64;
    for i in (0..5).rev() {
        x = (x | (x << S[i])) & B[i];
        y = (y | (y << S[i])) & B[i];
    }
    x | (y << 1)
}

fn deinterleave64(interleaved: u64) -> u64 {
    const B: [u64; 6] = [
        0x5555555555555555,
        0x3333333333333333,
        0x0F0F0F0F0F0F0F0F,
        0x00FF00FF00FF00FF,
        0x0000FFFF0000FFFF,
        0x00000000FFFFFFFF,
    ];
    const S: [u32; 6] = [0, 1, 2, 4, 8, 16];
    let mut x = interleaved;
    let mut y = interleaved >> 1;
    for i in 0..6 {
        x = (x | (x >> S[i])) & B[i];
        y = (y | (y >> S[i])) & B[i];
    }
    x | (y << 32)
}

/// `geohashEncode`; `None` outside the ranges.
fn encode(lon_r: Range, lat_r: Range, lon: f64, lat: f64, step: u8) -> Option<Bits> {
    if !(LONG_MIN..=LONG_MAX).contains(&lon) || !(LAT_MIN..=LAT_MAX).contains(&lat) {
        return None;
    }
    if lat < lat_r.min || lat > lat_r.max || lon < lon_r.min || lon > lon_r.max {
        return None;
    }
    let mut lat_offset = (lat - lat_r.min) / (lat_r.max - lat_r.min);
    let mut long_offset = (lon - lon_r.min) / (lon_r.max - lon_r.min);
    lat_offset *= (1u64 << step) as f64;
    long_offset *= (1u64 << step) as f64;
    Some(Bits { bits: interleave64(lat_offset as u32, long_offset as u32), step })
}

fn encode_wgs84(lon: f64, lat: f64, step: u8) -> Option<Bits> {
    encode(LONG_RANGE, LAT_RANGE, lon, lat, step)
}

/// `geohashDecode` (the reference fuses `min + x * scale`).
fn decode(lon_r: Range, lat_r: Range, hash: Bits) -> Option<Area> {
    if hash.is_zero() {
        return None;
    }
    let sep = deinterleave64(hash.bits);
    let lat_scale = lat_r.max - lat_r.min;
    let long_scale = lon_r.max - lon_r.min;
    let ilato = sep as u32;
    let ilono = (sep >> 32) as u32;
    let d = (1u64 << hash.step) as f64;
    let f = |min: f64, i: u32, scale: f64| (i as f64 * 1.0 / d).mul_add(scale, min);
    Some(Area {
        lat_min: f(lat_r.min, ilato, lat_scale),
        lat_max: f(lat_r.min, ilato.wrapping_add(1), lat_scale),
        lon_min: f(lon_r.min, ilono, long_scale),
        lon_max: f(lon_r.min, ilono.wrapping_add(1), long_scale),
    })
}

/// `decodeGeohash`: a score to its cell's centre `[lon, lat]`.
fn decode_score(score: f64) -> [f64; 2] {
    let area = decode(LONG_RANGE, LAT_RANGE, Bits { bits: score as u64, step: STEP_MAX }).expect("step is non-zero");
    let x = ((area.lon_min + area.lon_max) / 2.0).clamp(LONG_MIN, LONG_MAX);
    let y = ((area.lat_min + area.lat_max) / 2.0).clamp(LAT_MIN, LAT_MAX);
    [x, y]
}

fn move_x(h: &mut Bits, d: i8) {
    if d == 0 {
        return;
    }
    let mut x = h.bits & 0xaaaaaaaaaaaaaaaa;
    let y = h.bits & 0x5555555555555555;
    let zz = 0x5555555555555555u64 >> (64 - h.step as u32 * 2);
    if d > 0 {
        x = x.wrapping_add(zz + 1);
    } else {
        x |= zz;
        x = x.wrapping_sub(zz + 1);
    }
    x &= 0xaaaaaaaaaaaaaaaau64 >> (64 - h.step as u32 * 2);
    h.bits = x | y;
}

fn move_y(h: &mut Bits, d: i8) {
    if d == 0 {
        return;
    }
    let x = h.bits & 0xaaaaaaaaaaaaaaaa;
    let mut y = h.bits & 0x5555555555555555;
    let zz = 0xaaaaaaaaaaaaaaaau64 >> (64 - h.step as u32 * 2);
    if d > 0 {
        y = y.wrapping_add(zz + 1);
    } else {
        y |= zz;
        y = y.wrapping_sub(zz + 1);
    }
    y &= 0x5555555555555555u64 >> (64 - h.step as u32 * 2);
    h.bits = x | y;
}

fn neighbors(h: Bits) -> Neighbors {
    let mv = |dx: i8, dy: i8| {
        let mut b = h;
        move_x(&mut b, dx);
        move_y(&mut b, dy);
        b
    };
    Neighbors {
        east: mv(1, 0),
        west: mv(-1, 0),
        south: mv(0, -1),
        north: mv(0, 1),
        north_west: mv(-1, 1),
        north_east: mv(1, 1),
        south_east: mv(1, -1),
        south_west: mv(-1, -1),
    }
}

fn align52(h: Bits) -> u64 {
    h.bits << (52 - h.step as u32 * 2)
}

fn estimate_steps(mut range_meters: f64, lat: f64) -> u8 {
    if range_meters == 0.0 {
        return 26;
    }
    let mut step = 1i32;
    while range_meters < MERCATOR_MAX {
        range_meters *= 2.0;
        step += 1;
    }
    step -= 2;
    if !(-66.0..=66.0).contains(&lat) {
        step -= 1;
        if !(-80.0..=80.0).contains(&lat) {
            step -= 1;
        }
    }
    step.clamp(1, 26) as u8
}

fn deg_rad(a: f64) -> f64 {
    a * D_R
}

fn rad_deg(a: f64) -> f64 {
    a / D_R
}

/// `deg_rad(a) - deg_rad(b)` as the reference computes it: the first product is fused into the
/// subtraction (so the difference of equal angles is the rounding error of `b * D_R`, not 0).
fn rad_diff(a: f64, b: f64) -> f64 {
    a.mul_add(D_R, -deg_rad(b))
}

fn lat_distance(lat1d: f64, lat2d: f64) -> f64 {
    EARTH_RADIUS_IN_METERS * rad_diff(lat2d, lat1d).abs()
}

/// Haversine (`geohashGetDistance`).
fn distance(lon1d: f64, lat1d: f64, lon2d: f64, lat2d: f64) -> f64 {
    // `lon1r` is computed first and only feeds the subtraction, so it is the fused product.
    let v = libm::sin((-lon1d).mul_add(D_R, deg_rad(lon2d)) / 2.0);
    if v == 0.0 {
        return lat_distance(lat1d, lat2d);
    }
    let lat1r = deg_rad(lat1d);
    let lat2r = deg_rad(lat2d);
    let u = libm::sin((lat2r - lat1r) / 2.0);
    let a = u.mul_add(u, libm::cos(lat1r) * libm::cos(lat2r) * v * v);
    2.0 * EARTH_RADIUS_IN_METERS * libm::asin(a.sqrt())
}

// ---- shapes ----

#[derive(Clone, Copy)]
enum Kind {
    Circle { radius: f64 },
    Rect { width: f64, height: f64 },
}

#[derive(Clone, Copy)]
struct Shape {
    xy: [f64; 2],
    conversion: f64,
    kind: Kind,
}

impl Shape {
    /// `geohashBoundingBox`: `[min_lon, min_lat, max_lon, max_lat]`.
    fn bounds(&self) -> [f64; 4] {
        let [lon, lat] = self.xy;
        let (height, width) = match self.kind {
            Kind::Circle { radius } => (self.conversion * radius, self.conversion * radius),
            Kind::Rect { width, height } => (self.conversion * (height / 2.0), self.conversion * (width / 2.0)),
        };
        let lat_delta = rad_deg(height / EARTH_RADIUS_IN_METERS);
        let long_delta_top = rad_deg(width / EARTH_RADIUS_IN_METERS / libm::cos(deg_rad(lat + lat_delta)));
        let long_delta_bottom = rad_deg(width / EARTH_RADIUS_IN_METERS / libm::cos(deg_rad(lat - lat_delta)));
        let south = lat < 0.0;
        [
            if south { lon - long_delta_bottom } else { lon - long_delta_top },
            lat - lat_delta,
            if south { lon + long_delta_bottom } else { lon + long_delta_top },
            lat + lat_delta,
        ]
    }

    /// `geoWithinShape`: the point's coordinates and distance when inside.
    fn contains(&self, score: f64) -> Option<([f64; 2], f64)> {
        let xy = decode_score(score);
        let [x1, y1] = self.xy;
        let [x2, y2] = xy;
        match self.kind {
            Kind::Circle { radius } => {
                let d = distance(x1, y1, x2, y2);
                (d <= radius * self.conversion).then_some((xy, d))
            }
            Kind::Rect { width, height } => {
                let (w, h) = (width * self.conversion, height * self.conversion);
                if lat_distance(y2, y1) > h / 2.0 {
                    return None;
                }
                if distance(x2, y2, x1, y2) > w / 2.0 {
                    return None;
                }
                Some((xy, distance(x1, y1, x2, y2)))
            }
        }
    }

    /// `geohashCalculateAreasByShapeWGS84`: the centre box and its useful neighbours, in the
    /// order they're searched.
    fn areas(&self) -> [Bits; 9] {
        let [min_lon, min_lat, max_lon, max_lat] = self.bounds();
        let [lon, lat] = self.xy;
        let radius_meters = match self.kind {
            Kind::Circle { radius } => radius,
            Kind::Rect { width, height } => (width / 2.0).mul_add(width / 2.0, (height / 2.0) * (height / 2.0)).sqrt(),
        } * self.conversion;
        let mut steps = estimate_steps(radius_meters, lat);
        let mut hash = encode_wgs84(lon, lat, steps).unwrap_or_default();
        let mut n = neighbors(hash);
        let mut area = decode(LONG_RANGE, LAT_RANGE, hash).unwrap_or_default();

        let dec = |b: Bits| decode(LONG_RANGE, LAT_RANGE, b).unwrap_or_default();
        let decrease = dec(n.north).lat_max < max_lat
            || dec(n.south).lat_min > min_lat
            || dec(n.east).lon_max < max_lon
            || dec(n.west).lon_min > min_lon;
        if steps > 1 && decrease {
            steps -= 1;
            hash = encode_wgs84(lon, lat, steps).unwrap_or_default();
            n = neighbors(hash);
            area = decode(LONG_RANGE, LAT_RANGE, hash).unwrap_or_default();
        }
        if steps >= 2 {
            let z = Bits::default();
            if area.lat_min < min_lat {
                (n.south, n.south_west, n.south_east) = (z, z, z);
            }
            if area.lat_max > max_lat {
                (n.north, n.north_east, n.north_west) = (z, z, z);
            }
            if area.lon_min < min_lon {
                (n.west, n.south_west, n.north_west) = (z, z, z);
            }
            if area.lon_max > max_lon {
                (n.east, n.south_east, n.north_east) = (z, z, z);
            }
        }
        [hash, n.north, n.south, n.east, n.west, n.north_east, n.north_west, n.south_east, n.south_west]
    }
}

struct Point {
    xy: [f64; 2],
    dist: f64,
    score: f64,
    member: Str,
}

/// `membersOfAllNeighbors`: candidates in box order. `limit` 0 = unlimited (ANY stops early).
fn search(z: &ZSet, shape: &Shape, limit: usize) -> Vec<Point> {
    let boxes = shape.areas();
    let mut out = Vec::new();
    let mut last_processed = 0usize;
    for (i, &b) in boxes.iter().enumerate() {
        if b.is_zero() {
            continue;
        }
        if last_processed != 0 && b == boxes[last_processed] {
            continue;
        }
        if !out.is_empty() && limit != 0 && out.len() >= limit {
            break;
        }
        // Scores in [align52(b), align52(b + 1)).
        let min = align52(b) as f64;
        let max = align52(Bits { bits: b.bits.wrapping_add(1), step: b.step }) as f64;
        if let Some(first) = z.first_in(true, |s, _| s >= min, |s, _| s < max) {
            for (m, s) in z.iter_from(first, false) {
                if s >= max {
                    break;
                }
                if let Some((xy, dist)) = shape.contains(s) {
                    out.push(Point { xy, dist, score: s, member: Str::from_slice(m) });
                }
                if !out.is_empty() && limit != 0 && out.len() >= limit {
                    break;
                }
            }
        }
        last_processed = i;
    }
    out
}

// ---- argument helpers ----

/// `extractLongLatOrReply`.
fn lon_lat(a: &[u8], b: &[u8]) -> Result<[f64; 2], Error> {
    let x = zfloat(a).ok_or(NOT_FLOAT)?;
    let y = zfloat(b).ok_or(NOT_FLOAT)?;
    if !(LONG_MIN..=LONG_MAX).contains(&x) || !(LAT_MIN..=LAT_MAX).contains(&y) {
        return Err(Error::msg(format!("ERR invalid longitude,latitude pair {x:.6},{y:.6}")));
    }
    Ok([x, y])
}

fn unit(a: &[u8]) -> Result<f64, Error> {
    Ok(if eq(a, "m") {
        1.0
    } else if eq(a, "km") {
        1000.0
    } else if eq(a, "ft") {
        0.3048
    } else if eq(a, "mi") {
        1609.34
    } else {
        return Err(Error::new("ERR unsupported unit provided. please use M, KM, FT, MI"));
    })
}

/// `extractDistanceOrReply`: (radius, conversion).
fn radius_arg(r: &[u8], u: &[u8]) -> Result<(f64, f64), Error> {
    let d = zfloat(r).ok_or(Error::new("ERR need numeric radius"))?;
    if d < 0.0 {
        return Err(Error::new("ERR radius cannot be negative"));
    }
    Ok((d, unit(u)?))
}

/// `extractBoxOrReply`: (width, height, conversion).
fn box_arg(w: &[u8], h: &[u8], u: &[u8]) -> Result<(f64, f64, f64), Error> {
    let w = zfloat(w).ok_or(Error::new("ERR need numeric width"))?;
    let h = zfloat(h).ok_or(Error::new("ERR need numeric height"))?;
    if h < 0.0 || w < 0.0 {
        return Err(Error::new("ERR height or width cannot be negative"));
    }
    Ok((w, h, unit(u)?))
}

/// `fixedpoint_d2string(d, 4)`: `llrint(d * 10000)` printed with four decimals.
fn fmt_distance(d: f64) -> String {
    if d == 0.0 || !d.is_finite() {
        return "0.0000".into();
    }
    let v = (d * 10000.0).round_ties_even();
    if v.abs() >= 9.2e18 {
        // Out of `long long` range: llrint's result is unspecified; mimic the common saturation.
        return format!("{:.4}", d);
    }
    let v = v as i64;
    let neg = v < 0;
    let a = v.unsigned_abs();
    format!("{}{}.{:04}", if neg { "-" } else { "" }, a / 10000, a % 10000)
}

// ---- commands ----

/// GEOADD key [NX|XX] [CH] longitude latitude member [...]: rewritten as ZADD.
pub fn geoadd(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (mut xx, mut nx) = (false, false);
    let mut longidx = 2;
    while longidx < args.len() {
        let a = &args[longidx];
        if eq(a, "nx") {
            nx = true;
        } else if eq(a, "xx") {
            xx = true;
        } else if !eq(a, "ch") {
            break;
        }
        longidx += 1;
    }
    if !(args.len() - longidx).is_multiple_of(3) || (xx && nx) {
        return Err(SYNTAX);
    }
    let elements = (args.len() - longidx) / 3;
    let mut zargs: Vec<Bytes> = Vec::with_capacity(longidx + elements * 2);
    zargs.push(Bytes::from_static(b"zadd"));
    zargs.extend(args[1..longidx].iter().cloned());
    for i in 0..elements {
        let base = longidx + i * 3;
        let [x, y] = lon_lat(&args[base], &args[base + 1])?;
        let bits = align52(encode_wgs84(x, y, STEP_MAX).unwrap_or_default());
        zargs.push(Bytes::from(bits.to_string()));
        zargs.push(args[base + 2].clone());
    }
    let r = zadd_generic(cx, &zargs, false)?;
    cx.propagate_as(zargs);
    Ok(r)
}

/// GEODIST key member1 member2 [unit]
pub fn geodist(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let to_meter = match args.len() {
        5 => unit(&args[4])?,
        4 => 1.0,
        _ => return Err(SYNTAX),
    };
    let Some(z) = cx.get_zset(&args[1])? else { return Ok(Reply::Nil) };
    let (Some(s1), Some(s2)) = (z.score(&args[2]), z.score(&args[3])) else { return Ok(Reply::Nil) };
    let [x1, y1] = decode_score(s1);
    let [x2, y2] = decode_score(s2);
    Ok(Reply::bulk_str(fmt_distance(distance(x1, y1, x2, y2) / to_meter)))
}

/// GEOHASH key [member ...]: standard 11-character geohashes (re-encoded over ±90 latitude).
pub fn geohash(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    const ALPHABET: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";
    let z = cx.get_zset(&args[1])?;
    Ok(Reply::Array(
        args[2..]
            .iter()
            .map(|m| match z.and_then(|z| z.score(m)) {
                None => Reply::Nil,
                Some(score) => {
                    let [x, y] = decode_score(score);
                    let h = encode(Range { min: -180.0, max: 180.0 }, Range { min: -90.0, max: 90.0 }, x, y, 26)
                        .unwrap_or_default();
                    let buf: Vec<u8> = (0..11)
                        .map(|i| {
                            let idx = if i == 10 { 0 } else { (h.bits >> (52 - (i + 1) * 5)) & 0x1f };
                            ALPHABET[idx as usize]
                        })
                        .collect();
                    Reply::bulk(buf)
                }
            })
            .collect(),
    ))
}

/// GEOPOS key [member ...]
pub fn geopos(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let z = cx.get_zset(&args[1])?;
    Ok(Reply::Array(
        args[2..]
            .iter()
            .map(|m| match z.and_then(|z| z.score(m)) {
                None => Reply::NilArray,
                Some(score) => {
                    let [x, y] = decode_score(score);
                    Reply::Array(vec![Reply::Double(x), Reply::Double(y)])
                }
            })
            .collect(),
    ))
}

const RADIUS_COORDS: u8 = 1 << 0;
const RADIUS_MEMBER: u8 = 1 << 1;
const RADIUS_NOSTORE: u8 = 1 << 2;
const GEOSEARCH: u8 = 1 << 3;
const GEOSEARCHSTORE: u8 = 1 << 4;

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    None,
    Asc,
    Desc,
}

/// `georadiusGeneric`.
fn georadius_generic(cx: &mut Cx, args: &[Bytes], src_idx: usize, flags: u8) -> CmdResult {
    let src = &args[src_idx];
    let exists = cx.get_zset(src)?.is_some();
    let mut storekey: Option<&Bytes> = None;
    let mut storedist = false;
    let mut xy = [0.0; 2];
    let mut conversion = 0.0;
    let mut kind = Kind::Circle { radius: 0.0 };
    let base_args;
    if flags & RADIUS_COORDS != 0 {
        base_args = 6;
        xy = lon_lat(&args[2], &args[3])?;
        let (r, c) = radius_arg(&args[4], &args[5])?;
        (kind, conversion) = (Kind::Circle { radius: r }, c);
    } else if flags & RADIUS_MEMBER != 0 && !exists {
        base_args = 5;
    } else if flags & RADIUS_MEMBER != 0 {
        base_args = 5;
        let score = zset_ref(cx, src).and_then(|z| z.score(&args[2]));
        xy = decode_score(score.ok_or(Error::new("ERR could not decode requested zset member"))?);
        let (r, c) = radius_arg(&args[3], &args[4])?;
        (kind, conversion) = (Kind::Circle { radius: r }, c);
    } else {
        base_args = if flags & GEOSEARCHSTORE != 0 { 3 } else { 2 };
        if flags & GEOSEARCHSTORE != 0 {
            storekey = Some(&args[1]);
        }
    }

    let (mut withdist, mut withhash, mut withcoords) = (false, false, false);
    let (mut frommember, mut fromloc, mut byradius, mut bybox) = (false, false, false, false);
    let mut sort = Sort::None;
    let mut any = false;
    let mut count = 0i64;
    let search_cmd = flags & GEOSEARCH != 0;
    let can_store = flags & RADIUS_NOSTORE == 0 && !search_cmd;
    if args.len() > base_args {
        let remaining = args.len() - base_args;
        let mut i = 0;
        while i < remaining {
            let arg = &args[base_args + i];
            let at = |k: usize| &args[base_args + i + k];
            if eq(arg, "withdist") {
                withdist = true;
            } else if eq(arg, "withhash") {
                withhash = true;
            } else if eq(arg, "withcoord") {
                withcoords = true;
            } else if eq(arg, "any") {
                any = true;
            } else if eq(arg, "asc") {
                sort = Sort::Asc;
            } else if eq(arg, "desc") {
                sort = Sort::Desc;
            } else if eq(arg, "count") && i + 1 < remaining {
                count = int(at(1))?;
                if count <= 0 {
                    return Err(Error::new("ERR COUNT must be > 0"));
                }
                i += 1;
            } else if eq(arg, "store") && i + 1 < remaining && can_store {
                storekey = Some(at(1));
                storedist = false;
                i += 1;
            } else if eq(arg, "storedist") && i + 1 < remaining && can_store {
                storekey = Some(at(1));
                storedist = true;
                i += 1;
            } else if eq(arg, "storedist") && search_cmd && flags & GEOSEARCHSTORE != 0 {
                storedist = true;
            } else if eq(arg, "frommember") && i + 1 < remaining && search_cmd && !fromloc {
                if exists {
                    let score = zset_ref(cx, src).and_then(|z| z.score(at(1)));
                    xy = decode_score(score.ok_or(Error::new("ERR could not decode requested zset member"))?);
                }
                frommember = true;
                i += 1;
            } else if eq(arg, "fromlonlat") && i + 2 < remaining && search_cmd && !frommember {
                xy = lon_lat(at(1), at(2))?;
                fromloc = true;
                i += 2;
            } else if eq(arg, "byradius") && i + 2 < remaining && search_cmd && !bybox {
                let (r, c) = radius_arg(at(1), at(2))?;
                (kind, conversion) = (Kind::Circle { radius: r }, c);
                byradius = true;
                i += 2;
            } else if eq(arg, "bybox") && i + 3 < remaining && search_cmd && !byradius {
                let (w, h, c) = box_arg(at(1), at(2), at(3))?;
                (kind, conversion) = (Kind::Rect { width: w, height: h }, c);
                bybox = true;
                i += 3;
            } else {
                return Err(SYNTAX);
            }
            i += 1;
        }
    }

    if storekey.is_some() && (withdist || withhash || withcoords) {
        return Err(Error::msg(format!(
            "ERR {} is not compatible with WITHDIST, WITHHASH and WITHCOORD options",
            if flags & GEOSEARCHSTORE != 0 { "GEOSEARCHSTORE" } else { "STORE option in GEORADIUS" }
        )));
    }
    let name = || String::from_utf8_lossy(&args[0]).into_owned();
    if search_cmd && !(frommember || fromloc) {
        return Err(Error::msg(format!("ERR exactly one of FROMMEMBER or FROMLONLAT can be specified for {}", name())));
    }
    if search_cmd && !(byradius || bybox) {
        return Err(Error::msg(format!("ERR exactly one of BYRADIUS and BYBOX can be specified for {}", name())));
    }
    if any && count == 0 {
        return Err(Error::new("ERR the ANY argument requires COUNT argument"));
    }

    let Some(z) = zset_ref(cx, src) else {
        return Ok(match storekey {
            Some(dst) => {
                if cx.delete(dst) {
                    cx.notify(notify::GENERIC, "del", dst);
                }
                Reply::Int(0)
            }
            None => Reply::Array(vec![]),
        });
    };
    if count != 0 && sort == Sort::None && !any {
        sort = Sort::Asc;
    }
    let shape = Shape { xy, conversion, kind };
    let mut points = search(z, &shape, if any { count as usize } else { 0 });
    if points.is_empty() && storekey.is_none() {
        return Ok(Reply::Array(vec![]));
    }
    let returned = if count == 0 { points.len() } else { points.len().min(count as usize) };
    if sort != Sort::None {
        // `sort_gp_asc` / `sort_gp_desc`, with the same (unstable) sorts as the reference.
        let asc = |a: &Point, b: &Point| {
            if a.dist > b.dist {
                Ordering::Greater
            } else if a.dist == b.dist {
                Ordering::Equal
            } else {
                Ordering::Less
            }
        };
        let cmp = |a: &Point, b: &Point| if sort == Sort::Asc { asc(a, b) } else { asc(a, b).reverse() };
        if returned == points.len() {
            refsort::qsort(&mut points, cmp);
        } else {
            refsort::pqsort(&mut points, cmp, 0, returned - 1);
        }
    }
    points.truncate(returned);
    for p in &mut points {
        p.dist /= shape.conversion;
    }

    match storekey {
        None => {
            let options = withdist as usize + withcoords as usize + withhash as usize;
            let out = points
                .into_iter()
                .map(|p| {
                    if options == 0 {
                        return Reply::bulk(&p.member);
                    }
                    let mut v = Vec::with_capacity(options + 1);
                    v.push(Reply::bulk(&p.member));
                    if withdist {
                        v.push(Reply::bulk_str(fmt_distance(p.dist)));
                    }
                    if withhash {
                        v.push(Reply::Int(p.score as i64));
                    }
                    if withcoords {
                        v.push(Reply::Array(vec![Reply::Double(p.xy[0]), Reply::Double(p.xy[1])]));
                    }
                    Reply::Array(v)
                })
                .collect();
            Ok(Reply::Array(out))
        }
        Some(dst) => {
            let lim = limits(cx);
            let mut items: Vec<(Str, f64)> =
                points.into_iter().map(|p| (p.member, if storedist { p.dist } else { p.score })).collect();
            items.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
            items.dedup_by(|a, b| a.0 == b.0);
            let z = zset_from_pairs(&items, lim);
            let event = if search_cmd { "geosearchstore" } else { "georadiusstore" };
            Ok(Reply::Int(store_result(cx, dst, z, event)))
        }
    }
}

pub fn georadius(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    georadius_generic(cx, args, 1, RADIUS_COORDS)
}
pub fn georadius_ro(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    georadius_generic(cx, args, 1, RADIUS_COORDS | RADIUS_NOSTORE)
}
pub fn georadiusbymember(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    georadius_generic(cx, args, 1, RADIUS_MEMBER)
}
pub fn georadiusbymember_ro(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    georadius_generic(cx, args, 1, RADIUS_MEMBER | RADIUS_NOSTORE)
}
pub fn geosearch(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    georadius_generic(cx, args, 1, GEOSEARCH)
}
pub fn geosearchstore(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    georadius_generic(cx, args, 2, GEOSEARCH | GEOSEARCHSTORE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_matches_reference() {
        // Values observed on Redis 8.
        assert_eq!(redis_proto::fmt_double(decode_score(2163557714838307.0)[1]), "51.50720053692975");
        assert_eq!(redis_proto::fmt_double(decode_score(4503599627286694.0)[1]), "85.04999956172352");
        let [x, y] = decode_score(3479099956230698.0);
        let [x2, y2] = decode_score(3479447370796909.0);
        assert_eq!(fmt_distance(distance(x, y, x2, y2)), "166274.1516");
    }

    #[test]
    fn encode_roundtrip() {
        let h = encode_wgs84(13.361389, 38.115556, STEP_MAX).unwrap();
        assert_eq!(align52(h), 3479099956230698);
        assert_eq!(deinterleave64(interleave64(12345, 67890)), 12345 | (67890u64 << 32));
    }

    #[test]
    fn distances_format() {
        assert_eq!(fmt_distance(0.0), "0.0000");
        assert_eq!(fmt_distance(1.23456), "1.2346");
        assert_eq!(fmt_distance(0.00005), "0.0000");
        assert_eq!(fmt_distance(0.00015), "0.0001"); // 1.4999… after scaling
        assert_eq!(fmt_distance(123456.5 / 10000.0 * 10000.0 / 10000.0), "12.3456");
    }
}
