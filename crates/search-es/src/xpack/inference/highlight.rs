//! The `semantic` highlighter (`SemanticTextHighlighter`), the default for `semantic_text`
//! fields: a hit's chunks scored against the field's query in the search (its embedding, as the
//! rewrite put it into the query: `crate::xpack::inference::query`), the best
//! `number_of_fragments` (5) returned as the chunks' text, by score with `order: score`, else in
//! text order. A field the search doesn't query semantically gets its first chunks.

use crate::json::{Map, Value};
use crate::query::Query;

/// A field's query embedding, found in the rewritten query.
#[derive(Clone, Debug)]
pub enum Probe {
    Sparse(Vec<(String, f32)>),
    Dense(Vec<f32>),
}

/// Every `semantic_text` field's query embedding in a (rewritten) query tree: the `sparse_vector`
/// or `knn` inside a `nested` query on `<field>.inference.chunks`.
pub fn probes(q: Option<&Query>) -> Vec<(String, Probe)> {
    fn walk(q: &Query, out: &mut Vec<(String, Probe)>) {
        match q {
            Query::Nested(n) => {
                if let Some(field) = n.path.strip_suffix(".inference.chunks") {
                    match &n.query {
                        Query::SparseVector(s) => {
                            if let Some(t) = &s.query_vector {
                                out.push((field.to_string(), Probe::Sparse(t.clone())));
                            }
                        }
                        Query::Knn(k) => {
                            if let Some(crate::query::specialized::VectorData::Floats(v)) = &k.query_vector {
                                out.push((field.to_string(), Probe::Dense(v.clone())));
                            }
                        }
                        _ => {}
                    }
                }
                walk(&n.query, out);
            }
            Query::Bool(b) => b.must.iter().chain(&b.filter).chain(&b.should).chain(&b.must_not).for_each(|c| walk(c, out)),
            Query::DisMax(d) => d.queries.iter().for_each(|c| walk(c, out)),
            Query::Boosting(b) => {
                walk(&b.positive, out);
                walk(&b.negative, out);
            }
            Query::ConstantScore(c) => walk(&c.filter, out),
            Query::FunctionScore(f) => walk(&f.query, out),
            Query::ScriptScore(s) => walk(&s.query, out),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Some(q) = q {
        walk(q, &mut out);
    }
    out
}

fn score(probe: &Probe, emb: &Value, similarity: &str) -> Option<f64> {
    match (probe, emb) {
        (Probe::Sparse(q), Value::Obj(d)) => Some(q.iter().map(|(t, w)| f64::from(*w) * d.get(t).and_then(Value::as_f64).unwrap_or(0.0)).sum()),
        (Probe::Dense(q), Value::Arr(d)) => {
            let d: Vec<f64> = d.iter().map(|x| x.as_f64().unwrap_or(0.0)).collect();
            let dot: f64 = q.iter().zip(&d).map(|(a, b)| f64::from(*a) * b).sum();
            Some(match similarity {
                "l2_norm" => -q.iter().zip(&d).map(|(a, b)| (f64::from(*a) - b).powi(2)).sum::<f64>(),
                "cosine" => {
                    let (na, nb) = (q.iter().map(|a| f64::from(*a).powi(2)).sum::<f64>().sqrt(), d.iter().map(|b| b * b).sum::<f64>().sqrt());
                    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
                }
                _ => dot,
            })
        }
        _ => None,
    }
}

/// The highlight of a `semantic_text` field in one hit, from its stored `_inference_fields`.
/// Without a probe (no semantic query on the field), the first chunks in text order.
pub fn highlight(src: &Map, field: &str, probe: Option<&Probe>, similarity: &str, fragments: usize, by_score: bool) -> Option<Vec<String>> {
    let entry = super::semantic::inference_entries(src).into_iter().find(|(f, _, _)| f == field).map(|(_, e, _)| e)?;
    let chunks = entry.pointer(&["inference", "chunks"])?.as_object()?;
    // Offsets index the field's values joined by one separator.
    let text: String = super::semantic::texts_at(src, field).unwrap_or_default().join("\n");
    let mut scored: Vec<(usize, f64, super::chunking::Span)> = Vec::new();
    for list in chunks.values() {
        for c in list.as_array().into_iter().flatten() {
            let (Some(s), Some(e)) = (c.get("start_offset").and_then(Value::as_i64), c.get("end_offset").and_then(Value::as_i64)) else { continue };
            let sc = match probe {
                Some(p) => match c.get("embeddings").and_then(|emb| score(p, emb, similarity)) {
                    Some(sc) => sc,
                    None => continue,
                },
                None => 0.0,
            };
            scored.push((scored.len(), sc, super::chunking::Span { start: s.max(0) as usize, end: e.max(0) as usize }));
        }
    }
    if scored.is_empty() {
        return None;
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let n = if fragments == 0 { 1 } else { fragments }.min(scored.len());
    let mut best: Vec<(usize, f64, super::chunking::Span)> = scored.into_iter().take(n).collect();
    if !by_score {
        best.sort_by_key(|c| c.0);
    }
    Some(best.into_iter().map(|(_, _, span)| super::chunking::substring(&text, span)).collect())
}
