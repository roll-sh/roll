//! `SegmentReader`: every structure of one segment, opened over a [`Slice`].
//!
//! Opening parses the table of contents, verifies the `Meta` checksum, reads the
//! [`SegmentInfo`] and opens each field's readers (headers only: term indexes, doc sets, block tables);
//! no section is copied and postings, values and vectors are only touched when queried, so
//! opening a memory-mapped segment costs microseconds and faults in a few pages. Full checksum
//! verification ([`SegmentReader::verify`]) is a separate, explicit pass — the store runs it once
//! when a segment is downloaded or written.
//!
//! The reader is `Send + Sync` and clones by reference count; per-thread cursors
//! ([`PostingsCursor`], [`StoredReader`], doc set cursors) are created from it.

use std::sync::Arc;

use crate::container::{ContainerReader, GLOBAL, SectionKind};
use crate::docvalues::{
    BinaryDocValues, NumericDocValues, SortedDocValues, SortedNumericDocValues, SortedSetDocValues,
};
use crate::error::{Result, corrupt};
use crate::info::{DocValuesType, FieldInfo, SegmentInfo};
use crate::io::In;
use crate::norms::NormsReader;
use crate::points::PointsReader;
use crate::postings::{PostingsCursor, PostingsData, PostingsFlags, TermState};
use crate::slice::Slice;
use crate::stored::{StoredFields, StoredReader};
use crate::suggest::SuggestReader;
use crate::terms::TermsReader;
use crate::vectors::VectorsReader;

/// The doc values of a field, by type.
#[derive(Clone, Debug)]
pub enum DocValuesReader {
    Numeric(NumericDocValues),
    Binary(BinaryDocValues),
    Sorted(SortedDocValues),
    SortedSet(SortedSetDocValues),
    SortedNumeric(SortedNumericDocValues),
}

#[derive(Clone, Debug, Default)]
struct FieldReaders {
    terms: Option<TermsReader>,
    norms: Option<NormsReader>,
    doc_values: Option<DocValuesReader>,
    points: Option<PointsReader>,
    vectors: Option<VectorsReader>,
    suggest: Option<SuggestReader>,
}

struct Inner {
    container: ContainerReader,
    info: SegmentInfo,
    fields: Vec<FieldReaders>,
    stored: StoredFields,
}

