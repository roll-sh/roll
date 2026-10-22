//! Bounds-checked readers for the native-protocol value layouts.
//!
//! - collections (protocol v3+): `[i32 count]` then per element `[i32 len][bytes]`, maps
//!   alternating key and value; a negative length is a null element;
//! - tuples and UDTs: `[i32 len][bytes]` per field until the value ends, no count;
//! - variable-size vector elements: `[unsigned vint len][bytes]`, fixed-size ones packed;
//! - Cassandra's vints (`VIntCoding`): the number of leading one bits in the first byte is the
//!   number of extra bytes, the rest is big-endian; signed values are zigzag encoded.
//!
//! Nothing here allocates or trusts a count: every step checks the bytes actually present.

/// Reads a big-endian i32 at `at`.
pub(crate) fn i32_at(b: &[u8], at: usize) -> Option<i32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(i32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

pub(crate) fn i64_of(b: &[u8]) -> Option<i64> {
    let a: [u8; 8] = b.try_into().ok()?;
    Some(i64::from_be_bytes(a))
}

/// One step of an `[i32 len][bytes]` sequence.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Item<'a> {
    Value(&'a [u8]),
    Null,
}

/// Why a length-prefixed read failed, with the numbers ScyllaDB's messages quote.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Short {
    /// Fewer than 4 bytes left for a length.
    Len { got: usize },
    /// The length announced more bytes than remain.
    Body { requested: usize, got: usize },
}

/// Reads one `[i32 len][bytes]` item at `*pos`, advancing it.
pub(crate) fn read_item<'a>(b: &'a [u8], pos: &mut usize) -> Result<Item<'a>, Short> {
    let rest = b.len().saturating_sub(*pos);
    let Some(len) = i32_at(b, *pos) else {
        return Err(Short::Len { got: rest });
    };
    *pos += 4;
    if len < 0 {
        return Ok(Item::Null);
    }
    let len = len as usize;
    let rest = b.len() - *pos;
    if len > rest {
        return Err(Short::Body {
            requested: len,
            got: rest,
        });
    }
    let v = &b[*pos..*pos + len];
    *pos += len;
    Ok(Item::Value(v))
}

/// A serialised collection: the declared count and the element bytes after it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Collection<'a> {
    pub count: i32,
    pub body: &'a [u8],
}

impl<'a> Collection<'a> {
    pub fn parse(b: &'a [u8]) -> Option<Collection<'a>> {
        Some(Collection {
            count: i32_at(b, 0)?,
            body: &b[4..],
        })
    }

    /// The elements (map keys and values alternate), stopping at the first malformed one.
    /// Yields at most `count` (or `2 * count` for maps, via `per_entry`) items.
    pub fn items(self, per_entry: usize) -> Items<'a> {
        let n = if self.count < 0 {
            0
        } else {
            self.count as usize * per_entry
        };
        Items {
            buf: self.body,
            pos: 0,
            left: n,
        }
    }
}

/// Iterator over `[i32 len][bytes]` items; `None` elements are nulls. Ends early (and sets
/// `failed`) on truncation.
pub(crate) struct Items<'a> {
    buf: &'a [u8],
    pos: usize,
    left: usize,
}

impl<'a> Iterator for Items<'a> {
    type Item = Result<Option<&'a [u8]>, Short>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        match read_item(self.buf, &mut self.pos) {
            Ok(Item::Value(v)) => Some(Ok(Some(v))),
            Ok(Item::Null) => Some(Ok(None)),
            Err(e) => {
                self.left = 0;
                Some(Err(e))
            }
        }
    }
}

/// Fields of a tuple / UDT value: items until the bytes run out.
#[derive(Clone)]
pub(crate) struct Fields<'a> {
    buf: &'a [u8],
    pos: usize,
    done: bool,
}

pub(crate) fn fields(b: &[u8]) -> Fields<'_> {
    Fields {
        buf: b,
        pos: 0,
        done: false,
    }
}

