//! Geometry, interval, more-like-this and source-confirmed conversions:
//! `geo_shape` / `shape` queries on point and shape fields (inline or indexed shapes),
//! the `intervals` rules analyzed with the field's search analyzer, `more_like_this` over like
//! texts and documents, and `match_only_text` phrase confirmation against `_source`.

use super::query::{QueryCtx, arc, index_err, match_none, qse};
use crate::engine::fields::{FieldInfo, FieldTable, Kind};
use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::query::geo::Geometry;
use crate::query::term_level::Relation;
use search_index::query::geo::{GeoShape, Polygon};
use search_index::query::shape::QueryGeometry;
use search_index::query::{ConstantScoreQuery, ShapeRelation};
use search_index::util::tessellator::GeoPolygon;
use search_index::QueryRef;
use std::sync::Arc;

fn rel(r: Relation) -> ShapeRelation {
    match r {
        Relation::Intersects => ShapeRelation::Intersects,
        Relation::Disjoint => ShapeRelation::Disjoint,
        Relation::Within => ShapeRelation::Within,
        Relation::Contains => ShapeRelation::Contains,
    }
}

fn xy(c: &[f64]) -> (f64, f64) {
    (c.first().copied().unwrap_or(0.0), c.get(1).copied().unwrap_or(0.0))
}

fn ring(r: &[Vec<f64>]) -> Vec<(f64, f64)> {
    r.iter().map(|c| (xy(c).1, xy(c).0)).collect()
}

/// A DSL geometry as point-query shapes (`LatLonGeometry`s).
fn point_shapes(g: &Geometry, out: &mut Vec<GeoShape>) -> EsResult<()> {
    match g {
        Geometry::Point(c) => out.push(GeoShape::Point(xy(c).1, xy(c).0)),
        Geometry::MultiPoint(ps) => ps.iter().for_each(|c| out.push(GeoShape::Point(xy(c).1, xy(c).0))),
        Geometry::LineString(cs) => out.push(GeoShape::Line(ring(cs))),
        Geometry::MultiLineString(ls) => ls.iter().for_each(|l| out.push(GeoShape::Line(ring(l)))),
        Geometry::Polygon(rs) => out.push(GeoShape::Polygon(Polygon { outer: rs.first().map(|r| ring(r)).unwrap_or_default(), holes: rs.iter().skip(1).map(|r| ring(r)).collect() })),
        Geometry::MultiPolygon(ps) => {
            for rs in ps {
                out.push(GeoShape::Polygon(Polygon { outer: rs.first().map(|r| ring(r)).unwrap_or_default(), holes: rs.iter().skip(1).map(|r| ring(r)).collect() }));
            }
        }
        Geometry::Envelope { min_x, max_x, max_y, min_y } => out.push(GeoShape::Rectangle(*min_y, *max_y, *min_x, *max_x)),
        Geometry::Circle { center, radius_meters } => out.push(GeoShape::Circle { lat: xy(center).1, lon: xy(center).0, radius: *radius_meters }),
        Geometry::GeometryCollection(gs) => {
            for g in gs {
                crate::stack::maybe_grow(|| point_shapes(g, out))?;
            }
        }
    }
    Ok(())
}

fn geo_polygon(rs: &[Vec<Vec<f64>>]) -> EsResult<GeoPolygon> {
    let mk = |r: &Vec<Vec<f64>>| -> (Vec<f64>, Vec<f64>) { (r.iter().map(|c| xy(c).1).collect(), r.iter().map(|c| xy(c).0).collect()) };
    let mut holes = Vec::new();
    for h in rs.iter().skip(1) {
        let (la, lo) = mk(h);
        holes.push(GeoPolygon::new(la, lo, Vec::new()).map_err(index_err)?);
    }
    let (la, lo) = mk(rs.first().ok_or_else(|| EsError::illegal_argument("polygon without rings"))?);
    GeoPolygon::new(la, lo, holes).map_err(index_err)
}

