//! Segment and field metadata: what each field indexes and how.
//!
//! `FieldInfo` mirrors Lucene's: index options (docs/freqs/positions/offsets), norms, payloads,
//! the doc values type (plus whether it carries a skip index), the points shape and the vector
//! shape. Field numbers are assigned by the shard and are stable across its segments, so a
//! segment's fields may be a sparse subset of the shard's numbering. `SegmentInfo` adds the
//! segment id, doc count, optional index sort and free-form attributes. Both serialize into the
//! `Meta` section with a compact varint encoding.

use std::collections::BTreeMap;

use crate::error::{Result, corrupt};
use crate::io::{In, Out};

/// What an inverted field records per term.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IndexOptions {
    #[default]
    None = 0,
    Docs = 1,
    DocsAndFreqs = 2,
    DocsAndFreqsAndPositions = 3,
    DocsAndFreqsAndPositionsAndOffsets = 4,
}

impl IndexOptions {
    pub fn is_indexed(self) -> bool {
        self != IndexOptions::None
    }
    pub fn has_freqs(self) -> bool {
        self >= IndexOptions::DocsAndFreqs
    }
    pub fn has_positions(self) -> bool {
        self >= IndexOptions::DocsAndFreqsAndPositions
    }
    pub fn has_offsets(self) -> bool {
        self >= IndexOptions::DocsAndFreqsAndPositionsAndOffsets
    }
    fn from_u8(v: u8) -> Result<IndexOptions> {
        Ok(match v {
            0 => IndexOptions::None,
            1 => IndexOptions::Docs,
            2 => IndexOptions::DocsAndFreqs,
            3 => IndexOptions::DocsAndFreqsAndPositions,
            4 => IndexOptions::DocsAndFreqsAndPositionsAndOffsets,
            _ => return Err(corrupt(format!("bad index options {v}"))),
        })
    }
}

/// Doc values type of a field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DocValuesType {
    #[default]
    None = 0,
    Numeric = 1,
    Binary = 2,
    Sorted = 3,
    SortedSet = 4,
    SortedNumeric = 5,
}

impl DocValuesType {
    fn from_u8(v: u8) -> Result<DocValuesType> {
        Ok(match v {
            0 => DocValuesType::None,
            1 => DocValuesType::Numeric,
            2 => DocValuesType::Binary,
            3 => DocValuesType::Sorted,
            4 => DocValuesType::SortedSet,
            5 => DocValuesType::SortedNumeric,
            _ => return Err(corrupt(format!("bad doc values type {v}"))),
        })
    }
}

/// Shape of a points field: `dims` data dimensions of which the first `index_dims` are indexed
/// (the rest are carried along, as for shape triangles), each `bytes_per_dim` wide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PointsSpec {
    pub dims: u8,
    pub index_dims: u8,
    pub bytes_per_dim: u8,
}

impl PointsSpec {
    pub fn new(dims: u8, bytes_per_dim: u8) -> PointsSpec {
        PointsSpec {
            dims,
            index_dims: dims,
            bytes_per_dim,
        }
    }
    pub fn packed_len(&self) -> usize {
        self.dims as usize * self.bytes_per_dim as usize
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if self.dims == 0
            || self.dims > 16
            || self.index_dims == 0
            || self.index_dims > self.dims
            || self.index_dims > 8
            || self.bytes_per_dim == 0
            || self.bytes_per_dim > 32
        {
            return Err(corrupt(format!("invalid points spec {self:?}")));
        }
        Ok(())
    }
}

/// Vector component type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VectorElement {
    /// 32-bit floats, little-endian, 4 bytes per dimension.
    Float32 = 0,
    /// Signed bytes, one per dimension.
    Byte = 1,
    /// Bits, `dims / 8` bytes (dims is a multiple of 8).
    Bit = 2,
}

impl VectorElement {
    /// Bytes per vector of `dims` dimensions.
    pub fn vector_bytes(self, dims: u32) -> usize {
        match self {
            VectorElement::Float32 => dims as usize * 4,
            VectorElement::Byte => dims as usize,
            VectorElement::Bit => (dims as usize).div_ceil(8),
        }
    }
}

/// Vector similarity, with Lucene's score functions (see `vectors::similarity`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VectorSimilarity {
    L2Norm = 0,
    DotProduct = 1,
    Cosine = 2,
    MaxInnerProduct = 3,
}

/// Quantization of a vector field's search copy; raw vectors are always kept for rescoring.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VectorQuantization {
    None,
    /// Scalar quantization to `bits` (8, 7 or 4) per dimension. `confidence_interval` follows
    /// Elasticsearch: `None` picks `1 - 1/(dims + 1)`, `Some(0.0)` means dynamic.
    Scalar {
        bits: u8,
        confidence_interval: Option<f32>,
    },
    /// Better binary quantization: one bit per dimension plus per-vector corrections.
    Binary,
}