/// A read-only segment. Cheap to clone; `Send + Sync`.
#[derive(Clone)]
pub struct SegmentReader {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for SegmentReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentReader")
            .field("id", &hex(&self.inner.info.id))
            .field("max_doc", &self.inner.info.max_doc)
            .field("fields", &self.inner.info.fields.len())
            .finish()
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl SegmentReader {
    /// Opens a segment file.
    pub fn open(data: Slice) -> Result<SegmentReader> {
        let container = ContainerReader::open(data)?;
        container.verify_section(SectionKind::Meta, GLOBAL)?;
        let meta = container
            .section(SectionKind::Meta, GLOBAL)
            .ok_or_else(|| corrupt("missing meta section"))?;
        let mut r = In::new(&meta);
        let info = SegmentInfo::read(&mut r)?;
        if info.id != container.segment_id() {
            return Err(corrupt("segment id mismatch between header and metadata"));
        }
        let max_doc = info.max_doc;
        let mut fields = Vec::with_capacity(info.fields.len());
        for f in &info.fields {
            fields.push(open_field(&container, f, max_doc)?);
        }
        let stored = StoredFields::open(
            container
                .section(SectionKind::Stored, GLOBAL)
                .ok_or_else(|| corrupt("missing stored fields"))?,
            container.section(SectionKind::StoredDict, GLOBAL),
        )?;
        if stored.num_docs() != max_doc {
            return Err(corrupt("stored fields doc count mismatch"));
        }
        Ok(SegmentReader {
            inner: Arc::new(Inner {
                container,
                info,
                fields,
                stored,
            }),
        })
    }

    /// Verifies every byte of the segment against its checksum.
    pub fn verify(&self) -> Result<()> {
        self.inner.container.verify()
    }

    pub fn info(&self) -> &SegmentInfo {
        &self.inner.info
    }

    pub fn id(&self) -> [u8; 16] {
        self.inner.info.id
    }

    pub fn max_doc(&self) -> u32 {
        self.inner.info.max_doc
    }

    /// The segment file's bytes (zero-copy; e.g. to upload or cache them).
    pub fn data(&self) -> &Slice {
        self.inner.container.data()
    }

    /// Size of the segment file.
    pub fn size_in_bytes(&self) -> u64 {
        self.inner.container.data().len() as u64
    }

    /// Bytes per section kind and field (for disk usage and memory accounting).
    pub fn sections(&self) -> Vec<(SectionKind, u32, u64)> {
        self.inner
            .container
            .entries()
            .iter()
            .map(|e| (e.kind, e.field, e.len))
            .collect()
    }

    /// Approximate heap memory held by this reader beyond the segment bytes themselves
    /// (metadata, per-field headers, min/max values, the stored-fields dictionary), for
    /// per-cluster memory accounting. The segment bytes (a memory map or owned buffer) are
    /// reported by [`size_in_bytes`](Self::size_in_bytes).
    pub fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        let inner = &*self.inner;
        let mut n = size_of::<Inner>()
            + inner.container.entries().len() * (size_of::<crate::container::SectionEntry>() + 24);
        for f in &inner.info.fields {
            n += size_of::<FieldInfo>() + f.name.len();
            n += f.attributes.iter().map(|(k, v)| k.len() + v.len() + 48).sum::<usize>();
        }
        n += inner.info.attributes.iter().map(|(k, v)| k.len() + v.len() + 48).sum::<usize>();
        for fr in &inner.fields {
            n += size_of::<FieldReaders>();
            if let Some(t) = &fr.terms {
                n += t.min_term().len() + t.max_term().len();
            }
            if let Some(p) = &fr.points {
                n += p.min_packed_value().len() * 2;
            }
            if let Some(DocValuesReader::SortedSet(_) | DocValuesReader::Sorted(_)) = &fr.doc_values {
                n += 64;
            }
        }
        // A prepared zstd decompression dictionary keeps a copy plus entropy tables.
        n += inner
            .container
            .entry(SectionKind::StoredDict, GLOBAL)
            .map_or(0, |e| e.len as usize + 32 * 1024);
        n
    }

    pub fn field_info(&self, field: u32) -> Option<&FieldInfo> {
        self.inner.info.field(field)
    }

    fn field(&self, number: u32) -> Option<&FieldReaders> {
        let i = self
            .inner
            .info
            .fields
            .binary_search_by_key(&number, |f| f.number)
            .ok()?;
        Some(&self.inner.fields[i])
    }

    /// The term dictionary of an inverted field.
    pub fn terms(&self, field: u32) -> Option<&TermsReader> {
        self.field(field)?.terms.as_ref()
    }

    /// Postings of a term of `field` (an exhausted cursor if the field has no terms).
    pub fn postings(&self, field: u32, term: &TermState, flags: PostingsFlags) -> PostingsCursor {
        match self.terms(field) {
            Some(t) => t.postings(term, flags),
            None => PostingsCursor::empty(),
        }
    }

    pub fn norms(&self, field: u32) -> Option<&NormsReader> {
        self.field(field)?.norms.as_ref()
    }

    pub fn doc_values(&self, field: u32) -> Option<&DocValuesReader> {
        self.field(field)?.doc_values.as_ref()
    }

    pub fn numeric(&self, field: u32) -> Option<&NumericDocValues> {
        match self.doc_values(field)? {
            DocValuesReader::Numeric(v) => Some(v),
            _ => None,
        }
    }

    pub fn binary(&self, field: u32) -> Option<&BinaryDocValues> {
        match self.doc_values(field)? {
            DocValuesReader::Binary(v) => Some(v),
            _ => None,
        }
    }

    pub fn sorted(&self, field: u32) -> Option<&SortedDocValues> {
        match self.doc_values(field)? {
            DocValuesReader::Sorted(v) => Some(v),
            _ => None,
        }
    }

