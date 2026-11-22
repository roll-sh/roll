//! A segment's live documents, read in order: what `SegmentReader` and the stored fields,
//! live docs, doc values and vectors readers give a restore, joined (Lucene's
//! `SoftDeletesDirectoryReaderWrapper` view: a document lives when its live bit is set and it
//! has no `__soft_deletes` value).
//!
//! Files come through [`Files`] (the caller's I/O: whole small files, ranges of large ones).
//! Large files are read as streams in windows ([`Limits::window`]): `.fdt` from its header to
//! its footer, its checksum checked as it goes; `.dvd` and `.vec` are passed through once to
//! check their checksums while only the ranges needed are kept (charged through
//! [`Files::charge`]), and vector values are streamed again in document order. A compound
//! segment's files are ranges of its `.cfs`.

use crate::codec::{self, Checksum};
use crate::compound::{self, Entry};
use crate::docvalues::{self, Numeric, NumericEntry};
use crate::fields::{self, FieldInfo, FieldInfos};
use crate::live::{self, Bits};
use crate::segments::{SegmentCommit, SegmentInfo};
use crate::stored::{self, Stored};
use crate::vectors::{self, VectorEntry};
use crate::{Error, Result, corrupt};

/// The caller's side: the shard's files.
pub trait Files {
    /// A file's length, `None` when the shard has no such file.
    fn len(&mut self, name: &str) -> Result<Option<u64>>;
    /// `len` bytes of `name` from `offset` (exactly, or an error).
    fn read(&mut self, name: &str, offset: u64, len: usize) -> Result<Vec<u8>>;
    /// Memory being held (`delta > 0`) or let go: an error stops the read (a breaker).
    fn charge(&mut self, delta: i64) -> Result<()>;
    /// Called between documents and windows: an error stops the read (cancellation, time).
    fn check(&mut self) -> Result<()>;
}

/// Bounds on what one segment's read holds.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Small files read whole (`.si`, `.cfe`, `.fnm`, `.fdm`, `.dvm`, `.liv`, `.vemf`).
    pub small_file: usize,
    /// What a stored fields chunk may decompress to (at least the largest document).
    pub chunk: usize,
    /// The doc values and vector ordinals kept for one segment.
    pub kept: usize,
    /// Bytes read at a time from a large file.
    pub window: usize,
}

/// One live document.
#[derive(Debug)]
pub struct Doc<'a> {
    pub doc: u32,
    /// `_id` as stored (Elasticsearch's `Uid` encoding).
    pub id: &'a [u8],
    pub routing: Option<&'a str>,
    pub source: Option<&'a [u8]>,
    pub seq_no: Option<i64>,
    pub version: Option<i64>,
    pub primary_term: Option<i64>,
    /// The vectors of the fields asked for, by field name.
    pub vectors: Vec<(&'a str, Vec<f32>)>,
}

/// What a segment's read found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub max_doc: u32,
    /// Live root documents given to the caller.
    pub docs: u64,
    /// Live documents without an `_id` (nested documents).
    pub nested: u64,
    pub deleted: u64,
}

/// Where a file of the segment is: a physical file, or a range of its `.cfs`.
struct View {
    segment: String,
    cfs: Option<(String, Vec<Entry>)>,
}

impl View {
    fn locate(&self, files: &mut dyn Files, name: &str) -> Result<Option<(String, u64, u64)>> {
        if let Some((cfs, entries)) = &self.cfs
            && let Some(suffix) = name.strip_prefix(self.segment.as_str())
            && let Some(e) = entries.iter().find(|e| e.suffix == suffix)
        {
            return Ok(Some((cfs.clone(), e.offset, e.length)));
        }
        Ok(files.len(name)?.map(|len| (name.to_string(), 0, len)))
    }

    fn need(&self, files: &mut dyn Files, name: &str) -> Result<(String, u64, u64)> {
        self.locate(files, name)?.ok_or_else(|| Error::Corrupt(format!("the segment's file [{name}] is missing")))
    }

    fn small(&self, files: &mut dyn Files, name: &str, limits: &Limits) -> Result<Vec<u8>> {
        let (file, offset, len) = self.need(files, name)?;
        if len > limits.small_file as u64 {
            return Err(Error::Unsupported(format!("[{name}] is [{len}] bytes, more than the [{}] read whole", limits.small_file)));
        }
        files.read(&file, offset, len as usize)
    }
}

