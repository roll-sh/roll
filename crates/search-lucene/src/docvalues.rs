//! Numeric doc values (`Lucene90DocValuesProducer`, formats 0 to 2): what a restore reads of
//! them is `__soft_deletes` (Elasticsearch's deletes and replaced versions) and `_seq_no`,
//! `_version` and `_primary_term`.
//!
//! `.dvm` lists every field's entry; entries of other types are walked past (their metadata
//! only, `DirectMonotonicReader.loadMeta` included), so the numeric entries can be found. A
//! numeric entry's values live in `.dvd` at `[values_offset, +values_length)`, which documents
//! have them at `[docs_with_field_offset, +length)` (`IndexedDISI`: `-1` every document, `-2`
//! none). [`Numeric`] reads values in document order from those two ranges, which the caller
//! copied out of `.dvd` ([`NumericEntry::ranges`]): `DirectReader` bit packing, a table, a
//! GCD and minimum, or blocks of varying bits per value (`VaryingBPVReader`).

use crate::codec;
use crate::fields::{DocValues, FieldInfos};
use crate::input::Input;
use crate::{Id, Result, corrupt};

pub const META_CODEC: &str = "Lucene90DocValuesMetadata";
pub const DATA_CODEC: &str = "Lucene90DocValuesData";

/// A numeric field's entry.
#[derive(Clone, Debug, PartialEq)]
pub struct NumericEntry {
    pub docs_with_field_offset: i64,
    pub docs_with_field_length: i64,
    pub dense_rank_power: i8,
    /// Documents with a value.
    pub num_values: i64,
    pub table: Option<Vec<i64>>,
    /// `-1`: one block (`bits_per_value` for every value); else values in blocks of
    /// `1 << block_shift`, each with its own bits per value.
    pub block_shift: i32,
    pub bits_per_value: u8,
    pub min_value: i64,
    pub gcd: i64,
    pub values_offset: i64,
    pub values_length: i64,
}

impl NumericEntry {
    /// The `.dvd` ranges [`Numeric`] needs: which documents have values (when not all or
    /// none), then the values.
    pub fn ranges(&self) -> (Option<(u64, u64)>, (u64, u64)) {
        let docs = (self.docs_with_field_offset >= 0).then_some((self.docs_with_field_offset as u64, self.docs_with_field_length.max(0) as u64));
        (docs, (self.values_offset.max(0) as u64, self.values_length.max(0) as u64))
    }
}

/// `.dvm`'s numeric entries by field number (footer checked first).
pub fn read_meta(file: &[u8], id: &Id, suffix: &str, fields: &FieldInfos) -> Result<Vec<(u32, NumericEntry)>> {
    codec::check_footer(file)?;
    let mut inp = Input::new(codec::body(file)?);
    let version = codec::check_index_header(&mut inp, META_CODEC, 0, 2, id, suffix)?;
    let mut out = Vec::new();
    loop {
        let number = inp.i32()?;
        if number == -1 {
            break;
        }
        let Some(info) = fields.by_number(number as u32).filter(|_| number >= 0) else { return corrupt(format!("Invalid field number: {number}")) };
        let ty = inp.u8()?;
        if info.dv_skipper {
            // offset, length, max, min (longs), doc count, max doc (ints), format 2's max
            // value count.
            inp.skip(8 * 4 + 4 * 2 + if version >= 2 { 4 } else { 0 })?;
        }
        match ty {
            0 => {
                let e = read_numeric(&mut inp)?;
                if info.doc_values != DocValues::Numeric {
                    return corrupt(format!("field [{}] has numeric doc values but isn't numeric", info.name));
                }
                if out.iter().any(|(n, _)| *n == info.number) {
                    return corrupt(format!("field [{}] listed twice", info.name));
                }
                out.push((info.number, e));
            }
            1 => skip_binary(&mut inp)?,
            2 => skip_sorted(&mut inp)?,
            3 => match inp.u8()? {
                0 => skip_sorted(&mut inp)?,
                1 => {
                    skip_sorted_numeric(&mut inp)?;
                    skip_terms_dict(&mut inp)?;
                }
                m => return corrupt(format!("Invalid multiValued flag: {m}")),
            },
            4 => skip_sorted_numeric(&mut inp)?,
            t => return corrupt(format!("invalid type: {t}")),
        }
    }
    if inp.remaining() != 0 {
        return corrupt("trailing bytes after the doc values metadata");
    }
    Ok(out)
}

