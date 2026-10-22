//! Geo cell encodings shared by geo aggregations and formats: geohashes (Elasticsearch's
//! `Geohash` long format: interleaved lon/lat bits with the level in the low 4 bits) and map
//! tiles (`GeoTileUtils`: zoom in the high bits, then x and y).

use crate::error::{AggError, Result};

const BASE_32: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";
/// `Geohash.PRECISION`.
pub const GEOHASH_MAX_PRECISION: u32 = 12;
const MORTON_OFFSET: u32 = 64 - GEOHASH_MAX_PRECISION * 5;
const LAT_DECODE: f64 = 180.0 / (1u64 << 32) as f64;
const LON_DECODE: f64 = 360.0 / (1u64 << 32) as f64;

/// `BitUtil.interleave(even, odd)`: bits of `even` in even positions.
fn interleave(even: u32, odd: u32) -> u64 {
    fn spread(v: u32) -> u64 {
        let mut x = v as u64;
        x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
        x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
        x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
        x = (x | (x << 2)) & 0x3333_3333_3333_3333;
        x = (x | (x << 1)) & 0x5555_5555_5555_5555;
        x
    }
    spread(even) | (spread(odd) << 1)
}

/// `BitUtil.deinterleave`: the even bits.
fn deinterleave(b: u64) -> u64 {
    let mut b = b & 0x5555_5555_5555_5555;
    b = (b ^ (b >> 1)) & 0x3333_3333_3333_3333;
    b = (b ^ (b >> 2)) & 0x0F0F_0F0F_0F0F_0F0F;
    b = (b ^ (b >> 4)) & 0x00FF_00FF_00FF_00FF;
    b = (b ^ (b >> 8)) & 0x0000_FFFF_0000_FFFF;
    b = (b ^ (b >> 16)) & 0x0000_0000_FFFF_FFFF;
    b
}

pub(crate) fn encode_lat(lat: f64) -> i32 {
    let lat = if lat == 90.0 { f64::from_bits(90.0f64.to_bits() - 1) } else { lat };
    (lat / LAT_DECODE).floor() as i32
}

pub(crate) fn encode_lon(lon: f64) -> i32 {
    let lon = if lon == 180.0 { f64::from_bits(180.0f64.to_bits() - 1) } else { lon };
    (lon / LON_DECODE).floor() as i32
}

fn encode_lat_lon(lat: f64, lon: f64) -> u64 {
    let lat_enc = (encode_lat(lat) as u32) ^ 0x8000_0000;
    let lon_enc = (encode_lon(lon) as u32) ^ 0x8000_0000;
    interleave(lat_enc, lon_enc) >> 2
}

/// `Geohash.longEncode(lon, lat, level)`.
pub fn geohash_long(lon: f64, lat: f64, level: u32) -> i64 {
    let msf = (GEOHASH_MAX_PRECISION - level) * 5 + (MORTON_OFFSET - 2);
    (((encode_lat_lon(lat, lon) >> msf) << 4) | level as u64) as i64
}

/// `Geohash.stringEncode(long)`.
pub fn geohash_string(v: i64) -> String {
    let mut level = (v & 15) as usize;
    let mut h = (v as u64) >> 4;
    let mut chars = vec![b'0'; level];
    while level > 0 {
        level -= 1;
        chars[level] = BASE_32[(h & 31) as usize];
        h >>= 5;
    }
    String::from_utf8(chars).unwrap_or_default()
}

/// `Geohash.mortonEncode(hash)` then decoded: the cell's lower-left corner `(lat, lon)` as
/// Elasticsearch decodes a geohash point.
pub fn geohash_decode(hash: &str) -> Option<(f64, f64)> {
    if hash.is_empty() {
        return None;
    }
    let mut level: i32 = 11;
    let mut l: u64 = 0;
    for c in hash.bytes() {
        let b = BASE_32.iter().position(|x| *x == c)? as u64;
        l |= b << ((level as u32) * 5 + (MORTON_OFFSET - 2));
        level -= 1;
        if level < 0 {
            break;
        }
    }
    // BitUtil.flipFlop: swap even and odd bits.
    let flip = ((l & 0xAAAA_AAAA_AAAA_AAAA) >> 1) | ((l & 0x5555_5555_5555_5555) << 1);
    // GeoHashUtils.decodeLatitude/decodeLongitude: 31 bits per coordinate.
    let lon = deinterleave(flip) as f64 / ((1u64 << 31) as f64 / 360.0) - 180.0;
    let lat = deinterleave(flip >> 1) as f64 / ((1u64 << 31) as f64 / 180.0) - 90.0;
    Some((lat, lon))
}

