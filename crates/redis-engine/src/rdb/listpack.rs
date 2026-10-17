//! Redis' listpack wire format (`listpack.c`): what RDB stores for small lists, sets, hashes,
//! sorted sets and stream nodes. A writer that produces Redis' exact bytes, and a reader that
//! validates the whole structure before handing out entries.

use crate::util::parse_i64;

const HDR: usize = 6;
const EOF: u8 = 0xff;

/// Builds a listpack into a reusable buffer.
pub struct LpWriter<'a> {
    buf: &'a mut Vec<u8>,
    count: usize,
}

impl<'a> LpWriter<'a> {
    pub fn new(buf: &'a mut Vec<u8>) -> Self {
        buf.clear();
        buf.extend_from_slice(&[0; HDR]);
        LpWriter { buf, count: 0 }
    }

    /// `lpAppend`: strings that are canonical integers are stored as integers.
    #[inline]
    pub fn str(&mut self, s: &[u8]) {
        match parse_i64(s) {
            Some(v) => self.int(v),
            None => {
                let start = self.buf.len();
                let n = s.len();
                if n < 64 {
                    self.buf.push(0x80 | n as u8);
                } else if n < 4096 {
                    self.buf.extend_from_slice(&[0xe0 | (n >> 8) as u8, n as u8]);
                } else {
                    self.buf.push(0xf0);
                    self.buf.extend_from_slice(&(n as u32).to_le_bytes());
                }
                self.buf.extend_from_slice(s);
                self.backlen(self.buf.len() - start);
            }
        }
    }

    /// `lpAppendInteger`.
    pub fn int(&mut self, v: i64) {
        let start = self.buf.len();
        let b = &mut *self.buf;
        if (0..=127).contains(&v) {
            b.push(v as u8);
        } else if (-4096..=4095).contains(&v) {
            let u = if v < 0 { (1 << 13) + v } else { v } as u16;
            b.extend_from_slice(&[0xc0 | (u >> 8) as u8, u as u8]);
        } else if (-32768..=32767).contains(&v) {
            b.push(0xf1);
            b.extend_from_slice(&(v as i16).to_le_bytes());
        } else if (-8388608..=8388607).contains(&v) {
            b.push(0xf2);
            b.extend_from_slice(&(v as i32).to_le_bytes()[..3]);
        } else if (-2147483648..=2147483647).contains(&v) {
            b.push(0xf3);
            b.extend_from_slice(&(v as i32).to_le_bytes());
        } else {
            b.push(0xf4);
            b.extend_from_slice(&v.to_le_bytes());
        }
        self.backlen(self.buf.len() - start);
    }

    #[inline]
    fn backlen(&mut self, l: usize) {
        let l = l as u64;
        let b = &mut *self.buf;
        if l <= 127 {
            b.push(l as u8);
        } else if l <= 16383 {
            b.extend_from_slice(&[(l >> 7) as u8, (l & 127) as u8 | 128]);
        } else if l <= 2097151 {
            b.extend_from_slice(&[(l >> 14) as u8, ((l >> 7) & 127) as u8 | 128, (l & 127) as u8 | 128]);
        } else if l <= 268435455 {
            b.extend_from_slice(&[(l >> 21) as u8, ((l >> 14) & 127) as u8 | 128, ((l >> 7) & 127) as u8 | 128, (l & 127) as u8 | 128]);
        } else {
            b.extend_from_slice(&[
                (l >> 28) as u8,
                ((l >> 21) & 127) as u8 | 128,
                ((l >> 14) & 127) as u8 | 128,
                ((l >> 7) & 127) as u8 | 128,
                (l & 127) as u8 | 128,
            ]);
        }
        self.count += 1;
    }

    /// Terminate and return the listpack bytes.
    pub fn finish(self) -> &'a [u8] {
        self.buf.push(EOF);
        let total = self.buf.len() as u32;
        self.buf[..4].copy_from_slice(&total.to_le_bytes());
        let n = self.count.min(65535) as u16;
        self.buf[4..6].copy_from_slice(&n.to_le_bytes());
        &self.buf[..]
    }
}

/// A listpack entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Item<'a> {
    Int(i64),
    Str(&'a [u8]),
}

impl<'a> Item<'a> {
    /// The entry as a string (integers formatted into `buf`).
    #[inline]
    pub fn bytes<'b>(&self, buf: &'b mut itoa::Buffer) -> &'b [u8]
    where
        'a: 'b,
    {
        match *self {
            Item::Str(s) => s,
            Item::Int(v) => buf.format(v).as_bytes(),
        }
    }

    /// The entry as an integer, if it is stored as one.
    pub fn int(&self) -> Option<i64> {
        match *self {
            Item::Int(v) => Some(v),
            Item::Str(_) => None,
        }
    }

    /// The same entry with canonical integer strings as integers (for duplicate checks).
    #[inline]
    pub fn norm(self) -> Self {
        match self {
            Item::Str(s) => parse_i64(s).map_or(self, Item::Int),
            it => it,
        }
    }
}

/// A validated listpack.
#[derive(Clone, Copy)]
pub struct Listpack<'a> {
    body: &'a [u8],
    len: usize,
}