/// A DSL geometry as shape-query geometries (`LatLonGeometry`s for `LatLonShape`).
fn shape_geoms(g: &Geometry, out: &mut Vec<QueryGeometry>) -> EsResult<()> {
    match g {
        Geometry::Point(c) => out.push(QueryGeometry::Point { lat: xy(c).1, lon: xy(c).0 }),
        Geometry::MultiPoint(ps) => ps.iter().for_each(|c| out.push(QueryGeometry::Point { lat: xy(c).1, lon: xy(c).0 })),
        Geometry::LineString(cs) => out.push(QueryGeometry::Line { lats: cs.iter().map(|c| xy(c).1).collect(), lons: cs.iter().map(|c| xy(c).0).collect() }),
        Geometry::MultiLineString(ls) => {
            for cs in ls {
                out.push(QueryGeometry::Line { lats: cs.iter().map(|c| xy(c).1).collect(), lons: cs.iter().map(|c| xy(c).0).collect() });
            }
        }
        Geometry::Polygon(rs) => out.push(QueryGeometry::Polygon(geo_polygon(rs)?)),
        Geometry::MultiPolygon(ps) => {
            for rs in ps {
                out.push(QueryGeometry::Polygon(geo_polygon(rs)?));
            }
        }
        Geometry::Envelope { min_x, max_x, max_y, min_y } => out.push(QueryGeometry::Rectangle { min_lat: *min_y, max_lat: *max_y, min_lon: *min_x, max_lon: *max_x }),
        Geometry::Circle { center, radius_meters } => out.push(QueryGeometry::Circle { lat: xy(center).1, lon: xy(center).0, radius: *radius_meters }),
        Geometry::GeometryCollection(gs) => {
            for g in gs {
                crate::stack::maybe_grow(|| shape_geoms(g, out))?;
            }
        }
    }
    Ok(())
}

/// `GeoUtils.quantizeLat`: the latitude a point indexed at `lat` reads back as.
pub fn quantize_lat(lat: f64) -> f64 {
    use search_codec::points::encode;
    encode::decode_latitude(encode::encode_latitude(lat))
}

/// `GeoUtils.quantizeLon`.
pub fn quantize_lon(lon: f64) -> f64 {
    use search_codec::points::encode;
    encode::decode_longitude(encode::encode_longitude(lon))
}

/// Elasticsearch quantizes a query geometry to the index encoding before building the Lucene
/// query (`LuceneGeometriesUtils.toLatLonGeometry(geometry, true, …)`), so a point exactly on a
/// box's lower edge matches (Lucene alone rounds the lower bound up and would miss it).
fn quantize(shape: GeoShape) -> GeoShape {
    let pts = |v: Vec<(f64, f64)>| v.into_iter().map(|(la, lo)| (quantize_lat(la), quantize_lon(lo))).collect::<Vec<_>>();
    match shape {
        GeoShape::Point(la, lo) => GeoShape::Point(quantize_lat(la), quantize_lon(lo)),
        GeoShape::Rectangle(a, b, c, d) => GeoShape::Rectangle(quantize_lat(a), quantize_lat(b), quantize_lon(c), quantize_lon(d)),
        GeoShape::Polygon(p) => GeoShape::Polygon(Polygon { outer: pts(p.outer), holes: p.holes.into_iter().map(pts).collect() }),
        GeoShape::Circle { lat, lon, radius } => GeoShape::Circle { lat: quantize_lat(lat), lon: quantize_lon(lon), radius },
        GeoShape::Line(l) => GeoShape::Line(pts(l)),
    }
}

/// A point field query over shapes (index, doc values, or both).
pub fn point_query(f: &FieldInfo, relation: ShapeRelation, shapes: Vec<GeoShape>, constant: bool) -> EsResult<QueryRef> {
    let shapes: Vec<GeoShape> = shapes.into_iter().map(quantize).collect();
    // Lucene's `LatLonPoint.newGeometryQuery`: one intersecting circle is a distance query,
    // one rectangle a box query (each prints, and a box rewrites, as its own kind).
    let single = (relation == ShapeRelation::Intersects && shapes.len() == 1 && !crate::opensearch::active()).then(|| shapes[0].clone());
    let index = || -> EsResult<QueryRef> {
        Ok(match single {
            Some(GeoShape::Circle { lat, lon, radius }) => arc(search_index::query::geo::GeoDistanceQuery::new(f.number, lat, lon, radius).map_err(index_err)?),
            Some(GeoShape::Rectangle(min_lat, max_lat, min_lon, max_lon)) => search_index::query::geo::bounding_box(f.number, min_lat, max_lat, min_lon, max_lon).map_err(index_err)?,
            _ => arc(search_index::query::GeoPointShapeQuery { field: f.number, shapes: shapes.clone(), relation }),
        })
    };
    let dv = || -> EsResult<QueryRef> { search_index::query::GeoPointDvQuery::geometry(f.number, relation, shapes.clone()).map_err(index_err) };
    let q = match (f.index, f.doc_values) {
        (true, true) => arc(search_index::query::IndexOrDocValuesQuery { index: index()?, dv: dv()? }),
        (true, false) => index()?,
        (false, true) => dv()?,
        _ => return Err(super::query::not_searchable(f)),
    };
    // Elasticsearch's `GeoPointFieldType.geoShapeQuery` returns the point queries bare (they
    // score constantly themselves, and explain through the doc values side); OpenSearch wraps
    // them in a constant score.
    Ok(if constant && crate::opensearch::active() { arc(ConstantScoreQuery::new(q)) } else { q })
}