/// `GeoTileUtils.getXTile`.
pub fn tile_x(lon: f64, tiles: i32) -> i32 {
    let x = (lon + 180.0) / 360.0 * tiles as f64;
    (x.floor() as i32).clamp(0, tiles - 1).max(0)
}

/// `GeoTileUtils.getYTile` (Lucene's `SloppyMath.cos`, Elasticsearch's `FastMath.log`).
pub fn tile_y(lat: f64, tiles: i32) -> i32 {
    let lat_sin = search_index::util::geo::sloppy_cos(std::f64::consts::PI / 2.0 - lat.to_radians());
    let y = (0.5 - (crate::jmath::fast_log((1.0 + lat_sin) / (1.0 - lat_sin)) / (std::f64::consts::PI * 4.0))) * tiles as f64;
    (y.floor() as i32).clamp(0, tiles - 1).max(0)
}

/// `GeoTileUtils.longEncode(lon, lat, precision)`.
pub fn geotile_of(lat: f64, lon: f64, precision: u32) -> i64 {
    let tiles = 1i32 << precision;
    geotile_long_tiles(precision, tile_x(lon, tiles) as u32, tile_y(lat, tiles) as u32)
}

/// `GeoTileUtils.tileToLon`.
pub fn tile_to_lon(x: i64, tiles: i64) -> f64 {
    (x as f64 / tiles as f64 * 360.0) - 180.0
}

/// `GeoTileUtils.tileToLat`: `ESSloppyMath`'s `atan(sinh(n))`; from 2^20 tiles on, snapped to
/// the last quantized latitude still in the tile (`findBoundaryPoint`).
pub fn tile_to_lat(y: i64, tiles: i64) -> f64 {
    let lat = tile_to_lat_raw(y as f64, tiles);
    if tiles < (1 << 20) || y == 0 || y == tiles {
        return lat;
    }
    let q = quantize_lat(lat);
    let computed = tile_y(q, tiles as i32) as i64;
    let step = if computed == y { LUCENE_LAT_RES } else { -LUCENE_LAT_RES };
    // findBoundaryPoint, iteratively.
    let (mut q, mut tile) = (q, computed);
    loop {
        let next = q + step;
        let next_tile = tile_y(next, tiles as i32) as i64;
        if tile != next_tile {
            return if step > 0.0 { q } else { next };
        }
        q = next;
        tile = next_tile;
    }
}

const LUCENE_LAT_RES: f64 = 180.0 / (1u64 << 32) as f64;

fn tile_to_lat_raw(y: f64, tiles: i64) -> f64 {
    let n = std::f64::consts::PI - (std::f64::consts::PI * 2.0 * y) / tiles as f64;
    crate::jmath::fast_atan(crate::jmath::fast_sinh(n)).to_degrees()
}

/// `GeoEncodingUtils.decodeLatitude(encodeLatitude(lat))`.
pub(crate) fn quantize_lat(lat: f64) -> f64 {
    encode_lat(lat) as f64 * LAT_DECODE
}

/// `GeoTileBoundedPredicate`: which tiles of a precision intersect a bounding box (tiles that
/// only touch it are out).
#[derive(Clone, Copy, Debug)]
pub struct TileBounds {
    pub(crate) crosses_dateline: bool,
    precision: u32,
    pub(crate) left_x: i64,
    pub(crate) right_x: i64,
    pub(crate) min_y: i64,
    pub(crate) max_y: i64,
}

/// `GeoTileUtils.LATITUDE_MASK`.
const LATITUDE_MASK: f64 = 85.0511287798066;

