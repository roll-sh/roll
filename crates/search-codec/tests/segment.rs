//! End-to-end: an in-memory `SegmentSource` (the shape search-index's buffer will have) written
//! with `write_segment`, read back with `SegmentReader`, merged with deletions, index-sorted and
//! checked for corruption detection.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rand::{Rng, SeedableRng, rngs::StdRng};
use search_codec::points::encode;
use search_codec::stored::OwnedStoredValue;
use search_codec::*;

const BODY: u32 = 1;
const TAG: u32 = 2;
const NUM: u32 = 3;
const PRICE: u32 = 4;
const BIN: u32 = 5;
const CAT: u32 = 6;
const LOC: u32 = 7;
const VEC: u32 = 8;
const ID: u32 = 9;
const SUG: u32 = 10;

/// Suggest entries of a doc: one per tag, weighted by price, with the id as payload.
fn suggest_entries(d: &Doc, doc: u32) -> Vec<(Vec<u8>, u64, u32, Vec<u8>)> {
    d.tags
        .iter()
        .map(|t| (t.as_bytes().to_vec(), d.price.map_or(0, |p| (p / 100 % 997) as u64), doc, d.id.as_bytes().to_vec()))
        .collect()
}

#[derive(Clone, Debug)]
struct Tok {
    term: String,
    pos: u32,
    start: u32,
    end: u32,
    payload: Vec<u8>,
}

#[derive(Clone, Debug)]
struct Doc {
    id: String,
    body: Vec<Tok>,
    tags: Vec<String>,
    nums: Vec<i64>,
    price: Option<i64>,
    bin: Option<Vec<u8>>,
    cat: Option<String>,
    loc: Option<(i32, i32)>,
    vec: Option<Vec<f32>>,
}

fn random_doc(rng: &mut StdRng, i: usize) -> Doc {
    let words = [
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
        "lambda",
    ];
    let n = rng.gen_range(0..30);
    let mut body = Vec::new();
    let mut off = 0;
    let mut pos = 0;
    for _ in 0..n {
        let w = if rng.gen_bool(0.1) {
            format!("rare{}", rng.gen_range(0..500))
        } else {
            words[rng.gen_range(0..words.len())].to_string()
        };
        let start = off;
        off += w.len() as u32 + 1;
        let payload = if rng.gen_bool(0.2) {
            vec![rng.r#gen::<u8>(); rng.gen_range(1..4)]
        } else {
            Vec::new()
        };
        body.push(Tok {
            term: w.clone(),
            pos,
            start,
            end: start + w.len() as u32,
            payload,
        });
        pos += rng.gen_range(1..3);
    }
    Doc {
        id: format!("doc-{i}"),
        body,
        tags: (0..rng.gen_range(0..4))
            .map(|_| format!("t{}", rng.gen_range(0..40)))
            .collect(),
        nums: (0..rng.gen_range(0..3))
            .map(|_| rng.gen_range(-1000..1000))
            .collect(),
        price: rng.gen_bool(0.8).then(|| rng.gen_range(0..100_000) * 100),
        bin: rng
            .gen_bool(0.5)
            .then(|| (0..rng.gen_range(0..10)).map(|_| rng.r#gen()).collect()),
        cat: rng
            .gen_bool(0.9)
            .then(|| format!("cat{}", rng.gen_range(0..20))),
        loc: rng
            .gen_bool(0.7)
            .then(|| (rng.gen_range(-90..90), rng.gen_range(-180..180))),
        vec: rng
            .gen_bool(0.9)
            .then(|| (0..8).map(|_| rng.gen_range(-1.0..1.0)).collect()),
    }
}

fn fields() -> Vec<FieldInfo> {
    let mut body = FieldInfo::new(BODY, "body")
        .with_index(IndexOptions::DocsAndFreqsAndPositionsAndOffsets, true);
    body.payloads = true;
    vec![
        body,
        FieldInfo::new(TAG, "tag")
            .with_index(IndexOptions::Docs, false)
            .with_doc_values(DocValuesType::SortedSet),
        FieldInfo::new(NUM, "num")
            .with_doc_values(DocValuesType::SortedNumeric)
            .with_points(PointsSpec::new(1, 8)),
        FieldInfo::new(PRICE, "price").with_doc_values(DocValuesType::Numeric),
        FieldInfo::new(BIN, "bin").with_doc_values(DocValuesType::Binary),
        FieldInfo::new(CAT, "cat")
            .with_index(IndexOptions::DocsAndFreqs, false)
            .with_doc_values(DocValuesType::Sorted),
        FieldInfo::new(LOC, "loc").with_points(PointsSpec::new(2, 4)),
        FieldInfo::new(VEC, "vec").with_vectors(VectorSpec {
            dims: 8,
            element: VectorElement::Float32,
            similarity: VectorSimilarity::Cosine,
            hnsw: Some(HnswParams {
                m: 8,
                ef_construction: 50,
            }),
            quantization: VectorQuantization::Scalar {
                bits: 8,
                confidence_interval: None,
            },
        }),
        FieldInfo::new(ID, "_id").with_index(IndexOptions::Docs, false).with_bloom_filter(),
        FieldInfo::new(SUG, "suggest").with_suggest(),
    ]
}

type Postings = BTreeMap<Vec<u8>, Vec<(u32, Vec<Tok>)>>;

/// An in-memory segment source over `docs`.
struct MemSource {
    docs: Vec<Doc>,
    id: [u8; 16],
    sort: Option<IndexSort>,
    inverted: BTreeMap<u32, Postings>,
    /// Emit norms, doc values and vectors in a shuffled doc order (seed).
    shuffle: Option<u64>,
}

impl MemSource {
    fn new(docs: Vec<Doc>, id: u8, sort: Option<IndexSort>) -> MemSource {
        let mut inverted: BTreeMap<u32, Postings> = BTreeMap::new();
        for (d, doc) in docs.iter().enumerate() {
            for t in &doc.body {
                let e = inverted
                    .entry(BODY)
                    .or_default()
                    .entry(t.term.as_bytes().to_vec())
                    .or_default();
                match e.last_mut() {
                    Some((ld, toks)) if *ld == d as u32 => toks.push(t.clone()),
                    _ => e.push((d as u32, vec![t.clone()])),
                }
            }
            let tags: BTreeSet<&String> = doc.tags.iter().collect();
            for t in tags {
                inverted
                    .entry(TAG)
                    .or_default()
                    .entry(t.as_bytes().to_vec())
                    .or_default()
                    .push((d as u32, Vec::new()));
            }
            if let Some(c) = &doc.cat {
                inverted
                    .entry(CAT)
                    .or_default()
                    .entry(c.as_bytes().to_vec())
                    .or_default()
                    .push((d as u32, Vec::new()));
            }
            inverted
                .entry(ID)
                .or_default()
                .entry(doc.id.as_bytes().to_vec())
                .or_default()
                .push((d as u32, Vec::new()));
        }
        MemSource {
            docs,
            id: [id; 16],
            sort,
            inverted,
            shuffle: None,
        }
    }

    fn shuffle_per_doc_values(&mut self, rng: &mut StdRng) {
        self.shuffle = Some(rng.r#gen());
    }

    fn maybe_shuffle<T>(&self, field: u32, mut v: Vec<T>) -> Vec<T> {
        if let Some(seed) = self.shuffle {
            use rand::seq::SliceRandom;
            v.shuffle(&mut StdRng::seed_from_u64(seed ^ field as u64));
        }
        v
    }
}

struct MemTerms<'a> {
    it: std::collections::btree_map::Iter<'a, Vec<u8>, Vec<(u32, Vec<Tok>)>>,
    cur: Option<&'a Vec<(u32, Vec<Tok>)>>,
    p: MemPostings<'a>,
}

struct MemPostings<'a> {
    list: &'a [(u32, Vec<Tok>)],
    i: usize,
    pos: usize,
    started: bool,
}

impl PostingsSource for MemPostings<'_> {
    fn next_doc(&mut self) -> u32 {
        if self.started {
            self.i += 1;
        }
        self.started = true;
        self.pos = 0;
        self.list.get(self.i).map_or(NO_MORE_DOCS, |x| x.0)
    }
    fn freq(&mut self) -> u32 {
        self.list[self.i].1.len().max(1) as u32
    }
    fn next_position(&mut self) -> u32 {
        self.pos += 1;
        self.list[self.i].1[self.pos - 1].pos
    }
    fn offsets(&self) -> (u32, u32) {
        let t = &self.list[self.i].1[self.pos - 1];
        (t.start, t.end)
    }
    fn payload(&self) -> &[u8] {
        &self.list[self.i].1[self.pos - 1].payload
    }
}

impl TermsSource for MemTerms<'_> {
    fn next_term(&mut self) -> Result<Option<&[u8]>> {
        match self.it.next() {
            None => Ok(None),
            Some((t, l)) => {
                self.cur = Some(l);
                Ok(Some(t))
            }
        }
    }
    fn postings(&mut self) -> Result<&mut dyn PostingsSource> {
        self.p = MemPostings {
            list: self.cur.unwrap(),
            i: 0,
            pos: 0,
            started: false,
        };
        Ok(&mut self.p)
    }
}