fn read_numeric(inp: &mut Input) -> Result<NumericEntry> {
    let docs_with_field_offset = inp.i64()?;
    let docs_with_field_length = inp.i64()?;
    let _jump_table_entry_count = inp.i16()?;
    let dense_rank_power = inp.u8()? as i8;
    let num_values = inp.i64()?;
    let table_size = inp.i32()?;
    if table_size > 256 {
        return corrupt(format!("invalid table size: {table_size}"));
    }
    let table = if table_size >= 0 {
        let mut t = Vec::with_capacity(table_size as usize);
        for _ in 0..table_size {
            t.push(inp.i64()?);
        }
        Some(t)
    } else {
        None
    };
    let block_shift = if table_size < -1 { -2 - table_size } else { -1 };
    let bits_per_value = inp.u8()?;
    let min_value = inp.i64()?;
    let gcd = inp.i64()?;
    let values_offset = inp.i64()?;
    let values_length = inp.i64()?;
    let _value_jump_table_offset = inp.i64()?;
    if num_values < 0 || !(-1..=30).contains(&block_shift) || values_length < 0 || (docs_with_field_offset >= 0 && docs_with_field_length < 0) {
        return corrupt("invalid numeric doc values entry");
    }
    Ok(NumericEntry { docs_with_field_offset, docs_with_field_length, dense_rank_power, num_values, table, block_shift, bits_per_value, min_value, gcd, values_offset, values_length })
}

/// `DirectMonotonicReader.loadMeta` of `values` values in blocks of `1 << shift`: 21 bytes a
/// block.
fn skip_monotonic(inp: &mut Input, values: i64, shift: i32) -> Result<()> {
    if values < 0 || !(2..=30).contains(&shift) {
        return corrupt(format!("invalid monotonic block shift [{shift}] or count [{values}]"));
    }
    let blocks = (values as u64 >> shift) + u64::from(values as u64 & ((1u64 << shift) - 1) != 0);
    let bytes = blocks.checked_mul(21).filter(|b| *b <= inp.remaining() as u64).ok_or_else(|| crate::Error::Corrupt("monotonic metadata past the end".into()))?;
    inp.skip(bytes as usize)
}

fn skip_numeric(inp: &mut Input) -> Result<i64> {
    let e = read_numeric(inp)?;
    Ok(e.num_values)
}

fn skip_binary(inp: &mut Input) -> Result<()> {
    inp.skip(8 * 4 + 2 + 1)?;
    let docs = inp.i32()?;
    let (min, max) = (inp.i32()?, inp.i32()?);
    if min < max {
        inp.i64()?;
        let shift = inp.vint()?;
        skip_monotonic(inp, docs as i64 + 1, shift)?;
        inp.i64()?;
    }
    Ok(())
}

fn skip_sorted(inp: &mut Input) -> Result<()> {
    skip_numeric(inp)?;
    skip_terms_dict(inp)
}

fn skip_sorted_numeric(inp: &mut Input) -> Result<()> {
    let values = skip_numeric(inp)?;
    let docs = inp.i32()?;
    if docs as i64 != values {
        inp.i64()?;
        let shift = inp.vint()?;
        skip_monotonic(inp, docs as i64 + 1, shift)?;
        inp.i64()?;
    }
    Ok(())
}

fn skip_terms_dict(inp: &mut Input) -> Result<()> {
    let size = inp.vlong()?;
    let shift = inp.i32()?;
    let Some(blocks) = size.checked_add(63) else { return corrupt(format!("invalid terms dict size [{size}]")) };
    skip_monotonic(inp, blocks >> 6, shift)?;
    inp.skip(4 + 4 + 8 * 4)?;
    let index_shift = inp.i32()?;
    if !(0..=62).contains(&index_shift) {
        return corrupt(format!("invalid terms dict index shift [{index_shift}]"));
    }
    let Some(index_size) = size.checked_add((1i64 << index_shift) - 1).map(|n| (n >> index_shift) + 1) else {
        return corrupt(format!("invalid terms dict size [{size}]"));
    };
    skip_monotonic(inp, index_size, shift)?;
    inp.skip(8 * 4)
}

