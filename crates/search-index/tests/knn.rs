//! kNN over quantized fields: hits come back with exact (full-precision) scores, and
//! `rescore_vector.oversample` widens the candidate pool so recall does not drop.

use std::sync::Arc;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use search_codec::{HnswParams, VectorElement, VectorQuantization, VectorSimilarity, VectorSpec};
use search_index::collector::TopScoreCollector;
use search_index::query::{KnnQuery, QueryVector};
use search_index::schema::FieldSchema;
use search_index::{IndexOp, IndexableDoc, IndexableField, QueryRef, Schema, Shard, ShardConfig};

const V: u32 = 10;
const DIMS: usize = 48;

fn dot_score(a: &[f32], b: &[f32]) -> f32 {
    // Elasticsearch's dot_product score for normalized float vectors: (1 + dot) / 2.
    let d: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    (1.0 + d) / 2.0
}

fn unit(rng: &mut StdRng) -> Vec<f32> {
    let v: Vec<f32> = (0..DIMS).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.into_iter().map(|x| x / n).collect()
}

fn recall(s: &search_index::Searcher, q: KnnQuery, want: &[u32]) -> (usize, Vec<(u32, f32)>) {
    let q: QueryRef = Arc::new(q);
    let mut c = TopScoreCollector::new(want.len(), u64::MAX);
    s.search(&q, &mut c).unwrap();
    let hits: Vec<(u32, f32)> = c.top_docs().hits.iter().map(|h| (h.doc, h.score)).collect();
    (hits.iter().filter(|h| want.contains(&h.0)).count(), hits)
}

#[test]
fn quantized_knn_scores_exactly_and_oversampling_keeps_recall() {
    for quantization in [VectorQuantization::Binary, VectorQuantization::Scalar { bits: 4, confidence_interval: None }] {
        let schema = Schema::new()
            .with_field(FieldSchema::new(V, "v").with_vectors(VectorSpec {
                dims: DIMS as u32,
                element: VectorElement::Float32,
                similarity: VectorSimilarity::DotProduct,
                hnsw: Some(HnswParams::default()),
                quantization,
            }))
            .unwrap();
        let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema, Vec::new()).unwrap();
        let mut rng = StdRng::seed_from_u64(5);
        let mut vecs = Vec::new();
        for i in 0..3000u32 {
            let v = unit(&mut rng);
            let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            let doc = IndexableDoc::new(format!("{i}").into_bytes()).field(IndexableField::Vector { field: V, value: bytes.into() });
            shard.index(IndexOp::new(doc)).unwrap();
            vecs.push(v);
        }
        shard.refresh().unwrap();
        shard.force_merge(Some(1), false).unwrap();
        let s = shard.searcher();
        let (mut plain, mut over) = (0, 0);
        for _ in 0..20 {
            let q = unit(&mut rng);
            let mut exact: Vec<(u32, f32)> = vecs.iter().enumerate().map(|(i, v)| (i as u32, dot_score(&q, v))).collect();
            exact.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            let want: Vec<u32> = exact.iter().take(10).map(|x| x.0).collect();
            let (r, hits) = recall(&s, KnnQuery::new(V, QueryVector::Float(q.clone()), 10, 12), &want);
            plain += r;
            for (d, score) in hits {
                // Full-precision scores, whatever the quantization.
                assert!((score - dot_score(&q, &vecs[d as usize])).abs() < 1e-5, "{quantization:?}: doc {d} score {score}");
            }
            let mut k = KnnQuery::new(V, QueryVector::Float(q.clone()), 10, 12);
            k.oversample = Some(3.0);
            over += recall(&s, k, &want).0;
        }
        // Random unit vectors are the hard case for quantization; widening the pool must pay.
        assert!(over > plain + 20 && over >= 120, "{quantization:?}: recall {plain}/200, oversampled {over}/200");
    }
}