impl Fields<'_> {
    /// No bytes left (or a malformed field was hit).
    pub fn is_exhausted(&self) -> bool {
        self.done || self.pos >= self.buf.len()
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<Option<&'a [u8]>, Short>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.pos >= self.buf.len() {
            return None;
        }
        match read_item(self.buf, &mut self.pos) {
            Ok(Item::Value(v)) => Some(Ok(Some(v))),
            Ok(Item::Null) => Some(Ok(None)),
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// Appends an `[i32 len][bytes]` item (`None` = null).
pub(crate) fn put_item(out: &mut Vec<u8>, v: Option<&[u8]>) {
    match v {
        Some(v) => {
            out.extend_from_slice(&(v.len() as i32).to_be_bytes());
            out.extend_from_slice(v);
        }
        None => out.extend_from_slice(&(-1i32).to_be_bytes()),
    }
}

/// Decodes an unsigned vint at `*pos`. `None` if truncated.
pub(crate) fn read_unsigned_vint(b: &[u8], pos: &mut usize) -> Option<u64> {
    let first = *b.get(*pos)?;
    let extra = first.leading_ones() as usize;
    if b.len() - *pos < 1 + extra {
        return None;
    }
    let mut v: u64 = if extra >= 8 {
        0
    } else {
        (first & (0xffu8 >> extra)) as u64
    };
    for i in 0..extra {
        v = (v << 8) | b[*pos + 1 + i] as u64;
    }
    *pos += 1 + extra;
    Some(v)
}

/// Decodes a zigzag signed vint.
pub(crate) fn read_vint(b: &[u8], pos: &mut usize) -> Option<i64> {
    let u = read_unsigned_vint(b, pos)?;
    Some(((u >> 1) as i64) ^ -((u & 1) as i64))
}

pub(crate) fn unsigned_vint_size(v: u64) -> usize {
    // Cassandra's `computeUnsignedVIntSize`.
    let magnitude = (v | 1).leading_zeros() as usize;
    (639 - magnitude * 9) >> 6
}

pub(crate) fn put_unsigned_vint(out: &mut Vec<u8>, v: u64) {
    let size = unsigned_vint_size(v);
    if size == 1 {
        out.push(v as u8);
        return;
    }
    let extra = size - 1;
    let mut bytes = [0u8; 9];
    for (i, byte) in bytes.iter_mut().enumerate().take(size).skip(1) {
        *byte = (v >> (8 * (size - 1 - i))) as u8;
    }
    let top = if extra >= 8 {
        0
    } else {
        (v >> (8 * extra)) as u8
    };
    bytes[0] = (!(0xffu16 >> extra) as u8) | top;
    out.extend_from_slice(&bytes[..size]);
}

pub(crate) fn put_vint(out: &mut Vec<u8>, v: i64) {
    put_unsigned_vint(out, ((v << 1) ^ (v >> 63)) as u64);
}

pub(crate) fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

/// Decodes hex digits (either case). `None` on an odd length or a non-hex character.
pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks_exact(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vint_round_trip_and_sizes() {
        for v in [
            0u64,
            1,
            127,
            128,
            255,
            16383,
            16384,
            1 << 20,
            u32::MAX as u64,
            1 << 55,
            (1 << 56) - 1,
            1 << 56,
            u64::MAX,
        ] {
            let mut out = Vec::new();
            put_unsigned_vint(&mut out, v);
            assert_eq!(out.len(), unsigned_vint_size(v), "{v}");
            let mut pos = 0;
            assert_eq!(read_unsigned_vint(&out, &mut pos), Some(v), "{v} {out:?}");
            assert_eq!(pos, out.len());
        }
        for v in [0i64, -1, 1, -64, 64, i32::MIN as i64, i64::MIN, i64::MAX] {
            let mut out = Vec::new();
            put_vint(&mut out, v);
            let mut pos = 0;
            assert_eq!(read_vint(&out, &mut pos), Some(v));
        }
        // Known encodings: 1 -> 01, 128 -> 80 80, zigzag(-1) = 1.
        let mut out = Vec::new();
        put_unsigned_vint(&mut out, 128);
        assert_eq!(out, [0x80, 0x80]);
        let mut out = Vec::new();
        put_vint(&mut out, -1);
        assert_eq!(out, [0x01]);
        // Truncated multi-byte vint.
        let mut pos = 0;
        assert_eq!(read_unsigned_vint(&[0xfe, 0, 0], &mut pos), None);
        assert_eq!(read_unsigned_vint(&[], &mut pos), None);
    }

    #[test]
    fn items_never_overrun() {
        let c = Collection::parse(&[
            0, 0, 0, 3, 0, 0, 0, 1, 7, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 9,
        ])
        .unwrap();
        let v: Vec<_> = c.items(1).collect();
        assert_eq!(
            v,
            vec![
                Ok(Some(&[7u8][..])),
                Ok(None),
                Err(Short::Body {
                    requested: 9,
                    got: 0
                })
            ]
        );
        // A huge count over few bytes stops at the first short read.
        let c = Collection::parse(&[0x7f, 0xff, 0xff, 0xff, 0, 0]).unwrap();
        let v: Vec<_> = c.items(2).collect();
        assert_eq!(v, vec![Err(Short::Len { got: 2 })]);
        assert!(Collection::parse(&[0, 0]).is_none());
        let f: Vec<_> = fields(&[0, 0, 0, 1, 5, 0xff, 0xff, 0xff, 0xff, 0]).collect();
        assert_eq!(
            f,
            vec![Ok(Some(&[5u8][..])), Ok(None), Err(Short::Len { got: 1 })]
        );
    }

    #[test]
    fn hex_helpers() {
        assert_eq!(hex(&[0xca, 0xfe, 0x01]), "cafe01");
        assert_eq!(unhex("CAfe01"), Some(vec![0xca, 0xfe, 1]));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex(""), Some(vec![]));
    }
}
