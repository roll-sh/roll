//! Enrich: policies (`_enrich/policy`), their execution into enrich indices, `_enrich/_stats`,
//! and the `enrich` ingest processor.
//!
//! **Policies** live in the cluster metadata, as in Elasticsearch's cluster state. **Executing**
//! one copies the policy's match field and enrich fields of every source document matching its
//! query into a new hidden, write-blocked `.enrich-<policy>-<millis>` index (through the core
//! reindex API, with Elasticsearch's mappings and settings), points the `.enrich-<policy>` alias
//! at it and deletes the previous one.
//!
//! **Lookups:** Elasticsearch's processor runs a search against the enrich index for every
//! document. roll instead loads each enrich index once into an in-memory table ([`Tables`]):
//! a hash map from match value to documents for `match` policies, sorted interval lists for
//! `range` policies (numbers, dates, IPs and CIDRs), bounding boxes for `geo_match` — so a lookup
//! is a hash probe or a binary search, with no search on the write path. Enrich indices are
//! immutable, so a table is valid until the alias moves; [`warm`] (called before pipelines run)
//! reloads tables whose index changed, e.g. after a restart.

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal, store};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const POLICIES: &str = "enrich.policy";
const TYPES: [&str; 3] = ["match", "range", "geo_match"];
const README: &str = "This index is managed by Elasticsearch and should not be modified in any way.";

fn alias_of(name: &str) -> String {
    format!(".enrich-{name}")
}

/// The policy's type and its body.
fn kind_of(p: &Value) -> Option<(&'static str, &Map)> {
    let cfg = p.get("config")?.as_object()?;
    TYPES.iter().find_map(|t| cfg.get(*t).and_then(Value::as_object).map(|b| (*t, b)))
}

// --- policies ----------------------------------------------------------------------------------