/// The geometry of an indexed shape (`indexed_shape`): the field at `path` of another document.
fn indexed_shape(ctx: &QueryCtx, id: &str, index: Option<&str>, path: Option<&str>, routing: Option<&str>) -> EsResult<Geometry> {
    let index = index.unwrap_or("shapes");
    let path = path.unwrap_or("shape");
    let st = ctx.st;
    let names = crate::meta::resolve::resolve(st, &[index.to_string()], &crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN, ctx.now_ms)?;
    let Some(idx) = names.first().and_then(|n| st.metadata.indices.get(n)).cloned() else { return Err(EsError::index_not_found(index)) };
    let engine = ctx.cluster.engine.index(ctx.cluster, st, &idx)?;
    let shard = crate::meta::routing::shard_for(&idx, id, routing);
    let doc = engine.shard(shard).shard.get(id.as_bytes(), true).map_err(index_err)?;
    let Some(doc) = doc else { return Err(EsError::resource_not_found(format!("Shape with ID [{id}] not found"))) };
    let src = crate::fetch::source_bytes(&doc.fields).ok_or_else(|| EsError::illegal_state("no source"))?;
    let Value::Obj(m) = crate::fetch::parse_source(src)? else { return Err(EsError::illegal_state("bad source")) };
    let mut vals = Vec::new();
    crate::engine::source::extract_values(&m, path, &mut vals);
    let v = vals.first().ok_or_else(|| EsError::illegal_state(format!("Shape with name [{id}] found but missing {path} field")))?;
    let mut p = crate::query::XParser::from_value(v);
    p.next_token();
    crate::query::geo::parse_geometry(&mut p)
}

/// `geo_shape`.
pub fn geo_shape_query(ctx: &QueryCtx, g: &crate::query::geo::GeoShape) -> EsResult<QueryRef> {
    let Some(f) = ctx.field(&g.field) else {
        if g.ignore_unmapped {
            return Ok(match_none("unmapped"));
        }
        return Err(qse(ctx, format!("failed to find type for field [{}]", g.field)));
    };
    let geom = match &g.shape {
        crate::query::geo::ShapeSource::Inline(geo) => geo.clone(),
        crate::query::geo::ShapeSource::Indexed { id, index, path, routing } => indexed_shape(ctx, id, index.as_deref(), path.as_deref(), routing.as_deref())?,
    };
    let r = rel(g.relation);
    match f.kind {
        Kind::GeoPoint => {
            let mut shapes = Vec::new();
            point_shapes(&geom, &mut shapes)?;
            point_query(&f, r, shapes, true)
        }
        Kind::GeoShape => {
            // `MappedFieldType.failIfNotIndexedNorDocValuesFallback`.
            if !f.index && !f.doc_values {
                return Err(EsError::illegal_argument(format!("Cannot search on field [{}] since it is not indexed nor has doc values.", f.name)));
            }
            let mut gs = Vec::new();
            shape_geoms(&geom, &mut gs)?;
            Ok(arc(ConstantScoreQuery::new(search_index::query::shape::geo_shape_query(f.number, r, gs, f.index, f.doc_values).map_err(index_err)?)))
        }
        _ => Err(qse(ctx, format!("Field [{}] is of unsupported type [{}] for [geo_shape] query", f.name, f.ty))),
    }
}

fn xy_geoms(g: &Geometry, out: &mut Vec<search_index::query::XyGeometry>) -> EsResult<()> {
    use search_index::query::XyGeometry as X;
    match g {
        Geometry::Point(c) => out.push(X::Point { x: xy(c).0 as f32, y: xy(c).1 as f32 }),
        Geometry::MultiPoint(ps) => ps.iter().for_each(|c| out.push(X::Point { x: xy(c).0 as f32, y: xy(c).1 as f32 })),
        Geometry::LineString(cs) => out.push(X::Line { xs: cs.iter().map(|c| xy(c).0 as f32).collect(), ys: cs.iter().map(|c| xy(c).1 as f32).collect() }),
        Geometry::MultiLineString(ls) => {
            for cs in ls {
                out.push(X::Line { xs: cs.iter().map(|c| xy(c).0 as f32).collect(), ys: cs.iter().map(|c| xy(c).1 as f32).collect() });
            }
        }
        Geometry::Polygon(rs) => out.push(X::Polygon(xy_polygon(rs)?)),
        Geometry::MultiPolygon(ps) => {
            for rs in ps {
                out.push(X::Polygon(xy_polygon(rs)?));
            }
        }
        Geometry::Envelope { min_x, max_x, max_y, min_y } => out.push(X::Rectangle { min_x: *min_x as f32, max_x: *max_x as f32, min_y: *min_y as f32, max_y: *max_y as f32 }),
        Geometry::Circle { center, radius_meters } => out.push(X::Circle { x: xy(center).0 as f32, y: xy(center).1 as f32, radius: *radius_meters as f32 }),
        Geometry::GeometryCollection(gs) => {
            for g in gs {
                crate::stack::maybe_grow(|| xy_geoms(g, out))?;
            }
        }
    }
    Ok(())
}