impl TileBounds {
    pub fn new(precision: u32, top: f64, left: f64, bottom: f64, right: f64) -> TileBounds {
        let crosses_dateline = right < left;
        if bottom > quantize_lat(LATITUDE_MASK) || top < quantize_lat(-LATITUDE_MASK) {
            return TileBounds { crosses_dateline, precision, left_x: -1, right_x: -1, min_y: -1, max_y: -1 };
        }
        let tiles = 1i32 << precision;
        let min_x = tile_x(left, tiles) as i64;
        let min_y = tile_y(top, tiles) as i64;
        let t = tiles as i64;
        // Touching tiles are out: they must share an interior point.
        let left_x = if encode_lon(tile_to_lon(min_x + 1, t)) == encode_lon(left) { min_x + 1 } else { min_x };
        let min_y2 = if encode_lat(tile_to_lat(min_y + 1, t)) == encode_lat(top) { min_y + 1 } else { min_y };
        let max_x = tile_x(right, tiles) as i64;
        let max_y = tile_y(bottom, tiles) as i64;
        let right_x = if encode_lon(tile_to_lon(max_x, t)) == encode_lon(right) { max_x } else { max_x + 1 };
        let max_y2 = if encode_lat(tile_to_lat(max_y, t)) == encode_lat(bottom) { max_y } else { max_y + 1 };
        TileBounds { crosses_dateline, precision, left_x, right_x, min_y: min_y2, max_y: max_y2 }
    }

    /// `validTile(x, y, precision)`.
    pub fn valid(&self, x: i64, y: i64, precision: u32) -> bool {
        let splits = 1i64 << (self.precision - precision);
        let y_min = y * splits;
        if self.max_y > y_min && self.min_y < y_min + splits {
            let x_min = x * splits;
            if self.crosses_dateline {
                self.right_x > x_min || self.left_x < x_min + splits
            } else {
                self.right_x > x_min && self.left_x < x_min + splits
            }
        } else {
            false
        }
    }
}

/// `BitUtil.flipFlop`: swaps even and odd bits.
fn flip_flop(b: u64) -> u64 {
    ((b & 0xAAAA_AAAA_AAAA_AAAA) >> 1) | ((b & 0x5555_5555_5555_5555) << 1)
}

/// Decodes a morton-encoded point (`Geohash.decodeLongitude/decodeLatitude`): `(lat, lon)`.
fn morton_decode(m: u64) -> (f64, f64) {
    let lon = deinterleave(m) as f64 / ((1u64 << 31) as f64 / 360.0) - 180.0;
    let lat = deinterleave(m >> 1) as f64 / ((1u64 << 31) as f64 / 180.0) - 90.0;
    (lat, lon)
}

/// `Geohash.toBoundingBox` of a geohash long: `(min_lon, max_lon, max_lat, min_lat)`.
pub fn geohash_cell_bounds(cell: i64) -> (f64, f64, f64, f64) {
    let len = (cell & 15) as u32;
    let (min_lat, min_lon) = geohash_decode(&geohash_string(cell)).unwrap_or((f64::NAN, f64::NAN));
    let gh = (cell as u64) >> 4;
    let lon = deinterleave(gh >> 1);
    let lat = deinterleave(gh);
    let shift = (12 - len) * 5 + 2;
    const MAX_LAT_BITS: u64 = (1 << (GEOHASH_MAX_PRECISION * 5 / 2)) - 1;
    if lat < MAX_LAT_BITS {
        let m = flip_flop(interleave((lat + 1) as u32, (lon + 1) as u32) << shift);
        let (top, right) = morton_decode(m);
        (min_lon, right, top, min_lat)
    } else {
        let m = flip_flop(interleave(lat as u32, (lon + 1) as u32) << shift);
        let (_, right) = morton_decode(m);
        (min_lon, right, 90.0, min_lat)
    }
}

/// `Geohash.longEncode(String)`.
pub fn geohash_long_of(hash: &[u8]) -> i64 {
    let len = hash.len().min(GEOHASH_MAX_PRECISION as usize);
    let mut level = len as i32 - 1;
    let mut l: u64 = 0;
    for &c in hash {
        let b = BASE_32.iter().position(|x| *x == c).unwrap_or(0) as u64;
        l |= b << (level * 5);
        level -= 1;
        if level < 0 {
            break;
        }
    }
    ((l << 4) | len as u64) as i64
}