pub async fn put_policy(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    validate_name(&name)?;
    let body = ctx.body_map()?.unwrap_or_default();
    if let Some(k) = body.keys().find(|k| !TYPES.contains(&k.as_str())) {
        return Err(EsError::illegal_argument(format!("unsupported policy type [{k}], supported types are [match, geo_match, range]")));
    }
    let Some((ty, cfg)) = TYPES.iter().find_map(|t| body.get(*t).and_then(Value::as_object).map(|b| (*t, b.clone()))) else {
        return Err(EsError::illegal_argument("Required [enrich_fields]"));
    };
    let mut missing = vec![];
    for k in ["indices", "match_field", "enrich_fields"] {
        if !cfg.contains_key(k) {
            missing.push(k);
        }
    }
    if !missing.is_empty() {
        return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
    }
    let as_list = |v: &Value| -> Value {
        match v {
            Value::Arr(a) => Value::Arr(a.clone()),
            other => Value::Arr(other.to_es_string().split(',').map(|s| Value::from(s.trim())).collect()),
        }
    };
    let indices = as_list(&cfg["indices"]);
    let mut out = Map::new();
    out.insert("name".into(), name.clone().into());
    out.insert("indices".into(), indices.clone());
    out.insert("match_field".into(), cfg["match_field"].clone());
    out.insert("enrich_fields".into(), as_list(&cfg["enrich_fields"]));
    if let Some(q) = cfg.get("query") {
        out.insert("query".into(), q.clone());
    }
    if let Some(e) = cfg.get("elasticsearch_version") {
        out.insert("elasticsearch_version".into(), e.clone());
    }
    // Source indices must resolve, like the policy's validation search.
    let names: Vec<String> = indices.as_array().into_iter().flatten().map(Value::to_es_string).collect();
    internal::call(ctx.cluster, "GET", &format!("/{}/_mapping", enc(&names.join(","))), None).await?;
    let mut cfgm = Map::new();
    cfgm.insert(ty.into(), Value::Obj(out));
    let policy = obj! { "config" => Value::Obj(cfgm) };
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(POLICIES, &name).is_some() {
                return Err(EsError::resource_already_exists(format!("policy [{name}] already exists")));
            }
            store::put(st, POLICIES, &name, policy, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

fn validate_name(name: &str) -> EsResult<()> {
    if name.is_empty() {
        return Err(EsError::illegal_argument("Invalid policy name [], must not be empty"));
    }
    if name.contains(',') || name.contains(' ') || name.contains('*') || name.contains('#') {
        return Err(EsError::illegal_argument(format!("Invalid policy name [{name}], must not contain the following characters [ , \", *, \\, <, |, ,, >, /, ?]")));
    }
    if name.starts_with('_') || name.starts_with('-') || name.starts_with('+') {
        return Err(EsError::illegal_argument(format!("Invalid policy name [{name}], must not start with '_', '-', or '+'")));
    }
    if name.chars().any(|c| c.is_uppercase()) {
        return Err(EsError::illegal_argument(format!("Invalid policy name [{name}], must be lowercase")));
    }
    Ok(())
}

pub fn get_policy(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let st = ctx.state();
    let names = ctx.list_param("name").unwrap_or_default();
    let list: Vec<Value> = st
        .metadata
        .xpack
        .iter(POLICIES)
        .filter(|(k, _)| names.is_empty() || names.iter().any(|n| n == "_all" || crate::xcontent::filter::glob_match(n, k)))
        .map(|(_, v)| v.clone())
        .collect();
    Reply::ok(obj! { "policies" => Value::Arr(list) })
}

pub async fn delete_policy(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    let st = ctx.state();
    if st.metadata.xpack.get(POLICIES, &name).is_none() {
        return Err(EsError::resource_not_found(format!("policy [{name}] not found")));
    }
    let using: Vec<String> = st.metadata.pipelines.iter().filter(|(_, p)| references(p, &name)).map(|(id, _)| id.clone()).collect();
    if !using.is_empty() {
        return Err(EsError::new(409, "status_exception", format!("Could not delete policy [{name}] because a pipeline is referencing it [{}]", using.join(", "))));
    }
    let prefix = format!(".enrich-{name}-");
    let indices: Vec<String> = st.metadata.indices.keys().filter(|i| i.starts_with(&prefix)).cloned().collect();
    drop(st);
    if !indices.is_empty() {
        internal::call(ctx.cluster, "DELETE", &format!("/{}?expand_wildcards=all", enc(&indices.join(","))), None).await?;
    }
    ctx.cluster
        .update(move |st| {
            store::remove(st, POLICIES, &name);
            Ok(())
        })
        .await?;
    ctx.cluster.xpack.enrich.tables.write().remove(&ctx.path_param("name").unwrap_or_default().to_string());
    Reply::acknowledged()
}

/// Whether a pipeline definition has an enrich processor using `policy` (at any depth).
fn references(v: &Value, policy: &str) -> bool {
    match v {
        Value::Obj(m) => {
            if let Some(Value::Obj(e)) = m.get("enrich")
                && e.get("policy_name").and_then(Value::as_str) == Some(policy)
            {
                return true;
            }
            m.values().any(|x| references(x, policy))
        }
        Value::Arr(a) => a.iter().any(|x| references(x, policy)),
        _ => false,
    }
}

// --- execution ---------------------------------------------------------------------------------

pub async fn execute_policy(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let name = ctx.path_param("name").unwrap_or_default().to_string();
    let wait = ctx.bool_param("wait_for_completion", true)?;
    let Some(policy) = ctx.state().metadata.xpack.get(POLICIES, &name).cloned() else {
        return Err(EsError::resource_not_found(format!("policy [{name}] does not exist")));
    };
    let task = ctx.cluster.xpack.enrich.executions.fetch_add(1, Ordering::Relaxed) + 1;
    execute(ctx.cluster, &name, &policy, ctx.now_ms).await?;
    if wait {
        Reply::ok(obj! { "status" => obj! { "phase" => "COMPLETE" } })
    } else {
        Reply::ok(obj! { "task" => format!("{}:{}", ctx.cluster.node.id, task) })
    }
}

/// Field types of the source indices for the enrich index's mapping.
async fn source_types(cluster: &Cluster, indices: &str, fields: &[String]) -> EsResult<Map> {
    let uri = format!("/{}/_field_caps?fields={}", enc(indices), enc(&fields.join(",")));
    let caps = internal::call(cluster, "GET", &uri, None).await?;
    let mut out = Map::new();
    if let Some(Value::Obj(f)) = caps.get("fields") {
        for (name, types) in f {
            if let Some(t) = types.as_object().and_then(|t| t.keys().next()) {
                out.insert(name.clone(), Value::from(t.as_str()));
            }
        }
    }
    Ok(out)
}

async fn execute(cluster: &Cluster, name: &str, policy: &Value, now: i64) -> EsResult<()> {
    let Some((ty, cfg)) = kind_of(policy) else { return Err(EsError::illegal_state("invalid policy")) };
    let indices: Vec<String> = cfg.get("indices").and_then(Value::as_array).into_iter().flatten().map(Value::to_es_string).collect();
    let match_field = cfg.get("match_field").map(Value::to_es_string).unwrap_or_default();
    let enrich_fields: Vec<String> = cfg.get("enrich_fields").and_then(Value::as_array).into_iter().flatten().map(Value::to_es_string).collect();
    let mut fields = vec![match_field.clone()];
    fields.extend(enrich_fields.iter().cloned());
    let types = source_types(cluster, &indices.join(","), &fields).await?;
    let mut props = Map::new();
    let match_type = match ty {
        "geo_match" => "geo_shape".to_string(),
        "range" => types.get(&match_field).map(Value::to_es_string).unwrap_or_else(|| "long_range".into()),
        _ => "keyword".to_string(),
    };
    set_prop(&mut props, &match_field, obj! { "type" => match_type.clone(), "doc_values" => false });
    for f in &enrich_fields {
        if f == &match_field {
            continue;
        }
        if let Some(t) = types.get(f) {
            let t = t.to_es_string();
            if matches!(t.as_str(), "object" | "nested") {
                continue;
            }
            set_prop(&mut props, f, obj! { "type" => t, "index" => false });
        }
    }
    let index = format!(".enrich-{name}-{now}");
    let body = obj! {
        "settings" => obj! {
            "index.number_of_shards" => 1, "index.auto_expand_replicas" => "0-all", "index.refresh_interval" => "-1",
            "index.hidden" => true, "index.warmer.enabled" => false,
        },
        "mappings" => obj! {
            "dynamic" => "false",
            "_meta" => obj! { "enrich_policy_name" => name, "enrich_readme" => README, "enrich_policy_type" => ty, "enrich_match_field" => match_field.clone() },
            "properties" => Value::Obj(props),
        },
    };
    internal::call(cluster, "PUT", &format!("/{}", enc(&index)), Some(&body)).await?;
    let mut src = obj! { "index" => Value::Arr(indices.iter().map(|i| Value::from(i.as_str())).collect()), "_source" => Value::Arr(fields.iter().map(|f| Value::from(f.as_str())).collect()) };
    if let (Some(q), Value::Obj(m)) = (cfg.get("query"), &mut src) {
        m.insert("query".into(), q.clone());
    }
    let reindex = obj! { "source" => src, "dest" => obj! { "index" => index.clone() } };
    // Source values the enrich mapping rejects (a `{lat, lon}` point into the `geo_shape`
    // match field) fail the execution as Elasticsearch's `EnrichPolicyRunner` does.
    let copied = internal::call(cluster, "POST", "/_reindex?refresh=true", Some(&reindex)).await;
    if copied.as_ref().map_or(true, |r| r.get("failures").and_then(Value::as_array).is_some_and(|f| !f.is_empty())) {
        let _ = internal::call(cluster, "DELETE", &format!("/{}", enc(&index)), None).await;
        return Err(EsError::new(500, "exception", "Encountered bulk failures during reindex process"));
    }
    internal::call(cluster, "PUT", &format!("/{}/_settings", enc(&index)), Some(&obj! { "index.blocks.write" => true })).await?;
    // Move the alias and drop the previous enrich indices.
    let alias = alias_of(name);
    let prefix = format!(".enrich-{name}-");
    let old: Vec<String> = cluster.state().metadata.indices.keys().filter(|i| i.starts_with(&prefix) && **i != index).cloned().collect();
    let mut actions = vec![obj! { "add" => obj! { "index" => index.clone(), "alias" => alias.clone(), "is_hidden" => true } }];
    for o in &old {
        actions.push(obj! { "remove" => obj! { "index" => o.clone(), "alias" => alias.clone() } });
    }
    let _ = internal::call(cluster, "POST", "/_aliases", Some(&obj! { "actions" => Value::Arr(actions) })).await;
    if !old.is_empty() {
        let _ = internal::call(cluster, "DELETE", &format!("/{}?expand_wildcards=all", enc(&old.join(","))), None).await;
    }
    load(cluster, name, &index).await?;
    Ok(())
}

fn set_prop(props: &mut Map, path: &str, mapping: Value) {
    match path.split_once('.') {
        Some((head, rest)) => {
            let e = props.entry(head.to_string()).or_insert_with(|| obj! { "properties" => obj! {} });
            if let Some(Value::Obj(p)) = e.as_object_mut().and_then(|m| m.get_mut("properties")) {
                set_prop(p, rest, mapping);
            }
        }
        None => {
            props.insert(path.to_string(), mapping);
        }
    }
}

pub fn stats(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let e = &ctx.cluster.xpack.enrich;
    let tables = e.tables.read();
    let count: usize = tables.values().map(|t| t.docs.len()).sum();
    let size: usize = tables.values().map(|t| t.bytes).sum();
    let node = ctx.cluster.node.id.clone();
    Reply::ok(obj! {
        "executing_policies" => Value::Arr(vec![]),
        "coordinator_stats" => crate::arr![obj! {
            "node_id" => node.clone(), "queue_size" => 0, "remote_requests_current" => 0, "remote_requests_total" => 0,
            "executed_searches_total" => e.lookups.load(Ordering::Relaxed) as i64,
        }],
        "cache_stats" => crate::arr![obj! {
            "node_id" => node, "count" => count, "hits" => e.lookups.load(Ordering::Relaxed) as i64, "misses" => 0, "evictions" => 0,
            "hits_time_in_millis" => 0, "misses_time_in_millis" => 0, "size_in_bytes" => size,
        }],
    })
}

// --- lookup tables -----------------------------------------------------------------------------

/// The loaded enrich indices, by policy.
#[derive(Default)]
pub struct Tables {
    pub tables: parking_lot::RwLock<HashMap<String, Arc<Table>>>,
    /// Lookups done by enrich processors.
    pub lookups: AtomicU64,
    executions: AtomicU64,
}

/// One enrich index in memory.
pub struct Table {
    pub index: String,
    pub kind: &'static str,
    pub match_field: String,
    /// The enrich documents (match field and enrich fields, keys sorted).
    pub docs: Vec<Value>,
    /// `match`: documents by match value.
    by_key: HashMap<String, Vec<u32>>,
    /// `range`: intervals `[lo, hi]` sorted by `lo` (numbers, epoch millis or IPs as u128).
    ranges: Vec<(Bound, Bound, u32)>,
    /// `geo_match`: the documents' shapes.
    shapes: Vec<(Geom, u32)>,
    bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
enum Bound {
    Num(f64),
    Ip(u128),
}

fn ip_num(s: &str) -> Option<u128> {
    match s.parse::<std::net::IpAddr>().ok()? {
        std::net::IpAddr::V4(v4) => Some(u128::from(v4.to_ipv6_mapped())),
        std::net::IpAddr::V6(v6) => Some(u128::from(v6)),
    }
}

fn bound_of(v: &Value) -> Option<Bound> {
    match v {
        Value::Num(_) => v.as_f64().map(Bound::Num),
        Value::Str(s) => {
            if let Some(ip) = ip_num(s) {
                return Some(Bound::Ip(ip));
            }
            if let Ok(n) = s.parse::<f64>() {
                return Some(Bound::Num(n));
            }
            crate::time::DateFormatter::new("strict_date_optional_time||epoch_millis").ok()?.parse_millis(s).ok().map(|ms| Bound::Num(ms as f64))
        }
        _ => None,
    }
}

/// `[lo, hi]` of a range value: `{gte|gt, lte|lt}` or a CIDR string.
fn interval(v: &Value) -> Option<(Bound, Bound)> {
    if let Value::Str(s) = v
        && let Some((ip, bits)) = s.split_once('/')
    {
        let bits: u32 = bits.parse().ok()?;
        let addr = ip_num(ip)?;
        let total = if ip.contains(':') { bits } else { bits + 96 };
        let mask = if total == 0 { 0 } else { u128::MAX << (128 - total.min(128)) };
        return Some((Bound::Ip(addr & mask), Bound::Ip((addr & mask) | !mask)));
    }
    let m = v.as_object()?;
    let lo = m.get("gte").or_else(|| m.get("gt")).and_then(bound_of);
    let hi = m.get("lte").or_else(|| m.get("lt")).and_then(bound_of);
    let (lo, hi) = match (lo, hi) {
        (Some(l), Some(h)) => (l, h),
        (Some(l @ Bound::Ip(_)), None) => (l, Bound::Ip(u128::MAX)),
        (None, Some(h @ Bound::Ip(_))) => (Bound::Ip(0), h),
        (Some(l), None) => (l, Bound::Num(f64::INFINITY)),
        (None, Some(h)) => (Bound::Num(f64::NEG_INFINITY), h),
        (None, None) => return None,
    };
    Some((lo, hi))
}

/// A `geo_match` shape in planar `[lon, lat]` coordinates, as its points, lines and polygons
/// (rings, the first the exterior), with its bounding box `(min_lon, min_lat, max_lon,
/// max_lat)` for a cheap first test. Elasticsearch matches with a `geo_shape` query
/// (`INTERSECTS`) against the enrich index; [`Geom::intersects`] is that relation on the
/// in-memory shapes: boundaries count, a shape inside a polygon (or a polygon inside it)
/// intersects without any edges crossing, and a point inside a hole doesn't.
#[derive(Clone, Debug, Default)]
struct Geom {
    points: Vec<[f64; 2]>,
    lines: Vec<Vec<[f64; 2]>>,
    polys: Vec<Vec<Vec<[f64; 2]>>>,
    bbox: [f64; 4],
}

type Pt = [f64; 2];

/// The sign of the turn `a -> b -> c` (0: collinear).
fn orient(a: Pt, b: Pt, c: Pt) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

/// Whether `p`, collinear with `a`-`b`, lies within the segment's box.
fn within_box(a: Pt, b: Pt, p: Pt) -> bool {
    p[0] >= a[0].min(b[0]) && p[0] <= a[0].max(b[0]) && p[1] >= a[1].min(b[1]) && p[1] <= a[1].max(b[1])
}

fn on_segment(a: Pt, b: Pt, p: Pt) -> bool {
    orient(a, b, p) == 0.0 && within_box(a, b, p)
}

/// Whether segments `a`-`b` and `c`-`d` share a point (touching and overlapping count).
fn segments_meet(a: Pt, b: Pt, c: Pt, d: Pt) -> bool {
    let (o1, o2, o3, o4) = (orient(a, b, c), orient(a, b, d), orient(c, d, a), orient(c, d, b));
    if ((o1 > 0.0 && o2 < 0.0) || (o1 < 0.0 && o2 > 0.0)) && ((o3 > 0.0 && o4 < 0.0) || (o3 < 0.0 && o4 > 0.0)) {
        return true;
    }
    (o1 == 0.0 && within_box(a, b, c)) || (o2 == 0.0 && within_box(a, b, d)) || (o3 == 0.0 && within_box(c, d, a)) || (o4 == 0.0 && within_box(c, d, b))
}

/// The segments of a ring or line.
fn segments(r: &[Pt]) -> impl Iterator<Item = (Pt, Pt)> + '_ {
    r.windows(2).map(|w| (w[0], w[1]))
}

/// Even-odd ray casting: strictly inside the ring (the boundary is tested apart).
fn in_ring(r: &[Pt], p: Pt) -> bool {
    let mut inside = false;
    let n = r.len();
    let mut j = n.wrapping_sub(1);
    for i in 0..n {
        let (a, b) = (r[i], r[j]);
        if (a[1] > p[1]) != (b[1] > p[1]) && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0] {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Whether polygon `rings` covers `p`: on any ring's boundary, or inside the exterior and
/// outside every hole.
fn poly_covers(rings: &[Vec<Pt>], p: Pt) -> bool {
    if rings.iter().any(|r| segments(r).any(|(a, b)| on_segment(a, b, p))) {
        return true;
    }
    rings.first().is_some_and(|ext| in_ring(ext, p)) && !rings.iter().skip(1).any(|h| in_ring(h, p))
}

fn line_hits_poly(line: &[Pt], rings: &[Vec<Pt>]) -> bool {
    line.iter().any(|p| poly_covers(rings, *p)) || segments(line).any(|(a, b)| rings.iter().any(|r| segments(r).any(|(c, d)| segments_meet(a, b, c, d))))
}

fn polys_meet(p: &[Vec<Pt>], q: &[Vec<Pt>]) -> bool {
    // Without crossing edges, one polygon holds the other or they are apart.
    p.iter().any(|r| segments(r).any(|(a, b)| q.iter().any(|s| segments(s).any(|(c, d)| segments_meet(a, b, c, d)))))
        || p.first().and_then(|r| r.first()).is_some_and(|v| poly_covers(q, *v))
        || q.first().and_then(|r| r.first()).is_some_and(|v| poly_covers(p, *v))
}

impl Geom {
    fn from_geometry(g: &crate::query::geo::Geometry) -> Geom {
        let mut out = Geom::default();
        out.add(g);
        let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        let mut grow = |p: &Pt| {
            b = [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])];
        };
        out.points.iter().for_each(&mut grow);
        out.lines.iter().flatten().for_each(&mut grow);
        out.polys.iter().flatten().flatten().for_each(&mut grow);
        out.bbox = b;
        out
    }

    fn add(&mut self, g: &crate::query::geo::Geometry) {
        use crate::query::geo::Geometry;
        let pt = |c: &Vec<f64>| -> Option<Pt> { Some([*c.first()?, *c.get(1)?]) };
        let line = |cs: &Vec<Vec<f64>>| -> Vec<Pt> { cs.iter().filter_map(pt).collect() };
        match g {
            Geometry::Point(c) => self.points.extend(pt(c)),
            Geometry::MultiPoint(cs) => self.points.extend(cs.iter().filter_map(pt)),
            Geometry::LineString(cs) => self.lines.push(line(cs)),
            Geometry::MultiLineString(ls) => self.lines.extend(ls.iter().map(line)),
            Geometry::Polygon(rs) => self.polys.push(rs.iter().map(line).collect()),
            Geometry::MultiPolygon(ps) => self.polys.extend(ps.iter().map(|rs| rs.iter().map(line).collect())),
            Geometry::Envelope { min_x, max_x, max_y, min_y } => {
                let rect = |x0: f64, x1: f64| vec![vec![[x0, *min_y], [x1, *min_y], [x1, *max_y], [x0, *max_y], [x0, *min_y]]];
                // An envelope with min_x > max_x crosses the dateline: two boxes.
                if min_x > max_x {
                    self.polys.push(rect(*min_x, 180.0));
                    self.polys.push(rect(-180.0, *max_x));
                } else {
                    self.polys.push(rect(*min_x, *max_x));
                }
            }
            Geometry::Circle { center, radius_meters } => {
                // A 64-gon of the radius in degrees (a circle never reaches a geo_shape index
                // without the circle processor, which does the same).
                if let Some([x, y]) = pt(center) {
                    let r_lat = radius_meters / 111_320.0;
                    let r_lon = r_lat / y.to_radians().cos().max(1e-9);
                    let ring: Vec<Pt> = (0..=64).map(|k| {
                        let t = (k % 64) as f64 * std::f64::consts::TAU / 64.0;
                        [x + r_lon * t.cos(), y + r_lat * t.sin()]
                    }).collect();
                    self.polys.push(vec![ring]);
                }
            }
            Geometry::GeometryCollection(gs) => gs.iter().for_each(|x| self.add(x)),
        }
    }

    /// A shape as an enrich document or input holds it: a point in any of the point forms,
    /// WKT, or GeoJSON.
    fn of_value(v: &Value) -> Option<Geom> {
        if let Some((x, y)) = point_of(v) {
            return Some(Geom { points: vec![[x, y]], bbox: [x, y, x, y], ..Geom::default() });
        }
        let g = match v {
            Value::Str(s) => crate::xpack::esql::spatial::parse(s)?,
            Value::Obj(m) if m.contains_key("type") => crate::query::geo::parse_geojson(m).ok()?,
            _ => return None,
        };
        let out = Geom::from_geometry(&g);
        (out.bbox[0] <= out.bbox[2]).then_some(out)
    }

    fn boxes_meet(&self, o: &Geom) -> bool {
        self.bbox[0] <= o.bbox[2] && o.bbox[0] <= self.bbox[2] && self.bbox[1] <= o.bbox[3] && o.bbox[1] <= self.bbox[3]
    }

    fn covers_point(&self, p: Pt) -> bool {
        self.points.contains(&p) || self.lines.iter().any(|l| l.len() == 1 && l[0] == p || segments(l).any(|(a, b)| on_segment(a, b, p))) || self.polys.iter().any(|r| poly_covers(r, p))
    }

    /// `INTERSECTS`: the shapes share at least one point.
    fn intersects(&self, o: &Geom) -> bool {
        if !self.boxes_meet(o) {
            return false;
        }
        if self.points.iter().any(|p| o.covers_point(*p)) || o.points.iter().any(|p| self.covers_point(*p)) {
            return true;
        }
        let lines_meet = |l: &[Pt], m: &[Pt]| {
            l.iter().any(|p| m.len() == 1 && m[0] == *p) || segments(l).any(|(a, b)| m.iter().any(|q| on_segment(a, b, *q)) || segments(m).any(|(c, d)| segments_meet(a, b, c, d)))
        };
        self.lines.iter().any(|l| o.lines.iter().any(|m| lines_meet(l, m)) || o.polys.iter().any(|p| line_hits_poly(l, p)))
            || o.lines.iter().any(|l| self.polys.iter().any(|p| line_hits_poly(l, p)))
            || self.polys.iter().any(|p| o.polys.iter().any(|q| polys_meet(p, q)))
    }
}

fn point_of(v: &Value) -> Option<(f64, f64)> {
    match v {
        Value::Str(s) => match s.split_once(',') {
            Some((a, b)) if !s.trim_start().starts_with(|c: char| c.is_ascii_alphabetic()) => Some((b.trim().parse().ok()?, a.trim().parse().ok()?)),
            // `POINT (lon lat)`.
            _ => match crate::xpack::esql::spatial::parse(s)? {
                crate::query::geo::Geometry::Point(c) if c.len() >= 2 => Some((c[0], c[1])),
                _ => None,
            },
        },
        Value::Arr(a) if a.len() >= 2 => Some((a[0].as_f64()?, a[1].as_f64()?)),
        Value::Obj(m) => match (m.get("lon"), m.get("lat")) {
            (Some(x), Some(y)) => Some((x.as_f64()?, y.as_f64()?)),
            _ => {
                let c = m.get("coordinates")?.as_array()?;
                Some((c.first()?.as_f64()?, c.get(1)?.as_f64()?))
            }
        },
        _ => None,
    }
}

impl Table {
    fn build(index: String, kind: &'static str, match_field: String, docs: Vec<Value>) -> Table {
        let mut by_key: HashMap<String, Vec<u32>> = HashMap::new();
        let mut ranges = Vec::new();
        let mut shapes = Vec::new();
        let mut bytes = 0;
        for (i, d) in docs.iter().enumerate() {
            bytes += d.to_json().len();
            let Some(v) = field_value(d, &match_field) else { continue };
            let vals: Vec<&Value> = match v {
                // `[lon, lat]` is one point, not two values.
                Value::Arr(a) if kind == "geo_match" && a.len() == 2 && a.iter().all(|x| matches!(x, Value::Num(_))) => vec![v],
                Value::Arr(a) => a.iter().collect(),
                other => vec![other],
            };
            for v in vals {
                match kind {
                    "match" => by_key.entry(v.to_es_string()).or_default().push(i as u32),
                    "range" => {
                        if let Some((lo, hi)) = interval(v) {
                            ranges.push((lo, hi, i as u32));
                        }
                    }
                    _ => {
                        if let Some(g) = Geom::of_value(v) {
                            shapes.push((g, i as u32));
                        }
                    }
                }
            }
        }
        ranges.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        Table { index, kind, match_field, docs, by_key, ranges, shapes, bytes }
    }

    /// Up to `max` matching documents for the input value(s), in index order.
    pub fn lookup(&self, input: &Value, max: usize) -> Vec<&Value> {
        let inputs: Vec<&Value> = match input {
            // `[lon, lat]` is one point (`GeoMatchProcessor`), not two values.
            Value::Arr(a) if self.kind == "geo_match" && a.len() == 2 && a.iter().all(|x| matches!(x, Value::Num(_))) => vec![input],
            Value::Arr(a) => a.iter().collect(),
            other => vec![other],
        };
        let mut hits: Vec<u32> = Vec::new();
        for v in inputs {
            match self.kind {
                "match" => {
                    if let Some(ids) = self.by_key.get(&v.to_es_string()) {
                        hits.extend(ids);
                    }
                }
                "range" => {
                    let Some(x) = bound_of(v) else { continue };
                    // Intervals are sorted by their lower bound: only a prefix can contain x.
                    let end = self.ranges.partition_point(|r| r.0 <= x);
                    hits.extend(self.ranges[..end].iter().filter(|r| r.1 >= x).map(|r| r.2));
                }
                _ => {
                    // A point (ingest documents) or any shape (ES|QL `geo_shape` values).
                    let Some(input) = Geom::of_value(v) else { continue };
                    hits.extend(self.shapes.iter().filter(|(g, _)| g.intersects(&input)).map(|(_, i)| *i));
                }
            }
        }
        hits.sort_unstable();
        hits.dedup();
        hits.into_iter().take(max).map(|i| &self.docs[i as usize]).collect()
    }
}

fn field_value<'a>(d: &'a Value, path: &str) -> Option<&'a Value> {
    if let Some(v) = d.get(path) {
        return Some(v);
    }
    let mut cur = d;
    for p in path.split('.') {
        cur = cur.get(p)?;
    }
    Some(cur)
}