fn xy_polygon(rs: &[Vec<Vec<f64>>]) -> EsResult<GeoPolygon> {
    let mk = |r: &Vec<Vec<f64>>| -> (Vec<f32>, Vec<f32>) { (r.iter().map(|c| xy(c).0 as f32).collect(), r.iter().map(|c| xy(c).1 as f32).collect()) };
    let mut holes = Vec::new();
    for h in rs.iter().skip(1) {
        let (x, y) = mk(h);
        holes.push(GeoPolygon::xy(&x, &y, Vec::new()).map_err(index_err)?);
    }
    let (x, y) = mk(rs.first().ok_or_else(|| EsError::illegal_argument("polygon without rings"))?);
    GeoPolygon::xy(&x, &y, holes).map_err(index_err)
}

/// `shape` (cartesian).
pub fn shape_query(ctx: &QueryCtx, s: &crate::query::geo::Shape) -> EsResult<QueryRef> {
    let Some(f) = ctx.field(&s.field) else {
        if s.ignore_unmapped {
            return Ok(match_none("unmapped"));
        }
        return Err(qse(ctx, format!("failed to find type for field [{}]", s.field)));
    };
    let geom = match &s.shape {
        crate::query::geo::ShapeSource::Inline(geo) => geo.clone(),
        crate::query::geo::ShapeSource::Indexed { id, index, path, routing } => indexed_shape(ctx, id, index.as_deref(), path.as_deref(), routing.as_deref())?,
    };
    let mut gs = Vec::new();
    xy_geoms(&geom, &mut gs)?;
    let r = rel(s.relation);
    let q = match f.kind {
        Kind::Point => search_index::query::xy::point_shape_query(f.number, r, gs, f.index, f.doc_values).map_err(index_err)?,
        Kind::Shape => search_index::query::xy::shape_query(f.number, r, gs, f.index, f.doc_values).map_err(index_err)?,
        _ => return Err(qse(ctx, format!("Field [{}] is of unsupported type [{}] for [shape] query", f.name, f.ty))),
    };
    Ok(arc(ConstantScoreQuery::new(q)))
}

/// `intervals`.
pub fn intervals_query(ctx: &QueryCtx, i: &crate::query::full_text::Intervals) -> EsResult<QueryRef> {
    let Some(f) = ctx.field(&i.field) else { return Ok(match_none("unmapped")) };
    if !f.kind.is_text() {
        return Err(EsError::illegal_argument(format!("Can only use interval queries on text fields - not on [{}] which is of type [{}]", f.name, f.ty)));
    }
    let src = interval_source(ctx, &f, &i.source)?;
    Ok(arc(search_index::query::IntervalQuery::new(f.number, src)))
}

fn analyzer_for(ctx: &QueryCtx, f: &FieldInfo, name: Option<&str>) -> EsResult<search_analysis::Analyzer> {
    match name {
        Some(n) => ctx.table.analysis.analyzer(n).ok_or_else(|| EsError::illegal_argument(format!("analyzer [{n}] not found"))),
        None => f.query_analyzer().cloned().ok_or_else(|| EsError::illegal_argument("no analyzer")),
    }
}

