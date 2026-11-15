//! Grid cells of shapes: `GeoTileGridTiler` and `GeoHashGridTiler` (Elasticsearch's spatial
//! module), which `geotile_grid` and `geohash_grid` use on `geo_shape` fields.
//!
//! A shape's cells are those its triangles touch ([`ShapeValue::relate_tile`] on the cell's
//! encoded box, bounds shrunk by one unit where cells meet so that shared edges belong to one
//! side). A shape whose bounding box fits one cell gets that cell; small boxes are scanned cell
//! by cell; larger ones are rasterized from the top of the grid down, recursing into crossing
//! cells and taking every sub-cell of cells lying inside the shape. With `bounds`, cells outside
//! them are skipped (`GeoTileBoundedPredicate` / `GeoHashBoundedPredicate`). The aggregator then
//! sorts the cells and counts each once per doc.

use search_codec::points::encode::{encode_latitude, encode_longitude};

use crate::error::{AggError, Result};
use crate::geo::{
    GEOHASH_BASE_32, TileBounds, geohash_cell_bounds, geohash_cell_size, geohash_long, geohash_long_of, geohash_neighbor, geohash_string,
    geotile_long_tiles, tile_to_lat, tile_to_lon, tile_x, tile_y,
};
use crate::shape_field::{GeoRelation, ShapeValue};

/// `GeoTileUtils.LATITUDE_MASK`.
const LATITUDE_MASK: f64 = 85.0511287798066;

fn overflow() -> AggError {
    AggError::illegal_argument("Tile aggregation array overflow")
}

/// Cells must stay addressable by an `int` (`getNewSize`).
fn check_size(len: usize, add: u64) -> Result<()> {
    if len as u64 + add > i32::MAX as u64 { Err(overflow()) } else { Ok(()) }
}

/// `GeoTileGridTiler`.
pub struct TileTiler<'a> {
    pub precision: u32,
    pub bounds: Option<&'a TileBounds>,
}

