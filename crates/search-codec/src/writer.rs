//! `write_segment`: pulls a [`SegmentSource`] into one segment file.
//!
//! Sections are streamed into the container field by field (norms first, because postings
//! impacts need each doc's norm; then terms and postings, doc values, points, vectors), then
//! stored fields, then the `Meta` section with the [`SegmentInfo`].
//!
//! Memory stays bounded for the sections that grow with the segment: a field's doc stream goes
//! straight into the container while its terms are written, the positions, payloads, term index
//! and term metadata produced alongside it are `Spill` buffers (memory up to
//! [`SpillOptions::memory_bytes`] each, then a temporary file), and stored-field chunks are
//! streamed as they fill. Doc values and points collect their per-value arrays (docs, values,
//! ordinals, addresses, sort permutations) in `SpillVec`s, which continue in a temporary file
//! mapped back for random access, and their encoders write append-only into the container, so a
//! field of tens of millions of values costs page cache, not heap. Sorted dictionaries are read
//! from the source twice (sizes, then bytes) instead of being copied. A field's raw vectors past
//! the budget also go to a temporary file mapped back for the graph build. What stays in heap:
//! one norm byte per doc for the field being written, the quantized vector codes and the HNSW
//! graph of one field (Lucene builds graphs on heap too).
//!
//! When the source declares an index sort it does not already satisfy, the writer computes the
//! sort permutation and writes a [`RemappedSource`] instead.
//!
//! Every invariant the format relies on is validated here (sorted terms and docs, ordinal
//! ranges, value sizes), so readers can trust structure that passed the checksum.

use std::io::Write;

use crate::bloom::BloomBuilder;
use crate::container::{ContainerWriter, GLOBAL, SectionKind};
use crate::docvalues::{
    TermSeq, write_binary_dv_with, write_numeric_dv_with, write_sorted_dv_with, write_sorted_numeric_dv,
    write_sorted_set_dv_with,
};
use crate::io::SinkOut;
use crate::spill::{Pod, Spill, SpillOptions, SpillSlice, SpillVec};
use crate::error::{Result, invalid};
use crate::info::{DocValuesType, FieldInfo, SegmentInfo};
use crate::norms::write_norms;
use crate::points::write_points;
use crate::postings::PostingsWriter;
use crate::sort::sort_doc_map;
use crate::source::{DocValuesSource, RemappedSource, SegmentSource, SortedSetSource, SortedSource};
use crate::stored::{StoredOptions, StoredWriter, sample_docs, train_dictionary};
use crate::terms::TermsWriter;
use crate::vectors::{GraphSeed, write_vectors};
use crate::{MAX_DOCS, NO_MORE_DOCS};

/// Options for [`write_segment_with`].
#[derive(Clone, Debug)]
pub struct WriteOptions {
    pub stored: StoredOptions,
    /// Threads that build HNSW graphs (the dominant cost of writing vector fields). With 1 the
    /// graph is deterministic; more threads give an equally good, but interleaving-dependent,
    /// graph.
    pub vector_threads: usize,
    /// Where large section buffers spill (positions, payloads, term dictionaries).
    pub spill: SpillOptions,
}

impl Default for WriteOptions {
    fn default() -> WriteOptions {
        WriteOptions {
            stored: StoredOptions::default(),
            vector_threads: std::thread::available_parallelism().map_or(1, |n| n.get()).min(8),
            spill: SpillOptions::default(),
        }
    }
}

/// Writes `src` as one segment file to `out` and returns its metadata.
pub fn write_segment(src: &dyn SegmentSource, out: &mut dyn Write) -> Result<SegmentInfo> {
    write_segment_with(src, out, &WriteOptions::default())
}