/// A large file read front to back in windows; its checksum checked when `checked`.
struct Stream {
    file: String,
    base: u64,
    len: u64,
    /// Bytes fetched so far (relative to `base`).
    fetched: u64,
    buf: Vec<u8>,
    /// Where `buf` starts (relative to `base`).
    buf_start: u64,
    crc: Option<Checksum>,
    window: usize,
    charged: usize,
}

impl Stream {
    fn new(file: String, base: u64, len: u64, checked: bool, window: usize) -> Stream {
        Stream { file, base, len, fetched: 0, buf: Vec::new(), buf_start: 0, crc: checked.then(|| Checksum::new(len)), window: window.max(4096), charged: 0 }
    }

    /// Bytes from `at` on that are in memory.
    fn have(&self, at: u64) -> &[u8] {
        let from = (at.saturating_sub(self.buf_start) as usize).min(self.buf.len());
        &self.buf[from..]
    }

    /// Makes at least `n` bytes from `at` available (fewer at the end of the file): drops what
    /// is before `at`, fetches a window more.
    fn fill(&mut self, files: &mut dyn Files, at: u64, n: usize) -> Result<()> {
        if at < self.buf_start {
            return corrupt("a stream read backwards");
        }
        while (self.have(at).len()) < n && self.fetched < self.len {
            files.check()?;
            let stale = ((at - self.buf_start) as usize).min(self.buf.len());
            if stale > 0 {
                self.buf.drain(..stale);
                self.buf_start += stale as u64;
            }
            let want = (self.len - self.fetched).min(self.window.max(n.saturating_sub(self.buf.len())) as u64) as usize;
            let piece = files.read(&self.file, self.base + self.fetched, want)?;
            if piece.len() != want {
                return corrupt(format!("[{}] gave [{}] bytes, not [{want}]", self.file, piece.len()));
            }
            if let Some(c) = &mut self.crc {
                c.update(&piece);
            }
            self.fetched += want as u64;
            self.buf.extend_from_slice(&piece);
            let held = self.buf.capacity();
            files.charge(held as i64 - self.charged as i64)?;
            self.charged = held;
        }
        Ok(())
    }

    /// Reads to the end (what's left goes through the checksum only) and checks the checksum.
    fn finish(mut self, files: &mut dyn Files) -> Result<()> {
        while self.fetched < self.len {
            let at = self.fetched;
            self.buf.clear();
            self.buf_start = at;
            self.fill(files, at, 1)?;
        }
        files.charge(-(self.charged as i64))?;
        self.charged = 0;
        match self.crc.take() {
            Some(c) => c.finish(),
            None => Ok(()),
        }
    }

    fn release(&mut self, files: &mut dyn Files) {
        let _ = files.charge(-(self.charged as i64));
        self.charged = 0;
    }
}

/// Passes through a whole file once to check its checksum, keeping `ranges` (relative to the
/// file): their bytes, in the order asked.
fn keep_ranges(files: &mut dyn Files, at: (String, u64, u64), ranges: &[(u64, u64)], limits: &Limits, kept: &mut usize) -> Result<Vec<Vec<u8>>> {
    let (file, base, len) = at;
    let Some(total) = ranges.iter().try_fold(0u64, |t, r| t.checked_add(r.1)) else { return corrupt("doc values ranges overflow") };
    if *kept as u64 + total > limits.kept as u64 {
        return Err(Error::Unsupported(format!("the segment's doc values and vectors need more than [{}] bytes", limits.kept)));
    }
    for &(off, n) in ranges {
        if off.checked_add(n).is_none_or(|e| e > len) {
            return corrupt(format!("a range of [{n}] bytes at [{off}] is past the [{len}] bytes of [{file}]"));
        }
    }
    files.charge(total as i64)?;
    *kept += total as usize;
    let mut out: Vec<Vec<u8>> = ranges.iter().map(|r| Vec::with_capacity(r.1 as usize)).collect();
    let mut s = Stream::new(file, base, len, true, limits.window);
    let mut at = 0u64;
    while at < len {
        s.fill(files, at, 1)?;
        let piece = s.have(at).to_vec();
        for (i, &(off, n)) in ranges.iter().enumerate() {
            let (a, b) = (off.max(at), (off + n).min(at + piece.len() as u64));
            if a < b {
                out[i].extend_from_slice(&piece[(a - at) as usize..(b - at) as usize]);
            }
        }
        at += piece.len() as u64;
    }
    s.finish(files)?;
    Ok(out)
}

