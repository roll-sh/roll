//! Dense vectors: raw storage, exact and HNSW k-NN search, scalar and binary quantization.
//!
//! # Sections
//!
//! - `VectorData`: the docs that have a vector ([`DocSet`], doc → ordinal by rank), an explicit
//!   ordinal → doc array when the field is sparse, the raw vectors back to back (`f32`
//!   little-endian, signed bytes or packed bits) and, for float cosine fields, each vector's
//!   squared norm so cosine costs one dot product.
//! - `VectorGraph`: the HNSW graph over ordinals (`hnsw`).
//! - `VectorQuantized`: for `int8`/`int4` fields, per vector the quantized codes (4-bit codes
//!   packed two per byte) plus a correction term (`quant`); for `bbq` fields the centroid and
//!   per vector one bit per dimension plus corrections (`bbq`).
//!
//! Graphs are built with the quantized scores when the field has them (asymmetrically for
//! `bbq`: the inserted vector in 4-bit query form against one-bit records), on several threads
//! (`hnsw`), and merges start from the largest input graph that lost no vector.
//!
//! # Scores
//!
//! Scores are Lucene's `VectorSimilarityFunction` values, which Elasticsearch returns as `_score`
//! (before its own transformations): `l2_norm` `1 / (1 + d²)`, `dot_product` `max((1 + x·y) / 2,
//! 0)` for floats and `0.5 + x·y / (dims · 2¹⁵)` for bytes, `cosine` `(1 + cos) / 2`,
//! `max_inner_product` `x·y + 1` if positive else `1 / (1 - x·y)`, and for bit vectors
//! Elasticsearch's hamming score `(bits - hamming) / bits`.
//!
//! # Search
//!
//! [`VectorsReader::search`] runs the graph search with a beam of `max(k, num_candidates)`,
//! steering by quantized scores when the field is quantized, then rescores the beam with exact
//! scores. An accept filter (live docs and query filters) restricts results; when the search
//! scores more vectors than `visit_limit` it reports `complete: false` and the caller should run
//! [`VectorsReader::exact_search`] instead, as Lucene does for restrictive filters. Callers that
//! know the filter's cardinality use [`VectorsReader::search_filtered`]: under 60% of the
//! vectors it switches to the filtered graph search, which scores only accepted vectors and
//! steps over rejected ones (Lucene's ACORN-style `FilteredHnswGraphSearcher`). Fields without a
//! graph always search exactly.

pub mod distance;
pub(crate) mod bbq;
pub(crate) mod hnsw;
pub(crate) mod quant;

use crate::docset::{DocSet, write_docset};
use crate::error::{Result, corrupt, invalid};
use crate::info::{VectorElement, VectorQuantization, VectorSimilarity, VectorSpec};
use crate::io::{In, Out};
use crate::slice::Slice;
use distance::{dot_f32, dot_i8, hamming, l2sq_f32, l2sq_i8};
use hnsw::{GraphBuilder, GraphReader, Scored, search_graph, search_graph_filtered};
use bbq::{BinaryQuantizer, BinaryQuery};
use quant::{ScalarQuantizer, normalized};

/// Lucene `VectorUtil.scaleMaxInnerProductScore`.
#[inline]
pub fn scale_max_inner_product(dot: f32) -> f32 {
    if dot < 0.0 {
        1.0 / (1.0 + -dot)
    } else {
        dot + 1.0
    }
}

#[inline]
fn cosine_f32(dot: f32, na: f32, nb: f32) -> f32 {
    let denom = ((na as f64) * (nb as f64)).sqrt();
    if denom == 0.0 {
        0.0
    } else {
        (dot as f64 / denom) as f32
    }
}

/// The score of two raw vectors of a field (both in storage encoding).
pub fn similarity_score(spec: &VectorSpec, a: &[u8], b: &[u8]) -> f32 {
    match spec.element {
        VectorElement::Float32 => match spec.similarity {
            VectorSimilarity::L2Norm => 1.0 / (1.0 + l2sq_f32(a, b)),
            VectorSimilarity::DotProduct => ((1.0 + dot_f32(a, b)) / 2.0).max(0.0),
            VectorSimilarity::Cosine => {
                let c = cosine_f32(dot_f32(a, b), dot_f32(a, a), dot_f32(b, b));
                ((1.0 + c) / 2.0).max(0.0)
            }
            VectorSimilarity::MaxInnerProduct => scale_max_inner_product(dot_f32(a, b)),
        },
        VectorElement::Byte => match spec.similarity {
            VectorSimilarity::L2Norm => 1.0 / (1.0 + l2sq_i8(a, b) as f32),
            VectorSimilarity::DotProduct => {
                0.5 + dot_i8(a, b) as f32 / (a.len() as f32 * (1 << 15) as f32)
            }
            VectorSimilarity::Cosine => {
                let d = dot_i8(a, b) as f64;
                let denom = (dot_i8(a, a) as f64 * dot_i8(b, b) as f64).sqrt();
                let c = if denom == 0.0 {
                    0.0
                } else {
                    (d / denom) as f32
                };
                (1.0 + c) / 2.0
            }
            VectorSimilarity::MaxInnerProduct => scale_max_inner_product(dot_i8(a, b) as f32),
        },
        VectorElement::Bit => {
            let bits = (a.len() * 8) as f32;
            (bits - hamming(a, b) as f32) / bits
        }
    }
}

/// Sections of a vector field: `(data, graph, quantized)`. The data section is the header,
/// then the caller's raw vectors (streamed as they are, never copied), then the tail.
pub(crate) type VectorSections = (DataSection, Option<Vec<u8>>, Option<Vec<u8>>);

/// The `VectorData` section around the raw vectors.
pub(crate) struct DataSection {
    pub head: Vec<u8>,
    pub tail: Vec<u8>,
}

impl DataSection {
    /// The whole section (tests and small fields).
    #[cfg(test)]
    pub(crate) fn join(&self, data: &[u8]) -> Vec<u8> {
        [&self.head[..], data, &self.tail[..]].concat()
    }
}

/// An existing graph to start a field's graph from (merges): `ords[i]` is the new ordinal of the
/// seed's ordinal `i`.
pub(crate) struct GraphSeed<'a> {
    pub reader: &'a VectorsReader,
    pub ords: Vec<u32>,
}