/// [`write_segment`] with options.
pub fn write_segment_with(
    src: &dyn SegmentSource,
    out: &mut dyn Write,
    opts: &WriteOptions,
) -> Result<SegmentInfo> {
    if let Some(sort) = src.index_sort()
        && !src.is_sorted()
    {
        let map = sort_doc_map(src, &sort)?;
        let mut sorted = RemappedSource::new(src, map, Some(sort))?;
        sorted.set_spill(opts.spill.clone());
        return write_inner(&sorted, out, opts);
    }
    write_inner(src, out, opts)
}

/// Postings bytes buffered before they move to the container or a spill buffer.
const STREAM_FLUSH_BYTES: usize = 1 << 20;

fn check_doc(
    field: &FieldInfo,
    what: &str,
    doc: u32,
    last: &mut Option<u32>,
    max_doc: u32,
) -> Result<()> {
    if doc >= max_doc {
        return Err(invalid(format!(
            "{what} of field [{}]: doc {doc} >= max_doc {max_doc}",
            field.name
        )));
    }
    if last.is_some_and(|l| doc <= l) {
        return Err(invalid(format!(
            "{what} of field [{}]: docs out of order at {doc}",
            field.name
        )));
    }
    *last = Some(doc);
    Ok(())
}

fn check_bound(field: &FieldInfo, what: &str, doc: u32, max_doc: u32) -> Result<()> {
    if doc >= max_doc {
        return Err(invalid(format!("{what} of field [{}]: doc {doc} >= max_doc {max_doc}", field.name)));
    }
    Ok(())
}

/// Brings per-doc values that arrived in any order (a reordered merge streams them in old-doc
/// order) into doc order: returns the sorted docs and the permutation (`perm[i]` is the arrival
/// index of the `i`-th doc), or no permutation when they already were in order. A doc given
/// twice is an error. Sorting happens on spill-backed arrays (8 bytes per value, page cache
/// past the budget).
fn doc_order(
    field: &FieldInfo,
    what: &str,
    docs: SpillVec<u32>,
    spill: &SpillOptions,
) -> Result<(SpillSlice<u32>, Option<SpillSlice<u32>>)> {
    let docs = docs.finish()?;
    if docs.windows(2).all(|w| w[0] < w[1]) {
        return Ok((docs, None));
    }
    let mut pairs = SpillVec::<u64>::new(spill);
    for (i, &d) in docs.iter().enumerate() {
        pairs.push(((d as u64) << 32) | i as u64)?;
    }
    drop(docs);
    let mut pairs = pairs.finish()?;
    pairs.sort_unstable();
    let mut sorted = SpillVec::<u32>::new(spill);
    let mut perm = SpillVec::<u32>::new(spill);
    let mut prev = None;
    for &p in pairs.iter() {
        let d = (p >> 32) as u32;
        if prev == Some(d) {
            return Err(invalid(format!("{what} of field [{}]: doc {d} given twice", field.name)));
        }
        prev = Some(d);
        sorted.push(d)?;
        perm.push(p as u32)?;
    }
    Ok((sorted.finish()?, Some(perm.finish()?)))
}

