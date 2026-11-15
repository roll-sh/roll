//! The segment container: one file holding every structure of a segment.
//!
//! ```text
//! [header 24 B]  magic "RLSG" | format version u32 | segment id [16]
//! [section]*     each starts on an 8-byte boundary (zero padding between sections)
//! [toc]          count varint, then per section: kind u16, field varint, offset varint,
//!                length varint, crc32c u32
//! [footer 24 B]  toc offset u64 | toc length u32 | toc crc32c u32 | file crc32c u32 | "GSLR"
//! ```
//!
//! The file checksum covers every byte before the footer, so a downloaded segment can be verified
//! in one sequential pass ([`ContainerReader::verify`]). Opening only checks the header, footer
//! and table of contents (and, through [`ContainerReader::verify_section`], the small metadata
//! sections), so opening a large memory-mapped segment does not fault in its postings. Sections
//! are addressed by `(kind, field)`; global sections use field [`GLOBAL`].
//!
//! The 8-byte alignment lets readers reinterpret fixed-width arrays (vector components, skip
//! tables) without copying when the backing storage is itself aligned (memory maps are).

use std::collections::HashMap;
use std::io::Write;

use crate::error::{Result, corrupt};
use crate::io::{In, Out};
use crate::slice::Slice;

pub const MAGIC: [u8; 4] = *b"RLSG";
pub const FOOTER_MAGIC: [u8; 4] = *b"GSLR";
pub const FORMAT_VERSION: u32 = 1;
pub const HEADER_LEN: usize = 24;
pub const FOOTER_LEN: usize = 24;
/// Field id of sections that belong to the whole segment.
pub const GLOBAL: u32 = u32::MAX;

/// What a section holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u16)]
pub enum SectionKind {
    /// Segment info, field infos and per-field metadata.
    Meta = 1,
    /// Term dictionary index (one key per block of terms).
    TermsIndex = 2,
    /// Term metadata blocks (doc freq, total term freq, postings pointers).
    TermsMeta = 3,
    /// Doc ids, frequencies and inline skip data with impacts.
    Docs = 4,
    /// Positions.
    Positions = 5,
    /// Payloads and offsets.
    Payloads = 6,
    Norms = 7,
    DocValues = 8,
    Points = 9,
    Stored = 10,
    StoredDict = 11,
    VectorData = 12,
    VectorGraph = 13,
    VectorQuantized = 14,
    /// Bloom filter over a field's terms.
    TermsBloom = 15,
    /// Suggest (completion) entries of a field.
    Suggest = 16,
}

impl SectionKind {
    fn from_u16(v: u16) -> Option<SectionKind> {
        use SectionKind::*;
        Some(match v {
            1 => Meta,
            2 => TermsIndex,
            3 => TermsMeta,
            4 => Docs,
            5 => Positions,
            6 => Payloads,
            7 => Norms,
            8 => DocValues,
            9 => Points,
            10 => Stored,
            11 => StoredDict,
            12 => VectorData,
            13 => VectorGraph,
            14 => VectorQuantized,
            15 => TermsBloom,
            16 => Suggest,
            _ => return None,
        })
    }
}

/// One table-of-contents entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionEntry {
    pub kind: SectionKind,
    pub field: u32,
    pub offset: u64,
    pub len: u64,
    pub crc: u32,
}

/// Streams sections into a writer, computing checksums on the fly.
pub struct ContainerWriter<'w> {
    out: &'w mut dyn Write,
    pos: u64,
    crc: u32,
    toc: Vec<SectionEntry>,
    open: Option<(SectionKind, u32, u64, u32)>,
}

