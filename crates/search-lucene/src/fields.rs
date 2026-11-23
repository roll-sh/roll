//! Field infos (`.fnm`, `Lucene94FieldInfosFormat`): each field's name, number, doc values type
//! and generation, attributes (which per-field format holds its doc values or vectors) and
//! vector shape. Formats 0 (Lucene 9.4 to 9.9), 1 (parent fields, 9.10) and 2 (doc values
//! skippers, 10), and `Lucene90FieldInfosFormat` (Lucene 9.0 to 9.3, from lucene-backward-codecs),
//! which has no vector encoding byte (every vector is `float32`).

use crate::codec;
use crate::input::Input;
use crate::{Error, Id, Result, corrupt};
use std::collections::HashMap;

pub const CODEC: &str = "Lucene94FieldInfos";
pub const CODEC_90: &str = "Lucene90FieldInfos";
const FORMAT_PARENT_FIELD: i32 = 1;
const FORMAT_DOCVALUE_SKIPPER: i32 = 2;

pub const SOFT_DELETES_FIELD: u8 = 0x8;

/// `DocValuesType` as stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocValues {
    None,
    Numeric,
    Binary,
    Sorted,
    SortedNumeric,
    SortedSet,
}

/// `VectorEncoding` as stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorEncoding {
    Byte,
    Float32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldInfo {
    pub name: String,
    pub number: u32,
    pub soft_deletes: bool,
    pub doc_values: DocValues,
    /// Whether the doc values have a skip index (`DocValuesSkipIndexType` other than `NONE`).
    pub dv_skipper: bool,
    /// The generation of the field's doc values updates, `-1` for none.
    pub dv_gen: i64,
    pub attributes: Vec<(String, String)>,
    pub vector_dims: u32,
    pub vector_encoding: VectorEncoding,
}

impl FieldInfo {
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// A segment's fields, found by number or name in constant time (a file may list many: other
/// readers look a field up for each entry they read).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FieldInfos {
    fields: Vec<FieldInfo>,
    by_number: HashMap<u32, usize>,
    by_name: HashMap<String, usize>,
}

impl FieldInfos {
    pub fn by_number(&self, n: u32) -> Option<&FieldInfo> {
        self.by_number.get(&n).map(|&i| &self.fields[i])
    }

    pub fn by_name(&self, name: &str) -> Option<&FieldInfo> {
        self.by_name.get(name).map(|&i| &self.fields[i])
    }

    /// The fields in file order.
    pub fn iter(&self) -> std::slice::Iter<'_, FieldInfo> {
        self.fields.iter()
    }
}

/// Reads a segment's field infos (`suffix`: the field infos generation in base 36, empty for
/// the segment's own); the footer is checked first.
pub fn read_field_infos(file: &[u8], id: &Id, suffix: &str) -> Result<FieldInfos> {
    let lucene90 = match codec::header_name(file) {
        Some(CODEC) => false,
        Some(CODEC_90) => true,
        Some(other @ ("Lucene60FieldInfos" | "Lucene50FieldInfos")) => {
            return Err(Error::Unsupported(format!("field infos format [{other}] (Lucene 8 or earlier: Elasticsearch 7.x or earlier) isn't read; roll reads Lucene 9 and 10")));
        }
        Some(other) => return Err(Error::Unsupported(format!("unknown field infos format [{other}]"))),
        None => return corrupt("the field infos have no codec header"),
    };
    codec::check_footer(file)?;
    let mut inp = Input::new(codec::body(file)?);
    let format = match lucene90 {
        true => codec::check_index_header(&mut inp, CODEC_90, 0, 0, id, suffix)?,
        false => codec::check_index_header(&mut inp, CODEC, 0, FORMAT_DOCVALUE_SKIPPER, id, suffix)?,
    };
    // A field takes at least its name, number, bits, types, generation and counts.
    let n = inp.count(16)?;
    let mut fields: Vec<FieldInfo> = Vec::with_capacity(n);
    let (mut by_number, mut by_name) = (HashMap::with_capacity(n), HashMap::with_capacity(n));
    for _ in 0..n {
        let name = inp.string()?.to_string();
        let number = inp.vint()?;
        if number < 0 {
            return corrupt(format!("invalid field number for field: {name}, fieldNumber={number}"));
        }
        let bits = inp.u8()?;
        if bits & 0xC0 != 0 || (format < FORMAT_PARENT_FIELD && bits & 0xF0 != 0) {
            return corrupt(format!("unused bits are set \"{bits:b}\""));
        }
        let _index_options = inp.u8()?;
        let doc_values = match inp.u8()? {
            0 => DocValues::None,
            1 => DocValues::Numeric,
            2 => DocValues::Binary,
            3 => DocValues::Sorted,
            4 => DocValues::SortedSet,
            5 => DocValues::SortedNumeric,
            b => return corrupt(format!("invalid docvalues byte: {b}")),
        };
        let dv_skipper = if format >= FORMAT_DOCVALUE_SKIPPER {
            match inp.u8()? {
                0 => false,
                1 | 2 => true,
                b => return corrupt(format!("invalid docvaluesskipindex byte: {b}")),
            }
        } else {
            false
        };
        let dv_gen = inp.i64()?;
        let attributes = inp.string_map()?;
        let point_dims = inp.vuint()?;
        if point_dims != 0 {
            inp.vuint()?;
            inp.vuint()?;
        }
        let vector_dims = inp.vuint()?;
        let vector_encoding = match lucene90 {
            true => VectorEncoding::Float32,
            false => match inp.u8()? {
                0 => VectorEncoding::Byte,
                1 => VectorEncoding::Float32,
                b => return corrupt(format!("invalid vector encoding: {b}")),
            },
        };
        let _similarity = inp.u8()?;
        if by_number.contains_key(&(number as u32)) || by_name.contains_key(&name) {
            return corrupt(format!("duplicate field [{name}] or number [{number}]"));
        }
        by_number.insert(number as u32, fields.len());
        by_name.insert(name.clone(), fields.len());
        fields.push(FieldInfo { name, number: number as u32, soft_deletes: bits & SOFT_DELETES_FIELD != 0, doc_values, dv_skipper, dv_gen, attributes, vector_dims, vector_encoding });
    }
    if inp.remaining() != 0 {
        return corrupt("trailing bytes after the field infos");
    }
    Ok(FieldInfos { fields, by_number, by_name })
}