struct VecBytes(Vec<(u32, Vec<u8>)>, usize);
impl BytesSource for VecBytes {
    fn next(&mut self) -> Option<(u32, &[u8])> {
        let i = self.1;
        self.1 += 1;
        self.0.get(i).map(|(d, b)| (*d, b.as_slice()))
    }
}

struct MemSorted {
    dict: Vec<Vec<u8>>,
    docs: Vec<(u32, Vec<u64>)>,
    i: usize,
}

impl SortedSource for MemSorted {
    fn value_count(&self) -> u64 {
        self.dict.len() as u64
    }
    fn value(&mut self, ord: u64) -> &[u8] {
        &self.dict[ord as usize]
    }
    fn next_doc(&mut self) -> Option<(u32, u64)> {
        let i = self.i;
        self.i += 1;
        self.docs.get(i).map(|(d, o)| (*d, o[0]))
    }
}

impl SortedSetSource for MemSorted {
    fn value_count(&self) -> u64 {
        self.dict.len() as u64
    }
    fn value(&mut self, ord: u64) -> &[u8] {
        &self.dict[ord as usize]
    }
    fn next_doc(&mut self) -> Option<(u32, &[u64])> {
        let i = self.i;
        self.i += 1;
        self.docs.get(i).map(|(d, o)| (*d, o.as_slice()))
    }
}

struct MemSortedNumeric(Vec<(u32, Vec<i64>)>, usize);
impl SortedNumericSource for MemSortedNumeric {
    fn next_doc(&mut self) -> Option<(u32, &[i64])> {
        let i = self.1;
        self.1 += 1;
        self.0.get(i).map(|(d, v)| (*d, v.as_slice()))
    }
}

fn dict_source(per_doc: Vec<(u32, Vec<String>)>) -> MemSorted {
    let set: BTreeSet<&String> = per_doc.iter().flat_map(|(_, v)| v.iter()).collect();
    let dict: Vec<Vec<u8>> = set.iter().map(|s| s.as_bytes().to_vec()).collect();
    let docs = per_doc
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(d, v)| {
            (
                *d,
                v.iter()
                    .rev()
                    .map(|s| dict.binary_search(&s.as_bytes().to_vec()).unwrap() as u64)
                    .collect(),
            )
        })
        .collect();
    MemSorted { dict, docs, i: 0 }
}