/// How vectors are indexed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VectorSpec {
    pub dims: u32,
    pub element: VectorElement,
    pub similarity: VectorSimilarity,
    /// HNSW parameters; `None` for flat (brute force) fields.
    pub hnsw: Option<HnswParams>,
    pub quantization: VectorQuantization,
}

impl VectorSpec {
    pub fn vector_bytes(&self) -> usize {
        self.element.vector_bytes(self.dims)
    }
}

/// HNSW graph construction parameters (Elasticsearch defaults: m 16, ef_construction 100).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HnswParams {
    pub m: u16,
    pub ef_construction: u16,
}

impl Default for HnswParams {
    fn default() -> HnswParams {
        HnswParams {
            m: 16,
            ef_construction: 100,
        }
    }
}

/// Everything the codec knows about one field.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldInfo {
    pub number: u32,
    pub name: String,
    pub index: IndexOptions,
    pub norms: bool,
    pub payloads: bool,
    pub doc_values: DocValuesType,
    /// Write a doc values skip index (min/max per block of docs) for range filtering.
    pub doc_values_skip_index: bool,
    pub points: Option<PointsSpec>,
    pub vectors: Option<VectorSpec>,
    /// Put a Bloom filter in front of the term dictionary (sized for one term per doc), for
    /// `_id`-like fields probed by exact value across many segments.
    pub bloom_filter: bool,
    /// The field carries suggest (completion) entries ([`crate::suggest`]).
    pub suggest: bool,
    pub attributes: BTreeMap<String, String>,
}

impl FieldInfo {
    /// A field that indexes nothing yet; set the parts you need.
    pub fn new(number: u32, name: impl Into<String>) -> FieldInfo {
        FieldInfo {
            number,
            name: name.into(),
            index: IndexOptions::None,
            norms: false,
            payloads: false,
            doc_values: DocValuesType::None,
            doc_values_skip_index: false,
            points: None,
            vectors: None,
            bloom_filter: false,
            suggest: false,
            attributes: BTreeMap::new(),
        }
    }

    /// Marks the field as carrying suggest entries (see [`suggest`](Self::suggest)).
    pub fn with_suggest(mut self) -> FieldInfo {
        self.suggest = true;
        self
    }

    /// Adds a Bloom filter over the field's terms (see [`bloom_filter`](Self::bloom_filter)).
    pub fn with_bloom_filter(mut self) -> FieldInfo {
        self.bloom_filter = true;
        self
    }

    pub fn with_index(mut self, index: IndexOptions, norms: bool) -> FieldInfo {
        self.index = index;
        self.norms = norms;
        self
    }

    pub fn with_doc_values(mut self, dv: DocValuesType) -> FieldInfo {
        self.doc_values = dv;
        self
    }

    pub fn with_points(mut self, spec: PointsSpec) -> FieldInfo {
        self.points = Some(spec);
        self
    }

    pub fn with_vectors(mut self, spec: VectorSpec) -> FieldInfo {
        self.vectors = Some(spec);
        self
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.put_vu32(self.number);
        out.put_str(&self.name);
        out.put_u8(self.index as u8);
        let flags = (self.norms as u8)
            | ((self.payloads as u8) << 1)
            | ((self.doc_values_skip_index as u8) << 2)
            | ((self.points.is_some() as u8) << 3)
            | ((self.vectors.is_some() as u8) << 4)
            | ((self.bloom_filter as u8) << 5)
            | ((self.suggest as u8) << 6);
        out.put_u8(flags);
        out.put_u8(self.doc_values as u8);
        if let Some(p) = &self.points {
            out.put_u8(p.dims);
            out.put_u8(p.index_dims);
            out.put_u8(p.bytes_per_dim);
        }
        if let Some(v) = &self.vectors {
            out.put_vu32(v.dims);
            out.put_u8(v.element as u8);
            out.put_u8(v.similarity as u8);
            match v.hnsw {
                None => out.put_u8(0),
                Some(h) => {
                    out.put_u8(1);
                    out.put_u16(h.m);
                    out.put_u16(h.ef_construction);
                }
            }
            match v.quantization {
                VectorQuantization::None => out.put_u8(0),
                VectorQuantization::Scalar {
                    bits,
                    confidence_interval,
                } => {
                    out.put_u8(1);
                    out.put_u8(bits);
                    match confidence_interval {
                        None => out.put_u8(0),
                        Some(c) => {
                            out.put_u8(1);
                            out.put_f32(c);
                        }
                    }
                }
                VectorQuantization::Binary => out.put_u8(2),
            }
        }
        write_attrs(&self.attributes, out);
    }

