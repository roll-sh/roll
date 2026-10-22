//! `geohash_grid`, `geotile_grid` and `geohex_grid`: buckets per grid cell of `geo_point`
//! values, or of `geo_shape` values (every cell a shape touches: [`crate::bucket::shape_grid`],
//! [`crate::bucket::shape_hex`]).
//!
//! Points map to cell ids (`Geohash.longEncode` / `GeoTileUtils.longEncode` / H3's
//! `geoToH3`, see [`crate::h3`]), optionally only inside `bounds` (geohash and H3 cells must
//! contain the point or have a bounding box intersecting the box; tiles must intersect it). Each owning bucket keeps its `shard_size` most populated cells; the reduce
//! merges cells by id and keeps the top `size`. Ties order by descending cell id, as
//! Elasticsearch's `BucketPriorityQueue` does.

use std::cmp::Ordering;

use search_index::SegmentContext;
use smallvec::SmallVec;

use crate::bucket::composite::{GeoBounds, parse_bounds};
use crate::collect::{Aggregator, Batch, BindCtx, Subs, grow};
use crate::error::{AggError, Result};
use crate::geo::{TileBounds, geohash_long, geohash_string, geotile_long_tiles, geotile_string, tile_x, tile_y};
use crate::internal::{AggValue, Aggs, Internal, MultiBuckets, Prop, ReduceCtx, RenderCtx, multi_bucket_property};
use crate::json::Json;
use crate::memory::MemTracker;
use crate::request::{AggDef, PathElement};
use crate::util::OrdMap;
use crate::bucket::shape_grid::{HashTiler, TileTiler};
use crate::bucket::shape_hex::{HexBounds, HexTiler};
use crate::shape_field::ShapeReader;
use crate::values::{BoundVs, DocCounter, GeoReader, VsOpts, VsSpec, VsType, bind};
use crate::xparser::{Field, Token, Tokens, XParser};

/// Which grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GridKind {
    GeoHash,
    GeoTile,
    GeoHex,
}

impl GridKind {
    fn type_name(self) -> &'static str {
        match self {
            GridKind::GeoHash => "geohash_grid",
            GridKind::GeoTile => "geotile_grid",
            GridKind::GeoHex => "geohex_grid",
        }
    }
}

/// A parsed grid aggregation.
#[derive(Clone, Debug)]
pub struct GridSpec {
    pub kind: GridKind,
    pub vs: VsSpec,
    pub precision: u32,
    pub size: i32,
    pub shard_size: i32,
    pub bounds: Option<GeoBounds>,
}

/// `DistanceUnit.parse(distance, METERS)`: the distance in meters.
pub fn parse_distance_meters(s: &str) -> Result<f64> {
    const UNITS: [(&[&str], f64); 9] = [
        (&["in", "inch"], 0.0254),
        (&["yd", "yards"], 0.9144),
        (&["ft", "feet"], 0.3048),
        (&["km", "kilometers"], 1000.0),
        (&["NM", "nmi", "nauticalmiles"], 1852.0),
        (&["mm", "millimeters"], 0.001),
        (&["cm", "centimeters"], 0.01),
        (&["mi", "miles"], 1609.344),
        (&["m", "meters"], 1.0),
    ];
    for (names, factor) in UNITS {
        for n in names {
            if let Some(num) = s.strip_suffix(n) {
                return Ok(crate::xparser::java_parse_double(num)? * factor);
            }
        }
    }
    crate::xparser::java_parse_double(s)
}

/// `GeoUtils.geoHashLevelsForPrecision(meters)`.
pub fn geohash_levels_for_precision(meters: f64) -> i32 {
    if meters == 0.0 {
        return 24;
    }
    let equator = 2.0 * std::f64::consts::PI * 6378137.0;
    let polar = std::f64::consts::PI * 6356752.314245;
    let ratio = 1.0 + polar / equator;
    let width = ((meters * meters) / (ratio * ratio)).sqrt();
    let part = (equator / width).ceil();
    if part == 1.0 {
        return 1;
    }
    let bits = (part.ln() / 2f64.ln()).ceil().round() as i32;
    let full = bits / 5;
    let left = bits - full * 5;
    let even = full + i32::from(left > 0);
    let odd = full + i32::from(left > 3);
    even + odd
}

