//! The documents a query's JSON reads besides those it matches: the checks that keep such
//! queries out of places that can't read as anyone (role queries, alias filters). It walks every
//! key at every depth, so a query nested anywhere (`nested`, `script_score`, a key named
//! `params`) is seen, and a `wrapper`'s base64 query is decoded and walked too; a field that
//! happens to carry one of these names makes it refuse more, never less.

use crate::json::Value;

/// A query that reads documents other than those it matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentRead {
    /// `terms` with a lookup (by `id`, or OpenSearch's by `query`).
    TermsLookup,
    /// `geo_shape` or `shape` with an `indexed_shape`.
    IndexedShape,
    /// `percolate`; `indexed`: of a stored document (`index`/`id`) rather than given ones.
    Percolate { indexed: bool },
    /// `more_like_this` whose `like` or `unlike` names documents (not only texts).
    MoreLikeThisDocuments,
    HasChild,
    HasParent,
    /// A `wrapper` whose query doesn't decode, or nesting past [`MAX_DEPTH`]: what it reads
    /// isn't known, so it counts as reading elsewhere.
    Opaque,
}

/// How deep the walk goes, wrapped queries included (one budget for all of them: the JSON
/// parser's own limit is per document, and wrappers nest documents).
const MAX_DEPTH: usize = 1000;

/// Every [`DocumentRead`] of `v`, in the order met.
pub fn document_reads(v: &Value) -> Vec<DocumentRead> {
    let mut out = Vec::new();
    walk(v, 0, &mut out);
    out
}

impl DocumentRead {
    /// Whether it reads documents of other indices than the one searched (a terms lookup, an
    /// indexed shape, a stored percolated document, `more_like_this` documents), which their
    /// reader must be allowed to read where they run.
    pub fn reads_elsewhere(self) -> bool {
        matches!(self, DocumentRead::TermsLookup | DocumentRead::IndexedShape | DocumentRead::Percolate { indexed: true } | DocumentRead::MoreLikeThisDocuments | DocumentRead::Opaque)
    }
}

/// Whether a request section kept as JSON (aggregations, a retriever, knn's filters) reads
/// documents of other indices ([`DocumentRead::reads_elsewhere`]).
pub fn reads_elsewhere(m: &crate::json::Map) -> bool {
    let mut out = Vec::new();
    m.values().for_each(|v| walk(v, 1, &mut out));
    out.into_iter().any(DocumentRead::reads_elsewhere)
}

fn walk(v: &Value, depth: usize, out: &mut Vec<DocumentRead>) {
    if depth > MAX_DEPTH {
        out.push(DocumentRead::Opaque);
        return;
    }
    // A field of the query's body whose value is an object holding `key`.
    let field_holds = |c: &Value, keys: &[&str]| c.as_object().is_some_and(|m| m.values().any(|x| x.as_object().is_some_and(|o| keys.iter().any(|k| o.contains_key(*k)))));
    let names_documents = |c: &Value| match c {
        Value::Obj(_) => true,
        Value::Arr(a) => a.iter().any(|x| matches!(x, Value::Obj(_))),
        _ => false,
    };
    match v {
        Value::Obj(m) => {
            for (k, c) in m {
                let found = match k.as_str() {
                    "terms" if field_holds(c, &["index", "id", "path"]) => Some(DocumentRead::TermsLookup),
                    "geo_shape" | "shape" if field_holds(c, &["indexed_shape"]) => Some(DocumentRead::IndexedShape),
                    "percolate" if c.as_object().is_some() => Some(DocumentRead::Percolate { indexed: c.get("index").is_some() || c.get("id").is_some() }),
                    "more_like_this" | "mlt" if c.get("like").is_some_and(names_documents) || c.get("unlike").is_some_and(names_documents) => Some(DocumentRead::MoreLikeThisDocuments),
                    "has_child" if c.as_object().is_some() => Some(DocumentRead::HasChild),
                    "has_parent" if c.as_object().is_some() => Some(DocumentRead::HasParent),
                    _ => None,
                };
                out.extend(found);
                if k == "wrapper" {
                    match wrapped(c) {
                        Some(inner) => walk(&inner, depth + 1, out),
                        None => out.push(DocumentRead::Opaque),
                    }
                    continue;
                }
                walk(c, depth + 1, out);
            }
        }
        Value::Arr(a) => a.iter().for_each(|c| walk(c, depth + 1, out)),
        _ => {}
    }
}

/// A `wrapper`'s query (`{"query": "<base64 JSON>"}`), decoded and parsed as the query parser
/// does ([`super::specialized::Wrapper`]); `None` when it doesn't decode or parse.
fn wrapped(c: &Value) -> Option<Value> {
    use base64::Engine as _;
    let text = c.get("query")?.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(text.as_bytes()).ok()?;
    crate::json::parse(&bytes).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_are_found_at_any_depth() {
        let q = |s: &str| crate::json::parse(s.as_bytes()).unwrap().unwrap();
        let lookup = r#"{"terms":{"n":{"index":"secret","id":"1","path":"ids"}}}"#;
        for wrapped in [
            format!(r#"{{"nested":{{"path":"p","query":{lookup}}}}}"#),
            format!(r#"{{"script_score":{{"query":{lookup},"script":"1"}}}}"#),
            format!(r#"{{"bool":{{"filter":[{{"constant_score":{{"filter":{lookup}}}}}]}}}}"#),
        ] {
            assert_eq!(document_reads(&q(&wrapped)), [DocumentRead::TermsLookup], "{wrapped}");
        }
        assert_eq!(document_reads(&q(r#"{"more_like_this":{"fields":["k"],"like":[{"_index":"secret","_id":"1"}]}}"#)), [DocumentRead::MoreLikeThisDocuments]);
        assert!(document_reads(&q(r#"{"more_like_this":{"fields":["k"],"like":["some text"]}}"#)).is_empty());
        assert_eq!(document_reads(&q(r#"{"percolate":{"field":"q","document":{"a":1}}}"#)), [DocumentRead::Percolate { indexed: false }]);
        assert!(document_reads(&q(r#"{"terms":{"n":["a","b"]}}"#)).is_empty());
        use base64::Engine as _;
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let wrapper = format!(r#"{{"wrapper":{{"query":"{}"}}}}"#, b64(&format!(r#"{{"wrapper":{{"query":"{}"}}}}"#, b64(lookup))));
        assert_eq!(document_reads(&q(&wrapper)), [DocumentRead::TermsLookup], "a wrapper's query, wrapped twice");
        assert_eq!(document_reads(&q(r#"{"wrapper":{"query":"not base64!"}}"#)), [DocumentRead::Opaque]);
    }
}
