//! Ties at the edge of a sorted page: Lucene breaks them on doc ids, so the earliest docs of
//! a tied value fill the page (found by the differential suite: a size-1 sort with
//! `missing: _first` returned the last missing doc).

use search_codec::{DocValuesType, PointsSpec};
use search_index::collector::{FieldSort, Missing, SortKey, SortMode, SortType, TopFieldCollector};
use search_index::doc::{DocValue, IndexableDoc};
use search_index::query::MatchAllQuery;
use search_index::schema::FieldSchema;
use search_index::{IndexOp, QueryRef, Shard, ShardConfig};
use std::sync::Arc;

const F: u32 = 40;

/// One segment of `values` (`None`: the doc has no value), docs `0..n` in order.
fn shard(values: &[Option<i64>]) -> Shard {
    let schema = search_index::Schema::new().with_field(FieldSchema::new(F, "f").with_points(PointsSpec::new(1, 8)).with_doc_values(DocValuesType::SortedNumeric)).unwrap();
    let s = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema, Vec::new()).unwrap();
    for (i, v) in values.iter().enumerate() {
        let mut doc = IndexableDoc::new(format!("d{i}").into_bytes());
        if let Some(v) = v {
            doc = doc.point(F, search_codec::points::encode::i64_bytes(*v).to_vec()).doc_value(F, DocValue::SortedNumeric(*v));
        }
        s.index(IndexOp::new(doc)).unwrap();
    }
    s.refresh().unwrap();
    s
}

fn top(s: &Shard, n: usize, threshold: u64, reverse: bool, missing: Missing) -> Vec<u32> {
    let se = s.searcher();
    let key = SortKey::Field(FieldSort { field: F, kind: SortType::Long, reverse, missing, mode: if reverse { SortMode::Max } else { SortMode::Min }, nested: None });
    let mut c = TopFieldCollector::new(vec![key], n, threshold, false).with_searcher(&se);
    let q: QueryRef = Arc::new(MatchAllQuery);
    se.search(&q, &mut c).unwrap();
    c.top_docs().hits.iter().map(|h| h.doc).collect()
}

#[test]
fn tied_missing_values_keep_the_earliest_docs() {
    let s = shard(&[None, Some(28), None, Some(5), None]);
    for threshold in [u64::MAX, 1] {
        assert_eq!(top(&s, 1, threshold, false, Missing::First), vec![0], "asc first, threshold {threshold}");
        assert_eq!(top(&s, 2, threshold, false, Missing::First), vec![0, 2], "asc first, threshold {threshold}");
        assert_eq!(top(&s, 1, threshold, true, Missing::First), vec![0], "desc first, threshold {threshold}");
        assert_eq!(top(&s, 4, threshold, false, Missing::Last), vec![3, 1, 0, 2], "asc last, threshold {threshold}");
        assert_eq!(top(&s, 3, threshold, true, Missing::Last), vec![1, 3, 0], "desc last, threshold {threshold}");
    }
}

#[test]
fn tied_values_keep_the_earliest_docs() {
    let s = shard(&[Some(7), Some(3), Some(7), Some(7), Some(1)]);
    for threshold in [u64::MAX, 1] {
        assert_eq!(top(&s, 2, threshold, false, Missing::Last), vec![4, 1], "threshold {threshold}");
        assert_eq!(top(&s, 3, threshold, false, Missing::Last), vec![4, 1, 0], "threshold {threshold}");
        assert_eq!(top(&s, 2, threshold, true, Missing::Last), vec![0, 2], "threshold {threshold}");
    }
}
