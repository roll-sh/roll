//! Vectors kept out of the stored `_source` (`index.mapping.exclude_source_vectors`, on by
//! default in Elasticsearch 9).
//!
//! Elasticsearch takes the values of indexed vector fields out of `_source` when it indexes a
//! document (`SourceFieldMapper` with excluded vectors) and patches them back from the
//! vector values whenever a reader asks for them (its synthetic vectors loaders): the index
//! already holds every vector as floats, so the stored source doesn't keep them twice. roll
//! does the same for indexed `dense_vector` fields outside `nested` objects ([`stripped`]):
//! [`strip`] when the stored source is made (`write::stored_source`), [`patch`] where a reader
//! needs them back. Elasticsearch puts a patched vector at the end of its object (the first
//! element of an array of objects; several in one object in no fixed order: it changes with
//! each JVM start), and prints what it stores: float vectors of a `cosine` field normalized
//! and multiplied by their magnitude ([`es_floats`]), `bfloat16` ones rounded.
//!
//! Readers that patch: the get APIs (so `_update`, `_source`, `_mget` and the knn `lookup`
//! builder), search hits that show vectors or read them for `fields`, scripts'
//! `params._source`, reindex and update by query, and the copies resize makes. Readers that
//! never see vectors in Elasticsearch read the stored source as it is: runtime fields'
//! `params._source`, a percolated stored document.
//!
//! Kept in the stored source: vectors of `index: false` fields (Elasticsearch keeps and shows
//! them verbatim), inside `nested` objects, `sparse_vector` values (rebuilt by Elasticsearch
//! from their feature postings, which `synthetic::vectors` imitates when it shows them),
//! OpenSearch's `knn_vector`, and every vector of a document written before roll stripped
//! them: [`patch`] leaves a vector the source still has alone.

use super::fields::{FieldInfo, FieldTable, Kind};
use crate::json::{Map, Value};
use search_codec::{VectorElement, VectorSimilarity};
use std::sync::Arc;

/// The fields whose values `idx`'s stored `_source` leaves out, by name.
pub fn stripped(idx: &crate::meta::IndexMeta, table: &FieldTable) -> Vec<Arc<FieldInfo>> {
    use super::synthetic::{SourceMode, source_mode};
    if idx.dialect != crate::cluster::Dialect::Elasticsearch
        || idx.settings.get("index.mapping.exclude_source_vectors") == Some("false")
        || source_mode(idx) != SourceMode::Stored
        || idx.mapping.source.enabled == Some(false)
    {
        return Vec::new();
    }
    copied(table)
}

/// The engine fields a stripped field's vectors live in: its own, then its earlier layouts.
pub fn numbers(fields: &[Arc<FieldInfo>]) -> Vec<u32> {
    fields.iter().flat_map(|f| std::iter::once(f.number).chain(f.vector_layouts.iter().copied())).collect()
}

fn join(path: &str, k: &str) -> String {
    if path.is_empty() { k.to_string() } else { format!("{path}.{k}") }
}

/// Whether a field of `fields` lives under `path` (an object to look into).
fn under(fields: &[Arc<FieldInfo>], path: &str) -> bool {
    fields.iter().any(|f| f.name.len() > path.len() && f.name.starts_with(path) && f.name.as_bytes()[path.len()] == b'.')
}

/// A document's source without the values of `fields` (`null`s too), or `None` when it has
/// none of them. The vectors aren't copied.
pub fn strip(fields: &[Arc<FieldInfo>], m: &Map) -> Option<Map> {
    fields.iter().any(|f| contains(m, "", &f.name)).then(|| strip_in(fields, "", m))
}

