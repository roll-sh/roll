//! Byte-level encoding primitives shared by every structure.
//!
//! Writers append to a `Vec<u8>` through the [`Out`] extension trait; readers decode through
//! [`In`], a bounds-checked cursor over a borrowed slice that turns every overrun into
//! [`Error::Corrupt`] instead of a panic. Integers are little-endian; variable-length integers are
//! LEB128 (7 bits per byte, low groups first), and signed values are zigzag-encoded first so small
//! magnitudes stay short.

use crate::error::{Error, Result, corrupt};

/// Appending encoders for `Vec<u8>`.
/// Where streamed section bytes go: a `Vec` or an open section of a
/// [`ContainerWriter`](crate::container::ContainerWriter).
pub trait ByteSink {
    fn put_all(&mut self, b: &[u8]) -> crate::error::Result<()>;
}

impl ByteSink for Vec<u8> {
    fn put_all(&mut self, b: &[u8]) -> crate::error::Result<()> {
        self.extend_from_slice(b);
        Ok(())
    }
}

/// An [`Out`] that streams into a [`ByteSink`] through a small buffer, so encoders written
/// against `Out` can emit a large section straight into the container (or a spill file) instead
/// of building it in memory. `Out` is infallible; the first sink error is kept and returned by
/// [`finish`](Self::finish), and later writes are dropped.
pub struct SinkOut<'a> {
    sink: &'a mut dyn ByteSink,
    buf: Vec<u8>,
    flushed: u64,
    err: Option<Error>,
}

const SINK_BUF: usize = 64 << 10;

