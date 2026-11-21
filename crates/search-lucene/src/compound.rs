//! Compound files (`Lucene90CompoundFormat`, `Lucene90CompoundReader`): a segment's small files
//! concatenated in `.cfs` (each 64-byte aligned, each with its own header and footer), with
//! their offsets and lengths in `.cfe`.

use crate::codec;
use crate::input::Input;
use crate::{Id, Result, corrupt};

pub const DATA_CODEC: &str = "Lucene90CompoundData";
pub const ENTRY_CODEC: &str = "Lucene90CompoundEntries";

/// One file inside `.cfs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The file name without the segment name (`.fdt`, `_Lucene90_0.dvd`).
    pub suffix: String,
    pub offset: u64,
    pub length: u64,
}

/// The entries of `.cfe` (footer checked first); `cfs_len` is the length of `.cfs`, which the
/// entries must fill exactly as Lucene checks.
pub fn read_entries(cfe: &[u8], id: &Id, cfs_len: u64) -> Result<Vec<Entry>> {
    codec::check_footer(cfe)?;
    let mut inp = Input::new(codec::body(cfe)?);
    codec::check_index_header(&mut inp, ENTRY_CODEC, 0, 0, id, "")?;
    // An entry takes a suffix and two longs.
    let n = inp.count(17)?;
    let mut out: Vec<Entry> = Vec::with_capacity(n);
    for _ in 0..n {
        let suffix = inp.string()?.to_string();
        if out.iter().any(|e| e.suffix == suffix) {
            return corrupt(format!("Duplicate cfs entry id={suffix} in CFS"));
        }
        if suffix.contains('/') || suffix.contains('\\') {
            return corrupt(format!("invalid cfs entry id [{}]", suffix.escape_debug()));
        }
        let offset = inp.i64()?;
        let length = inp.i64()?;
        if offset < 0 || length < 0 || (offset as u64).checked_add(length as u64).is_none_or(|end| end + codec::FOOTER_LEN as u64 > cfs_len) {
            return corrupt(format!("cfs entry [{suffix}] at [{offset}] of [{length}] bytes is outside the [{cfs_len}] bytes of the compound file"));
        }
        out.push(Entry { suffix, offset: offset as u64, length: length as u64 });
    }
    let header = (4 + 1 + DATA_CODEC.len() + 4 + 16 + 1) as u64;
    let expected = out.iter().map(|e| e.offset + e.length).max().unwrap_or(header) + codec::FOOTER_LEN as u64;
    if expected != cfs_len {
        return corrupt(format!("length should be {expected} bytes, but is {cfs_len} instead"));
    }
    Ok(out)
}

/// Checks the header `.cfs` starts with.
pub fn check_data_header(head: &[u8], id: &Id) -> Result<()> {
    codec::check_index_header(&mut Input::new(head), DATA_CODEC, 0, 0, id, "").map(|_| ())
}