impl SegmentSource for MemSource {
    fn segment_id(&self) -> [u8; 16] {
        self.id
    }
    fn max_doc(&self) -> u32 {
        self.docs.len() as u32
    }
    fn field_infos(&self) -> Vec<FieldInfo> {
        fields()
    }
    fn index_sort(&self) -> Option<IndexSort> {
        self.sort.clone()
    }
    fn terms(&self, field: u32) -> Result<Option<Box<dyn TermsSource + '_>>> {
        Ok(self.inverted.get(&field).map(|p| {
            Box::new(MemTerms {
                it: p.iter(),
                cur: None,
                p: MemPostings {
                    list: &[],
                    i: 0,
                    pos: 0,
                    started: false,
                },
            }) as Box<dyn TermsSource>
        }))
    }
    fn norms(&self, field: u32) -> Result<Option<search_codec::source::NormsIter<'_>>> {
        if field != BODY {
            return Ok(None);
        }
        let v: Vec<(u32, u8)> = self
            .docs
            .iter()
            .enumerate()
            .filter(|(_, d)| !d.body.is_empty())
            .map(|(i, d)| (i as u32, int_to_byte4(d.body.len() as u32)))
            .collect();
        Ok(Some(Box::new(self.maybe_shuffle(field, v).into_iter())))
    }
    fn doc_values(&self, field: u32) -> Result<Option<DocValuesSource<'_>>> {
        let it = self.docs.iter().enumerate().map(|(i, d)| (i as u32, d));
        let shuffled = |mut m: MemSorted| {
            m.docs = self.maybe_shuffle(field, m.docs);
            m
        };
        Ok(Some(match field {
            TAG => DocValuesSource::SortedSet(Box::new(shuffled(dict_source(
                it.map(|(i, d)| (i, d.tags.clone())).collect(),
            )))),
            CAT => DocValuesSource::Sorted(Box::new(shuffled(dict_source(
                it.map(|(i, d)| (i, d.cat.iter().cloned().collect()))
                    .collect(),
            )))),
            NUM => DocValuesSource::SortedNumeric(Box::new(MemSortedNumeric(
                self.maybe_shuffle(
                    field,
                    it.filter(|(_, d)| !d.nums.is_empty())
                        .map(|(i, d)| (i, d.nums.clone()))
                        .collect(),
                ),
                0,
            ))),
            PRICE => DocValuesSource::Numeric(Box::new(
                self.maybe_shuffle(field, it.filter_map(|(i, d)| d.price.map(|p| (i, p))).collect()).into_iter(),
            )),
            BIN => DocValuesSource::Binary(Box::new(VecBytes(
                self.maybe_shuffle(field, it.filter_map(|(i, d)| d.bin.clone().map(|b| (i, b))).collect()),
                0,
            ))),
            _ => return Ok(None),
        }))
    }
    fn points(&self, field: u32) -> Result<Option<Box<dyn BytesSource + '_>>> {
        let mut v = Vec::new();
        for (i, d) in self.docs.iter().enumerate() {
            match field {
                NUM => v.extend(
                    d.nums
                        .iter()
                        .map(|&n| (i as u32, encode::i64_bytes(n).to_vec())),
                ),
                LOC => {
                    if let Some((a, b)) = d.loc {
                        let mut p = encode::i32_bytes(a).to_vec();
                        p.extend_from_slice(&encode::i32_bytes(b));
                        v.push((i as u32, p));
                    }
                }
                _ => return Ok(None),
            }
        }
        Ok(Some(Box::new(VecBytes(v, 0))))
    }
    fn vectors(&self, field: u32) -> Result<Option<Box<dyn BytesSource + '_>>> {
        if field != VEC {
            return Ok(None);
        }
        let v = self
            .docs
            .iter()
            .enumerate()
            .filter_map(|(i, d)| {
                d.vec
                    .as_ref()
                    .map(|v| (i as u32, v.iter().flat_map(|x| x.to_le_bytes()).collect()))
            })
            .collect();
        Ok(Some(Box::new(VecBytes(self.maybe_shuffle(field, v), 0))))
    }
    fn suggest(&self, field: u32) -> Result<Option<Box<dyn SuggestSource + '_>>> {
        if field != SUG {
            return Ok(None);
        }
        let entries: Vec<_> = self.docs.iter().enumerate().flat_map(|(i, d)| suggest_entries(d, i as u32)).collect();
        struct Entries(Vec<(Vec<u8>, u64, u32, Vec<u8>)>, usize);
        impl SuggestSource for Entries {
            fn next(&mut self) -> Option<SuggestEntry<'_>> {
                let e = self.0.get(self.1)?;
                self.1 += 1;
                Some(SuggestEntry { key: &e.0, weight: e.1, doc: e.2, payload: &e.3 })
            }
        }
        Ok(Some(Box::new(Entries(self.maybe_shuffle(field, entries), 0))))
    }
    fn stored(&self, doc: u32, out: &mut Vec<u8>) -> Result<()> {
        let d = &self.docs[doc as usize];
        let mut w = StoredDocWriter::new();
        w.add(ID, StoredValue::Str(&d.id));
        let body: Vec<&str> = d.body.iter().map(|t| t.term.as_str()).collect();
        w.add(BODY, StoredValue::Str(&body.join(" ")));
        if let Some(p) = d.price {
            w.add(PRICE, StoredValue::Long(p));
        }
        out.extend_from_slice(w.bytes());
        Ok(())
    }
}

fn write(src: &dyn SegmentSource) -> SegmentReader {
    let mut out = Vec::new();
    let info = write_segment(src, &mut out).unwrap();
    let r = SegmentReader::open(Slice::from(out)).unwrap();
    r.verify().unwrap();
    assert_eq!(r.info(), &info);
    r
}

/// Checks every structure of `r` against the model docs (in segment order).
fn check(r: &SegmentReader, docs: &[Doc]) {
    let max_doc = docs.len() as u32;
    assert_eq!(r.max_doc(), max_doc);
    // Postings of body, with positions/offsets/payloads.
    let src = MemSource::new(docs.to_vec(), 0, None);
    for field in [BODY, TAG, CAT, ID] {
        let Some(model) = src.inverted.get(&field) else {
            assert!(r.terms(field).is_none() || r.terms(field).unwrap().stats().num_terms == 0);
            continue;
        };
        let t = r.terms(field).unwrap();
        assert_eq!(t.stats().num_terms as usize, model.len(), "field {field}");
        if field == ID {
            // The Bloom filter never hides a present id and rejects most absent ones.
            let bloom = t.bloom().expect("_id has a Bloom filter");
            assert_eq!(bloom.len(), model.len() as u64);
            for (i, term) in model.keys().enumerate() {
                let st = t.seek_exact(term).unwrap();
                assert_eq!(st.ord, i as u64);
                let h = search_codec::bloom::hash64(term);
                assert_eq!(t.seek_exact_hashed(term, h), Some(st));
            }
            let absent = (0..2000).filter(|i| bloom.may_contain(format!("missing-{i}").as_bytes())).count();
            assert!(absent < 100, "{absent} of 2000 absent ids pass the filter");
            assert!(t.seek_exact(b"missing-1").is_none());
        }
        let sum_df: usize = model.values().map(|v| v.len()).sum();
        assert_eq!(t.stats().sum_doc_freq as usize, sum_df);
        let mut it = t.iter();
        for (term, list) in model {
            assert_eq!(it.next(), Some(term.as_slice()));
            let st = it.state();
            assert_eq!(st.doc_freq as usize, list.len());
            let mut c = t.postings(&st, PostingsFlags::ALL);
            for (d, toks) in list {
                assert_eq!(c.next(), *d);
                if field == BODY {
                    assert_eq!(c.freq() as usize, toks.len());
                    for tok in toks {
                        assert_eq!(c.next_position(), tok.pos);
                        assert_eq!((c.start_offset(), c.end_offset()), (tok.start, tok.end));
                        assert_eq!(c.payload(), tok.payload.as_slice());
                    }
                }
            }
            assert_eq!(c.next(), NO_MORE_DOCS);
        }
        assert!(it.next().is_none());
    }
    // Norms.
    let norms = r.norms(BODY);
    for (i, d) in docs.iter().enumerate() {
        let want = (!d.body.is_empty()).then(|| int_to_byte4(d.body.len() as u32));
        assert_eq!(norms.and_then(|n| n.get(i as u32)), want);
    }
    // Doc values.
    let mut buf = Vec::new();
    for (i, d) in docs.iter().enumerate() {
        let i = i as u32;
        assert_eq!(r.numeric(PRICE).unwrap().get(i), d.price);
        assert_eq!(r.binary(BIN).unwrap().get(i), d.bin.as_deref());
        let mut nums = d.nums.clone();
        nums.sort();
        assert_eq!(
            r.sorted_numeric(NUM).unwrap().values(i).collect::<Vec<_>>(),
            nums
        );
        let ss = r.sorted_set(TAG).unwrap();
        let tags: Vec<Vec<u8>> = ss
            .ords(i)
            .map(|o| {
                ss.lookup_ord(o, &mut buf);
                buf.clone()
            })
            .collect();
        let want: Vec<Vec<u8>> = d
            .tags
            .iter()
            .collect::<BTreeSet<_>>()
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        assert_eq!(tags, want);
        let sd = r.sorted(CAT).unwrap();
        let cat = sd.ord(i).map(|o| {
            sd.lookup_ord(o, &mut buf);
            String::from_utf8(buf.clone()).unwrap()
        });
        assert_eq!(cat, d.cat);
    }
    // Points: a range query on NUM.
    struct R(i64, i64, Vec<u32>);
    impl PointVisitor for R {
        fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
            let (a, b) = (encode::i64_from(min), encode::i64_from(max));
            if b < self.0 || a > self.1 {
                Relation::CellOutsideQuery
            } else if a >= self.0 && b <= self.1 {
                Relation::CellInsideQuery
            } else {
                Relation::CellCrossesQuery
            }
        }
        fn visit(&mut self, doc: u32) {
            self.2.push(doc);
        }
        fn visit_value(&mut self, doc: u32, v: &[u8]) {
            let x = encode::i64_from(v);
            if x >= self.0 && x <= self.1 {
                self.2.push(doc);
            }
        }
    }
    let mut q = R(-100, 250, Vec::new());
    if let Some(p) = r.points(NUM) {
        p.intersect(&mut q);
    }
    q.2.sort();
    let mut want: Vec<u32> = docs
        .iter()
        .enumerate()
        .flat_map(|(i, d)| {
            d.nums
                .iter()
                .filter(|&&n| (-100..=250).contains(&n))
                .map(move |_| i as u32)
        })
        .collect();
    want.sort();
    assert_eq!(q.2, want);
    // Vectors.
    if let Some(v) = r.vectors(VEC) {
        for (i, d) in docs.iter().enumerate() {
            assert_eq!(v.ord(i as u32).is_some(), d.vec.is_some());
            if let (Some(o), Some(want)) = (v.ord(i as u32), &d.vec) {
                let mut got = Vec::new();
                v.vector_f32(o, &mut got);
                assert_eq!(&got, want);
                assert_eq!(v.doc(o), i as u32);
            }
        }
        let qv = vec![0.5f32; 8];
        let pq = v.prepare(VectorQuery::Float(&qv)).unwrap();
        // Int8 steering on 8 dims is coarse and multi-threaded graphs vary between runs, so
        // only demand that the approximate best hit is among the exact top 5.
        let exact = v.exact_search(&pq, 5, None);
        let approx = v.search(&pq, 5, 100, None, usize::MAX);
        if let Some(best) = approx.hits.first() {
            assert!(exact.hits.contains(best), "{best:?} not in {:?}", exact.hits);
        }
        assert_eq!(approx.hits.len(), exact.hits.len());
    }
    // Suggest entries: every one present, in (key, weight desc, doc, payload) order, and
    // prefix completion agrees with a filter over them.
    let mut want: Vec<_> = docs.iter().enumerate().flat_map(|(i, d)| suggest_entries(d, i as u32)).collect();
    want.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
    match r.suggest(SUG) {
        None => assert!(want.is_empty() || r.max_doc() == 0),
        Some(sg) => {
            assert_eq!(sg.len() as usize, want.len());
            let mut key = Vec::new();
            for (i, e) in want.iter().enumerate() {
                sg.key(i as u64, &mut key);
                assert_eq!((&key, sg.weight(i as u64), sg.doc(i as u64), sg.payload(i as u64)), (&e.0, e.1, e.2, &e.3[..]));
            }
            let mut c = sg.complete(&Prefix::new("t1"));
            let mut got = Vec::new();
            while let Some(x) = c.next() {
                got.push((x.weight, x.index));
            }
            let mut expect: Vec<(u64, u64)> =
                want.iter().enumerate().filter(|(_, e)| e.0.starts_with(b"t1")).map(|(i, e)| (e.1, i as u64)).collect();
            expect.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            assert_eq!(got, expect);
        }
    }
    // Stored fields.
    let mut s = r.stored();
    for (i, d) in docs.iter().enumerate() {
        let f = s.document(i as u32).unwrap();
        assert_eq!(f[0], (ID, OwnedStoredValue::Str(d.id.clone())));
        assert_eq!(f.len(), 2 + d.price.is_some() as usize);
    }
}

