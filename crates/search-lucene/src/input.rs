//! A bounds-checked cursor over a file's bytes: Lucene's `DataInput` (`readVInt`, `readVLong`,
//! `readString`, `readSetOfStrings`, `readMapOfStrings`, little-endian fixed widths since
//! Lucene 9) plus the big-endian reads of `CodecUtil.readBEInt`/`readBELong`. Every overrun is
//! [`Error::Corrupt`], never a panic, and collections read from a count are bounded by the
//! bytes left (each element takes at least one byte).

use crate::{Result, corrupt};

#[derive(Clone, Debug)]
pub struct Input<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Input<'a> {
    pub fn new(buf: &'a [u8]) -> Input<'a> {
        Input { buf, pos: 0 }
    }

    pub fn at(buf: &'a [u8], pos: usize) -> Input<'a> {
        Input { buf, pos: pos.min(buf.len()) }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return corrupt(format!("read past the end: [{n}] bytes wanted at [{}] of [{}]", self.pos, self.buf.len()));
        }
        let b = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(b)
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.bytes(N)?);
        Ok(a)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.array()?))
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    pub fn be_i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    pub fn be_i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.array()?))
    }

    pub fn id(&mut self) -> Result<crate::Id> {
        self.array()
    }

    /// `readVInt`: at most five bytes (a negative int takes all five).
    pub fn vint(&mut self) -> Result<i32> {
        let mut v: u32 = 0;
        for i in 0..5 {
            let b = self.u8()?;
            if i == 4 && b & 0xF0 != 0 {
                return corrupt("invalid vInt (too many bits)");
            }
            v |= ((b & 0x7F) as u32) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v as i32);
            }
        }
        corrupt("invalid vInt (too many bytes)")
    }

    /// A `readVInt` that must not be negative.
    pub fn vuint(&mut self) -> Result<u32> {
        let v = self.vint()?;
        if v < 0 {
            return corrupt(format!("invalid count [{v}]"));
        }
        Ok(v as u32)
    }

    /// `readVLong`: at most nine bytes, never negative.
    pub fn vlong(&mut self) -> Result<i64> {
        let mut v: u64 = 0;
        for i in 0..9 {
            let b = self.u8()?;
            v |= ((b & 0x7F) as u64) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v as i64);
            }
        }
        corrupt("invalid vLong (negative values disallowed)")
    }

    /// `readZInt`.
    pub fn zint(&mut self) -> Result<i32> {
        let v = self.vint()? as u32;
        Ok(((v >> 1) as i32) ^ -((v & 1) as i32))
    }

    /// `readString`: a vInt byte length, then UTF-8.
    pub fn string(&mut self) -> Result<&'a str> {
        let n = self.vuint()? as usize;
        let b = self.bytes(n)?;
        match std::str::from_utf8(b) {
            Ok(s) => Ok(s),
            Err(_) => corrupt("a string isn't UTF-8"),
        }
    }

    /// A vInt count of something taking at least `min` bytes each, checked against what's
    /// left: what to reserve for it.
    pub fn count(&mut self, min: usize) -> Result<usize> {
        let n = self.vuint()? as usize;
        if n.saturating_mul(min.max(1)) > self.remaining() {
            return corrupt(format!("a count of [{n}] doesn't fit the [{}] bytes left", self.remaining()));
        }
        Ok(n)
    }

    /// `readSetOfStrings`.
    pub fn strings(&mut self) -> Result<Vec<String>> {
        let n = self.count(1)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.string()?.to_string());
        }
        Ok(out)
    }

    /// `readMapOfStrings`, in file order.
    pub fn string_map(&mut self) -> Result<Vec<(String, String)>> {
        let n = self.count(2)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let k = self.string()?.to_string();
            out.push((k, self.string()?.to_string()));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_and_bounds() {
        let mut i = Input::new(&[0x7F, 0x80, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
        assert_eq!(i.vint().unwrap(), 127);
        assert_eq!(i.vint().unwrap(), 128);
        assert_eq!(i.vint().unwrap(), -1);
        assert!(i.u8().is_err());
        // A count larger than the bytes left is refused before anything is reserved.
        let mut i = Input::new(&[0xFF, 0xFF, 0xFF, 0xFF, 0x07, 1, b'a']);
        assert!(i.strings().is_err());
        let mut i = Input::new(&[3, b'a', b'b']);
        assert!(i.string().is_err());
        let mut i = Input::new(&[0xFF; 10]);
        assert!(i.vlong().is_err());
    }
}