/// Builds the vector sections of a field.
pub(crate) fn write_vectors(
    spec: &VectorSpec,
    docs: &[u32],
    data: &[u8],
    max_doc: u32,
    seed: Option<GraphSeed<'_>>,
    threads: usize,
) -> Result<VectorSections> {
    let vb = spec.vector_bytes();
    let n = docs.len();
    if data.len() != n * vb {
        return Err(invalid(format!(
            "vector data length {} != {} vectors of {vb} bytes",
            data.len(),
            n
        )));
    }
    let cosine_norms =
        spec.element == VectorElement::Float32 && spec.similarity == VectorSimilarity::Cosine;
    let sparse = n as u32 != max_doc;
    let mut out = Vec::with_capacity(n * 4 + 64);
    out.put_u32(n as u32);
    out.put_u32(vb as u32);
    out.put_u8((cosine_norms as u8) | ((sparse as u8) << 1));
    write_docset(docs, max_doc, &mut out);
    if sparse {
        for &d in docs {
            out.put_u32(d);
        }
    }
    let mut tail = Vec::new();
    let mut norms = Vec::new();
    if cosine_norms {
        tail.reserve(n * 4);
        for v in data.chunks_exact(vb.max(1)).take(n) {
            let nv = dot_f32(v, v);
            norms.push(nv);
            tail.put_f32(nv);
        }
    }
    let vec_at = |i: u32| &data[i as usize * vb..(i as usize + 1) * vb];
    // Quantize first: like Lucene, a quantized field's graph is built with quantized scores,
    // which are several times cheaper than float distances.
    let dims = spec.dims as usize;
    let normalize = spec.similarity == VectorSimilarity::Cosine;
    let floats = |i: u32, tmp: &mut Vec<f32>| {
        if normalize {
            normalized(vec_at(i), tmp);
        } else {
            tmp.clear();
            tmp.extend(vec_at(i).chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())));
        }
    };
    let mut tmp = Vec::with_capacity(dims);
    let mut scratch = Vec::with_capacity(dims);
    let build = match spec.quantization {
        _ if spec.element != VectorElement::Float32 || n == 0 => BuildQuant::None,
        VectorQuantization::None => BuildQuant::None,
        VectorQuantization::Scalar { bits, confidence_interval } => {
            let q = ScalarQuantizer::train(data, dims, n, bits, confidence_interval, normalize);
            let cl = q.code_len(dims);
            let mut qb = Vec::with_capacity(n * (cl + 4) + 16);
            qb.put_f32(q.min);
            qb.put_f32(q.max);
            qb.put_u8(q.levels);
            let at = qb.len();
            let mut codes = vec![0u8; cl];
            for i in 0..n {
                floats(i as u32, &mut tmp);
                let corr = q.quantize(&tmp, &mut codes);
                qb.put_bytes(&codes);
                qb.put_f32(corr);
            }
            BuildQuant::Scalar { q, section: qb, at }
        }
        VectorQuantization::Binary => {
            let q = BinaryQuantizer::train(
                &mut (0..n as u32).map(|i| {
                    let mut v = Vec::with_capacity(dims);
                    floats(i, &mut v);
                    v
                }),
                dims,
                spec.similarity,
            );
            let mut qb = Vec::with_capacity(n * q.stride() + dims * 4 + 16);
            q.write(&mut qb);
            let at = qb.len();
            // Graph construction scores an inserted vector in its 4-bit query form against
            // the one-bit records (asymmetric, as at search time).
            let mut queries = Vec::with_capacity(if spec.hnsw.is_some() { n } else { 0 });
            for i in 0..n {
                floats(i as u32, &mut tmp);
                q.encode(&tmp, &mut scratch, &mut qb);
                if spec.hnsw.is_some() {
                    let mut bq = BinaryQuery::default();
                    q.query(&tmp, &mut scratch, &mut bq);
                    queries.push(bq);
                }
            }
            BuildQuant::Binary { q, section: qb, at, queries }
        }
    };
    let graph = match spec.hnsw {
        Some(p) if n > 0 => {
            let mut g = GraphBuilder::new(n, p.m as usize, p.ef_construction as usize);
            let sim = |a: u32, b: u32| -> f32 {
                match &build {
                    BuildQuant::Scalar { q, section, at } => {
                        let cl = q.code_len(dims);
                        let stride = cl + 4;
                        let row = |i: u32| &section[at + i as usize * stride..at + (i as usize + 1) * stride];
                        let (ra, rb) = (row(a), row(b));
                        let ca = f32::from_le_bytes(ra[cl..].try_into().unwrap());
                        let cb = f32::from_le_bytes(rb[cl..].try_into().unwrap());
                        let raw = q.raw(spec.similarity, &ra[..cl], ca, &rb[..cl], cb);
                        if spec.similarity == VectorSimilarity::L2Norm { -raw } else { raw }
                    }
                    BuildQuant::Binary { q, section, at, queries } => {
                        let stride = q.stride();
                        q.estimate(&queries[a as usize], &section[at + b as usize * stride..at + (b as usize + 1) * stride])
                    }
                    BuildQuant::None if cosine_norms => {
                        let c = cosine_f32(dot_f32(vec_at(a), vec_at(b)), norms[a as usize], norms[b as usize]);
                        ((1.0 + c) / 2.0).max(0.0)
                    }
                    BuildQuant::None => similarity_score(spec, vec_at(a), vec_at(b)),
                }
            };
            if let Some(seed) = &seed
                && let Some(sg) = &seed.reader.graph
                && sg.m() == g_m(&p)
                && seed.reader.spec.similarity == spec.similarity
                && seed.ords.iter().all(|&o| (o as usize) < n)
            {
                g.seed(sg, &seed.ords, &sim);
            }
            g.build(&sim, threads);
            let mut gb = Vec::new();
            g.write(&mut gb);
            Some(gb)
        }
        _ => None,
    };
    let quantized = match build {
        BuildQuant::None => None,
        BuildQuant::Scalar { section, .. } | BuildQuant::Binary { section, .. } => Some(section),
    };
    Ok((DataSection { head: out, tail }, graph, quantized))
}

/// The quantized copy of a field being written: the section bytes and where records start.
enum BuildQuant {
    None,
    Scalar { q: ScalarQuantizer, section: Vec<u8>, at: usize },
    Binary { q: BinaryQuantizer, section: Vec<u8>, at: usize, queries: Vec<BinaryQuery> },
}

fn g_m(p: &crate::info::HnswParams) -> usize {
    (p.m as usize).max(2)
}