#[test]
fn write_read_round_trip() {
    let mut rng = StdRng::seed_from_u64(1);
    for n in [0usize, 1, 2, 127, 128, 129, 2000] {
        let docs: Vec<Doc> = (0..n).map(|i| random_doc(&mut rng, i)).collect();
        let src = MemSource::new(docs.clone(), 1, None);
        let r = write(&src);
        check(&r, &docs);
    }
}

/// Spilling section buffers to temporary files changes where bytes wait, never the bytes.
#[test]
fn spilled_writes_are_identical() {
    let mut rng = StdRng::seed_from_u64(21);
    let docs: Vec<Doc> = (0..3000).map(|i| random_doc(&mut rng, i)).collect();
    let src = MemSource::new(docs.clone(), 1, None);
    // Per-doc values out of doc order go through the spill-backed sort.
    let mut shuffled = MemSource::new(docs.clone(), 1, None);
    shuffled.shuffle_per_doc_values(&mut rng);
    let write_with = |src: &MemSource, spill: SpillOptions| {
        let mut out = Vec::new();
        let opts = WriteOptions { vector_threads: 1, spill, ..WriteOptions::default() };
        write_segment_with(src, &mut out, &opts).unwrap();
        out
    };
    let mem = write_with(&src, SpillOptions::in_memory());
    let dir = std::env::temp_dir().join(format!("roll-codec-spill-seg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for budget in [0usize, 1000, 64 << 10] {
        for (name, s) in [("ordered", &src), ("shuffled", &shuffled)] {
            let spilled = write_with(s, SpillOptions { dir: Some(dir.clone()), memory_bytes: budget });
            assert!(spilled == mem, "budget {budget}, {name}: spilled segment differs");
        }
    }
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "temporary files left behind");
    std::fs::remove_dir(&dir).unwrap();
    let r = SegmentReader::open(Slice::from(mem)).unwrap();
    r.verify().unwrap();
    check(&r, &docs);
    // An unusable spill directory fails the write with an I/O error.
    let mut out = Vec::new();
    let opts = WriteOptions { spill: SpillOptions { dir: Some(dir.join("missing")), memory_bytes: 0 }, ..WriteOptions::default() };
    assert!(matches!(write_segment_with(&src, &mut out, &opts), Err(Error::Io(_))));
}