/// `Geohash.lonWidthInDegrees` / `latHeightInDegrees`: `(width, height)` of a level's cells.
pub fn geohash_cell_size(precision: u32) -> (f64, f64) {
    let (mut h, mut w) = (180.0, 360.0);
    let mut even = false;
    for _ in 0..precision {
        h /= if even { 8.0 } else { 4.0 };
        w /= if even { 4.0 } else { 8.0 };
        even = !even;
    }
    (w, h)
}

/// `Geohash.getNeighbor(geohash, level, dx, dy)`: `None` past a pole.
pub fn geohash_neighbor(hash: &[u8], level: usize, dx: i32, dy: i32) -> Option<Vec<u8>> {
    let cell = BASE_32.iter().position(|x| *x == hash[level - 1]).unwrap_or(0) as i32;
    let (x0, y0, x1, y1, x2) = (cell & 1, cell & 2, cell & 4, cell & 8, cell & 16);
    let x = x0 + (x1 / 2) + (x2 / 4);
    let y = (y0 / 2) + (y1 / 4);
    let enc = |x: i32, y: i32| BASE_32[(((x & 1) + ((y & 1) * 2) + ((x & 2) * 2) + ((y & 2) * 4) + ((x & 4) * 4)) % 32) as usize];
    if level == 1 {
        if (dy < 0 && y == 0) || (dy > 0 && y == 3) {
            return None;
        }
        return Some(vec![enc(x + dx, y + dy)]);
    }
    let (nx, ny) = if level % 2 == 1 { (x + dx, y + dy) } else { (x + dy, y + dx) };
    if (0..=7).contains(&nx) && (0..=3).contains(&ny) {
        let mut out = hash[..level - 1].to_vec();
        out.push(enc(nx, ny));
        Some(out)
    } else {
        let mut n = geohash_neighbor(hash, level - 1, dx, dy)?;
        n.push(enc(nx, ny));
        Some(n)
    }
}

/// The geohash alphabet, in `getSubGeohashes` order.
pub const GEOHASH_BASE_32: &[u8; 32] = BASE_32;

/// `GeoTileUtils.MAX_ZOOM`.
pub const MAX_ZOOM: u32 = 29;
const ZOOM_SHIFT: u32 = MAX_ZOOM * 2;
const X_Y_MASK: u64 = (1 << MAX_ZOOM) - 1;

/// `GeoTileUtils.longEncodeTiles`.
pub fn geotile_long_tiles(precision: u32, x: u32, y: u32) -> i64 {
    (((precision as u64) << ZOOM_SHIFT) | ((x as u64) << MAX_ZOOM) | y as u64) as i64
}

/// `(zoom, x, y)` of a geotile long.
pub fn geotile_parts(v: i64) -> (u32, u32, u32) {
    let u = v as u64;
    ((u >> ZOOM_SHIFT) as u32, ((u >> MAX_ZOOM) & X_Y_MASK) as u32, (u & X_Y_MASK) as u32)
}

/// `GeoTileUtils.stringEncode`.
pub fn geotile_string(v: i64) -> String {
    let (z, x, y) = geotile_parts(v);
    format!("{z}/{x}/{y}")
}

/// `GeoTileUtils.longEncode(String)`.
pub fn geotile_parse(s: &str) -> Result<i64> {
    let bad = || AggError::illegal_argument(format!("Invalid geotile_grid hash string of {s}. Must be three integers in a form \"zoom/x/y\"."));
    let parts: Vec<&str> = s.splitn(4, '/').collect();
    if parts.len() != 3 {
        return Err(bad());
    }
    let z: u32 = parts[0].parse().map_err(|_| bad())?;
    let x: u32 = parts[1].parse().map_err(|_| bad())?;
    let y: u32 = parts[2].parse().map_err(|_| bad())?;
    Ok(geotile_long_tiles(z, x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geohashes() {
        let h = geohash_long(-71.0, 42.0, 5);
        assert_eq!(geohash_string(h), "drmwb");
        let (lat, lon) = geohash_decode("drmwb").unwrap();
        assert!((lat - 41.98).abs() < 0.05 && (lon + 71.0).abs() < 0.05, "{lat} {lon}");
        assert_eq!(geotile_string(geotile_long_tiles(3, 2, 5)), "3/2/5");
        assert_eq!(geotile_parse("3/2/5").unwrap(), geotile_long_tiles(3, 2, 5));
    }
}
