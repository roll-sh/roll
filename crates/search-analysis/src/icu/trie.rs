//! ICU binary data: the common header (`ICUBinary`) and code point tries (`CodePointTrie`,
//! `ucptrie`), read in place from big-endian data as ICU4J ships it (Unicode-3.0).

/// Skips ICU's data header (`ICUBinary.readHeader`): the data's format, its format version's
/// major number and the bytes after the header.
pub(crate) fn header(b: &[u8]) -> Option<([u8; 4], u8, &[u8])> {
    let size = u16::from_be_bytes([*b.first()?, *b.get(1)?]) as usize;
    if b.get(2..4)? != [0xda, 0x27] || *b.get(8)? != 1 {
        return None;
    }
    let format: [u8; 4] = b.get(12..16)?.try_into().ok()?;
    Some((format, *b.get(16)?, b.get(size..)?))
}

pub(crate) fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[2 * i], b[2 * i + 1]])
}

pub(crate) fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Width {
    W16,
    W32,
    W8,
}

const FAST_SHIFT: u32 = 6;
const FAST_DATA_MASK: u32 = (1 << FAST_SHIFT) - 1;
const SHIFT_3: u32 = 4;
const SHIFT_2: u32 = 5 + SHIFT_3;
const SHIFT_1: u32 = 5 + SHIFT_2;
const INDEX_2_MASK: u32 = (1 << (SHIFT_1 - SHIFT_2)) - 1;
const INDEX_3_MASK: u32 = (1 << (SHIFT_2 - SHIFT_3)) - 1;
const SMALL_DATA_MASK: u32 = (1 << SHIFT_3) - 1;
const BMP_INDEX_LENGTH: u32 = 0x10000 >> FAST_SHIFT;
const SMALL_INDEX_LENGTH: u32 = 0x1000 >> FAST_SHIFT;
const OMITTED_BMP_INDEX_1_LENGTH: u32 = 0x10000 >> SHIFT_1;
const ERROR_VALUE_NEG_DATA_OFFSET: u32 = 1;
const HIGH_VALUE_NEG_DATA_OFFSET: u32 = 2;

/// A serialized `CodePointTrie` of either type and any value width.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CpTrie<'a> {
    index: &'a [u8],
    data: &'a [u8],
    width: Width,
    data_length: u32,
    high_start: u32,
    fast: bool,
}

impl<'a> CpTrie<'a> {
    /// The trie at the start of `b` and its size in bytes.
    pub(crate) fn parse(b: &'a [u8]) -> Option<(CpTrie<'a>, usize)> {
        if b.get(0..4)? != b"Tri3" {
            return None;
        }
        let h = |i: usize| be16(&b[4..16], i) as u32;
        let options = h(0);
        let index_length = h(1) as usize;
        let mut data_length = h(2);
        let shifted_high_start = h(5);
        let fast = match (options >> 6) & 3 {
            0 => true,
            1 => false,
            _ => return None,
        };
        let width = match options & 7 {
            0 => Width::W16,
            1 => Width::W32,
            2 => Width::W8,
            _ => return None,
        };
        data_length |= (options & 0xf000) << 4;
        let index = b.get(16..16 + 2 * index_length)?;
        let data_bytes = data_length as usize
            * match width {
                Width::W16 => 2,
                Width::W32 => 4,
                Width::W8 => 1,
            };
        let start = 16 + 2 * index_length;
        let data = b.get(start..start + data_bytes)?;
        let trie = CpTrie { index, data, width, data_length, high_start: shifted_high_start << SHIFT_2, fast };
        Some((trie, start + data_bytes))
    }

    fn ix(&self, i: u32) -> u32 {
        be16(self.index, i as usize) as u32
    }

    fn value(&self, i: u32) -> u32 {
        match self.width {
            Width::W16 => be16(self.data, i as usize) as u32,
            Width::W32 => be32(self.data, i as usize),
            Width::W8 => self.data[i as usize] as u32,
        }
    }

    fn small_index(&self, c: u32) -> u32 {
        if c >= self.high_start {
            return self.data_length - HIGH_VALUE_NEG_DATA_OFFSET;
        }
        let mut i1 = c >> SHIFT_1;
        i1 += if self.fast { BMP_INDEX_LENGTH - OMITTED_BMP_INDEX_1_LENGTH } else { SMALL_INDEX_LENGTH };
        let mut i3_block = self.ix(self.ix(i1) + ((c >> SHIFT_2) & INDEX_2_MASK));
        let mut i3 = (c >> SHIFT_3) & INDEX_3_MASK;
        let data_block = if i3_block & 0x8000 == 0 {
            self.ix(i3_block + i3)
        } else {
            i3_block = (i3_block & 0x7fff) + (i3 & !7) + (i3 >> 3);
            i3 &= 7;
            let hi = (self.ix(i3_block) << (2 + 2 * i3)) & 0x30000;
            hi | self.ix(i3_block + 1 + i3)
        };
        data_block + (c & SMALL_DATA_MASK)
    }

    /// `CodePointTrie.get`.
    pub(crate) fn get(&self, c: u32) -> u32 {
        let fast_limit = if self.fast { 0xffff } else { 0xfff };
        let i = if c <= fast_limit {
            self.ix(c >> FAST_SHIFT) + (c & FAST_DATA_MASK)
        } else if c <= 0x10ffff {
            self.small_index(c)
        } else {
            self.data_length - ERROR_VALUE_NEG_DATA_OFFSET
        };
        self.value(i)
    }
}