fn interval_source(ctx: &QueryCtx, f: &Arc<FieldInfo>, s: &crate::query::full_text::IntervalsSource) -> EsResult<search_index::query::IntervalsSource> {
    use crate::query::full_text::IntervalsSource as D;
    use search_index::query::IntervalsSource as S;
    let field_of = |use_field: &Option<String>| -> EsResult<Arc<FieldInfo>> {
        match use_field {
            Some(u) => ctx.field(u).ok_or_else(|| EsError::illegal_argument(format!("Cannot create interval source for unmapped field [{u}]"))),
            None => Ok(f.clone()),
        }
    };
    let fix = |uf: &Option<String>, fi: &FieldInfo, s: S| -> S { if uf.is_some() { S::fix_field(fi.number, s) } else { s } };
    Ok(match s {
        D::Match { query, max_gaps, ordered, no_overlap, analyzer, filter, use_field } => {
            let fi = field_of(use_field)?;
            let a = analyzer_for(ctx, &fi, analyzer.as_deref())?;
            let mut toks: Vec<(Vec<u8>, u32, u32)> = Vec::new();
            a.analyze(query, &mut |t| toks.push((t.term.as_bytes().to_vec(), t.position_increment, t.position_length.max(1))));
            let simple = toks.len() > 1 && toks.iter().all(|t| t.1 > 0 && t.2 == 1);
            let src = if *no_overlap && simple {
                // OpenSearch's `IntervalBuilder` for a plain phrase in `unordered_no_overlap`
                // mode (synonyms and graphs fall back to `unordered`).
                let terms = toks.iter().map(|(t, inc, _)| {
                    let term = S::term(t.clone());
                    if *inc > 1 { S::extend(term, *inc as i32 - 1, 0) } else { term }
                });
                no_overlap_sources(terms.collect(), *max_gaps)
            } else {
                S::analyzed(&toks, *max_gaps, *ordered).map_err(index_err)?
            };
            let src = fix(use_field, &fi, src);
            apply_filter(ctx, f, src, filter.as_deref())?
        }
        D::AnyOf { intervals, filter } => {
            let subs: EsResult<Vec<S>> = intervals.iter().map(|x| crate::stack::maybe_grow(|| interval_source(ctx, f, x))).collect();
            apply_filter(ctx, f, S::or(subs?), filter.as_deref())?
        }
        D::AllOf { intervals, ordered, no_overlap, max_gaps, filter } => {
            let subs: Vec<S> = intervals.iter().map(|x| crate::stack::maybe_grow(|| interval_source(ctx, f, x))).collect::<EsResult<_>>()?;
            if *no_overlap && subs.len() > 1 {
                return apply_filter(ctx, f, no_overlap_sources(subs, *max_gaps), filter.as_deref());
            }
            let base = if *ordered { S::ordered(subs) } else { S::unordered(subs) };
            let base = if *max_gaps >= 0 { S::max_gaps(*max_gaps, base) } else { base };
            apply_filter(ctx, f, base, filter.as_deref())?
        }
        D::Prefix { prefix, analyzer, use_field } => {
            let fi = field_of(use_field)?;
            let a = analyzer_for(ctx, &fi, analyzer.as_deref())?;
            let p = a.normalize(prefix);
            let mt = search_index::query::MultiTermQuery::prefix(fi.number, p.clone().into_bytes());
            fix(use_field, &fi, S::multi_term(mt, 128, format!("{p}*")))
        }
        D::Wildcard { pattern, analyzer, use_field, max_expansions } => {
            let fi = field_of(use_field)?;
            let a = analyzer_for(ctx, &fi, analyzer.as_deref())?;
            let p = a.normalize(pattern);
            let mt = search_index::query::MultiTermQuery::wildcard(fi.number, &p, false).map_err(index_err)?;
            fix(use_field, &fi, S::multi_term(mt, max_expansions.map(|m| m.max(1) as usize).unwrap_or(128), p))
        }
        // OpenSearch's regexp rule: the pattern as written (no normalization), its flags and
        // case sensitivity.
        D::Regexp { pattern, use_field, flags: Some(flags), case_insensitive, max_expansions, .. } => {
            let fi = field_of(use_field)?;
            let mt = search_index::query::MultiTermQuery::regexp(fi.number, pattern, crate::query::term_level::regexp_flags::to_codec(*flags), *case_insensitive, 10000)
                .map_err(index_err)?;
            fix(use_field, &fi, S::multi_term(mt, max_expansions.map(|m| m as usize).unwrap_or(128), format!("/{pattern}/")))
        }
        D::Regexp { pattern, analyzer, use_field, .. } => {
            let fi = field_of(use_field)?;
            let a = analyzer_for(ctx, &fi, analyzer.as_deref())?;
            let p = a.normalize(pattern);
            let mt = search_index::query::MultiTermQuery::regexp(fi.number, &p, crate::query::term_level::regexp_flags::to_codec(crate::query::term_level::regexp_flags::LUCENE_ALL), false, 10000).map_err(index_err)?;
            fix(use_field, &fi, S::multi_term(mt, 128, format!("/{p}/")))
        }
        D::Fuzzy { term, prefix_length, transpositions, fuzziness, analyzer, use_field } => {
            let fi = field_of(use_field)?;
            let a = analyzer_for(ctx, &fi, analyzer.as_deref())?;
            let t = a.normalize(term);
            let edits = super::query::fuzzy_edits(fuzziness, &t);
            let mt = search_index::query::MultiTermQuery::fuzzy(fi.number, &t, edits, (*prefix_length).max(0) as usize, 128, *transpositions);
            fix(use_field, &fi, S::multi_term(mt, 128, t))
        }
        D::Range { lower, upper, include_lower, include_upper, analyzer, use_field } => {
            let fi = field_of(use_field)?;
            let a = analyzer_for(ctx, &fi, analyzer.as_deref())?;
            let mt = search_index::query::MultiTermQuery::range(fi.number, Some(a.normalize(lower).into_bytes()), Some(a.normalize(upper).into_bytes()), *include_lower, *include_upper);
            fix(use_field, &fi, S::multi_term(mt, 128, format!("{lower}..{upper}")))
        }
    })
}