fn strip_in(fields: &[Arc<FieldInfo>], path: &str, m: &Map) -> Map {
    let mut out = Map::with_capacity(m.len());
    for (k, v) in m {
        let full = join(path, k);
        if fields.iter().any(|f| f.name == full) {
            continue;
        }
        let v = match v {
            Value::Obj(inner) if under(fields, &full) => Value::Obj(crate::stack::maybe_grow(|| strip_in(fields, &full, inner))),
            Value::Arr(items) if under(fields, &full) => Value::Arr(
                items
                    .iter()
                    .map(|it| match it {
                        Value::Obj(inner) => Value::Obj(crate::stack::maybe_grow(|| strip_in(fields, &full, inner))),
                        other => other.clone(),
                    })
                    .collect(),
            ),
            other => other.clone(),
        };
        out.insert(k.clone(), v);
    }
    out
}

/// Puts the vectors of `fields` back into a document's source map, as Elasticsearch patches
/// them; `vector` gives the document's raw vector of a field ([`InSegment`], [`InDoc`]). A field the source already has (a document stored with its vectors) is left
/// as it is. Returns whether anything was added.
pub fn patch(fields: &[Arc<FieldInfo>], m: &mut Map, vector: &mut impl Vectors) -> bool {
    patch_with(fields, m, vector, true)
}

/// [`patch`] with the floats as the index holds them, for copying a document into another
/// index (resize, restore: Elasticsearch copies the vectors, roll indexes the copy again).
pub fn patch_raw(fields: &[Arc<FieldInfo>], m: &mut Map, vector: &mut impl Vectors) -> bool {
    patch_with(fields, m, vector, false)
}

fn patch_with(fields: &[Arc<FieldInfo>], m: &mut Map, vector: &mut impl Vectors, es: bool) -> bool {
    let mut added = false;
    for f in fields {
        if contains(m, "", &f.name) {
            continue;
        }
        let Some(raw) = vector.vector(f) else { continue };
        insert(m, &f.name, value(f, raw, es));
        added = true;
    }
    added
}

/// [`patch`] over stored source bytes: `None` when nothing was added.
pub fn patch_bytes(fields: &[Arc<FieldInfo>], bytes: &[u8], vector: &mut impl Vectors) -> crate::error::EsResult<Option<Vec<u8>>> {
    if fields.is_empty() {
        return Ok(None);
    }
    let Value::Obj(mut m) = crate::fetch::parse_source(bytes)? else { return Ok(None) };
    Ok(patch(fields, &mut m, vector).then(|| Value::Obj(m).to_json().into_bytes()))
}

/// A stored source as Elasticsearch's patched one reads (`params._source` of script fields):
/// the stripped vectors back in, every vector printed and placed as it shows them
/// (`synthetic::vectors`). `None` when the index strips nothing.
pub fn patched_source(table: &FieldTable, bytes: &[u8], vector: &mut impl Vectors) -> crate::error::EsResult<Option<Vec<u8>>> {
    if table.stripped_vectors.is_empty() {
        return Ok(None);
    }
    let Value::Obj(mut m) = crate::fetch::parse_source(bytes)? else { return Ok(None) };
    super::synthetic::vectors(table, "", &mut m, false);
    patch(&table.stripped_vectors, &mut m, vector);
    Ok(Some(Value::Obj(m).to_json().into_bytes()))
}

/// [`patch_bytes`] of the `_source` of a document read by id (with the vectors of
/// [`numbers`]), in place.
pub fn patch_doc(fields: &[Arc<FieldInfo>], doc: &mut search_index::StoredDoc) -> crate::error::EsResult<()> {
    use search_codec::stored::OwnedStoredValue;
    let search_index::StoredDoc { fields: stored, vectors, .. } = doc;
    if fields.is_empty() || vectors.is_empty() {
        return Ok(());
    }
    let Some((_, OwnedStoredValue::Bytes(b))) = stored.iter_mut().find(|(n, _)| *n == super::fields::SOURCE_FIELD) else { return Ok(()) };
    if let Some(nb) = patch_bytes(fields, b, &mut InDoc(vectors))? {
        *b = nb;
    }
    Ok(())
}