/// The per-field format segment suffix (`PerFieldDocValuesFormat`, `PerFieldKnnVectorsFormat`)
/// and file base name of field `f`'s doc values or vectors at generation `gen`.
fn per_field(segment: &str, f: &FieldInfo, kind: &str, generation: i64) -> Option<(String, String)> {
    let format = f.attribute(&format!("PerField{kind}Format.format"))?;
    let suffix = f.attribute(&format!("PerField{kind}Format.suffix"))?;
    let inner = format!("{format}_{suffix}");
    let full = if generation > 0 { format!("{}_{inner}", crate::base36(generation)) } else { inner };
    Some((full.clone(), format!("{segment}_{full}")))
}

/// Reads segment `commit` (its info `si`), giving each live document with an `_id` to
/// `on_doc`, which answers whether to go on. `vector_fields`: the vector fields whose values
/// are to be read (those `_source` leaves out).
pub fn read(files: &mut dyn Files, commit: &SegmentCommit, si: &SegmentInfo, vector_fields: &[String], limits: &Limits, on_doc: &mut dyn FnMut(Doc) -> Result<bool>) -> Result<Stats> {
    let name = commit.name.as_str();
    let mut view = View { segment: name.to_string(), cfs: None };
    if si.compound {
        let cfe = view.small(files, &format!("{name}.cfe"), limits)?;
        let cfs = format!("{name}.cfs");
        let cfs_len = files.len(&cfs)?.ok_or_else(|| Error::Corrupt(format!("the segment's file [{cfs}] is missing")))?;
        let entries = compound::read_entries(&cfe, &commit.id, cfs_len)?;
        let head = files.read(&cfs, 0, (4 + 1 + compound::DATA_CODEC.len() + 4 + 16 + 1).min(cfs_len as usize))?;
        compound::check_data_header(&head, &commit.id)?;
        view.cfs = Some((cfs, entries));
    }
    let infos: FieldInfos = match commit.field_infos_gen {
        -1 => fields::read_field_infos(&view.small(files, &format!("{name}.fnm"), limits)?, &commit.id, "")?,
        g => {
            let suffix = crate::base36(g);
            fields::read_field_infos(&view.small(files, &format!("{name}_{suffix}.fnm"), limits)?, &commit.id, &suffix)?
        }
    };
    let id_field = infos.by_name("_id").map(|f| f.number);
    let source_field = infos.by_name("_source").map(|f| f.number);
    let routing_field = infos.by_name("_routing").map(|f| f.number);
    let max_doc = si.max_doc;
    let mut stats = Stats { max_doc, ..Stats::default() };
    if max_doc == 0 {
        return Ok(stats);
    }
    // Live documents: hard deletes, then soft deletes.
    files.charge(Bits::bytes(max_doc) as i64)?;
    let mut kept = Bits::bytes(max_doc);
    let mut live = match crate::file_name_from_generation(name, "liv", commit.del_gen) {
        Some(liv) => live::read_live_docs(&view.small(files, &liv, limits)?, &commit.id, commit.del_gen, max_doc, commit.del_count as u32)?,
        None if commit.del_count == 0 => Bits::all(max_doc),
        None => return corrupt(format!("[{}] deletions without a live docs file", commit.del_count)),
    };
    // Numeric doc values wanted, by the dv file holding them: (dv base name, suffix) →
    // [(role, field, entry)].
    #[derive(Clone, Copy, PartialEq)]
    enum Role {
        SoftDeletes,
        SeqNo,
        Version,
        PrimaryTerm,
        /// `<field>._magnitude` of `vector_fields[n]`: Elasticsearch keeps cosine vectors
        /// normalized and their magnitude (a `float`'s bits) apart, when it isn't 1.
        Magnitude(usize),
    }
    let mut wanted: Vec<(Role, &FieldInfo)> = Vec::new();
    if let Some(f) = infos.fields.iter().find(|f| f.soft_deletes) {
        wanted.push((Role::SoftDeletes, f));
    }
    for (role, n) in [(Role::SeqNo, "_seq_no"), (Role::Version, "_version"), (Role::PrimaryTerm, "_primary_term")] {
        if let Some(f) = infos.by_name(n).filter(|f| f.doc_values == fields::DocValues::Numeric) {
            wanted.push((role, f));
        }
    }
    let magnitudes: Vec<String> = vector_fields.iter().map(|f| format!("{f}._magnitude")).collect();
    for (k, m) in magnitudes.iter().enumerate() {
        if let Some(f) = infos.by_name(m).filter(|f| f.doc_values == fields::DocValues::Numeric) {
            wanted.push((Role::Magnitude(k), f));
        }
    }
    // (dv suffix, file base name) → the fields read from it.
    type Groups = Vec<((String, String), Vec<(Role, u32)>)>;
    let mut groups: Groups = Vec::new();
    for (role, f) in &wanted {
        let generation = if f.dv_gen > 0 { f.dv_gen } else { -1 };
        match per_field(name, f, "DocValues", generation) {
            Some(key) if f.attribute("PerFieldDocValuesFormat.format") == Some("Lucene90") => match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, v)) => v.push((*role, f.number)),
                None => groups.push((key, vec![(*role, f.number)])),
            },
            _ if *role == Role::SoftDeletes => {
                return Err(Error::Unsupported(format!(
                    "the soft deletes field [{}] is in doc values format [{}]; roll reads [Lucene90]",
                    f.name,
                    f.attribute("PerFieldDocValuesFormat.format").unwrap_or("?")
                )));
            }
            // Sequence numbers and versions in another format: given as unknown.
            _ => {}
        }
    }
    // (role, entry, docs bytes, values bytes)
    type Kept = Vec<(Role, NumericEntry, Option<Vec<u8>>, Vec<u8>)>;
    let mut numerics: Kept = Vec::new();
    for ((suffix, base), members) in &groups {
        let meta = view.small(files, &format!("{base}.dvm"), limits)?;
        let entries = docvalues::read_meta(&meta, &commit.id, suffix, &infos)?;
        let at = view.need(files, &format!("{base}.dvd"))?;
        let head = files.read(&at.0, at.1, (4 + 1 + docvalues::DATA_CODEC.len() + 4 + 16 + 1 + suffix.len()).min(at.2 as usize))?;
        codec::check_index_header(&mut crate::input::Input::new(&head), docvalues::DATA_CODEC, 0, 2, &commit.id, suffix)?;
        let mut ranges = Vec::new();
        let mut picked = Vec::new();
        for (role, number) in members {
            let Some((_, e)) = entries.iter().find(|(n, _)| n == number) else { continue };
            let (docs, values) = e.ranges();
            picked.push((*role, e.clone(), docs.is_some()));
            if let Some(d) = docs {
                ranges.push(d);
            }
            ranges.push(values);
        }
        let mut got = keep_ranges(files, at, &ranges, limits, &mut kept)?.into_iter();
        for (role, e, has_docs) in picked {
            let docs = if has_docs { got.next() } else { None };
            numerics.push((role, e, docs, got.next().unwrap_or_default()));
        }
    }
    let mut soft = 0u64;
    let mut cursors: Vec<(Role, Numeric)> = Vec::new();
    for (role, e, docs, values) in &numerics {
        let mut n = Numeric::new(e, max_doc, docs.as_deref(), values)?;
        if *role == Role::SoftDeletes {
            for doc in 0..max_doc {
                if doc % 65536 == 0 {
                    files.check()?;
                }
                if n.advance(doc)?.is_some() && live.get(doc) {
                    live.clear(doc);
                    soft += 1;
                }
            }
        } else {
            cursors.push((*role, n));
        }
    }
    if soft != commit.soft_del_count as u64 {
        return corrupt(format!("[{soft}] soft-deleted documents where the commit records [{}]", commit.soft_del_count));
    }
    stats.deleted = (max_doc - live.count()) as u64;
    // Vectors left out of `_source`.
    struct VectorRead {
        name: String,
        /// Its place in `vector_fields`.
        at: usize,
        entry: VectorEntry,
        stream: Stream,
    }
    let mut vecs: Vec<VectorRead> = Vec::new();
    // Each field's `IndexedDISI` of the documents with a vector, when not every one has one.
    let mut ord_bytes: Vec<Option<Vec<u8>>> = Vec::new();
    for (at_field, field) in vector_fields.iter().enumerate() {
        let Some(f) = infos.by_name(field) else { continue };
        if f.vector_dims == 0 {
            continue;
        }
        let Some((suffix, base)) = per_field(name, f, "KnnVectors", -1) else {
            return Err(Error::Unsupported(format!("the vectors of [{field}] name no format")));
        };
        let Some(_) = view.locate(files, &format!("{base}.vemf"))? else {
            return Err(Error::Unsupported(format!(
                "the vectors of [{field}] are in format [{}] without raw vectors roll reads (Lucene99FlatVectorsFormat)",
                f.attribute("PerFieldKnnVectorsFormat.format").unwrap_or("?")
            )));
        };
        let meta = view.small(files, &format!("{base}.vemf"), limits)?;
        let entries = vectors::read_meta(&meta, &commit.id, &suffix, &infos)?;
        let Some(entry) = entries.into_iter().find(|e| e.field == f.number) else { continue };
        let at = view.need(files, &format!("{base}.vec"))?;
        let head = files.read(&at.0, at.1, (4 + 1 + vectors::DATA_CODEC.len() + 4 + 16 + 1 + suffix.len()).min(at.2 as usize))?;
        codec::check_index_header(&mut crate::input::Input::new(&head), vectors::DATA_CODEC, 0, 0, &commit.id, &suffix)?;
        if entry.data_offset.checked_add(entry.data_length).is_none_or(|e| e > at.2) {
            return corrupt(format!("the vectors of [{field}] are past the end of their file"));
        }
        let ord_range: Vec<(u64, u64)> = if entry.docs_offset >= 0 { vec![(entry.docs_offset as u64, entry.docs_length.max(0) as u64)] } else { Vec::new() };
        let ords = keep_ranges(files, at.clone(), &ord_range, limits, &mut kept)?.into_iter().next();
        if entry.size > max_doc {
            return corrupt(format!("[{}] vectors of [{field}] in a segment of [{max_doc}]", entry.size));
        }
        if entry.docs_offset == -1 && entry.size != max_doc {
            return corrupt(format!("dense vectors of [{field}]: [{}] for [{max_doc}] documents", entry.size));
        }
        let stream = Stream::new(at.0.clone(), at.1 + entry.data_offset, entry.data_length, false, limits.window);
        vecs.push(VectorRead { name: field.clone(), at: at_field, entry, stream });
        ord_bytes.push(ords);
    }
    let mut vec_ords: Vec<Option<docvalues::Disi>> = Vec::new();
    let mut vec_pending: Vec<Option<(u32, u64)>> = Vec::new();
    for (v, ords) in vecs.iter().zip(&ord_bytes) {
        match ords {
            Some(o) => {
                let mut d = docvalues::Disi::new(o, v.entry.dense_rank_power, v.entry.size as u64, max_doc)?;
                vec_pending.push(d.next_doc()?);
                vec_ords.push(Some(d));
            }
            None => {
                vec_ords.push(None);
                vec_pending.push(None);
            }
        }
    }
    // Stored fields, chunk by chunk.
    let chunk_size = stored::read_chunk_size(&view.small(files, &format!("{name}.fdm"), limits)?, &commit.id)?;
    let at = view.need(files, &format!("{name}.fdt"))?;
    let mut fdt = Stream::new(at.0, at.1, at.2, true, limits.window);
    let Some(end) = at.2.checked_sub(codec::FOOTER_LEN as u64) else { return corrupt(format!("[{name}.fdt] is [{}] bytes", at.2)) };
    fdt.fill(files, 0, 128)?;
    let (mode, header_len) = stored::read_fdt_header(fdt.have(0), &commit.id)?;
    let mut pos = header_len as u64;
    if pos > end {
        return corrupt(format!("[{name}.fdt] is shorter than its header and footer"));
    }
    let mut out = Vec::new();
    let mut held = 0usize;
    let mut next_doc = 0u32;
    let result = (|| -> Result<()> {
        while next_doc < max_doc {
            let mut want = 4096usize;
            let header = loop {
                fdt.fill(files, pos, want)?;
                let left = end - pos;
                let avail = &fdt.have(pos)[..fdt.have(pos).len().min(left as usize)];
                match stored::measure(avail, mode, chunk_size, max_doc - next_doc, limits.chunk)? {
                    Some(h) => break h,
                    None if avail.len() as u64 >= left => return corrupt("the stored fields end inside a chunk"),
                    None if want >= limits.chunk.saturating_mul(2).saturating_add(1 << 20) => {
                        return Err(Error::Unsupported(format!("a stored fields chunk is larger than [{}] bytes", limits.chunk)));
                    }
                    None => want = want.saturating_mul(2),
                }
            };
            if header.doc_base != next_doc {
                return corrupt(format!("a chunk starts at document [{}], expected [{next_doc}]", header.doc_base));
            }
            fdt.fill(files, pos, header.len())?;
            let total = header.total();
            if total > held {
                files.charge((total - held) as i64)?;
                held = total;
            }
            header.decompress(&fdt.have(pos)[..header.len()], mode, chunk_size, &mut out, &mut || files.check())?;
            for i in 0..header.docs() as usize {
                let doc = next_doc + i as u32;
                if !live.get(doc) {
                    continue;
                }
                // Nested documents store nothing (no `_id`, no `_source`).
                if header.field_counts[i] == 0 {
                    stats.nested += 1;
                    continue;
                }
                files.check()?;
                let (mut id, mut source, mut routing) = (None, None, None);
                for f in stored::read_doc(header.doc(&out, i), header.field_counts[i])? {
                    match f? {
                        (number, Stored::Bytes(b)) if Some(number) == id_field => id = Some(b),
                        (number, Stored::Bytes(b)) if Some(number) == source_field => source = Some(b),
                        (number, Stored::Str(s)) if Some(number) == routing_field => routing = Some(s),
                        _ => {}
                    }
                }
                let Some(id) = id else {
                    stats.nested += 1;
                    continue;
                };
                let mut d = Doc { doc, id, routing, source, seq_no: None, version: None, primary_term: None, vectors: Vec::new() };
                let mut scale: Vec<Option<f32>> = vec![None; vector_fields.len()];
                for (role, n) in cursors.iter_mut() {
                    let v = n.advance(doc)?;
                    match role {
                        Role::SeqNo => d.seq_no = v,
                        Role::Version => d.version = v,
                        Role::PrimaryTerm => d.primary_term = v,
                        Role::Magnitude(k) => scale[*k] = v.map(|bits| f32::from_bits(bits as u32)),
                        Role::SoftDeletes => {}
                    }
                }
                for (k, v) in vecs.iter_mut().enumerate() {
                    let ord = match &mut vec_ords[k] {
                        None => Some(doc as u64),
                        Some(disi) => {
                            while let Some((dd, _)) = vec_pending[k]
                                && dd < doc
                            {
                                vec_pending[k] = disi.next_doc()?;
                            }
                            match vec_pending[k] {
                                Some((dd, o)) if dd == doc => Some(o),
                                _ => None,
                            }
                        }
                    };
                    if let Some(ord) = ord {
                        let vb = v.entry.vector_bytes();
                        let off = ord * vb as u64;
                        v.stream.fill(files, off, vb)?;
                        let bytes = v.stream.have(off);
                        if bytes.len() < vb {
                            return corrupt(format!("the vectors of [{}] end early", v.name));
                        }
                        let mut values = vectors::decode(&v.entry, &bytes[..vb]);
                        if let Some(m) = scale[v.at] {
                            for x in values.iter_mut() {
                                *x *= m;
                            }
                        }
                        d.vectors.push((v.name.as_str(), values));
                    }
                }
                stats.docs += 1;
                if !on_doc(d)? {
                    return Ok(());
                }
            }
            next_doc += header.docs();
            pos += header.len() as u64;
        }
        if pos != end {
            return corrupt(format!("[{}] bytes between the last chunk and the footer", end.abs_diff(pos)));
        }
        Ok(())
    })();
    files.charge(-(held as i64))?;
    files.charge(-(kept as i64))?;
    for v in &mut vecs {
        v.stream.release(files);
    }
    match result {
        Ok(()) if next_doc >= max_doc => fdt.finish(files)?,
        Ok(()) => fdt.release(files),
        Err(e) => {
            fdt.release(files);
            return Err(e);
        }
    }
    Ok(stats)
}
