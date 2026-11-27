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
//!
//! A segment still downloading ([`SegmentReader::open_lazy`], with a [`SectionWait`]) opens its
//! fields' doc values and points readers on first use instead, once their section is in: a waking
//! shard then waits for metadata and term dictionaries, not for every field's values (on a
//! numeric index, most of the file). A section that was already in when the segment opened is
//! opened at once, as before.

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

/// A segment file still downloading ([`SegmentReader::open_lazy`]): which byte ranges are in, and
/// a wait for the others. `wait` blocks the calling thread (a shard's readers run on blocking
/// threads).
pub trait SectionWait: Send + Sync {
    fn is_ready(&self, range: std::ops::Range<u64>) -> bool;
    /// Blocks until every byte of `range` is readable, or the download failed.
    fn wait(&self, range: std::ops::Range<u64>) -> std::io::Result<()>;
}

/// A field's reader of one section: open, or opened on first use once the section is in.
#[derive(Debug, Default)]
enum Lazy<R> {
    Now(Option<R>),
    Later {
        range: std::ops::Range<u64>,
        slot: std::sync::OnceLock<std::result::Result<R, String>>,
    },
    #[default]
    None,
}

impl<R: Clone> Clone for Lazy<R> {
    fn clone(&self) -> Self {
        match self {
            Lazy::Now(r) => Lazy::Now(r.clone()),
            Lazy::None => Lazy::None,
            Lazy::Later { range, slot } => {
                let copy = std::sync::OnceLock::new();
                if let Some(v) = slot.get() {
                    let _ = copy.set(v.clone());
                }
                Lazy::Later { range: range.clone(), slot: copy }
            }
        }
    }
}

impl<R> Lazy<R> {
    /// The reader if it is open (never waits).
    fn now(&self) -> Option<&R> {
        match self {
            Lazy::Now(r) => r.as_ref(),
            Lazy::Later { slot, .. } => slot.get().and_then(|r| r.as_ref().ok()),
            Lazy::None => None,
        }
    }
}

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
    doc_values: Lazy<DocValuesReader>,
    points: Lazy<PointsReader>,
    vectors: Option<VectorsReader>,
    suggest: Option<SuggestReader>,
    /// Sorted doc values rebuilt from the terms on first use ([`UNINVERTED`]); boxed so a
    /// field that never needs them costs one pointer.
    uninverted: std::sync::OnceLock<Option<Box<Uninverted>>>,
}

/// Built [`UNINVERTED`] doc values and the bytes they hold (fielddata memory stats).
#[derive(Clone, Debug)]
struct Uninverted {
    reader: DocValuesReader,
    bytes: usize,
}

/// Set on a field number passed to [`SegmentReader::doc_values`] to read the field's
/// *uninverted* terms instead: sorted doc values rebuilt from the terms index the first time
/// they are asked for and kept for the reader's life (Lucene's `PagedBytesIndexFieldData`,
/// the fielddata of `_id`). Each document gets the smallest term whose postings hold it, so
/// the field should be single-valued (an id). Fields without terms have none.
pub const UNINVERTED: u32 = 1 << 31;

struct Inner {
    container: ContainerReader,
    /// A segment still downloading: what lazy readers wait on ([`SegmentReader::open_lazy`]).
    wait: Option<Arc<dyn SectionWait>>,
    info: SegmentInfo,
    fields: Vec<FieldReaders>,
    stored: StoredFields,
    /// Field number -> index into `fields` (`u32::MAX`: absent), when numbers are dense
    /// enough; otherwise empty and lookups binary-search `info.fields`. Per-document readers
    /// (scripts, value sources) look fields up on hot paths, where the search's dependent
    /// loads over 88-byte infos showed up in profiles.
    slots: Box<[u32]>,
}

/// Field numbers up to this bound (and at most a few times the field count) get a direct slot
/// table.
const MAX_SLOT_TABLE: u32 = 1 << 16;

