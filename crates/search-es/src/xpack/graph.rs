//! Graph explore (`<index>/_graph/explore`): the terms that connect the documents a query
//! matches, hop by hop, computed like Elasticsearch's `TransportGraphExploreAction`.
//!
//! The request parses like `RestGraphAction` (its error texts included) into hops: the root
//! hop, then one per nested `connections`; a hop without `vertices` inherits its parent's.
//! Every hop is one search whose `sampler` (or `diversified_sampler`) aggregation holds one
//! `terms` (or, with `use_significance`, `significant_terms`) aggregation per vertex field,
//! named `field<i>` like Elasticsearch's (their validation errors name them):
//!
//! - the root hop's terms weigh their share of the summed scores (doc counts) of all fields;
//! - a later hop's search matches the previous hop's vertices (constant-score term queries
//!   boosted by their weights), aggregates each previous field's vertex terms and, under each,
//!   the next fields' terms. A connection carries `score / total * min(0.95, source weight)`
//!   (the total skips self joins), a vertex the sum of what reaches it; a hop then keeps the
//!   strongest `size` new vertices per field (Lucene's `PriorityQueue`, ties included) and
//!   drops the others with their connections.
//!
//! The response lists vertices and connections in the iteration order of the Java `HashMap`s
//! Elasticsearch keeps them in, so it matches byte for byte.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal};
use std::collections::HashMap;

/// `BooleanQuery` clause limit past which a hop matches its sources with `terms` queries.
const MAX_CLAUSES: usize = 1024;

/// A vertex request (`VertexRequest`, with `RestGraphAction`'s defaults).
#[derive(Clone, Debug)]
struct VertexReq {
    field: String,
    size: i64,
    min_doc_count: i64,
    shard_min_doc_count: i64,
    /// Terms and boosts, keyed by term (a later duplicate replaces the earlier boost).
    include: Option<Vec<(String, f64)>>,
    exclude: Option<Vec<String>>,
}

impl VertexReq {
    /// `VertexRequest.shardMinDocCount()`: never above `min_doc_count`.
    fn shard_min(&self) -> i64 {
        self.shard_min_doc_count.min(self.min_doc_count)
    }
}

#[derive(Debug, Default)]
struct Hop {
    vertices: Option<Vec<VertexReq>>,
    query: Option<Value>,
}

#[derive(Debug)]
struct Request {
    hops: Vec<Hop>,
    sample_size: i64,
    timeout_ms: Option<i64>,
    use_significance: bool,
    detailed: bool,
    diversity_field: Option<String>,
    max_docs_per_value: i64,
}

impl Request {
    /// A hop's vertex requests, inherited from its parent when it has none.
    fn effective(&self, hop: usize) -> &[VertexReq] {
        (0..=hop).rev().find_map(|h| self.hops[h].vertices.as_deref()).unwrap_or(&[])
    }
}

fn parse_err(msg: impl Into<String>) -> EsError {
    EsError::parse(msg)
}

/// `parser.intValue()` of a number.
fn int_of(v: &Value) -> i64 {
    v.as_f64().map(|f| f as i64).unwrap_or(0)
}

/// `RestGraphAction.parseHop`.
fn parse_hop(m: &Map, depth: usize, req: &mut Request) -> EsResult<()> {
    let idx = req.hops.len();
    req.hops.push(Hop::default());
    for (name, v) in m {
        match v {
            Value::Arr(items) => {
                if name == "vertices" {
                    let list = parse_vertices(items)?;
                    req.hops[idx].vertices.get_or_insert_with(Vec::new).extend(list);
                } else if let Some(first) = items.first() {
                    // Elasticsearch doesn't consume other arrays: it reads their elements as
                    // properties of the hop.
                    if !matches!(first, Value::Obj(_) | Value::Arr(_)) {
                        return Err(parse_err(format!("Illegal property in graph definition {name}")));
                    }
                }
            }
            Value::Obj(o) => match name.as_str() {
                "query" => req.hops[idx].query = Some(v.clone()),
                "connections" => parse_hop(o, depth + 1, req)?,
                "controls" => {
                    if depth > 0 {
                        return Err(parse_err(format!("Controls are a global setting that can only be set in the root {name}")));
                    }
                    parse_controls(o, req)?;
                }
                _ => return Err(parse_err(format!("Illegal object property in graph definition {name}"))),
            },
            _ => return Err(parse_err(format!("Illegal property in graph definition {name}"))),
        }
    }
    Ok(())
}

