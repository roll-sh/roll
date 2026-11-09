//! Bounded binary input: Java `DataInput` big-endian primitives and Cassandra's variable-length
//! integers (`org.apache.cassandra.utils.vint.VIntCoding`).
//!
//! Every length read from a file is untrusted: [`In::bytes`] fails unless that many bytes are
//! actually left, so a corrupt length never sizes an allocation.

use anyhow::{Result, bail};
use bytes::Bytes;

/// A source of bytes the SSTable parsers read from: a slice ([`SliceIn`]) or a whole data file
/// ([`crate::data::DataReader`]).
pub trait In {
    /// Bytes left (an upper bound on what any read may return).
    fn remaining(&self) -> u64;
    /// The next `n` bytes, which must all be present.
    fn bytes(&mut self, n: usize) -> Result<Bytes>;
    fn skip(&mut self, n: u64) -> Result<()>;
    fn u8(&mut self) -> Result<u8>;

    fn be16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn be32(&mut self) -> Result<u32> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn be64(&mut self) -> Result<u64> {
        let b = self.bytes(8)?;
        Ok(u64::from_be_bytes(b[..8].try_into().expect("8 bytes")))
    }

    /// `VIntCoding.readUnsignedVInt`: the number of leading one bits of the first byte is the
    /// number of extra bytes.
    fn uvint(&mut self) -> Result<u64> {
        let first = self.u8()?;
        if first & 0x80 == 0 {
            return Ok(first as u64);
        }
        let extra = first.leading_ones() as usize;
        let mut v = if extra >= 8 { 0 } else { (first & (0xff >> extra)) as u64 };
        for _ in 0..extra {
            v = (v << 8) | self.u8()? as u64;
        }
        Ok(v)
    }

    /// `readUnsignedVInt32`: the value must fit an `int` once sign-extended back (writers pass
    /// negative ints through `(long) value`).
    fn uvint32(&mut self) -> Result<i32> {
        let v = self.uvint()? as i64;
        i32::try_from(v).map_err(|_| anyhow::anyhow!("corrupt sstable: vint {v} out of 32-bit range"))
    }

    /// A non-negative length from an unsigned vint, checked against the bytes left.
    fn read_len(&mut self) -> Result<usize> {
        let n = self.uvint()?;
        if n > self.remaining() {
            bail!("corrupt sstable: length {n} with {} bytes left", self.remaining());
        }
        Ok(n as usize)
    }

    /// `ByteBufferUtil.readWithVIntLength`.
    fn vint_bytes(&mut self) -> Result<Bytes> {
        let n = self.read_len()?;
        self.bytes(n)
    }

    /// `ByteBufferUtil.readWithShortLength`.
    fn short_bytes(&mut self) -> Result<Bytes> {
        let n = self.be16()? as usize;
        self.bytes(n)
    }

    /// `DataInput.readUTF` (modified UTF-8; the names it holds here are ASCII).
    fn utf(&mut self) -> Result<String> {
        let b = self.short_bytes()?;
        Ok(String::from_utf8_lossy(&b).into_owned())
    }

    /// A count of elements each taking at least `min` bytes, checked against the bytes left.
    fn count(&mut self, n: u64, min: u64) -> Result<usize> {
        if n.saturating_mul(min.max(1)) > self.remaining() {
            bail!("corrupt sstable: {n} elements with {} bytes left", self.remaining());
        }
        Ok(n as usize)
    }
}

/// Reads from a byte buffer.
pub struct SliceIn {
    buf: Bytes,
    pos: usize,
}

impl SliceIn {
    pub fn new(buf: Bytes) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub fn rest(&self) -> &[u8] {
        &self.buf[self.pos..]
    }
}

impl In for SliceIn {
    fn remaining(&self) -> u64 {
        (self.buf.len() - self.pos) as u64
    }

    fn bytes(&mut self, n: usize) -> Result<Bytes> {
        if n > self.buf.len() - self.pos {
            bail!("corrupt sstable: {n} bytes wanted, {} left", self.buf.len() - self.pos);
        }
        let b = self.buf.slice(self.pos..self.pos + n);
        self.pos += n;
        Ok(b)
    }

    fn skip(&mut self, n: u64) -> Result<()> {
        if n > self.remaining() {
            bail!("corrupt sstable: skipping {n} bytes, {} left", self.remaining());
        }
        self.pos += n as usize;
        Ok(())
    }

    fn u8(&mut self) -> Result<u8> {
        let Some(b) = self.buf.get(self.pos) else { bail!("corrupt sstable: unexpected end of data") };
        self.pos += 1;
        Ok(*b)
    }
}

/// `VIntCoding.writeUnsignedVInt` (tests and fixture builders).
pub fn put_uvint(out: &mut Vec<u8>, v: u64) {
    let bits = 64 - v.leading_zeros() as usize;
    // 7 value bits per byte of size, up to 8 bytes; 9 bytes carry the full 64 bits.
    let size = if bits <= 7 { 1 } else { ((bits - 1) / 7 + 1).min(9) };
    if size == 1 {
        out.push(v as u8);
        return;
    }
    let extra = size - 1;
    let mut bytes = Vec::with_capacity(size);
    for i in (0..extra).rev() {
        bytes.push((v >> (i * 8)) as u8);
    }
    let first = if extra >= 8 { 0xff } else { (!(0xffu16 >> extra) as u8) | ((v >> (extra * 8)) as u8 & (0xffu16 >> (extra + 1)) as u8) };
    out.push(first);
    out.extend_from_slice(&bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vints_round_trip() {
        for v in [0u64, 1, 127, 128, 255, 16383, 16384, 1 << 20, (1 << 56) - 1, 1 << 56, u64::MAX, (-5i64) as u64, i32::MIN as i64 as u64] {
            let mut b = Vec::new();
            put_uvint(&mut b, v);
            let mut r = SliceIn::new(Bytes::from(b.clone()));
            assert_eq!(r.uvint().unwrap(), v, "{v} as {b:02x?}");
            assert!(r.is_empty());
        }
        // Cassandra's encodings: 128 → 0x80 0x80, a negative int → 9 bytes.
        let mut b = Vec::new();
        put_uvint(&mut b, 128);
        assert_eq!(b, [0x80, 0x80]);
        let mut r = SliceIn::new(Bytes::from(vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfb]));
        assert_eq!(r.uvint32().unwrap(), -5);
        let mut r = SliceIn::new(Bytes::from(vec![0xf0, 0x80, 0, 0, 0]));
        assert!(r.uvint32().is_err(), "2^31 doesn't fit an int");
    }

    #[test]
    fn lengths_are_bounded_by_the_input() {
        let mut r = SliceIn::new(Bytes::from_static(&[0x05, 1, 2]));
        assert!(r.vint_bytes().is_err());
        let mut r = SliceIn::new(Bytes::from_static(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]));
        assert!(r.read_len().is_err());
        let mut r = SliceIn::new(Bytes::from_static(&[0, 3, b'a', b'b', b'c', 9]));
        assert_eq!(r.utf().unwrap(), "abc");
        assert!(r.count(10, 1).is_err());
        assert!(r.be32().is_err());
    }
}
