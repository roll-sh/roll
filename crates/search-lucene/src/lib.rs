//! Readers for the Lucene index files an Elasticsearch snapshot holds, so roll can restore a
//! snapshot Elasticsearch wrote by indexing each live document's `_source` again.
//!
//! Only what a restore needs is read: a commit's segment infos ([`segments`]), each segment's
//! info, compound file entries ([`compound`]), field infos ([`fields`]), stored fields
//! ([`stored`]: `_id`, `_routing`, `_source`), live documents ([`live`]), numeric doc values
//! ([`docvalues`]: soft deletes, `_seq_no`, `_version`, `_primary_term`) and flat vectors
//! ([`vectors`]: the dense vectors Elasticsearch 9 leaves out of `_source`). Postings, points,
//! norms and vector graphs are never opened.
//!
//! Formats: Lucene 9.0 to 10.x (Elasticsearch 8.0 to 9.x). Every module ports a Lucene class
//! (Apache-2.0, named in its documentation); Lucene 9 made the index files little-endian
//! except CodecUtil's headers and footers and the segment infos, which stay big-endian. Files
//! of other versions are refused with [`Error::Unsupported`], naming what they are.
//!
//! Everything read is someone else's input (a tenant's bucket can hold anything). A count a
//! reader reads is bounded by the bytes present or by a fixed maximum (documents per chunk,
//! table sizes), lengths are checked against the input before anything is copied, decompressed
//! sizes are capped by the caller, and every file's CodecUtil checksum is checked
//! ([`codec::check_footer`], [`codec::Checksum`] for files read as a stream). What a segment's
//! read holds is charged through [`segment::Files::charge`] before it's fetched or allocated:
//! small files while they're read whole, and what parsing them may build (a bound per byte of
//! the file, each module's `HELD_PER_BYTE`) until the read ends; stream windows; decompressed
//! chunks; live docs; the doc values and vector ranges kept; and each document's vectors until
//! it's handed over. The commit and `.si` files are read by the caller, which charges them with
//! [`segments::HELD_PER_BYTE`]. What isn't charged: codec headers of a few dozen bytes, and
//! bookkeeping that grows with the fields asked for rather than with the data (the vector field
//! and magnitude names, the doc values wanted, a document's per-field scale), the sets that
//! refuse a duplicate while a file is parsed and a map's entries while it's collected (both
//! within the bound charged for that file). No reader does
//! I/O: they parse byte slices, so the caller decides how bytes arrive and what they cost.

pub mod codec;
pub mod compound;
pub mod docvalues;
pub mod fields;
pub mod input;
pub mod live;
pub mod lz4;
pub mod segment;
pub mod segments;
pub mod stored;
pub mod vectors;

/// What a reader's output may hold per byte of the file it read, at most, given what one item
/// holds (`item`, map slots counted twice for the tables' spare room) and the fewest bytes of
/// the file one takes (`min_bytes`). Strings bound it from below: an empty one takes a byte of
/// the file (its length) and a `String`; a name kept twice (a map's key) costs its bytes again.
pub(crate) const fn held_per_byte(item: usize, min_bytes: usize) -> u64 {
    let per_item = item / min_bytes + 1;
    let per_string = std::mem::size_of::<String>();
    (if per_item > per_string { per_item } else { per_string }) as u64 + 2
}

/// What reading a file can fail with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The bytes aren't what the format says (truncated, a bad checksum, a value out of
    /// range).
    #[error("{0}")]
    Corrupt(String),
    /// A valid file of a format or version roll doesn't read.
    #[error("{0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn corrupt<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Corrupt(msg.into()))
}

/// A segment's or commit's 16-byte id.
pub type Id = [u8; 16];

/// `IndexFileNames.segmentFileName`: `<segment>[_<suffix>].<ext>`.
pub fn segment_file_name(segment: &str, suffix: &str, ext: &str) -> String {
    match (suffix.is_empty(), ext.is_empty()) {
        (true, _) => format!("{segment}.{ext}"),
        (false, true) => format!("{segment}_{suffix}"),
        (false, false) => format!("{segment}_{suffix}.{ext}"),
    }
}

/// `Long.toString(n, Character.MAX_RADIX)`: generations in file names.
pub fn base36(mut n: i64) -> String {
    if n == 0 {
        return "0".into();
    }
    let neg = n < 0;
    let mut digits = Vec::new();
    while n != 0 {
        let d = (n % 36).unsigned_abs() as u8;
        digits.push(if d < 10 { b'0' + d } else { b'a' + d - 10 });
        n /= 36;
    }
    if neg {
        digits.push(b'-');
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_default()
}

/// `IndexFileNames.fileNameFromGeneration`: `<base>.<ext>` at generation 0, `<base>_<gen>.<ext>`
/// after, `None` for -1 (no file).
pub fn file_name_from_generation(base: &str, ext: &str, generation: i64) -> Option<String> {
    match generation {
        -1 => None,
        0 => Some(segment_file_name(base, "", ext)),
        g => Some(format!("{base}_{}.{ext}", base36(g))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        assert_eq!(file_name_from_generation("_3", "liv", 1).as_deref(), Some("_3_1.liv"));
        assert_eq!(file_name_from_generation("_3", "fnm", 0).as_deref(), Some("_3.fnm"));
        assert_eq!(file_name_from_generation("_3", "fnm", -1), None);
        assert_eq!(segment_file_name("_0", "1_Lucene90_0", "dvd"), "_0_1_Lucene90_0.dvd");
    }
}