impl TileTiler<'_> {
    fn valid(&self, x: i64, y: i64, z: u32) -> bool {
        self.bounds.is_none_or(|b| b.valid(x, y, z))
    }

    /// `relateTile`.
    fn relate(&self, v: &ShapeValue, x: i64, y: i64, z: u32) -> GeoRelation {
        if !self.valid(x, y, z) {
            return GeoRelation::Disjoint;
        }
        let tiles = 1i64 << z;
        let min_x = encode_longitude(tile_to_lon(x, tiles));
        let max_x = encode_longitude(tile_to_lon(x + 1, tiles));
        let min_y = encode_latitude(tile_to_lat(y + 1, tiles));
        let max_y = encode_latitude(tile_to_lat(y, tiles));
        let neg_mask = encode_latitude(-LATITUDE_MASK);
        v.relate_tile(min_x, if max_x == i32::MAX { max_x } else { max_x - 1 }, if min_y == neg_mask { min_y } else { min_y + 1 }, max_y)
    }

    /// `setValues`: the doc's tiles (unsorted).
    pub fn cells(&self, v: &ShapeValue, out: &mut Vec<i64>) -> Result<()> {
        out.clear();
        let b = v.bounding_box();
        if b.bottom > crate::geo::quantize_lat(LATITUDE_MASK) || b.top < crate::geo::quantize_lat(-LATITUDE_MASK) {
            return Ok(());
        }
        if self.precision == 0 {
            return self.brute_force(v, 0, 0, 0, 0, out);
        }
        let tiles = 1i32 << self.precision;
        let min_x = tile_x(b.min_x(), tiles) as i64;
        let min_y = tile_y(b.max_y(), tiles) as i64;
        let max_x = tile_x(b.max_x(), tiles) as i64;
        let max_y = tile_y(b.min_y(), tiles) as i64;
        let count = (max_x - min_x + 1) * (max_y - min_y + 1);
        if count == 1 {
            if self.valid(min_x, min_y, self.precision) {
                out.push(geotile_long_tiles(self.precision, min_x as u32, min_y as u32));
            }
            Ok(())
        } else if count <= 8 * self.precision as i64 {
            self.brute_force(v, min_x, min_y, max_x, max_y, out)
        } else {
            self.rasterize(v, 0, 0, 0, out)
        }
    }

    fn brute_force(&self, v: &ShapeValue, min_x: i64, min_y: i64, max_x: i64, max_y: i64, out: &mut Vec<i64>) -> Result<()> {
        for i in min_x..=max_x {
            for j in min_y..=max_y {
                if self.relate(v, i, j, self.precision) != GeoRelation::Disjoint {
                    out.push(geotile_long_tiles(self.precision, i as u32, j as u32));
                }
            }
        }
        Ok(())
    }

    fn rasterize(&self, v: &ShapeValue, x: i64, y: i64, z: u32, out: &mut Vec<i64>) -> Result<()> {
        let z = z + 1;
        for i in 0..2 {
            for j in 0..2 {
                let (nx, ny) = (2 * x + i, 2 * y + j);
                let rel = self.relate(v, nx, ny, z);
                if rel == GeoRelation::Disjoint {
                    continue;
                }
                if z == self.precision {
                    check_size(out.len(), 1)?;
                    out.push(geotile_long_tiles(z, nx as u32, ny as u32));
                } else if rel == GeoRelation::Inside {
                    let n = (1u64 << (2 * (self.precision - z))).min(self.max_cells());
                    check_size(out.len(), n + 1)?;
                    self.fully_contained(nx, ny, z, out);
                } else {
                    self.rasterize(v, nx, ny, z, out)?;
                }
            }
        }
        Ok(())
    }

    /// `getMaxCells`.
    fn max_cells(&self) -> u64 {
        match self.bounds {
            None => 1u64 << (2 * self.precision),
            Some(b) if b.left_x < 0 => 0,
            Some(b) if b.crosses_dateline => (((1i64 << self.precision) + b.right_x - b.left_x) * (b.max_y - b.min_y)).max(0) as u64,
            Some(b) => ((b.right_x - b.left_x) * (b.max_y - b.min_y)).max(0) as u64,
        }
    }

    /// `setValuesForFullyContainedTile`: every tile at the target precision under `(x, y, z)`.
    fn fully_contained(&self, x: i64, y: i64, z: u32, out: &mut Vec<i64>) {
        let splits = 1i64 << (self.precision - z);
        let mut fill = |min_x: i64, max_x: i64, min_y: i64, max_y: i64| {
            for i in min_x..max_x {
                for j in min_y..max_y {
                    out.push(geotile_long_tiles(self.precision, i as u32, j as u32));
                }
            }
        };
        match self.bounds {
            None => fill(x * splits, x * splits + splits, y * splits, y * splits + splits),
            Some(b) => {
                let min_y = b.min_y.max(y * splits);
                let max_y = b.max_y.min(y * splits + splits);
                if b.crosses_dateline {
                    let west_min = b.left_x.max(x * splits);
                    let west_max = x * splits + splits;
                    fill(west_min, west_max, min_y, max_y);
                    let east_max = west_min.min(b.right_x.min(x * splits + splits));
                    fill(x * splits, east_max, min_y, max_y);
                } else {
                    fill(b.left_x.max(x * splits), b.right_x.min(x * splits + splits), min_y, max_y);
                }
            }
        }
    }
}

/// `GeoHashGridTiler`.
pub struct HashTiler<'a> {
    pub precision: u32,
    /// `GeoHashBoundedPredicate.validHash`.
    pub valid: Option<&'a dyn Fn(i64) -> bool>,
}