fn check_geohash_precision(p: i32) -> Result<i32> {
    if !(1..=12).contains(&p) {
        return Err(AggError::illegal_argument(format!("Invalid geohash aggregation precision of {p}. Must be between 1 and 12.")));
    }
    Ok(p)
}

/// `XContentMapValues.nodeIntegerValue(String)`: `Integer.parseInt`.
fn parse_int(s: &str) -> Option<i32> {
    s.parse::<i32>().ok()
}

/// Parses `geohash_grid` / `geotile_grid`.
pub fn parse(p: &mut XParser<'_>, kind: GridKind, name: &str) -> Result<GridSpec> {
    let opts = VsOpts::new(false, false, false);
    let mut fields = VsSpec::fields(opts);
    fields.extend_from_slice(&[
        Field::new("precision", Tokens::of(&[Token::Number, Token::String])),
        Field::new("size", Tokens::INT),
        Field::new("shard_size", Tokens::INT),
        Field::new("bounds", Tokens::OBJECT),
    ]);
    let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
    let mut spec = GridSpec {
        kind,
        vs: VsSpec::default(),
        precision: if kind == GridKind::GeoTile { 7 } else { 5 },
        size: 10000,
        shard_size: -1,
        bounds: None,
    };
    p.object(kind.type_name(), &fields, |p, i| {
        let f = names[i];
        if spec.vs.parse_field(p, f)? {
            return Ok(());
        }
        match f {
            "precision" => {
                let v = match kind {
                    GridKind::GeoTile | GridKind::GeoHex => {
                        let v = if p.token() == Token::Number {
                            p.int_value()?
                        } else {
                            let t = p.text()?;
                            parse_int(&t).ok_or_else(|| AggError::new(400, "number_format_exception", format!("For input string: \"{t}\"")))?
                        };
                        if kind == GridKind::GeoHex && !(0..=crate::h3::MAX_RES).contains(&v) {
                            return Err(AggError::illegal_argument(format!("Invalid geohex aggregation precision of {v}. Must be between 0 and 15")));
                        }
                        if kind == GridKind::GeoTile && !(0..=29).contains(&v) {
                            return Err(AggError::illegal_argument(format!("Invalid geotile_grid precision of {v}. Must be between 0 and 29.")));
                        }
                        v
                    }
                    GridKind::GeoHash => {
                        let v = if p.token() == Token::Number {
                            p.int_value()?
                        } else {
                            let t = p.text()?;
                            match parse_int(&t) {
                                Some(v) => v,
                                None => {
                                    let lv = geohash_levels_for_precision(parse_distance_meters(&t)?);
                                    check_geohash_precision(lv)
                                        .map_err(|e| AggError::illegal_argument(format!("precision too high [{t}]")).caused_by(e))?
                                }
                            }
                        };
                        check_geohash_precision(v)?
                    }
                };
                spec.precision = v as u32;
            }
            "size" => {
                let v = p.int_value()?;
                if v <= 0 {
                    return Err(AggError::illegal_argument(format!("[size] must be greater than 0. Found [{v}] in [{name}]")));
                }
                spec.size = v;
            }
            "shard_size" => {
                let v = p.int_value()?;
                if v <= 0 {
                    return Err(AggError::illegal_argument(format!("[shardSize] must be greater than 0. Found [{v}] in [{name}]")));
                }
                spec.shard_size = v;
            }
            _ => spec.bounds = Some(parse_bounds(p)?),
        }
        Ok(())
    })?;
    spec.vs.check_required(opts)?;
    Ok(spec)
}

/// A grid bucket.
#[derive(Clone, Debug)]
pub struct GridBucket {
    pub cell: i64,
    pub doc_count: u64,
    pub aggs: Aggs,
}

