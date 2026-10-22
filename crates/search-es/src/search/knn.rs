//! Vector search: query vectors checked against the field (`DenseVectorFieldType.createKnnQuery`:
//! dimensions, element type, cosine zero vectors, normalized dot products), `similarity`
//! thresholds turned into scores, and the top-level `knn` section (`DfsPhase` + `KnnScoreDocQuery`):
//! each section's global top `k` across shards becomes a per-shard score-doc query that is
//! combined with the request's query as a disjunction.

use super::query::{QueryCtx, arc};
use super::Target;
use crate::engine::fields::FieldInfo;
use crate::error::{EsError, EsResult};
use crate::query::specialized::{Knn, VectorData};
use search_codec::{VectorElement, VectorSimilarity, VectorSpec};
use search_index::query::QueryVector;
use search_index::QueryRef;

/// The query vector of a kNN search on `f`.
pub fn query_vector(f: &FieldInfo, spec: &VectorSpec, v: Option<&VectorData>) -> EsResult<QueryVector> {
    let Some(v) = v else {
        return Err(EsError::illegal_argument("[query_vector] must be provided (inference is not available)"));
    };
    let decoded;
    let v = match v {
        VectorData::Encoded(e) => {
            decoded = decode_query_vector(e, spec.element, spec.dims as usize)?;
            &decoded
        }
        other => other,
    };
    let dims = spec.dims as usize;
    let got = match v {
        VectorData::Floats(x) => x.len(),
        VectorData::Bytes(b) => if spec.element == VectorElement::Bit { b.len() * 8 } else { b.len() },
        VectorData::Encoded(_) => unreachable!("decoded above"),
    };
    let want = dims;
    if got != want && !(spec.element == VectorElement::Bit && matches!(v, VectorData::Floats(x) if x.len() * 8 == want)) {
        return Err(EsError::illegal_argument(format!(
            "The query vector has a different number of dimensions [{got}] than the document vectors [{want}]."
        )));
    }
    Ok(match (spec.element, v) {
        (VectorElement::Float32, VectorData::Floats(x)) => {
            if spec.similarity == VectorSimilarity::Cosine && x.iter().all(|v| *v == 0.0) {
                return Err(EsError::illegal_argument(format!(
                    "The [cosine] similarity does not support vectors with zero magnitude. Preview of invalid vector: {}",
                    preview(x)
                )));
            }
            if spec.similarity == VectorSimilarity::DotProduct {
                let norm: f64 = x.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>().sqrt();
                if (norm - 1.0).abs() > 1e-4 {
                    return Err(EsError::illegal_argument(format!(
                        "The [dot_product] similarity can only be used with unit-length vectors. Preview of invalid vector: {}",
                        preview(x)
                    )));
                }
            }
            QueryVector::Float(x.clone())
        }
        (VectorElement::Float32, VectorData::Bytes(b)) => QueryVector::Float(b.iter().map(|v| *v as f32).collect()),
        (VectorElement::Byte, VectorData::Floats(x)) => {
            let mut out = Vec::with_capacity(x.len());
            for (i, v) in x.iter().enumerate() {
                if v.fract() != 0.0 {
                    return Err(EsError::illegal_argument(format!(
                        "element_type [byte] vectors only support non-decimal values but found decimal value [{}] at dim [{i}];",
                        crate::json::write::java_float(*v)
                    )));
                }
                if *v < -128.0 || *v > 127.0 {
                    return Err(EsError::illegal_argument(format!(
                        "element_type [byte] vectors only support integers between [-128, 127] but found [{}] at dim [{i}];",
                        crate::json::write::java_float(*v)
                    )));
                }
                out.push(*v as i8);
            }
            QueryVector::Byte(out)
        }
        (VectorElement::Byte, VectorData::Bytes(b)) => QueryVector::Byte(b.clone()),
        (VectorElement::Bit, VectorData::Bytes(b)) => QueryVector::Bits(b.iter().map(|v| *v as u8).collect()),
        (VectorElement::Bit, VectorData::Floats(x)) => QueryVector::Bits(x.iter().map(|v| *v as i8 as u8).collect()),
        (_, VectorData::Encoded(_)) => unreachable!("decoded above"),
    })
    .inspect(|_| {
        let _ = f;
    })
}