    pub fn sorted_set(&self, field: u32) -> Option<&SortedSetDocValues> {
        match self.doc_values(field)? {
            DocValuesReader::SortedSet(v) => Some(v),
            _ => None,
        }
    }

    pub fn sorted_numeric(&self, field: u32) -> Option<&SortedNumericDocValues> {
        match self.doc_values(field)? {
            DocValuesReader::SortedNumeric(v) => Some(v),
            _ => None,
        }
    }

    pub fn points(&self, field: u32) -> Option<&PointsReader> {
        self.field(field)?.points.as_ref()
    }

    pub fn vectors(&self, field: u32) -> Option<&VectorsReader> {
        self.field(field)?.vectors.as_ref()
    }

    /// Suggest (completion) entries of a field.
    pub fn suggest(&self, field: u32) -> Option<&SuggestReader> {
        self.field(field)?.suggest.as_ref()
    }

    /// Stored fields metadata (cheap to clone; create readers from it).
    pub fn stored_fields(&self) -> &StoredFields {
        &self.inner.stored
    }

    /// A new stored-fields reader (one per thread; it caches the last chunk).
    pub fn stored(&self) -> StoredReader {
        self.inner.stored.reader()
    }
}

fn open_field(c: &ContainerReader, f: &FieldInfo, max_doc: u32) -> Result<FieldReaders> {
    let n = f.number;
    let mut fr = FieldReaders::default();
    if f.index.is_indexed()
        && let (Some(index), Some(meta)) = (
            c.section(SectionKind::TermsIndex, n),
            c.section(SectionKind::TermsMeta, n),
        )
    {
        let data = PostingsData {
            docs: c.section(SectionKind::Docs, n).unwrap_or_default(),
            pos: c.section(SectionKind::Positions, n).unwrap_or_default(),
            pay: c.section(SectionKind::Payloads, n).unwrap_or_default(),
            opts: f.index,
            payloads: f.payloads && f.index.has_positions(),
        };
        let mut t = TermsReader::open(index, meta, data)?;
        if let Some(b) = c.section(SectionKind::TermsBloom, n) {
            t.set_bloom(crate::bloom::BloomFilter::open(b)?);
        }
        fr.terms = Some(t);
    }
    if let Some(s) = c.section(SectionKind::Norms, n) {
        fr.norms = Some(NormsReader::open(s, max_doc)?);
    }
    if let Some(s) = c.section(SectionKind::DocValues, n) {
        fr.doc_values = Some(match f.doc_values {
            DocValuesType::Numeric => DocValuesReader::Numeric(NumericDocValues::open(s, max_doc)?),
            DocValuesType::Binary => DocValuesReader::Binary(BinaryDocValues::open(s, max_doc)?),
            DocValuesType::Sorted => DocValuesReader::Sorted(SortedDocValues::open(s, max_doc)?),
            DocValuesType::SortedSet => {
                DocValuesReader::SortedSet(SortedSetDocValues::open(s, max_doc)?)
            }
            DocValuesType::SortedNumeric => {
                DocValuesReader::SortedNumeric(SortedNumericDocValues::open(s, max_doc)?)
            }
            DocValuesType::None => {
                return Err(corrupt(format!(
                    "doc values for field [{}] without a type",
                    f.name
                )));
            }
        });
    }
    if let (Some(spec), Some(s)) = (&f.points, c.section(SectionKind::Points, n)) {
        fr.points = Some(PointsReader::open(s, spec)?);
    }
    if let (Some(spec), Some(s)) = (&f.vectors, c.section(SectionKind::VectorData, n)) {
        fr.vectors = Some(VectorsReader::open(
            spec,
            s,
            c.section(SectionKind::VectorGraph, n),
            c.section(SectionKind::VectorQuantized, n),
            max_doc,
        )?);
    }
    if f.suggest
        && let Some(s) = c.section(SectionKind::Suggest, n)
    {
        fr.suggest = Some(SuggestReader::open(s)?);
    }
    Ok(fr)
}
