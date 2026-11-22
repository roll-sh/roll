//! CodecUtil (`org.apache.lucene.codecs.CodecUtil`): the header every Lucene file starts with
//! (magic, codec name, version; index headers add the segment id and a suffix) and the footer
//! it ends with (magic, algorithm 0, the CRC32 of everything before the checksum). Elasticsearch
//! wraps its own snapshot metadata blobs in the same header and footer.

use crate::input::Input;
use crate::{Error, Id, Result, corrupt};

pub const CODEC_MAGIC: u32 = 0x3fd7_6c17;
pub const FOOTER_MAGIC: u32 = !CODEC_MAGIC;
pub const FOOTER_LEN: usize = 16;

/// `checkHeader` without a name: the codec name and version, for telling formats apart.
pub fn read_header<'a>(inp: &mut Input<'a>) -> Result<(&'a str, i32)> {
    let magic = inp.be_i32()? as u32;
    if magic != CODEC_MAGIC {
        return corrupt(format!("codec header mismatch: actual header={magic:#x} vs expected header={CODEC_MAGIC:#x}"));
    }
    // CodecUtil writes the name as a string of at most 127 ASCII bytes.
    let name = inp.string()?;
    Ok((name, inp.be_i32()?))
}

/// The codec name a file starts with, when it has a header.
pub fn header_name(file: &[u8]) -> Option<&str> {
    read_header(&mut Input::new(file)).ok().map(|(n, _)| n)
}

/// `checkHeader`: the version, which must be in `min..=max`.
pub fn check_header(inp: &mut Input, name: &str, min: i32, max: i32) -> Result<i32> {
    let (actual, version) = read_header(inp)?;
    if actual != name {
        return Err(Error::Unsupported(format!("codec mismatch: actual codec={actual} vs expected codec={name}")));
    }
    if version < min || version > max {
        return Err(Error::Unsupported(format!("format version [{version}] of [{name}] is outside [{min}..{max}]")));
    }
    Ok(version)
}

/// `checkIndexHeader`: [`check_header`], then the segment id and the suffix.
pub fn check_index_header(inp: &mut Input, name: &str, min: i32, max: i32, id: &Id, suffix: &str) -> Result<i32> {
    let version = check_header(inp, name, min, max)?;
    check_id_and_suffix(inp, id, suffix)?;
    Ok(version)
}

