//! `_id` fielddata (`IdFieldMapper.IdFieldType.fielddataBuilder`): sorting, aggregating,
//! collapsing-errors and scripting on `_id`, behind the dynamic cluster setting
//! `indices.id_field_data.enabled` (OpenSearch's default is `true`, Elasticsearch's `false`).
//!
//! `_id` has no doc values: Elasticsearch and OpenSearch load it into memory from the terms
//! index (`PagedBytesIndexFieldData`). roll does the same through search-codec's
//! [`UNINVERTED`](search_codec::UNINVERTED) field numbers: [`FIELD`] reads as sorted doc
//! values rebuilt from `_id`'s terms on first use and cached in the segment reader, so the
//! sort, aggregation and script paths treat `_id` as a single-valued keyword without any
//! per-document cost at index time.

use crate::error::EsError;
use crate::meta::ClusterState;

/// The engine field number `_id`'s fielddata reads from.
pub const FIELD: u32 = search_index::schema::ID_FIELD | search_codec::UNINVERTED;

/// `IdFieldMapper`'s refusal while `indices.id_field_data.enabled` is `false`.
pub const DISABLED: &str =
    "Fielddata access on the _id field is disallowed, you can re-enable it by updating the dynamic cluster setting: indices.id_field_data.enabled";

/// Elasticsearch's deprecation warning when `_id` fielddata loads
/// (`ProvidedIdFieldMapper.ID_FIELD_DATA_DEPRECATION_MESSAGE`).
pub const DEPRECATION: &str = "Loading the fielddata on the _id field is deprecated and will be removed in future versions. If you require sorting or aggregating on this field you should also include the id in the body of your documents, and map this field as a keyword field that has [doc_values] enabled";

/// Whether a search loads `_id` fielddata: a sort, `docvalue_fields` entry or aggregation
/// `field` naming `_id` (the coordinator warns once for what every shard would).
pub fn used_by(src: &crate::search_source::SearchSource) -> bool {
    use crate::json::Value;
    fn in_aggs(v: &Value) -> bool {
        match v {
            Value::Obj(m) => m.iter().any(|(k, v)| (k == "field" && v.as_str() == Some("_id")) || in_aggs(v)),
            Value::Arr(a) => a.iter().any(in_aggs),
            _ => false,
        }
    }
    src.sort.iter().any(|s| matches!(s, crate::search_source::Sort::Field { field, .. } if field == "_id"))
        || src.docvalue_fields.iter().any(|f| f.field == "_id")
        || src.aggs.as_ref().is_some_and(|a| a.values().any(in_aggs))
}

/// Whether `_id` fielddata may load: the cluster setting, else the dialect's default.
pub fn enabled(st: &ClusterState) -> bool {
    match st.metadata.cluster_setting("indices.id_field_data.enabled") {
        Some(v) => v == "true",
        None => crate::opensearch::active(),
    }
}

/// Whether scripts read `doc['_id']` (only OpenSearch's do, under [`enabled`]; Elasticsearch's
/// scripts refuse it whatever the setting).
pub fn scripts(st: &ClusterState) -> bool {
    crate::opensearch::active() && enabled(st)
}

/// [`DISABLED`] as an error.
pub fn disabled() -> EsError {
    EsError::illegal_argument(DISABLED)
}

/// `_id` as an aggregation field: a keyword whose type name is `_id` (named in
/// "not supported for aggregation" errors).
pub fn agg_field() -> search_aggs::AggField {
    search_aggs::AggField::new("_id", "_id", search_aggs::SourceType::Keyword, FIELD)
}

/// The `_id` of `doc` (segment-local) through the fielddata (`None`: no such doc).
pub fn value(seg: &search_index::SegmentContext, doc: u32, buf: &mut Vec<u8>) -> bool {
    let Some(dv) = seg.reader.sorted(FIELD) else { return false };
    let Some(ord) = dv.ord(doc) else { return false };
    buf.clear();
    dv.lookup_ord(ord, buf);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // The field number never collides with mapped fields or the `u32::MAX` sentinel.
    const _: () = {
        assert!(FIELD & !search_codec::UNINVERTED == search_index::schema::ID_FIELD);
        assert!(FIELD > crate::engine::fields::FIRST_FIELD);
        assert!(FIELD != u32::MAX);
    };

    #[test]
    fn default_follows_the_dialect_and_setting_overrides() {
        let mut st = ClusterState::default();
        assert!(!enabled(&st));
        assert!(crate::opensearch::with(true, || enabled(&st)));
        st.metadata.transient.set("indices.id_field_data.enabled", "false");
        assert!(!crate::opensearch::with(true, || enabled(&st)));
        st.metadata.transient.set("indices.id_field_data.enabled", "true");
        assert!(enabled(&st));
    }
}
