//! Push-based GZIP (multi-member) and BZIP2 (multi-stream) decompression into a caller-sized
//! buffer, so a decompression bomb only ever produces one buffer at a time.

use crate::error::{Result, SelectError};
use crate::request::Compression;

fn truncated(detail: impl std::fmt::Display) -> SelectError {
    SelectError::new(
        "TruncatedInput",
        format!("Object decompression failed ({detail}). Check that the object is properly compressed using the format specified in the request."),
    )
}

pub(crate) enum Decoder {
    Gzip(Box<Gzip>),
    Bzip2(Box<Bzip2>),
}

impl Decoder {
    pub fn new(c: Compression) -> Option<Decoder> {
        match c {
            Compression::None => None,
            Compression::Gzip => Some(Decoder::Gzip(Box::new(Gzip::new()))),
            Compression::Bzip2 => Some(Decoder::Bzip2(Box::new(Bzip2 { d: None, streams: 0 }))),
        }
    }

    /// Decompress from `input` into `out`'s spare capacity (it never grows `out`). Returns the
    /// input consumed; call again (with the rest, or nothing) while it makes progress.
    pub fn decompress(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<usize> {
        match self {
            Decoder::Gzip(g) => g.decompress(input, out),
            Decoder::Bzip2(b) => b.decompress(input, out),
        }
    }

    /// The input ended: it must have ended between members/streams.
    pub fn finish(&self) -> Result<()> {
        match self {
            Decoder::Gzip(g) => match g.st {
                Gz::Between => Ok(()),
                Gz::Header if g.n == 0 && g.members == 0 => Ok(()),
                _ => Err(truncated("the GZIP stream is truncated")),
            },
            Decoder::Bzip2(b) => {
                if b.d.is_none() {
                    Ok(())
                } else {
                    Err(truncated("the BZIP2 stream is truncated"))
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gz {
    Header,
    ExtraLen,
    Extra,
    Name,
    Comment,
    HeaderCrc,
    Body,
    Trailer,
    /// Between members: another member, zero padding, or the end.
    Between,
}

const FHCRC: u8 = 2;
const FEXTRA: u8 = 4;
const FNAME: u8 = 8;
const FCOMMENT: u8 = 16;

pub(crate) struct Gzip {
    st: Gz,
    inflate: flate2::Decompress,
    crc: flate2::Crc,
    hdr: [u8; 10],
    n: usize,
    flags: u8,
    extra: usize,
    members: u64,
}

impl Gzip {
    fn new() -> Gzip {
        Gzip { st: Gz::Header, inflate: flate2::Decompress::new(false), crc: flate2::Crc::new(), hdr: [0; 10], n: 0, flags: 0, extra: 0, members: 0 }
    }

    /// The header state after `from`, by the flags.
    fn next_header_state(&self, from: Gz) -> Gz {
        let order = [(Gz::ExtraLen, FEXTRA), (Gz::Name, FNAME), (Gz::Comment, FCOMMENT), (Gz::HeaderCrc, FHCRC)];
        let start = match from {
            Gz::Header => 0,
            Gz::ExtraLen | Gz::Extra => 1,
            Gz::Name => 2,
            Gz::Comment => 3,
            _ => 4,
        };
        order[start..].iter().find(|(_, f)| self.flags & f != 0).map_or(Gz::Body, |(s, _)| *s)
    }

    /// Collect `want` header bytes into `hdr`; true once complete.
    fn collect(&mut self, input: &[u8], i: &mut usize, want: usize) -> bool {
        while self.n < want && *i < input.len() {
            self.hdr[self.n] = input[*i];
            self.n += 1;
            *i += 1;
        }
        self.n == want
    }

    fn decompress(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<usize> {
        let mut i = 0;
        loop {
            match self.st {
                Gz::Header => {
                    if !self.collect(input, &mut i, 10) {
                        return Ok(i);
                    }
                    if self.hdr[0] != 0x1f || self.hdr[1] != 0x8b || self.hdr[2] != 8 || self.hdr[3] & 0xe0 != 0 {
                        return Err(truncated("not a GZIP stream"));
                    }
                    self.flags = self.hdr[3];
                    self.n = 0;
                    self.st = self.next_header_state(Gz::Header);
                }
                Gz::ExtraLen => {
                    if !self.collect(input, &mut i, 2) {
                        return Ok(i);
                    }
                    self.extra = u16::from_le_bytes([self.hdr[0], self.hdr[1]]) as usize;
                    self.n = 0;
                    self.st = Gz::Extra;
                }
                Gz::Extra => {
                    let k = self.extra.min(input.len() - i);
                    i += k;
                    self.extra -= k;
                    if self.extra > 0 {
                        return Ok(i);
                    }
                    self.st = self.next_header_state(Gz::Extra);
                }
                Gz::Name | Gz::Comment => match memchr::memchr(0, &input[i..]) {
                    Some(j) => {
                        i += j + 1;
                        self.st = self.next_header_state(self.st);
                    }
                    None => return Ok(input.len()),
                },
                Gz::HeaderCrc => {
                    if !self.collect(input, &mut i, 2) {
                        return Ok(i);
                    }
                    self.n = 0;
                    self.st = Gz::Body;
                }
                Gz::Body => {
                    if out.len() == out.capacity() {
                        return Ok(i);
                    }
                    let (before_in, olen) = (self.inflate.total_in(), out.len());
                    let status = self
                        .inflate
                        .decompress_vec(&input[i..], out, flate2::FlushDecompress::None)
                        .map_err(|e| truncated(format_args!("invalid deflate data: {e}")))?;
                    let consumed = (self.inflate.total_in() - before_in) as usize;
                    i += consumed;
                    self.crc.update(&out[olen..]);
                    if status == flate2::Status::StreamEnd {
                        self.st = Gz::Trailer;
                        self.n = 0;
                    } else if consumed == 0 && out.len() == olen {
                        return Ok(i);
                    }
                }
                Gz::Trailer => {
                    if !self.collect(input, &mut i, 8) {
                        return Ok(i);
                    }
                    let crc = u32::from_le_bytes([self.hdr[0], self.hdr[1], self.hdr[2], self.hdr[3]]);
                    let size = u32::from_le_bytes([self.hdr[4], self.hdr[5], self.hdr[6], self.hdr[7]]);
                    if crc != self.crc.sum() || size != self.crc.amount() {
                        return Err(truncated("the GZIP checksum doesn't match"));
                    }
                    self.inflate.reset(false);
                    self.crc.reset();
                    self.members += 1;
                    self.n = 0;
                    self.st = Gz::Between;
                }
                Gz::Between => {
                    while i < input.len() && input[i] == 0 {
                        i += 1;
                    }
                    if i == input.len() {
                        return Ok(i);
                    }
                    self.st = Gz::Header;
                }
            }
        }
    }
}

pub(crate) struct Bzip2 {
    d: Option<bzip2::Decompress>,
    streams: u64,
}

impl Bzip2 {
    fn decompress(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<usize> {
        let mut i = 0;
        loop {
            if out.len() == out.capacity() {
                return Ok(i);
            }
            if self.d.is_none() {
                if i == input.len() {
                    return Ok(i);
                }
                self.d = Some(bzip2::Decompress::new(false));
            }
            let Some(d) = self.d.as_mut() else { return Ok(i) };
            let (before_in, olen) = (d.total_in(), out.len());
            let status = d.decompress_vec(&input[i..], out).map_err(|e| truncated(format_args!("invalid BZIP2 data: {e}")))?;
            let consumed = (d.total_in() - before_in) as usize;
            i += consumed;
            match status {
                bzip2::Status::StreamEnd => {
                    self.d = None;
                    self.streams += 1;
                }
                bzip2::Status::MemNeeded => return Err(truncated("out of memory")),
                _ if consumed == 0 && out.len() == olen => return Ok(i),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    pub(crate) fn bzip2(data: &[u8]) -> Vec<u8> {
        let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn run(c: Compression, input: &[u8], chunk: usize) -> Result<Vec<u8>> {
        let mut d = Decoder::new(c).unwrap();
        let mut all = Vec::new();
        let mut out = Vec::with_capacity(7);
        for piece in input.chunks(chunk.max(1)).chain(std::iter::once(&[][..])) {
            let mut rest = piece;
            loop {
                let n = d.decompress(rest, &mut out)?;
                rest = &rest[n..];
                let produced = !out.is_empty();
                all.extend_from_slice(&out);
                out.clear();
                if rest.is_empty() && !produced {
                    break;
                }
            }
        }
        d.finish()?;
        Ok(all)
    }

    #[test]
    fn gzip_members_in_any_chunks() {
        let a = b"hello, world\n".repeat(50);
        let b = b"second member\n";
        let mut input = gzip(&a);
        input.extend(gzip(b));
        input.extend([0, 0, 0]);
        let mut want = a.clone();
        want.extend_from_slice(b);
        for chunk in [1, 2, 5, 13, 1000, input.len()] {
            assert_eq!(run(Compression::Gzip, &input, chunk).unwrap(), want, "chunk {chunk}");
        }
        // A header with every optional field.
        let mut h = flate2::GzBuilder::new().filename("f.csv").comment("c").extra(vec![1, 2, 3]).write(Vec::new(), flate2::Compression::fast());
        h.write_all(b"x,y\n").unwrap();
        let input = h.finish().unwrap();
        for chunk in [1, 3, input.len()] {
            assert_eq!(run(Compression::Gzip, &input, chunk).unwrap(), b"x,y\n");
        }
    }

    #[test]
    fn gzip_errors() {
        let good = gzip(b"some data here");
        assert_eq!(run(Compression::Gzip, &good[..good.len() - 3], 4).unwrap_err().code, "TruncatedInput");
        let mut bad = good.clone();
        let n = bad.len();
        bad[n - 6] ^= 0xff;
        assert_eq!(run(Compression::Gzip, &bad, 4).unwrap_err().code, "TruncatedInput");
        assert_eq!(run(Compression::Gzip, b"plain text, not gzip", 4).unwrap_err().code, "TruncatedInput");
        assert!(run(Compression::Gzip, b"", 4).unwrap().is_empty());
    }

    #[test]
    fn bzip2_streams_in_any_chunks() {
        let a = b"a,b,c\n".repeat(1000);
        let mut input = bzip2(&a);
        input.extend(bzip2(b"tail\n"));
        let mut want = a.clone();
        want.extend_from_slice(b"tail\n");
        for chunk in [1, 3, 100, input.len()] {
            assert_eq!(run(Compression::Bzip2, &input, chunk).unwrap(), want, "chunk {chunk}");
        }
        let good = bzip2(b"some data");
        assert_eq!(run(Compression::Bzip2, &good[..good.len() - 2], 3).unwrap_err().code, "TruncatedInput");
        assert_eq!(run(Compression::Bzip2, b"not bzip2 at all", 3).unwrap_err().code, "TruncatedInput");
    }
}