/// What an index header has after its name and version.
pub fn check_id_and_suffix(inp: &mut Input, id: &Id, suffix: &str) -> Result<()> {
    let actual = inp.id()?;
    if &actual != id {
        return corrupt(format!("file mismatch, expected id={}, got={}", hex(id), hex(&actual)));
    }
    let n = inp.u8()? as usize;
    let s = inp.bytes(n)?;
    if s != suffix.as_bytes() {
        return corrupt(format!("file mismatch, expected suffix={suffix}, got={}", String::from_utf8_lossy(s)));
    }
    Ok(())
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The bytes of `file` before its footer.
pub fn body(file: &[u8]) -> Result<&[u8]> {
    if file.len() < FOOTER_LEN {
        return corrupt(format!("misplaced codec footer (file truncated?): length={} but footerLength=={FOOTER_LEN}", file.len()));
    }
    Ok(&file[..file.len() - FOOTER_LEN])
}

/// The checksum `footer` (a file's last 16 bytes) records, after checking its shape.
pub fn footer_checksum(footer: &[u8]) -> Result<u32> {
    let mut inp = Input::new(footer);
    let magic = inp.be_i32()? as u32;
    if magic != FOOTER_MAGIC {
        return corrupt(format!("codec footer mismatch (file truncated?): actual footer={magic:#x} vs expected footer={FOOTER_MAGIC:#x}"));
    }
    let algorithm = inp.be_i32()?;
    if algorithm != 0 {
        return corrupt(format!("codec footer mismatch: unknown algorithmID: {algorithm}"));
    }
    let value = inp.be_i64()?;
    if value as u64 & 0xFFFF_FFFF_0000_0000 != 0 {
        return corrupt(format!("Illegal CRC-32 checksum: {value}"));
    }
    Ok(value as u32)
}

/// `checkFooter` over a whole file: its footer's shape and its CRC32.
pub fn check_footer(file: &[u8]) -> Result<()> {
    let body = body(file)?;
    let expected = footer_checksum(&file[body.len()..])?;
    let mut h = crc32fast::Hasher::new();
    h.update(&file[..file.len() - 8]);
    let actual = h.finalize();
    if actual != expected {
        return corrupt(format!("checksum failed (hardware problem?) : expected={expected:x} actual={actual:x}"));
    }
    Ok(())
}

/// The checksum of a file read as a stream: bytes go in as they arrive ([`Checksum::update`]),
/// and [`Checksum::finish`] checks the footer once the file's `len` bytes went by.
pub struct Checksum {
    len: u64,
    seen: u64,
    crc: crc32fast::Hasher,
    tail: [u8; FOOTER_LEN],
}

impl Checksum {
    pub fn new(len: u64) -> Checksum {
        Checksum { len, seen: 0, crc: crc32fast::Hasher::new(), tail: [0; FOOTER_LEN] }
    }

    /// Bytes of the file, in order.
    pub fn update(&mut self, b: &[u8]) {
        let crc_end = self.len.saturating_sub(8);
        let footer_start = self.len.saturating_sub(FOOTER_LEN as u64);
        let start = self.seen;
        if start < crc_end {
            let n = ((crc_end - start) as usize).min(b.len());
            self.crc.update(&b[..n]);
        }
        for (i, &x) in b.iter().enumerate() {
            let at = start + i as u64;
            if at >= footer_start && at < self.len {
                self.tail[(at - footer_start) as usize] = x;
            }
        }
        self.seen += b.len() as u64;
    }

    /// Whether all of the file went by and its checksum matches.
    pub fn finish(self) -> Result<()> {
        if self.seen != self.len || self.len < FOOTER_LEN as u64 {
            return corrupt(format!("checksum not verified: [{}] of [{}] bytes read", self.seen, self.len));
        }
        let expected = footer_checksum(&self.tail)?;
        let actual = self.crc.finalize();
        if actual != expected {
            return corrupt(format!("checksum failed (hardware problem?) : expected={expected:x} actual={actual:x}"));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A file with an index header and a valid footer around `content`.
    pub fn file(name: &str, version: i32, id: &Id, suffix: &str, content: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&CODEC_MAGIC.to_be_bytes());
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&version.to_be_bytes());
        out.extend_from_slice(id);
        out.push(suffix.len() as u8);
        out.extend_from_slice(suffix.as_bytes());
        out.extend_from_slice(content);
        out.extend_from_slice(&FOOTER_MAGIC.to_be_bytes());
        out.extend_from_slice(&0i32.to_be_bytes());
        let mut h = crc32fast::Hasher::new();
        h.update(&out);
        out.extend_from_slice(&(h.finalize() as u64).to_be_bytes());
        out
    }

    #[test]
    fn headers_and_footers() {
        let id = [7u8; 16];
        let f = file("Test", 2, &id, "s", b"payload");
        check_footer(&f).unwrap();
        let mut inp = Input::new(&f);
        assert_eq!(check_index_header(&mut inp, "Test", 0, 2, &id, "s").unwrap(), 2);
        assert_eq!(inp.bytes(7).unwrap(), b"payload");
        assert!(matches!(check_header(&mut Input::new(&f), "Other", 0, 2), Err(Error::Unsupported(_))));
        assert!(check_index_header(&mut Input::new(&f), "Test", 0, 2, &[0; 16], "s").is_err());
        let mut bad = f.clone();
        bad[30] ^= 1;
        assert!(check_footer(&bad).is_err());
        assert!(check_footer(&f[..f.len() - 1]).is_err());
        // Streamed in odd pieces, the same answer.
        let mut c = Checksum::new(f.len() as u64);
        for piece in f.chunks(5) {
            c.update(piece);
        }
        c.finish().unwrap();
        let mut c = Checksum::new(bad.len() as u64);
        c.update(&bad);
        assert!(c.finish().is_err());
    }
}