impl<'a> SinkOut<'a> {
    pub fn new(sink: &'a mut dyn ByteSink) -> SinkOut<'a> {
        SinkOut { sink, buf: Vec::with_capacity(SINK_BUF + 64), flushed: 0, err: None }
    }

    /// Bytes written so far.
    pub fn len(&self) -> u64 {
        self.flushed + self.buf.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    fn spill(&mut self) {
        if self.buf.len() >= SINK_BUF {
            self.flush_buf();
        }
    }

    fn flush_buf(&mut self) {
        if self.err.is_none()
            && !self.buf.is_empty()
            && let Err(e) = self.sink.put_all(&self.buf)
        {
            self.err = Some(e);
        }
        self.flushed += self.buf.len() as u64;
        self.buf.clear();
    }

    /// Flushes the buffer and reports the first error.
    pub fn finish(mut self) -> Result<u64> {
        self.flush_buf();
        match self.err.take() {
            Some(e) => Err(e),
            None => Ok(self.flushed),
        }
    }
}

impl Out for SinkOut<'_> {
    #[inline]
    fn put_u8(&mut self, v: u8) {
        self.buf.push(v);
        self.spill();
    }
    #[inline]
    fn put_u16(&mut self, v: u16) {
        self.put_bytes(&v.to_le_bytes());
    }
    #[inline]
    fn put_u32(&mut self, v: u32) {
        self.put_bytes(&v.to_le_bytes());
    }
    #[inline]
    fn put_u64(&mut self, v: u64) {
        self.put_bytes(&v.to_le_bytes());
    }
    #[inline]
    fn put_i64(&mut self, v: i64) {
        self.put_bytes(&v.to_le_bytes());
    }
    #[inline]
    fn put_f32(&mut self, v: f32) {
        self.put_bytes(&v.to_le_bytes());
    }
    #[inline]
    fn put_f64(&mut self, v: f64) {
        self.put_bytes(&v.to_le_bytes());
    }
    #[inline]
    fn put_vu32(&mut self, v: u32) {
        self.buf.put_vu32(v);
        self.spill();
    }
    #[inline]
    fn put_vu64(&mut self, v: u64) {
        self.buf.put_vu64(v);
        self.spill();
    }
    #[inline]
    fn put_vi64(&mut self, v: i64) {
        self.buf.put_vi64(v);
        self.spill();
    }
    #[inline]
    fn put_bytes(&mut self, b: &[u8]) {
        if b.len() >= SINK_BUF {
            self.flush_buf();
            if self.err.is_none()
                && let Err(e) = self.sink.put_all(b)
            {
                self.err = Some(e);
            }
            self.flushed += b.len() as u64;
            return;
        }
        self.buf.extend_from_slice(b);
        self.spill();
    }
    #[inline]
    fn put_lbytes(&mut self, b: &[u8]) {
        self.put_vu64(b.len() as u64);
        self.put_bytes(b);
    }
    fn put_str(&mut self, s: &str) {
        self.put_lbytes(s.as_bytes());
    }
    fn align_to(&mut self, align: usize) {
        while !self.len().is_multiple_of(align as u64) {
            self.put_u8(0);
        }
    }
    fn out_len(&self) -> u64 {
        self.len()
    }
}

pub trait Out {
    /// Bytes written so far (offsets of append-only encoders).
    fn out_len(&self) -> u64;
    fn put_u8(&mut self, v: u8);
    fn put_u16(&mut self, v: u16);
    fn put_u32(&mut self, v: u32);
    fn put_u64(&mut self, v: u64);
    fn put_i64(&mut self, v: i64);
    fn put_f32(&mut self, v: f32);
    fn put_f64(&mut self, v: f64);
    fn put_vu32(&mut self, v: u32);
    fn put_vu64(&mut self, v: u64);
    fn put_vi64(&mut self, v: i64);
    fn put_bytes(&mut self, b: &[u8]);
    /// Length-prefixed (varint) bytes.
    fn put_lbytes(&mut self, b: &[u8]);
    fn put_str(&mut self, s: &str);
    /// Pads with zeros up to a multiple of `align`.
    fn align_to(&mut self, align: usize);
}

impl Out for Vec<u8> {
    #[inline]
    fn out_len(&self) -> u64 {
        self.len() as u64
    }
    #[inline]
    fn put_u8(&mut self, v: u8) {
        self.push(v);
    }
    #[inline]
    fn put_u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn put_u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn put_u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn put_i64(&mut self, v: i64) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn put_f32(&mut self, v: f32) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn put_f64(&mut self, v: f64) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    fn put_vu32(&mut self, v: u32) {
        self.put_vu64(v as u64);
    }
    #[inline]
    fn put_vu64(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.push(v as u8);
    }
    #[inline]
    fn put_vi64(&mut self, v: i64) {
        self.put_vu64(zigzag(v));
    }
    #[inline]
    fn put_bytes(&mut self, b: &[u8]) {
        self.extend_from_slice(b);
    }
    #[inline]
    fn put_lbytes(&mut self, b: &[u8]) {
        self.put_vu64(b.len() as u64);
        self.extend_from_slice(b);
    }
    #[inline]
    fn put_str(&mut self, s: &str) {
        self.put_lbytes(s.as_bytes());
    }
    fn align_to(&mut self, align: usize) {
        while !self.len().is_multiple_of(align) {
            self.push(0);
        }
    }
}

#[inline]
pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
pub fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Encoded length of a varint.
#[inline]
pub fn vlen(v: u64) -> usize {
    let bits = 64 - (v | 1).leading_zeros() as usize;
    bits.div_ceil(7)
}

/// A bounds-checked decoding cursor.
#[derive(Clone, Copy)]
pub struct In<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> In<'a> {
    #[inline]
    pub fn new(buf: &'a [u8]) -> In<'a> {
        In { buf, pos: 0 }
    }

    #[inline]
    pub fn at(buf: &'a [u8], pos: usize) -> In<'a> {
        In { buf, pos }
    }

    #[inline]
    pub fn pos(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
    }

    #[inline]
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    #[inline]
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos.min(self.buf.len())..]
    }

