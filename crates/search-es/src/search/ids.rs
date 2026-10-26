//! Queries that match like one of roll's but print like Elasticsearch's, where the two index
//! differently.
//!
//! - The `ids` query (and `term(s)` on `_id`): Elasticsearch's `IdFieldMapper.termsQuery`, a
//!   `TermInSetQuery` over `_id`. Roll keeps `_id` terms as the ids' UTF-8 bytes;
//!   Elasticsearch indexes `Uid.encodeId(id)` (numeric ids two digits a byte behind `0xfe`,
//!   URL-safe base64 ids decoded, anything else UTF-8 behind `0xff`), so its explanations and
//!   profiles read `_id:([fe 2f])`: the encoded terms sorted, printed as Lucene `BytesRef`s.
//! - `exists` on fields without doc values or norms (completion, binary without doc values):
//!   Elasticsearch looks the field name up in `_field_names`; roll keeps those names in its
//!   `_feature` field (`engine::fields::FEATURE_FIELD`), so the term prints `_field_names:f`.
//!
//! - `wildcard` fields: Elasticsearch confirms an n-gram approximation against binary doc
//!   values (`BinaryDvConfirmedQuery`), printed `: [value]` or
//!   `:PatternAutomatonProvider[…]` (see `extra::wildcard_field_query`); roll indexes the
//!   whole values as terms.
//!
//! [`Described`] only changes how the query prints; scoring, counting and rewriting are the
//! wrapped query's. Its explanation is a constant score's: it only wraps constant-scoring
//! queries, or ones a constant score wraps.

use std::any::Any;

use search_index::query::basic::explain_const;
use search_index::query::term::term_text;
use search_index::query::{ConstantScoreQuery, MultiTermQuery, TermQuery};
use search_index::search::{BulkScorer, Explanation, Query, QueryRef, QueryVisitor, ScoreMode, Scorer, ScorerSupplier, Searcher, SegmentContext, Weight};
use search_index::{Result, Schema};
use std::sync::Arc;

/// A query printed (`toString`, explanations, profile descriptions) as `desc`.
#[derive(Debug)]
pub struct Described {
    inner: QueryRef,
    profile: String,
    desc: String,
}

/// `inner` (a constant-scoring query) printed as `desc`, profiled as `profile`.
pub fn described(inner: QueryRef, profile: &str, desc: String) -> QueryRef {
    Arc::new(Described { inner, profile: profile.to_string(), desc })
}

/// A constant-score `_id` term set printed like Elasticsearch's. `ids` must not be empty (the
/// caller answers an empty list with `match_none`).
pub fn ids(ids: &[String]) -> QueryRef {
    let mtq = MultiTermQuery::set(search_index::schema::ID_FIELD, ids.iter().map(|s| s.as_bytes().to_vec()));
    Arc::new(Described { profile: mtq.profile_name(), desc: describe(ids), inner: Arc::new(ConstantScoreQuery::new(Arc::new(mtq))) })
}

/// The `_field_names` term of `field` (Elasticsearch's `exists` on a field without doc values
/// or norms), which roll indexes in `_feature`.
pub fn field_names_term(field: &str) -> QueryRef {
    let tq = TermQuery::new(crate::engine::fields::FEATURE_FIELD, field.as_bytes().to_vec());
    Arc::new(Described { profile: tq.profile_name(), desc: format!("_field_names:{field}"), inner: Arc::new(tq) })
}

/// `TermInSetQuery.toString` over the encoded ids: sorted, deduplicated, each a `BytesRef`
/// (hex unless the bytes are valid UTF-8, as `Term.toString` prints them).
fn describe(ids: &[String]) -> String {
    let mut enc: Vec<Vec<u8>> = ids.iter().map(|id| super::encode_uid(id)).collect();
    enc.sort();
    enc.dedup();
    let parts: Vec<String> = enc.iter().map(|t| term_text(t)).collect();
    format!("_id:({})", parts.join(" "))
}

struct DescribedWeight {
    inner: Box<dyn Weight>,
    desc: String,
    score: f32,
}

impl Weight for DescribedWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        self.inner.scorer(seg)
    }
    fn scorer_supplier(&self, seg: &SegmentContext) -> Result<Option<Box<dyn ScorerSupplier>>> {
        self.inner.scorer_supplier(seg)
    }
    fn bulk_scorer(&self, seg: &SegmentContext, mode: ScoreMode) -> Result<Option<Box<dyn BulkScorer>>> {
        self.inner.bulk_scorer(seg, mode)
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
    fn count(&self, seg: &SegmentContext) -> Result<Option<u32>> {
        self.inner.count(seg)
    }
}

impl Query for Described {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        Ok(Box::new(DescribedWeight { inner: self.inner.weight(s, mode, boost)?, desc: self.desc.clone(), score: boost }))
    }
    fn lucene_string(&self, _schema: &Schema) -> String {
        self.desc.clone()
    }
    fn visit(&self, v: &mut dyn QueryVisitor) {
        self.inner.visit(v)
    }
    fn cache_identity(&self, h: &mut dyn std::hash::Hasher) -> bool {
        h.write(b"described");
        self.inner.cache_identity(h)
    }
    fn profile_name(&self) -> String {
        self.profile.clone()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Descriptions Elasticsearch 9.5.5 printed for these ids (`explain: true`).
    #[test]
    fn prints_encoded_ids_like_elasticsearch() {
        assert_eq!(describe(&["2".into()]), "_id:([fe 2f])");
        let ids: Vec<String> = ["abc", "2", "AbCd", "x y", "007", "zz"].iter().map(|s| s.to_string()).collect();
        assert_eq!(describe(&ids), "_id:([1 b0 9d] [69 b7] [fe 0 7f] [fe 2f] [ff 78 20 79] [ff 7a 7a])");
        assert_eq!(describe(&["abc".into(), "2".into(), "abc".into()]), "_id:([69 b7] [fe 2f])");
    }
}