impl HashTiler<'_> {
    fn relate(&self, v: &ShapeValue, hash: &[u8]) -> GeoRelation {
        let cell = geohash_long_of(hash);
        if self.valid.is_some_and(|f| !f(cell)) {
            return GeoRelation::Disjoint;
        }
        let (min_lon, max_lon, max_lat, min_lat) = geohash_cell_bounds(cell);
        let (min_x, min_y) = (encode_longitude(min_lon), encode_latitude(min_lat));
        let (max_x, max_y) = (encode_longitude(max_lon), encode_latitude(max_lat));
        v.relate_tile(min_x, if max_x == i32::MAX { max_x } else { max_x - 1 }, min_y, if max_y == i32::MAX { max_y } else { max_y - 1 })
    }

    fn add(&self, v: &ShapeValue, hash: &[u8], out: &mut Vec<i64>) {
        if self.relate(v, hash) != GeoRelation::Disjoint {
            out.push(geohash_long_of(hash));
        }
    }

    /// `setValues`: the doc's geohashes (unsorted).
    pub fn cells(&self, v: &ShapeValue, out: &mut Vec<i64>) -> Result<()> {
        out.clear();
        if self.precision == 0 {
            return Ok(());
        }
        let b = v.bounding_box();
        let p = self.precision;
        if b.min_x() == b.max_x() && b.min_y() == b.max_y() {
            let h = geohash_string(geohash_long(b.min_x(), b.min_y(), p));
            self.add(v, h.as_bytes(), out);
            return Ok(());
        }
        let (w, h) = geohash_cell_size(p);
        let dx = ((b.max_x() - b.min_x()) / w).ceil() as i64;
        let dy = ((b.max_y() - b.min_y()) / h).ceil() as i64;
        if dx.saturating_mul(dy) <= 32 * p as i64 { self.brute_force(v, out) } else { self.rasterize(v, &[], out) }
    }

    fn brute_force(&self, v: &ShapeValue, out: &mut Vec<i64>) -> Result<()> {
        let b = v.bounding_box();
        let p = self.precision;
        let encode = |lon: f64, lat: f64| geohash_string(geohash_long(lon, lat, p)).into_bytes();
        let npe = || AggError::new(500, "null_pointer_exception", "geohash neighbor past a pole");
        let stop = encode(b.max_x(), b.max_y());
        let (mut first_in_row, mut last_in_row): (Option<Vec<u8>>, Option<Vec<u8>>) = (None, None);
        loop {
            last_in_row = Some(match last_in_row {
                None => encode(b.max_x(), b.min_y()),
                Some(h) => geohash_neighbor(&h, p as usize, 0, 1).ok_or_else(npe)?,
            });
            let last = last_in_row.as_deref().unwrap_or_default();
            let mut current: Option<Vec<u8>> = None;
            loop {
                let c = match current {
                    None => {
                        let f = match first_in_row.take() {
                            None => encode(b.min_x(), b.min_y()),
                            Some(h) => geohash_neighbor(&h, p as usize, 0, 1).ok_or_else(npe)?,
                        };
                        first_in_row = Some(f.clone());
                        f
                    }
                    Some(c) => geohash_neighbor(&c, p as usize, 1, 0).ok_or_else(npe)?,
                };
                self.add(v, &c, out);
                let done = c == last;
                current = Some(c);
                if done {
                    break;
                }
            }
            if last_in_row.as_deref() == Some(stop.as_slice()) {
                return Ok(());
            }
        }
    }

    fn rasterize(&self, v: &ShapeValue, hash: &[u8], out: &mut Vec<i64>) -> Result<()> {
        let mut s = hash.to_vec();
        s.push(b'0');
        for &c in GEOHASH_BASE_32 {
            *s.last_mut().expect("non-empty") = c;
            let rel = self.relate(v, &s);
            if rel == GeoRelation::Disjoint {
                continue;
            }
            if s.len() == self.precision as usize {
                check_size(out.len(), 1)?;
                out.push(geohash_long_of(&s));
            } else if rel == GeoRelation::Inside {
                let n = 32u64.saturating_pow(self.precision - hash.len() as u32).saturating_add(1).min(self.max_cells());
                check_size(out.len(), n + 1)?;
                self.fully_contained(&s, out);
            } else {
                self.rasterize(v, &s, out)?;
            }
        }
        Ok(())
    }

    /// `getMaxCells` (unbounded: `32^precision`; bounded grids are capped by the box later).
    fn max_cells(&self) -> u64 {
        32u64.saturating_pow(self.precision)
    }

    fn fully_contained(&self, hash: &[u8], out: &mut Vec<i64>) {
        let mut s = hash.to_vec();
        s.push(b'0');
        for &c in GEOHASH_BASE_32 {
            *s.last_mut().expect("non-empty") = c;
            let cell = geohash_long_of(&s);
            if self.valid.is_none_or(|f| f(cell)) {
                if s.len() == self.precision as usize {
                    out.push(cell);
                } else {
                    self.fully_contained(&s, out);
                }
            }
        }
    }
}
