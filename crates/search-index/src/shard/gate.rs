//! Segments that are still downloading: search-store's `SegmentFetch` behind [`SegmentGate`].
//!
//! A shard waking on a node whose cache lacks a large segment should not wait for the whole
//! file. The caller hands [`Shard::open`](super::Shard::open) a memory map of the partial file
//! (missing bytes read as zeros) plus a gate that says which byte ranges have arrived. Opening
//! waits only for what the codec's `SegmentReader::open` parses — header, footer, table of
//! contents, metadata, term dictionaries, norms, doc values, points, vectors, the stored-fields
//! index — and the `_id` postings (tiny: single-doc terms live in the term dictionary), so
//! writes and realtime gets never block on the download. The bulk of a segment stays lazy:
//!
//! - the doc streams (docs, freqs, skip data) of every other field, awaited per field the
//!   first time a scorer opens postings on the field in that segment
//!   ([`SegmentContext::ensure_postings`](crate::search::SegmentContext::ensure_postings));
//! - the stored-field chunks, awaited before a document is loaded
//!   ([`SegmentContext::ensure_stored`](crate::search::SegmentContext::ensure_stored));
//! - position and payload streams, awaited only by positional queries (phrases, spans,
//!   intervals: [`SegmentContext::ensure_positions`](crate::search::SegmentContext::ensure_positions)).
//!
//! Lazy ranges are prioritized at open in that order, after the eager ones: a term or boolean
//! query and its fetch phase never wait for positions, which are often most of a text field. Once the gate reports the whole file complete, every
//! check is one relaxed atomic load. Merges skip incomplete segments (a merge reads every byte);
//! `force_merge` waits for them.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use search_codec::container::{self, ContainerReader, FOOTER_LEN, GLOBAL, HEADER_LEN, SectionKind};
use search_codec::{Slice, stored};

use crate::error::Result;
use crate::schema::ID_FIELD;

/// Which byte ranges of a segment file are readable yet (search-store's `SegmentFetch`).
///
/// `wait_ready` blocks the calling thread: shard opens and searches run on blocking threads,
/// so an implementation over an async download may `block_on` its future there.
pub trait SegmentGate: Send + Sync {
    /// Whether every byte of `range` is readable now.
    fn is_ready(&self, range: Range<u64>) -> bool;
    /// Asks for these ranges ahead of the rest of the download (does not wait). Ranges asked
    /// for first arrive first.
    fn prioritize(&self, ranges: &[Range<u64>]);
    /// Blocks until every byte of `ranges` is readable, or the download failed.
    fn wait_ready(&self, ranges: &[Range<u64>]) -> std::io::Result<()>;
    /// Whether the whole file is readable (after which the gate is never consulted again).
    fn is_complete(&self) -> bool;
}

/// Byte ranges and whether they were seen ready.
#[derive(Default)]
struct Part {
    ready: AtomicBool,
    ranges: Vec<Range<u64>>,
}

impl Part {
    fn ensure(&self, src: &dyn SegmentGate) -> Result<()> {
        if !self.ready.load(Ordering::Acquire) {
            src.wait_ready(&self.ranges)?;
            self.ready.store(true, Ordering::Release);
        }
        Ok(())
    }
}

/// The lazy parts of one field's postings.
struct FieldParts {
    field: u32,
    docs: Part,
    /// Positions and payloads/offsets.
    positions: Part,
}

/// The lazy parts of one segment.
pub(crate) struct Gate {
    src: Arc<dyn SegmentGate>,
    len: u64,
    complete: AtomicBool,
    /// By field, ascending.
    fields: Vec<FieldParts>,
    stored: Part,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate").field("len", &self.len).field("complete", &self.complete.load(Ordering::Relaxed)).finish()
    }
}

