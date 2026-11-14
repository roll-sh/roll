//! Search-side APIs around the main search: `_validate/query`, search templates
//! (`_render/template`, `_search/template`, `_msearch/template`, Mustache through
//! search-painless), and term vectors (`_termvectors`, `_mtermvectors`; stored vectors or ones
//! computed from `_source` with the field's analyzer, with field and term statistics).

use super::query::QueryCtx;
use super::{SearchRequest, render, run};
use crate::engine::fields::FieldInfo;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use search_index::termvectors::{FieldTermVector, TermVector, TvTerm, TvToken};

/// `GET /{index}/_validate/query`.
pub fn validate_query(ctx: &Ctx) -> EsResult<Reply> {
    let explain = ctx.bool_param("explain", false)?;
    let rewrite = ctx.bool_param("rewrite", false)?;
    let body = ctx.body()?;
    let parsed: EsResult<Option<crate::query::Query>> = (|| {
        if let Some(Value::Obj(m)) = &body {
            for k in m.keys() {
                if k != "query" {
                    return Err(EsError::new(400, "parsing_exception", format!("request does not support [{k}]")));
                }
            }
            if let Some(q) = m.get("query") {
                return Ok(Some(crate::query::parse_query(q, ctx.raw_json(), &[crate::json::locate::Seg::Key("query")])?.0));
            }
        }
        Ok(None)
    })();
    let query = match parsed {
        Ok(q) => q,
        Err(e) => {
            let mut m = Map::new();
            m.insert("valid".into(), Value::Bool(false));
            if explain {
                // Anything but a `ParsingException` is wrapped in one ("Failed to parse"), and
                // printed with `ElasticsearchException.getDetailedMessage`.
                let e = if e.kind == "parsing_exception" { e } else { EsError::new(400, "parsing_exception", "Failed to parse").caused_by(e) };
                m.insert("error".into(), Value::Str(detailed_message(&e)));
            }
            return Reply::ok(Value::Obj(m));
        }
    };
    // `q` (with `df`, `default_operator`, `lenient`, …) parses as the search API does.
    let mut req = super::parse_request(ctx, None, None)?;
    if query.is_some() {
        req.source.query = query;
    }
    let st = ctx.state();
    let targets = super::shard_targets(ctx.cluster, &st, &req)?;
    let mut valid = true;
    let mut explanations = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let all_shards = ctx.bool_param("all_shards", false)?;
    for t in &targets {
        if seen.contains(&t.idx.name) && !all_shards {
            continue;
        }
        seen.push(t.idx.name.clone());
        let searcher = t.searcher()?;
        let table = t.table();
        let qctx = QueryCtx::new(ctx.cluster, &st, &t.idx, &table, &searcher, ctx.now_ms);
        let r = match &req.source.query {
            Some(q) => qctx.to_query(q),
            None if crate::opensearch::active() => Ok(crate::opensearch::approximate::ApproximateScoreQuery::match_all()),
            None => Ok(super::query::arc(search_index::query::MatchAllQuery)),
        };
        // `QueryExplanation`: the shard id only under `all_shards`.
        let shard_id = if all_shards { Some(Value::from(i64::from(t.shard.id))) } else { None };
        // `SearchContext.preProcess` adds the non-nested and alias filters.
        let r = r.and_then(|q| Ok((super::shard_query_main(&qctx, t, q.clone())?, q)));
        match r {
            Ok((q, parsed)) => {
                // `explain`: a rewrite to match-none shows the parsed query instead.
                let q = if rewrite {
                    let r = searcher.rewrite(&q).map_err(crate::engine::convert::index_error)?;
                    if r.as_any().is::<search_index::query::MatchNoneQuery>() { parsed } else { r }
                } else {
                    q
                };
                let mut e = Map::new();
                e.insert("index".into(), Value::Str(t.idx.name.clone()));
                if let Some(s) = shard_id {
                    e.insert("shard".into(), s);
                }
                e.insert("valid".into(), Value::Bool(true));
                e.insert("explanation".into(), Value::Str(q.lucene_string(searcher.schema())));
                explanations.push(Value::Obj(e));
            }
            Err(err) => {
                valid = false;
                let err = super::query::shard_exception(&t.idx, err);
                let mut e = Map::new();
                e.insert("index".into(), Value::Str(t.idx.name.clone()));
                if let Some(s) = shard_id {
                    e.insert("shard".into(), s);
                }
                e.insert("valid".into(), Value::Bool(false));
                // Lucene's query parser exceptions aren't Elasticsearch's `parse_exception`.
                let class = |c: &EsError| if !c.es && c.kind == "parse_exception" { "org.apache.lucene.queryparser.classic.ParseException".to_string() } else { java_class(&c.kind) };
                let cause = err.caused_by.as_ref().map(|c| format!("; {}: {}", class(c), c.reason())).unwrap_or_default();
                // Only the "failed to create query" wrapper (`SearchExecutionContext.toQuery`)
                // prints its index; the query builders' own exceptions print none.
                let prefix = if err.caused_by.is_some() { format!("[{}/{}] ", t.idx.name, t.idx.uuid) } else { String::new() };
                e.insert("error".into(), Value::Str(format!("{prefix}org.elasticsearch.index.query.QueryShardException: {}{cause}", err.reason())));
                explanations.push(Value::Obj(e));
            }
        }
    }
    // `TransportValidateQueryAction.shards`: one (random) shard per index unless `all_shards`.
    let n = if all_shards { targets.len() } else { seen.len() } as i64;
    let mut m = Map::new();
    m.insert("_shards".into(), obj! { "total" => n, "successful" => n, "failed" => 0 });
    m.insert("valid".into(), Value::Bool(valid));
    if explain || rewrite || all_shards {
        m.insert("explanations".into(), Value::Arr(explanations));
    }
    Reply::ok(Value::Obj(m))
}