#[test]
fn merge_with_deletions() {
    let mut rng = StdRng::seed_from_u64(2);
    let mut segs = Vec::new();
    let mut expected = Vec::new();
    for s in 0..3 {
        let docs: Vec<Doc> = (0..500)
            .map(|i| random_doc(&mut rng, s * 1000 + i))
            .collect();
        let reader = write(&MemSource::new(docs.clone(), s as u8 + 1, None));
        let mut live = LiveDocs::all_live(500);
        for d in 0..500u32 {
            if rng.gen_bool(0.3) {
                live.delete(d);
            }
        }
        for (d, doc) in docs.iter().enumerate() {
            if live.is_live(d as u32) {
                expected.push(doc.clone());
            }
        }
        segs.push(MergeInput {
            reader,
            live: Some(Arc::new(live)),
        });
    }
    // An input whose docs are all deleted, and one with nothing deleted (LiveDocs present).
    let docs: Vec<Doc> = (0..130).map(|i| random_doc(&mut rng, 9000 + i)).collect();
    let mut dead = LiveDocs::all_live(130);
    for d in 0..130 {
        dead.delete(d);
    }
    segs.insert(1, MergeInput { reader: write(&MemSource::new(docs.clone(), 7, None)), live: Some(Arc::new(dead)) });
    segs.push(MergeInput { reader: write(&MemSource::new(docs.clone(), 8, None)), live: Some(Arc::new(LiveDocs::all_live(130))) });
    expected.extend(docs.iter().cloned());
    let lives: Vec<Option<Arc<LiveDocs>>> = segs.iter().map(|s| s.live.clone()).collect();
    let m = MergeSource::new(segs, [9; 16]).unwrap();
    assert_eq!(m.max_doc() as usize, expected.len());
    // Doc maps: map_doc and source_of are inverse, in sequential and random order.
    let mut forward = Vec::new();
    for (i, live) in lives.iter().enumerate() {
        let n = live.as_ref().unwrap().max_doc();
        for d in 0..n {
            match m.map_doc(i, d) {
                Some(nd) => {
                    assert!(live.as_ref().unwrap().is_live(d));
                    assert_eq!(nd as usize, forward.len());
                    forward.push((i, d));
                }
                None => assert!(!live.as_ref().unwrap().is_live(d)),
            }
        }
        assert_eq!(m.map_doc(i, n), None);
    }
    assert_eq!(forward.len(), expected.len());
    for (nd, &src) in forward.iter().enumerate() {
        assert_eq!(m.source_of(nd as u32), src);
    }
    for _ in 0..2000 {
        let nd = rng.gen_range(0..forward.len());
        assert_eq!(m.source_of(nd as u32), forward[nd]);
    }
    let merged = write(&m);
    check(&merged, &expected);
    // A merge of the merged segment alone is identical in content.
    let again = write(
        &MergeSource::new(
            vec![MergeInput {
                reader: merged.clone(),
                live: None,
            }],
            [10; 16],
        )
        .unwrap(),
    );
    check(&again, &expected);
}

#[test]
fn index_sort_orders_docs() {
    let mut rng = StdRng::seed_from_u64(3);
    let docs: Vec<Doc> = (0..1500).map(|i| random_doc(&mut rng, i)).collect();
    let sort = IndexSort {
        fields: vec![
            SortField {
                field: CAT,
                kind: SortKind::String,
                reverse: true,
                missing: SortMissing::Last,
                selector: SortSelector::Min,
            },
            SortField {
                field: PRICE,
                kind: SortKind::Long,
                reverse: false,
                missing: SortMissing::First,
                selector: SortSelector::Min,
            },
        ],
    };
    let src = MemSource::new(docs.clone(), 4, Some(sort.clone()));
    let r = write(&src);
    assert_eq!(r.info().sort.as_ref(), Some(&sort));
    // Expected order: cat descending (missing last), then price ascending (missing first), ties by doc.
    let mut order: Vec<usize> = (0..docs.len()).collect();
    order.sort_by(|&a, &b| {
        let (ca, cb) = (&docs[a].cat, &docs[b].cat);
        let o = match (ca, cb) {
            (Some(x), Some(y)) => y.cmp(x),
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
        };
        o.then_with(|| match (docs[a].price, docs[b].price) {
            (Some(x), Some(y)) => x.cmp(&y),
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
        })
    });
    let sorted: Vec<Doc> = order.iter().map(|&i| docs[i].clone()).collect();
    check(&r, &sorted);
    // Merging two sorted segments yields a sorted segment.
    let docs2: Vec<Doc> = (0..700).map(|i| random_doc(&mut rng, 5000 + i)).collect();
    let r2 = write(&MemSource::new(docs2.clone(), 5, Some(sort.clone())));
    let m = MergeSource::new(
        vec![
            MergeInput {
                reader: r,
                live: None,
            },
            MergeInput {
                reader: r2,
                live: None,
            },
        ],
        [6; 16],
    )
    .unwrap();
    let merged = write(&m);
    let sd = merged.sorted(CAT).unwrap();
    let price = merged.numeric(PRICE).unwrap();
    let mut prev: Option<(Option<u64>, Option<i64>)> = None;
    let mut buf = Vec::new();
    for d in 0..merged.max_doc() {
        let cat = sd.ord(d);
        if let Some(o) = cat {
            sd.lookup_ord(o, &mut buf);
        }
        let key = (cat, price.get(d));
        if let Some(p) = prev {
            // cat descending with missing last
            match (p.0, key.0) {
                (Some(a), Some(b)) => {
                    assert!(a >= b);
                    if a == b {
                        assert!(match (p.1, key.1) {
                            (Some(x), Some(y)) => x <= y,
                            (Some(_), None) => false,
                            _ => true,
                        });
                    }
                }
                (None, Some(_)) => panic!("missing cat must sort last"),
                _ => {}
            }
        }
        prev = Some(key);
    }
}

#[test]
fn corruption_is_detected() {
    let mut rng = StdRng::seed_from_u64(4);
    let docs: Vec<Doc> = (0..300).map(|i| random_doc(&mut rng, i)).collect();
    let mut out = Vec::new();
    write_segment(&MemSource::new(docs, 7, None), &mut out).unwrap();
    for _ in 0..200 {
        let mut bad = out.clone();
        let i = rng.gen_range(0..bad.len());
        bad[i] ^= 1 << rng.gen_range(0..8);
        let ok = match SegmentReader::open(Slice::from(bad)) {
            Err(_) => false,
            Ok(r) => r.verify().is_ok(),
        };
        assert!(!ok, "flip at {i} undetected");
    }
    for n in [0, 10, out.len() / 2, out.len() - 1] {
        assert!(SegmentReader::open(Slice::from(out[..n].to_vec())).is_err());
    }
}

#[test]
fn source_validation_errors() {
    struct Src(Vec<(u32, i64)>);
    impl SegmentSource for Src {
        fn segment_id(&self) -> [u8; 16] {
            [0; 16]
        }
        fn max_doc(&self) -> u32 {
            3
        }
        fn field_infos(&self) -> Vec<FieldInfo> {
            vec![FieldInfo::new(1, "n").with_doc_values(DocValuesType::Numeric)]
        }
        fn terms(&self, _: u32) -> Result<Option<Box<dyn TermsSource + '_>>> {
            Ok(None)
        }
        fn norms(&self, _: u32) -> Result<Option<search_codec::source::NormsIter<'_>>> {
            Ok(None)
        }
        fn doc_values(&self, _: u32) -> Result<Option<DocValuesSource<'_>>> {
            Ok(Some(DocValuesSource::Numeric(Box::new(self.0.clone().into_iter()))))
        }
        fn points(&self, _: u32) -> Result<Option<Box<dyn BytesSource + '_>>> {
            Ok(None)
        }
        fn vectors(&self, _: u32) -> Result<Option<Box<dyn BytesSource + '_>>> {
            Ok(None)
        }
        fn stored(&self, _: u32, _: &mut Vec<u8>) -> Result<()> {
            Ok(())
        }
    }
    // Any doc order is accepted (and gives the same segment as doc order) ...
    let mut a = Vec::new();
    write_segment(&Src(vec![(2, 1), (0, 7), (1, 2)]), &mut a).unwrap();
    let mut b = Vec::new();
    write_segment(&Src(vec![(0, 7), (1, 2), (2, 1)]), &mut b).unwrap();
    assert!(a == b);
    let r = SegmentReader::open(Slice::from(a)).unwrap();
    assert_eq!((0..3).map(|d| r.numeric(1).unwrap().get(d)).collect::<Vec<_>>(), [Some(7), Some(2), Some(1)]);
    // ... but not a doc twice, or past max_doc.
    for bad in [vec![(2u32, 1i64), (1, 2), (2, 3)], vec![(1, 1), (3, 1)]] {
        let e = write_segment(&Src(bad), &mut Vec::new()).unwrap_err();
        assert!(matches!(e, Error::InvalidArgument(_)), "{e}");
    }
}