/// `RestGraphAction.parseVertices`.
fn parse_vertices(items: &[Value]) -> EsResult<Vec<VertexReq>> {
    let mut out = Vec::new();
    for item in items {
        let Value::Obj(m) = item else { continue };
        let mut vr = VertexReq { field: String::new(), size: 10, min_doc_count: 3, shard_min_doc_count: 2, include: None, exclude: None };
        let mut field = None;
        for (name, v) in m {
            match v {
                Value::Arr(list) => match name.as_str() {
                    "include" => {
                        if vr.exclude.is_some() {
                            return Err(parse_err("Graph vertices definition cannot contain both include and exclude clauses"));
                        }
                        let mut inc: Vec<(String, f64)> = Vec::new();
                        for t in list {
                            let (term, boost) = match t {
                                Value::Obj(o) => {
                                    let mut term = None;
                                    let mut boost = 1.0f64;
                                    for (k, x) in o {
                                        match x {
                                            Value::Str(s) if k == "term" => term = Some(s.clone()),
                                            Value::Num(_) if k == "boost" => boost = f64::from(x.as_f64().unwrap_or(1.0) as f32),
                                            Value::Str(_) | Value::Num(_) => {
                                                return Err(parse_err(format!("Graph vertices definition include clause has invalid property:{k}")));
                                            }
                                            other => {
                                                return Err(parse_err(format!("Graph vertices definition include clause has invalid property type:{}", token_name(other))));
                                            }
                                        }
                                    }
                                    let Some(term) = term else {
                                        return Err(parse_err("Graph vertices definition include clause has missing object property for term"));
                                    };
                                    (term, boost)
                                }
                                Value::Str(s) => (s.clone(), 1.0),
                                other => {
                                    return Err(parse_err(format!(
                                        "Graph vertices definition include clauses must be string terms or Objects with terms and boosts, not{}",
                                        token_name(other)
                                    )));
                                }
                            };
                            match inc.iter_mut().find(|(t, _)| *t == term) {
                                Some(e) => e.1 = boost,
                                None => inc.push((term, boost)),
                            }
                        }
                        vr.include = Some(inc);
                    }
                    "exclude" => {
                        if vr.include.is_some() {
                            return Err(parse_err("Graph vertices definition cannot contain both include and exclude clauses"));
                        }
                        let mut exc: Vec<String> = list.iter().map(Value::to_es_string).collect();
                        exc.sort();
                        exc.dedup();
                        vr.exclude = Some(exc);
                    }
                    _ => return Err(parse_err(format!("Illegal property in graph vertices definition {name}"))),
                },
                Value::Str(s) => {
                    if name != "field" {
                        return Err(parse_err(format!("Unknown string property: [{name}]")));
                    }
                    field = Some(s.clone());
                }
                Value::Num(_) => match name.as_str() {
                    "size" => vr.size = int_of(v),
                    "min_doc_count" => vr.min_doc_count = int_of(v),
                    "shard_min_doc_count" => vr.shard_min_doc_count = int_of(v),
                    _ => return Err(parse_err(format!("Unknown numeric property: [{name}]"))),
                },
                _ => {}
            }
        }
        let Some(field) = field else { return Err(parse_err("Missing field name in graph vertices definition")) };
        vr.field = field;
        out.push(vr);
    }
    Ok(out)
}

fn token_name(v: &Value) -> &'static str {
    match v {
        Value::Obj(_) => "START_OBJECT",
        Value::Arr(_) => "START_ARRAY",
        Value::Str(_) => "VALUE_STRING",
        Value::Num(_) => "VALUE_NUMBER",
        Value::Bool(_) => "VALUE_BOOLEAN",
        _ => "VALUE_NULL",
    }
}