/// `ElasticsearchException.getDetailedMessage`: `toString()` of the exception, then of its
/// cause (recursively while the causes are Elasticsearch exceptions).
fn detailed_message(e: &EsError) -> String {
    let mut text = format!("{}: {}", java_class(&e.kind), e.reason());
    if let Some(c) = &e.caused_by {
        text.push_str("; ");
        // `XContentParseException` is no `ElasticsearchException`: the chain stops there.
        if c.es && c.kind != "x_content_parse_exception" {
            text.push_str(&detailed_message(c));
        } else {
            text.push_str(&format!("{}: {}", java_class(&c.kind), c.reason()));
        }
    }
    text
}

fn java_class(kind: &str) -> String {
    match kind {
        "number_format_exception" => "java.lang.NumberFormatException".into(),
        "illegal_argument_exception" => "java.lang.IllegalArgumentException".into(),
        "illegal_state_exception" => "java.lang.IllegalStateException".into(),
        "unsupported_operation_exception" => "java.lang.UnsupportedOperationException".into(),
        "arithmetic_exception" => "java.lang.ArithmeticException".into(),
        "parsing_exception" => "org.elasticsearch.common.ParsingException".into(),
        "named_object_not_found_exception" => "org.elasticsearch.xcontent.NamedObjectNotFoundException".into(),
        "x_content_parse_exception" => "org.elasticsearch.xcontent.XContentParseException".into(),
        other => {
            let camel: String = other.split('_').map(|p| {
                let mut c = p.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            }).collect();
            format!("org.elasticsearch.{camel}")
        }
    }
}

/// Renders a search template body (`{source|id, params}`) to the search body JSON.
pub fn render_template(ctx: &Ctx, body: &Map, id_override: Option<&str>) -> EsResult<Value> {
    let out = render_template_text(ctx, body, id_override)?;
    match crate::json::parse(out.as_bytes()) {
        Ok(Some(v)) => Ok(v),
        Ok(None) => Ok(Value::Obj(Map::new())),
        Err(e) => Err(EsError::x_content_parse_bare(e.to_string())),
    }
}