/// An encoded query vector (Elasticsearch 9.5's `VectorData` string form) resolved against the
/// field: hex whose byte count is the vector length wins; otherwise base64 when it decodes to a
/// length the element type accepts (big-endian float32, or bfloat16, for float fields);
/// otherwise hex of the wrong length (failing on dimensions later), or an error.
pub fn decode_query_vector(encoded: &str, element: VectorElement, dims: usize) -> EsResult<VectorData> {
    // `ElementType.vectorLength`: elements per vector, bytes for bit vectors.
    let vector_len = if element == VectorElement::Bit { dims / 8 } else { dims };
    let is_hex = encoded.len().is_multiple_of(2) && encoded.bytes().all(|b| b.is_ascii_hexdigit());
    let hex_bytes = || (0..encoded.len() / 2).map(|i| u8::from_str_radix(&encoded[i * 2..i * 2 + 2], 16).unwrap_or(0) as i8);
    if is_hex && encoded.len() / 2 == vector_len {
        return Ok(VectorData::Bytes(hex_bytes().collect()));
    }
    // Hex of another length: element values, failing on the dimension count (which counts
    // values, not bits, as Elasticsearch reports it).
    let hex = || VectorData::Floats(hex_bytes().map(|b| b as f32).collect());
    let fail = |m: String| EsError::illegal_argument(format!("failed to parse field [query_vector]: [query_vector] {m}"));
    use base64::Engine as _;
    // `Base64.getDecoder()`: padding optional.
    let b64 = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    let Ok(bytes) = b64.decode(encoded.as_bytes()) else {
        if is_hex {
            return Ok(hex());
        }
        return Err(fail("must be a valid base64 or hex string".into()));
    };
    let len = bytes.len();
    let float = element == VectorElement::Float32;
    let fits = if float { len == dims * 4 || len == dims * 2 } else { len == vector_len };
    if !fits {
        if is_hex {
            return Ok(hex());
        }
        return Err(fail(format!(
            "must contain a valid Base64-encoded {} vector, but the decoded bytes length [{len}] is not compatible with the expected vector length",
            if float { "float" } else { "byte" }
        )));
    }
    if !float {
        return Ok(VectorData::Bytes(bytes.into_iter().map(|b| b as i8).collect()));
    }
    if len == dims * 2 {
        // bfloat16: the high half of a float32.
        return Ok(VectorData::Floats(bytes.as_chunks::<2>().0.iter().map(|c| f32::from_bits(((c[0] as u32) << 24) | ((c[1] as u32) << 16))).collect()));
    }
    Ok(VectorData::Floats(bytes.as_chunks::<4>().0.iter().map(|c| f32::from_be_bytes(*c)).collect()))
}

fn preview(x: &[f32]) -> String {
    let shown: Vec<String> = x.iter().take(10).map(|v| crate::json::write::java_float(*v)).collect();
    format!("[{}{}]", shown.join(", "), if x.len() > 10 { ", ..." } else { "" })
}

/// `similarity` (a distance or similarity threshold) as the minimum score Lucene computes
/// (`DenseVectorFieldMapper.VectorSimilarity.score`, by element type: bit vectors score
/// `(dims - hamming distance) / dims`, byte dot products are scaled by `dims * 2^15`).
pub fn similarity_to_score(spec: &search_codec::VectorSpec, s: f32) -> f32 {
    use search_codec::VectorElement;
    let dims = spec.dims as f32;
    match spec.similarity {
        VectorSimilarity::L2Norm if spec.element == VectorElement::Bit => (dims - s) / dims,
        VectorSimilarity::L2Norm => 1.0 / (1.0 + s * s),
        VectorSimilarity::DotProduct if spec.element == VectorElement::Byte => 0.5 + s / (dims * (1u32 << 15) as f32),
        VectorSimilarity::Cosine | VectorSimilarity::DotProduct => (1.0 + s) / 2.0,
        VectorSimilarity::MaxInnerProduct => {
            if s < 0.0 {
                1.0 / (1.0 - s)
            } else {
                s + 1.0
            }
        }
    }
}