/// OpenSearch's `IntervalBuilder.combineSources` in `unordered_no_overlap` mode: the sources
/// folded pairwise with `unordered_no_overlaps`, each inner pair gap-limited.
fn no_overlap_sources(subs: Vec<search_index::query::IntervalsSource>, max_gaps: i32) -> search_index::query::IntervalsSource {
    use search_index::query::IntervalsSource as S;
    let mut it = subs.into_iter();
    let (Some(a), Some(b)) = (it.next(), it.next()) else { unreachable!("callers pass two sources or more") };
    let mut inner = S::unordered_no_overlaps(a, b);
    for next in it {
        let left = if max_gaps == -1 { inner } else { S::max_gaps(max_gaps, inner) };
        inner = S::unordered_no_overlaps(left, next);
    }
    if max_gaps == -1 { inner } else { S::max_gaps(max_gaps, inner) }
}

fn apply_filter(ctx: &QueryCtx, f: &Arc<FieldInfo>, src: search_index::query::IntervalsSource, filter: Option<&crate::query::full_text::IntervalFilter>) -> EsResult<search_index::query::IntervalsSource> {
    use crate::query::full_text::IntervalFilter;
    use search_index::query::IntervalsSource as S;
    let Some(filter) = filter else { return Ok(src) };
    match filter {
        IntervalFilter::Rule { ty, source } => {
            let other = interval_source(ctx, f, source)?;
            Ok(match ty.as_str() {
                "containing" => S::containing(src, other),
                "contained_by" => S::contained_by(src, other),
                "not_containing" => S::not_containing(src, other),
                "not_contained_by" => S::not_contained_by(src, other),
                "overlapping" => S::overlapping(src, other),
                "not_overlapping" => S::non_overlapping(src, other),
                "before" => S::before(src, other),
                "after" => S::after(src, other),
                t => return Err(EsError::illegal_argument(format!("Unknown filter type [{t}]"))),
            })
        }
        IntervalFilter::Script(script) => {
            let bound = ctx.cluster.scripts.compile(ctx.st, script, search_painless::Context::Interval)?;
            let Some(compiled) = bound.painless().cloned() else {
                return Err(EsError::illegal_argument(format!("script_lang not supported [{}]", script.lang.as_deref().unwrap_or("painless"))));
            };
            Ok(S::scripted(src, Arc::new(IntervalPainless::new(compiled, bound.params.clone(), script.id_or_code.clone()))))
        }
    }
}

/// An `IntervalFilterScript`: one Painless instance per query (Elasticsearch creates one with
/// `newInstance()` and calls it for every interval of every segment), behind a lock because
/// segments may be searched concurrently. Its loops poll the search's deadline.
struct IntervalPainless {
    // Field order matters: the instance borrows `script` and must drop first.
    inst: parking_lot::Mutex<(search_painless::Instance<'static>, IntervalHost)>,
    _script: Arc<search_painless::Script>,
    name: String,
}

// SAFETY: the instance (with its reference-counted values) is only reached through the lock,
// so no two threads touch it at once, and nothing it holds escapes a call.
unsafe impl Send for IntervalPainless {}
unsafe impl Sync for IntervalPainless {}

impl std::fmt::Debug for IntervalPainless {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IntervalPainless({})", self.name)
    }
}

/// The interval script's host: no documents, the search's cancellation.
struct IntervalHost(Option<search_index::search::SearchControl>);

impl search_painless::Host for IntervalHost {
    fn check_cancelled(&mut self) -> Result<(), search_painless::HostError> {
        let Some(ctl) = &self.0 else { return Ok(()) };
        ctl.check().map_err(|e| {
            let kind: &'static str = if e.kind() == "task_cancelled_exception" { "task_cancelled_exception" } else { "time_exceeded_exception" };
            let reason = e.to_string();
            search_index::search::note_interrupted(e);
            search_painless::HostError { kind, reason, frames: Vec::new() }
        })
    }
}

impl IntervalPainless {
    fn new(script: Arc<search_painless::Script>, params: serde_json::Value, name: String) -> IntervalPainless {
        // SAFETY: `_script` keeps the script alive for as long as the instance exists.
        let mut inst: search_painless::Instance<'static> = unsafe { std::mem::transmute(script.instance()) };
        inst.set_params(&params);
        inst.set_cancellable(super::scripts::search_scripts_poll());
        let host = IntervalHost(search_index::search::ambient_control());
        IntervalPainless { inst: parking_lot::Mutex::new((inst, host)), _script: script, name }
    }
}

