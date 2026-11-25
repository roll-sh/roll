//! The documents a query's JSON reads besides those it matches: the checks that keep such
//! queries out of places that can't read as anyone (role queries, alias filters). It walks every
//! key at every depth, so a query nested anywhere (`nested`, `script_score`, a key named
//! `params`) is seen; a field that happens to carry one of these names makes it refuse more,
//! never less.

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
}

/// Every [`DocumentRead`] of `v`, in the order met.
pub fn document_reads(v: &Value) -> Vec<DocumentRead> {
    let mut out = Vec::new();
    walk(v, &mut out);
    out
}

impl DocumentRead {
    /// Whether it reads documents of other indices than the one searched (a terms lookup, an
    /// indexed shape, a stored percolated document, `more_like_this` documents), which their
    /// reader must be allowed to read where they run.
    pub fn reads_elsewhere(self) -> bool {
        matches!(self, DocumentRead::TermsLookup | DocumentRead::IndexedShape | DocumentRead::Percolate { indexed: true } | DocumentRead::MoreLikeThisDocuments)
    }
}

/// Whether a request section kept as JSON (aggregations, a retriever, knn's filters) reads
/// documents of other indices ([`DocumentRead::reads_elsewhere`]).
pub fn reads_elsewhere(m: &crate::json::Map) -> bool {
    let mut out = Vec::new();
    m.values().for_each(|v| walk(v, &mut out));
    out.into_iter().any(DocumentRead::reads_elsewhere)
}

fn walk(v: &Value, out: &mut Vec<DocumentRead>) {
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
                walk(c, out);
            }
        }
        Value::Arr(a) => a.iter().for_each(|c| walk(c, out)),
        _ => {}
    }
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
    }
}