/// `InternalGeoHashGrid` / `InternalGeoTileGrid`.
#[derive(Clone, Debug)]
pub struct GeoGrid {
    pub kind: GridKind,
    pub size: usize,
    pub buckets: Vec<GridBucket>,
}

/// `BucketPriorityQueue` order: more docs first, then larger cell ids.
fn grid_cmp(a_count: u64, a_cell: i64, b_count: u64, b_cell: i64) -> Ordering {
    b_count.cmp(&a_count).then(b_cell.cmp(&a_cell))
}

impl GeoGrid {
    fn key_string(&self, cell: i64) -> String {
        match self.kind {
            GridKind::GeoHash => geohash_string(cell),
            GridKind::GeoTile => geotile_string(cell),
            GridKind::GeoHex => crate::h3::h3_to_string(cell as u64),
        }
    }
}

impl Internal for GeoGrid {
    fn type_name(&self) -> &'static str {
        self.kind.type_name()
    }
    fn class_name(&self) -> &'static str {
        match self.kind {
            GridKind::GeoHash => "InternalGeoHashGrid",
            GridKind::GeoTile => "InternalGeoTileGrid",
            GridKind::GeoHex => "InternalGeoHexGrid",
        }
    }
    fn must_reduce_single(&self) -> bool {
        true
    }
    fn reduce(list: Vec<Self>, ctx: &mut ReduceCtx<'_>) -> Result<Self> {
        let (kind, size) = (list[0].kind, list[0].size);
        let mut index: hashbrown::HashMap<i64, usize> = hashbrown::HashMap::new();
        let mut groups: Vec<(i64, u64, Vec<Aggs>)> = Vec::new();
        for g in list {
            for b in g.buckets {
                match index.get(&b.cell) {
                    Some(&i) => {
                        groups[i].1 += b.doc_count;
                        groups[i].2.push(b.aggs);
                    }
                    None => {
                        index.insert(b.cell, groups.len());
                        groups.push((b.cell, b.doc_count, vec![b.aggs]));
                    }
                }
            }
        }
        let keep = if ctx.is_final() { size.min(groups.len()) } else { groups.len() };
        groups.sort_by(|a, b| grid_cmp(a.1, a.0, b.1, b.0));
        groups.truncate(keep);
        let mut buckets = Vec::with_capacity(groups.len());
        for (cell, doc_count, aggs) in groups {
            buckets.push(GridBucket { cell, doc_count, aggs: Aggs::reduce_hashed(aggs, ctx)? });
        }
        ctx.consume_buckets(buckets.len())?;
        Ok(GeoGrid { kind, size, buckets })
    }
    fn render(&self, out: &mut Json, rc: &RenderCtx) {
        let buckets = self
            .buckets
            .iter()
            .map(|b| {
                let mut o = Json::obj();
                o.push("key", Json::Str(self.key_string(b.cell)));
                o.push("doc_count", Json::from(b.doc_count));
                b.aggs.render_into(&mut o, rc);
                o
            })
            .collect();
        out.push("buckets", Json::Arr(buckets));
    }
    fn property(&self, name: &str, path: &[String]) -> Result<Prop<'_>> {
        multi_bucket_property(self, name, path)
    }
    fn ram_bytes(&self) -> usize {
        64 + self.buckets.iter().map(|b| 40 + b.aggs.ram_bytes()).sum::<usize>()
    }
}

impl MultiBuckets for GeoGrid {
    fn len(&self) -> usize {
        self.buckets.len()
    }
    fn doc_count(&self, i: usize) -> u64 {
        self.buckets[i].doc_count
    }
    /// `getKey()`: the cell's point (geohash: its corner; geotile: its center) as `lat, lon`.
    fn key(&self, i: usize) -> Json {
        Json::Str(self.key_string(self.buckets[i].cell))
    }
    fn key_string(&self, i: usize) -> String {
        self.key_string(self.buckets[i].cell)
    }
    fn aggs(&self, i: usize) -> &Aggs {
        &self.buckets[i].aggs
    }
    fn aggs_mut(&mut self, i: usize) -> &mut Aggs {
        &mut self.buckets[i].aggs
    }
    fn retain_buckets(&mut self, keep: &[bool]) {
        let mut i = 0;
        self.buckets.retain(|_| {
            let k = keep[i];
            i += 1;
            k
        });
    }
    fn reorder(&mut self, order: &[usize]) {
        let mut slots: Vec<Option<GridBucket>> = self.buckets.drain(..).map(Some).collect();
        self.buckets = order.iter().map(|&i| slots[i].take().expect("bucket")).collect();
    }
}