/// The text a search template renders to, before it is parsed.
pub fn render_template_text(ctx: &Ctx, body: &Map, id_override: Option<&str>) -> EsResult<String> {
    let params = body.get("params").cloned().unwrap_or(Value::Obj(Map::new()));
    let (source, options): (String, Vec<(String, String)>) = if let Some(id) = id_override.or_else(|| body.get("id").and_then(Value::as_str)) {
        let st = ctx.state();
        let Some(sc) = st.metadata.scripts.get(id) else {
            return Err(EsError::resource_not_found(format!("unable to find script [{id}] in cluster state")));
        };
        let src = match &sc.source {
            Value::Str(s) => s.clone(),
            other => other.to_json(),
        };
        (src, Vec::new())
    } else {
        match body.get("source") {
            Some(Value::Str(s)) => (s.clone(), Vec::new()),
            Some(other) => (other.to_json(), vec![("content_type".into(), "application/json".into())]),
            None => return Err(EsError::action_request_validation(&["template is missing".into()])),
        }
    };
    let t = ctx.cluster.scripts.engine.compile_template(&source, search_painless::Context::Template, &options).map_err(crate::scripting::script_error)?;
    t.render_json(&crate::scripting::to_serde(&params)).map_err(crate::scripting::script_error)
}

/// `GET/POST /_render/template`, `/_render/template/{id}`.
pub fn render_template_api(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let v = render_template(ctx, &body, ctx.path_param("id"))?;
    Reply::ok(obj! { "template_output" => v })
}

/// `GET/POST /{index}/_search/template`.
pub async fn search_template(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body_map()?.ok_or_else(|| EsError::action_request_validation(&["template is missing".into()]))?;
    let rendered = render_template(ctx, &body, None)?;
    crate::xpack::security::check_lookups(ctx, Some(&rendered))?;
    let explain = body.get("explain").and_then(Value::as_bool);
    let profile = body.get("profile").and_then(Value::as_bool);
    let mut req = super::parse_request(ctx, Some(&rendered), None)?;
    crate::xpack::inference::query::rewrite_request(ctx, &mut req).await?;
    if let Some(e) = explain {
        req.source.explain = Some(e);
    }
    if let Some(p) = profile {
        req.source.profile = p;
    }
    let out = run(ctx, req.clone())?;
    Reply::ok(render(&out, &req))
}