/// Norms, doc values of every type and vectors given in a shuffled doc order produce the very
/// same segment as in doc order.
#[test]
fn per_doc_values_in_any_order() {
    let mut rng = StdRng::seed_from_u64(31);
    let docs: Vec<Doc> = (0..700).map(|i| random_doc(&mut rng, i)).collect();
    let ordered = MemSource::new(docs.clone(), 3, None);
    let mut shuffled = MemSource::new(docs.clone(), 3, None);
    shuffled.shuffle_per_doc_values(&mut rng);
    let write_with = |src: &MemSource| {
        let mut out = Vec::new();
        let opts = WriteOptions { vector_threads: 1, ..WriteOptions::default() };
        write_segment_with(src, &mut out, &opts).unwrap();
        out
    };
    let (a, b) = (write_with(&ordered), write_with(&shuffled));
    assert!(a == b, "shuffled per-doc values changed the segment");
    let r = SegmentReader::open(Slice::from(b)).unwrap();
    check(&r, &docs);
}

#[test]
fn bp_reordering_shrinks_postings() {
    // Two topics with disjoint vocabularies, interleaved: BP should separate them.
    let mut rng = StdRng::seed_from_u64(5);
    let mut docs = Vec::new();
    for i in 0..4000usize {
        let topic = i % 2;
        let mut d = random_doc(&mut rng, i);
        let words: Vec<String> = (0..20).map(|w| format!("t{topic}w{w}")).collect();
        d.body = (0..12)
            .map(|p| {
                let w = words[rng.gen_range(0..words.len())].clone();
                Tok { term: w, pos: p, start: 0, end: 0, payload: Vec::new() }
            })
            .collect();
        docs.push(d);
    }
    let r = write(&MemSource::new(docs.clone(), 11, None));
    let m = MergeSource::new(vec![MergeInput { reader: r.clone(), live: None }], [12; 16]).unwrap();
    let opts = BpOptions { min_doc_freq: 16, ..BpOptions::default() };
    let map = bp_reorder(&m, &opts).unwrap().expect("reordering expected");
    let remapped = RemappedSource::new(&m, map.clone(), None).unwrap();
    let reordered = write(&remapped);
    let docs_size = |r: &SegmentReader| {
        r.sections().iter().filter(|(k, f, _)| *k == container::SectionKind::Docs && *f == BODY).map(|s| s.2).sum::<u64>()
    };
    let (before, after) = (docs_size(&r), docs_size(&reordered));
    let same = (1..map.new_max_doc()).filter(|&n| map.new_to_old(n) % 2 == map.new_to_old(n - 1) % 2).count();
    assert!(same * 100 >= 95 * (map.new_max_doc() as usize - 1), "topics not separated: {same}");
    assert!(after * 100 < before * 97, "BP did not shrink postings: {before} -> {after}");
    // Same content, new order.
    let new_docs: Vec<Doc> = (0..map.new_max_doc()).map(|n| docs[map.new_to_old(n) as usize].clone()).collect();
    check(&reordered, &new_docs);
}

