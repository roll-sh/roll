//! Judgment lists (docs/search/relevance-experiments.md, section 1): judged queries kept in the
//! cluster as a plain index, `roll-judgments-<name>`, one document per rating, so a list
//! branches, snapshots, restores and exports like any other index and nothing outside the
//! cluster holds them. `evaluate` reads one by name (`judgments_list`).
//!
//! The routes (`GET /v1/search/clusters/{id}/judgments`, `GET|PUT|DELETE …/judgments/{name}`) run
//! as the cluster's other routes do, the owner narrowed to the list's index, and touch only an
//! index whose mapping marks it a list (`_meta.roll_judgments`): one that merely has the name is
//! refused, never replaced.

use crate::control::{Control, ControlError, Result};
use crate::evaluate::{Judgment, MAX_QUERIES, MAX_RATINGS};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// What a list's index name starts with.
pub const PREFIX: &str = "roll-judgments-";
/// The mapping's `_meta` key that marks an index a judgment list.
pub const META: &str = "roll_judgments";
/// Ratings one list holds: as many as one evaluation takes.
pub const MAX_STORED: usize = MAX_QUERIES * MAX_RATINGS;
/// Ratings read or written per request.
const PAGE: usize = 5_000;

/// Where a list's ratings came from.
pub const SOURCES: &[&str] = &["manual", "import", "clicks"];

/// The index of list `name`: 1 to 64 of `a-z`, `0-9`, `_` and `-`, starting with a letter or
/// digit. So `roll-judgments-<name>` is always one plain index name, never an expression: no
/// `,` list, `*` pattern, leading `-` exclusion, `:` remote cluster or `<…>` date math, whatever
/// the cluster or a grant's index patterns would make of one.
pub fn index_name(name: &str) -> std::result::Result<String, String> {
    let ok = (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-') && name.as_bytes()[0].is_ascii_alphanumeric();
    if ok { Ok(format!("{PREFIX}{name}")) } else { Err(format!("a judgment list's name is 1 to 64 of a-z, 0-9, _ and -, starting with a letter or digit, not [{name}]")) }
}

/// A rating's document id: the same query, index and document always land on one document.
fn key(query: &str, index: &str, doc: &str) -> String {
    let mut h = Sha256::new();
    for part in [query, index, doc] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part.as_bytes());
    }
    h.finalize()[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// The create body of a list's index, `meta` its `_meta.roll_judgments`. Fields beyond these
/// are kept in `_source` and not indexed (`dynamic: false`), so a list can carry notes.
pub fn create_body(meta: Value) -> Value {
    let kw = json!({ "type": "keyword" });
    json!({
        "settings": { "index": { "number_of_shards": 1 } },
        "mappings": {
            "dynamic": false,
            "_meta": { META: meta },
            "properties": {
                "key": kw, "query": kw, "index": kw, "doc_id": kw, "source": kw,
                "rating": { "type": "integer" },
                "coec": { "type": "float" },
                "expected_clicks": { "type": "float" },
                "searches": { "type": "long" },
                "impressions": { "type": "long" },
                "clicks": { "type": "long" },
            },
        },
    })
}

/// One stored rating: the judged query's text, the rating's document, and what else the source
/// keeps beside it (a click rating's counts).
#[derive(Clone, Debug, PartialEq)]
pub struct Stored {
    pub query: String,
    /// Empty: a document of whichever index is evaluated.
    pub index: String,
    pub doc_id: String,
    pub rating: i64,
    pub extra: Map<String, Value>,
}

/// The `_bulk` bodies that write `ratings` (at most [`PAGE`] each), every one marked `source`.
pub fn bulk_bodies(ratings: &[Stored], source: &str) -> Vec<String> {
    ratings
        .chunks(PAGE)
        .map(|chunk| {
            let mut out = String::new();
            for r in chunk {
                let id = key(&r.query, &r.index, &r.doc_id);
                let mut doc = Map::new();
                doc.insert("key".into(), json!(id));
                doc.insert("query".into(), json!(r.query));
                if !r.index.is_empty() {
                    doc.insert("index".into(), json!(r.index));
                }
                doc.insert("doc_id".into(), json!(r.doc_id));
                doc.insert("rating".into(), json!(r.rating));
                doc.insert("source".into(), json!(source));
                for (k, v) in &r.extra {
                    doc.insert(k.clone(), v.clone());
                }
                out.push_str(&json!({ "index": { "_id": id } }).to_string());
                out.push('\n');
                out.push_str(&Value::Object(doc).to_string());
                out.push('\n');
            }
            out
        })
        .collect()
}

/// The judged queries as ratings to store, the last of a repeated (query, index, document) kept.
pub fn flatten(judgments: &[Judgment]) -> Vec<Stored> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out: Vec<Stored> = Vec::new();
    for j in judgments {
        for (index, doc, rating) in &j.ratings {
            let r = Stored { query: j.query.clone(), index: index.clone(), doc_id: doc.clone(), rating: *rating, extra: Map::new() };
            match seen.entry(key(&r.query, &r.index, &r.doc_id)) {
                std::collections::hash_map::Entry::Occupied(e) => out[*e.get()] = r,
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(out.len());
                    out.push(r);
                }
            }
        }
    }
    out
}