/// Where [`patch`] finds a document's raw vector of a field.
pub trait Vectors {
    fn vector(&mut self, f: &FieldInfo) -> Option<&[u8]>;
}

/// The vectors read with a document ([`numbers`]): a field's own engine field's, else an
/// earlier layout's.
pub struct InDoc<'a>(pub &'a [(u32, Vec<u8>)]);

impl Vectors for InDoc<'_> {
    fn vector(&mut self, f: &FieldInfo) -> Option<&[u8]> {
        let v = self.0;
        std::iter::once(f.number).chain(f.vector_layouts.iter().copied()).find_map(|n| v.iter().find(|(m, _)| *m == n).map(|(_, b)| b.as_slice()))
    }
}

/// Doc `local` of a segment of the field's index.
pub struct InSegment<'a>(pub &'a search_index::SegmentContext, pub u32);

impl Vectors for InSegment<'_> {
    fn vector(&mut self, f: &FieldInfo) -> Option<&[u8]> {
        let (seg, local) = (self.0, self.1);
        std::iter::once(f.number).chain(f.vector_layouts.iter().copied()).find_map(|n| seg.reader.vectors(n).and_then(|v| v.ord(local).map(|o| v.vector(o))))
    }
}

/// Doc `local` of a segment of another index (a copy's source: resize, restore), whose engine
/// fields are numbered its own way: the field's vector found by its path.
pub struct InSegmentByName<'a>(pub &'a search_index::SegmentContext, pub u32);

impl Vectors for InSegmentByName<'_> {
    fn vector(&mut self, f: &FieldInfo) -> Option<&[u8]> {
        let (seg, local) = (self.0, self.1);
        seg.reader
            .info()
            .fields
            .iter()
            .filter(|i| i.name == f.name && i.vectors.is_some())
            .find_map(|i| seg.reader.vectors(i.number).and_then(|v| v.ord(local).map(|o| v.vector(o))))
    }
}

/// Whether a fetch under `fs` shows a stripped vector or reads one for `fields` (the hit then
/// patches them in): `exclude_vectors: false`, includes naming one (Elasticsearch shows the
/// vectors they name), a forced synthetic source, or `fields` patterns matching one.
pub fn wanted<'f>(table: &FieldTable, fs: &crate::fetch::FetchSource, force_synthetic: bool, mut fields: impl Iterator<Item = &'f str>) -> bool {
    let named = |p: &str| table.matching(p).iter().any(|n| table.stripped_vectors.iter().any(|f| &f.name == n));
    (fs.fetch && (fs.exclude_vectors == Some(false) || force_synthetic || {
        let g = super::source::Globs::new(&fs.includes);
        !fs.includes.is_empty() && table.stripped_vectors.iter().any(|f| g.matches(&f.name))
    })) || fields.any(named)
}