/// Writes one section through an append-only [`SinkOut`] straight into the container.
fn stream_section(
    w: &mut ContainerWriter<'_>,
    kind: SectionKind,
    field: u32,
    body: impl FnOnce(&mut SinkOut<'_>) -> Result<()>,
) -> Result<()> {
    w.begin(kind, field)?;
    let mut o = SinkOut::new(w);
    let r = body(&mut o);
    let done = o.finish();
    r?;
    done?;
    w.end()
}

/// Applies `perm` (new position → old position) to fixed-size records in place, following
/// cycles, so reordering a field's vectors needs one spare record instead of a second copy.
fn permute_records(data: &mut [u8], size: usize, perm: &[u32]) {
    if size == 0 {
        return;
    }
    let mut done = vec![0u64; perm.len().div_ceil(64)];
    let mut tmp = vec![0u8; size];
    for start in 0..perm.len() {
        if done[start >> 6] >> (start & 63) & 1 != 0 || perm[start] as usize == start {
            continue;
        }
        tmp.copy_from_slice(&data[start * size..(start + 1) * size]);
        let mut j = start;
        loop {
            done[j >> 6] |= 1 << (j & 63);
            let k = perm[j] as usize;
            if k == start {
                data[j * size..(j + 1) * size].copy_from_slice(&tmp);
                break;
            }
            data.copy_within(k * size..(k + 1) * size, j * size);
            j = k;
        }
    }
}

fn write_inner(
    src: &dyn SegmentSource,
    out: &mut dyn Write,
    opts: &WriteOptions,
) -> Result<SegmentInfo> {
    let max_doc = src.max_doc();
    if max_doc > MAX_DOCS {
        return Err(invalid(format!("segment has too many docs ({max_doc})")));
    }
    let mut fields = src.field_infos();
    fields.sort_by_key(|f| f.number);
    if fields.windows(2).any(|w| w[0].number == w[1].number) {
        return Err(invalid("duplicate field numbers"));
    }
    for f in &fields {
        if let Some(p) = &f.points {
            p.validate()
                .map_err(|_| invalid(format!("invalid points spec for field [{}]", f.name)))?;
        }
        if let Some(v) = &f.vectors
            && (v.dims == 0
                || v.dims > 1 << 16
                || (v.element == crate::info::VectorElement::Bit && v.dims % 8 != 0))
        {
            return Err(invalid(format!(
                "invalid vector dims {} for field [{}]",
                v.dims, f.name
            )));
        }
    }
    let id = src.segment_id();
    let mut w = ContainerWriter::new(out, id)?;
    let mut buf = Vec::new();

    for f in &fields {
        // Norms (needed by postings impacts).
        let mut dense_norms: Option<Vec<u8>> = None;
        if f.norms
            && f.index.is_indexed()
            && let Some(it) = src.norms(f.number)?
        {
            // One byte per doc (the postings writer needs every doc's norm for impacts) plus a
            // bitset of the docs that have one.
            let mut has = vec![0u64; (max_doc as usize).div_ceil(64)];
            let mut dense = vec![0u8; max_doc as usize];
            for (d, n) in it {
                check_bound(f, "norms", d, max_doc)?;
                let word = &mut has[d as usize >> 6];
                if *word >> (d & 63) & 1 != 0 {
                    return Err(invalid(format!("norms of field [{}]: doc {d} given twice", f.name)));
                }
                *word |= 1 << (d & 63);
                dense[d as usize] = n;
            }
            stream_section(&mut w, SectionKind::Norms, f.number, |o| {
                write_norms(&has, &dense, max_doc, o);
                Ok(())
            })?;
            dense_norms = Some(dense);
        }

        // Terms and postings.
        if f.index.is_indexed()
            && let Some(mut terms) = src.terms(f.number)?
        {
            let has_freqs = f.index.has_freqs();
            let has_pos = f.index.has_positions();
            let has_offsets = f.index.has_offsets();
            let payloads = f.payloads && has_pos;
            let mut pw = PostingsWriter::new(f.index, payloads, dense_norms.as_deref());
            let mut tw = TermsWriter::with_spill(f.index, payloads, &opts.spill)?;
            let mut bloom = (f.bloom_filter).then(|| BloomBuilder::new(max_doc as u64));
            let mut pos_spill = Spill::new(&opts.spill);
            let mut pay_spill = Spill::new(&opts.spill);
            // The doc stream goes straight into its section; the streams written alongside it
            // collect in spill buffers.
            w.begin(SectionKind::Docs, f.number)?;
            let mut seen = vec![0u64; (max_doc as usize).div_ceil(64)];
            let mut doc_count = 0u32;
            let mut term = Vec::new();
            while let Some(t) = terms.next_term()? {
                term.clear();
                term.extend_from_slice(t);
                let p = terms.postings()?;
                pw.start_term();
                let mut last = None;
                loop {
                    let d = p.next_doc();
                    if d == NO_MORE_DOCS {
                        break;
                    }
                    check_doc(f, "postings", d, &mut last, max_doc)?;
                    let freq = if has_freqs { p.freq() } else { 1 };
                    if freq == 0 {
                        return Err(invalid(format!(
                            "postings of field [{}]: zero frequency",
                            f.name
                        )));
                    }
                    pw.start_doc(d, freq);
                    if has_pos {
                        for _ in 0..freq {
                            let pos = p.next_position();
                            let (s, e) = if has_offsets { p.offsets() } else { (0, 0) };
                            let pl = if payloads { p.payload() } else { &[] };
                            pw.add_position(pos, s, e, pl);
                        }
                    }
                    pw.finish_doc();
                    let word = &mut seen[(d >> 6) as usize];
                    if *word & (1 << (d & 63)) == 0 {
                        *word |= 1 << (d & 63);
                        doc_count += 1;
                    }
                }
                let st = pw.finish_term();
                if st.doc_freq > 0 {
                    tw.add(&term, &st)?;
                    if let Some(b) = &mut bloom {
                        b.add(&term);
                    }
                }
                pw.take_flushed(STREAM_FLUSH_BYTES, &mut |stream, bytes| match stream {
                    0 => w.write(bytes),
                    1 => pos_spill.append(bytes),
                    _ => pay_spill.append(bytes),
                })?;
            }
            pw.take_flushed(0, &mut |stream, bytes| match stream {
                0 => w.write(bytes),
                1 => pos_spill.append(bytes),
                _ => pay_spill.append(bytes),
            })?;
            w.end()?;
            drop(pw);
            tw.finish_into(doc_count, &mut w, f.number)?;
            if has_pos {
                w.begin(SectionKind::Positions, f.number)?;
                pos_spill.copy_to(&mut w)?;
                w.end()?;
            }
            if has_offsets || payloads {
                w.begin(SectionKind::Payloads, f.number)?;
                pay_spill.copy_to(&mut w)?;
                w.end()?;
            }
            if let Some(bytes) = bloom.and_then(BloomBuilder::finish) {
                w.section(SectionKind::TermsBloom, f.number, &bytes)?;
            }
        }

        // Doc values.
        if f.doc_values != DocValuesType::None
            && let Some(dv) = src.doc_values(f.number)?
        {
            write_doc_values(f, dv, max_doc, &opts.spill, &mut w)?;
        }

        // Points.
        if let Some(spec) = &f.points
            && let Some(mut p) = src.points(f.number)?
        {
            let plen = spec.packed_len();
            let mut docs = SpillVec::<u32>::new(&opts.spill);
            let mut values = SpillVec::<u8>::new(&opts.spill);
            while let Some((d, v)) = p.next() {
                if d >= max_doc || v.len() != plen {
                    return Err(invalid(format!(
                        "points of field [{}]: bad doc {d} or value length {} (expected {plen})",
                        f.name,
                        v.len()
                    )));
                }
                docs.push(d)?;
                values.extend_from_slice(v)?;
            }
            drop(p);
            let (docs, values) = (docs.finish()?, values.finish()?);
            stream_section(&mut w, SectionKind::Points, f.number, |o| {
                write_points(spec, &docs, &values, &opts.spill, o)
            })?;
        }

        // Vectors.
        if let Some(spec) = &f.vectors
            && let Some(mut v) = src.vectors(f.number)?
        {
            let vb = spec.vector_bytes();
            let mut docs = SpillVec::<u32>::new(&opts.spill);
            // Raw vectors past the spill budget go to a temporary file that is mapped back for
            // the graph build: page cache rather than heap.
            let mut collected = Spill::new(&opts.spill);
            while let Some((d, x)) = v.next() {
                check_bound(f, "vectors", d, max_doc)?;
                if x.len() != vb {
                    return Err(invalid(format!(
                        "vectors of field [{}]: {} bytes, expected {vb}",
                        f.name,
                        x.len()
                    )));
                }
                docs.push(d)?;
                collected.append(x)?;
            }
            drop(v);
            let mut data = collected.into_buffer()?;
            let (docs, perm) = doc_order(f, "vectors", docs, &opts.spill)?;
            if let Some(perm) = perm {
                permute_records(&mut data, vb, &perm);
            }
            // Merges may hand over an existing graph: map its vectors' new docs to ordinals.
            let seed_src = if spec.hnsw.is_some() { src.vector_graph_seed(f.number) } else { None };
            let seed = seed_src.as_ref().and_then(|(r, new_docs)| {
                let ords = new_docs
                    .iter()
                    .map(|d| docs.binary_search(d).ok().map(|o| o as u32))
                    .collect::<Option<Vec<u32>>>()?;
                (ords.len() as u32 == r.len()).then_some(GraphSeed { reader: r, ords })
            });
            let (d, g, q) = write_vectors(spec, &docs, &data, max_doc, seed, opts.vector_threads)?;
            w.begin(SectionKind::VectorData, f.number)?;
            w.write(&d.head)?;
            w.write(&data)?;
            w.write(&d.tail)?;
            w.end()?;
            if let Some(g) = g {
                w.section(SectionKind::VectorGraph, f.number, &g)?;
            }
            if let Some(q) = q {
                w.section(SectionKind::VectorQuantized, f.number, &q)?;
            }
        }
    }

    // Suggest entries.
    for f in fields.iter().filter(|f| f.suggest) {
        if let Some(s) = src.suggest(f.number)? {
            write_suggest_field(f, s, max_doc, &opts.spill, &mut w)?;
        }
    }

    // Stored fields, with a dictionary trained on a sample of documents.
    let mut doc = Vec::new();
    let dict = if opts.stored.dictionary {
        let mut samples = Vec::new();
        let mut total = 0usize;
        for d in sample_docs(max_doc) {
            doc.clear();
            src.stored(d, &mut doc)?;
            total += doc.len();
            samples.push(doc.clone());
            if total > crate::stored::max_sample_bytes() {
                break;
            }
        }
        train_dictionary(&samples)
    } else {
        None
    };
    let enc = dict
        .as_ref()
        .map(|d| zstd::dict::EncoderDictionary::copy(d, opts.stored.level));
    if let Some(d) = &dict {
        w.section(SectionKind::StoredDict, GLOBAL, d)?;
    }
    let mut sw = StoredWriter::new(opts.stored.clone(), enc.as_ref())?;
    w.begin(SectionKind::Stored, GLOBAL)?;
    for d in 0..max_doc {
        doc.clear();
        src.stored(d, &mut doc)?;
        sw.add(&doc, &mut w)?;
    }
    sw.finish(&mut w)?;
    w.end()?;

    let info = SegmentInfo {
        id,
        max_doc,
        fields,
        sort: src.index_sort(),
        attributes: src.attributes(),
    };
    buf.clear();
    info.write(&mut buf);
    w.section(SectionKind::Meta, GLOBAL, &buf)?;
    w.finish()?;
    Ok(info)
}

fn write_doc_values(
    f: &FieldInfo,
    dv: DocValuesSource<'_>,
    max_doc: u32,
    spill: &SpillOptions,
    w: &mut ContainerWriter<'_>,
) -> Result<()> {
    let mismatch = || {
        invalid(format!(
            "doc values of field [{}] do not match its type {:?}",
            f.name, f.doc_values
        ))
    };
    let unsorted = |ord: u64| {
        invalid(format!(
            "doc values of field [{}]: dictionary not strictly increasing at ordinal {ord}",
            f.name
        ))
    };
    let kind = SectionKind::DocValues;
    match dv {
        DocValuesSource::Numeric(it) => {
            if f.doc_values != DocValuesType::Numeric {
                return Err(mismatch());
            }
            let mut docs = SpillVec::<u32>::new(spill);
            let mut values = SpillVec::<i64>::new(spill);
            for (d, v) in it {
                check_bound(f, "doc values", d, max_doc)?;
                docs.push(d)?;
                values.push(v)?;
            }
            let (docs, perm) = doc_order(f, "doc values", docs, spill)?;
            let values = values.finish()?;
            stream_section(w, kind, f.number, |o| {
                match &perm {
                    None => write_numeric_dv_with(&docs, |i| values[i], max_doc, o),
                    Some(p) => write_numeric_dv_with(&docs, |i| values[p[i] as usize], max_doc, o),
                }
                Ok(())
            })
        }
        DocValuesSource::Binary(mut b) => {
            if f.doc_values != DocValuesType::Binary {
                return Err(mismatch());
            }
            let mut docs = SpillVec::<u32>::new(spill);
            let mut data = SpillVec::<u8>::new(spill);
            let mut ends = SpillVec::<u64>::new(spill);
            while let Some((d, v)) = b.next() {
                check_bound(f, "doc values", d, max_doc)?;
                docs.push(d)?;
                data.extend_from_slice(v)?;
                ends.push(data.len() as u64)?;
            }
            drop(b);
            let (docs, perm) = doc_order(f, "doc values", docs, spill)?;
            let (data, ends) = (data.finish()?, ends.finish()?);
            let get = |k: usize| {
                let s = if k == 0 { 0 } else { ends[k - 1] as usize };
                &data[s..ends[k] as usize]
            };
            stream_section(w, kind, f.number, |o| {
                match &perm {
                    None => write_binary_dv_with(&docs, get, max_doc, o),
                    Some(p) => write_binary_dv_with(&docs, |i| get(p[i] as usize), max_doc, o),
                }
                Ok(())
            })
        }
        DocValuesSource::Sorted(mut s) => {
            if f.doc_values != DocValuesType::Sorted {
                return Err(mismatch());
            }
            let count = s.value_count();
            let mut docs = SpillVec::<u32>::new(spill);
            let mut ords = SpillVec::<u64>::new(spill);
            while let Some((d, o)) = s.next_doc() {
                check_bound(f, "doc values", d, max_doc)?;
                if o >= count {
                    return Err(invalid(format!(
                        "doc values of field [{}]: ordinal {o} out of range",
                        f.name
                    )));
                }
                docs.push(d)?;
                ords.push(o)?;
            }
            let (docs, perm) = doc_order(f, "doc values", docs, spill)?;
            let ords = ords.finish()?;
            let mut dict = SortedDict(&mut *s);
            stream_section(w, kind, f.number, |o| {
                match &perm {
                    None => write_sorted_dv_with(&docs, |i| ords[i], &mut dict, max_doc, o),
                    Some(p) => write_sorted_dv_with(&docs, |i| ords[p[i] as usize], &mut dict, max_doc, o),
                }
                .map_err(unsorted)
            })
        }
        DocValuesSource::SortedSet(mut s) => {
            if f.doc_values != DocValuesType::SortedSet {
                return Err(mismatch());
            }
            let count = s.value_count();
            let mut docs = SpillVec::<u32>::new(spill);
            let mut addresses = SpillVec::<u64>::new(spill);
            let mut ords = SpillVec::<u64>::new(spill);
            addresses.push(0)?;
            let mut tmp = Vec::new();
            while let Some((d, os)) = s.next_doc() {
                if os.is_empty() {
                    continue;
                }
                check_bound(f, "doc values", d, max_doc)?;
                tmp.clear();
                tmp.extend_from_slice(os);
                tmp.sort_unstable();
                tmp.dedup();
                if tmp.last().is_some_and(|&o| o >= count) {
                    return Err(invalid(format!(
                        "doc values of field [{}]: ordinal out of range",
                        f.name
                    )));
                }
                docs.push(d)?;
                ords.extend_from_slice(&tmp)?;
                addresses.push(ords.len() as u64)?;
            }
            let (docs, perm) = doc_order(f, "doc values", docs, spill)?;
            let (mut addresses, mut ords) = (addresses.finish()?, ords.finish()?);
            if let Some(p) = &perm {
                (addresses, ords) = permute_lists(&addresses, &ords, p, spill)?;
            }
            let mut dict = SortedSetDict(&mut *s);
            stream_section(w, kind, f.number, |o| {
                write_sorted_set_dv_with(&docs, &addresses, &ords, &mut dict, max_doc, o).map_err(unsorted)
            })
        }
        DocValuesSource::SortedNumeric(mut s) => {
            if f.doc_values != DocValuesType::SortedNumeric {
                return Err(mismatch());
            }
            let mut docs = SpillVec::<u32>::new(spill);
            let mut addresses = SpillVec::<u64>::new(spill);
            let mut values = SpillVec::<i64>::new(spill);
            addresses.push(0)?;
            let mut tmp = Vec::new();
            while let Some((d, vs)) = s.next_doc() {
                if vs.is_empty() {
                    continue;
                }
                check_bound(f, "doc values", d, max_doc)?;
                tmp.clear();
                tmp.extend_from_slice(vs);
                tmp.sort_unstable();
                docs.push(d)?;
                values.extend_from_slice(&tmp)?;
                addresses.push(values.len() as u64)?;
            }
            drop(s);
            let (docs, perm) = doc_order(f, "doc values", docs, spill)?;
            let (mut addresses, mut values) = (addresses.finish()?, values.finish()?);
            if let Some(p) = &perm {
                (addresses, values) = permute_lists(&addresses, &values, p, spill)?;
            }
            stream_section(w, kind, f.number, |o| {
                write_sorted_numeric_dv(&docs, &addresses, &values, max_doc, o);
                Ok(())
            })
        }
    }
}

struct SortedDict<'a>(&'a mut dyn SortedSource);