/// `RestGraphAction.parseControls`.
fn parse_controls(m: &Map, req: &mut Request) -> EsResult<()> {
    for (name, v) in m {
        match v {
            Value::Num(_) => match name.as_str() {
                "sample_size" => req.sample_size = int_of(v),
                "timeout" => req.timeout_ms = Some(int_of(v)),
                _ => return Err(parse_err(format!("Unknown numeric property: [{name}]"))),
            },
            Value::Bool(b) => match name.as_str() {
                "use_significance" => req.use_significance = *b,
                "return_detailed_stats" => req.detailed = *b,
                _ => return Err(parse_err(format!("Unknown boolean property: [{name}]"))),
            },
            Value::Str(s) => {
                if name != "timeout" {
                    return Err(parse_err(format!("Unknown numeric property: [{name}]")));
                }
                req.timeout_ms = Some(crate::units::TimeValue::parse(s, "timeout")?.as_millis());
            }
            Value::Obj(o) if name == "sample_diversity" => {
                for (k, x) in o {
                    match k.as_str() {
                        "field" => req.diversity_field = Some(x.to_es_string()),
                        "max_docs_per_value" => req.max_docs_per_value = int_of(x),
                        _ => return Err(parse_err(format!("Unknown property: [{k}]"))),
                    }
                }
            }
            _ => return Err(parse_err(format!("Unknown object property: [{name}]"))),
        }
    }
    Ok(())
}

/// The checks `TermsAggregationBuilder` / `SignificantTermsAggregationBuilder` setters make, in
/// the order Elasticsearch calls them (`min_first`: `minDocCount` before `shardMinDocCount`).
fn check_agg(name: &str, vr: &VertexReq, size: i64, counts: bool, min_first: bool) -> EsResult<()> {
    let min = |v: i64, what: &str| {
        if v < 0 {
            return Err(EsError::illegal_argument(format!("[{what}] must be greater than or equal to 0. Found [{v}] in [{name}]")));
        }
        Ok(())
    };
    if counts {
        if min_first {
            min(vr.min_doc_count, "minDocCount")?;
            min(vr.shard_min(), "shardMinDocCount")?;
        } else {
            min(vr.shard_min(), "shardMinDocCount")?;
            min(vr.min_doc_count, "minDocCount")?;
        }
    }
    if size <= 0 {
        return Err(EsError::illegal_argument(format!("[size] must be greater than 0. Found [{size}] in [{name}]")));
    }
    Ok(())
}

fn sorted_includes(vr: &VertexReq) -> Option<Vec<Value>> {
    vr.include.as_ref().map(|inc| {
        let mut t: Vec<&str> = inc.iter().map(|(t, _)| t.as_str()).collect();
        t.sort_unstable();
        t.into_iter().map(Value::from).collect()
    })
}

/// A `terms` / `significant_terms` aggregation for a vertex request.
fn vertex_agg(vr: &VertexReq, significance: bool, size: i64, counts: bool, include_size: bool) -> Value {
    let mut m = Map::new();
    m.insert("field".into(), vr.field.clone().into());
    if counts {
        m.insert("min_doc_count".into(), vr.min_doc_count.into());
        m.insert("shard_min_doc_count".into(), vr.shard_min().into());
    }
    m.insert("execution_hint".into(), "map".into());
    let mut size = size;
    if let Some(inc) = sorted_includes(vr) {
        if include_size {
            size = inc.len() as i64;
        }
        m.insert("include".into(), Value::Arr(inc));
    } else if let Some(exc) = &vr.exclude {
        m.insert("exclude".into(), Value::Arr(exc.iter().map(|s| Value::from(s.as_str())).collect()));
    }
    m.insert("size".into(), size.into());
    let mut a = Map::new();
    a.insert(if significance { "significant_terms" } else { "terms" }.into(), Value::Obj(m));
    Value::Obj(a)
}

/// User `include` terms of a hop's vertex requests, as `should` clauses with boosts
/// normalized to start from 1.
fn include_clauses(vrs: &[VertexReq], should: &mut Vec<Value>) {
    for vr in vrs {
        let Some(inc) = &vr.include else { continue };
        if should.len() + inc.len() > MAX_CLAUSES {
            let terms: Vec<Value> = inc.iter().map(|(t, _)| Value::from(t.as_str())).collect();
            let mut t = Map::new();
            t.insert(vr.field.clone(), Value::Arr(terms));
            should.push(obj! { "constant_score" => obj! { "filter" => obj! { "terms" => Value::Obj(t) } } });
            continue;
        }
        let min = inc.iter().map(|(_, b)| *b).fold(f64::MAX, f64::min);
        for (term, boost) in inc {
            let mut t = Map::new();
            t.insert(vr.field.clone(), obj! { "value" => term.clone(), "boost" => f64::from((*boost / min) as f32) });
            should.push(obj! { "term" => Value::Obj(t) });
        }
    }
}