impl<'w> ContainerWriter<'w> {
    pub fn new(out: &'w mut dyn Write, segment_id: [u8; 16]) -> Result<ContainerWriter<'w>> {
        let mut w = ContainerWriter {
            out,
            pos: 0,
            crc: 0,
            toc: Vec::new(),
            open: None,
        };
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.put_bytes(&MAGIC);
        header.put_u32(FORMAT_VERSION);
        header.put_bytes(&segment_id);
        w.raw(&header)?;
        Ok(w)
    }

    fn raw(&mut self, b: &[u8]) -> Result<()> {
        self.out.write_all(b)?;
        self.crc = crc32c::crc32c_append(self.crc, b);
        self.pos += b.len() as u64;
        Ok(())
    }

    fn pad(&mut self) -> Result<()> {
        let pad = (8 - (self.pos % 8) as usize) % 8;
        if pad > 0 {
            self.raw(&[0u8; 8][..pad])?;
        }
        Ok(())
    }

    /// Writes a complete section.
    pub fn section(&mut self, kind: SectionKind, field: u32, bytes: &[u8]) -> Result<()> {
        self.begin(kind, field)?;
        self.write(bytes)?;
        self.end()
    }

    /// Starts a section written in parts with [`write`](Self::write).
    pub fn begin(&mut self, kind: SectionKind, field: u32) -> Result<()> {
        assert!(self.open.is_none(), "section already open");
        self.pad()?;
        self.open = Some((kind, field, self.pos, 0));
        Ok(())
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let (k, f, start, crc) = self.open.expect("no open section");
        self.raw(bytes)?;
        self.open = Some((k, f, start, crc32c::crc32c_append(crc, bytes)));
        Ok(())
    }

    pub fn end(&mut self) -> Result<()> {
        let (kind, field, offset, crc) = self.open.take().expect("no open section");
        self.toc.push(SectionEntry {
            kind,
            field,
            offset,
            len: self.pos - offset,
            crc,
        });
        Ok(())
    }

    /// Current file offset.
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// Writes the table of contents and footer; returns the file length.
    pub fn finish(mut self) -> Result<u64> {
        assert!(self.open.is_none(), "section left open");
        self.pad()?;
        let mut toc = Vec::new();
        toc.put_vu64(self.toc.len() as u64);
        for e in &self.toc {
            toc.put_u16(e.kind as u16);
            toc.put_vu32(e.field);
            toc.put_vu64(e.offset);
            toc.put_vu64(e.len);
            toc.put_u32(e.crc);
        }
        let toc_offset = self.pos;
        let toc_crc = crc32c::crc32c(&toc);
        self.raw(&toc)?;
        let mut footer = Vec::with_capacity(FOOTER_LEN);
        footer.put_u64(toc_offset);
        footer.put_u32(toc.len() as u32);
        footer.put_u32(toc_crc);
        footer.put_u32(self.crc);
        footer.put_bytes(&FOOTER_MAGIC);
        self.out.write_all(&footer)?;
        self.out.flush()?;
        Ok(self.pos + FOOTER_LEN as u64)
    }
}

impl crate::io::ByteSink for ContainerWriter<'_> {
    fn put_all(&mut self, b: &[u8]) -> Result<()> {
        self.write(b)
    }
}

/// Byte range of the table of contents of a `file_len`-byte segment, read from its footer (the
/// last [`FOOTER_LEN`] bytes) with the checks [`ContainerReader::open`] makes. A reader of a
/// segment that is still downloading fetches header and footer, then this range, before it opens
/// the container and learns where the sections are.
pub fn toc_range(file_len: u64, footer: &[u8]) -> Result<std::ops::Range<u64>> {
    if file_len < (HEADER_LEN + FOOTER_LEN) as u64 || footer.len() != FOOTER_LEN {
        return Err(corrupt(format!("segment too short ({file_len} bytes)")));
    }
    if footer[FOOTER_LEN - 4..] != FOOTER_MAGIC {
        return Err(corrupt("bad segment footer magic (truncated file?)"));
    }
    let toc_offset = u64::from_le_bytes(footer[..8].try_into().unwrap());
    let toc_len = u32::from_le_bytes(footer[8..12].try_into().unwrap()) as u64;
    let footer_at = file_len - FOOTER_LEN as u64;
    if toc_offset < HEADER_LEN as u64 || toc_offset.checked_add(toc_len) != Some(footer_at) {
        return Err(corrupt("table of contents does not end at the footer"));
    }
    Ok(toc_offset..footer_at)
}

/// Parsed container: header, table of contents and section lookup.
#[derive(Clone, Debug)]
pub struct ContainerReader {
    data: Slice,
    segment_id: [u8; 16],
    entries: Vec<SectionEntry>,
    index: HashMap<(SectionKind, u32), usize>,
    file_crc: u32,
}

impl ContainerReader {
    pub fn open(data: Slice) -> Result<ContainerReader> {
        if data.len() < HEADER_LEN + FOOTER_LEN {
            return Err(corrupt(format!("segment too short ({} bytes)", data.len())));
        }
        if data[..4] != MAGIC {
            return Err(corrupt("bad segment header magic"));
        }
        let version = u32::from_le_bytes(data[4..8].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(corrupt(format!(
                "unsupported segment format version {version}"
            )));
        }
        let segment_id: [u8; 16] = data[8..24].try_into().unwrap();
        let footer_at = data.len() - FOOTER_LEN;
        let mut f = In::new(&data[footer_at..]);
        let toc_offset = f.u64()?;
        let toc_len = f.u32()? as u64;
        let toc_crc = f.u32()?;
        let file_crc = f.u32()?;
        if f.bytes(4)? != FOOTER_MAGIC {
            return Err(corrupt("bad segment footer magic (truncated file?)"));
        }
        if toc_offset.checked_add(toc_len) != Some(footer_at as u64) {
            return Err(corrupt("table of contents does not end at the footer"));
        }
        let toc = &data[toc_offset as usize..footer_at];
        if crc32c::crc32c(toc) != toc_crc {
            return Err(corrupt("table of contents checksum mismatch"));
        }
        let mut t = In::new(toc);
        // Each entry takes at least 9 bytes, which bounds the count by the bytes present.
        let count = t.vlen(toc.len() / 9)?;
        let mut entries = Vec::with_capacity(count);
        let mut index = HashMap::with_capacity(count);
        for _ in 0..count {
            let kind_raw = t.u16()?;
            let kind = SectionKind::from_u16(kind_raw)
                .ok_or_else(|| corrupt(format!("unknown section kind {kind_raw}")))?;
            let field = t.vu32()?;
            let offset = t.vu64()?;
            let len = t.vu64()?;
            let crc = t.u32()?;
            if offset < HEADER_LEN as u64 || offset.checked_add(len).is_none_or(|e| e > toc_offset)
            {
                return Err(corrupt(format!("section {kind:?}/{field} out of bounds")));
            }
            if index.insert((kind, field), entries.len()).is_some() {
                return Err(corrupt(format!("duplicate section {kind:?}/{field}")));
            }
            entries.push(SectionEntry {
                kind,
                field,
                offset,
                len,
                crc,
            });
        }
        Ok(ContainerReader {
            data,
            segment_id,
            entries,
            index,
            file_crc,
        })
    }