/// Sorts an object's keys recursively (enrich documents come back with sorted fields).
fn sorted(v: Value) -> Value {
    match v {
        Value::Obj(m) => {
            let mut e: Vec<(String, Value)> = m.into_iter().map(|(k, v)| (k, sorted(v))).collect();
            e.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Obj(e.into_iter().collect())
        }
        other => other,
    }
}

/// Reads an enrich index into a table.
async fn load(cluster: &Cluster, policy: &str, index: &str) -> EsResult<()> {
    let st = cluster.state();
    let Some(p) = st.metadata.xpack.get(POLICIES, policy).cloned() else { return Ok(()) };
    drop(st);
    let Some((kind, cfg)) = kind_of(&p) else { return Ok(()) };
    let match_field = cfg.get("match_field").map(Value::to_es_string).unwrap_or_default();
    let mut docs = Vec::new();
    let mut after: Option<Value> = None;
    loop {
        let mut req = obj! { "size" => 10_000, "sort" => crate::arr!["_doc"], "query" => obj! { "match_all" => obj! {} } };
        if let (Some(a), Value::Obj(m)) = (&after, &mut req) {
            m.insert("search_after".into(), a.clone());
        }
        let resp = internal::call(cluster, "POST", &format!("/{}/_search", enc(index)), Some(&req)).await?;
        let hits = resp.pointer(&["hits", "hits"]).and_then(Value::as_array).cloned().unwrap_or_default();
        if hits.is_empty() {
            break;
        }
        after = hits.last().and_then(|h| h.get("sort")).cloned();
        let n = hits.len();
        docs.extend(hits.into_iter().filter_map(|h| h.get("_source").cloned()).map(sorted));
        if n < 10_000 || after.is_none() {
            break;
        }
    }
    let table = Arc::new(Table::build(index.to_string(), kind, match_field, docs));
    cluster.xpack.enrich.tables.write().insert(policy.to_string(), table);
    Ok(())
}