/// A query vector.
#[derive(Clone, Copy, Debug)]
pub enum VectorQuery<'a> {
    Float(&'a [f32]),
    Byte(&'a [i8]),
    /// Packed bits (`dims / 8` bytes).
    Bits(&'a [u8]),
}

/// A query vector converted to the field's storage encoding, with cached norm and quantized form.
#[derive(Clone, Debug)]
pub struct PreparedQuery {
    bytes: Vec<u8>,
    norm: f32,
    quant: Option<QueryQuant>,
}

/// The quantized form of a prepared query.
#[derive(Clone, Debug)]
enum QueryQuant {
    Scalar(Vec<u8>, f32),
    Binary(BinaryQuery),
}

/// One k-NN hit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KnnHit {
    pub doc: u32,
    pub score: f32,
}

/// Result of a k-NN search.
#[derive(Clone, Debug, Default)]
pub struct KnnResult {
    /// Best hits first (ties by ascending doc).
    pub hits: Vec<KnnHit>,
    /// Number of vectors scored.
    pub visited: usize,
    /// False when the visit limit stopped the search early (fall back to exact search).
    pub complete: bool,
}

/// The quantized search copy of a field.
#[derive(Clone, Debug)]
enum Quantized {
    Scalar {
        q: ScalarQuantizer,
        codes: Slice,
        /// Bytes of codes per vector (dims, or half of them for packed 4-bit codes).
        code_len: usize,
    },
    Binary {
        q: BinaryQuantizer,
        records: Slice,
    },
}

/// Reads one vector field. Cheap to clone; `Send + Sync`.
#[derive(Clone, Debug)]
pub struct VectorsReader {
    spec: VectorSpec,
    count: u32,
    vb: usize,
    docs: DocSet,
    ord_to_doc: Option<Slice>,
    vectors: Slice,
    norms: Option<Slice>,
    graph: Option<GraphReader>,
    quant: Option<Quantized>,
}

impl VectorsReader {
    pub(crate) fn open(
        spec: &VectorSpec,
        data: Slice,
        graph: Option<Slice>,
        quant: Option<Slice>,
        max_doc: u32,
    ) -> Result<VectorsReader> {
        let mut r = In::new(&data);
        let count = r.u32()?;
        let vb = r.u32()? as usize;
        if vb != spec.vector_bytes() {
            return Err(corrupt("vector size mismatch"));
        }
        let flags = r.u8()?;
        let mut pos = r.pos();
        let docs = DocSet::read(&data, &mut pos)?;
        if docs.len() != count || docs.max_doc() != max_doc {
            return Err(corrupt("vector doc set mismatch"));
        }
        let ord_to_doc = if flags & 2 != 0 {
            let s = data
                .try_slice(pos as u64, count as u64 * 4)
                .ok_or_else(|| corrupt("vectors truncated"))?;
            pos += count as usize * 4;
            Some(s)
        } else {
            None
        };
        let vlen = (count as usize)
            .checked_mul(vb)
            .ok_or_else(|| corrupt("vector size overflow"))?;
        let vectors = data
            .try_slice(pos as u64, vlen as u64)
            .ok_or_else(|| corrupt("vectors truncated"))?;
        pos += vlen;
        let norms = if flags & 1 != 0 {
            Some(
                data.try_slice(pos as u64, count as u64 * 4)
                    .ok_or_else(|| corrupt("vector norms truncated"))?,
            )
        } else {
            None
        };
        let graph = match graph {
            Some(g) => {
                let g = GraphReader::open(g)?;
                if g.len() != count && g.len() != 0 {
                    return Err(corrupt("vector graph size does not match the vector count"));
                }
                Some(g)
            }
            None => None,
        };
        let quant = match (quant, spec.quantization) {
            (Some(qs), VectorQuantization::Binary) => {
                let (q, at) = BinaryQuantizer::read(&qs, spec.dims as usize, spec.similarity)
                    .ok_or_else(|| corrupt("bad binary quantization header"))?;
                let records = qs
                    .try_slice(at as u64, count as u64 * q.stride() as u64)
                    .ok_or_else(|| corrupt("quantized vectors truncated"))?;
                Some(Quantized::Binary { q, records })
            }
            (Some(qs), _) => {
                let mut r = In::new(&qs);
                let min = r.f32()?;
                let max = r.f32()?;
                let levels = r.u8()?;
                if !(min.is_finite() && max.is_finite() && min <= max) || levels == 0 {
                    return Err(corrupt("bad scalar quantization header"));
                }
                let q = ScalarQuantizer::new(min, max, levels);
                let code_len = q.code_len(spec.dims as usize);
                let codes = qs
                    .try_slice(r.pos() as u64, count as u64 * (code_len as u64 + 4))
                    .ok_or_else(|| corrupt("quantized vectors truncated"))?;
                Some(Quantized::Scalar { q, codes, code_len })
            }
            (None, _) => None,
        };
        Ok(VectorsReader {
            spec: *spec,
            count,
            vb,
            docs,
            ord_to_doc,
            vectors,
            norms,
            graph,
            quant,
        })
    }

    pub fn spec(&self) -> &VectorSpec {
        &self.spec
    }

    /// Number of vectors.
    pub fn len(&self) -> u32 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Docs with a vector.
    pub fn docs(&self) -> &DocSet {
        &self.docs
    }

    pub fn has_graph(&self) -> bool {
        self.graph.is_some()
    }

    /// Ordinal of `doc`'s vector.
    #[inline]
    pub fn ord(&self, doc: u32) -> Option<u32> {
        self.docs.rank(doc)
    }

    /// Doc of ordinal `ord`.
    #[inline]
    pub fn doc(&self, ord: u32) -> u32 {
        match &self.ord_to_doc {
            None => ord,
            Some(s) => u32::from_le_bytes(
                s[ord as usize * 4..ord as usize * 4 + 4]
                    .try_into()
                    .unwrap(),
            ),
        }
    }

    /// Raw vector bytes of `ord`.
    #[inline]
    pub fn vector(&self, ord: u32) -> &[u8] {
        &self.vectors[ord as usize * self.vb..(ord as usize + 1) * self.vb]
    }

    /// Decodes a float vector (for `Float32` fields).
    pub fn vector_f32(&self, ord: u32, out: &mut Vec<f32>) {
        out.clear();
        out.extend(
            self.vector(ord)
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes(c.try_into().unwrap())),
        );
    }

    /// Validates and converts a query vector.
    pub fn prepare(&self, q: VectorQuery<'_>) -> Result<PreparedQuery> {
        let dims = self.spec.dims as usize;
        let bytes: Vec<u8> = match (self.spec.element, q) {
            (VectorElement::Float32, VectorQuery::Float(v)) if v.len() == dims => {
                v.iter().flat_map(|x| x.to_le_bytes()).collect()
            }
            (VectorElement::Byte, VectorQuery::Byte(v)) if v.len() == dims => {
                v.iter().map(|&x| x as u8).collect()
            }
            (VectorElement::Byte, VectorQuery::Float(v)) if v.len() == dims => {
                v.iter().map(|&x| x as i8 as u8).collect()
            }
            (VectorElement::Bit, VectorQuery::Bits(v)) if v.len() == self.vb => v.to_vec(),
            (VectorElement::Bit, VectorQuery::Byte(v)) if v.len() == self.vb => {
                v.iter().map(|&x| x as u8).collect()
            }
            _ => {
                return Err(invalid(format!(
                    "the query vector has a different number of dimensions [{}] than the field [{}]",
                    match q {
                        VectorQuery::Float(v) => v.len(),
                        VectorQuery::Byte(v) => v.len(),
                        VectorQuery::Bits(v) => v.len() * 8,
                    },
                    dims
                )));
            }
        };
        let norm = if self.spec.element == VectorElement::Float32 {
            dot_f32(&bytes, &bytes)
        } else {
            0.0
        };
        let quant = self.quant.as_ref().map(|qz| {
            let mut tmp = Vec::with_capacity(dims);
            if self.spec.similarity == VectorSimilarity::Cosine {
                normalized(&bytes, &mut tmp);
            } else {
                tmp.extend(bytes.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())));
            }
            match qz {
                Quantized::Scalar { q, code_len, .. } => {
                    let mut codes = vec![0u8; *code_len];
                    let c = q.quantize(&tmp, &mut codes);
                    QueryQuant::Scalar(codes, c)
                }
                Quantized::Binary { q, .. } => {
                    let mut bq = BinaryQuery::default();
                    q.query(&tmp, &mut Vec::with_capacity(dims), &mut bq);
                    QueryQuant::Binary(bq)
                }
            }
        });
        Ok(PreparedQuery { bytes, norm, quant })
    }

    /// Exact score of `ord` against a prepared query.
    #[inline]
    pub fn score(&self, q: &PreparedQuery, ord: u32) -> f32 {
        let v = self.vector(ord);
        match &self.norms {
            Some(norms) => {
                let nv = f32::from_le_bytes(
                    norms[ord as usize * 4..ord as usize * 4 + 4]
                        .try_into()
                        .unwrap(),
                );
                let c = cosine_f32(dot_f32(&q.bytes, v), q.norm, nv);
                ((1.0 + c) / 2.0).max(0.0)
            }
            None => similarity_score(&self.spec, &q.bytes, v),
        }
    }

    /// Exact score of `doc`, if it has a vector.
    pub fn score_doc(&self, q: &PreparedQuery, doc: u32) -> Option<f32> {
        self.ord(doc).map(|o| self.score(q, o))
    }

    /// The bytes [`steer_score`](Self::steer_score) reads for `ord` (to prefetch them).
    #[inline]
    fn steer_bytes(&self, q: &PreparedQuery, ord: u32) -> &[u8] {
        let o = ord as usize;
        match (&self.quant, &q.quant) {
            (Some(Quantized::Scalar { codes, code_len, .. }), Some(QueryQuant::Scalar(..))) => {
                codes.get(o * (code_len + 4)..(o + 1) * (code_len + 4)).unwrap_or(&[])
            }
            (Some(Quantized::Binary { q, records }), Some(QueryQuant::Binary(_))) => {
                records.get(o * q.stride()..(o + 1) * q.stride()).unwrap_or(&[])
            }
            _ => self.vectors.get(o * self.vb..(o + 1) * self.vb).unwrap_or(&[]),
        }
    }

    /// Approximate (quantized) similarity used to steer graph search; higher is better.
    #[inline]
    fn steer_score(&self, q: &PreparedQuery, ord: u32) -> f32 {
        let o = ord as usize;
        match (&self.quant, &q.quant) {
            (Some(Quantized::Scalar { q: sq, codes, code_len }), Some(QueryQuant::Scalar(qc, qcorr))) => {
                let cl = *code_len;
                let row = &codes[o * (cl + 4)..(o + 1) * (cl + 4)];
                let corr = f32::from_le_bytes(row[cl..].try_into().unwrap());
                let raw = sq.raw(self.spec.similarity, qc, *qcorr, &row[..cl], corr);
                if self.spec.similarity == VectorSimilarity::L2Norm { -raw } else { raw }
            }
            (Some(Quantized::Binary { q: bq, records }), Some(QueryQuant::Binary(query))) => {
                bq.estimate(query, &records[o * bq.stride()..(o + 1) * bq.stride()])
            }
            _ => self.score(q, ord),
        }
    }

    /// Brute-force k-NN over accepted docs with exact scores.
    pub fn exact_search(
        &self,
        q: &PreparedQuery,
        k: usize,
        accept: Option<&dyn Fn(u32) -> bool>,
    ) -> KnnResult {
        let (top, visited) = self.scan(k, accept, |ord| self.score(q, ord));
        KnnResult { hits: self.hits(top), visited, complete: true }
    }

    /// The `k` best accepted ordinals by `score` (best first) and the number scored.
    fn scan(&self, k: usize, accept: Option<&dyn Fn(u32) -> bool>, score: impl Fn(u32) -> f32) -> (Vec<Scored>, usize) {
        let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<Scored>> = std::collections::BinaryHeap::new();
        let mut visited = 0;
        if k > 0 {
            for ord in 0..self.count {
                if let Some(a) = accept
                    && !a(self.doc(ord))
                {
                    continue;
                }
                visited += 1;
                let s = Scored { score: score(ord), ord };
                if heap.len() < k {
                    heap.push(std::cmp::Reverse(s));
                } else if s > heap.peek().unwrap().0 {
                    heap.pop();
                    heap.push(std::cmp::Reverse(s));
                }
            }
        }
        let mut v: Vec<Scored> = heap.into_iter().map(|r| r.0).collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        (v, visited)
    }

    fn hits(&self, v: Vec<Scored>) -> Vec<KnnHit> {
        v.into_iter().map(|s| KnnHit { doc: self.doc(s.ord), score: s.score }).collect()
    }

    /// Rescores candidates with exact scores and keeps the best `k`.
    fn rescore(&self, q: &PreparedQuery, cands: &[Scored], k: usize) -> Vec<KnnHit> {
        for s in cands {
            distance::prefetch(self.vector(s.ord));
        }
        let mut v: Vec<Scored> = cands.iter().map(|s| Scored { score: self.score(q, s.ord), ord: s.ord }).collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v.truncate(k);
        self.hits(v)
    }

    /// Approximate k-NN with the HNSW graph (exact when the field has no graph).
    pub fn search(
        &self,
        q: &PreparedQuery,
        k: usize,
        num_candidates: usize,
        accept: Option<&dyn Fn(u32) -> bool>,
        visit_limit: usize,
    ) -> KnnResult {
        let Some(g) = &self.graph else {
            if q.quant.is_some() && self.quant.is_some() {
                // A quantized flat field (`int8_flat`, `bbq_flat`): scan the compact codes for
                // the best `num_candidates`, then rescore those exactly.
                let (cands, visited) = self.scan(k.max(num_candidates), accept, |ord| self.steer_score(q, ord));
                return KnnResult { hits: self.rescore(q, &cands, k), visited, complete: true };
            }
            return self.exact_search(q, k, accept);
        };
        if k == 0 || self.count == 0 {
            return KnnResult {
                hits: Vec::new(),
                visited: 0,
                complete: true,
            };
        }
        let ef = k.max(num_candidates);
        let accept_ord = accept.map(|a| move |ord: u32| a(self.doc(ord)));
        let accept_dyn: Option<&dyn Fn(u32) -> bool> =
            accept_ord.as_ref().map(|f| f as &dyn Fn(u32) -> bool);
        let mut score = |ord: u32| self.steer_score(q, ord);
        let prefetch = |ord: u32| distance::prefetch(self.steer_bytes(q, ord));
        let (beam, visited, complete) = search_graph(
            g,
            self.count as usize,
            &mut score,
            &prefetch,
            ef,
            accept_dyn,
            visit_limit,
        );
        KnnResult {
            hits: self.rescore(q, &beam, k),
            visited,
            complete,
        }
    }

    /// Approximate k-NN over the docs `accept` passes, `accepted` of them (the filter's
    /// cardinality; an overestimate is fine). Like Lucene, filters passing under 60% of the
    /// vectors use the filtered graph search, which scores only accepted vectors and steps
    /// over rejected ones to their neighbors; wider filters use [`search`](Self::search).
    /// `visit_limit` is usually `accepted`: past it, exact search over the filter is cheaper
    /// (`complete == false`).
    pub fn search_filtered(
        &self,
        q: &PreparedQuery,
        k: usize,
        num_candidates: usize,
        accept: &dyn Fn(u32) -> bool,
        accepted: u32,
        visit_limit: usize,
    ) -> KnnResult {
        let Some(g) = &self.graph else {
            return self.search(q, k, num_candidates, Some(accept), visit_limit);
        };
        if k == 0 || self.count == 0 || accepted == 0 {
            return KnnResult { hits: Vec::new(), visited: 0, complete: true };
        }
        if accepted as u64 * 100 >= self.count as u64 * FILTERED_SEARCH_THRESHOLD {
            return self.search(q, k, num_candidates, Some(accept), visit_limit);
        }
        let ef = k.max(num_candidates);
        let accept_ord = |ord: u32| accept(self.doc(ord));
        let mut score = |ord: u32| self.steer_score(q, ord);
        let prefetch = |ord: u32| distance::prefetch(self.steer_bytes(q, ord));
        let (beam, visited, complete) = search_graph_filtered(
            g,
            self.count as usize,
            g.m(),
            &mut score,
            &prefetch,
            ef,
            &accept_ord,
            accepted as usize,
            visit_limit,
        );
        KnnResult { hits: self.rescore(q, &beam, k), visited, complete }
    }
}