#[derive(Clone, Debug)]
struct Vertex {
    field: String,
    term: String,
    weight: f64,
    depth: usize,
    bg: i64,
    fg: i64,
}

#[derive(Clone, Debug)]
struct Connection {
    from: usize,
    to: usize,
    weight: f64,
    doc_count: i64,
}

/// The graph as Elasticsearch's `HashMap`s hold it: entries in insertion order (removed ones
/// become `None`), the key index, and the most entries held at once (the table's capacity).
#[derive(Default)]
struct Graph {
    vertices: Vec<Option<Vertex>>,
    vertex_index: HashMap<(String, String), usize>,
    vertex_peak: usize,
    connections: Vec<Option<Connection>>,
    connection_index: HashMap<(usize, usize), usize>,
    connection_peak: usize,
    /// Vertices found by each hop, by field (`hopFindings`).
    findings: Vec<HashMap<String, Vec<usize>>>,
}

impl Graph {
    fn get(&self, field: &str, term: &str) -> Option<usize> {
        self.vertex_index.get(&(field.to_string(), term.to_string())).copied()
    }

    /// `addVertex`: the existing vertex, or a new one recorded as a finding of `depth`.
    fn add_vertex(&mut self, field: &str, term: &str, weight: f64, depth: usize, bg: i64, fg: i64) -> (usize, bool) {
        if let Some(i) = self.get(field, term) {
            return (i, false);
        }
        let i = self.vertices.len();
        self.vertices.push(Some(Vertex { field: field.to_string(), term: term.to_string(), weight, depth, bg, fg }));
        self.vertex_index.insert((field.to_string(), term.to_string()), i);
        self.vertex_peak = self.vertex_peak.max(self.vertex_index.len());
        while self.findings.len() <= depth {
            self.findings.push(HashMap::new());
        }
        self.findings[depth].entry(field.to_string()).or_default().push(i);
        (i, true)
    }

    fn remove_vertex(&mut self, i: usize, depth: usize) {
        if let Some(v) = self.vertices[i].take() {
            self.vertex_index.remove(&(v.field.clone(), v.term.clone()));
            if let Some(list) = self.findings.get_mut(depth).and_then(|f| f.get_mut(&v.field)) {
                list.retain(|x| *x != i);
            }
        }
    }

    fn add_connection(&mut self, from: usize, to: usize, weight: f64, doc_count: i64) -> usize {
        let c = Connection { from, to, weight, doc_count };
        match self.connection_index.get(&(from, to)) {
            Some(&i) => {
                self.connections[i] = Some(c);
                i
            }
            None => {
                let i = self.connections.len();
                self.connections.push(Some(c));
                self.connection_index.insert((from, to), i);
                self.connection_peak = self.connection_peak.max(self.connection_index.len());
                i
            }
        }
    }

    fn remove_connection(&mut self, i: usize) {
        if let Some(c) = self.connections[i].take() {
            self.connection_index.remove(&(c.from, c.to));
        }
    }

    fn vertex(&self, i: usize) -> &Vertex {
        self.vertices[i].as_ref().expect("live vertex")
    }
}

/// The documents of a hop: `sampler` or `diversified_sampler` over `aggs`.
fn sample_agg(req: &Request, aggs: Map) -> Value {
    match &req.diversity_field {
        Some(f) => {
            let mut d = obj! { "field" => f.clone(), "shard_size" => req.sample_size };
            if req.max_docs_per_value > 0
                && let Value::Obj(m) = &mut d
            {
                m.insert("max_docs_per_value".into(), req.max_docs_per_value.into());
            }
            obj! { "diversified_sampler" => d, "aggs" => Value::Obj(aggs) }
        }
        None => obj! { "sampler" => obj! { "shard_size" => req.sample_size }, "aggs" => Value::Obj(aggs) },
    }
}

fn search_body(req: &Request, query: Value, aggs: Map) -> Value {
    let mut body = obj! { "size" => 0, "query" => query, "aggs" => obj! { "sample" => sample_agg(req, aggs) } };
    if let (Some(t), Value::Obj(m)) = (req.timeout_ms, &mut body) {
        m.insert("timeout".into(), format!("{t}ms").into());
    }
    body
}

