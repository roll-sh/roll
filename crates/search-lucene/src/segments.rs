//! A commit point (`segments_N`, Lucene's `SegmentInfos.readCommit`) and each segment's info
//! (`.si`, `Lucene99SegmentInfoFormat` and, for segments Lucene 9.0 to 9.8 wrote, the
//! `Lucene90SegmentInfoFormat` it replaced, which has no `hasBlocks` byte).
//!
//! Segment infos are read in big-endian order whatever the Lucene version (`CodecUtil.readBEInt`
//! and `readBELong`), so a Lucene 8 commit (Elasticsearch 7) reads here too; its segments are
//! refused when their `.si` is read ([`Error::Unsupported`]).

use crate::codec;
use crate::input::Input;
use crate::{Error, Id, Result, corrupt};

/// `org.apache.lucene.util.Version`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: i32,
    pub minor: i32,
    pub bugfix: i32,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.bugfix)
    }
}

impl Version {
    fn read_vints(inp: &mut Input) -> Result<Version> {
        Ok(Version { major: inp.vint()?, minor: inp.vint()?, bugfix: inp.vint()? })
    }

    fn read_ints(inp: &mut Input) -> Result<Version> {
        Ok(Version { major: inp.i32()?, minor: inp.i32()?, bugfix: inp.i32()? })
    }
}

/// `SegmentInfos.VERSION_74` and `VERSION_86`: the segment infos formats Lucene 8 to 10 write.
const SEGMENTS_MIN: i32 = 9;
const SEGMENTS_MAX: i32 = 10;

/// One segment of a commit (`SegmentCommitInfo`).
#[derive(Clone, Debug, PartialEq)]
pub struct SegmentCommit {
    pub name: String,
    pub id: Id,
    pub codec: String,
    /// Generation of the live docs file, `-1` for none.
    pub del_gen: i64,
    pub del_count: i32,
    /// Generation of the field infos file, `-1` for the segment's own.
    pub field_infos_gen: i64,
    pub dv_gen: i64,
    pub soft_del_count: i32,
    pub field_infos_files: Vec<String>,
    /// Doc values update files by field number.
    pub dv_update_files: Vec<(i32, Vec<String>)>,
}

/// A commit point.
#[derive(Clone, Debug, PartialEq)]
pub struct SegmentInfos {
    pub generation: i64,
    pub id: Id,
    /// The Lucene version that wrote the commit.
    pub lucene: Version,
    pub created_major: i32,
    pub min_segment_version: Option<Version>,
    pub segments: Vec<SegmentCommit>,
    pub user_data: Vec<(String, String)>,
}

/// The generation in a `segments_N` file name.
pub fn generation_of(file_name: &str) -> Option<i64> {
    let g = file_name.strip_prefix("segments_")?;
    i64::from_str_radix(g, 36).ok().filter(|g| *g >= 0)
}

/// `SegmentInfos.readCommit` of `segments_<generation>`; the footer is checked first.
pub fn read_commit(file: &[u8], generation: i64) -> Result<SegmentInfos> {
    codec::check_footer(file)?;
    let mut inp = Input::new(codec::body(file)?);
    let format = codec::check_header(&mut inp, "segments", SEGMENTS_MIN, SEGMENTS_MAX)?;
    let id = inp.id()?;
    let n = inp.u8()? as usize;
    let suffix = inp.bytes(n)?;
    if suffix != crate::base36(generation).as_bytes() {
        return corrupt(format!("file mismatch, expected suffix={}, got={}", crate::base36(generation), String::from_utf8_lossy(suffix)));
    }
    let lucene = Version::read_vints(&mut inp)?;
    let created_major = inp.vint()?;
    if lucene.major < created_major {
        return corrupt(format!("Creation version [{created_major}.x] can't be greater than the version that wrote the segment infos: [{lucene}]"));
    }
    let _version = inp.be_i64()?;
    let _counter = inp.vlong()?;
    let count = inp.be_i32()?;
    // A segment takes at least its name, id, codec and counts.
    if count < 0 || count as usize * 50 > inp.remaining() {
        return corrupt(format!("invalid segment count: {count}"));
    }
    let min_segment_version = if count > 0 { Some(Version::read_vints(&mut inp)?) } else { None };
    let mut segments = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name = inp.string()?.to_string();
        let id = inp.id()?;
        let codec = inp.string()?.to_string();
        let del_gen = inp.be_i64()?;
        let del_count = inp.be_i32()?;
        let field_infos_gen = inp.be_i64()?;
        let dv_gen = inp.be_i64()?;
        let soft_del_count = inp.be_i32()?;
        if del_count < 0 || soft_del_count < 0 {
            return corrupt(format!("invalid deletion count: {del_count} / {soft_del_count}"));
        }
        // VERSION_86 adds the commit info's id (marker 1 then 16 bytes, or 0).
        if format > SEGMENTS_MIN {
            match inp.u8()? {
                0 => {}
                1 => {
                    inp.id()?;
                }
                m => return corrupt(format!("invalid SegmentCommitInfo ID marker: {m}")),
            }
        }
        let field_infos_files = inp.strings()?;
        let fields = inp.be_i32()?;
        if fields < 0 || fields as usize * 5 > inp.remaining() {
            return corrupt(format!("invalid doc values update count: {fields}"));
        }
        let mut dv_update_files = Vec::with_capacity(fields as usize);
        for _ in 0..fields {
            let field = inp.be_i32()?;
            dv_update_files.push((field, inp.strings()?));
        }
        validate_name(&name)?;
        segments.push(SegmentCommit { name, id, codec, del_gen, del_count, field_infos_gen, dv_gen, soft_del_count, field_infos_files, dv_update_files });
    }
    let user_data = inp.string_map()?;
    if inp.remaining() != 0 {
        return corrupt("trailing bytes after the segment infos");
    }
    Ok(SegmentInfos { generation, id, lucene, created_major, min_segment_version, segments, user_data })
}