    #[inline]
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(eof)?;
        let s = self.buf.get(self.pos..end).ok_or_else(eof)?;
        self.pos = end;
        Ok(s)
    }

    #[inline]
    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }

    #[inline]
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let b = self.bytes(N)?;
        Ok(b.try_into().expect("length checked"))
    }

    #[inline]
    pub fn u8(&mut self) -> Result<u8> {
        let b = *self.buf.get(self.pos).ok_or_else(eof)?;
        self.pos += 1;
        Ok(b)
    }

    #[inline]
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    #[inline]
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    #[inline]
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    #[inline]
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    #[inline]
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.array()?))
    }

    #[inline]
    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.array()?))
    }

    #[inline]
    pub fn vu64(&mut self) -> Result<u64> {
        let mut v = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.u8()?;
            if shift == 63 && b > 1 {
                return Err(corrupt("varint overflow"));
            }
            v |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                return Err(corrupt("varint too long"));
            }
        }
    }

    #[inline]
    pub fn vu32(&mut self) -> Result<u32> {
        let v = self.vu64()?;
        u32::try_from(v).map_err(|_| corrupt("varint exceeds u32"))
    }

    /// A varint used as a length or count, bounded by `max`.
    #[inline]
    pub fn vlen(&mut self, max: usize) -> Result<usize> {
        let v = self.vu64()?;
        if v > max as u64 {
            return Err(corrupt(format!("length {v} exceeds bound {max}")));
        }
        Ok(v as usize)
    }

    #[inline]
    pub fn vi64(&mut self) -> Result<i64> {
        Ok(unzigzag(self.vu64()?))
    }

    /// Length-prefixed bytes; the length is bounded by the bytes actually present.
    #[inline]
    pub fn lbytes(&mut self) -> Result<&'a [u8]> {
        let n = self.vlen(self.remaining())?;
        self.bytes(n)
    }

    pub fn str(&mut self) -> Result<&'a str> {
        std::str::from_utf8(self.lbytes()?).map_err(|_| corrupt("invalid utf-8"))
    }
}

fn eof() -> Error {
    corrupt("unexpected end of data")
}

/// Unchecked-length varint decode used by hot loops over validated data. Reads past the end
/// return 0 bits rather than panicking, so corrupt input produces garbage, never UB or a panic.
#[inline(always)]
pub fn read_vu32(buf: &[u8], pos: &mut usize) -> u32 {
    let mut v = 0u32;
    let mut shift = 0;
    loop {
        let b = buf.get(*pos).copied().unwrap_or(0);
        *pos += 1;
        v |= ((b & 0x7f) as u32).wrapping_shl(shift);
        if b < 0x80 || shift >= 28 {
            return v;
        }
        shift += 7;
    }
}

/// See [`read_vu32`].
#[inline(always)]
pub fn read_vu64(buf: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = buf.get(*pos).copied().unwrap_or(0);
        *pos += 1;
        v |= ((b & 0x7f) as u64).wrapping_shl(shift);
        if b < 0x80 || shift >= 63 {
            return v;
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        let vals = [
            0u64,
            1,
            127,
            128,
            255,
            16383,
            16384,
            u32::MAX as u64,
            u64::MAX - 1,
            u64::MAX,
        ];
        let mut out = Vec::new();
        for &v in &vals {
            out.put_vu64(v);
            assert_eq!(vlen(v), {
                let mut t = Vec::new();
                t.put_vu64(v);
                t.len()
            });
        }
        let mut inp = In::new(&out);
        let mut pos = 0;
        for &v in &vals {
            assert_eq!(inp.vu64().unwrap(), v);
            assert_eq!(read_vu64(&out, &mut pos), v);
        }
        assert!(inp.is_empty());
        assert!(inp.vu64().is_err());
    }

    #[test]
    fn zigzag_round_trips() {
        for v in [0i64, 1, -1, 63, -64, i64::MAX, i64::MIN] {
            assert_eq!(unzigzag(zigzag(v)), v);
            let mut out = Vec::new();
            out.put_vi64(v);
            assert_eq!(In::new(&out).vi64().unwrap(), v);
        }
    }

    #[test]
    fn overruns_are_errors() {
        let mut inp = In::new(&[0x80, 0x80]);
        assert!(inp.vu64().is_err());
        let mut inp = In::new(&[5, 1, 2]);
        assert!(inp.lbytes().is_err());
        let mut inp = In::new(&[0xff; 11]);
        assert!(inp.vu64().is_err());
        let mut inp = In::new(&[1, 2]);
        assert!(inp.u32().is_err());
        assert!(inp.bytes(usize::MAX).is_err());
    }
}