/// Parses the `knn` sections of a request body.
pub fn sections(src: &crate::search_source::SearchSource) -> EsResult<Vec<Knn>> {
    Ok(sections_with_inner(src)?.into_iter().map(|(k, _)| k).collect())
}

/// The `knn` sections with their `inner_hits` (top-level knn only).
pub fn sections_with_inner(src: &crate::search_source::SearchSource) -> EsResult<Vec<(Knn, Option<crate::query::joining::InnerHits>)>> {
    let mut out = Vec::new();
    for m in &src.knn {
        let mut m = m.clone();
        let inner = match m.shift_remove("inner_hits") {
            Some(ih) => {
                let mut p = crate::query::XParser::from_value(&ih);
                p.next_token();
                Some(crate::query::joining::InnerHits::parse(&mut p)?)
            }
            None => None,
        };
        let v = crate::json::Value::Obj(m);
        let mut p = crate::query::XParser::from_value(&v);
        p.next_token();
        let mut k = Knn::parse(&mut p)?;
        if k.k.is_none() {
            // `KnnSearchBuilder`: `k` defaults to the request's `size`.
            let size = src.size.unwrap_or(10).max(1);
            k.k = Some(size);
            if k.num_candidates.is_some_and(|n| n < size) {
                return Err(EsError::illegal_argument("[num_candidates] cannot be less than [k]"));
            }
        }
        if k.num_candidates.is_none() {
            let kk = k.k.unwrap_or(10);
            k.num_candidates = Some(((kk as f64 * 1.5).ceil() as i32).max(kk).min(10_000));
        }
        out.push((k, inner));
    }
    Ok(out)
}

/// Inner hits of nested knn sections: the nested docs scored by the vector query.
pub fn inner_hits(qctx: &QueryCtx, sections: &[(Knn, Option<crate::query::joining::InnerHits>)]) -> EsResult<()> {
    for (k, ih) in sections {
        let Some(ih) = ih else { continue };
        let Some(f) = qctx.field(&k.field) else { continue };
        let Some(path) = f.nested_path.clone() else { continue };
        let Some(spec) = f.vector else { continue };
        let vector = query_vector(&f, &spec, k.query_vector.as_ref())?;
        let mut q = search_index::query::KnnQuery::new(f.number, vector, 10_000, 10_000);
        q.exact = true;
        if let Some(sim) = k.similarity {
            q.min_score = Some(similarity_to_score(&spec, sim));
        }
        let knn: QueryRef = if k.common.boost != 1.0 { arc(search_index::query::BoostQuery::new(arc(q), k.common.boost)) } else { arc(q) };
        let mut b = search_index::query::BooleanQuery::new()
            .add(search_index::query::Occur::Must, knn)
            .add(search_index::query::Occur::Filter, arc(search_index::query::TermQuery::new(crate::engine::fields::NESTED_PATH_FIELD, path.as_bytes().to_vec())));
        // `ExactKnnQueryBuilder` with each filter over the nested docs or over their parents
        // (`should(filter, ToChildBlockJoin(filter))`).
        let root = search_index::query::nested::root_filter();
        for x in &k.filter {
            let fq = qctx.to_filter(x)?;
            let parents = arc(search_index::query::BooleanQuery::new().add(search_index::query::Occur::Must, fq.clone()).add(search_index::query::Occur::Filter, root.clone()));
            let either = search_index::query::BooleanQuery::new()
                .add(search_index::query::Occur::Should, fq)
                .add(search_index::query::Occur::Should, arc(search_index::query::ToChildQuery { parent: parents, parent_filter: root.clone() }));
            b = b.add(search_index::query::Occur::Filter, arc(either));
        }
        let child: QueryRef = arc(b);
        let name = ih.name.clone().unwrap_or_else(|| path.clone());
        qctx.inner_hits.borrow_mut().push(super::query::InnerHitsDef { name, spec: ih.clone(), nested_path: Some(path), query: child, join: None });
    }
    Ok(())
}