impl Gate {
    /// Waits for everything a reader parses when it opens `data` and returns the gate of the
    /// rest (`None` once the file is complete).
    pub(crate) fn open(data: &Slice, src: Arc<dyn SegmentGate>) -> Result<Option<Arc<Gate>>> {
        let len = data.len() as u64;
        if src.is_complete() {
            return Ok(None);
        }
        if len < (HEADER_LEN + FOOTER_LEN) as u64 {
            // Too short to be a segment: let the reader report it once the bytes are there.
            src.wait_ready(std::slice::from_ref(&(0..len)))?;
            return Ok(None);
        }
        let footer = len - FOOTER_LEN as u64..len;
        src.wait_ready(&[0..HEADER_LEN as u64, footer.clone()])?;
        let toc = container::toc_range(len, &data[footer.start as usize..])?;
        src.wait_ready(&[toc])?;
        let c = ContainerReader::open(data.clone())?;
        let mut eager = Vec::new();
        let mut fields: Vec<FieldParts> = Vec::new();
        let mut stored = Part::default();
        for e in c.entries() {
            let r = e.offset..e.offset + e.len;
            match e.kind {
                SectionKind::Docs | SectionKind::Positions | SectionKind::Payloads if e.field != ID_FIELD => {
                    let i = match fields.iter().position(|f| f.field == e.field) {
                        Some(i) => i,
                        None => {
                            fields.push(FieldParts { field: e.field, docs: Part::default(), positions: Part::default() });
                            fields.len() - 1
                        }
                    };
                    let f = &mut fields[i];
                    if e.kind == SectionKind::Docs { &mut f.docs } else { &mut f.positions }.ranges.push(r);
                }
                SectionKind::Stored if e.field == GLOBAL && e.len >= 16 => {
                    let tail = r.end - 16..r.end;
                    src.wait_ready(std::slice::from_ref(&tail))?;
                    let open = stored::open_range(e.len, &data[tail.start as usize..tail.end as usize])?;
                    eager.push(e.offset + open.start..r.end);
                    stored.ranges.push(r.start..e.offset + open.start);
                }
                _ => eager.push(r),
            }
        }
        fields.sort_by_key(|f| f.field);
        // Eager first (the caller waits on them now), then what queries need most often: doc
        // streams, documents (every fetch phase), positions.
        src.prioritize(&eager);
        let docs: Vec<Range<u64>> = fields.iter().flat_map(|f| f.docs.ranges.iter().cloned()).collect();
        src.prioritize(&docs);
        src.prioritize(&stored.ranges);
        let positions: Vec<Range<u64>> = fields.iter().flat_map(|f| f.positions.ranges.iter().cloned()).collect();
        src.prioritize(&positions);
        src.wait_ready(&eager)?;
        Ok(Some(Arc::new(Gate { src, len, complete: AtomicBool::new(false), fields, stored })))
    }

    #[inline]
    fn done(&self) -> bool {
        if self.complete.load(Ordering::Relaxed) {
            return true;
        }
        if self.src.is_complete() {
            self.complete.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }

    fn field(&self, field: u32) -> Option<&FieldParts> {
        self.fields.binary_search_by_key(&field, |f| f.field).ok().map(|i| &self.fields[i])
    }

    /// Blocks until the doc streams (docs, freqs, impacts) of `field` are readable.
    pub(crate) fn ensure_postings(&self, field: u32) -> Result<()> {
        match self.field(field) {
            Some(f) if !self.done() => f.docs.ensure(self.src.as_ref()),
            _ => Ok(()),
        }
    }

    /// Blocks until the doc, position, offset and payload streams of `field` are readable.
    pub(crate) fn ensure_positions(&self, field: u32) -> Result<()> {
        match self.field(field) {
            Some(f) if !self.done() => {
                f.docs.ensure(self.src.as_ref())?;
                f.positions.ensure(self.src.as_ref())
            }
            _ => Ok(()),
        }
    }

    /// Blocks until stored documents are readable.
    pub(crate) fn ensure_stored(&self) -> Result<()> {
        if self.done() {
            return Ok(());
        }
        self.stored.ensure(self.src.as_ref())
    }

    /// Whether every byte is readable (non-blocking).
    pub(crate) fn is_complete(&self) -> bool {
        self.done()
    }

    /// Blocks until the whole file is readable (merges read all of it).
    pub(crate) fn wait_complete(&self) -> Result<()> {
        if !self.done() {
            self.src.wait_ready(std::slice::from_ref(&(0..self.len)))?;
            self.complete.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
}