/// One stored document as a rating, or what's wrong with it: a list is an ordinary index, so
/// anything may have been written into it.
fn stored(hit: &Value) -> std::result::Result<Stored, String> {
    let s = &hit["_source"];
    let id = hit["_id"].as_str().unwrap_or("");
    let text = |k: &str, max: usize| s[k].as_str().filter(|v| !v.is_empty() && v.len() <= max).map(str::to_string);
    let query = text("query", 1024).ok_or_else(|| format!("document [{id}] has no [query] of 1 to 1,024 bytes"))?;
    let doc_id = text("doc_id", 512).ok_or_else(|| format!("document [{id}] has no [doc_id]"))?;
    let index = match &s["index"] {
        Value::Null => String::new(),
        v => v.as_str().filter(|v| !v.is_empty() && v.len() <= 255).map(str::to_string).ok_or_else(|| format!("document [{id}] has an [index] that isn't an index name"))?,
    };
    let rating = s["rating"].as_i64().or_else(|| s["rating"].as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)).filter(|r| (0..=100).contains(r)).ok_or_else(|| format!("document [{id}] has no [rating] from 0 to 100"))?;
    Ok(Stored { query, index, doc_id, rating, extra: Map::new() })
}

/// The ratings as judged queries, in order of query text and then document.
pub fn group(mut ratings: Vec<Stored>) -> std::result::Result<Vec<Judgment>, String> {
    ratings.sort_by(|a, b| a.query.cmp(&b.query).then_with(|| a.index.cmp(&b.index)).then_with(|| a.doc_id.cmp(&b.doc_id)));
    let mut out: Vec<Judgment> = Vec::new();
    for r in ratings {
        match out.last_mut() {
            Some(j) if j.query == r.query => j.ratings.push((r.index, r.doc_id, r.rating)),
            _ => out.push(Judgment { query: r.query, ratings: vec![(r.index, r.doc_id, r.rating)] }),
        }
    }
    if out.len() > MAX_QUERIES {
        return Err(format!("it judges {} queries: an evaluation takes at most {MAX_QUERIES}", out.len()));
    }
    if let Some(j) = out.iter().find(|j| j.ratings.len() > MAX_RATINGS) {
        return Err(format!("it rates {} documents for [{}]: an evaluation takes at most {MAX_RATINGS}", j.ratings.len(), j.query));
    }
    Ok(out)
}

/// Judged queries as `evaluate` takes them.
pub fn judgments_json(judgments: &[Judgment]) -> Value {
    Value::Array(
        judgments
            .iter()
            .map(|j| {
                let ratings: Vec<Value> = j
                    .ratings
                    .iter()
                    .map(|(ix, id, r)| if ix.is_empty() { json!({ "_id": id, "rating": r }) } else { json!({ "_id": id, "rating": r, "_index": ix }) })
                    .collect();
                json!({ "query": j.query, "ratings": ratings })
            })
            .collect(),
    )
}

/// A `PUT …/judgments/{name}` body: `{judgments, source?}`, `source` `manual` (the default) or
/// `import`.
pub fn parse_put(body: &[u8]) -> std::result::Result<(Vec<Judgment>, String), String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("the body isn't JSON: {e}"))?;
    let Some(m) = v.as_object() else { return Err("the body is an object {judgments, source?}".into()) };
    if let Some(k) = m.keys().find(|k| !["judgments", "source"].contains(&k.as_str())) {
        return Err(format!("the body has no field [{k}] (it takes judgments, source)"));
    }
    let source = match m.get("source") {
        None | Some(Value::Null) => "manual".to_string(),
        Some(Value::String(s)) if s == "manual" || s == "import" => s.clone(),
        Some(_) => return Err("[source] is manual or import (judgments from clicks are built by …/_from_clicks)".into()),
    };
    Ok((crate::evaluate::parse_judgments(m.get("judgments"))?, source))
}