impl TermSeq for SortedDict<'_> {
    fn count(&self) -> u64 {
        self.0.value_count()
    }
    fn term(&mut self, ord: u64) -> &[u8] {
        self.0.value(ord)
    }
}

struct SortedSetDict<'a>(&'a mut dyn SortedSetSource);

impl TermSeq for SortedSetDict<'_> {
    fn count(&self) -> u64 {
        self.0.value_count()
    }
    fn term(&mut self, ord: u64) -> &[u8] {
        self.0.value(ord)
    }
}

/// Reorders per-doc value lists (`addresses` delimit doc `i`'s values in `flat`) by `perm`.
fn permute_lists<T: Pod>(
    addresses: &[u64],
    flat: &[T],
    perm: &[u32],
    spill: &SpillOptions,
) -> Result<(SpillSlice<u64>, SpillSlice<T>)> {
    let mut a = SpillVec::<u64>::new(spill);
    let mut v = SpillVec::<T>::new(spill);
    a.push(0)?;
    for &p in perm {
        let p = p as usize;
        v.extend_from_slice(&flat[addresses[p] as usize..addresses[p + 1] as usize])?;
        a.push(v.len() as u64)?;
    }
    Ok((a.finish()?, v.finish()?))
}


/// Suggest entries of one field collected in spill-backed arrays, viewed in sorted order.
struct SuggestArrays {
    key_bytes: SpillSlice<u8>,
    key_ends: SpillSlice<u64>,
    payload_bytes: SpillSlice<u8>,
    payload_ends: SpillSlice<u64>,
    weights: SpillSlice<u64>,
    docs: SpillSlice<u32>,
    /// Sorted position → arrival index (`None`: arrived sorted).
    perm: Option<SpillSlice<u32>>,
}