impl search_index::query::IntervalScript for IntervalPainless {
    fn name(&self) -> &str {
        &self.name
    }

    fn accept(&self, start: i32, end: i32, gaps: i32) -> search_index::Result<bool> {
        let mut g = self.inst.lock();
        let (inst, host) = &mut *g;
        inst.set_var("interval", search_painless::es::misc::interval(start, end, gaps));
        inst.execute_bool(host).map_err(super::scripts::script_err)
    }
}

/// `more_like_this` (`MoreLikeThisQueryBuilder.doToQuery`).
pub fn more_like_this(ctx: &QueryCtx, m: &crate::query::specialized::MoreLikeThis) -> EsResult<QueryRef> {
    use search_index::query::mlt::{MoreLikeThis, MoreLikeThisQuery};
    let fields: Vec<Arc<FieldInfo>> = match &m.fields {
        Some(fs) => {
            let mut out = Vec::new();
            for p in fs {
                for n in ctx.table.matching(p) {
                    if let Some(f) = ctx.field(&n)
                        && (f.kind.is_text() || f.kind.is_keyword_like())
                    {
                        out.push(f);
                    } else if m.fail_on_unsupported_field && !p.contains('*') {
                        return Err(EsError::illegal_argument(format!("more_like_this only supports text/keyword fields: [{n}]")));
                    }
                }
            }
            out
        }
        None => super::text::resolve_fields(ctx, &super::text::default_fields(ctx)).into_iter().filter_map(|(n, _)| ctx.field(&n)).filter(|f| f.kind.is_text() || f.kind.is_keyword_like()).collect(),
    };
    if fields.is_empty() {
        return Ok(match_none("more_like_this has no supported fields"));
    }
    let analyzer = |f: &FieldInfo| -> Option<search_analysis::Analyzer> {
        match &m.analyzer {
            Some(n) => ctx.table.analysis.analyzer(n),
            None => f.analyzer.clone().or_else(|| ctx.table.analysis.analyzer("keyword")),
        }
    };
    let mut mlt = MoreLikeThis {
        fields: fields.iter().map(|f| f.number).collect(),
        min_term_freq: m.min_term_freq.max(0) as u32,
        max_query_terms: m.max_query_terms.max(0) as usize,
        min_doc_freq: m.min_doc_freq.max(0) as u64,
        max_doc_freq: if m.max_doc_freq < 0 { u64::MAX } else { m.max_doc_freq as u64 },
        min_word_length: m.min_word_length.max(0) as usize,
        max_word_length: m.max_word_length.max(0) as usize,
        boost_terms: m.boost_terms,
        minimum_should_match: Some(m.minimum_should_match.clone()),
        ..Default::default()
    };
    if let Some(sw) = &m.stop_words {
        mlt.stop_words = sw.iter().map(|s| s.as_bytes().to_vec()).collect();
    }
    let first = analyzer(&fields[0]);
    let like_texts: Vec<Vec<Vec<u8>>> = m
        .like_texts
        .iter()
        .map(|t| {
            let mut out = Vec::new();
            if let Some(a) = &first {
                a.analyze(t, &mut |tok| out.push(tok.term.as_bytes().to_vec()));
            }
            out
        })
        .collect();
    let mut like_docs = Vec::new();
    let mut exclude_ids: Vec<String> = Vec::new();
    for item in &m.like_items {
        let doc_fields: Option<Value> = match (&item.doc, &item.id) {
            (Some(d), _) => Some(d.clone()),
            (None, Some(id)) => {
                // `handleExclude`: every like document's id is excluded, whatever its index.
                if !m.include {
                    exclude_ids.push(id.clone());
                }
                let st = ctx.st;
                match like_item_index(ctx, item.index.as_deref()) {
                    Some(idx) => {
                        let engine = ctx.cluster.engine.index(ctx.cluster, st, &idx)?;
                        let sh = crate::meta::routing::shard_for(&idx, id, item.routing.as_deref());
                        engine.shard(sh).shard.get(id.as_bytes(), true).map_err(index_err)?.and_then(|d| crate::fetch::source_bytes(&d.fields).map(crate::fetch::parse_source)).transpose()?
                    }
                    None => None,
                }
            }
            _ => None,
        };
        let Some(Value::Obj(src)) = doc_fields else { continue };
        let mut tv = Vec::new();
        for f in &fields {
            let mut vals = Vec::new();
            crate::engine::source::extract_values(&src, f.parent.as_deref().unwrap_or(&f.name), &mut vals);
            let mut freqs: std::collections::BTreeMap<Vec<u8>, u32> = std::collections::BTreeMap::new();
            if let Some(a) = analyzer(f) {
                for v in vals {
                    a.analyze(&super::query::value_text(v), &mut |tok| *freqs.entry(tok.term.as_bytes().to_vec()).or_insert(0) += 1);
                }
            }
            if !freqs.is_empty() {
                tv.push((f.number, freqs.into_iter().collect()));
            }
        }
        like_docs.push(tv);
    }
    // `unlike`: the terms of unlike texts (analyzed for every field) and of unlike documents are
    // never selected (`XMoreLikeThis.setSkipTerms`).
    let mut skip: std::collections::HashSet<(u32, Vec<u8>)> = std::collections::HashSet::new();
    for t in &m.unlike_texts {
        for f in &fields {
            if let Some(a) = analyzer(f) {
                a.analyze(t, &mut |tok| {
                    skip.insert((f.number, tok.term.as_bytes().to_vec()));
                });
            }
        }
    }
    for item in &m.unlike_items {
        let doc_fields: Option<Value> = match (&item.doc, &item.id) {
            (Some(d), _) => Some(d.clone()),
            (None, Some(id)) => {
                match like_item_index(ctx, item.index.as_deref()) {
                    Some(idx) => {
                        let engine = ctx.cluster.engine.index(ctx.cluster, ctx.st, &idx)?;
                        let sh = crate::meta::routing::shard_for(&idx, id, item.routing.as_deref());
                        engine.shard(sh).shard.get(id.as_bytes(), true).map_err(index_err)?.and_then(|d| crate::fetch::source_bytes(&d.fields).map(crate::fetch::parse_source)).transpose()?
                    }
                    None => None,
                }
            }
            _ => None,
        };
        let Some(Value::Obj(src)) = doc_fields else { continue };
        for f in &fields {
            let mut vals = Vec::new();
            crate::engine::source::extract_values(&src, f.parent.as_deref().unwrap_or(&f.name), &mut vals);
            if let Some(a) = analyzer(f) {
                for v in vals {
                    a.analyze(&super::query::value_text(v), &mut |tok| {
                        skip.insert((f.number, tok.term.as_bytes().to_vec()));
                    });
                }
            }
        }
    }
    let q = MoreLikeThisQuery { mlt, like_texts, like_docs, skip, description: "like:[...]".into(), ..Default::default() };
    let base: QueryRef = arc(q);
    if exclude_ids.is_empty() {
        return Ok(base);
    }
    Ok(arc(search_index::query::BooleanQuery::new().add(search_index::query::Occur::Must, base).add(search_index::query::Occur::MustNot, super::query::ids_query(&exclude_ids))))
}