/// The cluster's answer as an error: its status and reason.
fn refused(status: u16, out: &[u8]) -> ControlError {
    let v: Value = serde_json::from_slice(out).unwrap_or(Value::Null);
    let reason = v["error"]["reason"].as_str().or_else(|| v["error"].as_str()).map_or_else(|| String::from_utf8_lossy(out).into_owned(), str::to_string);
    match status {
        400 => ControlError::Invalid(reason),
        403 => ControlError::Forbidden(reason),
        404 => ControlError::NotFound(reason),
        409 => ControlError::Conflict(reason),
        429 => ControlError::Busy(reason),
        503 => ControlError::Unavailable(reason),
        _ => ControlError::Internal(anyhow::anyhow!("the cluster answered {status}: {reason}")),
    }
}

impl Control {
    /// The cluster's answer to one request about list index `index` (narrowed to it), as JSON.
    async fn list_call(&self, id: &str, method: &str, path: &str, index: &str, body: Option<(&str, String)>) -> Result<(u16, Value)> {
        let (ct, bytes) = body.map_or((None, bytes::Bytes::new()), |(ct, b)| (Some(ct), bytes::Bytes::from(b)));
        let (status, _, out) = self.es(id, method, path, Some(index), ct, bytes).await?;
        if !(200..300).contains(&status) && status != 404 {
            return Err(refused(status, &out));
        }
        Ok((status, serde_json::from_slice(&out).unwrap_or(Value::Null)))
    }

    /// List `name`'s `_meta.roll_judgments` (`None`: no such index), refusing an index of that
    /// name that isn't a list.
    async fn list_meta(&self, id: &str, name: &str) -> Result<Option<Value>> {
        let index = index_name(name).map_err(ControlError::Invalid)?;
        let (status, v) = self.list_call(id, "GET", &format!("/{index}/_mapping"), &index, None).await?;
        if status == 404 {
            return Ok(None);
        }
        match v[&index]["mappings"]["_meta"].get(META) {
            Some(m) => Ok(Some(m.clone())),
            None => Err(ControlError::Conflict(format!("[{index}] isn't a judgment list (its mapping has no _meta.{META})"))),
        }
    }