/// `DirectReader`: value `index` of `bpv` bits in a little-endian bit stream.
fn direct(data: &[u8], bpv: u8, index: u64) -> Result<u64> {
    if !matches!(bpv, 1 | 2 | 4 | 8 | 12 | 16 | 20 | 24 | 28 | 32 | 40 | 48 | 56 | 64) {
        return corrupt(format!("unsupported bitsPerValue {bpv}"));
    }
    let bit = index.checked_mul(bpv as u64).ok_or_else(|| crate::Error::Corrupt("value index overflows".into()))?;
    let (byte, shift) = ((bit / 8) as usize, (bit % 8) as u32);
    let need = (shift as usize + bpv as usize).div_ceil(8);
    let Some(b) = byte.checked_add(need).and_then(|end| data.get(byte..end)) else { return corrupt("doc values read past their data") };
    let mut acc: u128 = 0;
    for (i, &x) in b.iter().enumerate() {
        acc |= (x as u128) << (8 * i);
    }
    let mask: u128 = if bpv == 64 { u64::MAX as u128 } else { (1u128 << bpv) - 1 };
    Ok(((acc >> shift) & mask) as u64)
}

/// An `IndexedDISI` read in order: `(doc, index)` of each document with a value.
pub struct Disi<'a> {
    inp: Input<'a>,
    rank_bytes: usize,
    /// Values still to come.
    left: u64,
    index: u64,
    block: u32,
    kind: Block,
    max_doc: u32,
    /// The last block read and document given: both only grow, so a file can't make the
    /// iteration longer than the segment (a repeated all-docs block costs four bytes).
    last_block: Option<u32>,
    last_doc: Option<u32>,
}

enum Block {
    None,
    Sparse { left: u32 },
    Dense { word: usize, bits: u64, words: Box<[u64; 1024]> },
    All { next: u32 },
}

impl<'a> Disi<'a> {
    /// `data`: the `IndexedDISI` bytes of `count` documents of a segment of `max_doc`.
    pub fn new(data: &'a [u8], dense_rank_power: i8, count: u64, max_doc: u32) -> Result<Disi<'a>> {
        let rank_bytes = match dense_rank_power {
            -1 => 0,
            7..=15 => 1024 >> (dense_rank_power - 7),
            p => return corrupt(format!("invalid dense rank power {p}")),
        };
        if count > max_doc as u64 {
            return corrupt(format!("[{count}] documents with values in a segment of [{max_doc}]"));
        }
        Ok(Disi { inp: Input::new(data), rank_bytes, left: count, index: 0, block: 0, kind: Block::None, max_doc, last_block: None, last_doc: None })
    }

    pub fn next_doc(&mut self) -> Result<Option<(u32, u64)>> {
        loop {
            if self.left == 0 {
                return Ok(None);
            }
            let doc = match &mut self.kind {
                Block::None => {
                    self.block = (self.inp.u16()? as u32) << 16;
                    if self.last_block.is_some_and(|b| self.block <= b) || self.block >= self.max_doc {
                        return corrupt(format!("doc values block [{}] out of order or past the segment", self.block >> 16));
                    }
                    self.last_block = Some(self.block);
                    let n = self.inp.u16()? as u32 + 1;
                    self.kind = if n <= 4095 {
                        Block::Sparse { left: n }
                    } else if n == 65536 {
                        Block::All { next: 0 }
                    } else {
                        self.inp.skip(self.rank_bytes)?;
                        let mut words = Box::new([0u64; 1024]);
                        for w in words.iter_mut() {
                            *w = self.inp.i64()? as u64;
                        }
                        if words.iter().map(|w| w.count_ones()).sum::<u32>() != n {
                            return corrupt("a dense doc values block's bits don't match its count");
                        }
                        Block::Dense { word: 0, bits: words[0], words }
                    };
                    continue;
                }
                Block::Sparse { left } => {
                    if *left == 0 {
                        self.kind = Block::None;
                        continue;
                    }
                    *left -= 1;
                    self.inp.u16()? as u32
                }
                Block::All { next } => {
                    if *next == 65536 {
                        self.kind = Block::None;
                        continue;
                    }
                    *next += 1;
                    *next - 1
                }
                Block::Dense { word, bits, words } => {
                    while *bits == 0 {
                        *word += 1;
                        if *word == 1024 {
                            break;
                        }
                        *bits = words[*word];
                    }
                    if *word == 1024 {
                        self.kind = Block::None;
                        continue;
                    }
                    let b = bits.trailing_zeros();
                    *bits &= *bits - 1;
                    (*word as u32) * 64 + b
                }
            };
            let doc = self.block | doc;
            if doc >= self.max_doc || self.last_doc.is_some_and(|l| doc <= l) {
                return corrupt(format!("doc values document [{doc}] out of order or past the segment of [{}]", self.max_doc));
            }
            self.last_doc = Some(doc);
            self.left -= 1;
            let index = self.index;
            self.index += 1;
            return Ok(Some((doc, index)));
        }
    }
}

/// A numeric field's values in document order ([`Numeric::advance`]).
pub struct Numeric<'a> {
    entry: &'a NumericEntry,
    max_doc: u32,
    docs: Option<Disi<'a>>,
    /// The next document [`Numeric::docs`] has, and its index.
    pending: Option<(u32, u64)>,
    values: &'a [u8],
    /// Varying blocks: the block read last, its start and bits per value, and where the next
    /// one starts.
    block: Option<(u64, usize, u8, i64)>,
    next_block: usize,
    blocks_read: u64,
}