/// Vectors on nested docs: the best child of each of the top `k` parents, exactly (brute
/// force) and approximately (recall), with deletes and several segments.
#[test]
fn nested_knn_diversifies_by_parent() {
    use search_index::query::nested::root_filter;
    let schema = Schema::new()
        .with_field(FieldSchema::new(V, "v").with_vectors(VectorSpec {
            dims: DIMS as u32,
            element: VectorElement::Float32,
            similarity: VectorSimilarity::DotProduct,
            hnsw: Some(HnswParams::default()),
            quantization: VectorQuantization::None,
        }))
        .unwrap();
    let mut cfg = ShardConfig { merge_threads: 0, ..Default::default() };
    cfg.merge_policy.segments_per_tier = 1000.0;
    let shard = Shard::open(cfg, schema, Vec::new()).unwrap();
    let mut rng = StdRng::seed_from_u64(9);
    // parent id -> its children's vectors
    let mut parents: Vec<Vec<Vec<f32>>> = Vec::new();
    for p in 0..600u32 {
        let mut doc = IndexableDoc::new(format!("p{p}").into_bytes());
        let mut kids = Vec::new();
        for _ in 0..rng.gen_range(0..5) {
            let v = unit(&mut rng);
            let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
            doc = doc.child(IndexableDoc::new(Vec::new()).field(IndexableField::Vector { field: V, value: bytes.into() }));
            kids.push(v);
        }
        shard.index(IndexOp::new(doc)).unwrap();
        parents.push(kids);
        if p % 150 == 149 {
            shard.refresh().unwrap();
        }
    }
    let mut deleted = std::collections::HashSet::new();
    for _ in 0..40 {
        let p = rng.gen_range(0..600u32);
        shard.delete(search_index::DeleteOp::new(format!("p{p}").into_bytes())).unwrap();
        deleted.insert(p);
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    assert!(s.segments().len() >= 4);
    let mut found = 0;
    for _ in 0..15 {
        let q = unit(&mut rng);
        let k = 10;
        // Brute force: best child score per live parent.
        let mut want: Vec<(u32, f32)> = parents
            .iter()
            .enumerate()
            .filter(|(p, kids)| !kids.is_empty() && !deleted.contains(&(*p as u32)))
            .map(|(p, kids)| (p as u32, kids.iter().map(|v| dot_score(&q, v)).fold(f32::MIN, f32::max)))
            .collect();
        want.sort_by(|a, b| b.1.total_cmp(&a.1));
        want.truncate(k);
        for exact in [true, false] {
            let mut kq = KnnQuery::new(V, QueryVector::Float(q.clone()), k, 40);
            kq.exact = exact;
            kq.parent_filter = Some(root_filter());
            let hits = kq.search(&s).unwrap();
            assert_eq!(hits.len(), k, "one child per parent, k parents");
            // Map each child hit to its parent id through the stored parent of the block.
            let mut got: Vec<(String, f32)> = hits
                .iter()
                .map(|&(child, score)| {
                    let parent = (child..s.max_doc()).find(|&d| s.stored(d).map(|f| !f.is_empty()).unwrap_or(false)).unwrap();
                    let id = s.stored(parent).unwrap().into_iter().find(|(n, _)| *n == search_index::schema::ID_FIELD).unwrap().1;
                    let search_codec::stored::OwnedStoredValue::Bytes(b) = id else { panic!() };
                    (String::from_utf8(b).unwrap(), score)
                })
                .collect();
            got.sort_by(|a, b| b.1.total_cmp(&a.1));
            let parents_got: std::collections::HashSet<&str> = got.iter().map(|g| g.0.as_str()).collect();
            assert_eq!(parents_got.len(), k, "distinct parents");
            if exact {
                for ((gid, gs), (wp, ws)) in got.iter().zip(&want) {
                    assert!((gs - ws).abs() < 1e-6, "exact: {gid}@{gs} vs p{wp}@{ws}");
                }
            } else {
                let want_ids: std::collections::HashSet<String> = want.iter().map(|w| format!("p{}", w.0)).collect();
                found += parents_got.iter().filter(|p| want_ids.contains(**p)).count();
            }
        }
    }
    assert!(found >= 140, "approximate nested recall {found}/150");
}

/// Scores and texts of the `knn` query (Lucene's `DocAndScoreQuery`) and of the top-level `knn`
/// section's `KnnScoreDocQuery`, as Elasticsearch 9.5.5 explains and profiles them (cosine,
/// vectors [1,0,0], [0.5,0.5,0], [0,0,1], query [1,0.2,0], k 2).
#[test]
fn knn_explanations_match_elasticsearch() {
    use search_index::query::{BoostQuery, ScoreDocQuery};
    let schema = Schema::new()
        .with_field(FieldSchema::new(V, "v").with_vectors(VectorSpec {
            dims: 3,
            element: VectorElement::Float32,
            similarity: VectorSimilarity::Cosine,
            hnsw: Some(HnswParams::default()),
            quantization: VectorQuantization::None,
        }))
        .unwrap();
    let shard = Shard::open(ShardConfig { merge_threads: 0, ..Default::default() }, schema, Vec::new()).unwrap();
    for (i, v) in [[1.0f32, 0.0, 0.0], [0.5, 0.5, 0.0], [0.0, 0.0, 1.0]].iter().enumerate() {
        let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
        shard.index(IndexOp::new(IndexableDoc::new(format!("{i}").into_bytes()).field(IndexableField::Vector { field: V, value: bytes.into() }))).unwrap();
    }
    shard.refresh().unwrap();
    let s = shard.searcher();
    let knn: QueryRef = Arc::new(KnnQuery::new(V, QueryVector::Float(vec![1.0, 0.2, 0.0]), 2, 10));
    let (_, hits) = recall(&s, KnnQuery::new(V, QueryVector::Float(vec![1.0, 0.2, 0.0]), 2, 10), &[0, 1]);
    assert_eq!(hits, vec![(0, 0.99029034), (1, 0.91602516)]);
    let e = s.explain(&knn, 0).unwrap();
    assert_eq!((e.value(), e.description.as_str()), (0.99029034, "within top 2 docs"));
    let e = s.explain(&knn, 2).unwrap();
    assert!(!e.is_match());
    assert_eq!(e.description, "not in top 2 docs");
    let boosted: QueryRef = Arc::new(BoostQuery::new(knn.clone(), 2.0));
    assert_eq!(s.explain(&boosted, 0).unwrap().value(), 1.9805807);
    assert_eq!(s.rewrite(&knn).unwrap().lucene_string(s.schema()), "DocAndScoreQuery[0,...][0.99029034,...],0.99029034");
    // The top-level section's merged hits.
    let top: QueryRef = Arc::new(ScoreDocQuery::new(hits));
    assert_eq!(s.explain(&top, 1).unwrap().description, "within top k documents");
    assert_eq!(s.explain(&top, 2).unwrap().description, "not in top k documents");
    assert_eq!(top.lucene_string(s.schema()), "ScoreAndDocQuery");
}