    /// `GET …/judgments`: the cluster's lists, each with how it was built.
    pub async fn judgment_lists(&self, id: &str) -> Result<Value> {
        let pattern = format!("{PREFIX}*");
        let (_, v) = self.list_call(id, "GET", &format!("/{pattern}/_mapping?allow_no_indices=true"), &pattern, None).await?;
        let mut lists: Vec<Value> = v
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(index, m)| {
                let meta = m["mappings"]["_meta"].get(META)?;
                let name = index.strip_prefix(PREFIX)?;
                let mut row = json!({ "name": name, "index": index });
                for k in ["source", "queries", "ratings", "built_at", "clicks"] {
                    if let Some(x) = meta.get(k) {
                        row[k] = x.clone();
                    }
                }
                Some(row)
            })
            .collect();
        lists.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({ "lists": lists }))
    }

    /// List `name`'s judged queries, read page by page (at most [`MAX_STORED`] ratings).
    pub async fn judgment_list(&self, id: &str, name: &str) -> Result<(Value, Vec<Judgment>)> {
        let index = index_name(name).map_err(ControlError::Invalid)?;
        let Some(meta) = self.list_meta(id, name).await? else { return Err(ControlError::NotFound(format!("no judgment list [{name}]"))) };
        let mut ratings = Vec::new();
        let mut after: Option<Value> = None;
        loop {
            let mut body = json!({ "size": PAGE, "track_total_hits": false, "sort": [{ "key": "asc" }], "_source": ["query", "index", "doc_id", "rating"] });
            if let Some(a) = after.take() {
                body["search_after"] = a;
            }
            let (status, v) = self.list_call(id, "POST", &format!("/{index}/_search"), &index, Some(("application/json", body.to_string()))).await?;
            if status == 404 {
                return Err(ControlError::NotFound(format!("no judgment list [{name}]")));
            }
            let hits = v["hits"]["hits"].as_array().cloned().unwrap_or_default();
            for h in &hits {
                ratings.push(stored(h).map_err(|e| ControlError::Invalid(format!("[{index}]: {e}")))?);
            }
            if ratings.len() > MAX_STORED {
                return Err(ControlError::Invalid(format!("[{index}] holds more than {MAX_STORED} ratings: an evaluation takes at most {MAX_QUERIES} queries of {MAX_RATINGS}")));
            }
            if hits.len() < PAGE {
                break;
            }
            after = hits.last().map(|h| h["sort"].clone());
        }
        let judgments = group(ratings).map_err(|e| ControlError::Invalid(format!("[{index}]: {e}")))?;
        Ok((meta, judgments))
    }

    /// `PUT …/judgments/{name}` (and a build from clicks): list `name` becomes exactly `ratings`.
    /// An existing list's index is deleted and made again, so nothing of the old list stays.
    pub async fn put_judgment_list(&self, id: &str, name: &str, ratings: &[Stored], source: &str, more: Map<String, Value>) -> Result<Value> {
        let index = index_name(name).map_err(ControlError::Invalid)?;
        if ratings.len() > MAX_STORED {
            return Err(ControlError::Invalid(format!("a list holds at most {MAX_STORED} ratings")));
        }
        let queries = ratings.iter().map(|r| r.query.as_str()).collect::<std::collections::HashSet<_>>().len();
        let mut meta = json!({ "version": 1, "source": source, "queries": queries, "ratings": ratings.len(), "built_at": crate::meter::now_ms() });
        for (k, v) in more {
            meta[k] = v;
        }
        if self.list_meta(id, name).await?.is_some() {
            self.list_call(id, "DELETE", &format!("/{index}"), &index, None).await?;
        }
        self.list_call(id, "PUT", &format!("/{index}"), &index, Some(("application/json", create_body(meta).to_string()))).await?;
        for body in bulk_bodies(ratings, source) {
            let (_, v) = self.list_call(id, "POST", &format!("/{index}/_bulk"), &index, Some(("application/x-ndjson", body))).await?;
            if v["errors"] == true {
                let reason = v["items"].as_array().into_iter().flatten().find_map(|i| i["index"]["error"]["reason"].as_str()).unwrap_or("a rating wasn't stored");
                return Err(ControlError::Internal(anyhow::anyhow!("[{index}]: {reason}")));
            }
        }
        self.list_call(id, "POST", &format!("/{index}/_refresh"), &index, None).await?;
        Ok(json!({ "name": name, "index": index, "source": source, "queries": queries, "ratings": ratings.len() }))
    }

    /// `DELETE …/judgments/{name}`: the list's index goes.
    pub async fn delete_judgment_list(&self, id: &str, name: &str) -> Result<Value> {
        let index = index_name(name).map_err(ControlError::Invalid)?;
        if self.list_meta(id, name).await?.is_none() {
            return Err(ControlError::NotFound(format!("no judgment list [{name}]")));
        }
        self.list_call(id, "DELETE", &format!("/{index}"), &index, None).await?;
        Ok(json!({ "deleted": name }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_keys() {
        assert_eq!(index_name("shop-v2").unwrap(), "roll-judgments-shop-v2");
        for bad in ["", "-x", "Shop", "a/b", "a*", "a,b", "a:b", "<a-{now/d}>", "_a", &"x".repeat(65)] {
            assert!(index_name(bad).is_err(), "{bad}");
        }
        // Each part is length-prefixed: shifting a byte between parts is another key.
        assert_ne!(key("ab", "c", "d"), key("a", "bc", "d"));
    }

    #[test]
    fn a_list_reads_back_as_the_judgments_it_was_given() {
        let js = crate::evaluate::parse_judgments(Some(&json!([
            { "query": "red", "ratings": [{ "_id": "2", "rating": 1 }, { "_id": "1", "rating": 3, "_index": "other" }, { "_id": "2", "rating": 2 }] },
            { "query": "blue", "ratings": [{ "_id": "9", "rating": 0 }] },
        ])))
        .unwrap();
        let flat = flatten(&js);
        assert_eq!(flat.len(), 3, "the repeated rating is kept once, the last");
        let bodies = bulk_bodies(&flat, "manual");
        let docs: Vec<Value> = bodies[0].lines().skip(1).step_by(2).map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(docs[0], json!({ "key": key("red", "", "2"), "query": "red", "doc_id": "2", "rating": 2, "source": "manual" }));
        let hits: Vec<Value> = docs.iter().map(|d| json!({ "_id": d["key"], "_source": d })).collect();
        let back = group(hits.iter().map(|h| stored(h).unwrap()).collect()).unwrap();
        assert_eq!(judgments_json(&back), json!([
            { "query": "blue", "ratings": [{ "_id": "9", "rating": 0 }] },
            { "query": "red", "ratings": [{ "_id": "2", "rating": 2 }, { "_id": "1", "rating": 3, "_index": "other" }] },
        ]));
        let bad = json!({ "_id": "x", "_source": { "query": "red", "doc_id": "1", "rating": 101 } });
        assert!(stored(&bad).unwrap_err().contains("0 to 100"));
    }
}