    fn read(r: &mut In<'_>) -> Result<FieldInfo> {
        let number = r.vu32()?;
        let name = r.str()?.to_string();
        let index = IndexOptions::from_u8(r.u8()?)?;
        let flags = r.u8()?;
        let doc_values = DocValuesType::from_u8(r.u8()?)?;
        let points = if flags & 8 != 0 {
            let p = PointsSpec {
                dims: r.u8()?,
                index_dims: r.u8()?,
                bytes_per_dim: r.u8()?,
            };
            p.validate()?;
            Some(p)
        } else {
            None
        };
        let vectors = if flags & 16 != 0 {
            let dims = r.vu32()?;
            let element = match r.u8()? {
                0 => VectorElement::Float32,
                1 => VectorElement::Byte,
                2 => VectorElement::Bit,
                e => return Err(corrupt(format!("bad vector element {e}"))),
            };
            let similarity = match r.u8()? {
                0 => VectorSimilarity::L2Norm,
                1 => VectorSimilarity::DotProduct,
                2 => VectorSimilarity::Cosine,
                3 => VectorSimilarity::MaxInnerProduct,
                s => return Err(corrupt(format!("bad vector similarity {s}"))),
            };
            let hnsw = match r.u8()? {
                0 => None,
                1 => Some(HnswParams {
                    m: r.u16()?,
                    ef_construction: r.u16()?,
                }),
                h => return Err(corrupt(format!("bad vector index {h}"))),
            };
            let quantization = match r.u8()? {
                0 => VectorQuantization::None,
                1 => {
                    let bits = r.u8()?;
                    let confidence_interval = match r.u8()? {
                        0 => None,
                        _ => Some(r.f32()?),
                    };
                    VectorQuantization::Scalar {
                        bits,
                        confidence_interval,
                    }
                }
                2 => VectorQuantization::Binary,
                q => return Err(corrupt(format!("bad vector quantization {q}"))),
            };
            if dims == 0 || dims > 1 << 16 {
                return Err(corrupt(format!("bad vector dims {dims}")));
            }
            Some(VectorSpec {
                dims,
                element,
                similarity,
                hnsw,
                quantization,
            })
        } else {
            None
        };
        let attributes = read_attrs(r)?;
        Ok(FieldInfo {
            number,
            name,
            index,
            norms: flags & 1 != 0,
            payloads: flags & 2 != 0,
            doc_values,
            doc_values_skip_index: flags & 4 != 0,
            points,
            vectors,
            bloom_filter: flags & 32 != 0,
            suggest: flags & 64 != 0,
            attributes,
        })
    }
}

fn write_attrs(attrs: &BTreeMap<String, String>, out: &mut Vec<u8>) {
    out.put_vu64(attrs.len() as u64);
    for (k, v) in attrs {
        out.put_str(k);
        out.put_str(v);
    }
}

fn read_attrs(r: &mut In<'_>) -> Result<BTreeMap<String, String>> {
    let n = r.vlen(r.remaining() / 2)?;
    let mut m = BTreeMap::new();
    for _ in 0..n {
        let k = r.str()?.to_string();
        let v = r.str()?.to_string();
        m.insert(k, v);
    }
    Ok(m)
}

/// How one index-sort key reads its doc values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SortKind {
    /// Numeric (or sorted-numeric) doc values compared as signed 64-bit integers. Elasticsearch
    /// stores doubles and floats as sortable bits, so this covers every numeric type.
    Long,
    /// Sorted (or sorted-set) doc values compared by term bytes (ordinals within a segment).
    String,
}

/// Where docs without a value sort.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SortMissing {
    First,
    Last,
}

/// Which value of a multi-valued doc is its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SortSelector {
    Min,
    Max,
}

/// One key of an index sort.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SortField {
    pub field: u32,
    pub kind: SortKind,
    pub reverse: bool,
    pub missing: SortMissing,
    pub selector: SortSelector,
}

/// An index sort (`index.sort.*`): the segment's docs are ordered by these keys, ties by the
/// original order.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IndexSort {
    pub fields: Vec<SortField>,
}

/// Segment-level metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct SegmentInfo {
    pub id: [u8; 16],
    pub max_doc: u32,
    /// Fields sorted by number.
    pub fields: Vec<FieldInfo>,
    pub sort: Option<IndexSort>,
    pub attributes: BTreeMap<String, String>,
}

impl SegmentInfo {
    /// The field with this number.
    pub fn field(&self, number: u32) -> Option<&FieldInfo> {
        self.fields
            .binary_search_by_key(&number, |f| f.number)
            .ok()
            .map(|i| &self.fields[i])
    }