/// Segment names are `_<base 36>`: anything else would name files outside the segment.
fn validate_name(name: &str) -> Result<()> {
    let ok = name.len() > 1 && name.len() <= 16 && name.starts_with('_') && name[1..].bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase());
    if !ok {
        return corrupt(format!("invalid segment name [{}]", name.escape_debug()));
    }
    Ok(())
}

/// A segment's `.si`.
#[derive(Clone, Debug, PartialEq)]
pub struct SegmentInfo {
    /// The Lucene version that wrote the segment.
    pub version: Version,
    pub min_version: Option<Version>,
    pub max_doc: u32,
    pub compound: bool,
    pub has_blocks: bool,
    pub diagnostics: Vec<(String, String)>,
    pub files: Vec<String>,
    pub attributes: Vec<(String, String)>,
    /// Fields the segment is sorted by (an index sort; documents are read in doc id order either
    /// way, so only the count matters here).
    pub sort_fields: u32,
}

/// The `hasBlocks` byte came with `Lucene99SegmentInfoFormat`, Lucene 9.9's.
const HAS_BLOCKS_SINCE: Version = Version { major: 9, minor: 9, bugfix: 0 };

/// Reads the `.si` of the segment with id `id`; the footer is checked first.
pub fn read_segment_info(file: &[u8], id: &Id) -> Result<SegmentInfo> {
    match codec::header_name(file) {
        Some("Lucene90SegmentInfo") => {}
        Some(other @ ("Lucene86SegmentInfo" | "Lucene70SegmentInfo" | "Lucene62SegmentInfo")) => {
            return Err(Error::Unsupported(format!("the segment was written by Lucene 8 or earlier ([{other}]: Elasticsearch 7.x or earlier); roll reads Lucene 9 and 10 segments (Elasticsearch 8.0 and later)")));
        }
        Some(other) => return Err(Error::Unsupported(format!("unknown segment info format [{other}]"))),
        None => return corrupt("the segment info has no codec header"),
    }
    codec::check_footer(file)?;
    let mut inp = Input::new(codec::body(file)?);
    codec::check_index_header(&mut inp, "Lucene90SegmentInfo", 0, 0, id, "")?;
    let version = Version::read_ints(&mut inp)?;
    if version.major < 9 {
        return Err(Error::Unsupported(format!("the segment was written by Lucene {version}; roll reads Lucene 9 and 10 segments")));
    }
    if version.major > 10 {
        return Err(Error::Unsupported(format!("the segment was written by Lucene {version}, newer than the Lucene 10 roll reads")));
    }
    let min_version = match inp.u8()? {
        0 => None,
        1 => Some(Version::read_ints(&mut inp)?),
        b => return corrupt(format!("Illegal boolean value {b}")),
    };
    let max_doc = inp.i32()?;
    if max_doc < 0 {
        return corrupt(format!("invalid docCount: {max_doc}"));
    }
    let compound = inp.u8()? == 1;
    let has_blocks = if version >= HAS_BLOCKS_SINCE { inp.u8()? == 1 } else { false };
    let diagnostics = inp.string_map()?;
    let files = inp.strings()?;
    let attributes = inp.string_map()?;
    let sort_fields = inp.vuint()?;
    Ok(SegmentInfo { version, min_version, max_doc: max_doc as u32, compound, has_blocks, diagnostics, files, attributes, sort_fields })
}

impl SegmentInfo {
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}