impl SuggestArrays {
    #[inline]
    fn arrival(&self, i: u64) -> usize {
        self.perm.as_ref().map_or(i as usize, |p| p[i as usize] as usize)
    }
    fn key_at(&self, a: usize) -> &[u8] {
        let s = if a == 0 { 0 } else { self.key_ends[a - 1] as usize };
        &self.key_bytes[s..self.key_ends[a] as usize]
    }
    fn payload_at(&self, a: usize) -> &[u8] {
        let s = if a == 0 { 0 } else { self.payload_ends[a - 1] as usize };
        &self.payload_bytes[s..self.payload_ends[a] as usize]
    }
    /// Entry order: key ascending, weight descending, doc, payload, then arrival.
    fn cmp_arrivals(&self, a: usize, b: usize) -> std::cmp::Ordering {
        self.key_at(a)
            .cmp(self.key_at(b))
            .then(self.weights[b].cmp(&self.weights[a]))
            .then(self.docs[a].cmp(&self.docs[b]))
            .then_with(|| self.payload_at(a).cmp(self.payload_at(b)))
            .then(a.cmp(&b))
    }
}

impl crate::suggest::SortedEntries for SuggestArrays {
    fn len(&self) -> u64 {
        self.docs.len() as u64
    }
    fn key(&self, i: u64) -> &[u8] {
        self.key_at(self.arrival(i))
    }
    fn weight(&self, i: u64) -> u64 {
        self.weights[self.arrival(i)]
    }
    fn doc(&self, i: u64) -> u32 {
        self.docs[self.arrival(i)]
    }
    fn payload(&self, i: u64) -> &[u8] {
        self.payload_at(self.arrival(i))
    }
}