struct SourceText {
    table: Arc<FieldTable>,
    path: String,
}

struct SourceTextLeaf {
    seg: search_index::SegmentContext,
    path: String,
}

impl search_index::query::SourceValues for SourceText {
    fn leaf(&self, seg: &search_index::SegmentContext) -> search_index::Result<Box<dyn search_index::query::LeafSourceValues>> {
        let _ = &self.table;
        Ok(Box::new(SourceTextLeaf { seg: seg.clone(), path: self.path.clone() }))
    }
}

impl search_index::query::LeafSourceValues for SourceTextLeaf {
    fn values(&mut self, doc: u32) -> search_index::Result<Vec<String>> {
        self.seg.ensure_stored()?;
        let stored = self.seg.reader.stored().document(doc)?;
        let Some(b) = crate::fetch::source_bytes(&stored) else { return Ok(Vec::new()) };
        let Ok(Some(Value::Obj(m))) = crate::json::parse(b) else { return Ok(Vec::new()) };
        let mut vals = Vec::new();
        crate::engine::source::extract_values(&m, &self.path, &mut vals);
        Ok(vals.into_iter().filter(|v| !v.is_null()).map(super::query::value_text).collect())
    }
}

/// `match_only_text` phrase queries confirmed against `_source`.
/// The index a like/unlike document is read from: its `_index` (an index or an alias of one
/// index, as for a multi term vectors item) or the searched index. An item that can't be
/// resolved is skipped, as Elasticsearch skips failed term vectors responses.
fn like_item_index(ctx: &QueryCtx, index: Option<&str>) -> Option<std::sync::Arc<crate::meta::IndexMeta>> {
    match index {
        None => ctx.st.metadata.indices.get(&ctx.idx.name).cloned(),
        Some(name) => crate::api::analyze::concrete_single_index(ctx.st, name, ctx.now_ms).ok().cloned(),
    }
}

pub fn source_confirmed(ctx: &QueryCtx, f: &FieldInfo, q: QueryRef) -> QueryRef {
    let Some(a) = f.analyzer.clone() else { return q };
    let src = Arc::new(SourceText { table: ctx.table.clone(), path: f.name.clone() });
    arc(ConstantScoreQuery::new(arc(search_index::query::SourceConfirmedQuery::new(q, f.number, a, src))))
}