/// The index the `.enrich-<policy>` alias points at.
fn current_index(st: &crate::meta::ClusterState, policy: &str) -> Option<String> {
    let alias = alias_of(policy);
    st.metadata.indices.values().find(|i| i.aliases.contains_key(&alias)).map(|i| i.name.clone())
}

/// Loads the tables of policies whose enrich index changed since they were loaded (cheap when
/// nothing changed: one map read per policy).
pub async fn warm(cluster: &Cluster) {
    let st = cluster.state();
    if st.metadata.xpack.len(POLICIES) == 0 {
        return;
    }
    let mut stale = Vec::new();
    {
        let tables = cluster.xpack.enrich.tables.read();
        for (name, _) in st.metadata.xpack.iter(POLICIES) {
            if let Some(idx) = current_index(&st, name)
                && tables.get(name).is_none_or(|t| t.index != idx)
            {
                stale.push((name.clone(), idx));
            }
        }
    }
    drop(st);
    for (name, idx) in stale {
        let _ = load(cluster, &name, &idx).await;
    }
}

// --- the processor -----------------------------------------------------------------------------

/// A compiled `enrich` processor.
#[derive(Clone, Debug)]
pub struct Processor {
    pub policy: String,
    pub field: String,
    pub target: String,
    pub ignore_missing: bool,
    pub overwrite: bool,
    pub max_matches: usize,
}

