//! The module's geo hash (its `geo` crate): 52-bit cell scores for GEO fields, cell centres,
//! haversine distances and the nine score ranges a radius query reads (`calc_ranges`).

pub const LAT_MIN: f64 = -85.05112878;
pub const LAT_MAX: f64 = 85.05112878;
pub const LONG_MIN: f64 = -180.0;
pub const LONG_MAX: f64 = 180.0;
const STEP_MAX: u8 = 26;
const MERCATOR_MAX: f64 = 20037726.37;
const EARTH_RADIUS_IN_METERS: f64 = 6372797.560856;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bits {
    pub bits: u64,
    pub step: u8,
}

#[derive(Clone, Copy, Debug)]
struct Area {
    lat_min: f64,
    lat_max: f64,
    lon_min: f64,
    lon_max: f64,
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

/// `deinterleave64`: (latitude cell, longitude cell).
fn deinterleave64(interleaved: u64) -> (u32, u32) {
    const B: [u64; 6] = [0x5555555555555555, 0x3333333333333333, 0x0F0F0F0F0F0F0F0F, 0x00FF00FF00FF00FF, 0x0000FFFF0000FFFF, 0x00000000FFFFFFFF];
    const S: [u32; 6] = [0, 1, 2, 4, 8, 16];
    let mut x = interleaved;
    let mut y = interleaved >> 1;
    for i in 0..6 {
        x = (x | (x >> S[i])) & B[i];
        y = (y | (y >> S[i])) & B[i];
    }
    (x as u32, y as u32)
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

/// `WGS84Coordinates::new`.
pub fn valid(lon: f64, lat: f64) -> bool {
    (LONG_MIN..=LONG_MAX).contains(&lon) && (LAT_MIN..=LAT_MAX).contains(&lat)
}

/// `encode_wgs84`: cells clamp at the top edge.
fn encode(lon: f64, lat: f64, step: u8) -> Bits {
    let lon_offset = (lon - LONG_MIN) / (LONG_MAX - LONG_MIN);
    let lat_offset = (lat - LAT_MIN) / (LAT_MAX - LAT_MIN);
    let step_size = (1u64 << step) as f64;
    let max_fixed = step_size as u64 - 1;
    let lat_fixed = ((lat_offset * step_size) as u64).min(max_fixed) as u32;
    let lon_fixed = ((lon_offset * step_size) as u64).min(max_fixed) as u32;
    Bits { bits: interleave64(lat_fixed, lon_fixed), step }
}

/// `encodeGeo`: a point's 52-bit score (`None` outside the WGS-84 range).
pub fn encode_geo(lon: f64, lat: f64) -> Option<u64> {
    valid(lon, lat).then(|| align52(encode(lon, lat, STEP_MAX)))
}

/// `decodeGeo`: the centre of a 52-bit score's cell, `(lon, lat)`.
pub fn decode_geo(score: f64) -> (f64, f64) {
    let (ilat, ilon) = deinterleave64(score as u64);
    let d = (1u64 << STEP_MAX) as f64;
    let lon = (LONG_MIN + ((ilon as f64 + 0.5) / d) * (LONG_MAX - LONG_MIN)).clamp(LONG_MIN, LONG_MAX);
    let lat = (LAT_MIN + ((ilat as f64 + 0.5) / d) * (LAT_MAX - LAT_MIN)).clamp(LAT_MIN, LAT_MAX);
    (lon, lat)
}

fn cell_lat(h: Bits, add: f64) -> f64 {
    let (ilat, _) = deinterleave64(h.bits);
    LAT_MIN + ((ilat as f64 + add) / (1u64 << h.step) as f64) * (LAT_MAX - LAT_MIN)
}

fn cell_lon(h: Bits, add: f64) -> f64 {
    let (_, ilon) = deinterleave64(h.bits);
    LONG_MIN + ((ilon as f64 + add) / (1u64 << h.step) as f64) * (LONG_MAX - LONG_MIN)
}

fn decode(h: Bits) -> Area {
    Area { lat_min: cell_lat(h, 0.0), lat_max: cell_lat(h, 1.0), lon_min: cell_lon(h, 0.0), lon_max: cell_lon(h, 1.0) }
}

fn align52(h: Bits) -> u64 {
    h.bits << (52 - h.step as u32 * 2)
}

/// `haversine_distance`, in meters.
pub fn distance(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    let (lat1r, lon1r, lat2r, lon2r) = (lat1.to_radians(), lon1.to_radians(), lat2.to_radians(), lon2.to_radians());
    let u = ((lat2r - lat1r) / 2.0).sin();
    let v = ((lon2r - lon1r) / 2.0).sin();
    2.0 * EARTH_RADIUS_IN_METERS * (u * u + lat1r.cos() * lat2r.cos() * v * v).sqrt().min(1.0).asin()
}

fn meridian_distance(lat1: f64, lat2: f64) -> f64 {
    let u = ((lat2.to_radians() - lat1.to_radians()) / 2.0).sin();
    2.0 * EARTH_RADIUS_IN_METERS * u.abs().min(1.0).asin()
}

fn parallel_distance(lat: f64, lon1: f64, lon2: f64) -> f64 {
    let v = ((lon2.to_radians() - lon1.to_radians()) / 2.0).sin();
    2.0 * EARTH_RADIUS_IN_METERS * (lat.to_radians().cos() * v.abs()).min(1.0).asin()
}

fn estimate_steps(range_meters: f64, lat: f64) -> u8 {
    if range_meters.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return STEP_MAX;
    }
    let ratio = (MERCATOR_MAX / range_meters).ceil().min(u32::MAX as f64) as u64;
    let base = ratio.next_power_of_two().ilog2() as i32 - 1;
    let polar = match lat.abs() {
        a if a > 80.0 => 2,
        a if a > 66.0 => 1,
        _ => 0,
    };
    (base - polar).clamp(1, STEP_MAX as i32) as u8
}

/// `calc_ranges`: the `[min, max]` scores of the radius's cell and its kept neighbours (the
/// zero ranges stand for skipped cells).
pub fn calc_ranges(lon: f64, lat: f64, radius_meters: f64) -> [(u64, u64); 9] {
    let lat_rad = lat.to_radians();
    let lon_delta = (radius_meters / EARTH_RADIUS_IN_METERS / lat_rad.cos()).to_degrees();
    let lat_delta = (radius_meters / EARTH_RADIUS_IN_METERS).to_degrees();
    let (min_lon, min_lat, max_lon, max_lat) = (lon - lon_delta, lat - lat_delta, lon + lon_delta, lat + lat_delta);
    let mut steps = estimate_steps(radius_meters, lat);
    let nbrs = |h: Bits| {
        let mv = |base: Bits, dx: i8, dy: i8| {
            let mut b = base;
            move_y(&mut b, dy);
            move_x(&mut b, dx);
            b
        };
        // north, south, east, west, NE, NW, SE, SW
        [mv(h, 0, 1), mv(h, 0, -1), mv(h, 1, 0), mv(h, -1, 0), mv(h, 1, 1), mv(h, -1, 1), mv(h, 1, -1), mv(h, -1, -1)]
    };
    let mut hash = encode(lon, lat, steps);
    let mut n = nbrs(hash);
    let mut area = decode(hash);
    let decrease = meridian_distance(lat, cell_lat(n[0], 1.0)) < radius_meters
        || meridian_distance(lat, cell_lat(n[1], 0.0)) < radius_meters
        || parallel_distance(lat, lon, cell_lon(n[2], 1.0)) < radius_meters
        || parallel_distance(lat, lon, cell_lon(n[3], 0.0)) < radius_meters;
    if steps > 1 && decrease {
        steps -= 1;
        hash = encode(lon, lat, steps);
        n = nbrs(hash);
        area = decode(hash);
    }
    let mut cells: [Option<Bits>; 9] = [Some(hash), Some(n[0]), Some(n[1]), Some(n[2]), Some(n[3]), Some(n[4]), Some(n[5]), Some(n[6]), Some(n[7])];
    if steps >= 2 {
        let mut drop = |is: &[usize]| is.iter().for_each(|&i| cells[i] = None);
        if area.lat_min < min_lat {
            drop(&[2, 8, 7]);
        }
        if area.lat_max > max_lat {
            drop(&[1, 5, 6]);
        }
        if area.lon_min < min_lon {
            drop(&[4, 8, 6]);
        }
        if area.lon_max > max_lon {
            drop(&[3, 7, 5]);
        }
    }
    let mut ranges = [(0u64, 0u64); 9];
    let mut last = 0;
    for i in 0..9 {
        let Some(c) = cells[i] else { continue };
        if last != 0 && Some(c) == cells[last] {
            continue;
        }
        ranges[i] = (align52(c), align52(Bits { bits: c.bits.wrapping_add(1), step: c.step }));
        last = i;
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_round_trip() {
        let s = encode_geo(13.361389, 38.115556).unwrap();
        let (lon, lat) = decode_geo(s as f64);
        assert!((lon - 13.361389).abs() < 1e-5 && (lat - 38.115556).abs() < 1e-5);
        assert!(encode_geo(181.0, 0.0).is_none());
        // the top edge clamps into the last cell
        assert!(encode_geo(180.0, LAT_MAX).unwrap() < 1 << 52);
    }

    #[test]
    fn ranges_cover_the_centre() {
        let s = encode_geo(2.35, 48.85).unwrap();
        let r = calc_ranges(2.35, 48.85, 1000.0);
        assert!(r[0].0 <= s && s < r[0].1);
        assert!((distance(2.35, 48.85, 2.36, 48.85) - 732.0).abs() < 5.0);
    }
}