/// A scattered reordering that also drops docs: stored documents come from the old-order copy
/// (spilled to disk with a zero budget) and every structure matches the model.
#[test]
fn scattered_remap_with_deletions() {
    let mut rng = StdRng::seed_from_u64(55);
    let docs: Vec<Doc> = (0..1500).map(|i| random_doc(&mut rng, i)).collect();
    let src = MemSource::new(docs.clone(), 13, None);
    let mut keep: Vec<u32> = (0..docs.len() as u32).filter(|_| rng.gen_bool(0.8)).collect();
    for i in (1..keep.len()).rev() {
        keep.swap(i, rng.gen_range(0..=i));
    }
    let map = DocMap::from_new_to_old(keep.clone(), docs.len() as u32).unwrap();
    let dir = std::env::temp_dir().join(format!("roll-codec-remap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let spill = SpillOptions { dir: Some(dir.clone()), memory_bytes: 0 };
    let mut remapped = RemappedSource::new(&src, map, None).unwrap();
    remapped.set_spill(spill.clone());
    let mut out = Vec::new();
    let opts = WriteOptions { vector_threads: 1, spill, ..WriteOptions::default() };
    write_segment_with(&remapped, &mut out, &opts).unwrap();
    drop(remapped);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "temporary files left behind");
    std::fs::remove_dir(&dir).unwrap();
    let r = SegmentReader::open(Slice::from(out)).unwrap();
    r.verify().unwrap();
    let new_docs: Vec<Doc> = keep.iter().map(|&o| docs[o as usize].clone()).collect();
    check(&r, &new_docs);
}

#[test]
fn bp_keeps_parent_blocks_together() {
    // Blocks of 1-3 child docs followed by a parent, two interleaved topics.
    let mut rng = StdRng::seed_from_u64(6);
    let mut docs = Vec::new();
    let mut parents = Vec::new();
    let mut block = 0usize;
    while docs.len() < 3000 {
        let topic = block % 2;
        let size = rng.gen_range(1..4) + 1;
        for k in 0..size {
            let mut d = random_doc(&mut rng, docs.len());
            d.body = (0..10)
                .map(|p| Tok {
                    term: format!("t{topic}w{}", rng.gen_range(0..15)),
                    pos: p,
                    start: 0,
                    end: 0,
                    payload: Vec::new(),
                })
                .collect();
            if k + 1 == size {
                parents.push(docs.len() as u32);
            }
            docs.push(d);
        }
        block += 1;
    }
    let max_doc = docs.len() as u32;
    let mut bits = vec![0u64; (max_doc as usize).div_ceil(64)];
    for &p in &parents {
        bits[(p >> 6) as usize] |= 1 << (p & 63);
    }
    let r = write(&MemSource::new(docs.clone(), 13, None));
    let m = MergeSource::new(vec![MergeInput { reader: r, live: None }], [14; 16]).unwrap();
    let opts = BpOptions { min_doc_freq: 16, parents: Some(bits.clone()), ..BpOptions::default() };
    let map = bp_reorder(&m, &opts).unwrap().expect("reordering expected");
    // Every block is contiguous and in its original internal order, parent last.
    let mut n = 0u32;
    while n < max_doc {
        let first = map.new_to_old(n);
        let mut k = 0;
        loop {
            let old = map.new_to_old(n + k);
            assert_eq!(old, first + k, "block broken at new doc {}", n + k);
            if bits[(old >> 6) as usize] >> (old & 63) & 1 != 0 {
                break;
            }
            k += 1;
        }
        n += k + 1;
    }
    let reordered = write(&RemappedSource::new(&m, map.clone(), None).unwrap());
    let new_docs: Vec<Doc> = (0..max_doc).map(|n| docs[map.new_to_old(n) as usize].clone()).collect();
    check(&reordered, &new_docs);
}

/// Readers trust sections that passed `verify()`, but corrupt bytes that slip past (bit rot
/// after verification) must never cause undefined behaviour or a panic.
#[test]
fn corrupted_sections_do_not_panic_in_codec_code() {
    use std::cell::RefCell;
    thread_local! { static LOC: RefCell<Option<String>> = const { RefCell::new(None) }; }
    let seed = std::env::var("ROLL_CODEC_FUZZ_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
    let mut rng = StdRng::seed_from_u64(seed);
    // ROLL_CODEC_FUZZ_DOCS=3000 exercises multi-block postings, several BKD leaves and more.
    let ndocs: usize = std::env::var("ROLL_CODEC_FUZZ_DOCS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    let docs: Vec<Doc> = (0..ndocs).map(|i| random_doc(&mut rng, i)).collect();
    let mut out = Vec::new();
    write_segment(&MemSource::new(docs.clone(), 7, None), &mut out).unwrap();
    // The hook is process-wide: only this thread's panics are captured, other tests running
    // concurrently keep the default report.
    thread_local! { static FUZZING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
    FUZZING.with(|f| f.set(true));
    let prev: std::sync::Arc<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync> = std::panic::take_hook().into();
    let fallback = prev.clone();
    std::panic::set_hook(Box::new(move |info| {
        if !FUZZING.with(|f| f.get()) {
            return fallback(info);
        }
        let loc = info.location().map(|l| format!("{}:{}: {}", l.file(), l.line(), info.payload().downcast_ref::<String>().map(String::as_str).or(info.payload().downcast_ref::<&str>().copied()).unwrap_or(""))).unwrap_or_default();
        LOC.with(|c| *c.borrow_mut() = Some(loc));
    }));
    let mut ours = Vec::new();
    let rounds: usize = std::env::var("ROLL_CODEC_FUZZ_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    for _ in 0..rounds {
        let mut bad = out.clone();
        for _ in 0..rng.gen_range(1..8) {
            let i = rng.gen_range(24..bad.len() - 24);
            bad[i] = rng.r#gen();
        }
        let Ok(r) = SegmentReader::open(Slice::from(bad)) else { continue };
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check_loosely(&r)));
        if res.is_err() {
            ours.push(LOC.with(|c| c.borrow_mut().take()).unwrap_or_default());
        }
    }
    std::panic::set_hook(Box::new(move |info| prev(info)));
    FUZZING.with(|f| f.set(false));
    assert!(ours.is_empty(), "codec panicked on corrupt input at {ours:?}");
}

fn check_loosely(r: &SegmentReader) {

    let max_doc = r.max_doc();
    // Dictionary lookups, ceilings, ranges and automata over corrupt blocks and indexes.
    let fuzzy = Dfa::levenshtein("alpha", 1, 0, true);
    let wild = Dfa::wildcard("*a?a*", false, 10_000).unwrap();
    for f in [BODY, TAG, CAT, ID] {
        if let Some(t) = r.terms(f) {
            for probe in [&b"alpha"[..], b"t1", b"cat3", b"doc-17", b"", b"\xff\xff", b"rare99"] {
                let _ = t.seek_exact(probe).map(|s| s.doc_freq);
                let mut it = t.seek_ceil(probe);
                for _ in 0..40 {
                    if it.next().is_none() {
                        break;
                    }
                    let _ = it.state();
                }
            }
            for aut in [&fuzzy as &dyn Automaton, &wild, &Prefix::new(&b"r"[..])] {
                let mut it = t.intersect(aut);
                for _ in 0..2000 {
                    if it.next().is_none() {
                        break;
                    }
                }
            }
            let _ = t.term_state(t.stats().num_terms.saturating_sub(1));
        }
    }
    for f in [BODY, TAG, CAT, ID] {
        if let Some(t) = r.terms(f) {
            let mut it = t.iter();
            let mut n = 0;
            while it.next().is_some() && n < 1000 {
                let mut c = it.postings(PostingsFlags::ALL);
                let mut k = 0;
                while c.next() != NO_MORE_DOCS && k < 10_000 {
                    let fr = c.freq().min(50);
                    for _ in 0..fr {
                        c.next_position();
                    }
                    k += 1;
                }
                n += 1;
            }
        }
    }
    let mut buf = Vec::new();
    for d in 0..max_doc {
        let _ = r.norms(BODY).map(|n| n.get(d));
        let _ = r.numeric(PRICE).map(|n| n.get(d));
        let _ = r.binary(BIN).map(|n| n.get(d).map(|b| b.len()));
        if let Some(s) = r.sorted_set(TAG) {
            for o in s.ords(d).take(100) {
                s.lookup_ord(o, &mut buf);
            }
        }
        let _ = r.sorted_numeric(NUM).map(|n| n.values(d).take(100).count());
    }
    let mut s = r.stored();
    for d in 0..max_doc {
        let _ = s.document(d);
    }
    // Skipping, impacts, points, vectors, skip indexes.
    if let Some(t) = r.terms(BODY) {
        let mut it = t.iter();
        let mut n = 0;
        while it.next().is_some() && n < 200 {
            let mut c = it.postings(PostingsFlags::FREQS);
            let mut target = 0u32;
            for _ in 0..50 {
                c.advance_shallow(target);
                let levels = c.impact_levels().min(2);
                for l in 0..levels {
                    let _ = c.impact_doc_upto(l);
                    let _ = c.impacts(l).len();
                }
                let d = c.advance(target);
                if d == NO_MORE_DOCS {
                    break;
                }
                target = d.saturating_add(7);
            }

            // Bit set filling over (possibly dense) blocks.
            let mut c = it.postings(PostingsFlags::DOCS);
            if c.next() != NO_MORE_DOCS {
                let mut bits = vec![0u64; (max_doc as usize).div_ceil(64) + 1];
                c.into_bitset(max_doc, &mut bits, 0);
            }

            n += 1;
        }
    }
    for f in [NUM, LOC] {
        if let Some(p) = r.points(f) {
            let mut n = 0usize;
            p.visit_all_values(&mut |_, _| n += 1);
            // A box over the first dimension's middle range (crossing leaves, runs).
            struct Half(usize, usize);
            impl PointVisitor for Half {
                fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation {
                    let (lo, hi) = (&[0x7f, 0xff][..], &[0x80, 0x10][..]);
                    if max[..2] < *lo || min[..2] > *hi {
                        Relation::CellOutsideQuery
                    } else if min[..2] >= *lo && max[..2] <= *hi {
                        Relation::CellInsideQuery
                    } else {
                        Relation::CellCrossesQuery
                    }
                }
                fn visit(&mut self, _: u32) {
                    self.0 += 1;
                }
                fn visit_value(&mut self, _: u32, v: &[u8]) {
                    self.1 += v.len();
                }
            }
            let mut v = Half(0, 0);
            p.intersect(&mut v);
            let _ = p.estimate_point_count(&mut v);
        }
    }
    if let Some(sg) = r.suggest(SUG) {
        let mut key = Vec::new();
        for aut in [&fuzzy as &dyn Automaton, &Prefix::new(&b"t"[..]), &AlwaysMatch, &wild] {
            let mut c = sg.complete(aut);
            for _ in 0..500 {
                let Some(x) = c.next() else { break };
                sg.key(x.index, &mut key);
                let _ = sg.payload(x.index).len();
            }
        }
    }
    if let Some(v) = r.vectors(VEC)
        && let Ok(q) = v.prepare(VectorQuery::Float(&[0.5; 8]))
    {
        let _ = v.search(&q, 5, 20, None, 10_000);
        let _ = v.exact_search(&q, 5, None);
    }
    if let Some(mut sk) = r.numeric(PRICE).and_then(|n| n.skipper()) {
        for t in (0..max_doc).step_by(50) {
            sk.advance(t);
            let _ = (sk.num_levels(), sk.min_value(0), sk.max_doc_id(0));
        }
    }
    if let Some(n) = r.numeric(PRICE) {
        let mut c = n.cursor();
        let mut k = 0;
        while c.next() != NO_MORE_DOCS && k < 100_000 {
            let _ = n.value_at(c.index());
            k += 1;
        }
    }
}

/// `cargo test --release -p search-codec --test segment bench_write -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_write_and_merge() {
    let mut rng = StdRng::seed_from_u64(42);
    let n = 100_000;
    let docs: Vec<Doc> = (0..n).map(|i| random_doc(&mut rng, i)).collect();
    let src = MemSource::new(docs.clone(), 30, None);
    for dictionary in [false, true] {
        let opts = WriteOptions { stored: search_codec::stored::StoredOptions { dictionary, ..Default::default() }, ..Default::default() };
        let t = std::time::Instant::now();
        let mut out = Vec::new();
        write_segment_with(&src, &mut out, &opts).unwrap();
        println!("write {n} docs (dictionary {dictionary}): {:?}, {} bytes", t.elapsed(), out.len());
    }
    // Small refresh-sized segments: fixed costs per write.
    for small in [10usize, 1000] {
        let sdocs: Vec<Doc> = docs[..small].to_vec();
        let ssrc = MemSource::new(sdocs, 40, None);
        let opts = WriteOptions { stored: search_codec::stored::StoredOptions { dictionary: false, ..Default::default() }, vector_threads: 1, ..Default::default() };
        let rounds = 200;
        let t = std::time::Instant::now();
        for _ in 0..rounds {
            let mut out = Vec::new();
            write_segment_with(&ssrc, &mut out, &opts).unwrap();
            SegmentReader::open(Slice::from(out)).unwrap();
        }
        println!("write+open {small}-doc segment: {:?}", t.elapsed() / rounds);
    }
    // The same docs without the vector field (whose HNSW build dominates writing).
    struct NoVectors<'a>(&'a MemSource);
    impl SegmentSource for NoVectors<'_> {
        fn segment_id(&self) -> [u8; 16] {
            self.0.segment_id()
        }
        fn max_doc(&self) -> u32 {
            self.0.max_doc()
        }
        fn field_infos(&self) -> Vec<FieldInfo> {
            self.0.field_infos().into_iter().filter(|f| f.number != VEC).collect()
        }
        fn terms(&self, f: u32) -> Result<Option<Box<dyn TermsSource + '_>>> {
            self.0.terms(f)
        }
        fn norms(&self, f: u32) -> Result<Option<search_codec::source::NormsIter<'_>>> {
            self.0.norms(f)
        }
        fn doc_values(&self, f: u32) -> Result<Option<DocValuesSource<'_>>> {
            self.0.doc_values(f)
        }
        fn points(&self, f: u32) -> Result<Option<Box<dyn BytesSource + '_>>> {
            self.0.points(f)
        }
        fn vectors(&self, _: u32) -> Result<Option<Box<dyn BytesSource + '_>>> {
            Ok(None)
        }
        fn suggest(&self, f: u32) -> Result<Option<Box<dyn SuggestSource + '_>>> {
            self.0.suggest(f)
        }
        fn stored(&self, d: u32, out: &mut Vec<u8>) -> Result<()> {
            self.0.stored(d, out)
        }
    }
    let t = std::time::Instant::now();
    let mut out = Vec::new();
    write_segment(&NoVectors(&src), &mut out).unwrap();
    println!("write {n} docs without vectors: {:?}", t.elapsed());
    let mut out = Vec::new();
    write_segment(&src, &mut out).unwrap();
    let r = SegmentReader::open(Slice::from(out)).unwrap();
    let mut by_kind: BTreeMap<String, u64> = BTreeMap::new();
    for (k, _, len) in r.sections() {
        *by_kind.entry(format!("{k:?}")).or_default() += len;
    }
    println!("sections: {by_kind:?}");
    let t = std::time::Instant::now();
    for _ in 0..1000 {
        SegmentReader::open(r.data().clone()).unwrap();
    }
    println!("open: {:?}/segment", t.elapsed() / 1000);
    let t = std::time::Instant::now();
    r.verify().unwrap();
    println!("verify: {:?}", t.elapsed());
    let inputs = vec![
        MergeInput { reader: r.clone(), live: None },
        MergeInput { reader: r.clone(), live: None },
    ];
    let m = MergeSource::new(inputs, [31; 16]).unwrap();
    let t = std::time::Instant::now();
    let mut out = Vec::new();
    write_segment(&m, &mut out).unwrap();
    println!("merge 2x{n}: {:?}", t.elapsed());
    let t = std::time::Instant::now();
    let map = bp_reorder(&m, &BpOptions { min_doc_freq: 256, ..BpOptions::default() }).unwrap();
    println!("bp: {:?} (reordered: {})", t.elapsed(), map.is_some());
    if let Some(map) = map {
        let remapped = RemappedSource::new(&m, map, None).unwrap();
        let t = std::time::Instant::now();
        let mut out = Vec::new();
        write_segment(&remapped, &mut out).unwrap();
        println!("write reordered merge: {:?}", t.elapsed());
    }
}

#[test]
fn readers_are_shared_across_threads() {
    let mut rng = StdRng::seed_from_u64(9);
    let docs: Vec<Doc> = (0..1500).map(|i| random_doc(&mut rng, i)).collect();
    let r = write(&MemSource::new(docs.clone(), 15, None));
    std::thread::scope(|s| {
        for t in 0..4 {
            let (r, docs) = (r.clone(), &docs);
            s.spawn(move || {
                // Every thread checks everything, with its own cursors and stored reader.
                let mut stored = r.stored();
                for (i, d) in docs.iter().enumerate().skip(t).step_by(4) {
                    let f = stored.document(i as u32).unwrap();
                    assert_eq!(f[0], (ID, OwnedStoredValue::Str(d.id.clone())));
                }
                check(&r, docs);
            });
        }
    });
}