/// The global top `k` of each section across `targets` (`DfsPhase` kNN), as per-target
/// score-doc queries (`KnnScoreDocQuery`) with the section's boost.
/// Also returns each target's dfs profile (`knn` entries) when `profile` is set.
#[allow(clippy::type_complexity)]
pub fn dfs(ctx: &crate::cluster::Cluster, st: &crate::meta::ClusterState, now: i64, targets: &[Target], knn: &[Knn], profile: bool) -> EsResult<(Vec<Vec<QueryRef>>, Vec<Option<crate::json::Value>>)> {
    let mut per_target: Vec<Vec<QueryRef>> = vec![Vec::new(); targets.len()];
    let mut profiles: Vec<Vec<crate::json::Value>> = vec![Vec::new(); targets.len()];
    // Several sections are summed per document into one score-doc query (the engine's boolean
    // rewrite would merge equal-looking score-doc clauses).
    let mut merged: Vec<std::collections::BTreeMap<u32, f32>> = vec![Default::default(); targets.len()];
    let multi = knn.len() > 1;
    for k in knn {
        let mut hits: Vec<(usize, u32, f32)> = Vec::new();
        for (ti, t) in targets.iter().enumerate() {
            let profiler = profile.then(search_index::search::profile::Profiler::new);
            let searcher = match &profiler {
                Some(p) => t.searcher()?.with_profiler(Some(p.clone())),
                None => t.searcher()?,
            };
            let started = std::time::Instant::now();
            let table = t.engine.table();
            let qctx = QueryCtx::new(ctx, st, &t.idx, &table, &searcher, now);
            // The boost applies once, to the score-doc queries below; an alias filter is one
            // more pre-filter of the vector search (`filtersInitial`).
            let mut kq = k.clone();
            kq.common.boost = 1.0;
            if let Some(af) = &t.alias_filter {
                kq.filter.push(af.clone());
            }
            let base = qctx.to_query(&crate::query::Query::from(kq)).map_err(|e| super::query::shard_exception(&t.idx, e))?;
            // The knn query rewrites to its top docs (a score-doc query).
            let mut c = search_index::collector::top_docs::TopScoreCollector::new(k.k.unwrap_or(10).max(1) as usize, u64::MAX);
            searcher.search(&base, &mut c).map_err(crate::engine::convert::index_error)?;
            let top = c.top_docs();
            if let Some(p) = &profiler {
                let ops = searcher.max_doc() as u64;
                profiles[ti].push(super::profile::knn_dfs_entry(p, started.elapsed().as_nanos() as u64, ops));
            }
            for h in top.hits {
                hits.push((ti, h.doc, h.score));
            }
        }
        hits.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
        hits.truncate(k.k.unwrap_or(10).max(1) as usize);
        if multi {
            for h in &hits {
                *merged[h.0].entry(h.1).or_insert(0.0) += h.2 * k.common.boost;
            }
            continue;
        }
        for (ti, list) in per_target.iter_mut().enumerate() {
            let docs: Vec<(u32, f32)> = hits.iter().filter(|h| h.0 == ti).map(|h| (h.1, h.2)).collect();
            let q: QueryRef = arc(search_index::query::ScoreDocQuery::new(docs));
            let boost = k.common.boost;
            list.push(if boost != 1.0 { arc(search_index::query::BoostQuery::new(q, boost)) } else { q });
            if let Some(n) = &k.common.name {
                let _ = n;
            }
        }
    }
    if multi {
        for (list, docs) in per_target.iter_mut().zip(merged) {
            list.push(arc(search_index::query::ScoreDocQuery::new(docs.into_iter().collect())));
        }
    }
    let profiles = profiles.into_iter().map(|k| (profile && !k.is_empty()).then(|| crate::obj! { "knn" => crate::json::Value::Arr(k) })).collect();
    Ok((per_target, profiles))
}