/// `GeoHashBoundedPredicate` + `CellIdSource.pointInBounds`.
#[derive(Clone, Copy, Debug)]
pub struct HashBounds {
    b: GeoBounds,
    crosses: bool,
}

impl HashBounds {
    pub fn new(b: GeoBounds) -> HashBounds {
        HashBounds { b, crosses: b.right < b.left }
    }

    fn point_in(&self, lat: f64, lon: f64) -> bool {
        if self.b.top >= lat && self.b.bottom <= lat {
            if self.crosses { lon >= self.b.left || lon <= self.b.right } else { lon >= self.b.left && lon <= self.b.right }
        } else {
            false
        }
    }

    fn intersects(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> bool {
        if self.b.top > min_y && self.b.bottom < max_y {
            if self.crosses { self.b.left < max_x || self.b.right > min_x } else { self.b.left < max_x && self.b.right > min_x }
        } else {
            false
        }
    }

    /// `GeoHashBoundedPredicate.validHash`.
    pub fn valid_hash(&self, cell: i64) -> bool {
        let (min_x, max_x, max_y, min_y) = crate::geo::geohash_cell_bounds(cell);
        self.valid_box(min_x, max_x, min_y, max_y)
    }

    /// `GeoHexPredicate.validHex`: the H3 cell's bounding box intersects.
    fn valid_hex(&self, cell: u64) -> bool {
        let (top, left, bottom, right) = crate::h3::cell_bounds(cell);
        self.valid_box(left, right, bottom, top)
    }

    /// A cell box (wrapping the dateline when `max_x < min_x`) intersects.
    fn valid_box(&self, min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> bool {
        if max_x < min_x {
            self.intersects(-180.0, max_x, min_y, max_y) || self.intersects(min_x, 180.0, min_y, max_y)
        } else {
            self.intersects(min_x, max_x, min_y, max_y)
        }
    }
}

/// Binds a grid aggregation.
pub fn create(def: &AggDef, spec: &GridSpec, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    let vs = bind(&spec.vs, ctx.lookup, VsType::GeoPoint, false)?;
    if !vs.unmapped && !matches!(vs.vs_type, VsType::GeoPoint | VsType::GeoShape) {
        return Err(vs.unsupported(spec.kind.type_name()));
    }
    let size = spec.size;
    let mut shard_size = spec.shard_size;
    if shard_size < 0 {
        shard_size = crate::bucket::terms::suggest_shard_size(size as usize).min(i32::MAX as usize) as i32;
    }
    if shard_size < size {
        shard_size = size;
    }
    let subs = Subs::bind(def, ctx)?;
    Ok(Box::new(GridAgg {
        kind: spec.kind,
        precision: spec.precision,
        size: size as usize,
        shard_size: shard_size as usize,
        tile_bounds: match spec.kind {
            GridKind::GeoTile => spec.bounds.map(|b| TileBounds::new(spec.precision, b.top, b.left, b.bottom, b.right)),
            GridKind::GeoHash | GridKind::GeoHex => None,
        },
        hash_bounds: match spec.kind {
            GridKind::GeoHash | GridKind::GeoHex => spec.bounds.map(HashBounds::new),
            GridKind::GeoTile => None,
        },
        hex_bounds: match (spec.kind, spec.bounds) {
            (GridKind::GeoHex, Some(b)) if vs.vs_type == VsType::GeoShape => Some(HexBounds::new(spec.precision as i32, b.top, b.left, b.bottom, b.right)),
            _ => None,
        },
        shape: vs.vs_type == VsType::GeoShape,
        shapes: None,
        cells: Vec::new(),
        vs,
        reader: None,
        ords: OrdMap::new(),
        counts: Vec::new(),
        subs,
        doc_counter: DocCounter::none(),
        doc_count_field: ctx.doc_count_field,
        tracker: ctx.tracker(def),
    }))
}

struct GridAgg {
    kind: GridKind,
    precision: u32,
    size: usize,
    shard_size: usize,
    tile_bounds: Option<TileBounds>,
    hash_bounds: Option<HashBounds>,
    /// `bounds` of `geohex_grid` on shapes (with the per-resolution inflated boxes).
    hex_bounds: Option<HexBounds>,
    /// A `geo_shape` source: cells from the shape tilers.
    shape: bool,
    shapes: Option<ShapeReader>,
    cells: Vec<i64>,
    vs: BoundVs,
    reader: Option<GeoReader>,
    ords: OrdMap,
    counts: Vec<u64>,
    subs: Subs,
    doc_counter: DocCounter,
    doc_count_field: Option<u32>,
    tracker: MemTracker,
}

impl GridAgg {
    fn cell(&self, lat: f64, lon: f64) -> Option<i64> {
        match self.kind {
            GridKind::GeoHash => {
                let h = geohash_long(lon, lat, self.precision);
                match &self.hash_bounds {
                    None => Some(h),
                    Some(b) => (b.point_in(lat, lon) || b.valid_hash(h)).then_some(h),
                }
            }
            GridKind::GeoHex => {
                let h = crate::h3::geo_to_h3(lat, lon, self.precision as i32);
                match &self.hash_bounds {
                    None => Some(h as i64),
                    Some(b) => (b.point_in(lat, lon) || b.valid_hex(h)).then_some(h as i64),
                }
            }
            GridKind::GeoTile => {
                let tiles = 1i32 << self.precision;
                let (x, y) = (tile_x(lon, tiles), tile_y(lat, tiles));
                if let Some(b) = &self.tile_bounds
                    && !b.valid(x as i64, y as i64, self.precision)
                {
                    return None;
                }
                Some(geotile_long_tiles(self.precision, x as u32, y as u32))
            }
        }
    }

    /// `GeoShapeCellValues`: each doc's cells, sorted, each counted once.
    fn collect_shapes(&mut self, b: Batch<'_>, shapes: &mut ShapeReader) -> Result<()> {
        let mut cells = std::mem::take(&mut self.cells);
        let r = (|| {
            for i in 0..b.len() {
                let doc = b.docs[i];
                let Some(v) = shapes.get(doc)? else { continue };
                match self.kind {
                    GridKind::GeoTile => TileTiler { precision: self.precision, bounds: self.tile_bounds.as_ref() }.cells(v, &mut cells)?,
                    GridKind::GeoHash => {
                        let hb = self.hash_bounds;
                        let valid = move |c: i64| hb.is_none_or(|h| h.valid_hash(c));
                        HashTiler { precision: self.precision, valid: hb.is_some().then_some(&valid as &dyn Fn(i64) -> bool) }.cells(v, &mut cells)?
                    }
                    GridKind::GeoHex => HexTiler { precision: self.precision as i32, bounds: self.hex_bounds.as_ref() }.cells(v, &mut cells)?,
                }
                self.tracker.ensure(self.ords.len() * 40 + cells.capacity() * 8)?;
                if cells.is_empty() {
                    continue;
                }
                cells.sort_unstable();
                cells.dedup();
                let inc = self.doc_counter.count(doc);
                for &c in &cells {
                    let (ord, new) = self.ords.add(b.ords[i], c as u64);
                    if new {
                        grow(&mut self.counts, ord, 0);
                        self.tracker.ensure(self.ords.len() * 40 + cells.capacity() * 8)?;
                    }
                    self.counts[ord as usize] += inc;
                    self.subs.push(doc, ord, b.score(i))?;
                }
            }
            Ok(())
        })();
        self.cells = cells;
        r
    }

    fn empty(&self) -> AggValue {
        AggValue::GeoGrid(Box::new(GeoGrid { kind: self.kind, size: self.size, buckets: Vec::new() }))
    }
}

impl Aggregator for GridAgg {
    fn set_segment(&mut self, seg: &SegmentContext) -> Result<()> {
        if self.shape {
            self.shapes = Some(ShapeReader::for_source(seg, &self.vs));
            self.doc_counter = DocCounter::open(seg, self.doc_count_field);
            return self.subs.set_segment(seg);
        }
        self.reader = if self.vs.unmapped && self.vs.missing.is_none() { None } else { Some(self.vs.geo_reader(seg)?) };
        self.doc_counter = DocCounter::open(seg, self.doc_count_field);
        self.subs.set_segment(seg)
    }

    fn collect(&mut self, b: Batch<'_>) -> Result<()> {
        if let Some(mut shapes) = self.shapes.take() {
            let r = self.collect_shapes(b, &mut shapes);
            self.shapes = Some(shapes);
            return r;
        }
        let Some(mut reader) = self.reader.take() else { return Ok(()) };
        let r = (|| {
            for i in 0..b.len() {
                let doc = b.docs[i];
                let mut cells: SmallVec<[i64; 4]> = SmallVec::new();
                for &(lat, lon) in reader.points(doc) {
                    if let Some(c) = self.cell(lat, lon) {
                        cells.push(c);
                    }
                }
                if cells.is_empty() {
                    continue;
                }
                // CellMultiValues sorts the cells; consecutive duplicates count once.
                cells.sort_unstable();
                cells.dedup();
                let inc = self.doc_counter.count(doc);
                for c in cells {
                    let (ord, new) = self.ords.add(b.ords[i], c as u64);
                    if new {
                        grow(&mut self.counts, ord, 0);
                        self.tracker.ensure(self.ords.len() * 40)?;
                    }
                    self.counts[ord as usize] += inc;
                    self.subs.push(doc, ord, b.score(i))?;
                }
            }
            Ok(())
        })();
        self.reader = Some(reader);
        r
    }

    fn finish_segment(&mut self) -> Result<()> {
        self.subs.finish_segment()
    }

    fn post_collection(&mut self) -> Result<()> {
        self.subs.post_collection()
    }

    fn build(&mut self, owning: &[u32]) -> Result<Vec<AggValue>> {
        // Per owning bucket: its cells, top `shard_size` by count then cell id.
        let mut per: Vec<Vec<(i64, u32)>> = vec![Vec::new(); owning.len()];
        let pos: hashbrown::HashMap<u32, usize> = owning.iter().enumerate().map(|(i, &o)| (o, i)).collect();
        for ord in 0..self.ords.len() as u32 {
            let (o, key) = self.ords.key(ord);
            if let Some(&i) = pos.get(&o) {
                per[i].push((key as i64, ord));
            }
        }
        let mut all_ords = Vec::new();
        for list in &mut per {
            list.sort_by(|a, b| grid_cmp(self.counts[a.1 as usize], a.0, self.counts[b.1 as usize], b.0));
            list.truncate(self.shard_size);
            all_ords.extend(list.iter().map(|x| x.1));
        }
        let mut aggs = self.subs.build(&all_ords)?.into_iter();
        let mut out = Vec::with_capacity(owning.len());
        for list in per {
            let buckets = list
                .into_iter()
                .map(|(cell, ord)| GridBucket { cell, doc_count: self.counts[ord as usize], aggs: aggs.next().unwrap_or_default() })
                .collect();
            out.push(AggValue::GeoGrid(Box::new(GeoGrid { kind: self.kind, size: self.size, buckets })));
        }
        Ok(out)
    }

    fn build_empty(&self) -> AggValue {
        self.empty()
    }

    fn needs_scores(&self) -> bool {
        self.subs.needs_scores()
    }

    fn validate_sort(&self, _name: &str, _key: Option<&str>, _rest: &[PathElement]) -> Result<()> {
        Err(AggError::illegal_argument(
            "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.",
        ))
    }
}