fn buckets(v: Option<&Value>) -> &[Value] {
    v.and_then(|a| a.get("buckets")).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn key_string(b: &Value) -> String {
    b.get("key_as_string").or_else(|| b.get("key")).map(Value::to_es_string).unwrap_or_default()
}

/// A bucket's signal: its significance score or its doc count.
fn signal(b: &Value, significance: bool) -> f64 {
    if significance { b.get("score").and_then(Value::as_f64).unwrap_or(0.0) } else { b.get("doc_count").and_then(Value::as_f64).unwrap_or(0.0) }
}

pub async fn explore(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let started = std::time::Instant::now();
    let Some(body) = ctx.body()? else { return Err(parse_err("Body missing for graph request")) };
    let Value::Obj(body) = body else {
        return Err(parse_err("failed to parse search source. source must be an object, but found [{}] instead"));
    };
    let mut req = Request {
        hops: Vec::new(),
        sample_size: 100,
        timeout_ms: None,
        use_significance: true,
        detailed: false,
        diversity_field: None,
        max_docs_per_value: 0,
    };
    if let Some(t) = ctx.query("timeout") {
        req.timeout_ms = Some(crate::units::TimeValue::parse(t, "timeout")?.as_millis());
    }
    parse_hop(&body, 0, &mut req)?;
    let errors: Vec<String> =
        (0..req.hops.len()).filter(|h| req.effective(*h).is_empty()).map(|_| "Graph explore hop must have at least one VertexRequest".to_string()).collect();
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    let index = ctx.path_param("index").unwrap_or("_all").to_string();
    let mut uri = format!("/{}/_search?allow_no_indices=false", enc(&index));
    for p in ["routing", "ignore_unavailable", "expand_wildcards"] {
        if let Some(v) = ctx.query(p) {
            uri.push_str(&format!("&{p}={}", enc(v)));
        }
    }
    if let Some(v) = ctx.query("allow_no_indices") {
        uri = uri.replacen("allow_no_indices=false", &format!("allow_no_indices={}", enc(v)), 1);
    }
    let sig = req.use_significance;
    let mut graph = Graph::default();
    let mut timed_out = false;
    let mut failures: Vec<Value> = Vec::new();
    let mut note = |resp: &Value| {
        timed_out |= resp.get("timed_out").and_then(Value::as_bool).unwrap_or(false);
        if let Some(Value::Arr(f)) = resp.pointer(&["_shards", "failures"]) {
            failures.extend(f.iter().cloned());
        }
    };

    // The root hop.
    let root = req.effective(0).to_vec();
    let mut aggs = Map::new();
    for (i, vr) in root.iter().enumerate() {
        let name = format!("field{i}");
        check_agg(&name, vr, vr.size, sig, false)?;
        aggs.insert(name, vertex_agg(vr, sig, vr.size, sig, true));
    }
    let mut must = vec![req.hops[0].query.clone().unwrap_or_else(|| obj! { "match_all" => obj! {} })];
    let mut includes = Vec::new();
    include_clauses(&root, &mut includes);
    if !includes.is_empty() {
        must.push(obj! { "bool" => obj! { "should" => Value::Arr(includes) } });
    }
    let resp = internal::call(ctx.cluster, "POST", &uri, Some(&search_body(&req, obj! { "bool" => obj! { "must" => Value::Arr(must) } }, aggs))).await?;
    note(&resp);
    let sample = resp.pointer(&["aggregations", "sample"]).cloned().unwrap_or(Value::Null);
    let total: f64 = (0..root.len()).flat_map(|i| buckets(sample.get(&format!("field{i}")))).map(|b| signal(b, sig)).sum();
    for (j, vr) in root.iter().enumerate() {
        for b in buckets(sample.get(&format!("field{j}"))) {
            let (bg, fg) = if sig {
                (b.get("bg_count").and_then(Value::as_i64).unwrap_or(0), b.get("doc_count").and_then(Value::as_i64).unwrap_or(0))
            } else {
                (0, 0)
            };
            graph.add_vertex(&vr.field, &key_string(b), signal(b, sig) / total, 0, bg, fg);
        }
    }

    // Later hops.
    let mut hop = 0;
    while hop + 1 < req.hops.len() && graph.findings.get(hop).is_some_and(|f| f.values().any(|l| !l.is_empty())) {
        let last = req.effective(hop).to_vec();
        hop += 1;
        let current = req.effective(hop).to_vec();
        let findings = graph.findings[hop - 1].clone();
        let mut should = Vec::new();
        include_clauses(&current, &mut should);
        let found: usize = findings.values().map(Vec::len).sum();
        if should.len() + found < MAX_CLAUSES {
            for list in findings.values() {
                for &i in list {
                    let v = graph.vertex(i);
                    let mut t = Map::new();
                    t.insert(v.field.clone(), Value::from(v.term.as_str()));
                    should.push(obj! { "constant_score" => obj! { "filter" => obj! { "term" => Value::Obj(t) }, "boost" => f64::from(v.weight as f32) } });
                }
            }
        } else {
            for (field, list) in &findings {
                let terms: Vec<Value> = list.iter().map(|i| Value::from(graph.vertex(*i).term.as_str())).collect();
                let mut t = Map::new();
                t.insert(field.clone(), Value::Arr(terms));
                should.push(obj! { "constant_score" => obj! { "filter" => obj! { "terms" => Value::Obj(t) } } });
            }
        }
        let guiding = req.hops[hop].query.clone().unwrap_or_else(|| obj! { "match_all" => obj! {} });
        let query = obj! { "bool" => obj! { "must" => crate::arr![guiding, obj! { "bool" => obj! { "should" => Value::Arr(should) } }] } };
        let mut aggs = Map::new();
        for (j, lvr) in last.iter().enumerate() {
            let Some(list) = findings.get(&lvr.field) else { continue };
            let mut terms: Vec<String> = list.iter().map(|i| graph.vertex(*i).term.clone()).collect();
            terms.sort_unstable();
            terms.dedup();
            let mut sub = Map::new();
            for (f, vr) in current.iter().enumerate() {
                let name = format!("field{f}");
                let size = vr.size + i64::from(vr.field == lvr.field);
                check_agg(&name, vr, size, true, true)?;
                let mut a = vertex_agg(vr, sig, size, true, false);
                if sig
                    && size < 10
                    && let Value::Obj(outer) = &mut a
                    && let Some(Value::Obj(m)) = outer.get_mut("significant_terms")
                {
                    m.insert("shard_size".into(), 10.into());
                }
                sub.insert(name, a);
            }
            let n = terms.len();
            aggs.insert(
                format!("field{j}"),
                obj! {
                    "terms" => obj! {
                        "field" => lvr.field.clone(),
                        "include" => Value::Arr(terms.into_iter().map(Value::from).collect()),
                        "shard_min_doc_count" => 1,
                        "min_doc_count" => 1,
                        "execution_hint" => "map",
                        "size" => n,
                    },
                    "aggs" => Value::Obj(sub),
                },
            );
        }
        let resp = internal::call(ctx.cluster, "POST", &uri, Some(&search_body(&req, query, aggs))).await?;
        note(&resp);
        let sample = resp.pointer(&["aggregations", "sample"]).cloned().unwrap_or(Value::Null);
        // The total signal, self joins excluded.
        let mut total = 0.0;
        for (j, lvr) in last.iter().enumerate() {
            for lb in buckets(sample.get(&format!("field{j}"))) {
                let lkey = key_string(lb);
                for (k, vr) in current.iter().enumerate() {
                    for b in buckets(lb.get(&format!("field{k}"))) {
                        if !(vr.field == lvr.field && key_string(b) == lkey) {
                            total += signal(b, sig);
                        }
                    }
                }
            }
        }
        let mut new_vertices = Vec::new();
        let mut new_connections = Vec::new();
        for (j, lvr) in last.iter().enumerate() {
            for lb in buckets(sample.get(&format!("field{j}"))) {
                let Some(from) = graph.get(&lvr.field, &key_string(lb)) else { continue };
                for (k, vr) in current.iter().enumerate() {
                    for b in buckets(lb.get(&format!("field{k}"))) {
                        let key = key_string(b);
                        let from_v = graph.vertex(from);
                        // Popularity mode keeps self joins (as Elasticsearch does).
                        if sig && vr.field == from_v.field && key == from_v.term {
                            continue;
                        }
                        let strength = signal(b, sig) / total * from_v.weight.min(0.95);
                        let doc_count = b.get("doc_count").and_then(Value::as_i64).unwrap_or(0);
                        let (bg, fg) = if sig { (b.get("bg_count").and_then(Value::as_i64).unwrap_or(0), doc_count) } else { (0, 0) };
                        let (to, created) = graph.add_vertex(&vr.field, &key, strength, hop, bg, fg);
                        if created {
                            new_vertices.push(to);
                        } else if let Some(v) = graph.vertices[to].as_mut() {
                            v.weight += strength;
                            if sig {
                                v.fg = v.fg.max(fg);
                            }
                        }
                        new_connections.push(graph.add_connection(from, to, strength, doc_count));
                    }
                }
            }
        }
        // Keep the strongest `size` new vertices per field.
        let mut evicted: Vec<usize> = Vec::new();
        for vr in &current {
            if new_vertices.len() as i64 <= vr.size {
                continue;
            }
            let mut pq = LucenePq::new(vr.size.max(0) as usize);
            for &v in &new_vertices {
                let vx = graph.vertex(v);
                if vx.field == vr.field
                    && let Some(e) = pq.insert_with_overflow(v, vx.weight)
                {
                    evicted.push(e);
                }
            }
        }
        if !evicted.is_empty() {
            for &c in &new_connections {
                let Some(to) = graph.connections[c].as_ref().map(|x| x.to) else { continue };
                if evicted.contains(&to) {
                    graph.remove_connection(c);
                    graph.remove_vertex(to, hop);
                }
            }
        }
    }

    // The response, in `HashMap` order.
    let live: Vec<usize> = (0..graph.vertices.len()).filter(|i| graph.vertices[*i].is_some()).collect();
    let hashes: Vec<i32> = live.iter().map(|&i| vertex_id_hash(&graph.vertex(i).field, &graph.vertex(i).term)).collect();
    let order = java_hash_map_order(&hashes, graph.vertex_peak);
    let mut number: HashMap<usize, usize> = HashMap::new();
    let mut vs = Vec::with_capacity(order.len());
    for (n, &o) in order.iter().enumerate() {
        let v = graph.vertex(live[o]);
        number.insert(live[o], n);
        let mut m = obj! { "field" => v.field.clone(), "term" => v.term.clone(), "weight" => v.weight, "depth" => v.depth };
        if req.detailed
            && let Value::Obj(mm) = &mut m
        {
            mm.insert("fg".into(), v.fg.into());
            mm.insert("bg".into(), v.bg.into());
        }
        vs.push(m);
    }
    let live_c: Vec<&Connection> = graph.connections.iter().flatten().collect();
    let hash_of = |i: usize| vertex_id_hash(&graph.vertex(i).field, &graph.vertex(i).term);
    let c_hashes: Vec<i32> = live_c.iter().map(|c| hash_of(c.from).wrapping_mul(31).wrapping_add(hash_of(c.to))).collect();
    let cs: Vec<Value> = java_hash_map_order(&c_hashes, graph.connection_peak)
        .into_iter()
        .map(|o| {
            let c = live_c[o];
            obj! { "source" => number[&c.from], "target" => number[&c.to], "weight" => c.weight, "doc_count" => c.doc_count }
        })
        .collect();
    let took = started.elapsed().as_millis() as i64;
    Reply::ok(obj! {
        "took" => took,
        "timed_out" => timed_out || req.timeout_ms.is_some_and(|t| took > t),
        "failures" => Value::Arr(failures),
        "vertices" => Value::Arr(vs),
        "connections" => Value::Arr(cs),
    })
}

/// Lucene's `PriorityQueue` with `lessThan(a, b) = a.weight < b.weight` (the least element on
/// top), for `insertWithOverflow`'s exact tie behaviour.
struct LucenePq {
    heap: Vec<(usize, f64)>,
    max: usize,
}

impl LucenePq {
    fn new(max: usize) -> LucenePq {
        LucenePq { heap: Vec::with_capacity(max), max }
    }

    /// Adds `item`; returns the element that no longer fits (the item itself when it is
    /// weaker than every kept element).
    fn insert_with_overflow(&mut self, item: usize, weight: f64) -> Option<usize> {
        if self.heap.len() < self.max {
            self.heap.push((item, weight));
            self.up_heap(self.heap.len() - 1);
            None
        } else if self.heap.first().is_some_and(|top| weight.partial_cmp(&top.1) != Some(std::cmp::Ordering::Less)) {
            let old = self.heap[0].0;
            self.heap[0] = (item, weight);
            self.down_heap(0);
            Some(old)
        } else {
            Some(item)
        }
    }

    fn up_heap(&mut self, mut i: usize) {
        let node = self.heap[i];
        while i > 0 {
            let parent = (i - 1) / 2;
            if node.1 < self.heap[parent].1 {
                self.heap[i] = self.heap[parent];
                i = parent;
            } else {
                break;
            }
        }
        self.heap[i] = node;
    }

    fn down_heap(&mut self, mut i: usize) {
        let node = self.heap[i];
        let n = self.heap.len();
        loop {
            let l = 2 * i + 1;
            if l >= n {
                break;
            }
            let r = l + 1;
            let smaller = if r < n && self.heap[r].1 < self.heap[l].1 { r } else { l };
            if self.heap[smaller].1 < node.1 {
                self.heap[i] = self.heap[smaller];
                i = smaller;
            } else {
                break;
            }
        }
        self.heap[i] = node;
    }
}

/// `String.hashCode`.
fn java_string_hash(s: &str) -> i32 {
    s.encode_utf16().fold(0i32, |h, u| h.wrapping_mul(31).wrapping_add(i32::from(u)))
}

/// `Vertex.VertexId.hashCode`.
fn vertex_id_hash(field: &str, term: &str) -> i32 {
    java_string_hash(field).wrapping_mul(31).wrapping_add(java_string_hash(term))
}

/// The iteration order of a `java.util.HashMap` (default capacity, grown for the most entries
/// it held, `peak`) whose live keys, with these `hashCode`s, were inserted in index order: by
/// bucket, insertion order within one (resizes keep it). Treeified buckets (8+ keys in one
/// bucket) are not modelled.
fn java_hash_map_order(hashes: &[i32], peak: usize) -> Vec<usize> {
    let mut cap = 16usize;
    while peak.max(hashes.len()) > cap * 3 / 4 {
        cap *= 2;
    }
    let bucket = |h: i32| ((h ^ ((h as u32) >> 16) as i32) as u32 as usize) & (cap - 1);
    let mut order: Vec<usize> = (0..hashes.len()).collect();
    order.sort_by_key(|&i| (bucket(hashes[i]), i));
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_map_order_matches_java() {
        // Inserted alice, bob, carol; Elasticsearch 9.5.5 listed carol, bob, alice.
        let h: Vec<i32> = ["alice", "bob", "carol"].iter().map(|t| vertex_id_hash("user", t)).collect();
        assert_eq!(java_hash_map_order(&h, 3), vec![2, 1, 0]);
        assert_eq!(java_string_hash("user"), 3_599_307);
    }

    #[test]
    fn priority_queue_keeps_the_strongest() {
        let mut pq = LucenePq::new(2);
        assert_eq!(pq.insert_with_overflow(0, 0.5), None);
        assert_eq!(pq.insert_with_overflow(1, 0.1), None);
        assert_eq!(pq.insert_with_overflow(2, 0.3), Some(1));
        // A tie with the weakest kept element replaces it.
        assert_eq!(pq.insert_with_overflow(3, 0.3), Some(2));
        assert_eq!(pq.insert_with_overflow(4, 0.01), Some(4));
    }

    #[test]
    fn parses_like_rest_graph_action() {
        let mut req = Request { hops: vec![], sample_size: 100, timeout_ms: None, use_significance: true, detailed: false, diversity_field: None, max_docs_per_value: 0 };
        let body = crate::json::parse(br#"{"vertices":[{"field":"a"}],"connections":{"query":{"match_all":{}}}}"#).unwrap().unwrap();
        parse_hop(body.as_object().unwrap(), 0, &mut req).unwrap();
        assert_eq!(req.hops.len(), 2);
        // The second hop inherits the root's vertex requests.
        assert_eq!(req.effective(1)[0].field, "a");
        assert_eq!(req.effective(1)[0].size, 10);
        let bad = crate::json::parse(br#"{"bogus":1}"#).unwrap().unwrap();
        let e = parse_hop(bad.as_object().unwrap(), 0, &mut req).unwrap_err();
        assert_eq!(e.reason(), "Illegal property in graph definition bogus");
        let bad = crate::json::parse(br#"{"vertices":[{"field":"a","include":["x"],"exclude":["y"]}]}"#).unwrap().unwrap();
        let e = parse_hop(bad.as_object().unwrap(), 0, &mut req).unwrap_err();
        assert_eq!(e.reason(), "Graph vertices definition cannot contain both include and exclude clauses");
    }
}