    pub fn segment_id(&self) -> [u8; 16] {
        self.segment_id
    }

    pub fn entries(&self) -> &[SectionEntry] {
        &self.entries
    }

    pub fn data(&self) -> &Slice {
        &self.data
    }

    pub fn entry(&self, kind: SectionKind, field: u32) -> Option<&SectionEntry> {
        self.index.get(&(kind, field)).map(|&i| &self.entries[i])
    }

    /// The bytes of a section (zero-copy).
    pub fn section(&self, kind: SectionKind, field: u32) -> Option<Slice> {
        let e = self.entry(kind, field)?;
        Some(
            self.data
                .slice(e.offset as usize..(e.offset + e.len) as usize),
        )
    }

    /// Checks one section's checksum.
    pub fn verify_section(&self, kind: SectionKind, field: u32) -> Result<()> {
        if let Some(e) = self.entry(kind, field) {
            let bytes = &self.data[e.offset as usize..(e.offset + e.len) as usize];
            if crc32c::crc32c(bytes) != e.crc {
                return Err(corrupt(format!(
                    "checksum mismatch in section {kind:?}/{field}"
                )));
            }
        }
        Ok(())
    }

    /// Checks the whole-file checksum (one sequential pass over the file).
    pub fn verify(&self) -> Result<()> {
        let body = &self.data[..self.data.len() - FOOTER_LEN];
        if crc32c::crc32c(body) != self.file_crc {
            return Err(corrupt("segment checksum mismatch"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build() -> Vec<u8> {
        let mut out = Vec::new();
        let mut w = ContainerWriter::new(&mut out, [7; 16]).unwrap();
        w.section(SectionKind::Meta, GLOBAL, b"meta!").unwrap();
        w.begin(SectionKind::Docs, 3).unwrap();
        w.write(b"abc").unwrap();
        w.write(b"def").unwrap();
        w.end().unwrap();
        w.section(SectionKind::Norms, 3, b"").unwrap();
        w.finish().unwrap();
        out
    }

    #[test]
    fn round_trips() {
        let out = build();
        let r = ContainerReader::open(Slice::from(out)).unwrap();
        assert_eq!(r.segment_id(), [7; 16]);
        assert_eq!(&*r.section(SectionKind::Meta, GLOBAL).unwrap(), b"meta!");
        assert_eq!(&*r.section(SectionKind::Docs, 3).unwrap(), b"abcdef");
        assert_eq!(r.section(SectionKind::Norms, 3).unwrap().len(), 0);
        assert!(r.section(SectionKind::Docs, 4).is_none());
        for e in r.entries() {
            assert_eq!(e.offset % 8, 0);
            r.verify_section(e.kind, e.field).unwrap();
        }
        r.verify().unwrap();
    }

    #[test]
    fn detects_every_single_byte_flip() {
        let out = build();
        for i in 0..out.len() {
            let mut bad = out.clone();
            bad[i] ^= 0x10;
            let detected = match ContainerReader::open(Slice::from(bad)) {
                Err(_) => true,
                Ok(r) => {
                    r.verify().is_err()
                        || r.entries()
                            .iter()
                            .any(|e| r.verify_section(e.kind, e.field).is_err())
                }
            };
            // The file checksum itself sits in the footer: flipping it is caught by `verify`.
            assert!(detected, "flip at byte {i} undetected");
        }
    }

    #[test]
    fn toc_range_matches_open() {
        let out = build();
        let len = out.len() as u64;
        let r = toc_range(len, &out[out.len() - FOOTER_LEN..]).unwrap();
        // The entries parse from exactly that range.
        let c = ContainerReader::open(Slice::from(out.clone())).unwrap();
        let last = c.entries().iter().map(|e| e.offset + e.len).max().unwrap();
        assert!(r.start >= last && r.end == len - FOOTER_LEN as u64);
        assert!(toc_range(len, &out[..FOOTER_LEN]).is_err());
        assert!(toc_range(10, &out[out.len() - FOOTER_LEN..]).is_err());
        assert!(toc_range(len + 8, &out[out.len() - FOOTER_LEN..]).is_err());
        assert!(toc_range(len, &out[out.len() - FOOTER_LEN + 1..]).is_err());
    }

    #[test]
    fn rejects_truncation() {
        let out = build();
        for n in 0..out.len() {
            assert!(
                ContainerReader::open(Slice::from(out[..n].to_vec())).is_err(),
                "len {n}"
            );
        }
    }
}