fn write_suggest_field(
    f: &FieldInfo,
    mut s: Box<dyn crate::suggest::SuggestSource + '_>,
    max_doc: u32,
    spill: &SpillOptions,
    w: &mut ContainerWriter<'_>,
) -> Result<()> {
    let mut key_bytes = SpillVec::<u8>::new(spill);
    let mut key_ends = SpillVec::<u64>::new(spill);
    let mut payload_bytes = SpillVec::<u8>::new(spill);
    let mut payload_ends = SpillVec::<u64>::new(spill);
    let mut weights = SpillVec::<u64>::new(spill);
    let mut docs = SpillVec::<u32>::new(spill);
    while let Some(e) = s.next() {
        check_bound(f, "suggest entries", e.doc, max_doc)?;
        if e.weight > i64::MAX as u64 {
            return Err(invalid(format!("suggest entries of field [{}]: weight {} too large", f.name, e.weight)));
        }
        key_bytes.extend_from_slice(e.key)?;
        key_ends.push(key_bytes.len() as u64)?;
        payload_bytes.extend_from_slice(e.payload)?;
        payload_ends.push(payload_bytes.len() as u64)?;
        weights.push(e.weight)?;
        docs.push(e.doc)?;
    }
    drop(s);
    let n = docs.len();
    if n > u32::MAX as usize {
        return Err(invalid(format!("suggest entries of field [{}]: too many", f.name)));
    }
    let mut a = SuggestArrays {
        key_bytes: key_bytes.finish()?,
        key_ends: key_ends.finish()?,
        payload_bytes: payload_bytes.finish()?,
        payload_ends: payload_ends.finish()?,
        weights: weights.finish()?,
        docs: docs.finish()?,
        perm: None,
    };
    if (1..n).any(|i| a.cmp_arrivals(i - 1, i).is_gt()) {
        let mut perm = SpillVec::<u32>::new(spill);
        for i in 0..n as u32 {
            perm.push(i)?;
        }
        let mut perm = perm.finish()?;
        perm.sort_unstable_by(|&x, &y| a.cmp_arrivals(x as usize, y as usize));
        a.perm = Some(perm);
    }
    stream_section(w, SectionKind::Suggest, f.number, |o| {
        crate::suggest::write_suggest(&a, o);
        Ok(())
    })
}
