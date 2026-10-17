//! The integer encodings inverted index records use: `qint` (up to four u32s behind one length
//! byte), the `varint` crate's continuation-bit varints and its `VectorWriter` (VVW) of
//! delta-encoded term offsets.

/// Bytes `qint_encode_stepwise` writes for `v`: its little-endian bytes without high zeros, at
/// least one.
const fn qint_len(v: u32) -> usize {
    match v {
        0..=0xff => 1,
        0x100..=0xffff => 2,
        0x1_0000..=0xff_ffff => 3,
        _ => 4,
    }
}

/// `qint_encode`: a leading byte holding each value's byte count minus one in 2-bit fields
/// (first value in the low bits), then every value's little-endian bytes. `values` holds 2 to 4
/// integers. Returns the bytes written.
pub fn qint_encode(out: &mut Vec<u8>, values: &[u32]) -> usize {
    debug_assert!((2..=4).contains(&values.len()));
    let start = out.len();
    out.push(0);
    let mut leading = 0u8;
    for (i, &v) in values.iter().enumerate() {
        let n = qint_len(v);
        out.extend_from_slice(&v.to_le_bytes()[..n]);
        leading |= ((n - 1) as u8) << (i * 2);
    }
    out[start] = leading;
    out.len() - start
}

/// `qint_decode`: `N` values from `data[*pos..]`, advancing `pos`; `None` when truncated.
pub fn qint_decode<const N: usize>(data: &[u8], pos: &mut usize) -> Option<[u32; N]> {
    let leading = *data.get(*pos)?;
    let mut p = *pos + 1;
    let mut out = [0u32; N];
    for (i, slot) in out.iter_mut().enumerate() {
        let n = ((leading >> (i * 2)) & 3) as usize + 1;
        let mut b = [0u8; 4];
        b[..n].copy_from_slice(data.get(p..p + n)?);
        *slot = u32::from_le_bytes(b);
        p += n;
    }
    *pos = p;
    Some(out)
}

/// `VarintEncode::write_as_varint`: big-endian 7-bit groups, continuation bit on every byte but
/// the last, each extra group storing its value minus one. The same bytes for u32, u64 and u128
/// (field masks of the `*Wide` encoders). Returns the bytes written.
pub fn varint_encode(out: &mut Vec<u8>, mut v: u128) -> usize {
    let mut buf = [0u8; 19];
    let mut pos = buf.len() - 1;
    buf[pos] = (v & 0x7f) as u8;
    v >>= 7;
    while v != 0 {
        pos -= 1;
        v -= 1;
        buf[pos] = 0x80 | (v & 0x7f) as u8;
        v >>= 7;
    }
    out.extend_from_slice(&buf[pos..]);
    buf.len() - pos
}

/// `VarintEncode::read_as_varint`: decodes from `data[*pos..]`, advancing `pos`; `None` when
/// truncated.
pub fn varint_decode(data: &[u8], pos: &mut usize) -> Option<u128> {
    let mut p = *pos;
    let mut c = *data.get(p)?;
    p += 1;
    let mut v = (c & 0x7f) as u128;
    while c & 0x80 != 0 {
        v = v.wrapping_add(1);
        c = *data.get(p)?;
        p += 1;
        v = (v << 7) | (c & 0x7f) as u128;
    }
    *pos = p;
    Some(v)
}

/// `varint_decode` read as the u32 the narrow decoders use.
pub fn varint_decode_u32(data: &[u8], pos: &mut usize) -> Option<u32> {
    varint_decode(data, pos).map(|v| v as u32)
}

/// `VarintVectorWriter` (`VVW_Write`): term positions as varint deltas from the previous one
/// (wrapping when a position goes backwards).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OffsetsWriter {
    buf: Vec<u8>,
    count: usize,
    last: u32,
}

impl OffsetsWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// `VVW_Write`: returns the bytes the delta took.
    pub fn write(&mut self, pos: u32) -> usize {
        let n = varint_encode(&mut self.buf, pos.wrapping_sub(self.last) as u128);
        self.count += 1;
        self.last = pos;
        n
    }

    /// `VVW_GetByteData`.
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    /// `VVW_GetByteLength`.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// `VVW_GetCount`.
    pub fn count(&self) -> usize {
        self.count
    }

    /// `VVW_Reset`.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.count = 0;
        self.last = 0;
    }
}

/// Decodes a term offsets vector back into positions (`RSOffsetVector_Iterate`).
#[derive(Clone, Debug)]
pub struct OffsetsIter<'a> {
    data: &'a [u8],
    pos: usize,
    last: u32,
}

impl<'a> OffsetsIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, last: 0 }
    }
}

impl Iterator for OffsetsIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let d = varint_decode_u32(self.data, &mut self.pos)?;
        self.last = self.last.wrapping_add(d);
        Some(self.last)
    }
}

/// Positions stored in an offsets vector.
pub fn offsets(data: &[u8]) -> OffsetsIter<'_> {
    OffsetsIter::new(data)
}