/// Decode the entry at `p`: (item, entry length without backlen).
#[inline]
fn decode(b: &[u8], p: usize) -> Option<(Item<'_>, usize)> {
    let e = *b.get(p)?;
    let get = |n: usize| b.get(p + 1..p + 1 + n);
    Some(if e & 0x80 == 0 {
        (Item::Int(e as i64), 1)
    } else if e & 0xc0 == 0x80 {
        let n = (e & 0x3f) as usize;
        (Item::Str(get(n)?), 1 + n)
    } else if e & 0xe0 == 0xc0 {
        let u = (((e & 0x1f) as u16) << 8) | *b.get(p + 1)? as u16;
        let v = if u >= 1 << 12 { u as i64 - (1 << 13) } else { u as i64 };
        (Item::Int(v), 2)
    } else if e & 0xf0 == 0xe0 {
        let n = (((e & 0x0f) as usize) << 8) | *b.get(p + 1)? as usize;
        (Item::Str(b.get(p + 2..p + 2 + n)?), 2 + n)
    } else {
        match e {
            0xf1 => (Item::Int(i16::from_le_bytes(get(2)?.try_into().ok()?) as i64), 3),
            0xf2 => {
                let x = get(3)?;
                (Item::Int((i32::from_le_bytes([0, x[0], x[1], x[2]]) >> 8) as i64), 4)
            }
            0xf3 => (Item::Int(i32::from_le_bytes(get(4)?.try_into().ok()?) as i64), 5),
            0xf4 => (Item::Int(i64::from_le_bytes(get(8)?.try_into().ok()?)), 9),
            0xf0 => {
                let n = u32::from_le_bytes(get(4)?.try_into().ok()?) as usize;
                (Item::Str(b.get(p + 5..p.checked_add(5 + n)?)?), 5 + n)
            }
            _ => return None,
        }
    })
}

#[inline]
fn backlen_size(l: usize) -> usize {
    match l {
        0..=127 => 1,
        128..=16383 => 2,
        16384..=2097151 => 3,
        2097152..=268435455 => 4,
        _ => 5,
    }
}

/// Read a backlen whose last byte is at `end - 1`.
fn read_backlen(b: &[u8], end: usize) -> Option<usize> {
    let mut v = 0usize;
    let mut shift = 0;
    let mut p = end;
    for _ in 0..5 {
        p = p.checked_sub(1)?;
        let x = b[p];
        v |= ((x & 127) as usize) << shift;
        if x & 128 == 0 {
            return Some(v);
        }
        shift += 7;
    }
    None
}

impl<'a> Listpack<'a> {
    /// Validate every entry (`lpValidateIntegrity` deep): header, encodings, backlens, count.
    pub fn parse(lp: &'a [u8]) -> Option<Self> {
        if lp.len() < HDR + 1 || u32::from_le_bytes(lp[..4].try_into().unwrap()) as usize != lp.len() || lp[lp.len() - 1] != EOF {
            return None;
        }
        let declared = u16::from_le_bytes([lp[4], lp[5]]) as usize;
        let body = &lp[..lp.len() - 1];
        let mut p = HDR;
        let mut n = 0;
        while p < body.len() {
            let (_, l) = decode(body, p)?;
            let end = p.checked_add(l)?.checked_add(backlen_size(l))?;
            if end > body.len() || read_backlen(body, end)? != l {
                return None;
            }
            p = end;
            n += 1;
        }
        if declared != 65535 && declared != n {
            return None;
        }
        Some(Listpack { body, len: n })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> Iter<'a> {
        Iter { body: self.body, p: HDR }
    }
}

pub struct Iter<'a> {
    body: &'a [u8],
    p: usize,
}

impl<'a> Iterator for Iter<'a> {
    type Item = Item<'a>;

    #[inline]
    fn next(&mut self) -> Option<Item<'a>> {
        if self.p >= self.body.len() {
            return None;
        }
        let (it, l) = decode(self.body, self.p)?;
        self.p += l + backlen_size(l);
        Some(it)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_every_encoding() {
        let mut buf = Vec::new();
        let mut w = LpWriter::new(&mut buf);
        let ints = [0i64, 127, 128, -1, -4096, 4095, 4096, -32768, 32767, 8388607, -8388608, i32::MAX as i64, i32::MIN as i64, i64::MAX, i64::MIN];
        for v in ints {
            w.int(v);
        }
        let long = vec![b'x'; 5000];
        let mid = vec![b'y'; 300];
        for s in [&b""[..], b"a", b"007", b"-0", &mid, &long] {
            w.str(s);
        }
        w.str(b"12");
        let lp = w.finish().to_vec();
        let p = Listpack::parse(&lp).unwrap();
        let got: Vec<Item> = p.iter().collect();
        let mut want: Vec<Item> = ints.iter().map(|&v| Item::Int(v)).collect();
        for s in [&b""[..], b"a", b"007", b"-0", &mid, &long] {
            want.push(Item::Str(s));
        }
        want.push(Item::Int(12));
        assert_eq!(got, want);
        // Corruptions are caught.
        for i in 0..lp.len() {
            let mut bad = lp.clone();
            bad[i] ^= 0x40;
            if let Some(p) = Listpack::parse(&bad) {
                assert_eq!(p.iter().count(), p.len());
            }
        }
        assert!(Listpack::parse(&lp[..lp.len() - 1]).is_none());
    }

    #[test]
    fn redis_bytes() {
        // RPUSH l a b 1 2 → DUMP's listpack.
        let mut buf = Vec::new();
        let mut w = LpWriter::new(&mut buf);
        for s in [b"a", b"b", b"1", b"2"] {
            w.str(s);
        }
        let want = [0x11, 0, 0, 0, 4, 0, 0x81, 0x61, 2, 0x81, 0x62, 2, 1, 1, 2, 1, 0xff];
        assert_eq!(w.finish(), want);
    }
}