/// `GET|POST /{index}/_knn_search` (`RestKnnSearchAction`, REST API compatibility v8 only):
/// the deprecated kNN search API, run as a search whose `knn` section carries the request's
/// `filter` and whose `size` is `k`.
pub async fn knn_search(ctx: &crate::rest::Ctx<'_>) -> EsResult<crate::rest::Reply> {
    use crate::query::xparser::{Field, Tokens};
    use crate::query::{Token, XParser};
    ctx.warn("The kNN search API has been replaced by the `knn` option in the search API.");
    let Some(body) = ctx.body()? else {
        return Err(EsError::illegal_argument("request body is required"));
    };
    const FIELDS: &[Field] = &[
        Field::new("knn", Tokens::OBJECT),
        Field::new("filter", Tokens::OBJECT_ARRAY),
        Field::new("_source", Tokens::of(&[Token::Boolean, Token::StartObject, Token::StartArray, Token::String])),
        Field::new("docvalue_fields", Tokens::OBJECT_ARRAY_OR_STRING),
        Field::new("stored_fields", Tokens::STRING_ARRAY),
        Field::new("fields", Tokens::OBJECT_ARRAY_OR_STRING),
    ];
    let mut out = crate::json::Map::new();
    let mut knn: Option<crate::json::Map> = None;
    let mut filter: Option<crate::json::Value> = None;
    {
        let mut p = XParser::new(&body, ctx.raw_json(), Vec::new());
        p.object("knn-search", FIELDS, |p, i| {
            let v = p.take_value();
            match i {
                0 => knn = v.as_object().cloned(),
                1 => filter = Some(v),
                _ => {
                    out.insert(FIELDS[i].name.to_string(), v);
                }
            }
            Ok(())
        })?;
    }
    let Some(mut knn) = knn else {
        return Err(EsError::action_request_validation(&["missing required [knn] section in search body".into()]));
    };
    if let Some(f) = filter {
        knn.insert("filter".into(), f);
    }
    let k = knn.get("k").cloned();
    out.insert("knn".into(), crate::json::Value::Obj(knn));
    if let Some(k) = k {
        out.insert("size".into(), k);
    }
    let body = crate::json::Value::Obj(out);
    let req = super::parse_request(ctx, Some(&body), None)?;
    let res = super::run(ctx, req.clone())?;
    crate::rest::Reply::ok(super::render(&res, &req))
}

/// Java class names of source values (`Field [..] is of unsupported type [..]`): the mapped
/// field type's values, else what Jackson reads.
fn java_class(f: Option<&FieldInfo>, v: &crate::json::Value) -> &'static str {
    use crate::json::Value;
    if let Some(f) = f {
        match f.ty.as_str() {
            "long" | "date" | "date_nanos" | "unsigned_long" => return "java.lang.Long",
            "integer" | "short" | "byte" => return "java.lang.Integer",
            "double" | "scaled_float" => return "java.lang.Double",
            "float" | "half_float" => return "java.lang.Float",
            "boolean" => return "java.lang.Boolean",
            _ => {}
        }
    }
    match v {
        Value::Str(_) => "java.lang.String",
        Value::Bool(_) => "java.lang.Boolean",
        Value::Num(n) if n.as_i64().is_some_and(|i| i32::try_from(i).is_ok()) => "java.lang.Integer",
        Value::Num(n) if n.as_i64().is_some() => "java.lang.Long",
        Value::Num(_) => "java.lang.Double",
        Value::Arr(_) => "java.util.ArrayList",
        Value::Obj(_) => "java.util.HashMap",
        Value::Null => "null",
    }
}