/// The fields a copy of an index into one with `table` patches back: every indexed
/// `dense_vector` outside `nested` objects, whether or not the copy strips them again.
pub fn copied(table: &FieldTable) -> Vec<Arc<FieldInfo>> {
    let mut out: Vec<Arc<FieldInfo>> = table
        .fields
        .values()
        .filter(|f| f.is_source_vector() && f.kind == Kind::DenseVector && f.index && f.vector.is_some() && f.nested_path.is_none())
        .cloned()
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Whether the source has a value (even `null`) at `name`, through objects and arrays of them.
fn contains(m: &Map, path: &str, name: &str) -> bool {
    m.iter().any(|(k, v)| {
        let full = join(path, k);
        if full == name {
            return true;
        }
        if !(name.len() > full.len() && name.starts_with(full.as_str()) && name.as_bytes()[full.len()] == b'.') {
            return false;
        }
        match v {
            Value::Obj(inner) => contains(inner, &full, name),
            Value::Arr(items) => items.iter().any(|it| matches!(it, Value::Obj(inner) if contains(inner, &full, name))),
            _ => false,
        }
    })
}

/// Adds `v` at the end of the object `name` lives in: through the first object of an array,
/// making objects that are missing.
fn insert(m: &mut Map, name: &str, v: Value) {
    let Some((head, rest)) = name.split_once('.') else {
        m.insert(name.to_string(), v);
        return;
    };
    let child = m.entry(head.to_string()).or_insert_with(|| Value::Obj(Map::new()));
    match child {
        Value::Obj(inner) => insert(inner, rest, v),
        Value::Arr(items) => {
            if let Some(Value::Obj(inner)) = items.iter_mut().find(|it| matches!(it, Value::Obj(_))) {
                insert(inner, rest, v);
            }
        }
        _ => {}
    }
}

/// A stored vector as Elasticsearch's source shows it: floats ([`es_floats`] unless `es` is
/// false), or the byte values of `byte` and `bit` vectors.
fn value(f: &FieldInfo, raw: &[u8], es: bool) -> Value {
    match f.vector.map(|s| s.element) {
        Some(VectorElement::Byte | VectorElement::Bit) => Value::Arr(raw.iter().map(|b| Value::from(*b as i8 as i64)).collect()),
        _ => {
            let mut x: Vec<f32> = raw.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
            if es {
                es_floats(f, &mut x);
            }
            Value::Arr(x.into_iter().map(crate::query::xparser::float_value).collect())
        }
    }
}

/// The floats Elasticsearch keeps for a float vector as given (`DenseVectorFieldMapper`):
/// a `cosine` field's vector is normalized unless its squared magnitude (summed in float, in
/// order) is within 1e-3 of 1, and read back multiplied by the magnitude; `bfloat16`
/// elements keep the upper half of each float, rounded to nearest even.
pub fn es_floats(f: &FieldInfo, x: &mut [f32]) {
    let bf16 = f.mapper.raw("element_type").and_then(Value::as_str) == Some("bfloat16");
    let round = |v: f32| if bf16 { bfloat16(v) } else { v };
    if f.vector.is_some_and(|s| s.similarity == VectorSimilarity::Cosine) {
        let mut sq = 0f32;
        for v in x.iter() {
            sq += v * v;
        }
        if (sq - 1.0).abs() > 1e-3 {
            let m = (sq as f64).sqrt() as f32;
            for v in x.iter_mut() {
                *v = round(*v / m) * m;
            }
            return;
        }
    }
    for v in x.iter_mut() {
        *v = round(*v);
    }
}

/// A float rounded to `bfloat16` (nearest even), as a float.
fn bfloat16(x: f32) -> f32 {
    let b = x.to_bits();
    f32::from_bits(((b + 0x7fff + ((b >> 16) & 1)) >> 16) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(s: &str) -> Map {
        match crate::json::parse(s.as_bytes()).unwrap().unwrap() {
            Value::Obj(m) => m,
            _ => unreachable!(),
        }
    }

    #[test]
    fn insert_goes_to_the_end_of_the_first_object() {
        let mut m = obj(r#"{"o":[{"q":1},{"r":1}],"t":1}"#);
        insert(&mut m, "o.w", Value::from(1));
        insert(&mut m, "v", Value::from(2));
        insert(&mut m, "x.y.z", Value::from(3));
        assert_eq!(Value::Obj(m).to_json(), r#"{"o":[{"q":1,"w":1},{"r":1}],"t":1,"v":2,"x":{"y":{"z":3}}}"#);
    }

    #[test]
    fn contains_looks_through_arrays_and_dotted_keys() {
        let m = obj(r#"{"o":[{"q":1},{"w":null}],"a.b":{"c":1}}"#);
        assert!(contains(&m, "", "o.w"));
        assert!(contains(&m, "", "a.b.c"));
        assert!(!contains(&m, "", "o.q2"));
        assert!(!contains(&m, "", "a.bc"));
    }
}