/// `EnrichProcessorFactory.create`.
pub fn compile(cluster: &Cluster, cfg: &Map, tag: Option<&str>) -> EsResult<Processor> {
    use crate::ingest::processors::{config_error, creation_exception};
    let ptype = "enrich";
    let s = |k: &str| cfg.get(k).map(Value::to_es_string);
    let Some(policy) = s("policy_name") else { return Err(config_error(ptype, tag, "policy_name", "required property is missing")) };
    let st = cluster.state();
    if current_index(&st, &policy).is_none() {
        return Err(creation_exception(
            ptype,
            tag,
            "java.lang.IllegalArgumentException",
            EsError::illegal_argument(format!("no enrich index exists for policy with name [{policy}]")),
        ));
    }
    let Some(field) = s("field") else { return Err(config_error(ptype, tag, "field", "required property is missing")) };
    let Some(target) = s("target_field") else { return Err(config_error(ptype, tag, "target_field", "required property is missing")) };
    let bool_of = |k: &str, d: bool| match cfg.get(k) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Str(s)) => s == "true",
        _ => d,
    };
    let max_matches = cfg.get("max_matches").and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(1);
    if !(1..=128).contains(&max_matches) {
        return Err(config_error(ptype, tag, "max_matches", "should be between 1 and 128"));
    }
    Ok(Processor { policy, field, target, ignore_missing: bool_of("ignore_missing", false), overwrite: bool_of("override", true), max_matches: max_matches as usize })
}