impl<'a> Numeric<'a> {
    /// `docs` and `values`: the ranges [`NumericEntry::ranges`] names.
    pub fn new(entry: &'a NumericEntry, max_doc: u32, docs: Option<&'a [u8]>, values: &'a [u8]) -> Result<Numeric<'a>> {
        let disi = match (entry.docs_with_field_offset, docs) {
            (-2 | -1, _) => None,
            (_, Some(d)) => Some(Disi::new(d, entry.dense_rank_power, entry.num_values as u64, max_doc)?),
            (_, None) => return corrupt("sparse doc values without their documents"),
        };
        if entry.docs_with_field_offset == -1 && entry.num_values as u64 != max_doc as u64 {
            return corrupt(format!("dense doc values with [{}] values for [{max_doc}] documents", entry.num_values));
        }
        let mut n = Numeric { entry, max_doc, docs: disi, pending: None, values, block: None, next_block: 0, blocks_read: 0 };
        if let Some(d) = &mut n.docs {
            n.pending = d.next_doc()?;
        }
        Ok(n)
    }

    /// The value of `doc` (documents asked in increasing order), `None` when it has none.
    pub fn advance(&mut self, doc: u32) -> Result<Option<i64>> {
        if doc >= self.max_doc {
            return Ok(None);
        }
        let index = match self.entry.docs_with_field_offset {
            -2 => return Ok(None),
            -1 => doc as u64,
            _ => {
                while let Some((d, _)) = self.pending
                    && d < doc
                {
                    self.pending = self.docs.as_mut().map(Disi::next_doc).transpose()?.flatten();
                }
                match self.pending {
                    Some((d, i)) if d == doc => i,
                    _ => return Ok(None),
                }
            }
        };
        self.value(index).map(Some)
    }

    fn value(&mut self, index: u64) -> Result<i64> {
        let e = self.entry;
        if index >= e.num_values as u64 {
            return corrupt("a doc values index past the values");
        }
        if e.bits_per_value == 0 {
            return Ok(e.min_value);
        }
        if e.block_shift >= 0 {
            let block = index >> e.block_shift;
            loop {
                if let Some((b, start, bpv, delta)) = self.block
                    && b == block
                {
                    let v = if bpv == 0 { 0 } else { direct(&self.values[start..], bpv, index & ((1 << e.block_shift) - 1))? };
                    return Ok(e.gcd.wrapping_mul(v as i64).wrapping_add(delta));
                }
                if self.block.is_some_and(|(b, ..)| b > block) {
                    return corrupt("doc values asked out of order");
                }
                // The next block: bits per value, delta, then (unless 0 bits) its length and data.
                let mut inp = Input::at(self.values, self.next_block);
                let bpv = inp.u8()?;
                let delta = inp.i64()?;
                let start;
                if bpv == 0 {
                    start = inp.pos();
                    self.next_block = start;
                } else {
                    let len = inp.i32()?;
                    start = inp.pos();
                    if len < 0 || start + len as usize > self.values.len() {
                        return corrupt("a doc values block past its data");
                    }
                    self.next_block = start + len as usize;
                }
                self.block = Some((self.blocks_read, start, bpv, delta));
                self.blocks_read += 1;
            }
        }
        let v = direct(self.values, e.bits_per_value, index)?;
        Ok(match &e.table {
            Some(t) => *t.get(v as usize).ok_or_else(|| crate::Error::Corrupt("a doc values table index out of range".into()))?,
            None => e.gcd.wrapping_mul(v as i64).wrapping_add(e.min_value),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_reads_little_endian_bit_streams() {
        let data = [0b1011_0001, 0xFF, 0x0F, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(direct(&data, 4, 0).unwrap(), 1);
        assert_eq!(direct(&data, 4, 1).unwrap(), 0b1011);
        assert_eq!(direct(&data, 12, 1).unwrap(), 0x0FF);
        assert!(direct(&data, 64, 1).is_err());
        assert!(direct(&data, 3, 0).is_err());
    }

    #[test]
    fn disi_blocks() {
        // A sparse block of docs 3 and 70000 - 65536 in block 1, then an all block.
        let mut d = Vec::new();
        d.extend_from_slice(&0u16.to_le_bytes());
        d.extend_from_slice(&0u16.to_le_bytes());
        d.extend_from_slice(&3u16.to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&0u16.to_le_bytes());
        d.extend_from_slice(&4464u16.to_le_bytes());
        let mut disi = Disi::new(&d, -1, 2, 80000).unwrap();
        assert_eq!(disi.next_doc().unwrap(), Some((3, 0)));
        assert_eq!(disi.next_doc().unwrap(), Some((70000, 1)));
        assert_eq!(disi.next_doc().unwrap(), None);
        // A block read again (four bytes for 65,536 documents each time) or past the segment.
        let mut again = Vec::new();
        for _ in 0..3 {
            again.extend_from_slice(&0u16.to_le_bytes());
            again.extend_from_slice(&u16::MAX.to_le_bytes());
        }
        let mut disi = Disi::new(&again, -1, 65536, 200_000).unwrap();
        let mut n = 0;
        let e = loop {
            match disi.next_doc() {
                Ok(Some(_)) => n += 1,
                Ok(None) => break None,
                Err(e) => break Some(e),
            }
        };
        assert!(e.is_none() && n == 65536);
        let mut disi = Disi::new(&again, -1, 70000, 200_000).unwrap();
        assert!(std::iter::from_fn(|| disi.next_doc().transpose()).any(|r| r.is_err()));
        assert!(Disi::new(&again, -1, 70000, 100).is_err());
        // A sparse block repeating a document, and one past the segment.
        let sparse = |docs: &[u16]| {
            let mut d = 0u16.to_le_bytes().to_vec();
            d.extend_from_slice(&(docs.len() as u16 - 1).to_le_bytes());
            docs.iter().for_each(|x| d.extend_from_slice(&x.to_le_bytes()));
            d
        };
        let repeated = sparse(&[3, 3]);
        let mut disi = Disi::new(&repeated, -1, 2, 10).unwrap();
        assert!(std::iter::from_fn(|| disi.next_doc().transpose()).any(|r| r.is_err()));
        let past = sparse(&[3, 12]);
        let mut disi = Disi::new(&past, -1, 2, 10).unwrap();
        assert!(std::iter::from_fn(|| disi.next_doc().transpose()).any(|r| r.is_err()));
        // Truncated: an error, not a panic.
        let mut disi = Disi::new(&d[..9], -1, 2, 80000).unwrap();
        assert!(disi.next_doc().is_ok());
        assert!(disi.next_doc().is_err());
    }
}