/// Filters passing fewer than this percentage of a field's vectors use the filtered graph
/// search (Lucene's `KnnSearchStrategy.DEFAULT_FILTERED_SEARCH_THRESHOLD`).
const FILTERED_SEARCH_THRESHOLD: u64 = 60;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::info::HnswParams;
    use super::hnsw::Adjacency;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn spec(
        sim: VectorSimilarity,
        element: VectorElement,
        dims: u32,
        hnsw: bool,
        quant: bool,
    ) -> VectorSpec {
        VectorSpec {
            dims,
            element,
            similarity: sim,
            hnsw: hnsw.then_some(HnswParams {
                m: 16,
                ef_construction: 100,
            }),
            quantization: if quant {
                VectorQuantization::Scalar {
                    bits: 8,
                    confidence_interval: None,
                }
            } else {
                VectorQuantization::None
            },
        }
    }

    fn open(spec: &VectorSpec, docs: &[u32], data: &[u8], max_doc: u32) -> VectorsReader {
        let (d, g, q) = write_vectors(spec, docs, data, max_doc, None, 1).unwrap();
        let d = d.join(data);
        VectorsReader::open(
            spec,
            Slice::from(d),
            g.map(Slice::from),
            q.map(Slice::from),
            max_doc,
        )
        .unwrap()
    }

    #[test]
    fn lucene_score_formulas() {
        let f = |v: &[f32]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        let a = f(&[1.0, 2.0, 3.0]);
        let b = f(&[4.0, 5.0, 6.0]);
        let s = |sim| similarity_score(&spec(sim, VectorElement::Float32, 3, false, false), &a, &b);
        assert_eq!(s(VectorSimilarity::L2Norm), 1.0 / 28.0);
        assert_eq!(s(VectorSimilarity::DotProduct), 16.5);
        assert_eq!(s(VectorSimilarity::MaxInnerProduct), 33.0);
        let cos = 32.0 / (14f64.sqrt() * 77f64.sqrt());
        assert!((s(VectorSimilarity::Cosine) as f64 - (1.0 + cos) / 2.0).abs() < 1e-6);
        assert_eq!(scale_max_inner_product(-3.0), 0.25);
        let ba = [1u8, 2, 3];
        let bb = [255u8, 5, 6]; // -1, 5, 6
        let bs = |sim| similarity_score(&spec(sim, VectorElement::Byte, 3, false, false), &ba, &bb);
        assert_eq!(
            bs(VectorSimilarity::DotProduct),
            0.5 + 27.0 / (3.0 * 32768.0)
        );
        assert_eq!(bs(VectorSimilarity::L2Norm), 1.0 / (1.0 + 22.0));
        let bits = similarity_score(
            &spec(
                VectorSimilarity::L2Norm,
                VectorElement::Bit,
                16,
                false,
                false,
            ),
            &[0xFF, 0],
            &[0x0F, 0],
        );
        assert_eq!(bits, 12.0 / 16.0);
    }

    fn random_data(rng: &mut StdRng, n: usize, dims: usize) -> Vec<u8> {
        (0..n * dims)
            .flat_map(|_| rng.gen_range(-1.0f32..1.0).to_le_bytes())
            .collect()
    }

    #[test]
    fn hnsw_recall_and_filters() {
        let mut rng = StdRng::seed_from_u64(7);
        let dims = 16;
        let n = 3000;
        let data = random_data(&mut rng, n, dims);
        for (sim, quant) in [
            (VectorSimilarity::L2Norm, false),
            (VectorSimilarity::Cosine, false),
            (VectorSimilarity::DotProduct, true),
            (VectorSimilarity::Cosine, true),
            (VectorSimilarity::L2Norm, true),
        ] {
            let sp = spec(sim, VectorElement::Float32, dims as u32, true, quant);
            // Sparse docs: every other doc has a vector.
            let docs: Vec<u32> = (0..n as u32).map(|i| i * 2).collect();
            let r = open(&sp, &docs, &data, n as u32 * 2);
            assert_eq!(r.len(), n as u32);
            let mut recall = 0usize;
            let mut total = 0usize;
            for _ in 0..30 {
                let qv: Vec<f32> = (0..dims).map(|_| rng.gen_range(-1.0..1.0)).collect();
                let q = r.prepare(VectorQuery::Float(&qv)).unwrap();
                let exact = r.exact_search(&q, 10, None);
                let approx = r.search(&q, 10, 50, None, usize::MAX);
                assert!(approx.complete);
                assert_eq!(approx.hits.len(), 10);
                for h in &approx.hits {
                    assert_eq!(r.score_doc(&q, h.doc), Some(h.score));
                    assert_eq!(h.doc % 2, 0);
                }
                let ex: std::collections::HashSet<u32> = exact.hits.iter().map(|h| h.doc).collect();
                recall += approx.hits.iter().filter(|h| ex.contains(&h.doc)).count();
                total += 10;
                // Filtered: only docs divisible by 6.
                let accept = |d: u32| d.is_multiple_of(6);
                let fa = r.search(&q, 5, 50, Some(&accept), usize::MAX);
                assert!(fa.hits.iter().all(|h| h.doc % 6 == 0));
                let fe = r.exact_search(&q, 5, Some(&accept));
                assert!(fe.hits.iter().all(|h| h.doc % 6 == 0));
                assert_eq!(fe.hits.len(), 5);
                // Visit limit triggers incompleteness.
                let limited = r.search(&q, 5, 50, Some(&accept), 10);
                assert!(!limited.complete);
            }
            let rec = recall as f64 / total as f64;
            assert!(rec > 0.9, "{sim:?} quant={quant} recall {rec}");
        }
    }

    fn recall(r: &VectorsReader, rng: &mut StdRng, dims: usize) -> f64 {
        let (mut hit, mut total) = (0usize, 0usize);
        for _ in 0..30 {
            let qv: Vec<f32> = (0..dims).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let q = r.prepare(VectorQuery::Float(&qv)).unwrap();
            let exact: std::collections::HashSet<u32> =
                r.exact_search(&q, 10, None).hits.iter().map(|h| h.doc).collect();
            let approx = r.search(&q, 10, 50, None, usize::MAX);
            hit += approx.hits.iter().filter(|h| exact.contains(&h.doc)).count();
            total += 10;
        }
        hit as f64 / total as f64
    }

    #[test]
    fn int4_quantized_search_recall() {
        let mut rng = StdRng::seed_from_u64(23);
        for dims in [15usize, 32] {
            for sim in [VectorSimilarity::L2Norm, VectorSimilarity::Cosine, VectorSimilarity::DotProduct] {
                let mut sp = spec(sim, VectorElement::Float32, dims as u32, true, true);
                sp.quantization = VectorQuantization::Scalar { bits: 4, confidence_interval: None };
                let data = random_data(&mut rng, 2000, dims);
                let r = open(&sp, &(0..2000).collect::<Vec<u32>>(), &data, 2000);
                assert!(matches!(r.quant, Some(Quantized::Scalar { code_len, .. }) if code_len == dims.div_ceil(2)));
                let rec = recall(&r, &mut rng, dims);
                assert!(rec > 0.85, "{sim:?} dims {dims} recall {rec}");
            }
        }
    }

    fn clustered(rng: &mut StdRng, n: usize, centers: &[Vec<f32>]) -> Vec<u8> {
        (0..n)
            .flat_map(|_| {
                let c = &centers[rng.gen_range(0..centers.len())];
                c.iter().map(|x| x + rng.gen_range(-0.3f32..0.3)).flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>()
            })
            .collect()
    }

    #[test]
    fn binary_quantized_search_recall() {
        let mut rng = StdRng::seed_from_u64(29);
        for (dims, sims) in [
            (96usize, &[VectorSimilarity::Cosine, VectorSimilarity::DotProduct, VectorSimilarity::L2Norm, VectorSimilarity::MaxInnerProduct][..]),
            (200, &[VectorSimilarity::Cosine][..]),
        ] {
            let centers: Vec<Vec<f32>> = (0..50).map(|_| (0..dims).map(|_| rng.gen_range(-1.0..1.0)).collect()).collect();
            for &sim in sims {
                let mut sp = spec(sim, VectorElement::Float32, dims as u32, true, false);
                sp.quantization = VectorQuantization::Binary;
                let n = 2000;
                let data = clustered(&mut rng, n, &centers);
                let docs: Vec<u32> = (0..n as u32).map(|i| i * 2).collect();
                let r = open(&sp, &docs, &data, 2 * n as u32);
                assert!(matches!(r.quant, Some(Quantized::Binary { .. })));
                let (mut hit, mut total) = (0, 0);
                for _ in 0..20 {
                    let c = &centers[rng.gen_range(0..centers.len())];
                    let qv: Vec<f32> = c.iter().map(|x| x + rng.gen_range(-0.3f32..0.3)).collect();
                    let q = r.prepare(VectorQuery::Float(&qv)).unwrap();
                    let exact: std::collections::HashSet<u32> = r.exact_search(&q, 10, None).hits.iter().map(|h| h.doc).collect();
                    let approx = r.search(&q, 10, 100, None, usize::MAX);
                    for h in &approx.hits {
                        assert_eq!(r.score_doc(&q, h.doc), Some(h.score), "scores are exact");
                    }
                    hit += approx.hits.iter().filter(|h| exact.contains(&h.doc)).count();
                    total += 10;
                    // Filtered search still only returns accepted docs.
                    let accept = |d: u32| d.is_multiple_of(4);
                    assert!(r.search(&q, 5, 50, Some(&accept), usize::MAX).hits.iter().all(|h| h.doc % 4 == 0));
                }
                let rec = hit as f64 / total as f64;
                assert!(rec > 0.9, "{sim:?} dims {dims} recall {rec}");
            }
        }
    }

    #[test]
    fn quantized_flat_fields_scan_codes_then_rescore() {
        let mut rng = StdRng::seed_from_u64(31);
        let dims = 128;
        let centers: Vec<Vec<f32>> = (0..30).map(|_| (0..dims).map(|_| rng.gen_range(-1.0..1.0)).collect()).collect();
        let data = clustered(&mut rng, 3000, &centers);
        for quant in [VectorQuantization::Binary, VectorQuantization::Scalar { bits: 8, confidence_interval: None }] {
            let mut sp = spec(VectorSimilarity::Cosine, VectorElement::Float32, dims as u32, false, false);
            sp.quantization = quant;
            let r = open(&sp, &(0..3000).collect::<Vec<u32>>(), &data, 3000);
            assert!(!r.has_graph() && r.quant.is_some());
            let (mut hit, mut total) = (0, 0);
            for _ in 0..20 {
                let c = &centers[rng.gen_range(0..centers.len())];
                let qv: Vec<f32> = c.iter().map(|x| x + rng.gen_range(-0.3f32..0.3)).collect();
                let q = r.prepare(VectorQuery::Float(&qv)).unwrap();
                let exact = r.exact_search(&q, 10, None);
                let got = r.search(&q, 10, 100, None, usize::MAX);
                assert_eq!(got.visited, 3000);
                for h in &got.hits {
                    assert_eq!(r.score_doc(&q, h.doc), Some(h.score));
                }
                let ex: std::collections::HashSet<u32> = exact.hits.iter().map(|h| h.doc).collect();
                hit += got.hits.iter().filter(|h| ex.contains(&h.doc)).count();
                total += 10;
            }
            assert!(hit as f64 / total as f64 > 0.95, "{quant:?}: {hit}/{total}");
        }
    }

    /// Concurrent construction must not degrade the graph: inserted nodes are linked
    /// bottom-up, own lists before reverse links, so concurrent searches never hit an
    /// unlinked node (which used to cost about 1 in 250 top-1 results here).
    #[test]
    fn concurrent_build_keeps_recall() {
        let mut rng = StdRng::seed_from_u64(1);
        let (dims, n) = (8usize, 3000usize);
        let data = random_data(&mut rng, n, dims);
        let mut sp = spec(VectorSimilarity::Cosine, VectorElement::Float32, dims as u32, true, false);
        sp.hnsw = Some(HnswParams { m: 8, ef_construction: 50 });
        let docs: Vec<u32> = (0..n as u32).collect();
        let (mut miss40, mut miss200) = (0, 0);
        for _ in 0..3 {
            let (d, g, q) = write_vectors(&sp, &docs, &data, n as u32, None, 8).unwrap();
            let d = d.join(&data);
            let r = VectorsReader::open(&sp, Slice::from(d), g.map(Slice::from), q.map(Slice::from), n as u32).unwrap();
            let mut qrng = StdRng::seed_from_u64(99);
            for _ in 0..300 {
                let qv: Vec<f32> = (0..dims).map(|_| qrng.gen_range(-1.0..1.0)).collect();
                let pq = r.prepare(VectorQuery::Float(&qv)).unwrap();
                let ex = r.exact_search(&pq, 1, None);
                miss40 += (r.search(&pq, 1, 40, None, usize::MAX).hits != ex.hits) as usize;
                miss200 += (r.search(&pq, 1, 200, None, usize::MAX).hits != ex.hits) as usize;
            }
        }
        assert!(miss40 <= 1 && miss200 == 0, "top-1 misses: {miss40} at ef 40, {miss200} at ef 200");
    }

    /// Every node stays reachable from the entry point, even when the diversity heuristic
    /// leaves some without in-links (many duplicates and near-duplicates).
    #[test]
    fn every_node_is_reachable() {
        let mut rng = StdRng::seed_from_u64(5);
        let dims = 4usize;
        let n = 2000usize;
        // Tight clusters of near-identical vectors around few centers.
        let centers: Vec<Vec<f32>> = (0..5).map(|_| (0..dims).map(|_| rng.gen_range(-1.0f32..1.0)).collect()).collect();
        let data: Vec<u8> = (0..n)
            .flat_map(|i| {
                let c = &centers[i % centers.len()];
                c.iter().map(|x| x + if i % 3 == 0 { 0.0 } else { rng.gen_range(-1e-3f32..1e-3) }).collect::<Vec<_>>()
            })
            .flat_map(|x| x.to_le_bytes())
            .collect();
        for threads in [1, 8] {
            let mut sp = spec(VectorSimilarity::L2Norm, VectorElement::Float32, dims as u32, true, false);
            sp.hnsw = Some(HnswParams { m: 4, ef_construction: 20 });
            let docs: Vec<u32> = (0..n as u32).collect();
            let (_, g, _) = write_vectors(&sp, &docs, &data, n as u32, None, threads).unwrap();
            let g = GraphReader::open(Slice::from(g.unwrap())).unwrap();
            for level in 0..g.num_levels() {
                let nodes: Vec<u32> = if level == 0 { (0..n as u32).collect() } else { g.level_nodes(level).collect() };
                let mut seen = vec![false; n];
                let mut stack = vec![g.entry().unwrap()];
                seen[stack[0] as usize] = true;
                while let Some(o) = stack.pop() {
                    g.for_each_neighbor(level, o, |nb| {
                        if !seen[nb as usize] {
                            seen[nb as usize] = true;
                            stack.push(nb);
                        }
                    });
                }
                let lost = nodes.iter().filter(|&&o| !seen[o as usize]).count();
                assert_eq!(lost, 0, "threads {threads} level {level}: {lost} unreachable nodes");
            }
        }
    }

    /// Restrictive filters: the filtered (ACORN-style) search keeps recall while scoring far
    /// fewer vectors than the plain beam search, which must wade through rejected nodes.
    #[test]
    fn filtered_search_recall_and_cost() {
        let mut rng = StdRng::seed_from_u64(31);
        let (dims, n) = (16usize, 6000usize);
        let data = random_data(&mut rng, n, dims);
        for quant in [false, true] {
            let sp = spec(VectorSimilarity::L2Norm, VectorElement::Float32, dims as u32, true, quant);
            let docs: Vec<u32> = (0..n as u32).collect();
            let r = open(&sp, &docs, &data, n as u32);
            for modulo in [3u32, 20, 100] {
                let accept = move |d: u32| d.wrapping_mul(2654435761).is_multiple_of(modulo);
                let accepted = (0..n as u32).filter(|&d| accept(d)).count() as u32;
                let (mut hit_f, mut hit_p, mut total) = (0usize, 0usize, 0usize);
                let (mut cost_f, mut cost_p) = (0usize, 0usize);
                for _ in 0..20 {
                    let qv: Vec<f32> = (0..dims).map(|_| rng.gen_range(-1.0..1.0)).collect();
                    let q = r.prepare(VectorQuery::Float(&qv)).unwrap();
                    let exact: std::collections::HashSet<u32> =
                        r.exact_search(&q, 10, Some(&accept)).hits.iter().map(|h| h.doc).collect();
                    let f = r.search_filtered(&q, 10, 50, &accept, accepted, usize::MAX);
                    let p = r.search(&q, 10, 50, Some(&accept), usize::MAX);
                    assert!(f.complete && f.hits.len() == 10);
                    assert!(f.hits.iter().all(|h| accept(h.doc)));
                    assert!(f.hits.windows(2).all(|w| w[0].score >= w[1].score));
                    hit_f += f.hits.iter().filter(|h| exact.contains(&h.doc)).count();
                    hit_p += p.hits.iter().filter(|h| exact.contains(&h.doc)).count();
                    total += 10;
                    cost_f += f.visited;
                    cost_p += p.visited;
                }
                let (rf, rp) = (hit_f as f64 / total as f64, hit_p as f64 / total as f64);
                assert!(rf > 0.9, "quant {quant} 1/{modulo}: filtered recall {rf} (plain {rp})");
                if modulo >= 20 {
                    assert!(cost_f < cost_p, "quant {quant} 1/{modulo}: filtered scored {cost_f}, plain {cost_p}");
                }
                // A visit limit below what the search needs reports incompleteness.
                let qv: Vec<f32> = (0..dims).map(|_| rng.gen_range(-1.0..1.0)).collect();
                let q = r.prepare(VectorQuery::Float(&qv)).unwrap();
                assert!(!r.search_filtered(&q, 10, 50, &accept, accepted, 5).complete);
            }
            // Nothing accepted, and a filter accepting everything (falls back to plain search).
            let q = r.prepare(VectorQuery::Float(&vec![0.1; dims])).unwrap();
            assert!(r.search_filtered(&q, 5, 20, &|_| false, 0, usize::MAX).hits.is_empty());
            let all = r.search_filtered(&q, 5, 20, &|_| true, n as u32, usize::MAX);
            assert_eq!(all.hits, r.search(&q, 5, 20, None, usize::MAX).hits);
        }
    }

    #[test]
    fn seeded_graph_keeps_recall() {
        let mut rng = StdRng::seed_from_u64(21);
        let dims = 16;
        for quant in [false, true] {
            let sp = spec(VectorSimilarity::DotProduct, VectorElement::Float32, dims as u32, true, quant);
            let a = random_data(&mut rng, 2000, dims);
            let b = random_data(&mut rng, 800, dims);
            let ra = open(&sp, &(0..2000).collect::<Vec<u32>>(), &a, 2000);
            // Merged order: b's vectors first, then a's (so the seed's ordinals move).
            let mut all = b.clone();
            all.extend_from_slice(&a);
            let docs: Vec<u32> = (0..2800).collect();
            let seed = GraphSeed { reader: &ra, ords: (800..2800).collect() };
            let (d, g, q) = write_vectors(&sp, &docs, &all, 2800, Some(seed), 4).unwrap();
            let d = d.join(&all);
            let merged = VectorsReader::open(&sp, Slice::from(d), g.map(Slice::from), q.map(Slice::from), 2800).unwrap();
            let rec = recall(&merged, &mut rng, dims);
            assert!(rec > 0.9, "quant={quant} recall {rec}");
            for o in 0..2800u32 {
                assert_eq!(merged.vector(o), &all[o as usize * dims * 4..(o as usize + 1) * dims * 4]);
            }
        }
    }

    /// `cargo test --release -p search-codec --lib bench_hnsw -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_hnsw() {
        let mut rng = StdRng::seed_from_u64(5);
        let (n, dims) = (20_000usize, 384usize);
        // Clustered data (embeddings are far from uniform): 200 centers plus noise.
        let centers: Vec<Vec<f32>> = (0..200).map(|_| (0..dims).map(|_| rng.gen_range(-1.0..1.0)).collect()).collect();
        let data: Vec<u8> = (0..n)
            .flat_map(|_| {
                let c = &centers[rng.gen_range(0..centers.len())];
                c.iter().map(|x| x + rng.gen_range(-0.3f32..0.3)).flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>()
            })
            .collect();
        let docs: Vec<u32> = (0..n as u32).collect();
        for (name, quant, threads) in [
            ("float", VectorQuantization::None, 1),
            ("int8", VectorQuantization::Scalar { bits: 8, confidence_interval: None }, 1),
            ("int8 x8", VectorQuantization::Scalar { bits: 8, confidence_interval: None }, 8),
            ("int4", VectorQuantization::Scalar { bits: 4, confidence_interval: None }, 1),
            ("bbq", VectorQuantization::Binary, 1),
            ("bbq x8", VectorQuantization::Binary, 8),
        ] {
            let mut sp = spec(VectorSimilarity::Cosine, VectorElement::Float32, dims as u32, true, false);
            sp.quantization = quant;
            let t = std::time::Instant::now();
            let (d, g, q) = write_vectors(&sp, &docs, &data, n as u32, None, threads).unwrap();
            let d = d.join(&data);
            let build = t.elapsed();
            let r = VectorsReader::open(&sp, Slice::from(d), g.map(Slice::from), q.map(Slice::from), n as u32).unwrap();
            let queries: Vec<Vec<f32>> = (0..200)
                .map(|_| {
                    let c = &centers[rng.gen_range(0..centers.len())];
                    c.iter().map(|x| x + rng.gen_range(-0.3f32..0.3)).collect()
                })
                .collect();
            let t = std::time::Instant::now();
            let mut visited = 0;
            for qv in &queries {
                let pq = r.prepare(VectorQuery::Float(qv)).unwrap();
                visited += r.search(&pq, 10, 100, None, usize::MAX).visited;
            }
            let lat = t.elapsed() / queries.len() as u32;
            let t = std::time::Instant::now();
            let mut acc = 0f32;
            let mut x = 12345u32;
            for qv in &queries {
                let pq = r.prepare(VectorQuery::Float(qv)).unwrap();
                for _ in 0..519 {
                    x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                    acc += r.steer_score(&pq, x % n as u32);
                }
            }
            println!("  519 random steer scores: {:?}/query ({acc})", t.elapsed() / queries.len() as u32);
            let rec = recall(&r, &mut rng, dims);
            println!(
                "{name}: build {:.1} µs/vector, search {lat:?}/query ({} visited), recall@10 {rec:.3}",
                build.as_secs_f64() * 1e6 / n as f64,
                visited / queries.len()
            );
            for ef in [100usize, 200, 400] {
                let (mut hit, mut total) = (0, 0);
                for qv in queries.iter().take(30) {
                    let pq = r.prepare(VectorQuery::Float(qv)).unwrap();
                    let exact: std::collections::HashSet<u32> =
                        r.exact_search(&pq, 10, None).hits.iter().map(|h| h.doc).collect();
                    hit += r.search(&pq, 10, ef, None, usize::MAX).hits.iter().filter(|h| exact.contains(&h.doc)).count();
                    total += 10;
                }
                println!("  ef {ef}: recall@10 {:.3}", hit as f64 / total as f64);
            }
        }
    }

    #[test]
    fn byte_and_bit_vectors_exact() {
        let mut rng = StdRng::seed_from_u64(3);
        let n = 500;
        let data: Vec<u8> = (0..n * 8).map(|_| rng.r#gen()).collect();
        for (el, dims) in [(VectorElement::Byte, 8u32), (VectorElement::Bit, 64)] {
            let sp = spec(VectorSimilarity::L2Norm, el, dims, true, false);
            let docs: Vec<u32> = (0..n as u32).collect();
            let r = open(&sp, &docs, &data, n as u32);
            let q = match el {
                VectorElement::Byte => {
                    let v: Vec<i8> = (0..8).map(|_| rng.r#gen()).collect();
                    r.prepare(VectorQuery::Byte(&v)).unwrap()
                }
                _ => {
                    let v: Vec<u8> = (0..8).map(|_| rng.r#gen()).collect();
                    r.prepare(VectorQuery::Bits(&v)).unwrap()
                }
            };
            let e = r.exact_search(&q, 3, None);
            let best = (0..n as u32)
                .map(|o| r.score(&q, o))
                .fold(f32::MIN, f32::max);
            assert_eq!(e.hits[0].score, best);
            let a = r.search(&q, 3, 100, None, usize::MAX);
            assert_eq!(a.hits.len(), 3);
        }
    }

    #[test]
    fn wrong_dims_rejected() {
        let sp = spec(
            VectorSimilarity::L2Norm,
            VectorElement::Float32,
            4,
            false,
            false,
        );
        let r = open(&sp, &[0], &[0u8; 16], 1);
        assert!(r.prepare(VectorQuery::Float(&[1.0, 2.0])).is_err());
    }

    #[test]
    fn empty_field() {
        let sp = spec(
            VectorSimilarity::Cosine,
            VectorElement::Float32,
            4,
            true,
            true,
        );
        let r = open(&sp, &[], &[], 10);
        let q = r
            .prepare(VectorQuery::Float(&[1.0, 0.0, 0.0, 0.0]))
            .unwrap();
        assert!(r.search(&q, 3, 10, None, usize::MAX).hits.is_empty());
        assert!(r.exact_search(&q, 3, None).hits.is_empty());
    }
}