/// Checks the `enrich` processors of a pipeline definition being stored (anywhere, including
/// `on_failure` and `foreach` bodies): Elasticsearch creates the processors when it stores a
/// pipeline, and an enrich processor fails to create without its policy's enrich index.
pub fn validate_pipeline_refs(cluster: &Cluster, def: &Map) -> EsResult<()> {
    /// One processor (`{type: config}`).
    fn processor(cluster: &Cluster, p: &Value) -> EsResult<()> {
        let Value::Obj(m) = p else { return Ok(()) };
        for (ptype, cfg) in m {
            let Value::Obj(c) = cfg else { continue };
            if ptype == "enrich" {
                compile(cluster, c, c.get("tag").and_then(Value::as_str))?;
            }
            if ptype == "foreach"
                && let Some(inner) = c.get("processor")
            {
                processor(cluster, inner)?;
            }
            if let Some(Value::Arr(of)) = c.get("on_failure") {
                of.iter().try_for_each(|x| processor(cluster, x))?;
            }
        }
        Ok(())
    }
    for k in ["processors", "on_failure"] {
        if let Some(Value::Arr(list)) = def.get(k) {
            list.iter().try_for_each(|x| processor(cluster, x))?;
        }
    }
    Ok(())
}

/// Runs the processor on a document.
pub fn run(cluster: &Cluster, p: &Processor, d: &mut crate::ingest::doc::IngestDoc) -> EsResult<()> {
    let input = match d.get_opt(&p.field) {
        Some(Value::Null) | None if p.ignore_missing => return Ok(()),
        None => {
            d.get(&p.field)?;
            return Ok(());
        }
        Some(v) => v.clone(),
    };
    if !p.overwrite && d.has(&p.target) {
        return Ok(());
    }
    let table = cluster.xpack.enrich.tables.read().get(&p.policy).cloned();
    let Some(table) = table else {
        return Err(EsError::illegal_argument(format!("no enrich index exists for policy with name [{}]", p.policy)));
    };
    cluster.xpack.enrich.lookups.fetch_add(1, Ordering::Relaxed);
    let hits = table.lookup(&input, p.max_matches);
    if hits.is_empty() {
        return Ok(());
    }
    let v = if p.max_matches == 1 { hits[0].clone() } else { Value::Arr(hits.into_iter().cloned().collect()) };
    d.set(&p.target, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_lookup() {
        let docs = vec![obj! { "a" => "A1", "baz" => "k1" }, obj! { "a" => "A2", "baz" => "k2" }, obj! { "a" => "A3", "baz" => "k1" }];
        let t = Table::build("i".into(), "match", "baz".into(), docs);
        assert_eq!(t.lookup(&Value::from("k1"), 1), vec![&obj! { "a" => "A1", "baz" => "k1" }]);
        assert_eq!(t.lookup(&Value::from("k1"), 5).len(), 2);
        assert_eq!(t.lookup(&crate::arr!["k2", "k1"], 5).len(), 3);
        assert!(t.lookup(&Value::from("zz"), 1).is_empty());
    }

    #[test]
    fn range_lookup() {
        let docs = vec![
            obj! { "r" => obj! { "gte" => 1, "lte" => 10 }, "n" => "a" },
            obj! { "r" => obj! { "gte" => 5, "lt" => 20 }, "n" => "b" },
            obj! { "r" => "10.0.0.0/8", "n" => "c" },
            obj! { "r" => obj! { "gte" => "2020-01-01", "lte" => "2020-12-31" }, "n" => "d" },
        ];
        let t = Table::build("i".into(), "range", "r".into(), docs);
        assert_eq!(t.lookup(&Value::from(7), 5).len(), 2);
        assert_eq!(t.lookup(&Value::from(15), 5).len(), 1);
        assert_eq!(t.lookup(&Value::from("10.1.2.3"), 5)[0].get("n"), Some(&Value::from("c")));
        assert!(t.lookup(&Value::from("11.1.2.3"), 5).is_empty());
        assert_eq!(t.lookup(&Value::from("2020-06-01"), 5)[0].get("n"), Some(&Value::from("d")));
    }

    /// Shape inputs (ES|QL `ENRICH ... ON shape`): containment and crossings intersect, as
    /// Elasticsearch's `geo_shape` query finds them.
    #[test]
    fn geo_shape_inputs_intersect() {
        let docs = vec![
            obj! { "g" => "POLYGON ((-50 -50, 50 -50, 50 50, -50 50, -50 -50))", "n" => "big" },
            obj! { "g" => "LINESTRING (100 1, 100 -10)", "n" => "cross" },
            obj! { "g" => "POLYGON ((10 10, 20 10, 20 20, 10 20, 10 10), (12 12, 18 12, 18 18, 12 18, 12 12))", "n" => "ring" },
            obj! { "g" => crate::arr![150.0, 5.0], "n" => "pt" },
        ];
        let t = Table::build("i".into(), "geo_match", "g".into(), docs);
        let names = |input: &str| t.lookup(&Value::from(input), 10).iter().map(|d| d.get("n").and_then(Value::as_str).unwrap_or("").to_string()).collect::<Vec<_>>();
        // Inside the big polygon, no edge crossing.
        assert_eq!(names("POLYGON ((1 1, 2 1, 2 2, 1 2, 1 1))"), ["big"]);
        // Crosses the line, no vertex near it.
        assert_eq!(names("POLYGON ((99 -1, 101 -1, 101 0, 99 0, 99 -1))"), ["cross"]);
        // Holds the point.
        assert_eq!(names("POLYGON ((140 0, 160 0, 160 10, 140 10, 140 0))"), ["pt"]);
        // A polygon in the hole touches nothing of "ring" but lies in "big".
        assert_eq!(names("POLYGON ((14 14, 16 14, 16 16, 14 16, 14 14))"), ["big"]);
        // Points: in the hole, on the hole's boundary, on the line, beside the line's bbox.
        assert_eq!(names("POINT (15 15)"), ["big"]);
        assert_eq!(names("POINT (12 15)"), ["big", "ring"]);
        assert_eq!(names("POINT (100 -5)"), ["cross"]);
        assert!(names("POINT (100.5 -5)").is_empty());
        assert_eq!(names("LINESTRING (60 0, 120 0)"), ["cross"]);
        assert!(names("POLYGON ((200 0, 210 0, 210 10, 200 0))").is_empty());
    }

    #[test]
    fn geo_lookup() {
        let docs = vec![obj! { "g" => obj! { "type" => "envelope", "coordinates" => crate::arr![crate::arr![0.0, 10.0], crate::arr![10.0, 0.0]] }, "n" => "box" }];
        let t = Table::build("i".into(), "geo_match", "g".into(), docs);
        assert_eq!(t.lookup(&Value::from("5,5"), 1).len(), 1);
        assert!(t.lookup(&Value::from("50,5"), 1).is_empty());
        let tri = obj! { "g" => obj! { "type" => "Polygon", "coordinates" => crate::arr![crate::arr![crate::arr![0.0, 0.0], crate::arr![10.0, 0.0], crate::arr![0.0, 10.0], crate::arr![0.0, 0.0]]] } };
        let t = Table::build("i".into(), "geo_match", "g".into(), vec![tri]);
        assert_eq!(t.lookup(&crate::arr![2.0, 2.0], 1).len(), 1);
        assert!(t.lookup(&crate::arr![8.0, 8.0], 1).is_empty());
    }

    #[test]
    fn pipeline_references() {
        let p = obj! { "processors" => crate::arr![obj! { "foreach" => obj! { "processor" => obj! { "enrich" => obj! { "policy_name" => "p" } } } }] };
        assert!(references(&p, "p"));
        assert!(!references(&p, "q"));
    }
}