/// `GET/POST /_msearch/template`.
pub async fn msearch_template(ctx: &Ctx<'_>) -> EsResult<Reply> {
    super::check_max_concurrent_searches(ctx)?;
    let started = std::time::Instant::now();
    let data = ctx.body.clone();
    let lines: Vec<&[u8]> = data.split(|b| *b == b'\n').filter(|l| !l.iter().all(|b| b.is_ascii_whitespace())).collect();
    let default_index = ctx.path_param("index").map(str::to_string);
    let mut responses = Vec::new();
    for pair in lines.chunks(2) {
        let [h, b] = pair else { break };
        let parsed = (|| -> EsResult<SearchRequest> {
            let header = match crate::json::parse(h)? {
                Some(Value::Obj(m)) => m,
                _ => Map::new(),
            };
            let body = match crate::json::parse(b)? {
                Some(Value::Obj(m)) => m,
                _ => Map::new(),
            };
            let rendered = render_template(ctx, &body, None)?;
            crate::xpack::security::check_lookups(ctx, Some(&rendered))?;
            let mut req: SearchRequest = super::parse_request(ctx, Some(&rendered), None)?;
            req.indices = match header.get("index") {
                Some(Value::Str(s)) => crate::meta::resolve::split_expressions(s),
                Some(Value::Arr(a)) => a.iter().map(|v| v.to_es_string()).collect(),
                _ => default_index.as_deref().map(crate::meta::resolve::split_expressions).unwrap_or_default(),
            };
            if let Some(n) = crate::xpack::security::authorize_search(ctx, &req.indices, &req.opts)? {
                req.indices = n;
            }
            Ok(req)
        })();
        let r = match parsed {
            Ok(mut req) => match crate::xpack::inference::query::rewrite_request(ctx, &mut req).await {
                Ok(()) => run(ctx, req.clone()).map(|out| {
                    let mut v = render(&out, &req);
                    if let Value::Obj(m) = &mut v {
                        m.insert("status".into(), Value::from(200i64));
                    }
                    v
                }),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        responses.push(match r {
            Ok(v) => v,
            Err(e) => obj! { "error" => e.to_value_with_root(false), "status" => e.status as i64 },
        });
    }
    Reply::ok(obj! { "took" => started.elapsed().as_millis() as i64, "responses" => Value::Arr(responses) })
}

/// Term vectors computed from `_source` values with the field's analyzer.
fn computed_vector(f: &FieldInfo, values: &[String], positions: bool, offsets: bool) -> TermVector {
    let mut terms: std::collections::BTreeMap<Vec<u8>, Vec<TvToken>> = std::collections::BTreeMap::new();
    if let Some(a) = &f.analyzer {
        let refs: Vec<&str> = values.iter().map(String::as_str).collect();
        a.analyze_values(&refs, &mut |t| {
            terms.entry(t.term.as_bytes().to_vec()).or_default().push(TvToken {
                position: positions.then_some(t.position),
                start_offset: offsets.then_some(t.start_offset),
                end_offset: offsets.then_some(t.end_offset),
                payload: None,
            });
        });
    } else {
        for v in values {
            terms.entry(v.as_bytes().to_vec()).or_default().push(TvToken { position: positions.then_some(0), start_offset: offsets.then_some(0), end_offset: offsets.then_some(v.encode_utf16().count() as u32), payload: None });
        }
    }
    TermVector {
        positions,
        offsets,
        payloads: false,
        terms: terms.into_iter().map(|(term, tokens)| TvTerm { term, freq: tokens.len() as u32, tokens }).collect(),
    }
}

/// `TermVectorsRequest.FilterSettings`: which terms the `filter` keeps (`TermVectorsFilter`).
struct FilterSettings {
    max_num_terms: i64,
    min_term_freq: i64,
    max_term_freq: i64,
    min_doc_freq: i64,
    max_doc_freq: i64,
    min_word_length: i64,
    max_word_length: i64,
}

impl FilterSettings {
    fn parse(m: &Map) -> EsResult<FilterSettings> {
        let mut f = FilterSettings {
            max_num_terms: 25,
            min_term_freq: 0,
            max_term_freq: i64::from(i32::MAX),
            min_doc_freq: 0,
            max_doc_freq: i64::from(i32::MAX),
            min_word_length: 0,
            max_word_length: 0,
        };
        for (k, v) in m {
            let slot = match k.as_str() {
                "max_num_terms" => &mut f.max_num_terms,
                "min_term_freq" => &mut f.min_term_freq,
                "max_term_freq" => &mut f.max_term_freq,
                "min_doc_freq" => &mut f.min_doc_freq,
                "max_doc_freq" => &mut f.max_doc_freq,
                "min_word_length" => &mut f.min_word_length,
                "max_word_length" => &mut f.max_word_length,
                other => return Err(EsError::illegal_argument(format!("The parameter {other} is not valid for filter parameter for term vector request!"))),
            };
            *slot = match v {
                Value::Num(n) => n.as_f64() as i32 as i64,
                other => crate::units::parse_java_int(&other.to_es_string()).map(i64::from).ok_or_else(|| EsError::number_format(&other.to_es_string()))?,
            };
        }
        Ok(f)
    }
}

/// `TermVectorsFilter.selectBestTerms` for one field: each term's tf-idf score
/// (`ClassicSimilarity`), `None` for the terms the filter drops. The statistics come from the
/// shard, or from the document's own vector when the shard has no terms for the field; the
/// best terms are kept in a Lucene `PriorityQueue` replica, so ties at the cut fall as in
/// Elasticsearch.
fn best_terms(searcher: &search_index::Searcher, field: u32, tv: &TermVector, f: &FilterSettings) -> Vec<Option<f32>> {
    let shard_stats = searcher.collection_stats(field);
    let num_docs = shard_stats.map_or(1, |s| s.doc_count) as f64;
    let limit = (f.max_num_terms.min(tv.terms.len() as i64)).max(0) as usize;
    let single = {
        let mut dicts = searcher.segments().iter().filter_map(|s| s.reader.terms(field));
        match (dicts.next(), dicts.next()) {
            (Some(d), None) => Some(d),
            _ => None,
        }
    };
    // 1-based binary heap of (score, term index), smallest score on top.
    let mut heap: Vec<(f32, usize)> = vec![(0.0, usize::MAX)];
    for (i, t) in tv.terms.iter().enumerate() {
        let freq = i64::from(t.freq);
        let len = String::from_utf8_lossy(&t.term).encode_utf16().count() as i64;
        let noise = (f.min_word_length > 0 && len < f.min_word_length)
            || (f.max_word_length > 0 && len > f.max_word_length)
            || (f.min_term_freq > 0 && freq < f.min_term_freq)
            || freq > f.max_term_freq;
        if noise {
            continue;
        }
        let doc_freq = match shard_stats {
            None => 1,
            Some(_) => match searcher.term_stats(field, &t.term).doc_freq {
                // A term the shard lacks (`assert foundTerm` is off in production): a lone
                // segment's terms enum reports the stale frequency of the term its failed seek
                // stopped at (the next one, or the block's last); a multi-segment enum reports 0.
                0 => single.map_or(0, |tr| {
                    let mut it = tr.seek_ceil(&t.term);
                    if it.next().is_some() {
                        i64::from(it.doc_freq())
                    } else {
                        let mut all = tr.iter();
                        let mut last = 0;
                        while all.next().is_some() {
                            last = i64::from(all.doc_freq());
                        }
                        last
                    }
                }),
                df => df as i64,
            },
        };
        if (f.min_doc_freq > 0 && doc_freq < f.min_doc_freq) || doc_freq > f.max_doc_freq || doc_freq == 0 {
            continue;
        }
        let idf = (((num_docs + 1.0) / (doc_freq as f64 + 1.0)).ln() + 1.0) as f32;
        let score = t.freq as f32 * idf;
        if heap.len() - 1 < limit {
            heap.push((score, i));
            up_heap(&mut heap);
        } else if limit > 0 && heap[1].0 < score {
            heap[1] = (score, i);
            down_heap(&mut heap);
        }
    }
    let mut out = vec![None; tv.terms.len()];
    for &(score, i) in &heap[1..] {
        out[i] = Some(score);
    }
    out
}

/// Lucene `PriorityQueue.upHeap` on the last element.
fn up_heap(h: &mut [(f32, usize)]) {
    let mut i = h.len() - 1;
    let node = h[i];
    let mut j = i >> 1;
    while j > 0 && node.0 < h[j].0 {
        h[i] = h[j];
        i = j;
        j >>= 1;
    }
    h[i] = node;
}

/// Lucene `PriorityQueue.downHeap` from the top.
fn down_heap(h: &mut [(f32, usize)]) {
    let size = h.len() - 1;
    let mut i = 1;
    let node = h[i];
    let mut j = i << 1;
    let mut k = j + 1;
    if k <= size && h[k].0 < h[j].0 {
        j = k;
    }
    while j <= size && h[j].0 < node.0 {
        h[i] = h[j];
        i = j;
        j = i << 1;
        k = j + 1;
        if k <= size && h[k].0 < h[j].0 {
            j = k;
        }
    }
    h[i] = node;
}

fn render_vector(name: &str, tv: &FieldTermVector, positions: bool, offsets: bool, payloads: bool, scores: Option<&[Option<f32>]>, field_stats: bool) -> (String, Value) {
    let mut m = Map::new();
    if field_stats {
        // A field the shard has no terms for reports zeros (`TermVectorsWriter.EMPTY_TERMS`).
        let (sdf, dc, sttf) = tv.field_stats.as_ref().map_or((0, 0, 0), |fs| (fs.sum_doc_freq as i64, fs.doc_count as i64, fs.sum_total_term_freq as i64));
        m.insert("field_statistics".into(), obj! { "sum_doc_freq" => sdf, "doc_count" => dc, "sum_ttf" => sttf });
    }
    let mut terms = Map::new();
    for (i, t) in tv.vector.terms.iter().enumerate() {
        let score = match scores {
            Some(s) => match s.get(i).copied().flatten() {
                Some(x) => Some(x),
                None => continue,
            },
            None => None,
        };
        let mut tm = Map::new();
        // Terms the shard doesn't hold print no statistics (`buildTermStatistics`).
        if let Some(st) = tv.term_stats.get(i).filter(|st| st.doc_freq > 0) {
            tm.insert("doc_freq".into(), Value::from(st.doc_freq as i64));
            tm.insert("ttf".into(), Value::from(st.total_term_freq as i64));
        }
        tm.insert("term_freq".into(), Value::from(t.freq as i64));
        if positions || offsets || payloads {
            let toks: Vec<Value> = t
                .tokens
                .iter()
                .map(|k| {
                    let mut km = Map::new();
                    if positions && let Some(p) = k.position {
                        km.insert("position".into(), Value::from(p as i64));
                    }
                    if offsets {
                        if let Some(s) = k.start_offset {
                            km.insert("start_offset".into(), Value::from(s as i64));
                        }
                        if let Some(e) = k.end_offset {
                            km.insert("end_offset".into(), Value::from(e as i64));
                        }
                    }
                    if payloads && let Some(p) = &k.payload {
                        use base64::Engine as _;
                        km.insert("payload".into(), Value::Str(base64::engine::general_purpose::STANDARD.encode(p)));
                    }
                    Value::Obj(km)
                })
                .collect();
            if !toks.is_empty() {
                tm.insert("tokens".into(), Value::Arr(toks));
            }
        }
        if let Some(x) = score {
            tm.insert("score".into(), Value::Num(crate::json::Number::Float(crate::fetch::java_float_as_double(x))));
        }
        terms.insert(String::from_utf8_lossy(&t.term).into_owned(), Value::Obj(tm));
    }
    m.insert("terms".into(), Value::Obj(terms));
    (name.to_string(), Value::Obj(m))
}

/// One term vectors request.
fn termvectors_one(ctx: &Ctx, index: &str, id: Option<&str>, doc: Option<&Value>, opts: &Map) -> EsResult<Value> {
    let started = std::time::Instant::now();
    let flag = |k: &str, d: bool| -> bool {
        match opts.get(k) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Str(s)) => s != "false",
            _ => d,
        }
    };
    let positions = flag("positions", true);
    let offsets = flag("offsets", true);
    let payloads = flag("payloads", true);
    let term_stats = flag("term_statistics", false);
    let field_stats = flag("field_statistics", true);
    let idx = crate::api::document::single_index(ctx, index)?;
    let st = ctx.state();
    let engine = ctx.cluster.engine.index(ctx.cluster, &st, &idx)?;
    // Under field level security the fields the reader may not see have no vectors.
    let table = match crate::xpack::security::dls::ReadSecurity::of(ctx).and_then(|s| s.fields(crate::xpack::security::dls::READ, &idx.name)) {
        Some(f) => std::sync::Arc::new(engine.table().restricted(&f)),
        None => engine.table(),
    };
    let routing = opts.get("routing").map(|v| v.to_es_string());
    if doc.is_none()
        && let Some(id) = id
    {
        crate::write::check_routing(&idx, id, routing.as_deref())?;
    }
    let shard = engine.shard(crate::meta::routing::shard_for(&idx, id.unwrap_or(""), routing.as_deref())).clone();
    let mut searcher = shard.searcher();
    let mut m = Map::new();
    m.insert("_index".into(), Value::Str(idx.name.clone()));
    let (source, stored, version): (Option<Value>, Vec<(u32, search_codec::stored::OwnedStoredValue)>, i64) = match (doc, id) {
        (Some(d), _) => (Some(d.clone()), Vec::new(), 0),
        (None, Some(id)) => {
            m.insert("_id".into(), Value::Str(id.to_string()));
            let realtime = flag("realtime", true);
            let got = shard.shard.get(id.as_bytes(), realtime).map_err(crate::engine::convert::index_error)?;
            match got {
                Some(g) => {
                    // A realtime get reads through Elasticsearch's internal reader (refreshed
                    // for a buffered document), which the statistics then come from.
                    if realtime {
                        searcher = shard.shard.realtime_searcher(id.as_bytes()).map_err(crate::engine::convert::index_error)?;
                    }
                    let src = crate::fetch::source_bytes(&g.fields).map(crate::fetch::parse_source).transpose()?;
                    (src, g.fields, g.version)
                }
                None => {
                    m.insert("_version".into(), Value::from(0i64));
                    m.insert("found".into(), Value::Bool(false));
                    m.insert("took".into(), Value::from(started.elapsed().as_millis() as i64));
                    return Ok(Value::Obj(m));
                }
            }
        }
        _ => return Err(EsError::action_request_validation(&["id or doc is missing".into()])),
    };
    if doc.is_none() {
        m.insert("_version".into(), Value::from(version));
    } else {
        m.insert("_version".into(), Value::from(0i64));
    }
    m.insert("found".into(), Value::Bool(true));
    m.insert("took".into(), Value::from(started.elapsed().as_millis() as i64));
    let filter = match opts.get("filter") {
        Some(Value::Obj(fm)) => Some(FilterSettings::parse(fm)?),
        Some(Value::Null) | None => None,
        Some(_) => return Err(EsError::illegal_argument("Failed to parse filter settings")),
    };
    let stored_vectors = search_index::termvectors::from_stored(&stored, None).map_err(crate::engine::convert::index_error)?;
    let requested: Vec<String> = match opts.get("fields") {
        Some(Value::Arr(a)) => a.iter().flat_map(|v| table.matching(&v.to_es_string())).collect(),
        Some(Value::Str(s)) => s.split(',').flat_map(|p| table.matching(p.trim())).collect(),
        _ if doc.is_some() => {
            // An artificial document: every text field it has.
            let mut v: Vec<String> = table.fields.values().filter(|f| f.kind.is_text() && f.parent.is_none() && f.index).map(|f| f.name.clone()).collect();
            v.sort();
            v
        }
        _ => {
            // `TermVectorsService` without `fields`: only the vectors the document stored.
            let mut v: Vec<String> = stored_vectors.iter().filter_map(|(n, _)| table.by_number.get(n).map(|f| f.name.clone())).collect();
            v.sort();
            v.dedup();
            v
        }
    };
    let mut out = Map::new();
    let mut usage = super::field_usage::Notes::default();
    for name in requested {
        let Some(f) = table.get(&name) else { continue };
        if !(f.kind.is_text() || f.kind.is_keyword_like()) {
            continue;
        }
        let vector = match stored_vectors.iter().find(|(n, _)| *n == f.number) {
            Some((_, v)) => {
                usage.note(&f.name, super::field_usage::U_TERM_VECTORS);
                v.clone()
            }
            None => {
                let Some(Value::Obj(src)) = &source else { continue };
                let mut raw = Vec::new();
                crate::engine::source::extract_values(src, f.parent.as_deref().unwrap_or(&f.name), &mut raw);
                let vals: Vec<String> = raw.into_iter().filter(|v| !v.is_null()).map(super::query::value_text).collect();
                if vals.is_empty() {
                    continue;
                }
                computed_vector(f, &vals, positions, offsets)
            }
        };
        if vector.terms.is_empty() {
            continue;
        }
        let scores = filter.as_ref().map(|fs| best_terms(&searcher, f.number, &vector, fs));
        let ftv = searcher.with_stats(f.number, vector, term_stats, field_stats);
        let (k, v) = render_vector(&name, &ftv, positions, offsets, payloads, scores.as_deref(), field_stats);
        out.insert(k, v);
    }
    m.insert("term_vectors".into(), Value::Obj(out));
    shard.counters.field_usage.lock().record(&usage.used);
    Ok(Value::Obj(m))
}