/// `LookupQueryVectorBuilder`: the query vector a `query_vector_builder.lookup` names, read
/// from a document's `_source` (vectors kept out of `_source` read back from the index).
pub fn lookup_vector(ctx: &crate::rest::Ctx, builder: &crate::json::Value) -> EsResult<Option<crate::json::Value>> {
    use crate::json::Value;
    let Some(Value::Obj(l)) = builder.get("lookup") else { return Ok(None) };
    for k in l.keys() {
        if !matches!(k.as_str(), "id" | "index" | "path" | "routing") {
            return Err(EsError::new(400, "x_content_parse_exception", format!("[lookup] unknown field [{k}]")));
        }
    }
    let get = |k: &str| l.get(k).map(Value::to_es_string).ok_or_else(|| EsError::illegal_argument(format!("Required [{k}]")));
    let (id, index, path) = (get("id")?, get("index")?, get("path")?);
    let routing = l.get("routing").map(Value::to_es_string);
    let idx = crate::api::document::single_index(ctx, &index)?;
    let args = crate::api::document::GetArgs { routing, realtime: true, ..Default::default() };
    let not_found = || EsError::status(404, format!("Document with id [{id}] not found in index [{index}] for lookup query vector builder."));
    let got = crate::api::document::get_one(ctx, &idx, &id, &args)?.ok_or_else(not_found)?;
    let st = ctx.state();
    let table = ctx.cluster.engine.index(ctx.cluster, &st, &idx)?.table();
    let mut source = match crate::fetch::source_bytes(&got.doc.fields) {
        Some(b) => crate::fetch::parse_source(b)?,
        None => Value::Obj(crate::json::Map::new()),
    };
    if let Value::Obj(m) = &mut source
        && table.has_vectors
    {
        crate::engine::synthetic::vectors(&table, "", m, false);
    }
    let field = table.get(&path);
    let missing = || EsError::status(404, format!("Field [{path}] not found in document with id [{id}] for lookup query vector builder."));
    let v = match &source {
        Value::Obj(m) => source_at(m, &path).cloned().ok_or_else(missing)?,
        _ => return Err(missing()),
    };
    let is_vector = field.is_some_and(|f| f.vector.is_some());
    if !is_vector {
        // The field's first value names the type it has.
        let first = match &v {
            Value::Arr(a) => a.first().cloned().unwrap_or(Value::Null),
            other => other.clone(),
        };
        return Err(EsError::illegal_argument(format!(
            "Field [{path}] in document with id [{id}] is of unsupported type [{}] for lookup query vector builder.",
            java_class(field.map(|f| &**f), &first)
        )));
    }
    Ok(Some(v))
}

/// The value at a dotted `path` of a source object, keys with dots included (`a.b` as one key
/// or as `a` → `b`); arrays are not looked into.
fn source_at<'v>(m: &'v crate::json::Map, path: &str) -> Option<&'v crate::json::Value> {
    if let Some(v) = m.get(path) {
        return Some(v);
    }
    for (i, _) in path.match_indices('.') {
        if let Some(crate::json::Value::Obj(inner)) = m.get(&path[..i])
            && let Some(v) = source_at(inner, &path[i + 1..])
        {
            return Some(v);
        }
    }
    None
}

/// Resolves the `query_vector_builder.lookup` of the top-level `knn` sections (on the
/// coordinator, before any shard runs) into `query_vector`s.
pub fn resolve_lookups(ctx: &crate::rest::Ctx, src: &mut crate::search_source::SearchSource) -> EsResult<()> {
    for m in src.knn.iter_mut() {
        let Some(b) = m.get("query_vector_builder").cloned() else { continue };
        if m.contains_key("query_vector") {
            return Err(EsError::illegal_argument("cannot provide both [query_vector_builder] and [query_vector]"));
        }
        if let Some(v) = lookup_vector(ctx, &b)? {
            m.shift_remove("query_vector_builder");
            m.insert("query_vector".into(), v);
        }
    }
    Ok(())
}