    pub fn field_by_name(&self, name: &str) -> Option<&FieldInfo> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        out.put_bytes(&self.id);
        out.put_vu32(self.max_doc);
        out.put_vu64(self.fields.len() as u64);
        for f in &self.fields {
            f.write(out);
        }
        match &self.sort {
            None => out.put_u8(0),
            Some(s) => {
                out.put_u8(1);
                out.put_vu64(s.fields.len() as u64);
                for k in &s.fields {
                    out.put_vu32(k.field);
                    out.put_u8(k.kind as u8);
                    out.put_u8(
                        (k.reverse as u8)
                            | ((matches!(k.missing, SortMissing::First) as u8) << 1)
                            | ((matches!(k.selector, SortSelector::Max) as u8) << 2),
                    );
                }
            }
        }
        write_attrs(&self.attributes, out);
    }

    pub(crate) fn read(r: &mut In<'_>) -> Result<SegmentInfo> {
        let id: [u8; 16] = r.array()?;
        let max_doc = r.vu32()?;
        if max_doc > crate::MAX_DOCS {
            return Err(corrupt(format!("max_doc {max_doc} too large")));
        }
        let n = r.vlen(r.remaining() / 5)?;
        let mut fields = Vec::with_capacity(n);
        for _ in 0..n {
            fields.push(FieldInfo::read(r)?);
        }
        if !fields.windows(2).all(|w| w[0].number < w[1].number) {
            return Err(corrupt("field numbers not strictly increasing"));
        }
        let sort = match r.u8()? {
            0 => None,
            _ => {
                let n = r.vlen(r.remaining() / 3)?;
                let mut keys = Vec::with_capacity(n);
                for _ in 0..n {
                    let field = r.vu32()?;
                    let kind = match r.u8()? {
                        0 => SortKind::Long,
                        1 => SortKind::String,
                        k => return Err(corrupt(format!("bad sort kind {k}"))),
                    };
                    let flags = r.u8()?;
                    keys.push(SortField {
                        field,
                        kind,
                        reverse: flags & 1 != 0,
                        missing: if flags & 2 != 0 {
                            SortMissing::First
                        } else {
                            SortMissing::Last
                        },
                        selector: if flags & 4 != 0 {
                            SortSelector::Max
                        } else {
                            SortSelector::Min
                        },
                    });
                }
                Some(IndexSort { fields: keys })
            }
        };
        let attributes = read_attrs(r)?;
        Ok(SegmentInfo {
            id,
            max_doc,
            fields,
            sort,
            attributes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_info_round_trips() {
        let mut f2 = FieldInfo::new(9, "vec")
            .with_vectors(VectorSpec {
                dims: 3,
                element: VectorElement::Float32,
                similarity: VectorSimilarity::Cosine,
                hnsw: Some(HnswParams::default()),
                quantization: VectorQuantization::Scalar {
                    bits: 8,
                    confidence_interval: Some(0.9),
                },
            })
            .with_points(PointsSpec::new(2, 4));
        f2.attributes.insert("k".into(), "v".into());
        let info = SegmentInfo {
            id: [3; 16],
            max_doc: 42,
            fields: vec![
                FieldInfo::new(1, "title")
                    .with_index(IndexOptions::DocsAndFreqsAndPositionsAndOffsets, true),
                FieldInfo::new(2, "tag").with_doc_values(DocValuesType::SortedSet),
                f2,
                FieldInfo::new(10, "bbq").with_vectors(VectorSpec {
                    dims: 384,
                    element: VectorElement::Float32,
                    similarity: VectorSimilarity::MaxInnerProduct,
                    hnsw: None,
                    quantization: VectorQuantization::Binary,
                }),
                FieldInfo::new(11, "bits").with_vectors(VectorSpec {
                    dims: 64,
                    element: VectorElement::Bit,
                    similarity: VectorSimilarity::L2Norm,
                    hnsw: Some(HnswParams { m: 512, ef_construction: 3200 }),
                    quantization: VectorQuantization::None,
                }),
                FieldInfo {
                    payloads: true,
                    doc_values_skip_index: true,
                    bloom_filter: true,
                    points: Some(PointsSpec { dims: 7, index_dims: 4, bytes_per_dim: 4 }),
                    ..FieldInfo::new(12, "shape").with_index(IndexOptions::DocsAndFreqsAndPositions, false)
                },
            ],
            sort: Some(IndexSort {
                fields: vec![SortField {
                    field: 2,
                    kind: SortKind::String,
                    reverse: true,
                    missing: SortMissing::First,
                    selector: SortSelector::Max,
                }],
            }),
            attributes: [("a".to_string(), "b".to_string())].into(),
        };
        let mut out = Vec::new();
        info.write(&mut out);
        let back = SegmentInfo::read(&mut In::new(&out)).unwrap();
        assert_eq!(back, info);
        assert_eq!(back.field(9).unwrap().name, "vec");
        assert!(back.field(3).is_none());
        for n in 0..out.len() {
            assert!(SegmentInfo::read(&mut In::new(&out[..n])).is_err());
        }
    }
}