fn params_map(ctx: &Ctx) -> Map {
    let mut opts = Map::new();
    for k in ["positions", "offsets", "payloads", "term_statistics", "field_statistics", "routing", "realtime"] {
        if let Some(v) = ctx.query(k) {
            opts.insert(k.into(), Value::Str(if v.is_empty() { "true".into() } else { v.to_string() }));
        }
    }
    if let Some(f) = ctx.query("fields") {
        opts.insert("fields".into(), Value::Str(f.to_string()));
    }
    opts
}

/// `GET/POST /{index}/_termvectors/{id}`.
pub fn termvectors(ctx: &Ctx) -> EsResult<Reply> {
    let index = ctx.path_param("index").unwrap_or_default().to_string();
    let mut opts = params_map(ctx);
    let mut doc = None;
    if let Some(Value::Obj(b)) = ctx.body()? {
        for (k, v) in b {
            if k == "doc" {
                doc = Some(v);
            } else {
                opts.insert(k, v);
            }
        }
    }
    // `TermVectorsResponse` is 200 whether or not the document was found.
    let v = termvectors_one(ctx, &index, ctx.path_param("id"), doc.as_ref(), &opts)?;
    Ok(Reply::Body(200, v))
}

/// `GET/POST /_mtermvectors`.
#[allow(clippy::type_complexity)]
pub fn mtermvectors(ctx: &Ctx) -> EsResult<Reply> {
    let base = params_map(ctx);
    let default_index = ctx.path_param("index").map(str::to_string);
    let body = ctx.body_map()?.unwrap_or_default();
    let mut items: Vec<(Option<String>, Option<String>, Option<Value>, Map)> = Vec::new();
    if let Some(Value::Arr(docs)) = body.get("docs") {
        for d in docs {
            let Value::Obj(dm) = d else { continue };
            let mut o = base.clone();
            for (k, v) in dm {
                match k.as_str() {
                    "_index" | "_id" | "doc" => {}
                    // `TermVectorsRequest.parseRequest`'s fields.
                    "fields" | "offsets" | "positions" | "payloads" | "term_statistics" | "field_statistics" | "dfs" | "filter" | "per_field_analyzer"
                    | "routing" | "version" | "version_type" | "realtime" => {
                        o.insert(k.clone(), v.clone());
                    }
                    other => return Err(EsError::parse(format!("failed to parse term vectors request. unknown field [{other}]"))),
                }
            }
            items.push((dm.get("_index").map(|v| v.to_es_string()).or(default_index.clone()), dm.get("_id").map(|v| v.to_es_string()), dm.get("doc").cloned(), o));
        }
    }
    if let Some(Value::Arr(ids)) = body.get("ids") {
        for i in ids {
            items.push((default_index.clone(), Some(i.to_es_string()), None, base.clone()));
        }
    }
    if let Some(ids) = ctx.query("ids") {
        for i in ids.split(',') {
            items.push((default_index.clone(), Some(i.to_string()), None, base.clone()));
        }
    }
    let mut out = Vec::new();
    for (index, id, doc, opts) in items {
        let index = index.unwrap_or_default();
        // `TransportMultiTermVectorsAction` resolves aliases itself, with its own message.
        let r = termvectors_one(ctx, &index, id.as_deref(), doc.as_ref(), &opts).map_err(|e| {
            let reason = e.reason().to_string();
            match (reason.strip_prefix("alias ["), reason.find("has more than one index associated with it [")) {
                (Some(_), Some(at)) if e.kind == "illegal_argument_exception" => {
                    let head = &reason[1..at];
                    let rest = &reason[at + "has more than one index associated with it [".len()..];
                    let (list, tail) = rest.split_once(']').unwrap_or((rest, ""));
                    EsError::illegal_argument(format!("A{head}has more than one index associated with it [[{list}]]{tail}"))
                }
                _ => e,
            }
        });
        match r {
            Ok(v) => out.push(v),
            Err(e) => {
                let shown = crate::api::document::single_index(ctx, &index).map(|i| i.name.clone()).unwrap_or(index);
                out.push(obj! { "_index" => shown, "_id" => id.unwrap_or_default(), "error" => crate::opensearch::item_error(ctx, &e, true) })
            }
        }
    }
    Reply::ok(obj! { "docs" => Value::Arr(out) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lucene's `PriorityQueue` replaces its top only for a strictly better entry, and which
    /// of several tied entries is on top follows the heap's layout.
    #[test]
    fn best_terms_queue_breaks_ties_like_lucene() {
        let mut heap: Vec<(f32, usize)> = vec![(0.0, usize::MAX)];
        let limit = 2;
        for (i, score) in [1.0f32, 1.0, 1.0, 2.0].into_iter().enumerate() {
            if heap.len() - 1 < limit {
                heap.push((score, i));
                up_heap(&mut heap);
            } else if heap[1].0 < score {
                heap[1] = (score, i);
                down_heap(&mut heap);
            }
        }
        let mut kept: Vec<usize> = heap[1..].iter().map(|e| e.1).collect();
        kept.sort();
        assert_eq!(kept, vec![1, 3]);
    }

    #[test]
    fn filter_settings_defaults_and_errors() {
        let f = FilterSettings::parse(&Map::new()).unwrap();
        assert_eq!((f.max_num_terms, f.min_term_freq, f.max_doc_freq), (25, 0, i64::from(i32::MAX)));
        let mut m = Map::new();
        m.insert("bogus".into(), Value::from(1i64));
        assert!(FilterSettings::parse(&m).is_err());
    }
}