fn slot_table(info: &SegmentInfo) -> Box<[u32]> {
    let Some(max) = info.fields.iter().map(|f| f.number).max() else { return Box::default() };
    if max >= MAX_SLOT_TABLE || max as usize > 8 * info.fields.len() + 1024 {
        return Box::default();
    }
    let mut t = vec![u32::MAX; max as usize + 1];
    for (i, f) in info.fields.iter().enumerate() {
        t[f.number as usize] = i as u32;
    }
    t.into_boxed_slice()
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
        Self::open_lazy(data, None)
    }

    /// Opens a segment file that may still be downloading: `data` maps the whole file (bytes not
    /// in yet read as zeros) and `wait` says which ranges are in. Everything this parses must be in
    /// already (header, table of contents, metadata, term dictionaries, stored-fields index); a
    /// field's doc values or points section that isn't opens on first use
    /// ([`SegmentReader::doc_values`], [`SegmentReader::points`] block until it is in). `None`: a
    /// complete file, everything opened now.
    pub fn open_lazy(data: Slice, wait: Option<Arc<dyn SectionWait>>) -> Result<SegmentReader> {
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
            fields.push(open_field(&container, f, max_doc, wait.as_deref())?);
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
                wait,
                slots: slot_table(&info),
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
            if let Some(p) = fr.points.now() {
                n += p.min_packed_value().len() * 2;
            }
            if let Some(DocValuesReader::SortedSet(_) | DocValuesReader::Sorted(_)) = fr.doc_values.now() {
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

    #[inline]
    /// A field's reader of a section, opening it first when it was left for its first use: blocks
    /// until the section is in. A section that never arrives (the download failed) or doesn't
    /// open is a panic naming the segment, never a field without values: a search's pool
    /// reports it as the search's failure.
    fn opened<'a, R>(&'a self, l: &'a Lazy<R>, field: u32, kind: SectionKind, open: impl FnOnce(Slice, &FieldInfo, u32) -> Result<R>) -> Option<&'a R> {
        match l {
            Lazy::Now(r) => r.as_ref(),
            Lazy::None => None,
            Lazy::Later { range, slot } => {
                let r = slot.get_or_init(|| {
                    let inner = &*self.inner;
                    if let Some(w) = &inner.wait {
                        w.wait(range.clone()).map_err(|e| e.to_string())?;
                    }
                    let s = inner.container.section(kind, field).ok_or_else(|| "missing section".to_string())?;
                    let fi = inner.info.field(field).ok_or_else(|| "missing field".to_string())?;
                    open(s, fi, inner.info.max_doc).map_err(|e| e.to_string())
                });
                match r {
                    Ok(r) => Some(r),
                    Err(e) => panic!("segment {}: field {field}'s {kind:?} section isn't readable: {e}", hex(&self.inner.info.id)),
                }
            }
        }
    }

    fn field(&self, number: u32) -> Option<&FieldReaders> {
        let inner = &*self.inner;
        if !inner.slots.is_empty() {
            let i = *inner.slots.get(number as usize)?;
            return inner.fields.get(i as usize);
        }
        let i = inner.info.fields.binary_search_by_key(&number, |f| f.number).ok()?;
        Some(&inner.fields[i])
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
        match self.field(field) {
            Some(f) => self.opened(&f.doc_values, field, SectionKind::DocValues, |s, fi, max_doc| open_doc_values(s, fi, max_doc)),
            // Numbers this large are never real fields (`u32::MAX` sentinels included).
            None if field & UNINVERTED != 0 => self.uninverted(field & !UNINVERTED),
            None => None,
        }
    }

    /// The [`UNINVERTED`] doc values of `field`.
    #[cold]
    fn uninverted(&self, field: u32) -> Option<&DocValuesReader> {
        let fr = self.field(field)?;
        let terms = fr.terms.as_ref()?;
        fr.uninverted.get_or_init(|| uninvert(terms, self.max_doc()).map(Box::new)).as_deref().map(|u| &u.reader)
    }

    /// The memory the [`UNINVERTED`] doc values of `field` hold once built (0 before).
    pub fn uninverted_bytes(&self, field: u32) -> usize {
        self.field(field).and_then(|f| f.uninverted.get()).and_then(|u| u.as_deref()).map_or(0, |u| u.bytes)
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
        self.opened(&self.field(field)?.points, field, SectionKind::Points, |s, fi, _| {
            PointsReader::open(s, fi.points.as_ref().ok_or_else(|| corrupt(format!("points for field [{}] without a spec", fi.name)))?)
        })
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

/// Sorted doc values holding, for each document with a term of `terms`, its smallest term.
fn uninvert(terms: &TermsReader, max_doc: u32) -> Option<Uninverted> {
    // u32 ordinals (half the transient memory of u64 ones).
    const NONE: u32 = u32::MAX;
    let mut ord_of = vec![NONE; max_doc as usize];
    let mut dict = crate::docvalues::ByteList::new();
    let mut it = terms.iter();
    let mut cursor = PostingsCursor::empty();
    let mut ord = 0u32;
    while let Some(t) = it.next() {
        dict.push(t);
        it.postings_reset(PostingsFlags::DOCS, &mut cursor);
        loop {
            let d = cursor.next();
            if d == crate::NO_MORE_DOCS {
                break;
            }
            let slot = ord_of.get_mut(d as usize)?;
            if *slot == NONE {
                *slot = ord;
            }
        }
        // `NONE` stays free: more terms than that can't be ordinals here.
        ord = ord.checked_add(1).filter(|&o| o != NONE)?;
    }
    let mut docs = Vec::new();
    let mut ords = Vec::new();
    for (d, &o) in ord_of.iter().enumerate() {
        if o != NONE {
            docs.push(d as u32);
            ords.push(o);
        }
    }
    drop(ord_of);
    let mut out = Vec::new();
    crate::docvalues::write_sorted_dv_with(&docs, |i| u64::from(ords[i]), &mut dict, max_doc, &mut out).ok()?;
    let bytes = out.len();
    SortedDocValues::open(Slice::from(out), max_doc).ok().map(|dv| Uninverted { reader: DocValuesReader::Sorted(dv), bytes })
}

/// Opens a field's doc values reader over its section.
fn open_doc_values(s: Slice, f: &FieldInfo, max_doc: u32) -> Result<DocValuesReader> {
    Ok(match f.doc_values {
        DocValuesType::Numeric => DocValuesReader::Numeric(NumericDocValues::open(s, max_doc)?),
        DocValuesType::Binary => DocValuesReader::Binary(BinaryDocValues::open(s, max_doc)?),
        DocValuesType::Sorted => DocValuesReader::Sorted(SortedDocValues::open(s, max_doc)?),
        DocValuesType::SortedSet => DocValuesReader::SortedSet(SortedSetDocValues::open(s, max_doc)?),
        DocValuesType::SortedNumeric => DocValuesReader::SortedNumeric(SortedNumericDocValues::open(s, max_doc)?),
        DocValuesType::None => return Err(corrupt(format!("doc values for field [{}] without a type", f.name))),
    })
}

/// A section's reader now, or left for its first use when `wait` says its bytes aren't in yet.
fn lazy<R>(c: &ContainerReader, kind: SectionKind, field: u32, wait: Option<&dyn SectionWait>, open: impl FnOnce(Slice) -> Result<R>) -> Result<Lazy<R>> {
    let Some(e) = c.entry(kind, field) else { return Ok(Lazy::None) };
    let range = e.offset..e.offset + e.len;
    match wait {
        Some(w) if !w.is_ready(range.clone()) => Ok(Lazy::Later { range, slot: std::sync::OnceLock::new() }),
        _ => Ok(Lazy::Now(Some(open(c.section(kind, field).expect("an entry has a section"))?))),
    }
}

fn open_field(c: &ContainerReader, f: &FieldInfo, max_doc: u32, wait: Option<&dyn SectionWait>) -> Result<FieldReaders> {
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
    fr.doc_values = lazy(c, SectionKind::DocValues, n, wait, |s| open_doc_values(s, f, max_doc))?;
    if let Some(spec) = &f.points {
        fr.points = lazy(c, SectionKind::Points, n, wait, |s| PointsReader::open(s, spec))?;
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
